//! テーブル単位 `operation_id` 台帳（TASK-93、対象ビヘイビア: RECOVER-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-93・`docs/spec/04-behavior/recovery.md` RECOVER-2・
//! `docs/spec/04-behavior/error-format.md` ERR-2・`docs/spec/04-behavior/sql-surface.md`
//! SQL-10）。
//!
//! `recovery::required_op_id`（TASK-92・RECOVER-1）が「`operation_id` を必須とするか」
//! を判定するのに対し、本モジュールは検証済みの [`crate::recovery::required_op_id::OperationId`]
//! を**テナント内・テーブル単位**で永続化する側を担う。台帳への追記は行の書き込み・
//! 更新・削除と**同一の `redb::WriteTransaction` 内**で行い、commit で両方が反映され、
//! 失敗（トランザクション drop）で両方が破棄される（原子性）。呼び出し元は
//! `crate::tenant::*_unchecked`（テナント境界付き書き込みガード API の内部実体）で、
//! `record_in_txn` を「スキーマ取得 → 台帳追記 → 行書き込み/削除 → commit」の順に
//! 呼ぶ（`tenant.rs` モジュールドキュメント参照）。
//!
//! **本タスクのスコープ外**（`tenant.rs` 冒頭ドキュメントおよび PR 本文の申し送り参照）:
//! 同一 `operation_id` の重複拒否（`23505`）・事前チェックは TASK-94 の管轄。内容照合
//! ハッシュ（`22023`）は TASK-101 の管轄。本モジュールは「既存エントリを上書きしない
//! （keep-first）」ことだけを恒久契約として担保する。

use redb::{ReadableTable, TableDefinition};

use crate::recovery::required_op_id::OperationId;
use crate::storage::StorageError;

/// `operation_id` 台帳テーブル（TASK-93、対象ビヘイビア: RECOVER-2）。
///
/// キーは `(tenant_id, table_name, operation_id)` の複合キー。`tenant_id` を先頭に
/// 置くことで、将来のテナント名前空間 range 走査（TASK-98・RECOVER-7）が単一 range
/// で完結する。
///
/// - `tenant_id` は**必ずサーバー側導出**（[`crate::policy::PolicyContext::tenant_id`]）
///   を使う。クライアント自己申告の `RowInput::tenant_id` はキーに使わない
///   （TABLE-12・RLS-9 と同じ原則。security.md P0）。
/// - `table_name` は [`crate::catalog::validate_identifier`] 通過済みの論理名
///   （`user_rows/` プレフィックスなし）。
/// - `operation_id` は検証済み [`OperationId`]（TASK-80。長さ上限 256・制御文字排除済み）。
///
/// 値は先頭 1 バイトのフォーマットバージョンのみ（[`LEDGER_ENTRY_FORMAT_VERSION`]）。
/// TASK-98（二層目 `last_op`）・TASK-101（内容ハッシュ）の拡張はバージョン繰り上げで
/// 対応する想定で、未知バージョンの値は fail-closed に拒否する（[`decode_entry`]）。
///
/// カタログ（`Storage::list_tables` 等）はユーザーテーブルのみを列挙する既存設計のため、
/// 本テーブル名 `op_ledger` はユーザーから見えるテーブル一覧に混入しない。
pub(crate) const OP_LEDGER_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("op_ledger");

/// [`OP_LEDGER_TABLE`] の値フォーマットバージョン（現行: 1 バイトのみ）。
const LEDGER_ENTRY_FORMAT_VERSION: u8 = 1;

/// 台帳エントリの符号化値（現行フォーマット。バージョンバイトのみ）。
const LEDGER_ENTRY_V1: [u8; 1] = [LEDGER_ENTRY_FORMAT_VERSION];

/// 未知の台帳値フォーマットを検出したときのエラー（fail-closed）。テナント・テーブル・
/// `operation_id` を含まない固定文言のみを返す（security.md P0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnknownLedgerEntryFormat;

impl std::fmt::Display for UnknownLedgerEntryFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown operation_id ledger entry format")
    }
}

impl std::error::Error for UnknownLedgerEntryFormat {}

