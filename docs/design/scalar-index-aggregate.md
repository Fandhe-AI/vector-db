# 集計・GROUP BY 経路をスカラー列二次索引へ結線する

- **Issue**: #475（親 Issue #472・#359。前提 Issue #473・#474。ADR:
  `docs/design/scalar-secondary-index.md`「採用案（候補 B）の確定仕様」節・
  「Issue #464 実測の反映」節）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: TASK-166・TASK-167・SQL-13・
  SQL-14・RLS-7・RLS-8・`docs/spec/04-behavior/data-model.md` TABLE-12
- **ステータス**: 実装済み

## 背景・目的

`docs/design/crossdb-bench.md`（25,000 行・dim 128・wire 経由・p50）で self が
劣後する `where_compound_count`（`WHERE` 付き `COUNT(*)`）・`group_by_having`
は、`sql::aggregate::execute_aggregate_with_cache`・`sql::group_by::
execute_grouped_aggregate` が毎クエリ `user_rows/{table}` を全行走査
（`docs/design/scan-stage-profile.md` A1〜A5）することが主因である。Issue #473・
#474 で SELECT 経路には `ScalarIndex` の候補削減が結線済みだが、集計・`GROUP BY`
経路は未結線だった。

ADR `docs/design/scalar-secondary-index.md`「Issue #464 実測の反映」節は
`agg_count`（`WHERE` なし `COUNT(*)`）を索引の対象外とし、その改善は Issue #478
（`VisibleBitmapCache`。実測約 32 倍）へ帰属させている。本 Issue は `agg_count` に
production 変更を加えず、実測比較では #478 経路の非退行確認として併記する。

## 設計

### 適用条件（gate）

| 経路 | gate | 索引の使い道 |
| --- | --- | --- |
| `GROUP BY` なし・`WHERE` あり | `schema.vector_dim().is_some()` かつ `classify_scalar_plan(...) != PlainScan` | `resolve_candidates` の候補スロットを走査（候補走査形） |
| `GROUP BY` あり・`WHERE` なし | 同上・`WHERE` 自体が空 | `ScalarIndex::column_groups`／`slots_without_value` でグループを直接列挙（列挙形） |
| `GROUP BY` あり・`WHERE` あり（索引対応述語のみ） | 両方 | 候補スロットを走査しつつグループへ振り分け（候補走査形＋GROUP） |
| 上記以外（`VECTOR` 列なしテーブル・残余述語あり・索引構築/選択度不可 等） | — | 既存の全走査ループ（無変更） |

`classify_scalar_plan`（Issue #474・`sql::scalar_plan`）は「`WHERE` の全述語が
`TEXT` 列の等価／前方一致、または `id` の単純比較」の場合のみ `PlainScan` 以外を
返す。この条件下では `ScalarIndex::resolve_candidates` の `Use(slots)` が `WHERE`
全体に対する**厳密な**一致集合になる（各述語の候補取得・交差はいずれも exact な
集合演算）ことをコードで確認済みだが、`COUNT(*)` を候補数の直接返却で済ませる
ショートカットは採用しない——索引は「絞る」ことしかできず「通す」ことはできない
という既存の多層防御の設計原則（SELECT 経路・`sql::exec` と同型）を崩さないため、
候補行にも常に `matches_all`＋式述語（`observe_candidate_slots`／
`observe_candidate_slots_grouped`）を再適用してから集計する。再検証コストは
候補数に比例するだけで、索引が絞り込んだ以上のコストは生まない。

### piggyback 構築（非 vacuous 化）

SELECT が先に同じ世代を走査していなければ `SqlArenaCache`／`ScalarIndexCache` は
未構築のままで、集計クエリ単独では索引経路が永久に成立しない。そこで集計・
`GROUP BY` 経路自身がキャッシュミス時に構築する
（`aggregate.rs::capture_scalar_index_snapshot`）:

- `user_rows/{table}` を 1 回走査し、可視行を `SqlArenaCaptureBuilder` へ集める
  （embedding を含む完全デコード）。
