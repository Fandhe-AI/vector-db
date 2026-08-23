//! 性能計測プロトコル基盤（`harness/`）の実走スモーク（TASK-158）。
//!
//! `cargo bench --bench measurement` で手動実行する（`make ci` の対象外。
//! 時間依存の測定値を CI アサーションへ混ぜない方針は
//! `crates/engine/examples/multi_dim_bench.rs` と同じ）。軽量なダミーワークロード
//! （決定的シードで生成した f32 ベクトルの内積）で単独計測と interleaved A/B の
//! 両経路を実走し、要約統計を英語で標準出力する。
//!
//! `Cargo.toml` 側で `test = false` を指定しているため `cargo test` の対象には
//! 含まれない（プロトコル遵守そのものの回帰検証は `tests/bench_harness.rs` が担う）。

mod harness;

use harness::ab::run_ab;
use harness::protocol::{run, Measurement, MeasurementConfig};
use harness::rng::DeterministicRng;

/// 固定次元の内積ダミーワークロード。
///
/// 実測対象は本基盤自体（ハーネスがオーバーヘッドなく warmup/計測/統計を
/// 実行できること）であり、engine の実検索カーネルには依存しない。
const VECTOR_DIM: usize = 768;

fn dot_product_workload(seed: u64) -> f32 {
    let mut rng = DeterministicRng::new(seed);
    let a = rng.next_vector(VECTOR_DIM);
    let b = rng.next_vector(VECTOR_DIM);
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn print_summary(label: &str, measurement: &Measurement) {
    println!(
        "{label}: median={:?} q1={:?} q3={:?} n={}",
        measurement.summary.median,
        measurement.summary.q1,
        measurement.summary.q3,
        measurement.samples.len()
    );
}

fn main() {
    let config = MeasurementConfig::default();

    let single = run(&config, || dot_product_workload(config.seed()))
        .expect("smoke workload must satisfy protocol minimums");
    print_summary("single", &single);

    let ab = run_ab(
        &config,
        || dot_product_workload(config.seed()),
        || dot_product_workload(config.seed().wrapping_add(1)),
    )
    .expect(
        "smoke A/B workload must satisfy protocol minimums and yield a non-zero baseline median",
    );
    print_summary("ab.a", &ab.a);
    print_summary("ab.b", &ab.b);
    println!("ab.median_ratio={:.4}", ab.median_ratio);
}
