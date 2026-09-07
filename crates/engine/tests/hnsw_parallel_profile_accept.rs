//! `benches/harness/hnsw_parallel_profile.rs`（Issue #406 追記: 8→12 スレッド
//! 頭打ち要因の切り分け計測）の回帰テスト。
//!
//! `hnsw_parallel_build_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hnsw_build_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の集計・整形ロジックのみを `#[path]` で取り込み `cargo test`
//! 〔`make ci` 対象〕で検証する）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::time::Duration;

use engine::hnsw::{HnswBuildProfile, HnswRepairLevelStats, HnswRepairStats, HnswWorkerStats};
use harness::hnsw_parallel_profile::{
    aggregate_lock_blocked_ratio, aggregate_lock_wait, format_per_level, lock_wait_share,
    measured_tail, median_duration, min_median_max_duration, min_median_max_u64,
    parallel_vs_control_ceiling, pick_representative, render_memory_line, repair_phase1_cap_hits,
    repair_phase_wall_sum, repair_total_phase1_iterations, repair_total_phase2_nodes,
    repair_total_unreachable, repair_unreachable_per_level_min_med_max, repair_wall_gap,
    serial_share, speedup, total_entry_promotions,
};

// --- median_duration ---

#[test]
fn median_duration_odd_count_picks_middle_value() {
    let values = vec![
        Duration::from_millis(30),
        Duration::from_millis(10),
        Duration::from_millis(20),
    ];
    assert_eq!(median_duration(&values), Some(Duration::from_millis(20)));
}

#[test]
fn median_duration_even_count_interpolates_middle_pair() {
    // 4 要素ソート後 [10, 20, 30, 40] の中央値は中央 2 件 (20, 30) の線形補間
    // (`stats::summarize` と同一の規則) で 25ms になる（codex-review P2
    // 指摘・PR #445: 従来は中央 2 件の上側 30ms を採用しており、`total` の
    // 中央値〔`stats::summarize` 経由・補間あり〕と定義が食い違っていた）。
    let values = vec![
        Duration::from_millis(40),
        Duration::from_millis(10),
        Duration::from_millis(30),
        Duration::from_millis(20),
    ];
    assert_eq!(median_duration(&values), Some(Duration::from_millis(25)));
}

#[test]
fn median_duration_empty_is_none() {
    assert_eq!(median_duration(&[]), None);
}

// --- min_median_max_u64 / min_median_max_duration ---

#[test]
fn min_median_max_u64_reports_all_three() {
    let (min, median, max) = min_median_max_u64(&[5, 1, 3, 9, 7]).unwrap();
    assert_eq!(min, 1);
    assert_eq!(median, 5);
    assert_eq!(max, 9);
}

#[test]
fn min_median_max_u64_empty_is_none() {
    assert_eq!(min_median_max_u64(&[]), None);
}

#[test]
fn min_median_max_u64_even_count_interpolates_median() {
    // ソート後 [1, 3, 5, 9] の中央値は中央 2 件 (3, 5) の補間で 4
    // （`stats::summarize` と同一の補間規則を `f64` 空間で適用し四捨五入）。
    let (min, median, max) = min_median_max_u64(&[9, 1, 5, 3]).unwrap();
    assert_eq!(min, 1);
    assert_eq!(median, 4);
    assert_eq!(max, 9);
}

#[test]
fn min_median_max_duration_reports_all_three() {
    let values = vec![
        Duration::from_millis(5),
        Duration::from_millis(1),
        Duration::from_millis(3),
    ];
    let (min, median, max) = min_median_max_duration(&values).unwrap();
    assert_eq!(min, Duration::from_millis(1));
    assert_eq!(median, Duration::from_millis(3));
    assert_eq!(max, Duration::from_millis(5));
}

#[test]
fn min_median_max_duration_even_count_interpolates_median() {
    // 4 要素ソート後 [10, 20, 30, 40] の中央値は中央 2 件 (20, 30) の線形補間で
    // 25ms（`median_duration_even_count_interpolates_middle_pair` と同一の規則）。
    let values = vec![
        Duration::from_millis(40),
        Duration::from_millis(10),
        Duration::from_millis(30),
        Duration::from_millis(20),
    ];
    let (min, median, max) = min_median_max_duration(&values).unwrap();
    assert_eq!(min, Duration::from_millis(10));
    assert_eq!(median, Duration::from_millis(25));
    assert_eq!(max, Duration::from_millis(40));
}

// --- serial_share ---

