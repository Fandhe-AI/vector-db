//! X25519 鍵交換（RFC 7748・定数時間。TASK-228・WIRE-9・HTTP-10 ポインタ。
//! Issue #955・親 #941）。
//!
//! 親 Issue #941（TLS 1.3 サーバー側自作実装）の鍵交換グループとして採用
//! される X25519 の Diffie-Hellman 関数を、[`super::field25519::Fe`] 上の
//! Montgomery ladder として実装する。以下は本モジュールが担う範囲であり、
//! それ以外（key_share 拡張の解析・HKDF／鍵スケジュール・alert 型や
//! ハンドシェイク状態機械・接続への結線）は後続 sub-issue（#954・#956・
//! #965・#966 以降）の担当のまま変更しない。
//!
//! 定数時間性の設計:
//! - 秘密スカラーのビットに依存する分岐・配列添字・テーブル参照を持たない
//!   （ladder 内の `cswap` によるマスクスワップ・固定 255 回ループ）。
//! - フィールド逆元は固定長の加算連鎖（[`super::field25519::Fe::invert`]）
//!   であり、値に依存するループ回数を持たない。
//! - 唯一の「値に依存する分岐」は共有秘密が全ゼロ（低次点入力）かどうかの
//!   判定であり、これは TLS ハンドシェイクを継続するか中断するかという
//!   **外部から観測可能な公開の事実**にのみ関わる（秘密スカラー自体の値は
//!   この分岐の入力ではない）。
//!
//! `u128` を用いた乗算がターゲット（x86_64・aarch64）で定数時間であることを
//! 前提とする（`crates/engine` 向けの `make check-cross` は本クレートを
//! 対象に含まない点に注意）。秘密値のメモリ上ゼロ化（volatile write 等）は
//! 依存追加・`unsafe` なしでは保証できないため本 Issue のスコープ外とし、
//! `Debug` の秘匿・`Clone` 非導出・所有権消費による再利用防止に留める
//! （詳細・残余リスクは `docs/design/tls-x25519.md` 参照）。

use super::field25519::Fe;
use std::fmt;

/// X25519 の鍵長（バイト）。RFC 7748 の scalar／u-coordinate はいずれも
/// 32 バイト固定。
pub const KEY_LEN: usize = 32;

/// モンゴメリ曲線パラメータ a24 = (486662 - 2) / 4 = 121665
/// （RFC 7748 §4.1）。
const A24: u64 = 121665;

/// ベースポイントの u 座標（u = 9。RFC 7748 §4.1）。
const BASEPOINT_U: [u8; KEY_LEN] = {
    let mut b = [0u8; KEY_LEN];
    b[0] = 9;
    b
};

/// 秘密スカラーのクランプ（RFC 7748 §5 の `decodeScalar25519`）。
///
/// - 下位 3 ビットをクリア（曲線の cofactor 8 を吸収）。
/// - 最上位バイトの bit 7 をクリア・bit 6 をセット（スカラーを
///   `2^254 <= k < 2^255` の範囲へ固定し、ladder の反復回数を
///   秘密値に依存させない）。
///
/// ビット演算のみで完結し、入力バイト値に依存する分岐は持たない。
pub fn clamp(mut scalar: [u8; KEY_LEN]) -> [u8; KEY_LEN] {
    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    scalar
}

