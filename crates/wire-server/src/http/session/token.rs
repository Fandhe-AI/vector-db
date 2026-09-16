//! NoSQL 表層のセッション認証（TASK-174・HTTP-4）が使う不透明セッショントークン。
//!
//! 認証成功時（`POST /v1/session`。Issue #752）に OS の CSPRNG から 32 バイト
//! を取り、パディングなし base64url（RFC 4648 §5）表現（43 文字）で返す。
//! 以後のリクエストは `Authorization: Bearer <token>`（Issue #753・#754）で
//! これを提示するため、[`decode_base64url`]・[`SessionToken::parse`] は
//! **受信データ経路**として扱い、`unwrap`／`expect`／添字アクセス（`[]`）を
//! 使わず、閉じた文字集合・厳密な長さ検査のみで untrusted な文字列を判定する
//! （coding-rust.md）。
//!
//! パディング（`=`）は出力しない・入力でも拒否する。末尾グループの余剰ビット
//! が非ゼロな非正準表現（異なる文字列が同一バイト列へ写る aliasing）も拒否する
//! （decode の一意性を保つ fail-closed 設計）。
//!
//! セッションストア（Issue #751）・エンドポイント（#752〜#754）・トークン照合
//! の定数時間比較（#751／#754 の設計事項）は本モジュールの対象外。

use std::fmt;

/// セッショントークンの生バイト長（256bit）。
pub const TOKEN_BYTES: usize = 32;
/// パディングなし base64url でのセッショントークンの文字数
/// （`ceil(TOKEN_BYTES * 8 / 6)`）。
pub const TOKEN_ENCODED_LEN: usize = 43;

/// [`decode_base64url`] が受理する入力の上限バイト長。セッショントークン用途
/// （43 文字）に対して十分な余裕を持たせた固定値。この上限はアロケーション
/// （出力 `Vec` の確保）より前に検査し、無制限な確保を防ぐ。
const MAX_BASE64URL_INPUT_LEN: usize = 1024;

/// base64url（パディングなし・非正準表現拒否）の decode で検出した拒否理由。
///
/// untrusted 文字列のどの位置・どの文字が原因かは含めない
/// （エラー経由でトークン断片や内部実装の手がかりが漏れないようにする
/// fail-closed・情報最小の設計）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64UrlError {
    /// 入力が [`MAX_BASE64URL_INPUT_LEN`] を超過した。
    TooLong,
    /// 入力長が base64（4 文字単位）として成立しない（`len % 4 == 1`）。
    InvalidLength,
    /// base64url のアルファベット（`A-Z a-z 0-9 - _`）に含まれない文字が
    /// 混入していた（パディング `=`・空白・`+`／`/` を含む）。
    InvalidCharacter,
    /// 末尾グループの余剰ビットが非ゼロな非正準表現。
    NonCanonical,
}

impl fmt::Display for Base64UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Base64UrlError::TooLong => write!(f, "base64url input exceeds the length limit"),
            Base64UrlError::InvalidLength => write!(f, "base64url input has an invalid length"),
            Base64UrlError::InvalidCharacter => {
                write!(
                    f,
                    "base64url input contains a character outside the alphabet"
                )
            }
            Base64UrlError::NonCanonical => {
                write!(f, "base64url input is not in canonical form")
            }
        }
    }
}

impl std::error::Error for Base64UrlError {}

/// `SessionToken::parse` の拒否理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// 入力の文字数（バイト長）が [`TOKEN_ENCODED_LEN`] と一致しない。
    InvalidLength,
    /// base64url としての decode に失敗した。
    Encoding(Base64UrlError),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::InvalidLength => write!(f, "session token has an invalid length"),
            TokenError::Encoding(e) => write!(f, "session token encoding is invalid: {e}"),
        }
    }
}

impl std::error::Error for TokenError {}

/// 6bit 値（0..=63）を base64url のアルファベット文字（ASCII）へ写す。
///
/// 呼び出し元は常に `n & 0x3F` でマスクした値のみを渡す（internal に生成した
/// 値であり untrusted 入力由来ではない）ため、`63` をデフォルト分岐で受ける
/// ことで全 `u8` 値に対して exhaustive かつ panic しない実装にできる。
fn base64url_char(v: u8) -> u8 {
    match v {
        0..=25 => b'A' + v,
        26..=51 => b'a' + (v - 26),
        52..=61 => b'0' + (v - 52),
        62 => b'-',
        _ => b'_',
    }
}

