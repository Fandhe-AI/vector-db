//! 明示トランザクション `BEGIN`/`COMMIT`/`ROLLBACK`（SQL-31・TASK-221）の結合
//! テスト。ポインタ: `docs/spec/05-tasks.md` TASK-221・
//! `docs/spec/04-behavior/sql-surface.md` SQL-31・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `EngineCore::execute_sql_in_txn`（`sql::transaction::SessionTransaction` を
//! `&mut` で受け取るトランザクション対応入口）を production 経路として検証する。
//! `truncate_table.rs`・`insert_multi_row.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{TransactionLimits, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql31-transaction");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op_id}'"
    )
}

#[test]
fn begin_insert_insert_commit_makes_both_rows_visible() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    assert_eq!(txn.status(), TransactionStatus::Idle);

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin"),
        SqlOutcome::Begin
    );
    assert_eq!(txn.status(), TransactionStatus::InTransaction);

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert 1");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect("insert 2");

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
            .expect("commit"),
        SqlOutcome::Commit
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after commit");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn reading_a_table_already_written_in_the_same_transaction_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert");

    // 同一トランザクション内で既に書き込んだテーブルへの読み取りは、自分の
    // 未 commit 変更が黙って見えない・古い値が黙って返る、のいずれでもなく
    // fail-closed に `0A000` で拒否する（§2.5 の既知の逸脱。読み取りは
    // 「未書き込みテーブルのみ許可」）。
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect_err("read of a table already written in this txn is rejected");
    assert_eq!(err.wire_code(), "0A000");
    // 文実行中のエラーはトランザクション全体を Failed へ遷移させる
    // （部分書き込みを残さない fail-closed 契約）。
    assert_eq!(txn.status(), TransactionStatus::Failed);

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback"),
        SqlOutcome::Rollback
    );

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after rollback");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn rollback_discards_all_statements_in_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(10, "op-10"))
        .expect("insert");
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after rollback");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }

    // ROLLBACK 後に同じ operation_id で autocommit 再送すれば成功する
    // （台帳・行とも痕跡が残っていないことの証跡。RECOVER-12）。
    let outcome = engine
        .execute_sql_in_session(&caller, &mut session, &insert_sql(10, "op-10"))
        .expect("resend after rollback succeeds");
    assert!(matches!(outcome, SqlOutcome::Insert(_)));
}

#[test]
fn nested_begin_fails_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect_err("nested BEGIN is rejected");
    assert_eq!(err.wire_code(), "25001");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    // Failed 中は ROLLBACK 以外すべて 25P02。
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect_err("statements are rejected while failed");
    assert_eq!(err.wire_code(), "25P02");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit is rejected while failed");
    assert_eq!(err.wire_code(), "25P02");

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback recovers from failed"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
fn commit_or_rollback_without_begin_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit without begin");
    assert_eq!(err.wire_code(), "25P01");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect_err("rollback without begin");
    assert_eq!(err.wire_code(), "25P01");
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
fn reusing_operation_id_within_the_same_transaction_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "dup"))
        .expect("first insert with op id");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "dup"))
        .expect_err("reusing the same operation_id in the same txn is rejected");
    assert_eq!(err.wire_code(), "25000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

#[test]
fn unsupported_statement_inside_transaction_is_rejected_with_feature_not_supported() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    // 単一行 `DELETE` は明示トランザクション内では未対応（対象外）。
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'del-1'"),
        )
        .expect_err("DELETE inside a transaction is not yet supported");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

#[test]
fn statement_count_limit_fails_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let engine = engine.with_transaction_limits(TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: 1,
    });
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("first statement is within the limit");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect_err("second statement exceeds max_statements");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

#[test]
fn truncate_inside_transaction_is_committed_atomically_with_inserts() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");

    // 事前に 1 行投入しておく（TRUNCATE の対象があることを確認するため）。
    engine
        .execute_sql_in_session(&caller, &mut SessionState::default(), &insert_sql(1, "pre"))
        .expect("seed row");

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'trunc-1'"),
        )
        .expect("truncate");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect("insert after truncate");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after commit");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(
            result.rows.len(),
            1,
            "id=1 は TRUNCATE で消え id=2 のみ残る"
        ),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn dropping_an_active_transaction_without_commit_leaves_no_trace() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    {
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "drop-1"))
            .expect("insert");
        // `txn` はここで drop される（接続断に相当。commit しない）。
    }

    let outcome = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select after drop");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }
}
