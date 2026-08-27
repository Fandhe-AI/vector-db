//! `engine::core::EngineCore::execute_insert_sql`（ファイル形）の書き込み失敗注入試験
//! （TASK-100、対象ビヘイビア: RECOVER-9。ポインタ: `docs/spec/05-tasks.md` TASK-100・
//! `docs/spec/04-behavior/recovery.md` RECOVER-9。関連: RECOVER-5（TASK-96・
//! `tests/commit_boundary.rs`）・INDEX-1/2（TASK-120・`tests/incremental_index.rs`）。
//!
//! 本テストは TASK-120 の実索引経路（ファイル形 `INSERT` → チャンク化 → 注入型
//! `Embedder` によるベクトル化 → 同一パス置換書き込み）と、その上の検索読み取り経路を
//! 統合した状態で、書き込み失敗を 2 分岐に分けて注入し検証する:
//!
//! - (1) commit 前失敗: write トランザクションが commit へ到達する前に失敗する経路
//!   （`FailingEmbedder` による埋め込み失敗）。行データ・`operation_id` 台帳への
//!   副作用ゼロであることを検証する。
//! - (2) commit 成功後・索引反映途中の失敗: 本リポの検索索引は redb から導出される
//!   遅延構築型（`VectorArena` はクエリ毎に再構築、`DictionaryCache` は世代番号不一致で
//!   fail-closed に破棄・再構築。post-commit フックは存在しない — `core.rs` の
//!   `DictionaryCache` ドキュメント参照）。そのため「索引反映」は「commit 後の次回
//!   読み取り時の導出構造再構築」であり、(2) は次の 2 通りとして実現する:
//!   (2-a) commit 成功後、導出索引が一度も観測される前にプロセスが中断する
//!   （読み取りゼロで drop → 再オープン）。
//!   (2-b) 反映（再構築）の最初の読み取り試行自体が失敗する
//!   （`FailFirstSearchProvider` で初回 `search` のみ失敗させる）。
//!   いずれも「成功応答を返した commit 済みデータ」が、再構築後の索引・SELECT で
//!   欠落・重複なく完全一致することを検証する。
//!
//! 注入点はすべて公開 API 経由で確保する（テスト専用 feature ゲート API は新設しない
//! 方針・codex P0-2 再発防止。`tests/commit_boundary.rs` 冒頭コメント参照）:
//! `EngineCore::with_embedder`（(1) の注入）・`EngineCore::from_storage` へ渡す
//! `Box<dyn SearchProvider>`（(2-b) の注入）・drop ＋ `Storage::open` 再オープン
//! （(2-a) のプロセス再起動相当）。

