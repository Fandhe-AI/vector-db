//! LLM クエリプランニング（TASK-110、対象ビヘイビア: PLAN-1。ポインタ:
//! `docs/spec/05-tasks.md` TASK-110・`docs/spec/04-behavior/query-planning.md`
//! PLAN-1）。
//!
//! 責務境界: 常駐 LLM プロセス（Ollama 等）に対する**純粋なクエリ展開クライアント層**を
//! 提供する。`storage`/`catalog`/`policy` へは一切結線しない（`dictionary.rs`・
//! `chunking.rs` と同じ流儀）。辞書的情報源（TASK-109・`dictionary.rs`）から得た
//! `Arc<Dictionary>` を固定接頭辞コンテキストへレンダリングし、質問文と連結した
//! プロンプトを [`LlmClient`] へ渡し、その応答を厳格に検証済みの [`QueryExpansion`]
//! へパースするところまでを本モジュールの責務とする。辞書スナップショット
//! （`dictionary_snapshot`。テナント境界の担保はここが担う）との結線・
//! `EngineCore` への注入点は `core.rs::EngineCore::with_query_planner` /
//! `core.rs::EngineCore::plan_query` の管轄（本モジュールへは持ち込まない）。
//!
//! 依存は追加しない（dependency-policy.md）。HTTP クライアントは
//! `std::net::TcpStream` 上に POST・`Content-Length`／chunked 応答対応の最小限の
//! HTTP/1.1 クライアントを自作し（本リポが pg wire v3 を自作している方針と整合。
//! 汎用 HTTP クライアント化はしない）、JSON はリクエスト組み立て用の文字列エスケープと
//! 応答パース用の最小 JSON パーサを本モジュール内に閉じて自作する（`dictionary.rs` が
//! 正規表現を手書きパーサで代替した前例に倣う）。
//!
//! fail-closed の判断根拠（security.md「不安全な設計」対応）:
//! - LLM 出力（untrusted）は [`QueryExpansion`] として型付き・上限検証を通過したものだけを
//!   返す。プロンプトインジェクションで LLM が異常出力しても、影響は検証済みの検索語・
//!   ソフトヒントに閉じ、SQL・プラン文字列へ未検証のまま連結される経路を持たない
//!   （ハードフィルタ化しない。呼び出し元がハードフィルタへ昇格させないことは
//!   TASK-111 側の設計判断）。
//! - 未知フィールドは無視するが、必須フィールドの型不一致・欠落・上限超過は
//!   [`PlanError::InvalidResponse`] として応答全体を拒否する（切り詰めではなく拒否。
//!   不完全な展開結果を正常応答として黙って返さない）。
//! - 接続先は既定でループバック（`127.0.0.1`）のみとし、untrusted 入力から
//!   ホスト・URL を組み立てる経路を持たない（SSRF 対策）。
//! - `PlanError`・ログ出力にプロンプト本文・LLM 応答本文を含めない（`embedding.rs`
//!   の `EmbedError` と同じ P0 方針）。
//!
//! `wire_code`（ERR-2）への接続は SQL 表層（`USING PLAN` 展開。TASK-77）が
//! `PlanError` を写像する段になった時点の管轄とし、本モジュールでは行わない。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use std::collections::BTreeMap;

/// [`LlmClient::complete`] の失敗理由。メッセージは英語
/// （japanese-style.md: プログラム出力文字列は英語）。プロンプト本文・応答本文は
/// 含めない（security.md P0・`embedding.rs::EmbedError` と同じ方針）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// LLM サービスへ接続できなかった（未起動・DNS 不達・接続拒否等）。
    Unavailable,
    /// 接続・読み書きが設定タイムアウトを超えた。
    Timeout,
    /// LLM サービスの応答（HTTP 応答自体、または応答内の JSON）が想定形状ではなかった。
    InvalidResponse,
    /// 応答本文が [`MAX_RESPONSE_BYTES`] を超えた（無制限確保を避けるための拒否）。
    ResponseTooLarge,
    /// 組み立てたプロンプトが [`MAX_PROMPT_BYTES`] を超えた。
    PromptTooLarge,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Unavailable => write!(f, "query planning llm unavailable"),
            PlanError::Timeout => write!(f, "query planning llm request timed out"),
            PlanError::InvalidResponse => {
                write!(f, "query planning llm returned an invalid response")
            }
            PlanError::ResponseTooLarge => {
                write!(f, "query planning llm response exceeded the size limit")
            }
            PlanError::PromptTooLarge => write!(f, "query planning prompt exceeded the size limit"),
        }
    }
}

impl std::error::Error for PlanError {}

/// 常駐 LLM プロセスに対するクエリ展開の差し替え可能な注入点
/// （[`crate::embedding::Embedder`] と同型の設計）。
///
/// 呼び出し元は `core.rs::EngineCore::plan_query`。
pub trait LlmClient: Send + Sync {
    /// `prompt` を LLM へ渡し、生成テキストをそのまま返す（JSON 抽出・検証は
    /// 呼び出し元の [`parse_expansion`] が行う。本 trait は completion 取得のみに
    /// 責務を限定する）。
    fn complete(&self, prompt: &str) -> Result<String, PlanError>;
}

/// クエリ展開結果。あくまでソフトな補助情報であり、ハードフィルタ化はしない
/// （TASK-111 のソフトブースト機構でも同様。本モジュールのドキュメント参照）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryExpansion {
    /// 展開された検索語。件数上限は [`MAX_SEARCH_TERMS`]、各語の長さ上限は
    /// [`MAX_TERM_LEN`] 文字。
    pub search_terms: Vec<String>,
    /// パスのソフトヒント（部分一致等の補助情報として使う想定。ハードフィルタではない）。
    pub path_hint: Option<String>,
    /// シンボル種別のソフトヒント（例: "fn"・"struct"）。
    pub kind_hint: Option<String>,
}

// ---------------------------------------------------------------------------
// プロンプト組み立て
// ---------------------------------------------------------------------------

/// 固定接頭辞（辞書コンテキスト）の最大バイト量。超過分は決定的に切り詰める
/// （`dictionary.rs` の切り詰め方針を踏襲。バイト境界ではなく `push_bounded` が
/// 追加単位ごとに判定するため、実際の切り詰め位置は追加単位の境界に揃う）。
pub const MAX_PROMPT_PREFIX_BYTES: usize = 64 * 1024;

/// 質問文として受理する最大文字数。超過分は決定的に切り詰める。
pub const MAX_QUESTION_CHARS: usize = 2_000;

