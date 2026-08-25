# ADR: バッチ台帳（TABLE-10）の適用範囲と書き込み経路非統合の判断

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: Issue #133
- 前提: TASK-90（PR #129 でバッチ台帳の型分離・契約範囲の明文化を実施済み）・TASK-140・TASK-88
- 関連: `docs/spec/05-tasks.md`（TASK-90・TASK-93）・`docs/spec/04-behavior/data-model.md`（TABLE-10）・
  `docs/spec/06-roadmap.md`（MS-5）

## 背景

`crates/engine/src/storage.rs` の `BATCH_LOG_TABLE`（バッチ台帳）と
`crates/engine/src/txn.rs` の `BatchWriteTxn` は、TASK-90（対象ビヘイビア: TABLE-10）で
「台帳の row_count 合計 == `ROWS_TABLE` の行総数」という不変条件を、**`BatchWriteTxn` だけを
使って書き込んだ場合に限り**保証する契約として実装・明文化されている（PR #129）。
一方 `Storage::put` / `Storage::put_batch` / `WriteTxn::put` は台帳を経由しない独立経路であり、
DB 全体の不変条件としては型システムでは強制されない。これは PR #129 時点で既知の制限として
ドキュメント化され、ピン留めテスト
（`crates/engine/tests/cross_table_txn.rs` の
`table10_mixing_plain_write_txn_with_batch_write_txn_is_a_documented_out_of_contract_limitation`）
で回帰検出されている。Issue #133 は、この制限を MS-5（障害回復）の要件に照らして解消すべきか、
それとも契約として恒久化すべきかの方針判断を求めるものである。

### 調査で確認した事実

- バッチ台帳は TASK-90（TABLE-10）のクラッシュ耐性オラクル用途で導入された。書き込み側は
  `BatchWriteTxn::log_batch` のみ、検証側は `crates/engine/examples/crash_tool_cross_table.rs` の
  `verify`（台帳合計 == 行総数）と `scripts/crash_test_cross_table.sh`。
- 製品の行書き込み経路（`crates/engine/src/catalog.rs` の `insert_rows_into_table` /
  `insert_typed_row` / `insert_row_into_table`、TASK-146・TASK-75）は `user_rows/<table>` の
  per-table 行テーブルへ生の `begin_write` で書いており、`ROWS_TABLE` にも `BATCH_LOG_TABLE` にも
  触れない。
- `Storage::put` / `Storage::put_batch` の製品コード内呼び出しは無い（利用はテスト・examples・
  ベンチのみ）。`put_batch` は TASK-143（PERSIST-2）の増分書き込み性能回帰テストの計測対象
  そのものである（`crates/engine/tests/incremental_write_perf.rs`）。
- MS-5 の障害回復契約は `operation_id` 台帳（TASK-93。`storage.rs` に既にポインタ公開済み）を
  中心とする別系統の台帳であり、TABLE-10 のバッチ台帳（`batch_seq → row_count`）を再利用・
  前提にしていない。
- `txn.rs` の従来コメントにあった「混在検出には DB 全体のスキャンを要する」という根拠は
  不正確だった。`redb` の `Table::len()` は B-tree ルートヘッダの長さフィールドを返す O(1) 操作
  であり、スキャンは不要である。
- `ROWS_TABLE` に対する削除 API は現状存在しない。

## 検討した選択肢

### A. 全経路の台帳連携（自動採番で書き込み経路を統合する）

`Storage::put` / `put_batch` / `WriteTxn::put` からもバッチ台帳へ自動採番で記録する設計。
**不採用**: `put_batch` は PERSIST-2 の性能回帰テストの計測対象であり、台帳連携
（追加テーブル書き込み・採番）は計測条件を変えてしまう。また `BatchWriteTxn::log_batch` の
呼び出し元採番（クラッシュツールの 0 起点連番オラクル）と自動採番が衝突する設計変更を伴い、
現時点の基盤に対して正当化できる規模ではない。

### B. `BatchWriteTxn::commit` での O(1) 混在検出

`ROWS_TABLE.len()` と台帳の累計値を比較し、不一致なら fail-closed に拒否する案。
`redb::Table::len()` が O(1) であるため技術的な実装コストは低い。**本 Issue では不採用**
（将来 TASK-93 系の台帳設計を見直す際の再評価候補として残す）。理由:

