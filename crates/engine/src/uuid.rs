//! `UUID` 列型の内部表現・パース・整形（TABLE-13〔検討中〕・TASK-197、
//! Issue #887）。128bit 識別子を RFC 4122 ネットワークバイトオーダー
//! （テキスト表記の先頭 16 進 2 桁が先頭バイト）の `[u8; 16]` で表現する
//! 自作実装。外部クレートに依存しない（dependency-policy）。
//!
//! [`catalog::ColumnType::Uuid`]（列宣言）・[`row_codec`]（行バイト表現）・
//! `sql::parser`（INSERT/UPDATE/UPSERT リテラル束縛）から呼ばれる。
//! version／variant ビットは検証しない（nil `00000000-0000-0000-0000-000000000000`・
//! 全 1 `ffffffff-ffff-ffff-ffff-ffffffffffff` も有効値として受理する）。

use std::fmt;

/// 正規テキスト表記の長さ（`8-4-4-4-12` 形。ハイフン 4 個 + 16 進数字 32 個）。
pub const TEXT_LEN: usize = 36;

/// ハイフンの出現位置（`TEXT_LEN` 未満の入力を弾いたあとの添字アクセスは
/// この定数群を経由し、範囲外添字を作らない）。
const HYPHEN_POSITIONS: [usize; 4] = [8, 13, 18, 23];

/// 128bit UUID 値。RFC 4122 のネットワークバイトオーダーで保持する。
///
/// `Ord`（derive）はバイト列の辞書順（`memcmp` と同じ）になり、これは
/// 符号なし 128bit big-endian の大小および正規テキストの辞書順のどちらとも
/// 一致する（本モジュールの単体テストで機械的に固定）。WHERE 比較・二次索引・
/// ORDER BY がこの順序を共有する唯一の定義とする（本 Issue では未結線。
/// 対象外は #891・#893 へ申し送り）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid([u8; 16]);

/// UUID テキストリテラルの解析エラー。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UuidTextError {
    /// 入力バイト長が `TEXT_LEN`（36）と異なる。
    Length,
    /// 長さは正しいがハイフン位置・16 進数字のいずれかが規範形を満たさない。
    Format,
}

impl fmt::Display for UuidTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UuidTextError::Length => write!(f, "invalid UUID length"),
            UuidTextError::Format => write!(f, "invalid UUID format"),
        }
    }
}

impl std::error::Error for UuidTextError {}

impl Uuid {
    /// 16 バイトの生値から構築する（row_codec のデコード経路が使う）。
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Uuid(bytes)
    }

    /// 16 バイトの生値（ネットワークバイトオーダー）を返す。
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for Uuid {
    /// 小文字 16 進の `8-4-4-4-12` 正規テキストへ整形する。wire の
    /// DataRow テキスト・HTTP JSON 文字列はこの表記を共有する（U4）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; TEXT_LEN];
        let mut out_idx = 0usize;
        let mut byte_idx = 0usize;
        while byte_idx < 16 {
            if HYPHEN_POSITIONS.contains(&out_idx) {
                out[out_idx] = b'-';
                out_idx += 1;
                continue;
            }
            let byte = self.0[byte_idx];
            out[out_idx] = HEX[(byte >> 4) as usize];
            out[out_idx + 1] = HEX[(byte & 0x0f) as usize];
            out_idx += 2;
            byte_idx += 1;
        }
        // out は ASCII のみで構成されるため str への変換は必ず成功する。
        let text = std::str::from_utf8(&out).unwrap_or("");
        f.write_str(text)
    }
}

