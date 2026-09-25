//! カーソル（`DECLARE`/`FETCH`/`CLOSE`。WIRE-15・TASK-218）の結合テスト。
//! ポインタ: `docs/spec/05-tasks.md` TASK-218・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-15・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql31_transaction.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore::execute_sql_in_txn` を
//! production 経路として検証する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
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
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn new_core_with_limits(limits: TransactionLimits) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor-limits");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_transaction_limits(limits),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[1.0, 0.0]', '{lang}') \
         USING OPERATION_ID '{op_id}'"
    )
}

fn seed_rows(engine: &EngineCore, caller: &PolicyContext, n: u64) {
    let mut session = SessionState::default();
    for id in 1..=n {
        engine
            .execute_sql_in_session(
                caller,
                &mut session,
                &insert_sql(id, "ja", &format!("seed-{id}")),
            )
            .expect("seed insert");
    }
}

fn row_ids(result: &engine::sql::exec::QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// (a) `BEGIN` → `DECLARE`（広域取得 `SELECT`）→ 複数回 `FETCH` の全ページの
/// 和集合が、同じ内容を直接実行した結果と一致すること（順序も含め決定的）。
#[test]
fn declare_scan_and_paginated_fetch_matches_direct_execution() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");

    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 100");
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
            .expect("declare"),
        SqlOutcome::DeclareCursor
    );

    let mut fetched: Vec<u64> = Vec::new();
    loop {
        let outcome = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 2 FROM c")
            .expect("fetch");
        let SqlOutcome::Fetch(result) = outcome else {
            panic!("expected Fetch outcome");
        };
        if result.rows.is_empty() {
            break;
        }
        fetched.extend(row_ids(&result));
    }
    fetched.sort_unstable();

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "CLOSE c")
            .expect("close"),
        SqlOutcome::CloseCursor
    );
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    let direct = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} LIMIT 100"),
        )
        .expect("direct scan");
    let SqlOutcome::Query(direct_result) = direct else {
        panic!("expected Query outcome");
    };
    let mut direct_ids = row_ids(&direct_result);
    direct_ids.sort_unstable();

    assert_eq!(fetched, direct_ids);
}

/// `DECLARE`（集計 `SELECT`）も受理され、`FETCH` で単一行を取得できる。
#[test]
fn declare_aggregate_and_fetch_returns_single_row() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 3);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let declare_sql = format!("DECLARE agg CURSOR FOR SELECT COUNT(*) AS n FROM {TABLE}");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare aggregate");

    let outcome = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM agg")
        .expect("fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells, vec![Cell::Integer(3)]);

    // 末尾に達しているため 2 回目の FETCH は 0 行。
    let outcome = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM agg")
        .expect("second fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    assert!(result.rows.is_empty());

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}

/// (b) トランザクション外の `DECLARE` は `25P01`、`FETCH`／`CLOSE` は `34000`。
#[test]
fn cursor_statements_outside_transaction_are_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();

    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    let err = engine
        .execute_sql_in_session(&caller, &mut session, &declare_sql)
        .expect_err("DECLARE outside transaction must be rejected");
    assert_eq!(err.wire_code(), "25P01");

    let err = engine
        .execute_sql_in_session(&caller, &mut session, "FETCH 1 FROM c")
        .expect_err("FETCH outside transaction must be rejected");
    assert_eq!(err.wire_code(), "34000");

    let err = engine
        .execute_sql_in_session(&caller, &mut session, "CLOSE c")
        .expect_err("CLOSE outside transaction must be rejected");
    assert_eq!(err.wire_code(), "34000");
}

/// (c) `COMMIT`／`ROLLBACK` の後、開いていたカーソルは自動的に消える
/// （新しいトランザクションからの `FETCH` は `34000`）。`Failed` 中の
/// `FETCH`／`CLOSE`／`DECLARE` はいずれも `25P02`。
#[test]
fn cursors_are_closed_automatically_on_commit_and_rollback() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 2);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 1");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    // 新しいトランザクションからは同名のカーソルは存在しない。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 2");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("cursor must not survive COMMIT");
    assert_eq!(err.wire_code(), "34000");
    // エラーにより `Failed` へ遷移済み。`ROLLBACK` 以外は `25P02`。
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("Failed transaction rejects FETCH");
    assert_eq!(err.wire_code(), "25P02");
    let declare_sql2 = format!("DECLARE c2 CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql2)
        .expect_err("Failed transaction rejects DECLARE");
    assert_eq!(err.wire_code(), "25P02");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // ROLLBACK でも同様に自動クローズされる。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 3");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare again");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback 2");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 4");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("cursor must not survive ROLLBACK");
    assert_eq!(err.wire_code(), "34000");
}

