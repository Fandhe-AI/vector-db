//! `benches/harness/accept.rs`（TASK-127 受け入れ判定ヘルパ）の回帰テスト。
//!
//! 対象ビヘイビア: CORE-3, CORE-4, CORE-5, SEARCH-4（ポインタ: `docs/spec/05-tasks.md`
//! TASK-127）。`parallel_bench.rs`・`contrast_bench.rs` は時間依存のためこのテストからは
//! 実行しない（`tests/bench_harness.rs` と同様、実測タイマーに依存しない判定ロジックのみを
//! `#[path]` で取り込み `cargo test`（`make ci` 対象）で検証する）。CORE-5 は
//! `contrast_bench.rs` で接続済み（Issue #176）。

// 本テストは `accept`（受け入れ判定ヘルパ）と、そのエラー型 `stats::BenchError` のみを
// 検証対象とし `ab`/`protocol`/`rng`（harness 自体の契約は `tests/bench_harness.rs` が
// 別途検証）は経由しない。未到達の `pub` 項目が dead_code として警告されうるため許容する
// （`measurement.rs`・`parallel_smoke.rs`・`tests/bench_harness.rs` と同一の理由・対処。
// `harness/mod.rs` 自体は変更しない）。
#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::accept::{
    check_contrast_ratio_within_limit, check_p95_within_limit, check_recall_within_limit,
    p95_from_samples, p95_ratio, parse_contrast_ratio_limit, recall_at_k, worst_recall,
};
use harness::stats::BenchError;
use std::time::Duration;

// recall_at_k（CORE-4）。

#[test]
fn recall_at_k_is_one_when_actual_matches_expected_exactly() {
    let expected = vec![1, 2, 3, 4, 5];
    let actual = vec![5, 4, 3, 2, 1]; // 順序が異なっても id 集合が一致すれば 1.0
    let recall = recall_at_k(&expected, &actual).unwrap();
    assert!((recall - 1.0).abs() < f64::EPSILON);
}

#[test]
fn recall_at_k_reflects_partial_overlap() {
    let expected = vec![1, 2, 3, 4];
    let actual = vec![1, 2, 99, 100]; // 4 件中 2 件一致
    let recall = recall_at_k(&expected, &actual).unwrap();
    assert!((recall - 0.5).abs() < f64::EPSILON);
}

#[test]
fn recall_at_k_is_zero_when_no_overlap() {
    let expected = vec![1, 2, 3];
    let actual = vec![4, 5, 6];
    let recall = recall_at_k(&expected, &actual).unwrap();
    assert_eq!(recall, 0.0);
}

#[test]
fn recall_at_k_ignores_extra_actual_ids_beyond_expected_set() {
    // actual が expected より多くの id を含んでいても、一致率は expected 基準で
    // 頭打ちになる（分母は expected.len()）。
    let expected = vec![1, 2];
    let actual = vec![1, 2, 3, 4, 5];
    let recall = recall_at_k(&expected, &actual).unwrap();
    assert!((recall - 1.0).abs() < f64::EPSILON);
}

#[test]
fn recall_at_k_deduplicates_repeated_actual_ids() {
    // actual に重複 id が含まれても、集合演算前に重複除去するため Recall は
    // 1.0 を超えない（重複除去しないと expected=[1,2] / actual=[1,1,2,2] で
    // matched=4 となり recall=2.0 という不正値になる）。
    let expected = vec![1, 2];
    let actual = vec![1, 1, 2, 2];
    let recall = recall_at_k(&expected, &actual).unwrap();
    assert!((recall - 1.0).abs() < f64::EPSILON);
}

#[test]
fn recall_at_k_rejects_empty_expected_set() {
    let err = recall_at_k(&[], &[1, 2, 3]).unwrap_err();
    assert_eq!(err, BenchError::EmptySamples);
}

// worst_recall（CORE-4）。

#[test]
fn worst_recall_picks_the_minimum_of_many_values() {
    let recalls = vec![1.0; 19]
        .into_iter()
        .chain(std::iter::once(0.95))
        .collect::<Vec<f64>>();
    // 19 件が完全一致（1.0）でも 1 件の不一致（0.95）がそのまま判定に反映される
    // ことを確認する（平均だと 0.9975 相当に埋もれてしまう回帰防止）。
    let recall = worst_recall(&recalls).unwrap();
    assert!((recall - 0.95).abs() < f64::EPSILON);
}

#[test]
fn worst_recall_returns_the_single_value_for_singleton_input() {
    let recall = worst_recall(&[0.5]).unwrap();
    assert!((recall - 0.5).abs() < f64::EPSILON);
}

#[test]
fn worst_recall_rejects_empty_input() {
    let err = worst_recall(&[]).unwrap_err();
    assert_eq!(err, BenchError::EmptySamples);
}

// p95_from_samples（CORE-3・SEARCH-4）。

#[test]
fn p95_from_samples_rejects_empty_input() {
    let err = p95_from_samples(&[]).unwrap_err();
    assert_eq!(err, BenchError::EmptySamples);
}

#[test]
fn p95_from_samples_picks_nearest_rank_from_twenty_samples() {
    // 1..=20 ms の 20 点。rank = ceil(20 * 0.95) = 19 -> idx=18 (0-indexed) -> 19ms。
    let samples: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
    let p95 = p95_from_samples(&samples).unwrap();
    assert_eq!(p95, Duration::from_millis(19));
}

#[test]
fn p95_from_samples_is_order_independent() {
    let ascending: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
    let mut shuffled = ascending.clone();
    shuffled.reverse();
    assert_eq!(
        p95_from_samples(&ascending).unwrap(),
        p95_from_samples(&shuffled).unwrap()
    );
}

