//! 検索カーネルの実行経路選択ディスパッチ決定表（TASK-155・対象ビヘイビア: CORE-11, CORE-12）。
//!
//! `kernel.rs`（CORE-13 の provider 差し替え点）・`search_engine.rs`（CORE-9 の既定
//! provider 選択）・`batch_search.rs`（CORE-6, CORE-7 のバッチ実行エンジン）・
//! `batch_fallback.rs`（CORE-8 の GPU→CPU 縮退）は、それぞれ「入力 → 実行経路」の
//! 判断を個別に持ちうる構造だった。本モジュールはその判断を [`select_execution_path`]
//! という副作用なしの純関数へ 1 箇所に集約し、決定表として一意に定義する
//! （CORE-11）。呼び出し元（`core.rs::EngineCore::open` や `batch_search.rs` の
//! バッチ実行経路）は、実行前にここへ入力を渡して経路を確定してから、
//! 対応する provider・エンジンを構築・実行する想定である。
//!
//! # 実配線（単発クエリ経路・バッチ経路）
//!
//! `kernel.rs` が提供する provider は現状 `CpuScalarProvider`／`ParallelSearchProvider`
//! （スレッド構成の違いのみで、ISA 別の SIMD 幅を持つ別実装ではない）に限られるため、
//! [`ExecutionPath::CpuSimd { width }`] の `width` は「決定表が確定させた実行幅の
//! ラベル」であり、`width` ごとに異なる provider を切り替える先はまだ存在しない
//! （SIMD 幅別 provider の実装は TASK-156・CORE-14 の管轄）。それでも
//! `core.rs::EngineCore::search` は毎回 `select_execution_path` を呼び、戻り値が
//! `CpuSimd` の場合だけ既存 provider を実行し、`Gpu` の場合は
//! `CoreError::GpuPathUnavailable` を返して fail-closed に拒否する
//! （単発クエリ経路は [`GpuCapability`] を保持しないため `Gpu` は理論上到達しない
//! 分岐だが、決定表が返しうる全 variant を網羅させることで、将来 GPU capability を
//! 単発クエリ経路へ持ち込む変更が発生した際にコンパイルエラーで気付ける）。
//!
//! `dim` の検証上限の不一致（旧: 本モジュールの `batch_search::MAX_BATCH_DIM` 固定 vs.
//! 単発クエリ経路の `storage::MAX_EMBEDDING_DIM`）は、[`DispatchInput`] の
//! コンストラクタ（[`DispatchInput::for_single_query`]／[`DispatchInput::for_batch`]）を
//! 呼び出し元の文脈で分け、それぞれ適切な上限を内部に持たせることで解消した
//! （`DimLimit` 参照。単発クエリ用コンストラクタは常に `storage::MAX_EMBEDDING_DIM`、
//! バッチ用は `batch_search::MAX_BATCH_DIM` を使う）。
//!
//! `batch_fallback.rs::FallbackBatchEngine::batch_search` は `select_execution_path` の
//! 戻り値で primary（GPU）／CPU 縮退のどちらを試みるかを決める（旧来の
//! `match &self.primary { Available => .., Unavailable => .. }` という独自分岐を
//! 置き換えた。詳細は `batch_fallback.rs` のモジュールドキュメント参照）。primary が
//! 実際に構築成功した場合にのみ得られる [`GpuCapability`]（sealed トークン）を渡すため、
//! 未検証の GPU capability を経路選択へ持ち込む余地がない（CORE-12）。
//!
//! `batch_search.rs::should_aggregate_into_batch`（動的窓集約の判定）は本モジュールが
//! 呼び出す既存の純関数であり、二重に判定ロジックを持たない（同モジュールの
//! ドキュメンテーションコメントに明記の契約）。`batch_fallback.rs` が実装する
//! GPU 失敗時の実行時縮退（primary 失敗→CPU、CORE-8）はこの決定表の対象外である。
//! 本モジュールが担うのは「実行前の経路選択」であり、`batch_fallback` が担うのは
//! 「選択後の実行時 fail-safe」という責務分担を維持する。
//!
//! # CORE-12: 外部入力による経路上書き機構の不存在
//!
//! 本モジュールは経路選択を外部から上書きする引数・設定構造体・feature flag・
//! 環境変数読み取りを一切設けない。これは実装漏れの防止策ではなく、そもそも
//! そのような入力経路をコード上に作らないという設計方針そのものである
//! （未検証の ISA・バックエンド指定で `unsafe` カーネルを強制起動させる攻撃面を、
//! 機構の不存在によって構造的に排除する）。デバッグ用の経路可視化（`EXPLAIN` 等）が
//! 必要になった場合も、本モジュールへ書き込み経路を追加するのではなく、
//! wire/SQL 表層側で [`select_execution_path`] の戻り値を読み取り専用に
//! 表示する形の後続タスクとして検討する。
//!
//! ISA の完全な実行時検出（`is_x86_feature_detected!` 等による CPUID/HWCAP 照会、
//! TASK-156・CORE-14）は本タスクの範囲外だが、[`detect_current_isa`] はコンパイル時
//! ターゲット（`cfg(target_arch = ..)`）だけに基づく保守的な下限検出を提供する
//! （fail-closed: 実際より広い ISA を報告することはない。x86_64 は常に `Scalar` を
//! 返し、AVX2/AVX-512 の実行時検出は TASK-156 が担う）。

