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
  （各テーブル 800 行、20 件/バッチ。mixed 側は 3 テーブルをバッチ単位で巡回
  ［round-robin］しながら `put_batch` する。テーブルごとに全バッチを書き終えて
  から次のテーブルへ進む連続書き込みにはしない）を比較し、書き込み
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
| baseline(single 768) | 40 | 6.027ms | 9.361ms | 12.786ms | 247.506ms | 3232.2 |
| mixed(384/768/1536) | 120 | 5.722ms | 7.249ms | 13.495ms | 721.610ms | 3325.9 |

| config | scan_page 全走査 rows | scan_page 全走査 elapsed | DB ファイルサイズ |
| ------ | --------------------- | ------------------------ | ------------------ |
| baseline(single 768) | 800 | 2.952ms | 5,738,496 bytes |
| mixed(384/768/1536) | 2400 | 6.229ms | 19,664,896 bytes |

（`op_count` は `put_batch` 呼び出し回数。`mixed` は 3 テーブル分のため
`baseline` の 3 倍の行・呼び出し数になり、かつ 3 テーブルをバッチ単位で
巡回［round-robin］しながら書き込む（テーブルごとに全バッチを書き終えて
から次へ進む連続書き込みではない）。再実行時も p50/rows-per-sec は
上記から大きく外れず、初回実測にスプリアス値は見られなかった。）

## 判断材料

- **書き込みスループット（rows/sec）は単一次元と同水準**（baseline 約 3232
  rows/sec、mixed 約 3326 rows/sec）。3 テーブルをバッチ単位で巡回
  （round-robin）しながら異なる次元のテーブルへ交互に書き込んでも劣化は
  観測されなかった。`put_batch` の直列化コミットコストが支配的という
  `concurrent-write-verification.md` の既存知見と整合する（行データの次元
  自体は書き込みコストにほぼ影響しない）。
- **`scan_page` 全走査時間は行数の増加とおおむね比例**（mixed は baseline
  の 3 倍の行数に対し約 2.1 倍の所要時間）。絶対値は両条件とも数 ms オーダー
  であり実運用上の懸念は小さい。
- **DB ファイルサイズは行データサイズの合計と整合**（mixed は 384+768+1536=2688
  次元相当のデータを保持するため baseline（768 次元×800 行）よりおおむね
  比例して大きい）。次元混在によるファイルサイズの想定外の膨張はない。
- **正しさ（検証範囲を限定して確認）**: カタログ層（3 テーブルの次元宣言・
  永続共存・fail-closed な次元検証・`alter_table_add_column` の境界分離）は
  実 API（`create_table` / `list_tables` / `get_table_schema` /
  `validate_embedding_dim`）を通じて確認した。行ストア層は、行とユーザー
  テーブルを関連付ける機構が現時点で未実装（下記「制約・スコープ外」1.）
  のため、id レンジ分割 + `RowInput::metadata` でテーブル帰属をテスト側が
  模擬した上での混在格納・完全ラウンドトリップ（混線 0 件）を確認したに
  留まる。**書き込み時に対象テーブルの次元を検証する fail-closed 経路の
  存在は本検証の範囲外**であり、`validate_embedding_dim` はテストから直接
  呼び出して単体で確認したのみで、`put`/`put_batch` の書き込み経路に
  組み込まれた検証ではない
  （`crates/engine/tests/multi_dim_tables.rs`、5/5 pass）。

以上より、**現行アーキテクチャ（カタログ層 + 単一 `ROWS_TABLE`）は、複数次元
テーブルのカタログ共存（宣言・永続化・境界分離）を正しさの面で support する**
（`create_table` / `list_tables` / `get_table_schema` / `validate_embedding_dim`
を通じた確認。カタログ層自体の性能は本 ADR の計測対象外）。また、**可変長行
データを単一行ストアへ物理的に混在保存すること（ラウンドトリップの正しさ・
書き込みスループット・`scan_page` 全走査時間）については、性能面の劣化なく
support する**と判断する（Proposed）。ただし後者の実測ハーネス（`multi_dim_bench.rs`）
は `put_batch` をカタログ上の対象テーブルへ関連付けておらず（`table_name` は
`metadata` への記録のみ）、単一 `ROWS_TABLE` への可変長行混在という物理的な
書き込み・走査コストを計測したものであり、カタログ層を経由した性能への言及
ではない。テーブル別の行絞り込み・書き込み時の次元 fail-closed 検証を含む
「TABLE-2 の完全な正しさ」の確認は、行ストアのテーブル関連付け本実装（後続
タスク）後に別途行う必要がある。

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
