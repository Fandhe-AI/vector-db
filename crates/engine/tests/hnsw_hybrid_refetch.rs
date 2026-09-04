//! hybrid 密側再取得ループの HNSW 結線（Issue #410・親 Issue #402・前提 #408〜#409。
//! `sql::hnsw_hybrid::HnswDenseProvider`）の SQL 表層結合テスト。対象ビヘイビア
//! （ポインタのみ）: CORE-9・CORE-10・TASK-132・SEARCH-1・SEARCH-3・RLS-1〜4。
//!
//! 単体テスト（`crates/engine/src/sql/hnsw_hybrid.rs` の `#[cfg(test)]`。
//! `search_falls_back_to_brute_force_when_k_exceeds_max_ef` が上限到達時の
//! brute-force 縮退——空集合の誤返却防止——をアダプタ単体で固定済み）・
//! SQL 表層結合テスト（`crates/engine/tests/hnsw_cache.rs::
//! hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`）が
//! 「非 vacuous・Recall・テナント境界」を検証済みのため、本ファイルは Issue #410
//! の受け入れ基準のうち残る 1 点——同点誘発コーパスでの停止性・決定性——に
//! 絞って検証する（`LIMIT` は SQL 表層の許容上限〔`engine::hnsw::MAX_EF` と同値〕
//! を超えられないため、上限到達時の縮退を SQL 経由で直接誘発することはできない。
//! 単体テストで検証済みの範囲として扱う）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;
use harness::hybrid_latency::{generate_corpus, generate_query};

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn hybrid_schema(dim: u32) -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn seed_corpus(storage: &Storage, tenant: &str, corpus: &harness::hybrid_latency::Corpus) {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    let dim = corpus.dim as usize;
    for (i, id) in corpus.ids.iter().enumerate() {
        let v = corpus.vectors[i * dim..(i + 1) * dim].to_vec();
        let body = corpus.texts[i].clone();
        let op_id =
            OperationId::parse(&format!("hnsw-hybrid-refetch-{tenant}-{id}")).expect("op id");
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(v), Value::Text(body)],
            &op_id,
        )
        .unwrap_or_else(|e| panic!("insert corpus row id={id} failed: {e}"));
    }
}

/// 同点誘発コーパス（`quantize_levels: Some(n)`。`benches/harness/hybrid_latency.rs`
/// モジュールドキュメント参照）を HNSW opt-in エンジンへ投入した hybrid SQL が、
/// (a) エラーなく完了し `hybrid_rounds_max` が本計画の理論上限
/// （`⌈log2(dense_cap / (2·pool_depth))⌉ + 1`。既定 `pool_depth=200` なら小規模
/// コーパスで高々 8）以内に収まること、(b) 同一クエリを複数回実行して結果が
/// 完全一致（決定性）することを固定する。
#[test]
fn tie_inducing_corpus_hybrid_search_terminates_and_is_deterministic() {
    const DIM: usize = 16;
    const NUM_DOCS: usize = 4_000;
    const VOCAB: usize = 64;
    const QUANTIZE_LEVELS: usize = 2;

    let corpus = generate_corpus(1, NUM_DOCS, VOCAB, DIM, Some(QUANTIZE_LEVELS))
        .expect("tie-inducing corpus");
    let query = generate_query(1, DIM, VOCAB);

    let dir = unique_db_path("hnsw-hybrid-refetch-tie");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage
        .create_table(&hybrid_schema(DIM as u32))
        .expect("create table");
    seed_corpus(&storage, "tenant-a", &corpus);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let sql = format!(
        "SELECT id FROM docs ORDER BY HYBRID(embedding, '{}', body, '{}') LIMIT 10",
        vec_literal(&query.vector),
        query.text
    );

    let first = core.execute_sql(&ctx, &sql).expect("hybrid query (1st)");
    for attempt in 2..=3 {
        let repeated = core
            .execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| panic!("hybrid query (run {attempt}) failed: {e}"));
        assert_eq!(
            repeated.rows, first.rows,
            "repeated hybrid queries against a tie-inducing corpus must be bit-identical (run {attempt})"
        );
    }

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.hybrid_dense_searches > 0,
        "tie-inducing hybrid query must exercise HnswDenseProvider (non-vacuous)"
    );
    // `QUANTIZE_LEVELS=2` は文書のおよそ半数が同一プロトタイプベクトルを共有する
    // 極端な同点誘発コーパスであり、初期 `fetch_k`（`pool_depth * 2` = 400）
    // では境界同点グループの終端を確定できず複数ラウンドの再取得が必ず発生する
    // （実測 `hybrid_rounds_max = 4`）。ここで `>= 2` を固定することで、
    // `HnswDenseProvider` が単発ラウンドだけでなく `prepared` を再利用した
    // 複数ラウンドの探索（Issue #410 の核心）を実際に SQL 表層経由で通ることを
    // 検証する（単体テストでは手動で k=10/20/40 を与えて確認済みだが、SQL 表層
    // 経由での複数ラウンド発生はこのテストでのみ確認できる）。
    assert!(
        stats.hybrid_rounds_max >= 2,
        "tie-inducing corpus must force multiple dense refetch rounds (got {})",
        stats.hybrid_rounds_max
    );
    // 理論上限（実装計画「(4) hybrid 密側の ANN 化」節）: `dense_cap =
    // MAX_FETCH_K.min(visible_rows)`・初期 `fetch_k = pool_depth * 2`（既定
    // `pool_depth=200` なら 400）からの倍増ラウンド数は
    // `⌈log2(dense_cap / 400)⌉ + 1` 以下。`NUM_DOCS=4,000` 規模では 4 を大きく
    // 下回るはずだが、実装詳細に強く結合しないよう緩めの上限 8 で固定する
    // （`hybrid_latency_bench.rs`・`docs/design/hybrid-refetch-latency.md` の
    // 既存ベンチと同じ規範値）。
    assert!(
        stats.hybrid_rounds_max <= 8,
        "hybrid dense refetch loop must terminate within a bounded number of rounds (got {})",
        stats.hybrid_rounds_max
    );
}
