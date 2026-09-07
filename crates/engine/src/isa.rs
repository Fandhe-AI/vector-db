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
//!
//! # tail 処理方式の切り替え（TASK-156・CORE-14。Issue #528）
//!
//! [`dot_lanes`] の端数（`as_chunks` で割り切れない末尾要素）は、現行のスカラー
//! 逐次和（`dot_with_scalar_tail` 相当）と、零埋め固定長バッファへ詰めてから
//! レーン数ぶんの積和を取る「分岐なし tail」（`dot_with_padded_tail` 相当）の
//! 2 方式を `const PADDED_TAIL: bool` ジェネリックで切り替えられる構造にしている。
//! [`SimdKernel::dot`]（既定経路）は `DEFAULT_PADDED_TAIL`（現状 `false`）に固定して
//! おり、production の挙動は本 Issue で変更しない。両方式は
//! [`SimdKernel::dot_with_scalar_tail`]／[`SimdKernel::dot_with_padded_tail`] として
//! テスト・ベンチ向けに公開する（`hybrid::sparse_refetch_observed` と同じ、生産経路と
//! 検証経路が同一実装を共有するための hook という位置付け）。採否・既定切替の判断は
//! Issue #529 が担う（詳細・機械検証・不採用形は
//! `docs/design/dot-kernel-branchless-tail.md` 参照。spec 本文は転記しない）。
//! `unsafe` ブロックの個数（3 個）は本変更でも増減しない
//! （`tests/isa.rs::unsafe_is_confined_to_isa_module_with_safety_comments`）。

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
    ///
    /// tail 方式は [`DEFAULT_PADDED_TAIL`]（現状 `false`＝現行のスカラー逐次和）に
    /// 固定する。production 経路の挙動は Issue #528 で変更しない
    /// （採否・切替は Issue #529）。
    pub fn dot(self, a: &[f32], b: &[f32]) -> f32 {
        self.dot_impl::<DEFAULT_PADDED_TAIL>(a, b)
    }

    /// 現行のスカラー逐次和 tail（`dot_lanes` の `rem_sum` を素朴な `iter().sum()` で
    /// 計算する方式）を強制した内積計算。ビット同一性テスト（`tests/isa.rs`）・
    /// Issue #529 の A/B 計測から参照する hook で、production 経路
    /// （[`Self::dot`]）は経由しない。
    pub fn dot_with_scalar_tail(self, a: &[f32], b: &[f32]) -> f32 {
        self.dot_impl::<false>(a, b)
    }

    /// 零埋め固定長バッファによる分岐なし tail（Issue #528）を強制した内積計算。
    /// [`Self::dot_with_scalar_tail`] とのビット同一性テスト・Issue #529 の A/B
    /// 計測から参照する hook で、production 経路（[`Self::dot`]）は経由しない。
    pub fn dot_with_padded_tail(self, a: &[f32], b: &[f32]) -> f32 {
        self.dot_impl::<true>(a, b)
    }

    /// [`Self::dot`]／[`Self::dot_with_scalar_tail`]／[`Self::dot_with_padded_tail`]
    /// が共有する本体。`PADDED_TAIL` は `dot_lanes` へそのまま伝播する const
    /// ジェネリックで、SIMD カーネルを呼ぶ 3 箇所の `unsafe` ブロック構造・SAFETY
    /// 根拠はトークン所持のみに依存し `PADDED_TAIL` の値には依存しない。
    fn dot_impl<const PADDED_TAIL: bool>(self, a: &[f32], b: &[f32]) -> f32 {
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
                unsafe { dot_neon::<PADDED_TAIL>(a, b) }
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
                unsafe { dot_avx2_fma::<PADDED_TAIL>(a, b) }
            }
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx512(_) => {
                // SAFETY: この variant は `Avx512Token::try_new` が
                // `is_x86_feature_detected!("avx512f")` を実行時確認できた場合に
                // のみ構築される sealed トークンを保持する（上記 Avx2Fma 分岐と同じ
                // 構造）。値の存在が CPU 対応の証明であり、`dot_avx512` の
                // `#[target_feature]` 契約を満たす。
                unsafe { dot_avx512::<PADDED_TAIL>(a, b) }
            }
        }
    }
}

