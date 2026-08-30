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

/// warmup 回数の下限。極端に少ない回数では JIT・キャッシュ・周波数遷移等の
/// ウォームアップ効果が計測フェーズへ持ち越され、統計値の再現性が損なわれるため
/// 一定の下限を設ける。この値を下回る `MeasurementConfig` は `new` が拒否する。
const MIN_WARMUP_ITERATIONS: u32 = 20;

/// 計測回数の下限（`MIN_WARMUP_ITERATIONS` 同様、外れ値に頑健な要約統計値
/// （中央値・四分位）を得るための最小サンプル数という一般的な統計上の根拠による）。
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
/// うるため）。戻り値は `black_box` 通過後、計測区間の内側で drop される
/// （`ab::run_ab` と同一の契約。Issue #302。ヒープ確保を伴う戻り値の解放コストが
/// 無視できない場合、呼び出し側は測定区間外の sink へ退避してから `run` を呼ぶ）。
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

/// [`run_fallible`] が 1 試行の失敗をどう扱うかを表す分類（TASK-116・Issue #316）。
/// 呼び出し元（`tier_latency_bench.rs`）が `classify` クロージャで各エラー値を
/// 本 enum へ写像する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialFailure {
    /// 有効サンプルに数えず、追加試行で埋め合わせる対象（例: LLM の不正応答
    /// `PlanError::InvalidResponse`）。除外数には段ごとの上限（`max_excluded`）が
    /// あり、超過は `BenchError::ExcludedTrialsExceeded`。
    Excluded,
    /// p95 判定を fail-open にしないため除外しない致命エラー（例: `Timeout`・
    /// `Unavailable`）。即座に `BenchError::FatalTrial` として計測全体を打ち切る。
    Fatal,
}

/// [`run_fallible`] の結果。時間統計に加え、試行回数・除外回数の内訳を保持する
/// （`tier_latency_bench.rs` が段ごとに `attempts=… invalid_responses=…` として
/// 出力するために使う。p95 上限自体は含まないため出力しても閾値漏えいにならない）。
#[derive(Debug, Clone)]
pub struct FallibleMeasurement {
    pub measurement: Measurement,
    /// 計測フェーズで実際に行った試行回数（成功・除外を問わない）。
    pub measured_attempts: u32,
    /// 計測フェーズで `Excluded` と分類された試行数。
    pub measured_excluded: u32,
    /// warmup フェーズで `Excluded` と分類された試行数（合否には数えない。
    /// モジュールドキュメント・呼び出し元の設計判断コメント参照）。
    pub warmup_excluded: u32,
}

/// warmup フェーズ（計測しない）ののち、失敗許容の計測フェーズを実行する
/// （TASK-116・Issue #316: `tier_latency_bench.rs` が LLM 不正応答〔`Excluded`〕を
/// 交えても既定の有効サンプル数（`config.measured_iterations`）に到達できるよう
/// 一般化した計測プリミティブ。既存 [`run`] の契約・挙動は変更しない）。
///
/// - warmup: `config.warmup_iterations` 回試行する。`Excluded` は
///   `warmup_excluded` へ加算するのみ（合否に数えない）。`Fatal` は直ちに
///   `Err(FatalTrial)` で打ち切る（warmup 中の致命エラーも計測を継続しない）。
/// - 計測: 有効サンプル（`Ok`）が `config.measured_iterations` 件に達するまで
///   試行を続ける。`Excluded` が `max_excluded` を超えた時点で
///   `Err(ExcludedTrialsExceeded)`。`Fatal` は直ちに `Err(FatalTrial)`。
///
/// 試行総数の上限は `config.measured_iterations.checked_add(max_excluded)`
/// （coding-rust.md「整数演算は `checked_*`／`saturating_*` を使う」対応。
/// オーバーフロー時は `BenchError::ProtocolViolation` で拒否し、無限ループを
/// 防ぐ）。`workload` の呼び出し規約（`black_box` の位置・同期完了の責務）は
/// [`run`] と同一。
pub fn run_fallible<T, E: std::fmt::Display>(
    config: &MeasurementConfig,
    max_excluded: u32,
    mut workload: impl FnMut() -> Result<T, E>,
    classify: impl Fn(&E) -> TrialFailure,
) -> Result<FallibleMeasurement, BenchError> {
    let attempt_bound = config.measured_iterations.checked_add(max_excluded).ok_or(
        BenchError::ProtocolViolation("measured_iterations + max_excluded overflows u32"),
    )?;

    let mut warmup_excluded: u32 = 0;
    for _ in 0..config.warmup_iterations {
        match black_box(workload()) {
            Ok(v) => {
                black_box(v);
            }
            Err(e) => match classify(&e) {
                TrialFailure::Excluded => warmup_excluded += 1,
                TrialFailure::Fatal => return Err(BenchError::FatalTrial(format!("{e}"))),
            },
        }
    }

    let mut samples = Vec::with_capacity(config.measured_iterations as usize);
    let mut measured_excluded: u32 = 0;
    let mut measured_attempts: u32 = 0;

    while samples.len() < config.measured_iterations as usize {
        if measured_attempts >= attempt_bound {
            // 上限は measured_iterations + max_excluded なので、ここに到達するのは
            // 除外数が max_excluded を超えた場合のみ（Excluded 分類のたびに下の
            // if で早期 Err を返すため、通常は到達しないガード。防御的に残す）。
            return Err(BenchError::ExcludedTrialsExceeded {
                excluded: measured_excluded,
                max_excluded,
            });
        }
        measured_attempts += 1;
        let start = Instant::now();
        let result = black_box(workload());
        match result {
            // 成功値は `run` と同じく所有値を `black_box` へ渡して計測区間の内側で
            // drop してから `elapsed` を取る（codex-review 指摘・PR #329。以前は
            // `elapsed()` 取得後に `black_box(&v)` していたため drop が計測区間外に
            // なり、ヒープ所有値を返す workload では `run` より短い値を記録して
            // A/B 比較を歪めていた）。
            Ok(v) => {
                black_box(v);
                let elapsed = start.elapsed();
                samples.push(elapsed);
            }
            Err(e) => match classify(&e) {
                TrialFailure::Excluded => {
                    measured_excluded += 1;
                    if measured_excluded > max_excluded {
                        return Err(BenchError::ExcludedTrialsExceeded {
                            excluded: measured_excluded,
                            max_excluded,
                        });
                    }
                }
                TrialFailure::Fatal => return Err(BenchError::FatalTrial(format!("{e}"))),
            },
        }
    }

    let summary = stats::summarize(&samples)?;
    Ok(FallibleMeasurement {
        measurement: Measurement { summary, samples },
        measured_attempts,
        measured_excluded,
        warmup_excluded,
    })
}
