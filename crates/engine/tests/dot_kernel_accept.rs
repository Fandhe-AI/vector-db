//! `benches/harness/dot_kernel.rs`（Issue #365。`isa.rs` dot カーネルの複数
//! アキュムレータ化マイクロベンチ）の回帰テスト。
//!
//! `dot_kernel_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hybrid_latency_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::dot_kernel::{
    check_matches_scalar_reference, classify_change, generate_corpus, generate_query, ns_per_dot,
    refuse_under_github_actions, render_line, rows_for, speedup_ratio, ChangeClass, DotKernelError,
    WorkingSet, ARENA_SCALE_ROWS, MAX_CORPUS_ROWS_GUARD,
};
use std::time::Duration;

// --- refuse_under_github_actions ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, DotKernelError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- rows_for ---

#[test]
fn rows_for_cache_resident_is_deterministic_and_positive() {
    let a = rows_for(WorkingSet::CacheResident, 768).expect("rows ok");
    let b = rows_for(WorkingSet::CacheResident, 768).expect("rows ok");
    assert_eq!(a, b);
    assert!(a >= 1);
}

#[test]
fn rows_for_cache_resident_shrinks_as_dim_grows() {
    let small_dim = rows_for(WorkingSet::CacheResident, 128).expect("rows ok");
    let large_dim = rows_for(WorkingSet::CacheResident, 1536).expect("rows ok");
    assert!(
        large_dim <= small_dim,
        "larger dim must not need more rows to fill the same byte budget: {large_dim} vs {small_dim}"
    );
}

#[test]
fn rows_for_cache_resident_has_lower_bound_of_one() {
    // 極端に大きい dim でもゼロ行にはならない（最低 1 行保証）。
    let rows = rows_for(WorkingSet::CacheResident, 10_000_000).expect("rows ok");
    assert_eq!(rows, 1);
}

#[test]
fn rows_for_arena_scale_is_fixed() {
    let rows = rows_for(WorkingSet::ArenaScale, 128).expect("rows ok");
    assert_eq!(rows, ARENA_SCALE_ROWS);
}

// --- generate_corpus ---

#[test]
fn generate_corpus_is_deterministic_for_same_seed() {
    let a = generate_corpus(1, 8, 16).expect("corpus ok");
    let b = generate_corpus(1, 8, 16).expect("corpus ok");
    assert_eq!(a, b);
    assert_eq!(a.len(), 8 * 16);
}

#[test]
fn generate_corpus_differs_across_seeds() {
    let a = generate_corpus(1, 8, 16).expect("corpus ok");
    let b = generate_corpus(2, 8, 16).expect("corpus ok");
    assert_ne!(a, b, "異なるシードから同一コーパスが生成された");
}

#[test]
fn generate_corpus_rejects_rows_beyond_guard() {
    let err = generate_corpus(1, 8, MAX_CORPUS_ROWS_GUARD + 1).unwrap_err();
    assert_eq!(err, DotKernelError::CorpusTooLarge);
}

// --- generate_query ---

#[test]
fn generate_query_is_deterministic_and_differs_from_corpus_seed() {
    let q1 = generate_query(1, 8);
    let q2 = generate_query(1, 8);
    assert_eq!(q1, q2);
    assert_eq!(q1.len(), 8);

    let corpus_row = generate_corpus(1, 8, 1).expect("corpus ok");
    assert_ne!(
        q1, corpus_row,
        "クエリ生成がコーパス生成と同一の乱数系列を共有している"
    );
}

// --- ns_per_dot ---

#[test]
fn ns_per_dot_rejects_zero_dots() {
    let err = ns_per_dot(Duration::from_millis(10), 0).unwrap_err();
    assert_eq!(err, DotKernelError::ZeroDots);
}

#[test]
fn ns_per_dot_computes_expected_value() {
    let ns = ns_per_dot(Duration::from_millis(1), 1_000_000).expect("ns ok");
    assert!((ns - 1.0).abs() < 1e-6, "expected ~1.0 ns/dot, got {ns}");
}

// --- speedup_ratio / classify_change ---

#[test]
fn speedup_ratio_computes_candidate_over_baseline() {
    assert!((speedup_ratio(100.0, 50.0) - 0.5).abs() < 1e-9);
    assert!((speedup_ratio(100.0, 150.0) - 1.5).abs() < 1e-9);
}

#[test]
fn classify_change_boundaries() {
    let noise_band = 0.05;
    assert_eq!(
        classify_change(0.95, noise_band),
        ChangeClass::Improved,
        "ratio exactly at the improved boundary must classify as Improved"
    );
    assert_eq!(classify_change(0.90, noise_band), ChangeClass::Improved);
    assert_eq!(classify_change(1.0, noise_band), ChangeClass::Neutral);
    assert_eq!(
        classify_change(1.05, noise_band),
        ChangeClass::Regressed,
        "ratio exactly at the regressed boundary must classify as Regressed"
    );
    assert_eq!(classify_change(1.10, noise_band), ChangeClass::Regressed);
    // ノイズ帯の内側（境界未満・未満）は Neutral。
    assert_eq!(classify_change(0.96, noise_band), ChangeClass::Neutral);
    assert_eq!(classify_change(1.04, noise_band), ChangeClass::Neutral);
}

// --- render_line ---

#[test]
fn render_line_includes_all_fields() {
    let line = render_line(
        "current",
        WorkingSet::CacheResident,
        768,
        20,
        Duration::from_millis(5),
        123.4,
    );
    assert!(line.contains("label=current"));
    assert!(line.contains("working_set=cache_resident"));
    assert!(line.contains("dim=768"));
    assert!(line.contains("rows=20"));
    assert!(line.contains("ns_per_dot=123.40"));
}

#[test]
fn render_line_arena_scale_label() {
    let line = render_line(
        "current",
        WorkingSet::ArenaScale,
        128,
        25_000,
        Duration::from_millis(5),
        1.0,
    );
    assert!(line.contains("working_set=arena_scale"));
}

// --- check_matches_scalar_reference ---

#[test]
fn check_matches_scalar_reference_accepts_exact_match() {
    assert!(check_matches_scalar_reference(1.0, 1.0, 1.0).is_ok());
}

#[test]
fn check_matches_scalar_reference_accepts_within_tolerance() {
    assert!(check_matches_scalar_reference(100.00005, 100.0, 100.0).is_ok());
}

#[test]
fn check_matches_scalar_reference_rejects_beyond_tolerance() {
    let err = check_matches_scalar_reference(2.0, 1.0, 1.0).unwrap_err();
    assert_eq!(
        err,
        DotKernelError::ToleranceExceeded {
            actual: 2.0,
            expected: 1.0
        }
    );
}

#[test]
fn check_matches_scalar_reference_rejects_non_finite() {
    let err = check_matches_scalar_reference(f32::NAN, 1.0, 1.0).unwrap_err();
    assert_eq!(err, DotKernelError::NonFiniteResult);
}
