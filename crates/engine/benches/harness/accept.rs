//! 性能・Recall 受け入れ基準の判定ヘルパ（TASK-127。ポインタ: `docs/spec/05-tasks.md`
//! TASK-127・対象ビヘイビア CORE-3, CORE-4, CORE-5, SEARCH-4）。
//!
//! `simd_bench.rs` から呼ばれる時間非依存の判定ロジックを本モジュールへ分離する。
//! `tests/bench_accept.rs` が `#[path]` で本モジュールを取り込み、実測タイマーに
//! 依存せず `cargo test`（`make ci` 対象）で回帰検証する（`harness/mod.rs`・
//! `harness/ab.rs` と同一パターン）。
//!
//! 数値基準そのもの（p95 上限・Recall 下限・対照エンジン比の上限）は spec が SSOT
//! であり、本ファイルにはハードコードしない。判定関数は呼び出し元（`simd_bench.rs`）
//! から閾値を引数で受け取るのみとする（`.claude/rules/spec-confidentiality.md`:
//! spec 本文・数値基準を public 資産へ転記しない）。
//!
//! 全関数は空入力・不正な閾値を `Err` で拒否する fail-closed 方針（`stats::BenchError`
//! を再利用し、判定基盤のエラー型を harness 全体で単一にする）。

use std::time::Duration;

use super::stats::BenchError;

/// 厳密最近傍（参照実装）の Top-k と、被検 provider の Top-k を比較して Recall@k を
/// 算出する（CORE-4）。
///
/// Recall@k = |被検結果 ∩ 厳密結果| / |厳密結果|。id の集合演算のみで判定し、
/// スコア値・並び順には依存しない（同点近傍が実装間で順序入れ替わっても
/// Recall を過小評価しない）。`actual_ids` は重複除去してから集合演算する
/// ため、被検側の実装が同一 id を複数回返しても Recall が 1.0 を超えない
/// （fail-closed。重複を許すと判定関数が異常値でも `min_recall` 判定を
/// 素通りしてしまう）。`expected`（厳密結果の id 列）が空の場合は
/// 判定不能として `Err`（fail-closed。0 件の正解に対する「一致率」は定義できない）。
pub fn recall_at_k(expected_ids: &[u64], actual_ids: &[u64]) -> Result<f64, BenchError> {
    if expected_ids.is_empty() {
        return Err(BenchError::EmptySamples);
    }
    let expected: std::collections::HashSet<u64> = expected_ids.iter().copied().collect();
    let actual: std::collections::HashSet<u64> = actual_ids.iter().copied().collect();
    let matched = actual.intersection(&expected).count();
    Ok(matched as f64 / expected.len() as f64)
}

/// 生サンプル列（`protocol::Measurement::samples`）から p95 を抽出する（CORE-3・
/// SEARCH-4）。`stats::Summary` は median/q1/q3 のみを持つため（`harness/stats.rs` の
/// 契約は変更しない）、p95 が必要な呼び出し側は本関数を経由する
/// （`parallel_smoke.rs` の自前算出ロジックと同一の最近傍法。線形補間法の
/// `stats::percentile` とは意図的に方式を分けない——p95 は SLO 判定に使う値であり、
/// 実測範囲外への補間ではなく実際に観測されたサンプル点を返す方が保守的）。
pub fn p95_from_samples(samples: &[Duration]) -> Result<Duration, BenchError> {
    if samples.is_empty() {
        return Err(BenchError::EmptySamples);
    }
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort_unstable();

    let rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len().saturating_sub(1));
    sorted.get(idx).copied().ok_or(BenchError::EmptySamples)
}

/// p95 レイテンシが上限（`max_p95`）以下かを判定する（CORE-3・SEARCH-4）。
/// 上限値そのものは呼び出し元が spec のポインタ（TASK-127）に基づいて渡す。
pub fn check_p95_within_limit(p95: Duration, max_p95: Duration) -> bool {
    p95 <= max_p95
}

/// Recall@k が下限（`min_recall`）以上かを判定する（CORE-4）。
/// `min_recall` は `[0.0, 1.0]` の範囲外だと判定基準として意味を持たないため
/// `Err`（fail-closed）とする。
pub fn check_recall_within_limit(recall: f64, min_recall: f64) -> Result<bool, BenchError> {
    if !(0.0..=1.0).contains(&min_recall) {
        return Err(BenchError::ProtocolViolation(
            "min_recall must be within [0.0, 1.0]",
        ));
    }
    Ok(recall >= min_recall)
}

/// 対照エンジンとの中央値比（`ab::AbMeasurement::median_ratio`。被検/対照）が
/// 上限（`max_ratio`）以下かを判定する（CORE-5）。
///
/// 呼び出し元は本 PR の時点では未接続（対照エンジンクレートの導入がユーザー承認必須
/// のため。`.claude/rules/dependency-policy.md`）。判定ロジックのみ先行実装し、
/// `tests/bench_accept.rs` で単体検証する。
pub fn check_contrast_ratio_within_limit(
    median_ratio: f64,
    max_ratio: f64,
) -> Result<bool, BenchError> {
    if !median_ratio.is_finite() || median_ratio < 0.0 {
        return Err(BenchError::DegenerateRatio(
            "median_ratio must be a finite, non-negative value",
        ));
    }
    if !max_ratio.is_finite() || max_ratio <= 0.0 {
        return Err(BenchError::ProtocolViolation(
            "max_ratio must be a finite, positive value",
        ));
    }
    Ok(median_ratio <= max_ratio)
}
