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
//!
//! # Issue #495 追記: CSR 化（Issue #494）の前後比較実測
//!
//! `HnswBuildProfile.flatten`（凍結時の CSR 平坦化段。逐次縮退経路では
//! `Duration::ZERO`）を段別出力・`serial_share` の逐次段合算へ追加し、各
//! threads 点で 1 回だけ構築した索引を保持したまま常駐メモリ（RSS 前後差・
//! `HnswIndex::approx_heap_bytes`・VmHWM）を計測する行を追加した。CSR 化前
//! （commit `929c027`）とのビルド互換のため、メモリ計測は `flatten` に
//! 依存しない `HnswIndex::build_with_threads`（`build_with_threads_observed`
//! ではない）を使う——両コミットに存在する公開 API のみで構成し、CSR 化前
//! バイナリでもこの計測部分だけは変更なしに動く（`docs/design/hnsw-index.md`
//! §14.13 の before/after 実測手順参照）。
//!
//! ## レビュー対応追記: メモリ計測の子プロセス隔離
//!
//! 各 threads 点のメモリ計測を同一プロセス内で逐次実行すると、直前の点で
//! 構築・解放した `HnswIndex`（数百 MB 規模）のヒープページがアロケータに
//! 残留し、後続点の `vm_rss_delta_kb`／`vm_hwm_kb` が単発計測にならず
//! 過小評価になり得る（codex-review P2 指摘・Cursor Bugbot 指摘・PR #590）。
//! これを避けるため、各点のメモリ計測は `measure_memory_isolated` が
//! 自身の実行ファイルを `MEMORY_CHILD_ENV` 付きで再実行する新規子プロセスへ
//! 隔離する（`run_memory_child_if_requested`）。時間計測（スレッド数
//! ラダーの構築時間・段別プロファイル）は引き続き同一プロセス内で行う
//! （プロセス起動コストが時間計測のノイズになるのを避けるため。汚染の
//! 影響は RSS/VmHWM 系の統計に限られる）。

#[allow(dead_code)]
mod harness;

use std::sync::Mutex;
use std::time::Duration;

