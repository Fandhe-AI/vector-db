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
//!
//! Issue #732（TASK-172・NOSQL-8 ポインタ）で、オブジェクトの重複キー拒否・数値
//! リテラルの RFC 8259 準拠の厳格化を行った（先頭ゼロ・小数部/指数部の数字欠落等
//! を拒否）。[`JsonError`] は [`crate::error_format::ClassifiedError`] を実装し、
//! `wire_code()` で `42601`（`UnsupportedSqlSyntax`）を返す。

use std::collections::BTreeMap;

use crate::error_format::{ClassifiedError, ErrorClass};

/// [`parse_json`] が受理するネスト深さの上限（スタック消費・DoS 対策）。
/// コンテナ（配列・オブジェクト）のネスト数の上限であり、`JsonParser::parse_object`・
/// `JsonParser::parse_array`（いずれも非公開）の入口で `depth >= MAX_JSON_DEPTH` を
/// 検査することで、空コンテナ（`[]`・`{}`）を最内に置いた場合でも 1 段多く受理して
/// しまう off-by-one（Issue #733 の境界テストで検出）が生じない形に統一している。
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

impl JsonError {
    /// SQLSTATE 風 `wire_code`（coding-rust.md「エラー型は SQLSTATE 風 wire_code の
    /// 設計に従う」）。[`ClassifiedError`] へ委譲する（実装型ごとに再定義しない。
    /// `tenant.rs::TenantWriteError::wire_code` と同じパターン）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }
}

/// [`parse_json`] の失敗はすべて「受理範囲外の構文」（`42601`）として写像する。
/// 重複キー・非 RFC 8259 数値・深さ/件数/長さ上限超過のいずれも同じ分類とし、
/// 内部詳細（バイト位置等）はクライアントへ運ばない（security.md P0）。
impl ClassifiedError for JsonError {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::UnsupportedSqlSyntax
    }

    fn client_message(&self) -> String {
        "invalid JSON".to_string()
    }
}

/// [`parse_json`] が返す JSON 値。オブジェクトは `BTreeMap` で保持するため、
/// キーの反復順序はキー文字列の昇順で決定的になる（呼び出し元が結果を再現可能に
/// 扱えるようにするための設計選択。ハッシュ順や挿入順には依存しない）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

/// JSON 数値リテラルの分類済み表現（Issue #823・PR #823 レビュー指摘対応）。
///
/// `f64` 単一表現では 2^53 を超える整数が桁落ちし、`insert.rs` の `id`
/// 疑似列のような「精度を落とさず整数として受理するか判定したい」呼び出し元が
/// 丸め後の値でしか判定できなかった（例: `9007199254740993` が最近傍の
/// 偶数である `9007199254740992` へ丸められ、上限検査を通過して別の行 id へ
/// 書き込まれ得た）。構文上「小数点・指数部を含まない整数リテラル」かどうかは
/// パース時点のテキストでのみ判定できるため、[`JsonParser::parse_number`] が
/// 精度を失う `f64` 変換の**前**に整数／小数を分類し、整数は `u64`／`i64` の
/// 無損失表現で保持する（オーバーフロー時のみ `Float` へ縮退。RFC 8259 上は
/// 有効な数値のため拒否はしない）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JsonNumber {
    /// 小数点・指数部を含まない非負整数リテラル（`"-"` 始まりでない）。
    /// `u64` の範囲で無損失に表現できる。
    PosInt(u64),
    /// 小数点・指数部を含まない負の整数リテラル（`"-"` 始まり。`-0` を含む）。
    /// `i64` の範囲で無損失に表現できる。
    NegInt(i64),
    /// 小数点・指数部を含む、または整数として `u64`／`i64` に収まらない
    /// リテラル。`f64` 変換（丸めを伴いうる）で保持する。
    Float(f64),
}

impl JsonNumber {
    /// 全 variant を `f64` へ変換する（丸めを伴いうる。表示・比較等、精度が
    /// 問題にならない用途向け）。
    pub fn as_f64(self) -> f64 {
        match self {
            JsonNumber::PosInt(n) => n as f64,
            JsonNumber::NegInt(n) => n as f64,
            JsonNumber::Float(f) => f,
        }
    }

