//! DDL（`CREATE TABLE`・`DROP TABLE`・`ALTER TABLE ADD COLUMN` 等）の実行権限
//! ゲートと実行本体（SQL-23・TASK-85・TASK-202・TASK-203、Issue #899・#900・#902）。
//!
//! 責務境界: `core.rs::EngineCore::execute_parsed_in_session` の
//! `ParsedSql::CreateTable`／`ParsedSql::DropTable`／`ParsedSql::AlterTable`
//! 分岐から呼ばれる唯一の
//! 入口として、DDL 実行権限の判定（[`require_ddl_permission`]。全 DDL 文が
//! 通る単一の判定点。#900・#901・#907〜#909 の `ALTER TABLE`／`VIEW`／
//! `FOREIGN KEY` 等もここを経由する想定）と、`crate::catalog::Storage::
//! create_table`／`drop_table`（既存のカタログ反映・削除本体。いずれも
//! `PolicyContext` を取らない全テナント対象の DDL）を SQL 表層の
//! [`crate::sql::allowlist::SqlSurfaceError`] 契約へ写像する実行本体
//! （[`execute_create_table`]・[`execute_drop_table`]）、および
//! `crate::catalog::Storage::alter_table_add_column`（TABLE-5。O(1) 列追加・
//! 既存行のバイト列を変えない）へ型名解決のうえ委譲する
//! [`execute_alter_table_add_column`]（Issue #900）を担う。構文の許可
//! リスト判定は `sql::allowlist` の管轄、ディスパッチ（先頭トークンの
//! 覗き見・権限ゲートの呼び出し順序）は `core.rs` の管轄。
//!
//! **判定順序（fail-closed。決定的）**: 構文検証
//! （`sql::allowlist::validate_create_table_tokens`／
//! `validate_drop_table_tokens`／`validate_alter_table_tokens`。いずれも
//! カタログ照会を一切行わない）→
//! [`require_ddl_permission`]（`42501`。カタログ照会なし・書き込み
//! トランザクション未開始）→ 実行本体（[`execute_create_table`]／
//! [`execute_drop_table`]／[`execute_alter_table_add_column`]。書き込み
//! トランザクション内で対象テーブル・列の存在・重複を判定。`42P07`／`42P01`／
//! `42701`）。権限を持たない主体には対象
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

use crate::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use crate::sql::allowlist::{
    SqlSurfaceError, ValidatedAlterTableAddColumn, ValidatedCreateTable, ValidatedCreateView,
    ValidatedDropTable, ValidatedDropView,
};
use crate::sql::ddl_column_type::SqlColumnTypeName;
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
/// - `WriteLockTimeout`（書き込みゲートの待機上限超過。SQL-31）→
///   `SqlSurfaceError::LockNotAvailable`（`55P03`）
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
    // UNIQUE 制約（TABLE-16・TASK-204、Issue #905）。参照列の実在・型適格性は
    // `sql::allowlist` が構造検証段階で判定済みで、`catalog::validate_schema`
    // （`create_table` 内）が同じ不変条件を再検証する（違反は `Invalid` として
    // 下記の `42601` へ写像される）。
    if !validated.unique_constraints.is_empty() {
        schema = schema.with_unique_constraints(validated.unique_constraints.clone());
    }
    // `CHECK` 制約（TABLE-16・TASK-204、Issue #906）。意味論検証（列の存在・型・
    // 禁止要素・正規化の往復一致）は列定義だけを持つスキーマに対して行う（他の
    // `CHECK` を参照する `CHECK` は許可しない）。カタログを参照しないため、
    // テーブル名の既存衝突（`42P07`）より先に判定しても存在オラクルにならない。
    if !validated.checks.is_empty() {
        let checks = crate::sql::check_constraint::validate_and_build(&schema, &validated.checks)?;
        schema = schema.with_checks(checks);
    }
    storage.create_table(&schema).map_err(|e| match e {
        CatalogError::TableAlreadyExists(name) => SqlSurfaceError::duplicate_table(name),
        CatalogError::Invalid(detail) => {
            SqlSurfaceError::unsupported(format!("invalid table schema: {detail}"))
        }
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の書き込み入口と同じく `55P03` を返す。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
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
        | CatalogError::IncompatibleTypeChange { .. }
        // `ViewNotFound`／`WrongObjectKind`／`DependentViewsExist`／
        // `ViewLimitExceeded` は `CREATE VIEW`／`DROP VIEW`
        // （TABLE-18・SQL-23・TASK-205、Issue #909）専用の変種で、
        // `Storage::create_table` からは返らない（到達不能）。
        | CatalogError::ViewNotFound(_)
        | CatalogError::WrongObjectKind(_)
        | CatalogError::DependentViewsExist(_)
        | CatalogError::ViewLimitExceeded(_)
        // `TooManyColumns` は `ALTER TABLE ADD COLUMN`（Issue #900・
        // `Storage::alter_table_add_column`）専用の変種で、`Storage::create_table`
        // からは返らない（列数上限は `validate_create_table_tokens` が構造検証
        // 段階で `54000` として既に拒否する。到達不能）。
        | CatalogError::TooManyColumns { .. }
        // `UniqueConstraintViolation` は `Storage::alter_table_add_unique_constraint`
        // 専用（既存行の走査結果）で、`create_table`（新規テーブル・既存行なし）
        // からは返らない（到達不能）。
        | CatalogError::UniqueConstraintViolation => SqlSurfaceError::Internal {
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
        // TABLE-18・SQL-23・TASK-205（Issue #909）: 対象名がビューだった場合
        // （`42809`）、対象テーブルをビューが参照している場合（`2BP01`）。
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::DependentViewsExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で、書き込み
        // ゲートの待機上限を超えた。他の書き込み入口と同じく `55P03` を返す
        // （`catalog::table_lookup_error` 系の写像と同じ契約）。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        // それ以外（redb バックエンド障害・カタログ破損・世代カウンタ枯渇等）は
        // サーバー側の内部事象として `XX000` へ丸める（`Storage::drop_table` の
        // ドキュメントが一覧する他の `CatalogError` variant はいずれもこの
        // 経路には現れない設計だが、`_` 節で網羅する。detail は固定文言のみ）。
        _ => SqlSurfaceError::Internal {
            detail: "DROP TABLE failed".to_string(),
        },
    }
}

/// `ALTER TABLE ... ADD COLUMN ...`（SQL-23・TASK-202、Issue #900）の成功応答。
/// 返す情報は文に書かれたテーブル名・列名のみに限定する（行数・テナント固有の
/// 件数は持たない。[`CreateTableOutcome`]・[`DropTableOutcome`] と同じ設計）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterTableOutcome {
    pub table_name: String,
    pub column_name: String,
}

