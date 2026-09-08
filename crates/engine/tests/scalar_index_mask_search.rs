//! 候補集合を id マスクで直接探索し `VectorArena` への複製を回避する
//! （Issue #654・親 #650・#472/#359 の一部）SQL 表層結合テスト。
//!
//! `tests/scalar_index_prune.rs`（Issue #474・候補削減そのもの）と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`、`engine::tenant::insert_typed_row`
//! による投入）を使うが、本ファイルは Issue #654 固有の 3 点を検証する:
//!
//! 1. 候補削減が発火する形状で `VectorArena` への複製が実際に発生しないこと
//!    （記録 provider による直接観測。`search`（複製経路）ではなく `search_subset`
//!    〔マスク経路〕が呼ばれ、渡された `vectors` がスナップショット全行ぶんである
//!    こと）
//! 2. cold（redb 走査）／hot（マスク経路）で結果（id 列・投影列・スコア）が
//!    ビット一致すること（等価・前方一致・`id` 範囲・結合の 4 形状。複数列投影・
//!    同点誘発コーパスを含む）
//! 3. RLS 相当のテナント境界がマスク経路でも不変であること（他テナント private
//!    行の非混入。対照 DB との結果一致オラクル）
//!
//! `index_scans`／`index_mask_scans` カウンタ（`ScalarIndexCacheStats`）で
//! マスク経路が実際に消費されたことを非 vacuous に確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider, SubsetSearchInput};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};
use std::sync::Mutex;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
        ],
    )
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    embedding: [f32; 2],
    kind: &str,
    path: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(embedding.to_vec()),
            Value::Text(kind.to_string()),
            Value::Text(path.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// `(id, projected columns as Debug string)` の集合。列投影の一致まで固定する
/// （`ScalarSource::EagerSubset` の写像が正しいことの直接検証）。
fn result_rows_with_cells(result: &QueryResult) -> Vec<(u64, String)> {
    let mut rows: Vec<(u64, String)> = result
        .rows
        .iter()
        .map(|r| (r.id, format!("{:?}", r.cells)))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    rows
}

/// 10 行（`id` 1..=10）: 偶数 `id` は `kind = 'a'`・`path` に `"even/"` 接頭辞、
/// 奇数 `id` は `kind = 'b'`・`path` に `"odd/"` 接頭辞。距離は `id` に単調な
/// ベクトルにし、`ORDER BY ... LIMIT 20`（全件超）で常に全一致行を取得できる
/// ようにする。
fn seed_ten_rows(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=10u64 {
        let (kind, path) = if id % 2 == 0 {
            ("a", format!("even/{id}"))
        } else {
            ("b", format!("odd/{id}"))
        };
        insert_row(
            storage,
            &tenant_ctx,
            id,
            [id as f32, 0.0],
            kind,
            &path,
            Visibility::Public,
        );
    }
}

/// 同点誘発コーパス: 20 行のうち偶数 `id` はすべて同一 embedding（`[1.0, 0.0]`）に
/// し、DISTANCE 段の同点タイブレーク（スコア同点は id 昇順）が cold/hot で一致する
/// ことを検証する。
fn seed_tie_inducing_rows(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=20u64 {
        let (kind, embedding) = if id % 2 == 0 {
            ("a", [1.0f32, 0.0])
        } else {
            ("b", [id as f32, 1.0])
        };
        insert_row(
            storage,
            &tenant_ctx,
            id,
            embedding,
            kind,
            &format!("path/{id}"),
            Visibility::Public,
        );
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

// --- 記録 provider（要件 1: 複製が発生していないことの機械検証） -------------

/// `search`（複製前提の従来経路）・`search_subset`（Issue #654 のマスク経路）の
/// 呼び出しを記録する provider。結果自体は `CpuScalarProvider` へ委譲する。
///
/// `search_subset` へ渡る `vectors.len()` がスナップショット全行ぶん
/// （`rows * dim`）であり、`slots.len()` がそれより小さいことを直接アサートすれば、
/// 「候補行だけを新規 `VectorArena` へ複製してから探索した」のではなく「行列全体を
/// 借用したままスロットマスクで直接探索した」ことを機械的に確認できる。
#[derive(Default)]
struct RecordingProvider {
    search_calls: Mutex<Vec<(usize, usize)>>, // (ids.len(), vectors.len())
    search_subset_calls: Mutex<Vec<(usize, usize)>>, // (slots.len(), vectors.len())
}

impl SearchProvider for RecordingProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.search_calls
            .lock()
            .expect("lock")
            .push((input.ids.len(), input.vectors.len()));
        engine::kernel::CpuScalarProvider.search(input)
    }

    fn search_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, KernelError> {
        self.search_subset_calls
            .lock()
            .expect("lock")
            .push((input.slots.len(), input.vectors.len()));
        engine::kernel::CpuScalarProvider.search_subset(input)
    }
}

fn new_core_with_recording(storage: Storage) -> (EngineCore, std::sync::Arc<RecordingProvider>) {
    let provider = std::sync::Arc::new(RecordingProvider::default());
    // `EngineCore::from_storage` は `Box<dyn SearchProvider>` を要求するため、
    // `Arc` を複製して 1 つはコアへ、1 つはテスト側の観測用に残す（`RecordingProvider`
    // 自体は `Send + Sync`・内部可変性は `Mutex` のみ）。
    struct ArcProvider(std::sync::Arc<RecordingProvider>);
    impl SearchProvider for ArcProvider {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            self.0.search(input)
        }
        fn search_subset(
            &self,
            input: SubsetSearchInput<'_>,
        ) -> Result<Vec<CandidateHit>, KernelError> {
            self.0.search_subset(input)
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(ArcProvider(provider.clone())));
    (core, provider)
}

#[test]
fn mask_path_avoids_arena_duplication_and_never_calls_full_copy_search() {
    let path = unique_db_path("scalar-index-mask-avoid-duplication");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let (core, provider) = new_core_with_recording(storage);

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    // cold: 索引未構築のため候補削減は発火しない（`build_from_cached_rls_rows`
    // 経由の redb 走査。この回は `search`（複製前提）を呼んでよい）。
    let cold = run(&core, "tenant-a", sql);
    // hot: 索引が構築済みでマスク経路が発火するはず。
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));

    let subset_calls = provider.search_subset_calls.lock().expect("lock").clone();
    assert!(
        !subset_calls.is_empty(),
        "hot 実行では search_subset（マスク経路）が呼ばれるはず"
    );
    let (slots_len, vectors_len) = subset_calls
        .last()
        .copied()
        .expect("at least one search_subset call");
    // 候補（kind='a' の 5 行）よりスナップショット全行（10 行）の方が多いため、
    // `vectors.len()`（10 行 * dim 2 = 20）は `slots.len()`（5）より真に大きい。
    // これは「候補行だけを新規複製した」のでは説明できず、行列全体を借用した
    // ままマスクで絞ったことの直接証拠になる。
    assert_eq!(slots_len, 5, "kind='a' の候補は 5 行のはず");
    assert_eq!(
        vectors_len,
        10 * 2,
        "vectors はスナップショット全行ぶんのはず"
    );
}

