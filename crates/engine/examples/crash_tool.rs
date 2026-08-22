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
//! - `write <db_path>`: `Storage::put`（対象ビヘイビア: PERSIST-1）で行を 1 行ずつ
//!   無限に近い反復でコミットし続ける。`PROGRESS_INTERVAL` 行ごとに
//!   `COMMITTED row=<n> rows=<total>` を stdout へ出力する（`scripts/crash_test.sh`
//!   が「書き込みが実際に進み始めてから kill する」ための同期点）。再起動時は
//!   既存データの最大行 ID の続きから採番する。
//! - `verify <db_path>`: 全行を上限付きページングで走査し、破損 0 件・ID が
//!   0 から連続で欠番なし・内容一致を確認する（単一行コミットのため、途中で
//!   kill されても「その時点までにコミット済みの行が過不足なく揃っている」ことが
//!   PERSIST-1 のオラクルであり、バッチ単位の倍数制約は課さない）。
//!   結果を `RESULT ok=true rows=<N>` / `RESULT ok=false reason=<...>` の 1 行で
//!   stdout へ出力し、失敗時は非 0 終了する（fail-closed。空虚な成功を許さない）。

use std::io::Write as _;

use engine::storage::{RowInput, Storage, Visibility};

/// write/verify で固定して使うテナント識別子。本ツールは単一テナントの
/// クラッシュ耐性（PERSIST-1）検証のみが目的で、RLS ポリシー評価そのものは
/// 対象外のため、`RowInput::tenant_id` は固定値で足りる。
const CRASH_TOOL_TENANT_ID: &str = "crash-tool-tenant";

/// stdout への進捗出力間隔（行数）。`scripts/crash_test.sh` が
/// 「書き込みが実際に進み始めた」ことを検知する同期点の頻度を決める
/// （小さすぎると stdout I/O が支配的になり kill の当たりどころが偏るため、
/// 1 行ごとではなくまとめて出力する）。
const PROGRESS_INTERVAL: u64 = 10;
/// 生成する埋め込みベクトルの次元数。
const EMBEDDING_DIM: usize = 64;
/// 生成するメタデータバイト列の長さ。
const METADATA_LEN: usize = 32;
/// `scan_page` の 1 回あたり取得件数。
const PAGE_LIMIT: u32 = 10_000;
/// write の自走上限行数（安全弁）。`scripts/crash_test.sh` は書き込み進行中に
/// SIGKILL する想定のため通常はここへ到達しない。到達した場合はテストダブルとして
/// 正常終了する（無限ループにしない。security.md「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。
const MAX_ROWS: u64 = 5_000_000;

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

    for row_no in 1..=MAX_ROWS {
        let (embedding, metadata) = derive_row(next_id);
        let row = RowInput {
            tenant_id: CRASH_TOOL_TENANT_ID,
            visibility: Visibility::Public,
            embedding: &embedding,
            metadata: &metadata,
        };
        // PERSIST-1 の単一行書き込み経路そのものを検証する（PERSIST-2 の
        // `put_batch` は使わない。回帰対象がこの API 呼び出しであるため）。
        storage
            .put(next_id, &row)
            .map_err(|e| format!("put failed: {e}"))?;
        next_id = next_id
            .checked_add(1)
            .ok_or_else(|| "row id overflow after commit".to_string())?;

        if row_no.is_multiple_of(PROGRESS_INTERVAL) {
            // 進捗行はスクリプト側が「書き込みが実際に進み始めた」ことを検知する
            // 同期点なので、バッファリングで遅延しないよう毎回明示的に flush する。
            writeln!(handle, "COMMITTED row={row_no} rows={next_id}")
                .map_err(|e| format!("stdout write failed: {e}"))?;
            handle
                .flush()
                .map_err(|e| format!("stdout flush failed: {e}"))?;
        }
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
            // write が書き込んだ RLS フィールド（`tenant_id`・`visibility`）も
            // 再オープン後に破損なく往復することを検証する（write 側の固定値と
            // 突き合わせる。値そのものの意味検証ではなく、クラッシュ耐性
            // オラクルの対象を write が実際に書く列に追随させるための確認）。
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

    // 空虚な成功（vacuous pass）を拒否する: DB が空/未作成のまま検証をすり抜けない。
    if total_rows == 0 {
        return Err("no rows found".to_string());
    }
    // `put` は 1 行ずつ独立にコミットするため、バッチ単位の倍数制約は課さない。
    // 上のループで expected_id との突き合わせ（0 起点で欠番なし）を既に確認済み
    // であり、それ自体が「途中で kill されてもコミット済みの行が過不足なく
    // 揃っている」ことの PERSIST-1 オラクルになっている。
    Ok(total_rows)
}