use std::sync::atomic::{AtomicBool, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::chunking::{chunk_file, ChunkingConfig};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder, HashingEmbedder};
use engine::incremental::IncrementalConfig;
use engine::kernel::{CandidateHit, CpuScalarProvider, KernelError, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const DIM: u32 = 32;

/// `tests/incremental_index.rs::small_chunk_config` と同じ流儀（1 チャンク = 2 行）で
/// 生成チャンク数を小さく固定する。
fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: ChunkingConfig {
            lines_per_chunk: 2,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn documents_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

fn open_storage_with_table(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&documents_schema())
        .expect("create table");
    storage
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn insert_file_sql(path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
}

fn vector_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

fn zero_vec_literal() -> String {
    vector_literal(&vec![0.0f32; DIM as usize])
}

fn write_ctx() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("valid tenant")
}

fn read_ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
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

fn select_by_path(core: &EngineCore, path: &str) -> Vec<String> {
    let zero_vec = zero_vec_literal();
    let result = core
        .execute_sql(
            &read_ctx(),
            &format!(
                "SELECT body FROM {TABLE} WHERE path = '{}' ORDER BY embedding <=> {zero_vec} LIMIT 100",
                sql_escape(path)
            ),
        )
        .expect("select by path should succeed");
    let mut bodies = body_text_cells(&result);
    bodies.sort();
    bodies
}

/// `path` の期待チャンク本文集合を、`chunking` 公開 API で独立計算する
/// （テスト対象の書き込み経路と同一の計算式を使わない、というオラクル分離の意図。
/// `HashingEmbedder` は決定的だが本文自体は変換しないため、本文集合の一致だけで
/// チャンク化・置換書き込みの正しさを検証できる）。
fn expected_chunk_bodies(path: &str, body: &str, config: &ChunkingConfig) -> Vec<String> {
    let mut texts: Vec<String> = chunk_file(path, body, config)
        .expect("independent chunk_file computation should succeed")
        .into_iter()
        .map(|c| c.text)
        .collect();
    texts.sort();
    texts
}

/// `Err(Unavailable)` を常に返すフェイク埋め込み実装（commit 前失敗の注入用。
/// `tests/incremental_index.rs::FailingEmbedder` と同型）。
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

/// 初回 `search` 呼び出しのみ `KernelError::WorkerPanicked` を返し、以降は
/// `CpuScalarProvider` へ委譲するフェイク検索 provider（commit 成功後・索引反映の
/// 最初の読み取り試行が失敗するケースの注入用）。`AtomicBool` は
/// `SearchProvider: Send + Sync`（object-safe・`&self` メソッドのみ）の制約下で
/// 呼び出し回数を数える唯一の内部可変性手段。
struct FailFirstSearchProvider {
    failed_once: AtomicBool,
    inner: CpuScalarProvider,
}
impl FailFirstSearchProvider {
    fn new() -> Self {
        Self {
            failed_once: AtomicBool::new(false),
            inner: CpuScalarProvider,
        }
    }
}
impl SearchProvider for FailFirstSearchProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        if self
            .failed_once
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(KernelError::WorkerPanicked);
        }
        self.inner.search(input)
    }
}

