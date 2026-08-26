//! WASM UDF ランタイムの契約層（TASK-149、対象ビヘイビア: EXT-5, EXT-6。ポインタ:
//! `docs/spec/05-tasks.md` TASK-149・`docs/spec/04-behavior/extensions.md`
//! EXT-5, EXT-6・PoC: `docs/spec/03-poc/udf-mechanism/`）。
//!
//! 責務境界: `sql::udf_call`（SQL-9 の束縛・評価層。TASK-79）が「組み込み関数／
//! 宣言的 UDF」に続く第 3 の呼び出し先として WASM UDF を扱えるようにするための、
//! wasmtime 型を一切露出しない契約（[`WasmUdfBackend`] trait・[`SandboxLimits`]・
//! [`WasmUdfError`]・[`compile`]）を定義する。`sql::udf_call::BoundExpr::WasmCall`
//! は本モジュールの `Arc<dyn WasmUdfBackend>` のみを保持し、呼び出し（`eval`）の
//! シグネチャは変えない。
//!
//! ABI（「1 関数の縦切り」。PoC-11 参照）: 登録対象モジュールは export 関数
//! `f(ptr: i32, len: i32, scalar: f32) -> f32` と export `memory` を持ち、import は
//! 0 件（1 件でも登録拒否。ホスト能力を一切付与しない多重防御）。静的シグネチャは
//! `(Vector, Scalar) -> Scalar` に固定する。
//!
//! サンドボックス（EXT-6）: 無限ループは epoch interruption（[`SandboxLimits::deadline_ticks`]）、
//! 過大メモリ確保は `StoreLimits`（[`SandboxLimits::memory_limit_bytes`]）、ホスト
//! ファイルアクセスは import 全拒否＋ WASI 未リンクで防ぐ設計だが、これらを実装する
//! wasmtime バックエンドは依存追加のユーザー承認が未取得（`.claude/rules/dependency-policy.md`。
//! 承認記録は Issue #97 参照）。承認が得られるまで本ファイルはこの契約層のみを
//! 実装し、[`compile`] は常に [`WasmUdfError::RuntimeUnavailable`] を返す
//! （モジュールを一切実行しない）。承認後は `compile` 内部にバックエンド実装を
//! 追加する変更のみで足り、`sql::udf_call`・`sql::mode` 側の呼び出し口は
//! 変更不要な設計にしてある。
//!
//! fail-closed: 登録時・呼び出し時のあらゆる失敗は `Result` として伝播し
//! （`unwrap`/`expect`/添字アクセス禁止。`.claude/rules/coding-rust.md`）、
//! `Mutex` poison もエラー化する。エラー文言は固定の英語文言とし、モジュール内容・
//! 行値・テナント情報を含めない（security.md「情報漏えい」対応）。

use std::fmt;

/// 登録対象モジュールのバイト長上限（パース前に検証する。WIRE-4 の 1 メッセージ
/// 上限と整合させ、無制限 `Vec` 確保を防ぐ。security.md「不安全な設計」対応）。
pub const MAX_MODULE_BYTES: usize = 1024 * 1024;

/// epoch interruption のティック間隔（無限ループ検出の粒度）。
pub const EPOCH_TICK_INTERVAL_MS: u64 = 100;

/// 呼び出し 1 回あたりの既定 deadline（ティック数）。`EPOCH_TICK_INTERVAL_MS` との
/// 積が既定の実行時間上限（約 5 秒）になる。
pub const DEFAULT_CALL_DEADLINE_TICKS: u64 = 50;

/// インスタンスへ許可する線形メモリの既定上限（バイト）。
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// サンドボックスの実行時上限（登録 API が受け取る。テストは短い `deadline_ticks`
/// を指定し、既定値〔約 5 秒〕への依存を避けられる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    pub memory_limit_bytes: usize,
    pub deadline_ticks: u64,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        SandboxLimits {
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            deadline_ticks: DEFAULT_CALL_DEADLINE_TICKS,
        }
    }
}

/// WASM UDF の登録・呼び出し双方で起こりうる失敗を表す（`Display` は固定の英語
/// 文言。モジュール内容・行値・テナント情報を含めない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmUdfError {
    /// 登録対象モジュールが [`MAX_MODULE_BYTES`] を超える。
    ModuleTooLarge,
    /// wasm バイナリとしてパース・検証できない。
    InvalidModule,
    /// モジュールが 1 件以上の import を宣言している（ホスト能力の付与を拒否）。
    ImportsNotAllowed,
    /// export `memory` が存在しない。
    MissingMemoryExport,
    /// 指定した entry export 関数が存在しない。
    MissingEntryExport,
    /// entry export 関数のシグネチャが `(i32, i32, f32) -> f32` と一致しない。
    SignatureMismatch,
    /// 初期メモリまたは実行中の `memory.grow` が [`SandboxLimits::memory_limit_bytes`]
    /// を超える。
    MemoryLimitExceeded,
    /// 実行時トラップ（`kind` はトラップ種別の固定文言。deadline 超過を含む）。
    Trap(&'static str),
    /// wasmtime バックエンドが未実装（依存追加のユーザー承認待ち）の間に
    /// [`compile`] が呼ばれた。
    RuntimeUnavailable,
    /// バックエンド内部の排他制御（`Mutex`）が poison 状態だった。
    Poisoned,
}

