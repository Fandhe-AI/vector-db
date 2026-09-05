//! HNSW 構築の並列化（Issue #406・親 #402・前提 #404/#405）の受け入れ条件 (b):
//! 「100k 点で構築時間がスレッド数に応じて短縮すること」を記録するベンチ。
//!
//! # Issue #406 追記: 8→12 スレッド頭打ち要因の切り分け計測
//!
//! 単純な `median(1) / median(threads)` の speedup だけでは、頭打ちが
//! （a）並列化されない逐次段（レベル割当・逐次プレフィックス・凍結・
//! `repair_reachability`）の割合が相対的に増える構造的な要因なのか、
//! （b）並列フェーズ自体がハードウェア天井（メモリ帯域・キャッシュ競合等）に
//! 当たっているのかを切り分けられない。本ベンチは
//! `engine::hnsw::HnswIndex::build_with_threads_observed` で段別の壁時間・
//! ワーカー統計（`HnswBuildProfile`／`HnswWorkerStats`）を実測し、
//! 共有可変状態を持たない embarrassingly parallel な対照負荷（`dot` 計算の
//! 単純な行分割スキャン）の speedup と並べて出力することで、この 2 つの
//! 仮説を区別できるようにする。
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
//! ごとに構築の段別中央値・ワーカー統計・対照負荷 speedup を出す。合否閾値は
//! 持たない情報提供専用ベンチ（spec 由来の基準ではない）。

#[allow(dead_code)]
mod harness;

use std::sync::Mutex;
use std::time::Duration;

use harness::env_report::EnvReport;
use harness::hnsw_parallel_profile::{
    aggregate_lock_blocked_ratio, min_median_max_duration, min_median_max_u64, pick_representative,
    serial_share, speedup, total_entry_promotions,
};
use harness::proc_stats::read_vm_rss_kb;
use harness::protocol::{run, MeasurementConfig};

use engine::hnsw::{HnswBuildProfile, HnswIndex, HnswParams, MAX_BUILD_THREADS};
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

/// `threads` での構築を `protocol::run` の下限（warmup・計測いずれも 20 回）で
/// 計測し、`run` 自身の外部計測（`total_median`）と、各試行内で
/// `build_with_threads_observed` が返した [`HnswBuildProfile`] 一式
/// （`Mutex<Vec<_>>` へ push して閉包の外へ回収する）を返す。
fn measure_threads_profiled(
    corpus: &[f32],
    params: HnswParams,
    threads: usize,
) -> Result<(Duration, Vec<HnswBuildProfile>), String> {
    // `protocol::MeasurementConfig` の下限（warmup・計測回数いずれも 20）は
    // 緩めない（`hnsw_build_bench.rs::measure_stage` と同一方針）。
    let config = MeasurementConfig::new(20, 20, 0xB0BA_1234 ^ threads as u64)
        .map_err(|e| format!("threads={threads}: {e}"))?;
    let profiles: Mutex<Vec<HnswBuildProfile>> = Mutex::new(Vec::new());
    let measurement = run(&config, || {
        let (_, profile) =
            HnswIndex::build_with_threads_observed(params, DIM as u32, corpus, 1, threads)
                .expect("parallel build should succeed on well-formed corpus");
        if let Ok(mut guard) = profiles.lock() {
            guard.push(profile);
        }
    })
    .map_err(|e| format!("threads={threads}: {e}"))?;
    let collected = profiles.into_inner().unwrap_or_default();
    Ok((measurement.summary.median, collected))
}

/// 対照負荷のパス数。1 パス（100k×dim64 ≈ 数 ms）ではスレッド生成・join の
/// 固定費が計測値を支配し speedup が意味を持たないため、単一スレッドで
/// 数百 ms 規模になるまで同一コーパスを繰り返し走査する（パスごとにクエリ行を
/// 変えて計算を畳み込まれないようにする）。
const CONTROL_PASSES: usize = 64;

