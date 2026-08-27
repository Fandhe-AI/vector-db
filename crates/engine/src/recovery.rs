//! TASK-92（対象ビヘイビア: RECOVER-1）の入口モジュール。障害回復系のガード・台帳の
//! 実装をここへ集約する（ポインタ: docs/spec/05-tasks.md TASK-92〜94・TASK-96・
//! docs/spec/04-behavior/recovery.md RECOVER-1〜3・RECOVER-5）。
//!
//! [`required_op_id`] が `operation_id` 必須化ガード（RECOVER-1）を提供する
//! （詳細は `required_op_id` モジュールドキュメント参照）。[`ledger`] がテーブル単位
//! `operation_id` 台帳（TASK-93、対象ビヘイビア: RECOVER-2）を提供する（詳細は
//! `ledger` モジュールドキュメント参照）。[`dedup`] が台帳の照会結果を重複拒否へ
//! 写像する（TASK-94、対象ビヘイビア: RECOVER-3。詳細は `dedup` モジュール
//! ドキュメント参照）。[`commit_boundary`] が commit 成功境界と応答一意性の保証
//! （TASK-96、対象ビヘイビア: RECOVER-5）を提供する（詳細は `commit_boundary`
//! モジュールドキュメント参照）。

pub mod commit_boundary;
pub mod dedup;
pub mod ledger;
pub mod required_op_id;
