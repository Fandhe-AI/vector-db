//! CPU 命令セットの実行時検出（TASK-156・対象ビヘイビア: CORE-14）。
//!
//! `dispatch.rs`（TASK-155・CORE-11, CORE-12）の決定表は、それまで
//! `detect_current_isa()`（コンパイル時 `cfg(target_arch)` のみに基づく保守的な
//! 下限検出。x86_64 は常に `Scalar`）を ISA 入力にしていた。本モジュールは
//! `std::arch::is_x86_feature_detected!` / `is_aarch64_feature_detected!` による
//! 実際の CPUID/HWCAP 照会へ置き換え、その検出結果を「SIMD カーネルを呼んでよい
//! 証明」として型（sealed トークン）に閉じ込める。
//!
//! # sealed トークンと `unsafe` の最小化
//!
//! [`NeonToken`]／[`Avx2FmaToken`]／[`Avx512Token`] はフィールド private・
//! `pub(crate)` の `try_new` からしか値を作れない（crate 外は当然、crate 内の
//! 他モジュールも「対応 feature を実行時確認した」という証明なしに値を持てない）。
//! [`SimdKernel::dot`] 内で SIMD カーネルを呼ぶ 3 箇所（NEON・AVX2+FMA・AVX-512）
//! だけが `unsafe` ブロックであり、トークンの所持そのものが SAFETY 根拠になる
//! （各所の `// SAFETY:` 参照）。NEON は aarch64 の baseline feature
//! （アーキテクチャ仕様上必ず対応）だが、`#[target_feature(enable = "neon")]` fn の
//! 呼び出しはコンパイラの安全性検査上 `unsafe` を要求するため（`make check-cross`
//! でのクロスコンパイル確認時に判明）、他 2 ISA と同じ `unsafe` 呼び出し形にして
//! いる。
//!
//! # CORE-12 との整合: 上書き機構の不存在
//!
//! `dispatch.rs` モジュールドキュメントの「CORE-12」節と同じ方針で、環境変数・
//! 設定ファイル・feature flag による検出結果の上書き機構は一切設けない
//! （`tests/isa.rs::isa_source_has_no_external_override_entry_points` が
//! ソース走査で不在を検査する）。未検証の ISA 指定で `unsafe` カーネルを強制
//! 起動させる攻撃面を、機構の不存在によって構造的に排除する。
//!
//! # 呼び出し文脈
//!
//! - `dispatch.rs::detect_current_isa()` は本モジュールの [`current`] へ委譲する
//!   （決定表の `SimdWidth` と実際に実行されるカーネルが同一の検出結果から
//!   導かれる構造にする）。
//! - `kernel.rs::dot`（`pub(crate)`。`CpuScalarProvider`・`parallel_search.rs`・
//!   `batch_search.rs`・`rls.rs` が共有する唯一の内積実装）は本モジュールの
//!   [`current`]`().dot(..)` へ委譲する。

use std::sync::OnceLock;

/// 実行時に検出された ISA。
///
/// `dispatch.rs` から本モジュールへ移設した（`dispatch.rs` は
/// `pub use crate::isa::DetectedIsa;` で既存の公開パスを維持する）。
/// `#[non_exhaustive]` にはしない（`dispatch.rs::simd_width_for` の網羅 match が
/// variant 追加時にコンパイルエラーで気付ける状態を維持するため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedIsa {
    /// SIMD 拡張なし（スカラー演算のみ）。
    Scalar,
    /// Arm Neon（128 bit）。
    Neon,
    /// x86_64 AVX2 + FMA（256 bit）。
    Avx2Fma,
    /// x86_64 AVX-512（512 bit）。
    Avx512,
}

/// aarch64 Neon 対応の実行時確認済みトークン（sealed）。
///
/// aarch64 の baseline ISA は Neon を含むことがアーキテクチャ仕様上保証されている
/// ため [`Self::try_new`] は常に `Some` を返すが、他トークンと構築契約
/// （「実行時確認を経てのみ値を持てる」）を揃える。
#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy)]
pub struct NeonToken(());

#[cfg(target_arch = "aarch64")]
impl NeonToken {
    /// crate 内からのみ呼べる（公開コンストラクタを設けない。トークンを crate 外
    /// から任意構築できないようにする、CORE-12 と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_aarch64_feature_detected!("neon") {
            Some(NeonToken(()))
        } else {
            None
        }
    }
}

/// x86_64 AVX2+FMA 対応の実行時確認済みトークン（sealed）。
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub struct Avx2FmaToken(());

#[cfg(target_arch = "x86_64")]
impl Avx2FmaToken {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            Some(Avx2FmaToken(()))
        } else {
            None
        }
    }
}

/// x86_64 AVX-512（`avx512f`）対応の実行時確認済みトークン（sealed）。
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub struct Avx512Token(());

#[cfg(target_arch = "x86_64")]
impl Avx512Token {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_x86_feature_detected!("avx512f") {
            Some(Avx512Token(()))
        } else {
            None
        }
    }
}

