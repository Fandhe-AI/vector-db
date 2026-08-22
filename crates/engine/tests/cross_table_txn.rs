//! `engine::txn::WriteTxn::log_batch`（[`ROWS_TABLE`] とバッチ台帳
//! [`crate::storage::BATCH_LOG_TABLE`] を同一トランザクションで扱う経路）の統合テスト
//! （TASK-90、対象ビヘイビア: TABLE-10。ポインタ: `docs/spec/05-tasks.md` TASK-90・
//! `docs/spec/04-behavior/data-model.md` TABLE-10）。
//!
//! `crates/engine/examples/crash_tool_cross_table.rs` + `scripts/crash_test_cross_table.sh`
//! はプロセス外からの SIGKILL に対する耐性を検証するのに対し、本ファイルは
//! プロセス内テストとして「commit で両テーブルへ原子的に反映される」「commit 前に drop・
//! `abort` した場合は両テーブルとも破棄される」という 2 テーブル横断トランザクションの
//! 原子性そのものを検証する（クラッシュ回帰テストとは独立に、通常経路での正しさを保証する）。

use engine::storage::{RowInput, Storage, Visibility};

static UNIQUE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// テストごとに一意な DB ファイルパスを払い出す
/// （`crates/engine/tests/txn_isolation.rs` の同名ヘルパーと同じ方針）。
fn unique_db_path(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::Ordering;
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-cross-table-txn-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(std::path::PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn row<'a>(embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding,
        metadata,
    }
}

// 対象ビヘイビア: TABLE-10（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
#[test]
fn table10_commit_reflects_both_tables_atomically() {
    let path = unique_db_path("commit-both");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let embedding = [1.0_f32, 2.0, 3.0];
    let metadata = [9_u8, 8, 7];

    let mut txn = storage.begin_write().expect("begin_write");
    txn.put(0, &row(&embedding, &metadata)).expect("put row 0");
    txn.put(1, &row(&embedding, &metadata)).expect("put row 1");
    txn.log_batch(0).expect("log_batch");
    txn.commit().expect("commit");

    assert_eq!(storage.get(0).expect("get row 0").embedding, embedding);
    assert_eq!(storage.get(1).expect("get row 1").embedding, embedding);
    assert_eq!(
        storage.scan_batch_log().expect("scan_batch_log"),
        vec![(0, 2)]
    );
}

// 対象ビヘイビア: TABLE-10。commit 前に drop した場合は両テーブルとも破棄される
// （`redb::WriteTransaction` の Drop 契約に委譲。`txn.rs` のドキュメントコメント参照）。
#[test]
fn table10_drop_without_commit_discards_both_tables() {
    let path = unique_db_path("drop-discards-both");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    {
        let mut txn = storage.begin_write().expect("begin_write");
        txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
        txn.log_batch(0).expect("log_batch");
        // commit も abort も呼ばずスコープを抜ける。
    }

    let get_err = storage.get(0).expect_err("row must not exist after drop");
    assert!(matches!(
        get_err,
        engine::storage::StorageError::NotFound(0)
    ));
    assert_eq!(
        storage.scan_batch_log().expect("scan_batch_log"),
        Vec::<(u64, u64)>::new()
    );
}

// 対象ビヘイビア: TABLE-10。明示的な abort でも両テーブルとも破棄される。
#[test]
fn table10_abort_discards_both_tables() {
    let path = unique_db_path("abort-discards-both");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut txn = storage.begin_write().expect("begin_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    txn.log_batch(0).expect("log_batch");
    txn.abort().expect("abort");

    let get_err = storage.get(0).expect_err("row must not exist after abort");
    assert!(matches!(
        get_err,
        engine::storage::StorageError::NotFound(0)
    ));
    assert_eq!(
        storage.scan_batch_log().expect("scan_batch_log"),
        Vec::<(u64, u64)>::new()
    );
}

// 対象ビヘイビア: TABLE-10。複数バッチのコミット後に「batch_log の row_count 合計 ==
// 行総数」というテーブル間不変条件が成立し、reopen（プロセス内での再オープン）後も
// 維持されることを確認する。
#[test]
fn table10_batch_totals_match_row_count_and_survive_reopen() {
    let path = unique_db_path("totals-survive-reopen");
    let _cleanup = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage");
        let mut next_id: u64 = 0;
        for batch_seq in 0..3_u64 {
            let mut txn = storage.begin_write().expect("begin_write");
            for _ in 0..4 {
                txn.put(next_id, &row(&[next_id as f32], &[next_id as u8]))
                    .expect("put row");
                next_id += 1;
            }
            txn.log_batch(batch_seq).expect("log_batch");
            txn.commit().expect("commit");
        }
    }

    // 同一パスを再オープンし、ディスクへ確定した状態のみを見ていることを確認する。
    let storage = Storage::open(&path).expect("reopen storage");
    let (rows, cursor) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(cursor, None);
    assert_eq!(rows.len(), 12);

    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(batch_log, vec![(0, 4), (1, 4), (2, 4)]);
    let total_from_log: u64 = batch_log.iter().map(|(_, count)| count).sum();
    assert_eq!(total_from_log, rows.len() as u64);
}

