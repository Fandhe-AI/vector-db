//! `benches/harness/dot_block.rs`（Issue #512。行ブロックカーネル
//! `SimdKernel::dot_block4` の前後比較・採否記録）の回帰テスト。
//!
//! `dot_kernel_bench.rs` の block4 A/B セクションは時間依存のためこのテストから
//! は実行しない（`tests/dot_kernel_accept.rs` と同様、実測タイマー・env に
//! 依存しない時間非依存の契約のみを `#[path]` で取り込み `cargo test`
//! 〔`make ci` 対象〕で検証する）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::dot_block::{
    check_block_bit_identical, min_of_samples, parse_block_ab_env, relative_band,
    render_block_ab_line, render_block_ab_reference_line, BlockAbMode, DotBlockError,
    BLOCK_AB_DIMS,
};
use std::time::Duration;

// --- parse_block_ab_env ---

#[test]
fn parse_block_ab_env_unset_is_off() {
    assert_eq!(parse_block_ab_env(None).unwrap(), BlockAbMode::Off);
}

#[test]
fn parse_block_ab_env_zero_is_off() {
    assert_eq!(parse_block_ab_env(Some("0")).unwrap(), BlockAbMode::Off);
}

#[test]
fn parse_block_ab_env_one_is_on() {
    assert_eq!(parse_block_ab_env(Some("1")).unwrap(), BlockAbMode::On);
}

#[test]
fn parse_block_ab_env_empty_string_is_rejected() {
    let err = parse_block_ab_env(Some("")).unwrap_err();
    assert_eq!(err, DotBlockError::InvalidEnvValue);
}

#[test]
fn parse_block_ab_env_arbitrary_value_is_rejected() {
    let err = parse_block_ab_env(Some("true")).unwrap_err();
    assert_eq!(err, DotBlockError::InvalidEnvValue);
}

#[test]
fn parse_block_ab_env_numeric_but_out_of_range_is_rejected() {
    let err = parse_block_ab_env(Some("2")).unwrap_err();
    assert_eq!(err, DotBlockError::InvalidEnvValue);
}

// --- check_block_bit_identical ---

#[test]
fn check_block_bit_identical_accepts_exact_match() {
    assert!(check_block_bit_identical(128, 0, 0, 1.5f32, 1.5f32).is_ok());
}

#[test]
fn check_block_bit_identical_accepts_signed_zero_bit_match() {
    // -0.0 と -0.0 はビット同一だが 0.0 とはビットが異なる（IEEE754 符号ビット）。
    assert!(check_block_bit_identical(128, 0, 0, -0.0f32, -0.0f32).is_ok());
}

