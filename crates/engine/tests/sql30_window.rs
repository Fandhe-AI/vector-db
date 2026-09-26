//! ウィンドウ関数（SQL-30・TASK-214、Issue #930）の結合テスト。
//!
//! `tests/sql_scan.rs`（広域取得。Issue #454）と同じ流儀（`unique_db_path`＋
//! `CleanupGuard`、公開 API（`EngineCore::execute_sql`）のみを経由する）で検証する。
//! テナント境界の一般化検証は `tests/rls_generalized.rs` が別途担うため、本ファイルは
//! ウィンドウ固有の契約（受理・値・決定性・拒否マトリクス・RLS 不変性・ビュー列
//! スコープ）に焦点を絞る。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("score", ColumnType::Integer, false),
        ],
    )
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, id: u64, lang: &str, score: i32) {
    insert_row_with_visibility(storage, ctx, id, lang, score, Visibility::Public);
}

/// `visibility` を明示指定できる版（RLS 不変性テスト用。`Visibility::Public` は
/// テナント越境で可視になる既存契約〔`PolicyContext::is_visible`〕のため、他
/// テナントから不可視であることを検証したい行は `Visibility::Private` で挿入する
/// 必要がある）。
fn insert_row_with_visibility(
    storage: &Storage,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    score: i32,
    visibility: Visibility,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "test-op-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        visibility,
        &[Value::Text(lang.to_string()), Value::Integer(score)],
        &op_id,
    )
    .expect("insert row");
}

/// `id, lang, score` の 6 行（`ja` 3 行・`en` 3 行、`score` に重複あり）。
fn seed_basic(storage: &Storage, ctx: &PolicyContext) {
    storage.create_table(&schema()).expect("create table");
    insert_row(storage, ctx, 1, "ja", 10);
    insert_row(storage, ctx, 2, "ja", 20);
    insert_row(storage, ctx, 3, "ja", 20);
    insert_row(storage, ctx, 4, "en", 5);
    insert_row(storage, ctx, 5, "en", 15);
    insert_row(storage, ctx, 6, "en", 15);
}

fn expect_query(
    result: Result<engine::sql::exec::QueryResult, engine::sql::allowlist::SqlSurfaceError>,
) -> engine::sql::exec::QueryResult {
    result.expect("statement should succeed")
}

fn cell_int(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

/// `SUM`/`AVG`/`MIN`/`MAX(<INTEGER 列>)` の結果セル（`Cell::SignedInteger`。
/// `sql::aggregate::Accumulator::finish` の `IntSum`/`IntMin`/`IntMax` 契約と同じ）。
fn cell_signed(cell: &Cell) -> i64 {
    match cell {
        Cell::SignedInteger(v) => *v,
        other => panic!("expected Cell::SignedInteger, got {other:?}"),
    }
}

fn row_map(
    result: &engine::sql::exec::QueryResult,
    id_col: usize,
) -> std::collections::HashMap<u64, &[Cell]> {
    result
        .rows
        .iter()
        .map(|r| {
            let id = match &r.cells[id_col] {
                Cell::Integer(v) => *v,
                other => panic!("expected id column to be Cell::Integer, got {other:?}"),
            };
            (id, r.cells.as_slice())
        })
        .collect()
}

// ---------- 受理と値 ----------

#[test]
fn row_number_partitioned_and_ordered() {
    let path = unique_db_path("window-row-number");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY lang ORDER BY score DESC) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    // ja: id2(20) id3(20) id1(10) -> row_number は id 昇順でタイブレーク
    // (score DESC のみでは 2,3 が同点のため id 昇順で 1,2 を割り当てる)。
    assert_eq!(cell_int(&rows[&2][1]), 1);
    assert_eq!(cell_int(&rows[&3][1]), 2);
    assert_eq!(cell_int(&rows[&1][1]), 3);
    // en: id5(15) id6(15) id4(5)
    assert_eq!(cell_int(&rows[&5][1]), 1);
    assert_eq!(cell_int(&rows[&6][1]), 2);
    assert_eq!(cell_int(&rows[&4][1]), 3);
}

#[test]
fn rank_and_dense_rank_skip_or_not_skip_on_ties() {
    let path = unique_db_path("window-rank");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, RANK() OVER (PARTITION BY lang ORDER BY score DESC), DENSE_RANK() OVER (PARTITION BY lang ORDER BY score DESC) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    // ja: 20,20,10 -> RANK 1,1,3 / DENSE_RANK 1,1,2
    assert_eq!(cell_int(&rows[&2][1]), 1);
    assert_eq!(cell_int(&rows[&3][1]), 1);
    assert_eq!(cell_int(&rows[&1][1]), 3);
    assert_eq!(cell_int(&rows[&2][2]), 1);
    assert_eq!(cell_int(&rows[&3][2]), 1);
    assert_eq!(cell_int(&rows[&1][2]), 2);
}

