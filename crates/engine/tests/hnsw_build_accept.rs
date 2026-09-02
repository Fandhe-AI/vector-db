//! `benches/harness/hnsw_build.rs`（TASK-132・Issue #404。HNSW グラフ構築の
//! 受け入れ条件 (b): 構築計算量スケーリング確認）の回帰テスト。
//!
//! `hnsw_build_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/dot_kernel_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::hnsw_build::{
    classify_scaling, generate_corpus, n_log_n_ratio, refuse_under_github_actions,
    scaling_exponent, HnswBuildBenchError, ScalingClass, DEFAULT_SUPER_LINEAR_THRESHOLD,
    MAX_CORPUS_ROWS_GUARD, MAX_DIM_GUARD,
};
use std::time::Duration;

// --- refuse_under_github_actions ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, HnswBuildBenchError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- generate_corpus ---

#[test]
fn generate_corpus_deterministic_across_calls() {
    let a = generate_corpus(99, 16, 100).unwrap();
    let b = generate_corpus(99, 16, 100).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.len(), 1_600);
}

#[test]
fn generate_corpus_rejects_rows_guard_overflow() {
    let err = generate_corpus(1, 8, MAX_CORPUS_ROWS_GUARD + 1).unwrap_err();
    assert_eq!(err, HnswBuildBenchError::CorpusTooLarge);
}

#[test]
fn generate_corpus_rejects_dim_guard_overflow() {
    let err = generate_corpus(1, MAX_DIM_GUARD + 1, 8).unwrap_err();
    assert_eq!(err, HnswBuildBenchError::CorpusTooLarge);
}

// --- scaling_exponent / n_log_n_ratio / classify_scaling ---

#[test]
fn scaling_exponent_linear_case_is_close_to_one() {
    let k = scaling_exponent(
        1_000,
        Duration::from_millis(10),
        4_000,
        Duration::from_millis(40),
    )
    .unwrap();
    assert!((k - 1.0).abs() < 1e-9, "k={k}");
}

#[test]
fn scaling_exponent_quadratic_case_is_close_to_two() {
    let k = scaling_exponent(
        1_000,
        Duration::from_millis(10),
        4_000,
        Duration::from_millis(160),
    )
    .unwrap();
    assert!((k - 2.0).abs() < 1e-9, "k={k}");
}

#[test]
fn scaling_exponent_rejects_equal_n() {
    let err =
        scaling_exponent(500, Duration::from_millis(1), 500, Duration::from_millis(2)).unwrap_err();
    assert_eq!(err, HnswBuildBenchError::InsufficientSamples);
}

#[test]
fn classify_scaling_boundary_is_inclusive_super_linear() {
    assert_eq!(
        classify_scaling(
            DEFAULT_SUPER_LINEAR_THRESHOLD,
            DEFAULT_SUPER_LINEAR_THRESHOLD
        ),
        ScalingClass::SuperLinear
    );
    assert_eq!(
        classify_scaling(
            DEFAULT_SUPER_LINEAR_THRESHOLD - 0.01,
            DEFAULT_SUPER_LINEAR_THRESHOLD
        ),
        ScalingClass::NearNLogN
    );
}

#[test]
fn n_log_n_ratio_is_positive_for_valid_input() {
    let ratio = n_log_n_ratio(1_000, Duration::from_millis(50)).unwrap();
    assert!(ratio > 0.0);
}
