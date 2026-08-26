//! SQL 表層モジュールの入口（TASK-74・SQL-8 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md）。
//!
//! 責務境界: 受信 SQL テキスト（wire プロトコル経由の untrusted 入力）に対する
//! **許可リスト形式の構造検証**（[`allowlist`]）から、束縛（[`parser`]）・評価順序
//! （[`plan`]、TASK-76・SQL-7）・実行（[`exec`]、TASK-75・SQL-1〜4）までを担う。
//! `EngineCore::execute_sql`（`core.rs`。TASK-75 で追加した固有メソッド。
//! `VectorCore` trait は不変）が本モジュールの公開 API を土台に SQL 文を実行する。
//!
//! 書き込み系 SQL 文（`INSERT`）は `EngineCore::execute_insert_sql`（TASK-80、
//! 対象ビヘイビア: SQL-10）が別エントリポイントとして扱う。文末専用句
//! `USING OPERATION_ID '<id>'`（[`using_operation_id`]）は本モジュールが構造
//! パース（省略・明示 `NULL` はいずれも `None`）のみを行い、必須化の判断
//! （省略を書き込みトランザクション開始前に `23502` で拒否するか否か）は
//! サーバー構成 [`crate::recovery::required_op_id::LedgerMode`] へ移した
//! （TASK-92・RECOVER-1。`allowlist::validate_insert` が `LedgerMode::require` へ
//! 委譲する）。
//!
//! 本モジュール配下は wire プロトコル入力と同じ untrusted 入力の扱い
//! （`.claude/rules/coding-rust.md`）に従う。
//!
//! 下位モジュール:
//! - [`lexer`][]: untrusted な SQL テキストの自作トークナイザ
//! - [`allowlist`][]: 許可リスト検証本体・`SqlSurfaceError`。`HINT ORDER(...)` の構造検証も含む（TASK-76）
//! - [`parser`][]: 許可リスト通過後の束縛（列名・型照合、ベクトルリテラル解析。TASK-75）
//! - [`plan`][]: `HINT ORDER(...)` の評価順序規則（RLS は暗黙事前フィルタ＋
//!   [`crate::rls::RlsSafetyNet`]（TASK-136）による最終安全網の二重適用を維持し、
//!   `HINT` で外せない。TASK-76・SQL-7・RLS-5）
//! - [`exec`][]: 実行計画（既定 RLS→SCALAR→DISTANCE、`HINT ORDER` 指定時は [`plan`] に従う）
//! - [`mode`][]: 取得モード（`recall`／`precision`）の優先順位解決・セッション状態
//!   （TASK-161・SQL-12）
//! - [`using_operation_id`][]: `USING OPERATION_ID '<id>'` 文末句の値型・検証（TASK-80）
//!
//! TASK-152（対象ビヘイビア: ERR-2）: `allowlist::SqlSurfaceError` の `wire_code` 写像は
//! [`crate::error_format`]（`ErrorClass`・`ClassifiedError` trait）へ委譲する。本モジュール
//! の公開シグネチャ・返値は変更しない（詳細は `error_format.rs` モジュールドキュメント
//! 参照）。
//!
//! TASK-161（対象ビヘイビア: SQL-12）: クエリ単位の専用句 `USING MODE '<literal>'`
//! （[`allowlist`]）とセッション変数 `SET search_mode = '<literal>'`（同）を追加し、
//! 優先順位（クエリ句 > セッション変数 > 既定）の解決を [`mode::resolve_mode`] に
//! 集約した。`core.rs::EngineCore::execute_sql_in_session` が接続単位の
//! [`mode::SessionState`] を受け取って呼び出す新しい公開 API で、既存の
//! `execute_sql`（セッションなし）は空のセッションでこれへ委譲する。
//!
//! TASK-80（対象ビヘイビア: SQL-10）: `INSERT ... USING OPERATION_ID '<id>'` の
//! 許可形状を追加した。実行は [`exec::execute_insert`] が担い、行の書き込みは
//! `tenant.rs` のガード付き API（`tenant::insert_typed_row`）経由に統一する
//! （TABLE-12・RLS-9）。
//!
//! TASK-147（対象ビヘイビア: EXT-3）: `WHERE` 句に前方一致条件
//! `<col> LIKE '<prefix>%'` を追加した（[`allowlist`] が構造を、
//! `crate::declarative_filter` が意味論を検証する。`LIKE` は末尾ちょうど 1 つの
//! `%` のみを許可し、`NOT LIKE`・`ILIKE`・中間 `%`・`_`・エスケープは拒否する）。
//! 既存の等価条件 `<col> = '<literal>'`（SQL-2）と合わせ、両者は
//! `crate::declarative_filter::MetadataFilter`（汎用 API。任意の `TEXT` 列に
//! 対する宣言的フィルタ）として一本化した（**BREAKING CHANGE**: 旧
//! `sql::parser::ScalarEq`・`BoundStatement::scalar_filters` を置換。詳細は
//! `declarative_filter.rs`・`sql/parser.rs` モジュールドキュメント参照）。

pub mod allowlist;
pub mod exec;
pub mod lexer;
pub mod mode;
pub mod parser;
pub mod plan;
pub mod udf_call;
pub mod using_operation_id;

/// `EngineCore::execute_sql_in_session`（TASK-161）の成功応答。`SELECT` は
/// [`exec::QueryResult`] を、`SET search_mode` は解決前の設定値
/// （[`mode::SearchMode`]）そのものを返す。TASK-79（SQL-9）で `CREATE FUNCTION` の
/// 応答として `CreateFunction` を追加した（**BREAKING CHANGE**: 既存の網羅的
/// `match` はワイルドカードアームの追加が必要）。
#[derive(Debug, Clone, PartialEq)]
pub enum SqlOutcome {
    Query(exec::QueryResult),
    SetSearchMode(mode::SearchMode),
    /// `CREATE FUNCTION <name>(...) AS <expr>`（TASK-79・SQL-9）がセッションへの
    /// 登録に成功したことを示す応答。登録された関数名を保持する。
    CreateFunction {
        name: String,
    },
}