/// 共有可変状態を持たない embarrassingly parallel な対照負荷: コーパスを
/// `threads` 本へ行範囲分割し、各ワーカーが担当範囲全体とクエリ行
/// （パス番号に対応するコーパス行）の `dot` を `CONTROL_PASSES` 回計算して
/// f32 和を返す（`black_box` 相当にコンパイラの最適化除去を防ぐため戻り値は
/// `run` が消費する）。ハードウェア天井（メモリ帯域・キャッシュ競合・vCPU
/// 配分等）の影響を、HNSW 構築のロック・グラフ探索を一切含まない最小構成で
/// 見積もる対照区間。
fn control_dot_scan(corpus: &[f32], dim: usize, threads: usize) -> f32 {
    let rows = corpus.len() / dim.max(1);
    if rows == 0 || dim == 0 {
        return 0.0;
    }
    let kernel = isa::current();
    let threads = threads.max(1);
    let chunk = rows.div_ceil(threads).max(1);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            let start = t.saturating_mul(chunk);
            if start >= rows {
                continue;
            }
            let end = (start + chunk).min(rows);
            let corpus_ref: &[f32] = corpus;
            handles.push(scope.spawn(move || {
                let mut sum = 0f32;
                for pass in 0..CONTROL_PASSES {
                    let q_base = (pass % rows) * dim;
                    let Some(query) = corpus_ref.get(q_base..q_base + dim) else {
                        continue;
                    };
                    for r in start..end {
                        let base = r * dim;
                        if let Some(row) = corpus_ref.get(base..base + dim) {
                            sum += kernel.dot(row, query);
                        }
                    }
                }
                sum
            }));
        }
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .fold(0f32, |acc, v| acc + v)
    })
}

fn measure_control(corpus: &[f32], threads: usize) -> Result<Duration, String> {
    let config = MeasurementConfig::new(20, 20, 0xC0FFEE_u64 ^ threads as u64)
        .map_err(|e| format!("control threads={threads}: {e}"))?;
    let measurement = run(&config, || control_dot_scan(corpus, DIM, threads))
        .map_err(|e| format!("control threads={threads}: {e}"))?;
    Ok(measurement.summary.median)
}

