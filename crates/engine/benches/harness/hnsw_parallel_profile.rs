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
