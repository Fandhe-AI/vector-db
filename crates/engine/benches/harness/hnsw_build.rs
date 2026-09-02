//! HNSW グラフ構築（TASK-132・`engine::hnsw`）の構築計算量スケーリング確認
//! （受け入れ条件 (b): 「構築計算量が規模に対してほぼ N log N であることの簡易
//! ベンチ確認」）が使う時間非依存ヘルパ。`benches/hnsw_build_bench.rs`（実測。
//! 時間依存・`make ci` 対象外）と `tests/hnsw_build_accept.rs`（`#[path]` で本
//! モジュールを取り込む時間非依存の回帰。`make ci` 対象）の双方から共有される
//! （`harness/mod.rs` 冒頭コメント・`harness/dot_kernel.rs` と同じ取り込み方針）。
//!
//! `engine::hnsw::HnswIndex::build` を計測対象として直接呼ぶため `engine::` に
//! 依存する（`harness/dot_kernel.rs` が `engine::isa::dot_scalar` を参照実装として
//! 使うのと同じ理由）。
//!
//! # 暗号用途禁止
//!
//! [`crate::rng::DeterministicRng`] を経由するため非暗号 PRNG である。ベンチ入力
//! 生成専用。
//!
//! # インラインの `#[cfg(test)] mod tests` を置かない理由
//!
//! `harness/ab.rs::median_ratio` の doc コメントと同じ制約: 本モジュールは
//! `#[path]` 経由で複数の bench バイナリ（`hnsw_build_bench.rs` に限らず、
//! `mod.rs` を通じて他の全 bench バイナリからも）取り込まれるが、bench 側の
//! コンパイル（`--test` フラグなし）では `#[test]` 項目が丸ごと除去されるため、
//! ここにインラインの `mod tests` を置くと `use super::*;` が unused import に
//! なる。回帰テストは `tests/hnsw_build_accept.rs` 側にのみ置く。

use std::fmt;
use std::time::Duration;

use super::rng::DeterministicRng;

/// [`generate_corpus`] が許容する行数の安全上限（coding-rust.md「無制限確保禁止」。
/// `harness/dot_kernel.rs::MAX_CORPUS_ROWS_GUARD` と同一方針。本ベンチの規模点
/// （最大 32,000 行）に対し十分な余裕を持たせる）。
pub const MAX_CORPUS_ROWS_GUARD: usize = 200_000;

/// [`generate_corpus`] が許容する 1 行あたりの次元数の安全上限
/// （`harness/dot_kernel.rs::MAX_DIM_GUARD` と同一方針）。
pub const MAX_DIM_GUARD: usize = 16_384;

/// [`generate_corpus`] が許容する総要素数（`rows * dim`）の安全上限
/// （`harness/dot_kernel.rs::MAX_CORPUS_ELEMENTS_GUARD` と同一方針。本ベンチの
/// 実ワークロード〔32,000 行 × dim 64 = 204.8 万要素〕に十分な余裕を持たせた
/// 固定上限）。
pub const MAX_CORPUS_ELEMENTS_GUARD: usize = 16 * 1024 * 1024;

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq)]
pub enum HnswBuildBenchError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された。
    RefusedUnderGitHubActions,
    /// [`MAX_CORPUS_ROWS_GUARD`]・[`MAX_DIM_GUARD`]・[`MAX_CORPUS_ELEMENTS_GUARD`]
    /// のいずれかを超過した。
    CorpusTooLarge,
    /// 実効指数を求めるための所要時間が 0 以下、または規模点数が 2 未満。
    InsufficientSamples,
}

impl fmt::Display for HnswBuildBenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HnswBuildBenchError::RefusedUnderGitHubActions => write!(
                f,
                "hnsw_build_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); \
                 this bench is manual-only and not wired into any workflow"
            ),
            HnswBuildBenchError::CorpusTooLarge => write!(
                f,
                "rows exceeds {MAX_CORPUS_ROWS_GUARD}, dim exceeds {MAX_DIM_GUARD}, \
                 or rows * dim exceeds {MAX_CORPUS_ELEMENTS_GUARD}"
            ),
            HnswBuildBenchError::InsufficientSamples => {
                write!(f, "need at least 2 measured (rows, duration) points with rows >= 2 and positive duration")
            }
        }
    }
}

