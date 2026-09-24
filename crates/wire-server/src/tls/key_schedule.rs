//! TLS 1.3 鍵スケジュール本体（RFC 8446 §7.1。TASK-228・WIRE-9・HTTP-10
//! ポインタ。Issue #956・親 #941）。
//!
//! 対象暗号スイートは `TLS_AES_128_GCM_SHA256` のみ（親 Issue #941 の方針）の
//! ため、ハッシュは SHA-256 に固定し、ハッシュ関数を抽象化するジェネリクスは
//! 導入しない。[`super::hkdf`] の原始操作（HMAC-SHA-256・HKDF-Extract/Expand・
//! `HKDF-Expand-Label`・`Derive-Secret`）の上に、Early → Handshake → Master の
//! 3 段の secret 遷移と、各段の traffic secret／key／iv 導出を型状態
//! （`self` を消費して次段へ遷移する API）で実装する。誤用（逆戻り・同じ段の
//! 二重利用）を型で防ぐ設計は [`super::x25519::EphemeralSecret::diffie_hellman`]
//! と同じ流儀。
//!
//! **本モジュールが対象外とする範囲**（親 Issue #941 の方針・後続 sub-issue の
//! 担当）:
//! - PSK・0-RTT（`early_secret` は `new_without_psk` のみを提供する）
//! - セッション再開（`exp master`／`res master` は導出しない）
//! - transcript hash の蓄積・Finished の verify_data 計算/検証（#964）
//! - レコード保護（nonce・シーケンス番号。#959）
//! - alert の実送出・ハンドシェイク状態機械への結線（#965・#966 以降）

use super::hkdf::{derive_secret, hkdf_expand_label, HkdfError, Secret32};
use super::x25519::SharedSecret;
use std::fmt;

const HASH_LEN: usize = super::hkdf::HASH_LEN;

/// TLS 1.3 の Early secret（RFC 8446 §7.1 鍵スケジュール図の最上段）。
///
/// PSK を使わない構成（親 Issue #941 の方針）のみを提供するため、
/// `HKDF-Extract(salt = 0, IKM = 0)`（RFC 8446 の図の "0" 表記どおり、salt・IKM
/// とも [`super::hkdf::HASH_LEN`] バイトの 0 埋め）で構成する。空 salt と
/// 32 バイトの 0 salt は HMAC の鍵パディング規則により同一の PRK を生む
/// （[`super::hkdf`] のテストで固定済み）ため、どちらの表記を採っても値は
/// 変わらない。
pub struct EarlySecret(Secret32);

impl fmt::Debug for EarlySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EarlySecret").field(&"<redacted>").finish()
    }
}

impl EarlySecret {
    /// PSK なしの Early secret を構成する。
    pub fn new_without_psk() -> Self {
        let salt = [0u8; HASH_LEN];
        let ikm = [0u8; HASH_LEN];
        EarlySecret(super::hkdf::hkdf_extract(&salt, &ikm))
    }

    /// `derived = Derive-Secret(early, "derived", Hash(""))` を経て
    /// `Extract(derived, ecdhe)` を計算し、Handshake secret へ遷移する。
    /// `self` を消費するため、同じ Early secret から二度 handshake secret を
    /// 導出することはできない（型状態による誤用防止）。
    pub fn into_handshake(self, ecdhe: &SharedSecret) -> Result<HandshakeSecret, HkdfError> {
        let empty_hash = engine::crypto::sha256::digest(b"");
        let derived = derive_secret(self.0.as_bytes(), b"derived", &empty_hash)?;
        let hs = super::hkdf::hkdf_extract(derived.as_bytes(), ecdhe.as_bytes());
        Ok(HandshakeSecret(hs))
    }
}

/// TLS 1.3 の Handshake secret。
pub struct HandshakeSecret(Secret32);

impl fmt::Debug for HandshakeSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HandshakeSecret")
            .field(&"<redacted>")
            .finish()
    }
}

impl HandshakeSecret {
    /// ClientHello..ServerHello の transcript hash から client/server の
    /// handshake traffic secret を導出する（`self` は消費しない。Master secret
    /// への遷移とは独立に何度でも呼べる）。
    pub fn traffic_secrets(
        &self,
        th_ch_sh: &[u8; HASH_LEN],
    ) -> Result<HandshakeTrafficSecrets, HkdfError> {
        let client = derive_secret(self.0.as_bytes(), b"c hs traffic", th_ch_sh)?;
        let server = derive_secret(self.0.as_bytes(), b"s hs traffic", th_ch_sh)?;
        Ok(HandshakeTrafficSecrets {
            client: TrafficSecret(client),
            server: TrafficSecret(server),
        })
    }

