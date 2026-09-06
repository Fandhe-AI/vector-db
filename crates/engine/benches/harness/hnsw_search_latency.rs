//! HNSW 探索（`engine::hnsw::HnswIndex::search`／`search_masked`）のレイテンシに
//! ついて、受理判定後 prefetch（Issue #490・PR #574）導入前後（before `4d2bd23`／
//! after `eabff3a`）を比較する `benches/hnsw_search_bench.rs` が使う時間非依存
//! ヘルパ（Issue #491）。
//!
//! `harness/hnsw_compare.rs`（Issue #402 系・usearch 対照）・`harness/dot_kernel.rs`
//! （Issue #365）と同じく、時間依存の実測本体（`hnsw_search_bench.rs`。`make ci`
//! 対象外）と時間非依存のロジック（本ファイル。`tests/hnsw_search_latency_accept.rs`
//! から `#[path]` で取り込み `make ci` 対象）を分離する。
//!
//! 本モジュールは 1 プロセス = 1 規模点（`rows` × `dim` × マスク有無）の入力生成・
//! 出力整形のみを担い、before/after の比較そのもの（交互実行・比率算出・採否判定）
//! は運用者・呼び出し元シェルスクリプトの責務とする（Issue #313 の教訓＝
//! 複数規模点の同一プロセス内逐次比較は不可。`docs/design/
//! benchmark-judgement-policy.md` 参照）。
//!
//! # 参照区間（ノイズ帯算出）
//!
//! 変更（prefetch）を含まない区間として、同一プロセス内で同じ正規化コーパス・
//! クエリに対する brute-force Top-k（`engine::kernel::CpuScalarProvider`）の
//! 所要時間分布を使う。[`reference_band`] はその分布の `(max - min) / min` を
//! 百分率で返し、呼び出し元が計測対象（HNSW 探索）の比率変化と比較して
//! 「実測帯を超えたか」を判定する材料にする（`.claude/rules/
//! spec-confidentiality.md`: 数値基準・実測値はオーナー判断で公開可）。
//!
//! # 暗号用途禁止
//!
//! [`super::rng::DeterministicRng`] を経由するため非暗号 PRNG である。
//! ベンチ入力生成専用。

use std::fmt;
use std::time::Duration;

use engine::hnsw::NodeMask;

use super::rng::DeterministicRng;

/// [`parse_rows`] が許容する行数の上限（DoS 防止・上限検証。
/// `harness::hnsw_compare::MAX_ROWS_GUARD` と同一方針の固定上限）。
pub const MAX_ROWS_GUARD: usize = 200_000;

/// [`parse_dim`] が許容する次元の上限（`harness::bench_engine::MAX_BENCH_DIM`
/// と同値。dim=768 が本 Issue の計測規模点の 1 つ）。
pub const MAX_DIM_GUARD: usize = 4_096;

/// [`parse_queries`] が許容するクエリ数の上限（DoS 防止・上限検証）。
pub const MAX_QUERIES_GUARD: usize = 2_000;

/// [`generate_corpus`] が許容する総要素数（`rows * dim`）の安全上限。
/// `100_000 行 × dim 768` ≒ 7,680 万要素（約 293 MiB・f32 換算）を受理しつつ、
/// 無制限確保は許さない固定上限（`harness::dot_kernel::MAX_CORPUS_ELEMENTS_GUARD`
/// の 2 倍。本 Issue の最大規模点 100k×768 に対し約 1.68 倍の余裕を持たせた値。
/// coding-rust.md「無制限 `Vec::with_capacity` 禁止」）。
pub const MAX_CORPUS_ELEMENTS_GUARD: usize = 128 * 1024 * 1024;

/// 既定の行数（`BENCH_HNSW_SEARCH_ROWS` 未設定・不正値時のフォールバック）。
pub const DEFAULT_ROWS: usize = 10_000;

/// 既定の次元（`BENCH_HNSW_SEARCH_DIM` 未設定・不正値時のフォールバック）。
pub const DEFAULT_DIM: usize = 128;

/// 既定のクエリ数（`BENCH_HNSW_SEARCH_QUERIES` 未設定・不正値時のフォールバック）。
pub const DEFAULT_QUERIES: usize = 200;

/// 既定の `ef_search`（`engine::hnsw::HnswParams::default().ef_search` と同値）。
pub const DEFAULT_EF: usize = 64;

/// 既定の `k`。
pub const DEFAULT_K: usize = 10;

/// マスク可視率の許容範囲（`1..=99`。0%・100% は「マスクなし」「全可視」相当で
/// 本 Issue が比較したい「一部可視」条件を代表しないため受理しない）。
pub const MASK_PERCENT_RANGE: std::ops::RangeInclusive<u8> = 1..=99;

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq)]
pub enum HnswSearchLatencyError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された。
    RefusedUnderGitHubActions,
    /// `rows * dim` が [`MAX_CORPUS_ELEMENTS_GUARD`] を超過した。
    CorpusTooLarge,
    /// [`reference_band`] に渡した `min` が 0（0 除算・NaN/inf 混入の回避）。
    ZeroMinDuration,
}

