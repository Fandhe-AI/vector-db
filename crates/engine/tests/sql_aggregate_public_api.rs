//! `bind_aggregate`／`execute_aggregate`／`BoundAggregate`（TASK-186・NOSQL-4・
//! NOSQL-5。Issue #727）が engine クレート外から到達可能な公開 API であることを
//! 固定する結合テスト。
//!
//! `tests/sql_scan_public_api.rs` と同じ流儀（`Storage` で投入 → drop →
//! 生 `redb::Database::open` で読み取り専用トランザクションを得る）で
//! `&redb::ReadTransaction` を用意する。`Storage::db()` は `pub(crate)` のまま
//! 変更しないため、`execute_aggregate` の呼び出しにはこの経路以外に手段がない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::aggregate::execute_aggregate;
use engine::sql::allowlist::{validate_sql, AggregateFunc, Statement};
use engine::sql::exec::Cell;
use engine::sql::parser::bind_aggregate;
use engine::sql::udf_call::UdfRegistry;
use engine::storage::{Storage, Visibility};
use redb::ReadableDatabase;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// tenant-a に `Public` 行 id 1..=5（`lang` = `"ja"` 3 件・`"en"` 2 件）、
/// tenant-b に `Private` 行 id 101..=103（`lang` = `"xx"`。tenant-b にしか
/// 存在しないグループ値）を投入する（RLS 境界確認用。
/// `tests/sql_scan_public_api.rs::seed_two_tenants` と同じ判断で tenant-b 側を
/// `Private` にし、tenant-a の `PolicyContext` からは不可視にする）。
fn seed_two_tenants(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");

    let langs = ["ja", "ja", "ja", "en", "en"];
    for (idx, lang) in langs.iter().enumerate() {
        let id = idx as u64 + 1;
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-a-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
                Value::Text((*lang).to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }
    for id in 101..=103u64 {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-b-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
                Value::Text("xx".to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-b row");
    }
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant ctx")
}

#[test]
fn bind_aggregate_and_execute_aggregate_are_reachable_from_outside_the_crate() {
    let path = unique_db_path("sql-aggregate-public-api-bind-execute");
    let _guard = CleanupGuard(path.clone());

    // `validate_sql`・`bind_aggregate` は `Storage`（`TableLookup`）を要求する
    // ため、`execute_aggregate` に渡す `&redb::ReadTransaction` を生
    // `redb::Database` で得る前に、この単一の `Storage` セッション内で完結させる
    // （`tests/sql_scan_public_api.rs` と同じ制約）。
    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let validated = validate_sql("SELECT COUNT(*), SUM(id), MIN(lang) FROM docs", &storage)
            .expect("validate_sql should accept aggregate form");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        assert_eq!(bound.table(), TABLE);
        assert_eq!(bound.items().len(), 3);
        assert_eq!(bound.items()[0].func(), AggregateFunc::Count);
        assert_eq!(bound.items()[0].name(), "count");
        assert!(!bound.has_group_by());
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should succeed (本 Issue で pub 化)");
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    // tenant-a の可視行は 5 件（id 1..=5）。tenant-b の Private 行
    // （id 101..=103・`lang="xx"`）が混入しないことは MIN(lang) の値で確認する
    // （RLS-7・RLS-8。混入していれば辞書順最小の "en" ではなく別値になる）。
    assert_eq!(row.cells[0], Cell::Integer(5));
    assert_eq!(row.cells[1], Cell::Integer(15));
    assert_eq!(row.cells[2], Cell::Text("en".to_string()));
}

#[test]
fn execute_aggregate_dispatches_group_by_without_caches() {
    let path = unique_db_path("sql-aggregate-public-api-group-by");
    let _guard = CleanupGuard(path.clone());

    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let validated = validate_sql(
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY n DESC",
            &storage,
        )
        .expect("validate_sql should accept group by form");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        assert!(bound.has_group_by());
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    // `execute_aggregate`（3 キャッシュとも `None`）越しに
    // `execute_grouped_aggregate` の列挙形フォールバック経路が動くことを固定
    // する（Issue #475）。tenant-b 専有のグループ値 `"xx"` は現れない。
    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should dispatch to group by execution");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].cells[0], Cell::Text("ja".to_string()));
    assert_eq!(result.rows[0].cells[1], Cell::Integer(3));
    assert_eq!(result.rows[1].cells[0], Cell::Text("en".to_string()));
    assert_eq!(result.rows[1].cells[1], Cell::Integer(2));
}

#[test]
fn execute_aggregate_applies_index_eligible_where_without_caches() {
    let path = unique_db_path("sql-aggregate-public-api-where");
    let _guard = CleanupGuard(path.clone());

    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let validated = validate_sql("SELECT COUNT(*) FROM docs WHERE lang = 'ja'", &storage)
            .expect("validate_sql should accept aggregate form with WHERE");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        assert_eq!(bound.metadata_filters().len(), 1);
        assert!(bound.expr_filters().is_empty());
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    // 索引対応述語（`TEXT` 等価）だが `ScalarIndex` 非提供（`None`）のため
    // 従来の全行走査へ縮退し、正しい値を返すことを固定する。
    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should succeed with WHERE and no caches");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(3));
}

#[test]
fn execute_aggregate_returns_empty_set_contract_for_table_without_visible_rows() {
    // SQL-13 の空集合契約（`COUNT=0`・`SUM=NULL`）を確認するため、行を一切
    // 投入していないテーブルに対して集計を実行する。
    let path = unique_db_path("sql-aggregate-public-api-empty-table");
    let _guard = CleanupGuard(path.clone());

    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let validated = validate_sql("SELECT COUNT(*), SUM(id) FROM docs", &storage)
            .expect("validate_sql should accept aggregate form");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should succeed on a table without rows");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(0));
    assert_eq!(result.rows[0].cells[1], Cell::Null);
}
