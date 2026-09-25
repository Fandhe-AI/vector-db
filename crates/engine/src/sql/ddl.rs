//! DDL（`CREATE TABLE`・`DROP TABLE` 等）の実行権限ゲートと実行本体
//! （SQL-23・TASK-85・TASK-202・TASK-203、Issue #899・#902）。
//!
//! 責務境界: `core.rs::EngineCore::execute_parsed_in_session` の
//! `ParsedSql::CreateTable`／`ParsedSql::DropTable` 分岐から呼ばれる唯一の
//! 入口として、DDL 実行権限の判定（[`require_ddl_permission`]。全 DDL 文が
//! 通る単一の判定点。#900・#901・#907〜#909 の `ALTER TABLE`／`VIEW`／
//! `FOREIGN KEY` 等もここを経由する想定）と、`crate::catalog::Storage::
//! create_table`／`drop_table`（既存のカタログ反映・削除本体。いずれも
//! `PolicyContext` を取らない全テナント対象の DDL）を SQL 表層の
//! [`crate::sql::allowlist::SqlSurfaceError`] 契約へ写像する実行本体
//! （[`execute_create_table`]・[`execute_drop_table`]）を担う。構文の許可
//! リスト判定は `sql::allowlist` の管轄、ディスパッチ（先頭トークンの
//! 覗き見・権限ゲートの呼び出し順序）は `core.rs` の管轄。
//!
//! **判定順序（fail-closed。決定的）**: 構文検証
//! （`sql::allowlist::validate_create_table_tokens`／
//! `validate_drop_table_tokens`。いずれもカタログ照会を一切行わない）→
//! [`require_ddl_permission`]（`42501`。カタログ照会なし・書き込み
//! トランザクション未開始）→ 実行本体（[`execute_create_table`]／
//! [`execute_drop_table`]。書き込みトランザクション内で対象テーブルの
//! 存在・重複を判定。`42P07`／`42P01`）。権限を持たない主体には対象
//! テーブルの有無を問わず常に `42501` を返し、DDL 権限をテーブル存在の
//! オラクルにしない（security.md「エラー・ログ経由で他テナントの
//! データ・存在情報を漏らさない」対応）。
//!
//! [`crate::policy::PolicyContext`] はテナント ID と可視性のみを運び認証主体を
//! 持たないため、DDL 権限は [`crate::sql::mode::SessionState::ddl_allowed`]
//! （接続単位）として持ち、wire-server の handshake が認証成功後に 1 回だけ
//! 付与する（`SessionState::allow_ddl` ドキュメント参照）。テナント境界とは
//! 別軸の権限であり（テーブル・カタログは全テナント共有）、RLS の判定を
//! 一切変更しない。

use crate::catalog::{CatalogError, TableSchema};
use crate::sql::allowlist::{SqlSurfaceError, ValidatedCreateTable, ValidatedDropTable};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// DDL 実行権限の唯一の判定点。`session.ddl_allowed()` が `false`（既定）の
/// 場合、カタログ照会・書き込みトランザクション開始のいずれよりも前に
/// [`SqlSurfaceError::InsufficientPrivilege`]（`42501`）へ fail-closed に落とす。
pub(crate) fn require_ddl_permission(session: &SessionState) -> Result<(), SqlSurfaceError> {
    if session.ddl_allowed() {
        Ok(())
    } else {
        Err(SqlSurfaceError::InsufficientPrivilege)
    }
}

/// `CREATE TABLE`（SQL-23・TASK-85）の成功応答。行数・件数のいずれも返さない
/// （[`DropTableOutcome`] と同じ設計。DDL はテナント横断の共有資源
///〔カタログ〕を変更するのみで、テナント固有の件数概念を持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreateTableOutcome {}