/// 台帳値のデコード。空値・未知バージョンはいずれも fail-closed に拒否する
/// （`storage.rs` の行フォーマット検証と同方針）。
fn decode_entry(value: &[u8]) -> Result<(), UnknownLedgerEntryFormat> {
    match value {
        [LEDGER_ENTRY_FORMAT_VERSION] => Ok(()),
        _ => Err(UnknownLedgerEntryFormat),
    }
}

/// 「台帳へ書くか」を型で明示する（[`crate::recovery::required_op_id::LedgerMode::resolve`]
/// が生成する）。`Option` の `None` を黙って skip 扱いにしない（呼び出し元に判断を
/// 委ねず、`Disabled` という意図を型で残す）。
pub(crate) enum LedgerWrite<'a> {
    /// 台帳あり構成（`LedgerMode::Ledgered`）。検証済み `operation_id` を記録する。
    Record(&'a OperationId),
    /// 台帳を持たない構成（`LedgerMode::CompareOnlyWithoutLedger`）。台帳テーブルへは
    /// 一切触れない（テーブル自体も作らない）。
    Disabled,
}

/// [`record_in_txn`] の記録結果。呼び出し元（本タスク時点）は結果を無視してよいが、
/// 型として残すことで TASK-94 が `AlreadyPresent` を `23505` へ写像する際の土台にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    /// 新規記録した。
    Recorded,
    /// 同一キーが既に存在した（記録済みだったため上書きしていない。keep-first）。
    AlreadyPresent,
    /// [`LedgerWrite::Disabled`] のため台帳へ触れなかった。
    Skipped,
}

/// [`crate::core::EngineCore::operation_recorded`] の照会結果（TASK-93、対象ビヘイビア:
/// RECOVER-2）。後続 TASK-94（重複拒否）・TASK-98（二層台帳照会）の共通語彙として、
/// 本モジュールで `pub` 定義する。
///
/// `NoLedger` を `NotRecorded` へ丸めない: `LedgerMode::CompareOnlyWithoutLedger`
/// （台帳を持たない構成）では台帳テーブルへ一切触れないため、「未記録」という
/// 積極的な判定と「そもそも台帳を持たない」という消極的な事実を型で区別する
/// （fail-closed な区別。呼び出し元が両者を同じ意味に扱ってしまう誤用を防ぐ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerLookup {
    /// 台帳に記録済み。
    Recorded,
    /// 台帳あり構成だが未記録。
    NotRecorded,
    /// 台帳を持たない構成（[`LedgerWrite::Disabled`]）のため照会できない。
    NoLedger,
}

/// `write_txn` 内で `(tenant_id, table, operation_id)` を台帳へ追記する（TASK-93、
/// 対象ビヘイビア: RECOVER-2）。
///
/// 呼び出し元（`crate::tenant::*_unchecked`）が保持する同一 `redb::WriteTransaction`
/// を渡すこと。台帳追記は行の書き込み/削除と同一トランザクションになるため、
/// 呼び出し元が後続で commit しなければ台帳エントリも一緒に破棄される（原子性）。
///
/// 既存キーがあれば**上書きしない**（keep-first。台帳の恒久契約。TASK-98 が二層目
/// `last_op` を追加する際もこの一層目の意味は変えない）。
pub(crate) fn record_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
    ledger: LedgerWrite<'_>,
) -> Result<RecordOutcome, StorageError> {
    let op_id = match ledger {
        LedgerWrite::Record(op_id) => op_id,
        LedgerWrite::Disabled => return Ok(RecordOutcome::Skipped),
    };
    let mut ledger_table = write_txn.open_table(OP_LEDGER_TABLE)?;
    let key = (tenant_id, table, op_id.as_str());
    if let Some(guard) = ledger_table.get(key)? {
        // 値の中身は判定に使わない（存在すれば keep-first で終了）。ただし未知
        // フォーマットが混入していないかは fail-closed に確認しておく。
        decode_entry(guard.value()).map_err(|_| {
            StorageError::Codec("op_ledger entry has unknown format version".to_string())
        })?;
        return Ok(RecordOutcome::AlreadyPresent);
    }
    ledger_table.insert(key, LEDGER_ENTRY_V1.as_slice())?;
    Ok(RecordOutcome::Recorded)
}