impl fmt::Display for HnswSearchLatencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HnswSearchLatencyError::RefusedUnderGitHubActions => write!(
                f,
                "hnsw_search_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); \
                 this bench is manual-only and not wired into any workflow"
            ),
            HnswSearchLatencyError::CorpusTooLarge => write!(
                f,
                "rows * dim exceeds MAX_CORPUS_ELEMENTS_GUARD ({MAX_CORPUS_ELEMENTS_GUARD})"
            ),
            HnswSearchLatencyError::ZeroMinDuration => write!(
                f,
                "reference_band refuses min == 0 (would divide by zero / emit NaN)"
            ),
        }
    }
}

impl std::error::Error for HnswSearchLatencyError {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`harness::hnsw_compare::
/// refuse_under_github_actions` と同一パターン）。
pub fn refuse_under_github_actions(
    under_github_actions: bool,
) -> Result<(), HnswSearchLatencyError> {
    if under_github_actions {
        return Err(HnswSearchLatencyError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// `BENCH_HNSW_SEARCH_ROWS` の生文字列を読み、`1..=MAX_ROWS_GUARD` の範囲で
/// 検証する。未設定・不正値・範囲外は [`DEFAULT_ROWS`] へフォールバックする
/// （時間依存ベンチの入力は既定値へ倒す既存方針。`hnsw_compare.rs::parse_rows`
/// と同一方針）。
pub fn parse_rows(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=MAX_ROWS_GUARD).contains(&n))
        .unwrap_or(DEFAULT_ROWS)
}

/// `BENCH_HNSW_SEARCH_DIM` の生文字列を読み、`1..=MAX_DIM_GUARD` の範囲で
/// 検証する。未設定・不正値・範囲外は [`DEFAULT_DIM`] へフォールバックする。
pub fn parse_dim(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=MAX_DIM_GUARD).contains(&n))
        .unwrap_or(DEFAULT_DIM)
}

/// `BENCH_HNSW_SEARCH_QUERIES` の生文字列を読み、`1..=MAX_QUERIES_GUARD` の
/// 範囲で検証する。未設定・不正値・範囲外は [`DEFAULT_QUERIES`] へ
/// フォールバックする。
pub fn parse_queries(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=MAX_QUERIES_GUARD).contains(&n))
        .unwrap_or(DEFAULT_QUERIES)
}

/// `BENCH_HNSW_SEARCH_EF` の生文字列を読み、`1..=engine::hnsw::MAX_EF` の
/// 範囲で検証する。未設定・不正値・範囲外は [`DEFAULT_EF`] へフォール
/// バックする。
pub fn parse_ef(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=engine::hnsw::MAX_EF).contains(&n))
        .unwrap_or(DEFAULT_EF)
}

/// `BENCH_HNSW_SEARCH_K` の生文字列を読み、`1..=ef` の範囲で検証する
/// （`k > ef` は `HnswIndex::search_masked` が受理しても実務上の意味が薄い
/// ため、ここで `ef` を上限にクランプする）。未設定・不正値・範囲外は
/// `min(DEFAULT_K, ef)` へフォールバックする。
pub fn parse_k(raw: Option<&str>, ef: usize) -> usize {
    let ef = ef.max(1);
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=ef).contains(&n))
        .unwrap_or_else(|| DEFAULT_K.min(ef))
}

/// マスク指定（`BENCH_HNSW_SEARCH_MASK`）を表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskSpec {
    /// マスクなし（`HnswIndex::search`。全ノードが探索経路として使える）。
    None,
    /// 可視率（`1..=99` %）で受理ノードを間引く（`HnswIndex::search_masked`。
    /// RLS 事前フィルタ統合〔Issue #409〕の `Subset` 形状を模す）。
    VisiblePercent(u8),
}

impl MaskSpec {
    /// 出力用のトークン（`none` または `<percent>%`）。
    pub fn token(self) -> String {
        match self {
            MaskSpec::None => "none".to_string(),
            MaskSpec::VisiblePercent(p) => format!("{p}%"),
        }
    }
}

/// `BENCH_HNSW_SEARCH_MASK` の生文字列を読み、[`MaskSpec`] を解決する。
/// `"none"`・未設定・空文字列は [`MaskSpec::None`]。`1..=99` の整数
/// （[`MASK_PERCENT_RANGE`]）は [`MaskSpec::VisiblePercent`]。それ以外
/// （`0`・`100`・非数値等）は既定 [`MaskSpec::None`] へフォールバックする
/// （時間依存ベンチ入力を fail-closed に拒否せず既定へ倒す既存方針。
/// `hnsw_compare.rs::parse_rows` 系と同一方針）。
pub fn parse_mask(raw: Option<&str>) -> MaskSpec {
    match raw.map(str::trim) {
        None | Some("") | Some("none") => MaskSpec::None,
        Some(s) => s
            .parse::<u8>()
            .ok()
            .filter(|p| MASK_PERCENT_RANGE.contains(p))
            .map(MaskSpec::VisiblePercent)
            .unwrap_or(MaskSpec::None),
    }
}

