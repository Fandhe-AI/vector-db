//! `ClientHello`（`super::handshake::ClientHello`。構造のみ parse 済み）を
//! 入力に、拡張の意味解釈と TLS 1.3 以外の拒否を行う純粋関数層（親 Issue
//! #941・TASK-228・WIRE-9・HTTP-10 ポインタ。本モジュールは #954 の担当）。
//!
//! 対象拡張: `supported_versions`(43)・`key_share`(51)・
//! `signature_algorithms`(13)・`server_name`(0)・`supported_groups`(10)。
//! 本体フィールドの `cipher_suites` もあわせて検査する。
//!
//! # 責務境界（後続 sub-issue との分担）
//!
//! 本モジュールは**状態を持たない**判定関数と HelloRetryRequest（HRR）の
//! 構築のみを担う。
//! - #965（状態機械・alert 送出）: 「HRR を送ったか」の状態保持と alert の
//!   実送出・切断を担う。本モジュールは `after_hrr: bool` を引数で受け取る
//!   だけで状態は一切持たない。「HRR は 1 回のみ」の判定ロジックは
//!   本モジュールが単一情報源として持ち、#965 側で再実装しない
//! - #964（transcript hash）: HRR 時の `message_hash` 置換を担う。本モジュール
//!   は関与しない
//! - #955（X25519）: 共有秘密の計算・全ゼロ検出を担う。本モジュールは
//!   client 公開鍵 32 バイトを取り出して渡すだけ
//! - #959／#965（0-RTT）: `early_data` を無視した場合の早期 application_data
//!   レコードの破棄を担う。本モジュールは PSK／0-RTT を受理せず、
//!   `pre_shared_key`・`psk_key_exchange_modes` は RFC 8446 §4.2.11・§4.2.9
//!   の MUST（位置・併存）だけを検査し中身は解釈しない。`early_data` は
//!   未知拡張として無視する
//! - 通常の（HRR でない）`ServerHello` の組み立て（サーバー鍵を含む）は
//!   #965 が #955 の鍵生成と組み合わせて行う。本モジュールでは作らない
//!
//! # 定数時間についての整理
//!
//! `ClientHello` の全フィールドは通信路上に平文で流れる公開値であり、
//! 本モジュールでは秘密値として扱わない。長さ・値による分岐は公開値にのみ
//! 依存する。client x25519 公開鍵はコピーするだけで、DH は #955 が
//! 定数時間実装で担う。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを用いず `get()`・
//! `checked_*`・配列パターン束縛・`try_into()` で処理する
//! （`.claude/rules/coding-rust.md` P0）。`unsafe` は使わない。

use super::handshake::{self, HandshakeError, Reader};
use super::record::AlertDescription;

/// TLS 1.3 のプロトコルバージョン値（`supported_versions` 拡張の対象）。
pub const TLS13_VERSION: u16 = 0x0304;
/// 本サーバーが受理する唯一の暗号スイート。
pub const TLS_AES_128_GCM_SHA256: u16 = 0x1301;
/// 本サーバーが受理する唯一の鍵交換グループ（X25519。RFC 7748）。
pub const GROUP_X25519: u16 = 0x001d;
/// 本サーバーが受理する唯一の署名アルゴリズム（Ed25519）。
pub const SIG_ED25519: u16 = 0x0807;

const EXT_SERVER_NAME: u16 = 0;
const EXT_SUPPORTED_GROUPS: u16 = 10;
const EXT_SIGNATURE_ALGORITHMS: u16 = 13;
const EXT_PRE_SHARED_KEY: u16 = 41;
const EXT_SUPPORTED_VERSIONS: u16 = 43;
const EXT_PSK_KEY_EXCHANGE_MODES: u16 = 45;
const EXT_KEY_SHARE: u16 = 51;

/// HelloRetryRequest の `random`（RFC 8446 §4.1.3）。
/// SHA-256("HelloRetryRequest") の固定値。
pub const HELLO_RETRY_REQUEST_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// `negotiate` が受理した `ClientHello` から後続段が必要とする値だけを
/// 抜き出したもの。`ClientHello` 全体を渡さないことで、拡張の意味解釈が
/// 本モジュールに閉じていることを型で表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedClientHello {
    pub client_random: [u8; 32],
    /// `ServerHello`／HRR の `legacy_session_id_echo` に echo するための値
    /// （互換モード判定は #965 が担う）。
    pub legacy_session_id: Vec<u8>,
    /// クライアントの x25519 公開鍵（32 バイト）。共有秘密の計算は #955 が担う。
    pub client_x25519_public: [u8; 32],
    /// `server_name` 拡張の host_name を不透明バイト列で保持する
    /// （選択・ログ出力には使わない。RFC 6066 §3）。
    pub server_name: Option<Vec<u8>>,
}

/// [`negotiate`] の判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientHelloDecision {
    /// そのまま 1-RTT で進めてよい。
    Accept(NegotiatedClientHello),
    /// `key_share` に x25519 が無いが `supported_groups` にはある
    /// （HelloRetryRequest で x25519 を要求する）。
    RetryRequestX25519,
}