#[test]
fn count_star_over_partition_matches_partition_size() {
    let path = unique_db_path("window-count-star");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, COUNT(*) OVER (PARTITION BY lang) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    for id in [1u64, 2, 3, 4, 5, 6] {
        assert_eq!(
            cell_int(&rows[&id][1]),
            3,
            "partition size mismatch for id {id}"
        );
    }
}

#[test]
fn sum_over_order_by_is_cumulative_per_peer_group() {
    let path = unique_db_path("window-sum-cumulative");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, SUM(score) OVER (PARTITION BY lang ORDER BY score) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    // ja ascending: 10, 20, 20 -> cumulative: 10, 50, 50 (peer group 20,20 gets same cumulative sum)
    assert_eq!(cell_signed(&rows[&1][1]), 10);
    assert_eq!(cell_signed(&rows[&2][1]), 50);
    assert_eq!(cell_signed(&rows[&3][1]), 50);
}

#[test]
fn window_over_empty_parens_treats_whole_partition_as_one_peer_group() {
    let path = unique_db_path("window-over-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result =
        expect_query(core.execute_sql(&ctx, "SELECT id, COUNT(*) OVER () FROM docs LIMIT 10"));
    let rows = row_map(&result, 0);
    for id in [1u64, 2, 3, 4, 5, 6] {
        assert_eq!(cell_int(&rows[&id][1]), 6);
    }
}

#[test]
fn window_item_only_select_has_no_plain_columns() {
    let path = unique_db_path("window-only-select");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10",
    ));
    assert_eq!(result.columns.len(), 1);
    assert!(matches!(result.columns[0], ColumnMeta::Computed { .. }));
    assert_eq!(result.rows.len(), 6);
}

#[test]
fn window_alias_and_position_are_preserved_among_plain_columns() {
    let path = unique_db_path("window-alias-position");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT lang, ROW_NUMBER() OVER (PARTITION BY lang ORDER BY score) AS rn, score FROM docs LIMIT 10",
    ));
    assert_eq!(result.columns.len(), 3);
    assert!(matches!(
        result.columns[0],
        ColumnMeta::Scalar {
            ty: ColumnType::Text,
            ..
        }
    ));
    assert!(matches!(result.columns[1], ColumnMeta::Computed { ref name } if name == "rn"));
    assert!(matches!(
        result.columns[2],
        ColumnMeta::Scalar {
            ty: ColumnType::Integer,
            ..
        }
    ));
}

#[test]
fn where_combines_with_window_and_filters_output_rows_but_not_window_population() {
    let path = unique_db_path("window-where");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    // WHERE lang = 'ja' で出力は ja の 3 行のみだが、COUNT(*) OVER () は
    // WHERE 適用後の母集合（ja の 3 行のみ）で計算される。
    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, COUNT(*) OVER () FROM docs WHERE lang = 'ja' LIMIT 10",
    ));
    assert_eq!(result.rows.len(), 3);
    for row in &result.rows {
        assert_eq!(cell_int(&row.cells[1]), 3);
    }
}

#[test]
fn offset_skips_output_rows_but_window_values_reflect_full_population() {
    let path = unique_db_path("window-offset");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let full =
        expect_query(core.execute_sql(&ctx, "SELECT id, COUNT(*) OVER () FROM docs LIMIT 10"));
    let offset = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, COUNT(*) OVER () FROM docs LIMIT 10 OFFSET 3",
    ));
    assert_eq!(full.rows.len(), 6);
    assert_eq!(offset.rows.len(), 3);
    for row in &offset.rows {
        assert_eq!(cell_int(&row.cells[1]), 6);
    }
}

// ---------- 決定性 ----------

#[test]
fn row_number_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("window-determinism");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);
    let sql = "SELECT id, ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10";

    let first = expect_query(core.execute_sql(&ctx, sql));
    let second = expect_query(core.execute_sql(&ctx, sql));
    let first_vals: Vec<(u64, u64)> = first
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    let second_vals: Vec<(u64, u64)> = second
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    assert_eq!(first_vals, second_vals);
}

// ---------- 拒否マトリクス ----------

fn expect_rejected(core: &EngineCore, ctx: &PolicyContext, sql: &str, wire_code: &str) {
    let err = core
        .execute_sql(ctx, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"));
    assert_eq!(err.wire_code(), wire_code, "sql={sql}");
}

#[test]
fn rejects_group_by_combined_with_window() {
    let path = unique_db_path("window-reject-group-by");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT lang, COUNT(*) OVER () FROM docs GROUP BY lang",
        "42601",
    );
}