/// `core` を drop し、同一パスで `EngineCore` を再構築する（プロセス再起動相当。
/// `tests/commit_boundary.rs::drop_and_reopen` と同型）。索引読み取り（`SELECT`・
/// `dictionary_snapshot` 等）を一切経由せずに呼ぶことで、commit 成功後・導出索引が
/// 観測可能になる前の中断を模擬できる。
fn drop_and_reopen(core: EngineCore, path: &std::path::Path) -> EngineCore {
    drop(core);
    let storage = Storage::open(path).expect("reopen storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

// --- (1) commit 前失敗: 副作用ゼロ ------------------------------------------------

/// (1)-a: 空のテーブルへ `FailingEmbedder` 構成でファイル形 `INSERT` を送ると
/// commit 前（`XX000`）で拒否され、行 0 件・台帳未記録のまま残ることを確認する。
/// 再オープン後に同一 `operation_id` での正常 `INSERT`（`HashingEmbedder`）が
/// 成功することを独立オラクルとし、台帳残渣がないことを証明する。
#[test]
fn precommit_failure_on_fresh_path_leaves_rows_and_ledger_untouched() {
    let path = unique_db_path("index-fail-precommit-fresh");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(FailingEmbedder { dim: DIM }))
        .with_incremental_config(small_chunk_config());
    let op = |id: &str| OperationId::parse(id).expect("valid operation_id");

    let err = core
        .execute_insert_sql(
            &write_ctx(),
            &insert_file_sql("docs/fresh.txt", "line one\nline two", "op-fresh-1"),
        )
        .expect_err("embedder failure must be rejected before commit");
    assert_eq!(err.wire_code(), "XX000");

    assert!(select_by_path(&core, "docs/fresh.txt").is_empty());
    assert_eq!(
        core.operation_recorded(&write_ctx(), TABLE, &op("op-fresh-1"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );

    // 再オープン（プロセス再起動相当）してもなお副作用ゼロであることを確認する。
    let core = drop_and_reopen(core, &path);
    assert!(select_by_path(&core, "docs/fresh.txt").is_empty());
    assert_eq!(
        core.operation_recorded(&write_ctx(), TABLE, &op("op-fresh-1"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );

    // 独立オラクル: 同一 operation_id・同一 path での正常 INSERT が成功する
    // （台帳・行のどちらにも残渣がないことの証明）。
    let core = core.with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")));
    core.execute_insert_sql(
        &write_ctx(),
        &insert_file_sql("docs/fresh.txt", "line one\nline two", "op-fresh-1"),
    )
    .expect("retry with the same operation_id must succeed after zero-side-effect failure");
    assert_eq!(select_by_path(&core, "docs/fresh.txt").len(), 1);
}

/// (1)-b: 正常 `INSERT` で baseline チャンクを commit した後、同一 `path` へ
/// `FailingEmbedder` 構成・新 `operation_id` で再送すると commit 前で拒否され、
/// 旧チャンクの削除が漏れ出さない（置換の途中状態が残らない）ことを確認する。
#[test]
fn precommit_failure_on_resend_keeps_existing_index_exact() {
    let path = unique_db_path("index-fail-precommit-resend");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let baseline_config = small_chunk_config();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(baseline_config.clone());
    let baseline_body = "alpha line one\nalpha line two\nalpha line three\nalpha line four";
    core.execute_insert_sql(
        &write_ctx(),
        &insert_file_sql("docs/resend.txt", baseline_body, "op-resend-baseline"),
    )
    .expect("baseline insert should succeed");

    let baseline_bodies = select_by_path(&core, "docs/resend.txt");
    let expected_baseline =
        expected_chunk_bodies("docs/resend.txt", baseline_body, &baseline_config.chunking);
    assert_eq!(baseline_bodies, expected_baseline);
    let op = |id: &str| OperationId::parse(id).expect("valid operation_id");
    assert_eq!(
        core.last_operation_id(&write_ctx(), TABLE)
            .expect("last operation lookup should succeed"),
        engine::recovery::ledger::LastOperationLookup::Committed(op("op-resend-baseline"))
    );

    // 埋め込みだけ故障する構成へ差し替え、同一 path へ新 operation_id で再送する。
    let core = core.with_embedder(Box::new(FailingEmbedder { dim: DIM }));
    let err = core
        .execute_insert_sql(
            &write_ctx(),
            &insert_file_sql(
                "docs/resend.txt",
                "bravo new content that must not land",
                "op-resend-fail",
            ),
        )
        .expect_err("embedder failure on resend must be rejected before commit");
    assert_eq!(err.wire_code(), "XX000");
    assert_eq!(
        core.operation_recorded(&write_ctx(), TABLE, &op("op-resend-fail"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );

    // 再オープン後、baseline のチャンクが完全一致で残っており、失敗 operation_id は
    // 台帳に記録されず last_operation_id は baseline のまま。
    let core = drop_and_reopen(core, &path);
    assert_eq!(select_by_path(&core, "docs/resend.txt"), expected_baseline);
    assert_eq!(
        core.last_operation_id(&write_ctx(), TABLE)
            .expect("last operation lookup should succeed"),
        engine::recovery::ledger::LastOperationLookup::Committed(op("op-resend-baseline"))
    );
    assert_eq!(
        core.operation_recorded(&write_ctx(), TABLE, &op("op-resend-fail"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );
}

// --- (2) commit 成功後・索引反映途中の失敗: 成功応答＋再構築で完全一致 -----------------

/// (2)-a: 正常 `INSERT` の成功応答（`chunks_written == N`）を得た直後、一切読み取らずに
/// drop（導出索引・キャッシュが構築される前の中断を模擬）→ 再オープン → SELECT・
/// 距離検索・hybrid 検索の結果が、独立計算した期待チャンク集合と完全一致すること
/// （件数 N・本文・欠落なし・重複なし）を確認する。`operation_recorded` が commit
/// 済みであることも併せて確認する。
#[test]
fn postcommit_interruption_then_restart_rebuilds_index_exactly() {
    let path = unique_db_path("index-fail-postcommit-restart");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let config = small_chunk_config();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(config.clone());

    let body = "l1 unique-zzqq\nl2\nl3\nl4\nl5\nl6";
    let outcome = core
        .execute_insert_sql(
            &write_ctx(),
            &insert_file_sql("docs/restart.txt", body, "op-restart-1"),
        )
        .expect("file-form insert should succeed");
    let incremental = outcome
        .incremental
        .expect("file-form insert sets incremental");
    let expected = expected_chunk_bodies("docs/restart.txt", body, &config.chunking);
    assert_eq!(incremental.chunks_written, expected.len());
    assert_eq!(outcome.rows_affected as usize, expected.len());
    let op = OperationId::parse("op-restart-1").expect("valid operation_id");
    assert_eq!(
        core.operation_recorded(&write_ctx(), TABLE, &op)
            .expect("ledger lookup should succeed"),
        LedgerLookup::Recorded
    );

    // 読み取りゼロで drop → 再オープン（導出索引・DictionaryCache が一度も
    // 構築されていない状態からの再構築を強制する）。
    let core = drop_and_reopen(core, &path);

    let select_bodies = select_by_path(&core, "docs/restart.txt");
    assert_eq!(
        select_bodies, expected,
        "SELECT must match independently computed chunks exactly"
    );
    assert_eq!(
        select_bodies.len(),
        select_bodies
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "no duplicate chunk bodies must remain"
    );

    let embedder = HashingEmbedder::new(DIM).expect("valid dim");
    let query_vec = embedder
        .embed_batch(&["unique-zzqq"])
        .expect("query embedding should succeed")
        .remove(0);
    let query_literal = vector_literal(&query_vec);
    let distance_result = core
        .execute_sql(
            &read_ctx(),
            &format!("SELECT body FROM {TABLE} ORDER BY embedding <=> {query_literal} LIMIT 1"),
        )
        .expect("distance search should succeed");
    let distance_bodies = body_text_cells(&distance_result);
    assert!(
        distance_bodies.iter().any(|b| b.contains("unique-zzqq")),
        "distance top-1 after restart should contain the unique marker, got: {distance_bodies:?}"
    );

    let hybrid_result = core
        .execute_sql(
            &read_ctx(),
            &format!(
                "SELECT body FROM {TABLE} ORDER BY HYBRID(embedding, {query_literal}, body, 'unique-zzqq') LIMIT 1"
            ),
        )
        .expect("hybrid search should succeed");
    let hybrid_bodies = body_text_cells(&hybrid_result);
    assert!(
        hybrid_bodies.iter().any(|b| b.contains("unique-zzqq")),
        "hybrid top-1 after restart should contain the unique marker, got: {hybrid_bodies:?}"
    );
}

/// (2)-b: `FailFirstSearchProvider` 構成で正常 `INSERT`（成功応答）→ 初回検索
/// （反映試行）がエラー → 同一プロセス内の再試行検索が完全一致集合を返す →
/// さらに再オープン後も一致することを確認する（反映失敗が持続的破損を残さない）。
#[test]
fn postcommit_first_reflection_read_failure_recovers_on_retry_and_restart() {
    let path = unique_db_path("index-fail-postcommit-read-retry");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let config = small_chunk_config();
    let core = EngineCore::from_storage(storage, Box::new(FailFirstSearchProvider::new()))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(config.clone());

    let body = "m1\nm2\nm3\nm4";
    core.execute_insert_sql(
        &write_ctx(),
        &insert_file_sql("docs/retry.txt", body, "op-retry-1"),
    )
    .expect("file-form insert should succeed");
    let expected = expected_chunk_bodies("docs/retry.txt", body, &config.chunking);

    // 初回検索（索引反映の最初の読み取り試行）は注入した provider により失敗する。
    let zero_vec = zero_vec_literal();
    core.execute_sql(
        &read_ctx(),
        &format!(
            "SELECT body FROM {TABLE} WHERE path = 'docs/retry.txt' ORDER BY embedding <=> {zero_vec} LIMIT 100"
        ),
    )
    .expect_err("first reflection read must fail as injected");

    // 同一プロセス内の再試行は成功し、完全一致集合を返す（反映失敗が索引そのものを
    // 破損させたわけではなく、1 回の読み取り試行だけが失敗したことの確認）。
    assert_eq!(select_by_path(&core, "docs/retry.txt"), expected);

    // 再オープン（通常の CpuScalarProvider）後も一致する。
    let core = drop_and_reopen(core, &path);
    assert_eq!(select_by_path(&core, "docs/retry.txt"), expected);
}

/// (2)-c: 旧本文で `INSERT` → 検索でキャッシュ・導出索引を温める（反映済み）→
/// 新本文・新 `operation_id` で同一 `path` を再送 commit → 読み取らずに drop・
/// 再オープン → 検索・SELECT に旧チャンクが 1 件も残らず（重複なし）、新チャンクのみ
/// 完全一致（欠落なし）で現れることを確認する。
#[test]
fn resend_replacement_leaves_no_stale_or_duplicate_entries_after_restart() {
    let path = unique_db_path("index-fail-resend-restart");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let config = small_chunk_config();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(config.clone());

    let old_body = "old line one stale-marker-aaaa\nold line two\nold line three\nold line four";
    core.execute_insert_sql(
        &write_ctx(),
        &insert_file_sql("docs/replace.txt", old_body, "op-replace-old"),
    )
    .expect("first insert should succeed");

    // 検索・dictionary_snapshot でキャッシュ・導出索引を温める（世代 G で反映済み）。
    let warmed = select_by_path(&core, "docs/replace.txt");
    let expected_old = expected_chunk_bodies("docs/replace.txt", old_body, &config.chunking);
    assert_eq!(warmed, expected_old);
    core.dictionary_snapshot(&write_ctx(), TABLE)
        .expect("dictionary snapshot should succeed while warming the cache");

    // 新本文・新 operation_id で同一 path を再送 commit（世代 G+1）。
    let new_body = "new line one fresh-marker-bbbb\nnew line two\nnew line three\nnew line four\nnew line five\nnew line six";
    core.execute_insert_sql(
        &write_ctx(),
        &insert_file_sql("docs/replace.txt", new_body, "op-replace-new"),
    )
    .expect("resend insert should succeed");
    let expected_new = expected_chunk_bodies("docs/replace.txt", new_body, &config.chunking);

    // 読み取らずに drop・再オープン。
    let core = drop_and_reopen(core, &path);

    let after_bodies = select_by_path(&core, "docs/replace.txt");
    assert_eq!(
        after_bodies, expected_new,
        "only the newly resent chunks must remain after restart"
    );
    assert!(
        after_bodies
            .iter()
            .all(|b| !b.contains("stale-marker-aaaa")),
        "no stale chunk from the old content must remain, got: {after_bodies:?}"
    );
    assert_eq!(
        after_bodies.len(),
        after_bodies
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "no duplicate chunk bodies must remain"
    );

    let dict = core
        .dictionary_snapshot(&write_ctx(), TABLE)
        .expect("dictionary snapshot after restart should succeed");
    // 辞書スナップショットが旧本文の固有語を保持していないことを独立オラクルとして
    // 確認する（デバッグ表現の文字列一致。`Dictionary` は辞書構築 API のトークン
    // 集合を直接公開しないため）。
    let dict_debug = format!("{dict:?}");
    assert!(
        !dict_debug.contains("stale-marker-aaaa"),
        "dictionary snapshot must not retain the stale token after restart"
    );
}
