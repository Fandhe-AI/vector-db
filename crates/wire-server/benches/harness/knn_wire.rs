//! `vector_knn` 786µs の wire／SQL 表層／距離カーネル・Top-k 内訳を切り分ける
//! （Issue #463）ための時間非依存ロジック。
//!
//! `knn_wire_profile_bench.rs`（実測本体・時間依存）と
//! `tests/knn_wire_profile_accept.rs`（`make ci` 対象の回帰テスト）の双方から
//! 取り込む。実測タイマー（`std::time::Instant`）には依存しない純関数のみを置き、
//! rounds の fail-closed パース・段間差分の帰属計算・ノイズ帯判定・出力整形を
//! 担う（`.claude/rules/coding-rust.md`「エラーハンドリング」: panic させず
//! `Result` で異常系を表現する）。
//!
//! 段の定義（`docs/design/knn-wire-stage-profile.md` 参照）:
//! T1′(kernel_distance_only) ≤ T1s(provider_scalar) ≤ T1p(provider_parallel。
//! production 既定） ≤ T2(sql_surface_hot) ≤ T3(wire_roundtrip)。各段は独立した
//! `harness::protocol::run` 呼び出しの中央値であり、測定ノイズにより逆転しうる
//! （`knn_profile_bench.rs::stage_diff_ns_per_row` と同じ設計判断）。本モジュールは
//! 逆転を panic にせず `None`／informational な表示へ倒す（fail-closed は
//! 「合否判定を歪めない」ことに向け、実測できなかった事実自体は隠さない）。

use std::time::Duration;

/// ラウンド数（`BENCH_KNN_WIRE_ROUNDS`）の下限。`docs/design/
/// benchmark-judgement-policy.md` §3 の「交互実行 min-of-N（N≥5）」に対応する。
pub const MIN_ROUNDS: u32 = 5;
/// ラウンド数の上限（coding-rust.md「長さフィールドは上限検証してから
/// アロケーションに使う」対応。1 ラウンドあたり 6 段 × `MeasurementConfig` の
/// サンプル列を保持するため、開発者操作起点の巨大値でも無制限確保させない）。
pub const MAX_ROUNDS: u32 = 50;
/// 既定ラウンド数。
pub const DEFAULT_ROUNDS: u32 = 5;

/// 本モジュールのエラー型。`harness::stats::BenchError` とは責務が異なる
/// （こちらは Issue #463 固有の rounds パース・帰属計算の失敗であり、汎用の
/// 計測プロトコル基盤のエラーではない）ため独立させる
/// （`harness::sql_c1::SqlC1Error` と同一の分離方針）。
#[derive(Debug, Clone, PartialEq)]
pub enum KnnWireError {
    /// `GITHUB_ACTIONS` 実行環境下での起動を拒否した（Issue #463・
    /// `.claude/rules/security.md`「セキュリティ設定ミス」観点。CI へは配線
    /// しない方針そのものを二重に守る defense-in-depth）。
    RefusedUnderGitHubActions,
    /// `BENCH_KNN_WIRE_ROUNDS` の値が数値として解釈できない、または
    /// [`MIN_ROUNDS`]〜[`MAX_ROUNDS`] の範囲外だった。
    InvalidRounds(String),
    /// 統計計算対象のサンプル列・所要時間列が空だった。
    EmptySamples,
    /// 比率計算の分母が 0 で NaN／inf 化する状態だった
    /// （`harness::stats::BenchError::DegenerateRatio` と同一の fail-closed 方針）。
    DegenerateRatio(&'static str),
}

impl std::fmt::Display for KnnWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KnnWireError::RefusedUnderGitHubActions => write!(
                f,
                "knn_wire_profile_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set)"
            ),
            KnnWireError::InvalidRounds(reason) => {
                write!(f, "invalid BENCH_KNN_WIRE_ROUNDS: {reason}")
            }
            KnnWireError::EmptySamples => write!(f, "empty sample set"),
            KnnWireError::DegenerateRatio(reason) => write!(f, "degenerate ratio: {reason}"),
        }
    }
}

impl std::error::Error for KnnWireError {}

