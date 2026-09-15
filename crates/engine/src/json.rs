//! 最小 JSON パーサ（依存追加なし・共有モジュール）。
//!
//! 元は `query_planner.rs` が Ollama `/api/generate` 応答パース専用として自作していた
//! パーサを、Issue #731 で共有モジュールへ移設したもの（挙動不変）。TASK-172・
//! NOSQL-8（spec ポインタ。`docs/spec/05-tasks.md` TASK-172）の NoSQL 表層が別途 JSON
//! パースを必要とする見込みのため、`query_planner` 固有のコンシューマに依存しない
//! 汎用パーサとして切り出している。
//!
//! 責務境界: untrusted なテキストを [`JsonValue`] へパースするところまでが責務で、
//! 意味的な検証（フィールドの型・件数・長さの上限等）は呼び出し元が行う
//! （`query_planner.rs::parse_expansion` が検索語件数・長さの意味的上限を独立に
//! 検証する例を参照）。エラーは常に単一の不透明な [`JsonError`] のみを返し、
//! 失敗理由（バイト位置等）を外部に漏らさない（fail-closed。security.md 準拠）。

use std::collections::BTreeMap;

/// [`parse_json`] が受理するネスト深さの上限（スタック消費・DoS 対策）。
pub const MAX_JSON_DEPTH: usize = 16;
/// JSON 文字列リテラル 1 つあたりの最大文字数（トランスポート層の DoS 対策専用の
/// 緩い上限）。呼び出し元が意味的な上限（件数・長さ等）を独立に検証する前提の、
/// メモリ確保量を頭打ちさせるためだけの粗いバックストップ（狭すぎると正常な入力を
/// transport 層で拒否してしまうため、実用上の応答本文サイズに合わせた値とする）。
pub const MAX_JSON_STRING_CHARS: usize = 1024 * 1024;
/// JSON 配列・オブジェクトが保持できる要素数の上限（同上の理由でトランスポート層の
/// 粗い上限）。
pub const MAX_JSON_CONTAINER_ITEMS: usize = 65_536;

/// [`parse_json`] の失敗を表す不透明なマーカー型。詳細な失敗理由（どのバイト位置で
/// 何が起きたか）はあえて持たない。呼び出し元は自身のエラー型へ変換して扱う
/// （`query_planner.rs` の `impl From<JsonError> for PlanError` を参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError;