use crate::batch_search::{should_aggregate_into_batch, MAX_BATCH_DIM, MAX_BATCH_QUERIES};

/// 実行時に検出された ISA（TASK-156 の検出トークン導入前のデータ表現）。
///
/// `#[non_exhaustive]` にはしない。variant 追加時に [`select_execution_path`] の
/// 網羅 match がコンパイルエラーになり、決定表の更新漏れを構造的に防ぐため
/// （CORE-11: 決定表を 1 箇所に保つという設計意図と表裏一体）。
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

/// コンパイル時ターゲットのみに基づく保守的な ISA 下限検出（モジュールドキュメント
/// 「ISA の完全な実行時検出」の項参照）。呼び出し元が任意の [`DetectedIsa`] を
/// 偽装できないよう、[`DispatchInput::for_single_query`]／[`DispatchInput::for_batch`]
/// は `isa` を引数に取らず、内部でこの関数の戻り値のみを使う（codex-review P1
/// 指摘対応・PR #158。`isa` フィールド自体も private のため、呼び出し元が別の値を
/// 混入させる経路は構造的に存在しない）。
pub fn detect_current_isa() -> DetectedIsa {
    if cfg!(target_arch = "aarch64") {
        // aarch64 の baseline ISA は Neon（128 bit）を含むことがアーキテクチャ仕様上
        // 保証されている（実行時検出なしでも安全に主張できる下限）。
        DetectedIsa::Neon
    } else {
        // x86_64 等は AVX2/AVX-512 の実際の対応有無をコンパイル時には判定できないため、
        // 最も保守的な `Scalar` を返す（fail-closed。実行時検出は TASK-156 の管轄）。
        DetectedIsa::Scalar
    }
}

/// GPU バックエンドが実際に利用可能であることを証明する sealed capability トークン
/// （CORE-12: 未検証の GPU capability を外部から構築させない）。
///
/// コンストラクタ [`GpuCapability::proven`] は「検証済み GPU backend の参照
/// （`&dyn` [`crate::batch_fallback::BatchBackend`]）を提示できること」を型で
/// 要求する（codex-review P1 指摘対応・PR #158: 従来は単なる `pub(crate)` の
/// 引数なし関数だったため、backend を構築していない crate 内の任意モジュールからも
/// `GpuCapability::proven()` を呼べてしまい、CORE-12 の「未検証 capability を経路
/// 選択へ持ち込めない」契約を型で保証できていなかった。witness 引数を要求する
/// ことで、検証済み backend の所有・借用と capability の構築を分離不能にする。
/// `dispatch` と `batch_fallback` はモジュール階層上の祖先関係にないため
/// `pub(in ...)` によるモジュール限定は表現できず、この witness 引数方式で
/// 同等以上の保証（「値を持っている」ことそのものが証明になる）を型で与える）。
/// 現状唯一の生成元は `batch_fallback.rs::FallbackBatchEngine::build` が primary
/// backend の構築に成功した経路のみである。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuCapability(());

