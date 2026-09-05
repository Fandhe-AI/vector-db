//! HNSW 並列構築プロファイル（Issue #406 追記: 8→12 スレッド頭打ち要因の
//! 切り分け計測）の時間非依存な集計・整形ロジック。
//! `hnsw_parallel_build_bench.rs`（実測。時間依存・`make ci` 対象外）と
//! `tests/hnsw_parallel_profile_accept.rs`（`#[path]` で本モジュールを取り込む
//! 回帰。`make ci` 対象）の双方が共有する（`harness/hnsw_build.rs` と同じ
//! 取り込み方針）。
//!
//! `engine::hnsw::{HnswBuildProfile, HnswWorkerStats}` の実測値を受け取って
//! 中央値・比率を計算するだけで、計測（`Instant`）そのものには関与しない。
//! spec 由来の合否閾値は持たない情報提供専用の集計（本ベンチ自体が
//! `hnsw_parallel_build_bench.rs` 冒頭コメントのとおり閾値なし）。
//!
//! # インラインの `#[cfg(test)] mod tests` を置かない理由
//!
//! `harness/hnsw_build.rs` 冒頭コメントと同じ制約: 本モジュールは `#[path]`
//! 経由で複数の bench バイナリから取り込まれるが、bench 側のコンパイル
//! （`--test` フラグなし）では `#[test]` 項目が丸ごと除去され `use super::*;`
//! が unused import になるため、回帰テストは `tests/hnsw_parallel_profile_accept.rs`
//! 側にのみ置く。

use std::time::Duration;

use engine::hnsw::{HnswBuildProfile, HnswWorkerStats};

/// `Duration` のスライスから中央値を返す（偶数個は下位側の値を採用）。
/// 対象は計測回数と同程度の小さな配列のため、都度ソートする単純な実装で足りる
/// （`harness::stats` のような要約統計一式は必要としない）。
pub fn median_duration(values: &[Duration]) -> Option<Duration> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted.get(sorted.len() / 2).copied()
}

/// `u64` のスライスから `(min, median, max)` を返す。空なら `None`。
pub fn min_median_max_u64(values: &[u64]) -> Option<(u64, u64, u64)> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let min = *sorted.first()?;
    let max = *sorted.last()?;
    let median = *sorted.get(sorted.len() / 2)?;
    Some((min, median, max))
}

/// `Duration` のスライスから `(min, median, max)` を返す。
pub fn min_median_max_duration(values: &[Duration]) -> Option<(Duration, Duration, Duration)> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort();
    let min = *sorted.first()?;
    let max = *sorted.last()?;
    let median = *sorted.get(sorted.len() / 2)?;
    Some((min, median, max))
}

/// 逐次段（`level_assign + sequential_prefix + freeze + repair_reachability`。
/// いずれもスレッド数に依らず単一スレッドで実行される）が `total` に占める
/// 割合（Amdahl の法則でいう逐次割合）。`total` が 0 の場合は `None`。
pub fn serial_share(
    level_assign: Duration,
    sequential_prefix: Duration,
    freeze: Duration,
    repair_reachability: Duration,
    total: Duration,
) -> Option<f64> {
    if total.is_zero() {
        return None;
    }
    let serial = level_assign + sequential_prefix + freeze + repair_reachability;
    Some(serial.as_secs_f64() / total.as_secs_f64())
}

/// `baseline` に対する `sample` の高速化率（`baseline / sample`）。
/// `sample` がゼロ時間の場合は測定不能として `None`
/// （`harness::hnsw_build::scaling_exponent` と同じ「時間 0 は測定不能」という
/// 扱い）。
pub fn speedup(baseline: Duration, sample: Duration) -> Option<f64> {
    if sample.is_zero() {
        return None;
    }
    Some(baseline.as_secs_f64() / sample.as_secs_f64())
}

/// ロック取得試行のうちブロックへ落ちた比率（0.0〜1.0）。ワーカー群の
/// `link_lock_blocked`／`link_lock_acquired` を合算してから割る（1 ワーカー
/// ごとの比率を単純平均すると、担当ノード数が少ないワーカーの比率が
/// 過大に効いてしまうため、合算後に割ることで担当ノード数の重みを反映する）。
/// 合算した取得試行数が 0 の場合は `None`。
pub fn aggregate_lock_blocked_ratio(workers: &[HnswWorkerStats]) -> Option<f64> {
    let total_acquired: u64 = workers.iter().map(|w| w.link_lock_acquired).sum();
    if total_acquired == 0 {
        return None;
    }
    let total_blocked: u64 = workers.iter().map(|w| w.link_lock_blocked).sum();
    Some(total_blocked as f64 / total_acquired as f64)
}

/// ワーカー群の `entry_promotions` の合計。
pub fn total_entry_promotions(workers: &[HnswWorkerStats]) -> u64 {
    workers.iter().map(|w| w.entry_promotions).sum()
}

