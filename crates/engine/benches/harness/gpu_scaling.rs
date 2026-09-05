//! GPU バッチ検索（`engine::gpu_batch`。TASK-128〜130・Issue #178 ポインタ）と
//! CPU-SIMD バッチ経路（`engine::batch_search::BatchEngine`）のどちらが速いかを
//! 規模・バッチサイズごとに実測するための、時間非依存な純関数群。
//!
//! 本モジュールが提供するのは「env 変数からの計測条件パース」「出力行の整形」
//! 「Top-k 結果の同点許容つき不一致検知」のみで、いずれも `engine`・GPU デバイス
//! そのものには依存しない（`tests/gpu_scaling_accept.rs` から GPU 非依存で
//! 単体検証できる。`bench_engine.rs`・`recall_engine.rs` と同じ切り分け方針）。
//! GPU バックエンドの構築・計測ループ本体は `benches/gpu_scaling_bench.rs`
//! （手動専用・`harness = false`）が担う。
//!
//! 本ベンチは spec が定める受け入れ基準（CORE-6/CORE-16 等の閾値ゲート）を
//! 持たない情報提供専用の計測ツールである
//! （`hnsw_parallel_build_bench.rs`・`Makefile: bench-hnsw-parallel-build` と
//! 同型の位置づけ）。したがって本モジュールにも判定用の閾値は持たせない。

use std::fmt;
use std::time::Duration;

/// 計測条件パース・出力整形いずれかの失敗を表す fail-closed なエラー型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuScalingError {
    message: String,
}

impl fmt::Display for GpuScalingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GpuScalingError {}

fn err(message: impl Into<String>) -> GpuScalingError {
    GpuScalingError {
        message: message.into(),
    }
}

/// `BENCH_GPU_SCALING_ROWS`/`BENCH_GPU_SCALING_DIMS`/`BENCH_GPU_SCALING_BATCH`
/// が受け付けるカンマ区切りリストの最大要素数（本ベンチ固有の安全弁。開発者の
/// typo で桁違いの直積が組まれ計測が長時間・大容量アロケーションに陥ることを
/// 防ぐための上限であり、spec の数値基準ではない）。
const MAX_LIST_ENTRIES: usize = 16;

/// 行数（rows）の上限（本ベンチ固有の安全弁）。500,000 行・dim 256 の f32 常駐
/// バッファが約 512 MiB になる規模を上回る値を開発者が誤って指定した場合に、
/// アロケーション前に拒否する。
const MAX_ROWS: usize = 5_000_000;

/// 次元数（dim）の上限（本ベンチ固有の安全弁）。
const MAX_DIM: usize = 8192;

/// バッチサイズの上限（本ベンチ固有の安全弁）。
const MAX_BATCH: usize = 100_000;

/// `BENCH_GPU_SCALING_TOPK` の上限（本ベンチ固有の安全弁）。
const MAX_TOPK: usize = 10_000;

/// `BENCH_GPU_SCALING_ITERS`（計測反復回数）の上限。
/// `harness::protocol::MeasurementConfig::new` 自体が持つ上限
/// （`MAX_ITERATIONS` = 1,000,000）と同じ値を採用し、プロトコル層より緩い
/// 上限を課さない。
const MAX_MEASURED_ITERATIONS: u32 = 1_000_000;

/// `harness::protocol::MeasurementConfig::new` が要求する計測回数の下限
/// （20 回）と同じ値。`BENCH_GPU_SCALING_ITERS` 未満の値は `MeasurementConfig::new`
/// 自体が拒否するが、本モジュールでも早期に同じ理由で拒否し、パース段階と
/// 計測プロトコル段階のエラーメッセージを一致させる。
const MIN_MEASURED_ITERATIONS: u32 = 20;