- `SqlArenaCaptureBuilder::push` は `embedding.len() == expected_dim` を検証しない
  （`GrowableArenaBuffers::push_row` が `extend_from_slice` するだけ）ため、
  呼び出し元が dim を検証してから渡す。nullable `VECTOR` 列の `NULL` 行
  （`dim == 0`）を 1 件でも検出した時点で、この世代の採取全体を断念する
  （soft-fail。`capture_scalar_index_snapshot_soft_fails_on_nullable_vector_null_row`
  で固定）。アリーナのレイアウト不変条件（`vectors.len() == ids.len() * dim`）を
  守れない世代を索引化しないための防御であり、クエリ応答自体は従来の全走査へ
  フォールバックするだけで失敗しない。
- 採取成功後は `SqlArenaCache::insert`／`ScalarIndex::build`／
  `ScalarIndexCache::insert` へ渡す。挿入前に構築した `ScalarIndex` は
  `SqlArenaCache::insert` が返す `Arc<SqlArenaSnapshot>` と中身が不変（`Arc` で
  包むだけ）のため再構築しない（`SqlArenaCache::insert` の fail-closed 契約
  ドキュメント参照）。
- cold コストは「embedding デコード＋複製」が世代ごとに 1 回増える。`SqlArenaCache`
  が SELECT で既に受け入れているのと同じトレードオフである。

### GROUP BY 列挙形（`sql::group_by::observe_group_enumeration`）

`ScalarIndex::column_groups(group_by.column_index)`（値のバイト列昇順・値ごとの
スロット列）と `slots_without_value(...)`（NULL 行のスロット）を直接グループ表へ
写像する。`ScalarIndex::build` は当該列の可視行が持つ非 `NULL` 値を**すべて**
索引化するか（成功）、予算超過等で索引全体の構築を諦めるか（`Err`。索引なし）の
いずれかであり、一部の値だけを欠落させたまま索引を返すことはない
（`scalar_index.rs` モジュールドキュメント「データモデル」参照）ため、
「索引に現れないスロット＝NULL」という差分計算がそのまま NULL グループの完全な
補完になる。`WHERE` が無いため候補の再検証は不要。`MAX_GROUPS`／
`MAX_GROUP_KEY_TOTAL_BYTES`／`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES` は既存の
`check_new_group_budget`／`accumulate_row` をそのまま呼ぶため全走査と同じ位置で
適用される。

### GROUP BY 候補走査形（`sql::group_by::observe_candidate_slots_grouped`）

`resolve_candidates` の候補スロットを走査し、行ごとに `matches_all`・式述語を
再適用したうえで、既存の全走査ループと同一の GROUP 段ロジック（`String:
Borrow<str>` による借用キー探索→新規グループのみ所有化）で
`string_groups`／`null_group` へ振り分ける。

### `sql::group_by.rs` の構造変更

`execute_grouped_aggregate` を「集計表（`string_groups`／`null_group`）を構築する
段」と「FINISH 段（`HAVING`・`ORDER BY`・`LIMIT`・投影）」に分離した。索引経路・
全走査経路（`used_index_path` フラグで分岐）のいずれも同じ集計表変数へ書き込み、
FINISH 段は完全に共有する（順序・タイブレーク・NULL 末尾規約のコード重複を作ら
ない）。

### `core.rs` の結線

`Statement::Aggregate` アームで `execute_aggregate_with_cache` へ
`ArenaCacheAccess`（`sql_arena_cache`）・`ScalarCacheAccess`
（`scalar_index_cache`）を追加で渡す（既存の `VisibleCacheAccess` はそのまま）。
`EXPLAIN` は `USING PLAN` 専用構文（SQL-6）で集計文を受理しないため無変更。

## 対象ファイル

| パス | 変更 |
| --- | --- |
| `crates/engine/src/sql/scalar_index.rs` | `column_groups`／`slots_without_value` の追加、`ScalarIndexCacheStats` へ `aggregate_index_scans`／`aggregate_plain_scan_fallbacks` と記録メソッド追加 |
| `crates/engine/src/sql/aggregate.rs` | `try_scalar_index_aggregate`・`ensure_scalar_index_snapshot`・`observe_candidate_slots`・`capture_scalar_index_snapshot` の追加、シグネチャへ `arena_cache`／`scalar_cache` 追加 |
| `crates/engine/src/sql/group_by.rs` | 集計表構築段／FINISH 段の分離、`observe_group_enumeration`・`observe_group_slots`・`observe_candidate_slots_grouped` の追加 |
| `crates/engine/src/core.rs` | `Statement::Aggregate` アームでの結線 |
| `crates/engine/tests/scalar_index_aggregate.rs`（新規） | cold/hot 等価性・非 vacuous 性・残余述語・RLS オラクル・`VECTOR` 列なしテーブルの結合テスト |

