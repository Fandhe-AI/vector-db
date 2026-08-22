//! `engine::storage::Storage` の統合テスト（TASK-140、対象ビヘイビア:
//! PERSIST-1, PERSIST-2, PERSIST-4。ポインタ: `docs/spec/04-behavior/persistence.md`）。
//!
//! PERSIST-1・PERSIST-4 の検証は `Storage` の公開 API だけでは表現できないトランザクション
//! 境界（未コミットのまま中断・書き込みの直列化）に踏み込むため、テストスコープでのみ
//! `redb` を直接操作する（`crates/engine/Cargo.toml` の `[dev-dependencies]` 参照）。
//! プロセス強制終了（SIGKILL）を伴うクラッシュ再現の CI ジョブ化は TASK-142 のスコープ、
//! 増分書き込みの所要時間比の回帰測定は TASK-143 のスコープのため、本ファイルには含めない。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use engine::storage::{RowInput, Storage, StorageError};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// `Storage` 内部の行テーブルと同一の名前・型（キー: 行 ID、値: エンコード済みバイト列）。
/// テスト側は行の中身を解釈しないため、値のエンコード詳細に依存しない。
const ROWS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("rows");

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに一意な DB ファイルパスを払い出す（`cargo test` のデフォルト並列実行でも
/// 衝突しないよう、プロセス ID とプロセス内連番を組み合わせる）。
fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-persist-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

/// テスト終了時（panic 時含む）に DB ファイルを確実に削除するガード。
struct CleanupGuard(PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn row<'a>(embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        embedding,
        metadata,
    }
}

// PERSIST-1: commit 前に書き込みトランザクションを中断した場合、直前にコミット済みの
// データは無傷であり、中断された書き込みは反映されないこと（`redb` の ACID 保証）。
#[test]
fn persist1_uncommitted_write_is_discarded_and_committed_data_survives_reopen() {
    let path = unique_db_path("persist1");
    let _cleanup = CleanupGuard(path.clone());

    // 1. Storage 経由でコミット済みの行を 1 件作る。
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(1, &row(&[1.0, 2.0, 3.0], b"committed"))
            .expect("commit row 1");
    } // ここで Storage（= redb::Database）を drop し、ファイルロックを解放する。

    // 2. 生の redb::Database で同じファイルを再オープンし、書き込みトランザクションを
    //    開始・行を挿入するが、明示的に abort して「commit 前の中断」を再現する。
    {
        let db = Database::open(&path).expect("reopen raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            // この raw ハンドルが Storage と同じテーブルを見ていることを確認する
            // （テーブル名が食い違うと、以降の「abort 後に行 2 が消えている」検証が
            // 別テーブルを見ているだけの偽陽性になり得るため）。
            assert!(
                table.get(1u64).expect("get row 1 via raw handle").is_some(),
                "raw Database handle must see the row committed via Storage"
            );
            table
                .insert(2u64, &b""[..])
                .expect("insert into uncommitted txn");
        }
        write_txn.abort().expect("abort uncommitted txn");
    } // Database を drop し、ファイルロックを解放する。

    // 3. Storage で再オープンし、コミット済みの行 1 は無傷、中断した行 2 は存在しないこと
    //    を確認する。
    let storage = Storage::open(&path).expect("reopen storage");
    let committed = storage.get(1).expect("row 1 must survive reopen");
    assert_eq!(committed.embedding, vec![1.0, 2.0, 3.0]);
    assert_eq!(committed.metadata, b"committed");

    assert!(
        storage.get(2).is_err(),
        "aborted write must not be visible after reopen"
    );
}