/// `std::env::var` を fail-closed に読む（`bench_engine.rs::read_env_var` と同型。
/// 非 UTF-8 値〔`NotUnicode`〕を「未設定」へ黙って合流させない）。
pub fn read_env_var(name: &'static str) -> Result<Option<String>, GpuScalingError> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(err(format!("{name} value is not valid UTF-8")))
        }
    }
}

/// カンマ区切りの正整数リストをパースする（`BENCH_GPU_SCALING_ROWS` 等）。
/// 未設定・空文字列は `default` をそのまま返す。要素は `min..=max` の範囲内かつ
/// 全体で `MAX_LIST_ENTRIES` 件以下でなければならない（超過・範囲外・非数値は
/// すべて fail-closed で拒否する）。
pub fn parse_usize_list(
    raw: Option<&str>,
    default: &[usize],
    min: usize,
    max: usize,
    field_name: &str,
) -> Result<Vec<usize>, GpuScalingError> {
    let trimmed = raw.map(str::trim);
    match trimmed {
        None | Some("") => Ok(default.to_vec()),
        Some(s) => {
            let parts: Vec<&str> = s.split(',').collect();
            if parts.is_empty() || parts.len() > MAX_LIST_ENTRIES {
                return Err(err(format!(
                    "{field_name} must list 1..={MAX_LIST_ENTRIES} comma-separated values (got {})",
                    parts.len()
                )));
            }
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                let value: usize = part.trim().parse().map_err(|_| {
                    err(format!(
                        "{field_name} entries must be positive integers (got {part:?})"
                    ))
                })?;
                if value < min || value > max {
                    return Err(err(format!(
                        "{field_name} entries must be in range {min}..={max} (got {value})"
                    )));
                }
                out.push(value);
            }
            Ok(out)
        }
    }
}

/// `BENCH_GPU_SCALING_ROWS` を規模リストへ解決する。
pub fn parse_rows(raw: Option<&str>, default: &[usize]) -> Result<Vec<usize>, GpuScalingError> {
    parse_usize_list(raw, default, 1, MAX_ROWS, "BENCH_GPU_SCALING_ROWS")
}

/// `BENCH_GPU_SCALING_DIMS` を次元リストへ解決する。
pub fn parse_dims(raw: Option<&str>, default: &[usize]) -> Result<Vec<usize>, GpuScalingError> {
    parse_usize_list(raw, default, 1, MAX_DIM, "BENCH_GPU_SCALING_DIMS")
}

/// `BENCH_GPU_SCALING_BATCH` をバッチサイズリストへ解決する。
pub fn parse_batches(raw: Option<&str>, default: &[usize]) -> Result<Vec<usize>, GpuScalingError> {
    parse_usize_list(raw, default, 1, MAX_BATCH, "BENCH_GPU_SCALING_BATCH")
}

/// `BENCH_GPU_SCALING_TOPK`（Top-k）を解決する。未設定・空文字列は `default`。
pub fn parse_top_k(raw: Option<&str>, default: usize) -> Result<usize, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(default),
        Some(s) => {
            let value: usize = s.parse().map_err(|_| {
                err(format!(
                    "BENCH_GPU_SCALING_TOPK must be a positive integer (got {s:?})"
                ))
            })?;
            if value == 0 || value > MAX_TOPK {
                return Err(err(format!(
                    "BENCH_GPU_SCALING_TOPK must be in range 1..={MAX_TOPK} (got {value})"
                )));
            }
            Ok(value)
        }
    }
}

/// `BENCH_GPU_SCALING_ITERS`（計測反復回数。warmup は固定 20 回で本設定の対象外）を
/// 解決する。未設定・空文字列は `default`。
pub fn parse_measured_iterations(raw: Option<&str>, default: u32) -> Result<u32, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(default),
        Some(s) => {
            let value: u32 = s.parse().map_err(|_| {
                err(format!(
                    "BENCH_GPU_SCALING_ITERS must be a positive integer (got {s:?})"
                ))
            })?;
            if !(MIN_MEASURED_ITERATIONS..=MAX_MEASURED_ITERATIONS).contains(&value) {
                return Err(err(format!(
                    "BENCH_GPU_SCALING_ITERS must be in range {MIN_MEASURED_ITERATIONS}..={MAX_MEASURED_ITERATIONS} \
                     (got {value})"
                )));
            }
            Ok(value)
        }
    }
}

