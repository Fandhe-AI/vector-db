//! `operation_id` 必須化ガード（TASK-92、対象ビヘイビア: RECOVER-1。ポインタ:
//! docs/spec/05-tasks.md TASK-92・docs/spec/04-behavior/recovery.md RECOVER-1・
//! docs/spec/04-behavior/error-format.md ERR-2・docs/spec/04-behavior/sql-surface.md
//! SQL-10）。
//!
//! 責務境界: 「台帳あり構成（既定）では書き込み系操作に `operation_id` の指定を
//! 必須とし、省略（句の欠落・明示 `NULL` を含む）を書き込みトランザクション開始前に
//! `23502` で拒否する」という判断を、[`LedgerMode::require`] という副作用なしの
//! 純関数 1 箇所へ集約する。呼び出し元は 2 経路（[`crate::sql::allowlist::validate_insert`]
//! の `INSERT` 構造検証段階、および [`crate::core::EngineCore::insert_row`]・
//! [`crate::core::EngineCore::update_row`]・[`crate::core::EngineCore::delete_row`] の
//! 先頭）で、いずれも `crate::tenant::*`（行ストアへの書き込み）へ委譲する**前**に
//! 本ガードを通す。`operation_id` の構文パース（`USING OPERATION_ID` 句）・
//! 値検証（空文字・長さ上限・制御文字）は本モジュールの管轄外で、
//! [`crate::sql::using_operation_id::OperationId`]（本モジュールが再エクスポートする）
//! が担う。
//!
//! 保護の適用可否は**サーバー構成（[`LedgerMode`]）のみ**で決まり、クライアント入力
//! （SQL 句・セッション変数）では変えられない。`LedgerMode` は
//! `crate::sql::mode::SessionState`・`SET` 構文・`USING` 句のいずれからも到達できない
//! （`crate::precision::PrecisionPolicy` と同じ「fail-open 経路を構造的に持たない」
//! 設計）。差し替えは [`crate::core::EngineCore::with_ledger_mode`]（所有権を消費する
//! サーバー側ビルダー）のみが行う。
//!
//! 台帳への永続化・重複拒否（`23505`）・内容不一致（`22023`）は本モジュールの管轄外
//! （TASK-93・TASK-94・TASK-101 が本ガードの通過後に得られる検証済み
//! [`OperationId`] を土台にする）。

/// SQL 表層・`sql::using_operation_id` が定義する値型をそのまま再エクスポートする
/// （TASK-80 の差分を最小化し、値型の実体を移動しない）。
pub use crate::sql::using_operation_id::OperationId;

/// `operation_id` 保護の適用可否を決めるサーバー側構成（RECOVER-1）。
///
/// クエリ・セッション変数から到達できるフィールド・構文を一切持たない
/// （`crate::sql::mode::SessionState`・`SET` 構文・`USING` 句のいずれにも対応する
/// 差し替え経路を追加しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LedgerMode {
    /// 台帳あり構成（既定・fail-closed）。書き込み系操作は `operation_id` の指定を
    /// 必須とする。
    #[default]
    Ledgered,
    /// 台帳を持たない比較専用構成。本番運用では使わない（性能比較・回帰計測等の
    /// 限定用途）。`operation_id` の省略を許し、明示された値もそのまま通す
    /// （台帳が存在しないため、値自体は使われない）。
    CompareOnlyWithoutLedger,
}

/// `operation_id` が省略された（句の欠落・明示 `NULL` のいずれも含む）ことを表す
/// エラー。`Display`・`Debug` のいずれにもテーブル名・テナント・行内容を一切含めない
/// （`.claude/rules/security.md` P0「エラー・ログ経由で他テナントのデータ・存在情報を
/// 漏らさない」対応。固定文言のみを返す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingOperationId;

impl std::fmt::Display for MissingOperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing operation_id")
    }
}

impl std::error::Error for MissingOperationId {}

impl LedgerMode {
    /// `op_id` が `operation_id` 必須化の要件を満たすかを判定する。副作用なし・
    /// アロケーションなしの純関数（呼び出し位置で「書き込みトランザクション開始前」
    /// を保証しやすくするため）。
    ///
    /// - [`LedgerMode::Ledgered`] かつ `op_id` が `None`（句の欠落・明示 `NULL` の
    ///   いずれも呼び出し元がここへ到達する前に `None` へ正規化する）→
    ///   [`MissingOperationId`]（呼び出し元が `23502` へ写像する）
    /// - [`LedgerMode::Ledgered`] かつ `op_id` が `Some` → 受け取った参照をそのまま返す
    /// - [`LedgerMode::CompareOnlyWithoutLedger`] → `op_id` の有無を問わずそのまま返す
    ///   （台帳がないため値は使われない）
    pub fn require(
        self,
        op_id: Option<&OperationId>,
    ) -> Result<Option<&OperationId>, MissingOperationId> {
        match self {
            LedgerMode::Ledgered => match op_id {
                Some(id) => Ok(Some(id)),
                None => Err(MissingOperationId),
            },
            LedgerMode::CompareOnlyWithoutLedger => Ok(op_id),
        }
    }
}

/// [`crate::sql::allowlist::validate_insert`] が `?` 演算子で本ガードの結果を
/// `SqlSurfaceError` へ写像するための変換（SQL-10・ERR-2: `23502`）。
impl From<MissingOperationId> for crate::sql::allowlist::SqlSurfaceError {
    fn from(_: MissingOperationId) -> Self {
        crate::sql::allowlist::SqlSurfaceError::MissingOperationId
    }
}

/// [`crate::core::EngineCore::insert_row`]・[`crate::core::EngineCore::update_row`]・
/// [`crate::core::EngineCore::delete_row`] が `?` 演算子で本ガードの結果を
/// `TenantWriteError` へ写像するための変換（RECOVER-1・ERR-2: `23502`）。
impl From<MissingOperationId> for crate::tenant::TenantWriteError {
    fn from(_: MissingOperationId) -> Self {
        crate::tenant::TenantWriteError::MissingOperationId
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_ledgered() {
        assert_eq!(LedgerMode::default(), LedgerMode::Ledgered);
    }

    #[test]
    fn ledgered_rejects_missing_operation_id() {
        let err = LedgerMode::Ledgered.require(None).unwrap_err();
        assert_eq!(err, MissingOperationId);
    }

    #[test]
    fn ledgered_accepts_and_returns_same_reference() {
        let id = OperationId::parse("op-0001").expect("valid operation_id");
        let result = LedgerMode::Ledgered
            .require(Some(&id))
            .expect("must accept");
        assert_eq!(result, Some(&id));
    }

    #[test]
    fn compare_only_without_ledger_accepts_missing_operation_id() {
        let result = LedgerMode::CompareOnlyWithoutLedger
            .require(None)
            .expect("compare-only mode must not require operation_id");
        assert_eq!(result, None);
    }

    #[test]
    fn compare_only_without_ledger_passes_through_present_value() {
        let id = OperationId::parse("op-0002").expect("valid operation_id");
        let result = LedgerMode::CompareOnlyWithoutLedger
            .require(Some(&id))
            .expect("must accept");
        assert_eq!(result, Some(&id));
    }

    // security.md P0: エラーの `Display` にテーブル名・テナント・行内容を含めない
    // ことをピン留めする（固定文言のみ）。
    #[test]
    fn missing_operation_id_display_has_no_sensitive_detail() {
        assert_eq!(MissingOperationId.to_string(), "missing operation_id");
    }
}