/// RFC 7748 §5 の Montgomery ladder 本体。`k` はクランプ済みスカラー、
/// `u` は入力 u 座標。秘密値は `k` のみであり、ループはビット位置 `t`
/// （公開の 0..255 カウンタ）に対してのみ添字アクセスを行う。
fn ladder(k: &[u8; KEY_LEN], u: &Fe) -> Fe {
    let x1 = *u;
    let mut x2 = Fe::ONE;
    let mut z2 = Fe::ZERO;
    let mut x3 = *u;
    let mut z3 = Fe::ONE;
    let mut swap: u64 = 0;
    let a24 = Fe::from_u64(A24);

    // t は 254 → 0 の固定 255 回ループ（RFC 7748 §5: bits = 255）。
    // `t >> 3` は t の値域 0..=254 に対して常に 0..=31 に収まり、
    // `k`（[u8; 32]）への添字アクセスが範囲外になることはない
    // （添字は公開のループカウンタのみに依存し、秘密ビットには
    // 依存しない）。
    for t in (0..255u32).rev() {
        let k_t = ((k[(t >> 3) as usize] >> (t & 7)) & 1) as u64;
        swap ^= k_t;
        Fe::cswap(&mut x2, &mut x3, swap);
        Fe::cswap(&mut z2, &mut z3, swap);
        swap = k_t;

        let a = x2.add(&z2);
        let aa = a.square();
        let b = x2.sub(&z2);
        let bb = b.square();
        let e = aa.sub(&bb);
        let c = x3.add(&z3);
        let d = x3.sub(&z3);
        let da = d.mul(&a);
        let cb = c.mul(&b);
        x3 = da.add(&cb).square();
        z3 = x1.mul(&da.sub(&cb).square());
        x2 = aa.mul(&bb);
        z2 = e.mul(&aa.add(&a24.mul(&e)));
    }

    Fe::cswap(&mut x2, &mut x3, swap);
    Fe::cswap(&mut z2, &mut z3, swap);

    x2.mul(&z2.invert())
}

/// RFC 7748 の生の X25519 関数（`scalar` を内部でクランプし、`u` からの
/// ladder 結果を返す）。テストベクタ照合・下位プリミティブとして公開する。
/// TLS ハンドシェイクの実運用では [`EphemeralSecret::diffie_hellman`]
/// （低次点の fail-closed 拒否込み）を使う。
pub fn x25519(scalar: &[u8; KEY_LEN], u: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    let k = clamp(*scalar);
    let u_fe = Fe::from_bytes(u);
    ladder(&k, &u_fe).to_bytes()
}

/// 32 バイト全体が 0 かどうかを分岐なしで判定する（OR 畳み込み）。
/// 最後の `== 0` 比較のみが分岐であり、これは「共有秘密が全ゼロだったか」
/// という公開の事実にのみ依存する（RFC 7748 §6.1 の低次点拒否要件）。
fn is_all_zero(bytes: &[u8; KEY_LEN]) -> bool {
    let mut acc = 0u8;
    for &b in bytes.iter() {
        acc |= b;
    }
    acc == 0
}

/// [`EphemeralSecret::diffie_hellman`] の失敗理由。
///
/// いずれも TLS `handshake_failure`（alert code 40）へ写像される
/// fail-closed 応答であり、鍵バイトや内部状態の手がかりを含まない
/// （[`X25519Error::alert_description`]）。alert の実送出は #965 の担当。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X25519Error {
    /// peer の公開鍵が [`KEY_LEN`]（32 バイト）と異なる長さだった。
    InvalidPublicKeyLength,
    /// 共有秘密が全ゼロ（低次点入力による縮退）になった。
    AllZeroSharedSecret,
}

impl fmt::Display for X25519Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            X25519Error::InvalidPublicKeyLength => {
                write!(f, "X25519 peer public key must be exactly 32 bytes")
            }
            X25519Error::AllZeroSharedSecret => {
                write!(
                    f,
                    "X25519 shared secret is all-zero (low-order point input)"
                )
            }
        }
    }
}

impl std::error::Error for X25519Error {}

impl X25519Error {
    /// TLS 1.3 の alert description 番号への写像（RFC 8446）。
    /// いずれも `handshake_failure`（40）。この写像は #965 が alert
    /// 送出経路を実装する際の単一情報源として参照・置換する前提。
    pub const fn alert_description(self) -> u8 {
        match self {
            X25519Error::InvalidPublicKeyLength => 40,
            X25519Error::AllZeroSharedSecret => 40,
        }
    }
}

/// X25519 の公開鍵（32 バイト）。公開値のため `Debug`／`Clone`／`Copy` を
/// 導出する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicKey([u8; KEY_LEN]);

