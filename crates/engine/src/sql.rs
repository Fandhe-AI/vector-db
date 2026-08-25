//! SQL 表層モジュールの入口（TASK-74・SQL-8 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md）。
//!
//! 責務境界: 受信 SQL テキスト（wire プロトコル経由の untrusted 入力）に対する
//! **許可リスト形式の構造検証**（[`allowlist`]）から、束縛（[`parser`]）・評価順序
//! （[`plan`]、TASK-76・SQL-7）・実行（[`exec`]、TASK-75・SQL-1〜4）までを担う。
//! `EngineCore::execute_sql`（`core.rs`。TASK-75 で追加した固有メソッド。
//! `VectorCore` trait は不変）が本モジュールの公開 API を土台に SQL 文を実行する。
//!
//! 本モジュール配下は wire プロトコル入力と同じ untrusted 入力の扱い
//! （`.claude/rules/coding-rust.md`）に従う。
//!
//! 下位モジュール:
//! - [`lexer`][]: untrusted な SQL テキストの自作トークナイザ
//! - [`allowlist`][]: 許可リスト検証本体・`SqlSurfaceError`。`HINT ORDER(...)` の構造検証も含む（TASK-76）
//! - [`parser`][]: 許可リスト通過後の束縛（列名・型照合、ベクトルリテラル解析。TASK-75）
//! - [`plan`][]: `HINT ORDER(...)` の評価順序規則（RLS は暗黙事前フィルタ＋最終安全網の
//!   二重適用を維持し、`HINT` で外せない。TASK-76・SQL-7・RLS-5）
//! - [`exec`][]: 実行計画（既定 RLS→SCALAR→DISTANCE、`HINT ORDER` 指定時は [`plan`] に従う）
//! - [`mode`][]: 取得モード（`recall`／`precision`）の優先順位解決・セッション状態
//!   （TASK-161・SQL-12）
//!
//! TASK-161（対象ビヘイビア: SQL-12）: クエリ単位の専用句 `USING MODE '<literal>'`
//! （[`allowlist`]）とセッション変数 `SET search_mode = '<literal>'`（同）を追加し、
//! 優先順位（クエリ句 > セッション変数 > 既定）の解決を [`mode::resolve_mode`] に
//! 集約した。`core.rs::EngineCore::execute_sql_in_session` が接続単位の
//! [`mode::SessionState`] を受け取って呼び出す新しい公開 API で、既存の
//! `execute_sql`（セッションなし）は空のセッションでこれへ委譲する。

pub mod allowlist;
pub mod exec;
pub mod lexer;
pub mod mode;
pub mod parser;
pub mod plan;

/// `EngineCore::execute_sql_in_session`（TASK-161）の成功応答。`SELECT` は
/// [`exec::QueryResult`] を、`SET search_mode` は解決前の設定値
/// （[`mode::SearchMode`]）そのものを返す。
#[derive(Debug, Clone, PartialEq)]
pub enum SqlOutcome {
    Query(exec::QueryResult),
    SetSearchMode(mode::SearchMode),
}