/// [`negotiate`] が検出しうるエラー全体。各 variant は必ず 1 つの fatal
/// alert を持つ（TLS 層の失敗は alert と切断で表すため `wire_code`
/// （ERR-1/2/4）への写像は追加しない。alert の実送出は #965 が担う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientHelloError {
    /// 構造上の違反（decode_error）。理由は固定の英語文字列で、
    /// 受信バイト列そのものは含めない。
    Decode(&'static str),
    /// 同一 `extension_type` の拡張が複数回現れた（illegal_parameter。
    /// RFC 8446 §4.2「同一 type は 1 つまで」）。
    DuplicateExtension,
    /// 拡張の値そのものの意味検査違反（illegal_parameter）。
    IllegalParameter(&'static str),
    /// `supported_versions` に TLS 1.3（0x0304）が含まれない
    /// （protocol_version。RFC 8446 §4.2.1・付録 D）。
    UnsupportedVersion,
    /// 必須拡張が欠落している（missing_extension。RFC 8446 §9.2）。
    MissingExtension(&'static str),
    /// 暗号スイート・署名アルゴリズム・鍵交換グループのいずれかが
    /// サーバーの受理範囲と一致しない（handshake_failure）。
    HandshakeFailure(&'static str),
}

impl ClientHelloError {
    /// クライアントへ返すべき TLS alert の種別（全 variant が必ず持つ。
    /// `handshake::HandshakeError` と異なり `Option` ではない）。
    pub fn alert_description(&self) -> AlertDescription {
        match self {
            ClientHelloError::Decode(_) => AlertDescription::DecodeError,
            ClientHelloError::DuplicateExtension | ClientHelloError::IllegalParameter(_) => {
                AlertDescription::IllegalParameter
            }
            ClientHelloError::UnsupportedVersion => AlertDescription::ProtocolVersion,
            ClientHelloError::MissingExtension(_) => AlertDescription::MissingExtension,
            ClientHelloError::HandshakeFailure(_) => AlertDescription::HandshakeFailure,
        }
    }
}

impl std::fmt::Display for ClientHelloError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientHelloError::Decode(reason) => write!(f, "ClientHello decode error: {reason}"),
            ClientHelloError::DuplicateExtension => {
                write!(f, "ClientHello contains a duplicate extension type")
            }
            ClientHelloError::IllegalParameter(reason) => {
                write!(f, "ClientHello illegal parameter: {reason}")
            }
            ClientHelloError::UnsupportedVersion => {
                write!(f, "ClientHello does not offer TLS 1.3")
            }
            ClientHelloError::MissingExtension(reason) => {
                write!(f, "ClientHello missing required extension: {reason}")
            }
            ClientHelloError::HandshakeFailure(reason) => {
                write!(f, "ClientHello handshake failure: {reason}")
            }
        }
    }
}

impl std::error::Error for ClientHelloError {}

/// `handshake::Reader`（`HandshakeError` を返す）由来のエラーを、この
/// モジュールのエラー型へ写す。`Reader` を複製せず `?` のまま再利用する
/// ための橋渡し。`Decode` はそのまま引き継ぎ、それ以外（本モジュールの
/// 対象拡張の parse では発生しない種別）は内部矛盾として `Decode` に丸める。
impl From<HandshakeError> for ClientHelloError {
    fn from(err: HandshakeError) -> Self {
        match err {
            HandshakeError::Decode(reason) => ClientHelloError::Decode(reason),
            _ => ClientHelloError::Decode("internal handshake reader error"),
        }
    }
}

/// 拡張列から重複を検出しつつ、対象 5 拡張の `extension_data` への
/// 参照を集める。未知の拡張は中身を見ずに無視する（RFC 8446 §4.2・§4.1.2）。
struct ParsedExtensions<'a> {
    supported_versions: Option<&'a [u8]>,
    key_share: Option<&'a [u8]>,
    signature_algorithms: Option<&'a [u8]>,
    server_name: Option<&'a [u8]>,
    supported_groups: Option<&'a [u8]>,
    has_pre_shared_key: bool,
    pre_shared_key_is_last: bool,
    has_psk_key_exchange_modes: bool,
}

/// 拡張の重複検出に使うビットマップ（65536 bit・8 KiB）。`extension_type`
/// は u16 のため、拡張数が多くても O(n) で判定できる（O(n^2) の総当たり
/// 走査を避ける設計。DoS 耐性の観点）。
struct SeenExtensionTypes {
    bits: [u64; 1024],
}

impl SeenExtensionTypes {
    fn new() -> Self {
        Self { bits: [0u64; 1024] }
    }

    /// `ty` を既に見ていれば `true` を返し、そうでなければ記録して `false`
    /// を返す。
    fn mark_and_check_duplicate(&mut self, ty: u16) -> bool {
        let idx = usize::from(ty) / 64;
        let bit = usize::from(ty) % 64;
        let mask = 1u64 << bit;
        let Some(word) = self.bits.get_mut(idx) else {
            // idx は必ず 0..1024 に収まる（u16 の最大値 65535 / 64 = 1023）ため
            // 到達しないが、添字アクセスを避けるため fail-closed に扱う。
            return true;
        };
        let was_set = (*word & mask) != 0;
        *word |= mask;
        was_set
    }
}