    /// `PosInt` のときのみ無損失な `u64` を返す（`id` 疑似列のように
    /// 非負整数を精度を落とさず要求する呼び出し元向け）。
    pub fn as_exact_u64(self) -> Option<u64> {
        match self {
            JsonNumber::PosInt(n) => Some(n),
            _ => None,
        }
    }
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
        // コンテナ入口でも深さ上限を検査する（空オブジェクトを最内に置いた入力が
        // `parse_value` の値ベース判定だけでは 1 段多く受理されてしまう off-by-one
        // を防ぐ。`parse_value` 側の `depth > MAX_JSON_DEPTH` は多層防御として残置）。
        if depth >= MAX_JSON_DEPTH {
            return Err(JsonError);
        }
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
            // RFC 8259 は重複キーの扱いをパーサ依存とするが、後勝ちで無警告に
            // 上書きすると呼び出し元（NoSQL 表層等）が別実装と解釈を違えうる
            // （confusion attack の温床）。fail-closed に拒否する（Issue #732）。
            match map.entry(key) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(value);
                }
                std::collections::btree_map::Entry::Occupied(_) => return Err(JsonError),
            }
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
        // `parse_object` と同じ理由でコンテナ入口でも深さ上限を検査する。
        if depth >= MAX_JSON_DEPTH {
            return Err(JsonError);
        }
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
        // 呼び出しのたびに `out.chars().count()` で文字列全体を再走査すると
        // エスケープを多く含む入力で O(n^2) になる（未認証入力が到達し得る
        // 経路で DoS になり得るため P0。codex-review PR #813 指摘）。
        // 1 文字（またはバイト列片）を追加するたびに増分カウントし、
        // 全体再走査を行わない O(n) 方式へ変更する。
        let mut char_count: usize = 0;
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
                    // この 1 片ぶんの文字数だけを数える（`out` 全体は再走査しない）。
                    char_count += s.chars().count();
                    out.push_str(s);
                    if char_count > MAX_JSON_STRING_CHARS {
                        return Err(JsonError);
                    }
                    continue;
                }
            }
            // エスケープ経路はいずれも 1 文字だけを `out` へ追加する。
            char_count += 1;
            if char_count > MAX_JSON_STRING_CHARS {
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

    /// RFC 8259 `number = [ "-" ] int [ frac ] [ exp ]` に沿って明示的に検証する。
    /// `int = "0" / ( digit1-9 *DIGIT )` を `match` の分岐そのもので表現している
    /// ため、先頭ゼロの直後に数字が続く入力（`"01"` 等）は `Some(b'0')` 分岐が
    /// 1 桁しか消費しない構造上、後続の桁が未消費のまま残り
    /// `parse_json`（末尾ゴミ検査）または呼び出し元の走査で自然に拒否される
    /// （本関数内では桁数を数えない）。`frac`・`exp` はそれぞれ「区切り文字の後に
    /// 数字が 1 つも無い」場合（`"1."`・`"1e"`・`"1e+"` 等）を明示的に拒否する。
    /// `+` 符号始まり・`.` 始まり・`NaN`／`Infinity` は呼び出し元 `parse_value` の
    /// 分岐条件（`Some(b'-') | Some(b'0'..=b'9')` のときのみ本関数へ到達）により
    /// 構造的に到達しないため、ここでは扱わない（Issue #732）。
    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(JsonError),
        }
        // 小数点・指数部の有無を記録する（両方とも不在の場合のみ整数リテラルとして
        // `u64`／`i64` の無損失表現を試みる。丸めを伴う `f64` 変換は最後の手段）。
        let mut is_integer_literal = true;
        if self.peek() == Some(b'.') {
            is_integer_literal = false;
            self.pos += 1;
            let frac_start = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == frac_start {
                return Err(JsonError);
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_integer_literal = false;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == exp_start {
                return Err(JsonError);
            }
        }
        if self.pos - start > 64 {
            return Err(JsonError);
        }
        let Some(slice) = self.bytes.get(start..self.pos) else {
            return Err(JsonError);
        };
        let Ok(text) = std::str::from_utf8(slice) else {
            return Err(JsonError);
        };
        if is_integer_literal {
            // 小数点・指数部を含まない整数リテラル。`f64` へ変換する前に
            // `u64`／`i64` へのパースを試み、範囲内なら無損失表現で保持する
            // （PR #823 レビュー指摘: 2^53 を超える整数が `f64` の丸めで
            // 別の整数へエイリアスし、`id` 上限検査等をすり抜ける問題への対応）。
            // オーバーフローのみ `Float` へフォールバックする（RFC 8259 上は
            // 有効な数値のため受理自体は継続する）。
            if negative {
                if let Ok(n) = text.parse::<i64>() {
                    return Ok(JsonValue::Number(JsonNumber::NegInt(n)));
                }
            } else if let Ok(n) = text.parse::<u64>() {
                return Ok(JsonValue::Number(JsonNumber::PosInt(n)));
            }
        }
        text.parse::<f64>()
            .map(|f| JsonValue::Number(JsonNumber::Float(f)))
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

    // 重複キー拒否（Issue #732・TASK-172・NOSQL-8）: 後勝ちで無警告に上書きせず
    // fail-closed に拒否する。トップレベル・ネスト先いずれも対象。
    #[test]
    fn parse_json_rejects_duplicate_key_at_top_level() {
        let err = parse_json(r#"{"a":1,"a":2}"#).unwrap_err();
        assert_eq!(err, JsonError);
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn parse_json_rejects_duplicate_key_in_nested_object() {
        assert_eq!(parse_json(r#"{"a":{"b":1,"b":2}}"#).unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_accepts_non_duplicate_keys() {
        let value = parse_json(r#"{"a":1,"b":2}"#).unwrap();
        match value {
            JsonValue::Object(map) => {
                assert_eq!(
                    map.get("a"),
                    Some(&JsonValue::Number(JsonNumber::PosInt(1)))
                );
                assert_eq!(
                    map.get("b"),
                    Some(&JsonValue::Number(JsonNumber::PosInt(2)))
                );
            }
            _ => panic!("expected object"),
        }
    }

    // 数値構文の RFC 8259 厳格化（Issue #732）: 先頭ゼロ・小数部/指数部の数字欠落
    // 等の非準拠表記を拒否する。`+`／`.` 始まり・`NaN`／`Infinity` は
    // `parse_value` の分岐条件により既に構造的に拒否されるため、ここでは
    // その回帰防止のみ確認する。
    #[test]
    fn parse_json_rejects_non_rfc8259_numbers() {
        let rejected = [
            "01",
            "-01",
            "00",
            "+1",
            ".5",
            "1.",
            "1.e5",
            "1e",
            "1e+",
            "NaN",
            "Infinity",
            "-Infinity",
        ];
        for input in rejected {
            assert_eq!(
                parse_json(input).unwrap_err(),
                JsonError,
                "expected rejection for {input:?}"
            );
        }
    }

    #[test]
    fn parse_json_accepts_rfc8259_numbers() {
        let accepted: &[(&str, JsonNumber)] = &[
            ("0", JsonNumber::PosInt(0)),
            ("-0", JsonNumber::NegInt(0)),
            ("0.5", JsonNumber::Float(0.5)),
            ("-0.5", JsonNumber::Float(-0.5)),
            ("123", JsonNumber::PosInt(123)),
            ("-123", JsonNumber::NegInt(-123)),
            ("1.5e10", JsonNumber::Float(1.5e10)),
            ("1.5E-10", JsonNumber::Float(1.5e-10)),
            ("0e0", JsonNumber::Float(0.0)),
            ("100", JsonNumber::PosInt(100)),
        ];
        for (input, expected) in accepted {
            let value =
                parse_json(input).unwrap_or_else(|_| panic!("expected accept for {input:?}"));
            assert_eq!(value, JsonValue::Number(*expected), "input={input:?}");
        }
    }

    // 精度を失う `f64` 変換の前に整数リテラルを判定する契約（PR #823 レビュー
    // 指摘対応）: 2^53 を超える整数リテラルが最近傍の偶数へ丸められて
    // 別の整数値へエイリアスすることなく、無損失な `u64`／`i64` 表現のまま
    // 保持されることを固定する。
    #[test]
    fn parse_json_number_preserves_integer_precision_beyond_f64_safe_range() {
        // 2^53 + 1。`f64` へ直接変換すると最近傍の偶数 2^53 へ丸められる値。
        let value = parse_json("9007199254740993").expect("valid JSON number");
        assert_eq!(
            value,
            JsonValue::Number(JsonNumber::PosInt(9_007_199_254_740_993))
        );

        // u64::MAX も無損失に保持される。
        let value = parse_json(&u64::MAX.to_string()).expect("valid JSON number");
        assert_eq!(value, JsonValue::Number(JsonNumber::PosInt(u64::MAX)));

        // 小数点を含む場合は整数であっても丸めを伴う Float 表現になる
        // （`id` 疑似列等の呼び出し元は `as_exact_u64()` が `None` を返すため
        // 拒否する。桁数上限 64 文字の範囲内で構成する）。
        let value = parse_json("9007199254740993.0").expect("valid JSON number");
        assert!(matches!(value, JsonValue::Number(JsonNumber::Float(_))));
    }

    #[test]
    fn parse_json_number_rejection_reports_unsupported_sql_syntax_wire_code() {
        let err = parse_json("01").unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }
}
