//! SQL 表層 DDL（`CREATE TABLE`）の実行権限判定・実行本体（SQL-23・TASK-85・
//! TASK-202、Issue #899。ポインタ: `docs/spec/05-tasks.md` TASK-85・TASK-202・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・`docs/spec/04-behavior/
//! table-model.md` TABLE-1・TABLE-2・TABLE-4・TABLE-6）。
//!
//! 責務境界: 許可形状の構造検証（[`crate::sql::allowlist::ValidatedCreateTable`]）
//! を受け取り、(1) 実行権限の判定（[`require_ddl_privilege`]）、(2) カタログへの
//! 反映（[`execute_create_table`]）の 2 つのみを担う。構文の許可リスト判定は
//! `sql::allowlist` の管轄、ディスパッチ（先頭トークンの覗き見・権限ゲートの
//! 呼び出し順序）は `core.rs::EngineCore::execute_sql_in_session` の管轄。
//!
//! **権限ゲートの判定順序（fail-closed。決定的）**: `core.rs` は
//! (1) 字句解析 → (2) `CREATE`／`TABLE` の先頭 2 トークン判定
//! （[`crate::sql::allowlist::is_create_table_statement`]）→
//! (3) **本モジュールの [`require_ddl_privilege`]（未許可なら即 `42501`）** →
//! (4) 構造・上限の検証（[`crate::sql::allowlist::validate_create_table_tokens`]。
//! `42601`／`54000`／`42701`）→ (5) 書き込みトランザクション内での存在判定
//! （[`execute_create_table`]。`42P07`）、という順に呼ぶ契約とする。(3) を
//! (4)(5) より**前**に置くことで、未許可の主体は構文が正しいか・テーブルが
//! 存在するかに関わらず常に同じ `DdlNotPermitted`（`42501`）のみを受け取り、
//! カタログの状態（テーブルの有無）・構文の詳細を一切観測できない
//! （`.claude/rules/security.md`「アクセス制御の不備」対応）。
//!
//! **権限の付与経路**: [`crate::sql::mode::SessionState::ddl_allowed`] は
//! 既定 `false`。付与は wire-server の `handshake.rs` が `auth::verify` 成功後、
//! 起動時 opt-in `--ddl-principals`（`UserStore::is_ddl_principal`）による許可
//! 主体判定を経てのみ [`crate::sql::mode::SessionState::grant_ddl`] を呼ぶ
//! （`--ddl-principals` 未指定のサーバーでは許可主体が存在しないため全 DDL が
//! `42501`）。NoSQL 表層・`SessionState::default()` を使う既存テストは構造上
//! DDL を実行できないまま不変。

use crate::catalog::{CatalogError, TableSchema};
use crate::sql::allowlist::{SqlSurfaceError, ValidatedCreateTable};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// `CREATE TABLE`（SQL-23・TASK-85）の成功応答。行数・件数のいずれも返さない
/// （`sql::exec::TruncateOutcome` と同じ設計。DDL はテナント横断の共有資源
///〔カタログ〕を変更するのみで、テナント固有の件数概念を持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreateTableOutcome {}

/// このセッションが DDL を実行できるかを判定する（SQL-23・TASK-202）。唯一の
/// 判定点——`core.rs` はこの関数の戻り値のみで許可・拒否を決め、他の場所で
/// 独自に `session.ddl_allowed()` を判定しない（モジュールドキュメント参照）。
pub(crate) fn require_ddl_privilege(session: &SessionState) -> Result<(), SqlSurfaceError> {
    if session.ddl_allowed() {
        Ok(())
    } else {
        Err(SqlSurfaceError::ddl_not_permitted())
    }
}

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
    let schema = TableSchema::new(validated.table_name.clone(), validated.columns.clone());
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
        | CatalogError::DependentObjectsStillExist(_) => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    })?;
    Ok(CreateTableOutcome {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_ddl_privilege_rejects_default_session() {
        let session = SessionState::default();
        let err = require_ddl_privilege(&session).expect_err("default session must be denied");
        assert!(matches!(err, SqlSurfaceError::DdlNotPermitted));
    }

    #[test]
    fn require_ddl_privilege_accepts_granted_session() {
        let mut session = SessionState::default();
        session.grant_ddl();
        require_ddl_privilege(&session).expect("granted session must be allowed");
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
        };
        let err = execute_create_table(&storage, &validated)
            .expect_err("two VECTOR columns must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }
}