/// ASCII バイトを base64url の 6bit 値へ写す。アルファベット外は `None`
/// （untrusted 入力の判定に使う唯一の入口）。
fn base64url_sextet(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// パディングなし base64url の出力文字数を計算する（`bytes.len()` から）。
/// 桁あふれは `saturating_*` で吸収する（本関数は容量ヒントにのみ使う
/// ため、飽和しても呼び出し側の正しさには影響しない）。
fn encoded_len(byte_len: usize) -> usize {
    let full_groups = byte_len / 3;
    let remainder = byte_len % 3;
    let extra = match remainder {
        0 => 0,
        1 => 2,
        _ => 3,
    };
    full_groups.saturating_mul(4).saturating_add(extra)
}

/// バイト列をパディングなし base64url（RFC 4648 §5）文字列へ変換する。
///
/// 出力は常に `A-Z a-z 0-9 - _` のみで構成され、`=` を含まない。
pub fn encode_base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(encoded_len(bytes.len()));
    for chunk in bytes.chunks(3) {
        match chunk {
            [a, b, c] => {
                let n = ((*a as u32) << 16) | ((*b as u32) << 8) | (*c as u32);
                out.push(base64url_char(((n >> 18) & 0x3F) as u8) as char);
                out.push(base64url_char(((n >> 12) & 0x3F) as u8) as char);
                out.push(base64url_char(((n >> 6) & 0x3F) as u8) as char);
                out.push(base64url_char((n & 0x3F) as u8) as char);
            }
            [a, b] => {
                let n = ((*a as u32) << 16) | ((*b as u32) << 8);
                out.push(base64url_char(((n >> 18) & 0x3F) as u8) as char);
                out.push(base64url_char(((n >> 12) & 0x3F) as u8) as char);
                out.push(base64url_char(((n >> 6) & 0x3F) as u8) as char);
            }
            [a] => {
                let n = (*a as u32) << 16;
                out.push(base64url_char(((n >> 18) & 0x3F) as u8) as char);
                out.push(base64url_char(((n >> 12) & 0x3F) as u8) as char);
            }
            _ => {}
        }
    }
    out
}

/// パディングなし base64url 文字列をバイト列へ decode する。
///
/// **受信データ経路**（`SessionToken::parse` 経由で `Authorization: Bearer`
/// ヘッダの値まで到達しうる。Issue #753・#754）。アロケーション前に長さ上限を
/// 検査し、以下を fail-closed に拒否する（優先順）:
/// 1. [`MAX_BASE64URL_INPUT_LEN`] 超過（[`Base64UrlError::TooLong`]）
/// 2. `len % 4 == 1`（base64 として成立しない長さ。[`Base64UrlError::InvalidLength`]）
/// 3. アルファベット外の文字（パディング `=` を含む。[`Base64UrlError::InvalidCharacter`]）
/// 4. 末尾グループの非正準表現（[`Base64UrlError::NonCanonical`]）
pub fn decode_base64url(input: &str) -> Result<Vec<u8>, Base64UrlError> {
    let bytes = input.as_bytes();
    if bytes.len() > MAX_BASE64URL_INPUT_LEN {
        return Err(Base64UrlError::TooLong);
    }
    if bytes.len() % 4 == 1 {
        return Err(Base64UrlError::InvalidLength);
    }

    let out_capacity = (bytes.len() / 4).saturating_add(1).saturating_mul(3);
    let mut out = Vec::with_capacity(out_capacity);

    for chunk in bytes.chunks(4) {
        match chunk {
            [a, b, c, d] => {
                let v0 = base64url_sextet(*a).ok_or(Base64UrlError::InvalidCharacter)?;
                let v1 = base64url_sextet(*b).ok_or(Base64UrlError::InvalidCharacter)?;
                let v2 = base64url_sextet(*c).ok_or(Base64UrlError::InvalidCharacter)?;
                let v3 = base64url_sextet(*d).ok_or(Base64UrlError::InvalidCharacter)?;
                let n =
                    ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6) | (v3 as u32);
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            [a, b, c] => {
                let v0 = base64url_sextet(*a).ok_or(Base64UrlError::InvalidCharacter)?;
                let v1 = base64url_sextet(*b).ok_or(Base64UrlError::InvalidCharacter)?;
                let v2 = base64url_sextet(*c).ok_or(Base64UrlError::InvalidCharacter)?;
                if v2 & 0x03 != 0 {
                    return Err(Base64UrlError::NonCanonical);
                }
                let n = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6);
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
            }
            [a, b] => {
                let v0 = base64url_sextet(*a).ok_or(Base64UrlError::InvalidCharacter)?;
                let v1 = base64url_sextet(*b).ok_or(Base64UrlError::InvalidCharacter)?;
                if v1 & 0x0F != 0 {
                    return Err(Base64UrlError::NonCanonical);
                }
                let n = ((v0 as u32) << 18) | ((v1 as u32) << 12);
                out.push((n >> 16) as u8);
            }
            [] => {}
            _ => return Err(Base64UrlError::InvalidLength),
        }
    }

    Ok(out)
}