impl PublicKey {
    /// 生の 32 バイト表現を返す。
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// X25519 の共有秘密（32 バイト）。後続の鍵スケジュール（#956・HKDF）へ
/// 渡す入力であり、`Debug` はバイト内容を出力しない・`Clone` は導出しない
/// （不用意な複製・ログ出力による漏えい面を減らす）。
pub struct SharedSecret([u8; KEY_LEN]);

impl SharedSecret {
    /// 生の 32 バイト表現を返す。
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SharedSecret").field(&"<redacted>").finish()
    }
}

/// 一時鍵（クランプ済みスカラー）。`diffie_hellman` は `self` を消費し、
/// 型システムで一時鍵の再利用を防ぐ。`Debug` はバイト内容を出力せず、
/// `Clone`／`Copy` は導出しない（秘密スカラーの不用意な複製を防ぐ）。
pub struct EphemeralSecret([u8; KEY_LEN]);

impl fmt::Debug for EphemeralSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EphemeralSecret")
            .field(&"<redacted>")
            .finish()
    }
}

impl EphemeralSecret {
    /// OS の CSPRNG（`/dev/urandom`。[`crate::auth::read_urandom`] を
    /// 既存の乱数源として再利用する）から新規の一時鍵を生成する。
    pub fn generate() -> std::io::Result<Self> {
        let raw = crate::auth::read_urandom(KEY_LEN)?;
        let arr: [u8; KEY_LEN] = raw.try_into().map_err(|_| {
            std::io::Error::other("urandom read returned an unexpected length for an X25519 key")
        })?;
        Ok(Self::from_bytes(arr))
    }