/// 組み立て後のプロンプト全体（接頭辞＋質問）の最大バイト量。
/// [`MAX_PROMPT_PREFIX_BYTES`]・[`MAX_QUESTION_CHARS`] の上限を足し合わせても
/// 通常はここへ到達しないが、独立した防御線として保持する（コーディング規約:
/// 「長さフィールドは上限検証してからアロケーションに使う」）。
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// LLM へ渡す出力スキーマの指示文（英語固定。プログラム出力文字列は英語の規約）。
const INSTRUCTION_HEADER: &str = "You are a query planning assistant for a local vector search \
engine. Given the dictionary context below and a user question, respond with ONLY a single \
JSON object (no markdown fences, no extra text before or after) with exactly these fields:\n\
  \"search_terms\": an array of short search keyword strings\n\
  \"path_hint\": a file path substring hint, or null\n\
  \"kind_hint\": a symbol kind hint such as \"fn\" or \"struct\", or null\n\
Do not include any explanation outside the JSON object.\n";

/// `out` の末尾へ `s` を追加する。追加後のバイト長が [`MAX_PROMPT_PREFIX_BYTES`] を
/// 超える場合は追加せず `false` を返す（呼び出し元はこれ以上の追加を打ち切る）。
fn push_bounded(out: &mut String, s: &str) -> bool {
    if out.len().saturating_add(s.len()) > MAX_PROMPT_PREFIX_BYTES {
        return false;
    }
    out.push_str(s);
    true
}

/// 辞書的情報源（TASK-109・`dictionary.rs::Dictionary`）から決定的な固定接頭辞
/// テキストをレンダリングする。`Dictionary` の内部コンテナはすべて `BTreeSet`/
/// `BTreeMap` のため反復順序は決定的（モジュールドキュメント「決定性」参照）で、
/// 同一辞書（同一世代のスナップショット）から呼ぶ限り常にバイト同一の出力を返す
/// （常駐 LLM プロセスに対する接頭辞の使い回し前提。`docs/spec/04-behavior/
/// query-planning.md` PLAN-1 の性質に対応）。
pub fn render_prompt_prefix(dictionary: &crate::dictionary::Dictionary) -> String {
    let mut out = String::new();
    // ヘッダ自体が上限を超えることは実運用上ない（固定の短い定数文字列）が、
    // 一貫した打ち切り契約のため同じ `push_bounded` を通す。
    if !push_bounded(&mut out, INSTRUCTION_HEADER) {
        return out;
    }

    if !push_bounded(&mut out, "\n# Symbols\n") {
        return out;
    }
    for symbol in &dictionary.symbols {
        let line = format!(
            "{}:{} {:?} {}\n",
            symbol.path, symbol.line, symbol.kind, symbol.name
        );
        if !push_bounded(&mut out, &line) {
            return out;
        }
    }

    if !push_bounded(&mut out, "\n# Files\n") {
        return out;
    }
    for path in &dictionary.file_tree.paths {
        let line = format!("{path}\n");
        if !push_bounded(&mut out, &line) {
            return out;
        }
    }
    for (ext, count) in &dictionary.file_tree.by_extension {
        let line = format!("ext:{ext}={count}\n");
        if !push_bounded(&mut out, &line) {
            return out;
        }
    }
    for (dir, count) in &dictionary.file_tree.by_top_dir {
        let line = format!("dir:{dir}={count}\n");
        if !push_bounded(&mut out, &line) {
            return out;
        }
    }

    if !push_bounded(&mut out, "\n# Top terms\n") {
        return out;
    }
    for (term, count) in &dictionary.term_index {
        let line = format!("{term}={count}\n");
        if !push_bounded(&mut out, &line) {
            return out;
        }
    }

    out
}

/// `prefix`（[`render_prompt_prefix`] の出力）と untrusted な `question` から完全な
/// プロンプトを組み立てる。`question` は制御文字（改行・タブを除く）を除去し
/// [`MAX_QUESTION_CHARS`] 文字で決定的に切り詰める（untrusted 入力の有界化。
/// coding-rust.md）。
pub fn render_full_prompt(prefix: &str, question: &str) -> Result<String, PlanError> {
    let sanitized: String = question
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(MAX_QUESTION_CHARS)
        .collect();

    let mut out = String::new();
    out.push_str(prefix);
    out.push_str("\n# Question\n");
    out.push_str(&sanitized);
    out.push('\n');

    if out.len() > MAX_PROMPT_BYTES {
        return Err(PlanError::PromptTooLarge);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 最小 JSON 値・パーサ（依存追加なし。応答パース専用）
// ---------------------------------------------------------------------------

/// 最小 JSON パーサが受理するネスト深さの上限（スタック消費・DoS 対策）。
const MAX_JSON_DEPTH: usize = 16;
/// JSON 文字列リテラル 1 つあたりの最大文字数（トランスポート層の DoS 対策専用の
/// 緩い上限）。本パーサは Ollama `/api/generate` 応答本体（`response` フィールドに
/// LLM の生成テキスト全体を、`context` 配列にトークン列を含みうる）と、そこから
/// 抽出した展開結果 JSON の両方に使い回す。展開結果側の意味的な上限
/// （検索語件数・各語長・ヒント長）は [`MAX_SEARCH_TERMS`]・[`MAX_TERM_LEN`]・
/// [`MAX_HINT_LEN`] として [`parse_expansion`] が独立に検証するため、本パーサ自身の
/// 上限はメモリ確保量を [`MAX_RESPONSE_BYTES`] 相当に頭打ちさせるためだけの粗い
/// バックストップでよい（狭すぎると実際の Ollama 応答を transport 層で拒否して
/// しまう。1 文字 1 バイト以上を消費するため、応答本文の総バイト数上限
/// [`MAX_RESPONSE_BYTES`] を超える文字数にはそもそも到達しない）。
const MAX_JSON_STRING_CHARS: usize = MAX_RESPONSE_BYTES;
/// JSON 配列・オブジェクトが保持できる要素数の上限（同上の理由でトランスポート層の
/// 粗い上限。`context` 配列はプロンプト＋応答のトークン数に比例し数千要素になりうる）。
const MAX_JSON_CONTAINER_ITEMS: usize = 65_536;

#[derive(Debug, Clone, PartialEq)]
enum JsonValue {
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

    fn expect_byte(&mut self, expected: u8) -> Result<(), PlanError> {
        match self.bump() {
            Some(b) if b == expected => Ok(()),
            _ => Err(PlanError::InvalidResponse),
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, PlanError> {
        if depth > MAX_JSON_DEPTH {
            return Err(PlanError::InvalidResponse);
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            _ => Err(PlanError::InvalidResponse),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, PlanError> {
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
                return Err(PlanError::InvalidResponse);
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
                _ => return Err(PlanError::InvalidResponse),
            }
        }
        Ok(JsonValue::Object(map))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, PlanError> {
        self.expect_byte(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            if items.len() >= MAX_JSON_CONTAINER_ITEMS {
                return Err(PlanError::InvalidResponse);
            }
            let value = self.parse_value(depth + 1)?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(PlanError::InvalidResponse),
            }
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, PlanError> {
        self.expect_byte(b'"')?;
        let mut out = String::new();
        loop {
            let b = self.bump().ok_or(PlanError::InvalidResponse)?;
            match b {
                b'"' => break,
                b'\\' => {
                    let esc = self.bump().ok_or(PlanError::InvalidResponse)?;
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
                                    return Err(PlanError::InvalidResponse);
                                }
                                let low = self.parse_hex4()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    // 低位サロゲートが続かない = 孤立した高位サロゲート。
                                    // 破損文字列を U+FFFD へ丸めて正常応答として返すと
                                    // fail-closed 方針に反するため拒否する
                                    // （codex-review PR #252 P2 指摘）。
                                    return Err(PlanError::InvalidResponse);
                                }
                                let scalar = 0x10000u32
                                    + (u32::from(cp) - 0xd800) * 0x400
                                    + (u32::from(low) - 0xdc00);
                                out.push(char::from_u32(scalar).ok_or(PlanError::InvalidResponse)?);
                            } else if (0xdc00..=0xdfff).contains(&cp) {
                                // ペアの相方を伴わない孤立した低位サロゲートも不正な
                                // JSON 文字列表現であり、fail-closed に拒否する。
                                return Err(PlanError::InvalidResponse);
                            } else {
                                out.push(
                                    char::from_u32(u32::from(cp))
                                        .ok_or(PlanError::InvalidResponse)?,
                                );
                            }
                        }
                        _ => return Err(PlanError::InvalidResponse),
                    }
                }
                // 生の制御文字は JSON 仕様上不正（要エスケープ）。fail-closed に拒否する。
                0x00..=0x1f => return Err(PlanError::InvalidResponse),
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
                        return Err(PlanError::InvalidResponse);
                    };
                    let Ok(s) = std::str::from_utf8(slice) else {
                        return Err(PlanError::InvalidResponse);
                    };
                    out.push_str(s);
                }
            }
            if out.chars().count() > MAX_JSON_STRING_CHARS {
                return Err(PlanError::InvalidResponse);
            }
        }
        Ok(out)
    }

    fn parse_hex4(&mut self) -> Result<u16, PlanError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let b = self.bump().ok_or(PlanError::InvalidResponse)?;
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(PlanError::InvalidResponse),
            };
            value = value
                .checked_mul(16)
                .and_then(|v| v.checked_add(u16::from(digit)))
                .ok_or(PlanError::InvalidResponse)?;
        }
        Ok(value)
    }

    fn parse_bool(&mut self) -> Result<JsonValue, PlanError> {
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
            Err(PlanError::InvalidResponse)
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue, PlanError> {
        let rest = self.bytes.get(self.pos..).unwrap_or(&[]);
        if rest.starts_with(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(PlanError::InvalidResponse)
        }
    }

    fn parse_number(&mut self) -> Result<JsonValue, PlanError> {
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
            return Err(PlanError::InvalidResponse);
        }
        let Some(slice) = self.bytes.get(start..self.pos) else {
            return Err(PlanError::InvalidResponse);
        };
        let Ok(text) = std::str::from_utf8(slice) else {
            return Err(PlanError::InvalidResponse);
        };
        text.parse::<f64>()
            .map(JsonValue::Number)
            .map_err(|_| PlanError::InvalidResponse)
    }
}