/// `read_txn` 内で `(tenant_id, table, op_id)` が台帳に記録済みかを照会する（TASK-93、
/// 対象ビヘイビア: RECOVER-2）。台帳テーブルが未作成（台帳を一度も使っていない DB、
/// または [`LedgerWrite::Disabled`] のみで運用してきた構成）の場合は `false` を返す
/// （テーブル不在をエラーにしない）。
pub(crate) fn contains_in_read_txn(
    read_txn: &redb::ReadTransaction,
    tenant_id: &str,
    table: &str,
    op_id: &OperationId,
) -> Result<bool, StorageError> {
    let ledger_table = match read_txn.open_table(OP_LEDGER_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(StorageError::from(e)),
    };
    let key = (tenant_id, table, op_id.as_str());
    match ledger_table.get(key)? {
        Some(guard) => {
            decode_entry(guard.value()).map_err(|_| {
                StorageError::Codec("op_ledger entry has unknown format version".to_string())
            })?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::required_op_id::OperationId;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn op(id: &str) -> OperationId {
        OperationId::parse(id).expect("valid operation_id")
    }

    // (a) 記録 → 同一 txn commit 後に contains が true になる。
    #[test]
    fn record_then_commit_is_observable_in_read_txn() {
        let path = unique_db_path("ledger-a");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        let outcome = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a")),
        )
        .expect("record");
        assert_eq!(outcome, RecordOutcome::Recorded);
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-a"))
            .expect("contains");
        assert!(found);
    }

    // (b) Record 後に txn を drop すると、コミットされず false のまま。
    #[test]
    fn record_then_drop_without_commit_is_not_observable() {
        let path = unique_db_path("ledger-b");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        {
            let write_txn = db.begin_write().expect("begin write");
            let outcome = record_in_txn(
                &write_txn,
                "tenant-a",
                "documents",
                LedgerWrite::Record(&op("op-b")),
            )
            .expect("record");
            assert_eq!(outcome, RecordOutcome::Recorded);
            // 明示的に commit しない: drop により abort される。
        }

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-b"))
            .expect("contains");
        assert!(!found);
    }

    // (c) 同一キー 2 回目は AlreadyPresent で値が変わらない（keep-first）。
    #[test]
    fn second_record_of_same_key_is_already_present_and_keeps_first_value() {
        let path = unique_db_path("ledger-c");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        let first = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-c")),
        )
        .expect("record first");
        assert_eq!(first, RecordOutcome::Recorded);
        let second = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-c")),
        )
        .expect("record second");
        assert_eq!(second, RecordOutcome::AlreadyPresent);
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-c"))
            .expect("contains");
        assert!(found);
    }

    // (d) Disabled は台帳テーブルを作らない。
    #[test]
    fn disabled_write_does_not_create_ledger_table() {
        let path = unique_db_path("ledger-d");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        let outcome = record_in_txn(&write_txn, "tenant-a", "documents", LedgerWrite::Disabled)
            .expect("record");
        assert_eq!(outcome, RecordOutcome::Skipped);
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-d"))
            .expect("contains on missing table must be false, not an error");
        assert!(!found);
    }

    // (e) 未知フォーマットバージョンの値は fail-closed（デコードエラー）。
    #[test]
    fn unknown_format_version_is_rejected_fail_closed() {
        let path = unique_db_path("ledger-e");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        {
            let mut table = write_txn.open_table(OP_LEDGER_TABLE).expect("open table");
            table
                .insert(("tenant-a", "documents", "op-e"), [0xffu8].as_slice())
                .expect("insert raw unknown-version entry");
        }
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let err = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-e"))
            .expect_err("unknown format version must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));

        // record_in_txn 側でも同じキーへの追記が同様に拒否される（keep-first の
        // 分岐に入る前に、未知フォーマットの既存値を検出して fail-closed）。
        let write_txn2 = db.begin_write().expect("begin write");
        let err2 = record_in_txn(
            &write_txn2,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-e")),
        )
        .expect_err("unknown format version must be rejected on record too");
        assert!(matches!(err2, StorageError::Codec(_)));
    }
}
