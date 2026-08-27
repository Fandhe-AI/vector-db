//! `operation_id` 重複拒否の原子性（TASK-94、対象ビヘイビア: RECOVER-3。ポインタ:
//! `docs/spec/05-tasks.md` TASK-94・`docs/spec/04-behavior/recovery.md` RECOVER-3）。
//!
//! `recovery::ledger`（TASK-93・RECOVER-2）が write トランザクション内で
//! `(tenant_id, table, operation_id)` の存在確認と追記を行い、
//! [`crate::recovery::ledger::RecordOutcome`] を返す。本モジュールはその結果を
//! 「拒否してよいか」の判定へ薄く写像するだけの責務を持つ（判定自体を持ち込む
//! モジュールを増やさない）。
//!
//! 呼び出し元は `crate::tenant::*_unchecked`（行の書き込み・更新・削除・置換の各
//! 経路）で、`ledger::record_in_txn` の**直後・同一 write トランザクション内**で
//! [`ensure_first_use`] を呼ぶ。`Err` を返した場合、呼び出し元は行の書き込みへ
//! 進まずに `write_txn` を drop（commit しない）することで、台帳追記・行変更の
//! 両方を破棄する（redb の drop＝abort 契約に委ねる原子性。TOCTOU の窓は無い:
//! 判定と書き込みは同一トランザクション・redb 単一ライタ直列化の内側にある）。
//!
//! 事前チェック（書き込みトランザクションを開始する前の早期応答用最適化）は本
//! タスクのスコープ外。write トランザクション内の本判定が最終防御であり、事前
//! チェックはあくまで応答を速くするための追加最適化にすぎない（省略しても正しさは
//! 変わらない）。

use crate::recovery::ledger::RecordOutcome;

/// 同一 `(tenant_id, table, operation_id)` への書き込みが既に commit 済みであることを
/// 示す固定文言のエラー（TASK-94・RECOVER-3）。テナント ID・テーブル名・
/// `operation_id` の値のいずれも含めない（他テナントの存在情報を漏らさない。
/// security.md P0）。行キー衝突（[`crate::tenant::TenantWriteError::IdConflict`]・
/// [`crate::sql::allowlist::SqlSurfaceError::IdConflict`]、文言
/// `"row id already exists"`）とは異なる固定文言にすることで、クライアントが
/// 「`operation_id` の重複拒否＝先行実行が commit 済み」（RECOVER-7 が使う判定）を
/// 行キー衝突と取り違えないようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DuplicateOperationId;

impl std::fmt::Display for DuplicateOperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation_id already committed for this table")
    }
}

impl std::error::Error for DuplicateOperationId {}

/// [`RecordOutcome`] を「書き込みを続行してよいか」へ写像する（TASK-94・RECOVER-3）。
///
/// - [`RecordOutcome::Recorded`][]: 今回のトランザクションで新規記録した。続行してよい。
/// - [`RecordOutcome::AlreadyPresent`][]: 同一キーが既に台帳に存在する（keep-first。
///   `recovery::ledger` の恒久契約）。他セッションが先に commit 済みという意味なので、
///   今回の書き込みは拒否する。
/// - [`RecordOutcome::Skipped`][]: 台帳を持たない構成
///   （[`crate::recovery::required_op_id::LedgerMode::CompareOnlyWithoutLedger`]）。
///   台帳が無ければ重複の有無を判定できないため拒否しない（RECOVER-3 の対比構成:
///   台帳なし構成では重複防止そのものを提供しない、という既定仕様どおり）。
pub(crate) fn ensure_first_use(outcome: RecordOutcome) -> Result<(), DuplicateOperationId> {
    match outcome {
        RecordOutcome::Recorded | RecordOutcome::Skipped => Ok(()),
        RecordOutcome::AlreadyPresent => Err(DuplicateOperationId),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_is_ok() {
        assert!(ensure_first_use(RecordOutcome::Recorded).is_ok());
    }

    #[test]
    fn skipped_is_ok() {
        // 台帳なし構成では重複防止を提供しない既定仕様（上記ドキュメント参照）。
        assert!(ensure_first_use(RecordOutcome::Skipped).is_ok());
    }

    #[test]
    fn already_present_is_rejected() {
        assert_eq!(
            ensure_first_use(RecordOutcome::AlreadyPresent),
            Err(DuplicateOperationId)
        );
    }

    #[test]
    fn duplicate_operation_id_display_is_fixed_and_does_not_leak_identifiers() {
        let msg = DuplicateOperationId.to_string();
        assert_eq!(msg, "operation_id already committed for this table");
    }
}
