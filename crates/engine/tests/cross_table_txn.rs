//! `engine::txn::BatchWriteTxn`（[`ROWS_TABLE`] とバッチ台帳
//! [`crate::storage::BATCH_LOG_TABLE`] を同一トランザクションで扱う TASK-90 専用の型）の
//! 統合テスト（対象ビヘイビア: TABLE-10。ポインタ: `docs/spec/05-tasks.md` TASK-90・
//! `docs/spec/04-behavior/data-model.md` TABLE-10）。
//!
//! `crates/engine/examples/crash_tool_cross_table.rs` + `scripts/crash_test_cross_table.sh`
//! はプロセス外からの SIGKILL に対する耐性を検証するのに対し、本ファイルは
//! プロセス内テストとして「commit で両テーブルへ原子的に反映される」「commit 前に drop・
//! `abort` した場合は両テーブルとも破棄される」という 2 テーブル横断トランザクションの
//! 原子性そのものを検証する（クラッシュ回帰テストとは独立に、通常経路での正しさを保証する）。
//!
//! `BatchWriteTxn` は TABLE-10 専用の型で、バッチ台帳を使わない TABLE-3 の素の複数行
//! コミット（`engine::txn::WriteTxn`）は `crates/engine/tests/txn_isolation.rs` が
//! 別途カバーする（PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf 対応で型を分離した。
//! `txn.rs` の `BatchWriteTxn` ドキュメントコメント「型分離の理由」参照）。

use engine::storage::{RowInput, Storage, Visibility};

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

// 対象ビヘイビア: TABLE-10（詳細は関数名・ポインタ: docs/spec/04-behavior/data-model.md）。
#[test]
fn table10_commit_reflects_both_tables_atomically() {
    let path = unique_db_path("commit-both");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let embedding = [1.0_f32, 2.0, 3.0];
    let metadata = [9_u8, 8, 7];

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
    txn.put(0, &row(&embedding, &metadata)).expect("put row 0");
    txn.put(1, &row(&embedding, &metadata)).expect("put row 1");
    txn.log_batch(0).expect("log_batch");
    txn.commit().expect("commit");

    assert_eq!(
        storage.get("tenant-a", 0).expect("get row 0").embedding,
        embedding
    );
    assert_eq!(
        storage.get("tenant-a", 1).expect("get row 1").embedding,
        embedding
    );
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
        let mut txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
        txn.log_batch(0).expect("log_batch");
        // commit も abort も呼ばずスコープを抜ける。
    }

    let get_err = storage
        .get("tenant-a", 0)
        .expect_err("row must not exist after drop");
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

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    txn.log_batch(0).expect("log_batch");
    txn.abort().expect("abort");

    let get_err = storage
        .get("tenant-a", 0)
        .expect_err("row must not exist after abort");
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
            let mut txn = storage.begin_batch_write().expect("begin_batch_write");
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

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    txn.log_batch(0).expect("log_batch first write");
    txn.commit().expect("commit first batch");

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
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
        .get("tenant-a", 1)
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

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    txn.put(1, &row(&[2.0], &[2])).expect("put row 1");
    txn.put(2, &row(&[3.0], &[3])).expect("put row 2");
    txn.log_batch(0).expect("log_batch");
    txn.commit().expect("commit batch 0");

    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(batch_log, vec![(0, 3)]);
    let total_from_log: u64 = batch_log.iter().map(|(_, count)| count).sum();
    let (rows, _) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(
        total_from_log,
        rows.len() as u64,
        "batch_log row_count total must always match actually-put rows"
    );
}

// 対象ビヘイビア: TABLE-10。BatchWriteTxn は log_batch を一度も呼ばない commit を
// 一切許さない（PR #129 codex レビュー PRRT_kwDOUAKASM6bbnm4・PRRT_kwDOUAKASM6bbyWf
// 対応。put した行がある限り、log_batch で台帳へ記録しないと commit できない）。
// 台帳を使わない「素の複数行コミット」用途（TABLE-3）は型ごと分離した
// `engine::txn::WriteTxn`（`crates/engine/tests/txn_isolation.rs` がカバー）の責務であり、
// `BatchWriteTxn` はこの検証を無条件に適用してよい。
#[test]
fn table10_batch_write_txn_rejects_commit_of_unlogged_put_even_without_any_log_batch_call() {
    let path = unique_db_path("commit-rejects-unlogged-with-no-log-batch-call");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    // log_batch を一度も呼ばずに commit を試みる。
    let err = txn
        .commit()
        .expect_err("commit without any log_batch call must be rejected");
    assert!(matches!(
        err,
        engine::storage::StorageError::UnloggedRows(1)
    ));

    let get_err = storage
        .get("tenant-a", 0)
        .expect_err("row from rejected commit must not exist");
    assert!(matches!(
        get_err,
        engine::storage::StorageError::NotFound(0)
    ));
}

