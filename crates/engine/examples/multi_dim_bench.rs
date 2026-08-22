//! `engine::storage::Storage` の複数次元テーブル共存・実測ハーネス（TASK-91、
//! 対象ビヘイビア: TABLE-2。ポインタ: `docs/spec/05-tasks.md` TASK-91）。
//!
//! `cargo test` の対象には含めない手動実行専用ツール（`cargo run -p engine --release
//! --example multi_dim_bench` で実行する）。時間依存の測定値を CI のアサーションに
//! 混ぜない方針（.claude/rules/coding-rust.md）のため、`tests/multi_dim_tables.rs`
//! （正しさの回帰テスト・CI 常時実行）とはファイルを分離している
//! （`crates/engine/examples/concurrent_write_bench.rs` と同じ構成方針）。
//!
//! 比較設計: (a) 単一次元（768）のみのベースライン DB、(b) 384/768/1536 混在 DB
//! （各テーブル同一行数をバッチ書き込み）で、書き込み p50/p95/max・スループット
//! （rows/sec）・読み出し（`scan_page` 全走査）所要時間・DB ファイルサイズを計測する。
//! 出力は `docs/design/multi-dim-table-coexistence.md` の実測結果表に転記する。
//! 行数・次元・バッチサイズはすべて定数で上限固定し、無制限確保をしない
//! （security.md「不安全な設計｜無制限リソース確保（DoS）」）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::storage::{RowInput, Storage, Visibility};

/// 混在 DB 側で使う 3 次元（テストと同一の既定値。TASK-91 上、データセット選定自体は
/// 人間の判断事項のため、決定論的な合成ベクトルを既定とする）。
const MIXED_DIMS: [u32; 3] = [384, 768, 1536];
/// ベースライン DB（単一次元）で使う次元。混在側の中間値である 768 に揃え、
/// 「同じ 768 次元テーブル 1 本が単独か、384/768/1536 混在の一部か」を比較する。
const BASELINE_DIM: u32 = 768;

/// 1 テーブルあたりの書き込み行数（両条件で同一に揃える。手動実行が数十秒以内に
/// 終わるよう、行サイズ・テーブル数に対して控えめに固定する）。
const ROWS_PER_TABLE: u64 = 800;
/// `put_batch` 1 回あたりの行数。
const BATCH_SIZE: u64 = 20;
/// 小メタデータ列（テーブル名を模した固定長ダミー相当のサイズ）。
const METADATA_LEN: usize = 64;

fn unique_db_path(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-task91-bench-{label}-{}.redb",
        std::process::id()
    ));
    path
}

/// 決定論的な埋め込み（外部クレート非依存。値の分布自体は問題にしない）。
fn embedding_for(id: u64, dim: u32) -> Vec<f32> {
    (0..dim).map(|i| (id + i as u64) as f32).collect()
}

fn metadata_for(id: u64) -> Vec<u8> {
    let mut buf = vec![0u8; METADATA_LEN];
    let id_bytes = id.to_le_bytes();
    buf[..id_bytes.len()].copy_from_slice(&id_bytes);
    buf
}

struct WriteResult {
    op_count: usize,
    p50: Duration,
    p95: Duration,
    max: Duration,
    total: Duration,
    rows_per_sec: f64,
}

fn summarize(mut latencies: Vec<Duration>, total: Duration, rows: u64) -> WriteResult {
    latencies.sort_unstable();
    let op_count = latencies.len();
    let percentile = |p: f64| -> Duration {
        if op_count == 0 {
            return Duration::ZERO;
        }
        let idx = ((op_count as f64) * p).floor() as usize;
        latencies[idx.min(op_count - 1)]
    };
    let rows_per_sec = if total.as_secs_f64() > 0.0 {
        rows as f64 / total.as_secs_f64()
    } else {
        0.0
    };
    WriteResult {
        op_count,
        p50: percentile(0.50),
        p95: percentile(0.95),
        max: latencies.last().copied().unwrap_or(Duration::ZERO),
        total,
        rows_per_sec,
    }
}

