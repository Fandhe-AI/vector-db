//! `operation_id` 重複拒否の原子性の結合テスト（TASK-94、対象ビヘイビア:
//! RECOVER-3。ポインタ: `docs/spec/05-tasks.md` TASK-94・
//! `docs/spec/04-behavior/recovery.md` RECOVER-3・
//! `docs/spec/04-behavior/error-format.md` ERR-2）。
//!
//! `tests/recovery_ledger.rs`（TASK-93）と同じ流儀（実 `Storage` +
//! `CpuScalarProvider`、`EngineCore::from_storage`）で、同一
//! `(tenant_id, table, operation_id)` への 2 回目以降の書き込みが `23505` で拒否され、
//! 拒否時に行変更が一切残らない（原子性）ことを、INSERT 行形・ファイル形置換・
//! UPDATE・DELETE の各経路・並行 2 セッションで検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore, VectorCore};
use engine::embedding::HashingEmbedder;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::exec::Cell;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const OTHER_TABLE: &str = "other_docs";
const DIM: u32 = 3;

fn schema(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn open_storage_with_tables(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    storage
        .create_table(&schema(OTHER_TABLE))
        .expect("create other table");
    storage
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation_id")
}

fn row(embedding: &[f32]) -> RowInput<'_> {
    RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Private,
        embedding,
        metadata: &[],
    }
}

fn body_text_cells(result: &engine::sql::exec::QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => s.clone(),
            other => panic!("expected Cell::Text, got {other:?}"),
        })
        .collect()
}

// --- 行形 INSERT: 同一 operation_id の 2 回目（別行 id）が 23505 で拒否され、
//     2 回目の行は増えない。---------------------------------------------------------

#[test]
fn insert_form_second_use_of_same_operation_id_is_rejected_without_adding_a_row() {
    let path = unique_db_path("dedup-insert");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // `get_row` の可視性判定（`is_visible`）に通すため Private も許可する
    // （書き込み系 API の認可は `is_owner` のみを見るため可視性許可には依存しない）。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("first insert must succeed");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-a'",
        )
        .expect_err("second use of the same operation_id must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 2 回目の行 (id=2) は増えていない。1 回目の行 (id=1) は無傷。
    assert!(matches!(
        core.get_row(&ctx, TABLE, "tenant-a", 2),
        Err(CoreError::NotFound)
    ));
    assert!(core.get_row(&ctx, TABLE, "tenant-a", 1).is_ok());
}

// --- 行形 INSERT の重複拒否は行キー衝突と別文言（行 id 衝突と取り違えない）。--------

#[test]
fn insert_form_duplicate_operation_id_message_differs_from_row_id_conflict() {
    let path = unique_db_path("dedup-message");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("first insert must succeed");

    // (a) 同一 operation_id・別行 id → 台帳の重複拒否。
    let dup_err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-a'",
        )
        .expect_err("duplicate operation_id must be rejected");

    // (b) 別 operation_id・同一行 id → 行キー衝突。
    let id_conflict_err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.7,0.8,0.9]', 'c') USING OPERATION_ID 'op-b'",
        )
        .expect_err("row id conflict must be rejected");

    assert_eq!(dup_err.wire_code(), "23505");
    assert_eq!(id_conflict_err.wire_code(), "23505");
    assert_ne!(
        dup_err.to_string(),
        id_conflict_err.to_string(),
        "duplicate operation_id and row id conflict must carry distinct client messages"
    );
}

// --- 原子性: 拒否された 2 回目以降の書き込みは行が一切残らない。-------------------

#[test]
fn rejected_second_writes_leave_no_partial_row_changes() {
    let path = unique_db_path("dedup-atomic");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("first insert must succeed");

    for id in 2..=5u64 {
        let err = core
            .execute_insert_sql(
                &ctx,
                &format!(
                    "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.1,0.1]', 'x') USING OPERATION_ID 'op-a'"
                ),
            )
            .expect_err("every reuse of the same operation_id must be rejected");
        assert_eq!(err.wire_code(), "23505");
        assert!(
            matches!(
                core.get_row(&ctx, TABLE, "tenant-a", id),
                Err(CoreError::NotFound)
            ),
            "row {id} from a rejected write must not exist"
        );
    }

    // 1 回目の行は最後まで無傷。
    assert!(core.get_row(&ctx, TABLE, "tenant-a", 1).is_ok());
}

