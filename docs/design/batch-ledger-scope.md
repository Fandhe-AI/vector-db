# ADR: バッチ台帳（TABLE-10）の適用範囲（Issue #133）

- ステータス: Accepted
- 対応: Issue #133
- 関連: `docs/spec/05-tasks.md`（TASK-90・TASK-93）・`docs/spec/04-behavior/data-model.md`（TABLE-10）・
  `docs/spec/06-roadmap.md`（MS-5）

## 概要

Issue #133 は、`crates/engine/src/storage.rs` の `BATCH_LOG_TABLE`（バッチ台帳）と
`crates/engine/src/txn.rs` の `BatchWriteTxn`（TASK-90・TABLE-10）に関する既知の制限の
解消要否を検討するものである。検討内容・判断の根拠・不採用にした代替案は private spec
（`docs/spec/05-tasks.md` TASK-90・TASK-93）側で管理する。本ドキュメントは、対応する
公開コード上の参照先を記録するためのポインタである。本リポジトリで公開している設計方針の
範囲は README.md「実装方針（要点）」の通りであり、それを超える内容はここに記載しない。

## 影響を受けるコード

- `crates/engine/src/storage.rs`（`BATCH_LOG_TABLE`・`Storage::put`・`Storage::put_batch`）
- `crates/engine/src/txn.rs`（`BatchWriteTxn` ドキュメントコメント「契約の適用範囲」）
- `crates/engine/tests/cross_table_txn.rs`（`Storage::put` / `Storage::put_batch` と
  `BatchWriteTxn` の混在を対象とするピン留めテスト）

## 参照

- `docs/spec/05-tasks.md`（TASK-90・TASK-93）
- `docs/spec/04-behavior/data-model.md`（TABLE-10）
- `docs/spec/06-roadmap.md`（MS-5）