/// `table_name`（次元 `dim`）へ `id_base` から `ROWS_PER_TABLE` 行を `put_batch` で
/// 書き込み、バッチごとの所要時間を集めて返す。
fn write_table(storage: &Storage, table_name: &str, dim: u32, id_base: u64) -> Vec<Duration> {
    let batches = ROWS_PER_TABLE / BATCH_SIZE;
    let mut latencies = Vec::with_capacity(batches as usize);
    for batch_idx in 0..batches {
        let batch_base = id_base + batch_idx * BATCH_SIZE;
        let embeddings: Vec<Vec<f32>> = (0..BATCH_SIZE)
            .map(|offset| embedding_for(batch_base + offset, dim))
            .collect();
        let metadatas: Vec<Vec<u8>> = (0..BATCH_SIZE).map(metadata_for).collect();
        let rows: Vec<(u64, RowInput<'_>)> = (0..BATCH_SIZE as usize)
            .map(|i| {
                (
                    batch_base + i as u64,
                    RowInput {
                        tenant_id: table_name,
                        visibility: Visibility::Public,
                        embedding: &embeddings[i],
                        metadata: &metadatas[i],
                    },
                )
            })
            .collect();
        let started = Instant::now();
        storage
            .put_batch(&rows)
            .expect("put_batch should succeed against a healthy database");
        latencies.push(started.elapsed());
    }
    latencies
}

/// `Storage::scan_page` で全行を読み切り、所要時間と読み取り件数を返す。
fn scan_all(storage: &Storage) -> (Duration, usize) {
    let started = Instant::now();
    let mut total_rows = 0usize;
    let mut cursor = None;
    loop {
        let (page, next_cursor) = storage
            .scan_page(cursor, 512)
            .expect("scan_page should succeed");
        total_rows += page.len();
        if next_cursor.is_none() {
            break;
        }
        cursor = next_cursor;
    }
    (started.elapsed(), total_rows)
}

fn file_size(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn print_write_result(label: &str, r: &WriteResult) {
    println!(
        "{:<28} | {:>9} | {:>10.3?} | {:>10.3?} | {:>10.3?} | {:>10.3?} | {:>12.1}",
        label, r.op_count, r.p50, r.p95, r.max, r.total, r.rows_per_sec
    );
}

fn main() {
    println!(
        "multi_dim_bench: MIXED_DIMS={MIXED_DIMS:?}, BASELINE_DIM={BASELINE_DIM}, \
         ROWS_PER_TABLE={ROWS_PER_TABLE}, BATCH_SIZE={BATCH_SIZE}"
    );
    println!(
        "{:<28} | {:>9} | {:>10} | {:>10} | {:>10} | {:>10} | {:>12}",
        "config", "op_count", "p50", "p95", "max", "total", "rows/sec"
    );

    // (a) 単一次元（768）のみのベースライン DB。
    let baseline_path = unique_db_path("baseline-single-768");
    {
        let storage = Storage::open(&baseline_path).expect("open baseline storage");
        storage
            .create_table(&TableSchema::new(
                "docs_768",
                vec![ColumnDef::new(
                    "embedding",
                    ColumnType::Vector(BASELINE_DIM),
                    false,
                )],
            ))
            .expect("create baseline table");
        let latencies = write_table(&storage, "docs_768", BASELINE_DIM, 0);
        let total: Duration = latencies.iter().sum();
        let result = summarize(latencies, total, ROWS_PER_TABLE);
        print_write_result("baseline(single 768)", &result);

        let (scan_dur, scan_rows) = scan_all(&storage);
        println!(
            "  scan_page full read: rows={scan_rows} elapsed={scan_dur:?}, db_file_size={} bytes",
            file_size(&baseline_path)
        );
    }
    let _ = std::fs::remove_file(&baseline_path);

    // (b) 384/768/1536 混在 DB。各テーブル同一行数（ROWS_PER_TABLE）をバッチ書き込みする。
    let mixed_path = unique_db_path("mixed-384-768-1536");
    {
        let storage = Storage::open(&mixed_path).expect("open mixed storage");
        let table_names = ["docs_384", "docs_768", "docs_1536"];
        for (name, dim) in table_names.iter().zip(MIXED_DIMS.iter()) {
            storage
                .create_table(&TableSchema::new(
                    *name,
                    vec![ColumnDef::new("embedding", ColumnType::Vector(*dim), false)],
                ))
                .unwrap_or_else(|e| panic!("create_table({name}) should succeed: {e}"));
        }

        let mut all_latencies = Vec::new();
        let overall_start = Instant::now();
        for (idx, (name, dim)) in table_names.iter().zip(MIXED_DIMS.iter()).enumerate() {
            let id_base = idx as u64 * 10_000_000;
            let latencies = write_table(&storage, name, *dim, id_base);
            all_latencies.extend(latencies);
        }
        let total = overall_start.elapsed();
        let rows = ROWS_PER_TABLE * table_names.len() as u64;
        let result = summarize(all_latencies, total, rows);
        print_write_result("mixed(384/768/1536)", &result);

        let (scan_dur, scan_rows) = scan_all(&storage);
        println!(
            "  scan_page full read: rows={scan_rows} elapsed={scan_dur:?}, db_file_size={} bytes",
            file_size(&mixed_path)
        );
    }
    let _ = std::fs::remove_file(&mixed_path);
}
