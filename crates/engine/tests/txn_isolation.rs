//! `engine::txn`（`Storage::isolation_level` / `Storage::begin_read` /
//! `Storage::begin_write`）の統合テスト（TASK-88、対象ビヘイビア: TABLE-3。ポインタ:
//! `docs/spec/05-tasks.md` TASK-88・`docs/spec/04-behavior/data-model.md` TABLE-3）。
//!
//! `crates/engine/tests/persistence.rs` の `persist4_writes_are_serialized_and_reads_see_snapshot`
//! は `redb` を直接操作して分離レベルの根拠（PERSIST-4）を検証済みのため、本ファイルは
//! それと重複させず、engine の公開 txn API（[`engine::txn`]）経由でのみ TABLE-3 を検証する。

use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use engine::storage::{RowInput, Storage, StorageError, Visibility};
use engine::txn::IsolationLevel;

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した（旧: 結合テストごとの複製）。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn row<'a>(embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding,
        metadata,
    }
}

// 対象ビヘイビア: TABLE-3（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
#[test]
fn table3_isolation_level_is_declared_as_single_writer_snapshot_read() {
    let path = unique_db_path("declared-level");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let level = storage.isolation_level();
    assert_eq!(level, IsolationLevel::SingleWriterSnapshotRead);
    // Display の出力文字列（プログラム出力は英語。japanese-style.md）を固定する。
    // TABLE-3 の「分離レベルを確認する」操作に対する、最も直接的な確認手段。
    assert_eq!(level.to_string(), "single-writer, snapshot-read");
}

// 対象ビヘイビア: TABLE-3（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
#[test]
fn table3_write_txn_commits_multiple_rows_atomically_via_single_open_table() {
    let path = unique_db_path("multi-row-commit");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // WriteTxn::put を同一トランザクション内で複数回呼び出す経路
    // （Storage::put_batch と異なり、呼び出し元が 1 行ずつ put する主要な使い方）を検証する。
    let mut txn = storage.begin_write().expect("begin write txn");
    txn.put(1, &row(&[1.0], b"a")).expect("first put in txn");
    txn.put(2, &row(&[2.0], b"b")).expect("second put in txn");
    txn.commit().expect("commit multi-row write txn");

    assert_eq!(storage.get(1).expect("row 1 committed").metadata, b"a");
    assert_eq!(storage.get(2).expect("row 2 committed").metadata, b"b");
}

// 対象ビヘイビア: TABLE-3（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
#[test]
fn table3_write_txn_abort_discards_uncommitted_writes() {
    let path = unique_db_path("abort-discards");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut txn = storage.begin_write().expect("begin write txn");
    txn.put(1, &row(&[1.0], b"discarded"))
        .expect("put before abort");
    txn.abort().expect("abort write txn");

    let err = storage
        .get(1)
        .expect_err("aborted write must not be visible");
    assert!(
        matches!(err, StorageError::NotFound(_)),
        "expected row 1 to be reported as NotFound after abort, got: {err}"
    );
}

// 対象ビヘイビア: TABLE-3（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
// WriteTxn::commit / WriteTxn::abort のどちらも呼ばずに drop した場合、
// redb::WriteTransaction の Drop 実装により自動的に abort されることを固定する
// （txn.rs の WriteTxn ドキュメントコメント参照）。
#[test]
fn table3_write_txn_drop_without_commit_or_abort_auto_aborts() {
    let path = unique_db_path("drop-auto-aborts");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    {
        let mut txn = storage.begin_write().expect("begin write txn");
        txn.put(1, &row(&[1.0], b"dropped"))
            .expect("put before drop");
        // commit も abort も呼ばずにスコープを抜けて drop させる。
    }

    let err = storage
        .get(1)
        .expect_err("dropped-without-commit write must not be visible");
    assert!(
        matches!(err, StorageError::NotFound(_)),
        "expected row 1 to be reported as NotFound after drop, got: {err}"
    );

    // 排他ロックも解放されている（drop 後に新しい書き込みトランザクションを開始できる）ことを確認する。
    let mut next_txn = storage
        .begin_write()
        .expect("begin_write must not block after the prior WriteTxn was dropped");
    next_txn
        .put(2, &row(&[2.0], b"after-drop"))
        .expect("put after drop");
    next_txn.commit().expect("commit after drop");
    assert_eq!(
        storage.get(2).expect("row 2 committed").metadata,
        b"after-drop"
    );
}

