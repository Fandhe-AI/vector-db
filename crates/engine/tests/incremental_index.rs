//! `engine::core::EngineCore::execute_insert_sql`（ファイル形）の結合テスト
//! （TASK-120、対象ビヘイビア: INDEX-1, INDEX-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-120・`docs/spec/04-behavior/indexing.md`
//! INDEX-1, INDEX-2・TASK-123・`docs/design/resend-semantics.md`）。
//!
//! `tests/sql_operation_id.rs`・`tests/sql_surface.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage` 上にテーブルを構築）で、ファイル形 `INSERT`
//! （`path`/`body` 列。`id`・VECTOR 列を指定しない形）がチャンク化 → 決定的参照実装
//! `HashingEmbedder` によるベクトル化 → 置換書き込みを経て検索に反映されることを
//! 検証する。埋め込みは決定的だが意味的ではない（`embedding.rs` モジュール
//! ドキュメント参照）ため、Recall の厳密な数値検証ではなく「共有トークンを持つ
//! チャンクが上位に来る」ことを確認する構成にする。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder, HashingEmbedder};
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 128;

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM)))
        .with_incremental_config(small_chunk_config())
}

/// 生成チャンク数を小さく固定するための `IncrementalConfig`（1 チャンク = 2 行）。
fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 2,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn vector_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
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

/// `Err(Unavailable)` を常に返すフェイク埋め込み実装（原子性・副作用ゼロの検証用）。
struct FailingEmbedder {
    dim: u32,
}
impl Embedder for FailingEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }
    fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Unavailable)
    }
}

/// 非有限値（`NaN`）を含むベクトルを返すフェイク埋め込み実装（外部実装が壊れた応答を
/// 返す場合に、永続化前へ fail-closed に倒れることの検証用）。
struct NonFiniteEmbedder {
    dim: u32,
}
impl Embedder for NonFiniteEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut v = vec![0.0f32; self.dim as usize];
        if let Some(slot) = v.get_mut(0) {
            *slot = f32::NAN;
        }
        Ok(vec![v; texts.len()])
    }
}

// --- INDEX-2: 結果整合性（チャンク化・埋め込み・書き込みが検索へ反映される） --------

#[test]
fn index2_file_insert_chunks_are_searchable_by_hybrid_and_distance() {
    let path = unique_db_path("index2-searchable");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 6 行・lines_per_chunk=2 → 3 チャンク。最後のチャンクだけが固有語 zzzqq を含む。
    let body = "alpha alpha alpha token one\n\
                alpha alpha alpha token one continued\n\
                bravo bravo bravo token two\n\
                bravo bravo bravo token two continued\n\
                charlie charlie charlie unique marker zzzqq\n\
                charlie charlie charlie unique marker zzzqq continued";

    // 挿入前は該当パスの行が 0 件であること。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let before = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/note.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("pre-insert select should succeed");
    assert_eq!(before.rows.len(), 0);

    let outcome = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "docs/note.txt", body, "op-index2-1"),
        )
        .expect("file-form insert should succeed");
    assert_eq!(outcome.rows_affected, 3);
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");
    assert_eq!(incremental.chunks_written, 3);
    assert_eq!(incremental.rows_replaced, 3);

    let embedder = HashingEmbedder::new(DIM);
    let query_text = "unique marker zzzqq";
    let query_vec = embedder
        .embed_batch(&[query_text])
        .expect("query embedding should succeed")
        .remove(0);
    let query_literal = vector_literal(&query_vec);

    // HYBRID(embedding, query_vec, body, query_text) の Top-1 が固有語を含むこと。
    let hybrid_sql = format!(
        "SELECT body FROM documents ORDER BY HYBRID(embedding, {query_literal}, body, '{query_text}') LIMIT 1"
    );
    let hybrid_result = core
        .execute_sql(&read_ctx, &hybrid_sql)
        .expect("hybrid search should succeed");
    let hybrid_bodies = body_text_cells(&hybrid_result);
    assert!(
        hybrid_bodies.iter().any(|b| b.contains("zzzqq")),
        "hybrid top-1 should contain the unique marker, got: {hybrid_bodies:?}"
    );

    // ORDER BY embedding <=> '...' LIMIT 1（密検索単体）でも同じチャンクが Top-1。
    let distance_sql =
        format!("SELECT body FROM documents ORDER BY embedding <=> {query_literal} LIMIT 1");
    let distance_result = core
        .execute_sql(&read_ctx, &distance_sql)
        .expect("distance search should succeed");
    let distance_bodies = body_text_cells(&distance_result);
    assert!(
        distance_bodies.iter().any(|b| b.contains("zzzqq")),
        "distance top-1 should contain the unique marker, got: {distance_bodies:?}"
    );
}

// --- 置換セマンティクス（TASK-123）----------------------------------------------