#[test]
fn check_block_bit_identical_rejects_signed_zero_vs_positive_zero() {
    let err = check_block_bit_identical(128, 0, 0, -0.0f32, 0.0f32).unwrap_err();
    match err {
        DotBlockError::BitMismatch {
            dim,
            block_idx,
            lane,
            ..
        } => {
            assert_eq!(dim, 128);
            assert_eq!(block_idx, 0);
            assert_eq!(lane, 0);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn check_block_bit_identical_rejects_nan_bit_mismatch() {
    // 異なるビットパターンの NaN は `==` では両方 false だが本関数は
    // `to_bits()` 完全一致で判定するため確実に不一致検出できる。
    let nan_a = f32::from_bits(0x7fc0_0001);
    let nan_b = f32::from_bits(0x7fc0_0002);
    let err = check_block_bit_identical(768, 3, 2, nan_a, nan_b).unwrap_err();
    assert!(matches!(err, DotBlockError::BitMismatch { .. }));
}

#[test]
fn check_block_bit_identical_rejects_close_but_distinct_values() {
    // 実質的な数値近似ではなくビット完全一致契約であることを固定する。
    let err = check_block_bit_identical(128, 1, 1, 1.0f32, 1.0000001f32).unwrap_err();
    assert!(matches!(err, DotBlockError::BitMismatch { .. }));
}

// --- min_of_samples ---

#[test]
fn min_of_samples_rejects_empty() {
    let err = min_of_samples(&[]).unwrap_err();
    assert_eq!(err, DotBlockError::EmptySamples);
}

#[test]
fn min_of_samples_returns_minimum() {
    let samples = [
        Duration::from_millis(30),
        Duration::from_millis(10),
        Duration::from_millis(20),
    ];
    assert_eq!(min_of_samples(&samples).unwrap(), Duration::from_millis(10));
}

#[test]
fn min_of_samples_single_element() {
    let samples = [Duration::from_micros(7)];
    assert_eq!(min_of_samples(&samples).unwrap(), Duration::from_micros(7));
}

// --- relative_band ---

#[test]
fn relative_band_rejects_empty() {
    let err = relative_band(&[]).unwrap_err();
    assert_eq!(err, DotBlockError::EmptySamples);
}

#[test]
fn relative_band_rejects_non_finite() {
    let err = relative_band(&[1.0, f64::NAN, 1.2]).unwrap_err();
    assert_eq!(err, DotBlockError::NonPositiveOrNonFiniteBaseline);
}

#[test]
fn relative_band_rejects_infinite() {
    let err = relative_band(&[1.0, f64::INFINITY]).unwrap_err();
    assert_eq!(err, DotBlockError::NonPositiveOrNonFiniteBaseline);
}

#[test]
fn relative_band_rejects_zero_or_negative_minimum() {
    let err = relative_band(&[0.0, 1.0]).unwrap_err();
    assert_eq!(err, DotBlockError::NonPositiveOrNonFiniteBaseline);
    let err = relative_band(&[-1.0, 1.0]).unwrap_err();
    assert_eq!(err, DotBlockError::NonPositiveOrNonFiniteBaseline);
}

#[test]
fn relative_band_computes_max_minus_min_over_min() {
    // (1.2 - 1.0) / 1.0 = 0.2
    let band = relative_band(&[1.0, 1.1, 1.2]).unwrap();
    assert!((band - 0.2).abs() < 1e-12);
}

#[test]
fn relative_band_single_element_is_zero() {
    let band = relative_band(&[3.5]).unwrap();
    assert_eq!(band, 0.0);
}

// --- render_block_ab_line / render_block_ab_reference_line ---

#[test]
fn render_block_ab_line_uses_non_colliding_prefix() {
    let line = render_block_ab_line(
        "cache_resident",
        128,
        Duration::from_micros(100),
        Duration::from_micros(80),
        0.8,
    );
    // `harness/chip.rs::parse_dot_kernel_line` は `dot_kernel: label=` 接頭辞
    // のみを拾うため、block4 A/B 行はこの接頭辞と衝突してはならない
    // （`docs/design/dot-kernel-row-block.md` §5 の bench-chip 非破壊要件）。
    assert!(line.starts_with("dot_kernel: block4_ab "));
    assert!(!line.starts_with("dot_kernel: label="));
    assert!(line.contains("working_set=cache_resident"));
    assert!(line.contains("dim=128"));
}

#[test]
fn render_block_ab_reference_line_uses_non_colliding_prefix() {
    let line = render_block_ab_reference_line("arena_scale", 768, 0.0123);
    assert!(line.starts_with("dot_kernel: block4_ab_ref "));
    assert!(!line.starts_with("dot_kernel: label="));
    assert!(line.contains("working_set=arena_scale"));
    assert!(line.contains("dim=768"));
}

// --- BLOCK_AB_DIMS ---

#[test]
fn block_ab_dims_matches_row_block_documented_workloads() {
    // `docs/design/dot-kernel-row-block.md` の主要ワークロード次元（128・768）と
    // 一致すること（doc とコードの記述が乖離しないことの固定）。
    assert_eq!(BLOCK_AB_DIMS, [128, 768]);
}
