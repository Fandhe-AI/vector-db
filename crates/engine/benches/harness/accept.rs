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
/// 呼び出し元（`contrast_bench.rs`）は [`p95_ratio`] の結果を渡す（`ab::
/// AbMeasurement::median_ratio` は補助情報として算出されるのみで本判定には使わず、
/// 実測値であるため標準出力へも出さない。AGENTS.md P0: 実測値の公開禁止）。
/// 本関数の責務は値の妥当性検証（有限・非負）と上限との突き合わせのみ
/// とし、比率の算出は呼び出し元へ委ねる（TASK-127・Issue #176 で対照エンジン
/// 〔usearch〕へ接続済み。判定ヘルパを public 実装として置くことはオーナー承認済み
/// ——2026-08-26。閾値の具体値は本リポジトリに持たず env 経由で注入する）。
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

/// 2 つの所要時間サンプル列から p95 レイテンシの比率（a/b）を算出する（CORE-5）。
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

/// `baseline`（対照経路）に対する `candidate`（被検経路）の p95 劣化率（%）を算出する
/// （TASK-130・CORE-7 ポインタ）。`check_degradation_within_limit`・
/// `median_degradation_pct`（Issue #302。複数試行の中央値化）の双方から共有する
/// 算出ロジックとして切り出す。
///
/// 劣化率は `(candidate - baseline) / baseline * 100`（%）。`candidate` が
/// `baseline` より速い（劣化なし）場合は負値になる。`baseline` が
/// `Duration::ZERO` だと除算不能（NaN/inf 化し暗黙の fail-open を招く）なため
/// `Err`（`ab::median_ratio` と同一の fail-closed 方針）。
pub fn degradation_pct(baseline_p95: Duration, candidate_p95: Duration) -> Result<f64, BenchError> {
    if baseline_p95.is_zero() {
        return Err(BenchError::DegenerateRatio(
            "cannot compute degradation: baseline p95 is zero",
        ));
    }
    let pct = (candidate_p95.as_secs_f64() - baseline_p95.as_secs_f64())
        / baseline_p95.as_secs_f64()
        * 100.0;
    if !pct.is_finite() {
        return Err(BenchError::DegenerateRatio(
            "computed degradation_pct is not finite",
        ));
    }
    Ok(pct)
}

/// `baseline`（対照経路）に対する `candidate`（被検経路）の p95 劣化率が上限
/// （`max_degradation_pct`）以内かを判定する（TASK-130・CORE-7 ポインタ:
/// 動的窓集約を経由することによる単発クエリ経路の劣化上限）。
///
/// 劣化率の算出は [`degradation_pct`] に委譲する（Issue #302 でヘルパを共有化）。
/// `max_degradation_pct` が正である限り、劣化なし（負の劣化率）は自動的に判定を
/// 通過する。
pub fn check_degradation_within_limit(
    baseline_p95: Duration,
    candidate_p95: Duration,
    max_degradation_pct: f64,
) -> Result<bool, BenchError> {
    let pct = degradation_pct(baseline_p95, candidate_p95)?;
    if !max_degradation_pct.is_finite() || max_degradation_pct < 0.0 {
        return Err(BenchError::ProtocolViolation(
            "max_degradation_pct must be a finite, non-negative value",
        ));
    }
    Ok(pct <= max_degradation_pct)
}

/// 複数試行の劣化率（%）列から中央値を算出する（TASK-130・CORE-7・Issue #302。
/// hosted runner での突発的な単発スパイクが 1 試行だけを外れ値化しても、
/// ゲート全体の判定を歪めないようにする「複数試行＋中央値採用」方針の要）。
///
/// `samples` が空の場合は判定不能として `Err(BenchError::EmptySamples)`
/// （`worst_recall`・`p95_from_samples` と同一の空入力拒否方針）。非有限値
/// （NaN・±inf）を含む場合も `Err(BenchError::ProtocolViolation)`
/// （[`degradation_pct`] は非有限な結果を既に `Err` で拒否するため、ここに
/// 非有限値が渡ること自体が呼び出し元の契約違反であり fail-closed に倒す）。
pub fn median_degradation_pct(samples: &[f64]) -> Result<f64, BenchError> {
    if samples.is_empty() {
        return Err(BenchError::EmptySamples);
    }
    if samples.iter().any(|v| !v.is_finite()) {
        return Err(BenchError::ProtocolViolation(
            "median_degradation_pct: samples must all be finite",
        ));
    }
    let mut sorted = samples.to_vec();
    // 浮動小数点は `Ord` を持たないため `partial_cmp` を使う。直前に非有限値を
    // 拒否済みのため `unwrap_or` の分岐へは到達しない（防御的に等価扱いへ倒す）。
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    let median = if sorted.len().is_multiple_of(2) {
        // 偶数個: 中央 2 件の平均。`mid` は 1 以上（空入力は上で拒否済み）。
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    };
    Ok(median)
}

/// 劣化率（%）の中央値が上限（`max_pct`）以内かを判定する（TASK-130・CORE-7・
/// Issue #302）。[`check_degradation_within_limit`] の単一試行版に対応する
/// 複数試行版で、`batch_bench.rs::run_core7_gate` の 5 試行フローから使う。
///
/// `max_pct` の妥当性検証は [`check_degradation_within_limit`] と同一
/// （有限・非負のみ許容。fail-closed）。
pub fn check_degradation_pct_within_limit(pct: f64, max_pct: f64) -> Result<bool, BenchError> {
    if !pct.is_finite() {
        return Err(BenchError::DegenerateRatio(
            "check_degradation_pct_within_limit: pct must be finite",
        ));
    }
    if !max_pct.is_finite() || max_pct < 0.0 {
        return Err(BenchError::ProtocolViolation(
            "max_pct must be a finite, non-negative value",
        ));
    }
    Ok(pct <= max_pct)
}

/// `baseline`（対照経路）に対する `candidate`（被検経路）の p95 短縮率が下限
/// （`min_improvement_pct`）以上かを判定する（TASK-130・CORE-6/CORE-16 ポインタ:
/// GPU 経路・f16 常駐経路の性能受け入れ）。
///
/// 呼び出し元は `batch_bench.rs` の CORE-6/CORE-16 opt-in ゲート（Issue #178 で
/// 実 GPU バックエンドへ接続済み）。判定ロジック自体は時間非依存のため
/// `tests/batch_accept.rs` が `make ci` 側から単体検証する。
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
