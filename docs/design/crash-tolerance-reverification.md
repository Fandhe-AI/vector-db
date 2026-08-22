# クラッシュ耐性再検証レポート: 電源断シナリオ

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-145（MS-1・基盤・工程管理・並行フォローアップ）
- 前提: TASK-140（`redb` 永続化層、Issue #16 / PR #118 でマージ済み）・TASK-141（PR #120 でマージ済み）
- 対象ビヘイビア: なし（基盤）。ただし PERSIST-1・PERSIST-3
  （ポインタ: `docs/spec/04-behavior/persistence.md`）の不変条件を電源断シナリオへ
  拡張する形で再現している

## 目的

TASK-140 の永続化層（`crates/engine/src/storage.rs`）が、SIGKILL 以外の
OS クラッシュ相当（電源断・fsync タイミング依存のクラッシュ）に対しても
PERSIST-1・PERSIST-3（ポインタ: `docs/spec/04-behavior/persistence.md`）の
不変条件を維持できているかを再検証し、結果を記録する。

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
発生させることで、「commit に到達しない書き込みの一部がバックエンドへ書き戻される」
状況を作った。これは「トランザクション途中でページの一部がディスクへ書き戻されてから
電源断する」という、シナリオ 2・3 が意図する状況に対する妥当な近似である。

### 検証したシナリオ（`crates/engine/tests/power_loss.rs`）

| # | シナリオ | 対応するテスト |
| - | -------- | -------------- |
| 1 | commit 完了（応答済み）後に電源断 → 再オープンでコミット済み行がすべて読める | `power_loss_scenario1_committed_data_survives_crash_after_commit_response` |
| 2 | トランザクション途中（最終 sync 前）で電源断 → 当該トランザクションは丸ごと消え、既存コミット済みデータは無傷 | `power_loss_scenario2_mid_transaction_crash_discards_whole_transaction` |
| 3 | 部分 write-back 像 → 「正常に開けて内容は最後のコミット時点と一致」または「明示的なエラーで開けない」のいずれか（後述の通り、本モデルの探索空間では前者のみが構造的に到達可能） | `power_loss_scenario3_partial_writeback_is_either_consistent_or_rejected`（CI・固定シード 32 反復）／ `power_loss_scenario3_partial_writeback_extended_search`（`#[ignore]`・ローカル 2048 反復） |
| 3-否定コントロール | コミット済み電源断像を直接バイト破損させた場合に拒否経路（`Err`）が実際に機能する（ハーネス自体の拒否検出能力の確認） | `power_loss_scenario3_corrupted_durable_image_is_rejected` |
| 4 | PERSIST-3（ポインタ: `docs/spec/04-behavior/persistence.md`）の不変条件が電源断後も維持される | `power_loss_scenario4_rls_fields_survive_crash_after_commit` |

シナリオ 4 は、バックエンド差し替え済みの raw `redb::Database`（`redb::Builder::create_with_backend`
で開く。シナリオ 1・2・3 と同じ）を、`test-support` feature 限定の
`Storage::from_database_for_testing`（`crates/engine/src/storage.rs`）で本番の `Storage` へ渡し、
書き込み（`Storage::put`）・読み出し（`Storage::get`）ともに本番の `encode_row`/`decode_row`
（同ファイル参照）を経由させる。テスト側で行エンコーディングを複製しないため、電源断前後で
`Storage::get` の結果を比較するだけで、本番エンコーダのフィールド欠落・順序変更・visibility
値誤りも検出できる。実装は `crates/engine/tests/power_loss.rs` の
`power_loss_scenario4_rls_fields_survive_crash_after_commit` を参照。

シナリオ 3 の合格基準は fail-closed 原則に従う: 応答済みコミットの黙示的な消失・
別内容へのすり替わりが 1 件でも観測されれば検証 NG とし、アサーションを弱めたり
`#[ignore]` で隠したりしない（CI 実行分は固定シード・有限反復で実行時間を抑え、
より広い探索は `#[ignore]` 付きのローカル専用テストに分離した）。

**重要な構造上の制約**: シナリオ 3 が探索する「部分集合」は、commit に到達しなかった
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