fn parse_extensions(
    extensions: &[handshake::Extension],
) -> Result<ParsedExtensions<'_>, ClientHelloError> {
    let mut seen = SeenExtensionTypes::new();
    let mut out = ParsedExtensions {
        supported_versions: None,
        key_share: None,
        signature_algorithms: None,
        server_name: None,
        supported_groups: None,
        has_pre_shared_key: false,
        pre_shared_key_is_last: false,
        has_psk_key_exchange_modes: false,
    };
    let last_index = extensions.len().checked_sub(1);
    for (idx, ext) in extensions.iter().enumerate() {
        if seen.mark_and_check_duplicate(ext.extension_type) {
            return Err(ClientHelloError::DuplicateExtension);
        }
        match ext.extension_type {
            EXT_SUPPORTED_VERSIONS => out.supported_versions = Some(&ext.extension_data),
            EXT_KEY_SHARE => out.key_share = Some(&ext.extension_data),
            EXT_SIGNATURE_ALGORITHMS => out.signature_algorithms = Some(&ext.extension_data),
            EXT_SERVER_NAME => out.server_name = Some(&ext.extension_data),
            EXT_SUPPORTED_GROUPS => out.supported_groups = Some(&ext.extension_data),
            EXT_PRE_SHARED_KEY => {
                out.has_pre_shared_key = true;
                out.pre_shared_key_is_last = last_index == Some(idx);
            }
            EXT_PSK_KEY_EXCHANGE_MODES => out.has_psk_key_exchange_modes = true,
            _ => {
                // 未知の拡張（`early_data` を含む）は中身を見ずに無視する。
                // 早期 application_data レコードの破棄は #959／#965 が担う。
            }
        }
    }
    // RFC 8446 §4.2.11: pre_shared_key は最後の拡張でなければならない。
    if out.has_pre_shared_key && !out.pre_shared_key_is_last {
        return Err(ClientHelloError::IllegalParameter(
            "pre_shared_key extension must be the last extension",
        ));
    }
    // RFC 8446 §4.2.9: pre_shared_key があるなら psk_key_exchange_modes も必須。
    if out.has_pre_shared_key && !out.has_psk_key_exchange_modes {
        return Err(ClientHelloError::MissingExtension(
            "psk_key_exchange_modes required when pre_shared_key is present",
        ));
    }
    Ok(out)
}

/// `supported_versions`（ClientHello 形。`ProtocolVersion versions<2..254>`）
/// の構造を検証し、TLS 1.3（0x0304）を含むか判定する。
fn contains_tls13(data: &[u8]) -> Result<bool, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u8_len(2, 254)?;
    r.expect_end()?;
    if list.len() % 2 != 0 {
        return Err(ClientHelloError::Decode(
            "supported_versions list length must be even",
        ));
    }
    let mut lr = Reader::new(list);
    let mut found = false;
    while lr.remaining() > 0 {
        if lr.u16()? == TLS13_VERSION {
            found = true;
        }
    }
    Ok(found)
}

/// `supported_groups`（`NamedGroup named_group_list<2..2^16-1>`）を検証し、
/// x25519(0x001d) を含むか判定する。
fn contains_x25519_group(data: &[u8]) -> Result<bool, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u16_len(2, 0xFFFF)?;
    r.expect_end()?;
    if list.len() % 2 != 0 {
        return Err(ClientHelloError::Decode(
            "supported_groups list length must be even",
        ));
    }
    let mut lr = Reader::new(list);
    let mut found = false;
    while lr.remaining() > 0 {
        if lr.u16()? == GROUP_X25519 {
            found = true;
        }
    }
    Ok(found)
}

/// `signature_algorithms`（`SignatureScheme
/// supported_signature_algorithms<2..2^16-2>`）を検証し、ed25519(0x0807) を
/// 含むか判定する。
fn contains_ed25519_sig(data: &[u8]) -> Result<bool, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u16_len(2, 0xFFFE)?;
    r.expect_end()?;
    if list.len() % 2 != 0 {
        return Err(ClientHelloError::Decode(
            "signature_algorithms list length must be even",
        ));
    }
    let mut lr = Reader::new(list);
    let mut found = false;
    while lr.remaining() > 0 {
        if lr.u16()? == SIG_ED25519 {
            found = true;
        }
    }
    Ok(found)
}

/// `key_share`（ClientHello 形。`KeyShareEntry client_shares<0..2^16-1>`、
/// 各エントリは `NamedGroup group; opaque key_exchange<1..2^16-1>`）を
/// 検証し、x25519 エントリの `key_exchange` を返す（無ければ `None`）。
/// x25519 エントリが 2 件以上あれば `IllegalParameter`（RFC 8446 §4.2.8）。
fn find_x25519_key_share(data: &[u8]) -> Result<Option<[u8; 32]>, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u16_len(0, 0xFFFF)?;
    r.expect_end()?;
    let mut lr = Reader::new(list);
    let mut found: Option<[u8; 32]> = None;
    while lr.remaining() > 0 {
        let group = lr.u16()?;
        let key_exchange = lr.vec_u16_len(1, 0xFFFF)?;
        if group == GROUP_X25519 {
            if found.is_some() {
                return Err(ClientHelloError::IllegalParameter(
                    "key_share contains more than one x25519 entry",
                ));
            }
            let arr: [u8; 32] = key_exchange.try_into().map_err(|_| {
                ClientHelloError::IllegalParameter(
                    "key_share x25519 entry key_exchange must be 32 bytes",
                )
            })?;
            found = Some(arr);
        }
    }
    Ok(found)
}

/// `server_name`（RFC 6066 §3. `ServerName server_name_list<1..2^16-1>`）を
/// 検証し、`host_name`（name_type=0）を返す（無ければ `None`）。
/// `host_name` が重複していれば `IllegalParameter`（RFC 6066「同一
/// name_type は 1 つまで」）。未知の name_type は不透明データとして
/// 読み飛ばす。
fn parse_server_name(data: &[u8]) -> Result<Option<Vec<u8>>, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u16_len(1, 0xFFFF)?;
    r.expect_end()?;
    let mut lr = Reader::new(list);
    let mut host_name: Option<Vec<u8>> = None;
    while lr.remaining() > 0 {
        let name_type = lr.u8()?;
        let name = lr.vec_u16_len(1, 0xFFFF)?;
        if name_type == 0 {
            if host_name.is_some() {
                return Err(ClientHelloError::IllegalParameter(
                    "server_name contains more than one host_name entry",
                ));
            }
            host_name = Some(name.to_vec());
        }
    }
    Ok(host_name)
}

