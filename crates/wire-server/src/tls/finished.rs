//! TLS 1.3 の Finished（RFC 8446 §4.4.4。TASK-228・WIRE-9・HTTP-10 ポインタ。
//! Issue #964・親 #941）。
//!
//! `verify_data = HMAC(finished_key, Transcript-Hash(Handshake Context))` の
//! 計算・検証本体。`finished_key` の導出自体は
//! [`super::key_schedule::TrafficSecret::finished_key`] が担い、
//! transcript hash（引数の `[u8; 32]`）は [`super::transcript::Transcript`]
//! の名前付きチェックポイントから得る契約（呼び出し元がどちらを渡すかを
//! 誤らないよう、本モジュールは `Transcript` 自体には依存しない）。
//!
//! 秘密値（finished key・期待 verify_data）を扱うコードを [`super::transcript`]
//! と視覚的に分離するため、独立モジュールとして置く。
//!
//! **定数時間方針**: client Finished の検証は [`super::hkdf::ct_eq`]
//! （GCM タグ検証・本モジュールが共有する唯一の定数時間比較）のみで行い、
//! 長さ・一致有無以外の分岐を受信値のビットに依存させない。
//!
//! **対象外**: alert の実送出・ハンドシェイク状態機械（#965 の担当）。
//! 本モジュールは失敗理由を [`FinishedError`] へ写像するところまでを担う。

use super::handshake;
use super::hkdf::{ct_eq, hmac_sha256, HkdfError, Secret32};
use super::key_schedule::TrafficSecret;
use super::record::AlertDescription;
use std::fmt;

/// `verify_data = HMAC(finished_key, transcript_hash)`（RFC 8446 §4.4.4）。
/// 結果は比較が終わるまで秘密扱いのため [`Secret32`] で返す（Drop で
/// best-effort ゼロ化。[`super::hkdf`] のドキュメンテーションコメント参照）。
pub fn compute_verify_data(finished_key: &Secret32, transcript_hash: &[u8; 32]) -> Secret32 {
    let mac = hmac_sha256(finished_key.as_bytes(), &[transcript_hash]);
    Secret32::from_bytes(mac)
}

/// server Finished を構成する（送信側）。`server_hs_traffic` は
/// `HandshakeSecret::traffic_secrets` の `server` フィールド、
/// `th_ch_cv` は `Transcript::hash_through_certificate_verify()` の戻り値。
pub fn build_server_finished(
    server_hs_traffic: &TrafficSecret,
    th_ch_cv: &[u8; 32],
) -> Result<handshake::Finished, FinishedError> {
    let key = server_hs_traffic
        .finished_key()
        .map_err(FinishedError::Hkdf)?;
    let verify_data = compute_verify_data(&key, th_ch_cv);
    Ok(handshake::Finished {
        verify_data: verify_data.as_bytes().to_vec(),
    })
}

/// client Finished を検証する（受信側）。`client_hs_traffic` は
/// `HandshakeSecret::traffic_secrets` の `client` フィールド、`th_ch_sf` は
/// `Transcript::hash_through_server_finished()` の戻り値。不一致・長さ違いは
/// いずれも [`FinishedError::VerifyDataMismatch`]（[`ct_eq`] は長さ不一致でも
/// 早期 return せず `false` を返すため、分岐は最終判定の 1 箇所のみ）。
pub fn verify_client_finished(
    client_hs_traffic: &TrafficSecret,
    th_ch_sf: &[u8; 32],
    received: &handshake::Finished,
) -> Result<(), FinishedError> {
    let key = client_hs_traffic
        .finished_key()
        .map_err(FinishedError::Hkdf)?;
    let expected = compute_verify_data(&key, th_ch_sf);
    if ct_eq(expected.as_bytes(), &received.verify_data) {
        Ok(())
    } else {
        Err(FinishedError::VerifyDataMismatch)
    }
}

/// Finished の生成・検証で起こり得る失敗。受信値・期待値・鍵のバイト列は
/// 一切保持しない（ログ・エラー経由の漏えい防止。`Display` も固定文字列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishedError {
    /// client Finished の verify_data が期待値と一致しない
    /// （`decrypt_error`。RFC 8446 §4.4.4 の実装注記どおり、TLS 1.3 では
    /// bad_record_mac ではなく decrypt_error を用いる）。
    VerifyDataMismatch,
    /// `finished_key` 導出（HKDF-Expand-Label）の呼び出し契約違反
    /// （内部起因。`internal_error`）。
    Hkdf(HkdfError),
}

impl FinishedError {
    /// alert の実送出は #965 の担当。本モジュールは定型コードへの写像のみ
    /// 提供する（`handshake::HandshakeError::alert_description` と同じ流儀）。
    pub fn alert_description(&self) -> AlertDescription {
        match self {
            FinishedError::VerifyDataMismatch => AlertDescription::DecryptError,
            FinishedError::Hkdf(_) => AlertDescription::InternalError,
        }
    }
}

