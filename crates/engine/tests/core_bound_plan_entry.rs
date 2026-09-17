//! `EngineCore::execute_bound_scan_in_session`／`execute_bound_aggregate_in_session`
//! （TASK-186・NOSQL-3・NOSQL-4・NOSQL-5。Issue #728）が、単一の `Storage` を
//! `EngineCore` が所有したまま SQL テキストを経由せずに束縛済み scan／aggregate
//! 計画を実行できること、RLS（`PolicyContext`）を暗黙適用すること、
//! fail-closed に拒否すべき入力を拒否することを固定する結合テスト。
//!
//! `tests/sql_scan_public_api.rs`・`tests/sql_aggregate_public_api.rs` と異なり、
//! ここでは `Storage` を `EngineCore::from_storage` へそのまま渡して単一
//! `Storage` 構成を保つ（生 `redb::Database` の再オープンを使わない。本 Issue が
//! 固定したいのはまさに「単一 `Storage` 構成での実行」であるため）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::parser::{
    bind_aggregate, bind_scan, AggregateTarget, BoundAggregate, BoundAggregateItem, BoundScan,
};
use engine::sql::udf_call::{define_function, Expr};
use engine::storage::{Storage, Visibility};

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
/// 存在しないグループ値）を投入する（`tests/sql_aggregate_public_api.rs::seed_two_tenants`
/// と同じ判断。RLS 境界確認用）。
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

fn open_engine_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed_two_tenants(&storage);
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `validate_sql`（`bind_aggregate` 用の `Statement::Aggregate` を得るため）が
/// 要求する `TableLookup` の固定応答実装。`EngineCore::from_storage` が
/// `Storage` を所有してしまった後は `Storage`（`impl TableLookup`）に SQL
/// テキスト検証専用でアクセスする手段がないため、本テストではテーブル名が
/// `TABLE` と一致する場合のみ `Ok(true)` を返す最小実装で代替する
/// （束縛・実行そのものは `EngineCore` 側のスキーマ・redb 走査で行うため、
/// この固定応答は構文検証の入口としてのみ使う）。
struct FixedTableLookup;

impl TableLookup for FixedTableLookup {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}