/// [`SimdKernel::dot`] が使う既定の tail 方式（Issue #528）。
///
/// `false` ＝現行のスカラー逐次和 tail。production 経路は本 Issue で変更せず、
/// 採否・切替（`true` への反転）は Issue #529 のオーナー判断に委ねる。
/// 環境変数・設定ファイルによる上書き機構は設けない（CORE-12 と同じ方針。
/// `tests/isa.rs::isa_source_has_no_external_override_entry_points` が検査する
/// 禁止トークン集合はこの `const` にも適用される）。
const DEFAULT_PADDED_TAIL: bool = false;

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
fn dot_neon<const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
    dot_lanes::<4, PADDED_TAIL>(a, b)
}

/// x86_64 AVX2+FMA（256 bit・8 レーン）向け内積カーネル。
///
/// `f32::mul_add` で FMA 契約を表現する safe fn。呼び出しには `unsafe` が必要
/// （[`SimdKernel::dot`] の SAFETY コメント参照）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn dot_avx2_fma<const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
    dot_lanes::<8, PADDED_TAIL>(a, b)
}

/// x86_64 AVX-512（`avx512f`。512 bit・16 レーン）向け内積カーネル。
///
/// `avx512f` は FMA を含意する。呼び出しには `unsafe` が必要（[`SimdKernel::dot`]
/// の SAFETY コメント参照）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
fn dot_avx512<const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
    dot_lanes::<16, PADDED_TAIL>(a, b)
}

/// ISA 別カーネルの共通本体。`LANES` 個ずつのレーンアキュムレータへ `f32::mul_add`
/// （FMA 契約）で積算し、最後にレーン和を固定順（インデックス昇順）で畳む。
/// 添字アクセス（`[]`）は使わず `zip`／イテレータのみで書く
/// （.claude/rules/coding-rust.md）。同一 ISA・同一 LANES・同一 `PADDED_TAIL` では
/// 常に同じ演算順序になるため、`kernel.rs::dot` 経由で呼ぶ全 provider
/// （`CpuScalarProvider`・`ParallelSearchProvider`・バッチ経路・RLS 事前フィルタ）が
/// 同一の丸め誤差で揃う（provider 間の Top-k 整合という既存設計意図を維持する）。
///
/// `as_chunks` の端数（`a_rem`／`b_rem`）は `PADDED_TAIL` の値で処理方式が変わる
/// （Issue #528）が、`PADDED_TAIL` は const ジェネリックのため実行時分岐は生じず、
/// 各 monomorphization 内では固定の演算列になる:
/// - `PADDED_TAIL == false`（現行・[`SimdKernel::dot_with_scalar_tail`]）: 端数を
///   左から右への逐次和（`iter().sum()`）で計算する。
/// - `PADDED_TAIL == true`（[`SimdKernel::dot_with_padded_tail`]）: 端数を
///   [`padded_tail_sum`] へ委譲する（零埋め固定長バッファ経由の分岐なし tail）。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
fn dot_lanes<const LANES: usize, const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
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
    let rem_sum: f32 = if PADDED_TAIL {
        padded_tail_sum::<LANES>(a_rem, b_rem)
    } else {
        a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum()
    };
    lane_sum + rem_sum
}