- 台帳累計を別途保持するメタキーの管理が新たに必要になる。
- `ROWS_TABLE` に削除 API が追加された場合、削除分を台帳累計から差し引く設計が伴わないと
  誤検出（正当な削除後の commit を不当に拒否）を起こし得る。
- 保護対象は製品書き込み経路ではなくテスト・ツール経路のみであり、費用対効果が低い。

### C. `Storage::put` / `put_batch` を `WriteTxn` へ委譲する純リファクタ

挙動は変えずに実装を共通化する案。**不採用**: `put_batch` の現状実装は 1 回の `open_table` で
複数行を書き込むが、`WriteTxn::put` へ委譲すると呼び出しのたびに `open_table` するかたちに
変わりうる。PERSIST-2 の計測対象に影響し得るため、現状の実装を維持する。

### D. 契約の恒久化（採用）

書き込み経路は統合せず、「台帳の row_count 合計 == 行総数」という不変条件の適用範囲を
「あるテーブルへの書き込みを `BatchWriteTxn` だけで行った場合」に固定したまま、ドキュメントと
ピン留めテストで維持する。

## 判断（Proposed）

**選択肢 D（恒久化）を採用する。DB 全体でのバッチ台帳整合は不要と判断し、書き込み経路の統合は
行わない。**

根拠:

1. MS-5 の障害回復契約が要求する台帳は `operation_id` 台帳（TASK-93 以降）であり、TABLE-10 の
   バッチ台帳とは目的・キー・スコープが異なる別テーブルとして実装される。バッチ台帳を DB 全体の
   不変条件へ昇格させても MS-5 の受入には寄与しない。
2. 製品書き込み経路（`catalog.rs` の per-table 行テーブル）は `ROWS_TABLE` / バッチ台帳を
   経由しないため、`Storage::put` / `put_batch` を台帳連携させても保護対象はテスト・ツール
   経路のみである。
3. 選択肢 A・B・C はいずれも、上記の保護範囲の狭さに対して実装・運用コストが見合わない
   （詳細は各節）。

## 影響

- 製品コードの挙動変更はなし。`Storage::put` / `Storage::put_batch` / `WriteTxn` /
  `BatchWriteTxn` の実装本体は変更しない。
- クラッシュ耐性回帰（`make crash-test-cross-table`）はフレッシュ DB に対して
  `BatchWriteTxn` のみで書き込むため影響を受けない。
- `crates/engine/src/txn.rs`・`crates/engine/src/storage.rs` のドキュメンテーションコメントを、
  「本 PR のスコープでは正当化されない」という暫定表現から「恒久契約（本 ADR 参照）」へ改め、
  不正確だった検出コストの根拠（DB 全体スキャンが必要）を選択肢 B の評価内容に差し替える。
- `crates/engine/tests/cross_table_txn.rs` に、`Storage::put` / `Storage::put_batch` と
  `BatchWriteTxn` の混在を明示的にピン留めするテストを追加する（既存は `WriteTxn` との混在の
  みカバーしていた）。

## 将来の再評価条件

以下のいずれかが生じた場合、本判断（特に選択肢 B の不採用）を再評価する。

- `ROWS_TABLE` が製品書き込み経路の書き込み先になる場合。
- TASK-93 系の台帳設計で DB 全体の整合オラクルが新たに必要になる場合。
- `ROWS_TABLE` に削除 API が追加される場合。

その際は選択肢 B（`Table::len()` を用いた O(1) 混在検出）を第一候補として再評価する。

## スコープ外

- 選択肢 B（O(1) 混在検出）の実装: 本 ADR の判断により見送り。将来の再評価条件に該当した
  時点で改めて Issue 化を検討する。
- `docs/spec` submodule の変更（spec リポ側の作業。本リポからは触らない）。
- TASK-93 系（`operation_id` 台帳・MS-5 障害回復）の設計・実装そのもの。

## 参照

- `docs/spec/05-tasks.md`（TASK-90・TASK-93）
- `docs/spec/04-behavior/data-model.md`（TABLE-10）
- `docs/spec/06-roadmap.md`（MS-5）
- `crates/engine/src/storage.rs`（`BATCH_LOG_TABLE`・`Storage::put`・`Storage::put_batch`）
- `crates/engine/src/txn.rs`（`BatchWriteTxn` ドキュメントコメント「契約の適用範囲」）
- `crates/engine/tests/cross_table_txn.rs`（ピン留めテスト）