#[test]
fn resend_same_path_replaces_chunks_and_old_content_disappears() {
    let path = unique_db_path("index-resend-replace");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    // 他パス（tenant-a）が置換の巻き添えにならないことも併せて確認する。
    let other_body = "other other other file marker qqzzz\nother other other continued";
    core.execute_insert_sql(
        &write_ctx,
        &insert_file_sql("documents", "docs/other.txt", other_body, "op-resend-other"),
    )
    .expect("other-path insert should succeed");

    let first_body = "first version line one alpha\nfirst version line two alpha";
    let first_outcome = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "docs/note.txt", first_body, "op-resend-1"),
        )
        .expect("first insert should succeed");
    assert_eq!(first_outcome.rows_affected, 1);

    let second_body = "second version line one bravo\nsecond version line two bravo\nsecond version line three bravo\nsecond version line four bravo";
    let second_outcome = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "docs/note.txt", second_body, "op-resend-2"),
        )
        .expect("resend insert should succeed");
    // 2 行/チャンクで 4 行 → 2 チャンク。旧 1 チャンクは全削除、新 2 チャンクを挿入。
    assert_eq!(second_outcome.rows_affected, 2);
    let incremental = second_outcome
        .incremental
        .expect("file-form insert sets incremental");
    assert_eq!(incremental.rows_replaced, 2);

    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows_for_note = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/note.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select by path should succeed");
    assert_eq!(rows_for_note.rows.len(), 2);
    let bodies = body_text_cells(&rows_for_note);
    assert!(bodies.iter().all(|b| b.contains("second version")));
    assert!(bodies.iter().all(|b| !b.contains("first version")));

    // 他パスは無変更（1 チャンクのまま）。
    let rows_for_other = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/other.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select other path should succeed");
    assert_eq!(rows_for_other.rows.len(), 1);
    assert!(body_text_cells(&rows_for_other)[0].contains("qqzzz"));
}

#[test]
fn empty_body_file_insert_is_rejected_and_does_not_wipe_existing_chunks() {
    // Issue #68 レビュー指摘: 本文が空（または空白のみで `chunk_file` が
    // 0 チャンクを返す）ファイル形 INSERT を送ると、ガードなしでは既存チャンクを
    // 全削除したうえで挿入 0 件のまま世代だけ bump してコミットしてしまう
    // （実質的な索引破壊が「成功」応答になる）。fail-closed に拒否し、
    // 既存チャンクが無傷で残ることを確認する。
    let path = unique_db_path("index-empty-body-rejected");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let first_body = "first version line one alpha\nfirst version line two alpha";
    core.execute_insert_sql(
        &write_ctx,
        &insert_file_sql("documents", "docs/note.txt", first_body, "op-empty-1"),
    )
    .expect("first insert should succeed");

    // 空白のみの本文 → `chunk_file` は 0 チャンクを返す。
    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "docs/note.txt", "   \n\n  ", "op-empty-2"),
        )
        .expect_err("empty-chunk file insert should be rejected");
    assert_eq!(err.wire_code(), "22000");

    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows_for_note = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/note.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select by path should succeed");
    // 既存チャンクが無傷で残る（全削除されていない）。
    assert_eq!(rows_for_note.rows.len(), 1);
    assert!(body_text_cells(&rows_for_note)[0].contains("first version"));
}

// --- テナント境界 ---------------------------------------------------------------

