# ADR: バッチ台帳（TABLE-10）の適用範囲と書き込み経路非統合の判断

- ステータス: Accepted
- 対応: Issue #133
- 前提: TASK-90（PR #129 でバッチ台帳の型分離・契約範囲の明文化を実施済み）・TASK-140・TASK-88
- 関連: `docs/spec/05-tasks.md`（TASK-90・TASK-93）・`docs/spec/04-behavior/data-model.md`（TABLE-10）・
  `docs/spec/06-roadmap.md`（MS-5）

## 背景

`crates/engine/src/storage.rs` の `BATCH_LOG_TABLE`（バッチ台帳）と
`crates/engine/src/txn.rs` の `BatchWriteTxn` は、TASK-90（対象ビヘイビア: TABLE-10）に基づき
実装・明文化されている（PR #129）。`Storage::put` / `Storage::put_batch` / `WriteTxn::put` は
台帳を経由しない独立経路であり、これは PR #129 時点で既知の制限としてドキュメント化され、
ピン留めテスト（`crates/engine/tests/cross_table_txn.rs` の
`table10_mixing_plain_write_txn_with_batch_write_txn_is_a_documented_out_of_contract_limitation`）
で回帰検出されている。Issue #133 は、この制限を MS-5（障害回復）の要件に照らして解消すべきか、
それとも契約として恒久化すべきかの方針判断を求めるものである。

調査で確認した事実・検討した選択肢・不採用理由の詳細は private spec
（`docs/spec/05-tasks.md` TASK-90・TASK-93、`docs/spec/04-behavior/data-model.md` TABLE-10、
`docs/spec/06-roadmap.md` MS-5）側の該当タスクを参照する。本リポジトリで公開している設計方針の
範囲は README.md「実装方針（要点）」の通りである。

## 判断（Accepted）

書き込み経路は統合せず、バッチ台帳（TABLE-10）の契約適用範囲を現状のまま恒久化する。
判断の根拠・検討した代替案は private spec 側（TASK-90・TASK-93）を参照する。

## 影響

- 製品コードの挙動変更はなし。`Storage::put` / `Storage::put_batch` / `WriteTxn` /
  `BatchWriteTxn` の実装本体は変更しない。
- クラッシュ耐性回帰（`make crash-test-cross-table`）はフレッシュ DB に対して
  `BatchWriteTxn` のみで書き込むため影響を受けない。
- `crates/engine/src/txn.rs`・`crates/engine/src/storage.rs` のドキュメンテーションコメントを、
  「本 PR のスコープでは正当化されない」という暫定表現から「恒久契約（本 ADR 参照）」へ改める。
- `crates/engine/tests/cross_table_txn.rs` に、`Storage::put` / `Storage::put_batch` と
  `BatchWriteTxn` の混在を明示的にピン留めするテストを追加する（既存は `WriteTxn` との混在の
  みカバーしていた）。

## スコープ外

- TABLE-10 の契約適用範囲拡大（O(1) 混在検出等）の実装: 本 ADR の判断により見送り。将来の
  再評価要否は private spec 側（TASK-90・TASK-93）の管理下とする。
- `docs/spec` submodule の変更（spec リポ側の作業。本リポからは触らない）。
- TASK-93 系（`operation_id` 台帳・MS-5 障害回復）の設計・実装そのもの。

## 参照

- `docs/spec/05-tasks.md`（TASK-90・TASK-93）
- `docs/spec/04-behavior/data-model.md`（TABLE-10）
- `docs/spec/06-roadmap.md`（MS-5）
- `crates/engine/src/storage.rs`（`BATCH_LOG_TABLE`・`Storage::put`・`Storage::put_batch`）
- `crates/engine/src/txn.rs`（`BatchWriteTxn` ドキュメントコメント「契約の適用範囲」）
- `crates/engine/tests/cross_table_txn.rs`（ピン留めテスト）