#[test]
fn serial_share_computes_ratio_of_sequential_stages_to_total() {
    // `flatten=0`（CSR 平坦化なしの旧アリティ相当）では従来どおり 0.5。
    let share = serial_share(
        Duration::from_millis(1),
        Duration::from_millis(2),
        Duration::from_millis(3),
        Duration::from_millis(4),
        Duration::ZERO,
        Duration::from_millis(20),
    )
    .unwrap();
    assert!((share - 0.5).abs() < 1e-9, "share={share}");
}

#[test]
fn serial_share_includes_flatten_in_sequential_stages() {
    // Issue #495: `flatten`（CSR 平坦化。単一スレッド実行）を逐次段へ合算する。
    // 1+2+3+4+5=15 / 30 = 0.5。
    let share = serial_share(
        Duration::from_millis(1),
        Duration::from_millis(2),
        Duration::from_millis(3),
        Duration::from_millis(4),
        Duration::from_millis(5),
        Duration::from_millis(30),
    )
    .unwrap();
    assert!((share - 0.5).abs() < 1e-9, "share={share}");
}

#[test]
fn serial_share_zero_total_is_none() {
    assert_eq!(
        serial_share(
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        ),
        None
    );
}

// --- speedup ---

#[test]
fn speedup_computes_baseline_over_sample() {
    let s = speedup(Duration::from_millis(100), Duration::from_millis(25)).unwrap();
    assert!((s - 4.0).abs() < 1e-9, "s={s}");
}

#[test]
fn speedup_zero_sample_is_none() {
    assert_eq!(speedup(Duration::from_millis(100), Duration::ZERO), None);
}

// --- aggregate_lock_blocked_ratio / total_entry_promotions ---

fn worker(inserted: u64, blocked: u64, acquired: u64, promotions: u64) -> HnswWorkerStats {
    HnswWorkerStats {
        inserted_nodes: inserted,
        busy: Duration::from_millis(1),
        link_lock_blocked: blocked,
        link_lock_acquired: acquired,
        link_lock_wait: Duration::ZERO,
        entry_promotions: promotions,
        degenerate_layer_searches: 0,
        reverse_link_reconnects: 0,
    }
}

#[test]
fn aggregate_lock_blocked_ratio_sums_before_dividing() {
    // ワーカー A: 1/10 blocked、ワーカー B: 90/90 blocked。単純平均だと 0.55 に
    // なるが、担当試行数で重みづけた合算後の比率は (1+90)/(10+90)=0.91 になる
    // ことを固定する（モジュールドキュメントの意図どおり担当量の少ない
    // ワーカーの比率が過大に効かないことを確認）。
    let workers = vec![worker(5, 1, 10, 0), worker(50, 90, 90, 0)];
    let ratio = aggregate_lock_blocked_ratio(&workers).unwrap();
    assert!((ratio - 0.91).abs() < 1e-9, "ratio={ratio}");
}

#[test]
fn aggregate_lock_blocked_ratio_zero_acquired_is_none() {
    let workers = vec![worker(0, 0, 0, 0)];
    assert_eq!(aggregate_lock_blocked_ratio(&workers), None);
}

#[test]
fn total_entry_promotions_sums_across_workers() {
    let workers = vec![worker(1, 0, 1, 2), worker(1, 0, 1, 3)];
    assert_eq!(total_entry_promotions(&workers), 5);
}

// --- pick_representative ---

fn profile_with_total(total_ms: u64) -> HnswBuildProfile {
    HnswBuildProfile {
        total: Duration::from_millis(total_ms),
        ..HnswBuildProfile::default()
    }
}

#[test]
fn pick_representative_selects_profile_closest_to_median_total() {
    let profiles = vec![
        profile_with_total(10),
        profile_with_total(20),
        profile_with_total(21),
        profile_with_total(100),
    ];
    // 中央値 (4 要素・線形補間) は sorted [10, 20, 21, 100] の中央 2 件 (20, 21)
    // の補間で 20.5ms。20ms・21ms が同じ距離 (0.5ms) で並ぶため、入力順で
    // 先に現れる 20ms を代表実行として選ぶ（`min_by_key` の決定的タイブレーク。
    // codex-review P2 指摘・PR #445 で `median_duration` を補間規則へ統一した
    // ことに伴う期待値更新）。
    let picked = pick_representative(&profiles).unwrap();
    assert_eq!(picked.total, Duration::from_millis(20));
}

#[test]
fn pick_representative_empty_is_none() {
    assert!(pick_representative(&[]).is_none());
}