impl fmt::Display for FinishedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FinishedError::VerifyDataMismatch => write!(f, "Finished.verify_data mismatch"),
            FinishedError::Hkdf(e) => write!(f, "finished key derivation failed: {e}"),
        }
    }
}

impl std::error::Error for FinishedError {}

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

    // RFC 8448 §3（Simple 1-RTT Handshake。IETF の公開文書由来）の
    // server finished key（`key_schedule.rs` のテストが `TrafficSecret::
    // finished_key` 経由で導出済み値と確認している）と、
    // th_ch_cv（ClientHello..CertificateVerify の transcript hash）。
    // th_ch_cv は RFC 8448 に直接のラベルが無いため、本体バイト列から
    // 独立に SHA-256 で算出した値（Transcript のインクリメンタル計算との
    // 一致は `tls_transcript_finished_rfc8448` 結合テストが検証する）。
    const SERVER_FINISHED_KEY_1RTT: &str =
        "008d3b66f816ea559f96b537e885c31fc068bf492c652f01f288a1d8cdc19fc8";
    const TH_CH_CV_1RTT: &str = "edb7725fa7a3473b031ec8ef65a2485493900138a2b91291407d7951a06110ed";
    const SERVER_FINISHED_VERIFY_DATA_1RTT: &str =
        "9b9b141d906337fbd2cbdce71df4deda4ab42c309572cb7fffee5454b78f0718";

    const CLIENT_FINISHED_KEY_1RTT: &str =
        "b80ad01015fb2f0bd65ff7d4da5d6bf83f84821d1f87fdc7d3c75b5a7b42d9c4";
    const TH_CH_SF_1RTT: &str = "9608102a0f1ccc6db6250b7b7e417b1a000eaada3daae4777a7686c9ff83df13";
    const CLIENT_FINISHED_VERIFY_DATA_1RTT: &str =
        "a8ec436d677634ae525ac1fcebe11a039ec17694fac6e98527b642f2edd5ce61";

    #[test]
    fn compute_verify_data_matches_rfc8448_server_finished() {
        let finished_key = Secret32::from_bytes(hex_decode32(SERVER_FINISHED_KEY_1RTT));
        let th_ch_cv = hex_decode32(TH_CH_CV_1RTT);
        let verify_data = compute_verify_data(&finished_key, &th_ch_cv);
        assert_eq!(
            hex(verify_data.as_bytes()),
            SERVER_FINISHED_VERIFY_DATA_1RTT
        );
    }

    #[test]
    fn compute_verify_data_matches_rfc8448_client_finished() {
        let finished_key = Secret32::from_bytes(hex_decode32(CLIENT_FINISHED_KEY_1RTT));
        let th_ch_sf = hex_decode32(TH_CH_SF_1RTT);
        let verify_data = compute_verify_data(&finished_key, &th_ch_sf);
        assert_eq!(
            hex(verify_data.as_bytes()),
            CLIENT_FINISHED_VERIFY_DATA_1RTT
        );
    }

    #[test]
    fn verify_data_mismatch_maps_to_decrypt_error() {
        let err = FinishedError::VerifyDataMismatch;
        assert_eq!(err.alert_description(), AlertDescription::DecryptError);
        assert_eq!(err.alert_description().as_u8(), 51);
    }

    #[test]
    fn hkdf_error_maps_to_internal_error() {
        let err = FinishedError::Hkdf(HkdfError::OutputTooLong);
        assert_eq!(err.alert_description(), AlertDescription::InternalError);
    }

    // 1 ビット反転・全ゼロ・長さ違いの verify_data はいずれも不一致
    // （ct_eq のみで判定し、早期 return しない設計の外形テスト。
    // `verify_client_finished` 自体の TrafficSecret 経由の結合は
    // `tls_transcript_finished_rfc8448` 結合テストが担う）。
    #[test]
    fn bit_flipped_or_wrong_length_verify_data_is_rejected() {
        let finished_key = Secret32::from_bytes([0x11u8; 32]);
        let th = [0x22u8; 32];
        let expected = compute_verify_data(&finished_key, &th);

        let mut flipped = *expected.as_bytes();
        flipped[0] ^= 0x01;
        assert!(!ct_eq(expected.as_bytes(), &flipped));

        let all_zero = [0u8; 32];
        assert!(!ct_eq(expected.as_bytes(), &all_zero));

        let too_short = vec![0u8; 31];
        assert!(!ct_eq(expected.as_bytes(), &too_short));
    }

    // `Debug`／`Display` に秘密バイト（finished key・期待 verify_data）の
    // 16 進表現が含まれないことを確認する。
    #[test]
    fn error_display_does_not_expose_secret_bytes() {
        let err = FinishedError::VerifyDataMismatch;
        let display = format!("{err}");
        assert_eq!(display, "Finished.verify_data mismatch");
        assert!(!display.contains("9b9b141d"));
    }
}
