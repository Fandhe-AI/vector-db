//! `engine::sql::allowlist` の統合テスト（TASK-74・SQL-8 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md）。
//!
//! `tests/catalog.rs` と同じ流儀（`unique_db_path` / `CleanupGuard`）で実 `Storage`
//! 上にテーブルを構築し、`impl TableLookup for Storage` を介した実カタログ照会付きの
//! `validate_statement` を検証する（`sql::allowlist` モジュール内の単体テストは
//! storage 非依存のフェイクを使うため、本ファイルは実カタログとの結合を確認する）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::sql::allowlist::validate_statement;
use engine::storage::Storage;

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-sql-allowlist-{label}-{}-{seq}.redb",
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

fn open_storage_with_documents_table(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create documents table");
    (storage, guard)
}

#[test]
fn accepts_c1_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("c1");
    let stmt = validate_statement(
        "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect("C1 shape against a real table must be accepted");
    assert_eq!(stmt.table_name, "documents");
}

#[test]
fn accepts_c2_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("c2");
    validate_statement(
        "SELECT * FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        &storage,
    )
    .expect("C2 shape against a real table must be accepted");
}

#[test]
fn rejects_table_absent_from_real_catalog_with_42p01() {
    let (storage, _guard) = open_storage_with_documents_table("undefined-table");
    let err = validate_statement(
        "SELECT * FROM nonexistent ORDER BY embedding <=> '[0.1]' LIMIT 10",
        &storage,
    )
    .expect_err("table absent from the real catalog must be rejected");
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn rejects_unsupported_syntax_before_touching_the_catalog() {
    // 未対応構文（GROUP BY）は、FROM に実在テーブルを指定していてもカタログ照会に
    // 先立って 42601 で拒否される（決定的な検証順序: 構造判定 → カタログ存在確認）。
    let (storage, _guard) = open_storage_with_documents_table("unsupported-syntax");
    let err = validate_statement(
        "SELECT * FROM documents GROUP BY lang ORDER BY embedding <=> '[0.1]' LIMIT 10",
        &storage,
    )
    .expect_err("unsupported syntax must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn never_silently_executes_an_unrecognized_where_condition_as_unfiltered_top_k() {
    // 未対応 WHERE 条件が黙殺されず明示的に拒否されることを固定する（SQL-8）。
    let (storage, _guard) = open_storage_with_documents_table("no-silent-fallback");
    let err = validate_statement(
        "SELECT * FROM documents WHERE lang = 'ja' AND 1 = 1 ORDER BY embedding <=> '[0.1]' LIMIT 10",
        &storage,
    )
    .expect_err("unrecognized WHERE condition must be rejected, never silently dropped");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn same_input_yields_same_wire_code_across_repeated_calls_against_real_catalog() {
    let (storage, _guard) = open_storage_with_documents_table("determinism");
    let sql = "SELECT * FROM documents JOIN other ON documents.id = other.id ORDER BY embedding <=> '[0.1]' LIMIT 10";
    let first = validate_statement(sql, &storage).unwrap_err().wire_code();
    let second = validate_statement(sql, &storage).unwrap_err().wire_code();
    assert_eq!(first, second);
}