// --- UPDATE: EngineCore::update_row 経由でも同一 operation_id の 2 回目は 23505 で
//     拒否され、行は 1 回目の更新内容のまま変わらない。-----------------------------

#[test]
fn update_row_second_use_of_same_operation_id_is_rejected_and_row_is_unchanged() {
    let path = unique_db_path("dedup-update");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3]),
        Some(&op("op-insert")),
    )
    .expect("insert_row must succeed");

    core.update_row(&ctx, TABLE, 1, &row(&[0.4, 0.5, 0.6]), Some(&op("op-u")))
        .expect("first update must succeed");

    let err = core
        .update_row(&ctx, TABLE, 1, &row(&[0.7, 0.8, 0.9]), Some(&op("op-u")))
        .expect_err("second use of the same operation_id must be rejected");
    assert!(matches!(
        err,
        engine::tenant::TenantWriteError::DuplicateOperationId
    ));
    assert_eq!(err.wire_code(), "23505");

    let current = core
        .get_row(&ctx, TABLE, "tenant-a", 1)
        .expect("row must still exist");
    assert_eq!(
        current.embedding,
        vec![0.4, 0.5, 0.6],
        "the rejected second update must not overwrite the first update's content"
    );
}

// --- DELETE: EngineCore::delete_row 経由でも同一 operation_id の 2 回目は 23505。
//     1 回目の削除で対象行は既に無いため、2 回目は台帳の重複拒否が先に効くことを、
//     `NotFound` ではなく `DuplicateOperationId` が返ることで確認する。-----------------

#[test]
fn delete_row_second_use_of_same_operation_id_is_rejected_as_duplicate_not_not_found() {
    let path = unique_db_path("dedup-delete");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3]),
        Some(&op("op-insert-1")),
    )
    .expect("insert_row must succeed");
    core.insert_row(
        &ctx,
        TABLE,
        2,
        &row(&[0.4, 0.5, 0.6]),
        Some(&op("op-insert-2")),
    )
    .expect("insert_row must succeed");

    core.delete_row(&ctx, TABLE, 1, Some(&op("op-d")))
        .expect("first delete must succeed");

    // 同じ operation_id で別行 (id=2) を削除しても台帳が先に拒否する
    // （削除対象は行 2 であり、行 1 の不存在に起因する NotFound にはならない）。
    let err = core
        .delete_row(&ctx, TABLE, 2, Some(&op("op-d")))
        .expect_err("second use of the same operation_id must be rejected");
    assert!(matches!(
        err,
        engine::tenant::TenantWriteError::DuplicateOperationId
    ));
    assert_eq!(err.wire_code(), "23505");

    // 行 2 はまだ存在する（拒否により削除されていない）。
    assert!(core.get_row(&ctx, TABLE, "tenant-a", 2).is_ok());
}

// --- ファイル形 INSERT（置換書き込み経路）: 同一 operation_id の 2 回目は 23505 で
//     拒否され、1 回目の内容が保持される（置換されない）。--------------------------

fn new_core_with_file_table(path: &std::path::Path, dim: u32) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(dim), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(dim).expect("valid dim")))
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{path}', '{body}') USING OPERATION_ID '{op_id}'"
    )
}

#[test]
fn file_form_insert_second_use_of_same_operation_id_is_rejected_without_replacing() {
    const FILE_DIM: u32 = 32;
    let path = unique_db_path("dedup-file-insert");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_file_table(&path, FILE_DIM);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    core.execute_insert_sql(
        &write_ctx,
        &insert_file_sql(
            "documents",
            "docs/note.txt",
            "first body content",
            "op-file-1",
        ),
    )
    .expect("first file insert must succeed");

    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql(
                "documents",
                "docs/note.txt",
                "second body content",
                "op-file-1",
            ),
        )
        .expect_err("second use of the same operation_id must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 内容が置換されていない（1 回目の 'first body content' のまま）ことを検索で確認する。
    let zero_vec = format!("'[{}]'", vec!["0"; FILE_DIM as usize].join(","));
    let rows_for_note = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/note.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select by path should succeed");
    let bodies = body_text_cells(&rows_for_note);
    assert!(bodies.iter().all(|b| b.contains("first body content")));
    assert!(bodies.iter().all(|b| !b.contains("second body content")));

    // 無関係の operation_id は引き続き成功する。
    core.execute_insert_sql(
        &write_ctx,
        &insert_file_sql(
            "documents",
            "docs/other.txt",
            "unrelated content",
            "op-file-2",
        ),
    )
    .expect("unrelated operation_id must still succeed");
}