impl std::error::Error for HnswBuildBenchError {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`harness::dot_kernel::
/// refuse_under_github_actions` と同一パターン）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), HnswBuildBenchError> {
    if under_github_actions {
        return Err(HnswBuildBenchError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// 決定的シードから `rows` 行 × `dim` 次元のコーパス（フラット化済み `f32`
/// ベクトル。`engine::hnsw::HnswIndex::build` にそのまま渡せるレイアウト）を
/// 生成する。[`DeterministicRng`] 経由のため同一シードから常に同一の値を
/// 再生成できる（`harness::dot_kernel::generate_corpus` と同型の上限検証つき）。
pub fn generate_corpus(
    seed: u64,
    dim: usize,
    rows: usize,
) -> Result<Vec<f32>, HnswBuildBenchError> {
    if rows > MAX_CORPUS_ROWS_GUARD {
        return Err(HnswBuildBenchError::CorpusTooLarge);
    }
    if dim > MAX_DIM_GUARD {
        return Err(HnswBuildBenchError::CorpusTooLarge);
    }
    let total_elements = rows
        .checked_mul(dim)
        .filter(|&total| total <= MAX_CORPUS_ELEMENTS_GUARD)
        .ok_or(HnswBuildBenchError::CorpusTooLarge)?;
    let mut rng = DeterministicRng::new(seed);
    let mut out = Vec::with_capacity(total_elements);
    for _ in 0..rows {
        out.extend_from_slice(&rng.next_vector(dim));
    }
    Ok(out)
}

/// 2 規模点 `(n1, t1)` → `(n2, t2)` の log-log 傾き（実効指数）。
/// `t = c * n^k` を仮定すると `k = ln(t2/t1) / ln(n2/n1)` になる
/// （`docs/design/dot-kernel-multi-accumulator.md` 等の既存ベンチが使う
/// speedup 比較とは異なり、本関数は「規模に対する伸び方」を要約する）。
///
/// `n1 == n2`（対数の分母が 0 になる）・`t1 <= 0`・`t2 <= 0` は
/// [`HnswBuildBenchError::InsufficientSamples`] として拒否する。
pub fn scaling_exponent(
    n1: usize,
    t1: Duration,
    n2: usize,
    t2: Duration,
) -> Result<f64, HnswBuildBenchError> {
    if n1 == n2 || n1 == 0 || n2 == 0 {
        return Err(HnswBuildBenchError::InsufficientSamples);
    }
    let t1_secs = t1.as_secs_f64();
    let t2_secs = t2.as_secs_f64();
    if t1_secs <= 0.0 || t2_secs <= 0.0 {
        return Err(HnswBuildBenchError::InsufficientSamples);
    }
    let ratio_n = (n2 as f64 / n1 as f64).ln();
    let ratio_t = (t2_secs / t1_secs).ln();
    Ok(ratio_t / ratio_n)
}

/// `t / (n * ln(n))` の比率（N log N 仮説の下で規模点間でおおむね一定になる
/// はずの量。実効指数が 2.0 に近いかどうかの判定を補強する情報提供指標）。
/// `n <= 1`（`ln(n) <= 0`）・`t == 0`（測定不能な所要時間）は
/// [`HnswBuildBenchError::InsufficientSamples`]。[`scaling_exponent`] が
/// `t1_secs <= 0.0` を同様に拒否しているのと対称にするため、ゼロ時間を
/// 有効な比率 `0.0` として受理しない。
pub fn n_log_n_ratio(n: usize, t: Duration) -> Result<f64, HnswBuildBenchError> {
    if n <= 1 || t.is_zero() {
        return Err(HnswBuildBenchError::InsufficientSamples);
    }
    let denom = n as f64 * (n as f64).ln();
    Ok(t.as_secs_f64() / denom)
}

/// [`scaling_exponent`] の判定結果を、`n log n`（指数 ~1.0〜1.3 程度を許容域と
/// する）と二乗相当（`SuperLinear`。しきい値超過）へ分類する。しきい値は
/// spec 由来の合否基準ではなく、本ベンチが「情報提供専用（合否閾値なし）」と
/// 位置付けつつも明示的に警告を出すための実装上の目安値である
/// （計画: 「実効指数が 2.0 に近い場合は明示的に `SuperLinear` と表示する」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalingClass {
    /// `exponent < threshold`。N log N 相当（またはそれ以下）とみなせる。
    NearNLogN,
    /// `exponent >= threshold`。二乗相当に近い、明示的な警告対象。
    SuperLinear,
}

/// 既定のしきい値（実効指数 1.6。N log N の理論的傾き〔`rows` の増加倍率に対し
/// `log` 増分だけ上乗せされる緩やかな伸び〕と二乗相当〔傾き 2.0〕の中間に
/// 余裕を持たせて置く目安値。閾値そのものは spec 由来ではない実装上の目安の
/// ため、合否判定には使わず表示の分類にのみ用いる）。
pub const DEFAULT_SUPER_LINEAR_THRESHOLD: f64 = 1.6;

pub fn classify_scaling(exponent: f64, threshold: f64) -> ScalingClass {
    if exponent >= threshold {
        ScalingClass::SuperLinear
    } else {
        ScalingClass::NearNLogN
    }
}

/// 1 規模点の実測結果を人間可読な形へ整形する（stdout 出力用。本ベンチは
/// spec 由来の合否閾値を持たない情報提供専用のため、実測値をそのまま出力して
/// よい。`.claude/rules/spec-confidentiality.md` のオーナー判断範囲）。
pub fn render_line(rows: usize, dim: usize, median: Duration, ratio: f64) -> String {
    format!(
        "hnsw_build: rows={rows} dim={dim} median_ms={:.3} t_over_n_log_n={ratio:.6e}",
        median.as_secs_f64() * 1e3
    )
}
