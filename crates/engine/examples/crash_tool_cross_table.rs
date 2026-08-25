//! TASK-90（対象ビヘイビア: TABLE-10。ポインタ: `docs/spec/05-tasks.md` TASK-90）の
//! 2 テーブル横断トランザクション・クラッシュ耐性回帰テスト用ツール。
//!
//! `engine::txn::BatchWriteTxn`（[`ROWS_TABLE`] + `BATCH_LOG_TABLE` を同一
//! トランザクションで扱う `log_batch` を持つ TASK-90 専用の型）の公開 API のみを
//! 使うブラックボックスバイナリで、
//! `scripts/crash_test_cross_table.sh` から `write` サブコマンドをバックグラウンド起動され
//! SIGKILL される想定、続けて `verify` サブコマンドで再オープン後の内容整合性を
//! 検証される想定で作られている。単一テーブル・単一行コミットを検証する
//! `crash_tool.rs`（TASK-142・PERSIST-1。本ブランチの時点では未マージにつき、
//! マージ後に write/verify の出力プロトコル様式を追随予定）と役割は近いが、本ツールは
//! 「行テーブルとバッチ台帳という 2 テーブルが常に運命を共にする」ことをオラクルとする点で
//! 異なる（単一テーブルの単一行コミットではなく、複数行 + バッチ台帳 1 エントリを
//! 1 トランザクションでコミットし続ける）。
//!
//! サブコマンド:
//! - `write <db_path>`: `BATCH` 件の行 put + `log_batch` を 1 トランザクションで
//!   コミットし続ける。`COMMITTED batch=<seq> rows=<total>` を stdout へ出力する
//!   （`scripts/crash_test_cross_table.sh` の開始同期点）。再起動時は `scan_page`（行 ID）・
//!   `batch_log_max_seq`（バッチ通番。Issue #132 で `scan_batch_log` 全件走査から
//!   移行。台帳件数の上限に依存せず再開できる）から採番を再開する。
//! - `verify <db_path>`: 行 ID の 0 起点連続性・内容一致（行）、`batch_seq` の
//!   0 起点連続性（台帳）、さらに「台帳の row_count 合計 == 行総数」（テーブル間整合）と
//!   「行総数が BATCH の倍数」（バッチ整合）を検証する。結果を
//!   `RESULT ok=true rows=<N> batches=<M>` / `RESULT ok=false reason=<...>` の 1 行で
//!   stdout へ出力し、失敗時は非 0 終了する（fail-closed。空虚な成功を許さない）。

use std::io::Write as _;

use engine::storage::{RowInput, Storage, StorageError, Visibility};

/// write/verify で固定して使うテナント識別子（単一テナントのクラッシュ耐性検証が
/// 目的で、RLS ポリシー評価そのものは対象外。TASK-142・PERSIST-1 のクラッシュ耐性
/// ツールと同方針だが、当該ツールは本ブランチの時点では未マージ）。
const CRASH_TOOL_TENANT_ID: &str = "crash-tool-cross-table-tenant";

/// 1 トランザクションで書き込む行数（バッチサイズ）。行総数はこの値の倍数になる
/// （verify のバッチ整合オラクル）。
const BATCH: u64 = 10;
/// 生成する埋め込みベクトルの次元数。
const EMBEDDING_DIM: usize = 64;
/// 生成するメタデータバイト列の長さ。
const METADATA_LEN: usize = 32;
/// `scan_page` の 1 回あたり取得件数。
const PAGE_LIMIT: u32 = 10_000;
/// write の自走上限バッチ数（安全弁）。`scripts/crash_test_cross_table.sh` は書き込み
/// 進行中に SIGKILL する想定のため通常はここへ到達しない。到達した場合は無限ループに
/// せず正常終了する（security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_BATCHES: u64 = 500_000;

