# クラッシュ耐性再検証レポート: 電源断シナリオ

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-145（ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-140（`redb` 永続化層、Issue #16 / PR #118 でマージ済み）・TASK-141（PR #120 でマージ済み）
- 関連ビヘイビア: PERSIST-1・PERSIST-3（ポインタ: `docs/spec/04-behavior/persistence.md`）

## 目的

`crates/engine/src/storage.rs` の永続化層について、TASK-145・PERSIST-1・PERSIST-3
（ポインタ: `docs/spec/05-tasks.md`、`docs/spec/04-behavior/persistence.md`）が定める
契約を電源断シナリオへ拡張して再検証し、結果を記録する。契約の具体的な内容は
private spec 側の SSOT を参照すること（本ドキュメントには転記しない）。

## 検証手法とシミュレーションモデル

実機の電源断は CI で再現できないため、`redb` 4.2.0 が公開する `StorageBackend` trait
（`redb::Builder::create_with_backend`）を使い、OS の page cache を決定論的に
シミュレーションするテストハーネス（`crates/engine/tests/power_loss.rs`）を実装した。
新規依存は追加しない（`redb =4.2.0` は既承認・完全固定のまま。乱数はテスト内蔵の
固定シード xorshift64 で代替し、`rand` 等を追加していない）。

### `PowerLossBackend` の構造

- `durable`: 直近の `sync_data()` 完了時点のバイト列（電源断後も必ず残る像）
- `log`: 直近の `sync_data()` 以降に発行された `write()` の記録（発行順）
- 「電源断」= `log` の任意部分集合だけを `durable` に反映した像をスナップショットし、
  それを初期像として新しい `PowerLossBackend` 上に `Database` を開き直すことでモデル化する

### 実装上の発見: write buffer の存在

`redb` は `StorageBackend` の手前に内部の write buffer（キャッシュ）を持ち、
既定の `cache_size`（1GiB）ではテスト規模の小さいトランザクションが
commit（`sync_data()`）まで一切 `StorageBackend::write()` を呼ばないことを実装中に確認した
（`redb` 内部でメモリ完結する）。そのため `Builder::set_cache_size` で意図的に小さい
キャッシュ（16KiB）を設定し、commit 前に write buffer から追い出し（eviction）を
発生させることで、部分的な書き戻しが起きた状態を模した。

### 検証したシナリオ（`crates/engine/tests/power_loss.rs`）

各シナリオが検証する契約内容は TASK-145・PERSIST-1・PERSIST-3（ポインタ上記）を参照。
本節ではテスト関数名と検証対象のみを記す。

| # | 対応するテスト |
| - | -------------- |
| 1 | `power_loss_scenario1_committed_data_survives_crash_after_commit_response` |
| 2 | `power_loss_scenario2_mid_transaction_crash_discards_whole_transaction` |
| 3 | `power_loss_scenario3_partial_writeback_is_either_consistent_or_rejected`（CI・固定シード 32 反復）／ `power_loss_scenario3_partial_writeback_extended_search`（`#[ignore]`・ローカル 2048 反復） |
| 3-否定コントロール | `power_loss_scenario3_corrupted_durable_image_is_rejected`（ハーネス自体の拒否検出能力の確認） |
| 4 | `crates/engine/src/storage.rs` 内 `storage::tests::power_loss::power_loss_scenario4_rls_fields_survive_crash_after_commit`（`#[cfg(test)]` ユニットテスト） |

シナリオ 1〜3・3-否定コントロールは `crates/engine/tests/power_loss.rs`（統合テスト）で
バックエンド差し替え済みの raw `redb::Database`（`redb::Builder::create_with_backend` で開く）を
直接操作する。シナリオ 4 のみ、`crates/engine/src/storage.rs` 内の `#[cfg(test)]` ユニット
テストとして実装されている。統合テスト（`tests/` 配下）は crate 外部からのコンパイル単位のため
`cfg(test)` を使えず、`Storage` の private フィールドへ触れられない。シナリオ 4 は本番の
`Storage::put`/`Storage::get`（＝実際の `encode_row`/`decode_row`）経由での検証が目的のため、
`Storage` の公開 API へバックエンド差し替え用のコンストラクタを追加する代わりに、`storage` モジュールの
子孫である `#[cfg(test)] mod power_loss` から `Storage { db }` の private フィールドへ直接
アクセスする設計とした（`Storage::open`（`redb::Database::create` 固定）は本番の唯一の公開
エントリポイントのまま維持し、公開 API 経由でのバイパス経路を作らない）。

