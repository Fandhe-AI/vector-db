//! 3 つの Recall 回帰ハーネス（`tests/hybrid_recall.rs`／`tests/rerank_recall.rs`／
//! `tests/query_planning_recall.rs`）の層 B（`#[ignore]` ゲート）が共有する、
//! ANN opt-in（Issue #412。前提: ADR `docs/design/ann-index-adoption.md` B 案
//! 〔Issue #403 Accepted〕・EXPLAIN 露出〔Issue #411〕）検索エンジン切替 fixture。
//!
//! `hnsw::provider::HnswSearchProvider::search` は常に brute-force へ委譲する
//! 契約であり、各ハーネスが直接使う `SearchProvider`（[`engine::kernel::
//! SearchProvider`]）の差し替えだけでは ANN 経路は発火しない。ANN の実 seam
//! （`sql::hnsw_cache::HnswIndexCache`／`sql::hnsw_hybrid::HnswDenseProvider`）は
//! いずれも `pub(crate)` のため、結合テスト（`tests/`）から ANN 経路へ到達できる
//! 唯一の production API は **SQL 表層**（`EngineCore::from_storage_with_engine
//! (storage, hnsw_kind(..))` ＋ `execute_sql` の `ORDER BY HYBRID(...)`）である
//! （`tests/hnsw_cache.rs::hybrid_queries_use_hnsw_dense_provider_and_match_
//! default_engine_recall` と同型の構成をここへ共通化する）。
//!
//! 既定経路（[`RecallEngine::BruteForce`]）は各ハーネスの既存 in-memory 測定
//! コード（`ParallelSearchProvider` ＋ `engine::hybrid::hybrid_search` を直接
//! 呼ぶ経路）をそのまま通し、本 fixture には一切触れない——層 A・層 B とも既存の
//! 実測値・固定値アサーションに影響を与えない（Issue #412 設計判断 2）。
#![allow(dead_code)]

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::storage::{Storage, Visibility};

// 本ファイルを取り込む各 `tests/*.rs` が `#[path = "../src/test_util/temp_db.rs"]
// mod temp_db;` をクレートルートで宣言している前提で `super::temp_db` を参照する
// （`mod temp_db` をこのファイル自身でも宣言すると、同一クレート内で同じ物理
// ファイルを 2 箇所から `mod` してしまい `clippy::duplicate_mod` に抵触する
// ——`hybrid_recall.rs` は本 fixture 追加前から独自に `mod temp_db;` を
// 宣言済みだったため顕在化した）。
use super::temp_db::{unique_db_path, CleanupGuard};

/// 検索エンジン種別（Issue #412・R1）。[`RecallEngine::from_env`] が唯一の
/// 生成経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallEngine {
    /// 既定。各ハーネスの既存 in-memory 測定コードをそのまま通す。
    BruteForce,
    /// ANN opt-in（SQL 表層 `EngineCore::from_storage_with_engine` 経由）。
    Hnsw,
}

impl RecallEngine {
    /// `RECALL_ENGINE` 環境変数から解決する。未設定・空文字列・`"brute_force"`
    /// は [`RecallEngine::BruteForce`]、`"hnsw"` は [`RecallEngine::Hnsw`]。
    /// それ以外は fail-closed で panic する（`sql/mode.rs` の「厳密一致のみ
    /// 受理」方針と同型。未知値を黙って既定へ倒すと、typo で意図せず ANN 測定が
    /// 静かにスキップされる事故を防げないため）。
    pub fn from_env() -> Self {
        let raw = std::env::var("RECALL_ENGINE").ok();
        match Self::parse(raw.as_deref()) {
            Ok(engine) => engine,
            Err(msg) => panic!("{msg}"),
        }
    }