/// NoSQL 表層のセッション認証（TASK-174・HTTP-4）で発行する不透明トークン。
///
/// 生バイトは 32 バイト（256bit）の CSPRNG 出力そのもので、意味的な構造を
/// 持たない。`Debug` は手書きで秘匿し（トークンの値をログへ意図せず流さない）、
/// `Display` は実装しない（暗黙の文字列化経路を作らない）。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionToken([u8; TOKEN_BYTES]);

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

impl SessionToken {
    /// OS の CSPRNG（[`crate::auth::read_urandom`]。`/dev/urandom` 読み取り。
    /// `main.rs` の `hash-password` の salt 生成と同じ乱数源を再利用する）から
    /// 32 バイトを取り新規セッショントークンを生成する。疑似乱数へのフォール
    /// バックは持たず、読み取り失敗はそのまま `Err` で fail-closed に伝える。
    pub fn generate() -> std::io::Result<SessionToken> {
        let raw = crate::auth::read_urandom(TOKEN_BYTES)?;
        Self::from_bytes(&raw)
    }

    /// `read_urandom` の戻り値を固定長配列へ変換する。長さが `TOKEN_BYTES`
    /// と一致しない場合は fail-closed に `Err` を返す（構造上は
    /// `read_urandom(TOKEN_BYTES)` により常に一致するが、契約変化に対する
    /// 防御として明示的に検査する）。
    fn from_bytes(raw: &[u8]) -> std::io::Result<SessionToken> {
        let arr: [u8; TOKEN_BYTES] = raw.try_into().map_err(|_| {
            std::io::Error::other("urandom read returned an unexpected length for a session token")
        })?;
        Ok(SessionToken(arr))
    }

    /// パディングなし base64url 表現（常に [`TOKEN_ENCODED_LEN`] 文字）。
    pub fn encoded(&self) -> String {
        encode_base64url(&self.0)
    }

