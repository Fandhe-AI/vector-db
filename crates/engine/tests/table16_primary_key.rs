//! `PRIMARY KEY` 宣言構文（単一列・複合キー。TABLE-16・TASK-204、Issue #903）の
//! 結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-204・
//! `docs/spec/04-behavior/table-model.md` TABLE-16・
//! `docs/spec/04-behavior/rls.md` RLS-10 (c)・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_create_table.rs` と同じ流儀（`EngineCore::execute_sql_in_session`・
//! 実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）で、
//! `CREATE TABLE` の列制約・表制約両形の `PRIMARY KEY` 構文、テナント内
//! 一意性制約の検査点（`constraint::enforce_primary_key_in_txn`）を production
//! 経路（SQL 表層）から検証する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant")
}

fn granted_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

// --- 構文の受理（単一列・複合キー） -------------------------------------

#[test]
fn create_table_accepts_single_column_primary_key_via_column_constraint() {
    let (core, path) = new_core("pk-single-column-constraint");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
        )
        .expect("CREATE TABLE with a column-constraint PRIMARY KEY should succeed");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));

    let schema = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 1")
        .map(|_| ())
        .is_ok();
    assert!(schema, "table must be queryable immediately");
}

#[test]
fn create_table_accepts_composite_primary_key_via_table_constraint() {
    let (core, path) = new_core("pk-composite-table-constraint");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), tenant_code TEXT, region TEXT, PRIMARY KEY (tenant_code, region))",
        )
        .expect("CREATE TABLE with a composite PRIMARY KEY should succeed");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));
}

#[test]
fn create_table_accepts_id_only_primary_key_as_a_no_op() {
    let (core, path) = new_core("pk-id-only-noop");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), body TEXT, PRIMARY KEY (id))",
    )
    .expect(
        "PRIMARY KEY (id) must be accepted as an explicit declaration of the implicit primary key",
    );

    // 主キー未宣言と完全に同じ挙動（`id` の物理キー衝突のみが検査される）で
    // あることを、同一 `id` の再挿入が `23505` になることで確認する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, body) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert should succeed");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, body) VALUES (1, '[0.5,0.6,0.7,0.8]', 'b') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate id must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

// --- 拒否系（構造検証段階。42601／42701／54000） -------------------------

#[test]
fn create_table_rejects_multiple_primary_key_declarations() {
    let (core, path) = new_core("pk-multiple-declarations");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT PRIMARY KEY, b TEXT PRIMARY KEY)",
        )
        .expect_err("two column-constraint PRIMARY KEY declarations must be rejected");
    assert_eq!(err.wire_code(), "42601");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs2 (a TEXT PRIMARY KEY, b TEXT, PRIMARY KEY (b))",
        )
        .expect_err("a column-constraint and a table-constraint PRIMARY KEY must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_empty_primary_key_column_list() {
    let (core, path) = new_core("pk-empty-list");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT, PRIMARY KEY ())",
        )
        .expect_err("PRIMARY KEY () must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_primary_key_referencing_unknown_column() {
    let (core, path) = new_core("pk-unknown-column");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT, PRIMARY KEY (missing))",
        )
        .expect_err("PRIMARY KEY referencing an unknown column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_vector_column_as_primary_key() {
    let (core, path) = new_core("pk-vector-column");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4) PRIMARY KEY)",
        )
        .expect_err("VECTOR column must not be usable as a primary key");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_id_mixed_with_other_columns_in_primary_key() {
    let (core, path) = new_core("pk-id-mixed");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (code TEXT, PRIMARY KEY (id, code))",
        )
        .expect_err("mixing the implicit id column with other columns must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_duplicate_column_name_within_primary_key() {
    let (core, path) = new_core("pk-duplicate-in-list");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (code TEXT, PRIMARY KEY (code, code))",
        )
        .expect_err("duplicate column name within PRIMARY KEY must be rejected");
    assert_eq!(err.wire_code(), "42701");
}

// --- テナント内一意性制約（書き込み経路。23505） -------------------------

#[test]
fn insert_rejects_duplicate_primary_key_value_within_the_same_tenant() {
    let (core, path) = new_core("pk-write-duplicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'dup') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert should succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, code) VALUES (2, '[0.5,0.6,0.7,0.8]', 'dup') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate primary key value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: 拒否された `operation_id` は台帳に残らない（内容を変えた
    // 再送は通常どおり成功する）。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (2, '[0.5,0.6,0.7,0.8]', 'not-dup') USING OPERATION_ID 'op-2'",
    )
    .expect("retry with a fixed value under the same operation_id must succeed");
}

#[test]
fn insert_rejects_duplicate_composite_primary_key_value() {
    let (core, path) = new_core("pk-write-composite-duplicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), a TEXT, b TEXT, PRIMARY KEY (a, b))",
    )
    .expect("CREATE TABLE should succeed");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, a, b) VALUES (1, '[0.1,0.2,0.3,0.4]', 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert should succeed");

    // 片方の列だけ一致する行は成功する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, a, b) VALUES (2, '[0.1,0.2,0.3,0.4]', 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect("differing in one column must succeed");

    // 両方一致する行は拒否される。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, a, b) VALUES (3, '[0.1,0.2,0.3,0.4]', 'x', 'y') USING OPERATION_ID 'op-3'",
        )
        .expect_err("matching both columns must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn update_rejects_change_that_would_create_a_duplicate_primary_key_value() {
    let (core, path) = new_core("pk-write-update-duplicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1 should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (2, '[0.5,0.6,0.7,0.8]', 'b') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 should succeed");

    // id=2 の code を 'a' へ変更しようとすると id=1 と衝突する。
    let err = core
        .execute_update_sql(
            &alice,
            "UPDATE docs SET code = 'a' WHERE id = 2 USING OPERATION_ID 'op-update'",
        )
        .expect_err("update that creates a duplicate primary key value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 自己更新（値を変えない SET）は自己衝突しない。
    core.execute_update_sql(
        &alice,
        "UPDATE docs SET code = 'a' WHERE id = 1 USING OPERATION_ID 'op-self-update'",
    )
    .expect("updating a row to its own existing primary key value must succeed");
}

// --- テナント境界（RLS-9・RLS-10 (c)） -----------------------------------

#[test]
fn different_tenants_may_share_the_same_primary_key_value() {
    let (core, path) = new_core("pk-cross-tenant");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'shared') USING OPERATION_ID 'op-a'",
    )
    .expect("tenant alice insert should succeed");
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'shared') USING OPERATION_ID 'op-b'",
    )
    .expect("tenant bob must be able to use the same primary key value as tenant alice");
}