#[test]
fn resend_does_not_touch_other_tenants_same_path_rows() {
    let path = unique_db_path("index-tenant-isolation");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

    let body_b = "tenant b content line one\ntenant b content line two";
    core.execute_insert_sql(
        &ctx_b,
        &insert_file_sql("documents", "shared/path.txt", body_b, "op-tenant-b"),
    )
    .expect("tenant-b insert should succeed");

    let body_a = "tenant a content line one\ntenant a content line two";
    core.execute_insert_sql(
        &ctx_a,
        &insert_file_sql("documents", "shared/path.txt", body_a, "op-tenant-a-1"),
    )
    .expect("tenant-a insert should succeed");

    // tenant-a が同じパスを再送しても tenant-b の行は削除されない。
    let body_a2 = "tenant a updated line one\ntenant a updated line two";
    core.execute_insert_sql(
        &ctx_a,
        &insert_file_sql("documents", "shared/path.txt", body_a2, "op-tenant-a-2"),
    )
    .expect("tenant-a resend should succeed");

    let read_ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows_b = core
        .execute_sql(
            &read_ctx_b,
            &format!(
                "SELECT body FROM documents WHERE path = 'shared/path.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("tenant-b select should succeed");
    assert_eq!(rows_b.rows.len(), 1);
    assert!(body_text_cells(&rows_b)[0].contains("tenant b content"));

    // tenant-a からは tenant-b のチャンクが不可視（Private 固定）。
    let read_ctx_a_public_only = PolicyContext::new("tenant-a").expect("valid tenant");
    let rows_a_public_only = core
        .execute_sql(
            &read_ctx_a_public_only,
            &format!(
                "SELECT body FROM documents WHERE path = 'shared/path.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("tenant-a public-only select should succeed");
    assert_eq!(rows_a_public_only.rows.len(), 0);
}

// --- 原子性・副作用ゼロ ----------------------------------------------------------

#[test]
fn embedder_failure_leaves_no_side_effects_and_returns_internal_error() {
    let path = unique_db_path("index-embedder-failure");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(FailingEmbedder { dim: DIM }));
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql(
                "documents",
                "docs/fail.txt",
                "line one\nline two",
                "op-fail-1",
            ),
        )
        .expect_err("embedder failure should be rejected");
    assert_eq!(err.wire_code(), "XX000");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/fail.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select should succeed");
    assert_eq!(rows.rows.len(), 0);
}

/// `Embedder` 応答に非有限値（`NaN`/`±Inf`）が含まれる場合、書き込み前に
/// fail-closed に拒否されることを固定する（codex-review P1 指摘・PR #221。
/// SQL のベクトルリテラル経路と同じ防御をこの新経路にも適用する）。
#[test]
fn non_finite_embedding_values_are_rejected_before_write() {
    let path = unique_db_path("index-non-finite");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(NonFiniteEmbedder { dim: DIM }))
        .with_incremental_config(small_chunk_config());
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql(
                "documents",
                "docs/nonfinite.txt",
                "line one\nline two",
                "op-nonfinite-1",
            ),
        )
        .expect_err("non-finite embedding values must be rejected");
    assert_eq!(err.wire_code(), "XX000");

    // 副作用ゼロ（write トランザクションへ入らない）。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/nonfinite.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select should succeed");
    assert_eq!(rows.rows.len(), 0);
}

#[test]
fn chunk_count_over_limit_is_rejected_with_no_side_effects() {
    let path = unique_db_path("index-chunk-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM)))
        .with_incremental_config(IncrementalConfig {
            chunking: engine::chunking::ChunkingConfig {
                lines_per_chunk: 1,
                max_markdown_section_chars: None,
            },
            max_chunks_per_file: 3,
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 1 行 1 チャンクで 5 行 → 5 チャンク > 上限 3。
    let body = "l1\nl2\nl3\nl4\nl5";
    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "docs/toolarge.txt", body, "op-toolarge-1"),
        )
        .expect_err("chunk count over limit should be rejected");
    assert_eq!(err.wire_code(), "54000");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/toolarge.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select should succeed");
    assert_eq!(rows.rows.len(), 0);
}

/// サーバー側のチャンク化設定が不正（`lines_per_chunk == 0`）な場合、クライアント
/// 入力起因の `54000`（payload too large）ではなくサーバー内部失敗 `XX000` を返す
/// ことを固定する（Cursor Bugbot 指摘・PR #221。再試行・障害判定の誤導を防ぐ）。
#[test]
fn invalid_chunking_config_is_reported_as_internal_error_not_payload_limit() {
    let path = unique_db_path("index-invalid-config");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM)))
        .with_incremental_config(IncrementalConfig {
            chunking: engine::chunking::ChunkingConfig {
                lines_per_chunk: 0,
                max_markdown_section_chars: None,
            },
            ..IncrementalConfig::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "docs/cfg.txt", "l1\nl2\nl3", "op-cfg-1"),
        )
        .expect_err("invalid server-side chunking config must be rejected");
    assert_eq!(err.wire_code(), "XX000");

    // 副作用ゼロ（write トランザクションを開始しない）。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/cfg.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select should succeed");
    assert_eq!(rows.rows.len(), 0);
}

#[test]
fn missing_embedder_is_rejected_fail_closed_with_no_side_effects() {
    let path = unique_db_path("index-no-embedder");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    // `with_embedder` を呼ばない（既定 `None`）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql(
                "documents",
                "docs/noembedder.txt",
                "line one\nline two",
                "op-noemb-1",
            ),
        )
        .expect_err("missing embedder should be rejected");
    assert_eq!(err.wire_code(), "XX000");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/noembedder.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select should succeed");
    assert_eq!(rows.rows.len(), 0);
}