/// `ALTER TABLE <table> ADD COLUMN <column> <type>` の実行本体（Issue #900）。
/// 呼び出し元（`core.rs`）は [`require_ddl_permission`] を必ず先に呼んでいる
/// 前提（対象テーブルの存在確認・ENUM 型名解決はいずれもカタログ照会のため、
/// 権限ゲートより後に置く）。
///
/// 判定順序（決定的）:
/// 1. 対象テーブルの存在確認。存在しなければ、同名のビューがあれば `42809`
///    （ビューへの DDL・書き込みは非対応。`CREATE VIEW`〔Issue #909〕の書き込み
///    系と同じ扱い）、無ければ `42P01`。型名解決より先に行い、存在しない
///    テーブルへの要求が型名の不正（`42601`／`0A000`）で答えられないようにする。
/// 2. 型名解決: `VECTOR` は常に `0A000`（`SqlSurfaceError::FeatureNotSupported`。
///    `sql::ddl_column_type` モジュールドキュメント参照）、ENUM 型名候補は
///    `storage.get_enum_type` で存在確認（未登録は `42601`）、それ以外のスカラー
///    型はそのまま `catalog::ColumnType` へ変換する。
/// 3. `catalog::Storage::alter_table_add_column`（単一 write トランザクション内で
///    テーブルの存在・列数上限・列名重複を再確認。TOCTOU なし）。1. と 3. の間に
///    テーブルが削除された場合も 1. と同じ写像（`42P01`／`42809`）になる。
///
/// 追加列は常に nullable として扱う（呼び出し元がこの契約を上書きする経路は
/// 存在しない。TABLE-5）。
pub(crate) fn execute_alter_table_add_column(
    storage: &Storage,
    stmt: &ValidatedAlterTableAddColumn,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match storage.get_table_schema(&stmt.table_name) {
        Ok(_) => {}
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_add_column_error(other)),
    }
    let ty = resolve_column_type(storage, &stmt.column_type)?;
    let column = ColumnDef::new(stmt.column_name.clone(), ty, true);
    storage
        .alter_table_add_column(&stmt.table_name, column)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_add_column_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        column_name: stmt.column_name.clone(),
    })
}