    /// [`Self::from_env`] の純関数本体（環境変数を直接読まない。単体テスト用に
    /// 切り出す）。
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim) {
            None | Some("") | Some("brute_force") => Ok(Self::BruteForce),
            Some("hnsw") => Ok(Self::Hnsw),
            Some(other) => Err(format!(
                "RECALL_ENGINE must be unset, \"brute_force\", or \"hnsw\" (got {other:?})"
            )),
        }
    }

    /// ゲート出力行へ付加するトークン（数値を含まない。
    /// `.claude/rules/spec-confidentiality.md` 準拠）。
    pub fn token(self) -> &'static str {
        match self {
            Self::BruteForce => "brute_force",
            Self::Hnsw => "hnsw",
        }
    }
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// [`SqlHybridFixture::stats`] が返す ANN opt-in エンジンの統計サマリ
/// （`sql::hnsw_cache::HnswIndexCacheStats` は `pub(crate)` モジュール配下のため
/// 結合テストからは型名を綴れない。フィールド値だけを公開型へ複製する）。
/// 機微情報を含まないため出力可（オーナー判断 2026-08-29・
/// `.claude/rules/spec-confidentiality.md`「数値基準・実測値」の許可範囲。
/// 閾値そのものではなく統計カウンタ）。
#[derive(Debug, Clone, Copy, Default)]
pub struct AnnStats {
    pub builds: u64,
    pub build_failures: u64,
    pub rebuilds: u64,
    pub delta_searches: u64,
    pub fallbacks: u64,
    pub hybrid_dense_searches: u64,
    pub hybrid_queries: u64,
    pub hybrid_rounds_max: u64,
    pub ef_cap_fallbacks: u64,
    pub entries: usize,
}

/// SQL 表層（`EngineCore::execute_sql`）経由で hybrid クエリを発行するための
/// 最小テーブル fixture（`docs(embedding VECTOR(dim), body TEXT)`。単一テナント
/// `tenant-a`・`Visibility::Public` 固定）。`tests/hnsw_cache.rs` の
/// `hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall` と
/// 同型の構成を、`RecallEngine::BruteForce`／`Hnsw` の両方で使えるよう共通化する。
pub struct SqlHybridFixture {
    core: EngineCore,
    ctx: PolicyContext,
    _guard: CleanupGuard,
}

