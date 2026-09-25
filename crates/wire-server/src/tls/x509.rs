//! X.509 証明書（RFC 5280 §4.1）の最小パースと、TLS 1.3 `Certificate`
//! ハンドシェイクメッセージ（RFC 8446 §4.4.2）の組み立て
//! （TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #963・親 #941）。
//!
//! [`super::pem::load_certificate_chain_file`]（Issue #962）が返す DER 列を
//! 起動時に最小限パースして自己整合性（validity・葉の SPKI 公開鍵）を
//! 検査し、[`super::handshake::Certificate`]（Issue #953）へ組み立てる。
//! 証明書チェーンを実際にハンドシェイクへ載せる状態機械（#965）・接続への
//! 結線（#966 以降）・CLI からの起動時呼び出し（#967）はいずれも本モジュール
//! の対象外である。
//!
//! ## 秘密鍵側の公開鍵との照合について（接続点・seam）
//!
//! Ed25519 の秘密鍵 seed から公開鍵を導出する処理（SHA-512 とスカラー倍算）
//! は本モジュールの対象外であり、#961 が担う。本モジュールは
//! 「導出済みの期待公開鍵」を `&[u8; 32]` として引数で受け取り、葉証明書の
//! SPKI 公開鍵と照合するところまでを担う。実際の起動時失敗化（鍵ファイル
//! 読み込み → 公開鍵導出 → 本モジュールでの照合 → 不一致ならプロセスを
//! 起動失敗させる、という一連の組み立て）は #967 の担当である。
//!
//! ## スコープ外
//!
//! - 証明書署名そのものの検証（発行者鍵の検証はクライアントの責務であり、
//!   本サーバーが自己整合性チェックとして検証するのは #961 の
//!   `CertificateVerify` 用の鍵一致のみ）
//! - SAN・ホスト名・keyUsage・basicConstraints 等 extensions の意味解釈
//!   （[`super::der::validate_structure`] による構造検証のみ行い、中身は
//!   スキップする）
//! - 中間証明書どうしの issuer/subject 連結検査・パス構築（RFC 8446
//!   §4.4.2 は後続証明書の順序を SHOULD とするに留まるため、本モジュールは
//!   チェーン先頭が葉であることのみを前提にする）
//!
//! ## パース手順（拒否理由が決定的に決まるよう、この順序で判定する）
//!
//! 1. 証明書 DER 長の上限検査（[`MAX_CERTIFICATE_DER_LEN`]）。確保より前に
//!    判定する
//! 2. [`super::der::validate_structure`] による TLV 全体の整形式・入れ子
//!    深さ上限の検証
//! 3. `Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
//!    signatureValue BIT STRING }`（後続バイトは拒否）
//! 4. `tbsCertificate` の各フィールドを順に読む（version → serialNumber →
//!    signature → issuer → validity → subject → subjectPublicKeyInfo →
//!    任意の unique ID／extensions）
//! 5. `tbsCertificate.signature` と外側 `signatureAlgorithm` の DER
//!    バイト列一致（RFC 5280 §4.1.1.2）
//! 6. `signatureValue` BIT STRING の形状検査（空でない・未使用ビット数
//!    0〜7）
//!
//! validity（notBefore／notAfter）はチェーン内の**全証明書**に対して
//! 現在時刻（呼び出し元が注入する `now_unix_secs`）で検査する。葉の SPKI
//! 公開鍵照合はチェーンの**先頭のみ**に適用する。
//!
//! ## 定数時間性
//!
//! 証明書の内容は公開データであり秘密ではないため、DER 構造・時刻値・
//! アルゴリズム OID による分岐は問題ない。唯一の秘密由来の値は期待公開鍵
//! （導出元は秘密鍵だが公開鍵自体は公開値）であり、その比較のみ
//! [`super::hkdf::ct_eq`]（定数時間比較）を使う。

use std::fmt;
use std::path::Path;

use super::der::{self, DerReader};
use super::handshake::{Certificate, CertificateEntry};
use super::hkdf::ct_eq;
use super::pem;
use super::pkcs8::KeyAlgorithm;

/// 証明書 1 個の DER 長の上限（本リポの実装既定値）。ファイル全体の上限は
/// [`super::pem::MAX_CERTIFICATE_FILE_LEN`] が別途課すが、こちらは
/// パース対象 1 個ごとの上限として、値を確保する前に判定する。
pub const MAX_CERTIFICATE_DER_LEN: usize = 64 * 1024;

const TAG_SEQUENCE: u8 = 0x30;
const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OID: u8 = 0x06;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;
/// `version [0] EXPLICIT`（constructed・context-specific・number 0）。
const TAG_VERSION_EXPLICIT: u8 = 0xa0;
/// `issuerUniqueID [1] IMPLICIT`（BIT STRING は primitive のため
/// constructed ビットは立たない）。
const TAG_ISSUER_UNIQUE_ID_IMPLICIT: u8 = 0x81;
/// `subjectUniqueID [2] IMPLICIT`。
const TAG_SUBJECT_UNIQUE_ID_IMPLICIT: u8 = 0x82;
/// `extensions [3] EXPLICIT`（constructed）。
const TAG_EXTENSIONS_EXPLICIT: u8 = 0xa3;

/// Ed25519（1.3.101.112。RFC 8410 §3）。
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
/// Ed448（1.3.101.113）。
const OID_ED448: &[u8] = &[0x2b, 0x65, 0x71];
/// X25519（1.3.101.110）。
const OID_X25519: &[u8] = &[0x2b, 0x65, 0x6e];
/// rsaEncryption（1.2.840.113549.1.1.1）。
const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// id-ecPublicKey（1.2.840.10045.2.1）。
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