| シナリオ | CI 実行 | 結果 |
| -------- | ------- | ---- |
| 1（commit 後の電源断） | 常時 | 合格。コミット済み全行が再オープン後に読める |
| 2（トランザクション途中の電源断） | 常時 | 合格。既存コミット済み行は無傷、未コミット行は `NotFound` 相当（存在しない） |
| 3（部分 write-back、CI 分・固定シード 32 反復） | 常時 | 合格。32/32 反復で「正常に開けて行 1 の内容が完全一致」。オープン失敗（fail-closed 拒否）は 0 件（実測値。`cargo test -- --nocapture` で採取）。`Err`（拒否）自体は fail-closed の観点で合格の結果のため、`rejected` の値そのものはアサートしない（各反復では部分集合が実際に電源断像を変化させたことのみを `assert_ne!` で確認する） |
| 3（部分 write-back、拡張・2048 反復、`--ignored`） | ローカルのみ（本 PR の作業時に再実行して確認） | 合格。2048/2048 反復で同上。オープン失敗は 0 件（実行時間は開発機で約 14.5 秒） |
| 3-否定コントロール（コミット済み像の直接バイト破損） | 常時 | 合格。`durable_snapshot()` 先頭 64 バイトを反転させた像は `reopen_from_image` が明示的に `Err` を返す（拒否経路が実際に機能することを確認） |
| 4（PERSIST-3 の電源断耐性） | 常時 | 合格。本番 `Storage::put`/`Storage::get`（実際の `encode_row`/`decode_row`）経由で、電源断前後の読み出し結果が完全一致 |

`cargo test -p engine`（CI 相当、`--ignored` を含まない）は engine クレート全体で
数秒程度で完了する（本ハーネス単体、`--test power_loss` のみでは 0.2〜0.3 秒程度）。
本ハーネスの追加による CI 時間への影響は軽微である。

## 観察

- 既定の `Storage::open`（`redb::Database::create`、`cache_size` 既定 1GiB）は commit が
  `Durability::Immediate`（既定）で動作しており、`commit()` が返る時点で当該
  トランザクションの書き込みが `sync_data()` まで完了していることをテスト側の
  `pending_write_count()`（バックエンドへの未 sync 書き込み件数）観測で確認した
  （シナリオ 1 のテスト内アサーション）。
- シナリオ 3 で「開けたが内容が最後のコミット時点と異なる」ケースは、CI 分・拡張分
  いずれの反復でも観測されなかった（0 件）。`opened_and_consistent`/
  `rejected_fail_closed` をカウントして実測したところ、CI 分・拡張分ともに
  **全反復（32/32・2048/2048）がオープンに成功し、かつ内容一致だった
  （オープン拒否は 0 件）**。ただし、これは `redb` の頑健性を実証した結果ではない。
  「検証したシナリオ」節・「モデルの限界」節に記載の通り、本探索が扱う部分集合（abort
  されたトランザクションの書き込み）は copy-on-write により新規割当ページにしか
  行かれず、コミット済みツリーへ構造的に干渉できないため、`rejected == 0` は
  **この探索空間で必然的に得られる結果**である。`Err`（拒否）は fail-closed の観点では
  合格の結果であるため、テストは `rejected` の値そのものをアサートしない。代わりに、
  各反復で選ばれた部分集合が電源断像を実際に変化させたこと（`assert_ne!`）を確認し、
  反復が空検証に縮退していないことだけを保証する。
- 拒否経路（`Err` 分岐）自体がハーネス内で実際に機能することは、シナリオ 3 とは別に
  否定コントロール（`power_loss_scenario3_corrupted_durable_image_is_rejected`）で
  直接確認した: コミット済み電源断像の先頭 64 バイトを反転させると
  `reopen_from_image` は `Err` を返す。より広範な破損パターン（ヘッダ領域のみが
  破損する等）を狙った拒否経路の探索は本再検証のスコープ外のまま残る
  （残リスク節を参照）。

## 結論と残リスク

- **検証範囲の限定**: シナリオ 1・2・3 は raw `redb::Database`（バックエンド差し替えの
  ため `redb::Builder::create_with_backend` で開く）を直接操作しており、`Storage::put`／
  `Storage::get` や `crates/engine/src/storage.rs` の行エンコーダは経由していない
  （「検証したシナリオ」節参照）。したがってシナリオ 1・2・3 の結論は
  **`redb` に対するハーネス検証の範囲**に限定される。一方シナリオ 4 は、`test-support`
  feature 限定の `Storage::from_database_for_testing`（同ファイル）でこの raw
  `redb::Database` を本番の `Storage` へ渡し、`Storage::put`／`Storage::get`（＝実際の
  `encode_row`／`decode_row`）を経由して検証している。`Storage::open`（本番の唯一の
  公開エントリポイント。`redb::Database::create` 固定）自体はバイパスされないため、
  `Storage::open` を通した電源断耐性そのものは引き続き本再検証の対象外である
  （残リスクとして明示する）。
