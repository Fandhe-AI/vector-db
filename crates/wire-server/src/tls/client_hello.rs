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
//!   実送出・切断を担う。本モジュールは 1 回目の `ClientHello`（`Option`。
//!   HRR 未送出なら `None`）を引数で受け取るだけで状態は一切持たない
//!   （#965 が「HRR を送ったか」と共に 1 回目の `ClientHello` を保持し、
//!   2 回目の呼び出しへそのまま渡す）。「HRR は 1 回のみ」の判定・2 回目
//!   `ClientHello` が 1 回目と（許可された差分を除き）同一であることの
//!   検証（RFC 8446 §4.1.2）はいずれも本モジュールが単一情報源として持ち、
//!   #965 側で再実装しない
//! - [`super::transcript`]（Issue #964。transcript hash）: HRR 時の
//!   `message_hash` 置換を担う。本モジュールは関与しない
//! - #955（X25519）: 共有秘密の計算・全ゼロ検出を担う。本モジュールは
//!   client 公開鍵 32 バイトを取り出して渡すだけ
//! - #959／#965（0-RTT）: 0-RTT を受理した場合の早期 application_data
//!   レコードの処理を担う。本モジュールは PSK／0-RTT を受理せず、
//!   `pre_shared_key`・`psk_key_exchange_modes` は RFC 8446 §4.2.11・§4.2.9
//!   の MUST（位置・併存）を、`early_data` は RFC 8446 §4.2.10 の MUST
//!   （`pre_shared_key` との併存）を検査するのみで中身は解釈しない
//!   （0-RTT データの送出自体は行わない）
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
const EXT_PADDING: u16 = 21;
const EXT_EARLY_DATA: u16 = 42;
const EXT_PRE_SHARED_KEY: u16 = 41;
const EXT_SUPPORTED_VERSIONS: u16 = 43;
const EXT_PSK_KEY_EXCHANGE_MODES: u16 = 45;
const EXT_KEY_SHARE: u16 = 51;

/// HelloRetryRequest 後の 2 回目 `ClientHello` で、他拡張との型・値の
/// 完全一致比較（[`check_hrr_consistency`]）から除外する拡張（RFC 8446
/// §4.1.2 が列挙する 4 種）。ただし「比較から除外」＝「何でも許可」では
/// ない非対称な拡張が含まれる点に注意（各拡張の実際の制約は
/// [`check_hrr_consistency`] 側の追加検査を参照）:
/// - `key_share`: 要求したグループ 1 件のみへの**置き換え**が必須
///   （[`find_x25519_key_share`] が担う）
/// - `early_data`: HRR 後の 0-RTT は禁止のため、1 回目にあったかどうかに
///   関わらず 2 回目には**含まれてはならない**（削除のみ許可・追加や
///   維持は許可されない。[`check_hrr_consistency`] が明示的に拒否する）
/// - `pre_shared_key`: binder の再計算・非互換 PSK の削除（全削除を含む）
///   のみ許可。本モジュールは PSK／0-RTT を受理しないため内容は解釈せず
///   位置検査（RFC 8446 §4.2.11）のみ行う
/// - `padding`: 追加・削除・長さ変更いずれも自由
///
/// `cookie`（44）は「HRR が `cookie` を提供していた場合にのみ追加して
/// よい」対象だが、本モジュールの `build_hello_retry_request` は
/// `cookie` を送出しないため、2 回目に `cookie` が現れることは許可された
/// 差分ではなく `IllegalParameter` のまま拒否する（fail-closed）。
/// `psk_key_exchange_modes` は RFC の例外一覧に含まれない（`pre_shared_key`
/// と併存する側の拡張であり、値の更新は許可されていない）ため対象外。
const HRR_MUTABLE_EXTENSIONS: [u16; 4] = [
    EXT_KEY_SHARE,
    EXT_PADDING,
    EXT_EARLY_DATA,
    EXT_PRE_SHARED_KEY,
];

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
/// PSK を採用しない場合でも、受理判定に使う `pre_shared_key`・
/// `psk_key_exchange_modes`・`early_data` はここで完全に構造解析する
/// （codex-review PR #1022 P0 指摘。値だけを見て構造を検証しないまま
/// 受理すると、binder 欠落・切断・余剰データを含む不正な拡張を
/// 通してしまう）。
struct ParsedExtensions<'a> {
    supported_versions: Option<&'a [u8]>,
    key_share: Option<&'a [u8]>,
    signature_algorithms: Option<&'a [u8]>,
    server_name: Option<&'a [u8]>,
    supported_groups: Option<&'a [u8]>,
    has_pre_shared_key: bool,
    pre_shared_key_is_last: bool,
    has_psk_key_exchange_modes: bool,
    has_early_data: bool,
}

/// u16 値集合の重複検出に使うビットマップ（65536 bit・8 KiB）。
/// `extension_type`（拡張の重複検出）・`NamedGroup`（`key_share` 内の
/// グループ重複検出）・`name_type`（`server_name` 内の重複検出。u8 を
/// そのまま u16 として渡す）のいずれも同じ表現で扱える。値の種類数が
/// 多くても O(n) で判定できる（O(n^2) の総当たり走査を避ける設計。
/// DoS 耐性の観点）。
struct SeenU16Set {
    bits: [u64; 1024],
}

impl SeenU16Set {
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

    /// `ty` を記録済みなら `true`（`mark_and_check_duplicate` と異なり状態を
    /// 変更しない読み取り専用の判定）。`key_share` の各グループが
    /// `supported_groups` に含まれるかの判定（RFC 8446 §4.2.8）に使う。
    fn contains(&self, ty: u16) -> bool {
        let idx = usize::from(ty) / 64;
        let bit = usize::from(ty) % 64;
        let mask = 1u64 << bit;
        // idx は必ず 0..1024 に収まる（u16 の最大値 65535 / 64 = 1023）ため
        // 到達しないが、添字アクセスを避けるため fail-closed（未記録扱い）
        // に倒す。
        self.bits.get(idx).is_some_and(|word| (*word & mask) != 0)
    }
}

fn parse_extensions(
    extensions: &[handshake::Extension],
) -> Result<ParsedExtensions<'_>, ClientHelloError> {
    let mut seen = SeenU16Set::new();
    let mut out = ParsedExtensions {
        supported_versions: None,
        key_share: None,
        signature_algorithms: None,
        server_name: None,
        supported_groups: None,
        has_pre_shared_key: false,
        pre_shared_key_is_last: false,
        has_psk_key_exchange_modes: false,
        has_early_data: false,
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
                // PSK を採用しない場合でも identities/binders の対応関係
                // （件数一致・各 binder 長 32..=255・余剰データ無し）を
                // ここで構造解析して確定させる（RFC 8446 §4.2.11）。
                // 結果（identities）は本関数では使わず捨てるが、構造検証
                // 自体が目的（codex-review PR #1022 P0 指摘）。
                parse_pre_shared_key(&ext.extension_data)?;
            }
            EXT_PSK_KEY_EXCHANGE_MODES => {
                out.has_psk_key_exchange_modes = true;
                validate_psk_key_exchange_modes(&ext.extension_data)?;
            }
            EXT_EARLY_DATA => {
                out.has_early_data = true;
                validate_client_hello_early_data(&ext.extension_data)?;
            }
            _ => {
                // その他の未知の拡張は中身を見ずに無視する。早期
                // application_data レコードの破棄は #959／#965 が担う。
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
    // RFC 8446 §4.2.10: early_data は PSK（0-RTT）と併せてのみ提示できる。
    // 本モジュールは PSK／0-RTT を受理しないため中身は解釈しないが、
    // pre_shared_key を伴わない early_data はそれ自体がプロトコル違反
    // （0-RTT の前提となる PSK が無いのに 0-RTT を提示している）である。
    // psk_key_exchange_modes が pre_shared_key に対して必須拡張として
    // 扱われている（上記 §4.2.9 の分岐）のと同型の「拡張 A の提示が
    // 拡張 B の必須化を意味する」依存関係のため、同じ missing_extension
    // （RFC 8446 §9.2）として扱う（codex-review PR #1022 指摘）。
    if out.has_early_data && !out.has_pre_shared_key {
        return Err(ClientHelloError::MissingExtension(
            "early_data extension requires a pre_shared_key extension to be present",
        ));
    }
    Ok(out)
}