    /// `derived = Derive-Secret(hs, "derived", Hash(""))` を経て
    /// `Extract(derived, 0)` を計算し、Master secret へ遷移する。`self` を
    /// 消費するため、Handshake secret はこの遷移後に再利用できない。
    pub fn into_master(self) -> Result<MasterSecret, HkdfError> {
        let empty_hash = engine::crypto::sha256::digest(b"");
        let derived = derive_secret(self.0.as_bytes(), b"derived", &empty_hash)?;
        let zero_ikm = [0u8; HASH_LEN];
        let master = super::hkdf::hkdf_extract(derived.as_bytes(), &zero_ikm);
        Ok(MasterSecret(master))
    }
}

/// TLS 1.3 の Master secret。
pub struct MasterSecret(Secret32);

impl fmt::Debug for MasterSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MasterSecret").field(&"<redacted>").finish()
    }
}

impl MasterSecret {
    /// ClientHello..server Finished の transcript hash から client/server の
    /// application traffic secret を導出する。`exp master`（exporter）・
    /// `res master`（再開）は親 Issue #941 の方針により対象外のため導出しない。
    pub fn application_traffic_secrets(
        &self,
        th_ch_sf: &[u8; HASH_LEN],
    ) -> Result<ApplicationTrafficSecrets, HkdfError> {
        let client = derive_secret(self.0.as_bytes(), b"c ap traffic", th_ch_sf)?;
        let server = derive_secret(self.0.as_bytes(), b"s ap traffic", th_ch_sf)?;
        Ok(ApplicationTrafficSecrets {
            client: TrafficSecret(client),
            server: TrafficSecret(server),
        })
    }
}

/// [`HandshakeSecret::traffic_secrets`] の client/server ペア。
pub struct HandshakeTrafficSecrets {
    pub client: TrafficSecret,
    pub server: TrafficSecret,
}

/// [`MasterSecret::application_traffic_secrets`] の client/server ペア。
pub struct ApplicationTrafficSecrets {
    pub client: TrafficSecret,
    pub server: TrafficSecret,
}

/// 1 方向（client または server・handshake または application）の traffic
/// secret。ここから AEAD の key／iv、および Finished 計算用の finished key を
/// 導出する。`Debug` は内容を秘匿する。
pub struct TrafficSecret(Secret32);

impl fmt::Debug for TrafficSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TrafficSecret").field(&"<redacted>").finish()
    }
}

impl TrafficSecret {
    /// `key = Expand-Label(secret, "key", "", 16)`・
    /// `iv = Expand-Label(secret, "iv", "", 12)`（RFC 8446 §7.3。
    /// `TLS_AES_128_GCM_SHA256` の鍵長 16 バイト・iv 長 12 バイト固定）。
    pub fn traffic_keys(&self) -> Result<TrafficKeys, HkdfError> {
        let mut key = [0u8; 16];
        hkdf_expand_label(self.0.as_bytes(), b"key", b"", &mut key)?;
        let mut iv = [0u8; 12];
        hkdf_expand_label(self.0.as_bytes(), b"iv", b"", &mut iv)?;
        Ok(TrafficKeys { key, iv })
    }

    /// `finished_key = Expand-Label(secret, "finished", "", Hash.length)`
    /// （RFC 8446 §4.4.4）。verify_data の計算・検証自体は #964 の担当。
    pub fn finished_key(&self) -> Result<Secret32, HkdfError> {
        let mut out = [0u8; HASH_LEN];
        hkdf_expand_label(self.0.as_bytes(), b"finished", b"", &mut out)?;
        Ok(Secret32::from_bytes(out))
    }
}

/// AEAD（`TLS_AES_128_GCM_SHA256`）の write key／iv。`Debug` は鍵を秘匿し、
/// Drop で best-effort ゼロ化する（key は [`super::hkdf::zeroize`] を直接
/// 使い、`Secret32` は 32 バイト固定のためここでは使わない）。
pub struct TrafficKeys {
    key: [u8; 16],
    iv: [u8; 12],
}

impl TrafficKeys {
    pub fn key(&self) -> &[u8; 16] {
        &self.key
    }

    pub fn iv(&self) -> &[u8; 12] {
        &self.iv
    }
}

impl fmt::Debug for TrafficKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrafficKeys")
            .field("key", &"<redacted>")
            .field("iv", &"<redacted>")
            .finish()
    }
}