// 対象ビヘイビア: TABLE-10。既知の制限を明文化する回帰テスト
// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf 対応）。`engine::txn::WriteTxn`
// （TABLE-3・台帳を経由しない）と `BatchWriteTxn`（TABLE-10）を同一 DB・同一
// `ROWS_TABLE` に対して混在させると、型分離だけでは「台帳の row_count 合計 ==
// 行総数」という不変条件を強制できない。`Storage::put`・`Storage::put_batch` も
// 同様に台帳を経由せず `ROWS_TABLE` へ書き込めるため、この不変条件は
// `BatchWriteTxn` だけを使い続けた場合にのみ保証される契約であることを
// `txn.rs`（`BatchWriteTxn` ドキュメントコメント「契約の適用範囲」）・
// `storage.rs`（`BATCH_LOG_TABLE` ドキュメントコメント）に明記している。
// 本テストは、その契約外の挙動が将来のリファクタで無自覚に変わっていないかを
// 検出するためのピン留めであり、「これが正しい」という主張ではない。
#[test]
fn table10_mixing_plain_write_txn_with_batch_write_txn_is_a_documented_out_of_contract_limitation()
{
    let path = unique_db_path("mixing-plain-and-batch-write-txn-is-out-of-contract");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // TABLE-3: 台帳を経由しない素の WriteTxn で行 0 を commit する。
    let mut plain_txn = storage.begin_write().expect("begin_write");
    plain_txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    plain_txn.commit().expect("commit via plain WriteTxn");

    // TABLE-10: 別トランザクションで BatchWriteTxn を使い、行 1 だけを台帳へ記録する。
    // BatchWriteTxn 単体としては不変条件（UnloggedRows・EmptyBatch・重複禁止）を
    // すべて満たしており、この commit 自体は正しく成功する。
    let mut batch_txn = storage.begin_batch_write().expect("begin_batch_write");
    batch_txn.put(1, &row(&[2.0], &[2])).expect("put row 1");
    batch_txn.log_batch(0).expect("log_batch for row 1");
    batch_txn.commit().expect("commit via BatchWriteTxn");

    // 実在行は 2 行（id=0, id=1）だが、台帳合計は BatchWriteTxn が扱った 1 行分のみ。
    // これが「BatchWriteTxn だけを使った場合にのみ不変条件が成立する」という契約の
    // 適用範囲外の挙動そのものであり、本テストはこれを意図的に許容・記録する。
    let (rows, _) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(rows.len(), 2);
    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(batch_log, vec![(0, 1)]);
}

// 対象ビヘイビア: TABLE-10。契約の適用範囲外を記録するピン留めであり、正しさの主張ではない
// （Issue #133・`docs/design/batch-ledger-scope.md` 参照）。`Storage::put`（バッチ台帳を経由
// しない別経路）と `BatchWriteTxn` を同一 DB・同一 `ROWS_TABLE` に対して混在させると、
// `WriteTxn` との混在（既存テスト参照）と同様に「台帳の row_count 合計 == 行総数」という
// 不変条件が成立しなくなることを固定する。
#[test]
fn table10_mixing_storage_put_with_batch_write_txn_is_a_documented_out_of_contract_limitation() {
    let path = unique_db_path("mixing-storage-put-and-batch-write-txn-is-out-of-contract");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // 台帳を経由しない Storage::put で行 0 を書き込む。
    storage
        .put(0, &row(&[1.0], &[1]))
        .expect("storage put row 0");

    // BatchWriteTxn で行 1 だけを台帳へ記録する。BatchWriteTxn 単体としては不変条件を
    // すべて満たしており、この commit 自体は正しく成功する。
    let mut batch_txn = storage.begin_batch_write().expect("begin_batch_write");
    batch_txn.put(1, &row(&[2.0], &[2])).expect("put row 1");
    batch_txn.log_batch(0).expect("log_batch for row 1");
    batch_txn.commit().expect("commit via BatchWriteTxn");

    let (rows, _) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(rows.len(), 2);
    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(batch_log, vec![(0, 1)]);
}

