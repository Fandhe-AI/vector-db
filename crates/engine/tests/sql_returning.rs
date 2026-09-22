//! `INSERT`／`DELETE`（単一行）の `RETURNING` 句（Issue #873・SQL-21）の結合
//! テスト。ポインタ: `docs/spec/05-tasks.md` TASK-193・
//! `docs/spec/04-behavior/sql-surface.md` SQL-21。関連ポインタ: SQL-10・
//! SQL-18・RLS-7・RLS-9・RLS-10・RECOVER-1〜3・RECOVER-10。
//!
//! `EngineCore::execute_sql_in_session` の INSERT／DELETE 分岐が
//! `stmt.returning.is_some()` で `SqlOutcome::Returning` へ切り替わる経路を
//! production 経路として検証する。`sql_delete_single_row.rs`・
//! `insert_multi_row.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）でヘルパを共有する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::returning::DmlCommand;
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
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core_with_table() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-returning");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn count_star(core: &EngineCore, ctx: &PolicyContext, table: &str) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {table}"))
        .expect("count(*) should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn expect_returning(outcome: SqlOutcome) -> engine::sql::exec::ReturningOutcome {
    match outcome {
        SqlOutcome::Returning(o) => o,
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
}

// --- INSERT ... RETURNING ---

/// `INSERT ... RETURNING *` の投影列・行内容が、同一行を `SELECT * ... LIMIT 1`
/// で読み戻した結果と完全一致すること（列メタ・`Cell::Vector` を含む）。
/// `rows_affected == result.rows.len() == 1`。
#[test]
fn insert_returning_star_matches_select_readback() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING * \
                 USING OPERATION_ID 'op-insert-returning'"
            ),
        )
        .expect("INSERT RETURNING should succeed"),
    );

    assert_eq!(outcome.command, DmlCommand::Insert);
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);

    let select = core
        .execute_sql(&alice, &format!("SELECT * FROM {TABLE} LIMIT 10"))
        .expect("select readback");
    assert_eq!(select.rows.len(), 1);
    assert_eq!(outcome.result.columns, select.columns);
    assert_eq!(outcome.result.rows[0].id, select.rows[0].id);
    assert_eq!(outcome.result.rows[0].cells, select.rows[0].cells);
}

/// RETURNING の `RowDescription`（列メタ）は `RETURNING` に列挙した投影
/// （`id`・`body`）のみで、投影に含めなかった列は現れない。
#[test]
fn insert_returning_projects_only_listed_columns() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING id, body \
                 USING OPERATION_ID 'op-insert-cols'"
            ),
        )
        .expect("INSERT RETURNING should succeed"),
    );

    assert_eq!(
        outcome.result.columns,
        vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: ColumnType::Text,
            },
        ]
    );
    assert_eq!(outcome.result.rows[0].cells.len(), 2);
    assert_eq!(outcome.result.rows[0].cells[0], Cell::Integer(1));
    assert_eq!(
        outcome.result.rows[0].cells[1],
        Cell::Text("hello".to_string())
    );
}

/// RLS 再判定（多層防御）: `Public` のみ可視な `PolicyContext` で `RETURNING`
/// した場合、挿入行（常に `Visibility::Private` 固定）は投影行から除外
/// されるが、`rows_affected` は実際に書き込んだ件数（`1`）のまま変えない
/// （`ReturningOutcome` ドキュメント参照）。
#[test]
fn insert_returning_rows_affected_is_independent_of_result_row_visibility() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice_public_only = ctx_for("alice", false);
    let mut session = SessionState::default();

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice_public_only,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING * \
                 USING OPERATION_ID 'op-insert-public-only'"
            ),
        )
        .expect("INSERT RETURNING should succeed even when the result row is not visible"),
    );

    assert_eq!(outcome.rows_affected, 1);
    assert!(
        outcome.result.rows.is_empty(),
        "Private row must not be visible under a Public-only PolicyContext"
    );

    // 行自体は書き込まれている（別 ctx で確認）。
    let alice_private = ctx_for("alice", true);
    assert_eq!(count_star(&core, &alice_private, TABLE), 1);
}

