//! 集計・`GROUP BY` 経路（`sql::aggregate`・`sql::group_by`）を
//! `sql::scalar_index::ScalarIndex` の候補削減・キー列挙経路へ結線する
//! （Issue #475・親 #472・#359）SQL 表層結合テスト。
//!
//! `tests/scalar_index_prune.rs`（Issue #474・SELECT 経路の結線）と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `engine::tenant::insert_typed_row` による投入、`EngineCore::execute_sql`
//! による production 経路の起動）で、集計・`GROUP BY` 側の候補削減・キー列挙
//! 経路を検証する。
//!
//! 検証する契約（Issue #475 のスコープ）:
//! 1. 索引対応述語のみの `WHERE` を持つ集計（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`）の
//!    cold（この集計クエリ自身の走査から piggyback で索引構築。SELECT を
//!    先に流さなくても `aggregate_index_scans` が非 vacuous に増える）・hot
//!    が完全一致
//! 2. `WHERE` なしの `GROUP BY`（列挙形）・索引対応述語ありの `GROUP BY`
//!    （候補走査形）双方の cold/hot 等価性（NULL グループの存在・順序を含む）
//! 3. 残余述語（評価エラーを起こしうる式・embedding 参照）を含む場合は索引を
//!    使わず、結果・エラー契約とも全走査と不変
//! 4. RLS: 他テナントの private 行は索引経路でも一切露出しない（オラクル対照）
//! 5. `VECTOR` 列なしテーブルの集計（SQL-13）は従来経路のまま結果不変

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
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
            ColumnDef::new("kind", ColumnType::Text, true),
            ColumnDef::new("lang", ColumnType::Text, true),
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
    kind: Option<&str>,
    lang: Option<&str>,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            kind.map(|k| Value::Text(k.to_string()))
                .unwrap_or(Value::Null),
            lang.map(|l| Value::Text(l.to_string()))
                .unwrap_or(Value::Null),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