// 対象ビヘイビア: TABLE-3（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
// PERSIST-4 と同一根拠（`crates/engine/tests/persistence.rs` の
// `persist4_writes_are_serialized_and_reads_see_snapshot` は redb 直接操作で検証済み）。
// 本テストは engine の公開 txn API（`Storage::begin_write`）経由でのみ検証するため重複しない。
#[test]
fn table3_begin_write_serializes_concurrent_writers_via_public_txn_api() {
    let path = unique_db_path("serialize-writes");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Arc::new(Storage::open(&path).expect("open storage"));

    let (first_acquired_tx, first_acquired_rx) = mpsc::channel::<()>();
    let (release_first_tx, release_first_rx) = mpsc::channel::<()>();
    let second_acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let first_storage = Arc::clone(&storage);
    let first = thread::spawn(move || {
        let mut txn = first_storage.begin_write().expect("begin first write txn");
        txn.put(1, &row(&[1.0], b"first"))
            .expect("insert row 1 in first txn");
        first_acquired_tx
            .send(())
            .expect("signal first txn acquired");
        release_first_rx.recv().expect("wait for commit permission");
        txn.commit().expect("commit first write txn");
    });

    first_acquired_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first writer did not acquire the write lock in time");

    let second_storage = Arc::clone(&storage);
    let second_acquired_writer = Arc::clone(&second_acquired);
    let (second_started_tx, second_started_rx) = mpsc::channel::<()>();
    let second = thread::spawn(move || {
        second_started_tx
            .send(())
            .expect("signal second writer started");
        let mut txn = second_storage
            .begin_write()
            .expect("begin second write txn (blocks until first commits)");
        second_acquired_writer.store(true, std::sync::atomic::Ordering::SeqCst);
        txn.put(2, &row(&[2.0], b"second"))
            .expect("insert row 2 in second txn");
        txn.commit().expect("commit second write txn");
    });

    second_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second writer did not start in time");
    // 2 本目が begin_write の排他ロック待ちで止まっていることを確認するための短い待機。
    // second_started_tx の送信は begin_write 呼び出しの直前であり、それ自体は
    // 「ロック待ちでブロックされている」ことまでは保証しない（送信後スレッドが
    // begin_write に到達する前にスケジューラに横取りされる可能性がある）。
    // 1 回のスリープ後の単発チェックではその隙間を見逃しうるため、待機窓の間
    // 繰り返しポーリングして「一度も先行取得していない」ことを固定する
    // （persistence.rs の persist4 テストの手法を、単発サンプリングから連続監視に強化）。
    let poll_deadline = std::time::Instant::now() + Duration::from_millis(200);
    while std::time::Instant::now() < poll_deadline {
        assert!(
            !second_acquired.load(std::sync::atomic::Ordering::SeqCst),
            "second begin_write must not have acquired the exclusive write lock \
             while the first write txn is still open"
        );
        thread::sleep(Duration::from_millis(10));
    }

    release_first_tx
        .send(())
        .expect("release first writer to commit");
    first.join().expect("first writer thread panicked");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !second.is_finished() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        second.is_finished(),
        "second writer must complete once the first write txn is released \
         (it appears still stuck on begin_write)"
    );
    second.join().expect("second writer thread panicked");
    assert!(
        second_acquired.load(std::sync::atomic::Ordering::SeqCst),
        "second begin_write must have acquired the lock after the first txn was released"
    );

    // 両方のコミットが反映されていること。
    assert_eq!(storage.get(1).expect("row 1 committed").metadata, b"first");
    assert_eq!(storage.get(2).expect("row 2 committed").metadata, b"second");
}

