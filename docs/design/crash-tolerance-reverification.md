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
PERSIST-1（コミット済みデータの生存・未コミットの破棄）・PERSIST-3（RLS フィールドの
永続化）を維持できているかを再検証し、結果を記録する。

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
| 3 | 部分 write-back 像 → 「正常に開けて内容は最後のコミット時点と一致」または「明示的なエラーで開けない」のいずれか | `power_loss_scenario3_partial_writeback_is_either_consistent_or_rejected`（CI・固定シード 32 反復）／ `power_loss_scenario3_partial_writeback_extended_search`（`#[ignore]`・ローカル 2048 反復） |
| 4 | RLS フィールド（tenant_id・visibility）が v2 行レイアウト内に同居した状態のまま電源断後も無傷で保持される | `power_loss_scenario4_rls_fields_survive_crash_after_commit` |

シナリオ 4 は `Storage` の公開 API がバックエンド差し替えに対応していないため、
`crates/engine/src/storage.rs` の v2 行レイアウトをテストローカルに再現したバイト列
（`tests/persistence.rs` の `persist3_rls_fields_are_colocated_in_single_row_entry_not_a_separate_table`
と同じ手法）を raw `redb::Database` へ直接書き込み、再オープン後に tenant_id・visibility
バイトの位置を検査する。当初案では人間可読な文字列を不透明ペイロードとして書き込む
簡易版だったが、それでは実質シナリオ 1 の再検証にしかならず PERSIST-3（RLS フィールドの
行内同居）を検証していなかったため、v2 レイアウトを模した構成に修正した。

シナリオ 3 の合格基準は fail-closed 原則に従う: 応答済みコミットの黙示的な消失・
別内容へのすり替わりが 1 件でも観測されれば検証 NG とし、アサーションを弱めたり
`#[ignore]` で隠したりしない（CI 実行分は固定シード・有限反復で実行時間を抑え、
より広い探索は `#[ignore]` 付きのローカル専用テストに分離した）。

## モデルの限界

本シミュレーションはユーザー空間での近似であり、以下は再検証の対象外である
（`crates/engine/tests/power_loss.rs` のモジュール doc コメントにも同内容を明記）:

- 実デバイス（SSD/HDD）のファームウェアレベルの書き込みキャッシュ・並べ替え
- OS カーネルの page cache の実際の write-back 順序（本モデルは「発行順の任意部分集合が
  反映される」という単純化で近似している）
- `set_len`（ファイル長変更）はメタデータ操作として即時 durable 化する単純化を置いている
  （多くのファイルシステムでメタデータジャーナリングは別経路のため、単純化として妥当と判断した）

## 結果

| シナリオ | CI 実行 | 結果 |
| -------- | ------- | ---- |
| 1（commit 後の電源断） | 常時 | 合格。コミット済み全行が再オープン後に読める |
| 2（トランザクション途中の電源断） | 常時 | 合格。既存コミット済み行は無傷、未コミット行は `NotFound` 相当（存在しない） |
| 3（部分 write-back、CI 分・固定シード 32 反復） | 常時 | 合格。32/32 反復で「正常に開けて行 1 の内容が完全一致」。オープン失敗（fail-closed 拒否）は 0 件（実測値。`cargo test -- --nocapture` で採取） |
| 3（部分 write-back、拡張・2048 反復、`--ignored`） | ローカルのみ（本 PR の作業時に 1 回実行） | 合格。2048/2048 反復で同上。オープン失敗は 0 件（実行時間は開発機で約 15 秒） |
| 4（RLS フィールドの電源断耐性） | 常時 | 合格。tenant_id・visibility バイトが v2 行レイアウトの期待オフセットのまま電源断後も無傷 |

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
  （オープン拒否は 0 件）**。本モデルの部分 write-back パターン（発行順の任意部分集合を
  反映）の範囲では、`redb` は黙示的な消失・すり替わりを起こさずに常に一貫した状態へ
  復元できている、という結果になった。ただし、この「常にオープンに成功する」という
  結果自体が「モデルの限界」節に記載の単純化（`set_len` の即時 durable 化・部分集合
  適用が発行順を保つ近似）に依存しており、より広範な破損パターン
  （ヘッダ領域のみが破損する等）を狙って構成すればオープン拒否が発生し得る余地は残る
  （残リスク節を参照）。

## 結論と残リスク

- **再検証で確認できた範囲**: 本シミュレーションモデルの下では、TASK-140/TASK-141 の
  永続化層は電源断シナリオ 1・2・3・4 のいずれにおいても fail-closed
  （応答済みコミットの黙示的消失・すり替わりなし）を維持している。
- **残リスク（未検証）**: 「モデルの限界」節に記載の通り、実デバイスのファームウェア
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
- `Storage::open` への `StorageBackend` 差し替えフック追加（プロダクションコードへの
  test 用フック追加は行わず、テストは `redb` を直接操作する既存前提を踏襲した。
  `tests/persistence.rs` と同方針）
- ヘッダ領域のみが破損する等、より狙った破損パターンでのオープン拒否経路の探索
  （本再検証の部分集合適用モデル・実測反復ではオープン拒否が 0 件だったため、
  fail-closed 経路自体が正しく機能しているかは今回のシナリオ 3 の範囲では
  確認できていない。観察節参照）

## 参照

- `docs/spec/05-tasks.md`（TASK-145・TASK-140・TASK-141）
- `docs/spec/04-behavior/persistence.md`（PERSIST-1・PERSIST-3）
- `crates/engine/src/storage.rs`（永続化層本体。本再検証では変更していない）
- `crates/engine/tests/power_loss.rs`（本再検証のテストハーネス・シナリオ実装）
- `crates/engine/tests/persistence.rs`（PERSIST-1/2/3/4 の通常系テスト。同じ
  「`redb` を直接操作する」方針を踏襲した参照実装）