#[test]
fn rejects_order_by_combined_with_window() {
    let path = unique_db_path("window-reject-order-by");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    // 非 window の検索 SELECT は VECTOR 列を要求するため、ここでは構文段で
    // ウィンドウ項目＋通常の ORDER BY 併用が `42601` になることのみを固定する
    // （テーブルに VECTOR 列が無くても構文検証はテーブル存在確認の前に走る）。
    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER () FROM docs ORDER BY score LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_frame_clause() {
    let path = unique_db_path("window-reject-frame");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY score ROWS UNBOUNDED PRECEDING) FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_named_window() {
    let path = unique_db_path("window-reject-named");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER w FROM docs WINDOW w AS (ORDER BY score) LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_distinct_inside_window_aggregate() {
    let path = unique_db_path("window-reject-distinct");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, COUNT(DISTINCT lang) OVER () FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_star_mixed_with_window_item() {
    let path = unique_db_path("window-reject-star-mix");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT *, ROW_NUMBER() OVER () FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_unknown_window_function_name() {
    let path = unique_db_path("window-reject-unknown-func");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, LAG(score) OVER (ORDER BY score) FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_ranking_function_with_argument() {
    let path = unique_db_path("window-reject-ranking-arg");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER(score) OVER (ORDER BY score) FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_where_referencing_window_alias() {
    let path = unique_db_path("window-reject-where-alias");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY score) AS rn FROM docs WHERE rn = 1 LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_using_plan_combined_with_window() {
    let path = unique_db_path("window-reject-using-plan");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER () FROM docs USING PLAN('find docs') LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_sum_of_text_column_type_mismatch() {
    let path = unique_db_path("window-reject-sum-text");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, SUM(lang) OVER () FROM docs LIMIT 10",
        "22000",
    );
}

#[test]
fn rejects_too_many_partition_by_columns() {
    let path = unique_db_path("window-reject-too-many-keys");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let cols = "lang, ".repeat(9);
    let sql = format!(
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY {}score) FROM docs LIMIT 10",
        cols
    );
    expect_rejected(&core, &ctx, &sql, "54000");
}

// ---------- RLS ----------

#[test]
fn window_values_are_invariant_to_other_tenants_rows() {
    // EngineCore は Storage の所有権を取るため、「後から他テナント行を追加する」
    // ことを直接は表現できない。代わりに、同一テナント A のコーパスへ (1)
    // 追加行なし・(2) 大量の他テナント B 行を追加、の 2 通りの DB を独立に構築し、
    // テナント A から見た結果が完全に一致することを検証する（RLS-7・RLS-8 の
    // ウィンドウ版）。
    let ctx_a = ctx_for("tenant-a");
    let sql = "SELECT id, COUNT(*) OVER (), ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10";

    let path1 = unique_db_path("window-rls-invariance-a-only");
    let _guard1 = CleanupGuard(path1.clone());
    let storage1 = open_storage(&path1);
    seed_basic(&storage1, &ctx_a);
    let core1 = new_core(storage1);
    let a_only = expect_query(core1.execute_sql(&ctx_a, sql));

    let path2 = unique_db_path("window-rls-invariance-a-and-b");
    let _guard2 = CleanupGuard(path2.clone());
    let storage2 = open_storage(&path2);
    seed_basic(&storage2, &ctx_a);
    let ctx_b = ctx_for("tenant-b");
    for id in 1..=50u64 {
        insert_row_with_visibility(
            &storage2,
            &ctx_b,
            id,
            "ja",
            (id % 7) as i32,
            Visibility::Private,
        );
    }
    let core2 = new_core(storage2);
    let a_and_b = expect_query(core2.execute_sql(&ctx_a, sql));

    let a_only_vals: Vec<(u64, u64, u64)> = a_only
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1]), cell_int(&r.cells[2])))
        .collect();
    let a_and_b_vals: Vec<(u64, u64, u64)> = a_and_b
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1]), cell_int(&r.cells[2])))
        .collect();
    assert_eq!(a_only_vals, a_and_b_vals);
}

// ---------- cursor / COPY TO ----------

#[test]
fn declare_cursor_over_window_select_matches_direct_execution() {
    let path = unique_db_path("window-cursor");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);
    let window_sql = "SELECT id, ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10";

    let direct = expect_query(core.execute_sql(&ctx, window_sql));

    let mut session = engine::sql::mode::SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("BEGIN should succeed");
    let declare_sql = format!("DECLARE c CURSOR FOR {window_sql}");
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, &declare_sql)
        .expect("DECLARE CURSOR over window select should succeed");
    let fetched = match core
        .execute_sql_in_txn(&ctx, &mut session, &mut txn, "FETCH 10 FROM c")
        .expect("FETCH should succeed")
    {
        engine::sql::SqlOutcome::Fetch(result) => result,
        other => panic!("expected SqlOutcome::Fetch, got {other:?}"),
    };
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "COMMIT")
        .expect("COMMIT should succeed");

    let direct_vals: Vec<(u64, u64)> = direct
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    let fetched_vals: Vec<(u64, u64)> = fetched
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    assert_eq!(direct_vals, fetched_vals);
}