- **再検証で確認できた範囲**: 本シミュレーションモデルの下では、raw `redb::Database`
  は電源断シナリオ 1・2 において fail-closed（応答済みコミットの黙示的消失・
  すり替わりなし）を維持している。シナリオ 4 は、本番の `Storage::put`／`Storage::get`
  （行エンコーダ含む）を経由したうえで、PERSIST-3 の不変条件が電源断後も維持されて
  いることを確認した。シナリオ 3 については、本モデルの部分集合探索が構造的に
  コミット済みツリーへ干渉できない（「検証したシナリオ」節・「モデルの限界」節
  参照）ため、「開けて内容一致」以外の結果を観測しうる検証にはなっていない。拒否経路
  自体がハーネス内で機能することは否定コントロールで別途確認したが、シナリオ 3 が
  意図した部分 write-back パターンに対する fail-closed 挙動の検証としては、
  本モデルの探索空間の限界により確認できていない。
- **残リスク（未検証）**: `Storage::open`（本番が実際に使う唯一のオープン経路。
  `redb::Database::create` 固定）自体を経由した電源断耐性は本再検証の対象外のまま
  である。シナリオ 4 で `Storage::put`／`Storage::get`・行エンコーダは検証範囲に
  含めたが、`Storage::open` の代わりに `Storage::from_database_for_testing`
  （バックエンド差し替え済み `redb::Database` を受け取る `test-support` feature 限定の
  コンストラクタ）を使っており、`Storage::open` 自体は差し替え不能なまま維持している
  （「スコープ外」節参照）。`Storage::open` 経由の電源断耐性を検証するには、本番の
  `open` 経路自体にテスト用 backend 注入を許す設計変更が必要であり、それには実装判断と
  ユーザー承認を要する。実施する場合は別途 Issue 化してユーザーと合意のうえで対応する
  （out-of-scope-tracking.md 準拠。本 PR では起票しない）。
- 「モデルの限界」節に記載の通り、実デバイスのファームウェア
  キャッシュ・OS page cache の実際の書き戻し順序・複数ページにまたがる非アトミックな
  デバイス書き込みは本再検証の対象外である。これらは `StorageBackend` より下位の層で
  発生し得るため、ユーザー空間シミュレーションでは原理的に再現できない。実電源断・
  実デバイスでの検証が必要な場合は、別途 Issue 化してユーザーと合意のうえで対応する
  （out-of-scope-tracking.md 準拠。本 PR では起票しない）。
- 万一将来のシミュレーション拡張で不変条件違反（応答済みコミットの消失等）が
  観測された場合は、修正を混入させず欠陥として記録し、対処は別 Issue とする方針を
  維持する。

## スコープ外

- 実デバイス・実 OS 環境での電源断試験（残リスク節を参照。別途ユーザー承認のうえ
  Issue 化する）
- `docs/spec` submodule の変更（spec リポ側の作業。本リポからは触らない。
  spec 側の宿題記述の消し込みが必要な場合は spec リポ側の課題としてユーザーに
  別途報告する）
- `Storage::open`（本番が実際に使う唯一のオープン経路）自体への `StorageBackend`
  差し替えフック追加。代わりに `test-support` feature 限定の別コンストラクタ
  `Storage::from_database_for_testing` を追加し、`Storage::open` 自体は
  `redb::Database::create` 固定のまま維持した。シナリオ 1・2・3 は引き続き
  `tests/persistence.rs` と同方針で raw `redb::Database` を直接操作する
- ヘッダ領域の特定フィールドのみを狙う等、より広範な・体系的な破損パターンでの
  オープン拒否経路の網羅的探索（否定コントロールにより拒否経路自体が機能することは
  確認済みだが、それは先頭 64 バイト反転という 1 パターンの確認に留まる。観察節参照）

## 参照

- `docs/spec/05-tasks.md`（TASK-145・TASK-140・TASK-141）
- `docs/spec/04-behavior/persistence.md`（PERSIST-1・PERSIST-3）
- `crates/engine/src/storage.rs`（永続化層本体。本再検証でシナリオ 4 検証用の
  `test-support` feature 限定コンストラクタ `Storage::from_database_for_testing` を
  追加した。`Storage::open` 自体は変更していない）
- `crates/engine/tests/power_loss.rs`（本再検証のテストハーネス・シナリオ実装）
- `crates/engine/tests/persistence.rs`（PERSIST-1/2/3/4 の通常系テスト。同じ
  「`redb` を直接操作する」方針を踏襲した参照実装）