impl fmt::Display for WasmUdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmUdfError::ModuleTooLarge => write!(f, "wasm udf: module exceeds size limit"),
            WasmUdfError::InvalidModule => write!(f, "wasm udf: invalid module"),
            WasmUdfError::ImportsNotAllowed => {
                write!(f, "wasm udf: module imports are not allowed")
            }
            WasmUdfError::MissingMemoryExport => {
                write!(f, "wasm udf: module does not export memory")
            }
            WasmUdfError::MissingEntryExport => {
                write!(f, "wasm udf: module does not export the entry function")
            }
            WasmUdfError::SignatureMismatch => {
                write!(f, "wasm udf: entry function signature mismatch")
            }
            WasmUdfError::MemoryLimitExceeded => write!(f, "wasm udf: memory limit exceeded"),
            WasmUdfError::Trap(kind) => write!(f, "wasm udf: trap ({kind})"),
            WasmUdfError::RuntimeUnavailable => write!(f, "wasm udf: runtime is not available"),
            WasmUdfError::Poisoned => write!(f, "wasm udf: internal lock is poisoned"),
        }
    }
}

impl std::error::Error for WasmUdfError {}

/// WASM UDF 1 件分のコンパイル済み呼び出し口（`sql::udf_call::BoundExpr::WasmCall`
/// が `Arc<dyn WasmUdfBackend>` として保持する）。wasmtime 等の実行時型を
/// `sql::udf_call` 側へ露出しないための境界インターフェース。
///
/// `call_vector_scalar` は ABI 固定シグネチャ `(Vector, Scalar) -> Scalar` の呼び出しを
/// 1 回実行する。実装はスレッド安全でなければならない（`SessionState` は `Clone` かつ
/// `BoundExpr` は複数スレッドから参照されうる契約はないが、`Send + Sync` を要求する
/// ことで将来の並列実行経路にも安全に対応する）。
pub trait WasmUdfBackend: fmt::Debug + Send + Sync {
    fn call_vector_scalar(&self, v: &[f32], scalar: f64) -> Result<f64, WasmUdfError>;
}

/// 登録対象の WASM モジュールを検証・コンパイルする（`sql::mode::SessionState::register_wasm_udf`
/// の呼び出し先）。実行時バックエンド（wasmtime）は依存追加のユーザー承認が
/// 得られていないため（`.claude/rules/dependency-policy.md`。承認記録は
/// Issue #97 参照）、本関数は常に [`WasmUdfError::RuntimeUnavailable`] を返す
/// 契約層のみを実装する。承認後に別変更でバックエンド実装（`wasmtime::Engine`
/// を用いた `Module::new` → import 検査 → `StoreLimits` 付き `Store` の構築）を
/// この関数の内部へ追加する（本ファイルの他 API・呼び出し元 `sql::udf_call`・
/// `sql::mode` は変更不要な設計にしてある）。
///
/// `bytes` は untrusted（wire 経由で搬送されうる）ため、パース前にサイズ上限
/// （[`MAX_MODULE_BYTES`]）を検証してから処理する契約をバックエンド実装は守ること。
pub fn compile(
    bytes: &[u8],
    entry: &str,
    limits: SandboxLimits,
) -> Result<std::sync::Arc<dyn WasmUdfBackend>, WasmUdfError> {
    let _ = (bytes, entry, limits);
    Err(WasmUdfError::RuntimeUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_is_unavailable_without_backend() {
        let err = compile(b"\0asm", "f", SandboxLimits::default()).unwrap_err();
        assert_eq!(err, WasmUdfError::RuntimeUnavailable);
    }

    #[test]
    fn sandbox_limits_default_matches_documented_constants() {
        let limits = SandboxLimits::default();
        assert_eq!(limits.memory_limit_bytes, DEFAULT_MEMORY_LIMIT_BYTES);
        assert_eq!(limits.deadline_ticks, DEFAULT_CALL_DEADLINE_TICKS);
    }

    #[test]
    fn wasm_udf_error_display_is_stable_english_text() {
        assert_eq!(
            WasmUdfError::ModuleTooLarge.to_string(),
            "wasm udf: module exceeds size limit"
        );
        assert_eq!(
            WasmUdfError::Trap("deadline exceeded").to_string(),
            "wasm udf: trap (deadline exceeded)"
        );
    }
}
