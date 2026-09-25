//! TLS 1.3 の `CertificateVerify`（RFC 8446 §4.4.3。TASK-228・WIRE-9・
//! HTTP-10 ポインタ。Issue #961・親 #941）。
//!
//! [`super::handshake::CertificateVerify`]（parse/serialize のみ担う不透明
//! 型。Issue #953）に対し、本モジュールは署名対象バイト列の組み立てと、
//! [`super::ed25519`] を使った生成・検証を提供する。[`super::finished`]
//! （Finished の生成・検証）と同じ役割分担で、独立モジュールとして置く。
//!
//! **署名対象**（RFC 8446 §4.4.3）: 0x20 を 64 個 ‖ コンテキスト文字列
//! ‖ 0x00 ‖ transcript hash。サーバー側のコンテキスト文字列のみを扱う
//! （クライアント証明書は親 Issue #941 の対象外）。`transcript_hash` は
//! 呼び出し元（#965）が [`super::transcript::Transcript::
//! hash_through_certificate`] の戻り値をそのまま渡す契約であり、本モジュール
//! は [`super::transcript::Transcript`] 自体には依存しない（[`super::finished`]
//! と同じ設計判断）。
//!
//! **対象外**: alert の実送出・ハンドシェイク状態機械（#965 の担当）。
//! クライアント側のコンテキスト文字列・クライアント証明書（親 #941 の
//! 対象外）。

use super::client_hello::SIG_ED25519;
use super::ed25519::{self, Ed25519Error, SigningKey};
use super::handshake::{self, CertificateVerify};
use super::record::AlertDescription;
use std::fmt;

/// サーバー側 `CertificateVerify` の署名対象コンテキスト文字列
/// （RFC 8446 §4.4.3）。
pub const SERVER_CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify";

/// transcript hash の長さ（SHA-256。TLS の暗号スイートが
/// `TLS_AES_128_GCM_SHA256` 固定であるため）。
const TRANSCRIPT_HASH_LEN: usize = 32;

/// 署名対象バイト列の長さ（0x20 * 64 ‖ コンテキスト ‖ 0x00 ‖ hash）。
pub const SIGNED_CONTENT_LEN: usize = 64 + SERVER_CONTEXT.len() + 1 + TRANSCRIPT_HASH_LEN;

/// サーバー側 `CertificateVerify` の署名対象バイト列を組み立てる
/// （RFC 8446 §4.4.3）。`transcript_hash` は
/// [`super::transcript::Transcript::hash_through_certificate`] の戻り値
/// （ClientHello..Certificate の累積 SHA-256）を呼び出し元が渡す契約。
pub fn server_signed_content(
    transcript_hash: &[u8; TRANSCRIPT_HASH_LEN],
) -> [u8; SIGNED_CONTENT_LEN] {
    let mut out = [0u8; SIGNED_CONTENT_LEN];
    // 各区間はコンパイル時に長さが確定する定数オフセットであり、値には
    // 依存しない（[`super::field25519`] の固定オフセット読み書きと同じ方針）。
    out[..64].fill(0x20);
    out[64..64 + SERVER_CONTEXT.len()].copy_from_slice(SERVER_CONTEXT);
    out[64 + SERVER_CONTEXT.len()] = 0x00;
    out[64 + SERVER_CONTEXT.len() + 1..].copy_from_slice(transcript_hash);
    out
}

/// サーバー側 `CertificateVerify` を構成する（送信側）。`key` は
/// [`super::x509::ServerCertificateChain::from_der_chain`] の
/// `expected_leaf_public_key` と対をなす署名鍵、`transcript_hash` は
/// [`server_signed_content`] のドキュメンテーションコメント参照。
pub fn build_server_certificate_verify(
    key: &SigningKey,
    transcript_hash: &[u8; TRANSCRIPT_HASH_LEN],
) -> CertificateVerify {
    let content = server_signed_content(transcript_hash);
    CertificateVerify {
        algorithm: SIG_ED25519,
        signature: key.sign(&content).to_vec(),
    }
}

/// サーバー側 `CertificateVerify` を検証する（受信側。自己整合性テスト用。
/// production 経路での呼び出し元は無い。サーバーはクライアント証明書を
/// 扱わないため）。曲線演算より前にアルゴリズム不一致・署名長不正を拒否する
/// （untrusted 入力に対する fail-closed）。
pub fn verify_server_certificate_verify(
    public_key: &[u8; 32],
    transcript_hash: &[u8; TRANSCRIPT_HASH_LEN],
    cv: &handshake::CertificateVerify,
) -> Result<(), CertificateVerifyError> {
    if cv.algorithm != SIG_ED25519 {
        return Err(CertificateVerifyError::UnsupportedAlgorithm);
    }
    if cv.signature.len() != 64 {
        return Err(CertificateVerifyError::InvalidSignatureLength);
    }
    let content = server_signed_content(transcript_hash);
    ed25519::verify(public_key, &content, &cv.signature).map_err(CertificateVerifyError::Ed25519)
}

