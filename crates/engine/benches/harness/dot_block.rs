//! `isa.rs::SimdKernel::dot_block4`（Issue #510・#511。`docs/design/
//! dot-kernel-row-block.md`）の前後比較・採否記録（Issue #512）が使う
//! 時間非依存ヘルパ。`benches/dot_kernel_bench.rs`（層 A・単一バイナリ内
//! block4 A/B。時間依存・`make ci` 対象外）と `tests/dot_block_accept.rs`
//! （`#[path]` で本モジュールを取り込む時間非依存の回帰。`make ci` 対象）の
//! 双方から共有される（`harness/dot_kernel.rs` 冒頭コメントと同じ取り込み方針）。
//!
//! `harness::dot_kernel` の `speedup_ratio`／`classify_change` はそのまま
//! 再利用し、本モジュールは block4 A/B 固有の env パース・ビット同一検証・
//! run 間集計（min-of-N・参照区間の相対ノイズ帯）のみを追加する
//! （`docs/design/benchmark-judgement-policy.md` §3・§4 の計測プロトコル要件）。

use std::fmt;
use std::time::Duration;

/// 層 A（`dot_kernel_bench` の block4 A/B）の対象次元。`docs/design/
/// dot-kernel-row-block.md` の主要ワークロード次元（128・768）に合わせる
/// （`dot_kernel_bench.rs::DIMS` の全次元を A/B すると計測時間が既存ベンチの
/// 数倍になるため、代表 2 点に絞る）。
pub const BLOCK_AB_DIMS: [usize; 2] = [128, 768];

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq)]
pub enum DotBlockError {
    /// `BENCH_DOT_KERNEL_BLOCK_AB` env の値が許可値（未設定／`"0"`／`"1"`）
    /// のいずれでもない（fail-closed。非 UTF-8 もこの扱い）。
    InvalidEnvValue,
    /// `dot_block4` の要素が 1 行版 `dot` とビット同一でなかった。
    BitMismatch {
        dim: usize,
        block_idx: usize,
        lane: usize,
        actual_bits: u32,
        expected_bits: u32,
    },
    /// 集計対象のサンプル列が空。
    EmptySamples,
    /// 相対ノイズ帯の算出に使う分母（min）が非正、またはサンプルに非有限値を含む。
    NonPositiveOrNonFiniteBaseline,
}

impl fmt::Display for DotBlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DotBlockError::InvalidEnvValue => write!(
                f,
                "BENCH_DOT_KERNEL_BLOCK_AB must be unset, \"0\", or \"1\""
            ),
            DotBlockError::BitMismatch {
                dim,
                block_idx,
                lane,
                actual_bits,
                expected_bits,
            } => write!(
                f,
                "dot_block4 bit mismatch at dim={dim} block_idx={block_idx} lane={lane}: \
                 actual_bits={actual_bits:#010x} expected_bits={expected_bits:#010x}"
            ),
            DotBlockError::EmptySamples => write!(f, "cannot summarize an empty sample set"),
            DotBlockError::NonPositiveOrNonFiniteBaseline => write!(
                f,
                "relative_band requires a positive, finite minimum sample"
            ),
        }
    }
}

impl std::error::Error for DotBlockError {}

/// 層 A block4 A/B の有効・無効。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockAbMode {
    Off,
    On,
}

/// `BENCH_DOT_KERNEL_BLOCK_AB` env の文字列値を解釈する（fail-closed。
/// `dot_kernel_bench.rs::main` が `std::env::var_os` → `to_str`（非 UTF-8 は
/// ここへ渡す前に拒否）した結果を渡す）。未設定は `raw = None` として呼び出す。
pub fn parse_block_ab_env(raw: Option<&str>) -> Result<BlockAbMode, DotBlockError> {
    match raw {
        None => Ok(BlockAbMode::Off),
        Some("0") => Ok(BlockAbMode::Off),
        Some("1") => Ok(BlockAbMode::On),
        Some(_) => Err(DotBlockError::InvalidEnvValue),
    }
}