シナリオ 3 の合否判定は fail-closed 原則に従う（判定の具体的な基準は TASK-145 側の
契約を参照。アサーションを弱めたり `#[ignore]` で隠したりしない）。

**構造上の制約**: シナリオ 3 が探索する「部分集合」は、commit に到達しなかった
（`abort` された）トランザクションの `write()` ログに限られる。`redb` は copy-on-write
B-tree のため、そうした書き込みは新規割当ページ（既存のコミット済みルートが一切参照しない
領域）にしか行かれない。したがって本モデルの部分集合探索は **構造的に** コミット済み
ツリーへ干渉できず、`reopen_from_image` の `Err` 分岐（拒否経路）へは到達し得ない。
これは `redb` の頑健性の実証ではなく、本ハーネスの探索空間の限界である
（詳細は「モデルの限界」節）。ハーネス自体が破損を検出できることは、コミット済み電源断像
（`durable_snapshot()`）の先頭 64 バイトを直接反転させる否定コントロール
（`power_loss_scenario3_corrupted_durable_image_is_rejected`）で別途確認しており、
その反転は `reopen_from_image` の `Err` を実際に引き起こす。

## モデルの限界

本シミュレーションはユーザー空間での近似であり、以下は再検証の対象外である
（`crates/engine/tests/power_loss.rs` のモジュール doc コメントにも同内容を明記）:

- 実デバイス（SSD/HDD）のファームウェアレベルの書き込みキャッシュ・並べ替え
- OS カーネルの page cache の実際の write-back 順序（本モデルは「発行順の任意部分集合が
  反映される」という単純化で近似している）
- `set_len`（ファイル長変更）はメタデータ操作として即時 durable 化する単純化を置いている
  （多くのファイルシステムでメタデータジャーナリングは別経路のため、単純化として妥当と判断した）
- シナリオ 3 の部分集合探索（`run_partial_writeback_search`）は、`redb` が copy-on-write
  B-tree であることに起因して、コミット済みツリーへ構造的に干渉できない（上記「検証した
  シナリオ」節参照）。この探索は「フォーマットは正しいがコミット済み内容と異なる像」
  「破損ヘッダによるオープン拒否」といったパターンを原理的に生成できない。拒否経路自体の
  動作確認は否定コントロール（`power_loss_scenario3_corrupted_durable_image_is_rejected`）
  で別途行っている

## 結果

各シナリオの合否とアサーション根拠はテスト実装（`crates/engine/tests/power_loss.rs`・
`crates/engine/src/storage.rs` 内 `mod power_loss`）を SSOT とする。CI 実行分
（`cargo test -p engine`）は全シナリオ・否定コントロールとも合格しており、`#[ignore]`
の拡張反復（`power_loss_scenario3_partial_writeback_extended_search`）もローカルで
合格を確認済みである。`cargo test -p engine`（`--ignored` を含まない）は engine クレート
全体で数秒程度、本ハーネス単体（`--test power_loss`）では 0.2〜0.3 秒程度で完了し、CI
時間への影響は軽微である。

## 観察

- シナリオ 1 が使うテスト用 builder 上の raw `redb::Database` は commit が
  `Durability::Immediate`（既定）で動作しており、`commit()` が返る時点で当該
  トランザクションの書き込みが `sync_data()` まで完了していることをテスト側の
  `pending_write_count()`（バックエンドへの未 sync 書き込み件数）観測で確認した
  （シナリオ 1 のテスト内アサーション）。この観測は raw `redb::Database` に対する
  ものであり、`Storage::open` を経由した検証ではない（「検証範囲の限定」節参照）。
- シナリオ 3 の CI 分・拡張分の実測値（`opened_and_consistent`/`rejected_fail_closed`
  のカウント）はテスト実装自体を参照。`rejected == 0` は、「検証したシナリオ」節・
  「モデルの限界」節に記載の探索空間の構造的制約から導かれる結果であり、`redb` の
  頑健性を実証したものではない。`Err`（拒否）は fail-closed の観点では合格の結果である
  ため、テストは `rejected` の値そのものをアサートせず、各反復で選ばれた部分集合が
  電源断像を実際に変化させたこと（`assert_ne!`）のみを確認し、反復が空検証に縮退
  していないことを保証する。
