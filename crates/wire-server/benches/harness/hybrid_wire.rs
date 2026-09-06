//! `hybrid_rrf` 6,178µs（`docs/design/crossdb-bench.md`）の wire／SQL 表層／
//! engine 内 hybrid 経路の内訳を切り分ける（Issue #465）ための時間非依存
//! ロジック。
//!
//! `hybrid_wire_profile_bench.rs`（実測本体・時間依存）と
//! `tests/hybrid_wire_profile_accept.rs`（`make ci` 対象の回帰テスト）の双方から
//! `#[path]` で取り込む。`knn_wire.rs`（Issue #463）を拡張しない理由:
//! `knn_wire_profile_bench.rs` のコーパスは `embedding` 列のみで、`body` 列を
//! 足すと同 Issue の基線比較が壊れるため、hybrid 専用の最小ハーネスを独立に
//! 新設する。rounds パース・min/median・帯判定・レンダリングは
//! [`super::knn_wire`] を再利用し、本モジュールは hybrid 固有の SQL 文字列
//! 組み立て・決定的本文生成のみを持つ。
//!
//! `std`・`super::rng::DeterministicRng` のみに依存する（engine の内部 API・
//! 非既定 feature には依存しない）。

use std::fmt::Write as _;

use super::sql_c1::{SqlC1Error, VectorLiteral};

/// 投影種別（Issue #465）: crossdb 規範形（`SELECT id`）と、本文列複製を
/// 含む対照投影（`SELECT id, body`）を区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HybridWireProjection {
    /// `SELECT id`（crossdb 規範形）。
    Id,
    /// `SELECT id, body`（investigational。本文列を候補行分複製する）。
    IdAndBody,
}

impl HybridWireProjection {
    fn select_list(self) -> &'static str {
        match self {
            HybridWireProjection::Id => "id",
            HybridWireProjection::IdAndBody => "id, body",
        }
    }
}

fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// クエリ本文テキストが SQL 単一引用符リテラルへそのまま埋め込んで安全かを
/// 検証する（ASCII 英数字・空白・ハイフンのみ許可。[`generate_body_text`] が
/// 組み立てるテキストは常にこの部分集合だが、埋め込み直前に必ず検証する——
/// `.claude/rules/coding-rust.md`「SQL / プラン文字列の組み立てに未検証入力を
/// 連結しない」）。
fn validate_query_text(text: &str) -> Result<(), SqlC1Error> {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
    {
        Ok(())
    } else {
        Err(SqlC1Error::InvalidIdentifier("query_text"))
    }
}

/// hybrid_rrf 経由の規範形クエリ文字列を投影種別付きで組み立てる（Issue #465）:
/// `SELECT <id|id, body> FROM <table> ORDER BY hybrid_rrf(<vector_column>,
/// '<literal>', <text_column>, '<query_text>') LIMIT <k>`
/// （`crates/engine/benches/harness/hybrid_profile.rs::
/// sql_hybrid_statement_with_projection` の wire-server 側複製。engine 内部
/// API を経由できない wire-server クレートからも同じ規範形を組み立てられる
/// ようにする）。
pub fn hybrid_statement(
    table: &str,
    vector_column: &str,
    text_column: &str,
    literal: &VectorLiteral,
    query_text: &str,
    k: usize,
    projection: HybridWireProjection,
) -> Result<String, SqlC1Error> {
    if !is_valid_identifier(table) {
        return Err(SqlC1Error::InvalidIdentifier("table"));
    }
    if !is_valid_identifier(vector_column) {
        return Err(SqlC1Error::InvalidIdentifier("vector_column"));
    }
    if !is_valid_identifier(text_column) {
        return Err(SqlC1Error::InvalidIdentifier("text_column"));
    }
    validate_query_text(query_text)?;
    let select_list = projection.select_list();
    Ok(format!(
        "SELECT {select_list} FROM {table} ORDER BY hybrid_rrf({vector_column}, '{literal}', {text_column}, '{query_text}') LIMIT {k}"
    ))
}

/// 疎チャネル本文を組み立てる合成語彙（`crates/engine/benches/harness/
/// hybrid_profile.rs::WORDS` と同一方式・独立した定義。spec 由来の値ではない）。
const WORDS: [&str; 40] = [
    "vector", "search", "database", "index", "query", "hybrid", "dense", "sparse", "rank", "score",
    "token", "term", "corpus", "document", "field", "value", "match", "filter", "tenant", "policy",
    "storage", "engine", "kernel", "cache", "cluster", "shard", "batch", "stream", "graph", "plan",
    "cost", "latency", "profile", "metric", "sample", "seed", "vocab", "weight", "rerank", "fuse",
];

/// 決定的 RNG（`super::rng::DeterministicRng`）から本文（30 語／文書。
/// `WORDS` からの合成語彙）を生成する（`hybrid_profile.rs::generate_corpus` の
/// 疎チャネル生成と同じ方式）。
pub fn generate_body_text(rng: &mut super::rng::DeterministicRng) -> String {
    let mut out = String::new();
    for i in 0..30 {
        if i > 0 {
            out.push(' ');
        }
        let idx = (rng.next_u64() as usize) % WORDS.len();
        let _ = write!(out, "{}", WORDS[idx]);
    }
    out
}

/// クエリ本文（3 語／クエリ。コーパス本文の語彙から抽出するため疎チャネルで
/// 一致が生じる）を生成する。
pub fn generate_query_text(rng: &mut super::rng::DeterministicRng) -> String {
    let mut out = String::new();
    for i in 0..3 {
        if i > 0 {
            out.push(' ');
        }
        let idx = (rng.next_u64() as usize) % WORDS.len();
        let _ = write!(out, "{}", WORDS[idx]);
    }
    out
}