/// `dot_block4` の 1 要素が 1 行版 `dot` の結果とビット同一か検証する
/// （`docs/design/dot-kernel-row-block.md` §4 の契約 `dot_block4(rows,
/// q)[i].to_bits() == dot(rows[i], q).to_bits()` を計測前に再確認する
/// fail-closed ガード。`isa.rs` 側の単体テストと独立に、本ベンチが使う
/// コーパス・次元の組み合わせでも崩れていないことを実測前に固定する）。
pub fn check_block_bit_identical(
    dim: usize,
    block_idx: usize,
    lane: usize,
    actual: f32,
    expected: f32,
) -> Result<(), DotBlockError> {
    if actual.to_bits() != expected.to_bits() {
        return Err(DotBlockError::BitMismatch {
            dim,
            block_idx,
            lane,
            actual_bits: actual.to_bits(),
            expected_bits: expected.to_bits(),
        });
    }
    Ok(())
}

/// run 間（N ≥ 5 プロセス起動）の最小値（min-of-N。`docs/design/
/// benchmark-judgement-policy.md` §3 の必須項目）。
pub fn min_of_samples(samples: &[Duration]) -> Result<Duration, DotBlockError> {
    samples
        .iter()
        .copied()
        .min()
        .ok_or(DotBlockError::EmptySamples)
}

/// 相対ノイズ帯 `(max - min) / min`（`policy.md` §4「変更を含まない参照区間の
/// run-to-run 差分」の算出式）。`samples` は同一プロセス内の複数反復値、または
/// run 間（プロセス起動ごと）の代表値列のどちらにも使える純関数。
///
/// 空・非有限値混入・`min <= 0.0`（`Duration` は非負のため理論上は
/// `min == 0.0` のみだが、将来の呼び出し元がスケール後の値を渡す可能性に
/// 備え `<=` で fail-closed に倒す）はいずれも拒否する。
pub fn relative_band(samples_secs: &[f64]) -> Result<f64, DotBlockError> {
    if samples_secs.is_empty() {
        return Err(DotBlockError::EmptySamples);
    }
    if samples_secs.iter().any(|v| !v.is_finite()) {
        return Err(DotBlockError::NonPositiveOrNonFiniteBaseline);
    }
    let min = samples_secs.iter().copied().fold(f64::INFINITY, f64::min);
    let max = samples_secs
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if min <= 0.0 {
        return Err(DotBlockError::NonPositiveOrNonFiniteBaseline);
    }
    Ok((max - min) / min)
}

/// block4 A/B の対象区間（1 working_set × 1 dim）の実測行を整形する
/// （stdout 出力用。接頭辞 `dot_kernel: block4_ab` は既存 `label=current` 行
/// （`dot_kernel: label=current ...`）と衝突しない——`harness/chip.rs::
/// parse_dot_kernel_line` は `dot_kernel: label=` 接頭辞のみを拾うため、
/// `bench-chip` の `dot_kernel` 子プロセス出力の既存解釈を壊さない）。
pub fn render_block_ab_line(
    working_set_label: &str,
    dim: usize,
    single_row_median: Duration,
    block4_median: Duration,
    ratio: f64,
) -> String {
    format!(
        "dot_kernel: block4_ab working_set={working_set_label} dim={dim} \
         single_row_median_ms={:.3} block4_median_ms={:.3} ratio={ratio:.3}",
        single_row_median.as_secs_f64() * 1e3,
        block4_median.as_secs_f64() * 1e3,
    )
}

/// 参照区間（1 行版 `dot` の既存 `label=current` 計測。変更を含まない区間）の
/// run 内相対ノイズ帯を整形する（`policy.md` §4 の「参照区間帯」併記要件）。
pub fn render_block_ab_reference_line(working_set_label: &str, dim: usize, band: f64) -> String {
    format!("dot_kernel: block4_ab_ref working_set={working_set_label} dim={dim} band={band:.4}")
}