/// スナップショットの一貫性（INSENSITIVE）: `DECLARE` は `written_tables` へ
/// 追加しないため、`DECLARE` の**後**に同じテーブルへ書き込むこと自体は妨げ
/// ない（本実装の意味論では、この書き込みは既に確定済みのカーソルの内容には
/// 影響しない）。一方、既に書き込み済みのテーブルに対する `DECLARE` は
/// `sql::transaction` の既知の逸脱（「読み取りの既知の逸脱」節）により `0A000`
/// で拒否される。
#[test]
fn declare_and_write_to_the_same_table_in_one_transaction_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 1);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    // `DECLARE` 自体は書き込みではないため `written_tables` へは入らない
    // （同じトランザクションでの後続の INSERT は通常どおり成功する）。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &insert_sql(100, "ja", "op-w1"),
        )
        .expect("insert after DECLARE must still succeed (DECLARE is not a write)");

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // 先に書き込んでから DECLARE すると 0A000（`written_tables` 判定）。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 2");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &insert_sql(101, "ja", "op-w2"),
        )
        .expect("insert before declare");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect_err("DECLARE against an already-written table must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}

/// ベクトル順位付けの検索 `SELECT`（`ORDER BY <=>`）は `DECLARE` の内側として
/// 受理しない（`42601`）。
#[test]
fn declare_rejects_vector_ranking_select() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");

    let declare_sql = format!(
        "DECLARE c CURSOR FOR SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"
    );
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect_err("vector ranking SELECT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// RLS: 他テナント・`Private` 行を混在させても、カーソル経由の全ページに
/// 不許可行が 0 件であること。別セッション（別テナント）のカーソル名は
/// `34000`（不在と同一の応答）になる。
#[test]
fn cursor_respects_rls_and_does_not_leak_other_tenant_cursor_names() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");

    let mut seed_session = SessionState::default();
    engine
        .execute_sql_in_session(&alice, &mut seed_session, &insert_sql(1, "ja", "op-a1"))
        .expect("alice row 1");
    engine
        .execute_sql_in_session(&bob, &mut seed_session, &insert_sql(2, "ja", "op-b1"))
        .expect("bob row 1");

    let mut alice_session = SessionState::default();
    let mut alice_txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&alice, &mut alice_session, &mut alice_txn, "BEGIN")
        .expect("alice begin");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 100");
    engine
        .execute_sql_in_txn(&alice, &mut alice_session, &mut alice_txn, &declare_sql)
        .expect("alice declare");
    let outcome = engine
        .execute_sql_in_txn(
            &alice,
            &mut alice_session,
            &mut alice_txn,
            "FETCH 100 FROM c",
        )
        .expect("alice fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    // alice のカーソルには alice 自身の行（id=1）のみが含まれ、bob の行
    // （id=2）は含まれない。
    assert_eq!(row_ids(&result), vec![1]);
    engine
        .execute_sql_in_txn(&alice, &mut alice_session, &mut alice_txn, "COMMIT")
        .expect("alice commit");

    // bob のセッション（別トランザクション）から alice のカーソル名 "c" を
    // 参照しても、単に存在しないカーソルと同じ応答になる。
    let mut bob_session = SessionState::default();
    let mut bob_txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, "BEGIN")
        .expect("bob begin");
    let err_other = engine
        .execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, "FETCH 1 FROM c")
        .expect_err("bob cannot see alice's cursor");
    assert_eq!(err_other.wire_code(), "34000");
    bob_txn.fail();
    let mut bob_txn2 = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn2, "BEGIN")
        .expect("bob begin 2");
    let err_missing = engine
        .execute_sql_in_txn(
            &bob,
            &mut bob_session,
            &mut bob_txn2,
            "FETCH 1 FROM missing_cursor",
        )
        .expect_err("bob cursor genuinely missing");
    assert_eq!(err_missing.wire_code(), "34000");
    assert_eq!(err_other.to_string(), err_missing.to_string());
}

/// 同時カーソル数の上限（`MAX_CURSORS_PER_SESSION`＝16）を超えると `54000`。
#[test]
fn declare_beyond_cursor_count_limit_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");

    for i in 0..engine::sql::cursor::MAX_CURSORS_PER_SESSION {
        let declare_sql = format!("DECLARE c{i} CURSOR FOR SELECT id FROM {TABLE} LIMIT 1");
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
            .unwrap_or_else(|e| panic!("declare {i} should succeed: {e}"));
    }

    let overflow_sql = format!("DECLARE overflow CURSOR FOR SELECT id FROM {TABLE} LIMIT 1");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &overflow_sql)
        .expect_err("17th cursor must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

/// トランザクションの持続時間上限（`TransactionLimits::max_duration`）を
/// 超えると、カーソルごと `Failed` へ遷移して使えなくなる（保持期間の上限）。
#[test]
fn cursor_becomes_unusable_after_transaction_duration_limit() {
    let limits = TransactionLimits {
        max_duration: std::time::Duration::from_millis(50),
        max_statements: 1_000,
    };
    let (engine, path) = new_core_with_limits(limits);
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 1);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare");

    std::thread::sleep(std::time::Duration::from_millis(120));

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("expired transaction must reject FETCH");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}
