# スカラー列二次索引の構築とテーブル世代整合キャッシュ

- **Issue**: #473（親 Issue #472・#359。ADR: `docs/design/scalar-secondary-index.md`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/04-behavior/data-model.md`
  TABLE-12・`docs/spec/04-behavior/rls.md`
- **ステータス**: 実装済み（構築とキャッシュのみ。索引を使った候補削減は Issue #474）

## 背景・目的

SQL 表層の `WHERE` スカラー条件（`Equality`・`Prefix`）は
`sql/exec.rs::on_visible_row` で RLS 可視行の全行走査＋インライン比較（O(N)）に
より評価されている（`docs/design/scan-stage-profile.md` の該当段参照）。ADR
`docs/design/scalar-secondary-index.md`（Issue #359・#472 で候補 B を採用案として
確定）は、メモリ常駐・テーブル世代整合キャッシュ方式の二次索引を提案している。

本 Issue はその第 1 段として、**索引の構築とキャッシュのみ**を実装する。索引を
使った候補削減・`ExecutionPlan` 統合・`EXPLAIN` 露出は Issue #474 が担う。

**不変条件**: 本 Issue の前後で SQL クエリの結果は完全一致する。索引は構築・
キャッシュされるだけで、いずれのクエリの応答にも一切使われない。

## データモデル（`crates/engine/src/sql/scalar_index.rs::ScalarIndex`）

構築元は `sql::arena_cache::SqlArenaSnapshot`（Issue #363。RLS 段適用済み・ctx
可視行のみを含むスナップショット）。索引のスロット番号は**このスナップショットの
スロット**（`snapshot.arena().ids()[slot]`／`snapshot.metadata()[slot]` の添字）
であり、クエリごとに異なる SCALAR 段適用後アリーナのスロットではない
（Issue #474 はスナップショット経由でこの写像を扱う）。

- `TEXT` 列ごとに `TextColumnIndex`（値の辞書 `values`〔バイト列昇順・重複
  排除〕・CSR 形式の一致スロット列 `offsets`/`slots`・等価直引き用
  `equality: HashMap<String, u32>`）を持つ。`NULL` 値はいずれの索引にも
  エントリを作らない（`declarative_filter::MetadataFilter::matches` の NULL
  常時不一致と同じ判定になることを単体テストで固定）
- `id` 昇順の順序索引（`id_index: Option<Vec<(u64, u32)>>`）。全行の `id` が
  `sql::udf_call::id_as_finite_scalar`（`id > 2^53` を拒否）を満たす場合のみ
  `Some`。1 件でも超過があれば索引全体を `None` にする（fail-closed。Issue #474
  が全走査へ縮退する契機になる）

構築は 1 回の O(N) スロット走査（`scan_scalar_columns` による borrow-only
デコード）と、列ごとの安定ソート・CSR 構築からなる。`u32::try_from`・
`try_reserve`（`try_reserve_exact` を含む）で untrusted な行数・値数に対する
無制限確保を防ぐ（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
概算バイト量が単体上限（`MAX_SCALAR_INDEX_BYTES`）を超える場合は構築失敗
（`ScalarIndexBuildError::TooLarge`）として索引なしへ縮退する。

## キャッシュ（`ScalarIndexCache`）

キー・世代源泉・fail-closed の`lookup`契約は`sql::arena_cache::SqlArenaCache`
（Issue #363）・`sql::sparse_cache::SparseIndexCache`（Issue #357）と同型:
`(table, PolicyContext)` の完全一致 × テーブル単位世代
（`catalog::table_generation_in_txn`）。呼び出し元の `read_txn` が古いだけの
可能性を考慮し、`lookup` の破棄判定は `storage` から読んだ「真に最新の」世代と
比較し**厳密に古い**場合のみ行う（`SqlArenaCache::lookup` と同じ理由）。

`ScalarIndexCache::insert` は **`SqlArenaCache::insert` とは意図的に非対称**
（`core.rs::PrefilterCache::insert`・Issue #280 と同じ契約）: 世代不一致・ロック
毒化・世代読み取り失敗のいずれも `None` を返し、キャッシュへ反映しないだけで
なく呼び出し元へも一切渡さない。本索引はまだ誰にも消費されない派生データであり、
`SqlArenaCache` のように「このクエリの応答に限って stale でも使ってよい」対象が
存在しないため（挿入失敗時は呼び出し元が単に索引なしとして扱う。fail-soft な
派生キャッシュ）。