/// ワーカー群の `link_lock_wait`（ブロックする取得に落ちた場合のみ累積される
/// 待ち時間。`HnswWorkerStats::link_lock_wait` 参照）の合計と最大値
/// （`(sum, max)`）。`workers` が空なら双方 `Duration::ZERO`。
pub fn aggregate_lock_wait(workers: &[HnswWorkerStats]) -> (Duration, Duration) {
    let sum: Duration = workers.iter().map(|w| w.link_lock_wait).sum();
    let max: Duration = workers
        .iter()
        .map(|w| w.link_lock_wait)
        .max()
        .unwrap_or(Duration::ZERO);
    (sum, max)
}

/// ロック待ち時間がワーカーのループ全体（`busy`）に占める割合
/// （Σ`link_lock_wait` ÷ Σ`busy`）。ロック競合が頭打ちの主要因かどうかを
/// 判定する根拠値（codex-review P2 指摘・PR #445: 従来の
/// `aggregate_lock_blocked_ratio`〔取得試行に対する回数の比率〕だけでは
/// 「ブロックした回数は多いが待ち時間は無視できる」ケースと「回数は
/// 少ないが長時間ブロックする」ケースを区別できないため、実測待ち時間を
/// 直接 `busy` に対する割合として出す）。Σ`busy` が 0 の場合は `None`。
pub fn lock_wait_share(workers: &[HnswWorkerStats]) -> Option<f64> {
    let total_busy: Duration = workers.iter().map(|w| w.busy).sum();
    if total_busy.is_zero() {
        return None;
    }
    let total_wait: Duration = workers.iter().map(|w| w.link_lock_wait).sum();
    Some(total_wait.as_secs_f64() / total_busy.as_secs_f64())
}

/// `protocol::run` は warmup フェーズ→計測フェーズの順に `workload` を呼ぶ
/// （`harness/protocol.rs::run` の実装・モジュールコメント参照）。呼び出し側が
/// `workload` 内で副作用として蓄積した標本列（例: 本ベンチが
/// `Mutex<Vec<HnswBuildProfile>>` へ push する構築プロファイル）は、
/// 呼び出し順そのままに「先頭 `warmup_iterations` 件が warmup 標本・
/// 末尾 `measured_iterations` 件が計測標本」という並びになる。段別中央値・
/// serial_share・ワーカー統計は計測フェーズの標本のみから算出すべきなので、
/// 本関数で末尾 `measured_iterations` 件へ限定する（codex-review P1 指摘・
/// PR #445。修正前は warmup 標本込みの全件から中央値等を計算しており、
/// warmup 回数ぶん標本が水増しされていた）。
///
/// 蓄積件数が `measured_iterations` 未満の場合（呼び出し漏れ等の想定外の
/// 状態）は空スライスを返す（fail-closed。呼び出し側は「集計不能」として
/// 扱う——`min_median_max_duration` 等は空スライスに対して `None` を返す）。
pub fn measured_tail<T>(all: &[T], measured_iterations: usize) -> &[T] {
    if measured_iterations == 0 || all.len() < measured_iterations {
        return &[];
    }
    &all[all.len() - measured_iterations..]
}

/// `parallel_speedup`（基準 `parallel_base_threads` に対する `parallel_phase`
/// の高速化率）と `control_speedup_rel`（同じ基準スレッド数に対する対照負荷の
/// 高速化率）を同一基準で正規化した比較値を返す（codex-review P1 指摘・
/// PR #445: 従来は `parallel_speedup` が `threads>=2` の最小点基準、
/// `control_speedup` が `threads=1` 基準という異なる基準同士を割っていたため、
/// 対照負荷が理想的な線形スケールでも `parallel_vs_control` が 1.0 から
/// 系統的にずれていた）。
///
/// どちらかが計測不能（`None`）の場合は `None`（呼び出し側は `n/a` として
/// 出力する）。基準点で対照負荷が退化してゼロ除算になる場合
/// （`control_speedup_rel == 0.0`）は `Some(f64::NAN)`。
pub fn parallel_vs_control_ceiling(
    parallel_speedup: Option<f64>,
    control_speedup_rel: Option<f64>,
) -> Option<f64> {
    let parallel_speedup = parallel_speedup?;
    let control_speedup_rel = control_speedup_rel?;
    if control_speedup_rel == 0.0 {
        Some(f64::NAN)
    } else {
        Some(parallel_speedup / control_speedup_rel)
    }
}

/// 複数実行分の [`HnswBuildProfile`] のうち、`total` が中央値に最も近い 1 件を
/// 「代表実行」として選ぶ（ワーカー内訳はスレッド数ぶんの要素を持つため、
/// 複数実行をまたいで平坦化するのではなく 1 実行の内訳を代表させる方が
/// 解釈しやすい）。`profiles` が空なら `None`。同点の場合は入力順で最初に
/// 見つかったものを選ぶ（`Vec::iter().min_by_key` の決定的タイブレーク）。
pub fn pick_representative(profiles: &[HnswBuildProfile]) -> Option<&HnswBuildProfile> {
    let totals: Vec<Duration> = profiles.iter().map(|p| p.total).collect();
    let median = median_duration(&totals)?;
    profiles.iter().min_by_key(|p| p.total.abs_diff(median))
}
