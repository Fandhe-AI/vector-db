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

use engine::hnsw::{HnswBuildProfile, HnswWorkerStats};
use harness::hnsw_parallel_profile::{
    aggregate_lock_blocked_ratio, median_duration, min_median_max_duration, min_median_max_u64,
    pick_representative, serial_share, speedup, total_entry_promotions,
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
fn median_duration_even_count_picks_lower_of_middle_pair() {
    // 4 要素ソート後 [10, 20, 30, 40] の中央値は下位側 (index len/2 = 2) を採用する。
    let values = vec![
        Duration::from_millis(40),
        Duration::from_millis(10),
        Duration::from_millis(30),
        Duration::from_millis(20),
    ];
    assert_eq!(median_duration(&values), Some(Duration::from_millis(30)));
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

// --- serial_share ---

#[test]
fn serial_share_computes_ratio_of_sequential_stages_to_total() {
    let share = serial_share(
        Duration::from_millis(1),
        Duration::from_millis(2),
        Duration::from_millis(3),
        Duration::from_millis(4),
        Duration::from_millis(20),
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
        entry_promotions: promotions,
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
    // 中央値 (4 要素・下位側採用) は sorted [10, 20, 21, 100] の index 2 = 21。
    let picked = pick_representative(&profiles).unwrap();
    assert_eq!(picked.total, Duration::from_millis(21));
}

#[test]
fn pick_representative_empty_is_none() {
    assert!(pick_representative(&[]).is_none());
}
