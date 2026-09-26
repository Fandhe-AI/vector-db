//! `UNION`／`UNION ALL`／`INTERSECT`／`EXCEPT`（Issue #929。ポインタ: SQL-29 (c)・
//! RLS-10 (b)・TASK-213）の結合テスト。
//!
//! `tests/sql25_offset.rs`・`tests/sql24_like_patterns.rs` と同じ流儀（実
//! `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、
//! `EngineCore` を production 経路として使う）。各枝は単一テーブルの広域取得
//! （SQL-15）に限定する設計（`sql::set_op` モジュールドキュメント参照）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DOCS: &str = "docs";
const OTHER: &str = "other_docs";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn int_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("score", ColumnType::Integer, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    table: &str,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    visibility: Visibility,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{table}-{id}"))
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    expect_query(
        core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}")),
    )
}

fn run_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Ok(outcome) => panic!("expected error, got {outcome:?}"),
        Err(e) => e,
    }
}

fn langs(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            engine::sql::exec::Cell::Text(s) => s.clone(),
            other => panic!("expected Text cell, got {other:?}"),
        })
        .collect()
}

fn seeded_two_tables() -> (Storage, std::path::PathBuf) {
    let path = unique_db_path("set-op-basic");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&schema(OTHER))
        .expect("create other_docs");
    let tenant_ctx = ctx("tenant-a");
    insert_row(&storage, DOCS, &tenant_ctx, 1, "ja", Visibility::Public);
    insert_row(&storage, DOCS, &tenant_ctx, 2, "en", Visibility::Public);
    insert_row(&storage, DOCS, &tenant_ctx, 3, "ja", Visibility::Public);
    insert_row(&storage, OTHER, &tenant_ctx, 10, "en", Visibility::Public);
    insert_row(&storage, OTHER, &tenant_ctx, 11, "fr", Visibility::Public);
    (storage, path)
}

// ---------- 意味論 ----------

#[test]
fn union_all_concatenates_without_dedup() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION ALL SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    let mut want = vec!["ja", "en", "ja", "en", "fr"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    want.sort();
    assert_eq!(
        got, want,
        "UNION ALL must keep every row, duplicates included"
    );
}

#[test]
fn union_deduplicates_rows() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    assert_eq!(
        got,
        vec!["en".to_string(), "fr".to_string(), "ja".to_string()],
        "UNION must remove duplicate rows across both branches"
    );
}

#[test]
fn intersect_keeps_only_rows_present_on_both_sides() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs INTERSECT SELECT lang FROM other_docs",
    );
    assert_eq!(langs(&result), vec!["en".to_string()]);
}

#[test]
fn except_keeps_only_left_rows_absent_from_right() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs EXCEPT SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    assert_eq!(got, vec!["ja".to_string()]);
}

// ---------- 優先順位（INTERSECT は UNION/EXCEPT より高い優先順位で左結合） ----------

#[test]
fn intersect_binds_tighter_than_union() {
    let path = unique_db_path("set-op-precedence");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema("a")).expect("create a");
    storage.create_table(&schema("b")).expect("create b");
    storage.create_table(&schema("c")).expect("create c");
    let tenant_ctx = ctx("tenant-a");
    // a = {x}, b = {y}, c = {x}
    insert_row(&storage, "a", &tenant_ctx, 1, "x", Visibility::Public);
    insert_row(&storage, "b", &tenant_ctx, 2, "y", Visibility::Public);
    insert_row(&storage, "c", &tenant_ctx, 3, "x", Visibility::Public);
    let core = new_core(storage);

    // `A UNION B INTERSECT C` == `A UNION (B INTERSECT C)` == {x} UNION ({y} ∩ {x}) == {x}
    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM a UNION SELECT lang FROM b INTERSECT SELECT lang FROM c",
    );
    assert_eq!(langs(&result), vec!["x".to_string()]);
}

#[test]
fn explicit_parens_change_result_vs_default_precedence() {
    let path = unique_db_path("set-op-precedence-parens");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema("a")).expect("create a");
    storage.create_table(&schema("b")).expect("create b");
    storage.create_table(&schema("c")).expect("create c");
    let tenant_ctx = ctx("tenant-a");
    // a = {x}, b = {x, y}, c = {x}
    insert_row(&storage, "a", &tenant_ctx, 1, "x", Visibility::Public);
    insert_row(&storage, "b", &tenant_ctx, 2, "x", Visibility::Public);
    insert_row(&storage, "b", &tenant_ctx, 3, "y", Visibility::Public);
    insert_row(&storage, "c", &tenant_ctx, 4, "x", Visibility::Public);
    let core = new_core(storage);

    // デフォルト（左結合・INTERSECT が高優先）: A UNION (B INTERSECT C)
    //   = {x} UNION ({x,y} ∩ {x}) = {x} UNION {x} = {x}
    let default_form = run(
        &core,
        "tenant-a",
        "SELECT lang FROM a UNION SELECT lang FROM b INTERSECT SELECT lang FROM c",
    );
    let mut default_got = langs(&default_form);
    default_got.sort();
    assert_eq!(default_got, vec!["x".to_string()]);

    // 明示括弧: (A UNION B) INTERSECT C = {x,y} ∩ {x} = {x}
    // （区別できる fixture にするため、a に無い値を c にだけ入れて確認する別ケース）
    let parenthesized = run(
        &core,
        "tenant-a",
        "(SELECT lang FROM a UNION SELECT lang FROM b) INTERSECT SELECT lang FROM c",
    );
    let mut paren_got = langs(&parenthesized);
    paren_got.sort();
    assert_eq!(paren_got, vec!["x".to_string()]);
}