/// `s` 全体を単一の JSON 値としてパースする（末尾に余分な非空白文字があれば拒否する。
/// 上限は [`MAX_JSON_DEPTH`]・[`MAX_JSON_STRING_CHARS`]・[`MAX_JSON_CONTAINER_ITEMS`]）。
fn parse_json(s: &str) -> Result<JsonValue, PlanError> {
    let mut parser = JsonParser::new(s);
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(PlanError::InvalidResponse);
    }
    Ok(value)
}

/// `s` の中から最初のバランスの取れた JSON オブジェクト（`{`〜対応する `}`）を抽出する。
/// LLM 応答にコードフェンス（```` ```json ... ``` ````）や前後の説明文が混じっていても、
/// 最初に現れる完結した `{...}` を拾える（文字列リテラル内の `{`/`}` は無視する）。
/// バイト単位の走査だが、比較対象はすべて 1 バイト ASCII（`"`・`\`・`{`・`}`）であり
/// UTF-8 の継続バイト（`0x80..=0xBF`）はこれらと衝突しないため、マルチバイト文字を
/// 含む入力に対しても境界を壊さない。
fn extract_first_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut i = start;
    while i < bytes.len() {
        // ループ条件 `i < bytes.len()` により範囲内が保証されるが、untrusted 入力
        // 経路のため添字アクセスではなく `get()` を使う（coding-rust.md）。
        // 取得できなければ走査を打ち切り fail-closed に拒否する。
        let &b = bytes.get(i)?;
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return s.get(start..=i);
                    }
                    if depth < 0 {
                        return None;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// 展開結果パース
// ---------------------------------------------------------------------------

/// [`QueryExpansion::search_terms`] の最大件数。
pub const MAX_SEARCH_TERMS: usize = 32;
/// 検索語 1 件あたりの最大文字数。
pub const MAX_TERM_LEN: usize = 128;
/// `path_hint`/`kind_hint` の最大文字数。
pub const MAX_HINT_LEN: usize = 256;

/// LLM の生成テキスト `response` から [`QueryExpansion`] を厳格パースする。
///
/// コードフェンス等が混在していても最初に現れる完結した JSON オブジェクトを対象とする
/// （[`extract_first_json_object`]）。未知フィールドは無視するが、`search_terms`・
/// `path_hint`・`kind_hint` の型不一致・欠落（キー自体が存在しない）・上限超過は
/// いずれも [`PlanError::InvalidResponse`] として応答全体を fail-closed に拒否する
/// （切り詰めて部分的に受理しない。モジュールドキュメント参照）。
pub fn parse_expansion(response: &str) -> Result<QueryExpansion, PlanError> {
    let json_text = extract_first_json_object(response).ok_or(PlanError::InvalidResponse)?;
    let value = parse_json(json_text)?;
    let JsonValue::Object(map) = value else {
        return Err(PlanError::InvalidResponse);
    };

    let search_terms = match map.get("search_terms") {
        Some(JsonValue::Array(items)) => {
            if items.len() > MAX_SEARCH_TERMS {
                return Err(PlanError::InvalidResponse);
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let JsonValue::String(s) = item else {
                    return Err(PlanError::InvalidResponse);
                };
                if s.chars().count() > MAX_TERM_LEN {
                    return Err(PlanError::InvalidResponse);
                }
                out.push(s.clone());
            }
            out
        }
        _ => return Err(PlanError::InvalidResponse),
    };

    let path_hint = parse_optional_hint(map.get("path_hint"))?;
    let kind_hint = parse_optional_hint(map.get("kind_hint"))?;

    Ok(QueryExpansion {
        search_terms,
        path_hint,
        kind_hint,
    })
}

/// `path_hint`/`kind_hint` 共通の検証: キー自体が存在しない場合は拒否
/// （「欠落」。モジュールドキュメント参照）。値が JSON `null` なら `None`、
/// 文字列なら長さ上限・制御文字の不在を検証して `Some` を返す。それ以外の型は拒否する。
fn parse_optional_hint(value: Option<&JsonValue>) -> Result<Option<String>, PlanError> {
    match value {
        None => Err(PlanError::InvalidResponse),
        Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(s)) => {
            if s.chars().count() > MAX_HINT_LEN {
                return Err(PlanError::InvalidResponse);
            }
            if s.chars().any(|c| c.is_control()) {
                return Err(PlanError::InvalidResponse);
            }
            Ok(Some(s.clone()))
        }
        _ => Err(PlanError::InvalidResponse),
    }
}