/// `legacy_compression_methods` が `[0x00]` ちょうどであることを検証する
/// （RFC 8446 §4.1.2 の MUST）。
fn check_compression(methods: &[u8]) -> Result<(), ClientHelloError> {
    if methods == [0u8] {
        Ok(())
    } else {
        Err(ClientHelloError::IllegalParameter(
            "legacy_compression_methods must be exactly [0]",
        ))
    }
}

/// `ClientHello` を受理条件（TLS 1.3・`TLS_AES_128_GCM_SHA256`・X25519・
/// Ed25519 のみ）で判定する。判定順序はこの関数が単一情報源であり、
/// `docs/design/tls-client-hello.md` の記述もこれに従う。
///
/// `after_hrr`: 直前にこの接続へ HRR を送っていれば `true`。`true` の
/// ときは `RetryRequestX25519` を決して返さない（HRR は 1 回のみという
/// 契約を本関数が保証する。#965 は本関数の戻り値をそのまま使うだけで
/// この判定を再実装しない）。
pub fn negotiate(
    ch: &handshake::ClientHello,
    after_hrr: bool,
) -> Result<ClientHelloDecision, ClientHelloError> {
    let ext = parse_extensions(&ch.extensions)?;

    // 1. バージョン（暗号スイート・署名より先に判定する。TLS 1.2 以前の
    //    クライアントには handshake_failure ではなく protocol_version を
    //    返すため）。
    let has_tls13 = match ext.supported_versions {
        Some(data) => contains_tls13(data)?,
        None => false,
    };
    if !has_tls13 {
        return Err(ClientHelloError::UnsupportedVersion);
    }

    // 2. compression。
    check_compression(&ch.legacy_compression_methods)?;

    // 3. 暗号スイート。
    if !ch.cipher_suites.contains(&TLS_AES_128_GCM_SHA256) {
        return Err(ClientHelloError::HandshakeFailure(
            "cipher_suites does not offer TLS_AES_128_GCM_SHA256",
        ));
    }

    // 4. 署名アルゴリズム。
    let signature_algorithms =
        ext.signature_algorithms
            .ok_or(ClientHelloError::MissingExtension(
                "signature_algorithms extension is required",
            ))?;
    if !contains_ed25519_sig(signature_algorithms)? {
        return Err(ClientHelloError::HandshakeFailure(
            "signature_algorithms does not offer ed25519",
        ));
    }

    // 5. グループ／鍵共有。
    let (supported_groups, key_share) = match (ext.supported_groups, ext.key_share) {
        (Some(sg), Some(ks)) => (sg, ks),
        _ => {
            return Err(ClientHelloError::MissingExtension(
                "supported_groups and key_share must both be present",
            ));
        }
    };
    let x25519_share = find_x25519_key_share(key_share)?;
    let has_x25519_group = contains_x25519_group(supported_groups)?;

    let client_x25519_public = match x25519_share {
        Some(key) => {
            if !has_x25519_group {
                return Err(ClientHelloError::IllegalParameter(
                    "key_share offers x25519 but supported_groups does not",
                ));
            }
            key
        }
        None => {
            if !has_x25519_group {
                return Err(ClientHelloError::HandshakeFailure(
                    "no common key exchange group (x25519 not offered)",
                ));
            }
            if after_hrr {
                return Err(ClientHelloError::HandshakeFailure(
                    "client did not include x25519 key_share after HelloRetryRequest",
                ));
            }
            return Ok(ClientHelloDecision::RetryRequestX25519);
        }
    };

    let server_name = match ext.server_name {
        Some(data) => parse_server_name(data)?,
        None => None,
    };

    Ok(ClientHelloDecision::Accept(NegotiatedClientHello {
        client_random: ch.random,
        legacy_session_id: ch.legacy_session_id.clone(),
        client_x25519_public,
        server_name,
    }))
}