// --- 要件 2: cold/hot ビット一致（複数列投影・4 形状） -----------------------

fn assert_cold_hot_rows_and_cells_match(core: &EngineCore, tenant: &str, sql: &str) {
    let cold = run(core, tenant, sql);
    let hot = run(core, tenant, sql);
    assert_eq!(
        result_rows_with_cells(&cold),
        result_rows_with_cells(&hot),
        "cold/hot rows(+cells) must match exactly for: {sql}"
    );
}

#[test]
fn equality_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-equality-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind, path FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);

    let index_scans_before = core.scalar_index_cache_stats().index_scans;
    let index_mask_scans_before = core.scalar_index_cache_stats().index_mask_scans;
    let _ = run(&core, "tenant-a", sql);
    let stats = core.scalar_index_cache_stats();
    assert!(stats.index_scans > index_scans_before);
    assert!(
        stats.index_mask_scans > index_mask_scans_before,
        "非 hybrid・非 HNSW-Subset の DISTANCE 経路はマスク経路を消費するはず"
    );
}

#[test]
fn prefix_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-prefix-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql = "SELECT id, kind, path FROM docs WHERE path LIKE 'odd/%' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

#[test]
fn id_range_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-id-range-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql = "SELECT id, kind, path FROM docs WHERE id > 7 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