/// 零埋め固定長バッファによる分岐なし tail（Issue #528）。
///
/// `a_rem`／`b_rem`（`LANES` 未満の端数スライス）を `LANES` 長の固定長配列へ詰め、
/// 積を取ってから [`dot_lanes`] の `lane_sum` と同じ `iter().sum()` で縮約する。
/// パディングレーンの積が `s + pad == s`（`s` が符号付きゼロ・±inf・NaN を含む
/// 任意の `f32` でもビット不変）になるよう、`a` 側は `+0.0`・`b` 側は `-0.0` で
/// 埋める（`x * (-0.0) == -0.0`（`x` が有限かつ非ゼロ）・`0.0 * (-0.0) == -0.0`
/// であり、両側を `+0.0` にすると `+0.0` パディングが混ざり `Sum<f32>` の中立元
/// `-0.0` との等価性が崩れるケースが生じるため、片側のみを負にする）。
/// これにより `dot_lanes::<LANES, true>` は `dot_lanes::<LANES, false>` と
/// 全入力でビット同一になる（`tests/isa.rs`・本モジュール unit test で機械検証）。
///
/// `a_rem.len()` と `b_rem.len()` は呼び出し元（[`dot_lanes`]。`a.as_chunks`／
/// `b.as_chunks` を同一の切り詰め済み `len` へ適用した結果）で常に一致するが、
/// 添字アクセスを避けるため `zip` で書き、万一の長さ不一致でも短い方への
/// 切り詰めという `dot_scalar` と同じ意味論を保つ。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline(always)]
fn padded_tail_sum<const LANES: usize>(a_rem: &[f32], b_rem: &[f32]) -> f32 {
    let mut a_pad = [0.0f32; LANES];
    let mut b_pad = [-0.0f32; LANES];
    for (dst, src) in a_pad.iter_mut().zip(a_rem.iter()) {
        *dst = *src;
    }
    for (dst, src) in b_pad.iter_mut().zip(b_rem.iter()) {
        *dst = *src;
    }

    let mut prod = [0.0f32; LANES];
    for (p, (x, y)) in prod.iter_mut().zip(a_pad.iter().zip(b_pad.iter())) {
        *p = x * y;
    }

    prod.iter().sum()
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

    /// [`dot_lanes`] の `PADDED_TAIL` 両方式（`false`＝現行のスカラー逐次和・
    /// `true`＝[`padded_tail_sum`] 経由の分岐なし tail、Issue #528）が全入力で
    /// ビット同一であることを機械検証する。`dot_lanes` は `#[target_feature]` を
    /// 持たない generic fn（本体は自動ベクトル化に委ねるだけで intrinsics を使わない）
    /// のため、`unsafe` なしに実 ISA へ依存せず直接呼べる。`f32::mul_add` は IEEE
    /// 準拠の正確丸め FMA であり実行 ISA に依存しないため、この unit test の結果は
    /// 実機 ISA（本体テストが動く CI runner の AVX2/AVX-512/NEON 対応有無）に
    /// 関わらず有効な回帰になる。
    ///
    /// NaN 入力は上流（`kernel.rs::KernelError::NonFiniteQuery`）で拒否される契約の
    /// ため対象に含めない（含める場合も、同一 ISA・同一 monomorphization 内であれば
    /// `f32::mul_add` は決定的なので `to_bits()` は一致するはずだが、本テストの
    /// スコープ外とする）。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn dot_lanes_padded_tail_matches_scalar_tail_bit_exact() {
        // 決定的擬似乱数（xorshift64*。`tests/isa.rs`・`tests/hybrid_recall.rs` 等と
        // 同一アルゴリズム。外部クレート不使用）。
        struct XorShift64Star(u64);
        impl XorShift64Star {
            fn next_u64(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.0 = x;
                x.wrapping_mul(0x2545_f491_4f6c_dd1d)
            }
            fn next_f32(&mut self) -> f32 {
                let bits = (self.next_u64() >> 40) as u32;
                (bits as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
            }
        }

        fn check<const LANES: usize>(seed: u64) {
            let mut rng = XorShift64Star(seed.max(1));
            // dim 0..=129: LANES ∈ {4, 8, 16} いずれでも複数チャンク＋端数の
            // 組み合わせを網羅する（129 = 8*16+1 = 16*8+1 等、境界を跨ぐ）。
            for dim in 0..=129usize {
                let a: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();
                let b: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();
                let scalar_tail = dot_lanes::<LANES, false>(&a, &b);
                let padded_tail = dot_lanes::<LANES, true>(&a, &b);
                assert_eq!(
                    scalar_tail.to_bits(),
                    padded_tail.to_bits(),
                    "LANES={LANES} dim={dim} scalar_tail={scalar_tail} padded_tail={padded_tail}"
                );
            }

            // エッジ値集合（符号付きゼロ・微小値・subnormal・境界値）を各要素へ
            // ブロードキャストして dim 1..=LANES*2+1 を走査する。
            let edge_values: [f32; 8] = [
                0.0,
                -0.0,
                1e-30,
                -1e-30,
                1e-25,
                -1e-25,
                f32::MIN_POSITIVE,
                -f32::MIN_POSITIVE,
            ];
            for &value in &edge_values {
                for dim in 1..=(LANES * 2 + 1) {
                    let a = vec![value; dim];
                    let b = vec![value; dim];
                    let scalar_tail = dot_lanes::<LANES, false>(&a, &b);
                    let padded_tail = dot_lanes::<LANES, true>(&a, &b);
                    assert_eq!(
                        scalar_tail.to_bits(),
                        padded_tail.to_bits(),
                        "LANES={LANES} dim={dim} value={value} scalar_tail={scalar_tail} \
                         padded_tail={padded_tail}"
                    );
                }
            }
        }

        for seed in [0x1234_5678_9abc_def1u64, 0x0fed_cba9_8765_4321, 42, 1] {
            check::<4>(seed);
            check::<8>(seed);
            check::<16>(seed);
        }
    }
}
