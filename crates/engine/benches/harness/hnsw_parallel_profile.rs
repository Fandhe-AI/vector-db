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

use engine::hnsw::{HnswBuildProfile, HnswRepairStats, HnswWorkerStats};

use super::stats;

/// 昇順ソート済み `f64` 列の中央値を線形補間で返す（`harness::stats::summarize`
/// の `percentile(sorted, 0.5)` と同一の補間規則。codex-review P2 指摘・PR #445:
/// 従来は偶数個のとき中央 2 件の上側 `sorted[len/2]` を採用しており、`total` の
/// 中央値（`stats::summarize` 経由・補間あり）と定義が食い違っていたため、
/// `serial_share`・`parallel_vs_control` が歪む要因になっていた）。
/// `sorted` が空の場合の呼び出しは想定しない（呼び出し元で空チェック済み）。
fn interpolated_median_f64(sorted: &[f64]) -> f64 {
    let last_index = sorted.len().saturating_sub(1);
    let rank = 0.5 * last_index as f64;
    let lower_index = rank.floor() as usize;
    let upper_index = rank.ceil() as usize;
    let lower = sorted.get(lower_index).copied().unwrap_or_default();
    let upper = sorted.get(upper_index).copied().unwrap_or_default();
    if lower_index == upper_index {
        return lower;
    }
    let frac = rank - lower_index as f64;
    lower + (upper - lower) * frac
}

/// `Duration` のスライスから中央値を返す（線形補間。`stats::summarize` の
/// `median` と同一の値になる——`stats::summarize` をそのまま呼ぶ）。
pub fn median_duration(values: &[Duration]) -> Option<Duration> {
    if values.is_empty() {
        return None;
    }
    stats::summarize(values).ok().map(|s| s.median)
}

/// `u64` のスライスから `(min, median, max)` を返す。空なら `None`。
/// 中央値は [`interpolated_median_f64`] と同一の補間規則（`stats::summarize`
/// の `percentile` と同一の式を `f64` 空間で適用し、最も近い整数へ丸める）。
pub fn min_median_max_u64(values: &[u64]) -> Option<(u64, u64, u64)> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let min = *sorted.first()?;
    let max = *sorted.last()?;
    let as_f64: Vec<f64> = sorted.iter().map(|&v| v as f64).collect();
    let median = interpolated_median_f64(&as_f64).round() as u64;
    Some((min, median, max))
}

/// `Duration` のスライスから `(min, median, max)` を返す。中央値は
/// `stats::summarize` と同一の線形補間規則。
pub fn min_median_max_duration(values: &[Duration]) -> Option<(Duration, Duration, Duration)> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort();
    let min = *sorted.first()?;
    let max = *sorted.last()?;
    let median = stats::summarize(&sorted).ok()?.median;
    Some((min, median, max))
}