// PERSIST-2: 既存データを保持したまま増分書き込みが反映されること（全体再構築を伴わない
// 追記の機能検証。所要時間比の回帰測定は TASK-143 のスコープのため本テストには含めない）。
#[test]
fn persist2_incremental_write_preserves_existing_rows() {
    let path = unique_db_path("persist2");
    let _cleanup = CleanupGuard(path.clone());

    let storage = Storage::open(&path).expect("open storage");

    let initial_embeddings: Vec<[f32; 1]> = (0..50u64).map(|i| [i as f32]).collect();
    let initial: Vec<(u64, RowInput<'_>)> = (0..50u64)
        .map(|i| (i, row(&initial_embeddings[i as usize], b"initial")))
        .collect();
    storage.put_batch(&initial).expect("initial batch write");

    let after_initial = storage.scan().expect("scan after initial batch");
    assert_eq!(after_initial.len(), 50);

    // 増分書き込み: 既存範囲に触れず、新規 ID のみを追加する。
    let incremental_embeddings: Vec<[f32; 1]> = (50..80u64).map(|i| [i as f32]).collect();
    let incremental: Vec<(u64, RowInput<'_>)> = (50..80u64)
        .map(|i| {
            (
                i,
                row(&incremental_embeddings[(i - 50) as usize], b"incremental"),
            )
        })
        .collect();
    storage
        .put_batch(&incremental)
        .expect("incremental batch write");

    let after_incremental = storage.scan().expect("scan after incremental batch");
    assert_eq!(after_incremental.len(), 80);

    // 既存データが増分書き込みの影響を受けていないこと（再構築されていないこと）を
    // スポットチェックする。
    let preserved = storage.get(10).expect("original row must remain readable");
    assert_eq!(preserved.embedding, vec![10.0]);
    assert_eq!(preserved.metadata, b"initial");

    let appended = storage
        .get(65)
        .expect("incrementally written row must be readable");
    assert_eq!(appended.embedding, vec![65.0]);
    assert_eq!(appended.metadata, b"incremental");
}

// PERSIST-4: 書き込みトランザクションが直列化されること（`begin_write` の排他ロック）、
// および書き込み進行中に開始した読み取りが開始時点のスナップショットを見ること
// （未コミットの変更が見えないこと）を検証する。
#[test]
fn persist4_writes_are_serialized_and_reads_see_snapshot() {
    let path = unique_db_path("persist4");
    let _cleanup = CleanupGuard(path.clone());

    // 既存行を 1 件用意しておく（読み取りスナップショットの比較対象）。
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(1, &row(&[9.0], b"pre-existing"))
            .expect("seed row");
    }

    let db = Arc::new(Database::open(&path).expect("reopen raw database"));

    let (writer_ready_tx, writer_ready_rx) = mpsc::channel::<()>();
    let (release_writer_tx, release_writer_rx) = mpsc::channel::<()>();

    let writer_db = Arc::clone(&db);
    let writer = thread::spawn(move || {
        let write_txn = writer_db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            table.insert(2u64, &b""[..]).expect("insert row 2");
        }
        // 挿入済みだが未コミットである旨をメインスレッドへ通知し、コミット許可を待つ。
        writer_ready_tx.send(()).expect("signal writer ready");
        release_writer_rx
            .recv()
            .expect("wait for commit permission");
        write_txn.commit().expect("commit write txn");
    });

    writer_ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer did not signal readiness in time");

    // 書き込み進行中（未コミット）に開始した読み取りは、その時点のスナップショットを
    // 見るべきであり、未コミットの行 2 は見えてはならない。
    {
        let read_txn = db
            .begin_read()
            .expect("begin read txn during pending write");
        let table = read_txn
            .open_table(ROWS_TABLE)
            .expect("open table for read");
        assert!(
            table.get(1u64).expect("get row 1").is_some(),
            "pre-existing committed row must be visible"
        );
        assert!(
            table.get(2u64).expect("get row 2").is_none(),
            "uncommitted row must not be visible to a snapshot reader"
        );
    }

    // 直列化の確認: 別スレッドから 2 本目の書き込みトランザクションを開始しようとしても、
    // 1 本目がコミットするまで `begin_write` は完了しない（排他ロックにより直列化される）
    // ことを確認する。
    let second_writer_db = Arc::clone(&db);
    let (second_started_tx, second_started_rx) = mpsc::channel::<()>();
    let second_writer = thread::spawn(move || {
        second_started_tx
            .send(())
            .expect("signal second writer started");
        let _write_txn = second_writer_db
            .begin_write()
            .expect("begin second write txn (blocks until first commits)");
        // 直列化の確認のみが目的のため、2 本目は commit せず drop（abort）する。
    });

    second_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second writer did not start in time");
    // 2 本目が `begin_write` の排他ロック待ちで止まっていることを確認するための短い待機
    // （タイミング計測ではなく、明らかにブロックされていることの確認が目的）。
    thread::sleep(Duration::from_millis(200));
    assert!(
        !second_writer.is_finished(),
        "second begin_write must remain blocked while the first write txn is still open"
    );

    release_writer_tx
        .send(())
        .expect("release first writer to commit");
    writer.join().expect("writer thread panicked");
    second_writer.join().expect("second writer thread panicked");

    // コミット後に新たに開始した読み取りは、コミット済みの行 2 を見えるようになる。
    let read_txn = db.begin_read().expect("begin read txn after commit");
    let table = read_txn
        .open_table(ROWS_TABLE)
        .expect("open table for read");
    assert!(
        table.get(2u64).expect("get row 2 after commit").is_some(),
        "row committed by the first writer must be visible after commit"
    );
}

// デコード fail-closed の統合テスト（不正バイト列を Storage 経由で読んだ場合に、
// 黙殺フォールバックせず Err で拒否されること）。encode/decode 単体の詳細な境界値検証は
// `crates/engine/src/storage.rs` のユニットテストで行う。
#[test]
fn storage_get_rejects_corrupted_row_bytes() {
    let path = unique_db_path("fail-closed");
    let _cleanup = CleanupGuard(path.clone());

    {
        let db = Database::create(&path).expect("create raw database");
        let write_txn = db.begin_write().expect("begin write txn");
        {
            let mut table = write_txn.open_table(ROWS_TABLE).expect("open table");
            // version バイトのみで、後続のフィールドが一切ないバイト列（意図的な破損）。
            table
                .insert(1u64, &[1u8][..])
                .expect("insert malformed row");
        }
        write_txn.commit().expect("commit malformed row");
    }

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .get(1)
        .expect_err("malformed row must be rejected, not decoded with defaults");
    // `NotFound` ではなく、デコードそのものが拒否されたことを確認する
    // （テーブル名の取り違え等で行が別テーブルへ書かれてしまった場合、
    // `NotFound` になり得るため、これを区別しないとこのテストは黙殺フォールバックの
    // 有無を検証できない）。
    assert!(
        matches!(err, StorageError::Codec(_)),
        "expected a decode failure (StorageError::Codec), got: {err}"
    );
}