#[test]
fn embedder_dim_mismatch_with_table_schema_is_rejected_with_no_side_effects() {
    let path = unique_db_path("index-embedder-dim-mismatch");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    // テーブルは VECTOR(DIM) だが、注入した Embedder は異なる次元を返す
    // （サーバー側設定の不整合。クライアント入力の不正ではないため XX000 になる
    // べきで、22000（クライアント起因の値不正）に丸め込まれてはならない）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM + 1)));
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql(
                "documents",
                "docs/dimmismatch.txt",
                "line one\nline two",
                "op-dimmismatch-1",
            ),
        )
        .expect_err("embedder/table dim mismatch should be rejected");
    assert_eq!(err.wire_code(), "XX000");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    let rows = core
        .execute_sql(
            &read_ctx,
            &format!(
                "SELECT body FROM documents WHERE path = 'docs/dimmismatch.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
            ),
        )
        .expect("select should succeed");
    assert_eq!(rows.rows.len(), 0);
}

// --- 行形の回帰（既存 `id` + ベクトルリテラル指定の INSERT は不変） ------------------

/// ファイル形 `INSERT` も行形 `INSERT` と同じく `operation_id` を台帳へ記録し
/// （TASK-93・RECOVER-2）、拒否された場合は行も台帳エントリも残さないことを固定する
/// （置換書き込みと台帳記録が同一 write トランザクションで原子的であることの確認）。
#[test]
fn file_form_insert_records_operation_id_in_ledger_atomically() {
    let path = unique_db_path("index-ledger");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |id: &str| OperationId::parse(id).expect("valid operation_id");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql(
            "documents",
            "docs/ledger.txt",
            "line one\nline two",
            "op-led-1",
        ),
    )
    .expect("file-form insert should succeed");
    assert_eq!(
        core.operation_recorded(&ctx, "documents", &op("op-led-1"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::Recorded
    );
    assert_eq!(
        core.operation_recorded(&ctx, "documents", &op("op-led-unused"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );

    // 他テナントからは同じ `operation_id` が観測されない（RLS-9 と同型のスコープ）。
    let other = PolicyContext::new("tenant-b").expect("valid tenant");
    assert_eq!(
        core.operation_recorded(&other, "documents", &op("op-led-1"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );
}

/// 拒否されたファイル形 `INSERT` は行も台帳エントリも残さない（置換書き込みと
/// 台帳記録が同一 write トランザクションで abort される。TASK-93・RECOVER-2）。
#[test]
fn rejected_file_form_insert_leaves_no_ledger_entry() {
    let path = unique_db_path("index-ledger-abort");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(NonFiniteEmbedder { dim: DIM }))
        .with_incremental_config(small_chunk_config());
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |id: &str| OperationId::parse(id).expect("valid operation_id");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "docs/ledger2.txt", "line one", "op-led-2"),
    )
    .expect_err("non-finite embedding must be rejected");
    assert_eq!(
        core.operation_recorded(&ctx, "documents", &op("op-led-2"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );
}

#[test]
fn row_form_insert_still_writes_single_row_with_no_incremental_outcome() {
    let path = unique_db_path("index-row-form-regression");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let vec_literal = vector_literal(&vec![0.1f32; DIM as usize]);
    let sql = format!(
        "INSERT INTO documents (id, embedding, path, body) VALUES (1, {vec_literal}, 'manual/row.txt', 'manual body') USING OPERATION_ID 'op-row-form-1'"
    );
    let outcome = core
        .execute_insert_sql(&write_ctx, &sql)
        .expect("row-form insert should succeed unchanged");
    assert_eq!(outcome.rows_affected, 1);
    assert!(outcome.incremental.is_none());
}

// --- INDEX-1（スモーク）: 計測構造の健全性 ----------------------------------------
//
// 厳密な時間比・中央値の受け入れ基準は TASK-121（`tests/incremental_recall.rs`）の
// 管轄。ここでは `IndexTiming` が実際に各段階を計測して返る構造であることと、
// 作業量カウンタ（`chunks_written`/`rows_replaced`）が入力と整合することのみを
// 固定する（時間依存の閾値判定はしない。CI 環境差によるフレーク回避）。

#[test]
fn index1_timing_fields_are_populated_and_work_counters_match_input() {
    let path = unique_db_path("index1-timing-smoke");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let body = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8";
    let outcome = core
        .execute_insert_sql(
            &write_ctx,
            &insert_file_sql("documents", "docs/timing.txt", body, "op-timing-1"),
        )
        .expect("insert should succeed");
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");
    assert_eq!(incremental.chunks_written, 4); // 8 行 / 2 行毎チャンク
    assert_eq!(incremental.rows_replaced, 4);
    assert_eq!(outcome.rows_affected, 4);
    // `Duration` は常に非負。実測値が返っている（既定値のゼロ埋めでない）ことの
    // 弱い健全性チェックとして、少なくとも 1 段階が非ゼロであることを確認する
    // （全段階が厳密に 0ns になる環境依存の可能性を避けるため、合計で判定する）。
    let total =
        incremental.timing.chunking + incremental.timing.embedding + incremental.timing.write;
    assert!(total.as_nanos() > 0 || cfg!(miri));
}