/// 各 threads 点の直前に環境ノイズ（1 分ロードアベレージ・常駐メモリ）を
/// 出力する（実測値の解釈に必要な併記情報。`.claude/rules/security.md` の
/// 「機微情報は含めない」方針どおりテナント ID・DB パス等は含まない）。
fn print_noise_snapshot(threads: usize) {
    let loadavg = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_else(|| "unavailable".to_string());
    let rss = read_vm_rss_kb()
        .map(|kb| format!("{kb}kB"))
        .unwrap_or_else(|| "unavailable".to_string());
    println!("hnsw_parallel_build: noise threads={threads} loadavg_1m={loadavg} rss={rss}");
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

    // parallel_speedup の基準はラダー中の最小 threads>=2 点の parallel_phase
    // （threads=1 は縮退経路のため並列フェーズを持たない）。
    let parallel_base_threads = ladder.iter().copied().find(|&t| t >= 2);
    println!(
        "hnsw_parallel_build: parallel_base_threads={}",
        parallel_base_threads
            .map(|t| t.to_string())
            .unwrap_or_else(|| "unavailable(no threads>=2 in ladder)".to_string())
    );

    let mut baseline_total: Option<Duration> = None;
    let mut baseline_control: Option<Duration> = None;
    let mut parallel_phase_base: Option<Duration> = None;
    let mut had_error = false;

    for &threads in &ladder {
        print_noise_snapshot(threads);

        // この threads 点の `parallel_speedup`（ceiling 行が対照負荷 speedup と
        // 比較するために再利用する。`measure_control` 側で測り直さない——
        // 同じ計測を 2 回走らせるとベンチ全体の所要時間が倍化するため）。
        let mut parallel_speedup_for_ceiling: Option<f64> = None;

        match measure_threads_profiled(&corpus, params, threads) {
            Ok((total_median, profiles)) => {
                if threads == 1 {
                    baseline_total = Some(total_median);
                }
                let total_speedup = baseline_total
                    .map(|b| b.as_secs_f64() / total_median.as_secs_f64())
                    .unwrap_or(1.0);

                let level_assign = min_median_max_duration(
                    &profiles.iter().map(|p| p.level_assign).collect::<Vec<_>>(),
                )
                .map(|(_, med, _)| med)
                .unwrap_or_default();
                let sequential_prefix = min_median_max_duration(
                    &profiles
                        .iter()
                        .map(|p| p.sequential_prefix)
                        .collect::<Vec<_>>(),
                )
                .map(|(_, med, _)| med)
                .unwrap_or_default();
                let parallel_phase = min_median_max_duration(
                    &profiles
                        .iter()
                        .map(|p| p.parallel_phase)
                        .collect::<Vec<_>>(),
                )
                .map(|(_, med, _)| med)
                .unwrap_or_default();
                let freeze =
                    min_median_max_duration(&profiles.iter().map(|p| p.freeze).collect::<Vec<_>>())
                        .map(|(_, med, _)| med)
                        .unwrap_or_default();
                let repair = min_median_max_duration(
                    &profiles
                        .iter()
                        .map(|p| p.repair_reachability)
                        .collect::<Vec<_>>(),
                )
                .map(|(_, med, _)| med)
                .unwrap_or_default();

                if parallel_base_threads == Some(threads) {
                    parallel_phase_base = Some(parallel_phase);
                }
                let parallel_speedup = parallel_phase_base
                    .and_then(|base| speedup(base, parallel_phase))
                    .unwrap_or(1.0);
                parallel_speedup_for_ceiling =
                    parallel_phase_base.and_then(|base| speedup(base, parallel_phase));

                let share = serial_share(
                    level_assign,
                    sequential_prefix,
                    freeze,
                    repair,
                    total_median,
                )
                .map(|s| s * 100.0)
                .unwrap_or(f64::NAN);

                println!(
                    "hnsw_parallel_build: threads={threads} total={:.3}ms level={:.3}ms prefix={:.3}ms parallel={:.3}ms freeze={:.3}ms repair={:.3}ms serial_share={share:.2}% parallel_speedup={parallel_speedup:.3}x total_speedup={total_speedup:.3}x",
                    total_median.as_secs_f64() * 1000.0,
                    level_assign.as_secs_f64() * 1000.0,
                    sequential_prefix.as_secs_f64() * 1000.0,
                    parallel_phase.as_secs_f64() * 1000.0,
                    freeze.as_secs_f64() * 1000.0,
                    repair.as_secs_f64() * 1000.0,
                );

                if let Some(representative) = pick_representative(&profiles) {
                    let workers = &representative.workers;
                    let inserted_line = min_median_max_u64(
                        &workers.iter().map(|w| w.inserted_nodes).collect::<Vec<_>>(),
                    );
                    let busy_line = min_median_max_duration(
                        &workers.iter().map(|w| w.busy).collect::<Vec<_>>(),
                    );
                    let lock_blocked_ratio = aggregate_lock_blocked_ratio(workers)
                        .map(|r| r * 100.0)
                        .unwrap_or(f64::NAN);
                    let promotions = total_entry_promotions(workers);

                    match (inserted_line, busy_line) {
                        (Some((imin, imed, imax)), Some((bmin, bmed, bmax))) => {
                            println!(
                                "hnsw_parallel_build: threads={threads} workers={} inserted[min/med/max]={imin}/{imed}/{imax} busy[min/med/max]={:.3}/{:.3}/{:.3}ms lock_blocked_ratio={lock_blocked_ratio:.2}% entry_promotions={promotions}",
                                workers.len(),
                                bmin.as_secs_f64() * 1000.0,
                                bmed.as_secs_f64() * 1000.0,
                                bmax.as_secs_f64() * 1000.0,
                            );
                        }
                        _ => {
                            println!(
                                "hnsw_parallel_build: threads={threads} workers=0 (degenerate path; no worker stats)"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("hnsw_parallel_build_bench: {e}");
                had_error = true;
            }
        }

        match measure_control(&corpus, threads) {
            Ok(control_median) => {
                if threads == 1 {
                    baseline_control = Some(control_median);
                }
                let control_speedup = baseline_control
                    .map(|b| b.as_secs_f64() / control_median.as_secs_f64())
                    .unwrap_or(1.0);
                println!(
                    "hnsw_parallel_build: control=dot_scan threads={threads} median={:.3}ms speedup={control_speedup:.3}x",
                    control_median.as_secs_f64() * 1000.0
                );

                if let Some(parallel_speedup) = parallel_speedup_for_ceiling {
                    let ceiling = if control_speedup != 0.0 {
                        parallel_speedup / control_speedup
                    } else {
                        f64::NAN
                    };
                    println!(
                        "hnsw_parallel_build: ceiling threads={threads} parallel_vs_control={ceiling:.3}"
                    );
                }
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