/// 構造検証済みの `CREATE TABLE`（[`ValidatedCreateTable`]）をカタログへ反映する
/// （SQL-23・TASK-85・TABLE-4）。実行本体は既存の [`Storage::create_table`]
/// （単一 write トランザクション内で存在確認・挿入・世代進行・commit を行う。
/// TOCTOU なし）にそのまま委譲し、第 2 の DDL 実行器を作らない。
///
/// `CatalogError` の写像:
/// - `TableAlreadyExists` → [`SqlSurfaceError::DuplicateTable`]（`42P07`）
/// - `Invalid`（列数・`VECTOR` 列複数宣言等の意味論的不正。列数・識別子形状の
///   大半は [`crate::sql::allowlist::validate_create_table_tokens`] が構造検証
///   段階で既に拒否済みのため、ここに到達するのは主に `VECTOR` の次元範囲・
///   複数 `VECTOR` 列宣言）→ [`SqlSurfaceError::unsupported`]（`42601`）
/// - その他（`redb` I/O 等）→ `SqlSurfaceError::Internal`（`XX000`。詳細を
///   クライアントへ渡さない。security.md「情報漏えい」対応）
pub(crate) fn execute_create_table(
    storage: &Storage,
    validated: &ValidatedCreateTable,
) -> Result<CreateTableOutcome, SqlSurfaceError> {
    let mut schema = TableSchema::new(validated.table_name.clone(), validated.columns.clone());
    // `PRIMARY KEY`（TABLE-16・TASK-204、Issue #903）。`validated.primary_key` は
    // `sql::allowlist::finalize_primary_key` が `id` 単独宣言を `None` へ既に
    // 正規化済みのため、ここでは素通しするだけでよい。
    if let Some(primary_key) = validated.primary_key.clone() {
        schema = schema.with_primary_key(primary_key);
    }
    storage.create_table(&schema).map_err(|e| match e {
        CatalogError::TableAlreadyExists(name) => SqlSurfaceError::duplicate_table(name),
        CatalogError::Invalid(detail) => {
            SqlSurfaceError::unsupported(format!("invalid table schema: {detail}"))
        }
        CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::TableNotFound(_)
        | CatalogError::ColumnAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
        | CatalogError::TypeNotFound(_)
        | CatalogError::TypeAlreadyExists(_)
        | CatalogError::DependentObjectsStillExist(_)
        // `ColumnNotFound`／`ProtectedColumn`／`IncompatibleTypeChange` は
        // `ALTER TABLE`（Issue #901・`Storage::alter_table_drop_column` 等）
        // 専用の変種で、`Storage::create_table` からは返らない（到達不能）。
        | CatalogError::ColumnNotFound(_)
        | CatalogError::ProtectedColumn(_)
        | CatalogError::IncompatibleTypeChange { .. } => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    })?;
    Ok(CreateTableOutcome {})
}

/// `DROP TABLE <table>`（SQL-23、TASK-203、Issue #902）の成功応答。
/// [`crate::sql::exec::TruncateOutcome`] と同じく削除件数（全テナント分の
/// 行数）を一切保持しないフィールドなし構造体——他テナントの行数を応答から
/// 推測できないようにする（RLS-9 と同じ設計判断。`DROP TABLE` はテナント
/// スコープの操作ではないため、そもそも「自テナント分の件数」という概念を
/// 持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTableOutcome {}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。
/// `crate::catalog::Storage::drop_table`（カタログエントリ・全テナント行
/// ストア・`operation_id` 台帳エントリの単一 write txn 削除。テーブル単位
/// 世代 bump を含む）へ委譲し、`CatalogError` を SQL 表層の契約へ写像する。
///
/// 依存オブジェクト検査（`2BP01`。VIEW・FOREIGN KEY からの参照）の挿入点:
/// VIEW（#909）・FOREIGN KEY（#907）はいずれも未実装のため、現時点では
/// `drop_table` 呼び出しの前後どちらにも検査を追加していない。実装される際は
/// ここへ、`require_ddl_permission` の直後・`Storage::drop_table` 呼び出しの
/// 直前として追加する想定。
pub(crate) fn execute_drop_table(
    storage: &Storage,
    validated: &ValidatedDropTable,
) -> Result<DropTableOutcome, SqlSurfaceError> {
    storage
        .drop_table(&validated.table_name)
        .map_err(map_drop_table_error)?;
    Ok(DropTableOutcome {})
}

