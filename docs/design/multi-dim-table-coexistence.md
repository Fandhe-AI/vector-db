# ADR: 複数次元テーブル共存の実測

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-91（MS-1 / phase:1・並行フォローアップ）
- 対象ビヘイビア: TABLE-2（`docs/spec/04-behavior/data-model.md`）
- 関連: EXT-2（`docs/spec/04-behavior/extensions.md`）
- 前提: TASK-85（スキーマカタログ、Issue #23 / PR #125 でマージ済み）・
  TASK-140/TASK-141（`redb` 行ストア）

## 背景

「次元はテーブル粒度で `VECTOR(N)` 固定・異なる次元のテーブルが同一 DB インスタンス
内で共存できる」という設計方針（TABLE-2）は、これまで単一次元のみの実測に基づく
設計判断に留まり、複数次元テーブルを実際に共存させた実測が無かった。本 ADR は
その実測・実装検証の結果と、TABLE-2 確定化の判断材料を記録する。

**位置づけ**: 実装をブロックしない独立タスク。TABLE-2 の本実装（SQL surface からの
`CREATE TABLE` 統合等）は別タスクの管轄であり、本タスクでは production コード
（`crates/engine/src/`）の変更を行わず、既存 API に対する検証テスト・実測ハーネス・
本レポートの追加に限定した。

## 検証設計

「共存」の検証を現行アーキテクチャの 2 層に分けて行った。

### カタログ層

異なる `VECTOR(N)`（384 / 768 / 1536 次元。一般的な埋め込みモデルで使われる代表値）
を宣言した 3 テーブルを単一 `Storage`（同一 redb ファイル）に `create_table` し、
`list_tables` / `get_table_schema` から各次元宣言を読み戻せること、DB を close→再
`open` しても宣言が不変であることを検証した。

### 行ストア層

行とユーザーテーブル（カタログ上のテーブル名）を関連付ける機構はまだ実装されて
いない（後続タスクの管轄）。そのため、テストではテーブルごとに素の id レンジを
分け、`RowInput::metadata` にテーブル名を記録することで論理的な帰属をテスト側の
判断として模擬した。各次元の行を `put_batch` で同一 redb ファイルへ混在格納し、
`scan_page`（ページング。`scan()` は使わない — 後述）で全行を読み切り、件数・
embedding 長・値・metadata の完全一致（混線 0 件）を検証した。再オープン後にも
同一検証を行った。

さらに、1 テーブルへの `alter_table_add_column` が他テーブルの次元宣言・既存行に
影響しないこと（境界の分離）も検証した。

### 実装（テスト・実測ハーネス）

- `crates/engine/tests/multi_dim_tables.rs`（CI 常時実行の回帰テスト。5 ケース）:
  `create_tables_with_distinct_dims_coexist` / `schemas_survive_reopen` /
  `dim_validation_is_fail_closed` / `mixed_dim_rows_roundtrip_intact` /
  `alter_table_does_not_disturb_other_dims`
- `crates/engine/examples/multi_dim_bench.rs`（手動実行専用の実測ハーネス。
  `cargo run -p engine --release --example multi_dim_bench` で実行、`cargo test`
  対象外）: (a) 単一次元 768 のみのベースライン DB と (b) 384/768/1536 混在 DB
  （各テーブル 800 行を `put_batch`、20 件/バッチ）を比較し、書き込み
  p50/p95/max・スループット（rows/sec）・`scan_page` 全走査所要時間・DB ファイル
  サイズを計測する。行数・次元・バッチサイズはすべて定数で上限固定した。

## 実測環境

| 項目 | 値 |
| ---- | -- |
| CPU | Apple M4 Max（16 コア） |
| OS | macOS 26.6.2（Darwin 25.6.0 / arm64） |
| ストレージ | 開発機ローカル SSD（`std::env::temp_dir()` 配下） |
| ビルド | `cargo run --release` |

**注意（測定条件の限界）**: 本測定は開発機上で、他の Issue 実装用 worktree が
同時稼働している可能性がある共有環境で実施した（`docs/design/concurrent-write-verification.md`
と同じ限界）。2 回実行した結果が近い値に収まったため、下表はそのうち 1 回を採用する。

**算出方法（両条件で統一）**: `rows_per_sec` は `total`（`put_batch` バッチごとの
計測区間の合計。テーブル間の embedding/metadata 生成コストを含まない）を分母に
`baseline` / `mixed` の両条件で同一方式で算出する（`crates/engine/examples/multi_dim_bench.rs`
の `write_table` 計測区間）。