// 対象ビヘイビア: TABLE-10。契約の適用範囲外を記録するピン留めであり、正しさの主張ではない
// （Issue #133・`docs/design/batch-ledger-scope.md` 参照）。`Storage::put_batch` と
// `BatchWriteTxn` の混在でも同様に不変条件が成立しなくなることを固定する。
#[test]
fn table10_mixing_storage_put_batch_with_batch_write_txn_is_a_documented_out_of_contract_limitation(
) {
    let path = unique_db_path("mixing-storage-put-batch-and-batch-write-txn-is-out-of-contract");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // 台帳を経由しない Storage::put_batch で行 0・1 を書き込む。
    storage
        .put_batch(&[(0, row(&[1.0], &[1])), (1, row(&[2.0], &[2]))])
        .expect("storage put_batch rows 0,1");

    // BatchWriteTxn で行 2 だけを台帳へ記録する。
    let mut batch_txn = storage.begin_batch_write().expect("begin_batch_write");
    batch_txn.put(2, &row(&[3.0], &[3])).expect("put row 2");
    batch_txn.log_batch(0).expect("log_batch for row 2");
    batch_txn.commit().expect("commit via BatchWriteTxn");

    let (rows, _) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(rows.len(), 3);
    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(batch_log, vec![(0, 1)]);
}

// 対象ビヘイビア: TABLE-10。契約の適用範囲外を記録するピン留めであり、正しさの主張ではない
// （Issue #133・`docs/design/batch-ledger-scope.md` 参照）。`BatchWriteTxn` で書き込んだ後に
// 台帳非経由の `Storage::put` を upsert・新規挿入いずれで呼んでも、台帳の値は一切更新
// されないことを固定する（`Storage::put` からは台帳の存在自体が見えないため）。
#[test]
fn table10_storage_put_after_batch_write_txn_leaves_ledger_unchanged() {
    let path = unique_db_path("storage-put-after-batch-write-txn-leaves-ledger-unchanged");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut batch_txn = storage.begin_batch_write().expect("begin_batch_write");
    batch_txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    batch_txn.log_batch(0).expect("log_batch for row 0");
    batch_txn.commit().expect("commit via BatchWriteTxn");

    // 既存 ID への upsert。
    storage
        .put(0, &row(&[9.0], &[9]))
        .expect("storage put upsert row 0");
    // 新規 ID への挿入。
    storage
        .put(1, &row(&[2.0], &[2]))
        .expect("storage put row 1");

    let (rows, _) = storage.scan_page(None, 100).expect("scan_page");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        storage.get("tenant-a", 0).expect("get row 0").embedding,
        [9.0]
    );
    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(
        batch_log,
        vec![(0, 1)],
        "storage put (upsert or new) must not update the batch ledger"
    );
}

// 対象ビヘイビア: TABLE-10。直近の log_batch（または WriteTxn 生成）以降 1 件も put
// していない状態で log_batch を呼ぶと EmptyBatch で拒否され、ゼロ件エントリを台帳へ
// 残さないこと（PR #129 codex レビュー PRRT_kwDOUAKASM6bbnm7 対応）。
#[test]
fn table10_log_batch_with_no_prior_put_is_rejected_as_empty_batch() {
    let path = unique_db_path("log-batch-empty-batch-rejected");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
    let err = txn
        .log_batch(0)
        .expect_err("log_batch with zero pending puts must be rejected");
    assert!(matches!(err, engine::storage::StorageError::EmptyBatch));
    txn.abort().expect("abort after rejected log_batch");

    assert_eq!(
        storage.scan_batch_log().expect("scan_batch_log"),
        Vec::<(u64, u64)>::new(),
        "no zero-count entry must be persisted"
    );
}

