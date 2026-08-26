//! TASK-92（対象ビヘイビア: RECOVER-1）の入口モジュール。障害回復系のガード・台帳の
//! 実装をここへ集約する（後続 TASK-93: `operation_id` 台帳の永続化、TASK-94:
//! 重複拒否の配置先。ポインタ: docs/spec/05-tasks.md TASK-92〜94・
//! docs/spec/04-behavior/recovery.md RECOVER-1〜3）。
//!
//! [`required_op_id`] が「書き込み系操作は `operation_id` の指定を必須とする」
//! ガード（RECOVER-1）を提供する。`sql::allowlist::validate_insert`（SQL 表層）と
//! `core::EngineCore::{insert_row, update_row, delete_row}`（wire 層が DML を行う際の
//! 想定入口。TASK-95）の両経路が同一ガードを通ることで、SQL 表層に閉じない
//! engine 横断的な必須化を担保する（詳細は `required_op_id` モジュールドキュメント
//! 参照）。

pub mod required_op_id;