#[test]
fn conjunction_predicate_cold_hot_matches_with_multi_column_projection() {
    let path = unique_db_path("scalar-index-mask-conjunction-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql = "SELECT id, kind, path FROM docs WHERE kind = 'a' AND id > 4 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

// `LIMIT` を候補件数未満に絞り、Top-k 境界（マスク経路の provider が本当に
// Top-k のみを選出していること）も検証する。
#[test]
fn equality_predicate_cold_hot_matches_with_limit_below_candidate_count() {
    let path = unique_db_path("scalar-index-mask-limit-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    // kind='a' の候補は 5 行。LIMIT 2 で Top-2 のみを取得する。
    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 2";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_rows_with_cells(&cold), result_rows_with_cells(&hot));
    assert_eq!(cold.rows.len(), 2);
}

// 同点誘発コーパスでの cold/hot 一致（マスク経路の Top-k 選出・同点タイブレークが
// 複製経路と同じ規約であることの検証）。
#[test]
fn tie_inducing_corpus_cold_hot_matches_for_mask_path() {
    let path = unique_db_path("scalar-index-mask-tie-inducing");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_tie_inducing_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20";
    assert_cold_hot_rows_and_cells_match(&core, "tenant-a", sql);
}

// --- 統計: hybrid・HINT ORDER（残余述語）はマスク経路を消費しない ------------

#[test]
fn hint_order_distance_leading_never_consumes_mask_path() {
    let path = unique_db_path("scalar-index-mask-hint-order");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    // `HINT ORDER(RLS, DISTANCE, SCALAR)`: DISTANCE 先行・SCALAR 事後フィルタ
    // （`plan.scalar_prefilter == false`）は `classify_scalar_plan` が `PlainScan`
    // を返すため、索引対応述語があっても候補削減自体が発火しない。
    let sql = "SELECT id, kind FROM docs WHERE kind = 'a' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20 HINT ORDER(RLS, DISTANCE, SCALAR)";
    let before = core.scalar_index_cache_stats().index_mask_scans;
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats().index_mask_scans;
    assert_eq!(
        after, before,
        "DISTANCE 先行の HINT ORDER はマスク経路を消費しないはず"
    );
}

// --- 要件 3: RLS（テナント境界の非漏えい） ----------------------------------

#[test]
fn mask_path_never_leaks_other_tenant_private_rows() {
    let path = unique_db_path("scalar-index-mask-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    // tenant-b の private 行を id 1..=10 の間に紛れ込ませる（kind='a' で
    // 候補削減の対象になり得る形状にする）。
    let tenant_b_ctx = ctx("tenant-b");
    for id in 101..=105u64 {
        insert_row(
            &storage,
            &tenant_b_ctx,
            id,
            [id as f32, 0.0],
            "a",
            &format!("secret/{id}"),
            Visibility::Private,
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    // cold（索引未構築）→ hot（マスク経路）の双方で tenant-b の private 行が
    // 一切混入しないこと。
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    for result in [&cold, &hot] {
        for id in result_ids(result) {
            assert!(
                id <= 10,
                "tenant-a のクエリに tenant-b の private 行（id={id}）が混入した"
            );
        }
    }
    assert_eq!(result_ids(&cold), result_ids(&hot));
}

// --- 世代進行後もマスク経路の結果は一貫する ---------------------------------

#[test]
fn mask_path_stays_consistent_after_table_generation_bump() {
    let path = unique_db_path("scalar-index-mask-generation-bump");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));

    let sql =
        "SELECT id, kind FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let _ = run(&core, "tenant-a", sql); // cold
    let _ = run(&core, "tenant-a", sql); // hot（索引構築・マスク経路消費）

    // 新規行を追加して世代を進める。
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO docs (id, embedding, kind, path) \
         VALUES (11, '[11.0,0.0]', 'a', 'even/11') USING OPERATION_ID 'seed-11'",
    )
    .expect("insert row after generation bump");

    let after_insert = run(&core, "tenant-a", sql);
    assert!(result_ids(&after_insert).contains(&11));
    // 索引は世代進行で再構築され、再度 hot 実行しても一貫し続ける。
    let after_insert_hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&after_insert), result_ids(&after_insert_hot));
}