#[test]
fn scan_entry_applies_rls_implicitly_with_single_storage() {
    let path = unique_db_path("bound-plan-scan-rls");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let result = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql = "SELECT id, lang FROM docs LIMIT 10";
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Scan(validated_scan) = validated else {
                panic!("expected Statement::Scan");
            };
            bind_scan(&validated_scan, schema, udfs)
        })
        .expect("execute_bound_scan_in_session should succeed");

    // tenant-a の可視行は id 1..=5 のみ（tenant-b の Private 行は非漏えい）。
    assert_eq!(result.columns.len(), 2);
    assert_eq!(result.rows.len(), 5);
    let mut ids: Vec<u64> = result
        .rows
        .iter()
        .map(|row| match row.cells[0] {
            Cell::Integer(id) => id,
            ref other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn scan_entry_rejects_undefined_table_before_invoking_binder() {
    let path = unique_db_path("bound-plan-scan-undefined-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, "no_such_table", |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for an undefined table");
        })
        .expect_err("undefined table should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(err, SqlSurfaceError::UndefinedTable { .. }));
}

/// PR #788 レビュー指摘（Issue #728）: `table` に識別子として形式不正な文字列
/// （`catalog::validate_identifier` が `CatalogError::Invalid` を返す入力。
/// ここでは英数字のみだが上限〔63 バイト〕超過の 64 バイト文字列を使う。
/// SQL テキスト経由〔`validate_sql` → `TableLookup::table_exists` →
/// `catalog::table_lookup_error`〕はこれを `wire_code` `42601`
/// （`UnsupportedSyntax`）へ分類するため、本エントリでも `Internal`（`XX000`）
/// ではなく同じ分類に丸め込まれることを固定する（`read_txn_with_schema` が
/// 単一の写像本体 `catalog::table_lookup_error` を共有する契約）。
#[test]
fn scan_entry_classifies_malformed_table_name_same_as_sql_path() {
    let path = unique_db_path("bound-plan-scan-malformed-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();
    let malformed_table = "a".repeat(64);

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, &malformed_table, |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for a malformed table name");
        })
        .expect_err("malformed table name should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }),
        "expected UnsupportedSyntax (42601), got {err:?}"
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn scan_entry_propagates_binder_error_unchanged() {
    let path = unique_db_path("bound-plan-scan-binder-error");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
            Err(SqlSurfaceError::InvalidInput {
                detail: "synthetic binder failure".to_string(),
            })
        })
        .expect_err("binder error should propagate unchanged");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

#[test]
fn scan_entry_rejects_bound_plan_for_another_table() {
    let path = unique_db_path("bound-plan-scan-table-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    // `table = "docs"` を要求しつつ、closure は無関係なテーブル名で束縛済み
    // 計画を返す（`BoundScan::new` の直接構築。TASK-186・NOSQL-3・Issue #726）。
    let err = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |_schema, _udfs| {
            Ok(BoundScan::new(
                "other_table".to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                10,
            ))
        })
        .expect_err("table mismatch should be rejected");

    assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
}

#[test]
fn aggregate_entry_matches_sql_path_and_shares_visible_bitmap_cache() {
    let path = unique_db_path("bound-plan-aggregate-cache");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let run_once = || {
        core.execute_bound_aggregate_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql = "SELECT COUNT(*) FROM docs";
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            bind_aggregate(&validated_aggregate, schema, udfs)
        })
        .expect("execute_bound_aggregate_in_session should succeed")
    };

    // 同じセッション経由の SQL 実行結果（`VisibleBitmapCache`・Issue #478 の
    // 対象クエリ形）と `Cell` レベルで一致することを固定する。
    let mut sql_session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(&ctx_a, &mut sql_session, "SELECT COUNT(*) FROM docs")
        .expect("execute_sql_in_session should succeed");
    let engine::sql::SqlOutcome::Query(sql_result) = sql_outcome else {
        panic!("expected SqlOutcome::Query");
    };

    let bound_result_1 = run_once();
    assert_eq!(bound_result_1.rows, sql_result.rows);
    assert_eq!(bound_result_1.rows[0].cells[0], Cell::Integer(5));

    // 2 回目の呼び出しで `VisibleBitmapCache` がヒットすること
    // （公開ラッパー `sql::aggregate::execute_aggregate`〔キャッシュ非経由〕との
    // 差別化。SQL 経路と同じキャッシュ配線を共有する証拠）。
    let _bound_result_2 = run_once();
    let stats = core.visible_bitmap_cache_stats();
    assert!(
        stats.hits >= 1,
        "expected at least one VisibleBitmapCache hit, got {stats:?}"
    );
}

/// `scan_entry_classifies_malformed_table_name_same_as_sql_path` と同じ判断
/// （PR #788 レビュー指摘・Issue #728）を `execute_bound_aggregate_in_session`
/// 側にも固定する。
#[test]
fn aggregate_entry_classifies_malformed_table_name_same_as_sql_path() {
    let path = unique_db_path("bound-plan-aggregate-malformed-table");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();
    let malformed_table = "a".repeat(64);

    let binder_called = std::sync::atomic::AtomicBool::new(false);
    let err = core
        .execute_bound_aggregate_in_session(&ctx_a, &session, &malformed_table, |_schema, _udfs| {
            binder_called.store(true, std::sync::atomic::Ordering::SeqCst);
            unreachable!("binder must not be invoked for a malformed table name");
        })
        .expect_err("malformed table name should be rejected before the binder runs");

    assert!(!binder_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }),
        "expected UnsupportedSyntax (42601), got {err:?}"
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn aggregate_entry_group_by_excludes_other_tenant_groups() {
    let path = unique_db_path("bound-plan-aggregate-group-by");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let result = core
        .execute_bound_aggregate_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY n DESC";
            let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            let bound = bind_aggregate(&validated_aggregate, schema, udfs)?;
            assert!(bound.has_group_by());
            Ok(bound)
        })
        .expect("execute_bound_aggregate_in_session should succeed for GROUP BY");

    assert_eq!(result.rows.len(), 2);
    let langs: Vec<String> = result
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(text) => text.clone(),
            other => panic!("expected Cell::Text for lang, got {other:?}"),
        })
        .collect();
    assert!(langs.contains(&"ja".to_string()));
    assert!(langs.contains(&"en".to_string()));
    // tenant-b 専有のグループ値（`"xx"`）は現れない（RLS-7・RLS-8）。
    assert!(!langs.contains(&"xx".to_string()));
}