/// SPKI の AlgorithmIdentifier OID を [`super::pkcs8::KeyAlgorithm`] へ
/// 分類する（`pkcs8::classify_oid` と同じ判定だが、独立モジュールのため
/// 重複して持つ。値は内容非依存の分類のみで秘密は含まない）。
fn classify_spki_oid(oid: &[u8]) -> KeyAlgorithm {
    if oid == OID_RSA {
        KeyAlgorithm::Rsa
    } else if oid == OID_EC {
        KeyAlgorithm::Ecdsa
    } else if oid == OID_ED448 {
        KeyAlgorithm::Ed448
    } else if oid == OID_X25519 {
        KeyAlgorithm::X25519
    } else {
        KeyAlgorithm::Other
    }
}

/// X.509 証明書 1 個のパース・validity・SPKI 検査で検出した拒否理由。
/// 証明書内容・実バイト・時刻値は含めない（分類のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X509Error {
    /// DER 構文（タグ・長さ・入れ子深さ・後続バイト等）そのものの違反、
    /// または本パーサが要求する固定フィールド順序からの逸脱。
    Malformed,
    /// 証明書 DER が [`MAX_CERTIFICATE_DER_LEN`] を超える。
    CertificateTooLarge,
    /// version が `[0] EXPLICIT INTEGER 2`（v3）でない（欠落＝v1・
    /// v2・その他の値を含む）。
    UnsupportedVersion,
    /// `tbsCertificate.signature` と外側 `signatureAlgorithm` の DER
    /// バイト列が一致しない（RFC 5280 §4.1.1.2）。
    SignatureAlgorithmMismatch,
    /// validity の時刻表現（UTCTime／GeneralizedTime）が形式違反。
    InvalidTime,
    /// `notBefore > notAfter`。
    InvalidValidityRange,
    /// 現在時刻が `notBefore` より前。
    NotYetValid,
    /// 現在時刻が `notAfter` より後。
    Expired,
    /// 葉証明書の SPKI アルゴリズムが Ed25519 以外、または Ed25519 だが
    /// parameters を持つ等 RFC 8410 §3 の形状に反する。
    UnsupportedPublicKeyAlgorithm(KeyAlgorithm),
    /// SPKI の BIT STRING がちょうど 32 バイトの鍵として取り出せない。
    InvalidPublicKey,
    /// 葉証明書の SPKI 公開鍵が期待公開鍵（秘密鍵から導出済み）と一致しない。
    PublicKeyMismatch,
}

impl fmt::Display for X509Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            X509Error::Malformed => write!(f, "X.509 certificate DER is malformed"),
            X509Error::CertificateTooLarge => write!(f, "X.509 certificate DER exceeds size limit"),
            X509Error::UnsupportedVersion => {
                write!(f, "X.509 certificate version is not v3")
            }
            X509Error::SignatureAlgorithmMismatch => write!(
                f,
                "tbsCertificate.signature does not match the outer signatureAlgorithm"
            ),
            X509Error::InvalidTime => write!(f, "X.509 certificate time value is malformed"),
            X509Error::InvalidValidityRange => write!(f, "notBefore is later than notAfter"),
            X509Error::NotYetValid => write!(f, "certificate is not yet valid"),
            X509Error::Expired => write!(f, "certificate has expired"),
            X509Error::UnsupportedPublicKeyAlgorithm(algo) => write!(
                f,
                "leaf certificate public key algorithm {algo} is not supported (Ed25519 only)"
            ),
            X509Error::InvalidPublicKey => write!(f, "SPKI public key is malformed"),
            X509Error::PublicKeyMismatch => write!(
                f,
                "leaf certificate public key does not match the private key"
            ),
        }
    }
}

impl std::error::Error for X509Error {}

/// 証明書チェーン全体の検査で検出した拒否理由。`index` はチェーン内の
/// 0 始まり位置（先頭＝葉。0 が葉に対応することを含め公開情報）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateChainError {
    /// チェーンが空。
    EmptyChain,
    /// チェーンの証明書数が [`super::pem::MAX_CERTIFICATE_CHAIN_LEN`] を
    /// 超える。
    ChainTooLong { max: usize },
    /// `index` 番目の証明書で `error` が発生した。
    Certificate { index: usize, error: X509Error },
    /// `Certificate` メッセージ本体が u24 長さフィールドの上限
    /// （`0xFF_FFFF`）を超える。
    MessageTooLarge,
}

impl fmt::Display for CertificateChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CertificateChainError::EmptyChain => write!(f, "certificate chain is empty"),
            CertificateChainError::ChainTooLong { max } => {
                write!(f, "certificate chain has more than {max} certificates")
            }
            CertificateChainError::Certificate { index, error } => {
                write!(f, "certificate at index {index}: {error}")
            }
            CertificateChainError::MessageTooLarge => {
                write!(f, "Certificate message body exceeds the u24 length limit")
            }
        }
    }
}

impl std::error::Error for CertificateChainError {}

/// ファイル入出力・PEM 構文・チェーン検査・時刻取得のいずれかで失敗した
/// サーバー証明書チェーンの読み込みエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerCertificateLoadError {
    Load(pem::CertificateLoadError),
    /// システム時刻がエポック（1970-01-01）より前、または `i64` 秒の
    /// 範囲外で取得できなかった（fail-closed。#967 が起動時に扱う）。
    Clock,
    Chain(CertificateChainError),
}

