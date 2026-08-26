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

/// 複数クエリの Recall@k 列から worst-query（最小値）を取り出す（CORE-4）。
///
/// 両 provider（`ParallelSearchProvider`・`CpuScalarProvider`）はいずれも厳密最近傍
/// であり、本測定の本質は Top-k 完全一致の回帰チェックである。平均を採用すると
/// 1 クエリだけ不一致でも他クエリの一致に埋もれて `min_recall` を通過しうる
/// （例: 19 件完全一致・1 件不一致でも平均は 1.0 に近い高値を維持する）。
/// worst-query（最小値）を採用することで単一クエリの不一致もそのまま判定へ反映する。
/// `recalls` が空の場合は判定不能として `Err`（fail-closed。`recall_at_k`・
/// `p95_from_samples` と同一の空入力拒否方針）。
pub fn worst_recall(recalls: &[f64]) -> Result<f64, BenchError> {
    recalls
        .iter()
        .copied()
        .fold(None, |acc, value| match acc {
            None => Some(value),
            Some(min) => Some(f64::min(min, value)),
        })
        .ok_or(BenchError::EmptySamples)
}

/// Recall@k が下限（`min_recall`）以上かを判定する（CORE-4）。
/// `min_recall` は `(0.0, 1.0]` の範囲外だと判定基準として意味を持たないため
/// `Err`（fail-closed）とする。`0.0` を許容すると「どんな recall 値でも pass」
/// となり CORE-4 のゲートが実質的に無効化されるため下限からも除外する
/// （`simd_bench.rs` の `min_recall_from_env` と同一の不変条件）。
pub fn check_recall_within_limit(recall: f64, min_recall: f64) -> Result<bool, BenchError> {
    if !(min_recall > 0.0 && min_recall <= 1.0) {
        return Err(BenchError::ProtocolViolation(
            "min_recall must be within (0.0, 1.0]",
        ));
    }
    Ok(recall >= min_recall)
}

/// 対照エンジンとの比率（被検/対照）が上限（`max_ratio`）以下かを判定する
/// （CORE-5。ポインタ: `docs/spec/04-behavior/core-engine.md` CORE-5）。
///
/// `ratio` の算出方法は本関数の関知するところではなく、呼び出し元
/// （`contrast_bench.rs`）が算出済みの比率を渡す契約とする。本関数の責務は値の
/// 妥当性検証（有限・非負）と上限との突き合わせのみで、比率の定義を含む CORE-5 の
/// 判定内容は上記ポインタ先が SSOT のため本コメントには記載しない（TASK-127・
/// Issue #176 で対照エンジン〔usearch〕へ接続済み）。
pub fn check_contrast_ratio_within_limit(ratio: f64, max_ratio: f64) -> Result<bool, BenchError> {
    if !ratio.is_finite() || ratio < 0.0 {
        return Err(BenchError::DegenerateRatio(
            "ratio must be a finite, non-negative value",
        ));
    }
    if !max_ratio.is_finite() || max_ratio <= 0.0 {
        return Err(BenchError::ProtocolViolation(
            "max_ratio must be a finite, positive value",
        ));
    }
    Ok(ratio <= max_ratio)
}

/// 2 つの所要時間サンプル列の p95 の比（a/b）を算出する汎用ヘルパ。
///
/// `contrast_bench.rs` が `a`＝被検（`ParallelSearchProvider`）・`b`＝対照
/// （usearch `exact_search`）の順で渡す契約とする。`p95_from_samples` を経由するため
/// 空サンプルは `Err(BenchError::EmptySamples)`。対照側の p95 が `Duration::ZERO`
/// （極めて軽量なワークロード・粗い clock 分解能等）だと a/b が NaN/+inf 化し
/// 暗黙の fail-open を招くため、`ab::median_ratio` と同一方針で `Err` とする。
pub fn p95_ratio(a_samples: &[Duration], b_samples: &[Duration]) -> Result<f64, BenchError> {
    let p95_a = p95_from_samples(a_samples)?;
    let p95_b = p95_from_samples(b_samples)?;
    if p95_b.is_zero() {
        return Err(BenchError::DegenerateRatio(
            "cannot compute p95_ratio: baseline (b) p95 is zero",
        ));
    }
    Ok(p95_a.as_secs_f64() / p95_b.as_secs_f64())
}