// ---------------------------------------------------------------------------
// Ollama クライアント（自作 HTTP/1.1・依存追加なし）
// ---------------------------------------------------------------------------

/// [`OllamaConfig`] の既定接続先ホスト（ループバック限定。SSRF 面の安全側。
/// モジュールドキュメント参照）。
pub const DEFAULT_OLLAMA_HOST: &str = "127.0.0.1";
/// [`OllamaConfig`] の既定接続先ポート（Ollama の既定値）。
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

/// 1 回の応答本文として受理する最大バイト数（無制限確保を避ける安全弁）。
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// HTTP 応答ヘッダ部（ステータス行＋ヘッダ）として受理する最大バイト数。
const MAX_HTTP_HEADER_BYTES: usize = 8 * 1024;
/// `Transfer-Encoding: chunked` デコード中にストリームから読み取る総バイト数
/// （デコード後データだけでなく、チャンクサイズ行・CRLF 等のオーバーヘッドも含む）の
/// 上限。デコード後データは [`MAX_RESPONSE_BYTES`] で頭打ちにしているが、1 バイトずつの
/// 極小チャンクを大量に返す応答ではチャンクメタデータのオーバーヘッドだけが際限なく
/// 蓄積しうる（codex-review PR #252 P1 指摘）。オーバーヘッドを含む受信総量そのものに
/// 独立した上限を設けて無制限アロケーションを防ぐ（AGENTS.md 入力検証方針）。
const MAX_CHUNKED_TOTAL_BYTES: usize = MAX_RESPONSE_BYTES * 8;

/// [`OllamaClient`] の接続構成。
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// 接続先ホスト。既定はループバック（[`DEFAULT_OLLAMA_HOST`]）。untrusted 入力から
    /// 組み立てず、サーバー構成としてのみ設定する（SSRF 対策。モジュールドキュメント）。
    pub host: String,
    /// 接続先ポート（既定 [`DEFAULT_OLLAMA_PORT`]）。
    pub port: u16,
    /// 使用するモデル名。
    pub model: String,
    /// TCP 接続確立のタイムアウト。
    pub connect_timeout: Duration,
    /// 読み書きのタイムアウト。
    pub read_timeout: Duration,
    /// Ollama の `keep_alive` パラメータ（常駐プロセスをアンロードさせない設定値。
    /// 例: `"5m"`）。
    pub keep_alive: String,
}

impl OllamaConfig {
    /// `model` 以外は既定値（ループバック・標準ポート・タイムアウト 30 秒・
    /// `keep_alive: "5m"`）で構成を作る。
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            host: DEFAULT_OLLAMA_HOST.to_string(),
            port: DEFAULT_OLLAMA_PORT,
            model: model.into(),
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(30),
            keep_alive: "5m".to_string(),
        }
    }
}

/// Ollama（`POST /api/generate`、`stream: false`）に対する [`LlmClient`] 実装。
/// TCP 接続・HTTP/1.1 リクエスト送信・応答受信は本構造体内に閉じる。
#[derive(Debug, Clone)]
pub struct OllamaClient {
    config: OllamaConfig,
}

impl OllamaClient {
    pub fn new(config: OllamaConfig) -> Self {
        Self { config }
    }
}

impl LlmClient for OllamaClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(PlanError::PromptTooLarge);
        }
        let body = build_generate_request_body(&self.config.model, prompt, &self.config.keep_alive);
        let response_bytes = http_post_json(&self.config, "/api/generate", &body)?;
        let response_text =
            String::from_utf8(response_bytes).map_err(|_| PlanError::InvalidResponse)?;
        extract_response_field(&response_text)
    }
}

/// `out` の末尾へ JSON 文字列リテラル（引用符込み）として `s` をエスケープ出力する
/// （リクエスト組み立て専用の最小エスケーパ。応答パースの [`JsonParser`] とは非対称の
/// 単純な片方向処理で十分）。
fn json_write_escaped_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Ollama `/api/generate`（`stream: false`）のリクエスト JSON 本文を組み立てる。
fn build_generate_request_body(model: &str, prompt: &str, keep_alive: &str) -> String {
    let mut out = String::new();
    out.push_str("{\"model\":");
    json_write_escaped_string(&mut out, model);
    out.push_str(",\"prompt\":");
    json_write_escaped_string(&mut out, prompt);
    out.push_str(",\"stream\":false,\"keep_alive\":");
    json_write_escaped_string(&mut out, keep_alive);
    out.push('}');
    out
}

/// `std::io::Error` を [`PlanError`] へ分類する（タイムアウト系は
/// [`PlanError::Timeout`]、それ以外は [`PlanError::Unavailable`]）。
fn classify_io_error(e: std::io::Error) -> PlanError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => PlanError::Timeout,
        _ => PlanError::Unavailable,
    }
}

/// `haystack` 内で `needle` が最初に現れるバイトオフセットを返す（依存追加なしの
/// 単純な部分列探索。応答本文は [`MAX_RESPONSE_BYTES`] 級で小さく、線形探索で十分）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// ステータス行が `HTTP/1.x 2xx ...` 形式かを判定する。
fn is_success_status_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(proto) = parts.next() else {
        return false;
    };
    if !proto.starts_with("HTTP/1.") {
        return false;
    }
    let Some(code) = parts.next() else {
        return false;
    };
    code.len() == 3 && code.starts_with('2') && code.bytes().all(|b| b.is_ascii_digit())
}

/// `POST {path}` で `body`（JSON）を送信し、HTTP 応答本文（生バイト列）を返す。
/// 接続・読み書きタイムアウト、応答ヘッダ・本文の各サイズ上限、`Content-Length`／
/// `Transfer-Encoding: chunked` の双方に対応する（モジュールドキュメント参照）。
fn http_post_json(config: &OllamaConfig, path: &str, body: &str) -> Result<Vec<u8>, PlanError> {
    let addr = format!("{}:{}", config.host, config.port);
    let mut addrs = addr.to_socket_addrs().map_err(|_| PlanError::Unavailable)?;
    let sock_addr = addrs.next().ok_or(PlanError::Unavailable)?;

    let mut stream = TcpStream::connect_timeout(&sock_addr, config.connect_timeout)
        .map_err(classify_io_error)?;
    stream
        .set_read_timeout(Some(config.read_timeout))
        .map_err(classify_io_error)?;
    stream
        .set_write_timeout(Some(config.read_timeout))
        .map_err(classify_io_error)?;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        path = path,
        host = config.host,
        len = body.len(),
        body = body,
    );
    stream
        .write_all(request.as_bytes())
        .map_err(classify_io_error)?;

    read_http_response_body(&mut stream)
}