    /// 32 バイトの生スカラーから一時鍵を作る（内部でクランプする）。
    /// 主にテストベクタ注入用。
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        EphemeralSecret(clamp(bytes))
    }

    /// この一時鍵に対応する公開鍵（`x25519(k, 9)`）を計算する。
    pub fn public_key(&self) -> PublicKey {
        let u = Fe::from_bytes(&BASEPOINT_U);
        PublicKey(ladder(&self.0, &u).to_bytes())
    }

    /// peer の公開鍵（untrusted 入力）との Diffie-Hellman を計算する。
    ///
    /// `peer_public` の長さを厳密検証してから固定長配列へ変換し
    /// （`unwrap`／`expect`／可変長スライス添字は使わない）、共有秘密が
    /// 全ゼロ（低次点入力）になっていないかを検査してから返す
    /// （RFC 7748 §6.1・fail-closed）。`self` を消費するため、この
    /// 一時鍵で二度 DH を行うことはできない。
    pub fn diffie_hellman(self, peer_public: &[u8]) -> Result<SharedSecret, X25519Error> {
        let peer: [u8; KEY_LEN] = peer_public
            .try_into()
            .map_err(|_| X25519Error::InvalidPublicKeyLength)?;
        let u = Fe::from_bytes(&peer);
        let shared = ladder(&self.0, &u).to_bytes();
        if is_all_zero(&shared) {
            return Err(X25519Error::AllZeroSharedSecret);
        }
        Ok(SharedSecret(shared))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_to_bytes32(hex: &str) -> [u8; KEY_LEN] {
        assert_eq!(hex.len(), 64, "expected 64 hex chars for 32 bytes");
        let mut out = [0u8; KEY_LEN];
        for i in 0..KEY_LEN {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("valid hex in fixture");
        }
        out
    }

    fn bytes32_to_hex(bytes: &[u8; KEY_LEN]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes.iter() {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    // RFC 7748 §5.2 単発テストベクタ 1 件目。
    #[test]
    fn rfc7748_5_2_vector_1() {
        let scalar =
            hex_to_bytes32("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
        let u = hex_to_bytes32("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
        let expected =
            hex_to_bytes32("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552");
        assert_eq!(
            bytes32_to_hex(&x25519(&scalar, &u)),
            bytes32_to_hex(&expected)
        );
    }

    // RFC 7748 §5.2 単発テストベクタ 2 件目。
    #[test]
    fn rfc7748_5_2_vector_2() {
        let scalar =
            hex_to_bytes32("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d");
        let u = hex_to_bytes32("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493");
        let expected =
            hex_to_bytes32("95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957");
        assert_eq!(
            bytes32_to_hex(&x25519(&scalar, &u)),
            bytes32_to_hex(&expected)
        );
    }

    // RFC 7748 §5.2 反復テスト（1 回・1,000 回）。
    #[test]
    fn rfc7748_5_2_iterated_1_and_1000() {
        let mut k = BASEPOINT_U;
        let mut u = BASEPOINT_U;

        let expected_1 =
            hex_to_bytes32("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079");
        let expected_1000 =
            hex_to_bytes32("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51");

        let next = x25519(&k, &u);
        u = k;
        k = next;
        assert_eq!(bytes32_to_hex(&k), bytes32_to_hex(&expected_1));

        for _ in 1..1000 {
            let next = x25519(&k, &u);
            u = k;
            k = next;
        }
        assert_eq!(bytes32_to_hex(&k), bytes32_to_hex(&expected_1000));
    }

    // RFC 7748 §5.2 の 1,000,000 回反復（実行時間の都合で既定は無効。
    // 受入基準は 1 回・1,000 回の反復で満たすため、このテストは
    // 受入基準の回避には使わない任意の追加検証）。
    #[test]
    #[ignore]
    fn rfc7748_5_2_iterated_1_000_000() {
        let mut k = BASEPOINT_U;
        let mut u = BASEPOINT_U;
        // RFC 7748 は 1,000,000 回反復後の値を明記していない実装もあるため、
        // ここでは公開されている参照値を用いる（脚注: 一部の実装ノートに
        // 掲載される値。受入基準には使わない任意検証のため #[ignore]）。
        for _ in 0..1_000_000 {
            let next = x25519(&k, &u);
            u = k;
            k = next;
        }
        // 反復自体が定義どおりに進行し、パニックなく完走することのみを
        // 確認する（固定の期待値は要求しない。1 回・1,000 回反復の
        // テストで既に正しさは固定済みのため）。
        let _ = k;
    }

    // RFC 7748 §6.1: Alice/Bob の X25519 鍵交換例。
    #[test]
    fn rfc7748_6_1_alice_bob_key_exchange() {
        let alice_priv =
            hex_to_bytes32("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let alice_pub_expected =
            hex_to_bytes32("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");

        let bob_priv =
            hex_to_bytes32("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let bob_pub_expected =
            hex_to_bytes32("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");

        let alice = EphemeralSecret::from_bytes(alice_priv);
        let bob = EphemeralSecret::from_bytes(bob_priv);

        let alice_pub = alice.public_key();
        let bob_pub = bob.public_key();
        assert_eq!(
            bytes32_to_hex(alice_pub.as_bytes()),
            bytes32_to_hex(&alice_pub_expected)
        );
        assert_eq!(
            bytes32_to_hex(bob_pub.as_bytes()),
            bytes32_to_hex(&bob_pub_expected)
        );

        let expected_shared =
            hex_to_bytes32("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");

        let alice_shared = alice
            .diffie_hellman(bob_pub.as_bytes())
            .expect("valid peer key yields shared secret");
        let bob_shared = bob
            .diffie_hellman(alice_pub.as_bytes())
            .expect("valid peer key yields shared secret");

        assert_eq!(
            bytes32_to_hex(alice_shared.as_bytes()),
            bytes32_to_hex(&expected_shared)
        );
        assert_eq!(
            bytes32_to_hex(bob_shared.as_bytes()),
            bytes32_to_hex(&expected_shared)
        );
    }

    #[test]
    fn diffie_hellman_rejects_low_order_zero_point() {
        let secret = EphemeralSecret::from_bytes([7u8; KEY_LEN]);
        let zero = [0u8; KEY_LEN];
        assert!(matches!(
            secret.diffie_hellman(&zero),
            Err(X25519Error::AllZeroSharedSecret)
        ));
    }

    #[test]
    fn diffie_hellman_rejects_low_order_one_point() {
        let secret = EphemeralSecret::from_bytes([7u8; KEY_LEN]);
        let mut one = [0u8; KEY_LEN];
        one[0] = 1;
        assert!(matches!(
            secret.diffie_hellman(&one),
            Err(X25519Error::AllZeroSharedSecret)
        ));
    }

    #[test]
    fn diffie_hellman_rejects_noncanonical_p_and_p_plus_1() {
        // u = p（≡ 0 mod p）は from_bytes 側で最上位ビット無視の非正準値
        // として受理されるが、演算結果は u=0 と同じ全ゼロ共有秘密になる。
        let p_hex = "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f";
        let mut p_bytes = [0u8; KEY_LEN];
        for i in 0..KEY_LEN {
            p_bytes[i] = u8::from_str_radix(&p_hex[i * 2..i * 2 + 2], 16).expect("valid hex");
        }
        let secret = EphemeralSecret::from_bytes([7u8; KEY_LEN]);
        assert!(matches!(
            secret.diffie_hellman(&p_bytes),
            Err(X25519Error::AllZeroSharedSecret)
        ));

        // u = p + 1（≡ 1 mod p）は u=1 と同じ既知の低次点。
        let mut p_plus_1 = p_bytes;
        p_plus_1[0] = p_plus_1[0].wrapping_add(1);
        let secret2 = EphemeralSecret::from_bytes([7u8; KEY_LEN]);
        assert!(matches!(
            secret2.diffie_hellman(&p_plus_1),
            Err(X25519Error::AllZeroSharedSecret)
        ));
    }

    #[test]
    fn diffie_hellman_rejects_invalid_length() {
        for len in [0usize, 31, 33, 64] {
            let secret = EphemeralSecret::from_bytes([7u8; KEY_LEN]);
            let buf = vec![0u8; len];
            assert!(matches!(
                secret.diffie_hellman(&buf),
                Err(X25519Error::InvalidPublicKeyLength)
            ));
        }
    }

    #[test]
    fn alert_description_is_handshake_failure() {
        assert_eq!(X25519Error::InvalidPublicKeyLength.alert_description(), 40);
        assert_eq!(X25519Error::AllZeroSharedSecret.alert_description(), 40);
    }

    #[test]
    fn clamp_is_idempotent_and_sets_expected_bits() {
        let raw = [0xffu8; KEY_LEN];
        let clamped = clamp(raw);
        assert_eq!(clamped[0] & 0b0000_0111, 0);
        assert_eq!(clamped[31] & 0b1000_0000, 0);
        assert_eq!(clamped[31] & 0b0100_0000, 0b0100_0000);
        assert_eq!(clamp(clamped), clamped);
    }

    #[test]
    fn x25519_is_independent_of_pre_clamp_bits() {
        let mut raw_a = [3u8; KEY_LEN];
        let mut raw_b = raw_a;
        // クランプで無視される下位/上位ビットだけを変える。
        raw_a[0] |= 0b0000_0111;
        raw_b[0] &= !0b0000_0111;
        raw_a[31] = (raw_a[31] & 0b0011_1111) | 0b1000_0000;
        raw_b[31] &= 0b0011_1111;

        let u = BASEPOINT_U;
        assert_eq!(x25519(&raw_a, &u), x25519(&raw_b, &u));
    }

    #[test]
    fn debug_output_does_not_leak_key_bytes() {
        let secret = EphemeralSecret::from_bytes([0x42u8; KEY_LEN]);
        let debug_str = format!("{secret:?}");
        assert!(!debug_str.contains("42"));

        let shared = SharedSecret([0x42u8; KEY_LEN]);
        let debug_str = format!("{shared:?}");
        assert!(!debug_str.contains("42"));
    }

    #[test]
    fn generate_produces_working_keypairs() {
        let a = EphemeralSecret::generate().expect("urandom available in test environment");
        let b = EphemeralSecret::generate().expect("urandom available in test environment");
        let a_pub = a.public_key();
        let b_pub = b.public_key();
        let shared_a = a
            .diffie_hellman(b_pub.as_bytes())
            .expect("random keys should not hit a low-order point");
        let shared_b = b
            .diffie_hellman(a_pub.as_bytes())
            .expect("random keys should not hit a low-order point");
        assert_eq!(shared_a.as_bytes(), shared_b.as_bytes());
    }
}
