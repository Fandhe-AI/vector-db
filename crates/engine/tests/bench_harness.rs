//! `benches/harness/`（TASK-158 性能計測プロトコル基盤）の回帰テスト。
//!
//! 対象ビヘイビア ID なし（TASK-158 は「なし（基盤。CORE-3〜6・SEARCH-4・SQL-1 等の
//! 測定条件を担保）」。ポインタ: `docs/spec/05-tasks.md` TASK-158）。ビヘイビア ID を
//! 持たないため、本テストは「ハーネス自体がプロトコル要件（warmup 20 回以上・
//! 計測 20 回以上・中央値＋Q1/Q3・決定的シード RNG・interleaved A/B）を遵守すること」
//! を受け入れ条件として検証する。
//!
//! `#[path]` で `benches/harness/mod.rs` を直接取り込む（内部クレートを新設せず、
//! `cargo bench` 入口〔`benches/measurement.rs`〕と同一ソースを共有する構成。
//! `harness/mod.rs` のモジュールドキュメント参照）。

#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::ab::{run_ab, AbMeasurement};
use harness::protocol::{run, Measurement, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::stats::{self, BenchError, Summary};
use std::time::Duration;

// protocol::MeasurementConfig の下限拒否（fail-closed）。

#[test]
fn config_rejects_warmup_below_protocol_minimum() {
    let err = MeasurementConfig::new(19, 20, 0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn config_rejects_measured_below_protocol_minimum() {
    let err = MeasurementConfig::new(20, 19, 0).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn default_config_matches_protocol_minimum_and_run_accepts_it() {
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
    for pair in recorded.chunks(2) {
        assert_eq!(pair, ['a', 'b']);
    }
}

// 空サンプル・不正構成でパニックしないこと（Err 経路）。

#[test]
fn invalid_config_returns_err_without_panicking() {
    let result = MeasurementConfig::new(1, 1, 0);
    assert!(result.is_err());
}
