//! 性能計測プロトコルのコア実行機構（単独計測）。
//!
//! TASK-127・TASK-130・TASK-83 等の性能系受け入れ検証タスクが、実測値を得る前に
//! 必ず経由する入口（TASK-158。ポインタ: `docs/spec/05-tasks.md` TASK-158）。
//! warmup フェーズと計測フェーズを分離し、計測フェーズの各回所要時間を
//! `stats::summarize` に渡して要約統計値を得る。
//!
//! # 呼び出し側の責務
//!
//! `run` に渡す `workload` クロージャは 1 回分の作業単位を同期的に完了させること
//! （非同期ジョブを投げっぱなしにして返る実装は、計測される所要時間が実際の
//! ワークロード完了時間と乖離するため禁止）。本モジュールは同期呼び出しの
//! 前後で `Instant` を取るのみで、完了同期そのものは検証しない。

use std::hint::black_box;
use std::time::{Duration, Instant};

use super::stats::{self, BenchError, Summary};

/// 計測プロトコルが要求する warmup 回数の下限（TASK-158 参照）。
/// この値を下回る `MeasurementConfig` は `new` が拒否する。
const MIN_WARMUP_ITERATIONS: u32 = 20;

/// 計測プロトコルが要求する計測回数の下限（`MIN_WARMUP_ITERATIONS` 同様の根拠）。
const MIN_MEASURED_ITERATIONS: u32 = 20;

/// warmup・計測回数の上限（coding-rust.md: 「長さフィールドは上限検証してから
/// アロケーションに使う」「無制限 `Vec::with_capacity` 禁止」に対応）。
/// `run`/`run_ab` は `measured_iterations` をそのまま `Vec::with_capacity` に渡すため、
/// 開発者操作起点の巨大値でも上限検証なしには数十 GB 規模のアロケーションが発生しうる。
/// `Duration`（16 bytes）× この上限 × 2（A/B 経路）でも数十 MB に収まる値を採用する。
const MAX_ITERATIONS: u32 = 1_000_000;

/// 単独計測の実行設定。
///
/// フィールドは非公開とし、`new`（検証コンストラクタ）経由でのみ構築できる。
/// これにより「プロトコル下限を回避できる直接構築経路」を作らない
/// （security.md「設定ミス」観点、fail-closed）。
#[derive(Debug, Clone, Copy)]
pub struct MeasurementConfig {
    warmup_iterations: u32,
    measured_iterations: u32,
    seed: u64,
}

impl MeasurementConfig {
    /// 検証コンストラクタ。プロトコル下限（warmup 20 回以上・計測 20 回以上）を
    /// 満たさない場合、または `MAX_ITERATIONS` を超える場合は
    /// `Err(BenchError::ProtocolViolation)` を返す。
    pub fn new(
        warmup_iterations: u32,
        measured_iterations: u32,
        seed: u64,
    ) -> Result<Self, BenchError> {
        if warmup_iterations < MIN_WARMUP_ITERATIONS {
            return Err(BenchError::ProtocolViolation(
                "warmup_iterations below protocol minimum",
            ));
        }
        if measured_iterations < MIN_MEASURED_ITERATIONS {
            return Err(BenchError::ProtocolViolation(
                "measured_iterations below protocol minimum",
            ));
        }
        if warmup_iterations > MAX_ITERATIONS {
            return Err(BenchError::ProtocolViolation(
                "warmup_iterations exceeds protocol maximum",
            ));
        }
        if measured_iterations > MAX_ITERATIONS {
            return Err(BenchError::ProtocolViolation(
                "measured_iterations exceeds protocol maximum",
            ));
        }
        Ok(Self {
            warmup_iterations,
            measured_iterations,
            seed,
        })
    }

    /// 決定的シードの RNG を初期化する際に使うシード値。
    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn warmup_iterations(&self) -> u32 {
        self.warmup_iterations
    }

    pub fn measured_iterations(&self) -> u32 {
        self.measured_iterations
    }
}

impl Default for MeasurementConfig {
    /// プロトコル下限ちょうどの既定構成（warmup 20 回・計測 20 回、シード 0）。
    ///
    /// 下限値そのものなので `new` は必ず成功する。
    fn default() -> Self {
        Self::new(MIN_WARMUP_ITERATIONS, MIN_MEASURED_ITERATIONS, 0)
            .expect("default config must satisfy its own protocol minimums")
    }
}

/// 単独計測の結果。中央値・Q1/Q3 に加え、後続の再集計・可視化のため生サンプルも保持する。
#[derive(Debug, Clone)]
pub struct Measurement {
    pub summary: Summary,
    pub samples: Vec<Duration>,
}

/// warmup フェーズ（計測しない）ののち計測フェーズを実行し、統計値を返す。
///
/// `workload` は毎回呼び出され、戻り値は `black_box` に通してコンパイラによる
/// 呼び出し省略・結果未使用最適化を防ぐ（結果を使わない計測はゼロ秒に最適化され
/// うるため）。
pub fn run<T>(
    config: &MeasurementConfig,
    mut workload: impl FnMut() -> T,
) -> Result<Measurement, BenchError> {
    for _ in 0..config.warmup_iterations {
        black_box(workload());
    }

    let mut samples = Vec::with_capacity(config.measured_iterations as usize);
    for _ in 0..config.measured_iterations {
        let start = Instant::now();
        black_box(workload());
        let elapsed = start.elapsed();
        samples.push(elapsed);
    }

    let summary = stats::summarize(&samples)?;
    Ok(Measurement { summary, samples })
}
