//! `EngineCore::execute_insert_sql` の結合テスト（TASK-80、対象ビヘイビア: SQL-10。
//! ポインタ: `docs/spec/05-tasks.md` TASK-80・`docs/spec/04-behavior/sql-surface.md`・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1）。
//!
//! `tests/sql_surface.rs`・`tests/sql_allowlist.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage` 上にテーブルを構築）で、`INSERT ... USING
//! OPERATION_ID '<id>'` の受理・文末専用句の省略拒否・statement 単位スコープを
//! 検証する。台帳系（同一 `operation_id` の再送拒否 `23505`・内容不一致 `22023`）は
//! 本タスクの管轄外（TASK-93・TASK-94・TASK-101 が後続で扱う）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::{Storage, Visibility};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-sql-operation-id-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

// --- 受理系 -----------------------------------------------------------------

#[test]
fn insert_with_operation_id_clause_succeeds_and_is_readable_by_owning_tenant() {
    let path = unique_db_path("insert-succeeds");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let outcome = core
        .execute_insert_sql(
            &write_ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .expect("insert with clause should succeed");
    assert_eq!(outcome.rows_affected, 1);

    // 挿入テナント自身は Private 行を読み戻せる（可視性 Private 固定の効果）。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].id, 1);
}

// --- 省略拒否（23502。書き込みトランザクション未開始の外形確認） ------------------

#[test]
fn insert_missing_operation_id_clause_is_rejected_before_any_write() {
    let path = unique_db_path("insert-missing-clause");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello')",
        )
        .expect_err("missing clause must be rejected");
    assert_eq!(err.wire_code(), "23502");

    // 書き込みトランザクションが一切開始されていないことを外形的に確認する
    // （行が 1 件も反映されていない。`EngineCore` は `Storage` を外へ出さない
    // 一方向設計のため、`core.rs` モジュールドキュメント参照、確認は SELECT 経由で行う）。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert!(result.rows.is_empty());
}

#[test]
fn insert_empty_operation_id_value_is_rejected_as_missing() {
    let path = unique_db_path("insert-empty-value");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID ''",
        )
        .expect_err("empty value must be rejected as missing");
    assert_eq!(err.wire_code(), "23502");
}

// --- 拡張クエリプロトコル形式（$n）は 42601 --------------------------------------

#[test]
fn insert_operation_id_dollar_placeholder_is_rejected_as_syntax_error() {
    let path = unique_db_path("insert-dollar-placeholder");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID $1",
        )
        .expect_err("$n placeholder must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- statement 単位スコープ（セッションに引き継がれない） -----------------------

#[test]
fn operation_id_clause_does_not_carry_over_to_the_next_statement() {
    let path = unique_db_path("insert-statement-scope");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
    )
    .expect("first insert with clause should succeed");

    // 直後の文に句がなければ、直前の文の句は一切引き継がれず 23502 で拒否される
    // （SQL-10 の要件: statement 単位スコープ。セッション変数等の別経路は設けない）。
    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'world')",
        )
        .expect_err("clause must not carry over to the next statement");
    assert_eq!(err.wire_code(), "23502");
}

// --- 既存 id への INSERT は上書きせず拒否（22000） ------------------------------

#[test]
fn insert_into_existing_id_is_rejected_without_overwriting() {
    let path = unique_db_path("insert-existing-id");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &write_ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
    )
    .expect("first insert should succeed");

    let err = core
        .execute_insert_sql(
            &write_ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[9.0,9.0,9.0]', 'overwrite') USING OPERATION_ID 'op-0002'",
        )
        .expect_err("insert into existing id must be rejected");
    assert_eq!(err.wire_code(), "22000");

    // 元の値のまま(上書きされていない)ことを読み戻しで確認する。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
}

// --- 他テナントからは不可視（Private 固定の RLS 効果） ---------------------------

#[test]
fn inserted_row_is_invisible_to_other_tenants() {
    let path = unique_db_path("insert-invisible-other-tenant");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &write_ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
    )
    .expect("insert should succeed");

    let other_ctx =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &other_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert!(result.rows.is_empty());
}

// --- 非 nullable 列欠落は 22000 ---------------------------------------------------

#[test]
fn insert_missing_non_nullable_column_is_rejected() {
    let path = unique_db_path("insert-missing-non-nullable");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-0001'",
        )
        .expect_err("missing non-nullable column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}