/// `BENCH_MAX_CONTRAST_RATIO` 環境変数の生文字列を CORE-5 の上限比として解析する
/// 純関数（`contrast_bench.rs` の env 読み取りから分離し、`tests/bench_accept.rs` で
/// 実測タイマー・env に依存せず検証できるようにする）。
///
/// 未設定の repo variable は GitHub Actions 上では空文字列に解決される
/// （`.github/workflows/bench.yml` 参照）ため、空文字列も明示的に拒否する
/// （`max_p95_from_env`〔`simd_bench.rs`〕と同一の fail-closed 方針）。
pub fn parse_contrast_ratio_limit(raw: &str) -> Result<f64, BenchError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(BenchError::ProtocolViolation(
            "BENCH_MAX_CONTRAST_RATIO must not be empty",
        ));
    }
    let value: f64 = trimmed.parse().map_err(|_| {
        BenchError::ProtocolViolation("BENCH_MAX_CONTRAST_RATIO must be a floating-point number")
    })?;
    if !value.is_finite() || value <= 0.0 {
        return Err(BenchError::ProtocolViolation(
            "BENCH_MAX_CONTRAST_RATIO must be a finite, positive value",
        ));
    }
    Ok(value)
}

/// `baseline`（対照経路）に対する `candidate`（被検経路）の p95 劣化率が上限
/// （`max_degradation_pct`）以内かを判定する（TASK-130・CORE-7 ポインタ:
/// 動的窓集約を経由することによる単発クエリ経路の劣化上限）。
///
/// 劣化率は `(candidate - baseline) / baseline * 100`（%）。`candidate` が
/// `baseline` より速い（劣化なし）場合は負値になり、`max_degradation_pct` が
/// 正である限り自動的に判定を通過する。`baseline` が `Duration::ZERO` だと
/// 除算不能（NaN/inf 化し暗黙の fail-open を招く）なため `Err`
/// （`ab::median_ratio` と同一の fail-closed 方針）。
pub fn check_degradation_within_limit(
    baseline_p95: Duration,
    candidate_p95: Duration,
    max_degradation_pct: f64,
) -> Result<bool, BenchError> {
    if baseline_p95.is_zero() {
        return Err(BenchError::DegenerateRatio(
            "cannot compute degradation: baseline p95 is zero",
        ));
    }
    if !max_degradation_pct.is_finite() || max_degradation_pct < 0.0 {
        return Err(BenchError::ProtocolViolation(
            "max_degradation_pct must be a finite, non-negative value",
        ));
    }
    let degradation_pct = (candidate_p95.as_secs_f64() - baseline_p95.as_secs_f64())
        / baseline_p95.as_secs_f64()
        * 100.0;
    Ok(degradation_pct <= max_degradation_pct)
}

/// `baseline`（対照経路）に対する `candidate`（被検経路）の p95 短縮率が下限
/// （`min_improvement_pct`）以上かを判定する（TASK-130・CORE-6/CORE-16 ポインタ:
/// GPU 経路・f16 常駐経路の性能受け入れ）。
///
/// 本 PR の時点では呼び出し元（`batch_bench.rs`）から未接続（実 GPU バックエンド
/// 未接続のため。opt-in fail-closed で「判定不能」を扱う。判定ロジックのみ
/// 先行実装し `tests/batch_accept.rs` で単体検証する）。
///
/// 短縮率は `(baseline - candidate) / baseline * 100`（%）。`baseline` が
/// `Duration::ZERO` だと除算不能なため `Err`（`check_degradation_within_limit` と
/// 同一の fail-closed 方針）。
pub fn check_improvement_at_least(
    baseline_p95: Duration,
    candidate_p95: Duration,
    min_improvement_pct: f64,
) -> Result<bool, BenchError> {
    if baseline_p95.is_zero() {
        return Err(BenchError::DegenerateRatio(
            "cannot compute improvement: baseline p95 is zero",
        ));
    }
    if !min_improvement_pct.is_finite() || min_improvement_pct <= 0.0 {
        return Err(BenchError::ProtocolViolation(
            "min_improvement_pct must be a finite, positive value",
        ));
    }
    let improvement_pct = (baseline_p95.as_secs_f64() - candidate_p95.as_secs_f64())
        / baseline_p95.as_secs_f64()
        * 100.0;
    Ok(improvement_pct >= min_improvement_pct)
}
