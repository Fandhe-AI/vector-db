//! HNSW グラフ構築（TASK-132・`engine::hnsw::HnswIndex::build`）の構築計算量
//! スケーリング確認ベンチ（受け入れ条件 (b): 「構築計算量が規模に対してほぼ
//! N log N であることの簡易ベンチ確認」）。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make bench-hnsw-build`
//! からの手動実行専用）。`harness::hnsw_build::refuse_under_github_actions` で
//! defense-in-depth の拒否を行う（`dot_kernel_bench.rs` 等と同一方針）。
//!
//! # 測定対象・出力
//!
//! `rows = [2_000, 8_000, 32_000]`（dim=64・既定パラメータ `HnswParams::default()`）
//! ごとに `HnswIndex::build` の所要時間中央値を計測し、隣接する規模点対の
//! log-log 傾き（実効指数）を出す。判定は情報提供専用（合否閾値を持たない。
//! spec 由来の閾値ではない）だが、実効指数が [`harness::hnsw_build::
//! DEFAULT_SUPER_LINEAR_THRESHOLD`] 以上（二乗相当に近い）の場合は明示的に
//! `SuperLinear` と表示する（計画「実効指数が 2.0 に近い場合は明示的に
//! `SuperLinear` と表示する」の実装）。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::hnsw_build::{
    classify_scaling, generate_corpus, n_log_n_ratio, refuse_under_github_actions, render_line,
    scaling_exponent, ScalingClass, DEFAULT_SUPER_LINEAR_THRESHOLD,
};
use harness::protocol::{run, MeasurementConfig};

use engine::hnsw::{HnswIndex, HnswParams};
use engine::isa;

const DIM: usize = 64;
const ROWS: [usize; 3] = [2_000, 8_000, 32_000];

/// `GITHUB_ACTIONS` が設定されているか（値は見ず存在有無のみ判定。
/// `dot_kernel_bench.rs::running_under_github_actions` と同一パターン）。
fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// 1 規模点の計測。決定的コーパスを構築し、`HnswIndex::build` を繰り返し呼んで
/// 中央値を得る。大規模点ほど 1 回の構築コストが大きいため、規模に応じて
/// warmup・計測回数を `protocol::MeasurementConfig` の下限へ寄せる
/// （`MIN_WARMUP_ITERATIONS`／`MIN_MEASURED_ITERATIONS` はいずれも 20。
/// 32,000 行 × 20 回超の構築は時間依存ベンチとして許容する）。
fn measure_stage(rows: usize) -> Result<(usize, std::time::Duration), String> {
    let corpus =
        generate_corpus(0xB0BA_1234 ^ rows as u64, DIM, rows).map_err(|e| e.to_string())?;
    let params = HnswParams::default();

    let config = MeasurementConfig::new(20, 20, 0xB0BA_1234 ^ rows as u64)
        .map_err(|e| format!("rows={rows}: {e}"))?;
    let measurement = run(&config, || {
        HnswIndex::build(params, DIM as u32, &corpus, 1)
            .expect("build should succeed on well-formed corpus")
    })
    .map_err(|e| format!("rows={rows}: {e}"))?;

    let ratio = n_log_n_ratio(rows, measurement.summary.median).map_err(|e| e.to_string())?;
    println!(
        "{}",
        render_line(rows, DIM, measurement.summary.median, ratio)
    );
    Ok((rows, measurement.summary.median))
}

fn main() {
    if let Err(e) = refuse_under_github_actions(running_under_github_actions()) {
        eprintln!("hnsw_build_bench: {e}");
        std::process::exit(1);
    }

    let detected = isa::current().isa();
    let env = EnvReport::capture(format!("{detected:?}"));
    println!("{env}");

    let mut points: Vec<(usize, std::time::Duration)> = Vec::new();
    let mut had_error = false;
    for &rows in &ROWS {
        match measure_stage(rows) {
            Ok(point) => points.push(point),
            Err(e) => {
                eprintln!("hnsw_build_bench: {e}");
                had_error = true;
            }
        }
    }
    if had_error || points.len() < 2 {
        std::process::exit(1);
    }

    // 隣接する規模点対の実効指数を出す（`docs/design/dot-kernel-multi-accumulator.md`
    // 等の既存ベンチと同じく、複数区間の傾きを並べて全体の伸び方の傾向を見る）。
    let mut had_scaling_error = false;
    for pair in points.windows(2) {
        let (n1, t1) = pair[0];
        let (n2, t2) = pair[1];
        match scaling_exponent(n1, t1, n2, t2) {
            Ok(exponent) => {
                let class = classify_scaling(exponent, DEFAULT_SUPER_LINEAR_THRESHOLD);
                let warn = matches!(class, ScalingClass::SuperLinear);
                println!(
                    "hnsw_build: scaling n1={n1} n2={n2} exponent={exponent:.4} class={class:?}{}",
                    if warn {
                        " (WARNING: super-linear growth)"
                    } else {
                        ""
                    }
                );
            }
            Err(e) => {
                eprintln!("hnsw_build_bench: scaling exponent n1={n1} n2={n2}: {e}");
                had_scaling_error = true;
            }
        }
    }
    if had_scaling_error {
        std::process::exit(1);
    }
}
