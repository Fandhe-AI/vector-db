# ADR: `operation_id` 必須化ガード（RECOVER-1・Issue #80）

- ステータス: Accepted
- 対応: Issue #80（TASK-92）
- 関連: `docs/spec/05-tasks.md`（TASK-92・TASK-93・TASK-94・TASK-95・TASK-101）・
  `docs/spec/04-behavior/recovery.md`（RECOVER-1）・
  `docs/spec/04-behavior/error-format.md`（ERR-2）・
  `docs/spec/04-behavior/sql-surface.md`（SQL-10）

## 概要

Issue #80（TASK-92）は、書き込み系操作に対する `operation_id` の必須化ガードを
engine 横断（SQL 表層・`EngineCore` の行書き込み API の両方）へ適用する対応である。
検討内容・判断の根拠は private spec（`docs/spec/04-behavior/recovery.md` RECOVER-1）
側で管理する。本ドキュメントは、対応する公開コード上の参照先を記録するための
ポインタである。本リポジトリで公開している設計方針の範囲は README.md
「実装方針（要点）」の通りであり、それを超える内容はここに記載しない。

## 設計判断（公開範囲のみ）

- 必須化の可否は `crates/engine/src/recovery/required_op_id.rs::LedgerMode` という
  **サーバー側構成専用**の値 1 箇所に集約する。クエリ句・セッション変数からは
  到達できない（`crate::precision::PrecisionPolicy` と同型の fail-open 非経路設計）。
- SQL 表層（`sql::allowlist::validate_insert`）と `EngineCore::{insert_row, update_row,
  delete_row}`（TASK-95）の 2 経路が同一ガードを通ることで、SQL に閉じない
  engine 横断の必須化にする。

## 影響を受けるコード

- `crates/engine/src/recovery.rs`・`crates/engine/src/recovery/required_op_id.rs`（新規:
  `LedgerMode`・`MissingOperationId`・`require`）
- `crates/engine/src/sql/allowlist.rs`（`parse_operation_id_clause`・`validate_insert`
  への `LedgerMode` 引数追加。`ParsedInsertShape`/`ValidatedInsert.operation_id` の
  `Option` 化）
- `crates/engine/src/sql/parser.rs`（`BoundInsert.operation_id` の `Option` 化）
- `crates/engine/src/sql/exec.rs`（`TenantWriteError::MissingOperationId` の写像アーム）
- `crates/engine/src/core.rs`（`EngineCore::ledger_mode` フィールド・
  `with_ledger_mode`・`insert_row`/`update_row`/`delete_row` への `operation_id` 引数）
- `crates/engine/src/tenant.rs`（`TenantWriteError::MissingOperationId` variant）
- `crates/engine/tests/recovery_required_op_id.rs`（結合テスト）・
  `crates/engine/tests/sql_operation_id.rs`・`crates/engine/tests/tenant_breach.rs`

## 参照

- `docs/spec/05-tasks.md`（TASK-92・TASK-93・TASK-94・TASK-95・TASK-101）
- `docs/spec/04-behavior/recovery.md`（RECOVER-1）
- `docs/spec/04-behavior/error-format.md`（ERR-2）
- `docs/spec/04-behavior/sql-surface.md`（SQL-10）