/// `rows * dim` 要素の row-major フラットコーパスを決定的に生成する。
/// [`MAX_CORPUS_ELEMENTS_GUARD`] を超える要素数は `Err`
/// （coding-rust.md「無制限確保禁止」。乗算は `checked_mul` を使う）。
pub fn generate_corpus(
    seed: u64,
    dim: usize,
    rows: usize,
) -> Result<Vec<f32>, HnswSearchLatencyError> {
    let total_elements = rows
        .checked_mul(dim)
        .filter(|&total| total <= MAX_CORPUS_ELEMENTS_GUARD)
        .ok_or(HnswSearchLatencyError::CorpusTooLarge)?;
    let mut rng = DeterministicRng::new(seed);
    let mut out = Vec::with_capacity(total_elements);
    for _ in 0..rows {
        out.extend_from_slice(&rng.next_vector(dim));
    }
    Ok(out)
}

/// [`generate_corpus`] と系列を分離したクエリ生成（`harness::dot_kernel::
/// generate_query` と同じくシードへ固定オフセットを加える）。単一クエリ
/// ベクトルを 1 本返す（呼び出し元が `queries` 本のクエリをそれぞれ別の
/// `query_seed` で生成する）。
pub fn generate_query(seed: u64, dim: usize) -> Vec<f32> {
    let mut rng = DeterministicRng::new(seed.wrapping_add(0x1357_9bdf_1357_9bdf));
    rng.next_vector(dim)
}

/// `len` ノード分のマスクを、指定した可視率（`percent`。`1..=99`）で決定的に
/// 生成する（`DeterministicRng` によるベルヌーイ試行。1 ノードにつき 1 回
/// `next_f32()` を消費するため、同一シードなら常に同一のビット列になる）。
/// `MaskSpec::None` を渡した呼び出し元は本関数を呼ばず `search`（マスクなし）
/// を使う契約とする。
pub fn generate_mask(seed: u64, len: usize, percent: u8) -> NodeMask {
    let mut rng = DeterministicRng::new(seed);
    let threshold = f32::from(percent) / 100.0;
    let mut mask = NodeMask::new(len);
    for node in 0..len {
        if rng.next_f32() < threshold {
            // `len <= MAX_ROWS_GUARD`（200,000）であり `u32` へ収まることを
            // 呼び出し元（`parse_rows`）が保証する。
            if let Ok(node_u32) = u32::try_from(node) {
                mask.set(node_u32);
            }
        }
    }
    mask
}

/// 変更を含まない参照区間（brute-force Top-k）の所要時間分布から、
/// `(max - min) / min` を百分率で返す（ノイズ帯。`min == 0` は
/// [`HnswSearchLatencyError::ZeroMinDuration`] として拒否する）。
pub fn reference_band(min: Duration, max: Duration) -> Result<f64, HnswSearchLatencyError> {
    let min_secs = min.as_secs_f64();
    if min_secs <= 0.0 {
        return Err(HnswSearchLatencyError::ZeroMinDuration);
    }
    Ok((max.as_secs_f64() - min_secs) / min_secs * 100.0)
}

/// 実行条件のヘッダ 1 行分の出力整形。
#[allow(clippy::too_many_arguments)]
pub fn render_header_line(
    rows: usize,
    dim: usize,
    mask: MaskSpec,
    ef: usize,
    k: usize,
    queries: usize,
    dedicated: bool,
    commit: &str,
    build_ms: f64,
) -> String {
    format!(
        "hnsw_search_bench: rows={rows} dim={dim} mask={} ef={ef} k={k} queries={queries} \
         corpus=l2_normalized dedicated={dedicated} commit={commit} build_ms={build_ms:.3}",
        mask.token()
    )
}

/// HNSW 探索本体 1 行分の出力整形。
pub fn render_target_line(min_us: f64, median_us: f64, p95_us: f64, samples: usize) -> String {
    format!(
        "hnsw_search_bench: target=hnsw_search min_us={min_us:.3} median_us={median_us:.3} \
         p95_us={p95_us:.3} samples={samples}"
    )
}

/// 参照区間（brute-force）1 行分の出力整形。
pub fn render_reference_line(
    min_us: f64,
    median_us: f64,
    max_us: f64,
    reference_band_pct: f64,
) -> String {
    format!(
        "hnsw_search_bench: reference=brute_force min_us={min_us:.3} median_us={median_us:.3} \
         max_us={max_us:.3} reference_band_pct={reference_band_pct:.3}"
    )
}

/// マスク適用時の「返却件数が `k` 未満」だったクエリ数（informational。
/// エラーにはしない）1 行分の出力整形。
pub fn render_masked_short_line(count: usize) -> String {
    format!("hnsw_search_bench: masked_short_queries={count}")
}