/// テーブルとして存在しない `name` について、ビューとして存在すれば
/// `WrongObjectType`（`42809`）、しなければ `UndefinedTable`（`42P01`）を返す
/// （`core.rs::EngineCore::reclassify_write_to_view_error` と同じ判定。
/// `catalog::Storage::view_definition` を単一の情報源とする）。ビュー定義の
/// 読み直し自体が失敗した場合は「ビューではない」と同一視せず
/// `catalog::table_lookup_error` で写像する（fail-closed。破損したビューを
/// 「存在しない」と誤報告しない）。
fn undefined_table_or_view(storage: &Storage, name: &str) -> SqlSurfaceError {
    match storage.view_definition(name) {
        Ok(Some(_)) => SqlSurfaceError::WrongObjectType {
            name: name.to_string(),
        },
        Ok(None) => SqlSurfaceError::UndefinedTable {
            name: name.to_string(),
        },
        Err(e) => crate::catalog::table_lookup_error(e),
    }
}

/// [`SqlColumnTypeName`]（構文木）を `catalog::ColumnType`（意味づけ済みの型）へ
/// 解決する。ENUM 型名候補の存在確認だけをここで行い、他の型は無条件で対応する
/// `ColumnType` へ写す（範囲検証は `catalog::alter_table_add_column` 内部の
/// `validate_column` に一本化。`sql::ddl_column_type` モジュールドキュメント
/// 参照）。
fn resolve_column_type(
    storage: &Storage,
    ty: &SqlColumnTypeName,
) -> Result<ColumnType, SqlSurfaceError> {
    match ty {
        SqlColumnTypeName::Text => Ok(ColumnType::Text),
        SqlColumnTypeName::Integer => Ok(ColumnType::Integer),
        SqlColumnTypeName::BigInt => Ok(ColumnType::BigInt),
        SqlColumnTypeName::Real => Ok(ColumnType::Real),
        SqlColumnTypeName::Double => Ok(ColumnType::Double),
        SqlColumnTypeName::Boolean => Ok(ColumnType::Boolean),
        SqlColumnTypeName::Date => Ok(ColumnType::Date),
        SqlColumnTypeName::Timestamp => Ok(ColumnType::Timestamp),
        SqlColumnTypeName::Bytea => Ok(ColumnType::Bytea),
        SqlColumnTypeName::Json => Ok(ColumnType::Json),
        SqlColumnTypeName::Jsonb => Ok(ColumnType::Jsonb),
        SqlColumnTypeName::Uuid => Ok(ColumnType::Uuid),
        SqlColumnTypeName::Numeric { precision, scale } => Ok(ColumnType::Numeric {
            precision: *precision,
            scale: *scale,
        }),
        // 既存行が埋め込みバイトを持たないテーブルへの VECTOR 列追加は、
        // arena 構築・KNN・HNSW 各経路の安全性が未検証のため常に拒否する
        // （`sql::ddl_column_type` モジュールドキュメント参照）。
        SqlColumnTypeName::Vector(_) => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "ALTER TABLE ADD COLUMN does not support VECTOR columns yet".to_string(),
        }),
        SqlColumnTypeName::Enum(name) => {
            let def = storage.get_enum_type(name).map_err(|e| match e {
                CatalogError::TypeNotFound(_) => SqlSurfaceError::UnsupportedSyntax {
                    detail: format!("unknown type name: {name}"),
                },
                other => map_add_column_error(other),
            })?;
            Ok(ColumnType::Enum(def))
        }
    }
}