/// `GITHUB_ACTIONS` 環境変数が設定されている場合は起動直後に拒否する
/// （`harness::sql_c1::resolve_verbose` の `under_github_actions` 引数と同一方針。
/// 手動専用ベンチが CI ログへ実測値を出す事故を防ぐ）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), KnnWireError> {
    if under_github_actions {
        Err(KnnWireError::RefusedUnderGitHubActions)
    } else {
        Ok(())
    }
}

/// `BENCH_KNN_WIRE_ROUNDS` を fail-closed にパースする。未設定は [`DEFAULT_ROUNDS`]。
/// 数値変換不能・[`MIN_ROUNDS`] 未満・[`MAX_ROUNDS`] 超過はいずれも `Err`
/// （`harness::sql_c1::MAX_VECTOR_LITERAL_BYTES` 系と同じ「開発者操作起点の
/// 巨大値・不正値を無検証で使わない」方針）。
pub fn parse_rounds(raw: Option<&str>) -> Result<u32, KnnWireError> {
    let raw = match raw {
        None => return Ok(DEFAULT_ROUNDS),
        Some(raw) => raw,
    };
    let value: u32 = raw
        .trim()
        .parse()
        .map_err(|_| KnnWireError::InvalidRounds(format!("{raw:?} is not a valid u32")))?;
    if value < MIN_ROUNDS {
        return Err(KnnWireError::InvalidRounds(format!(
            "{value} is below the protocol minimum {MIN_ROUNDS}"
        )));
    }
    if value > MAX_ROUNDS {
        return Err(KnnWireError::InvalidRounds(format!(
            "{value} exceeds the protocol maximum {MAX_ROUNDS}"
        )));
    }
    Ok(value)
}

/// 所要時間列の最小値（min-of-N。`docs/design/benchmark-judgement-policy.md` §3）。
pub fn min_of(samples: &[Duration]) -> Result<Duration, KnnWireError> {
    samples
        .iter()
        .copied()
        .min()
        .ok_or(KnnWireError::EmptySamples)
}

/// 所要時間列の中央値（median-of-N。偶数個は中央 2 件の算術平均）。
pub fn median_of(samples: &[Duration]) -> Result<Duration, KnnWireError> {
    if samples.is_empty() {
        return Err(KnnWireError::EmptySamples);
    }
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        // `mid` は 1 以上（空入力は上で拒否済み）。
        Ok((sorted[mid - 1] + sorted[mid]) / 2)
    } else {
        Ok(sorted[mid])
    }
}

/// 参照区間帯（`docs/design/benchmark-judgement-policy.md` §4）: 変更を含まない
/// 区間（本ベンチでは距離カーネルのみの T1′）の複数ラウンド中央値列から
/// `(max - min) / min` を算出する。run-to-run のノイズ幅そのものであり、
/// 固定 ±5% 帯とあわせて各段間差分の判定に用いる。
///
/// `min` が 0 だと除算不能（NaN／inf 化）なため `Err`（fail-closed。
/// `harness::stats::BenchError::DegenerateRatio` と同一方針）。
pub fn reference_band(round_medians: &[Duration]) -> Result<f64, KnnWireError> {
    let min = min_of(round_medians)?;
    let max = round_medians
        .iter()
        .copied()
        .max()
        .ok_or(KnnWireError::EmptySamples)?;
    if min.is_zero() {
        return Err(KnnWireError::DegenerateRatio(
            "reference_band: min of round medians is zero",
        ));
    }
    Ok((max.as_secs_f64() - min.as_secs_f64()) / min.as_secs_f64())
}

/// 累積段どうしの差分（下流段 `to` の中央値 − 上流段 `from` の中央値）。
/// `to < from`（測定ノイズによる逆転。`knn_profile_bench.rs` と同じ現象）の場合は
/// `None` を返し、呼び出し元は「ノイズにより逆転・未確定（n/a）」として継続する
/// （呼び出し元をベンチ全体の中断へ倒さない。モジュール冒頭コメント参照）。
pub fn bucket_diff(from: Duration, to: Duration) -> Option<Duration> {
    to.checked_sub(from)
}