impl GpuCapability {
    /// 呼び出し元が実際に構築成功した GPU backend（`&dyn` [`crate::batch_fallback::
    /// BatchBackend`]）を提示した場合にのみ呼べる。`pub(crate)` だが、検証済み
    /// backend への参照を渡せない限り値を作れないため、未検証の capability を経路
    /// 選択へ持ち込む経路は構造的にない（CORE-12。codex-review P1 指摘対応・
    /// PR #158）。`_verified_backend` は値そのものを使わない（存在の証明としてのみ
    /// 使う witness 引数）。
    pub(crate) fn proven(_verified_backend: &dyn crate::batch_fallback::BatchBackend) -> Self {
        GpuCapability(())
    }
}

/// クエリベクトルの要素型。現状は `F32` のみを扱う。
///
/// `batch_search.rs::ResidentMatrix` が内部で保持する f16 パック表現（CORE-16
/// ポインタ）はバッチエンジン内部の常駐形式であり、呼び出し元が指定する
/// クエリ入力の型とは独立のため、決定表の入力には含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryDtype {
    F32,
}

/// CPU-SIMD 経路の実行幅。[`DetectedIsa`] からの純写像（[`select_execution_path`] 内で
/// 決まり、これ自体が別の判断を持つことはない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimdWidth {
    Scalar,
    W128,
    W256,
    W512,
}

/// 決定表が確定させる実行経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPath {
    /// CPU-SIMD 経路。`width` は [`DetectedIsa`] に対応する実行幅。
    CpuSimd { width: SimdWidth },
    /// GPU 経路（`batch_search.rs::BatchEngine` の f16 パック常駐行列参照実装、
    /// または `batch_fallback.rs::BatchBackend` を実装する将来の実 GPU バックエンド）。
    Gpu,
}

/// [`DispatchInput`] の `dim` 検証に使う上限の文脈（単発クエリ経路とバッチ経路で
/// 異なる。旧: 両経路とも `batch_search::MAX_BATCH_DIM` へ固定していたための不一致を
/// コンストラクタ分離で解消した。モジュールドキュメント「実配線」の項参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DimLimit {
    /// `core.rs::EngineCore::search`（単発クエリ経路）用。`storage::MAX_EMBEDDING_DIM`。
    SingleQuery,
    /// `batch_fallback.rs`／`batch_search.rs`（バッチ経路）用。`batch_search::MAX_BATCH_DIM`。
    Batch,
}

impl DimLimit {
    fn max(self) -> usize {
        match self {
            DimLimit::SingleQuery => SINGLE_QUERY_MAX_DIM,
            DimLimit::Batch => MAX_BATCH_DIM,
        }
    }
}

/// [`DispatchInput::for_single_query`] が用いる次元上限。`storage::MAX_EMBEDDING_DIM`
/// （`pub(crate)`）をここで再公開し、crate 外（`tests/dispatch.rs` 等）が上限値を
/// 二重管理せずに参照できるようにする。
pub const SINGLE_QUERY_MAX_DIM: usize = crate::storage::MAX_EMBEDDING_DIM as usize;

/// [`select_execution_path`] への入力。すべて値渡しで、参照透過性（同一入力→同一出力）を
/// 保つ（グローバル状態・環境変数・ファイル・時刻を一切参照しない。CORE-12）。
///
/// フィールドはすべて private（codex-review P1 指摘対応: `gpu_available`／`isa` が
/// public field だと任意の呼び出し元が未検証の ISA・GPU capability を直接構築できて
/// しまい、CORE-12 の「未検証指定による経路上書きを構造的に排除する」契約と矛盾する
/// ため）。構築は [`Self::for_single_query`]／[`Self::for_batch`] の 2 コンストラクタ
/// のみを経由し、`gpu`（[`GpuCapability`]。`pub(crate)` コンストラクタのみが値を持てる
/// sealed トークン）・`dim` の検証上限（[`DimLimit`]）は各コンストラクタが文脈に応じて
/// 固定する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchInput {
    /// GPU バックエンドが実際に利用可能であることの証明（[`GpuCapability`]）。
    /// `None` は「GPU 不能」（`batch_fallback.rs::PrimarySlot::Unavailable`、または
    /// 単発クエリ経路のように構造的に GPU capability を持ちえない呼び出し元）を表す。
    gpu: Option<GpuCapability>,
    /// 実行時に検出された ISA（[`detect_current_isa`] の戻り値を渡す想定）。
    isa: DetectedIsa,
    /// クエリベクトルの次元。0、または `dim_limit` の上限超過は不正入力として `Err` を
    /// 返す（fail-closed）。
    dim: usize,
    dim_limit: DimLimit,
    /// バッチ内のクエリ件数。0、または `batch_search::MAX_BATCH_QUERIES` 超過は不正入力
    /// として `Err` を返す（fail-closed）。単発クエリは常に 1。
    batch_size: usize,
    /// クエリベクトルの要素型。
    dtype: QueryDtype,
    /// 動的窓判定用の入力。キューから 1 件取り出した直後に後続が存在するかどうか
    /// （`batch_search.rs::should_aggregate_into_batch` へそのまま渡す）。バッチ経路
    /// （[`Self::for_batch`]）は待機キューを持たないため常に `false` を渡す。
    pending_after_pop: bool,
}

