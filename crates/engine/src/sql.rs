//! SQL 表層モジュールの入口（TASK-74、対象ビヘイビア: SQL-8。ポインタ:
//! `docs/spec/05-tasks.md` TASK-74・`docs/spec/04-behavior/sql-surface.md` SQL-8）。
//!
//! 責務境界: 受信 SQL テキスト（wire プロトコル経由の untrusted 入力）に対する
//! **許可リスト形式の構造検証**のみを担う。許可した形状を通過した SQL が実際に
//! 検索カーネルへディスパッチされ結果を返す経路（受理側の実行・パース詳細）は
//! 本モジュールの管轄外で、TASK-75 以降（`sql/parser.rs`・`sql/exec.rs` 相当）が
//! 本モジュールの公開 API（[`allowlist::validate_statement`]）を土台に実装する。
//!
//! 将来 wire-server の簡易クエリプロトコル処理（Query メッセージハンドラ）から
//! 呼ばれる想定のため、本モジュール配下は wire プロトコル入力と同じ untrusted 入力の
//! 扱い（`.claude/rules/coding-rust.md`）に従う。
//!
//! 下位モジュール:
//! - [`lexer`]: untrusted な SQL テキストの自作トークナイザ
//! - [`allowlist`]: TASK-74 の成果物。許可リスト検証本体・`SqlSurfaceError`

pub mod allowlist;
pub mod lexer;