use harness::env_report::EnvReport;
use harness::hnsw_parallel_profile::{
    aggregate_lock_blocked_ratio, aggregate_lock_wait, lock_wait_share, measured_tail,
    min_median_max_duration, min_median_max_u64, parallel_vs_control_ceiling, pick_representative,
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
/// 計測し、`run` 自身の外部計測（`wall_median`。索引の drop を含む壁時間。
/// codex-review P2 指摘・PR #445 対応で `serial_share`・`total_speedup` の
/// 分母には使わない——下記コメント参照）と、各試行内で
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
    // `run` は warmup→計測の順にクロージャを呼ぶため（`protocol.rs::run` の
    // モジュールコメント・実装参照）、`collected` は先頭 `warmup_iterations`
    // 件が warmup 標本・末尾 `measured_iterations` 件が計測標本という並びに
    // なる。段別中央値・ワーカー統計は計測フェーズの標本のみから算出する
    // （codex-review P1 指摘・PR #445）。
    let measured = measured_tail(&collected, config.measured_iterations() as usize).to_vec();
    // `measurement.summary.median`（`wall_median`）は `run` のクロージャ内で
    // 生成された `HnswIndex` の drop（索引破棄）まで含む壁時間であり、
    // `HnswBuildProfile.total`（構築のみの壁時間）より長くなり得る。
    // `serial_share`・`total_speedup` の分母に使うと構築中の逐次割合を
    // 過小評価する（codex-review P2 指摘・PR #445）ため、呼び出し元へは
    // 参考値として返すのみに留め、実際の分母には `measured` から算出した
    // `HnswBuildProfile.total` の中央値を使わせる。
    Ok((measurement.summary.median, measured))
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

/// この threads 点で 1 回だけ `build_with_threads` した [`HnswIndex`] を保持
/// したまま常駐メモリ（RSS）増分・`approx_heap_bytes` を計測する（Issue #495。
/// `hybrid_profile_bench.rs`「索引を保持したまま RSS 前後を比較する」方式を
/// 踏襲）。`protocol::run`（warmup・計測の反復測定）の外側で 1 回のみ構築
/// することで、時間計測（`measure_threads_profiled`）とメモリ計測を独立させ、
/// 前者のウォームアップ回数に引きずられない単発のメモリスナップショットにする。
/// `HnswIndex::approx_heap_bytes` は Issue #494 の CSR 化メモリ見積り
/// （`docs/design/hnsw-index.md` §14.6）を実測で突き合わせる材料。
fn measure_memory(corpus: &[f32], params: HnswParams, threads: usize) -> Result<String, String> {
    let vm_rss_kb_before = read_vm_rss_kb();
    let index = HnswIndex::build_with_threads(params, DIM as u32, corpus, 1, threads)
        .map_err(|e| format!("threads={threads}: memory measurement build failed: {e}"))?;
    let approx_heap_bytes = index.approx_heap_bytes();
    let vm_rss_kb_after = read_vm_rss_kb();
    let vm_hwm_kb = harness::proc_stats::read_vm_hwm_kb();
    // 索引はこのメモリ差分計測の対象そのものであり、以降の段では参照しないため
    // ここで明示的に drop する（計測意図の明確化。`hybrid_profile_bench.rs` と
    // 同一方針）。
    drop(index);
    Ok(harness::hnsw_parallel_profile::render_memory_line(
        threads,
        approx_heap_bytes,
        vm_rss_kb_before,
        vm_rss_kb_after,
        vm_hwm_kb,
    ))
}

/// メモリ計測を新規プロセスで隔離するための子プロセス起動用環境変数名。
/// スレッド数ラダーの各点を同一プロセス内で逐次計測すると、直前の点で構築・
/// 解放した `HnswIndex`（数百 MB 規模）のヒープページがアロケータに残留し、
/// 後続点の `vm_rss_delta_kb`／`vm_hwm_kb` を汚染する（codex-review P2 指摘・
/// Cursor Bugbot 指摘・PR #590）。本ベンチ自身を `threads` を指定して
/// 再実行し、子プロセス側で 1 threads 点だけの `measure_memory` を実行する
/// ことで、各点を独立したプロセス（新規ヒープ・新規 VmHWM）で計測する。
const MEMORY_CHILD_ENV: &str = "BENCH_HNSW_PARALLEL_MEMORY_CHILD_THREADS";

/// `MEMORY_CHILD_ENV` が設定されている場合のみ実行される子プロセス経路。
/// 指定 `threads` 1 点分のコーパス生成・`measure_memory` を行い、結果行を
/// 標準出力へ書いて終了する（`main` の通常経路には戻らない）。親プロセス
/// （`measure_memory_isolated`）が `rows` を明示的に環境変数で渡すため、
/// ここでの `resolve_rows()` は親と同一の値を再現する。
fn run_memory_child_if_requested() {
    let Ok(raw) = std::env::var(MEMORY_CHILD_ENV) else {
        return;
    };
    // GITHUB_ACTIONS 下拒否は親プロセスの起動時点で既に検査済みだが、子
    // プロセス単体で誤って呼ばれた場合の defense-in-depth として再検査する
    // （`hnsw_build_bench.rs` と同一方針）。
    if running_under_github_actions() {
        eprintln!(
            "hnsw_parallel_build_bench: refusing to run under GITHUB_ACTIONS (manual-only bench)"
        );
        std::process::exit(1);
    }
    let threads: usize = match raw.parse() {
        Ok(t) if (1..=MAX_BUILD_THREADS).contains(&t) => t,
        _ => {
            eprintln!(
                "hnsw_parallel_build_bench: invalid {MEMORY_CHILD_ENV}={raw} (must be 1..={MAX_BUILD_THREADS})"
            );
            std::process::exit(1);
        }
    };
    let rows = resolve_rows();
    let params = HnswParams::default();
    let corpus = match harness::hnsw_build::generate_corpus(0xB0BA_1234 ^ rows as u64, DIM, rows) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_parallel_build_bench: corpus generation failed (memory child): {e}");
            std::process::exit(1);
        }
    };
    match measure_memory(&corpus, params, threads) {
        Ok(line) => {
            println!("{line}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("hnsw_parallel_build_bench: {e}");
            std::process::exit(1);
        }
    }
}