impl DispatchInput {
    /// `core.rs::EngineCore::search`（単発クエリ経路、CORE-9）用のコンストラクタ。
    /// GPU capability を引数に取らない（構造的に `gpu: None` へ固定する）。単発クエリ
    /// 経路は `batch_fallback.rs::FallbackBatchEngine` を経由しないため GPU backend の
    /// 構築結果自体を持ちえず、これは実装漏れではなく設計上の制約である。
    ///
    /// `isa` は引数に取らず、本コンストラクタが内部で [`detect_current_isa`] を呼んで
    /// 固定する（codex-review P1 指摘対応・PR #158: 旧版は `isa` を呼び出し元から
    /// 引数で受け取っており、`DispatchInput` のフィールドを private 化していても
    /// crate 外から任意の未検証 [`DetectedIsa`]（例: 実機で対応していない
    /// `Avx512`）を経路選択へ持ち込めてしまっていた。CORE-12 の「未検証指定による
    /// 経路上書きを構造的に排除する」契約は、`isa` を外部入力として受け取らないこと
    /// でのみ型で保証できる）。
    ///
    /// `dim` は 0、または `storage::MAX_EMBEDDING_DIM` 超過で `Err`（fail-closed）。
    pub fn for_single_query(dim: usize, pending_after_pop: bool) -> Result<Self, DispatchError> {
        Self::new(
            None,
            detect_current_isa(),
            dim,
            DimLimit::SingleQuery,
            1,
            pending_after_pop,
        )
    }

    /// `batch_fallback.rs::FallbackBatchEngine::batch_search`／`batch_search.rs` の
    /// バッチ経路（CORE-6, 7, 8）用のコンストラクタ。`gpu` は primary backend の構築に
    /// 成功した場合のみ [`GpuCapability`] を渡せる（呼び出し元が `Some`/`None` を自由に
    /// 選べるが、値自体は `dispatch` モジュール外から偽装できない）。バッチ経路は
    /// 待機キューを持たないため `pending_after_pop` は常に `false` として扱う。
    ///
    /// `isa` は [`Self::for_single_query`] と同じ理由で引数に取らず、内部で
    /// [`detect_current_isa`] を呼んで固定する（codex-review P1 指摘対応・PR #158）。
    ///
    /// `dim` は 0、または `batch_search::MAX_BATCH_DIM` 超過で `Err`（fail-closed）。
    pub fn for_batch(
        gpu: Option<GpuCapability>,
        dim: usize,
        batch_size: usize,
    ) -> Result<Self, DispatchError> {
        Self::new(
            gpu,
            detect_current_isa(),
            dim,
            DimLimit::Batch,
            batch_size,
            false,
        )
    }

    fn new(
        gpu: Option<GpuCapability>,
        isa: DetectedIsa,
        dim: usize,
        dim_limit: DimLimit,
        batch_size: usize,
        pending_after_pop: bool,
    ) -> Result<Self, DispatchError> {
        let max_dim = dim_limit.max();
        if dim == 0 || dim > max_dim {
            return Err(DispatchError::InvalidDim { dim, max: max_dim });
        }
        if batch_size == 0 || batch_size > MAX_BATCH_QUERIES {
            return Err(DispatchError::InvalidBatchSize {
                batch_size,
                max: MAX_BATCH_QUERIES,
            });
        }
        Ok(Self {
            gpu,
            isa,
            dim,
            dim_limit,
            batch_size,
            dtype: QueryDtype::F32,
            pending_after_pop,
        })
    }
}

