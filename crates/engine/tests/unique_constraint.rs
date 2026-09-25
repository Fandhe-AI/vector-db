//! UNIQUE 制約（TABLE-16・TASK-204、Issue #905）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-204・`docs/spec/04-behavior/data-model.md`
//! TABLE-16・TABLE-12・`docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)。
//!
//! `sql_create_table.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）。SQL 表層（`CREATE TABLE ... UNIQUE`・
//! `INSERT`・`UPDATE`・`UPSERT`）経由の検証を主とし、Rust API（`tenant::` の
//! 生 `RowInput` 経路）・`Storage::alter_table_add_unique_constraint` は
//! 単一検査点であることの確認のみ最小限行う。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
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

// --- CREATE TABLE 構文（列制約・表制約） -------------------------------

#[test]
fn create_table_accepts_column_level_and_table_level_unique() {
    let (core, path) = new_core("uniq-create-ok");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT, c TEXT, UNIQUE (b, c))",
    )
    .expect("CREATE TABLE with column and table UNIQUE constraints must succeed");
}

#[test]
fn create_table_rejects_unique_on_undeclared_column() {
    let (core, path) = new_core("uniq-create-undeclared");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT, UNIQUE (z))",
        )
        .expect_err("UNIQUE referencing an undeclared column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_unique_on_vector_column() {
    let (core, path) = new_core("uniq-create-vector");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4) UNIQUE)",
        )
        .expect_err("UNIQUE on a VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- INSERT: 単一列・複合列・NULL・テナントスコープ ---------------------

#[test]
fn insert_rejects_duplicate_single_column_and_allows_distinct_values() {
    let (core, path) = new_core("uniq-insert-single");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (3, 'y') USING OPERATION_ID 'op-3'",
    )
    .expect("distinct value must succeed");
}

#[test]
fn insert_allows_multiple_null_rows_for_unique_column() {
    let (core, path) = new_core("uniq-insert-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (1) USING OPERATION_ID 'op-1'",
    )
    .expect("first NULL row must succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (2) USING OPERATION_ID 'op-2'",
    )
    .expect("second NULL row must also succeed (NULLS DISTINCT)");
}

#[test]
fn insert_composite_unique_requires_all_columns_to_match() {
    let (core, path) = new_core("uniq-insert-composite");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (b TEXT, c TEXT, UNIQUE (b, c))",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    // 片方だけ一致では違反にならない。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect("partial match must succeed");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, b, c) VALUES (3, 'x', 'y') USING OPERATION_ID 'op-3'",
        )
        .expect_err("full match must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn insert_uniqueness_is_scoped_per_tenant() {
    let (core, path) = new_core("uniq-insert-tenant-scope");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-alice'",
    )
    .expect("alice insert must succeed");
    // 他テナントが同じ値を保持していても成功する（RLS-9）。
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-bob'",
    )
    .expect("bob insert of the same value under a different tenant must succeed");
}

#[test]
fn insert_uniqueness_includes_private_rows_not_just_visible_ones() {
    // 母集合はテナント所有の全行（Public/Private を問わない）であり、可視
    // スナップショットではないことを固定する。書き込みセッション自身は常に
    // 自分の書いた行を書けるため、Private 可視性を持たない別コンテキストで
    // 同じテナントとして重複挿入を試みて確認する。
    let (core, path) = new_core("uniq-insert-private-scope");
    let _guard = CleanupGuard(path);
    let alice_all =
        PolicyContext::with_visibilities("alice", [engine::storage::Visibility::Private])
            .expect("valid tenant");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice_all,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice_all,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert (private) must succeed");
    let err = core
        .execute_insert_sql(
            &alice_all,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate against a private row must still be rejected");
    assert_eq!(err.wire_code(), "23505");
}

// --- バッチ内重複・台帳優先 ---------------------------------------------

#[test]
fn multi_row_insert_batch_rejects_internal_duplicate_with_no_partial_effect() {
    let (core, path) = new_core("uniq-insert-batch");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'x'), (2, 'x') USING OPERATION_ID 'op-batch'",
        )
        .expect_err("batch-internal duplicate must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: id=1 も反映されていない。
    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert!(rows.is_empty(), "no row must have been written");
}