// --- スコープ: 別テーブルへの同一 operation_id は成功する（テーブル単位）。---------

#[test]
fn different_table_with_same_operation_id_succeeds() {
    let path = unique_db_path("dedup-scope-table");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("insert into documents must succeed");
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO other_docs (id, embedding, body) VALUES (1, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-a'",
    )
    .expect("reusing the same operation_id on a different table must succeed");
}

// --- スコープ: 別テナントの同一 operation_id は成功する（テナント単位）。-----------

#[test]
fn different_tenant_with_same_operation_id_succeeds() {
    let path = unique_db_path("dedup-scope-tenant");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

    core.execute_insert_sql(
        &ctx_a,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("tenant-a insert must succeed");
    core.execute_insert_sql(
        &ctx_b,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-a'",
    )
    .expect("tenant-b reusing the same operation_id must succeed (different tenant namespace)");
}

// --- 対比構成: CompareOnlyWithoutLedger では拒否されない（台帳なし構成の仕様どおり）。

#[test]
fn compare_only_without_ledger_does_not_reject_duplicates() {
    let path = unique_db_path("dedup-no-ledger");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("first insert must succeed under compare-only mode");
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-a'",
    )
    .expect("compare-only mode must not reject reuse of the same operation_id");
}

// --- 並行性: 2 スレッドが同一 operation_id・別行 id を同時 INSERT すると、
//     成功 1・拒否 1 になり、当該テーブルの行数は試行ごとに +1 のまま。-------------
//     （TASK-94・RECOVER-3 の実測契約。redb 単一ライタ直列化 + 同一 write
//     トランザクション内の判定により、2 セッションのうち一方のみ commit する。
//     `Storage`/`PolicyContext`/`OperationId` はいずれも `Sync` のため、
//     `crate::tenant::insert_row` を直接 2 スレッドから共有参照で呼べる。）

#[test]
fn concurrent_same_operation_id_inserts_yield_exactly_one_success() {
    for attempt in 0..10u64 {
        let path = unique_db_path(&format!("dedup-concurrent-{attempt}"));
        let _guard = CleanupGuard(path.clone());
        let storage = open_storage_with_tables(&path);
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_id = op("op-concurrent");

        let (successes, row_count) = std::thread::scope(|s| {
            let storage_ref = &storage;
            let ctx_ref = &ctx;
            let op_ref = &op_id;
            let h1 = s.spawn(move || {
                engine::tenant::insert_row(
                    storage_ref,
                    TABLE,
                    ctx_ref,
                    1,
                    &row(&[0.1, 0.2, 0.3]),
                    op_ref,
                )
            });
            let h2 = s.spawn(move || {
                engine::tenant::insert_row(
                    storage_ref,
                    TABLE,
                    ctx_ref,
                    2,
                    &row(&[0.4, 0.5, 0.6]),
                    op_ref,
                )
            });
            let r1 = h1.join().expect("thread 1 must not panic");
            let r2 = h2.join().expect("thread 2 must not panic");

            let successes = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
            for r in [&r1, &r2] {
                if let Err(e) = r {
                    assert!(
                        matches!(e, engine::tenant::TenantWriteError::DuplicateOperationId),
                        "the losing session must fail with DuplicateOperationId, got {e:?}"
                    );
                }
            }

            let visible = engine::tenant::visible_rows(storage_ref, TABLE, ctx_ref)
                .expect("visible_rows must succeed");
            (successes, visible.len())
        });

        assert_eq!(
            successes, 1,
            "attempt {attempt}: exactly one of the two concurrent sessions must succeed"
        );
        assert_eq!(
            row_count, 1,
            "attempt {attempt}: exactly one row must have been committed"
        );
    }
}