/// [`DispatchInput`] のコンストラクタが返すエラー。fail-closed（曖昧な入力は拒否側に
/// 倒す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    /// `dim` が 0、または呼び出し元の文脈（[`DispatchInput::for_single_query`] なら
    /// `storage::MAX_EMBEDDING_DIM`、[`DispatchInput::for_batch`] なら
    /// `batch_search::MAX_BATCH_DIM`）の上限を超過した。
    InvalidDim { dim: usize, max: usize },
    /// `batch_size` が 0、または `batch_search::MAX_BATCH_QUERIES` を超過した。
    InvalidBatchSize { batch_size: usize, max: usize },
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::InvalidDim { dim, max } => {
                write!(f, "dispatch input dim invalid: dim={dim} max={max}")
            }
            DispatchError::InvalidBatchSize { batch_size, max } => {
                write!(
                    f,
                    "dispatch input batch_size invalid: batch_size={batch_size} max={max}"
                )
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// ISA から CPU-SIMD 実行幅への純写像。[`select_execution_path`] からのみ呼ばれる。
fn simd_width_for(isa: DetectedIsa) -> SimdWidth {
    match isa {
        DetectedIsa::Scalar => SimdWidth::Scalar,
        DetectedIsa::Neon => SimdWidth::W128,
        DetectedIsa::Avx2Fma => SimdWidth::W256,
        DetectedIsa::Avx512 => SimdWidth::W512,
    }
}

/// 実行経路選択の決定表本体（TASK-155・対象ビヘイビア: CORE-6, 7, 8, 11, 12）。
/// 副作用なしの純関数（同一 `input` に対し常に同一の `Result` を返す）。
///
/// `dim`・`batch_size` の 0・上限超過検証は [`DispatchInput`] のコンストラクタ
/// （[`DispatchInput::for_single_query`]／[`DispatchInput::for_batch`]）が構築時点で
/// 行っており（fail-closed）、本関数へ渡る `input` は既にその不変条件を満たす。
/// `select_execution_path` は `Result` を返すシグネチャを維持する（将来 `input` 以外の
/// 検証条件が決定表へ加わった場合に呼び出し元のエラーハンドリングを壊さないため）。
///
/// 具体的な判定条件（バッチ経由／単発クエリ経由の別・動的窓判定・GPU capability の
/// 有無の組み合わせ）は `docs/spec` の対応ビヘイビア（CORE-6, CORE-7, CORE-8）を
/// 正とする。実装は下記コードの網羅 match が唯一の表現であり、既存モジュール
/// （`batch_search.rs`／`batch_fallback.rs`）が個別に持っていた判定をここへ集約
/// しただけで、判定条件そのものは変更していない（CORE-11: 決定表を 1 箇所に保つ
/// という設計意図）。
///
/// `dtype` は現状 `F32` の 1 variant のみのため経路分岐には寄与しないが、
/// 網羅 match の対象に含め、将来 variant が増えた際に分岐漏れをコンパイルエラーで
/// 検出できるようにする。
pub fn select_execution_path(input: DispatchInput) -> Result<ExecutionPath, DispatchError> {
    // dtype は現時点で分岐に寄与しないが、網羅 match で束縛して将来の variant 追加を
    // コンパイルエラーで検出可能にしておく（決定表更新漏れの構造的防止。CORE-11）。
    match input.dtype {
        QueryDtype::F32 => {}
    }

    // バッチ扱いにするかどうか。`for_batch` 経由は常にバッチ扱い（CORE-6, 7, 8）。
    // `for_single_query` 経由は動的窓判定の吸収（CORE-7）で決まる。
    let treated_as_batch = match input.dim_limit {
        DimLimit::Batch => true,
        DimLimit::SingleQuery => {
            input.batch_size >= 2 || should_aggregate_into_batch(input.pending_after_pop)
        }
    };

    if treated_as_batch && input.gpu.is_some() {
        return Ok(ExecutionPath::Gpu);
    }

    // GPU 不能（CORE-8 縮退対応行）、または単発クエリで動的窓に入らない場合は
    // CPU-SIMD を選ぶ。ISA からの実行幅写像は分岐を持たない純写像のため、
    // ここでは呼び出すだけで新たな判断は追加しない。
    Ok(ExecutionPath::CpuSimd {
        width: simd_width_for(input.isa),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // `GpuCapability::proven()` は `pub(crate)` のため、本 unit テスト（同一クレート内）
    // からは呼べるが `tests/dispatch.rs`（別クレート扱いの結合テスト）からは呼べない
    // （CORE-12: 未検証の GPU capability を crate 外から構築できないことの回帰）。
    // GPU capability を伴う決定表の全網羅走査は、そのため本 unit テスト側に置く。
    // `proven()` は検証済み backend への witness 参照を要求するため、テストでは
    // 実際に呼び出されることのないスタブ実装（本体は unreachable）を渡して満たす。
    struct WitnessOnlyBackend;
    impl crate::batch_fallback::BatchBackend for WitnessOnlyBackend {
        fn batch_search(
            &self,
            _queries: &[crate::batch_search::BatchQuery<'_>],
        ) -> Result<Vec<crate::batch_search::BatchHit>, crate::batch_fallback::BatchExecError>
        {
            unreachable!("decision-table 網羅テスト用の witness スタブは実行されない")
        }
    }

    fn gpu() -> GpuCapability {
        GpuCapability::proven(&WitnessOnlyBackend)
    }

    /// `isa` を任意の [`DetectedIsa`] へ差し替えた [`DispatchInput`] を組み立てる
    /// テスト専用ヘルパー。`isa` フィールドは private だが、`mod tests` は
    /// `dispatch` モジュールの子であるため struct-update 構文でアクセスできる
    /// （codex-review P1 指摘対応・PR #158: 公開 `for_single_query`／`for_batch` は
    /// もはや `isa` を引数に取らず内部で [`detect_current_isa`] を呼んで固定するため、
    /// ISA ごとの決定表網羅走査は crate 内 unit テストでのみ、この struct-update
    /// 経由で行う。`GpuCapability` を仮に付与する既存パターンと同じ考え方）。
    fn with_isa(input: DispatchInput, isa: DetectedIsa) -> DispatchInput {
        DispatchInput { isa, ..input }
    }

    #[test]
    fn single_query_without_pending_uses_cpu_simd() {
        let input = with_isa(
            DispatchInput::for_single_query(8, false).expect("valid input"),
            DetectedIsa::Scalar,
        );
        assert_eq!(
            select_execution_path(input).expect("valid input"),
            ExecutionPath::CpuSimd {
                width: SimdWidth::Scalar
            }
        );
    }

    /// `for_single_query` 経由は GPU capability を構造的に持てない（引数に取らない）ため、
    /// 単発クエリで動的窓判定により GPU 昇格する分岐（CORE-7）が実際に `Gpu` へ帰着する
    /// ことは現状の公開 API からは起こらない。この分岐自体の回帰は、crate 内だけに
    /// 見える private フィールドの struct-update で「将来 GPU capability を伴う単発
    /// クエリ経路が追加された場合」を模した入力を組み立てて確認する。
    #[test]
    fn single_query_with_pending_promotes_to_batch_row_reaches_gpu_if_capability_were_present() {
        let base = with_isa(
            DispatchInput::for_single_query(8, true).expect("valid input"),
            DetectedIsa::Scalar,
        );
        assert_eq!(base.dim_limit, DimLimit::SingleQuery);
        let hypothetical = DispatchInput {
            gpu: Some(gpu()),
            ..base
        };
        assert_eq!(
            select_execution_path(hypothetical).expect("valid input"),
            ExecutionPath::Gpu
        );
        // 一方、現実の `for_single_query` の戻り値（GPU capability なし）は pending でも
        // CpuSimd のままである。
        assert_eq!(
            select_execution_path(base).expect("valid input"),
            ExecutionPath::CpuSimd {
                width: SimdWidth::Scalar
            }
        );
    }

    /// バッチ経路（`for_batch`）は件数によらず常にバッチ扱いになる（CORE-6, 7, 8）。
    /// `FallbackBatchEngine::batch_search` は呼び出し時点で既にバッチとして確定した
    /// 集合を渡すため、`batch_size == 1` でも「単発クエリの動的窓判定」は適用しない
    /// （`batch_fallback.rs` の実配線と一致させる回帰）。
    #[test]
    fn batch_prefers_gpu_when_available_even_for_single_item_batch() {
        for batch_size in [1usize, 2, MAX_BATCH_QUERIES] {
            let input = with_isa(
                DispatchInput::for_batch(Some(gpu()), 8, batch_size).expect("valid input"),
                DetectedIsa::Scalar,
            );
            assert_eq!(
                select_execution_path(input).expect("valid input"),
                ExecutionPath::Gpu,
                "batch_size={batch_size}"
            );
        }
    }

    #[test]
    fn gpu_unavailable_always_falls_back_to_cpu_simd() {
        for batch_size in [1usize, 2, MAX_BATCH_QUERIES] {
            let input = with_isa(
                DispatchInput::for_batch(None, 8, batch_size).expect("valid input"),
                DetectedIsa::Avx2Fma,
            );
            assert_eq!(
                select_execution_path(input).expect("valid input"),
                ExecutionPath::CpuSimd {
                    width: SimdWidth::W256
                },
                "batch_size={batch_size}"
            );
        }
    }

    #[test]
    fn isa_maps_to_expected_simd_width() {
        let cases = [
            (DetectedIsa::Scalar, SimdWidth::Scalar),
            (DetectedIsa::Neon, SimdWidth::W128),
            (DetectedIsa::Avx2Fma, SimdWidth::W256),
            (DetectedIsa::Avx512, SimdWidth::W512),
        ];
        for (isa, expected_width) in cases {
            let input = with_isa(
                DispatchInput::for_single_query(8, false).expect("valid input"),
                isa,
            );
            assert_eq!(
                select_execution_path(input).expect("valid input"),
                ExecutionPath::CpuSimd {
                    width: expected_width
                },
                "isa={isa:?}"
            );
        }
    }

    #[test]
    fn zero_dim_is_rejected_for_single_query() {
        let max = crate::storage::MAX_EMBEDDING_DIM as usize;
        assert_eq!(
            DispatchInput::for_single_query(0, false).unwrap_err(),
            DispatchError::InvalidDim { dim: 0, max }
        );
    }

    #[test]
    fn dim_over_limit_is_rejected_for_batch() {
        assert_eq!(
            DispatchInput::for_batch(None, MAX_BATCH_DIM + 1, 1).unwrap_err(),
            DispatchError::InvalidDim {
                dim: MAX_BATCH_DIM + 1,
                max: MAX_BATCH_DIM
            }
        );
    }

    /// 単発クエリ経路は `storage::MAX_EMBEDDING_DIM`（バッチ経路の `MAX_BATCH_DIM` より
    /// 大きい）を上限に使うため、バッチ経路では拒否される次元でも単発クエリ経路では
    /// 受理される（旧: 両経路とも `MAX_BATCH_DIM` に固定していたための不一致の回帰）。
    #[test]
    fn single_query_dim_limit_is_wider_than_batch_dim_limit() {
        let dim = MAX_BATCH_DIM + 1;
        assert!(DispatchInput::for_single_query(dim, false).is_ok());
        assert!(DispatchInput::for_batch(None, dim, 1).is_err());
    }

    #[test]
    fn zero_batch_size_is_rejected() {
        assert_eq!(
            DispatchInput::for_batch(None, 8, 0).unwrap_err(),
            DispatchError::InvalidBatchSize {
                batch_size: 0,
                max: MAX_BATCH_QUERIES
            }
        );
    }

    #[test]
    fn batch_size_over_limit_is_rejected() {
        assert_eq!(
            DispatchInput::for_batch(None, 8, MAX_BATCH_QUERIES + 1).unwrap_err(),
            DispatchError::InvalidBatchSize {
                batch_size: MAX_BATCH_QUERIES + 1,
                max: MAX_BATCH_QUERIES
            }
        );
    }

    #[test]
    fn determinism_same_input_yields_same_output_across_repeated_calls() {
        let cases = [
            with_isa(
                DispatchInput::for_single_query(8, false).expect("valid input"),
                DetectedIsa::Scalar,
            ),
            with_isa(
                DispatchInput::for_batch(Some(gpu()), 8, 4).expect("valid input"),
                DetectedIsa::Avx512,
            ),
            DispatchInput {
                pending_after_pop: true,
                ..with_isa(
                    DispatchInput::for_batch(Some(gpu()), 8, 1).expect("valid input"),
                    DetectedIsa::Scalar,
                )
            },
        ];
        for input in cases {
            let first = select_execution_path(input);
            let second = select_execution_path(input);
            assert_eq!(first, second, "input={input:?}");
        }
    }

    /// 動的窓行（`for_single_query` 経由・pending 有無）が
    /// `batch_search::should_aggregate_into_batch` と一致すること（二重管理を避ける
    /// という契約の回帰確認）。GPU capability は crate 内 struct-update で仮に付与し、
    /// 「昇格した場合に GPU へ帰着するか」を検証する（`single_query_with_pending_...`
    /// テストと同じ理由）。
    #[test]
    fn dynamic_window_row_matches_should_aggregate_into_batch() {
        for pending in [false, true] {
            let input = DispatchInput {
                gpu: Some(gpu()),
                ..with_isa(
                    DispatchInput::for_single_query(8, pending).expect("valid input"),
                    DetectedIsa::Scalar,
                )
            };
            let expected = if should_aggregate_into_batch(pending) {
                ExecutionPath::Gpu
            } else {
                ExecutionPath::CpuSimd {
                    width: SimdWidth::Scalar,
                }
            };
            assert_eq!(select_execution_path(input).expect("valid input"), expected);
        }
    }

    /// 決定表の全網羅走査（コンテキスト（`for_single_query`／`for_batch`）× `gpu` 有無 ×
    /// `isa` × `batch_size`（`for_batch` のみ 1・2・上限）× `pending_after_pop` の直積）。
    /// 旧 `tests/dispatch.rs` に置いていた同等の走査を、`GpuCapability::proven()` が
    /// `pub(crate)` のため crate 内 unit テストへ移設した（CORE-12 の回帰でもある:
    /// 結合テスト側から GPU capability を偽装できないこと自体がテストの一部）。
    #[test]
    fn decision_table_covers_gpu_and_cpu_product() {
        let isa_variants = [
            DetectedIsa::Scalar,
            DetectedIsa::Neon,
            DetectedIsa::Avx2Fma,
            DetectedIsa::Avx512,
        ];

        // `for_batch`: 件数によらず常にバッチ扱い（CORE-6, 7, 8）。
        let batch_sizes = [1usize, 2, MAX_BATCH_QUERIES];
        for gpu_present in [false, true] {
            for isa in isa_variants {
                for batch_size in batch_sizes {
                    let gpu_opt = if gpu_present { Some(gpu()) } else { None };
                    let input = with_isa(
                        DispatchInput::for_batch(gpu_opt, 8, batch_size).expect("valid input"),
                        isa,
                    );
                    let expected = if gpu_present {
                        ExecutionPath::Gpu
                    } else {
                        ExecutionPath::CpuSimd {
                            width: simd_width_for(isa),
                        }
                    };
                    assert_eq!(
                        select_execution_path(input).expect("valid input"),
                        expected,
                        "context=batch gpu_present={gpu_present} isa={isa:?} batch_size={batch_size}"
                    );
                }
            }
        }

        // `for_single_query`: 動的窓判定（`pending_after_pop`）で昇格するかどうかが
        // 決まる。実際の呼び出し元は GPU capability を持たないが、決定表そのものの
        // 網羅性は「GPU capability を仮に付与した場合」も含めて確認する。
        for gpu_present in [false, true] {
            for isa in isa_variants {
                for pending_after_pop in [false, true] {
                    let gpu_opt = if gpu_present { Some(gpu()) } else { None };
                    let input = DispatchInput {
                        gpu: gpu_opt,
                        ..with_isa(
                            DispatchInput::for_single_query(8, pending_after_pop)
                                .expect("valid input"),
                            isa,
                        )
                    };
                    let treated_as_batch = should_aggregate_into_batch(pending_after_pop);
                    let expected = if treated_as_batch && gpu_present {
                        ExecutionPath::Gpu
                    } else {
                        ExecutionPath::CpuSimd {
                            width: simd_width_for(isa),
                        }
                    };
                    assert_eq!(
                        select_execution_path(input).expect("valid input"),
                        expected,
                        "context=single gpu_present={gpu_present} isa={isa:?} pending={pending_after_pop}"
                    );
                }
            }
        }
    }
}