/// `CertificateVerify` の生成・検証で起こり得る失敗。署名バイト列・
/// transcript hash は一切保持しない（ログ・エラー経由の漏えい防止）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateVerifyError {
    /// `signature_algorithms` が Ed25519（0x0807）以外
    /// （RFC 8446 §4.2.3。本サーバーは Ed25519 のみを受理する）。
    UnsupportedAlgorithm,
    /// 署名長が 64 バイトでない。
    InvalidSignatureLength,
    /// [`super::ed25519`] の検証失敗（詳細は [`Ed25519Error`] 参照）。
    Ed25519(Ed25519Error),
}

impl CertificateVerifyError {
    /// alert の実送出は #965 の担当。アルゴリズム不一致は
    /// `illegal_parameter`（RFC 8446 §4.2.3 の意味検査違反と同種）、
    /// 署名長不正・検証失敗は [`Ed25519Error::alert_description`] と同じ
    /// 分類（`decode_error`／`decrypt_error`）へ写像する。
    pub fn alert_description(&self) -> AlertDescription {
        match self {
            CertificateVerifyError::UnsupportedAlgorithm => AlertDescription::IllegalParameter,
            CertificateVerifyError::InvalidSignatureLength => AlertDescription::DecodeError,
            CertificateVerifyError::Ed25519(e) => e.alert_description(),
        }
    }
}

impl fmt::Display for CertificateVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CertificateVerifyError::UnsupportedAlgorithm => {
                write!(f, "CertificateVerify.algorithm is not Ed25519")
            }
            CertificateVerifyError::InvalidSignatureLength => {
                write!(f, "CertificateVerify.signature must be 64 bytes")
            }
            CertificateVerifyError::Ed25519(e) => write!(f, "Ed25519 verification failed: {e}"),
        }
    }
}

impl std::error::Error for CertificateVerifyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_for_test() -> SigningKey {
        SigningKey::from_seed_bytes([0x42u8; 32])
    }

    #[test]
    fn server_signed_content_has_expected_layout() {
        let hash = [0x11u8; TRANSCRIPT_HASH_LEN];
        let content = server_signed_content(&hash);
        assert_eq!(content.len(), SIGNED_CONTENT_LEN);
        assert!(content[..64].iter().all(|&b| b == 0x20));
        assert_eq!(&content[64..64 + SERVER_CONTEXT.len()], SERVER_CONTEXT);
        assert_eq!(content[64 + SERVER_CONTEXT.len()], 0x00);
        assert_eq!(&content[64 + SERVER_CONTEXT.len() + 1..], &hash);
    }

    #[test]
    fn build_and_verify_round_trip() {
        let key = key_for_test();
        let hash = [0x22u8; TRANSCRIPT_HASH_LEN];
        let cv = build_server_certificate_verify(&key, &hash);
        assert_eq!(cv.algorithm, SIG_ED25519);
        assert_eq!(cv.signature.len(), 64);
        assert!(verify_server_certificate_verify(&key.public_key(), &hash, &cv).is_ok());
    }

    #[test]
    fn signature_matches_direct_ed25519_sign() {
        let key = key_for_test();
        let hash = [0x33u8; TRANSCRIPT_HASH_LEN];
        let cv = build_server_certificate_verify(&key, &hash);
        let content = server_signed_content(&hash);
        assert_eq!(cv.signature, key.sign(&content).to_vec());
    }

    #[test]
    fn bit_flipped_hash_is_rejected() {
        let key = key_for_test();
        let hash = [0x44u8; TRANSCRIPT_HASH_LEN];
        let cv = build_server_certificate_verify(&key, &hash);
        let mut wrong_hash = hash;
        wrong_hash[0] ^= 0x01;
        assert!(verify_server_certificate_verify(&key.public_key(), &wrong_hash, &cv).is_err());
    }

    #[test]
    fn wrong_algorithm_is_rejected_before_curve_operations() {
        let key = key_for_test();
        let hash = [0x55u8; TRANSCRIPT_HASH_LEN];
        let mut cv = build_server_certificate_verify(&key, &hash);
        cv.algorithm = 0x0804; // rsa_pss_rsae_sha256（TLS 1.3 が定義する別コード）
        assert_eq!(
            verify_server_certificate_verify(&key.public_key(), &hash, &cv),
            Err(CertificateVerifyError::UnsupportedAlgorithm)
        );
    }

    #[test]
    fn wrong_signature_length_is_rejected() {
        let key = key_for_test();
        let hash = [0x66u8; TRANSCRIPT_HASH_LEN];
        let mut cv = build_server_certificate_verify(&key, &hash);
        cv.signature.push(0);
        assert_eq!(
            verify_server_certificate_verify(&key.public_key(), &hash, &cv),
            Err(CertificateVerifyError::InvalidSignatureLength)
        );
    }

    #[test]
    fn alert_descriptions_map_as_expected() {
        assert_eq!(
            CertificateVerifyError::UnsupportedAlgorithm.alert_description(),
            AlertDescription::IllegalParameter
        );
        assert_eq!(
            CertificateVerifyError::InvalidSignatureLength.alert_description(),
            AlertDescription::DecodeError
        );
        assert_eq!(
            CertificateVerifyError::Ed25519(Ed25519Error::VerificationFailed).alert_description(),
            AlertDescription::DecryptError
        );
    }

    #[test]
    fn error_display_does_not_expose_secret_bytes() {
        let err = CertificateVerifyError::Ed25519(Ed25519Error::VerificationFailed);
        let display = format!("{err}");
        assert!(!display.contains("0x"));
    }
}