依存追加なし・`unsafe` なし・spec 本文転記なし。

## 検証

`cargo fmt --all --check`・`cargo clippy --workspace --all-targets -- -D
warnings`・`cargo test --workspace`（`--all-features` は本開発環境に usearch の
C++17 ビルド環境が無く未実行。`crates/engine/`・`crates/wire-server/` いずれも
green）・`scripts/check_sort_determinism.sh` を確認済み。`make core-api-check`
（`VectorCore`／`SearchProvider` は無変更のため対象外）。

## 前後比較

before = `origin/main`（`c7f478a`）、after = 本 Issue の HEAD。同一
`cargo build --release -p engine --example feature_bench`（別 `CARGO_TARGET_DIR`）
を交互に N=5 ペア実行し、各 run の p50 を記録した（本開発環境は共有環境のため
**参考値**。採用根拠は構造的非退行——縮退先が既存の全走査経路そのもの・hot 経路
は候補数 O(|hits|)——と cold/hot・RLS オラクルの等価性テストに置く）。

### 25,000 行（既定規模）

| phase | before median (µs) | after median (µs) | ratio |
| --- | ---: | ---: | ---: |
| `where_compound`（`WHERE visible() AND id > 100 AND lang = 'ja'` の `COUNT(*)`） | 2902 | 323 | 0.111 |
| `group_by_having`（`WHERE` なし `GROUP BY` の候補削減対象） | 3174 | 1098 | 0.346 |
| `agg_count`（`WHERE` なし `COUNT(*)`。#478 経路。非退行確認） | 54 | 54 | 1.000 |
| `vector_knn`（参照区間・変更を含まない） | 685 | 662 | 0.966 |
| `rls_isolation`（参照区間・変更を含まない） | 50 | 50 | 1.000 |

他 8 フェーズ（`ingest`・`point_where`・`agg_multi`・`vector_knn_where`・
`hybrid_rrf`・`mode_recall`・`mode_precision`・`udf_call`）はいずれも比
0.79〜1.10x の範囲内で明確な退行は無い。`vector_knn_where`（0.79x）は本 Issue の
直接の対象ではないが、`where_compound` フェーズが先に走ることで `SqlArenaCache`／
`ScalarIndexCache` が温まり、後続の SELECT 側候補削減（Issue #474）が初回構築
コストを避けられる副次効果と考えられる。

### 100,000 行（`BENCH_FEATURE_SCALE=4`。単発実測・参考値）

| phase | before (µs) | after (µs) |
| --- | ---: | ---: |
| `where_compound` | 13353 | 2013 |
| `group_by_having` | 14062 | 8345 |
| `agg_count` | 211 | 211 |
| `vector_knn` | 3488 | 3757 |

25k 規模と同方向の改善が確認できる。

`crates/engine/tests/scalar_index_aggregate.rs` の cold/hot 完全一致テスト・
`crates/engine/tests/sql_aggregate.rs`／`sql_group_by.rs`（既存テスト無変更）が
いずれも green であることを結果の正しさの一次根拠とする。

## スコープ外・申し送り

- 索引非対応の集計（`agg_multi` の `MIN(text)` 等）に対する全走査回避は対象外。
- 選択度閾値（`ScalarIndex` 既定 1/2。#474 で確定）の再確定・RLS 統合スイート・
  wire 経由 crossdb 実測は Issue #476 の担当のまま。
- `MAX_GROUPS`（10,000）超過を列挙形・候補走査形の双方で構造的に固定する専用の
  大規模結合テストは追加していない（`check_new_group_budget` は全走査経路と同一
  関数を同じ引数で呼ぶため、既存 `tests/sql_group_by.rs` の上限テストが検証する
  契約をそのまま継承する設計）。
- `id > 2^53`（`id_index` が `None` になる世代）での縮退は SELECT 経路
  （`scalar_index.rs::id_index_is_none_when_any_id_exceeds_exact_f64_range`）と
  同一の `resolve_candidates` 契約をそのまま消費するのみで、集計・`GROUP BY`
  固有のロジックを持たないため専用テストは追加していない。
