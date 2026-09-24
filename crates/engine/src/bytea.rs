//! `BYTEA` 列型（TABLE-13・TASK-197。関連: WIRE-13・NOSQL-17）向けの
//! テキスト表現（PostgreSQL 既定の `\x` 16 進形式）の解析・出力を集約する。
//!
//! `catalog.rs::ColumnType::Bytea`（カタログ往復）・`row_codec.rs::Value::Bytes`
//! （行バイト表現）とは独立に、SQL リテラル（`sql/parser.rs`）と wire のテキスト
//! 応答（`wire-server::result_encoder`）が共有する変換だけをここへ置く
//! （engine と wire-server の双方から到達できるよう `engine::bytea` として公開する）。
//!
//! 受理する hex 形式は PostgreSQL の `\x` 接頭辞形式の最小部分集合に限る
//! （`\x`／`\X` 接頭辞＋偶数個の 16 進数字。空白・escape 形式・接頭辞省略は拒否。
//! 曖昧さを避ける fail-closed な実装既定値）。

use std::fmt;

/// `BYTEA` 列 1 個分のバイト長上限。行バイト表現（`row_codec::Value::Bytes`）の
/// 上限と同値を維持する（片方だけの変更を防ぐため下部の const assert で強制する）。
pub const MAX_BYTEA_FIELD_LEN: u32 = 4 * 1024 * 1024;

const _: () = assert!(
    MAX_BYTEA_FIELD_LEN == crate::row_codec::MAX_TEXT_FIELD_LEN,
    "bytea::MAX_BYTEA_FIELD_LEN must stay in sync with row_codec::MAX_TEXT_FIELD_LEN"
);

/// `parse_hex_text` の失敗種別。値そのものは含めない（`.claude/rules/security.md`
/// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」と同じ方針で、
/// 長さ・種別のみを保持する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteaTextError {
    /// 復号後の長さが [`MAX_BYTEA_FIELD_LEN`] を超える（確保より前に入力長から
    /// 判定するため、実際に確保が発生することはない）。
    TooLong,
    /// `\x`／`\X` 接頭辞を持たない。
    MissingPrefix,
    /// 16 進本体の桁数が奇数。
    OddLength,
    /// 16 進アルファベット外の文字を含む。
    InvalidDigit,
}

impl fmt::Display for ByteaTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ByteaTextError::TooLong => write!(f, "bytea hex literal too long"),
            ByteaTextError::MissingPrefix => write!(f, "bytea hex literal missing \\x prefix"),
            ByteaTextError::OddLength => write!(f, "bytea hex literal has odd digit count"),
            ByteaTextError::InvalidDigit => write!(f, "bytea hex literal has invalid digit"),
        }
    }
}

impl std::error::Error for ByteaTextError {}

/// 1 桁の 16 進文字を 4 bit 値へ変換する。ASCII の大小文字を両方受理する。
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// PostgreSQL `\x` 16 進形式の文字列をバイト列へ解析する（B4）。
///
/// 受理: `\x`／`\X` 接頭辞＋偶数個の 16 進数字（大小文字混在可）。`\x` のみ
/// （本体 0 桁）は空バイト列として受理する。
/// 拒否: 接頭辞なし・奇数桁・非 16 進文字・空白混在（`MissingPrefix`／
/// `OddLength`／`InvalidDigit`）。
///
/// untrusted 入力を扱うため、確保（`Vec::with_capacity`）より前に入力長から
/// 上限超過を判定する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub fn parse_hex_text(s: &str) -> Result<Vec<u8>, ByteaTextError> {
    // 復号後の長さは高々 `(s.len() - 2) / 2` であり、`s.len()` の上限判定だけで
    // 確保前に MAX_BYTEA_FIELD_LEN 超過を検出できる（`2 * MAX + 2` 超なら
    // 復号後も必ず MAX を超える）。
    let max_input_len = (MAX_BYTEA_FIELD_LEN as usize)
        .checked_mul(2)
        .and_then(|v| v.checked_add(2));
    if let Some(limit) = max_input_len {
        if s.len() > limit {
            return Err(ByteaTextError::TooLong);
        }
    }

    let bytes = s.as_bytes();
    let prefix_ok = bytes.len() >= 2 && (bytes[0] == b'\\') && matches!(bytes[1], b'x' | b'X');
    if !prefix_ok {
        return Err(ByteaTextError::MissingPrefix);
    }
    let hex_body = bytes.get(2..).unwrap_or(&[]);
    if hex_body.len() % 2 != 0 {
        return Err(ByteaTextError::OddLength);
    }

    let out_len = hex_body.len() / 2;
    let mut out = Vec::new();
    out.try_reserve_exact(out_len)
        .map_err(|_| ByteaTextError::TooLong)?;
    let mut i = 0usize;
    while i < hex_body.len() {
        let hi = hex_digit(hex_body[i]).ok_or(ByteaTextError::InvalidDigit)?;
        let lo = hex_digit(hex_body[i + 1]).ok_or(ByteaTextError::InvalidDigit)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// [`parse_hex_text`] の逆変換。`\x` ＋ 小文字 16 進（PostgreSQL の
/// `bytea_output=hex` 既定と同じ表現。B5）。
pub fn format_hex_text(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_lower_upper_and_mixed_case() {
        assert_eq!(
            parse_hex_text("\\xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            parse_hex_text("\\XDEADBEEF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            parse_hex_text("\\xDeAdBeEf").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn parse_accepts_empty_body() {
        assert_eq!(parse_hex_text("\\x").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        assert_eq!(
            parse_hex_text("deadbeef"),
            Err(ByteaTextError::MissingPrefix)
        );
        assert_eq!(parse_hex_text(""), Err(ByteaTextError::MissingPrefix));
    }

    #[test]
    fn parse_rejects_odd_length() {
        assert_eq!(parse_hex_text("\\xabc"), Err(ByteaTextError::OddLength));
    }

    #[test]
    fn parse_rejects_invalid_digit() {
        assert_eq!(parse_hex_text("\\xzz"), Err(ByteaTextError::InvalidDigit));
        // 偶数桁（4 文字）のまま非 16 進文字（空白）を混入させる。
        assert_eq!(parse_hex_text("\\xa cd"), Err(ByteaTextError::InvalidDigit));
    }

    #[test]
    fn parse_rejects_over_limit_by_length_before_allocation() {
        let huge = "\\x".to_string() + &"a".repeat((MAX_BYTEA_FIELD_LEN as usize) * 2 + 1);
        assert_eq!(parse_hex_text(&huge), Err(ByteaTextError::TooLong));
    }

    #[test]
    fn parse_accepts_exact_limit() {
        let body_len = (MAX_BYTEA_FIELD_LEN as usize) * 2;
        let exact = "\\x".to_string() + &"ab".repeat(MAX_BYTEA_FIELD_LEN as usize);
        assert_eq!(exact.len(), 2 + body_len);
        let decoded = parse_hex_text(&exact).unwrap();
        assert_eq!(decoded.len(), MAX_BYTEA_FIELD_LEN as usize);
    }

    #[test]
    fn format_outputs_lowercase_hex_with_prefix() {
        assert_eq!(format_hex_text(&[0xde, 0xad, 0xbe, 0xef]), "\\xdeadbeef");
        assert_eq!(format_hex_text(&[]), "\\x");
    }

    #[test]
    fn round_trip_parse_then_format() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let text = format_hex_text(&bytes);
        assert_eq!(parse_hex_text(&text).unwrap(), bytes);
    }
}