// 対象ビヘイビア: TABLE-3（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
#[test]
fn table3_read_snapshot_does_not_see_later_commits_or_uncommitted_writes() {
    let path = unique_db_path("snapshot-read");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // 事前にコミット済みの行を 1 件用意する。
    storage
        .put(1, &row(&[9.0], b"pre-existing"))
        .expect("seed pre-existing row");

    // スナップショットを開始する（この時点でコミット済みの状態を固定する）。
    let snapshot = storage.begin_read().expect("begin read snapshot");

    // スナップショット開始後に、新たな書き込みトランザクションでコミットする。
    let mut write_txn = storage
        .begin_write()
        .expect("begin write txn after snapshot");
    write_txn
        .put(2, &row(&[10.0], b"committed-after-snapshot"))
        .expect("insert row 2");
    write_txn
        .commit()
        .expect("commit row 2 after snapshot was opened");

    // 事前に存在した行はスナップショットから見える。
    let seen = snapshot
        .get(1)
        .expect("pre-existing row visible in snapshot");
    assert_eq!(seen.metadata, b"pre-existing");

    // スナップショット開始後にコミットされた行は、このスナップショットからは見えない
    // （スナップショットが開始時点の状態に固定されていることの確認）。
    let err = snapshot
        .get(2)
        .expect_err("row committed after snapshot start must not be visible to it");
    assert!(
        matches!(err, StorageError::NotFound(_)),
        "expected row 2 to be reported as NotFound within the snapshot, got: {err}"
    );

    // 新たに開始したスナップショットからは、コミット済みの行 2 が見える
    // （後続の読み取りはコミット済み最新状態を見る）。
    let fresh_snapshot = storage.begin_read().expect("begin fresh read snapshot");
    let fresh = fresh_snapshot
        .get(2)
        .expect("row 2 must be visible to a snapshot opened after commit");
    assert_eq!(fresh.metadata, b"committed-after-snapshot");
}

// 対象ビヘイビア: TABLE-3（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
#[test]
fn table3_read_snapshot_does_not_see_uncommitted_write_from_open_write_txn() {
    let path = unique_db_path("snapshot-uncommitted");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Arc::new(Storage::open(&path).expect("open storage"));

    storage
        .put(1, &row(&[1.0], b"pre-existing"))
        .expect("seed pre-existing row");

    let (writer_ready_tx, writer_ready_rx) = mpsc::channel::<()>();
    let (release_writer_tx, release_writer_rx) = mpsc::channel::<()>();

    let writer_storage = Arc::clone(&storage);
    let writer = thread::spawn(move || {
        let mut txn = writer_storage.begin_write().expect("begin write txn");
        txn.put(2, &row(&[2.0], b"uncommitted"))
            .expect("insert uncommitted row 2");
        writer_ready_tx.send(()).expect("signal writer ready");
        release_writer_rx
            .recv()
            .expect("wait for commit permission");
        txn.commit().expect("commit write txn");
    });

    writer_ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer did not signal readiness in time");

    // 書き込みが進行中（未コミット）の間に開始したスナップショットは、その未コミットの
    // 行を見てはならない。
    let snapshot = storage
        .begin_read()
        .expect("begin read snapshot while write is pending");
    let err = snapshot
        .get(2)
        .expect_err("uncommitted row must not be visible to a snapshot reader");
    assert!(
        matches!(err, StorageError::NotFound(_)),
        "expected row 2 to be reported as NotFound, got: {err}"
    );

    release_writer_tx
        .send(())
        .expect("release writer to commit");
    writer.join().expect("writer thread panicked");
}