fn main() {
    let mut args = std::env::args().skip(1);
    let sub = args.next();
    let path = args.next();
    let (sub, path) = match (sub, path) {
        (Some(sub), Some(path)) => (sub, path),
        _ => {
            eprintln!("usage: crash_tool_cross_table <write|verify> <db_path>");
            std::process::exit(2);
        }
    };

    let exit_code = match sub.as_str() {
        "write" => run_write(&path),
        "verify" => run_verify(&path),
        other => {
            eprintln!("ERROR: unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(exit_code);
}

/// 決定的な擬似乱数生成器（splitmix64。追加依存を増やさない方針 - dependency-policy.md。
/// TASK-142・PERSIST-1 のクラッシュ耐性ツールと同一実装を意図しているが、当該ツールは
/// 本ブランチの時点では未マージにつきマージ後に整合を確認予定）。
struct DeterministicRng(u64);

impl DeterministicRng {
    fn seeded(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// 行 ID から埋め込み・メタデータを決定的に導出する（write が生成した内容を verify が
/// 同じ関数で再計算し突き合わせる。TASK-142・PERSIST-1 の `derive_row` 相当と同方針だが、
/// 当該実装は本ブランチの時点では未マージ）。
fn derive_row(id: u64) -> (Vec<f32>, Vec<u8>) {
    let mut rng = DeterministicRng::seeded(id);
    let embedding = (0..EMBEDDING_DIM)
        .map(|_| {
            let word = rng.next_u64();
            ((word % 200_001) as f32 - 100_000.0) / 1000.0
        })
        .collect();
    let metadata = (0..METADATA_LEN)
        .map(|_| (rng.next_u64() % 256) as u8)
        .collect();
    (embedding, metadata)
}

/// 既存データから採番を再開するための状態（行 ID の続き・バッチ通番の続き）。
struct ResumeState {
    next_row_id: u64,
    next_batch_seq: u64,
}

/// `scan_page`（行テーブル）・`batch_log_max_seq`（バッチ台帳）の両方から採番を再開する
/// （クラッシュ → 再起動 → 再クラッシュを反復するために必要）。バッチ台帳側は
/// `scan_batch_log` の全件走査ではなく `batch_log_max_seq`（redb の B-tree 末尾キー
/// 取得のみ・O(log n)）を使うため、台帳件数が `MAX_BATCH_LOG_ROWS` を超えても
/// 再開経路自体は上限に依存しない（Issue #132）。
fn find_resume_state(storage: &Storage) -> Result<ResumeState, String> {
    let mut cursor: Option<u64> = None;
    let mut last_row_id: Option<u64> = None;
    loop {
        let (rows, next_cursor) = storage
            .scan_page(cursor, PAGE_LIMIT)
            .map_err(|e| format!("scan_page failed: {e}"))?;
        if let Some(last) = rows.last() {
            last_row_id = Some(last.id);
        }
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let next_row_id = match last_row_id {
        Some(id) => id
            .checked_add(1)
            .ok_or_else(|| "row id overflow while resuming".to_string())?,
        None => 0,
    };

    let next_batch_seq = match storage
        .batch_log_max_seq()
        .map_err(|e| format!("batch_log_max_seq failed: {e}"))?
    {
        Some(max_seq) => max_seq
            .checked_add(1)
            .ok_or_else(|| "batch seq overflow while resuming".to_string())?,
        None => 0,
    };

    Ok(ResumeState {
        next_row_id,
        next_batch_seq,
    })
}

fn run_write(path: &str) -> i32 {
    match write_inner(path) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}

fn write_inner(path: &str) -> Result<(), String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    let resume = find_resume_state(&storage)?;
    let mut next_row_id = resume.next_row_id;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    for batch_no in 0..MAX_BATCHES {
        // 既存台帳の最大通番が u64::MAX 付近の場合、無検査の加算だと debug では panic、
        // release ではラップアラウンドして既存の batch_seq=0 を再利用してしまう
        // （`log_batch` は既存 batch_seq への上書きを DuplicateBatchSeq で拒否するが、
        // 折り返し自体は checked_add で事前に検出し fail-closed に終了する方が
        // 早期に・分かりやすいエラーで失敗できる）。
        let batch_seq = resume
            .next_batch_seq
            .checked_add(batch_no)
            .ok_or_else(|| "batch seq overflow while writing".to_string())?;
        // TABLE-10 の 2 テーブル横断コミットそのものを検証する経路。同一トランザクション
        // 内で行テーブル（BATCH 件）とバッチ台帳（1 エントリ）の両方へ書き込む。
        let mut txn = storage
            .begin_batch_write()
            .map_err(|e| format!("begin_batch_write failed: {e}"))?;
        for _ in 0..BATCH {
            let (embedding, metadata) = derive_row(next_row_id);
            let row = RowInput {
                tenant_id: CRASH_TOOL_TENANT_ID,
                visibility: Visibility::Public,
                embedding: &embedding,
                metadata: &metadata,
            };
            txn.put(next_row_id, &row)
                .map_err(|e| format!("put failed: {e}"))?;
            next_row_id = next_row_id
                .checked_add(1)
                .ok_or_else(|| "row id overflow after put".to_string())?;
        }
        txn.log_batch(batch_seq)
            .map_err(|e| format!("log_batch failed: {e}"))?;
        txn.commit().map_err(|e| format!("commit failed: {e}"))?;

        // 進捗行はスクリプト側が「書き込みが実際に進み始めた」ことを検知する同期点
        // なので、バッファリングで遅延しないよう毎回明示的に flush する。
        writeln!(handle, "COMMITTED batch={batch_seq} rows={next_row_id}")
            .map_err(|e| format!("stdout write failed: {e}"))?;
        handle
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;
    }
    Ok(())
}

fn run_verify(path: &str) -> i32 {
    match verify_inner(path) {
        Ok((rows, batches)) => {
            println!("RESULT ok=true rows={rows} batches={batches}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

fn verify_inner(path: &str) -> Result<(u64, u64), String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;

    // 行テーブル側: ID の 0 起点連続性・内容一致（PERSIST-1 のオラクルと同方針）。
    let mut cursor: Option<u64> = None;
    let mut expected_id: u64 = 0;
    let mut total_rows: u64 = 0;
    loop {
        let (rows, next_cursor) = storage
            .scan_page(cursor, PAGE_LIMIT)
            .map_err(|e| format!("scan_page failed: {e}"))?;
        for row in &rows {
            if row.id != expected_id {
                return Err(format!(
                    "id gap or disorder: expected={expected_id} actual={}",
                    row.id
                ));
            }
            if row.tenant_id != CRASH_TOOL_TENANT_ID {
                return Err(format!(
                    "tenant_id mismatch at id={}: expected={CRASH_TOOL_TENANT_ID} actual={}",
                    row.id, row.tenant_id
                ));
            }
            if row.visibility != Visibility::Public {
                return Err(format!(
                    "visibility mismatch at id={}: expected=Public actual={:?}",
                    row.id, row.visibility
                ));
            }
            let (expected_embedding, expected_metadata) = derive_row(row.id);
            if row.embedding != expected_embedding {
                return Err(format!("embedding mismatch at id={}", row.id));
            }
            if row.metadata != expected_metadata {
                return Err(format!("metadata mismatch at id={}", row.id));
            }
            expected_id = expected_id
                .checked_add(1)
                .ok_or_else(|| "row id overflow during verify".to_string())?;
            total_rows = total_rows
                .checked_add(1)
                .ok_or_else(|| "row count overflow during verify".to_string())?;
        }
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    // バッチ台帳側: batch_seq の 0 起点連続性・row_count 合計。
    //
    // `StorageError::ScanLimitExceeded` の `Display` はそのまま使わない（Issue #131・
    // PR #193 codex レビュー再指摘対応）: 固定文言 `"scan limit exceeded: use scan_page"`
    // は `Storage::scan` 専用の代替 API 案内であり、台帳（`scan_batch_log`。ページング API
    // を持たない）には当てはまらない。加えて台帳エントリ数を削減する compact/rotate 相当の
    // API・運用手順も本リポには存在しない（通常の compaction は論理エントリ数を減らさず、
    // rotation で台帳を捨てれば検証対象そのものを失うため、いずれも実行可能な代替手段では
    // ない: PR #193 codex レビュー再指摘対応）。実行不能な手段を示唆せず、「このツールでは
    // 上限を超えた台帳を検証できない」という事実のみを明示する
    // （`storage.rs::Storage::scan_batch_log` のドキュメンテーションコメント参照）。
    let mut batch_log = storage.scan_batch_log().map_err(|e| match e {
        StorageError::ScanLimitExceeded => {
            "scan_batch_log failed: batch log exceeds the scan limit of this tool; \
             cross-table verification cannot be performed on this ledger \
             (no paginated ledger API is available yet)"
                .to_string()
        }
        other => format!("scan_batch_log failed: {other}"),
    })?;
    batch_log.sort_by_key(|(seq, _)| *seq);
    let mut expected_seq: u64 = 0;
    let mut total_from_log: u64 = 0;
    for (seq, row_count) in &batch_log {
        if *seq != expected_seq {
            return Err(format!(
                "batch seq gap or disorder: expected={expected_seq} actual={seq}"
            ));
        }
        if *row_count != BATCH {
            return Err(format!(
                "batch row_count mismatch at seq={seq}: expected={BATCH} actual={row_count}"
            ));
        }
        total_from_log = total_from_log
            .checked_add(*row_count)
            .ok_or_else(|| "batch row_count sum overflow during verify".to_string())?;
        expected_seq = expected_seq
            .checked_add(1)
            .ok_or_else(|| "batch seq overflow during verify".to_string())?;
    }
    let total_batches = batch_log.len() as u64;

    // 空虚な成功（vacuous pass）を拒否する: DB が空/未作成のまま検証をすり抜けない。
    if total_rows == 0 || total_batches == 0 {
        return Err("no rows or no batches found".to_string());
    }
    // テーブル間整合（TABLE-10 の中核オラクル）: バッチ台帳の row_count 合計と
    // 行テーブルの総行数が一致すること。片方だけコミットされて他方が失われていない
    // （＝2 テーブル横断コミットが原子的だった）ことの直接的な確認。
    if total_from_log != total_rows {
        return Err(format!(
            "cross-table mismatch: batch_log total={total_from_log} row_count={total_rows}"
        ));
    }
    // バッチ整合: 行総数は BATCH の倍数でなければならない（部分バッチが存在しない）。
    if !total_rows.is_multiple_of(BATCH) {
        return Err(format!(
            "row count {total_rows} is not a multiple of BATCH ({BATCH}), partial batch suspected"
        ));
    }

    Ok((total_rows, total_batches))
}