// --- measured_tail ---

#[test]
fn measured_tail_excludes_leading_warmup_samples() {
    // `protocol::run` は warmup → 計測の順に `workload` を呼ぶため、蓄積列は
    // 先頭が warmup 標本・末尾が計測標本になる（`harness/protocol.rs::run`
    // モジュールコメント参照）。ここでは warmup 3 件・計測 2 件を模した
    // 合計 5 件の列から、末尾 2 件だけが返ることを固定する
    // （codex-review P1 指摘・PR #445）。
    let all: Vec<u32> = vec![
        1, // warmup
        2, // warmup
        3, // warmup
        4, // measured
        5, // measured
    ];
    let tail = measured_tail(&all, 2);
    assert_eq!(tail, &[4, 5]);
}

#[test]
fn measured_tail_insufficient_samples_is_empty() {
    let all: Vec<u32> = vec![1, 2];
    assert!(measured_tail(&all, 5).is_empty());
}

#[test]
fn measured_tail_zero_measured_iterations_is_empty() {
    let all: Vec<u32> = vec![1, 2, 3];
    assert!(measured_tail(&all, 0).is_empty());
}

#[test]
fn measured_tail_exact_length_returns_all() {
    let all: Vec<u32> = vec![1, 2, 3];
    assert_eq!(measured_tail(&all, 3), &[1, 2, 3]);
}

// --- aggregate_lock_wait / lock_wait_share ---

fn worker_with_wait(busy_ms: u64, wait_ms: u64) -> HnswWorkerStats {
    HnswWorkerStats {
        inserted_nodes: 1,
        busy: Duration::from_millis(busy_ms),
        link_lock_blocked: 1,
        link_lock_acquired: 1,
        link_lock_wait: Duration::from_millis(wait_ms),
        entry_promotions: 0,
        degenerate_layer_searches: 0,
        reverse_link_reconnects: 0,
    }
}

#[test]
fn aggregate_lock_wait_reports_sum_and_max() {
    let workers = vec![worker_with_wait(100, 5), worker_with_wait(100, 20)];
    let (sum, max) = aggregate_lock_wait(&workers);
    assert_eq!(sum, Duration::from_millis(25));
    assert_eq!(max, Duration::from_millis(20));
}

#[test]
fn aggregate_lock_wait_empty_is_zero() {
    let (sum, max) = aggregate_lock_wait(&[]);
    assert_eq!(sum, Duration::ZERO);
    assert_eq!(max, Duration::ZERO);
}

#[test]
fn lock_wait_share_divides_total_wait_by_total_busy() {
    let workers = vec![worker_with_wait(100, 10), worker_with_wait(100, 10)];
    let share = lock_wait_share(&workers).unwrap();
    // Σwait=20ms Σbusy=200ms -> 0.1
    assert!((share - 0.1).abs() < 1e-9, "share={share}");
}

#[test]
fn lock_wait_share_zero_busy_is_none() {
    let workers = vec![worker_with_wait(0, 0)];
    assert_eq!(lock_wait_share(&workers), None);
}

// --- parallel_vs_control_ceiling ---

#[test]
fn parallel_vs_control_ceiling_normalizes_linear_scaling_to_one() {
    // 並列側・対照側とも同一基準で線形スケールしている場合、ceiling は
    // 1.0（頭打ちなし）になるべき（codex-review P1 指摘・PR #445: 従来は
    // 基準スレッド数が異なっていたため線形スケール時でも 0.5 に系統的に
    // ずれていた）。
    let ceiling = parallel_vs_control_ceiling(Some(2.0), Some(2.0)).unwrap();
    assert!((ceiling - 1.0).abs() < 1e-9, "ceiling={ceiling}");
}

#[test]
fn parallel_vs_control_ceiling_missing_parallel_speedup_is_none() {
    assert_eq!(parallel_vs_control_ceiling(None, Some(2.0)), None);
}

#[test]
fn parallel_vs_control_ceiling_missing_control_speedup_rel_is_none() {
    assert_eq!(parallel_vs_control_ceiling(Some(2.0), None), None);
}

#[test]
fn parallel_vs_control_ceiling_zero_control_speedup_rel_is_nan() {
    let ceiling = parallel_vs_control_ceiling(Some(2.0), Some(0.0)).unwrap();
    assert!(ceiling.is_nan());
}

// --- render_memory_line（Issue #495） ---

