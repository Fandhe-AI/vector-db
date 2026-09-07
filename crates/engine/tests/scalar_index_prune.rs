//! `sql::scalar_index::ScalarIndex` を SCALAR 事前フィルタの候補削減へ結線する
//! （Issue #474・前提 Issue #473）SQL 表層結合テスト。
//!
//! `tests/scalar_index_cache.rs`（Issue #473・構築とキャッシュのみ）と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `engine::tenant::insert_typed_row` による投入）で、`EngineCore::execute_sql`
//! （`sql::exec::execute_statement_with_cache` の候補削減結線を必ず経由する
//! production 経路）と `EngineCore::scalar_index_cache_stats()`（テナント ID・
//! 値を含まないカウンタのみの観測用 API）を突き合わせる。
//!
//! 検証する契約（Issue #474 のスコープ）:
//! 1. 索引対応述語（等価・前方一致・`id` 単純比較・その組合せ）を持つクエリで、
//!    初回（cold・全走査のみ。索引はこの回で構築される）と 2 回目以降
//!    （hot・索引を候補削減に消費）の結果が完全一致し、2 回目以降は
//!    `index_scans` が増える
//! 2. 索引非対応の残余述語（`Builtin`／`HINT ORDER` による DISTANCE 先行）は
//!    索引を一切消費せず（`index_scans` 不変）、結果は全走査と一致し続ける
//! 3. 選択度が閾値を超える述語では索引経路を使わず（`plain_scan_fallbacks` が
//!    増える）、結果は全走査と一致する
//! 4. 評価エラー（0 除算等）を起こす式述語を含むクエリは、索引対応述語を含んで
//!    いても従来どおり `22000` で拒否する（残余述語ありなので索引不使用）
//! 5. RLS: 他テナントの private 行は索引経路でも一切露出しない
//! 6. テーブル世代の進行後、索引は再構築され結果は一貫し続ける

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

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

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
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

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

/// クエリを 2 回実行し（cold: 索引構築のみ・hot: 索引消費）、結果が完全一致
/// することを固定したうえで、hot 実行後の `index_scans` 増分を返す。
fn assert_cold_hot_equivalent(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, tenant, sql);
    let hot = run(core, tenant, sql);
    assert_eq!(
        result_ids(&cold),
        result_ids(&hot),
        "cold/hot results must match exactly for: {sql}"
    );
    let after = core.scalar_index_cache_stats().index_scans;
    after.saturating_sub(before)
}

// --- 契約 1: 索引対応述語の cold/hot 等価性 ---------------------------------

#[test]
fn equality_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-prune-equality");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "an equality predicate must be consumed by the index on the hot path"
    );
    // オラクル: 偶数 id のみ。
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![2, 4, 6, 8, 10]);
}

#[test]
fn prefix_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-prune-prefix");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql =
        "SELECT id FROM docs WHERE path LIKE 'odd/%' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a prefix predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![1, 3, 5, 7, 9]);
}

#[test]
fn id_range_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-prune-id-range");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `WHERE kind = 'a'` を含めるとメタデータ索引経路に固定されてしまうため、
    // `id` の単純比較のみを索引対応述語にする（`kind` を含まない裸の `WHERE`）。
    // 一部の行のみ選ぶことで既定の選択度閾値（1/2）を下回らせる（10 行中 3 行）。
    let sql = "SELECT id FROM docs WHERE id > 7 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "an id range predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![8, 9, 10]);

    // 左右入替・`=`／`<`／`>=`／`<=` も同じオラクルに一致する（`sql::scalar_plan::
    // id_predicate_from_expr` の左右入替・`id_bounds` の演算子網羅は単体テストで
    // 別途固定済みのため、ここでは SQL 表層からの往復を代表 1 パターンずつ確認する）。
    for (sql, expected) in [
        (
            "SELECT id FROM docs WHERE 7 < id ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
            vec![8u64, 9, 10],
        ),
        (
            "SELECT id FROM docs WHERE id = 5 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
            vec![5],
        ),
        (
            "SELECT id FROM docs WHERE id <= 3 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
            vec![1, 2, 3],
        ),
    ] {
        let result = run(&core, "tenant-a", sql);
        assert_eq!(result_ids(&result), expected, "sql={sql}");
    }
}

#[test]
fn conjunction_of_metadata_and_id_predicates_cold_hot_equivalence() {
    let path = unique_db_path("scalar-index-prune-conjunction");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql =
        "SELECT id FROM docs WHERE kind = 'a' AND id > 5 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a conjunction of index-eligible predicates must be consumed by the index"
    );
    // 偶数 id かつ id > 5。
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![6, 8, 10]);
}

// --- 契約 2: 索引非対応形状は索引を消費しない -------------------------------

#[test]
fn residual_builtin_expr_never_consumes_index() {
    let path = unique_db_path("scalar-index-prune-residual-builtin");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `vec_norm(embedding)` は `VectorRef` を参照する残余述語のため、
    // `kind = 'a'`（索引対応）が同居していても全体が `PlainScan` へ縮退する。
    let sql = "SELECT id FROM docs WHERE kind = 'a' AND vec_norm(embedding) > 0 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(
        before, after,
        "a residual VectorRef predicate must never be consumed by the index"
    );
    assert_eq!(result_ids(&hot), vec![2, 4, 6, 8, 10]);
}

