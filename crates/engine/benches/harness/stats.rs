//! 所要時間サンプル列からの決定的な統計値算出。
//!
//! `protocol::run`（単独計測）・`ab::run_ab`（A/B 計測）の双方が計測フェーズ終了後に
//! 呼び出す共通ユーティリティ（TASK-158。ポインタ: `docs/spec/05-tasks.md` TASK-158）。

use std::time::Duration;

/// 統計計算・プロトコル検証の失敗を表す fail-closed なエラー型。
///
/// wire 入力は経由しないが、engine ライブラリ全体の方針（coding-rust.md）に合わせ
/// panic ではなく `Result` で異常系を表現する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchError {
    /// 統計計算対象のサンプル列が空だった。
    EmptySamples,
    /// 計測プロトコルの下限・上限（warmup・計測回数など）を満たさない構成が渡された。
    ProtocolViolation(&'static str),
    /// 比率計算の分母が 0 で、NaN/+inf など fail-open な値になりうる状態だった
    /// （`ab::run_ab` の `median_ratio` 算出。回帰ゲートが NaN で暗黙に false 評価される
    /// 事態を防ぐため、算出不能を明示的な `Err` として呼び出し側に伝える）。
    DegenerateRatio(&'static str),
    /// 対照エンジン（`contrast.rs`。TASK-127 CORE-5・Issue #176）の FFI 呼び出しが
    /// エラーを返した。呼び出し元（`contrast_bench.rs`）が `unwrap`/`expect` で panic
    /// させず `Result` 伝播できるよう、エラー内容（`cxx::Exception::what()` 等）を
    /// 文字列化して保持する（動的な理由文字列を要するため他の variant の `&'static str`
    /// では表現できない）。
    ExternalEngine(String),
    /// 失敗許容計測（[`super::protocol::run_fallible`]。TASK-116・Issue #316）で、
    /// 除外対象と分類された試行（`TrialFailure::Excluded`）が段ごとの上限
    /// （`max_excluded`）を超えた。有効サンプル数を規定回数まで満たせなかった
    /// ことを示す fail-closed なエラーであり、呼び出し元（`tier_latency_bench.rs`）
    /// はこれを判定未到達として非ゼロ終了する。
    ExcludedTrialsExceeded { excluded: u32, max_excluded: u32 },
    /// 失敗許容計測で、除外対象ではない試行が失敗した（`TrialFailure::Fatal`）。
    /// `PlanError::Timeout`／`PlanError::Unavailable` 等、p95 判定を fail-open に
    /// しないため除外しない致命エラーが該当する（Issue #316 設計方針: 除外対象は
    /// `InvalidResponse` のみ）。理由文字列は呼び出し元エラー型の `Display`
    /// （固定文言のみ・LLM 応答本文を含まない。`query_planner.rs` の P0 方針を
    /// 維持）から構成する。
    FatalTrial(String),
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchError::EmptySamples => write!(f, "empty sample set"),
            BenchError::ProtocolViolation(reason) => {
                write!(f, "protocol violation: {reason}")
            }
            BenchError::DegenerateRatio(reason) => {
                write!(f, "degenerate ratio: {reason}")
            }
            BenchError::ExternalEngine(reason) => {
                write!(f, "external engine error: {reason}")
            }
            BenchError::ExcludedTrialsExceeded {
                excluded,
                max_excluded,
            } => {
                write!(
                    f,
                    "excluded trials exceeded limit: excluded={excluded} max_excluded={max_excluded}"
                )
            }
            BenchError::FatalTrial(reason) => {
                write!(f, "fatal trial error: {reason}")
            }
        }
    }
}

impl std::error::Error for BenchError {}

/// 中央値・Q1/Q3 をまとめた要約統計値。
#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub median: Duration,
    pub q1: Duration,
    pub q3: Duration,
}

/// 所要時間サンプル列から本モジュールの要約統計値を算出する。
///
/// 実装方式: サンプルを昇順ソートした複製に対し、パーセンタイル位置を線形補間する
/// （最近傍点法ではなく補間法を採用し、少数サンプルでも決定的な値を得られるようにした
/// 本モジュール内の実装選択）。空サンプルは `Err(BenchError::EmptySamples)` とし、
/// 呼び出し側に計測未実施を判別可能にする（fail-closed）。
pub fn summarize(samples: &[Duration]) -> Result<Summary, BenchError> {
    if samples.is_empty() {
        return Err(BenchError::EmptySamples);
    }
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort_unstable();

    Ok(Summary {
        median: percentile(&sorted, 0.5)?,
        q1: percentile(&sorted, 0.25)?,
        q3: percentile(&sorted, 0.75)?,
    })
}

/// 昇順ソート済みサンプルに対する線形補間パーセンタイル。
///
/// 添字アクセスは `get()` 経由で行い、境界計算は `saturating_sub` で
/// アンダーフローを避ける（coding-rust.md: untrusted 入力の扱いに準じた防御的実装。
/// 本経路は untrusted 入力ではないが、同一の防御規律を保つ）。
fn percentile(sorted: &[Duration], p: f64) -> Result<Duration, BenchError> {
    if sorted.is_empty() {
        return Err(BenchError::EmptySamples);
    }
    if sorted.len() == 1 {
        return sorted.first().copied().ok_or(BenchError::EmptySamples);
    }

    let last_index = sorted.len().saturating_sub(1);
    let rank = p * last_index as f64;
    let lower_index = rank.floor() as usize;
    let upper_index = rank.ceil() as usize;

    let lower = *sorted.get(lower_index).ok_or(BenchError::EmptySamples)?;
    let upper = *sorted.get(upper_index).ok_or(BenchError::EmptySamples)?;

    if lower_index == upper_index {
        return Ok(lower);
    }

    let frac = rank - lower_index as f64;
    let lower_secs = lower.as_secs_f64();
    let upper_secs = upper.as_secs_f64();
    let interpolated = lower_secs + (upper_secs - lower_secs) * frac;
    Ok(Duration::from_secs_f64(interpolated.max(0.0)))
}