/// `Storage::alter_table_add_column`（および ENUM 型名解決）の [`CatalogError`] を
/// SQL 表層の契約へ写像する（ERR-2。Issue #900）。`catalog::table_lookup_error`
/// （読み取り専用経路向け）とは意図的に共有しない——`ColumnAlreadyExists`・
/// `TooManyColumns` は読み取り経路には現れない DDL 固有の意味を持つため。
/// エラー文言にテナント・行内容・redb 内部詳細は含めない（security.md）。
fn map_add_column_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::ColumnAlreadyExists(name) => SqlSurfaceError::duplicate_column(name),
        // 列数上限超過は `CREATE TABLE` の列数上限（`validate_create_table_tokens`）
        // と同じく `54000` へ写像する。
        CatalogError::TooManyColumns { count } => {
            SqlSurfaceError::payload_too_large(format!("too many columns: {count}"))
        }
        CatalogError::Invalid(detail) => SqlSurfaceError::UnsupportedSyntax { detail },
        CatalogError::TypeNotFound(name) => SqlSurfaceError::UnsupportedSyntax {
            detail: format!("unknown type name: {name}"),
        },
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の DDL・書き込み入口と同じく `55P03`。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        // 対象名がビュー（Issue #909）。現行の `alter_table_add_column` はテーブル
        // 名前空間のみを引くため到達しないが、将来ビュー判別を内包した場合も
        // `42809` に揃える。
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        // ストレージ側の内部破損・想定外事象・本 DDL からは到達しないはずの
        // variant はいずれも詳細を露出しない `Internal`（`XX000`）へ丸める
        // （fail-closed。ワイルドカード腕を置かず全 variant を明示列挙する
        // ことで、将来の追加 variant をコンパイラに検出させる）。
        CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::TableAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
        | CatalogError::TypeAlreadyExists(_)
        | CatalogError::DependentObjectsStillExist(_)
        | CatalogError::ColumnNotFound(_)
        | CatalogError::ProtectedColumn(_)
        | CatalogError::IncompatibleTypeChange { .. }
        | CatalogError::ViewNotFound(_)
        | CatalogError::DependentViewsExist(_)
        | CatalogError::ViewLimitExceeded(_)
        // `UniqueConstraintViolation` は `Storage::alter_table_add_unique_constraint`
        // 専用（TABLE-16・TASK-204、Issue #905）で、`alter_table_add_column` からは
        // 返らない（到達不能）。
        | CatalogError::UniqueConstraintViolation => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
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
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の書き込み入口（`CREATE TABLE`／
        // `DROP TABLE`）と同じく `55P03` を返す（`XX000` へ丸めない）。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
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
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の書き込み入口（`CREATE TABLE`／
        // `DROP TABLE`）と同じく `55P03` を返す（`XX000` へ丸めない）。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "DROP VIEW failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 書き込みゲートの待機上限超過は `ALTER TABLE ADD COLUMN` でも `55P03`
    /// （SQL-31・TASK-221。他の DDL 入口と同じ契約。Issue #900）。
    #[test]
    fn add_column_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_add_column_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

    /// テーブルとして存在しない名前は、ビューなら `42809`、どちらでもなければ
    /// `42P01`（`execute_alter_table_add_column` の事前確認・書き込み txn 内の
    /// 再確認〔競合でテーブルが消えた場合〕の双方が共有する判定。Issue #900）。
    #[test]
    fn undefined_table_or_view_distinguishes_view_and_missing() {
        let (storage, _guard) = tmp_storage("undefined-or-view");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![crate::catalog::ColumnDef::new(
                "body",
                crate::catalog::ColumnType::Text,
                true,
            )],
            primary_key: None,
            unique_constraints: Vec::new(),
            checks: Vec::new(),
        };
        execute_create_table(&storage, &validated).expect("create table");
        storage
            .create_view("v_docs", "docs", "SELECT * FROM docs")
            .expect("create view");

        assert_eq!(
            undefined_table_or_view(&storage, "v_docs").wire_code(),
            "42809"
        );
        assert_eq!(
            undefined_table_or_view(&storage, "missing").wire_code(),
            "42P01"
        );
    }

    /// 列数上限超過は `54000`、列名重複は `42701` へ写像する（Issue #900）。
    #[test]
    fn add_column_error_mapping_distinguishes_limit_and_duplicate() {
        assert_eq!(
            map_add_column_error(CatalogError::TooManyColumns { count: 257 }).wire_code(),
            "54000"
        );
        assert_eq!(
            map_add_column_error(CatalogError::ColumnAlreadyExists("c".to_string())).wire_code(),
            "42701"
        );
    }

    /// 書き込みゲートの待機上限超過は `XX000` ではなく `55P03` へ写像する
    /// （SQL-31・TASK-221。DROP TABLE も他の書き込み入口と同じ契約）。
    #[test]
    fn drop_table_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_drop_table_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

    /// `CREATE VIEW`／`DROP VIEW`（TABLE-18・SQL-23・TASK-205、Issue #909）も
    /// `CREATE TABLE`／`DROP TABLE` と同じ書き込み入口の契約を守り、書き込み
    /// ゲートの待機上限超過を `XX000`（内部エラー）へ丸めず `55P03` として
    /// 返すことを固定する（SQL-31・TASK-221 との base 取り込みマージ統合で
    /// 見落としやすい写像の一つ）。
    #[test]
    fn create_view_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_create_view_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

    #[test]
    fn drop_view_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_drop_view_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

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
            unique_constraints: Vec::new(),
            checks: Vec::new(),
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
            unique_constraints: Vec::new(),
            checks: Vec::new(),
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
            unique_constraints: Vec::new(),
            checks: Vec::new(),
        };
        let err = execute_create_table(&storage, &validated)
            .expect_err("two VECTOR columns must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }
}