/// `psk_key_exchange_modes`（ClientHello 形。RFC 8446 §4.2.9。
/// `PskKeyExchangeMode ke_modes<1..255>`）の構造のみを検証する。本モジュール
/// は PSK を採用しないため個々の `PskKeyExchangeMode` 値は解釈しない
/// （未知の値も拒否しない。拡張可能な列挙のため）。
fn validate_psk_key_exchange_modes(data: &[u8]) -> Result<(), ClientHelloError> {
    let mut r = Reader::new(data);
    let _ke_modes = r.vec_u8_len(1, 255)?;
    r.expect_end()?;
    Ok(())
}

/// ClientHello 内の `early_data`（RFC 8446 §4.2.10）は本体を持たない
/// （`Empty` 型）ため、`extension_data` が空バイト列であることのみを
/// 検証する。
fn validate_client_hello_early_data(data: &[u8]) -> Result<(), ClientHelloError> {
    if data.is_empty() {
        Ok(())
    } else {
        Err(ClientHelloError::Decode(
            "early_data extension body must be empty in ClientHello",
        ))
    }
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

/// [`parse_supported_groups`] の結果。`membership` は `key_share` の各
/// エントリが提示されたグループ集合に含まれるかの判定（RFC 8446 §4.2.8）
/// に使う。
struct SupportedGroups {
    has_x25519: bool,
    membership: SeenU16Set,
}

/// `supported_groups`（`NamedGroup named_group_list<2..2^16-1>`）を検証する。
/// x25519(0x001d) を含むかの判定に加え、同一 `NamedGroup` が重複して列挙
/// されていないかも検査する（RFC 8446 §4.2.7 はクライアントが対応する
/// グループを列挙するリストであり、同一グループの重複列挙は
/// `key_share` の重複エントリ検査（[`find_x25519_key_share`]）と同様に
/// 構造上意味を持たないため `IllegalParameter` とする）。
fn parse_supported_groups(data: &[u8]) -> Result<SupportedGroups, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u16_len(2, 0xFFFF)?;
    r.expect_end()?;
    if list.len() % 2 != 0 {
        return Err(ClientHelloError::Decode(
            "supported_groups list length must be even",
        ));
    }
    let mut lr = Reader::new(list);
    let mut membership = SeenU16Set::new();
    let mut has_x25519 = false;
    while lr.remaining() > 0 {
        let group = lr.u16()?;
        if membership.mark_and_check_duplicate(group) {
            return Err(ClientHelloError::IllegalParameter(
                "supported_groups contains a duplicate NamedGroup",
            ));
        }
        if group == GROUP_X25519 {
            has_x25519 = true;
        }
    }
    Ok(SupportedGroups {
        has_x25519,
        membership,
    })
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
/// 同一 `NamedGroup`（x25519 に限らず）のエントリが 2 件以上あれば
/// `IllegalParameter`（RFC 8446 §4.2.8「クライアントは同一グループに
/// つき高々 1 個の `KeyShareEntry` を送ってよい」）。
///
/// `reject_non_x25519`: `true`（HelloRetryRequest 後の 2 回目
/// `ClientHello`）のときは x25519 以外のグループのエントリが 1 件でも
/// あれば `IllegalParameter`（RFC 8446 §4.1.2: 2 回目の `key_share` は
/// HRR が要求した唯一のグループのみを含まなければならない）。
///
/// `supported_groups`: 同時に受信した `supported_groups` 拡張の会員判定
/// （[`parse_supported_groups`]）。x25519 に限らずすべてのエントリについて
/// `group` がここに含まれることを検証する（RFC 8446 §4.2.8「`key_share`
/// が提示するグループはいずれも `supported_groups` に含まれていなければ
/// ならない」）。
fn find_x25519_key_share(
    data: &[u8],
    reject_non_x25519: bool,
    supported_groups: &SeenU16Set,
) -> Result<Option<[u8; 32]>, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u16_len(0, 0xFFFF)?;
    r.expect_end()?;
    let mut lr = Reader::new(list);
    let mut seen_groups = SeenU16Set::new();
    let mut found: Option<[u8; 32]> = None;
    while lr.remaining() > 0 {
        let group = lr.u16()?;
        let key_exchange = lr.vec_u16_len(1, 0xFFFF)?;
        if seen_groups.mark_and_check_duplicate(group) {
            return Err(ClientHelloError::IllegalParameter(
                "key_share contains more than one entry for the same group",
            ));
        }
        if !supported_groups.contains(group) {
            return Err(ClientHelloError::IllegalParameter(
                "key_share entry offers a group not present in supported_groups",
            ));
        }
        if group == GROUP_X25519 {
            let arr: [u8; 32] = key_exchange.try_into().map_err(|_| {
                ClientHelloError::IllegalParameter(
                    "key_share x25519 entry key_exchange must be 32 bytes",
                )
            })?;
            found = Some(arr);
        } else if reject_non_x25519 {
            return Err(ClientHelloError::IllegalParameter(
                "key_share after HelloRetryRequest must only offer the requested group",
            ));
        }
    }
    Ok(found)
}

/// `server_name`（RFC 6066 §3. `ServerName server_name_list<1..2^16-1>`）を
/// 検証し、`host_name`（name_type=0）を返す（無ければ `None`）。
/// 同一 `name_type`（`host_name` に限らずいずれの型でも）が重複していれば
/// `IllegalParameter`（RFC 6066 §3「同一 name_type は 1 つまで」は
/// `host_name` 限定の制約ではない）。未知の name_type は不透明データとして
/// 読み飛ばす。`host_name` は [`validate_host_name`] で構文検証してから
/// 返す（受理したまま未検証のバイト列を上位へ渡さない。codex-review
/// PR #1022 P1 指摘）。
fn parse_server_name(data: &[u8]) -> Result<Option<Vec<u8>>, ClientHelloError> {
    let mut r = Reader::new(data);
    let list = r.vec_u16_len(1, 0xFFFF)?;
    r.expect_end()?;
    let mut lr = Reader::new(list);
    let mut host_name: Option<Vec<u8>> = None;
    let mut seen_name_types = SeenU16Set::new();
    while lr.remaining() > 0 {
        let name_type = lr.u8()?;
        let name = lr.vec_u16_len(1, 0xFFFF)?;
        if seen_name_types.mark_and_check_duplicate(u16::from(name_type)) {
            return Err(ClientHelloError::IllegalParameter(
                "server_name contains more than one entry for the same name_type",
            ));
        }
        if name_type == 0 {
            validate_host_name(name)?;
            host_name = Some(name.to_vec());
        }
    }
    Ok(host_name)
}

