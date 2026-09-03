//! HNSW 構築の並列化（Issue #406・親 #402・前提 #404/#405）の受け入れ条件 (b):
//! 「100k 点で構築時間がスレッド数に応じて短縮すること」を記録するベンチ。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make
//! bench-hnsw-parallel-build` からの手動実行専用。`hnsw_build_bench.rs` と同一
//! 方針の defense-in-depth 拒否）。
//!
//! # 測定対象・出力
//!
//! `rows`（既定 100,000・`BENCH_HNSW_PARALLEL_ROWS` で上書き可）・dim=64・
//! 既定パラメータで、スレッド数ラダー `[1, 2, 4, 8, ..]`（既定は利用可能な
//! 論理コア数まで・`BENCH_HNSW_PARALLEL_THREADS` でカンマ区切り上書き可）
//! ごとに `HnswIndex::build_with_threads` の所要時間中央値を計測し、
//! `speedup = median(1) / median(threads)` を出す。合否閾値は持たない
//! 情報提供専用ベンチ（spec 由来の基準ではない）。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::protocol::{run, MeasurementConfig};

use engine::hnsw::{HnswIndex, HnswParams, MAX_BUILD_THREADS};
use engine::isa;

const DIM: usize = 64;
const DEFAULT_ROWS: usize = 100_000;
/// `BENCH_HNSW_PARALLEL_ROWS` の受理上限（DoS 防止・上限検証。
/// `hnsw::MAX_HNSW_NODES` より十分小さい値に固定する）。
const MAX_ROWS_GUARD: usize = 200_000;

fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// `BENCH_HNSW_PARALLEL_ROWS` を読み、`1..=MAX_ROWS_GUARD` の範囲で検証する。
/// 未設定・不正値は既定値へフォールバックする（時間依存ベンチの入力なので
/// fail-closed に拒否するより既定値へ倒す方が運用上有用）。
fn resolve_rows() -> usize {
    std::env::var("BENCH_HNSW_PARALLEL_ROWS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=MAX_ROWS_GUARD).contains(&n))
        .unwrap_or(DEFAULT_ROWS)
}

/// `BENCH_HNSW_PARALLEL_THREADS`（カンマ区切り）を読み、`1..=MAX_BUILD_THREADS`
/// に検証したうえで昇順・重複なしに正規化する。未設定・全滅時は
/// `[1, 2, 4, 8, .., available_parallelism]`（`MAX_BUILD_THREADS` でクランプ）
/// を既定ラダーとする。
fn resolve_thread_ladder() -> Vec<usize> {
    if let Ok(raw) = std::env::var("BENCH_HNSW_PARALLEL_THREADS") {
        let mut values: Vec<usize> = raw
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .filter(|&t| (1..=MAX_BUILD_THREADS).contains(&t))
            .collect();
        values.sort_unstable();
        values.dedup();
        if !values.is_empty() {
            return values;
        }
    }

    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_BUILD_THREADS);
    let mut ladder = vec![1usize];
    let mut t = 2usize;
    while t < available {
        ladder.push(t);
        t *= 2;
    }
    if available > 1 {
        ladder.push(available);
    }
    ladder.sort_unstable();
    ladder.dedup();
    ladder
}

fn measure_threads(
    corpus: &[f32],
    params: HnswParams,
    threads: usize,
) -> Result<std::time::Duration, String> {
    // `protocol::MeasurementConfig` の下限（warmup・計測回数いずれも 20）は
    // 緩めない（`hnsw_build_bench.rs::measure_stage` と同一方針）。
    let config = MeasurementConfig::new(20, 20, 0xB0BA_1234 ^ threads as u64)
        .map_err(|e| format!("threads={threads}: {e}"))?;
    let measurement = run(&config, || {
        HnswIndex::build_with_threads(params, DIM as u32, corpus, 1, threads)
            .expect("parallel build should succeed on well-formed corpus")
    })
    .map_err(|e| format!("threads={threads}: {e}"))?;
    Ok(measurement.summary.median)
}

fn main() {
    if running_under_github_actions() {
        eprintln!(
            "hnsw_parallel_build_bench: refusing to run under GITHUB_ACTIONS (manual-only bench)"
        );
        std::process::exit(1);
    }

    let detected = isa::current().isa();
    let env = EnvReport::capture(format!("{detected:?}"));
    println!("{env}");

    let rows = resolve_rows();
    let ladder = resolve_thread_ladder();
    println!("hnsw_parallel_build: rows={rows} dim={DIM} thread_ladder={ladder:?}");

    let params = HnswParams::default();
    let corpus = match harness::hnsw_build::generate_corpus(0xB0BA_1234 ^ rows as u64, DIM, rows) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_parallel_build_bench: corpus generation failed: {e}");
            std::process::exit(1);
        }
    };

    let mut baseline: Option<std::time::Duration> = None;
    let mut had_error = false;
    for &threads in &ladder {
        match measure_threads(&corpus, params, threads) {
            Ok(median) => {
                if threads == 1 {
                    baseline = Some(median);
                }
                let speedup = baseline
                    .map(|b| b.as_secs_f64() / median.as_secs_f64())
                    .unwrap_or(1.0);
                println!(
                    "hnsw_parallel_build: threads={threads} median={:.3}ms speedup={speedup:.3}x",
                    median.as_secs_f64() * 1000.0
                );
            }
            Err(e) => {
                eprintln!("hnsw_parallel_build_bench: {e}");
                had_error = true;
            }
        }
    }

    if had_error {
        std::process::exit(1);
    }
}
