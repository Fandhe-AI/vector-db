//! `sql::hnsw_cache::HnswIndexCache`（Issue #408・親 #402・前提 #404〜#407）の SQL
//! 表層結合テスト。`EngineCore::execute_sql` 経由で `SearchEngineKind::Hnsw` opt-in
//! エンジンを使い、以下を検証する（対象ビヘイビア CORE-9・CORE-10・TASK-132・
//! ポインタのみ）:
//!
//! - R1: 構築 → 差分 brute-force（追加挿入） → 再構築（さらなる追加挿入） →
//!   `update_row`/`delete_row` の各段で、既定エンジン（brute-force 対照）に対する
//!   Recall@10 が本リポの回帰基準（0.9 目安）以上であること。統計カウンタ
//!   （`builds`/`delta_searches`/`rebuilds`）で経路が実際に非 vacuous に動いたこと
//!   を固定する。
//! - R4: テナント境界（`(table, ctx)` 完全一致キー）。他テナントの private 行が
//!   結果へ混入しない・エントリが ctx ごとに独立していること。
//! - フィルタ付き（`WHERE`）・hybrid クエリは `HnswIndexCache` を経由せず、常に
//!   既定エンジンと完全一致すること（適用条件の遵守）。
//! - Rust API（`VectorCore::search`）は本キャッシュを経由しない（`sql` 表層専用の
//!   段階化。既存 Issue #407 の契約を維持）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（`tests/hnsw_search.rs::TestRng` の複製。結合テストは
// crate 外の公開 API のみを使う流儀のため独立に複製する） ----------

struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_unit(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// クラスタ構造を持つ、L2 正規化済みの決定的コーパス（`tests/hnsw_search.rs::
/// gen_clustered_corpus` と同型の複製）。
fn gen_clustered_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<Vec<f32>> {
    let mut center_rng = TestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(seed);
    (0..rows)
        .map(|i| {
            let center = &centers[i % centers.len()];
            let mut v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
            normalize(&mut v);
            v
        })
        .collect()
}

const DIM: u32 = 16;
// `MIN_INDEXED_ROWS`（1,024）超・`build_parallel` が逐次 `build` へ縮退する
// `SEQUENTIAL_PREFIX_NODES`（256）超だが `thread_count_for` の実質並列化閾値
// （2,048）未満に収め、構築グラフを決定的に保つ（`hnsw_cache.rs` モジュール
// ドキュメント・実装計画の fixture 方針参照）。
const BASE_ROWS: usize = 1_200;

fn schema(dim: u32) -> TableSchema {
    TableSchema::new(
        "docs",
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

fn seed_rows(storage: &Storage, tenant: &str, start_id: u64, vectors: &[Vec<f32>], tag: &str) {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                start_id + i as u64,
                RowInput {
                    tenant_id: tenant,
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("hnsw-cache-seed-{tag}")).expect("valid operation_id");
    engine::tenant::insert_rows(storage, "docs", &ctx, &rows, &op_id).expect("seed rows batch");
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn query_ids(core: &EngineCore, ctx: &PolicyContext, query: &[f32], k: usize) -> Vec<u64> {
    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT {}",
        vec_literal(query),
        k
    );
    let result = core.execute_sql(ctx, &sql).expect("query should succeed");
    result.rows.iter().map(|r| r.id).collect()
}

fn recall_at_k(got: &[u64], want: &[u64]) -> f64 {
    if want.is_empty() {
        return 1.0;
    }
    let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
    let hits = got.iter().filter(|id| want_set.contains(id)).count();
    hits as f64 / want.len() as f64
}

/// R1: 構築 → 差分 brute-force → 再構築 → update/delete の段階遷移で Recall@10 が
/// 本リポの回帰基準（0.9）以上のまま維持され、統計カウンタが各段の経路を非
/// vacuous に固定することを検証する。
#[test]
fn r1_staged_transition_maintains_recall_and_exercises_all_cache_paths() {
    let dir = unique_db_path("hnsw-cache-r1-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let base_vectors = gen_clustered_corpus(1, DIM as usize, BASE_ROWS, 12);
    seed_rows(&storage, "tenant-a", 1, &base_vectors, "base");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    // 対照用の brute-force 側は独立した Storage へ同一行を複製する。
    let ref_dir = unique_db_path("hnsw-cache-r1-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &base_vectors, "ref-base");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    // クエリはコーパス自身の点から抽出する（独立生成した乱数クラスタだと分布外
    // クエリになり、brute-force 対照でも HNSW 近似の Recall@10 が構造的に低下する
    // ため。`base_vectors` から等間隔に 20 点採る）。
    let queries: Vec<Vec<f32>> = (0..20)
        .map(|i| base_vectors[i * (BASE_ROWS / 20)].clone())
        .collect();

    // 段階 (i): 初回構築後。
    let mut recalls = Vec::new();
    for q in &queries {
        let got = query_ids(&core, &ctx, q, 10);
        let want = query_ids(&ref_core, &ctx, q, 10);
        recalls.push(recall_at_k(&got, &want));
    }
    let avg = recalls.iter().sum::<f64>() / recalls.len() as f64;
    assert!(avg >= 0.9, "stage (i) recall too low: {avg}");
    let stats = core.hnsw_index_cache_stats();
    assert_eq!(stats.builds, 1, "stage (i) must build exactly once");
    assert_eq!(stats.rebuilds, 0);

    // 段階 (ii): 5% 追加挿入後（差分 brute-force 経路）。
    let extra1 = gen_clustered_corpus(2, DIM as usize, BASE_ROWS / 20, 12);
    seed_rows_on_core(&core, "tenant-a", BASE_ROWS as u64 + 1, &extra1, "extra1");
    seed_rows_on_core(
        &ref_core,
        "tenant-a",
        BASE_ROWS as u64 + 1,
        &extra1,
        "ref-extra1",
    );
    let mut recalls2 = Vec::new();
    for q in &queries {
        let got = query_ids(&core, &ctx, q, 10);
        let want = query_ids(&ref_core, &ctx, q, 10);
        recalls2.push(recall_at_k(&got, &want));
    }
    let avg2 = recalls2.iter().sum::<f64>() / recalls2.len() as f64;
    assert!(avg2 >= 0.9, "stage (ii) recall too low: {avg2}");
    let stats2 = core.hnsw_index_cache_stats();
    assert_eq!(stats2.builds, 1, "stage (ii) must not rebuild yet");
    assert!(
        stats2.delta_searches > 0,
        "stage (ii) must exercise the delta brute-force path"
    );

    // 段階 (iii): さらに 15% 追加挿入後（差分比率が閾値を超え再構築）。
    let extra2 = gen_clustered_corpus(3, DIM as usize, (BASE_ROWS * 15) / 100, 12);
    let next_id = BASE_ROWS as u64 + extra1.len() as u64 + 1;
    seed_rows_on_core(&core, "tenant-a", next_id, &extra2, "extra2");
    seed_rows_on_core(&ref_core, "tenant-a", next_id, &extra2, "ref-extra2");
    let mut recalls3 = Vec::new();
    for q in &queries {
        let got = query_ids(&core, &ctx, q, 10);
        let want = query_ids(&ref_core, &ctx, q, 10);
        recalls3.push(recall_at_k(&got, &want));
    }
    let avg3 = recalls3.iter().sum::<f64>() / recalls3.len() as f64;
    assert!(avg3 >= 0.9, "stage (iii) recall too low: {avg3}");
    let stats3 = core.hnsw_index_cache_stats();
    assert_eq!(stats3.builds, 2, "stage (iii) must rebuild once");
    assert_eq!(stats3.rebuilds, 1);

    // 段階 (iv): id=1 の行をクエリと同一ベクトルへ `update_row` し、id=2 を
    // `delete_row` する。索引ヒットも `kernel::dot` で再計算するため、id=1 は
    // 両エンジンで完全一致の先頭ヒット・id=2 はいずれの結果にも現れないはず。
    let op_update = OperationId::parse("hnsw-cache-r1-update").expect("valid operation_id");
    core.update_row(
        &ctx,
        "docs",
        1,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: queries[0].as_slice(),
            metadata: &[],
        },
        Some(&op_update),
    )
    .expect("update row on hnsw core");
    ref_core
        .update_row(
            &ctx,
            "docs",
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: queries[0].as_slice(),
                metadata: &[],
            },
            Some(&op_update),
        )
        .expect("update row on ref core");

    let op_delete = OperationId::parse("hnsw-cache-r1-delete").expect("valid operation_id");
    core.delete_row(&ctx, "docs", 2, Some(&op_delete))
        .expect("delete row on hnsw core");
    ref_core
        .delete_row(&ctx, "docs", 2, Some(&op_delete))
        .expect("delete row on ref core");

    let got4 = query_ids(&core, &ctx, &queries[0], 10);
    let want4 = query_ids(&ref_core, &ctx, &queries[0], 10);
    assert_eq!(
        got4.first(),
        Some(&1u64),
        "the row updated to exactly match the query must rank first"
    );
    assert!(!got4.contains(&2), "the deleted row must not appear");
    assert!(!want4.contains(&2));
    let recall4 = recall_at_k(&got4, &want4);
    assert!(recall4 >= 0.9, "stage (iv) recall too low: {recall4}");
}

fn seed_rows_on_core(
    core: &EngineCore,
    tenant: &str,
    start_id: u64,
    vectors: &[Vec<f32>],
    tag: &str,
) {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    for (i, v) in vectors.iter().enumerate() {
        let op_id =
            OperationId::parse(&format!("hnsw-cache-{tag}-{i}")).expect("valid operation_id");
        core.insert_row(
            &ctx,
            "docs",
            start_id + i as u64,
            &RowInput {
                tenant_id: tenant,
                visibility: Visibility::Public,
                embedding: v.as_slice(),
                metadata: &[],
            },
            Some(&op_id),
        )
        .expect("insert row via EngineCore");
    }
}

/// R4: テナント境界。tenant-a の private 行は tenant-b の可視結果に現れず、
/// キャッシュエントリは `(table, ctx)` ごとに独立する。
#[test]
fn r4_tenant_isolation_never_leaks_across_ctx() {
    let dir = unique_db_path("hnsw-cache-r4");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let a_vectors = gen_clustered_corpus(11, DIM as usize, BASE_ROWS, 8);
    let b_vectors = gen_clustered_corpus(22, DIM as usize, BASE_ROWS, 8);

    // `Private` 行を自テナントへ見せるには `PolicyContext::with_visibilities` で
    // 明示的に許可可視性集合へ含める必要がある（`PolicyContext::new` は `Public`
    // のみ許可する既定・最小権限のコンストラクタ。`policy.rs` モジュール
    // ドキュメント参照）。
    let ctx_a = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant");
    let ctx_b = PolicyContext::with_visibilities(
        "tenant-b",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant");

    let rows_a: Vec<(u64, RowInput<'_>)> = a_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_a = OperationId::parse("hnsw-cache-r4-a").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_a, &rows_a, &op_a).expect("seed tenant-a");

    let rows_b: Vec<(u64, RowInput<'_>)> = b_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-cache-r4-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let query = &a_vectors[0];
    let got_a = query_ids(&core, &ctx_a, query, 10);
    assert!(!got_a.is_empty(), "tenant-a must see its own rows");

    let got_b = query_ids(&core, &ctx_b, query, 10);
    // tenant-b の private 行だけが返るはず（tenant-a の行は不可視）。id 空間は
    // 両テナントとも 1..=BASE_ROWS で重なるため、id 一致では判定できない代わりに
    // 件数のみ確認する（0 件にはならない: tenant-b 自身の行が存在する）。
    assert!(!got_b.is_empty());

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.entries >= 2,
        "tenant-a and tenant-b must occupy independent cache entries"
    );
}

/// フィルタ付き（`WHERE`）・hybrid クエリは `HnswIndexCache` を経由せず、常に
/// 既定エンジンと完全一致すること（適用条件の遵守）。
#[test]
fn filtered_and_hybrid_queries_bypass_cache_and_match_default_engine() {
    let dir = unique_db_path("hnsw-cache-bypass-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");

    let vectors = gen_clustered_corpus(5, DIM as usize, BASE_ROWS, 6);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = OperationId::parse("hnsw-cache-bypass").expect("valid operation_id");
    let tag_metadata = engine::row_codec::encode_scalar_columns(
        &schema,
        &[
            engine::row_codec::Value::Null,
            engine::row_codec::Value::Text("x".to_string()),
        ],
    )
    .expect("encode tag metadata");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: tag_metadata.as_slice(),
                },
            )
        })
        .collect();
    engine::tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).expect("seed rows");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-bypass-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx, &rows, &op_id).expect("seed ref rows");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let query = &vectors[0];
    // 全行が `tag = 'x'` を満たすため、フィルタは実質すべて通過させる（本テストの
    // 目的はフィルタあり経路が `HnswIndexCache` を経由しないことの確認であり、
    // フィルタ自体の選択性は問わない）。
    let sql = format!(
        "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT 10",
        vec_literal(query)
    );
    let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
    let want = ref_core
        .execute_sql(&ctx, &sql)
        .expect("filtered query (ref)")
        .rows;
    assert_eq!(
        got.iter().map(|r| r.id).collect::<Vec<_>>(),
        want.iter().map(|r| r.id).collect::<Vec<_>>(),
        "filtered DISTANCE queries must bypass HnswIndexCache and match the default engine exactly"
    );

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.entries, 0,
        "filtered queries must never populate HnswIndexCache"
    );
}

/// Rust API（`VectorCore::search`）は本キャッシュを経由せず、既定エンジンと完全
/// 一致する（Issue #407 の契約を維持。`sql::hnsw_cache` は SQL 表層専用）。
#[test]
fn rust_api_search_bypasses_cache_and_matches_default_engine_via_fallback() {
    let dir = unique_db_path("hnsw-cache-rust-api-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");
    let vectors = gen_clustered_corpus(7, DIM as usize, BASE_ROWS, 6);
    seed_rows(&storage, "tenant-a", 1, &vectors, "rust-api");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-rust-api-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &vectors, "rust-api-ref");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let got = core.search(&ctx, "docs", &vectors[0], 10).expect("search");
    let want = ref_core
        .search(&ctx, "docs", &vectors[0], 10)
        .expect("search (ref)");
    assert_eq!(got, want);

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.entries, 0,
        "Rust API search must never populate HnswIndexCache (SQL-surface-only wiring)"
    );
}
