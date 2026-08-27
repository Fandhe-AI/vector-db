//! TASK-92（対象ビヘイビア: RECOVER-1）の入口モジュール。障害回復系のガード・台帳の
//! 実装をここへ集約する（ポインタ: docs/spec/05-tasks.md TASK-92〜94・TASK-96・
//! TASK-101・docs/spec/04-behavior/recovery.md RECOVER-1〜3・RECOVER-5・
//! RECOVER-10）。
//!
//! [`required_op_id`] が `operation_id` 必須化ガード（RECOVER-1）を提供する
//! （詳細は `required_op_id` モジュールドキュメント参照）。[`ledger`] がテーブル単位
//! `operation_id` 台帳（TASK-93、対象ビヘイビア: RECOVER-2）を提供する（詳細は
//! `ledger` モジュールドキュメント参照）。[`content_hash`] が台帳エントリの内容照合
//! ハッシュ（TASK-101、対象ビヘイビア: RECOVER-10）を提供する（詳細は
//! `content_hash` モジュールドキュメント参照）。重複拒否（`23505`、TASK-94・
//! RECOVER-3）は独立モジュールを持たず、[`ledger::record_in_txn`] が内容ハッシュ照合
//! の一部として（一致なら `23505`・不一致なら `22023`）直接判定する。
//! [`commit_boundary`] が commit 成功境界と応答一意性の保証（TASK-96、対象
//! ビヘイビア: RECOVER-5）を提供する（詳細は `commit_boundary` モジュール
//! ドキュメント参照）。

pub mod commit_boundary;
pub(crate) mod content_hash;
pub mod ledger;
pub mod required_op_id;