/// [`bucket_diff`] の結果を T3（wire e2e）の中央値に対する比率（%）へ変換する。
/// `total` が 0 だと除算不能なため `Err`。
pub fn diff_ratio_pct(diff: Duration, total: Duration) -> Result<f64, KnnWireError> {
    if total.is_zero() {
        return Err(KnnWireError::DegenerateRatio(
            "diff_ratio_pct: total duration is zero",
        ));
    }
    Ok(diff.as_secs_f64() / total.as_secs_f64() * 100.0)
}

/// 段間差分がノイズ帯（固定 ±5% 帯・実測参照区間帯のいずれか広い方）以内かを
/// 判定する（`docs/design/benchmark-judgement-policy.md` §4「固定帯・実測帯の
/// 双方を併記」）。合否判定には使わない informational な分類（本ベンチは
/// production 無変更のため合否ゲートを持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandClass {
    WithinNoiseBand,
    AboveNoiseBand,
}

/// 固定 ±5% 帯（`docs/design/benchmark-judgement-policy.md` §4 が挙げる
/// `dot_kernel.rs::classify_change` と同一のデフォルト帯）。
pub const FIXED_BAND_PCT: f64 = 5.0;

pub fn classify_against_bands(diff_ratio_pct: f64, reference_band_pct: f64) -> BandClass {
    let band = FIXED_BAND_PCT.max(reference_band_pct);
    if diff_ratio_pct.abs() <= band {
        BandClass::WithinNoiseBand
    } else {
        BandClass::AboveNoiseBand
    }
}

impl std::fmt::Display for BandClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BandClass::WithinNoiseBand => write!(f, "within_noise_band"),
            BandClass::AboveNoiseBand => write!(f, "above_noise_band"),
        }
    }
}

/// 4 区分内訳 1 行の描画。`diff` が `None`（測定ノイズによる逆転。[`bucket_diff`]
/// 参照）の場合は数値を出さず `n/a` とする。
pub fn render_bucket_line(
    label: &str,
    diff: Option<Duration>,
    ratio_pct: Option<f64>,
    band: Option<BandClass>,
) -> String {
    match (diff, ratio_pct, band) {
        (Some(diff), Some(ratio_pct), Some(band)) => {
            format!("bucket({label}): diff={diff:?} ratio={ratio_pct:.2}% band={band}")
        }
        _ => format!(
            "bucket({label}): n/a (独立計測どうしの中央値比較のため測定ノイズにより逆転・未確定)"
        ),
    }
}

/// 段（tier）1 個・1 ラウンド分の min/median 行の描画。
pub fn render_tier_round_line(tier: &str, round: u32, median: Duration, p95: Duration) -> String {
    format!("tier({tier}) round={round}: median={median:?} p95={p95:?}")
}

/// 段（tier）1 個・全ラウンド集約（min-of-R・median-of-R）行の描画。
pub fn render_tier_summary_line(tier: &str, min_of_r: Duration, median_of_r: Duration) -> String {
    format!("tier({tier}) summary: min_of_r={min_of_r:?} median_of_r={median_of_r:?}")
}

/// 参照区間帯（T1′ の複数ラウンド中央値から算出）の描画。
pub fn render_reference_band_line(band_pct: f64) -> String {
    format!("reference_band(kernel_distance_only): {band_pct:.2}%")
}

// インラインの `#[cfg(test)] mod tests` を置かない理由（`harness/hnsw_build.rs`・
// `harness/bench_engine.rs` 冒頭コメントと同じ制約）: 本モジュールは `#[path]`
// 経由で bench バイナリ（`knn_wire_profile_bench.rs`）へ取り込まれるが、
// bench 側のコンパイルでは `#[test]` 項目は実行対象として収集されない一方
// `#[cfg(test)]` ブロック自体はコンパイルされてしまうため、
// `#[cfg(test)] mod tests { use super::*; ... }` を置くと bench ビルド時に
// `use super::*` が unused import になる。回帰テストは
// `tests/knn_wire_profile_accept.rs` に集約する（`make ci` 対象）。