#[test]
fn p95_from_samples_returns_the_single_sample_for_singleton_input() {
    let p95 = p95_from_samples(&[Duration::from_millis(42)]).unwrap();
    assert_eq!(p95, Duration::from_millis(42));
}

// check_p95_within_limit（CORE-3・SEARCH-4）。

#[test]
fn check_p95_within_limit_accepts_equal_to_limit() {
    assert!(check_p95_within_limit(
        Duration::from_millis(100),
        Duration::from_millis(100)
    ));
}

#[test]
fn check_p95_within_limit_rejects_over_limit() {
    assert!(!check_p95_within_limit(
        Duration::from_millis(101),
        Duration::from_millis(100)
    ));
}

// check_recall_within_limit（CORE-4）。

#[test]
fn check_recall_within_limit_accepts_recall_meeting_threshold() {
    assert!(check_recall_within_limit(0.99, 0.99).unwrap());
    assert!(check_recall_within_limit(1.0, 0.99).unwrap());
}

#[test]
fn check_recall_within_limit_rejects_recall_below_threshold() {
    assert!(!check_recall_within_limit(0.98, 0.99).unwrap());
}

#[test]
fn check_recall_within_limit_rejects_out_of_range_min_recall() {
    let err = check_recall_within_limit(0.5, 1.5).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = check_recall_within_limit(0.5, -0.1).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn check_recall_within_limit_rejects_zero_min_recall() {
    // 0.0 を許容すると「どんな recall 値でも pass」となり CORE-4 のゲートが
    // 実質的に無効化されるため、`min_recall_from_env`（parallel_bench.rs）と同様に
    // 下限からも除外する（`(0.0, 1.0]`）。
    let err = check_recall_within_limit(0.0, 0.0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

// check_contrast_ratio_within_limit（CORE-5。本 PR では parallel_bench.rs から未接続だが
// 判定ロジック自体は検証しておく）。

#[test]
fn check_contrast_ratio_within_limit_accepts_ratio_at_or_below_limit() {
    assert!(check_contrast_ratio_within_limit(1.5, 1.5).unwrap());
    assert!(check_contrast_ratio_within_limit(1.0, 1.5).unwrap());
}

#[test]
fn check_contrast_ratio_within_limit_rejects_ratio_above_limit() {
    assert!(!check_contrast_ratio_within_limit(1.51, 1.5).unwrap());
}

#[test]
fn check_contrast_ratio_within_limit_rejects_non_finite_or_negative_ratio() {
    let err = check_contrast_ratio_within_limit(f64::NAN, 1.5).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));

    let err = check_contrast_ratio_within_limit(f64::INFINITY, 1.5).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));

    let err = check_contrast_ratio_within_limit(-0.1, 1.5).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));
}

#[test]
fn check_contrast_ratio_within_limit_rejects_non_positive_max_ratio() {
    let err = check_contrast_ratio_within_limit(1.0, 0.0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = check_contrast_ratio_within_limit(1.0, -1.0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

// p95_ratio（CORE-5・Issue #176）。

#[test]
fn p95_ratio_computes_ratio_of_p95_values() {
    let a: Vec<Duration> = (1..=20).map(Duration::from_millis).collect(); // p95=19ms
    let b: Vec<Duration> = (1..=20).map(|i| Duration::from_millis(i * 2)).collect(); // p95=38ms
    let ratio = p95_ratio(&a, &b).unwrap();
    assert!((ratio - 0.5).abs() < 1e-9);
}

#[test]
fn p95_ratio_rejects_empty_samples() {
    let a: Vec<Duration> = vec![];
    let b: Vec<Duration> = vec![Duration::from_millis(1)];
    let err = p95_ratio(&a, &b).unwrap_err();
    assert_eq!(err, BenchError::EmptySamples);

    let a: Vec<Duration> = vec![Duration::from_millis(1)];
    let b: Vec<Duration> = vec![];
    let err = p95_ratio(&a, &b).unwrap_err();
    assert_eq!(err, BenchError::EmptySamples);
}

#[test]
fn p95_ratio_rejects_zero_baseline() {
    let a: Vec<Duration> = vec![Duration::from_millis(1)];
    let b: Vec<Duration> = vec![Duration::ZERO];
    let err = p95_ratio(&a, &b).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));
}

// parse_contrast_ratio_limit（CORE-5・Issue #176）。

#[test]
fn parse_contrast_ratio_limit_accepts_positive_finite_value() {
    assert!((parse_contrast_ratio_limit("1.5").unwrap() - 1.5).abs() < f64::EPSILON);
    // 前後の空白は trim される（GitHub Actions の repo variable 経由でも安全に扱える）。
    assert!((parse_contrast_ratio_limit(" 2.0 \n").unwrap() - 2.0).abs() < f64::EPSILON);
}

#[test]
fn parse_contrast_ratio_limit_rejects_empty_string() {
    // 未設定の repo variable は GitHub Actions 上で空文字列に解決されるため
    // （`.github/workflows/bench.yml` 参照）、空文字列を明示的に拒否できることを検証する。
    let err = parse_contrast_ratio_limit("").unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = parse_contrast_ratio_limit("   ").unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn parse_contrast_ratio_limit_rejects_non_numeric_string() {
    let err = parse_contrast_ratio_limit("abc").unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn parse_contrast_ratio_limit_rejects_non_positive_or_non_finite_value() {
    let err = parse_contrast_ratio_limit("0").unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = parse_contrast_ratio_limit("-1.0").unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = parse_contrast_ratio_limit("NaN").unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = parse_contrast_ratio_limit("inf").unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}
