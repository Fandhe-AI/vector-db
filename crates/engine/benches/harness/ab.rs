//! interleaved A/B 計測（2 経路比較の相互実行）。
//!
//! GPU vs CPU-SIMD 等、2 経路を比較する性能検証タスク（TASK-130 等）が使う入口
//! （TASK-158。ポインタ: `docs/spec/05-tasks.md` TASK-158）。
//!
//! 本モジュールの実装選択: `protocol::run` と異なり、warmup も含めて A/B を
//! 1 反復単位で交互実行することでサーマルスロットリング等の時間経過に伴う偏りが
//! 片方の経路だけに乗るのを防ぐ。さらに反復ごとに先行経路を入れ替え、実行順序
//! そのものに起因する系統的バイアス（常に先行する経路がキャッシュ・分岐予測等で
//! 有利/不利になる効果）も相殺する。

use std::hint::black_box;
use std::time::{Duration, Instant};

use super::protocol::{Measurement, MeasurementConfig};
use super::stats::{self, BenchError};

/// A/B 比較の結果。経路別の `Measurement` に加え、中央値の比率（a/b）を保持する。
#[derive(Debug, Clone)]
pub struct AbMeasurement {
    pub a: Measurement,
    pub b: Measurement,
    /// 中央値の比率（`a.summary.median` / `b.summary.median`）。
    /// 1.0 未満なら A が B より高速。分母（B の中央値）が 0 の場合は `run_ab` が
    /// `Err(BenchError::DegenerateRatio)` を返すため、この値は常に有限かつ非負。
    pub median_ratio: f64,
}

/// 2 つのワークロードを 1 反復単位で交互実行し、経路別の統計値を返す。
///
/// `workload_a` と `workload_b` は毎反復ちょうど 1 回ずつ呼ばれる（非対称な反復配分は
/// ドリフト排除という設計意図に反するため、`config` の warmup・計測回数はそのまま
/// 両経路に等しく適用される。個別に回数を変える API は意図的に提供しない）。
/// 反復ごとに先行経路を入れ替える（偶数反復 A→B・奇数反復 B→A）ため、
/// 実行順序自体に起因する系統的バイアスは片方の経路にのみ乗らない。
pub fn run_ab<T>(
    config: &MeasurementConfig,
    mut workload_a: impl FnMut() -> T,
    mut workload_b: impl FnMut() -> T,
) -> Result<AbMeasurement, BenchError> {
    // warmup フェーズも交互実行する（サーマル状態を両経路で揃えた状態から
    // 計測フェーズに入るため）。先行経路は計測フェーズと同じ規則で反復ごとに
    // 入れ替える（下記ループ内コメント参照）。
    for i in 0..config.warmup_iterations() {
        if i % 2 == 0 {
            black_box(workload_a());
            black_box(workload_b());
        } else {
            black_box(workload_b());
            black_box(workload_a());
        }
    }

    let measured = config.measured_iterations() as usize;
    let mut samples_a = Vec::with_capacity(measured);
    let mut samples_b = Vec::with_capacity(measured);

    for i in 0..config.measured_iterations() {
        // 反復ごとに先行経路を入れ替える（偶数反復は A→B、奇数反復は B→A）。
        // 常に同じ経路を先行実行すると、キャッシュ・分岐予測・周波数遷移等の
        // 「直前に何を実行したか」に依存する効果が片方の経路にだけ系統的に
        // 乗り、median_ratio が実際の性能差ではなく実行順序を反映しうる。
        if i % 2 == 0 {
            let start_a = Instant::now();
            black_box(workload_a());
            let elapsed_a = start_a.elapsed();
            samples_a.push(elapsed_a);

            let start_b = Instant::now();
            black_box(workload_b());
            let elapsed_b = start_b.elapsed();
            samples_b.push(elapsed_b);
        } else {
            let start_b = Instant::now();
            black_box(workload_b());
            let elapsed_b = start_b.elapsed();
            samples_b.push(elapsed_b);

            let start_a = Instant::now();
            black_box(workload_a());
            let elapsed_a = start_a.elapsed();
            samples_a.push(elapsed_a);
        }
    }

    // 交互実行の対称性が崩れていないことの内部一貫性チェック（fail-closed）。
    // 通常経路では samples_a/samples_b は常に同数になるはずで、ここに到達するのは
    // 将来の実装変更でループ構造が壊れた場合のみ。
    if samples_a.len() != samples_b.len() {
        return Err(BenchError::ProtocolViolation(
            "interleaved A/B iteration counts diverged",
        ));
    }

    let summary_a = stats::summarize(&samples_a)?;
    let summary_b = stats::summarize(&samples_b)?;

    let median_ratio = median_ratio(summary_a.median, summary_b.median)?;

    Ok(AbMeasurement {
        a: Measurement {
            summary: summary_a,
            samples: samples_a,
        },
        b: Measurement {
            summary: summary_b,
            samples: samples_b,
        },
        median_ratio,
    })
}

/// 中央値比率（a/b）を算出する。B 側が `Duration::ZERO`（極めて軽量なワークロード・
/// 粗い clock 分解能等）だと単純な a/b は NaN（両方 0）または +inf（a のみ非 0）になり、
/// `median_ratio < threshold` 等の回帰ゲートが NaN で暗黙に false 評価される fail-open を
/// 生む（TASK-130 がこの入口を経由する契約のため fail-closed に倒す）。
///
/// `pub(crate)` として切り出し `tests/bench_harness.rs` から実測タイマーに依存せず
/// 直接検証できるようにしている（本ファイルは `#[path]` 経由で bench クレート・
/// テストクレート双方に取り込まれるため、bench コンパイル時（`--test` フラグなし）は
/// `#[test]` 項目が丸ごと除去され、同じ場所に `#[cfg(test)] mod tests` を置くと
/// `use super::*;` が unused import になってしまう）。
pub(crate) fn median_ratio(median_a: Duration, median_b: Duration) -> Result<f64, BenchError> {
    if median_b.is_zero() {
        return Err(BenchError::DegenerateRatio(
            "cannot compute median_ratio: baseline (b) median is zero",
        ));
    }
    Ok(median_a.as_secs_f64() / median_b.as_secs_f64())
}