/// `host_name`（RFC 6066 §3. DNS ホスト名。RFC 1034 準拠）の構文を検証する。
/// RFC 6066 §3 は「literal IPv4 and IPv6 addresses are not permitted in
/// "HostName"」と明記しており、DNS ホスト名として無効な値（IP リテラル・
/// 空ラベル・ラベル内の許可されない文字・全長超過等）は fail-closed に
/// `IllegalParameter` として拒否する（codex-review PR #1022 P1 指摘。
/// 未検証のバイト列を選択・ログ出力に使わない設計自体は不変。
/// [`NegotiatedClientHello::server_name`] のドキュメントコメント参照）。
fn validate_host_name(name: &[u8]) -> Result<(), ClientHelloError> {
    if name.is_empty() || name.len() > 253 {
        return Err(ClientHelloError::IllegalParameter(
            "server_name host_name length must be 1..=253 bytes",
        ));
    }
    if !name.is_ascii() {
        return Err(ClientHelloError::IllegalParameter(
            "server_name host_name must consist of ASCII bytes",
        ));
    }
    if name.ends_with(b".") {
        return Err(ClientHelloError::IllegalParameter(
            "server_name host_name must not end with a trailing dot",
        ));
    }
    if is_ip_literal(name) {
        return Err(ClientHelloError::IllegalParameter(
            "server_name host_name must not be an IPv4/IPv6 literal",
        ));
    }
    for label in name.split(|&b| b == b'.') {
        if label.is_empty() {
            return Err(ClientHelloError::IllegalParameter(
                "server_name host_name must not contain an empty label",
            ));
        }
        if label.len() > 63 {
            return Err(ClientHelloError::IllegalParameter(
                "server_name host_name label must be 1..=63 bytes",
            ));
        }
        if label.first() == Some(&b'-') || label.last() == Some(&b'-') {
            return Err(ClientHelloError::IllegalParameter(
                "server_name host_name label must not start or end with a hyphen",
            ));
        }
        if !label
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(ClientHelloError::IllegalParameter(
                "server_name host_name label must contain only ASCII alphanumerics and hyphens",
            ));
        }
    }
    Ok(())
}

/// `name` が IPv4／IPv6 の literal アドレス表記であるかを判定する
/// （RFC 6066 §3 は `HostName` に IP リテラルを許可しない）。IPv6 は
/// `:` を含むことで判定し（DNS ホスト名のラベルは `:` を許可しない）、
/// IPv4 はドット区切り 4 要素すべてが数字のみのラベルであることで判定
/// する（`999.999.999.999` のような桁数・値域違反も、DNS ホスト名としての
/// 妥当性検査ではなく「数字のみの 4 ラベル」という形自体を IP リテラルの
/// 試みとみなして拒否する。数値域の妥当性まで見る必要はない）。
fn is_ip_literal(name: &[u8]) -> bool {
    if name.contains(&b':') {
        return true;
    }
    let labels: Vec<&[u8]> = name.split(|&b| b == b'.').collect();
    labels.len() == 4
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.iter().all(u8::is_ascii_digit))
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

/// HelloRetryRequest 後の 2 回目 `ClientHello` が、1 回目の `ClientHello`
/// と（許可された差分を除き）同一であることを検証する（RFC 8446
/// §4.1.2「クライアントは HelloRetryRequest への応答として、以下を除いて
/// 変更を加えていない `ClientHello` を送らなければならない」）。
/// [`HRR_MUTABLE_EXTENSIONS`] に列挙した拡張（`key_share`・`early_data`・
/// `pre_shared_key`・`padding`）は型・値の完全一致比較の対象外とするが、
/// `early_data` は追加・維持を許可する趣旨ではなく削除のみが許可される
/// 非対称な拡張のため、比較除外とは別に 2 回目の `ClientHello` に
/// 含まれていないことを明示的に検査する（0-RTT は HRR 後には許可されない。
/// RFC 8446 §4.1.2「Removing the "early_data" extension ... Early data is
/// not permitted after a HelloRetryRequest」）。それ以外の拡張は型・値
/// ともに完全一致でなければ `IllegalParameter`。`legacy_version`・
/// `random`・`legacy_session_id`・`cipher_suites`・
/// `legacy_compression_methods` も同一でなければならない。
fn check_hrr_consistency(
    first: &handshake::ClientHello,
    second: &handshake::ClientHello,
) -> Result<(), ClientHelloError> {
    if first.legacy_version != second.legacy_version
        || first.random != second.random
        || first.legacy_session_id != second.legacy_session_id
        || first.cipher_suites != second.cipher_suites
        || first.legacy_compression_methods != second.legacy_compression_methods
    {
        return Err(ClientHelloError::IllegalParameter(
            "second ClientHello after HelloRetryRequest must repeat the first \
             ClientHello's version/random/session id/cipher suites/compression",
        ));
    }
    if second
        .extensions
        .iter()
        .any(|e| e.extension_type == EXT_EARLY_DATA)
    {
        // early_data は「比較除外＝何を送ってもよい」対象ではなく、削除
        // のみが許可された非対称な拡張。1 回目の有無に関わらず 2 回目に
        // 存在すること自体が違反（0-RTT は HRR 後には許可されないため）。
        return Err(ClientHelloError::IllegalParameter(
            "early_data extension must not be present in the second ClientHello \
             after HelloRetryRequest",
        ));
    }
    fn immutable(exts: &[handshake::Extension]) -> Vec<&handshake::Extension> {
        exts.iter()
            .filter(|e| !HRR_MUTABLE_EXTENSIONS.contains(&e.extension_type))
            .collect()
    }
    let a = immutable(&first.extensions);
    let b = immutable(&second.extensions);
    if a.len() != b.len() || a.iter().zip(b.iter()).any(|(x, y)| *x != *y) {
        return Err(ClientHelloError::IllegalParameter(
            "second ClientHello after HelloRetryRequest changed an extension \
             that must remain identical",
        ));
    }
    check_hrr_psk_identities(first, second)?;
    Ok(())
}

/// [`parse_pre_shared_key`] の戻り値。`identities` は比較・再送検証に
/// 使う値のみを保持し（`obfuscated_ticket_age` は捨てる。理由は関数
/// ドキュメント参照）、`binders` は件数対応の検証にのみ使い内容は解釈しない
/// （binder の再計算が許可されているため。RFC 8446 §4.1.2）。
struct PreSharedKey {
    identities: Vec<Vec<u8>>,
}

/// `pre_shared_key`（ClientHello 形。RFC 8446 §4.2.11）を完全に構造解析する。
///
/// ```text
/// struct {
///     PskIdentity identities<7..2^16-1>;
///     PskBinderEntry binders<33..2^16-1>;
/// } PreSharedKeyExtension;
/// PskIdentity: opaque identity<1..2^16-1>; uint32 obfuscated_ticket_age;
/// PskBinderEntry: opaque binder<32..255>;
/// ```
///
/// PSK を採用しない場合でも、以下をすべて検証してから `identities` を
/// 返す（未検証のまま受理すると binder 欠落・切断・余剰データを含む
/// 不正な `pre_shared_key` を通してしまう。codex-review PR #1022 P0 指摘）:
/// - `identities`・`binders` それぞれのベクタ長・各エントリの境界
///   （`vec_u16_len`／`vec_u8_len` の範囲検査が担う）
/// - 拡張全体の終端（`identities` と `binders` の 2 ベクタ以外の余剰
///   バイトが無いこと。`r.expect_end()`）
/// - `identities` と `binders` の件数が一致すること（RFC 8446 §4.2.11
///   「total number of identities and binders MUST be equal」）
///
/// `obfuscated_ticket_age` はここで読み進めて構造を検証するが、値そのものは
/// 戻り値に含めない（HRR 後の 2 回目 ClientHello では、ticket 発行からの
/// 経過時間が変化するため、クライアントは同一 PSK に対しても age を
/// 再計算してよい。RFC 8446 §4.1.2。[`check_hrr_psk_identities`] の同一性
/// 比較は identity 値のみで行い、age の一致は要求しない。Cursor Bugbot
/// 指摘・PR #1022）。`binders` の中身（バイト列そのもの）も同様に比較対象に
/// しない（binder の再計算が許可されているため）ので読み進めるだけで
/// 保持しない。
fn parse_pre_shared_key(data: &[u8]) -> Result<PreSharedKey, ClientHelloError> {
    let mut r = Reader::new(data);
    let identities_data = r.vec_u16_len(7, 0xFFFF)?;
    let mut ir = Reader::new(identities_data);
    let mut identities = Vec::new();
    while ir.remaining() > 0 {
        let identity = ir.vec_u16_len(1, 0xFFFF)?;
        let _obfuscated_ticket_age = ir.array::<4>()?;
        identities.push(identity.to_vec());
    }
    let binders_data = r.vec_u16_len(33, 0xFFFF)?;
    r.expect_end()?;
    let mut br = Reader::new(binders_data);
    let mut binder_count = 0usize;
    while br.remaining() > 0 {
        let _binder = br.vec_u8_len(32, 255)?;
        binder_count = binder_count.checked_add(1).ok_or(ClientHelloError::Decode(
            "pre_shared_key binders count overflow",
        ))?;
    }
    if identities.len() != binder_count {
        return Err(ClientHelloError::Decode(
            "pre_shared_key identities and binders count must match",
        ));
    }
    Ok(PreSharedKey { identities })
}