#[test]
fn render_memory_line_reports_delta_when_both_present() {
    let line = render_memory_line(12, 1024, Some(1000), Some(1500), Some(1600));
    assert_eq!(
        line,
        "hnsw_parallel_build: memory threads=12 approx_heap_bytes=1024 \
         vm_rss_kb_before=1000 vm_rss_kb_after=1500 vm_rss_delta_kb=500 vm_hwm_kb=1600"
    );
}

#[test]
fn render_memory_line_reports_unavailable_when_proc_unreadable() {
    let line = render_memory_line(1, 512, None, None, None);
    assert_eq!(
        line,
        "hnsw_parallel_build: memory threads=1 approx_heap_bytes=512 \
         vm_rss_kb_before=unavailable vm_rss_kb_after=unavailable \
         vm_rss_delta_kb=unavailable vm_hwm_kb=unavailable"
    );
}

// --- repair_reachability 統計の集計・整形（Issue #447） ---

fn level_stats(
    level: usize,
    unreachable_before: u64,
    phase1_iterations: u64,
    phase1_cap_hit: bool,
    phase2_nodes: u64,
    phase1_wall_ms: u64,
    phase2_wall_ms: u64,
) -> HnswRepairLevelStats {
    HnswRepairLevelStats {
        level,
        unreachable_before,
        phase1_iterations,
        phase1_cap_hit,
        phase2_nodes,
        phase2_entry_relinked: 0,
        phase1_wall: Duration::from_millis(phase1_wall_ms),
        phase2_wall: Duration::from_millis(phase2_wall_ms),
    }
}

#[test]
fn repair_phase_wall_sum_adds_all_levels() {
    let stats = HnswRepairStats {
        levels: vec![
            level_stats(0, 1, 1, false, 0, 3, 2),
            level_stats(1, 0, 0, false, 0, 1, 1),
        ],
        wall: Duration::from_millis(10),
    };
    assert_eq!(repair_phase_wall_sum(&stats), Duration::from_millis(7));
}

#[test]
fn repair_wall_gap_returns_difference_when_outer_is_larger() {
    let stats = HnswRepairStats {
        levels: vec![],
        wall: Duration::from_millis(4),
    };
    assert_eq!(
        repair_wall_gap(&stats, Duration::from_millis(10)),
        Some(Duration::from_millis(6))
    );
}

#[test]
fn repair_wall_gap_is_none_when_outer_is_smaller_than_wall() {
    let stats = HnswRepairStats {
        levels: vec![],
        wall: Duration::from_millis(10),
    };
    assert_eq!(repair_wall_gap(&stats, Duration::from_millis(4)), None);
}

#[test]
fn repair_total_functions_sum_across_levels() {
    let stats = HnswRepairStats {
        levels: vec![
            level_stats(0, 5, 3, true, 2, 1, 1),
            level_stats(1, 1, 1, false, 0, 1, 1),
        ],
        wall: Duration::ZERO,
    };
    assert_eq!(repair_total_unreachable(&stats), 6);
    assert_eq!(repair_total_phase1_iterations(&stats), 4);
    assert_eq!(repair_total_phase2_nodes(&stats), 2);
    assert_eq!(repair_phase1_cap_hits(&stats), 1);
}

fn profile_with_repair_level(unreachable_before: u64) -> HnswBuildProfile {
    HnswBuildProfile {
        repair: HnswRepairStats {
            levels: vec![level_stats(0, unreachable_before, 0, false, 0, 0, 0)],
            wall: Duration::ZERO,
        },
        ..HnswBuildProfile::default()
    }
}

#[test]
fn repair_unreachable_per_level_min_med_max_reports_per_level_stats() {
    let profiles = vec![
        profile_with_repair_level(5),
        profile_with_repair_level(1),
        profile_with_repair_level(3),
    ];
    let per_level = repair_unreachable_per_level_min_med_max(&profiles);
    assert_eq!(per_level, vec![(0, (1, 3, 5))]);
}

#[test]
fn repair_unreachable_per_level_min_med_max_empty_profiles_is_empty() {
    assert_eq!(repair_unreachable_per_level_min_med_max(&[]), vec![]);
}

#[test]
fn format_per_level_renders_bracketed_list() {
    let per_level = vec![(0, (1u64, 3u64, 5u64)), (1, (0, 0, 2))];
    assert_eq!(format_per_level(&per_level), "[L0:1/3/5,L1:0/0/2]");
}

#[test]
fn format_per_level_empty_is_bracket_pair() {
    assert_eq!(format_per_level(&[]), "[]");
}