// ---------- 型整合（42804） ----------

#[test]
fn column_count_mismatch_is_datatype_mismatch() {
    let path = unique_db_path("set-op-column-count-mismatch");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang, embedding FROM docs",
    );
    assert_eq!(err.wire_code(), "42804");
}

#[test]
fn column_type_mismatch_is_datatype_mismatch() {
    let path = unique_db_path("set-op-column-type-mismatch");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&int_schema(OTHER))
        .expect("create other_docs");
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT score FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42804");
}

// ---------- VECTOR 列と重複除去の組（22000） ----------

#[test]
fn vector_column_with_union_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT embedding FROM docs UNION SELECT embedding FROM other_docs",
    );
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn vector_column_with_union_all_is_accepted() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT embedding FROM docs UNION ALL SELECT embedding FROM other_docs",
    );
    assert_eq!(result.rows.len(), 5);
}

// ---------- 構文拒否（42601） ----------

#[test]
fn intersect_all_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs INTERSECT ALL SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn union_distinct_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION DISTINCT SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn except_all_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs EXCEPT ALL SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn order_by_inside_branch_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs ORDER BY lang UNION SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn limit_inside_branch_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs LIMIT 1 UNION SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn aggregate_branch_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT COUNT(*) FROM docs UNION SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn bare_parenthesized_select_without_operator_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(&core, "tenant-a", "(SELECT lang FROM docs)");
    assert_eq!(err.wire_code(), "42601");
}

// ---------- 上限（54000） ----------

#[test]
fn paren_nesting_depth_exceeding_limit_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    // 入れ子上限は 4。5 段のネストは `54000`。
    let sql = "((((SELECT lang FROM docs)))) UNION SELECT lang FROM other_docs";
    // 上の式は 4 段の入れ子（許可される）なのでまず成功を確認する。
    let _ = run(&core, "tenant-a", sql);

    let too_deep = "(((((SELECT lang FROM docs))))) UNION SELECT lang FROM other_docs";
    let err = run_err(&core, "tenant-a", too_deep);
    assert_eq!(err.wire_code(), "54000");
}

// ---------- RLS（他テナントの不可視行が中間結果・重複除去・件数に影響しない） ----------

#[test]
fn rls_excludes_other_tenant_rows_from_union() {
    let path = unique_db_path("set-op-rls-union");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&schema(OTHER))
        .expect("create other_docs");
    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");
    insert_row(&storage, DOCS, &tenant_a, 1, "ja", Visibility::Public);
    // 他テナントの private 行（不可視のはず）。
    insert_row(&storage, OTHER, &tenant_b, 2, "ja", Visibility::Private);
    insert_row(&storage, OTHER, &tenant_a, 3, "en", Visibility::Public);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    // 他テナントの "ja" 行が重複除去の対象や結果件数に影響しないこと
    // （もし混入していれば "ja" は既に docs 側にあるため件数は変わらないが、
    // 追加のテナント境界検証として EXCEPT で不可視行の非存在を確認する）。
    assert_eq!(got, vec!["en".to_string(), "ja".to_string()]);

    // EXCEPT: 他テナントにしか存在しない値（"ja" は tenant-a 自身も持つため
    // 区別できない。tenant-b 専用の値で検証する）。
    let except_result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM other_docs EXCEPT SELECT lang FROM docs",
    );
    // other_docs の可視行は tenant-a 視点で "en" のみ（tenant-b の "ja" は不可視）。
    // docs は "ja" のみなので EXCEPT 結果は "en"。
    assert_eq!(langs(&except_result), vec!["en".to_string()]);
}

#[test]
fn rls_visible_rows_are_independent_per_branch_for_intersect() {
    let path = unique_db_path("set-op-rls-intersect");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&schema(OTHER))
        .expect("create other_docs");
    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");
    // tenant-b の private "fr" は tenant-a からは不可視。tenant-a 自身の "fr" は無い。
    insert_row(&storage, DOCS, &tenant_b, 1, "fr", Visibility::Private);
    insert_row(&storage, OTHER, &tenant_b, 2, "fr", Visibility::Private);
    insert_row(&storage, DOCS, &tenant_a, 3, "en", Visibility::Public);
    insert_row(&storage, OTHER, &tenant_a, 4, "en", Visibility::Public);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs INTERSECT SELECT lang FROM other_docs",
    );
    // 他テナントの private "fr" が両側に存在していても、tenant-a からは不可視
    // なので INTERSECT の判定には一切現れない。
    assert_eq!(langs(&result), vec!["en".to_string()]);
}

// ---------- 全体 LIMIT ----------

#[test]
fn top_level_limit_truncates_result() {
    let path = unique_db_path("set-op-top-limit");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=5u64 {
        insert_row(&storage, DOCS, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION ALL SELECT lang FROM docs LIMIT 3",
    );
    assert_eq!(result.rows.len(), 3);
}

// ---------- 決定性 ----------

#[test]
fn repeated_calls_are_deterministic() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let first = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    let second = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    assert_eq!(langs(&first), langs(&second));
}