/// `second` が `first` の順序保存部分列であるかを判定する（RFC 8446
/// §4.1.2「クライアントは（更新された）2 回目の `ClientHello` のパラメータ
/// と非互換になった PSK identity を削除してよい」の反映）。削除のみが
/// 許可されるため、`first` に存在しない identity（値の完全一致で比較。
/// `obfuscated_ticket_age` は比較対象に含めない。[`parse_pre_shared_key`]
/// のドキュメントコメント参照）の新規追加・並べ替えは部分列とはみなさず
/// `false` を返す。
fn is_ordered_subsequence(first: &[Vec<u8>], second: &[Vec<u8>]) -> bool {
    let mut first_iter = first.iter();
    for entry in second {
        let matched = first_iter.by_ref().any(|candidate| candidate == entry);
        if !matched {
            return false;
        }
    }
    true
}

/// HRR 後の 2 回目 `ClientHello` の `pre_shared_key` が、RFC 8446 §4.1.2 の
/// 許可された更新（binder の再計算・非互換 PSK の削除。全削除を含む）の
/// 範囲に収まっていることを検証する。`pre_shared_key` は
/// [`HRR_MUTABLE_EXTENSIONS`] により型・値の完全一致比較（本関数の呼び
/// 出し元）からは除外されているが、「比較除外＝何を送ってもよい」ではない
/// 非対称な拡張であることは `early_data` と同様（本関数のドキュメント
/// コメント上部を参照）。
/// - 1 回目に無かった `pre_shared_key` を 2 回目で新規追加するのは許可
///   されない（RFC 8446 は「更新」としてのみ言及しており新規提示は
///   想定していない）
/// - 両方にある場合、2 回目の `identities` は 1 回目の順序保存部分列
///   でなければならない（新規 identity の追加・並べ替えは不可）。
///   `binders` は内容を検証しない（再計算が許可されているため）
fn check_hrr_psk_identities(
    first: &handshake::ClientHello,
    second: &handshake::ClientHello,
) -> Result<(), ClientHelloError> {
    fn find_psk(exts: &[handshake::Extension]) -> Option<&[u8]> {
        exts.iter()
            .find(|e| e.extension_type == EXT_PRE_SHARED_KEY)
            .map(|e| e.extension_data.as_slice())
    }
    match (find_psk(&first.extensions), find_psk(&second.extensions)) {
        (None, None) | (Some(_), None) => Ok(()),
        (None, Some(_)) => Err(ClientHelloError::IllegalParameter(
            "second ClientHello must not add a pre_shared_key extension \
             that was absent from the first ClientHello",
        )),
        (Some(first_data), Some(second_data)) => {
            let first_identities = parse_pre_shared_key(first_data)?.identities;
            let second_identities = parse_pre_shared_key(second_data)?.identities;
            if !is_ordered_subsequence(&first_identities, &second_identities) {
                return Err(ClientHelloError::IllegalParameter(
                    "second ClientHello's pre_shared_key identities must be an \
                     order-preserving subsequence of the first ClientHello's \
                     (only removing incompatible PSKs is permitted)",
                ));
            }
            Ok(())
        }
    }
}

/// `ClientHello` を受理条件（TLS 1.3・`TLS_AES_128_GCM_SHA256`・X25519・
/// Ed25519 のみ）で判定する。判定順序はこの関数が単一情報源であり、
/// `docs/design/tls-client-hello.md` の記述もこれに従う。
///
/// `after_hrr`: 直前にこの接続へ HRR を送っていれば、その原因となった
/// 1 回目の `ClientHello` を `Some` で渡す（未送出なら `None`）。`Some`
/// のときは [`check_hrr_consistency`] による同一性検証を行った上で
/// `RetryRequestX25519` を決して返さない（HRR は 1 回のみという契約を
/// 本関数が保証する。#965 は 1 回目の `ClientHello` を保持して本関数の
/// 戻り値をそのまま使うだけで、この判定を再実装しない）。
pub fn negotiate(
    ch: &handshake::ClientHello,
    after_hrr: Option<&handshake::ClientHello>,
) -> Result<ClientHelloDecision, ClientHelloError> {
    let ext = parse_extensions(&ch.extensions)?;

    // 1. HRR 後の 2 回目 ClientHello は、鍵交換に関わる差分を除き 1 回目と
    //    同一でなければならない（RFC 8446 §4.1.2）。他のどの意味検査より
    //    先に実行する。後段の検査（missing_extension・handshake_failure
    //    等）を先に走らせると、同一性違反があるにもかかわらず別の alert
    //    種別で応答してしまい、RFC が要求する illegal_parameter を返せない
    //    ため（本関数はこの優先順位も含めて単一情報源とする）。
    if let Some(first) = after_hrr {
        check_hrr_consistency(first, ch)?;
    }

    // 2. legacy_version（RFC 8446 §4.1.2 の MUST。TLS 1.3 クライアントは
    //    0x0303 固定で送らなければならない）。TLS 1.0/1.1 の
    //    legacy_version（0x0301/0x0302）を送る旧クライアントもここで
    //    protocol_version として拒否されるが、そうしたクライアントは
    //    supported_versions 自体も欠くため、この判定が無くても次の
    //    supported_versions 判定で同じ protocol_version に到達する
    //    （分類結果は変わらない）。
    if ch.legacy_version != 0x0303 {
        return Err(ClientHelloError::UnsupportedVersion);
    }

    // 3. バージョン（暗号スイート・署名より先に判定する。TLS 1.2 以前の
    //    クライアントには handshake_failure ではなく protocol_version を
    //    返すため）。
    let has_tls13 = match ext.supported_versions {
        Some(data) => contains_tls13(data)?,
        None => false,
    };
    if !has_tls13 {
        return Err(ClientHelloError::UnsupportedVersion);
    }

    // 4. compression。
    check_compression(&ch.legacy_compression_methods)?;

    // 5. 暗号スイート。
    if !ch.cipher_suites.contains(&TLS_AES_128_GCM_SHA256) {
        return Err(ClientHelloError::HandshakeFailure(
            "cipher_suites does not offer TLS_AES_128_GCM_SHA256",
        ));
    }

    // 6. 署名アルゴリズム。
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

    // 7. server_name の構造検証。HRR で戻る（Accept に到達しない）経路
    //    でも必ず実行する（不正な server_name を含む ClientHello に対して
    //    RetryRequestX25519 を返してしまわないため）。
    let server_name = match ext.server_name {
        Some(data) => parse_server_name(data)?,
        None => None,
    };

    // 8. グループ／鍵共有。
    let (supported_groups, key_share) = match (ext.supported_groups, ext.key_share) {
        (Some(sg), Some(ks)) => (sg, ks),
        _ => {
            return Err(ClientHelloError::MissingExtension(
                "supported_groups and key_share must both be present",
            ));
        }
    };
    // supported_groups を先に解析し、その会員判定を key_share の各エントリ
    // （x25519 に限らずすべて）の検証へ渡す（RFC 8446 §4.2.8）。
    let groups = parse_supported_groups(supported_groups)?;
    let x25519_share = find_x25519_key_share(key_share, after_hrr.is_some(), &groups.membership)?;
    let has_x25519_group = groups.has_x25519;

    let client_x25519_public = match x25519_share {
        // x25519 エントリが見つかった時点で、その group（x25519）が
        // supported_groups に含まれることは find_x25519_key_share の会員
        // チェックで既に保証されている（`has_x25519_group` との重複検査は
        // 行わない）。
        Some(key) => key,
        None => {
            if !has_x25519_group {
                return Err(ClientHelloError::HandshakeFailure(
                    "no common key exchange group (x25519 not offered)",
                ));
            }
            if after_hrr.is_some() {
                // RFC 8446 §4.2.8: HRR で要求したグループの key_share を
                // 2 回目の ClientHello に含めないのは、鍵交換の合意に
                // 失敗した（handshake_failure）のではなく、クライアントが
                // サーバーの要求に従わなかったプロトコル違反であるため
                // illegal_parameter。
                return Err(ClientHelloError::IllegalParameter(
                    "client did not include x25519 key_share after HelloRetryRequest",
                ));
            }
            return Ok(ClientHelloDecision::RetryRequestX25519);
        }
    };

    Ok(ClientHelloDecision::Accept(NegotiatedClientHello {
        client_random: ch.random,
        legacy_session_id: ch.legacy_session_id.clone(),
        client_x25519_public,
        server_name,
    }))
}