/// 10 行（`id` 1..=10）: 偶数 `id` は `kind = 'a'`、奇数 `id` は `kind = 'b'`。
/// `lang` は `id` を 3 で割った余りで 3 グループ（0 => "ja", 1 => "en",
/// 2 => NULL）に分ける。
fn seed_ten_rows(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=10u64 {
        let kind = if id % 2 == 0 { "a" } else { "b" };
        let lang = match id % 3 {
            0 => Some("ja"),
            1 => Some("en"),
            _ => None,
        };
        insert_row(
            storage,
            &tenant_ctx,
            id,
            Some(kind),
            lang,
            Visibility::Public,
        );
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn single_row_cells(result: &QueryResult) -> Vec<Cell> {
    assert_eq!(result.rows.len(), 1, "expected a single aggregate row");
    result.rows[0].cells.clone()
}

fn group_rows(result: &QueryResult) -> Vec<Vec<Cell>> {
    result.rows.iter().map(|r| r.cells.clone()).collect()
}

/// クエリを 2 回実行し（cold: 索引 piggyback 構築のみ・hot: 索引消費）、
/// 結果が完全一致することを固定したうえで、hot 実行後の
/// `aggregate_index_scans` 増分を返す。SELECT を先に流さないため、非 vacuous
/// 性（cold 自身が索引を構築すること）も同時に検証している。
fn assert_cold_hot_equivalent(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    let cold = run(core, tenant, sql);
    let hot = run(core, tenant, sql);
    assert_eq!(
        cold.rows.len(),
        hot.rows.len(),
        "cold/hot row count must match for: {sql}"
    );
    for (c, h) in cold.rows.iter().zip(&hot.rows) {
        assert_eq!(c.cells, h.cells, "cold/hot cells must match for: {sql}");
    }
    let after = core.scalar_index_cache_stats().aggregate_index_scans;
    after.saturating_sub(before)
}

// --- 契約 1: GROUP BY なし集計の cold/hot 等価性・非 vacuous 性 -------------

#[test]
fn where_equality_count_star_cold_hot_equivalence_and_non_vacuous() {
    let path = unique_db_path("scalar-index-aggregate-count-eq");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT COUNT(*) AS n FROM docs WHERE kind = 'a'";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "an equality predicate must be consumed by the index without priming via SELECT"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(single_row_cells(&result), vec![Cell::Integer(5)]);
}

#[test]
fn where_id_range_multi_aggregate_cold_hot_equivalence() {
    let path = unique_db_path("scalar-index-aggregate-multi");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT COUNT(kind) AS c, SUM(id) AS s, AVG(id) AS a, MIN(kind) AS mn, \
               MAX(kind) AS mx FROM docs WHERE id > 5";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(index_scans > 0);
    let result = run(&core, "tenant-a", sql);
    // id in {6,7,8,9,10}: kind = a,b,a,b,a (id%2==0 => a)
    assert_eq!(
        single_row_cells(&result),
        vec![
            Cell::Integer(5),
            Cell::Integer(40),
            Cell::Float(8.0),
            Cell::Text("a".to_string()),
            Cell::Text("b".to_string()),
        ]
    );
}

#[test]
fn where_conjunction_cold_hot_equivalence() {
    let path = unique_db_path("scalar-index-aggregate-conjunction");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT COUNT(*) AS n FROM docs WHERE kind = 'a' AND id > 4";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(index_scans > 0);
    // even ids > 4: 6, 8, 10
    let result = run(&core, "tenant-a", sql);
    assert_eq!(single_row_cells(&result), vec![Cell::Integer(3)]);
}

// --- 契約 3: 残余述語・選択度超過は索引を使わず結果・エラー契約とも不変 ----

#[test]
fn residual_expression_error_still_fails_closed_with_22000() {
    let path = unique_db_path("scalar-index-aggregate-residual-error");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `kind = 'a'`（索引対応）と、常にエラーになる残余述語（0 除算）を含む。
    let sql = "SELECT COUNT(*) AS n FROM docs WHERE kind = 'a' AND (1.0 / 0.0) > 0";
    let err = core
        .execute_sql(&ctx("tenant-a"), sql)
        .expect_err("division-by-zero predicate must fail closed");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn embedding_referencing_predicate_does_not_consume_index() {
    let path = unique_db_path("scalar-index-aggregate-embedding-where");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    let sql = "SELECT COUNT(*) AS n FROM docs WHERE kind = 'a' AND vec_norm(embedding) >= 0";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(single_row_cells(&cold), single_row_cells(&hot));
    let after = core.scalar_index_cache_stats().aggregate_index_scans;
    assert_eq!(
        before, after,
        "an embedding-referencing residual predicate must never consume the index"
    );
    assert_eq!(single_row_cells(&cold), vec![Cell::Integer(5)]);
}

// --- 契約 2: GROUP BY 列挙形（WHERE なし）・候補走査形（WHERE あり） -------

#[test]
fn group_by_enumeration_cold_hot_equivalence_with_null_group() {
    let path = unique_db_path("scalar-index-aggregate-group-enum");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    let sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY lang";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(group_rows(&cold), group_rows(&hot));
    let after = core.scalar_index_cache_stats().aggregate_index_scans;
    assert!(
        after > before,
        "a WHERE-less GROUP BY must consume the enumeration form without priming via SELECT"
    );

    // id 1..=10, id%3: 0=>ja(3,6,9), 1=>en(1,4,7,10), other=>NULL(2,5,8)
    let rows = group_rows(&hot);
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("en".to_string()), Cell::Integer(4)],
            vec![Cell::Text("ja".to_string()), Cell::Integer(3)],
            vec![Cell::Null, Cell::Integer(3)],
        ]
    );
}

#[test]
fn group_by_candidate_walk_with_where_cold_hot_equivalence() {
    let path = unique_db_path("scalar-index-aggregate-group-where");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    let sql = "SELECT lang, COUNT(*) AS n FROM docs WHERE kind = 'a' GROUP BY lang ORDER BY lang";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(group_rows(&cold), group_rows(&hot));
    let after = core.scalar_index_cache_stats().aggregate_index_scans;
    assert!(after > before);

    // kind='a' (even ids: 2,4,6,8,10). id%3: 2=>NULL,4=>en,6=>ja,8=>NULL,10=>en
    let rows = group_rows(&hot);
    assert_eq!(
        rows,
        vec![
            vec![Cell::Text("en".to_string()), Cell::Integer(2)],
            vec![Cell::Text("ja".to_string()), Cell::Integer(1)],
            vec![Cell::Null, Cell::Integer(2)],
        ]
    );
}