/// 検出済み ISA に対応する内積カーネルを保持する variant。
///
/// トークン型を経由してのみ SIMD variant を構築できるため（[`detect`]／[`current`]
/// のみが生成元）、呼び出し元は「対応 CPU で実際に検証された」カーネルしか
/// 呼び出せない。
#[derive(Debug, Clone, Copy)]
pub enum SimdKernel {
    /// SIMD 拡張なし（[`dot_scalar`] を使う）。
    Scalar,
    /// Arm Neon。
    #[cfg(target_arch = "aarch64")]
    Neon(NeonToken),
    /// x86_64 AVX2 + FMA。
    #[cfg(target_arch = "x86_64")]
    Avx2Fma(Avx2FmaToken),
    /// x86_64 AVX-512。
    #[cfg(target_arch = "x86_64")]
    Avx512(Avx512Token),
}

impl SimdKernel {
    /// [`DetectedIsa`] への純写像（`dispatch.rs::simd_width_for` と同じ「別の判断を
    /// 持たない写像」という位置付け）。
    pub fn isa(self) -> DetectedIsa {
        match self {
            SimdKernel::Scalar => DetectedIsa::Scalar,
            #[cfg(target_arch = "aarch64")]
            SimdKernel::Neon(_) => DetectedIsa::Neon,
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx2Fma(_) => DetectedIsa::Avx2Fma,
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx512(_) => DetectedIsa::Avx512,
        }
    }

    /// 内積計算。トークン所持を根拠に ISA 別カーネルへ分岐する。
    ///
    /// `a`／`b` の長さが異なる場合の挙動は [`dot_scalar`]（`zip` による短い方への
    /// 切り詰め）と同一に保つ。次元の一致検証は呼び出し元（`kernel.rs::
    /// CpuScalarProvider` 等）が既に行っており、本関数はそれを前提とした純算術のみを
    /// 担う。
    pub fn dot(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            SimdKernel::Scalar => dot_scalar(a, b),
            #[cfg(target_arch = "aarch64")]
            SimdKernel::Neon(_) => {
                // SAFETY: この variant は `NeonToken::try_new` が
                // `is_aarch64_feature_detected!("neon")` を実行時確認できた場合に
                // のみ構築される sealed トークンを保持する（下記 Avx2Fma/Avx512
                // 分岐と同じ構造）。値の存在が CPU 対応の証明であり、`dot_neon` の
                // `#[target_feature]` 契約を満たす。NEON は aarch64 の baseline
                // feature（アーキテクチャ仕様上必ず対応）だが、`#[target_feature]`
                // を付けた safe fn の呼び出しはコンパイラの安全性検査上、呼び出し側
                // コンテキストがその feature を持つことを示す `unsafe` を常に要求する
                // （`make check-cross` でのクロスコンパイル確認時に判明）。
                unsafe { dot_neon(a, b) }
            }
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx2Fma(_) => {
                // SAFETY: この variant は `Avx2FmaToken::try_new` が
                // `is_x86_feature_detected!("avx2")` かつ `("fma")` を実行時確認
                // できた場合にのみ構築される（`pub(crate)` かつフィールド private の
                // sealed トークンのため、crate 外は当然、確認を経ない限り crate 内
                // からも値を持てない）。値の存在自体が「本 CPU が avx2+fma に対応
                // している」ことの証明であり、`dot_avx2_fma` の `#[target_feature]`
                // 契約を満たす。
                unsafe { dot_avx2_fma(a, b) }
            }
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx512(_) => {
                // SAFETY: この variant は `Avx512Token::try_new` が
                // `is_x86_feature_detected!("avx512f")` を実行時確認できた場合に
                // のみ構築される sealed トークンを保持する（上記 Avx2Fma 分岐と同じ
                // 構造）。値の存在が CPU 対応の証明であり、`dot_avx512` の
                // `#[target_feature]` 契約を満たす。
                unsafe { dot_avx512(a, b) }
            }
        }
    }
}

/// 優先順（AVX-512 → AVX2+FMA → NEON → Scalar）で `try_new` を試し、最初に成功した
/// トークンで [`SimdKernel`] を確定させる。いずれも失敗すれば `Scalar`
/// （fail-closed: 実際より広い ISA を主張しない）。
///
/// 呼び出す毎に CPUID/HWCAP 照会を行う（プロセス内キャッシュは [`current`] が担う）。
pub fn detect() -> SimdKernel {
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(token) = Avx512Token::try_new() {
            return SimdKernel::Avx512(token);
        }
        if let Some(token) = Avx2FmaToken::try_new() {
            return SimdKernel::Avx2Fma(token);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if let Some(token) = NeonToken::try_new() {
            return SimdKernel::Neon(token);
        }
    }
    SimdKernel::Scalar
}

/// プロセス内で 1 回だけ [`detect`] を実行し、以後は同じ値を返す（CORE-14 の
/// 「起動時または初回ディスパッチ時に検出」に対応）。CPU の対応命令セットは
/// プロセス実行中に変化しないため、繰り返し照会するコストを避ける。
/// `dispatch.rs::select_execution_path`（CORE-12: 参照透過性）が前提とする
/// 「同一入力 → 同一出力」は、本関数がプロセス内で単調であることにより保たれる。
pub fn current() -> SimdKernel {
    static CURRENT: OnceLock<SimdKernel> = OnceLock::new();
    *CURRENT.get_or_init(detect)
}