#[test]
fn ledger_duplicate_operation_id_takes_priority_over_unique_violation() {
    // 同一 operation_id・同一内容の再送は、値そのものが重複していても
    // 台帳由来の 23505（DuplicateOperationId）として確定済み処理の再送を示す
    // 契約を維持する（TASK-101・RECOVER-10 の優先順位。UNIQUE 制約導入後も
    // 台帳照合が一意性検査より先に走ることを固定する）。
    let (core, path) = new_core("uniq-ledger-priority");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    // 同一 operation_id・同一内容の再送は台帳由来の 23505
    // （`DuplicateOperationId`。commit 済み確定の根拠）として拒否される。
    // 台帳照合が一意性検査より先に走るため、値そのものが UNIQUE 制約と
    // 衝突していても `UniqueConstraintViolation` 経由の別分類には化けない
    // （両者は現行アーキテクチャでは同一 wire_code `23505` を共有する）。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
        )
        .expect_err("identical resend must be classified as a ledger duplicate (23505)");
    assert_eq!(err.wire_code(), "23505");
}

// --- UPDATE / UPSERT ----------------------------------------------------

#[test]
fn update_rejects_conflicting_value_but_allows_self_assignment() {
    let (core, path) = new_core("uniq-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2");

    let err = core
        .execute_update_sql(
            &alice,
            "UPDATE docs SET a = 'x' WHERE id = 2 USING OPERATION_ID 'op-upd-1'",
        )
        .expect_err("updating to another row's value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 自身の現在値へ再度 SET するのは成功する（自己比較を除外する）。
    core.execute_update_sql(
        &alice,
        "UPDATE docs SET a = 'y' WHERE id = 2 USING OPERATION_ID 'op-upd-2'",
    )
    .expect("re-assigning the current value must succeed");
}

#[test]
fn upsert_do_update_rejects_conflicting_value() {
    let (core, path) = new_core("uniq-upsert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'op-upsert-1'",
        )
        .expect_err("DO UPDATE that collides with another row's value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 新規挿入分岐での違反も拒否される。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (3, 'x') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-2'",
        )
        .expect_err("DO NOTHING branch insert colliding on UNIQUE column must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

// --- ALTER TABLE ADD UNIQUE（Rust API）・DROP COLUMN 依存検査 ----------

#[test]
fn alter_table_add_unique_constraint_rejects_existing_duplicates_with_no_side_effect() {
    let (core, path) = new_core("uniq-alter-add-reject");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 (duplicate value, no constraint yet)");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect_err("adding UNIQUE over a column with existing duplicates must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::UniqueConstraintViolation
    ));
    let schema = storage.get_table_schema("docs").expect("schema must exist");
    assert!(
        schema.unique_constraints().is_empty(),
        "the constraint must not have been persisted"
    );
}

#[test]
fn alter_table_add_unique_constraint_succeeds_when_no_duplicates_and_is_enforced_afterward() {
    let (core, path) = new_core("uniq-alter-add-ok");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect("adding UNIQUE with no existing duplicates must succeed");
    let schema = storage.get_table_schema("docs").expect("schema must exist");
    assert_eq!(schema.unique_constraints().len(), 1);

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("the newly added constraint must now be enforced");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn alter_table_drop_column_rejects_when_column_is_used_by_a_unique_constraint() {
    let (core, path) = new_core("uniq-drop-column-dependent");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT)",
    )
    .expect("create table");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_drop_column("docs", "a")
        .expect_err("dropping a column referenced by a UNIQUE constraint must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::DependentObjectsStillExist(_)
    ));
}