- 拒否経路（`Err` 分岐）自体がハーネス内で実際に機能することは、シナリオ 3 とは別に
  否定コントロール（`power_loss_scenario3_corrupted_durable_image_is_rejected`）で
  直接確認した。より広範な破損パターンを狙った拒否経路の探索は本再検証のスコープ外の
  まま残る（残リスク節を参照）。

## 結論と残リスク

- **検証範囲の限定**: シナリオ 1・2・3 は raw `redb::Database`（バックエンド差し替えの
  ため `redb::Builder::create_with_backend` で開く）を直接操作しており、`Storage::put`／
  `Storage::get` や `crates/engine/src/storage.rs` の行エンコーダは経由していない
  （「検証したシナリオ」節参照）。したがってシナリオ 1・2・3 の結論は
  **`redb` に対するハーネス検証の範囲**に限定され、`Storage::open` を経由した検証では
  ない。一方シナリオ 4 は、同じ raw `redb::Database` を `crates/engine/src/storage.rs` 内の
  `#[cfg(test)] mod power_loss` から `Storage { db }` の private フィールドへ直接渡し、
  `Storage::put`／`Storage::get`（＝実際の `encode_row`／`decode_row`）を経由して検証して
  いる。`Storage::open`（本番の唯一の公開エントリポイント。`redb::Database::create` 固定）
  自体はバイパスされないため、`Storage::open` を通した電源断耐性そのものは引き続き本
  再検証の対象外である（残リスクとして明示する）。
- **残リスク（未検証）**: `Storage::open`（本番が実際に使う唯一のオープン経路。
  `redb::Database::create` 固定）自体を経由した電源断耐性は本再検証の対象外のまま
  である。`Storage::open` 経由の電源断耐性を検証するには、本番の `open` 経路自体に
  テスト用 backend 注入を許す設計変更が必要であり、それには実装判断とユーザー承認を
  要する。実施する場合は別途 Issue 化してユーザーと合意のうえで対応する
  （out-of-scope-tracking.md 準拠。本 PR では起票しない）。
- 「モデルの限界」節に記載の通り、実デバイスのファームウェア
  キャッシュ・OS page cache の実際の書き戻し順序・複数ページにまたがる非アトミックな
  デバイス書き込みは本再検証の対象外である。これらは `StorageBackend` より下位の層で
  発生し得るため、ユーザー空間シミュレーションでは原理的に再現できない。実電源断・
  実デバイスでの検証が必要な場合は、別途 Issue 化してユーザーと合意のうえで対応する
  （out-of-scope-tracking.md 準拠。本 PR では起票しない）。
- 万一将来のシミュレーション拡張で不変条件違反が観測された場合は、修正を混入させず
  欠陥として記録し、対処は別 Issue とする方針を維持する。

## スコープ外

- 実デバイス・実 OS 環境での電源断試験（残リスク節を参照。別途ユーザー承認のうえ
  Issue 化する）
- `docs/spec` submodule の変更（spec リポ側の作業。本リポからは触らない）
- `Storage::open`（本番が実際に使う唯一のオープン経路）自体への `StorageBackend`
  差し替えフック追加、および公開 API へのバックエンド差し替え用コンストラクタ追加。
  代わりにシナリオ 4 を `crates/engine/src/storage.rs` 内の `#[cfg(test)]` ユニットテスト
  として実装し、private フィールドへの直接構築で `Storage` を得ることで公開 API を
  増やさずに検証した。`Storage::open` 自体は `redb::Database::create` 固定のまま維持した。
  シナリオ 1・2・3 は引き続き `tests/persistence.rs` と同方針で raw `redb::Database` を
  直接操作する
- ヘッダ領域の特定フィールドのみを狙う等、より広範な・体系的な破損パターンでの
  オープン拒否経路の網羅的探索（否定コントロールにより拒否経路自体が機能することは
  確認済みだが、それは先頭 64 バイト反転という 1 パターンの確認に留まる。観察節参照）

## 参照

- TASK-145（ポインタ: `docs/spec/05-tasks.md`）
- PERSIST-1・PERSIST-3（ポインタ: `docs/spec/04-behavior/persistence.md`）
- `crates/engine/tests/power_loss.rs`（シナリオ 1〜3・否定コントロール）
- `crates/engine/src/storage.rs`（シナリオ 4、`mod power_loss`）