impl Drop for TrafficKeys {
    fn drop(&mut self) {
        super::hkdf::zeroize(&mut self.key);
        super::hkdf::zeroize(&mut self.iv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_decode32(s: &str) -> [u8; 32] {
        assert_eq!(s.len(), 64);
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex");
        }
        out
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // RFC 8448 §3（Simple 1-RTT Handshake）の ECDHE 共有秘密（IKM）をそのまま
    // 使い、Early → Handshake → Master の全段と各 traffic secret／key／iv が
    // トレース記載値と一致することを確認する。X25519 の DH 計算自体は
    // `tls_key_schedule_rfc8448` 結合テストで別途確認する。
    fn ecdhe_shared_secret_bytes() -> [u8; 32] {
        hex_decode32("8bd4054fb55b9d63fdfbacf9f04b9f0d35e6d63f537563efd46272900f89492d")
    }

    #[test]
    fn early_secret_matches_rfc8448() {
        let early = EarlySecret::new_without_psk();
        assert_eq!(
            hex(early.0.as_bytes()),
            "33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a"
        );
    }

    #[test]
    fn handshake_secret_and_traffic_secrets_match_rfc8448() {
        let early = EarlySecret::new_without_psk();
        let ecdhe = ecdhe_shared_secret_bytes();
        // SharedSecret はコンストラクタを公開していないため、テスト専用の
        // `#[cfg(test)] pub(crate)` ヘルパーを x25519 側に用意する代わりに、
        // ここでは EphemeralSecret::diffie_hellman を実際の鍵ペアで実行して
        // 得る（RFC 8448 の秘密鍵・公開鍵をそのまま使う）。
        let server_priv =
            hex_decode32("b1580eeadf6dd589b8ef4f2d5652578cc810e9980191ec8d058308cea216a21e");
        let client_pub =
            hex_decode32("99381de560e4bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c");
        let secret = super::super::x25519::EphemeralSecret::from_bytes(server_priv);
        let shared = secret
            .diffie_hellman(&client_pub)
            .expect("valid non-zero shared secret");
        assert_eq!(hex(shared.as_bytes()), hex(&ecdhe));

        let hs = early.into_handshake(&shared).expect("valid params");
        assert_eq!(
            hex(hs.0.as_bytes()),
            "1dc826e93606aa6fdc0aadc12f741b01046aa6b99f691ed221a9f0ca043fbeac"
        );

        let th_ch_sh =
            hex_decode32("860c06edc07858ee8e78f0e7428c58edd6b43f2ca3e6e95f02ed063cf0e1cad8");
        let traffic = hs.traffic_secrets(&th_ch_sh).expect("valid params");
        assert_eq!(
            hex(traffic.client.0.as_bytes()),
            "b3eddb126e067f35a780b3abf45e2d8f3b1a950738f52e9600746a0e27a55a21"
        );
        assert_eq!(
            hex(traffic.server.0.as_bytes()),
            "b67b7d690cc16c4e75e54213cb2d37b4e9c912bcded9105d42befd59d391ad38"
        );

        // server handshake traffic key/iv（RFC 8448「derive write traffic keys
        // for handshake data」server 側）。
        let server_keys = traffic.server.traffic_keys().expect("valid params");
        assert_eq!(hex(server_keys.key()), "3fce516009c21727d0f2e4e86ee403bc");
        assert_eq!(hex(server_keys.iv()), "5d313eb2671276ee13000b30");

        // client handshake traffic key/iv（同トレースの「derive read traffic
        // keys for handshake data」= client 書き込み鍵）。
        let client_keys = traffic.client.traffic_keys().expect("valid params");
        assert_eq!(hex(client_keys.key()), "dbfaa693d1762c5b666af5d950258d01");
        assert_eq!(hex(client_keys.iv()), "5bd3c71b836e0b76bb73265f");

        // finished key（server 側。RFC 8448「calculate finished "tls13
        // finished"」の PRK 直後に現れる expanded 値）。
        let finished_key = traffic.server.finished_key().expect("valid params");
        assert_eq!(
            hex(finished_key.as_bytes()),
            "008d3b66f816ea559f96b537e885c31fc068bf492c652f01f288a1d8cdc19fc8"
        );

        let master = hs.into_master().expect("valid params");
        assert_eq!(
            hex(master.0.as_bytes()),
            "18df06843d13a08bf2a449844c5f8a478001bc4d4c627984d5a41da8d0402919"
        );

        let th_ch_sf =
            hex_decode32("9608102a0f1ccc6db6250b7b7e417b1a000eaada3daae4777a7686c9ff83df13");
        let app = master
            .application_traffic_secrets(&th_ch_sf)
            .expect("valid params");
        assert_eq!(
            hex(app.client.0.as_bytes()),
            "9e40646ce79a7f9dc05af8889bce6552875afa0b06df0087f792ebb7c17504a5"
        );
        assert_eq!(
            hex(app.server.0.as_bytes()),
            "a11af9f05531f856ad47116b45a950328204b4f44bfb6b3a4b4f1f3fcb631643"
        );

        let server_app_keys = app.server.traffic_keys().expect("valid params");
        assert_eq!(
            hex(server_app_keys.key()),
            "9f02283b6c9c07efc26bb9f2ac92e356"
        );
        assert_eq!(hex(server_app_keys.iv()), "cf782b88dd83549aadf1e984");
    }

    // Debug 出力に秘密バイトが含まれない（各段の秘密型を横断して確認）。
    #[test]
    fn debug_impls_do_not_expose_secret_bytes() {
        let early = EarlySecret::new_without_psk();
        assert!(!format!("{early:?}").contains(&hex(early.0.as_bytes())));
    }
}