#[test]
fn binder_receives_session_udf_registry() {
    let path = unique_db_path("bound-plan-scan-session-udf");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let mut session = SessionState::default();
    define_function(
        session.udfs_mut(),
        "answer",
        &[],
        &Expr::Number("42".to_string()),
    )
    .expect("define_function should succeed");

    let observed = std::sync::Mutex::new(false);
    let result = core.execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
        *observed.lock().expect("lock") = udfs.get("answer").is_some();
        let sql = "SELECT id FROM docs LIMIT 10";
        let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
        let Statement::Scan(validated_scan) = validated else {
            panic!("expected Statement::Scan");
        };
        bind_scan(&validated_scan, schema, udfs)
    });

    assert!(result.is_ok());
    assert!(
        *observed.lock().expect("lock"),
        "binder should observe the session's registered UDF"
    );
}

/// TASK-186・NOSQL-4（Issue #768）: `execute_bound_aggregate_in_session` の
/// binder closure が `BoundAggregate::new`（SQL テキスト非経由の直接構築）を
/// 返す形で、SQL テキスト経由（`bind_aggregate`）と `Cell` レベルで完全一致し
/// RLS（`ctx`）を暗黙適用することを固定する。`bound-plan-session-entry.md` が
/// 「TASK-177 へ申し送り」としていた `BoundAggregate::new` の到達性が、本 Issue
/// で初めて成立することの非 vacuous 証跡（`wire-server::http::query::aggregate`
/// が呼ぶ経路そのものを in-process で再現する）。
#[test]
fn aggregate_entry_accepts_binder_built_without_sql_text() {
    let path = unique_db_path("bound-plan-aggregate-no-sql-text");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_for("tenant-a");
    let session = SessionState::default();

    let bound_result = core
        .execute_bound_aggregate_in_session(&ctx_a, &session, TABLE, |schema, _udfs| {
            let items = vec![
                BoundAggregateItem::bind(
                    engine::sql::allowlist::AggregateFunc::Count,
                    AggregateTarget::Star,
                    schema,
                )?,
                BoundAggregateItem::bind(
                    engine::sql::allowlist::AggregateFunc::Min,
                    AggregateTarget::Column("lang".to_string()),
                    schema,
                )?,
            ];
            BoundAggregate::new(TABLE.to_string(), items, Vec::new(), Vec::new())
        })
        .expect("execute_bound_aggregate_in_session should succeed without SQL text");

    let mut sql_session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(
            &ctx_a,
            &mut sql_session,
            "SELECT COUNT(*), MIN(lang) FROM docs",
        )
        .expect("execute_sql_in_session should succeed");
    let engine::sql::SqlOutcome::Query(sql_result) = sql_outcome else {
        panic!("expected SqlOutcome::Query");
    };

    // tenant-a の可視行は 5 件（`lang` = "ja" 3 件・"en" 2 件）。tenant-b の
    // Private 行（`lang = "xx"`）が RLS 越しに混入していれば `MIN(lang)` は
    // 辞書順最小の `"en"` ではなく別値になる（RLS-7・RLS-8 相当の非漏えい確認）。
    assert_eq!(bound_result.rows, sql_result.rows);
    assert_eq!(bound_result.rows[0].cells[0], Cell::Integer(5));
    assert_eq!(bound_result.rows[0].cells[1], Cell::Text("en".to_string()));
}