容量は `MAX_SCALAR_INDEX_CACHE_ENTRIES`（32）・`MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES`
（`crate::arena::MAX_ARENA_TOTAL_BYTES` と同じ桁）を超えないよう、同一キー重複
除去 → 挿入対象テーブルに限定した世代不整合エントリの一括破棄 → LRU 追い出しの
順で管理する（`SqlArenaCache`・`PrefilterCache` と同じ手順）。

統計 `ScalarIndexCacheStats`（`hits`/`misses`/`stale_evictions`/
`capacity_evictions`/`builds`/`build_failures`/`entries`）はテナント ID・行 ID・
スカラー値等の機微情報を一切含まない。`EngineCore::scalar_index_cache_stats()`
（`VectorCore` trait には載せない固有メソッド）から観測用にのみ公開する。

## `sql::exec.rs` への結線（構築のみ）

`execute_statement_with_cache` に 10 番目の引数
`scalar_cache: Option<scalar_index::ScalarCacheAccess<'_>>` を追加した
（既存の `#[allow(clippy::too_many_arguments)]` を維持）。公開 API
`execute_statement`（`sparse_cache`/`arena_cache`/`hnsw_cache` と同じく `None`
を渡す薄いラッパー）は互換性を保つ。

適用条件（gate）: `scalar_cache.is_some() && plan.scalar_prefilter &&
!bound.metadata_filters.is_empty()`（索引対応述語 `Equality`/`Prefix` を持つ
SCALAR 事前フィルタ経路のみ。`expr_filters` の分類は Issue #474 の
`classify_scalar_plan` に委ね、本 Issue の gate には含めない）。

索引の構築材料（`SqlArenaSnapshot`）は `arena_cache`（Issue #363）経由で
スナップショットが手に入った経路（ヒット・ミスいずれも）でのみ得られる。
`arena_cache` が `None`（`execute_statement` 経由・テスト等）の場合はこの
クエリでは構築しない。手順は `scalar_cache.cache.lookup(...)` → `None` なら
`ScalarIndex::build(schema, &snapshot)` → `Ok` なら `insert`（戻り値は使わない。
`Err` は `build_failures` を計上して無視）。結果は一切消費しない（候補削減は
Issue #474）。構築・登録の失敗はクエリを失敗させない（fail-soft）。

未消費の `pub(crate)` 照会 API（`candidates_for`・`candidates_equals`・
`candidates_id_range` 等）は Issue #474 の消費者向けに用意してあるが、production
の非テストビルドでは現時点で未参照のため `#[cfg_attr(not(test), allow(dead_code))]`
を付与している（`catalog.rs::insert_row_into_table` と同じ理由・パターン）。

## テスト

- in-module（`sql/scalar_index.rs`）: プロパティ的な全走査オラクル比較（等価・
  前方一致述語）、NULL の非索引化、RLS 部分可視（他テナント Private 行の非漏えい）、
  空テーブル、`id > 2^53` を含む場合の `id_index` の `None` 化、`id` 範囲照会、
  キャッシュのヒット・世代競合時 `None` 契約・キー分離
- 結合テスト（`crates/engine/tests/scalar_index_cache.rs`）: `EngineCore::
  execute_sql` 経由（production の gated 構築経路）で、索引対応述語ありクエリの
  初回 build → 反復ヒット、`WHERE` なしクエリでの gate 不発火、対象テーブルへの
  書き込みによる失効・再構築、`(table, ctx)` キー分離をそれぞれ固定。索引が
  応答へ影響しないこと（結果が索引の有無に関わらず不変）もあわせて確認する

## スコープ外（Issue #474・#475・#476 へ）

- 索引による候補削減・`ExecutionPlan` 統合・選択度切替・`classify_scalar_plan`・
  `EXPLAIN` の `scalar_plan:` 露出
- 集計・`GROUP BY` 経路への結線
- RLS 不変の統合テストスイート・損益分岐実測・閾値確定
- `IN`／`BETWEEN` 構文（現行許可リストに無い）
- `core.rs::PrefilterCache` のテーブル単位世代への統一