/// 逐次段（`level_assign + sequential_prefix + freeze + repair_reachability +
/// flatten`。いずれもスレッド数に依らず単一スレッドで実行される）が `total`
/// に占める割合（Amdahl の法則でいう逐次割合）。`total` が 0 の場合は `None`。
///
/// `flatten`（CSR 平坦化。Issue #494・`engine::hnsw::HnswBuildProfile::flatten`）
/// は `repair_reachability` 完了後の最終段として追加された逐次段であり
/// （`hnsw.rs::HnswBuildProfile.flatten` ドキュメンテーションコメント参照）、
/// 他の 4 段と同じくスレッド数に依らず単一スレッドで実行されるため逐次割合
/// へ合算する（Issue #495）。逐次縮退経路（`build` と同一グラフを返す経路）
/// では `flatten` が `Duration::ZERO` のまま呼ばれる契約のため、呼び出し元が
/// 更新前の値をそのまま渡しても既存の期待値は変わらない。
pub fn serial_share(
    level_assign: Duration,
    sequential_prefix: Duration,
    freeze: Duration,
    repair_reachability: Duration,
    flatten: Duration,
    total: Duration,
) -> Option<f64> {
    if total.is_zero() {
        return None;
    }
    let serial = level_assign + sequential_prefix + freeze + repair_reachability + flatten;
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

// --------------------------------------------------
// `repair_reachability` 統計（Issue #447: 修復対象ノード数・反復回数の
// 観測フックとベンチへの追加）の時間非依存な集計・整形ロジック。
// --------------------------------------------------

/// [`HnswRepairStats::levels`] の `phase1_wall + phase2_wall` の総和
/// （層をまたいだ Σ）。[`repair_wall_gap`] が呼び出し元の外側計測
/// （`HnswBuildProfile::repair_reachability`）との入れ子区間を検証する材料。
pub fn repair_phase_wall_sum(stats: &HnswRepairStats) -> Duration {
    stats
        .levels
        .iter()
        .map(|l| l.phase1_wall + l.phase2_wall)
        .sum()
}

/// 呼び出し元の外側計測 `outer`（`HnswBuildProfile::repair_reachability`）と
/// 観測版本体の壁時間 `stats.wall` の差（`outer - stats.wall`）。
/// `outer < stats.wall` は入れ子区間の整合違反（タイマーの単調性が壊れて
/// いる・実装のバグ）であり `None` を返す（呼び出し側が fail-closed で
/// 「整合しない」と報告できるようにする）。
pub fn repair_wall_gap(stats: &HnswRepairStats, outer: Duration) -> Option<Duration> {
    outer.checked_sub(stats.wall)
}

/// 層横断の到達不能ノード数合計（`saturating_add`。層数が `u32::MAX` 級に
/// なることはない——`MAX_LEVEL`＝32——が、他の合計系関数と同じ防御的な
/// 演算にそろえる）。
pub fn repair_total_unreachable(stats: &HnswRepairStats) -> u64 {
    stats
        .levels
        .iter()
        .fold(0u64, |acc, l| acc.saturating_add(l.unreachable_before))
}

/// 層横断のフェーズ 1 反復回数合計。
pub fn repair_total_phase1_iterations(stats: &HnswRepairStats) -> u64 {
    stats
        .levels
        .iter()
        .fold(0u64, |acc, l| acc.saturating_add(l.phase1_iterations))
}

/// 層横断のフェーズ 2 結線ノード数合計。
pub fn repair_total_phase2_nodes(stats: &HnswRepairStats) -> u64 {
    stats
        .levels
        .iter()
        .fold(0u64, |acc, l| acc.saturating_add(l.phase2_nodes))
}

/// フェーズ 1 が [`engine::hnsw::PRECISE_REPAIR_CAP`] まで到達した層数
/// （`phase1_cap_hit == true` の層数）。
pub fn repair_phase1_cap_hits(stats: &HnswRepairStats) -> u64 {
    stats.levels.iter().filter(|l| l.phase1_cap_hit).count() as u64
}

/// 複数の計測標本（[`HnswBuildProfile`]）横断で、層 index ごとの
/// 到達不能ノード数（`unreachable_before`）の (min, median, max) を返す
/// （[`min_median_max_u64`] を層ごとに適用する）。標本間で `levels.len()` が
/// 異なる場合は最大長に揃え、ある標本にその層 index が存在しない場合は
/// その標本を当該層の集計対象から除外する（`levels.len()` は seed と
/// ノード数で決まる `max_level+1` のため通常は標本間で一致するが、
/// 万一の食い違いを「標本なし」として扱い panic・パニックしない
/// fail-closed な扱いにする）。戻り値は層 index 昇順。
pub fn repair_unreachable_per_level_min_med_max(
    profiles: &[HnswBuildProfile],
) -> Vec<(usize, (u64, u64, u64))> {
    let max_levels = profiles
        .iter()
        .map(|p| p.repair.levels.len())
        .max()
        .unwrap_or(0);
    let mut out = Vec::with_capacity(max_levels);
    for level in 0..max_levels {
        let values: Vec<u64> = profiles
            .iter()
            .filter_map(|p| p.repair.levels.get(level))
            .map(|l| l.unreachable_before)
            .collect();
        if let Some(mmm) = min_median_max_u64(&values) {
            out.push((level, mmm));
        }
    }
    out
}

/// [`repair_unreachable_per_level_min_med_max`] の結果をベンチ出力の 1 行に
/// 埋め込む短い形式（`[L0:min/med/max,L1:min/med/max,...]`）へ整形する。
/// 空スライスは `"[]"`。
pub fn format_per_level(per_level: &[(usize, (u64, u64, u64))]) -> String {
    if per_level.is_empty() {
        return "[]".to_string();
    }
    let body = per_level
        .iter()
        .map(|(level, (min, med, max))| format!("L{level}:{min}/{med}/{max}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

/// 各 threads 点で 1 回だけ構築した [`engine::hnsw::HnswIndex`] を保持したまま
/// 計測した常駐メモリ（RSS）増分行を描画する（Issue #495）。
/// `harness::hybrid_profile::render_memory_line` と同型の書式・同じ
/// `Option<u64>` 契約（`/proc` を読めない環境では `"unavailable"`。診断目的の
/// ためベンチ自体は止めない）。`approx_heap_bytes` は
/// `engine::hnsw::HnswIndex::approx_heap_bytes` の実測値（Issue #494 の CSR 化
/// メモリ見積り〔`docs/design/hnsw-index.md` §14.6〕を突き合わせる材料）。
pub fn render_memory_line(
    threads: usize,
    approx_heap_bytes: usize,
    vm_rss_kb_before: Option<u64>,
    vm_rss_kb_after: Option<u64>,
    vm_hwm_kb: Option<u64>,
) -> String {
    let fmt_opt = |v: Option<u64>| v.map_or_else(|| "unavailable".to_string(), |v| v.to_string());
    let rss_delta = match (vm_rss_kb_before, vm_rss_kb_after) {
        (Some(before), Some(after)) => after.saturating_sub(before).to_string(),
        _ => "unavailable".to_string(),
    };
    format!(
        "hnsw_parallel_build: memory threads={threads} approx_heap_bytes={approx_heap_bytes} \
         vm_rss_kb_before={} vm_rss_kb_after={} vm_rss_delta_kb={rss_delta} vm_hwm_kb={}",
        fmt_opt(vm_rss_kb_before),
        fmt_opt(vm_rss_kb_after),
        fmt_opt(vm_hwm_kb),
    )
}