impl fmt::Display for ServerCertificateLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerCertificateLoadError::Load(e) => write!(f, "{e}"),
            ServerCertificateLoadError::Clock => {
                write!(f, "failed to read the current time for validity checks")
            }
            ServerCertificateLoadError::Chain(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ServerCertificateLoadError {}

/// 1 個の証明書からパースした、後続検査に必要なフィールドだけを保持する
/// 内部表現。issuer／subject の中身・extensions の意味は保持しない。
struct ParsedCertificate {
    not_before: i64,
    not_after: i64,
    spki_oid: Vec<u8>,
    spki_has_parameters: bool,
    spki_key_bits: Vec<u8>,
}

/// `AlgorithmIdentifier ::= SEQUENCE { algorithm OBJECT IDENTIFIER,
/// parameters ANY DEFINED BY algorithm OPTIONAL }`（RFC 5280 §4.1.1.2）の
/// 構造を検査する。`tbsCertificate.signature`・外側 `signatureAlgorithm`・
/// `subjectPublicKeyInfo` の AlgorithmIdentifier はいずれもこの形を
/// 満たさなければならないが、`read_any` でタグが `SEQUENCE` であることしか
/// 確認していなかったため、algorithm OID を持たない空 SEQUENCE や、
/// 切り詰められた OID・複数の余剰 TLV を含む不正な AlgorithmIdentifier が
/// そのまま受理されてしまっていた（signature 側は DER バイト列一致検査
/// （手順 5）を、SPKI 側は葉証明書限定の Ed25519 判定を、それぞれすり抜けて
/// いた）。ここで「先頭に OID が 1 個存在し、続く要素は任意の parameters
/// 高々 1 個までで、それ以外の余剰要素が無い」ことを検査し、signature 側は
/// 手順 5 の一致検査より前に、SPKI 側は葉・中間を問わず必ず通す。
fn validate_algorithm_identifier_structure(tlv_value: &[u8]) -> Result<(), X509Error> {
    let mut reader = DerReader::new(tlv_value);
    let oid = reader
        .read_expected(TAG_OID)
        .map_err(|_| X509Error::Malformed)?;
    validate_oid_content(oid)?;
    if !reader.is_empty() {
        // parameters ANY: 中身の意味は解釈しないが、1 個の TLV として
        // 整形式であることだけは要求する。
        reader.read_any().map_err(|_| X509Error::Malformed)?;
    }
    reader.expect_end().map_err(|_| X509Error::Malformed)
}

/// serialNumber INTEGER（RFC 5280 §4.1.2.2）が「正の整数」「20 オクテット
/// 以下」「最小符号化（不要な先頭 0x00 オクテットを持たない）」であることを
/// 検査する。`serial.is_empty()` のみの検査では負数・ゼロ・21 オクテット
/// 以上・非最小符号化（例: 先頭が `0x00 0x7f...` のように不要な `0x00` を
/// 持つ）を受理してしまい、本パーサが起動時に通した証明書が RFC 5280
/// 準拠の TLS クライアント側パーサからは不正として拒否されうる
/// （Issue #963 レビュー指摘）。
fn validate_serial_number(serial: &[u8]) -> Result<(), X509Error> {
    let (&first, rest) = serial.split_first().ok_or(X509Error::Malformed)?;
    // DER INTEGER の最上位ビットが立っていれば負数。RFC 5280 は
    // serialNumber を正の整数と規定する。
    if first & 0x80 != 0 {
        return Err(X509Error::Malformed);
    }
    // 全オクテットが 0 ならゼロ（正の整数ではない）。
    if serial.iter().all(|&b| b == 0) {
        return Err(X509Error::Malformed);
    }
    // 最小符号化: 先頭が 0x00 で、かつ次のオクテットの最上位ビットが
    // 立っていない場合、その 0x00 は符号ビット確保のために不要（非最小）。
    if first == 0x00 {
        if let Some(&second) = rest.first() {
            if second & 0x80 == 0 {
                return Err(X509Error::Malformed);
            }
        }
    }
    if serial.len() > 20 {
        return Err(X509Error::Malformed);
    }
    Ok(())
}

/// OBJECT IDENTIFIER の値部分（BER/DER の base-128 可変長サブ識別子列）が
/// 最小限整形式であることを検査する（`validate_structure` はタグ・長さ・
/// 入れ子だけを見て primitive の中身には潜らないため、OID として無意味な
/// バイト列——空・サブ識別子が継続ビット付きのまま終端・先頭バイトが
/// 非最小符号化の `0x80`——を通してしまう）。DER の値そのものの解釈
/// （既知 OID との比較）は呼び出し元の責務のまま変えない。
fn validate_oid_content(oid: &[u8]) -> Result<(), X509Error> {
    if oid.is_empty() {
        return Err(X509Error::Malformed);
    }
    let mut at_subidentifier_start = true;
    for byte in oid {
        if at_subidentifier_start && *byte == 0x80 {
            // サブ識別子の先頭バイトが 0x80 は非最小符号化（先行ゼロ）。
            return Err(X509Error::Malformed);
        }
        at_subidentifier_start = byte & 0x80 == 0;
    }
    if !at_subidentifier_start {
        // 最後のサブ識別子が継続ビット付きのまま終端している
        // （切り詰められた OID）。
        return Err(X509Error::Malformed);
    }
    Ok(())
}

/// パース手順（モジュール doc 参照）に従い 1 個の証明書 DER を検査する。
fn parse_certificate(der_bytes: &[u8]) -> Result<ParsedCertificate, X509Error> {
    if der_bytes.len() > MAX_CERTIFICATE_DER_LEN {
        return Err(X509Error::CertificateTooLarge);
    }
    der::validate_structure(der_bytes, der::MAX_DER_NESTING_DEPTH)
        .map_err(|_| X509Error::Malformed)?;

    let mut outer = DerReader::new(der_bytes);
    let cert_tlv = outer.read_any().map_err(|_| X509Error::Malformed)?;
    if cert_tlv.tag != TAG_SEQUENCE {
        return Err(X509Error::Malformed);
    }
    outer.expect_end().map_err(|_| X509Error::Malformed)?;

    let mut cert_reader = DerReader::new(cert_tlv.value);
    let tbs_tlv = cert_reader.read_any().map_err(|_| X509Error::Malformed)?;
    if tbs_tlv.tag != TAG_SEQUENCE {
        return Err(X509Error::Malformed);
    }
    let sig_alg_tlv = cert_reader.read_any().map_err(|_| X509Error::Malformed)?;
    if sig_alg_tlv.tag != TAG_SEQUENCE {
        return Err(X509Error::Malformed);
    }
    let signature_value = cert_reader
        .read_expected(TAG_BIT_STRING)
        .map_err(|_| X509Error::Malformed)?;
    cert_reader.expect_end().map_err(|_| X509Error::Malformed)?;

    let mut tbs = DerReader::new(tbs_tlv.value);

    // version [0] EXPLICIT INTEGER 2（v3）のみ受理。欠落（v1）・
    // 他のタグはここで UnsupportedVersion として拒否する。
    let version_wrapper = tbs
        .read_expected(TAG_VERSION_EXPLICIT)
        .map_err(|_| X509Error::UnsupportedVersion)?;
    let mut version_reader = DerReader::new(version_wrapper);
    let version_bytes = version_reader
        .read_expected(TAG_INTEGER)
        .map_err(|_| X509Error::Malformed)?;
    version_reader
        .expect_end()
        .map_err(|_| X509Error::Malformed)?;
    if version_bytes != [0x02] {
        return Err(X509Error::UnsupportedVersion);
    }

    // serialNumber INTEGER（正の整数・20 オクテット以下・最小符号化。
    // RFC 5280 §4.1.2.2）。
    let serial = tbs
        .read_expected(TAG_INTEGER)
        .map_err(|_| X509Error::Malformed)?;
    validate_serial_number(serial)?;

    // signature AlgorithmIdentifier（tbsCertificate 側）。外側の
    // signatureAlgorithm との DER バイト列一致検査は、モジュール doc の
    // 「パース手順」どおり tbsCertificate の残りのフィールドをすべて
    // 読み終えたあと（手順 5）にまとめて行う。
    let tbs_sig_alg_tlv = tbs.read_any().map_err(|_| X509Error::Malformed)?;
    if tbs_sig_alg_tlv.tag != TAG_SEQUENCE {
        return Err(X509Error::Malformed);
    }

    // issuer Name（中身は解釈しない）。
    tbs.read_expected(TAG_SEQUENCE)
        .map_err(|_| X509Error::Malformed)?;

    // validity SEQUENCE { notBefore Time, notAfter Time }。
    let validity_body = tbs
        .read_expected(TAG_SEQUENCE)
        .map_err(|_| X509Error::Malformed)?;
    let mut validity_reader = DerReader::new(validity_body);
    let not_before = parse_time_tlv(&mut validity_reader)?;
    let not_after = parse_time_tlv(&mut validity_reader)?;
    validity_reader
        .expect_end()
        .map_err(|_| X509Error::Malformed)?;
    if not_before > not_after {
        return Err(X509Error::InvalidValidityRange);
    }

    // subject Name（中身は解釈しない）。
    tbs.read_expected(TAG_SEQUENCE)
        .map_err(|_| X509Error::Malformed)?;

    // subjectPublicKeyInfo SEQUENCE { AlgorithmIdentifier, BIT STRING }。
    let spki_body = tbs
        .read_expected(TAG_SEQUENCE)
        .map_err(|_| X509Error::Malformed)?;
    let mut spki_reader = DerReader::new(spki_body);
    let spki_alg_tlv = spki_reader.read_any().map_err(|_| X509Error::Malformed)?;
    if spki_alg_tlv.tag != TAG_SEQUENCE {
        return Err(X509Error::Malformed);
    }
    // SPKI 側の AlgorithmIdentifier も tbsCertificate.signature／外側
    // signatureAlgorithm と同じ構造検査を通す（OID の整形式性・parameters
    // 高々 1 個・余剰要素なし）。葉証明書では後続の check_leaf_public_key が
    // Ed25519 OID 完全一致・parameters 不在を要求するため結果的に多くの
    // 不正形を拒否できていたが、中間証明書にはその検査が無く、切り詰められた
    // OID や複数の余剰 TLV を含む不正な AlgorithmIdentifier がそのまま
    // 受理されていた（Issue #963 レビュー指摘）。
    validate_algorithm_identifier_structure(spki_alg_tlv.value)?;
    let mut spki_alg_reader = DerReader::new(spki_alg_tlv.value);
    let spki_oid = spki_alg_reader
        .read_expected(TAG_OID)
        .map_err(|_| X509Error::Malformed)?;
    let spki_has_parameters = !spki_alg_reader.is_empty();
    let spki_key_bits = spki_reader
        .read_expected(TAG_BIT_STRING)
        .map_err(|_| X509Error::Malformed)?;
    spki_reader.expect_end().map_err(|_| X509Error::Malformed)?;

    // 任意の issuerUniqueID [1]／subjectUniqueID [2]／extensions [3]。
    // 存在すれば読み飛ばすのみで、意味は解釈しない。
    let _ = tbs.read_optional(TAG_ISSUER_UNIQUE_ID_IMPLICIT);
    let _ = tbs.read_optional(TAG_SUBJECT_UNIQUE_ID_IMPLICIT);
    let _ = tbs.read_optional(TAG_EXTENSIONS_EXPLICIT);
    tbs.expect_end().map_err(|_| X509Error::Malformed)?;

    // tbsCertificate.signature と外側 signatureAlgorithm がいずれも
    // 有効な AlgorithmIdentifier（algorithm OID を持つこと・許容外の
    // 余剰要素が無いこと）であることを、DER バイト列一致（手順 5。
    // RFC 5280 §4.1.1.2）より先に検査する。両者は raw バイト列一致を
    // 要求するため、一方を検証すれば他方も同じ構造であることが保証される。
    validate_algorithm_identifier_structure(tbs_sig_alg_tlv.value)?;
    if tbs_sig_alg_tlv.raw != sig_alg_tlv.raw {
        return Err(X509Error::SignatureAlgorithmMismatch);
    }

    // signatureValue BIT STRING の形状検査（手順 6）。unused-bits
    // オクテットが 0〜7 の範囲であることに加え、(a) その直後に署名データが
    // 1 バイト以上存在すること（実体のない signatureValue を拒否）、
    // (b) unused-bits が非ゼロの場合、最終オクテットの下位 unused-bits
    // ビットがすべて 0 であること（DER の正規化要件。非正規表現を拒否）
    // を検査する。
    let (unused_bits, signature_bytes) =
        signature_value.split_first().ok_or(X509Error::Malformed)?;
    if *unused_bits > 7 {
        return Err(X509Error::Malformed);
    }
    let last_byte = signature_bytes.last().ok_or(X509Error::Malformed)?;
    if *unused_bits > 0 {
        let unused_mask = (1u8 << *unused_bits) - 1;
        if last_byte & unused_mask != 0 {
            return Err(X509Error::Malformed);
        }
    }

    Ok(ParsedCertificate {
        not_before,
        not_after,
        spki_oid: spki_oid.to_vec(),
        spki_has_parameters,
        spki_key_bits: spki_key_bits.to_vec(),
    })
}

fn check_validity(certificate: &ParsedCertificate, now_unix_secs: i64) -> Result<(), X509Error> {
    if now_unix_secs < certificate.not_before {
        return Err(X509Error::NotYetValid);
    }
    if now_unix_secs > certificate.not_after {
        return Err(X509Error::Expired);
    }
    Ok(())
}

/// 葉証明書の SPKI が Ed25519 で、その公開鍵が `expected` と一致することを
/// 検査する（受入基準 2・#961 との seam。モジュール doc 参照）。
fn check_leaf_public_key(
    spki_oid: &[u8],
    spki_has_parameters: bool,
    spki_key_bits: &[u8],
    expected: &[u8; 32],
) -> Result<(), X509Error> {
    if spki_oid != OID_ED25519 {
        return Err(X509Error::UnsupportedPublicKeyAlgorithm(classify_spki_oid(
            spki_oid,
        )));
    }
    // RFC 8410 §3: Ed25519 の AlgorithmIdentifier は parameters を
    // 「持たない」ことを要求する。
    if spki_has_parameters {
        return Err(X509Error::InvalidPublicKey);
    }
    let (unused_bits, key_value) = spki_key_bits
        .split_first()
        .ok_or(X509Error::InvalidPublicKey)?;
    if *unused_bits != 0 {
        return Err(X509Error::InvalidPublicKey);
    }
    let key_array: [u8; 32] = key_value
        .try_into()
        .map_err(|_| X509Error::InvalidPublicKey)?;
    // 公開鍵は秘密鍵から導出された値だが公開データそのものであり秘密では
    // ない。それでも導出元が秘密値であるため保守的に定数時間比較する
    // （モジュール doc「定数時間性」参照）。
    if !ct_eq(&key_array, expected) {
        return Err(X509Error::PublicKeyMismatch);
    }
    Ok(())
}

fn ascii_digit(byte: u8) -> Result<i64, X509Error> {
    if byte.is_ascii_digit() {
        Ok(i64::from(byte - b'0'))
    } else {
        Err(X509Error::InvalidTime)
    }
}

fn parse_two_digit(a: u8, b: u8) -> Result<i64, X509Error> {
    Ok(ascii_digit(a)? * 10 + ascii_digit(b)?)
}

fn parse_four_digit(a: u8, b: u8, c: u8, d: u8) -> Result<i64, X509Error> {
    Ok(ascii_digit(a)? * 1000 + ascii_digit(b)? * 100 + ascii_digit(c)? * 10 + ascii_digit(d)?)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// 1970-01-01 からの経過日数を求める（Howard Hinnant の
/// `days_from_civil` アルゴリズム。グレゴリオ暦・`i64`）。呼び出し元
/// （[`build_epoch_seconds`]）が month を 1〜12・day を暦上有効な範囲に
/// 検証済みであることを前提とする。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 暦上の各フィールドを検査したうえでエポック秒（`i64`）へ変換する。
/// うるう秒（60 秒）は受理しない。
fn build_epoch_seconds(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> Result<i64, X509Error> {
    if !(1..=12).contains(&month) {
        return Err(X509Error::InvalidTime);
    }
    let max_day = days_in_month(year, month);
    if day < 1 || day > max_day {
        return Err(X509Error::InvalidTime);
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(X509Error::InvalidTime);
    }
    let days = days_from_civil(year, month, day);
    let seconds_of_day = hour
        .checked_mul(3600)
        .and_then(|v| minute.checked_mul(60).and_then(|m| v.checked_add(m)))
        .and_then(|v| v.checked_add(second))
        .ok_or(X509Error::InvalidTime)?;
    days.checked_mul(86_400)
        .and_then(|d| d.checked_add(seconds_of_day))
        .ok_or(X509Error::InvalidTime)
}

/// UTCTime（RFC 5280 §4.1.2.5.1）: `YYMMDDHHMMSSZ` の 13 バイト固定。
/// `YY >= 50` は 19YY、`YY < 50` は 20YY と解釈する。
fn parse_utc_time(value: &[u8]) -> Result<i64, X509Error> {
    let bytes: [u8; 13] = value.try_into().map_err(|_| X509Error::InvalidTime)?;
    let [y1, y2, mo1, mo2, d1, d2, h1, h2, mi1, mi2, s1, s2, z] = bytes;
    if z != b'Z' {
        return Err(X509Error::InvalidTime);
    }
    let two_digit_year = parse_two_digit(y1, y2)?;
    let year = if two_digit_year >= 50 {
        1900 + two_digit_year
    } else {
        2000 + two_digit_year
    };
    let month = parse_two_digit(mo1, mo2)?;
    let day = parse_two_digit(d1, d2)?;
    let hour = parse_two_digit(h1, h2)?;
    let minute = parse_two_digit(mi1, mi2)?;
    let second = parse_two_digit(s1, s2)?;
    build_epoch_seconds(year, month, day, hour, minute, second)
}

/// GeneralizedTime（RFC 5280 §4.1.2.5.2）: `YYYYMMDDHHMMSSZ` の 15 バイト
/// 固定。RFC 5280 は 2050 年以降にのみ GeneralizedTime を使うことを
/// 要求するため、2050 年未満は fail-closed で拒否する（小数秒・UTC 以外の
/// タイムゾーンは本パーサの対応外で拒否）。
fn parse_generalized_time(value: &[u8]) -> Result<i64, X509Error> {
    let bytes: [u8; 15] = value.try_into().map_err(|_| X509Error::InvalidTime)?;
    let [y1, y2, y3, y4, mo1, mo2, d1, d2, h1, h2, mi1, mi2, s1, s2, z] = bytes;
    if z != b'Z' {
        return Err(X509Error::InvalidTime);
    }
    let year = parse_four_digit(y1, y2, y3, y4)?;
    if year < 2050 {
        return Err(X509Error::InvalidTime);
    }
    let month = parse_two_digit(mo1, mo2)?;
    let day = parse_two_digit(d1, d2)?;
    let hour = parse_two_digit(h1, h2)?;
    let minute = parse_two_digit(mi1, mi2)?;
    let second = parse_two_digit(s1, s2)?;
    build_epoch_seconds(year, month, day, hour, minute, second)
}

fn parse_time_tlv(reader: &mut DerReader<'_>) -> Result<i64, X509Error> {
    let tlv = reader.read_any().map_err(|_| X509Error::Malformed)?;
    match tlv.tag {
        TAG_UTC_TIME => parse_utc_time(tlv.value),
        TAG_GENERALIZED_TIME => parse_generalized_time(tlv.value),
        _ => Err(X509Error::Malformed),
    }
}

/// 検査済みのサーバー証明書チェーン。DER 列と Certificate メッセージへの
/// 組み立て結果を保持する。`super::x509` の外からは
/// [`ServerCertificateChain::from_der_chain`]／
/// [`load_server_certificate_chain_file`] を経由してのみ構築できる。
#[derive(Debug)]
pub struct ServerCertificateChain {
    message: Certificate,
    leaf_public_key: [u8; 32],
}

impl ServerCertificateChain {
    /// DER 列（先頭が葉証明書）を検査し、`Certificate` メッセージへ
    /// 組み立てる。判定順序はモジュール doc の「パース手順」を参照。
    pub fn from_der_chain(
        chain: Vec<Vec<u8>>,
        expected_leaf_public_key: &[u8; 32],
        now_unix_secs: i64,
    ) -> Result<Self, CertificateChainError> {
        if chain.is_empty() {
            return Err(CertificateChainError::EmptyChain);
        }
        if chain.len() > pem::MAX_CERTIFICATE_CHAIN_LEN {
            return Err(CertificateChainError::ChainTooLong {
                max: pem::MAX_CERTIFICATE_CHAIN_LEN,
            });
        }

        for (index, der_bytes) in chain.iter().enumerate() {
            let certificate = parse_certificate(der_bytes)
                .map_err(|error| CertificateChainError::Certificate { index, error })?;
            check_validity(&certificate, now_unix_secs)
                .map_err(|error| CertificateChainError::Certificate { index, error })?;
            if index == 0 {
                check_leaf_public_key(
                    &certificate.spki_oid,
                    certificate.spki_has_parameters,
                    &certificate.spki_key_bits,
                    expected_leaf_public_key,
                )
                .map_err(|error| CertificateChainError::Certificate { index, error })?;
            }
        }

        let certificate_list = chain
            .into_iter()
            .map(|cert_data| CertificateEntry {
                cert_data,
                extensions: Vec::new(),
            })
            .collect();
        let message = Certificate {
            certificate_request_context: Vec::new(),
            certificate_list,
        };
        // Certificate メッセージ本体（u24 長さフィールド）に収まることを
        // 起動時に確認する（ハンドシェイク中の送出失敗を避ける）。
        let mut body = Vec::new();
        message
            .serialize_body_into(&mut body)
            .map_err(|_| CertificateChainError::MessageTooLarge)?;

        Ok(ServerCertificateChain {
            message,
            leaf_public_key: *expected_leaf_public_key,
        })
    }

    /// RFC 8446 §4.4.2 の `Certificate` メッセージを組み立てる
    /// （#965 の状態機械が送出する）。
    pub fn certificate_message(&self) -> Certificate {
        self.message.clone()
    }

    /// 検査済みの葉公開鍵（`expected_leaf_public_key` と一致することを
    /// 確認済み）。#961／#967 が結果確認に使う。
    pub fn leaf_public_key(&self) -> &[u8; 32] {
        &self.leaf_public_key
    }
}

/// 現在時刻をエポック秒（`i64`）で取得する。エポック以前・`i64` 範囲外は
/// fail-closed で拒否する（#967 が起動時に呼ぶ）。
pub fn current_unix_secs() -> Result<i64, ServerCertificateLoadError> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ServerCertificateLoadError::Clock)?;
    i64::try_from(duration.as_secs()).map_err(|_| ServerCertificateLoadError::Clock)
}

/// 証明書チェーンファイル（PEM）を読み込み、検査済みの
/// [`ServerCertificateChain`] を返す（[`super::pem::load_certificate_chain_file`]
/// → [`ServerCertificateChain::from_der_chain`] の合成）。CLI からの結線は
/// #967 の担当。
pub fn load_server_certificate_chain_file(
    path: &Path,
    expected_leaf_public_key: &[u8; 32],
    now_unix_secs: i64,
) -> Result<ServerCertificateChain, ServerCertificateLoadError> {
    let chain = pem::load_certificate_chain_file(path).map_err(ServerCertificateLoadError::Load)?;
    ServerCertificateChain::from_der_chain(chain, expected_leaf_public_key, now_unix_secs)
        .map_err(ServerCertificateLoadError::Chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 既知のエポック秒（RFC 8410 §10.2 の証明書 validity・UNIX epoch・
    // 1950 年の負値）。時刻パースだけを単独で先に固定する。
    #[test]
    fn parse_utc_time_matches_known_epoch_seconds() {
        assert_eq!(
            parse_utc_time(b"160801121924Z").expect("valid UTCTime"),
            1_470_053_964
        );
        assert_eq!(
            parse_utc_time(b"401231235959Z").expect("valid UTCTime"),
            2_240_611_199
        );
        assert_eq!(parse_utc_time(b"700101000000Z").expect("valid UTCTime"), 0);
        assert_eq!(
            parse_utc_time(b"500101000000Z").expect("valid UTCTime"),
            -631_152_000
        );
    }

    #[test]
    fn parse_utc_time_49_50_boundary() {
        // YY=49 → 2049 年、YY=50 → 1950 年。
        let epoch_2049 = parse_utc_time(b"490101000000Z").expect("2049");
        let epoch_1950 = parse_utc_time(b"500101000000Z").expect("1950");
        assert!(epoch_2049 > 0);
        assert!(epoch_1950 < 0);
    }

    #[test]
    fn parse_generalized_time_matches_known_epoch_seconds() {
        // 独立系統（`date -u -d '2050-12-31T23:59:59Z' +%s`）で確認した値
        // との一致を固定する（本実装の `build_epoch_seconds` を期待値の
        // 算出にも使う循環参照を避けるため）。
        assert_eq!(
            parse_generalized_time(b"20501231235959Z").expect("valid GeneralizedTime"),
            2_556_143_999
        );
    }

    #[test]
    fn parse_generalized_time_rejects_year_before_2050() {
        let err = parse_generalized_time(b"20491231235959Z").unwrap_err();
        assert_eq!(err, X509Error::InvalidTime);
    }

    #[test]
    fn leap_year_boundaries() {
        // 2024 は閏年、2023・2100 は違う、2000 は閏年（400 で割り切れる）。
        assert!(parse_utc_time(b"240229000000Z").is_ok());
        assert!(parse_utc_time(b"230229000000Z").is_err());
        assert!(build_epoch_seconds(2100, 2, 29, 0, 0, 0).is_err());
        assert!(build_epoch_seconds(2000, 2, 29, 0, 0, 0).is_ok());
    }

    /// `YYMMDDHHMMSSZ` の 13 バイトを各フィールドの 2 桁文字列から組み立てる
    /// （手でバイト列を書くと桁位置を誤りやすいため、テスト専用に用意する）。
    fn utc_time_bytes(yy: &str, mo: &str, dd: &str, hh: &str, mi: &str, ss: &str) -> Vec<u8> {
        format!("{yy}{mo}{dd}{hh}{mi}{ss}Z").into_bytes()
    }

    #[test]
    fn rejects_invalid_calendar_fields() {
        assert_eq!(
            parse_utc_time(&utc_time_bytes("00", "00", "01", "00", "00", "00")).unwrap_err(),
            X509Error::InvalidTime
        ); // month 0
        assert_eq!(
            parse_utc_time(&utc_time_bytes("00", "13", "01", "00", "00", "00")).unwrap_err(),
            X509Error::InvalidTime
        ); // month 13
        assert_eq!(
            parse_utc_time(&utc_time_bytes("00", "01", "32", "00", "00", "00")).unwrap_err(),
            X509Error::InvalidTime
        ); // day 32（1 月は 31 日まで）
        assert_eq!(
            parse_utc_time(&utc_time_bytes("00", "01", "01", "24", "00", "00")).unwrap_err(),
            X509Error::InvalidTime
        ); // hour 24
        assert_eq!(
            parse_utc_time(&utc_time_bytes("00", "01", "01", "00", "60", "00")).unwrap_err(),
            X509Error::InvalidTime
        ); // minute 60
        assert_eq!(
            parse_utc_time(&utc_time_bytes("00", "01", "01", "00", "00", "60")).unwrap_err(),
            X509Error::InvalidTime
        ); // second 60（うるう秒は非受理）
    }

    #[test]
    fn rejects_missing_z_and_offset_and_fraction_and_wrong_length() {
        assert_eq!(
            parse_utc_time(b"000101000000+").unwrap_err(),
            X509Error::InvalidTime
        );
        assert_eq!(
            parse_utc_time(b"0001010000+09").unwrap_err(),
            X509Error::InvalidTime
        );
        assert_eq!(
            parse_utc_time(b"00010100000.Z").unwrap_err(),
            X509Error::InvalidTime
        );
        assert_eq!(
            parse_utc_time(b"0001010000Z").unwrap_err(),
            X509Error::InvalidTime
        ); // 短い
        assert_eq!(
            parse_utc_time(b"000101000000ZZ").unwrap_err(),
            X509Error::InvalidTime
        ); // 長い
    }

    #[test]
    fn validity_boundary_now_equal_notbefore_or_notafter_is_accepted() {
        let cert = ParsedCertificate {
            not_before: 1000,
            not_after: 2000,
            spki_oid: OID_ED25519.to_vec(),
            spki_has_parameters: false,
            spki_key_bits: vec![0u8; 33],
        };
        assert!(check_validity(&cert, 1000).is_ok());
        assert!(check_validity(&cert, 2000).is_ok());
        assert_eq!(
            check_validity(&cert, 999).unwrap_err(),
            X509Error::NotYetValid
        );
        assert_eq!(check_validity(&cert, 2001).unwrap_err(), X509Error::Expired);
    }

    #[test]
    fn check_leaf_public_key_accepts_matching_ed25519_key() {
        let expected = [0x11u8; 32];
        let mut key_bits = vec![0u8]; // unused bits = 0
        key_bits.extend_from_slice(&expected);
        assert!(check_leaf_public_key(OID_ED25519, false, &key_bits, &expected).is_ok());
    }

    #[test]
    fn check_leaf_public_key_rejects_mismatch() {
        let expected = [0x11u8; 32];
        let other = [0x22u8; 32];
        let mut key_bits = vec![0u8];
        key_bits.extend_from_slice(&other);
        let err = check_leaf_public_key(OID_ED25519, false, &key_bits, &expected).unwrap_err();
        assert_eq!(err, X509Error::PublicKeyMismatch);
    }

    #[test]
    fn check_leaf_public_key_rejects_non_ed25519_algorithm() {
        let expected = [0x11u8; 32];
        let mut key_bits = vec![0u8];
        key_bits.extend_from_slice(&[0u8; 32]);
        let err = check_leaf_public_key(OID_X25519, false, &key_bits, &expected).unwrap_err();
        assert_eq!(
            err,
            X509Error::UnsupportedPublicKeyAlgorithm(KeyAlgorithm::X25519)
        );
    }

    #[test]
    fn check_leaf_public_key_rejects_parameters_and_wrong_key_length() {
        let expected = [0x11u8; 32];
        let mut key_bits_ok = vec![0u8];
        key_bits_ok.extend_from_slice(&expected);
        assert_eq!(
            check_leaf_public_key(OID_ED25519, true, &key_bits_ok, &expected).unwrap_err(),
            X509Error::InvalidPublicKey
        );

        let mut key_bits_31 = vec![0u8];
        key_bits_31.extend_from_slice(&[0u8; 31]);
        assert_eq!(
            check_leaf_public_key(OID_ED25519, false, &key_bits_31, &expected).unwrap_err(),
            X509Error::InvalidPublicKey
        );

        let mut key_bits_unused = vec![1u8];
        key_bits_unused.extend_from_slice(&expected);
        assert_eq!(
            check_leaf_public_key(OID_ED25519, false, &key_bits_unused, &expected).unwrap_err(),
            X509Error::InvalidPublicKey
        );
    }

    #[test]
    fn validate_serial_number_accepts_minimal_positive_values() {
        assert!(validate_serial_number(&[0x01]).is_ok());
        // 最上位ビットが立つ正の値は、符号ビット確保のための単一の 0x00
        // 接頭辞が必須（かつそれのみ許容される）。
        assert!(validate_serial_number(&[0x00, 0x80]).is_ok());
        assert!(validate_serial_number(&[0x7f]).is_ok());
        // 20 オクテットちょうどは受理する。
        assert!(validate_serial_number(&[0x01; 20]).is_ok());
    }

    #[test]
    fn validate_serial_number_rejects_negative_zero_and_oversized() {
        assert_eq!(
            validate_serial_number(&[]).unwrap_err(),
            X509Error::Malformed
        );
        // 最上位ビットが立っている（DER INTEGER としては負数）。
        assert_eq!(
            validate_serial_number(&[0x80]).unwrap_err(),
            X509Error::Malformed
        );
        assert_eq!(
            validate_serial_number(&[0xff]).unwrap_err(),
            X509Error::Malformed
        );
        // ゼロ（1 オクテット・複数オクテットいずれも）。
        assert_eq!(
            validate_serial_number(&[0x00]).unwrap_err(),
            X509Error::Malformed
        );
        assert_eq!(
            validate_serial_number(&[0x00, 0x00]).unwrap_err(),
            X509Error::Malformed
        );
        // 非最小符号化: 次オクテットの最上位ビットが立っていないのに
        // 先頭 0x00 を付けている。
        assert_eq!(
            validate_serial_number(&[0x00, 0x7f]).unwrap_err(),
            X509Error::Malformed
        );
        // 21 オクテット（RFC 5280 の上限 20 を超える）。
        assert_eq!(
            validate_serial_number(&[0x01; 21]).unwrap_err(),
            X509Error::Malformed
        );
    }

    #[test]
    fn empty_and_too_long_chain_are_rejected() {
        let expected = [0u8; 32];
        let err = ServerCertificateChain::from_der_chain(vec![], &expected, 0).unwrap_err();
        assert_eq!(err, CertificateChainError::EmptyChain);

        let chain: Vec<Vec<u8>> = (0..9).map(|_| vec![0x30, 0x00]).collect();
        let err = ServerCertificateChain::from_der_chain(chain, &expected, 0).unwrap_err();
        assert_eq!(
            err,
            CertificateChainError::ChainTooLong {
                max: pem::MAX_CERTIFICATE_CHAIN_LEN
            }
        );
    }
}