// 対象ビヘイビア: TABLE-10。log_batch を 1 回以上呼んで TABLE-10 の契約に参加した
// トランザクションで、log_batch 後に put した行を再び log_batch せず commit すると
// UnloggedRows で拒否され、未台帳行が確定しないこと
// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbnm4 対応）。
#[test]
fn table10_commit_rejects_rows_put_after_last_log_batch() {
    let path = unique_db_path("commit-rejects-unlogged-rows");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
    txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
    txn.log_batch(0).expect("log_batch for row 0");
    // log_batch 後にさらに put するが、この行は 2 度目の log_batch で記録しないまま
    // commit を試みる。
    txn.put(1, &row(&[2.0], &[2])).expect("put row 1");
    let err = txn.commit().expect_err("unlogged row must reject commit");
    assert!(matches!(
        err,
        engine::storage::StorageError::UnloggedRows(1)
    ));

    // commit 自体が失敗しているため、行 0（log_batch 済み）を含めトランザクション
    // 全体が未確定のまま。再オープンしても何も残っていないことを確認する。
    let get_err = storage
        .get("tenant-a", 0)
        .expect_err("row from rejected commit must not exist");
    assert!(matches!(
        get_err,
        engine::storage::StorageError::NotFound(0)
    ));
    assert_eq!(
        storage.scan_batch_log().expect("scan_batch_log"),
        Vec::<(u64, u64)>::new()
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

    let mut txn = storage.begin_batch_write().expect("begin_batch_write");
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
    assert_eq!(
        storage.get("tenant-a", 0).expect("get row 0").embedding,
        [9.0]
    );

    let batch_log = storage.scan_batch_log().expect("scan_batch_log");
    assert_eq!(
        batch_log,
        vec![(0, 2)],
        "overwrite of an existing id must not inflate the logged row count"
    );
}

// 対象ビヘイビア: TABLE-10（Issue #132）。`Storage::batch_log_max_seq` は複数バッチ
// コミット後、`scan_batch_log` の全件走査から求めた最大値と一致すること
// （公開 API 経由での整合性確認。単体での「キー最大値」契約は storage.rs の
// `mod tests` が直接検証する）。
#[test]
fn table10_batch_log_max_seq_matches_scan_batch_log_max_after_multiple_batches() {
    let path = unique_db_path("batch-log-max-seq-multiple-batches");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    assert_eq!(
        storage.batch_log_max_seq().expect("batch_log_max_seq"),
        None,
        "empty ledger must report None before any batch is committed"
    );

    for batch_seq in 0..3u64 {
        let mut txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.put(batch_seq, &row(&[1.0], &[1]))
            .expect("put row for batch");
        txn.log_batch(batch_seq).expect("log_batch");
        txn.commit().expect("commit");
    }

    let expected_max = storage
        .scan_batch_log()
        .expect("scan_batch_log")
        .iter()
        .map(|(seq, _)| *seq)
        .max();
    assert_eq!(expected_max, Some(2));
    assert_eq!(
        storage.batch_log_max_seq().expect("batch_log_max_seq"),
        expected_max
    );
}

// 対象ビヘイビア: TABLE-10（Issue #132）。DB を close してから再オープンしても
// 最大通番が一致すること（採番再開経路は再起動後に呼ばれるため、永続化された値が
// 正しく読めることを確認する）。
#[test]
fn table10_batch_log_max_seq_survives_reopen() {
    let path = unique_db_path("batch-log-max-seq-reopen");
    let _cleanup = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage");
        let mut txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
        txn.log_batch(5).expect("log_batch seq=5");
        txn.commit().expect("commit");
    }

    let reopened = Storage::open(&path).expect("reopen storage");
    assert_eq!(
        reopened
            .batch_log_max_seq()
            .expect("batch_log_max_seq after reopen"),
        Some(5)
    );
}

// 対象ビヘイビア: TABLE-10（Issue #132）。commit 前に drop したバッチは台帳へ反映
// されず、最大通番も `None` のままであること（原子性オラクルとの整合。
// `table10_drop_without_commit_discards_both_tables` と同種の観点を
// `batch_log_max_seq` 側でも確認する）。
#[test]
fn table10_batch_log_max_seq_ignores_dropped_uncommitted_batch() {
    let path = unique_db_path("batch-log-max-seq-drop-uncommitted");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    {
        let mut txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.put(0, &row(&[1.0], &[1])).expect("put row 0");
        txn.log_batch(9).expect("log_batch seq=9");
        // commit せずスコープを抜けて drop する。
    }

    assert_eq!(
        storage.batch_log_max_seq().expect("batch_log_max_seq"),
        None,
        "an uncommitted (dropped) batch must not be visible to batch_log_max_seq"
    );
}