/// HTTP 応答（ステータス行・ヘッダ・本文）を受信して本文のみを返す。
fn read_http_response_body(stream: &mut TcpStream) -> Result<Vec<u8>, PlanError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut read_buf = [0u8; 4096];

    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            let end = pos + 4;
            // 区切り（`\r\n\r\n`）は 1 回の `read` が一括で大きなバイト列を
            // 運んできた場合、`buf.len()` が `MAX_HTTP_HEADER_BYTES` 未満のまま
            // 見つかることがある（下方の読み取り量制限だけでは、区切り発見時点の
            // 総量チェックを兼ねられない）。区切り発見のたびに実ヘッダ長
            // （`end`）そのものを上限と照合し、超過分は fail-closed に拒否する
            // （codex-review PR #252 P1 指摘）。
            if end > MAX_HTTP_HEADER_BYTES {
                return Err(PlanError::InvalidResponse);
            }
            break end;
        }
        if buf.len() >= MAX_HTTP_HEADER_BYTES {
            return Err(PlanError::InvalidResponse);
        }
        // 1 回の read で受理できる残り許容量までに読み取り量を制限し、
        // 上限超過分を一度でも `buf` へ取り込まないようにする。
        let remaining = MAX_HTTP_HEADER_BYTES - buf.len();
        let read_len = remaining.min(read_buf.len());
        let n = stream
            .read(&mut read_buf[..read_len])
            .map_err(classify_io_error)?;
        if n == 0 {
            return Err(PlanError::InvalidResponse);
        }
        buf.extend_from_slice(&read_buf[..n]);
    };

    // `header_end` は直前のループが `find_subslice(&buf, ...)` の一致位置から
    // 導いた値で、一致条件成立時点で常に `buf.len()` 以内（境界はループの不変条件で
    // 保証済み。`[]` は panic しない）。加えて上のループで
    // `header_end <= MAX_HTTP_HEADER_BYTES` も確認済み。
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| PlanError::InvalidResponse)?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(PlanError::InvalidResponse)?;
    if !is_success_status_line(status_line) {
        return Err(PlanError::InvalidResponse);
    }

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            let len: usize = value.parse().map_err(|_| PlanError::InvalidResponse)?;
            if len > MAX_RESPONSE_BYTES {
                return Err(PlanError::ResponseTooLarge);
            }
            content_length = Some(len);
        } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }

    let body = buf.split_off(header_end);

    if chunked {
        return dechunk_body(stream, body);
    }

    if let Some(len) = content_length {
        return read_fixed_length_body(stream, body, len);
    }

    // `Content-Length` も `chunked` も無い応答: EOF まで読む（上限あり）。
    read_until_eof_bounded(stream, body)
}

/// `Content-Length: len` の応答本文を、既読分 `body` に続けて `len` バイトへ到達する
/// まで読み進める。
fn read_fixed_length_body(
    stream: &mut TcpStream,
    mut body: Vec<u8>,
    len: usize,
) -> Result<Vec<u8>, PlanError> {
    let mut read_buf = [0u8; 4096];
    while body.len() < len {
        if body.len() >= MAX_RESPONSE_BYTES {
            return Err(PlanError::ResponseTooLarge);
        }
        let n = stream.read(&mut read_buf).map_err(classify_io_error)?;
        if n == 0 {
            return Err(PlanError::InvalidResponse);
        }
        body.extend_from_slice(&read_buf[..n]);
    }
    body.truncate(len);
    Ok(body)
}

/// 長さ情報を持たない応答本文を、接続が閉じられる（`read` が 0 を返す）まで読む。
fn read_until_eof_bounded(stream: &mut TcpStream, mut body: Vec<u8>) -> Result<Vec<u8>, PlanError> {
    let mut read_buf = [0u8; 4096];
    loop {
        if body.len() >= MAX_RESPONSE_BYTES {
            return Err(PlanError::ResponseTooLarge);
        }
        let n = stream.read(&mut read_buf).map_err(classify_io_error)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&read_buf[..n]);
    }
    Ok(body)
}

/// `Transfer-Encoding: chunked` の応答本文をデコードする。`buf` はヘッダ読み取り時に
/// 既に受信済みのボディ先頭部分（チャンクサイズ行の途中を含みうる）。
fn dechunk_body(stream: &mut TcpStream, mut buf: Vec<u8>) -> Result<Vec<u8>, PlanError> {
    let mut read_buf = [0u8; 4096];
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    // ストリームから読み取った総バイト数（チャンクサイズ行・CRLF オーバーヘッドを含む）。
    // `buf` の増分ごとに加算し [`MAX_CHUNKED_TOTAL_BYTES`] で頭打ちにする。
    let mut total_received: usize = buf.len();

    loop {
        // 前反復までに処理済みの先頭領域（`buf[..pos]`）を破棄する。破棄せずに
        // 追記のみ続けると、`buf` 自体の確保量が受信総量に比例して際限なく
        // 増え続ける（total_received の上限チェックだけでは “その時点までの
        // ピークメモリ” を抑えられない）ため、処理済み分は都度解放する。
        if pos > 0 {
            buf.drain(0..pos);
            pos = 0;
        }

        let size_line_end = loop {
            if let Some(rel) = find_subslice(&buf[pos..], b"\r\n") {
                break pos + rel;
            }
            if buf.len().saturating_sub(pos) > MAX_HTTP_HEADER_BYTES {
                return Err(PlanError::InvalidResponse);
            }
            let n = stream.read(&mut read_buf).map_err(classify_io_error)?;
            if n == 0 {
                return Err(PlanError::InvalidResponse);
            }
            total_received = total_received
                .checked_add(n)
                .ok_or(PlanError::ResponseTooLarge)?;
            if total_received > MAX_CHUNKED_TOTAL_BYTES {
                return Err(PlanError::ResponseTooLarge);
            }
            buf.extend_from_slice(&read_buf[..n]);
        };

        // `size_line_end` は `find_subslice` の一致位置由来で `buf.len()` 以内
        // （直前ループの不変条件）。`pos <= size_line_end` は前回反復の
        // `pos = size_line_end + 2` 更新か初期値 0 のいずれかで維持される。
        let size_line = std::str::from_utf8(&buf[pos..size_line_end])
            .map_err(|_| PlanError::InvalidResponse)?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| PlanError::InvalidResponse)?;
        pos = size_line_end + 2;

        if size == 0 {
            // トレーラ・終端 `\r\n` はデコード結果に影響しないため読み捨てて終了する。
            return Ok(out);
        }

        let new_out_len = out
            .len()
            .checked_add(size)
            .ok_or(PlanError::ResponseTooLarge)?;
        if new_out_len > MAX_RESPONSE_BYTES {
            return Err(PlanError::ResponseTooLarge);
        }

        let data_end = pos.checked_add(size).ok_or(PlanError::ResponseTooLarge)?;
        while buf.len() < data_end + 2 {
            if buf.len().saturating_sub(pos) > MAX_RESPONSE_BYTES + MAX_HTTP_HEADER_BYTES {
                return Err(PlanError::ResponseTooLarge);
            }
            let n = stream.read(&mut read_buf).map_err(classify_io_error)?;
            if n == 0 {
                return Err(PlanError::InvalidResponse);
            }
            total_received = total_received
                .checked_add(n)
                .ok_or(PlanError::ResponseTooLarge)?;
            if total_received > MAX_CHUNKED_TOTAL_BYTES {
                return Err(PlanError::ResponseTooLarge);
            }
            buf.extend_from_slice(&read_buf[..n]);
        }
        // 直前の `while buf.len() < data_end + 2` ループが抜けた時点で
        // `buf.len() >= data_end + 2` が成立している（境界はループの不変条件で保証済み）。
        out.extend_from_slice(&buf[pos..data_end]);
        pos = data_end + 2;
    }
}

