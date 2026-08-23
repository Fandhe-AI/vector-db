//! 所要時間サンプル列からの決定的な統計値算出。
//!
//! `protocol::run`（単独計測）・`ab::run_ab`（A/B 計測）の双方が計測フェーズ終了後に
//! 呼び出す共通ユーティリティ（TASK-158。計測プロトコルが定める代表値の算出方式に
//! 対応。ポインタ: `docs/spec/04-behavior/README.md` 前提条件節）。

use std::time::Duration;

/// 統計計算・プロトコル検証の失敗を表す fail-closed なエラー型。
///
/// wire 入力は経由しないが、engine ライブラリ全体の方針（coding-rust.md）に合わせ
/// panic ではなく `Result` で異常系を表現する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchError {
    /// 統計計算対象のサンプル列が空だった。
    EmptySamples,
    /// 計測プロトコルの下限（warmup・計測回数など）を満たさない構成が渡された。
    ProtocolViolation(&'static str),
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchError::EmptySamples => write!(f, "empty sample set"),
            BenchError::ProtocolViolation(reason) => {
                write!(f, "protocol violation: {reason}")
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

/// 所要時間サンプル列から中央値・Q1/Q3 を算出する。
///
/// 方式: サンプルを昇順ソートした複製に対し、パーセンタイル位置を線形補間する
/// （最近傍点法ではなく補間法を採用し、少数サンプルでも決定的な値を得る）。
/// 空サンプルは `Err(BenchError::EmptySamples)` とし、呼び出し側に計測未実施を
/// 判別可能にする（fail-closed）。
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
/// 添字アクセスは `get()` 経由で行い、境界計算のオーバーフローを避けるため
/// `checked_*` 演算を使う（coding-rust.md: untrusted 入力の扱いに準じた防御的実装。
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