/// [`parse_json`] が返す JSON 値。オブジェクトは `BTreeMap` で保持するため、
/// キーの反復順序はキー文字列の昇順で決定的になる（呼び出し元が結果を再現可能に
/// 扱えるようにするための設計選択。ハッシュ順や挿入順には依存しない）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), JsonError> {
        match self.bump() {
            Some(b) if b == expected => Ok(()),
            _ => Err(JsonError),
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth > MAX_JSON_DEPTH {
            return Err(JsonError);
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            _ => Err(JsonError),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.expect_byte(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            if map.len() >= MAX_JSON_CONTAINER_ITEMS {
                return Err(JsonError);
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect_byte(b':')?;
            let value = self.parse_value(depth + 1)?;
            map.insert(key, value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(JsonError),
            }
        }
        Ok(JsonValue::Object(map))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.expect_byte(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            if items.len() >= MAX_JSON_CONTAINER_ITEMS {
                return Err(JsonError);
            }
            let value = self.parse_value(depth + 1)?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(JsonError),
            }
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.expect_byte(b'"')?;
        let mut out = String::new();
        loop {
            let b = self.bump().ok_or(JsonError)?;
            match b {
                b'"' => break,
                b'\\' => {
                    let esc = self.bump().ok_or(JsonError)?;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.parse_hex4()?;
                            if (0xd800..=0xdbff).contains(&cp) {
                                // 高位サロゲート: 直後に `\uXXXX` 形式の低位サロゲート
                                // が続く場合のみ、正規のサロゲートペアとして 1 個の
                                // 補助平面コードポイントへ復号する（絵文字等）。
                                if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                                    return Err(JsonError);
                                }
                                let low = self.parse_hex4()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    // 低位サロゲートが続かない = 孤立した高位サロゲート。
                                    // 破損文字列を U+FFFD へ丸めて正常応答として返すと
                                    // fail-closed 方針に反するため拒否する
                                    // （codex-review PR #252 P2 指摘）。
                                    return Err(JsonError);
                                }
                                let scalar = 0x10000u32
                                    + (u32::from(cp) - 0xd800) * 0x400
                                    + (u32::from(low) - 0xdc00);
                                out.push(char::from_u32(scalar).ok_or(JsonError)?);
                            } else if (0xdc00..=0xdfff).contains(&cp) {
                                // ペアの相方を伴わない孤立した低位サロゲートも不正な
                                // JSON 文字列表現であり、fail-closed に拒否する。
                                return Err(JsonError);
                            } else {
                                out.push(char::from_u32(u32::from(cp)).ok_or(JsonError)?);
                            }
                        }
                        _ => return Err(JsonError),
                    }
                }
                // 生の制御文字は JSON 仕様上不正（要エスケープ）。fail-closed に拒否する。
                0x00..=0x1f => return Err(JsonError),
                _ => {
                    // マルチバイト UTF-8 継続バイトも含め、そのままバイト列として
                    // 再構成する（`str::from_utf8` 相当の妥当性は元の `&str` 入力が
                    // 既に保証しているため、1 バイトずつ ASCII 相当のみを個別処理し
                    // それ以外はバイト列を後段でまとめて UTF-8 復元する）。
                    let start = self.pos - 1;
                    let mut end = self.pos;
                    while let Some(next) = self.peek() {
                        if next == b'"' || next == b'\\' || next < 0x20 {
                            break;
                        }
                        end += 1;
                        self.pos += 1;
                    }
                    // untrusted 入力経路のため添字アクセスではなく `get()` で明示的に
                    // 検証する（coding-rust.md）。`start`・`end` は上の走査で
                    // 常に `self.bytes` の範囲内に収まるが、範囲外を返す実装変更に
                    // 対しても fail-closed に振る舞う。
                    let Some(slice) = self.bytes.get(start..end) else {
                        return Err(JsonError);
                    };
                    let Ok(s) = std::str::from_utf8(slice) else {
                        return Err(JsonError);
                    };
                    out.push_str(s);
                }
            }
            if out.chars().count() > MAX_JSON_STRING_CHARS {
                return Err(JsonError);
            }
        }
        Ok(out)
    }

    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let b = self.bump().ok_or(JsonError)?;
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(JsonError),
            };
            value = value
                .checked_mul(16)
                .and_then(|v| v.checked_add(u16::from(digit)))
                .ok_or(JsonError)?;
        }
        Ok(value)
    }

    fn parse_bool(&mut self) -> Result<JsonValue, JsonError> {
        // untrusted 入力経路のため添字アクセスではなく `get()` で明示的に検証する
        // （coding-rust.md）。範囲外なら `unwrap_or(&[])` で空スライスとして扱い、
        // `starts_with` が自然に `false` を返す（fail-closed）。
        let rest = self.bytes.get(self.pos..).unwrap_or(&[]);
        if rest.starts_with(b"true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if rest.starts_with(b"false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(JsonError)
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue, JsonError> {
        let rest = self.bytes.get(self.pos..).unwrap_or(&[]);
        if rest.starts_with(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(JsonError)
        }
    }

    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut saw_digit = false;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
            saw_digit = true;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if !saw_digit || self.pos - start > 64 {
            return Err(JsonError);
        }
        let Some(slice) = self.bytes.get(start..self.pos) else {
            return Err(JsonError);
        };
        let Ok(text) = std::str::from_utf8(slice) else {
            return Err(JsonError);
        };
        text.parse::<f64>()
            .map(JsonValue::Number)
            .map_err(|_| JsonError)
    }
}

/// `s` 全体を単一の JSON 値としてパースする（末尾に余分な非空白文字があれば拒否する。
/// 上限は [`MAX_JSON_DEPTH`]・[`MAX_JSON_STRING_CHARS`]・[`MAX_JSON_CONTAINER_ITEMS`]）。
pub fn parse_json(s: &str) -> Result<JsonValue, JsonError> {
    let mut parser = JsonParser::new(s);
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(JsonError);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_rejects_excess_nesting_depth() {
        let mut s = String::new();
        for _ in 0..(MAX_JSON_DEPTH + 4) {
            s.push('[');
        }
        for _ in 0..(MAX_JSON_DEPTH + 4) {
            s.push(']');
        }
        assert_eq!(parse_json(&s).unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_rejects_trailing_garbage() {
        assert_eq!(parse_json("{}garbage").unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_handles_escaped_unicode() {
        let value = parse_json("\"\\u0041\\u0042\"").unwrap();
        assert_eq!(value, JsonValue::String("AB".to_string()));
    }

    // 回帰テスト（codex-review PR #252 P2 指摘対応）: 正規のサロゲートペアは
    // 補助平面のコードポイント 1 個へ復号され、孤立サロゲート（相方を伴わない
    // 高位・低位サロゲート）は破損文字列を U+FFFD へ丸めて返さず fail-closed に
    // 拒否する。
    #[test]
    fn parse_json_decodes_surrogate_pair_to_supplementary_plane_char() {
        // U+1F600 (😀) の UTF-16 サロゲートペア表現。
        let value = parse_json("\"\\ud83d\\ude00\"").unwrap();
        assert_eq!(value, JsonValue::String("\u{1f600}".to_string()));
    }

    #[test]
    fn parse_json_rejects_isolated_high_surrogate() {
        assert_eq!(parse_json("\"\\ud800\"").unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_rejects_isolated_low_surrogate() {
        assert_eq!(parse_json("\"\\udc00\"").unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_rejects_high_surrogate_not_followed_by_low_surrogate() {
        assert_eq!(parse_json("\"\\ud800\\u0041\"").unwrap_err(), JsonError);
    }
}
