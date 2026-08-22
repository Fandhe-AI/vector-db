//! TASK-142（対象ビヘイビア: PERSIST-1。ポインタ: `docs/spec/05-tasks.md` TASK-142）の
//! クラッシュ耐性回帰テスト用ツール。
//!
//! `engine::storage::Storage` の公開 API のみを使うブラックボックスバイナリで、
//! `scripts/crash_test.sh` から `write` サブコマンドをバックグラウンド起動され
//! SIGKILL される想定、続けて `verify` サブコマンドで再オープン後の内容整合性を
//! 検証される想定で作られている。プロセス内テスト（`crates/engine/tests/persistence.rs`）
//! では表現できない「プロセス外からの強制終了 → 再起動」を成立させるための補助ツールであり、
//! engine クレートの内部実装（`storage.rs` の非公開関数等）には依存しない。
//!
//! サブコマンド:
//! - `write <db_path>`: 固定バッチサイズで行を無限に近い反復でコミットし続ける。
//!   1 バッチごとに `COMMITTED batch=<n> rows=<total>` を stdout へ出力する
//!   （`scripts/crash_test.sh` が「書き込みが実際に進み始めてから kill する」ための
//!   同期点）。再起動時は既存データの最大行 ID の続きから採番する。
//! - `verify <db_path>`: 全行を上限付きページングで走査し、破損 0 件・ID 連続・
//!   内容一致・行数がバッチサイズの倍数（部分コミット無し）を確認する。
//!   結果を `RESULT ok=true rows=<N>` / `RESULT ok=false reason=<...>` の 1 行で
//!   stdout へ出力し、失敗時は非 0 終了する（fail-closed。空虚な成功を許さない）。

use std::io::Write as _;

use engine::storage::{RowInput, Storage};

/// 1 バッチのコミット行数。`verify` 側は総行数がこの倍数であることを
/// 部分コミット（トランザクション原子性の崩れ）が無いことのオラクルとして使う。
const BATCH_SIZE: u64 = 10;
/// 生成する埋め込みベクトルの次元数。
const EMBEDDING_DIM: usize = 64;
/// 生成するメタデータバイト列の長さ。
const METADATA_LEN: usize = 32;
/// `scan_page` の 1 回あたり取得件数。
const PAGE_LIMIT: u32 = 10_000;
/// write の自走上限（安全弁）。`scripts/crash_test.sh` は書き込み進行中に SIGKILL する
/// 想定のため通常はここへ到達しない。到達した場合はテストダブルとして正常終了する
/// （無限ループにしない。security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_BATCHES: u64 = 500_000;

fn main() {
    let mut args = std::env::args().skip(1);
    let sub = args.next();
    let path = args.next();
    let (sub, path) = match (sub, path) {
        (Some(sub), Some(path)) => (sub, path),
        _ => {
            eprintln!("usage: crash_tool <write|verify> <db_path>");
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

/// 決定的な擬似乱数生成器（splitmix64）。追加依存を増やさず（dependency-policy.md）、
/// 行 ID から埋め込み・メタデータを再現可能に導出するために自前実装する。
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

/// 行 ID から埋め込み・メタデータを決定的に導出する。`write` が生成した内容を
/// `verify` が同じ関数で再計算し、ディスク上の値と突き合わせる（write/verify 間の
/// 唯一の共有契約）。ビット列がそのままエンコード/デコードされる
/// （`storage.rs` の固定レイアウト）ため、NaN 等の比較不能な値を避けて
/// 整数演算のみで値を組み立てる。
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

/// 既存データの最大行 ID の次から採番を再開するため、`scan_page` のページングで
/// 現在の最大行 ID を求める（クラッシュ → 再起動 → 再クラッシュを反復するために必要。
/// `write` はプロセス起動のたびにこの関数から採番を引き継ぐ）。
fn find_next_id(storage: &Storage) -> Result<u64, String> {
    let mut cursor: Option<u64> = None;
    let mut last_id: Option<u64> = None;
    loop {
        let (rows, next_cursor) = storage
            .scan_page(cursor, PAGE_LIMIT)
            .map_err(|e| format!("scan_page failed: {e}"))?;
        if let Some(last) = rows.last() {
            last_id = Some(last.id);
        }
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    match last_id {
        Some(id) => id
            .checked_add(1)
            .ok_or_else(|| "row id overflow while resuming".to_string()),
        None => Ok(0),
    }
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
    let mut next_id = find_next_id(&storage)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    for batch_no in 1..=MAX_BATCHES {
        let mut derived: Vec<(u64, Vec<f32>, Vec<u8>)> = Vec::with_capacity(BATCH_SIZE as usize);
        for offset in 0..BATCH_SIZE {
            let id = next_id
                .checked_add(offset)
                .ok_or_else(|| "row id overflow while writing".to_string())?;
            let (embedding, metadata) = derive_row(id);
            derived.push((id, embedding, metadata));
        }
        let batch: Vec<(u64, RowInput<'_>)> = derived
            .iter()
            .map(|(id, embedding, metadata)| {
                (
                    *id,
                    RowInput {
                        embedding,
                        metadata,
                    },
                )
            })
            .collect();
        storage
            .put_batch(&batch)
            .map_err(|e| format!("put_batch failed: {e}"))?;
        next_id = next_id
            .checked_add(BATCH_SIZE)
            .ok_or_else(|| "row id overflow after commit".to_string())?;

        // 進捗行はスクリプト側が「書き込みが実際に進み始めた」ことを検知する同期点
        // なので、バッファリングで遅延しないよう毎回明示的に flush する。
        writeln!(handle, "COMMITTED batch={batch_no} rows={next_id}")
            .map_err(|e| format!("stdout write failed: {e}"))?;
        handle
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;
    }
    Ok(())
}

fn run_verify(path: &str) -> i32 {
    match verify_inner(path) {
        Ok(rows) => {
            println!("RESULT ok=true rows={rows}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

fn verify_inner(path: &str) -> Result<u64, String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
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

    // 空虚な成功（vacuous pass）を拒否する: DB が空/未作成のまま検証をすり抜けない。
    if total_rows == 0 {
        return Err("no rows found".to_string());
    }
    // put_batch は単一トランザクションで BATCH_SIZE 行をコミットするため、行数が
    // その倍数でなければ部分コミット（原子性違反）が疑われる。
    if !total_rows.is_multiple_of(BATCH_SIZE) {
        return Err(format!(
            "row count {total_rows} is not a multiple of batch size {BATCH_SIZE} \
             (partial commit suspected)"
        ));
    }
    Ok(total_rows)
}
