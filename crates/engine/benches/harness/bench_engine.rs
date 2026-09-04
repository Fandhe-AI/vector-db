//! `feature_bench.rs`（examples）・`knn_profile_bench.rs`（benches）が ANN opt-in
//! （Issue #403 B 案・`search_engine::hnsw_kind`）の有無・規模を測定条件として
//! 選べるようにする、依存を持たない純パース関数群（Issue #413）。
//!
//! `crates/engine/tests/fixtures/recall_engine.rs::RecallEngine::parse`（Recall
//! ゲート層 B の ANN opt-in・Issue #412）と同じ「未設定・空・既定トークンは既定へ、
//! それ以外の未知値は fail-closed で拒否」の語彙・判定方針を、計測 example・bench
//! 側の env 変数（`BENCH_FEATURE_ENGINE`／`BENCH_KNN_PROFILE_ENGINE`・
//! `BENCH_FEATURE_SCALE`）向けに複製したもの。`ingest_profile_bench.rs::
//! read_env_var` と同じ理由（`VarError::NotUnicode` を「未設定」へ黙って合流させ
//! ない）で env 読み取りも本モジュールへ集約する。
//!
//! `super::` を参照しない純関数のみで構成する。`examples/feature_bench.rs` からは
//! `#[path = "../benches/harness/bench_engine.rs"] mod bench_engine;` で単一ファイル
//! を取り込み（`harness::` モジュールツリー全体は取り込まない）、
//! `benches/knn_profile_bench.rs` からは既存の `harness::bench_engine` 経由で使う。
//! `tests/bench_engine_accept.rs`（`cargo test`・`make ci` 対象）が本ファイルを
//! 独立に取り込み、パース関数の回帰を時間非依存に検証する。
//!
//! 単体テストは本ファイルへインラインで置かない（`harness/ab.rs::median_ratio`
//! ドキュメンテーションコメント参照。bench ターゲット〔`harness = false`〕の
//! コンパイルは `#[test]` 属性のみ除去され `#[cfg(test)]` ブロック自体は
//! コンパイルされてしまうため、`#[cfg(test)] mod tests { use super::*; ... }`
//! を置くと bench ビルド時に `use super::*` が unused import になる）。
//! テストは `tests/bench_engine_accept.rs` に集約する。

use std::env::VarError;

/// ベンチが構築する `EngineCore` の検索エンジン種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchEngine {
    /// 既定（`search_engine::default_engine()`）。
    BruteForce,
    /// ANN opt-in（`search_engine::hnsw_kind(HnswParams::default())`）。
    Hnsw,
}

impl BenchEngine {
    /// ゲート出力・JSON `meta` へ書き出すトークン（数値を含まない。
    /// `.claude/rules/spec-confidentiality.md` 準拠）。
    pub fn token(self) -> &'static str {
        match self {
            Self::BruteForce => "brute_force",
            Self::Hnsw => "hnsw",
        }
    }
}

/// env 変数値の取得エラー。`fmt::Display` を実装し呼び出し元は `eprintln!` へ
/// そのまま渡して非 0 終了する（`feature_bench.rs::fail_bench`／
/// `knn_profile_bench.rs::fail_closed` と同じ fail-closed の入口を経由する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchEngineError {
    message: String,
}

impl std::fmt::Display for BenchEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BenchEngineError {}

fn err(message: impl Into<String>) -> BenchEngineError {
    BenchEngineError {
        message: message.into(),
    }
}

/// `std::env::var` を fail-closed に読む（`ingest_profile_bench.rs::read_env_var`
/// と同型）。未設定（`NotPresent`）は `Ok(None)`、非 UTF-8（`NotUnicode`）は
/// 明示的に `Err` とし、typo・環境破損を黙って既定値へ合流させない。
pub fn read_env_var(name: &'static str) -> Result<Option<String>, BenchEngineError> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(err(format!("{name} value is not valid UTF-8"))),
    }
}

/// `raw`（`read_env_var` が返した値。前後の空白は許容: GitHub Actions の
/// variable 展開が末尾改行を持ち込む経路への対応。`recall_engine.rs` と同方針）
/// から [`BenchEngine`] を解決する。未設定・空文字列・`"brute_force"` は
/// [`BenchEngine::BruteForce`]、`"hnsw"` は [`BenchEngine::Hnsw`]。それ以外は
/// fail-closed で拒否する（黙って既定へ倒すと、typo で ANN 測定が静かに
/// スキップされる事故を防げない）。
pub fn parse_engine(raw: Option<&str>) -> Result<BenchEngine, BenchEngineError> {
    match raw.map(str::trim) {
        None | Some("") | Some("brute_force") => Ok(BenchEngine::BruteForce),
        Some("hnsw") => Ok(BenchEngine::Hnsw),
        Some(other) => Err(err(format!(
            "must be unset, \"brute_force\", or \"hnsw\" (got {other:?})"
        ))),
    }
}

/// 行数スケール倍率の上限。`max_nodes / rows_per_scale_unit` の呼び出し元計算
/// （`feature_bench.rs`: `ROWS_A + ROWS_B` = 25,000 行/単位）から
/// `hnsw::MAX_HNSW_NODES`（1,000,000）を超えない最大倍率を渡す契約とし、本関数
/// 自体は汎用の bound 付き正整数パーサとする。
pub fn parse_scale(raw: Option<&str>, max: u64) -> Result<u64, BenchEngineError> {
    let trimmed = raw.map(str::trim);
    let value: u64 = match trimmed {
        None | Some("") => 1,
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| err(format!("must be a positive integer (got {s:?})")))?,
    };
    if value == 0 {
        return Err(err("must be >= 1 (got 0)"));
    }
    if value > max {
        return Err(err(format!("must be <= {max} (got {value})")));
    }
    Ok(value)
}