impl SqlHybridFixture {
    /// `rows`（`(id, vector, body_text)`）を `docs` へ投入し、`engine` に応じて
    /// ANN opt-in（`from_storage_with_engine` ＋ `hnsw_kind`）または既定エンジン
    /// （`from_storage` ＋ `default_engine`）でオープンする。
    pub fn new(dim: u32, rows: &[(u64, Vec<f32>, String)], engine: RecallEngine) -> Self {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(dim), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        );
        let dir = unique_db_path("recall-engine-sql-hybrid");
        let guard = CleanupGuard(dir.clone());
        let storage = Storage::open(&dir).expect("open storage");
        storage.create_table(&schema).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for (id, vector, text) in rows {
            let op_id =
                OperationId::parse(&format!("recall-engine-seed-{id}")).expect("valid op id");
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                *id,
                Visibility::Public,
                &[Value::Vector(vector.clone()), Value::Text(text.clone())],
                &op_id,
            )
            .unwrap_or_else(|e| panic!("insert row id={id} failed: {e}"));
        }
        let core = match engine {
            RecallEngine::Hnsw => {
                let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                    .expect("valid hnsw params");
                EngineCore::from_storage_with_engine(storage, kind)
            }
            RecallEngine::BruteForce => {
                EngineCore::from_storage(storage, search_engine::default_engine())
            }
        };
        Self {
            core,
            ctx,
            _guard: guard,
        }
    }

    /// `SELECT id FROM docs ORDER BY HYBRID(embedding, '<vec>', body, '<text>')
    /// LIMIT k` を実行し `(id, score)` を返す。`score` は `sql/exec.rs` の hybrid
    /// 分岐が書き込む `ResultRow::score`（RRF 融合スコア降順）。
    pub fn hybrid_top(&self, query_vec: &[f32], query_text: &str, k: usize) -> Vec<(u64, f64)> {
        let sql = format!(
            "SELECT id FROM docs ORDER BY HYBRID(embedding, {}, body, '{}') LIMIT {k}",
            vec_literal(query_vec),
            sql_escape(query_text),
        );
        let result = self
            .core
            .execute_sql(&self.ctx, &sql)
            .expect("hybrid query should succeed");
        result.rows.into_iter().map(|r| (r.id, r.score)).collect()
    }

    /// ANN opt-in エンジンの内部統計。機微情報を含まないため出力可
    /// （オーナー判断 2026-08-29・`.claude/rules/spec-confidentiality.md`
    /// 「数値基準・実測値」の許可範囲。閾値そのものではなく統計カウンタ）。
    pub fn stats(&self) -> AnnStats {
        let s = self.core.hnsw_index_cache_stats();
        AnnStats {
            builds: s.builds,
            build_failures: s.build_failures,
            rebuilds: s.rebuilds,
            delta_searches: s.delta_searches,
            fallbacks: s.fallbacks,
            hybrid_dense_searches: s.hybrid_dense_searches,
            hybrid_queries: s.hybrid_queries,
            hybrid_rounds_max: s.hybrid_rounds_max,
            ef_cap_fallbacks: s.ef_cap_fallbacks,
            entries: s.entries,
        }
    }

    /// 非 vacuous 検証（Issue #412 設計判断 4）。`expect_indexed` が真の場合
    /// （コーパス行数 >= `MIN_INDEXED_ROWS`）は実際に索引が構築され構築失敗が
    /// ないこと・hybrid 密側再取得ループが実際に索引経路を通ったことを固定する
    /// （構築失敗→負のキャッシュ→黙って brute-force で「ANN pass」を誤報告する
    /// 経路を防ぐ）。偽の場合（`MIN_INDEXED_ROWS` 未満。構造的に brute-force）は
    /// 逆に索引が一切構築されていないことを固定し、「ANN 通過」と誤認しない
    /// ようにする。
    pub fn assert_ann_non_vacuous(&self, expect_indexed: bool) {
        let stats = self.stats();
        if expect_indexed {
            assert!(stats.builds >= 1, "expected at least one HNSW build");
            assert_eq!(stats.build_failures, 0, "HNSW build must not fail");
            assert!(
                stats.hybrid_dense_searches > 0,
                "expected the hybrid dense refetch loop to use the HNSW index"
            );
        } else {
            assert_eq!(
                stats.builds, 0,
                "corpus below MIN_INDEXED_ROWS must never build an HNSW index"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RecallEngine;

    #[test]
    fn parse_accepts_unset_empty_and_brute_force_as_default() {
        assert_eq!(RecallEngine::parse(None), Ok(RecallEngine::BruteForce));
        assert_eq!(RecallEngine::parse(Some("")), Ok(RecallEngine::BruteForce));
        assert_eq!(
            RecallEngine::parse(Some("brute_force")),
            Ok(RecallEngine::BruteForce)
        );
    }

    #[test]
    fn parse_accepts_hnsw() {
        assert_eq!(RecallEngine::parse(Some("hnsw")), Ok(RecallEngine::Hnsw));
    }

    #[test]
    fn parse_trims_surrounding_whitespace() {
        // GitHub Actions の variable 展開が末尾改行を持ち込む経路
        // （`recall_threshold_from_env` 等、他ゲートの慣行と同様）を許容する。
        assert_eq!(RecallEngine::parse(Some(" hnsw\n")), Ok(RecallEngine::Hnsw));
    }

    #[test]
    fn parse_rejects_unknown_values_fail_closed() {
        for raw in ["HNSW", "ann", "bruteforce", "0"] {
            assert!(
                RecallEngine::parse(Some(raw)).is_err(),
                "expected {raw:?} to be rejected"
            );
        }
    }

    #[test]
    fn token_does_not_reveal_numeric_thresholds() {
        assert_eq!(RecallEngine::BruteForce.token(), "brute_force");
        assert_eq!(RecallEngine::Hnsw.token(), "hnsw");
    }
}
