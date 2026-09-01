//! `isa.rs` dot カーネルの複数アキュムレータ化（Issue #365。前提: Issue #362・
//! `docs/design/knn-stage-profile.md`「`dot_lanes` の実アセンブリ確認」節で、
//! AVX2+FMA 環境の `dot_avx2_fma` が単一 FMA 依存チェーンに律速されていることが
//! 判明済み）のマイクロベンチが使う時間非依存ヘルパ。`benches/dot_kernel_bench.rs`
//! （実測。時間依存・`make ci` 対象外）と `tests/dot_kernel_accept.rs`（`#[path]` で
//! 本モジュールを取り込む時間非依存の回帰。`make ci` 対象）の双方から共有される
//! （`harness/mod.rs` 冒頭コメントと同じ取り込み方針）。
//!
//! `engine::isa::dot_scalar` を参照実装として使うため `engine::` に依存する
//! （`harness/hybrid_latency.rs` 冒頭コメントと同じ理由で、`crate::` の自己参照
//! 曖昧性は生じない）。
//!
//! # 暗号用途禁止
//!
//! [`crate::rng::DeterministicRng`] を経由するため非暗号 PRNG である。ベンチ入力
//! 生成専用。

use std::fmt;
use std::time::Duration;

use super::rng::DeterministicRng;

/// [`generate_corpus`] が許容する行数の安全上限（coding-rust.md「無制限確保禁止」。
/// `harness/hybrid_latency.rs::MAX_CORPUS_DOCS_GUARD` と同一方針）。
pub const MAX_CORPUS_ROWS_GUARD: usize = 200_000;

/// [`generate_corpus`] が許容する総要素数（`rows * dim`）の安全上限。`rows` は
/// [`MAX_CORPUS_ROWS_GUARD`] で個別に上限検証しているが、`dim` は呼び出し元の
/// 引数でありそれ自体に上限が無いため、乗算結果を `Vec::with_capacity` へ渡す前に
/// 総要素数として別途上限を課す（coding-rust.md「無制限確保禁止」。乗算は
/// `checked_mul` を使い、オーバーフロー時に `saturating_mul` のような巨大値への
/// 丸めで `Vec::with_capacity` が OOM/abort する経路を作らない）。本ベンチが使う
/// 最大次元（`dot_kernel_bench.rs::DIMS` の 1536）に十分な余裕を持たせつつ、
/// 誤って巨大な `dim` を渡した場合に早期拒否できる値として 1 行あたり 65536
/// 要素（次元）を上限に採る。
pub const MAX_CORPUS_ELEMENTS_GUARD: usize = MAX_CORPUS_ROWS_GUARD * 65_536;

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq)]
pub enum DotKernelError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された。
    RefusedUnderGitHubActions,
    /// 計測対象の dot 呼び出し回数が 0 のため ns/dot への換算ができない。
    ZeroDots,
    /// [`MAX_CORPUS_ROWS_GUARD`] を超過した。
    CorpusTooLarge,
    /// 実測値が非有限（NaN/inf）だった。
    NonFiniteResult,
    /// スカラー参照実装との数値差が許容差を超えた。
    ToleranceExceeded { actual: f32, expected: f32 },
}

impl fmt::Display for DotKernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DotKernelError::RefusedUnderGitHubActions => write!(
                f,
                "dot_kernel_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); \
                 this bench is manual-only and not wired into any workflow"
            ),
            DotKernelError::ZeroDots => write!(f, "cannot compute ns/dot for zero dot calls"),
            DotKernelError::CorpusTooLarge => {
                write!(
                    f,
                    "rows exceeds {MAX_CORPUS_ROWS_GUARD} or rows * dim exceeds {MAX_CORPUS_ELEMENTS_GUARD}"
                )
            }
            DotKernelError::NonFiniteResult => write!(f, "measured result is not finite"),
            DotKernelError::ToleranceExceeded { actual, expected } => write!(
                f,
                "dot result outside tolerance: actual={actual} expected={expected}"
            ),
        }
    }
}