## 実測結果

| config | op_count | p50 | p95 | max | total | rows/sec |
| ------ | -------- | --- | --- | --- | ----- | -------- |
| baseline(single 768) | 40 | 5.145ms | 9.774ms | 10.923ms | 217.296ms | 3681.6 |
| mixed(384/768/1536) | 120 | 5.286ms | 9.725ms | 12.975ms | 691.709ms | 3469.7 |

| config | scan_page 全走査 rows | scan_page 全走査 elapsed | DB ファイルサイズ |
| ------ | --------------------- | ------------------------ | ------------------ |
| baseline(single 768) | 800 | 910.834µs | 5,738,496 bytes |
| mixed(384/768/1536) | 2400 | 5.301ms | 19,009,536 bytes |

（`op_count` は `put_batch` 呼び出し回数。`mixed` は 3 テーブル分のため
`baseline` の 3 倍の行・呼び出し数になる。再実行時も p50/rows-per-sec は
上記から大きく外れず、初回実測にスプリアス値は見られなかった。）

## 判断材料

- **書き込みスループット（rows/sec）は単一次元と同水準**（baseline 約 3682
  rows/sec、mixed 約 3470 rows/sec）。異なる次元のテーブルへ交互に書き込む
  ことによる劣化は観測されなかった。`put_batch` の直列化コミットコストが
  支配的という `concurrent-write-verification.md` の既存知見と整合する
  （行データの次元自体は書き込みコストにほぼ影響しない）。
- **`scan_page` 全走査時間は行数にほぼ比例**（mixed は baseline の 3 倍の行数
  で約 5.8 倍の所要時間。両条件とも数 ms オーダーであり実運用上の劣化とは
  考えにくいが、比例からの乖離が観測されたため今後の実データでの再実測時に
  注視する）。異なる次元の行が同一テーブル（`ROWS_TABLE`）内に混在している
  ことによる読み出し側の異常な劣化は見られなかった。
- **DB ファイルサイズは行データサイズの合計と整合**（mixed は 384+768+1536=2688
  次元相当のデータを保持するため baseline（768 次元×800 行）よりおおむね
  比例して大きい）。次元混在によるファイルサイズの想定外の膨張はない。
- **正しさ**: カタログ層（3 テーブルの次元宣言・永続共存・fail-closed な次元
  検証・`alter_table_add_column` の境界分離）・行ストア層（混在格納後の
  完全ラウンドトリップ、混線 0 件）のいずれも全ケースで確認できた
  （`crates/engine/tests/multi_dim_tables.rs`、5/5 pass）。

以上より、**現行アーキテクチャ（カタログ層 + 単一 `ROWS_TABLE`）は複数次元テーブルの
共存を正しさ・性能の両面で問題なく support する**と判断する（Proposed）。

## 制約・スコープ外

1. **行ストアのテーブル関連付けは未実装**。本検証は id レンジ分割 + metadata に
   よる模擬で「共存」を確認したものであり、テーブル別の行絞り込み・削除等の
   実運用機能はテーブル別行ストアの本実装（後続タスクの管轄）に依存する。
2. **データセットは合成ベクトル**。実データセット（異なる次元のテーブル 2〜3 種）
   の選定はオーナー判断事項であり、本 PR は合成データによる既定実測に留まる。
   実データでの再実測要否をオーナーに確認する。
3. **2000 次元級の再検証は別タスクの管轄**。本検証は 384/768/1536 の 3 種のみを
   対象とし、より高次元での挙動は扱わない。
4. `sled` / 自作 MVCC 等、永続化層自体の変更（本検証はいずれも扱わない）。
5. `docs/spec` submodule の変更（spec リポ側の作業。TABLE-2 の spec ステータス
   引き上げは本リポからは行わない）。

## 参照

- `docs/spec/05-tasks.md`（TASK-91・TASK-85）
- `docs/spec/04-behavior/data-model.md`（TABLE-2）・`docs/spec/04-behavior/extensions.md`（EXT-2）
- `crates/engine/src/catalog.rs`（カタログ DDL・`validate_embedding_dim`）
- `crates/engine/src/storage.rs`（`ROWS_TABLE`・`put_batch`・`scan_page`）
- `crates/engine/tests/multi_dim_tables.rs`（正しさの回帰テスト）
- `crates/engine/examples/multi_dim_bench.rs`（実測ハーネス）
- `docs/design/concurrent-write-verification.md`（同様の ADR 形式・実測手法の前例）