/// HelloRetryRequest（`ServerHello` 形。RFC 8446 §4.1.3・§4.1.4・§4.2.8）を
/// 組み立てる。送出と transcript への反映は #965／#964 が担う。
pub fn build_hello_retry_request(legacy_session_id: &[u8]) -> handshake::ServerHello {
    let supported_versions_ext = handshake::Extension {
        extension_type: EXT_SUPPORTED_VERSIONS,
        extension_data: TLS13_VERSION.to_be_bytes().to_vec(),
    };
    let key_share_ext = handshake::Extension {
        extension_type: EXT_KEY_SHARE,
        extension_data: GROUP_X25519.to_be_bytes().to_vec(),
    };
    handshake::ServerHello {
        legacy_version: 0x0303,
        random: HELLO_RETRY_REQUEST_RANDOM,
        legacy_session_id_echo: legacy_session_id.to_vec(),
        cipher_suite: TLS_AES_128_GCM_SHA256,
        legacy_compression_method: 0,
        extensions: vec![supported_versions_ext, key_share_ext],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空白・改行を無視して 16 進文字列をバイト列へ変換する（テスト専用の
    /// 補助関数。受信データ経路ではないため `expect` を使ってよい。
    /// `handshake.rs` のテストの同名関数と同じ実装）。
    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        digits
            .chunks(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("ascii hex pair");
                u8::from_str_radix(text, 16).expect("valid hex byte")
            })
            .collect()
    }

    /// RFC 8448 §3「Simple 1-RTT Handshake」の ClientHello。公開文書
    /// （RFC 8448）由来の値であり `docs/spec`（private）の内容ではない。
    ///
    /// 重要な注意: この ClientHello の `signature_algorithms` は ed25519
    /// (0x0807) を含まない。それ以外（0x1301・0x0304・x25519 の
    /// key_share／supported_groups・compression [0]）は満たすため、
    /// **無改変のこのベクタは負のテスト（signature_algorithms 不一致による
    /// handshake_failure）であり、正常系ではない**。正常系は
    /// `with_ed25519_sig_alg` で signature_algorithms へ 0x0807 を追加した
    /// 派生を使う。
    fn rfc8448_client_hello_bytes() -> Vec<u8> {
        hex(
            "01 00 00 c0 03 03 cb 34 ec b1 e7 81 63 ba 1c 38 c6 da cb 19 6a 6d ff a2 1a 8d 99 12 \
             ec 18 a2 ef 62 83 02 4d ec e7 00 00 06 13 01 13 03 13 02 01 00 00 91 00 00 00 0b 00 \
             09 00 00 06 73 65 72 76 65 72 ff 01 00 01 00 00 0a 00 14 00 12 00 1d 00 17 00 18 00 \
             19 01 00 01 01 01 02 01 03 01 04 00 23 00 00 00 33 00 26 00 24 00 1d 00 20 99 38 1d \
             e5 60 e4 bd 43 d2 3d 8e 43 5a 7d ba fe b3 c0 6e 51 c1 3c ae 4d 54 13 69 1e 52 9a af \
             2c 00 2b 00 03 02 03 04 00 0d 00 20 00 1e 04 03 05 03 06 03 02 03 08 04 08 05 08 06 \
             04 01 05 01 06 01 02 01 04 02 05 02 06 02 02 02 00 2d 00 02 01 01 00 1c 00 02 40 01",
        )
    }

    fn rfc8448_client_hello() -> handshake::ClientHello {
        handshake::ClientHello::parse(&rfc8448_client_hello_bytes()[4..])
            .expect("RFC 8448 vector must parse")
    }

    const RFC8448_CLIENT_X25519_PUBLIC: [u8; 32] = [
        0x99, 0x38, 0x1d, 0xe5, 0x60, 0xe4, 0xbd, 0x43, 0xd2, 0x3d, 0x8e, 0x43, 0x5a, 0x7d, 0xba,
        0xfe, 0xb3, 0xc0, 0x6e, 0x51, 0xc1, 0x3c, 0xae, 0x4d, 0x54, 0x13, 0x69, 0x1e, 0x52, 0x9a,
        0xaf, 0x2c,
    ];

    const RFC8448_CLIENT_RANDOM: [u8; 32] = [
        0xcb, 0x34, 0xec, 0xb1, 0xe7, 0x81, 0x63, 0xba, 0x1c, 0x38, 0xc6, 0xda, 0xcb, 0x19, 0x6a,
        0x6d, 0xff, 0xa2, 0x1a, 0x8d, 0x99, 0x12, 0xec, 0x18, 0xa2, 0xef, 0x62, 0x83, 0x02, 0x4d,
        0xec, 0xe7,
    ];

    /// signature_algorithms へ ed25519(0x0807) を追加した派生。これで
    /// 受理条件をすべて満たす正常系になる。
    fn with_ed25519_sig_alg(mut ch: handshake::ClientHello) -> handshake::ClientHello {
        for ext in &mut ch.extensions {
            if ext.extension_type == EXT_SIGNATURE_ALGORITHMS {
                // 先頭 2 バイトは内側の長さ接頭辞（u16）。0x0807 を 1 エントリ
                // 追加した分だけ書き換える。
                let mut data = ext.extension_data.clone();
                let inner_len = u16::from_be_bytes([data[0], data[1]]);
                data[0..2].copy_from_slice(&(inner_len + 2).to_be_bytes());
                data.extend_from_slice(&SIG_ED25519.to_be_bytes());
                ext.extension_data = data;
            }
        }
        ch
    }

    fn remove_extension(mut ch: handshake::ClientHello, ty: u16) -> handshake::ClientHello {
        ch.extensions.retain(|e| e.extension_type != ty);
        ch
    }

    fn replace_extension_data(
        mut ch: handshake::ClientHello,
        ty: u16,
        data: Vec<u8>,
    ) -> handshake::ClientHello {
        for ext in &mut ch.extensions {
            if ext.extension_type == ty {
                ext.extension_data = data.clone();
            }
        }
        ch
    }

    fn push_extension(
        mut ch: handshake::ClientHello,
        ty: u16,
        data: Vec<u8>,
    ) -> handshake::ClientHello {
        ch.extensions.push(handshake::Extension {
            extension_type: ty,
            extension_data: data,
        });
        ch
    }

    fn empty_key_share_data() -> Vec<u8> {
        // client_shares<0..2^16-1> の長さ 0（空リスト）。
        vec![0x00, 0x00]
    }

    fn non_x25519_key_share_data() -> Vec<u8> {
        // group=0x0017（secp256r1）・key_exchange 長 65 バイト。
        let mut entry = Vec::new();
        entry.extend_from_slice(&0x0017u16.to_be_bytes());
        entry.extend_from_slice(&65u16.to_be_bytes());
        entry.extend_from_slice(&[0u8; 65]);
        let mut out = Vec::new();
        out.extend_from_slice(&(entry.len() as u16).to_be_bytes());
        out.extend_from_slice(&entry);
        out
    }

    // ---- 正常系・HRR ----

    #[test]
    fn accepts_rfc8448_derived_client_hello() {
        let ch = with_ed25519_sig_alg(rfc8448_client_hello());
        let decision = negotiate(&ch, false).expect("must accept");
        let ClientHelloDecision::Accept(negotiated) = decision else {
            panic!("must be Accept");
        };
        assert_eq!(
            negotiated.client_x25519_public,
            RFC8448_CLIENT_X25519_PUBLIC
        );
        assert_eq!(negotiated.client_random, RFC8448_CLIENT_RANDOM);
        assert_eq!(negotiated.server_name, Some(b"server".to_vec()));
        assert!(negotiated.legacy_session_id.is_empty());
    }

    #[test]
    fn accepts_with_unknown_extensions_present() {
        // RFC 8448 §3 のベクタには元から未知の拡張（ff01・0023・002d・
        // 001c）が含まれており、これらが無視されて Accept になることを
        // 確認する（A5: 未知拡張の無視）。
        let ch = with_ed25519_sig_alg(rfc8448_client_hello());
        assert!(ch.extensions.iter().any(|e| e.extension_type == 0xff01));
        assert!(matches!(
            negotiate(&ch, false),
            Ok(ClientHelloDecision::Accept(_))
        ));
    }

    #[test]
    fn empty_key_share_triggers_retry_before_hrr() {
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_KEY_SHARE,
            empty_key_share_data(),
        ));
        assert_eq!(
            negotiate(&ch, false),
            Ok(ClientHelloDecision::RetryRequestX25519)
        );
    }

    #[test]
    fn empty_key_share_after_hrr_is_handshake_failure() {
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_KEY_SHARE,
            empty_key_share_data(),
        ));
        let result = negotiate(&ch, true);
        assert!(matches!(result, Err(ClientHelloError::HandshakeFailure(_))));
        assert_eq!(
            result.unwrap_err().alert_description(),
            AlertDescription::HandshakeFailure
        );
    }

    #[test]
    fn non_x25519_key_share_triggers_retry_before_hrr() {
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_KEY_SHARE,
            non_x25519_key_share_data(),
        ));
        assert_eq!(
            negotiate(&ch, false),
            Ok(ClientHelloDecision::RetryRequestX25519)
        );
    }

    #[test]
    fn non_x25519_key_share_after_hrr_is_handshake_failure() {
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_KEY_SHARE,
            non_x25519_key_share_data(),
        ));
        assert!(matches!(
            negotiate(&ch, true),
            Err(ClientHelloError::HandshakeFailure(_))
        ));
    }

    #[test]
    fn after_hrr_never_returns_retry_request() {
        // after_hrr=true のとき、いかなる条件でも RetryRequestX25519 を
        // 返さない契約を固定する。
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_KEY_SHARE,
            empty_key_share_data(),
        ));
        let result = negotiate(&ch, true);
        assert!(!matches!(
            result,
            Ok(ClientHelloDecision::RetryRequestX25519)
        ));
    }

    #[test]
    fn build_hello_retry_request_matches_rfc8446_structure() {
        let hrr = build_hello_retry_request(&[0xaa, 0xbb]);
        assert_eq!(
            hrr.random,
            engine::crypto::sha256::digest(b"HelloRetryRequest"),
            "HELLO_RETRY_REQUEST_RANDOM must equal SHA-256(\"HelloRetryRequest\")"
        );
        assert_eq!(hrr.legacy_version, 0x0303);
        assert_eq!(hrr.legacy_session_id_echo, vec![0xaa, 0xbb]);
        assert_eq!(hrr.cipher_suite, TLS_AES_128_GCM_SHA256);
        assert_eq!(hrr.legacy_compression_method, 0);
        assert_eq!(hrr.extensions.len(), 2);
        assert_eq!(hrr.extensions[0].extension_type, EXT_SUPPORTED_VERSIONS);
        assert_eq!(
            hrr.extensions[0].extension_data,
            TLS13_VERSION.to_be_bytes()
        );
        assert_eq!(hrr.extensions[1].extension_type, EXT_KEY_SHARE);
        assert_eq!(hrr.extensions[1].extension_data, GROUP_X25519.to_be_bytes());

        let mut out = Vec::new();
        hrr.encode_into(&mut out).expect("must serialize");
        let parsed_raw = {
            let mut buffer = handshake::HandshakeBuffer::new();
            buffer.feed(&out).expect("feed must not error");
            buffer
                .next_message()
                .expect("must parse")
                .expect("must be Some")
        };
        let message =
            handshake::HandshakeMessage::parse(&parsed_raw).expect("must parse typed message");
        let handshake::HandshakeMessage::ServerHello(roundtripped) = message else {
            panic!("must be ServerHello");
        };
        assert_eq!(
            roundtripped, hrr,
            "HRR must round-trip through ServerHello parse"
        );
    }

    // ---- 拒否系 ----

    fn assert_rejected(ch: &handshake::ClientHello, expected: AlertDescription) {
        let result = negotiate(ch, false);
        let err = result.expect_err("must be rejected");
        assert_eq!(
            err.alert_description(),
            expected,
            "unexpected alert for {err:?}"
        );
    }

    #[test]
    fn missing_supported_versions_is_protocol_version() {
        let ch = remove_extension(rfc8448_client_hello(), EXT_SUPPORTED_VERSIONS);
        assert_rejected(&ch, AlertDescription::ProtocolVersion);
    }

    #[test]
    fn supported_versions_without_tls13_is_protocol_version() {
        let ch = replace_extension_data(
            rfc8448_client_hello(),
            EXT_SUPPORTED_VERSIONS,
            vec![0x04, 0x03, 0x03, 0x03, 0x02],
        );
        assert_rejected(&ch, AlertDescription::ProtocolVersion);
    }

    #[test]
    fn omitted_extensions_field_is_protocol_version() {
        // TLS 1.2 以前互換の「拡張フィールドをまるごと省略」形（#953 は
        // 空リストとして受理する）。
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0); // session_id 長 0
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher_suites
        body.push(1);
        body.push(0); // compression [0]
                      // 拡張フィールド省略（残りバイトなし）。
        let ch = handshake::ClientHello::parse(&body).expect("must parse");
        assert!(ch.extensions.is_empty());
        assert_rejected(&ch, AlertDescription::ProtocolVersion);
    }

    #[test]
    fn missing_versions_and_cipher_suite_prefers_protocol_version() {
        // 判定順序の固定: supported_versions が無く、かつ cipher_suites に
        // 0x1301 も無い場合でも protocol_version を返す（handshake_failure
        // ではない）。
        let mut ch = remove_extension(rfc8448_client_hello(), EXT_SUPPORTED_VERSIONS);
        ch.cipher_suites = vec![0x1302, 0x1303];
        assert_rejected(&ch, AlertDescription::ProtocolVersion);
    }

    #[test]
    fn cipher_suites_without_required_suite_is_handshake_failure() {
        let mut ch = rfc8448_client_hello();
        ch.cipher_suites = vec![0x1302, 0x1303];
        assert_rejected(&ch, AlertDescription::HandshakeFailure);
    }

    #[test]
    fn missing_signature_algorithms_is_missing_extension() {
        let ch = remove_extension(rfc8448_client_hello(), EXT_SIGNATURE_ALGORITHMS);
        assert_rejected(&ch, AlertDescription::MissingExtension);
    }

    #[test]
    fn unmodified_rfc8448_vector_lacks_ed25519_is_handshake_failure() {
        // 無改変の RFC 8448 §3 ベクタは ed25519 を含まないため、負の
        // テスト（handshake_failure）であることを明示的に固定する。
        let ch = rfc8448_client_hello();
        assert_rejected(&ch, AlertDescription::HandshakeFailure);
    }

    #[test]
    fn compression_with_extra_method_is_illegal_parameter() {
        let mut ch = with_ed25519_sig_alg(rfc8448_client_hello());
        ch.legacy_compression_methods = vec![0x00, 0x01];
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn compression_without_null_method_is_illegal_parameter() {
        let mut ch = with_ed25519_sig_alg(rfc8448_client_hello());
        ch.legacy_compression_methods = vec![0x01];
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn duplicate_extension_is_illegal_parameter() {
        let base = with_ed25519_sig_alg(rfc8448_client_hello());
        let dup_supported_versions =
            push_extension(base.clone(), EXT_SUPPORTED_VERSIONS, vec![0x02, 0x03, 0x04]);
        assert_rejected(&dup_supported_versions, AlertDescription::IllegalParameter);

        let dup_unknown = push_extension(base, 0xfeed, vec![0x00]);
        let dup_unknown = push_extension(dup_unknown, 0xfeed, vec![0x01]);
        assert_rejected(&dup_unknown, AlertDescription::IllegalParameter);
    }

    #[test]
    fn pre_shared_key_not_last_is_illegal_parameter() {
        let base = with_ed25519_sig_alg(rfc8448_client_hello());
        // pre_shared_key を追加した後にもう 1 拡張を追加し、最後でなくする。
        let ch = push_extension(base, EXT_PRE_SHARED_KEY, vec![0x00, 0x00, 0x00, 0x00]);
        let ch = push_extension(ch, EXT_PSK_KEY_EXCHANGE_MODES, vec![0x01, 0x01]);
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn pre_shared_key_last_without_modes_is_missing_extension() {
        // psk_key_exchange_modes(002d) を削除し、末尾に pre_shared_key を追加。
        let base = remove_extension(
            with_ed25519_sig_alg(rfc8448_client_hello()),
            EXT_PSK_KEY_EXCHANGE_MODES,
        );
        let ch = push_extension(base, EXT_PRE_SHARED_KEY, vec![0x00, 0x00, 0x00, 0x00]);
        assert_rejected(&ch, AlertDescription::MissingExtension);
    }

    #[test]
    fn key_share_x25519_entry_wrong_length_is_illegal_parameter() {
        for len in [31usize, 33] {
            let mut entry = Vec::new();
            entry.extend_from_slice(&GROUP_X25519.to_be_bytes());
            entry.extend_from_slice(&(len as u16).to_be_bytes());
            entry.extend_from_slice(&vec![0u8; len]);
            let mut data = Vec::new();
            data.extend_from_slice(&(entry.len() as u16).to_be_bytes());
            data.extend_from_slice(&entry);
            let ch = with_ed25519_sig_alg(replace_extension_data(
                rfc8448_client_hello(),
                EXT_KEY_SHARE,
                data,
            ));
            assert_rejected(&ch, AlertDescription::IllegalParameter);
        }
    }

    #[test]
    fn key_share_two_x25519_entries_is_illegal_parameter() {
        let mut entry = Vec::new();
        entry.extend_from_slice(&GROUP_X25519.to_be_bytes());
        entry.extend_from_slice(&32u16.to_be_bytes());
        entry.extend_from_slice(&[0u8; 32]);
        let mut data = Vec::new();
        data.extend_from_slice(&((entry.len() * 2) as u16).to_be_bytes());
        data.extend_from_slice(&entry);
        data.extend_from_slice(&entry);
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_KEY_SHARE,
            data,
        ));
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn key_share_without_supported_groups_is_missing_extension() {
        let ch = remove_extension(
            with_ed25519_sig_alg(rfc8448_client_hello()),
            EXT_SUPPORTED_GROUPS,
        );
        assert_rejected(&ch, AlertDescription::MissingExtension);
    }

    #[test]
    fn supported_groups_without_key_share_is_missing_extension() {
        let ch = remove_extension(with_ed25519_sig_alg(rfc8448_client_hello()), EXT_KEY_SHARE);
        assert_rejected(&ch, AlertDescription::MissingExtension);
    }

    #[test]
    fn no_common_group_is_handshake_failure() {
        // supported_groups にも key_share にも x25519 が無い。
        let ch = with_ed25519_sig_alg(rfc8448_client_hello());
        let ch = replace_extension_data(ch, EXT_SUPPORTED_GROUPS, {
            let mut data = Vec::new();
            data.extend_from_slice(&2u16.to_be_bytes());
            data.extend_from_slice(&0x0017u16.to_be_bytes());
            data
        });
        let ch = replace_extension_data(ch, EXT_KEY_SHARE, non_x25519_key_share_data());
        assert_rejected(&ch, AlertDescription::HandshakeFailure);
    }

    #[test]
    fn structural_violations_are_decode_error() {
        let base = with_ed25519_sig_alg(rfc8448_client_hello());

        // supported_versions: 奇数長。
        let ch = replace_extension_data(base.clone(), EXT_SUPPORTED_VERSIONS, vec![0x01, 0x03]);
        assert_rejected(&ch, AlertDescription::DecodeError);

        // supported_versions: 長さ 0（最小 2 未満）。
        let ch = replace_extension_data(base.clone(), EXT_SUPPORTED_VERSIONS, vec![0x00]);
        assert_rejected(&ch, AlertDescription::DecodeError);

        // signature_algorithms: 奇数長。
        let ch = replace_extension_data(
            base.clone(),
            EXT_SIGNATURE_ALGORITHMS,
            vec![0x00, 0x03, 0x08, 0x07, 0x00],
        );
        assert_rejected(&ch, AlertDescription::DecodeError);

        // key_share: 途中で切れたエントリ。
        let ch = replace_extension_data(base.clone(), EXT_KEY_SHARE, {
            let mut data = Vec::new();
            data.extend_from_slice(&4u16.to_be_bytes());
            data.extend_from_slice(&GROUP_X25519.to_be_bytes());
            data.extend_from_slice(&32u16.to_be_bytes());
            // key_exchange 本体を書かず切り詰める。
            data
        });
        assert_rejected(&ch, AlertDescription::DecodeError);

        // server_name: 空リスト（最小 1 未満）。
        let ch = replace_extension_data(base.clone(), EXT_SERVER_NAME, vec![0x00, 0x00]);
        assert_rejected(&ch, AlertDescription::DecodeError);

        // server_name: 長さ 0 の host_name（最小 1 未満）。
        let ch = replace_extension_data(base, EXT_SERVER_NAME, {
            let mut data = Vec::new();
            data.extend_from_slice(&3u16.to_be_bytes());
            data.push(0); // name_type = host_name
            data.extend_from_slice(&0u16.to_be_bytes());
            data
        });
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn duplicate_server_name_host_name_is_illegal_parameter() {
        let data = {
            let mut entry = Vec::new();
            entry.push(0); // name_type = host_name
            entry.extend_from_slice(&1u16.to_be_bytes());
            entry.push(b'a');
            let mut data = Vec::new();
            data.extend_from_slice(&((entry.len() * 2) as u16).to_be_bytes());
            data.extend_from_slice(&entry);
            data.extend_from_slice(&entry);
            data
        };
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_SERVER_NAME,
            data,
        ));
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn truncated_extension_data_never_panics() {
        // 対象 5 拡張それぞれの extension_data を先頭から全長さで切り詰めても
        // panic せず Result が返ることを確認する。
        let base = with_ed25519_sig_alg(rfc8448_client_hello());
        for ty in [
            EXT_SUPPORTED_VERSIONS,
            EXT_KEY_SHARE,
            EXT_SIGNATURE_ALGORITHMS,
            EXT_SERVER_NAME,
            EXT_SUPPORTED_GROUPS,
        ] {
            let original = base
                .extensions
                .iter()
                .find(|e| e.extension_type == ty)
                .expect("extension present")
                .extension_data
                .clone();
            for len in 0..=original.len() {
                let ch = replace_extension_data(base.clone(), ty, original[..len].to_vec());
                let _ = negotiate(&ch, false);
            }
        }
    }

    #[test]
    fn many_unknown_extensions_are_rejected_in_linear_time() {
        // 重複判定がビットマップにより線形時間で完了することの回帰防止
        // （時間はアサーションせず完走することのみ確認する）。
        let mut ch = with_ed25519_sig_alg(rfc8448_client_hello());
        for ty in 0x1000u16..0x1000u16.saturating_add(16_000) {
            ch.extensions.push(handshake::Extension {
                extension_type: ty,
                extension_data: Vec::new(),
            });
        }
        let result = negotiate(&ch, false);
        assert!(matches!(
            result,
            Ok(ClientHelloDecision::Accept(_)) | Err(_)
        ));
    }
}
