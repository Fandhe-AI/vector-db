//! TASK-92（対象ビヘイビア: RECOVER-1）の入口モジュール。障害回復系のガード・台帳の
//! 実装をここへ集約する（後続 TASK-93・TASK-94。ポインタ: docs/spec/05-tasks.md
//! TASK-92〜94・docs/spec/04-behavior/recovery.md RECOVER-1〜3）。
//!
//! [`required_op_id`] が `operation_id` 必須化ガード（RECOVER-1）を提供する
//! （詳細は `required_op_id` モジュールドキュメント参照）。

pub mod required_op_id;
