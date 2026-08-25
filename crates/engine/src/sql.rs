//! SQL 表層モジュールの入口（TASK-74・SQL-8 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md）。
//!
//! 責務境界: 受信 SQL テキスト（wire プロトコル経由の untrusted 入力）に対する
//! **許可リスト形式の構造検証**（[`allowlist`]）から、束縛（[`parser`]）・実行計画
//! （[`exec`]、TASK-75・SQL-1〜4）までを担う。`EngineCore::execute_sql`
//! （`core.rs`。TASK-75 で追加した固有メソッド。`VectorCore` trait は不変）が
//! 本モジュールの公開 API を土台に SQL 文を実行する。
//!
//! 書き込み系 SQL 文（`INSERT`）は `EngineCore::execute_insert_sql`（TASK-80、
//! 対象ビヘイビア: SQL-10）が別エントリポイントとして扱う。文末専用句
//! `USING OPERATION_ID '<id>'`（[`using_operation_id`]）の省略は、書き込み
//! トランザクションを開始する前に構造検証段階で fail-closed に拒否する
//! （RECOVER-1 の必須化ガードの前段）。
//!
//! 本モジュール配下は wire プロトコル入力と同じ untrusted 入力の扱い
//! （`.claude/rules/coding-rust.md`）に従う。
//!
//! 下位モジュール:
//! - [`lexer`][]: untrusted な SQL テキストの自作トークナイザ
//! - [`allowlist`][]: 許可リスト検証本体・`SqlSurfaceError`
//! - [`parser`][]: 許可リスト通過後の束縛（列名・型照合、ベクトルリテラル解析。TASK-75）
//! - [`exec`][]: 実行計画（RLS→SCALAR→DISTANCE 固定順、TASK-75）・INSERT 実行（TASK-80）
//! - [`using_operation_id`][]: `USING OPERATION_ID '<id>'` 文末句の値型・検証（TASK-80）

pub mod allowlist;
pub mod exec;
pub mod lexer;
pub mod parser;
pub mod using_operation_id;
