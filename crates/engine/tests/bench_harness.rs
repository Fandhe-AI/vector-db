//! `benches/harness/`（TASK-158 性能計測プロトコル基盤）の回帰テスト。
//!
//! 対象ビヘイビア ID なし（TASK-158 は基盤タスク。ポインタ: `docs/spec/05-tasks.md`
//! TASK-158）。ビヘイビア ID を持たないため、本テストは harness 自体の実装契約
//! （各モジュールの公開 API が示す契約を遵守すること）を受け入れ条件として検証する。
//!
//! `#[path]` で `benches/harness/mod.rs` を直接取り込む（内部クレートを新設せず、
//! `cargo bench` 入口〔`benches/measurement.rs`〕と同一ソースを共有する構成。
//! `harness/mod.rs` のモジュールドキュメント参照）。

#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::ab::{median_ratio, run_ab, AbMeasurement};
use harness::protocol::{run, Measurement, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::stats::{self, BenchError, Summary};
use std::time::Duration;

// protocol::MeasurementConfig の下限拒否（fail-closed）。

#[test]
fn config_rejects_warmup_below_min_warmup_iterations() {
    let err = MeasurementConfig::new(19, 20, 0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn config_rejects_measured_below_min_measured_iterations() {
    let err = MeasurementConfig::new(20, 19, 0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn config_rejects_iterations_above_max_iterations() {
    // 開発者操作起点とはいえ、上限検証なしには measured_iterations がそのまま
    // Vec::with_capacity に渡され無制限アロケーションを起こしうる
    // （coding-rust.md: 無制限 Vec::with_capacity 禁止）。
    let err = MeasurementConfig::new(20, u32::MAX, 0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = MeasurementConfig::new(u32::MAX, 20, 0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn default_config_matches_min_iterations_and_run_accepts_it() {
    let config = MeasurementConfig::default();
    // シードはこの検証では使わないが、下限だけでなくシード保持そのものが
    // 検証コンストラクタ・Default 双方から一貫して読めることを確認する。
    assert_eq!(config.seed(), 0);
    assert_eq!(config.warmup_iterations(), 20);
    assert_eq!(config.measured_iterations(), 20);

    let mut calls = 0u32;
    let measurement: Measurement = run(&config, || {
        calls += 1;
        calls
    })
    .expect("default config must be accepted by run");

    assert_eq!(calls, 40); // warmup 20 + 計測 20
    assert_eq!(measurement.samples.len(), 20);
    let _summary: Summary = measurement.summary;
}

// stats: 中央値・Q1/Q3 の記録・空サンプルの fail-closed 経路。

#[test]
fn measurement_records_median_and_quartiles() {
    let config = MeasurementConfig::new(20, 20, 0).unwrap();
    let measurement = run(&config, || {
        std::thread::sleep(Duration::from_micros(1));
    })
    .expect("valid config must run");

    assert!(measurement.summary.q1 <= measurement.summary.median);
    assert!(measurement.summary.median <= measurement.summary.q3);
}

#[test]
fn stats_summarize_matches_expected_statistics_for_known_samples() {
    // 1..=9 ms の 9 点。線形補間パーセンタイルでは
    // median=5ms, Q1=3ms, Q3=7ms（等間隔データのため補間による端数は出ない）。
    let samples: Vec<Duration> = (1..=9).map(Duration::from_millis).collect();
    let summary = stats::summarize(&samples).expect("non-empty samples must summarize");
    assert_eq!(summary.median, Duration::from_millis(5));
    assert_eq!(summary.q1, Duration::from_millis(3));
    assert_eq!(summary.q3, Duration::from_millis(7));
}

#[test]
fn stats_summarize_rejects_empty_samples() {
    let err = stats::summarize(&[]).unwrap_err();
    assert_eq!(err, BenchError::EmptySamples);
}

#[test]
fn stats_summarize_interpolates_at_min_measured_iterations_sample_count() {
    // MIN_MEASURED_ITERATIONS（20 サンプル）ちょうどでの実運用経路。
    // last_index=19 のため q1 のランクは 0.25*19=4.75 と非整数になり、
    // `percentile` の線形補間分岐（lower_index != upper_index）を必ず通る
    // （n=9 の他テストは端数の出ない特殊ケースのみを通っていたため、本番で
    // 必ず踏む経路を別途検証する）。
    let samples: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
    let summary = stats::summarize(&samples).expect("non-empty samples must summarize");

    // 期待値: q1 rank=4.75 -> 5ms + (6ms-5ms)*0.75 = 5.75ms
    //         median rank=9.5 -> 10ms + (11ms-10ms)*0.5 = 10.5ms
    //         q3 rank=14.25 -> 15ms + (16ms-15ms)*0.25 = 15.25ms
    assert_eq!(summary.q1, Duration::from_micros(5_750));
    assert_eq!(summary.median, Duration::from_micros(10_500));
    assert_eq!(summary.q3, Duration::from_micros(15_250));
}

// rng: 同一シードの決定性・異なるシードでの非決定性・単位区間の範囲・次元。

#[test]
fn deterministic_rng_reproduces_same_sequence_for_same_seed() {
    let mut a = DeterministicRng::new(123);
    let mut b = DeterministicRng::new(123);
    let seq_a: Vec<u64> = (0..10).map(|_| a.next_u64()).collect();
    let seq_b: Vec<u64> = (0..10).map(|_| b.next_u64()).collect();
    assert_eq!(seq_a, seq_b);
}

#[test]
fn deterministic_rng_differs_across_seeds() {
    let mut a = DeterministicRng::new(1);
    let mut b = DeterministicRng::new(2);
    let seq_a: Vec<u64> = (0..10).map(|_| a.next_u64()).collect();
    let seq_b: Vec<u64> = (0..10).map(|_| b.next_u64()).collect();
    assert_ne!(seq_a, seq_b);
}

#[test]
fn deterministic_rng_next_vector_has_requested_dimension_and_range() {
    let mut rng = DeterministicRng::new(99);
    let v = rng.next_vector(768);
    assert_eq!(v.len(), 768);
    assert!(v.iter().all(|&x| (-1.0..1.0).contains(&x)));
}

// ab: 両経路の Measurement 返却・interleaved 呼び出し順序。

#[test]
fn run_ab_returns_measurements_for_both_paths_in_alternating_order() {
    let config = MeasurementConfig::new(20, 20, 0).unwrap();
    let order = std::sync::Mutex::new(Vec::<char>::new());

    let result: AbMeasurement = run_ab(
        &config,
        || {
            order.lock().unwrap().push('a');
            std::thread::sleep(Duration::from_micros(1));
        },
        || {
            order.lock().unwrap().push('b');
            std::thread::sleep(Duration::from_micros(1));
        },
    )
    .expect("valid config must run");

    assert_eq!(result.a.samples.len(), 20);
    assert_eq!(result.b.samples.len(), 20);
    assert!(result.median_ratio.is_finite() && result.median_ratio >= 0.0);

    let recorded = order.into_inner().unwrap();
    assert_eq!(recorded.len(), 80); // (warmup 20 + 計測 20) * 2 経路
                                    // 反復ごとに先行経路を入れ替える（偶数反復 A→B・奇数反復 B→A）ため、
                                    // 各ペアは常に両経路を 1 回ずつ含み、かつ先行経路が交互に入れ替わる。
    for (i, pair) in recorded.chunks(2).enumerate() {
        let expected = if i % 2 == 0 { ['a', 'b'] } else { ['b', 'a'] };
        assert_eq!(pair, expected);
    }
}

#[test]
fn median_ratio_rejects_zero_baseline_denominator_instead_of_producing_nan_or_inf() {
    // B 側中央値が Duration::ZERO だと単純な a/b は NaN（両方 0）または +inf（a のみ
    // 非 0）になり、`median_ratio < threshold` 等の回帰ゲートが NaN で暗黙に false
    // 評価される fail-open を生む。実測タイマーに依存せず直接検証する。
    let err = median_ratio(Duration::from_millis(1), Duration::ZERO).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));

    let err = median_ratio(Duration::ZERO, Duration::ZERO).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));
}

#[test]
fn median_ratio_computes_finite_nonnegative_value_for_nonzero_denominator() {
    let ratio = median_ratio(Duration::from_millis(1), Duration::from_millis(2)).unwrap();
    assert!(ratio.is_finite() && ratio >= 0.0);
    assert!((ratio - 0.5).abs() < f64::EPSILON);
}

// 空サンプル・不正構成でパニックしないこと（Err 経路）。

#[test]
fn invalid_config_returns_err_without_panicking() {
    let result = MeasurementConfig::new(1, 1, 0);
    assert!(result.is_err());
}