/// 1 つの (rows, dim, batch) 構成に対する実測結果（出力整形の入力）。
#[derive(Debug, Clone, Copy)]
pub struct GpuScalingResult {
    pub rows: usize,
    pub dim: usize,
    pub batch: usize,
    pub k: usize,
    pub cpu_simd_p50: Duration,
    pub cpu_simd_p95: Duration,
    pub gpu_f16_p50: Duration,
    pub gpu_f16_p95: Duration,
    pub gpu_f32_p50: Duration,
    pub gpu_f32_p95: Duration,
    /// 1 クエリあたりの CPU-SIMD 中央値所要時間（`cpu_simd_p50 / batch`）。
    pub per_query_cpu_p50: Duration,
    /// 1 クエリあたりの GPU f16 常駐経路の中央値所要時間（`gpu_f16_p50 / batch`）。
    pub per_query_gpu_f16_p50: Duration,
    /// GPU f16 常駐経路の p95 短縮率（`cpu_simd_p95 / gpu_f16_p95`）。
    /// 1.0 を上回るほど GPU 側が高速。
    pub speedup_f16_p95: f64,
    /// A（CPU-SIMD）を厳密対照としたときの、B（GPU f16）・C（GPU f32）双方の
    /// Top-k 結果の同点許容つき不一致件数の合計（全クエリ分。0 が期待値）。
    pub mismatch: usize,
}

impl fmt::Display for GpuScalingResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gpu_scaling: rows={} dim={} batch={} k={} cpu_simd_p50={}us cpu_simd_p95={}us \
             gpu_f16_p50={}us gpu_f16_p95={}us gpu_f32_p50={}us gpu_f32_p95={}us \
             per_query_cpu_p50={}us per_query_gpu_f16_p50={}us speedup_f16_p95={:.2}x mismatch={}",
            self.rows,
            self.dim,
            self.batch,
            self.k,
            self.cpu_simd_p50.as_micros(),
            self.cpu_simd_p95.as_micros(),
            self.gpu_f16_p50.as_micros(),
            self.gpu_f16_p95.as_micros(),
            self.gpu_f32_p50.as_micros(),
            self.gpu_f32_p95.as_micros(),
            self.per_query_cpu_p50.as_micros(),
            self.per_query_gpu_f16_p50.as_micros(),
            self.speedup_f16_p95,
            self.mismatch,
        )
    }
}

/// `cpu_p95`/`gpu_f16_p95` から短縮率を算出する。`gpu_f16_p95` が 0 の場合
/// （実測上ほぼ起こらないが計測分解能未満に丸まった場合の防御）は fail-closed に
/// `Err` を返す（NaN/inf を出力へ混入させない）。
pub fn speedup_ratio(cpu_p95: Duration, gpu_f16_p95: Duration) -> Result<f64, GpuScalingError> {
    if gpu_f16_p95.is_zero() {
        return Err(err("gpu_f16_p95 is zero; cannot compute speedup ratio"));
    }
    Ok(cpu_p95.as_secs_f64() / gpu_f16_p95.as_secs_f64())
}

/// (rows, dim, batch) の組が測定量上限（`engine::batch_search::MAX_BATCH_WORK`
/// 相当）を超えるため計測をスキップした、という情報行を整形する。
pub fn format_skip_line(rows: usize, dim: usize, batch: usize, k: usize, reason: &str) -> String {
    format!("gpu_scaling: skip rows={rows} dim={dim} batch={batch} k={k} reason=\"{reason}\"")
}