/// `threads` 点のメモリ計測を新規子プロセス（自分自身の実行ファイルの
/// 再実行）へ隔離して実行する（Issue #495 追記。codex-review P2・Cursor
/// Bugbot 指摘対応。上記 `MEMORY_CHILD_ENV` のドキュメンテーションコメント
/// 参照）。子プロセスの標準出力から `render_memory_line` が出す
/// `"hnsw_parallel_build: memory ..."` 行を抜き出して返す。
fn measure_memory_isolated(threads: usize, rows: usize) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("threads={threads}: current_exe unavailable: {e}"))?;
    let output = std::process::Command::new(&exe)
        .env(MEMORY_CHILD_ENV, threads.to_string())
        .env("BENCH_HNSW_PARALLEL_ROWS", rows.to_string())
        .output()
        .map_err(|e| format!("threads={threads}: memory child process spawn failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "threads={threads}: memory child process exited with {:?}: {stderr}",
            output.status.code()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|line| line.starts_with("hnsw_parallel_build: memory "))
        .map(str::to_string)
        .ok_or_else(|| format!("threads={threads}: memory child process produced no memory line"))
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
    // メモリ計測の子プロセス経路（`MEMORY_CHILD_ENV` 設定時のみ）。設定されて
    // いれば 1 threads 点分の計測だけを行いここで終了し、通常のラダー計測
    // 経路（下記）には進まない。
    run_memory_child_if_requested();

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
    // `control_speedup`（下記の参考行）とは異なり、`parallel_speedup` と同一の
    // 基準スレッド数（`parallel_base_threads`）を基準にした対照負荷の
    // 高速化率。`parallel_vs_control` ceiling をこの基準で正規化する
    // （codex-review P1 指摘・PR #445）。
    let mut control_median_base: Option<Duration> = None;
    let mut had_error = false;

    for &threads in &ladder {
        print_noise_snapshot(threads);

        // 各 threads 点のメモリ計測は新規子プロセスへ隔離する（Issue #495
        // 追記。同一プロセス内で逐次計測すると直前点の `HnswIndex` 構築・
        // 解放によるアロケータのページ再利用が後続点の RSS/VmHWM を汚染する
        // ため。`measure_memory_isolated` ドキュメンテーションコメント参照）。
        match measure_memory_isolated(threads, rows) {
            Ok(line) => println!("{line}"),
            Err(e) => {
                eprintln!("hnsw_parallel_build_bench: {e}");
                had_error = true;
            }
        }

        // この threads 点の `parallel_speedup`（ceiling 行が対照負荷 speedup と
        // 比較するために再利用する。`measure_control` 側で測り直さない——
        // 同じ計測を 2 回走らせるとベンチ全体の所要時間が倍化するため）。
        let mut parallel_speedup_for_ceiling: Option<f64> = None;

        match measure_threads_profiled(&corpus, params, threads) {
            Ok((wall_median, profiles)) => {
                // `serial_share`・`total_speedup` の分母は外側 `run` の
                // `wall_median`（索引 drop を含む）ではなく、計測標本ごとの
                // `HnswBuildProfile.total`（構築のみの壁時間）の中央値を使う
                // （codex-review P2 指摘・PR #445）。`threads==1` の縮退経路
                // （`build_with_threads_observed` 参照）でも `total` は
                // 構築全量の壁時間を持つため、そのまま分母に使える。
                let total_median =
                    min_median_max_duration(&profiles.iter().map(|p| p.total).collect::<Vec<_>>())
                        .map(|(_, med, _)| med)
                        .unwrap_or_default();

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
                // Issue #495: CSR 平坦化段（`repair_reachability` 完了後の最終段。
                // `engine::hnsw::HnswBuildProfile::flatten` ドキュメンテーション
                // コメント参照）の中央値。逐次縮退経路（threads==1 または
                // n<=SEQUENTIAL_PREFIX_NODES）では `Duration::ZERO` のまま。
                let flatten = min_median_max_duration(
                    &profiles.iter().map(|p| p.flatten).collect::<Vec<_>>(),
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
                    flatten,
                    total_median,
                )
                .map(|s| s * 100.0)
                .unwrap_or(f64::NAN);

                println!(
                    "hnsw_parallel_build: threads={threads} total={:.3}ms level={:.3}ms prefix={:.3}ms parallel={:.3}ms freeze={:.3}ms repair={:.3}ms flatten={:.3}ms serial_share={share:.2}% parallel_speedup={parallel_speedup:.3}x total_speedup={total_speedup:.3}x wall_median_with_drop={:.3}ms",
                    total_median.as_secs_f64() * 1000.0,
                    level_assign.as_secs_f64() * 1000.0,
                    sequential_prefix.as_secs_f64() * 1000.0,
                    parallel_phase.as_secs_f64() * 1000.0,
                    freeze.as_secs_f64() * 1000.0,
                    repair.as_secs_f64() * 1000.0,
                    flatten.as_secs_f64() * 1000.0,
                    wall_median.as_secs_f64() * 1000.0,
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
                    let (lock_wait_sum, lock_wait_max) = aggregate_lock_wait(workers);
                    let lock_wait_share_pct = lock_wait_share(workers)
                        .map(|r| r * 100.0)
                        .unwrap_or(f64::NAN);
                    let promotions = total_entry_promotions(workers);

                    match (inserted_line, busy_line) {
                        (Some((imin, imed, imax)), Some((bmin, bmed, bmax))) => {
                            println!(
                                "hnsw_parallel_build: threads={threads} workers={} inserted[min/med/max]={imin}/{imed}/{imax} busy[min/med/max]={:.3}/{:.3}/{:.3}ms lock_blocked_ratio={lock_blocked_ratio:.2}% lock_wait[sum/max]={:.3}/{:.3}ms lock_wait_share={lock_wait_share_pct:.2}% entry_promotions={promotions}",
                                workers.len(),
                                bmin.as_secs_f64() * 1000.0,
                                bmed.as_secs_f64() * 1000.0,
                                bmax.as_secs_f64() * 1000.0,
                                lock_wait_sum.as_secs_f64() * 1000.0,
                                lock_wait_max.as_secs_f64() * 1000.0,
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
                // 参考値: threads=1 基準の対照負荷 speedup（従来どおりの基準。
                // `parallel_speedup` の基準〔`parallel_base_threads`〕とは
                // 異なるため、そのままでは ceiling の分母として使わない）。
                let control_speedup_ref = baseline_control
                    .map(|b| b.as_secs_f64() / control_median.as_secs_f64())
                    .unwrap_or(1.0);
                println!(
                    "hnsw_parallel_build: control=dot_scan threads={threads} median={:.3}ms speedup_ref(basis=threads=1)={control_speedup_ref:.3}x",
                    control_median.as_secs_f64() * 1000.0
                );

                if parallel_base_threads == Some(threads) {
                    control_median_base = Some(control_median);
                }
                // `parallel_speedup` と同一基準（`parallel_base_threads`）の
                // 対照負荷 speedup。基準点が未計測（`control_median_base`
                // 未設定）の場合は比較不能として `None`（codex-review P1
                // 指摘・PR #445: 従来は threads=1 基準の speedup をそのまま
                // ceiling の分母に使っており、線形スケール時でも 0.5 に
                // 系統的にずれていた）。
                let control_speedup_rel =
                    control_median_base.and_then(|base| speedup(base, control_median));

                if let Some(parallel_speedup) = parallel_speedup_for_ceiling {
                    match parallel_vs_control_ceiling(
                        Some(parallel_speedup),
                        control_speedup_rel,
                    ) {
                        Some(ceiling) => println!(
                            "hnsw_parallel_build: ceiling threads={threads} basis=threads={} parallel_vs_control={ceiling:.3}",
                            parallel_base_threads
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "n/a".to_string()),
                        ),
                        None => println!(
                            "hnsw_parallel_build: ceiling threads={threads} basis=threads={} parallel_vs_control=n/a",
                            parallel_base_threads
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "n/a".to_string()),
                        ),
                    }
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
