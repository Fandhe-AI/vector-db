//! `benches/harness/knn_wire.rs`（Issue #463。`knn_wire_profile_bench.rs` が
//! wire／SQL 表層／距離カーネル・Top-k 内訳を切り分けるための rounds パース・
//! 段間差分の帰属計算・ノイズ帯判定・出力整形を担う純関数群）の回帰テスト。
//!
//! `crates/engine/tests/knn_profile_accept.rs` と同様、時間依存のベンチ本体は
//! 実行せず `#[path]` で取り込んだ純関数のみを `cargo test`（`make ci` 対象）で
//! 検証する（`harness/knn_wire.rs` 冒頭コメント「インラインの `#[cfg(test)]
//! mod tests` を置かない理由」参照）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::time::Duration;

use harness::knn_wire::{
    bucket_diff, classify_against_bands, diff_ratio_pct, median_of, min_of, parse_rounds,
    reference_band, refuse_under_github_actions, render_bucket_line, BandClass, KnnWireError,
    DEFAULT_ROUNDS,
};

#[test]
fn parse_rounds_defaults_when_unset() {
    assert_eq!(parse_rounds(None), Ok(DEFAULT_ROUNDS));
}

#[test]
fn parse_rounds_rejects_below_minimum() {
    assert!(matches!(
        parse_rounds(Some("4")),
        Err(KnnWireError::InvalidRounds(_))
    ));
}

#[test]
fn parse_rounds_rejects_above_maximum() {
    assert!(matches!(
        parse_rounds(Some("51")),
        Err(KnnWireError::InvalidRounds(_))
    ));
}

#[test]
fn parse_rounds_rejects_non_numeric() {
    assert!(matches!(
        parse_rounds(Some("abc")),
        Err(KnnWireError::InvalidRounds(_))
    ));
}

#[test]
fn parse_rounds_accepts_boundary_values() {
    assert_eq!(parse_rounds(Some("5")), Ok(5));
    assert_eq!(parse_rounds(Some("50")), Ok(50));
}

#[test]
fn refuse_under_github_actions_rejects_when_set() {
    assert_eq!(
        refuse_under_github_actions(true),
        Err(KnnWireError::RefusedUnderGitHubActions)
    );
    assert_eq!(refuse_under_github_actions(false), Ok(()));
}

#[test]
fn min_of_and_median_of_match_expected_values() {
    let samples = vec![
        Duration::from_micros(300),
        Duration::from_micros(100),
        Duration::from_micros(200),
    ];
    assert_eq!(min_of(&samples), Ok(Duration::from_micros(100)));
    assert_eq!(median_of(&samples), Ok(Duration::from_micros(200)));
}

#[test]
fn median_of_averages_middle_pair_for_even_length() {
    let samples = vec![
        Duration::from_micros(100),
        Duration::from_micros(200),
        Duration::from_micros(300),
        Duration::from_micros(400),
    ];
    assert_eq!(median_of(&samples), Ok(Duration::from_micros(250)));
}

#[test]
fn min_of_and_median_of_reject_empty_samples() {
    assert_eq!(min_of(&[]), Err(KnnWireError::EmptySamples));
    assert_eq!(median_of(&[]), Err(KnnWireError::EmptySamples));
}

#[test]
fn reference_band_computes_relative_spread() {
    let medians = vec![Duration::from_micros(100), Duration::from_micros(110)];
    let band = reference_band(&medians).expect("finite band");
    assert!((band - 0.10).abs() < 1e-9, "band={band}");
}

#[test]
fn reference_band_rejects_zero_min() {
    let medians = vec![Duration::ZERO, Duration::from_micros(10)];
    assert!(matches!(
        reference_band(&medians),
        Err(KnnWireError::DegenerateRatio(_))
    ));
}

#[test]
fn bucket_diff_returns_none_on_noise_induced_reversal() {
    assert_eq!(
        bucket_diff(Duration::from_micros(100), Duration::from_micros(90)),
        None
    );
    assert_eq!(
        bucket_diff(Duration::from_micros(90), Duration::from_micros(100)),
        Some(Duration::from_micros(10))
    );
}

#[test]
fn diff_ratio_pct_computes_percentage_of_total() {
    let ratio = diff_ratio_pct(Duration::from_micros(50), Duration::from_micros(200))
        .expect("finite ratio");
    assert!((ratio - 25.0).abs() < 1e-9, "ratio={ratio}");
}

#[test]
fn diff_ratio_pct_rejects_zero_total() {
    assert!(matches!(
        diff_ratio_pct(Duration::from_micros(1), Duration::ZERO),
        Err(KnnWireError::DegenerateRatio(_))
    ));
}

#[test]
fn classify_against_bands_uses_wider_of_fixed_and_reference() {
    // 固定帯（5%）より参照帯（8%）が広い場合、6% の差分はノイズ帯内。
    assert_eq!(classify_against_bands(6.0, 8.0), BandClass::WithinNoiseBand);
    // 実測帯（2%）より固定帯（5%）が広い場合、4% の差分はノイズ帯内。
    assert_eq!(classify_against_bands(4.0, 2.0), BandClass::WithinNoiseBand);
    // いずれの帯（5%・2%）も超える 9% はノイズ帯外。
    assert_eq!(classify_against_bands(9.0, 2.0), BandClass::AboveNoiseBand);
}

#[test]
fn render_bucket_line_reports_na_on_missing_diff() {
    let line = render_bucket_line("sql_surface", None, None, None);
    assert!(line.contains("n/a"));
}

#[test]
fn render_bucket_line_includes_diff_ratio_and_band() {
    let line = render_bucket_line(
        "sql_surface",
        Some(Duration::from_micros(50)),
        Some(25.0),
        Some(BandClass::WithinNoiseBand),
    );
    assert!(line.contains("sql_surface"));
    assert!(line.contains("25.00%"));
    assert!(line.contains("within_noise_band"));
}