#[test]
fn group_by_having_and_limit_use_index_path_and_match_oracle() {
    let path = unique_db_path("scalar-index-aggregate-group-having");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql =
        "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n > 2 ORDER BY n DESC LIMIT 1";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(group_rows(&cold), group_rows(&hot));
    assert_eq!(
        group_rows(&hot),
        vec![vec![Cell::Text("en".to_string()), Cell::Integer(4)]]
    );
}

// --- 契約 4: RLS オラクル ----------------------------------------------------

#[test]
fn rls_private_rows_of_other_tenant_never_leak_via_index_path() {
    let path = unique_db_path("scalar-index-aggregate-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    // tenant-b の private 行を同じ kind/lang 値で混入させる。
    let tenant_b = ctx("tenant-b");
    for id in 101..=105u64 {
        insert_row(
            &storage,
            &tenant_b,
            id,
            Some("a"),
            Some("ja"),
            Visibility::Private,
        );
    }
    let core = new_core(storage);

    // 対照: tenant-b 行が存在しない DB との統計縮約オラクル。
    let count_sql = "SELECT COUNT(*) AS n FROM docs WHERE kind = 'a'";
    let group_sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY lang";

    // tenant-a から見ると、tenant-b の private 行は一切現れない。
    let count_result = run(&core, "tenant-a", count_sql);
    assert_eq!(single_row_cells(&count_result), vec![Cell::Integer(5)]);
    let group_result = run(&core, "tenant-a", group_sql);
    assert_eq!(
        group_rows(&group_result),
        vec![
            vec![Cell::Text("en".to_string()), Cell::Integer(4)],
            vec![Cell::Text("ja".to_string()), Cell::Integer(3)],
            vec![Cell::Null, Cell::Integer(3)],
        ]
    );

    // 索引経路（hot）でも同様（RLS-7・RLS-8: 他テナント行の件数・存在が
    // グループ集計結果へ一切現れない）。
    let count_hot = run(&core, "tenant-a", count_sql);
    assert_eq!(single_row_cells(&count_hot), vec![Cell::Integer(5)]);
    let group_hot = run(&core, "tenant-a", group_sql);
    assert_eq!(group_rows(&group_hot), group_rows(&group_result));
}

// --- 契約 5: VECTOR 列なしテーブルは従来経路のまま不変（SQL-13） -----------

#[test]
// `TableSchema::validate_embedding_dim` 系のチェックにより、本リポジトリの
// 現行公開経路（`tenant::insert_typed_row` 等）は `VECTOR` 列を持たない
// テーブルへの行挿入を受け付けない（`tests/sql_aggregate.rs`
// `sql13_works_on_empty_table_without_vector_column` の既存コメント・
// TASK-166 スコープ外の既存制約を参照）。そのため本 Issue のスコープでも
// 「行が 1 件もない `VECTOR` 列なしテーブル」でゲートが不使用のまま従来経路
// （空集合契約）で応答することのみ検証する。
fn table_without_vector_column_uses_plain_scan_and_is_unaffected() {
    let path = unique_db_path("scalar-index-aggregate-no-vector");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let no_vector_schema = TableSchema::new(
        "notes",
        vec![ColumnDef::new("kind", ColumnType::Text, true)],
    );
    storage
        .create_table(&no_vector_schema)
        .expect("create table");
    let core = new_core(storage);

    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    let sql = "SELECT COUNT(*) AS n FROM notes WHERE kind = 'a'";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(single_row_cells(&cold), vec![Cell::Integer(0)]);
    assert_eq!(single_row_cells(&cold), single_row_cells(&hot));
    let after = core.scalar_index_cache_stats().aggregate_index_scans;
    assert_eq!(
        before, after,
        "a table without a VECTOR column must never use the scalar index aggregate path"
    );
}

// --- codex-review P1（PR #603）: `WHERE` なしの `GROUP BY` 列挙形は
// TEXT MIN/MAX を含む場合に使わない ---------------------------------------
//
// PR #603 で追加された `is_text_accumulator_budget_error` による捕捉・
// フォールバック（`crates/engine/tests/sql_group_by.rs::
// text_min_max_index_path_key_order_transient_overflow_falls_back_to_plain_scan`）
// は「索引経路の処理順序でのみ超過を検出したケース」を救うが、その逆方向
// （全走査〔物理行順〕なら容量超過で `54000` になるはずが、索引経路の
// グループ単位の縮小により超過を一度も観測せず誤って成功してしまうケース）
// までは救えない。列挙形は値ごとにグループをまとめて処理するため、この
// テストの構成（各グループが「大きい値の行」と「小さい値の行」の 2 行から
// 成る）では、あるグループの処理が完了するたびに `MIN`/`MAX` の一時的な
// 累計バイト数が縮小され、全走査の物理行順ピーク（先頭グループの大きい値が
// 積み上がって超過する）を索引経路の処理順序では決して再現できない。
// 索引が使えるかどうかで `54000` の成否が変わってはならないため
// （AGENTS.md「公開 API・エラー契約の互換性」）、TEXT MIN/MAX を含む
// `WHERE` なしの `GROUP BY` は列挙形を最初から使わない
// （`sql::group_by::has_text_min_max_aggregate`）。

/// 6 グループ、各グループが 3 MiB の値を持つ行（`id`=0..5）と 1 バイトの値を
/// 持つ行（`id`=6..11）から成る具体例を固定する。全走査（物理行順）は先頭
/// 6 行の時点で 18 MiB となり容量超過（`54000`）になるが、列挙形はグループ
/// 単位で直ちに縮小するため超過を検出できない。索引が使えるかどうかで
/// 成否が変わらないことを検証する。
#[test]
fn text_min_max_group_by_capacity_judgement_matches_full_scan() {
    let path = unique_db_path("scalar-index-aggregate-text-minmax-capacity");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let capacity_schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("k", ColumnType::Text, true),
            ColumnDef::new("v", ColumnType::Text, true),
        ],
    );
    storage
        .create_table(&capacity_schema)
        .expect("create table");

    let tenant_ctx = ctx("tenant-a");
    const GROUPS: u64 = 6;
    let large_value = "z".repeat(3 * 1024 * 1024);
    for group in 0..GROUPS {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &tenant_ctx,
            group,
            Visibility::Public,
            &[
                Value::Vector(vec![group as f32, 0.0]),
                Value::Text(group.to_string()),
                Value::Text(large_value.clone()),
            ],
            &op_id(&format!("seed-large-{group}")),
        )
        .expect("insert large row");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &tenant_ctx,
            group + GROUPS,
            Visibility::Public,
            &[
                Value::Vector(vec![(group + GROUPS) as f32, 0.0]),
                Value::Text(group.to_string()),
                Value::Text("a".to_string()),
            ],
            &op_id(&format!("seed-small-{group}")),
        )
        .expect("insert small row");
    }
    let core = new_core(storage);

    let plain_before = core
        .scalar_index_cache_stats()
        .aggregate_plain_scan_fallbacks;
    let index_before = core.scalar_index_cache_stats().aggregate_index_scans;

    let sql = "SELECT k, MIN(v) AS mn FROM docs GROUP BY k";
    let cold_err = core
        .execute_sql(&ctx("tenant-a"), sql)
        .expect_err("capacity judgement must fail regardless of index availability (cold)");
    assert_eq!(cold_err.wire_code(), "54000");
    let hot_err = core
        .execute_sql(&ctx("tenant-a"), sql)
        .expect_err("capacity judgement must fail regardless of index availability (hot)");
    assert_eq!(hot_err.wire_code(), "54000");

    let plain_after = core
        .scalar_index_cache_stats()
        .aggregate_plain_scan_fallbacks;
    let index_after = core.scalar_index_cache_stats().aggregate_index_scans;
    assert_eq!(
        index_before, index_after,
        "a TEXT MIN/MAX GROUP BY must never consume the enumeration index path"
    );
    assert!(
        plain_after > plain_before,
        "a TEXT MIN/MAX GROUP BY must always fall back to the full scan"
    );
}