// 対象ビヘイビア: TABLE-10。既存 batch_seq への 2 度目の log_batch は
// `StorageError::DuplicateBatchSeq` で拒否され、既存エントリを黙って上書きしないこと
// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbFyu 対応）。呼び出し元の採番バグ・
// 再試行ミスがあっても台帳の不変条件（batch_seq ごとに 1 エントリ）を守る。
#[test]
fn table10_log_batch_rejects_duplicate_seq_and_preserves_existing_entry() {
    let path = unique_db_path("duplicate-batch-seq-rejected");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut txn = storage.begin_write().expect("begin_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    txn.log_batch(0).expect("log_batch first write");
    txn.commit().expect("commit first batch");

    let mut txn = storage.begin_write().expect("begin_write");
    txn.put(1, &row(&[2.0], &[2])).expect("put row 1");
    let err = txn
        .log_batch(0)
        .expect_err("duplicate batch_seq must be rejected");
    assert!(matches!(
        err,
        engine::storage::StorageError::DuplicateBatchSeq(0)
    ));
    // トランザクション自体は commit せず破棄し、行 1 も台帳の上書きも確定させない
    // （呼び出し元は log_batch のエラーを見てトランザクション全体を中断する想定）。
    txn.abort().expect("abort after duplicate detected");

    assert_eq!(
        storage.scan_batch_log().expect("scan_batch_log"),
        vec![(0, 1)],
        "existing batch_log entry must remain unchanged"
    );
    let get_err = storage
        .get(1)
        .expect_err("row from aborted duplicate txn must not exist");
    assert!(matches!(
        get_err,
        engine::storage::StorageError::NotFound(1)
    ));
}

// 対象ビヘイビア: TABLE-10。log_batch はもう引数で row_count を受け取らず、直近の
// log_batch 以降に実際に put した件数だけを記録する。呼び出し元が put の件数と
// 無関係な値を台帳へ書き込む経路が存在しないことを確認する
// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbQ7l 対応）。
#[test]
fn table10_log_batch_records_actual_put_count_not_caller_supplied_value() {
    let path = unique_db_path("log-batch-tracks-actual-puts");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // 1 バッチ目: 3 行 put してから log_batch。
    let mut txn = storage.begin_write().expect("begin_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    txn.put(1, &row(&[2.0], &[2])).expect("put row 1");
    txn.put(2, &row(&[3.0], &[3])).expect("put row 2");
    txn.log_batch(0).expect("log_batch");
    txn.commit().expect("commit batch 0");

    // 2 バッチ目: 1 回も put せず log_batch すると、直近 log_batch 以降の実績どおり
    // 0 が記録される（呼び出し元が任意の値を申告する余地がないことの確認）。
    let mut txn = storage.begin_write().expect("begin_write");
    txn.log_batch(1).expect("log_batch with zero puts");
    txn.commit().expect("commit batch 1");

    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(batch_log, vec![(0, 3), (1, 0)]);
    let total_from_log: u64 = batch_log.iter().map(|(_, count)| count).sum();
    let (rows, _) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(
        total_from_log,
        rows.len() as u64,
        "batch_log row_count total must always match actually-put rows"
    );
}

// 対象ビヘイビア: TABLE-10。同一トランザクション内で同じ行 ID へ 2 回 put（upsert に
// よる上書き）しても、log_batch が記録する行数は実在する行数（新規挿入数）のままで
// あり、put 回数と一致しないこと（PR #129 codex レビュー PRRT_kwDOUAKASM6bbc_I 対応）。
// `redb::Table::insert` は既存 ID を上書きできるため、上書きも新規挿入と同様にカウント
// すると「台帳の row_count 合計 == 行総数」という契約を公開 API だけで破れてしまう。
#[test]
fn table10_log_batch_does_not_double_count_overwritten_id_within_same_batch() {
    let path = unique_db_path("log-batch-no-double-count-overwrite");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut txn = storage.begin_write().expect("begin_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0 (first)");
    // 同じ行 ID 0 へ 2 回目の put（upsert による上書き）。新規挿入ではないため
    // pending_row_count は増えないはず。
    txn.put(0, &row(&[9.0], &[9]))
        .expect("put row 0 (overwrite)");
    txn.put(1, &row(&[2.0], &[2])).expect("put row 1 (new)");
    txn.log_batch(0).expect("log_batch");
    txn.commit().expect("commit");

    // 実在行は id=0（上書き後の値）・id=1 の 2 行のみ。
    let (rows, _) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(rows.len(), 2);
    assert_eq!(storage.get(0).expect("get row 0").embedding, [9.0]);

    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(
        batch_log,
        vec![(0, 2)],
        "overwrite of an existing id must not inflate the logged row count"
    );
}