/// 1 桁の 16 進 ASCII 文字を数値へ変換する。untrusted 入力の走査でのみ使う
/// ため添字アクセスは行わず `match` で完結させる。
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// UUID テキストリテラルを厳密文法で解析する（U3）。
///
/// 受理するのはちょうど 36 バイトの `8-4-4-4-12` 形（ハイフンは位置
/// 8・13・18・23）だけ。16 進数字は大文字・小文字混在可。波括弧・
/// `urn:uuid:` 接頭辞・ハイフンなし 32 桁・ハイフン位置違い・前後空白・
/// 非 ASCII はすべて拒否する。長さは解析前に検査する（DoS 防止。
/// coding-rust.md: untrusted 入力の長さ検証）。
pub fn parse_uuid_text(s: &str) -> Result<Uuid, UuidTextError> {
    let bytes = s.as_bytes();
    if bytes.len() != TEXT_LEN {
        return Err(UuidTextError::Length);
    }
    let mut out = [0u8; 16];
    let mut out_idx = 0usize;
    let mut in_idx = 0usize;
    while in_idx < TEXT_LEN {
        if HYPHEN_POSITIONS.contains(&in_idx) {
            let Some(&c) = bytes.get(in_idx) else {
                return Err(UuidTextError::Format);
            };
            if c != b'-' {
                return Err(UuidTextError::Format);
            }
            in_idx += 1;
            continue;
        }
        let Some(&hi_c) = bytes.get(in_idx) else {
            return Err(UuidTextError::Format);
        };
        let Some(&lo_c) = bytes.get(in_idx + 1) else {
            return Err(UuidTextError::Format);
        };
        let Some(hi) = hex_digit(hi_c) else {
            return Err(UuidTextError::Format);
        };
        let Some(lo) = hex_digit(lo_c) else {
            return Err(UuidTextError::Format);
        };
        let Some(slot) = out.get_mut(out_idx) else {
            return Err(UuidTextError::Format);
        };
        *slot = (hi << 4) | lo;
        out_idx += 1;
        in_idx += 2;
    }
    if out_idx != 16 {
        return Err(UuidTextError::Format);
    }
    Ok(Uuid(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_lowercase() {
        let text = "12345678-9abc-def0-1234-56789abcdef0";
        let u = parse_uuid_text(text).expect("parse should succeed");
        assert_eq!(u.to_string(), text);
    }

    #[test]
    fn uppercase_input_normalizes_to_lowercase_output() {
        let text = "12345678-9ABC-DEF0-1234-56789ABCDEF0";
        let u = parse_uuid_text(text).expect("parse should succeed");
        assert_eq!(u.to_string(), "12345678-9abc-def0-1234-56789abcdef0");
    }

    #[test]
    fn mixed_case_input_is_accepted() {
        let text = "12345678-9aBc-DeF0-1234-56789aBcDeF0";
        assert!(parse_uuid_text(text).is_ok());
    }

    #[test]
    fn nil_uuid_is_valid() {
        let text = "00000000-0000-0000-0000-000000000000";
        let u = parse_uuid_text(text).expect("nil UUID should be valid");
        assert_eq!(u.as_bytes(), &[0u8; 16]);
    }

    #[test]
    fn all_ones_uuid_is_valid() {
        let text = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let u = parse_uuid_text(text).expect("all-ones UUID should be valid");
        assert_eq!(u.as_bytes(), &[0xffu8; 16]);
    }

    #[test]
    fn rejects_too_short() {
        let text = "12345678-9abc-def0-1234-56789abcdef"; // 35 文字
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Length));
    }

    #[test]
    fn rejects_too_long() {
        let text = "12345678-9abc-def0-1234-56789abcdef00"; // 37 文字
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Length));
    }

    #[test]
    fn rejects_braces() {
        let text = "{12345678-9abc-def0-1234-56789abcdef0}";
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Length));
    }

    #[test]
    fn rejects_urn_prefix() {
        let text = "urn:uuid:12345678-9abc-def0-1234-56789abcdef";
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Length));
    }

    #[test]
    fn rejects_no_hyphens() {
        // 36 文字ちょうど（ハイフン位置を任意の 16 進数字で埋めた形）だが
        // ハイフンを一切含まない。
        let text = "1234567809abc0def001234056789abcdef0";
        assert_eq!(text.len(), TEXT_LEN);
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Format));
    }

    #[test]
    fn rejects_wrong_hyphen_position() {
        let text = "1234567-89abc-def0-1234-56789abcdef0";
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Format));
    }

    #[test]
    fn rejects_non_hex_digit() {
        let text = "1234567g-9abc-def0-1234-56789abcdef0";
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Format));
    }

    #[test]
    fn rejects_leading_whitespace() {
        let text = " 2345678-9abc-def0-1234-56789abcdef0";
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Format));
    }

    #[test]
    fn rejects_trailing_whitespace() {
        let text = "12345678-9abc-def0-1234-56789abcdef ";
        assert_eq!(parse_uuid_text(text), Err(UuidTextError::Format));
    }

    #[test]
    fn rejects_multibyte_characters() {
        // 全角文字を含む入力（バイト長は 36 と一致しうるが非 ASCII のため拒否される）。
        let text = "１2345678-9abc-def0-1234-56789abcdef0";
        assert!(parse_uuid_text(text).is_err());
    }

    /// バイト順（`Ord`）と正規テキストの辞書順が一致することを固定する
    /// （U2。WHERE・二次索引・ORDER BY が共有する唯一の順序の定義）。
    #[test]
    fn byte_order_matches_canonical_text_order() {
        let mut values = vec![
            "00000000-0000-0000-0000-000000000000",
            "00000000-0000-0000-0000-000000000001",
            "0fffffff-ffff-ffff-ffff-ffffffffffff",
            "10000000-0000-0000-0000-000000000000",
            "7fffffff-ffff-ffff-ffff-ffffffffffff",
            "80000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-fffffffffffe",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ];
        let parsed: Vec<Uuid> = values
            .iter()
            .map(|s| parse_uuid_text(s).expect("valid literal"))
            .collect();
        let mut sorted_by_ord = parsed.clone();
        sorted_by_ord.sort();
        let sorted_texts: Vec<String> = sorted_by_ord.iter().map(|u| u.to_string()).collect();
        values.sort_unstable();
        assert_eq!(sorted_texts, values);
    }

    #[test]
    fn sort_result_is_independent_of_input_order() {
        let a = parse_uuid_text("00000000-0000-0000-0000-000000000001").unwrap();
        let b = parse_uuid_text("10000000-0000-0000-0000-000000000000").unwrap();
        let c = parse_uuid_text("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        let mut v1 = vec![c, a, b];
        let mut v2 = vec![b, c, a];
        v1.sort();
        v2.sort();
        assert_eq!(v1, vec![a, b, c]);
        assert_eq!(v2, vec![a, b, c]);
    }
}