/// Ollama `/api/generate`（`stream: false`）の JSON 応答本文から `response`
/// フィールド（生成テキスト）を取り出す。
fn extract_response_field(json_text: &str) -> Result<String, PlanError> {
    let value = parse_json(json_text)?;
    let JsonValue::Object(map) = value else {
        return Err(PlanError::InvalidResponse);
    };
    match map.get("response") {
        Some(JsonValue::String(s)) => Ok(s.clone()),
        _ => Err(PlanError::InvalidResponse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::{Dictionary, DictionaryBuilder, DictionaryConfig};
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::thread;

    fn build_test_dictionary() -> Dictionary {
        let mut builder = DictionaryBuilder::new(DictionaryConfig::default());
        builder.ingest("src/lib.rs", "pub fn hello() {}\npub struct Foo;\n");
        builder.ingest("README.md", "some readme body text here\n");
        builder.finish()
    }

    // --- プロンプト組み立て ---

    #[test]
    fn render_prompt_prefix_is_deterministic_for_same_dictionary() {
        let dict = build_test_dictionary();
        let a = render_prompt_prefix(&dict);
        let b = render_prompt_prefix(&dict);
        assert_eq!(a, b);
    }

    #[test]
    fn render_prompt_prefix_contains_symbols_and_files() {
        let dict = build_test_dictionary();
        let prefix = render_prompt_prefix(&dict);
        assert!(prefix.contains("src/lib.rs"));
        assert!(prefix.contains("README.md"));
        assert!(prefix.contains("hello"));
    }

    #[test]
    fn render_prompt_prefix_truncates_within_budget() {
        let mut builder = DictionaryBuilder::new(DictionaryConfig::default());
        for i in 0..5_000 {
            builder.ingest(
                &format!("src/file_{i}.rs"),
                &format!("pub fn sym_{i}() {{}}\n"),
            );
        }
        let dict = builder.finish();
        let prefix = render_prompt_prefix(&dict);
        assert!(prefix.len() <= MAX_PROMPT_PREFIX_BYTES);
    }

    #[test]
    fn render_full_prompt_strips_control_chars_and_bounds_length() {
        let prefix = "PREFIX\n";
        let question = "hello\u{0}world\u{7}!";
        let prompt = render_full_prompt(prefix, question).unwrap();
        assert!(!prompt.contains('\u{0}'));
        assert!(!prompt.contains('\u{7}'));
        assert!(prompt.starts_with(prefix));
    }

    #[test]
    fn render_full_prompt_truncates_overlong_question() {
        let prefix = "PREFIX\n";
        let question = "x".repeat(MAX_QUESTION_CHARS + 500);
        let prompt = render_full_prompt(prefix, &question).unwrap();
        let question_section = prompt.split("# Question\n").nth(1).unwrap();
        assert!(question_section.trim_end().chars().count() <= MAX_QUESTION_CHARS);
    }

    // --- 展開結果パース ---

    #[test]
    fn parse_expansion_accepts_well_formed_json() {
        let response =
            r#"{"search_terms": ["hello", "world"], "path_hint": "src/", "kind_hint": "fn"}"#;
        let expansion = parse_expansion(response).unwrap();
        assert_eq!(expansion.search_terms, vec!["hello", "world"]);
        assert_eq!(expansion.path_hint.as_deref(), Some("src/"));
        assert_eq!(expansion.kind_hint.as_deref(), Some("fn"));
    }

    #[test]
    fn parse_expansion_accepts_null_hints() {
        let response = r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#;
        let expansion = parse_expansion(response).unwrap();
        assert!(expansion.search_terms.is_empty());
        assert_eq!(expansion.path_hint, None);
        assert_eq!(expansion.kind_hint, None);
    }

    #[test]
    fn parse_expansion_strips_surrounding_code_fence_and_prose() {
        let response = "Sure, here is the plan:\n```json\n{\"search_terms\": [\"a\"], \
                         \"path_hint\": null, \"kind_hint\": null}\n```\nHope that helps!";
        let expansion = parse_expansion(response).unwrap();
        assert_eq!(expansion.search_terms, vec!["a"]);
    }

    #[test]
    fn parse_expansion_ignores_unknown_fields() {
        let response =
            r#"{"search_terms": ["a"], "path_hint": null, "kind_hint": null, "extra": 123}"#;
        let expansion = parse_expansion(response).unwrap();
        assert_eq!(expansion.search_terms, vec!["a"]);
    }

    #[test]
    fn parse_expansion_rejects_missing_required_field() {
        let response = r#"{"search_terms": ["a"], "path_hint": null}"#;
        assert_eq!(
            parse_expansion(response).unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    #[test]
    fn parse_expansion_rejects_type_mismatch() {
        let response = r#"{"search_terms": "not-an-array", "path_hint": null, "kind_hint": null}"#;
        assert_eq!(
            parse_expansion(response).unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    #[test]
    fn parse_expansion_rejects_too_many_search_terms() {
        let terms: Vec<String> = (0..MAX_SEARCH_TERMS + 1)
            .map(|i| format!("\"t{i}\""))
            .collect();
        let response = format!(
            "{{\"search_terms\": [{}], \"path_hint\": null, \"kind_hint\": null}}",
            terms.join(",")
        );
        assert_eq!(
            parse_expansion(&response).unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    #[test]
    fn parse_expansion_rejects_no_json_object_present() {
        assert_eq!(
            parse_expansion("no json here at all").unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    #[test]
    fn parse_expansion_rejects_overlong_hint() {
        let long_hint = "x".repeat(MAX_HINT_LEN + 1);
        let response = format!(
            "{{\"search_terms\": [], \"path_hint\": \"{long_hint}\", \"kind_hint\": null}}"
        );
        assert_eq!(
            parse_expansion(&response).unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    // --- 最小 JSON パーサ自体の回帰 ---

    #[test]
    fn parse_json_rejects_excess_nesting_depth() {
        let mut s = String::new();
        for _ in 0..(MAX_JSON_DEPTH + 4) {
            s.push('[');
        }
        for _ in 0..(MAX_JSON_DEPTH + 4) {
            s.push(']');
        }
        assert_eq!(parse_json(&s).unwrap_err(), PlanError::InvalidResponse);
    }

    #[test]
    fn parse_json_rejects_trailing_garbage() {
        assert_eq!(
            parse_json("{}garbage").unwrap_err(),
            PlanError::InvalidResponse
        );
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
        assert_eq!(
            parse_json("\"\\ud800\"").unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    #[test]
    fn parse_json_rejects_isolated_low_surrogate() {
        assert_eq!(
            parse_json("\"\\udc00\"").unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    #[test]
    fn parse_json_rejects_high_surrogate_not_followed_by_low_surrogate() {
        assert_eq!(
            parse_json("\"\\ud800\\u0041\"").unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    // --- OllamaClient: プロセス内 TCP スタブサーバー経由の結合テスト ---

    /// 1 接続だけを受理し、`handler` が返すバイト列をそのまま応答として返す最小の
    /// スタブサーバーを立てる。戻り値はスタブが待ち受けるアドレス。
    fn spawn_stub_server(
        handler: impl FnOnce(String) -> Vec<u8> + Send + 'static,
    ) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
        let addr = listener.local_addr().expect("local addr");
        thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut reader = BufReader::new(socket.try_clone().expect("clone socket"));
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                // ヘッダを読み飛ばす（本文はテストが直接検証しないため不要）。
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let response = handler(request_line);
                let _ = socket.write_all(&response);
            }
        });
        addr
    }

    fn config_for(addr: std::net::SocketAddr) -> OllamaConfig {
        let mut config = OllamaConfig::new("test-model");
        config.host = addr.ip().to_string();
        config.port = addr.port();
        config.connect_timeout = Duration::from_secs(2);
        config.read_timeout = Duration::from_secs(2);
        config
    }

    #[test]
    fn ollama_client_parses_normal_response() {
        let addr = spawn_stub_server(|_req| {
            let body = br#"{"model":"test-model","response":"{\"search_terms\":[\"a\"],\"path_hint\":null,\"kind_hint\":null}","done":true}"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes()
            .into_iter()
            .chain(body.iter().copied())
            .collect()
        });
        let client = OllamaClient::new(config_for(addr));
        let text = client.complete("does not matter").unwrap();
        let expansion = parse_expansion(&text).unwrap();
        assert_eq!(expansion.search_terms, vec!["a"]);
    }

    #[test]
    fn ollama_client_rejects_invalid_json_body() {
        let addr = spawn_stub_server(|_req| {
            let body = b"not json";
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                .into_bytes()
                .into_iter()
                .chain(body.iter().copied())
                .collect()
        });
        let client = OllamaClient::new(config_for(addr));
        assert_eq!(
            client.complete("q").unwrap_err(),
            PlanError::InvalidResponse
        );
    }

    #[test]
    fn ollama_client_rejects_oversized_response() {
        let addr = spawn_stub_server(|_req| {
            let len = MAX_RESPONSE_BYTES + 1;
            format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n").into_bytes()
        });
        let client = OllamaClient::new(config_for(addr));
        assert_eq!(
            client.complete("q").unwrap_err(),
            PlanError::ResponseTooLarge
        );
    }

    // 回帰テスト（codex-review PR #252 P1 指摘対応）: ヘッダ区切り（`\r\n\r\n`）が
    // 1 回の `read` で `MAX_HTTP_HEADER_BYTES` の閾値をまたいで見つかった場合、
    // 区切り発見を理由にサイズ検査より先に受理してしまわないことを確認する。
    // サーバー側で書き込みを 2 回に分け、間に短いスリープを挟むことで、
    // クライアント側の 1 回目の `read` 群がヘッダ本体（区切り未満）だけを受信し、
    // 区切りが後続の別 `read` にまたがって出現する状況を決定的に再現する。
    #[test]
    fn ollama_client_rejects_header_exceeding_limit_found_in_single_read() {
        // `spawn_stub_server` は 1 回の書き込みで応答を返すクロージャ形状のため、
        // 意図的な分割書き込みを行うにはここで専用のサーバースレッドを立てる。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
        let addr = listener.local_addr().expect("local addr");

        // 1 回目の書き込み: `MAX_HTTP_HEADER_BYTES`（8 KiB）未満のヘッダ本体
        // （区切りは含まない）。ステータス行 + パディングヘッダ行。
        let status_line = "HTTP/1.1 200 OK\r\n";
        let mut first_chunk = status_line.as_bytes().to_vec();
        // `MAX_HTTP_HEADER_BYTES` から十分な余裕（200 バイト）を残して止める。
        // 1 回目の受信だけでは上限に達しないことを保証する。
        let pad_target = MAX_HTTP_HEADER_BYTES - 200;
        while first_chunk.len() < pad_target {
            first_chunk.extend_from_slice(b"X-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        assert!(first_chunk.len() < MAX_HTTP_HEADER_BYTES);

        // 2 回目の書き込み: 区切り（`\r\n\r\n`）を含む残りのヘッダ行。単独では
        // 数百バイトで小さく、TCP の 1 回の `read` で丸ごと受信されうる大きさに
        // 収めつつ、`first_chunk` と合算するとヘッダ総量が `MAX_HTTP_HEADER_BYTES`
        // を明確に超えるよう十分なパディングを積む。
        let mut second_chunk = Vec::new();
        while second_chunk.len() < 400 {
            second_chunk.extend_from_slice(b"X-Tail: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        // 本文は他テストと同じ、正当な Ollama `/api/generate` 応答形状にする。
        // ヘッダ上限違反を見逃す旧実装では、この本文が正常にパースされ
        // `client.complete` が `Ok` を返してしまう（本テストが検出したい誤り）。
        // 上限を正しく検査する実装は、本文の妥当性に関わらずヘッダ段階で
        // `InvalidResponse` を返す。
        let body = br#"{"model":"test-model","response":"{\"search_terms\":[\"a\"],\"path_hint\":null,\"kind_hint\":null}","done":true}"#;
        second_chunk
            .extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        second_chunk.extend_from_slice(body);
        assert!(first_chunk.len() + second_chunk.len() > MAX_HTTP_HEADER_BYTES);

        let first_chunk_for_server = first_chunk.clone();
        let second_chunk_for_server = second_chunk.clone();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept stub connection");
            let mut reader = BufReader::new(socket.try_clone().expect("clone socket"));
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            socket
                .write_all(&first_chunk_for_server)
                .expect("write first chunk");
            socket.flush().expect("flush first chunk");
            // クライアント側の読み取りが 1 回目の書き込み分を使い切ってから
            // 2 回目の書き込みが独立した `read` として届くよう間隔を空ける。
            thread::sleep(Duration::from_millis(50));
            socket
                .write_all(&second_chunk_for_server)
                .expect("write second chunk");
        });

        let client = OllamaClient::new(config_for(addr));
        assert_eq!(
            client.complete("q").unwrap_err(),
            PlanError::InvalidResponse
        );
        server.join().expect("stub server thread should not panic");
    }

    // 回帰テスト（advisor 指摘対応）: 実際の Ollama `/api/generate` 非ストリーミング
    // 応答は `response`（LLM 生成テキスト全体。プロンプト接頭辞を大きく取るほど
    // 数千〜数万文字になりうる）に加え `context`（プロンプト＋応答のトークン列。
    // 数千要素の整数配列）を含む。トランスポート層の JSON パーサ上限
    // （`MAX_JSON_STRING_CHARS`/`MAX_JSON_CONTAINER_ITEMS`）が展開結果向けの狭い
    // 上限のままだと、この現実的な応答形状を `InvalidResponse` として毎回拒否して
    // しまう（スタブが小さな応答しか返さない他のテストでは検知できなかった）。
    #[test]
    fn ollama_client_accepts_realistic_wrapper_with_long_response_and_context_array() {
        let addr = spawn_stub_server(|_req| {
            let long_response_text = "x".repeat(20_000);
            let context_ints: Vec<String> = (0..3_000).map(|i| i.to_string()).collect();
            let mut response_json = String::new();
            response_json.push_str("{\"search_terms\":[],\"path_hint\":null,\"kind_hint\":null}");
            // 実際の `response` は上記のような JSON 断片の前後に自由テキストが
            // 付くことも多いため、末尾へ長い散文を足して現実の形状に近づける。
            response_json.push_str(&long_response_text);

            let mut body = String::new();
            body.push_str("{\"model\":\"test-model\",\"response\":");
            json_write_escaped_string(&mut body, &response_json);
            body.push_str(",\"context\":[");
            body.push_str(&context_ints.join(","));
            body.push_str("],\"done\":true}");

            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                .into_bytes()
                .into_iter()
                .chain(body.into_bytes())
                .collect()
        });
        let client = OllamaClient::new(config_for(addr));
        let text = client
            .complete("q")
            .expect("realistic ollama wrapper (long response + context array) should parse");
        let expansion = parse_expansion(&text).expect("embedded expansion json should parse");
        assert!(expansion.search_terms.is_empty());
    }

    #[test]
    fn ollama_client_reports_unavailable_on_connection_refused() {
        // OS に割り当てさせたポートを一旦閉じ、誰も listen していないアドレスへ
        // 接続を試みることで確実に「接続不能」を再現する。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let client = OllamaClient::new(config_for(addr));
        assert_eq!(client.complete("q").unwrap_err(), PlanError::Unavailable);
    }

    #[test]
    fn ollama_client_reports_timeout_when_server_never_responds() {
        // 接続は受理するが応答を一切書かないスタブへ、短い読み取りタイムアウトで
        // アクセスし、`Timeout`（`WouldBlock`/`TimedOut` の分類）が返ることを確認する。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
        let addr = listener.local_addr().expect("local addr");
        thread::spawn(move || {
            if let Ok((socket, _)) = listener.accept() {
                // 応答を書かずに接続だけ保持する（drop すると即座に接続が閉じ、
                // 読み取りタイムアウトではなく EOF による `InvalidResponse` に
                // なってしまうため、テストスレッドの生存期間中保持する）。
                thread::sleep(Duration::from_secs(2));
                drop(socket);
            }
        });

        let mut config = config_for(addr);
        config.connect_timeout = Duration::from_secs(2);
        config.read_timeout = Duration::from_millis(200);
        let client = OllamaClient::new(config);
        assert_eq!(client.complete("q").unwrap_err(), PlanError::Timeout);
    }

    #[test]
    fn ollama_client_parses_chunked_response() {
        let addr = spawn_stub_server(|_req| {
            let body = br#"{"model":"test-model","response":"{\"search_terms\":[],\"path_hint\":null,\"kind_hint\":null}","done":true}"#;
            let mut out = Vec::new();
            out.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            // 1 チャンクへまとめて送る（デコード経路の基本確認）。
            out.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\r\n0\r\n\r\n");
            out
        });
        let client = OllamaClient::new(config_for(addr));
        let text = client.complete("q").unwrap();
        let expansion = parse_expansion(&text).unwrap();
        assert!(expansion.search_terms.is_empty());
    }

    // 回帰テスト（codex-review PR #252 P1 指摘対応）: `dechunk_body` はデコード後
    // データを `MAX_RESPONSE_BYTES` で頭打ちにしているが、1 バイトの極小チャンクを
    // チャンク拡張パラメータ（`;` 以降のジャンク文字列）で水増しして大量に送る
    // 応答では、デコード後データはごく小さいままチャンクメタデータの
    // オーバーヘッドだけが際限なく蓄積しうる。`MAX_CHUNKED_TOTAL_BYTES` による
    // 受信総量の独立した上限で拒否されることを確認する。
    #[test]
    fn ollama_client_rejects_chunked_metadata_overhead_flood() {
        let addr = spawn_stub_server(|_req| {
            let mut out = Vec::new();
            out.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            // 各チャンクはデータ 1 バイトのみだが、チャンクサイズ行に 4000 バイトの
            // 拡張ジャンクを付け、これを 2100 回繰り返す。デコード後データ総量は
            // 2100 バイト（`MAX_RESPONSE_BYTES` の 1MiB を大きく下回る）のままだが、
            // オーバーヘッド総量は約 8.4MiB に達し `MAX_CHUNKED_TOTAL_BYTES`
            // （`MAX_RESPONSE_BYTES` の 8 倍 = 8MiB）を超える。
            let extension = "a".repeat(4000);
            for _ in 0..2100 {
                out.extend_from_slice(format!("1;{extension}\r\n").as_bytes());
                out.extend_from_slice(b"X\r\n");
            }
            out.extend_from_slice(b"0\r\n\r\n");
            out
        });
        let client = OllamaClient::new(config_for(addr));
        assert_eq!(
            client.complete("q").unwrap_err(),
            PlanError::ResponseTooLarge
        );
    }

    #[test]
    fn build_generate_request_body_escapes_prompt() {
        let body = build_generate_request_body("m", "line1\nline2\"quoted\"", "5m");
        assert!(body.contains("\\n"));
        assert!(body.contains("\\\"quoted\\\""));
    }
}
