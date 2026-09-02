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
3. `guard.as_mut()` へ `encode_row_reimpl_into_slice` で直接書き込む

`unsafe` は使用しない。production コード（`crates/engine/src/`）は無変更。

## 前後比較の実測

計測環境: 本開発環境（`nproc`=12。他プロセスと計測環境を共有）。
`make bench-ingest-profile` 相当（`BENCH_INGEST_PROFILE_INSERT_MODE=insert`
／`=reserve` を交互に実行。既定 rows=1,000・dim=128）。`cargo build --release
-p engine --bench ingest_profile_bench` でビルドした単一バイナリを両モードで
起動する単一ビルド内 A/B（Issue #324・#366 と同型）。

### I6（redb insert）段（dim=128・rows=1,000。交互 5 ペア）

| ペア | insert（ns/行） | reserve（ns/行） |
| ---- | ---------------- | ------------------ |
| 1 | 833.5 | 1633.5 |
| 2 | 822.5 | 1644.6 |
| 3 | 846.9 | 1640.8 |
| 4 | 842.7 | 1637.2 |
| 5 | 828.1 | 1638.8 |
| **median** | **833.5** | **1638.8** |

`reserve` は `insert` に対し **中央値で約 +97%（ほぼ倍）** 悪化した。
E0（`engine::tenant::insert_rows`。両モードで production コードは無変更の
ため同一経路）は 3,424.7〜3,489.2 ns/行のレンジで両モード間に系統差は無く、
測定対象外区間（E0）の run-to-run 変動幅（最大約 65 ns/行）に対し、I6 の
`reserve` 側の悪化幅（約 800 ns/行）は 1 桁以上大きい。ノイズでは説明できない
一貫した悪化と判断できる。

### 規模点（dim=1024・rows=200。各モード 1 回）

| モード | I6（ns/行） |
| ------ | ------------ |
| insert | 4977.7 |
| reserve | 8349.1 |

次元が大きいほど零埋めコスト（`value_length` に比例）が効くという想定どおり、
絶対値では悪化幅が拡大した（dim=128: +805 ns/行 → dim=1024: +3371 ns/行）。
相対悪化率は dim=128 の約 +97% に対し dim=1024 では約 +68% とやや縮小する
（E0・SUM 側の他段コストが相対的に大きくなるため）が、いずれの規模でも
`reserve` が `insert` を明確に上回ることに変わりはない。

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
（`InsertMode`・`parse_insert_mode`・`encode_row_reimpl_into_slice`）は
`Rejected` の判断根拠を再現可能にする記録として残し、`Table::insert` を
既定経路のまま維持する。

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
