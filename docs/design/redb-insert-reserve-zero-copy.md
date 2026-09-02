# redb `insert_reserve` によるゼロコピー行書き込みの試作・実測・採否

- ステータス: **Rejected**（`Table::insert_reserve` は「ゼロコピー」にならない
  redb 4.2.0 の内部構造を静的照合で確認したうえで、bench 側 A/B 計測モードを
  試作し実測。I6（redb insert）段が一貫して悪化したため production 変更は
  行わない。本コミットは bench 側の A/B 計測モード追加・実測結果・判断根拠の
  記録のみを含む）
- 対応: Issue #400（`feat(engine): redb insert_reserve によるゼロコピー行
  書き込みの試作・実測・採否`）。親: #395「ingest 書き込み経路の固定コスト
  削減」Phase 2
- 前提: `docs/design/ingest-stage-profile.md`（Issue #396。I6 が content_hash
  に次ぐ支配的段であることの実測）・RECOVER-3/5/10・TABLE-12・TASK-93・
  TASK-101・TASK-122（`docs/spec/04-behavior/*`。判定内容・数値基準は spec
  側が SSOT）

## 背景

`docs/design/ingest-stage-profile.md` の実測では、I6（redb insert。
`Table::insert(key, encoded.as_slice())`）が content_hash に次いで支配的な段
だった。#397/#398 でエンコードは「行ごと 1 回・1 バッチ 1 arena」まで削減
済みだが、arena（スクラッチバッファ）から redb ページへのコピーは残っている。
redb 4.2.0 は `Table::insert_reserve` という API を持ち、名目上は「呼び出し側
が符号化先のバッファを redb 内部から直接借りて書く」ことでこの中間コピーを
省略できるように見える。本 Issue はこれを試作・実測し、採否と根拠を記録する。

## 型・trait 照合（適用は可能）

- 行ストア値型は `&'static [u8]`（`crates/engine/src/storage.rs`
  `RowStoreTableDef`・`catalog.rs::user_rows_table_def` も同型）
- redb 4.2.0 `src/types.rs:201` に `impl MutInPlaceValue for &[u8]`
  （`initialize` は no-op・`from_bytes_mut` は恒等）があり、
  `Table<K, &[u8]>::insert_reserve(key, value_length) ->
  AccessGuardMutInPlace<'_, &[u8]>`（`src/table.rs:462`）がそのまま呼べる。
  **型の変更・`unsafe` は不要**。`AccessGuardMutInPlace::as_mut()`
  （`AsMut<[u8]>` 実装。`src/tree_store/btree_base.rs:427`）で `&mut [u8]`
  を得る
- 行長は既存の符号化ロジックから書き込み前に計算できる（固定長制約は不要。
  可変長でも長さ事前確定で足りる）

## redb 内部構造の確認結果（「ゼロコピー」が成立しない構造的な事実）

`src/tree_store/btree.rs:875-899` の `BtreeMut::insert_reserve` を読むと:

```rust
let mut value = vec![0u8; value_length];   // ヒープ確保 + 零埋め
V::initialize(&mut value);                  // &[u8] では no-op
let mut operation = MutateHelper::<K, V>::new(...);
let result = operation.insert(key, &V::from_bytes(&value));  // 通常の insert 経路
```

つまり `insert_reserve` は **`value_length` バイトのヒープ確保＋零埋めを行い、
その零埋めベクタを通常の `MutateHelper::insert`（`btree_mutator.rs`）へ渡して
リーフページへコピーしてから**、そのページ内領域を指す
`AccessGuardMutInPlace` を返す。`btree_mutator.rs` の該当箇所
（例: `:852-853` `new_page.memory_mut()[value_start..value_end]
.copy_from_slice(value)`。`:725/764/830/899/953/995` の
`AccessGuardMutInPlace::new` 直前も同様）はいずれも通常の `insert` と同じ
ページ書き込みコードパスを通る。

したがって 1 行あたりのコストは:

- 現行（`Table::insert`）: スクラッチへ encode（1 書き込み）→ ページへ
  コピー（1 コピー）
- `insert_reserve`: 零埋め Vec 確保＋零埋め（**追加**）→ 零値のページ
  コピー（現行のページコピーと同型）→ ページ領域へ encode（1 書き込み。
  現行のスクラッチ書き込みをページ直接書き込みに置き換えただけ）

**ページへの書き込み回数は変わらず、ヒープ確保と零埋めが 1 行 1 回増える**
のみで、中間コピーの排除にはならない。改善ゼロ〜悪化が構造的に見込まれる。

## 契約面の制約

- `insert_reserve` は既存値を返さない（`insert` の `Option<AccessGuard>`
  相当が無い）。`tenant.rs::insert_unique_row` の `IdConflict`（`23505`）
  検出（`insert(..).is_some()`）を維持するには事前 `get` が必要で B-tree
  走査が 1 回増える。既存キーは無告知で上書きされる契約になる
- `tenant.rs::insert_rows_unchecked` は「全行 encode（arena）→
  content_hash → 台帳記録 → 行 insert」の順で、エラー優先順位
  （encode 失敗 → 台帳内容不一致 `22023` → `IdConflict`）を回帰テストで
  固定している（`tests/recovery_content_hash.rs`）。ページへ直接 encode し
  てハッシュ入力にもするには「insert を台帳記録より前に実行し guard バイト
  列からストリーミングでハッシュ」が必要で、順序・エラー優先順位の契約変更
  と `content_hash` API 変更（ストリーミング化は #397/#398 で申し送り済み）
  を伴う。主経路 `insert_rows_unchecked` では arena を残さざるを得ず、
  コピー削減効果は得られない
- `AccessGuardMutInPlace` は `Table` を `&mut` 借用し commit 前に drop が
  必須（同時に 1 行分しか保持できない）

以上より、たとえ性能が改善したとしても採用は台帳非経由の書き込みパス
（`Storage::put_batch`・`catalog::insert_rows_into_table`・
`tenant.rs::replace_typed_rows_by_text_key`）に限定する設計判断だった
（`insert_rows_unchecked` は対象外）。実測の結果、この限定の要否を判断する
前段階（性能改善そのものの有無）で不採用が確定した。

## 試作（bench 側 A/B 計測モードのみ・production 無変更）

`crates/engine/benches/harness/ingest_profile.rs` に
`InsertMode { Insert, Reserve }`・`parse_insert_mode`（`BENCH_INGEST_PROFILE_
INSERT_MODE` の解析。未設定→`Insert`、`insert`/`reserve` のみ受理し他は
fail-closed）・`encode_row_reimpl_into_slice`（`encode_row_reimpl` と同一の
検証・フィールド順序を固定長スライスへ直接書く版。バイト単位一致は
`tests/ingest_profile_accept.rs` で回帰固定）を追加した。

`crates/engine/benches/ingest_profile_bench.rs` の I6 段を `InsertMode` で
分岐させ、`Reserve` モードでは:

1. `row_table.get((tenant, id))` で一意性を事前検査（`insert_unique_row`
   相当。契約面の制約節）
2. `row_table.insert_reserve((tenant, id), encoded.len())` で確保
3. `guard.as_mut()` へ、I5 で作成済みの `encoded`（`row_encoded`）を
   `copy_from_slice` でそのまま書き込む

`unsafe` は使用しない。production コード（`crates/engine/src/`）は無変更。

### 計測範囲の訂正（codex-review 指摘・PR #420）

初版では手順 3 を `encode_row_reimpl_into_slice` による予約済みバッファへの
**再エンコード**として実装しており、`Insert` 側（I5 で作成済みの
`row_encoded` を I6 では insert するだけ）と処理範囲が揃っていなかった
（`Reserve` 側だけ I6 内で二重にエンコードしていた）。両モードとも I6 の
処理範囲を「I5 のエンコード結果を書き込むだけ」に揃えるため、上記手順 3 を
`encode_row_reimpl_into_slice` の再実行から `copy_from_slice`（既存
`encoded` のコピー）へ修正した。以下の実測値はこの訂正後のコードによる
再計測値（訂正前の数値は本節末尾に参考として残す）。

## 前後比較の実測

計測環境: 本開発環境（`nproc`=12。他プロセスと計測環境を共有）。
`make bench-ingest-profile` 相当（`BENCH_INGEST_PROFILE_INSERT_MODE=insert`
／`=reserve` を交互に実行。既定 rows=1,000・dim=128）。`cargo build --release
-p engine --bench ingest_profile_bench` でビルドした単一バイナリを両モードで
起動する単一ビルド内 A/B（Issue #324・#366 と同型）。

### I6（redb insert）段（dim=128・rows=1,000。交互 5 ペア）

| ペア | insert（ns/行） | reserve（ns/行） |
| ---- | ---------------- | ------------------ |
| 1 | 789.9 | 1193.7 |
| 2 | 884.0 | 1218.2 |
| 3 | 804.0 | 1204.3 |
| 4 | 794.5 | 1210.8 |
| 5 | 870.2 | 1203.4 |
| **median** | **804.0** | **1204.3** |

`reserve` は `insert` に対し **中央値で約 +49.8%** 悪化した（訂正前の
二重エンコードを含む計測では約 +97% だったが、二重エンコード分を除いても
なお `insert` を明確に上回る）。E0（`engine::tenant::insert_rows`。両モード
で production コードは無変更のため同一経路）は 3,395.2〜3,429.7 ns/行の
レンジで両モード間に系統差は無く、測定対象外区間（E0）の run-to-run 変動幅
（最大約 35 ns/行）に対し、I6 の `reserve` 側の悪化幅（約 400 ns/行）は
1 桁大きい。ノイズでは説明できない一貫した悪化と判断できる。

### 規模点（dim=1024・rows=200。各モード 2 回）

| モード | I6（ns/行） |
| ------ | ------------ |
| insert | 5068.6, 4929.2 |
| reserve | 5563.5, 5463.2 |

次元が大きいほど零埋めコスト（`value_length` に比例）が効くという想定どおり
絶対値の悪化幅は dim=128（約 400 ns/行）より拡大する（約 450〜530 ns/行）が、
相対悪化率は約 +9.8〜10.8% で dim=128（約 +49.8%）より縮小する（E0・SUM 側の
他段コストが相対的に大きくなるため）。いずれの規模でも `reserve` が
`insert` を明確に上回ることに変わりはない。

### 参考: 訂正前（二重エンコードを含む）計測値

`encode_row_reimpl_into_slice` による再エンコードを含んでいた訂正前の
実測値（dim=128・rows=1,000・交互 5 ペア）: insert median 833.5 ns/行・
reserve median 1638.8 ns/行（約 +97%）。規模点（dim=1024・rows=200）:
insert 4977.7 ns/行・reserve 8349.1 ns/行（約 +68%）。二重エンコード分
（I6 内で `encode_row_reimpl_into_slice` を追加実行していた分）が悪化幅を
過大に見せていたが、訂正後もなお `reserve` が `insert` を上回るため
「判断」節の不採用（Rejected）という結論そのものは変わらない。

### fail-closed の確認

`BENCH_INGEST_PROFILE_INSERT_MODE` に `Reserve`（大文字小文字違い）・空文字・
`foo`（未知値）を渡すといずれも起動直後に `invalid env
BENCH_INGEST_PROFILE_INSERT_MODE: unknown insert mode: ...` で拒否され、
`GITHUB_ACTIONS=true` 下でも従来どおり起動直後に拒否されることを確認した。
両モードとも整合性検証（`user_rows/docs` バイト単位一致・`table_generation`・
`op_ledger` 内容ハッシュ）は green（`ingest_profile_bench: OK`）。

## 決定規則の適用

計画時に定めた採用条件（「交互 5 ペア以上で `reserve` の I6 median が
`insert` より改善し、その差が同一実行群の E0 の run-to-run 幅を上回り、かつ
min-of-N でも改善方向であること。加えて契約変更を伴わず `unsafe` 不使用で
あること」）に対し、実測は **改善方向とは逆**（一貫した悪化）だった。
「redb 内部構造の確認結果」節の静的解析（零埋め Vec 確保＋零埋めコピーが
純粋に追加される）と実測が整合しており、環境ノイズによる偶然の逆転ではない。

## 判断

**不採用**。`crates/engine/src/` は無変更。bench 側の A/B 計測モード
（`InsertMode`・`parse_insert_mode`。`encode_row_reimpl_into_slice` は
`encode_row_reimpl` とのバイト単位一致を回帰固定する独立ヘルパーとして
`tests/ingest_profile_accept.rs` に残置し、I6 段の計測経路からは「計測範囲の
訂正」節のとおり除いた）は `Rejected` の判断根拠を再現可能にする記録として
残し、`Table::insert` を既定経路のまま維持する。

「契約面の制約」節の懸念（`insert_unique_row` の `Option` 契約・
`insert_rows_unchecked` のハッシュ順序）は、性能改善が無い以上いずれも
検討する必要がなくなった。

## スコープ外

- `content_hash` のストリーミング化と `insert_rows_unchecked` での順序変更
  （#397/#398 申し送りの継続。エラー優先順位契約に触れるためオーナー判断）
- redb 上流での真のゼロコピー `insert_reserve`（零埋め Vec 経由をやめる）の
  要望・追随（依存更新はユーザー承認制）
- 専有環境での確定測定（Issue #314・#366 と同じ運用者申し送り。ただし本
  Issue は悪化幅が E0 の run-to-run 変動を 1 桁以上上回る明確な結果のため、
  専有環境での再測定によって結論が覆る可能性は低いと判断する）
- #401（前後比較の総括・Durability／バッチ上限の判断記録）は本 Issue の
  結果を受けて別途実施

## 再現手順

```sh
cargo build --release -p engine --bench ingest_profile_bench
BENCH_INGEST_PROFILE_INSERT_MODE=insert  ./target/release/deps/ingest_profile_bench-<hash>
BENCH_INGEST_PROFILE_INSERT_MODE=reserve ./target/release/deps/ingest_profile_bench-<hash>
```

（`<hash>` はビルドごとに変わるハッシュ付きファイル名。`cargo bench --bench
ingest_profile_bench -p engine`〔`make bench-ingest-profile`〕でも同様に env
を渡せる。規模点は `BENCH_INGEST_PROFILE_ROWS`／`BENCH_INGEST_PROFILE_DIM` を
併用。交互実行・min-of-N の必要性は `docs/design/knn-two-stage-topk.md`
「再現手順」節と同じ理由による。）