/// HelloRetryRequest（`ServerHello` 形。RFC 8446 §4.1.3・§4.1.4・§4.2.8）を
/// 組み立てる。送出は #965、transcript への反映は
/// [`super::transcript::Transcript::append_hello_retry_request`]（Issue #964）
/// が担う。
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
        let decision = negotiate(&ch, None).expect("must accept");
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
            negotiate(&ch, None),
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
            negotiate(&ch, None),
            Ok(ClientHelloDecision::RetryRequestX25519)
        );
    }

    #[test]
    fn empty_key_share_after_hrr_is_illegal_parameter() {
        // 1 回目の ClientHello（`first`）は key_share 以外が 2 回目と一致
        // していなければならない（RFC 8446 §4.1.2。key_share 自体は比較
        // 対象外のため、1 回目の内容は無関係）。HRR で要求したグループの
        // key_share を欠くのは鍵交換の不合意（handshake_failure）ではなく
        // クライアントのプロトコル違反のため illegal_parameter
        // （RFC 8446 §4.2.8）。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let ch = replace_extension_data(first.clone(), EXT_KEY_SHARE, empty_key_share_data());
        let result = negotiate(&ch, Some(&first));
        assert!(matches!(result, Err(ClientHelloError::IllegalParameter(_))));
        assert_eq!(
            result.unwrap_err().alert_description(),
            AlertDescription::IllegalParameter
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
            negotiate(&ch, None),
            Ok(ClientHelloDecision::RetryRequestX25519)
        );
    }

    #[test]
    fn non_x25519_key_share_after_hrr_is_illegal_parameter() {
        // HRR 後の key_share は要求した唯一のグループ（x25519）のみを
        // 含まなければならない（RFC 8446 §4.1.2）。secp256r1 のみを
        // 提示するのは illegal_parameter（x25519 が無いだけの
        // handshake_failure ではない）。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let ch = replace_extension_data(first.clone(), EXT_KEY_SHARE, non_x25519_key_share_data());
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn after_hrr_never_returns_retry_request() {
        // after_hrr=Some のとき、いかなる条件でも RetryRequestX25519 を
        // 返さない契約を固定する。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let ch = replace_extension_data(first.clone(), EXT_KEY_SHARE, empty_key_share_data());
        let result = negotiate(&ch, Some(&first));
        assert!(!matches!(
            result,
            Ok(ClientHelloDecision::RetryRequestX25519)
        ));
    }

    #[test]
    fn legacy_version_mismatch_is_protocol_version() {
        // RFC 8446 §4.1.2: legacy_version は 0x0303 固定でなければ
        // ならない。supported_versions に TLS 1.3 があっても拒否する。
        let mut ch = with_ed25519_sig_alg(rfc8448_client_hello());
        ch.legacy_version = 0x0301;
        assert_rejected(&ch, AlertDescription::ProtocolVersion);
    }

    #[test]
    fn hrr_second_hello_changing_cipher_suites_is_illegal_parameter() {
        // 2 回目の ClientHello は key_share 等の許可された差分を除き
        // 1 回目と同一でなければならない（RFC 8446 §4.1.2）。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let mut ch = first.clone();
        ch.cipher_suites = vec![0x1302, TLS_AES_128_GCM_SHA256];
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_changing_random_is_illegal_parameter() {
        // 「変更を加えていない ClientHello」の対象は random も含む
        // （RFC 8446 §4.1.2 は random を許可された例外に挙げていない）。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let mut ch = first.clone();
        ch.random = [0xab; 32];
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_changing_psk_key_exchange_modes_is_illegal_parameter() {
        // psk_key_exchange_modes は RFC 8446 §4.1.2 が列挙する「変更可能な
        // 拡張」の一覧に含まれない（key_share／early_data／pre_shared_key／
        // padding のみが対象）ため、値の変更は illegal_parameter。
        // pre_shared_key は構造上有効な値にする（`ch` 自身の
        // `negotiate` 呼び出しが冒頭で構造検証するため。
        // codex-review PR #1022 P0 指摘）。
        let first = push_extension(
            with_ed25519_sig_alg(rfc8448_client_hello()),
            EXT_PRE_SHARED_KEY,
            psk_extension_data(&[(b"a", 0)]),
        );
        let ch =
            replace_extension_data(first.clone(), EXT_PSK_KEY_EXCHANGE_MODES, vec![0x01, 0x02]);
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_adding_unrelated_extension_is_illegal_parameter() {
        // key_share／early_data／pre_shared_key／psk_key_exchange_modes／
        // padding 以外の拡張を 2 回目で追加するのは許されない。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let ch = push_extension(first.clone(), 0xfeed, vec![0x00]);
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_adding_early_data_is_illegal_parameter() {
        // early_data は「比較除外＝自由に追加してよい」対象ではない。
        // 1 回目に無かった early_data を 2 回目で新たに加えるのは、
        // 0-RTT が HRR 後に許可されないという RFC 8446 §4.1.2 の制約に
        // 反するため illegal_parameter（finding #3 の一部。key_share 等と
        // 同列に一律除外していた旧実装の穴）。early_data は
        // pre_shared_key を伴わないとそれ自体が missing_extension（PR
        // #1022 追指摘）になり本 HRR 固有の検査に到達できないため、この
        // HRR 固有の規則を単独で検証できるよう pre_shared_key を伴わせる
        // （pre_shared_key は末尾でなければならないため、early_data は
        // その手前へ挿入する）。
        let first = client_hello_with_psk(&[(b"alice", 100)]);
        let mut ch = first.clone();
        let psk_index = ch.extensions.len() - 1;
        ch.extensions.insert(
            psk_index,
            handshake::Extension {
                extension_type: EXT_EARLY_DATA,
                extension_data: vec![],
            },
        );
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_retaining_early_data_is_illegal_parameter() {
        // 1 回目に early_data が存在した場合でも、2 回目にそのまま残す
        // （削除しない）のは許されない。RFC 8446 §4.1.2 は「削除」のみを
        // 許可された差分として列挙しており、維持は対象外。early_data は
        // pre_shared_key を伴わせて構築する（理由は上記テスト参照）。
        let first = {
            let mut ch = client_hello_with_psk(&[(b"alice", 100)]);
            let psk_index = ch.extensions.len() - 1;
            ch.extensions.insert(
                psk_index,
                handshake::Extension {
                    extension_type: EXT_EARLY_DATA,
                    extension_data: vec![],
                },
            );
            ch
        };
        let ch = first.clone();
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_removing_early_data_is_accepted() {
        // 1 回目にあった early_data を 2 回目で削除するのは RFC 8446
        // §4.1.2 が明示的に許可する差分であり、他が一致していれば
        // 受理される（Accept まで到達する）。
        let first = push_extension(
            with_ed25519_sig_alg(rfc8448_client_hello()),
            EXT_EARLY_DATA,
            vec![],
        );
        let ch = remove_extension(first.clone(), EXT_EARLY_DATA);
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Ok(ClientHelloDecision::Accept(_))
        ));
    }

    #[test]
    fn hrr_second_hello_offering_extra_group_alongside_x25519_is_illegal_parameter() {
        // x25519 に加えて別グループも提示する 2 回目の key_share は
        // 「要求した唯一のグループのみ」の制約に反する（finding #2 後半）。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let mut entry_x25519 = Vec::new();
        entry_x25519.extend_from_slice(&GROUP_X25519.to_be_bytes());
        entry_x25519.extend_from_slice(&32u16.to_be_bytes());
        entry_x25519.extend_from_slice(&[0u8; 32]);
        let mut entry_other = Vec::new();
        entry_other.extend_from_slice(&0x0017u16.to_be_bytes());
        entry_other.extend_from_slice(&65u16.to_be_bytes());
        entry_other.extend_from_slice(&[0u8; 65]);
        let mut data = Vec::new();
        data.extend_from_slice(&((entry_x25519.len() + entry_other.len()) as u16).to_be_bytes());
        data.extend_from_slice(&entry_x25519);
        data.extend_from_slice(&entry_other);
        let ch = replace_extension_data(first.clone(), EXT_KEY_SHARE, data);
        assert!(matches!(
            negotiate(&ch, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_consistency_check_takes_priority_over_later_semantic_checks() {
        // 同一性違反（cipher_suites の変更）に加えて、通常なら別の alert
        // （missing_extension）を先に引き当てる意味検査違反（signature_
        // algorithms 拡張の欠落）も同時に含む 2 回目の ClientHello。
        // check_hrr_consistency は他のどの意味検査よりも先に実行される
        // ため、missing_extension ではなく illegal_parameter を返す
        // （検査順序の入れ替えを固定する回帰テスト）。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let mut ch = remove_extension(first.clone(), EXT_SIGNATURE_ALGORITHMS);
        ch.cipher_suites = vec![0x1302, TLS_AES_128_GCM_SHA256];
        let result = negotiate(&ch, Some(&first));
        assert!(matches!(result, Err(ClientHelloError::IllegalParameter(_))));
        assert_eq!(
            result.unwrap_err().alert_description(),
            AlertDescription::IllegalParameter
        );
    }

    #[test]
    fn key_share_two_non_x25519_entries_same_group_is_illegal_parameter() {
        // x25519 以外のグループでも同一グループの重複エントリは
        // illegal_parameter（finding #3。x25519 限定だった旧実装の穴）。
        let mut entry = Vec::new();
        entry.extend_from_slice(&0x0017u16.to_be_bytes());
        entry.extend_from_slice(&65u16.to_be_bytes());
        entry.extend_from_slice(&[0u8; 65]);
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
    fn server_name_duplicate_unknown_name_type_is_illegal_parameter() {
        // name_type=0（host_name）限定だった旧実装の穴（finding #4）。
        // 未知の name_type（0x07）が重複していても illegal_parameter。
        let data = {
            let mut entry = Vec::new();
            entry.push(0x07); // 未知の name_type
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

    /// `server_name`（ClientHello 形）の `host_name` エントリ 1 件だけを
    /// 持つ拡張データを構築するテスト専用ヘルパー。
    fn host_name_data(name: &[u8]) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.push(0); // name_type = host_name
        entry.extend_from_slice(&(name.len() as u16).to_be_bytes());
        entry.extend_from_slice(name);
        let mut data = Vec::new();
        data.extend_from_slice(&(entry.len() as u16).to_be_bytes());
        data.extend_from_slice(&entry);
        data
    }

    fn with_host_name(name: &[u8]) -> handshake::ClientHello {
        with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_SERVER_NAME,
            host_name_data(name),
        ))
    }

    // ---- codex-review PR #1022 P1 指摘の回帰テスト
    //      （server_name host_name の RFC 6066 §3 構文検証） ----

    #[test]
    fn host_name_ipv4_literal_is_illegal_parameter() {
        // RFC 6066 §3: literal IPv4 addresses are not permitted in
        // "HostName"。
        let ch = with_host_name(b"192.168.0.1");
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_ipv6_literal_is_illegal_parameter() {
        // RFC 6066 §3: literal IPv6 addresses are not permitted either。
        // ':' を含む DNS ホスト名は存在しないため IPv6 リテラルとみなす。
        let ch = with_host_name(b"::1");
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_trailing_dot_is_illegal_parameter() {
        let ch = with_host_name(b"example.com.");
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_empty_label_is_illegal_parameter() {
        let ch = with_host_name(b"example..com");
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_label_starting_with_hyphen_is_illegal_parameter() {
        let ch = with_host_name(b"-example.com");
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_label_ending_with_hyphen_is_illegal_parameter() {
        let ch = with_host_name(b"example-.com");
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_label_too_long_is_illegal_parameter() {
        // ラベルは 1..=63 バイト（RFC 1034）。64 バイトは超過。
        let label = vec![b'a'; 64];
        let name = [label.as_slice(), b".com"].concat();
        let ch = with_host_name(&name);
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_non_ascii_is_illegal_parameter() {
        let ch = with_host_name(&[b'a', 0x80, b'b']);
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_total_length_over_253_is_illegal_parameter() {
        // 全長は 1..=253 バイト（RFC 1034）。63 バイトラベルを 4 つ・区切り
        // ドットを挟むと 63*4+3=255 バイトとなり上限超過。
        let label = vec![b'a'; 63];
        let name = [
            label.as_slice(),
            b".",
            label.as_slice(),
            b".",
            label.as_slice(),
            b".",
            label.as_slice(),
        ]
        .concat();
        assert!(name.len() > 253);
        let ch = with_host_name(&name);
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn host_name_valid_dns_names_are_accepted() {
        // 通常の DNS ホスト名（サブドメイン・ハイフンを含むラベル・単一
        // ラベル）は引き続き受理される。
        for name in [
            b"example.com".as_slice(),
            b"www.example.co.jp".as_slice(),
            b"my-host.example.com".as_slice(),
            b"localhost".as_slice(),
        ] {
            let ch = with_host_name(name);
            let decision = negotiate(&ch, None).expect("valid host_name must be accepted");
            let ClientHelloDecision::Accept(negotiated) = decision else {
                panic!("must be Accept");
            };
            assert_eq!(negotiated.server_name, Some(name.to_vec()));
        }
    }

    #[test]
    fn malformed_server_name_is_rejected_even_when_retry_would_otherwise_apply() {
        // finding #5（cursor）: server_name の構造検証は Accept 経路の後で
        // 遅延実行してはならない。key_share が空（本来なら
        // RetryRequestX25519 になる状況）でも、server_name が不正なら
        // decode_error で拒否し、RetryRequestX25519 を返してはならない。
        let ch = with_ed25519_sig_alg(replace_extension_data(
            replace_extension_data(
                rfc8448_client_hello(),
                EXT_KEY_SHARE,
                empty_key_share_data(),
            ),
            EXT_SERVER_NAME,
            vec![0x00, 0x00], // server_name_list 長 0（最小 1 未満。decode_error）。
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
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
        let result = negotiate(ch, None);
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
        // 位置検査（末尾でなければならない）を単独で検証するため、
        // pre_shared_key 自体は構造上有効な値にする（binder 件数一致・
        // 各エントリ長 32..=255 等の構造検証を満たさないと、この検査に
        // 到達する前に decode_error になってしまうため。
        // codex-review PR #1022 P0 指摘）。
        let ch = push_extension(base, EXT_PRE_SHARED_KEY, psk_extension_data(&[(b"a", 0)]));
        let ch = push_extension(ch, EXT_PSK_KEY_EXCHANGE_MODES, vec![0x01, 0x01]);
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    #[test]
    fn pre_shared_key_last_without_modes_is_missing_extension() {
        // psk_key_exchange_modes(002d) を削除し、末尾に pre_shared_key を追加。
        // pre_shared_key 自体は構造上有効な値にする（理由は上記テスト参照）。
        let base = remove_extension(
            with_ed25519_sig_alg(rfc8448_client_hello()),
            EXT_PSK_KEY_EXCHANGE_MODES,
        );
        let ch = push_extension(base, EXT_PRE_SHARED_KEY, psk_extension_data(&[(b"a", 0)]));
        assert_rejected(&ch, AlertDescription::MissingExtension);
    }

    /// `identities`／`binders` の件数・各 binder の長さを個別に指定できる
    /// `pre_shared_key` 生データビルダー（テスト専用。構造検証の境界値・
    /// 不正値を作るための下位ヘルパー。[`psk_extension_data`] は
    /// `identities` と 1 対 1 対応する妥当な `binders` を組み立てる薄い
    /// ラッパーとして本関数を使う）。
    fn build_psk_data(identities: &[(&[u8], u32)], binder_lens: &[usize]) -> Vec<u8> {
        let mut ids = Vec::new();
        for (identity, age) in identities {
            ids.extend_from_slice(&(identity.len() as u16).to_be_bytes());
            ids.extend_from_slice(identity);
            ids.extend_from_slice(&age.to_be_bytes());
        }
        let mut binders = Vec::new();
        for len in binder_lens {
            binders.push(*len as u8);
            binders.extend_from_slice(&vec![0u8; *len]);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(ids.len() as u16).to_be_bytes());
        out.extend_from_slice(&ids);
        out.extend_from_slice(&(binders.len() as u16).to_be_bytes());
        out.extend_from_slice(&binders);
        out
    }

    // ---- codex-review PR #1022 P0 指摘の回帰テスト
    //      （pre_shared_key の binders・psk_key_exchange_modes・
    //      early_data の完全な構造検証） ----

    #[test]
    fn pre_shared_key_binder_count_fewer_than_identities_is_decode_error() {
        // RFC 8446 §4.2.11: 「the total number of identities and binders
        // MUST be equal」。identities 2 件に対し binders 1 件しか無い場合、
        // binder 欠落を検出せずに受理してはならない。
        let data = build_psk_data(&[(b"alice", 100), (b"bob", 200)], &[32]);
        let ch = with_ed25519_sig_alg(push_extension(
            rfc8448_client_hello(),
            EXT_PRE_SHARED_KEY,
            data,
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn pre_shared_key_binder_count_more_than_identities_is_decode_error() {
        // 逆に binders が identities より多い場合も同じ MUST 違反。
        let data = build_psk_data(&[(b"alice", 100)], &[32, 32]);
        let ch = with_ed25519_sig_alg(push_extension(
            rfc8448_client_hello(),
            EXT_PRE_SHARED_KEY,
            data,
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn pre_shared_key_binder_too_short_is_decode_error() {
        // `opaque binder<32..255>` の下限未満（10 バイト）は decode_error。
        let data = build_psk_data(&[(b"alice", 100)], &[10]);
        let ch = with_ed25519_sig_alg(push_extension(
            rfc8448_client_hello(),
            EXT_PRE_SHARED_KEY,
            data,
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn pre_shared_key_trailing_bytes_after_binders_is_decode_error() {
        // identities・binders の 2 ベクタを読み終えた後に余剰バイトが
        // あってはならない（`r.expect_end()`）。
        let mut data = psk_extension_data(&[(b"alice", 100)]);
        data.push(0xff);
        let ch = with_ed25519_sig_alg(push_extension(
            rfc8448_client_hello(),
            EXT_PRE_SHARED_KEY,
            data,
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn pre_shared_key_truncated_missing_binders_is_decode_error() {
        // binders ベクタを丸ごと欠いた（identities だけの）
        // `pre_shared_key` は構造違反として decode_error になる
        // （本 finding の直接の再現。修正前は identities の直後で読み取りを
        // 終えて受理していた）。
        let mut ids = Vec::new();
        let identity: &[u8] = b"alice";
        ids.extend_from_slice(&(identity.len() as u16).to_be_bytes());
        ids.extend_from_slice(identity);
        ids.extend_from_slice(&100u32.to_be_bytes());
        let mut data = Vec::new();
        data.extend_from_slice(&(ids.len() as u16).to_be_bytes());
        data.extend_from_slice(&ids);
        // binders ベクタを書かずに切り詰める。
        let ch = with_ed25519_sig_alg(push_extension(
            rfc8448_client_hello(),
            EXT_PRE_SHARED_KEY,
            data,
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn psk_key_exchange_modes_empty_vector_is_decode_error() {
        // `PskKeyExchangeMode ke_modes<1..255>` は最低 1 件必要。
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_PSK_KEY_EXCHANGE_MODES,
            vec![0x00],
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn psk_key_exchange_modes_trailing_bytes_is_decode_error() {
        // 長さ接頭辞どおりに読み終えた後に余剰バイトがあってはならない。
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_PSK_KEY_EXCHANGE_MODES,
            vec![0x01, 0x01, 0xff],
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn early_data_non_empty_body_in_client_hello_is_decode_error() {
        // ClientHello 内の early_data は `Empty` 型（本体を持たない）。
        // pre_shared_key を伴わせずとも、構造検証（本体は空でなければ
        // ならない）が missing_extension より先に働くことを確認する。
        let ch = with_ed25519_sig_alg(push_extension(
            rfc8448_client_hello(),
            EXT_EARLY_DATA,
            vec![0x01],
        ));
        assert_rejected(&ch, AlertDescription::DecodeError);
    }

    #[test]
    fn pre_shared_key_must_be_last_extension_even_when_well_formed() {
        // pre_shared_key の構造自体は完全に妥当でも、位置制約
        // （RFC 8446 §4.2.11: 最後の拡張でなければならない）は独立して
        // 検査される（`pre_shared_key_not_last_is_illegal_parameter` の
        // 拡張版。psk_key_exchange_modes 以外の拡張を後続させても同じ
        // 判定になることを確認する）。
        let base = with_ed25519_sig_alg(rfc8448_client_hello());
        let ch = push_extension(
            base,
            EXT_PRE_SHARED_KEY,
            psk_extension_data(&[(b"alice", 100)]),
        );
        let ch = push_extension(ch, 0xfeed, vec![0x00]);
        assert_rejected(&ch, AlertDescription::IllegalParameter);
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
                let _ = negotiate(&ch, None);
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
        let result = negotiate(&ch, None);
        assert!(matches!(
            result,
            Ok(ClientHelloDecision::Accept(_)) | Err(_)
        ));
    }

    // ---- codex-review 指摘の回帰テスト（PR #1022） ----

    #[test]
    fn early_data_without_pre_shared_key_is_missing_extension() {
        // finding #2 (PR #1022 追指摘): early_data は pre_shared_key を
        // 伴わずに提示できない（RFC 8446 §4.2.10）。psk_key_exchange_modes
        // が pre_shared_key に対して必須拡張として扱われるのと同型の
        // 依存関係のため missing_extension（RFC 8446 §9.2）で拒否する。
        // HRR を経ない通常の 1 回目 ClientHello でも検査する。
        let ch = with_ed25519_sig_alg(push_extension(
            rfc8448_client_hello(),
            EXT_EARLY_DATA,
            vec![],
        ));
        assert_rejected(&ch, AlertDescription::MissingExtension);
    }

    #[test]
    fn supported_groups_duplicate_named_group_is_illegal_parameter() {
        // finding #3: supported_groups 内の同一 NamedGroup の重複列挙は
        // key_share と同様に illegal_parameter（RFC 8446 §4.2.7）。
        let data = {
            let mut list = Vec::new();
            list.extend_from_slice(&GROUP_X25519.to_be_bytes());
            list.extend_from_slice(&GROUP_X25519.to_be_bytes());
            let mut out = Vec::new();
            out.extend_from_slice(&(list.len() as u16).to_be_bytes());
            out.extend_from_slice(&list);
            out
        };
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_SUPPORTED_GROUPS,
            data,
        ));
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    fn key_share_group_absent_from_supported_groups_data() -> Vec<u8> {
        // group=0x0015（RFC 8448 §3 の supported_groups 既定リストに含まれ
        // ない値）・key_exchange 長 1 バイトの構造上有効な非 x25519 エントリ。
        let mut entry = Vec::new();
        entry.extend_from_slice(&0x0015u16.to_be_bytes());
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.push(0u8);
        let mut out = Vec::new();
        out.extend_from_slice(&(entry.len() as u16).to_be_bytes());
        out.extend_from_slice(&entry);
        out
    }

    #[test]
    fn key_share_entry_group_absent_from_supported_groups_is_illegal_parameter() {
        // finding #4: x25519 に限らずすべての KeyShareEntry のグループが
        // supported_groups に含まれていなければならない（RFC 8446
        // §4.2.8）。x25519 限定だった旧実装の穴。
        let ch = with_ed25519_sig_alg(replace_extension_data(
            rfc8448_client_hello(),
            EXT_KEY_SHARE,
            key_share_group_absent_from_supported_groups_data(),
        ));
        assert_rejected(&ch, AlertDescription::IllegalParameter);
    }

    /// `pre_shared_key`（ClientHello 形）の完全な構造（`identities`・
    /// `binders`）を構築するテスト専用ヘルパー。`binders` の中身
    /// （バイト列そのもの）は本実装が比較対象にしないため全て 0 で埋める
    /// が、件数は `identities` と一致させ・各エントリは最小長 32 バイトと
    /// する（構造検証〔codex-review PR #1022 P0 指摘〕を満たすため）。
    fn psk_extension_data(identities: &[(&[u8], u32)]) -> Vec<u8> {
        let mut ids = Vec::new();
        for (identity, age) in identities {
            ids.extend_from_slice(&(identity.len() as u16).to_be_bytes());
            ids.extend_from_slice(identity);
            ids.extend_from_slice(&age.to_be_bytes());
        }
        let mut binders = Vec::new();
        for _ in identities {
            binders.push(32u8); // opaque binder<32..255> の長さ接頭辞（最小長 32）。
            binders.extend_from_slice(&[0u8; 32]);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(ids.len() as u16).to_be_bytes());
        out.extend_from_slice(&ids);
        out.extend_from_slice(&(binders.len() as u16).to_be_bytes());
        out.extend_from_slice(&binders);
        out
    }

    /// ed25519 signature_algorithms・`psk_key_exchange_modes`・末尾
    /// `pre_shared_key`（指定した identities）を持つ `ClientHello` を組み立
    /// てる（RFC 8446 §4.2.11 の位置・併存の MUST を満たす）。
    fn client_hello_with_psk(identities: &[(&[u8], u32)]) -> handshake::ClientHello {
        // RFC 8448 §3 のベクタは既に psk_key_exchange_modes(002d) を含む
        // ため、追加で push すると重複拡張エラーになる。ここでは
        // pre_shared_key を末尾へ追加するだけでよい。
        push_extension(
            with_ed25519_sig_alg(rfc8448_client_hello()),
            EXT_PRE_SHARED_KEY,
            psk_extension_data(identities),
        )
    }

    #[test]
    fn hrr_second_hello_psk_identity_added_is_illegal_parameter() {
        // finding #1: HRR 後に許可されるのは非互換 PSK の削除のみであり、
        // 1 回目に無かった identity を新規追加するのは illegal_parameter。
        let first = client_hello_with_psk(&[(b"alice", 100)]);
        let second = client_hello_with_psk(&[(b"alice", 100), (b"bob", 200)]);
        assert!(matches!(
            negotiate(&second, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_psk_identity_reordered_is_illegal_parameter() {
        // finding #1: 削除のみが許可され、並べ替えは許可されない
        // （順序保存部分列でなければならない）。
        let first = client_hello_with_psk(&[(b"alice", 100), (b"bob", 200)]);
        let second = client_hello_with_psk(&[(b"bob", 200), (b"alice", 100)]);
        assert!(matches!(
            negotiate(&second, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }

    #[test]
    fn hrr_second_hello_psk_identity_removed_is_accepted() {
        // finding #1: 非互換になった PSK identity の削除（部分列を維持した
        // まま先頭以外を削る）は RFC 8446 §4.1.2 が明示的に許可する差分。
        let first = client_hello_with_psk(&[(b"alice", 100), (b"bob", 200)]);
        let second = client_hello_with_psk(&[(b"bob", 200)]);
        assert!(matches!(
            negotiate(&second, Some(&first)),
            Ok(ClientHelloDecision::Accept(_))
        ));
    }

    #[test]
    fn hrr_second_hello_psk_identity_with_updated_age_is_accepted() {
        // Cursor Bugbot 指摘（PR #1022）: RFC 8446 §4.1.2 により、クライアン
        // トは HRR 後の 2 回目 ClientHello で obfuscated_ticket_age を
        // 再計算してよい（ticket 発行からの経過時間が変化するため）。
        // identity 値が同一であれば age が変化していても illegal_parameter
        // にしてはならない。
        let first = client_hello_with_psk(&[(b"alice", 100), (b"bob", 200)]);
        let second = client_hello_with_psk(&[(b"alice", 150), (b"bob", 250)]);
        assert!(matches!(
            negotiate(&second, Some(&first)),
            Ok(ClientHelloDecision::Accept(_))
        ));
    }

    #[test]
    fn hrr_second_hello_removing_pre_shared_key_entirely_is_accepted() {
        // pre_shared_key の全削除は明示的に許可される（binders 再計算・
        // 非互換 PSK 削除と同じ「更新」の一形態）。
        let first = client_hello_with_psk(&[(b"alice", 100)]);
        let second = remove_extension(first.clone(), EXT_PRE_SHARED_KEY);
        assert!(matches!(
            negotiate(&second, Some(&first)),
            Ok(ClientHelloDecision::Accept(_))
        ));
    }

    #[test]
    fn hrr_second_hello_adding_pre_shared_key_absent_from_first_is_illegal_parameter() {
        // finding #1 関連: 1 回目に無かった pre_shared_key を 2 回目で
        // 新規追加するのは「更新」ではなく新規提示であり許可されない。
        // RFC 8448 §3 のベクタは既に psk_key_exchange_modes(002d) を含む。
        let first = with_ed25519_sig_alg(rfc8448_client_hello());
        let second = push_extension(
            first.clone(),
            EXT_PRE_SHARED_KEY,
            psk_extension_data(&[(b"alice", 100)]),
        );
        assert!(matches!(
            negotiate(&second, Some(&first)),
            Err(ClientHelloError::IllegalParameter(_))
        ));
    }
}