/// 複数行 `VALUES ... RETURNING *`（SQL-16 との併用）: `rows_affected` ・
/// 投影行数が挿入行数と一致し、投影順が `VALUES` の宣言順と一致する。
#[test]
fn insert_returning_multi_row_preserves_values_order() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'first'), (2, '[0.3,0.4]', 'ja', 'second') \
                 RETURNING id, body USING OPERATION_ID 'op-insert-multi'"
            ),
        )
        .expect("multi-row INSERT RETURNING should succeed"),
    );

    assert_eq!(outcome.rows_affected, 2);
    assert_eq!(outcome.result.rows.len(), 2);
    assert_eq!(outcome.result.rows[0].id, 1);
    assert_eq!(
        outcome.result.rows[0].cells[1],
        Cell::Text("first".to_string())
    );
    assert_eq!(outcome.result.rows[1].id, 2);
    assert_eq!(
        outcome.result.rows[1].cells[1],
        Cell::Text("second".to_string())
    );
}

/// ファイル形 `INSERT`（`path`/`body` 列指定）＋ `RETURNING` は `42601`
/// （サーバー側チャンク化行を返す応答形が未定義のため fail-closed）。行は
/// 一切書き込まれない。
#[test]
fn insert_returning_file_form_is_rejected_and_writes_no_rows() {
    let path_file = unique_db_path("sql-returning-file-form");
    let storage = Storage::open(&path_file).expect("open storage");
    let file_schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    storage.create_table(&file_schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _guard = CleanupGuard(path_file);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "INSERT INTO docs (path, body) VALUES ('a.txt', 'hello world') \
             RETURNING * USING OPERATION_ID 'op-file-returning'",
        )
        .expect_err("file-form INSERT RETURNING must be rejected");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(count_star(&core, &alice, "docs"), 0);
}

/// 非セッション入口（`execute_insert_sql`）は `RETURNING` 付き文を検証直後・
/// 書き込み前に `42601` で拒否し、台帳を一切消費しない——同一 `operation_id`
/// をその後セッション経由（`RETURNING` なし）で使うと成功する。
#[test]
fn insert_returning_is_rejected_on_nonsession_entry_without_consuming_the_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);

    let err = core
        .execute_insert_sql(
            &alice,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING * \
                 USING OPERATION_ID 'op-nonsession'"
            ),
        )
        .expect_err("RETURNING must be rejected on the session-less entry point");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    // 同一 operation_id をセッション経由・RETURNING なしで再送すると成功する
    // （台帳が未消費であることの証拠）。
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-nonsession'"
            ),
        )
        .expect("same operation_id must still be usable (ledger was not consumed)");
    match outcome {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Insert, got {other:?}"),
    }
}

/// 内容照合ハッシュ（TASK-101・RECOVER-10）は SQL テキストではなく符号化済み
/// 行・`id` から計算するため `RETURNING` の有無に依存しない: 同一
/// `operation_id`・同一内容で「RETURNING あり → なし」の順に送ると 2 回目は
/// `23505`（逆順も同様）。
#[test]
fn insert_returning_content_hash_is_independent_of_returning_clause() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let values = "(1, '[0.1,0.2]', 'ja', 'hello')";
    let with_returning = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES {values} \
         RETURNING * USING OPERATION_ID 'op-content-hash'"
    );
    let without_returning = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES {values} \
         USING OPERATION_ID 'op-content-hash'"
    );

    core.execute_sql_in_session(&alice, &mut session, &with_returning)
        .expect("first RETURNING insert should succeed");
    let err = core
        .execute_sql_in_session(&alice, &mut session, &without_returning)
        .expect_err("resend without RETURNING must be detected as a duplicate");
    assert_eq!(err.wire_code(), "23505");
}

// --- DELETE ... RETURNING ---

/// `DELETE ... RETURNING *` は削除**前**の値を返し、直後の `SELECT` では
/// 対象行が見つからない（削除は実際に完了している）。
#[test]
fn delete_returning_returns_pre_delete_values_and_removes_the_row() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert");

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 RETURNING * USING OPERATION_ID 'op-delete-returning'"),
        )
        .expect("DELETE RETURNING should succeed"),
    );

    assert_eq!(outcome.command, DmlCommand::Delete);
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);
    assert_eq!(outcome.result.rows[0].id, 1);
    assert_eq!(
        outcome.result.rows[0].cells,
        vec![
            Cell::Integer(1),
            Cell::Vector(vec![0.1, 0.2]),
            Cell::Text("ja".to_string()),
            Cell::Text("hello".to_string()),
        ]
    );

    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// 0 行成功（RLS-9・RLS-10）: 他テナント保持 id・未存在 id への