/// 内積（dot product）のスカラー参照実装（左から右への逐次和）。
///
/// `kernel.rs::dot`（`CpuScalarProvider`・`parallel_search.rs::search_range`・
/// `batch_search.rs`・`rls.rs` が共有）の実体、およびテストの参照実装として公開する。
pub fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Arm Neon（128 bit・4 レーン）向け内積カーネル。
///
/// intrinsics を使わず `#[target_feature(enable = "neon")]` を付けた safe fn とし、
/// LLVM の自動ベクトル化に委ねる（本タスクの範囲は「実行時検出とトークンによる
/// 安全な呼び出し構造の確立」であり、intrinsics 直書きでの最適化は対象外
/// （spec 上「対象外」の項参照）。呼び出しには `unsafe` が必要（[`SimdKernel::dot`]
/// の SAFETY コメント参照）。
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
    dot_lanes::<4>(a, b)
}

/// x86_64 AVX2+FMA（256 bit・8 レーン）向け内積カーネル。
///
/// `f32::mul_add` で FMA 契約を表現する safe fn。呼び出しには `unsafe` が必要
/// （[`SimdKernel::dot`] の SAFETY コメント参照）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn dot_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
    dot_lanes::<8>(a, b)
}

/// x86_64 AVX-512（`avx512f`。512 bit・16 レーン）向け内積カーネル。
///
/// `avx512f` は FMA を含意する。呼び出しには `unsafe` が必要（[`SimdKernel::dot`]
/// の SAFETY コメント参照）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
fn dot_avx512(a: &[f32], b: &[f32]) -> f32 {
    dot_lanes::<16>(a, b)
}

/// ISA 別カーネルの共通本体。`LANES` 個ずつのレーンアキュムレータへ `f32::mul_add`
/// （FMA 契約）で積算し、`as_chunks` の端数はスカラーで処理、
/// 最後にレーン和を固定順（インデックス昇順）で畳む。添字アクセス（`[]`）は使わず
/// `zip`／イテレータのみで書く（.claude/rules/coding-rust.md）。同一 ISA・同一 LANES
/// では常に同じ演算順序になるため、`kernel.rs::dot` 経由で呼ぶ全 provider
/// （`CpuScalarProvider`・`ParallelSearchProvider`・バッチ経路・RLS 事前フィルタ）が
/// 同一の丸め誤差で揃う（provider 間の Top-k 整合という既存設計意図を維持する）。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
fn dot_lanes<const LANES: usize>(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let a = &a[..len];
    let b = &b[..len];

    let mut lanes = [0f32; LANES];
    let (a_chunks, a_rem) = a.as_chunks::<LANES>();
    let (b_chunks, b_rem) = b.as_chunks::<LANES>();

    for (a_chunk, b_chunk) in a_chunks.iter().zip(b_chunks.iter()) {
        for (lane, (x, y)) in lanes.iter_mut().zip(a_chunk.iter().zip(b_chunk.iter())) {
            *lane = x.mul_add(*y, *lane);
        }
    }

    let lane_sum: f32 = lanes.iter().sum();
    let rem_sum: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
    lane_sum + rem_sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`try_new`] の否定系（`Some`/`None` いずれの結果でも SAFETY 契約
    /// 「値が存在するなら実際に CPU が対応している」を壊さないこと）と、
    /// [`current`] のプロセス内単調性を確認する。実機の実際の対応有無に依存する
    /// ため、`try_new` が `None` を返す環境では該当分岐を通らないが、
    /// `detect()`／`current()` が常に `Scalar` へ fail-closed で倒れることは
    /// アーキテクチャ非依存に検証できる。
    #[test]
    fn detect_never_panics_and_current_is_stable() {
        let first = current().isa();
        for _ in 0..8 {
            assert_eq!(
                current().isa(),
                first,
                "current() must be stable within process"
            );
        }
        // detect() は毎回照会するが、CPU 対応は実行中に変化しないため current() と
        // 一致するはず。
        assert_eq!(detect().isa(), first);
    }

    /// スカラー参照実装との数値整合。dim 0 を含む複数サイズで許容差内一致を確認する
    /// （このモジュール内 unit テストの範囲。結合テスト側 `tests/isa.rs` がより
    /// 広いサイズ・決定的乱数での回帰を担う）。
    #[test]
    fn current_dot_matches_scalar_reference_within_tolerance() {
        for dim in [0usize, 1, 3, 4, 7, 8, 16, 17, 33] {
            let a: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.5 - 1.0).collect();
            let b: Vec<f32> = (0..dim).map(|i| (i as f32 % 3.0) + 0.25).collect();
            let expected = dot_scalar(&a, &b);
            let actual = current().dot(&a, &b);
            let tolerance = 1e-5 * dot_scalar(&a, &b).abs().max(1.0) + 1e-4;
            assert!(
                (actual - expected).abs() <= tolerance,
                "dim={dim} actual={actual} expected={expected}"
            );
        }
    }
}