/// `Storage::drop_table` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （`catalog::table_lookup_error` と同じ役割分担だが、`TableNotFound` を
/// `Ok(false)` ではなく `42P01` 応答へ写像する点が異なるため独立させる）。
/// エラー文言にテナント・行内容・redb 内部詳細は含めない
/// （security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応）。
fn map_drop_table_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        // `name` は字句解析済みの識別子（既に長さ上限の対象）で、`CatalogError`
        // 側もテナント・行内容を含まないため、`exec.rs::map_insert_write_error`
        // と同じくそのまま運んでよい。
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        // 識別子形式の不正（現状の呼び出し経路では構文検証済みの識別子しか
        // 渡らないため到達しないはずだが、`validate_identifier` の判定基準が
        // 将来変わった場合に備えて fail-closed に `42601` へ丸める）。
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed table reference in DROP TABLE")
        }
        // それ以外（redb バックエンド障害・カタログ破損・世代カウンタ枯渇等）は
        // サーバー側の内部事象として `XX000` へ丸める（`Storage::drop_table` の
        // ドキュメントが一覧する他の `CatalogError` variant はいずれもこの
        // 経路には現れない設計だが、`_` 節で網羅する。detail は固定文言のみ）。
        _ => SqlSurfaceError::Internal {
            detail: "DROP TABLE failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_ddl_permission_rejects_default_session() {
        let session = SessionState::default();
        let err = require_ddl_permission(&session).expect_err("default session must be denied");
        assert!(matches!(err, SqlSurfaceError::InsufficientPrivilege));
        assert_eq!(err.wire_code(), "42501");
    }

    #[test]
    fn require_ddl_permission_accepts_after_allow_ddl() {
        let mut session = SessionState::default();
        session.allow_ddl();
        assert!(require_ddl_permission(&session).is_ok());
    }

    #[test]
    fn insufficient_privilege_client_message_has_no_identifying_detail() {
        let msg = SqlSurfaceError::InsufficientPrivilege.client_message();
        assert_eq!(msg, "permission denied for DDL statement");
    }

    /// 一時 DB の払い出しは Issue #173 の共通ヘルパー（`crate::test_util::
    /// temp_db`）へ一本化する（重複実装を作らない）。
    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        (storage, guard)
    }

    #[test]
    fn execute_create_table_succeeds_for_new_table() {
        let (storage, _guard) = tmp_storage("succeeds");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![
                crate::catalog::ColumnDef::new(
                    "embedding",
                    crate::catalog::ColumnType::Vector(4),
                    false,
                ),
                crate::catalog::ColumnDef::new("body", crate::catalog::ColumnType::Text, true),
            ],
            primary_key: None,
        };
        execute_create_table(&storage, &validated).expect("create table must succeed");
        let schema = storage.get_table_schema("docs").expect("schema must exist");
        assert_eq!(schema.columns.len(), 2);
    }

    #[test]
    fn execute_create_table_rejects_duplicate_name() {
        let (storage, _guard) = tmp_storage("duplicate-name");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![crate::catalog::ColumnDef::new(
                "body",
                crate::catalog::ColumnType::Text,
                true,
            )],
            primary_key: None,
        };
        execute_create_table(&storage, &validated).expect("first create must succeed");
        let err = execute_create_table(&storage, &validated)
            .expect_err("second create with same name must fail");
        assert!(matches!(err, SqlSurfaceError::DuplicateTable { .. }));
        assert_eq!(err.wire_code(), "42P07");
    }

    #[test]
    fn execute_create_table_rejects_two_vector_columns() {
        let (storage, _guard) = tmp_storage("two-vector");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![
                crate::catalog::ColumnDef::new("a", crate::catalog::ColumnType::Vector(4), false),
                crate::catalog::ColumnDef::new("b", crate::catalog::ColumnType::Vector(4), false),
            ],
            primary_key: None,
        };
        let err = execute_create_table(&storage, &validated)
            .expect_err("two VECTOR columns must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }
}