/// GPU バックエンドが利用不能だった（構築失敗・デバイス未検出等）ことを示す行を
/// 整形する。`combo` が `None` の場合はプロセス全体としてこのベンチが GPU を
/// 一切使えないことを示す（起動直後の疎通確認失敗）。
pub fn format_unavailable_line(
    combo: Option<(usize, usize, usize, usize)>,
    reason: &str,
) -> String {
    match combo {
        Some((rows, dim, batch, k)) => format!(
            "gpu_scaling: not measurable rows={rows} dim={dim} batch={batch} k={k} reason=\"{reason}\""
        ),
        None => format!("gpu_scaling: gpu unavailable ({reason})"),
    }
}

/// A（対照。CPU-SIMD 経路の厳密 Top-k）を基準に、B/C（GPU 経路）1 クエリ分の
/// Top-k 結果が「A の id 集合に含まれる」か「A の k 位スコア（`baseline` 中の
/// 最小スコア）以上である」かのいずれかを満たすかを確認し、いずれも満たさない
/// 候補の件数（不一致件数）を返す。
///
/// `baseline`・`candidate` は `(id, score)` の組の列（`SearchHit` から
/// テナント情報を落として比較する。単一テナント合成データセットのみを
/// 対象とするベンチのため、`tenant_id` の突き合わせは不要——呼び出し元
/// `gpu_scaling_bench.rs` 側で保証する）。`baseline` が空の場合、`candidate` も
/// 空であれば不一致 0、そうでなければ `candidate` の全件を不一致として数える
/// （比較対象を持たない候補はすべて疑わしいとみなす fail-closed 側の扱い）。
/// `candidate` が `baseline` より短い場合は欠落件数を、同一 id の重複返却は
/// その重複件数を、境界スコアより高い baseline の id が `candidate` に無い場合は
/// その件数を、それぞれ不一致に加算する（同点による置換は境界上の結果に限る）。
pub fn count_boundary_tolerant_mismatches(
    baseline: &[(u64, f32)],
    candidate: &[(u64, f32)],
) -> usize {
    if baseline.is_empty() {
        return candidate.len();
    }
    let boundary_score = baseline
        .iter()
        .map(|(_, score)| *score)
        .fold(f32::INFINITY, f32::min);
    let baseline_ids: std::collections::HashSet<u64> = baseline.iter().map(|(id, _)| *id).collect();
    let extra = candidate
        .iter()
        .filter(|(id, score)| !baseline_ids.contains(id) && *score < boundary_score)
        .count();
    // 候補が基準より少ない（Top-k が欠けている・空である）場合、その欠落分も
    // 不一致として数える。余分な候補だけを数えると GPU 経路が結果を取りこぼした
    // ときに `mismatch=0` と報告してしまう（fail-closed）。
    let missing = baseline.len().saturating_sub(candidate.len());
    // 同一 id の重複返却も不一致（Top-k は id の集合として一意であるべき）。
    let mut seen = std::collections::HashSet::with_capacity(candidate.len());
    let duplicates = candidate.iter().filter(|(id, _)| !seen.insert(*id)).count();
    // 同点による置換は境界スコア上の結果に限る。境界より高いスコアを持つ baseline の
    // id は必ず candidate に含まれていなければならず、欠けていれば不一致に数える
    // （件数差だけでは、境界同点の候補が上位 id を置き換えた取りこぼしを見逃す）。
    let candidate_ids: std::collections::HashSet<u64> =
        candidate.iter().map(|(id, _)| *id).collect();
    let dropped_above_boundary = baseline
        .iter()
        .filter(|(id, score)| *score > boundary_score && !candidate_ids.contains(id))
        .count();
    // 候補が短い場合の欠落件数と、境界より上位の id の欠落は同じ取りこぼしを別の
    // 側面から数えている（空の候補では両方が計上される）ため、二重計上せず大きい方を
    // 採る。
    extra + duplicates + missing.max(dropped_above_boundary)
}