/// `DELETE ... RETURNING` は、`ReturningOutcome`（列・0 行・`rows_affected: 0`）
/// が完全に一致し、対象行の有無（他テナント所有か未存在か）を区別しない。
#[test]
fn delete_returning_notfound_response_is_identical_for_other_tenant_and_nonexistent_id() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &bob,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (7, '[0.1,0.2]', 'ja', 'bob body') USING OPERATION_ID 'op-seed-bob'"
        ),
    )
    .expect("seed insert for bob");

    let outcome_other_tenant = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE id = 7 RETURNING * USING OPERATION_ID 'op-other-tenant'"
            ),
        )
        .expect("DELETE against another tenant's row must succeed as a 0-row no-op"),
    );
    let outcome_nonexistent = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 999 RETURNING * USING OPERATION_ID 'op-nonexistent'"),
        )
        .expect("DELETE against a nonexistent row must succeed as a 0-row no-op"),
    );

    assert_eq!(outcome_other_tenant.rows_affected, 0);
    assert_eq!(outcome_nonexistent.rows_affected, 0);
    assert!(outcome_other_tenant.result.rows.is_empty());
    assert!(outcome_nonexistent.result.rows.is_empty());
    assert_eq!(outcome_other_tenant.result, outcome_nonexistent.result);
    assert_eq!(outcome_other_tenant.command, outcome_nonexistent.command);

    // bob の行は無傷。
    assert_eq!(count_star(&core, &bob, TABLE), 1);
}

/// 非セッション入口（`execute_delete_sql`）は `RETURNING` 付き文を検証直後・
/// 書き込み前に `42601` で拒否し、台帳を一切消費しない。
#[test]
fn delete_returning_is_rejected_on_nonsession_entry_without_consuming_the_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert");

    let err = core
        .execute_delete_sql(
            &alice,
            &format!("DELETE FROM {TABLE} WHERE id = 1 RETURNING * USING OPERATION_ID 'op-delete-nonsession'"),
        )
        .expect_err("RETURNING must be rejected on the session-less entry point");
    assert_eq!(err.wire_code(), "42601");
    // 行は削除されていない。
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    // 同一 operation_id をセッション経由・RETURNING なしで再送すると成功する
    // （台帳が未消費であることの証拠）。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-delete-nonsession'"),
        )
        .expect("same operation_id must still be usable (ledger was not consumed)");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// codex-review Low 指摘（Issue #873）: `RETURNING` 付き複数行 INSERT
// （`execute_insert_returning_form` の `RowBatch` 分岐）が、非 `RETURNING`
// 経路（`insert_multi_row.rs`）と同一の `self.batch_limits`（INDEX-4・
// `EngineCore::validate_insert_row_batch_limits`）を共有していることを固定する。
// ---------------------------------------------------------------------

/// `RETURNING` 付き複数行 `INSERT ... VALUES` が、運用者が絞った
/// `self.batch_limits.max_files_per_batch`（ここでは 2）を超える行数（3 行）
/// で `54000` 拒否され、行が一切書き込まれないこと（
/// `insert_multi_row.rs::multi_row_insert_over_batch_limits_row_count_is_rejected_with_54000`
/// の `RETURNING` 版。`execute_insert_returning_form` の `RowBatch` 分岐が
/// 非 `RETURNING` 経路と同一の判定本体〔`validate_insert_row_batch_limits`〕を
/// 共有していることの直接証拠）。
#[test]
fn insert_returning_multi_row_over_batch_limits_row_count_is_rejected_with_54000() {
    let path = unique_db_path("sql-returning-batch-limits-row-count");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(
        engine::batch_limits::BatchLimits {
            max_files_per_batch: 2,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'first'), (2, '[0.3,0.4]', 'ja', 'second'), \
                 (3, '[0.5,0.6]', 'ja', 'third') \
                 RETURNING id USING OPERATION_ID 'op-insert-returning-batch-limit'"
            ),
        )
        .expect_err("row count over batch_limits.max_files_per_batch must be rejected");
    assert_eq!(err.wire_code(), "54000");

    // 行は一切書き込まれていない（副作用ゼロ）。
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}