    /// `Authorization: Bearer <token>`（Issue #753・#754）から受け取った
    /// untrusted な文字列をセッショントークンへ変換する。**受信データ経路**
    /// のため decode 前に長さを厳密検査する（42／44 文字等は decode を試みず
    /// 即座に拒否）。
    pub fn parse(input: &str) -> Result<SessionToken, TokenError> {
        if input.len() != TOKEN_ENCODED_LEN {
            return Err(TokenError::InvalidLength);
        }
        let decoded = decode_base64url(input).map_err(TokenError::Encoding)?;
        let arr: [u8; TOKEN_BYTES] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| TokenError::InvalidLength)?;
        Ok(SessionToken(arr))
    }

    /// セッションストア（Issue #751）がキーとして使う生バイト表現。
    pub fn as_bytes(&self) -> &[u8; TOKEN_BYTES] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // RFC 4648 §10 のテストベクタ（パディングを除去した形）。
    // 使用文字は標準アルファベットと url-safe アルファベットで重複するため
    // base64url decode/encode 双方の基礎正しさを検証できる。
    const RFC4648_VECTORS: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"f", "Zg"),
        (b"fo", "Zm8"),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg"),
        (b"fooba", "Zm9vYmE"),
        (b"foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn rfc4648_vectors_round_trip() {
        for (raw, encoded) in RFC4648_VECTORS {
            assert_eq!(encode_base64url(raw), *encoded);
            assert_eq!(decode_base64url(encoded).expect("valid vector"), *raw);
        }
    }

    #[test]
    fn url_safe_alphabet_used_not_standard() {
        // 0xFB の上位 6bit = 62（標準アルファベットでは '+'）。
        assert_eq!(encode_base64url(&[0xFB]), "-w");
        // 0xFC の上位 6bit = 63（標準アルファベットでは '/'）。
        assert_eq!(encode_base64url(&[0xFC]), "_A");

        for b in 0u8..=255 {
            let encoded = encode_base64url(&[b]);
            assert!(!encoded.contains('+'));
            assert!(!encoded.contains('/'));
            assert!(!encoded.contains('='));
        }
    }

    #[test]
    fn round_trip_all_lengths_up_to_64_bytes() {
        for len in 0..=64usize {
            let bytes: Vec<u8> = (0..len).map(|i| ((i * 37 + 7) % 256) as u8).collect();
            let encoded = encode_base64url(&bytes);
            let decoded = decode_base64url(&encoded).expect("round trip decode");
            assert_eq!(decoded, bytes, "len={len}");
        }
    }

    #[test]
    fn token_format_is_43_chars_and_round_trips() {
        let token = SessionToken::generate().expect("urandom available in test environment");
        let encoded = token.encoded();
        assert_eq!(encoded.len(), TOKEN_ENCODED_LEN);
        assert!(!encoded.contains('='));
        assert!(encoded.bytes().all(|b| base64url_sextet(b).is_some()));
        let parsed = SessionToken::parse(&encoded).expect("generated token parses");
        assert_eq!(parsed, token);
    }

    #[test]
    fn generate_produces_distinct_tokens() {
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let token = SessionToken::generate().expect("urandom available in test environment");
            assert!(seen.insert(token), "duplicate session token generated");
        }
        assert_eq!(seen.len(), 100);
    }

    #[test]
    fn decode_rejects_plus_and_slash() {
        assert_eq!(
            decode_base64url("+AAA"),
            Err(Base64UrlError::InvalidCharacter)
        );
        assert_eq!(
            decode_base64url("A/AA"),
            Err(Base64UrlError::InvalidCharacter)
        );
    }

    #[test]
    fn decode_rejects_padding() {
        assert_eq!(
            decode_base64url("AAA="),
            Err(Base64UrlError::InvalidCharacter)
        );
        // len=5 は base64 として成立しない長さのため、'=' の位置まで判定が
        // 進む前に InvalidLength で拒否される。
        assert_eq!(
            decode_base64url("Zm9v="),
            Err(Base64UrlError::InvalidLength)
        );
    }

    #[test]
    fn decode_rejects_whitespace_and_control_bytes() {
        assert_eq!(
            decode_base64url(" AAA"),
            Err(Base64UrlError::InvalidCharacter)
        );
        assert_eq!(
            decode_base64url("AAA\n"),
            Err(Base64UrlError::InvalidCharacter)
        );
    }

    #[test]
    fn decode_rejects_non_ascii() {
        // "aaé" は UTF-8 で 4 バイト（a, a, 0xC3, 0xA9）。バイト単位で判定する
        // ため、マルチバイト文字の一部が閉じたアルファベットの外側として
        // 拒否される。
        assert_eq!(
            decode_base64url("aaé"),
            Err(Base64UrlError::InvalidCharacter)
        );
    }

    #[test]
    fn decode_rejects_invalid_length_modulo() {
        assert_eq!(decode_base64url("A"), Err(Base64UrlError::InvalidLength));
        assert_eq!(
            decode_base64url("AAAAA"),
            Err(Base64UrlError::InvalidLength)
        );
    }

    #[test]
    fn decode_rejects_non_canonical_tail() {
        // 'h' の 6bit 値は 33（0b100001）。下位 4bit が非ゼロなため 2 文字
        // グループの余剰ビットが非ゼロ = 非正準表現。
        assert_eq!(decode_base64url("Zh"), Err(Base64UrlError::NonCanonical));
        // 対照: 'g' の 6bit 値は 32（0b100000）。下位 4bit がゼロなため正準。
        assert!(decode_base64url("Zg").is_ok());
    }

    #[test]
    fn decode_rejects_too_long_input() {
        let too_long = "A".repeat(MAX_BASE64URL_INPUT_LEN + 1);
        assert_eq!(decode_base64url(&too_long), Err(Base64UrlError::TooLong));
    }

    #[test]
    fn parse_rejects_wrong_length() {
        let short = "A".repeat(TOKEN_ENCODED_LEN - 1);
        let long = "A".repeat(TOKEN_ENCODED_LEN + 1);
        assert_eq!(SessionToken::parse(&short), Err(TokenError::InvalidLength));
        assert_eq!(SessionToken::parse(&long), Err(TokenError::InvalidLength));
        assert_eq!(SessionToken::parse(""), Err(TokenError::InvalidLength));
    }

    #[test]
    fn parse_rejects_invalid_character_at_correct_length() {
        let token = SessionToken::generate().expect("urandom available in test environment");
        let mut encoded = token.encoded();
        // 正しい長さ（43 文字）のまま、先頭文字だけアルファベット外に差し替える。
        encoded.replace_range(0..1, "+");
        assert_eq!(
            SessionToken::parse(&encoded),
            Err(TokenError::Encoding(Base64UrlError::InvalidCharacter))
        );
    }

    #[test]
    fn debug_output_does_not_leak_encoded_token() {
        let token = SessionToken::generate().expect("urandom available in test environment");
        let encoded = token.encoded();
        let debug_output = format!("{token:?}");
        assert_eq!(debug_output, "SessionToken(<redacted>)");
        assert!(!debug_output.contains(&encoded));
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        let too_short = vec![0u8; TOKEN_BYTES - 1];
        let too_long = vec![0u8; TOKEN_BYTES + 1];
        assert!(SessionToken::from_bytes(&too_short).is_err());
        assert!(SessionToken::from_bytes(&too_long).is_err());
        assert!(SessionToken::from_bytes(&[0u8; TOKEN_BYTES]).is_ok());
    }
}