impl std::error::Error for DotKernelError {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`harness::hybrid_latency::
/// refuse_under_github_actions` と同一パターン）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), DotKernelError> {
    if under_github_actions {
        return Err(DotKernelError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// 計測対象の作業集合規模。cache 常駐（L1/L2 相当に収まる規模）と arena 規模
/// （実運用のテーブル規模に近い行数）の 2 種を比較する（計画「参考プロトタイプ
/// 実測」節: 複数アキュムレータ化の効果は cache 常駐かつ次元が大きい場合に限られ、
/// DRAM 帯域律速の大規模データでは効果が薄いことを実測で切り分けるため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingSet {
    /// 作業集合を [`CACHE_RESIDENT_BUDGET_BYTES`] に収める規模。
    CacheResident,
    /// 実運用のテーブル規模に近い arena 規模（固定行数）。
    ArenaScale,
}

/// cache 常駐段の目標作業集合サイズ（バイト）。L1/L2 に概ね収まる規模として
/// 64 KiB を採る（`knn_profile_bench.rs` 等の既存ベンチが特定 CPU の実キャッシュ
/// 容量に依存しない、控えめで移植可能な値を採る方針と同じ）。
pub const CACHE_RESIDENT_BUDGET_BYTES: usize = 64 * 1024;

/// arena 規模段の固定行数（計画「参考プロトタイプ実測」節の実測条件と同一）。
pub const ARENA_SCALE_ROWS: usize = 25_000;

/// `working_set` と `dim` から計測対象の行数を決める純関数。cache 常駐段は
/// [`CACHE_RESIDENT_BUDGET_BYTES`] を `dim * 4`（f32 バイト数）で割った行数
/// （最低 1 行）、arena 規模段は [`ARENA_SCALE_ROWS`] 固定。
///
/// `dim == 0` は cache 常駐段の除算がゼロ除算になるため [`DotKernelError::
/// CorpusTooLarge`] とは異なる用途外の値として扱わず、呼び出し元契約として
/// `dim >= 1` を前提とする（本ベンチの `dims` 定数は常に 1 以上のためこの関数は
/// 内部専用）。
pub fn rows_for(working_set: WorkingSet, dim: usize) -> Result<usize, DotKernelError> {
    let rows = match working_set {
        WorkingSet::CacheResident => {
            let bytes_per_row = dim.saturating_mul(4).max(1);
            (CACHE_RESIDENT_BUDGET_BYTES / bytes_per_row).max(1)
        }
        WorkingSet::ArenaScale => ARENA_SCALE_ROWS,
    };
    if rows > MAX_CORPUS_ROWS_GUARD {
        return Err(DotKernelError::CorpusTooLarge);
    }
    Ok(rows)
}

/// 決定的シードから `rows` 行 × `dim` 次元のコーパス（フラット化済み `f32`
/// ベクトル）を生成する。[`DeterministicRng`] 経由のため同一シードから常に
/// 同一の値を再生成できる。
pub fn generate_corpus(seed: u64, dim: usize, rows: usize) -> Result<Vec<f32>, DotKernelError> {
    if rows > MAX_CORPUS_ROWS_GUARD {
        return Err(DotKernelError::CorpusTooLarge);
    }
    // `dim` は呼び出し元の引数で上限が無いため、`rows * dim` を `checked_mul` で
    // 計算し、オーバーフロー（`None`）または [`MAX_CORPUS_ELEMENTS_GUARD`] 超過を
    // ここで拒否してから `Vec::with_capacity` へ渡す（`saturating_mul` は
    // オーバーフロー時に `usize::MAX` へ丸まり、巨大 capacity 要求で
    // `Vec::with_capacity` が OOM/abort し得るため使わない）。
    let total_elements = rows
        .checked_mul(dim)
        .filter(|&total| total <= MAX_CORPUS_ELEMENTS_GUARD)
        .ok_or(DotKernelError::CorpusTooLarge)?;
    let mut rng = DeterministicRng::new(seed);
    let mut out = Vec::with_capacity(total_elements);
    for _ in 0..rows {
        out.extend_from_slice(&rng.next_vector(dim));
    }
    Ok(out)
}

/// [`generate_corpus`] と系列を分離したクエリベクトル生成（`harness::
/// hybrid_latency::generate_query` と同じくシードへ固定オフセットを加える）。
pub fn generate_query(seed: u64, dim: usize) -> Vec<f32> {
    let mut rng = DeterministicRng::new(seed.wrapping_add(0x1357_9bdf_1357_9bdf));
    rng.next_vector(dim)
}

/// 総 dot 呼び出し回数から ns/dot を換算する。
pub fn ns_per_dot(total: Duration, dots: usize) -> Result<f64, DotKernelError> {
    if dots == 0 {
        return Err(DotKernelError::ZeroDots);
    }
    Ok(total.as_secs_f64() * 1e9 / dots as f64)
}

/// 候補（`candidate_ns`）とベースライン（`baseline_ns`）の比率
/// （`candidate / baseline`。1.0 未満は改善、1.0 超過は悪化）。
pub fn speedup_ratio(baseline_ns: f64, candidate_ns: f64) -> f64 {
    candidate_ns / baseline_ns
}

/// 比率をノイズ帯（`noise_band`。例: 0.05 = ±5%）で分類する結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeClass {
    /// `ratio <= 1.0 - noise_band`。
    Improved,
    /// `1.0 - noise_band < ratio < 1.0 + noise_band`。
    Neutral,
    /// `ratio >= 1.0 + noise_band`。
    Regressed,
}

/// [`speedup_ratio`] の結果をノイズ帯で 3 分類する純関数（決定規則の判定条件を
/// 時間非依存に検証できるようにする）。
pub fn classify_change(ratio: f64, noise_band: f64) -> ChangeClass {
    if ratio <= 1.0 - noise_band {
        ChangeClass::Improved
    } else if ratio >= 1.0 + noise_band {
        ChangeClass::Regressed
    } else {
        ChangeClass::Neutral
    }
}

/// 1 行分の実測結果を人間可読な形へ整形する（stdout 出力用。本ベンチは spec 由来の
/// 閾値を持たない情報提供専用のため、実測値をそのまま出力してよい
/// （`.claude/rules/spec-confidentiality.md` のオーナー判断範囲）。
pub fn render_line(
    label: &str,
    working_set: WorkingSet,
    dim: usize,
    rows: usize,
    median: Duration,
    ns_per_dot: f64,
) -> String {
    let ws = match working_set {
        WorkingSet::CacheResident => "cache_resident",
        WorkingSet::ArenaScale => "arena_scale",
    };
    format!(
        "dot_kernel: label={label} working_set={ws} dim={dim} rows={rows} median_ms={:.3} ns_per_dot={ns_per_dot:.2}",
        median.as_secs_f64() * 1e3
    )
}

/// `actual` がスカラー参照実装（`engine::isa::dot_scalar`）の `expected` と
/// 許容差内で一致するか検証する（`crates/engine/src/isa.rs` の unit テスト
/// `current_dot_matches_scalar_reference_within_tolerance` と同じ許容差式を
/// 再利用し、ベンチ・回帰テストの双方から同一の判定基準で検証できるようにする）。
/// `magnitude` はスカラー参照値の絶対値（許容差算出に使う `dot_scalar(&a,&b).abs()`
/// 相当を呼び出し元が渡す）。
pub fn check_matches_scalar_reference(
    actual: f32,
    expected: f32,
    magnitude: f32,
) -> Result<(), DotKernelError> {
    if !actual.is_finite() {
        return Err(DotKernelError::NonFiniteResult);
    }
    let tolerance = 1e-5 * magnitude.abs().max(1.0) + 1e-4;
    if (actual - expected).abs() > tolerance {
        return Err(DotKernelError::ToleranceExceeded { actual, expected });
    }
    Ok(())
}
