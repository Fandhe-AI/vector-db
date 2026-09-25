//! DDL（`DROP TABLE` 等）の実行権限ゲートと実行本体（SQL-23、TASK-203、
//! Issue #902）。
//!
//! 責務境界: `core.rs::EngineCore::execute_parsed_in_session` の
//! `ParsedSql::DropTable` 分岐から呼ばれる唯一の入口として、DDL 実行権限の
//! 判定（[`require_ddl_permission`]。全 DDL 文が将来通る単一の判定点。
//! #899〜#901・#907〜#909 の `CREATE TABLE`／`ALTER TABLE`／`VIEW`／
//! `FOREIGN KEY` 等もここを経由する想定）と、`crate::catalog::Storage::
//! drop_table`（既存のカタログ削除本体。`PolicyContext` を取らない全テナント
//! 対象の DDL）を SQL 表層の [`crate::sql::allowlist::SqlSurfaceError`] 契約へ
//! 写像する実行本体（[`execute_drop_table`]）を担う。
//!
//! **判定順序（fail-closed）**: 構文検証（`sql::allowlist::validate_drop_table_tokens`。
//! カタログ照会なし）→ [`require_ddl_permission`]（`42501`。カタログ照会なし・
//! 書き込みトランザクション未開始）→ [`execute_drop_table`]（書き込み
//! トランザクション内で対象テーブルの存在を判定。`42P01`）。権限を持たない
//! 主体には対象テーブルの有無を問わず常に `42501` を返し、DDL 権限を
//! テーブル存在のオラクルにしない（security.md「エラー・ログ経由で他テナントの
//! データ・存在情報を漏らさない」）。
//!
//! [`crate::policy::PolicyContext`] はテナント ID と可視性のみを運び認証主体を
//! 持たないため、DDL 権限は [`crate::sql::mode::SessionState::ddl_allowed`]
//! （接続単位）として持ち、wire-server の handshake が認証成功後に 1 回だけ
//! 付与する（`SessionState::allow_ddl` ドキュメント参照）。テナント境界とは
//! 別軸の権限であり（テーブル・カタログは全テナント共有）、RLS の判定を
//! 一切変更しない。

use crate::catalog::CatalogError;
use crate::sql::allowlist::{
    SqlSurfaceError, ValidatedCreateView, ValidatedDropTable, ValidatedDropView,
};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// DDL 実行権限の唯一の判定点（Issue #902。#899〜#901・#907〜#909 の他の DDL も
/// 将来ここを経由する想定）。`session.ddl_allowed()` が `false`（既定）の場合、
/// カタログ照会・書き込みトランザクション開始のいずれよりも前に
/// `SqlSurfaceError::InsufficientPrivilege`（`42501`）へ fail-closed に落とす。
pub(crate) fn require_ddl_permission(session: &SessionState) -> Result<(), SqlSurfaceError> {
    if session.ddl_allowed() {
        Ok(())
    } else {
        Err(SqlSurfaceError::InsufficientPrivilege)
    }
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
/// （security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」）。
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
        // TABLE-18・SQL-23・TASK-205（Issue #909）: 対象名がビューだった場合
        // （`42809`）、対象テーブルをビューが参照している場合（`2BP01`）。
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::DependentViewsExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
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

/// `CREATE VIEW <name> AS <body>`（TABLE-18・SQL-23・TASK-205、Issue #909）の
/// 成功応答。[`DropTableOutcome`] と同じくフィールドを持たない（作成した
/// ビューの内容・他テナントの存在情報を応答から推測できないようにする必要は
/// ないが、`Insert`／`Truncate` と異なり返す値自体がないため設計を揃える）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateViewOutcome {}

/// `DROP VIEW <name>`（TABLE-18・SQL-23・TASK-205、Issue #909）の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropViewOutcome {}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。
/// `crate::catalog::Storage::create_view` へ委譲する（構文検証段〔
/// `validate_create_view_tokens`〕はカタログを一切照会していないため、
/// 参照先の存在確認・ネスト深さ判定・名前衝突判定はすべてこの呼び出しの
/// 中で初めて行われる）。
pub(crate) fn execute_create_view(
    storage: &Storage,
    validated: &ValidatedCreateView,
) -> Result<CreateViewOutcome, SqlSurfaceError> {
    storage
        .create_view(
            validated.name(),
            validated.base_relation(),
            validated.body_sql(),
        )
        .map_err(map_create_view_error)?;
    Ok(CreateViewOutcome {})
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。
/// `crate::catalog::Storage::drop_view` へ委譲する。
pub(crate) fn execute_drop_view(
    storage: &Storage,
    validated: &ValidatedDropView,
) -> Result<DropViewOutcome, SqlSurfaceError> {
    storage
        .drop_view(validated.name())
        .map_err(map_drop_view_error)?;
    Ok(DropViewOutcome {})
}

/// `Storage::create_view` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （ERR-6 の管轄表: 名前衝突 `42P07`、参照先不存在 `42P01`、ネスト深さ・
/// 登録件数上限超過 `54000`）。エラー文言にテナント・行内容・redb 内部詳細は
/// 含めない（security.md P0）。
fn map_create_view_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TableAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::ViewLimitExceeded(detail) => SqlSurfaceError::PayloadTooLarge { detail },
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed view definition in CREATE VIEW")
        }
        _ => SqlSurfaceError::Internal {
            detail: "CREATE VIEW failed".to_string(),
        },
    }
}

/// `Storage::drop_view` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （ERR-6 の管轄表: 対象不存在 `42P01`、テーブル名を指定 `42809`、依存する
/// ビューが残存 `2BP01`）。
fn map_drop_view_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::ViewNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::DependentViewsExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed view reference in DROP VIEW")
        }
        _ => SqlSurfaceError::Internal {
            detail: "DROP VIEW failed".to_string(),
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
}