#[test]
fn distance_leading_hint_order_never_consumes_index() {
    let path = unique_db_path("scalar-index-prune-hint-order");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `HINT ORDER(RLS, DISTANCE, SCALAR)`: SCALAR 段は DISTANCE 段の後で事後
    // 適用される（`scalar_prefilter == false`）ため索引経路の対象外。
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20 \
               HINT ORDER(RLS, DISTANCE, SCALAR)";
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(
        before, after,
        "DISTANCE-leading HINT ORDER must never consume the index"
    );
}

// --- 契約 3: 選択度切替 -----------------------------------------------------

#[test]
fn low_selectivity_predicate_falls_back_to_plain_scan_but_result_still_matches() {
    let path = unique_db_path("scalar-index-prune-selectivity");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `kind = 'a'` は 10 行中 5 行一致（比 1/2）。既定の選択度閾値
    // （`hits * 2 > row_count * 1`、すなわち一致比が厳密に 1/2 を超えると縮退）
    // をちょうど満たさないため索引経路が使われる契約自体は上の equality テストで
    // 固定済み。ここでは半数を超える一致（6/10）を作り、確実に縮退させる
    // （SQL 表層 `INSERT` 経由。書き込む行の可視性は常に `Private` 固定〔`sql::exec`
    // モジュールドキュメント参照〕だが `ctx("tenant-a")` は `Private` も許可する）。
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO docs (id, embedding, kind, path) \
         VALUES (11, '[11.0,0.0]', 'a', 'even/11') USING OPERATION_ID 'seed-11'",
    )
    .expect("insert extra matching row");
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[20.0,0.0]' LIMIT 20";
    let before = core.scalar_index_cache_stats();
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats();
    assert!(
        after.plain_scan_fallbacks > before.plain_scan_fallbacks,
        "a predicate exceeding the selectivity threshold must fall back to a plain scan \
         (before={before:?}, after={after:?})"
    );
    // 選択度超過による縮退は「候補削減を使わなかった」ことそのものを意味する
    // ため、`index_scans` は増えていないことも合わせて固定する（`plain_scan_
    // fallbacks` の増加だけでは「索引経路を経由したが最終的に絞れなかった」
    // 可能性を排除できない）。
    assert_eq!(
        after.index_scans, before.index_scans,
        "a selectivity fallback must not also be counted as an index scan"
    );
    assert_eq!(result_ids(&hot), vec![2, 4, 6, 8, 10, 11]);
}

// --- 契約 4: エラー契約（残余述語ありは索引不使用のまま従来どおり fail-closed） --

#[test]
fn division_by_zero_in_residual_expr_still_fails_closed_with_22000() {
    let path = unique_db_path("scalar-index-prune-error-contract");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // 可視行はすべて `kind` が `'a'` か `'b'` のいずれかであり、`kind = 'a'` に
    // 一致する行（偶数 id）が必ず存在する。`matches_all` が先に評価され一致
    // する行にのみ式述語が届くため、0 除算が実際に評価されることを保証する
    // （`on_visible_row` は `matches_all` を式述語より先に評価する既存契約）。
    let sql = "SELECT id FROM docs WHERE kind = 'a' AND (1.0 / 0.0) > 0 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let err = core
        .execute_sql(&ctx("tenant-a"), sql)
        .expect_err("division by zero must still be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- 契約 5: RLS ------------------------------------------------------------

#[test]
fn other_tenant_private_rows_never_leak_through_index_path() {
    let path = unique_db_path("scalar-index-prune-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    // 別テナントの private 行（同じ `kind = 'a'`。tenant-a からは絶対に見えない
    // はずの id）。
    insert_row(
        &storage,
        &ctx("tenant-b"),
        999,
        [1.0, 0.0],
        "a",
        "even/999",
        Visibility::Private,
    );
    let core = new_core(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    // cold（索引構築）→ hot（索引消費）の両方で他テナント行が漏れないことを固定。
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert!(!result_ids(&cold).contains(&999));
    assert!(!result_ids(&hot).contains(&999));
    assert_eq!(result_ids(&hot), vec![2, 4, 6, 8, 10]);
}

// --- 契約 6: テーブル世代の進行 ---------------------------------------------

#[test]
fn index_rebuilds_after_generation_bump_and_results_stay_consistent() {
    let path = unique_db_path("scalar-index-prune-generation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_equivalent(&core, "tenant-a", sql);
    let before_builds = core.scalar_index_cache_stats().builds;

    // 新規行を書き込み世代を進める（既存の `kind = 'a'` 一致集合を拡張する）。
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO docs (id, embedding, kind, path) \
         VALUES (12, '[12.0,0.0]', 'a', 'even/12') USING OPERATION_ID 'seed-12'",
    )
    .expect("insert row after generation bump");

    let after_write = run(&core, "tenant-a", sql);
    assert!(
        result_ids(&after_write).contains(&12),
        "the newly written row must be visible after the generation bump"
    );
    let after_builds = core.scalar_index_cache_stats().builds;
    assert!(
        after_builds > before_builds,
        "a write must invalidate the stale index and trigger a rebuild"
    );

    // 再構築後の索引でも cold/hot 等価性が維持される。
    assert_cold_hot_equivalent(&core, "tenant-a", sql);
}
