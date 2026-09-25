# WHERE 事前フィルタの等価・範囲述語を二次索引経路へ結線する

- **Issue**: #474（親 Issue #472・#359。前提 Issue #473。ADR:
  `docs/design/scalar-secondary-index.md`「採用案（候補 B）の確定仕様」節）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/04-behavior/data-model.md`
  TABLE-12・`docs/spec/04-behavior/rls.md`・SQL-6（`EXPLAIN`）
- **ステータス**: 実装済み

## 背景・目的

Issue #473 は `sql::scalar_index::ScalarIndex` の構築とテーブル世代整合キャッシュ
のみを実装し、索引の照会 API（`candidates_for` 等）は一切消費されなかった
（クエリ結果はいずれも従来どおり全可視行の `on_visible_row` 走査で決まっていた）。
本 Issue はこれを SELECT 経路（`DISTANCE`／`HYBRID`／`USING PLAN` 展開経由）の
SCALAR 事前フィルタへ結線し、索引対応述語（等価・前方一致・`id` の単純比較）を
持つクエリで走査対象を候補行のみへ絞る。

効果上限の見積り（`docs/design/scan-stage-profile.md` 25k 実測の内訳から）:
engine 内 hot e2e（W0-hot）1.118ms のうち索引で除去できるのは W1+W2（全可視行の
`scan_scalar_columns`＋述語判定）≈ 0.41ms 相当。`docs/design/crossdb-bench.md` の
wire 経由 `vector_knn_where`（p50 2,819µs。Qdrant HNSW 615µs）の残りは wire 往復・
応答組み立て等が占めるため、**本 Issue 単独で Qdrant 615µs 台へ到達する見込みは
ない**。前後比較・環境の性質は §6 参照。

集計・`GROUP BY` 経路（Issue #475）・RLS 統合スイート／選択度閾値の確定・全 13
フェーズ前後比較（Issue #476）はスコープ外。

## 設計

### 索引対応述語の狭い定義（`sql::scalar_plan::classify_scalar_plan`）

`sql::exec` の SCALAR 段は `bound.metadata_filters`（`TEXT` 列の等価・前方一致）
と `bound.expr_filters`（`WHERE` の式述語）を宣言順に短絡評価する。索引が扱える
のは前者すべてと、後者のうち **`id <op> <数値リテラル>`**（`op` は
`>`/`<`/`>=`/`<=`/`=`、左右いずれの位置でも可。左右入替時は演算子を反転する）
という狭い形だけである。`expr_filters` に 1 つでもこの形に一致しない要素
（`Builtin`・`WasmCall`・`VectorRef` を含む式、ネストした演算等）があれば、それは
評価時にエラーになりうる残余述語であり、既存の fail-closed 契約（`22000` 等）を
崩さないよう索引経路を一切使わず全走査へ縮退する（`ScalarPlan::PlainScan`）。
`WHERE visible()` は `rls_predicate_present` フラグのみを立て
`metadata_filters`/`expr_filters` を増やさないため、この判定には現れない。

`sql::exec`（実行時の索引消費・縮退判定）と `sql::explain`（`scalar_plan:` 行の
静的表示、下記）はこの分類関数だけを単一情報源として使う（`sql::hnsw_cache::
classify_ann_plan`〔Issue #411〕と同型の設計）。

### `id` 比較述語の範囲変換（`sql::scalar_plan::id_bounds`）

`id <op> literal` を `ScalarIndex::candidates_id_range` が受け取る
`(Bound<u64>, Bound<u64>)` へ変換する。`sql::udf_call::eval_binary` の
`(id as f64) op literal` 評価とビット同値になるよう導出し、`id ∈ 0..=20` ×
`literal ∈ {0.0, 0.5, 1.0, 5.0, 5.5, 10.0, 19.5, 20.0, 20.5}` × 5 演算子の全組合せ
を `eval_binary` オラクルと突き合わせるプロパティテストで固定した
（`sql::scalar_plan::tests::id_bounds_matches_eval_binary_property`）。

文法上リテラルは非負かつ有限のみ生成されうる（構文に単項マイナスが存在せず、
`sql::udf_call::parse_number_literal` が非有限値を bind 時に拒否する）が、将来の
構文拡張に備え、`id_bounds` は非有限・負のリテラルにも防御的に正しい境界（`Gt`/
`Ge` は全件、`Lt`/`Le`/`Eq` は空集合）を返す。判定不能（非有限リテラル）は `None`
であり「一致 0 件」ではない——呼び出し元は索引を使わず全走査へ縮退する。

### 候補削減（`ScalarIndex::resolve_candidates`）

`metadata_filters` と `id_preds`（`classify_scalar_plan` が `PlainScan` 以外へ
分類した述語のみ）から候補スロット集合を導出する。各述語の候補を個別に取得し
（`None` は「列未索引」または `id_index` が `None`〔`id > 2^53` を含む世代〕によ
る判定不能）、複数述語は昇順ベクトルの 2 本指マージ交差で結合する。交差はスロッ
トを「絞る」ことしかできず「通す」ことはできないため、呼び出し元（`sql::exec`）
が候補行にも引き続き `on_visible_row`（`matches_all`＋式述語）を適用する契約と
組み合わさって fail-closed が成立する——`resolve_candidates` 自体が正しさの唯一
の防御ではない。

選択度切替: 交差後の候補数（`hits`）が索引行数（`row_count`）に対して
`hits × ratio.denominator > row_count × ratio.numerator`（`checked_mul`。
`sql::hnsw_cache` の `full_scan_ratio` と同型）を満たす場合、索引経路より全走査
が有利と判断し `FallbackSelectivity` へ縮退する。**暫定既定値は 1/2**
（`sql::hnsw_cache` の ANN 先例〔1/10〕をそのまま採ると、想定フェーズ
`vector_knn_where`〔`lang = 'ja'` が可視行に占める割合は `feature_bench` コーパス
〔5 値輪番〕では約 20%、crossdb fixture（`scripts/crossdb_bench/`）では約 33%
（7,621/23,000。`docs/design/crossdb-bench.md`「公平性の注記」参照。Issue #661）〕
が索引経路に乗らず受入条件が vacuous になるため（いずれの割合でも暫定既定 1/2
は下回り非 vacuous）、根拠を「索引経路のコストは概ね `O(|hits|)`、全走査は
`O(N + |hits|)` であり損益分岐は `hits ≈ N` 近傍にしかない」という構造から緩めに
設定した）。数値の確定は Issue #476。

### 部分集合アリーナ構築（`VectorArena::build_from_cached_rls_rows_subset`）

`build_from_cached_rls_rows`（全走査）と同じ `push_visible_row` を共有し、候補
スロット（狭義昇順・重複なし）だけを辿って `on_visible_row` を適用する。候補が
全スロット（`0..metadata.len()`）の場合、出力アリーナ（行・順序・スロット番号）
は全走査版とビット同一になることを単体テストで固定した（`arena::tests::
build_from_cached_rls_rows_subset_with_all_slots_matches_full_scan`）。非昇順・
重複スロットは呼び出し規約違反として `InvalidInput` で拒否する（多層防御）。

### `sql::exec` の結線

`SqlArenaCache` ヒットかつ SCALAR 段に実質的な処理がある分岐でのみ結線する
（キャッシュミス・`cache_fast_path_eligible`〔恒等写像〕経路は無変更）。

1. `classify_scalar_plan` が `PlainScan` でない場合のみ `ScalarIndexCache::lookup`
   を呼ぶ
2. **索引↔スナップショット同一性ガード**: `ScalarIndexCache` のキーは
   `(table, ctx)` × テーブル世代でありこのクエリの `SqlArenaSnapshot` そのものの
   同一性ではないため、`index.row_count() == snapshot.arena().len()` かつ
   `index.built_table_generation() == snapshot.built_table_generation_for_index()`
   を満たす場合に限って候補を使う。不一致時は全走査へ縮退するだけでクエリの正し
   さには影響しない（`hnsw_cache` が `kernel::dot` 再計算でスロット写像を検証する
   のと同じ位置づけ）
3. `resolve_candidates` が `Use(slots)` を返した場合のみ
   `build_from_cached_rls_rows_subset`、それ以外（`FallbackNoIndex`／
   `FallbackSelectivity`／同一性ガード不一致）は従来どおり
   `build_from_cached_rls_rows`（全走査）
4. 観測用統計 `ScalarIndexCacheStats::index_scans`／`plain_scan_fallbacks`
   を追加した（機微情報なし）
5. Issue #473 の gated 構築ブロック（ミス経路後の初回索引構築）は残置しつつ、同一
   クエリ内で `lookup` を二重に呼んで `hits`/`misses` 統計を二重計上しないよう、
   結線ブロックの `lookup` 結果を後続ブロックへ引き継ぐ（`scalar_index_lookup_hit`
   変数。`tests/scalar_index_cache.rs::
   where_query_builds_once_then_hits_within_same_generation` が固定）
6. `precision` モード・HNSW `Subset`／hybrid 密側アダプタは出力アリーナが全走査
   経路と同一のため無変更で動く（`tests/hnsw_cache.rs`・`tests/
   hnsw_hybrid_refetch.rs` が green のまま）

### `EXPLAIN` の `scalar_plan:` 行

`sql::using_plan::PreCheckShape` へ `scalar_plan: ScalarPlan` を追加し
（`USING PLAN` は `HINT ORDER` を受理しないため `scalar_prefilter` は常に
`true`）、`sql::explain::ExplainEngine` へ渡す。`build_explain_result` は既存
`ann_plan:` 行の直後（末尾）へ `scalar_plan: <token>`（`plain_scan`／
`index_equality`／`index_prefix`／`index_id_range`／`index_conjunction`。閉じた
語彙・snake_case）を追記する。静的判定のみ（`id_index` None・選択度縮退・世代
競合などの実行時結果、件数・閾値は非露出。`EXPLAIN` は索引 `lookup`／構築を呼ば
ない契約を維持）。既存行の文言・順序は不変。

## 対象ファイル

| パス | 変更 |
| --- | --- |
| `crates/engine/src/sql/scalar_plan.rs`（新規） | `ScalarPlan`・`ScalarShapeInput`・`classify_scalar_plan`・`IdPredicate`・`id_predicate_from_expr`・`id_bounds` |
| `crates/engine/src/sql.rs` | `pub(crate) mod scalar_plan;` |
| `crates/engine/src/sql/scalar_index.rs` | 照会 API の公開化・`candidates_id_range` の二分探索化・`resolve_candidates`・`CandidateResolution`・選択度定数・統計フィールド |
| `crates/engine/src/arena.rs` | `build_from_cached_rls_rows_subset` |
| `crates/engine/src/sql/exec.rs` | ヒット分岐への結線・重複 `lookup` 回避 |
| `crates/engine/src/sql/using_plan.rs` | `PreCheckShape.scalar_plan` |
| `crates/engine/src/sql/explain.rs` | `ExplainEngine.scalar_plan`・`scalar_plan:` 行 |
| `crates/engine/src/core.rs` | `EXPLAIN` アームでの受け渡し |
| `crates/engine/tests/scalar_index_prune.rs`（新規） | 等価性マトリクス・エラー契約・RLS オラクル・選択度縮退 |
| `crates/engine/tests/sql_explain.rs`・`crates/wire-server/tests/wire_explain.rs` | `scalar_plan:` 行の追記に伴う既存アサーション更新 |

依存追加なし・`unsafe` なし・spec 本文転記なし。

## 検証

`cargo fmt --all --check`・`cargo clippy --workspace --all-targets -- -D warnings`
（`make lint` 相当）・`cargo test --workspace --all-features`（`make test` 相当）・
`make core-api-check`・`make sort-determinism-check` が green（`VectorCore`／
`SearchProvider` は無変更）。

## 前後比較（§7.2 ひな形。専有環境再実測はオーナーへ申し送り）

本開発環境は共有環境のため、以下は**参考値・採否根拠にしない**。採用根拠は
構造的非退行（索引は候補を絞るだけで `O(|hits|)`・全走査経路は無変更）と等価性
テスト（cold/hot 完全一致・RLS 対照オラクル）である。専有環境での
`make bench-scan-stage-profile`（W0-hot・R_dot 参照区間）・`feature_bench` の
`vector_knn_where`／`point_where`（参照区間: 変更を含まない `vector_knn`／
`rls_isolation`）・`scripts/crossdb_bench` 実測はオーナー作業として申し送る。

現行索引経路（本 doc）を選択率 33%（crossdb fixture 相当）で段別に内訳計測した
結果は `docs/design/filtered-distance-stage-profile.md`（Issue #653）を参照。

## 後続 Issue #654

候補削減が実際に消費された `VectorArena` の構築（`build_from_cached_rls_rows_subset`）
は候補行を新規アリーナへ複製していたため、Issue #654 で候補スロットを直接
マスクとして探索する経路（`arena.rs::filter_cached_rls_rows_subset`・
`kernel.rs::SearchProvider::search_subset`）を追加し、hybrid・HNSW `Subset`
形状を除く DISTANCE 経路で複製を回避した。詳細は
`docs/design/scalar-index-mask-search.md` 参照。#654 の前後比較実測（本 doc
§7.2「専有環境再実測はオーナーへ申し送り」の対象範囲）は Issue #655 で
段別プロファイル・crossdb 双方を実施済み（`vector_knn_where` の e2e min-of-N
比で段別 0.5177・crossdb 0.6767。同ドキュメント「前後比較実測（Issue #655）」
節参照。共有環境の参考値のため専有環境再実測は引き続きオーナーへ申し送り）。

## Issue #844: 索引で完全被覆された述語の候補再検証省略（多層防御の限定緩和）

本 doc の「索引は行を『絞る』ことしかできず『通す』ことはできない」設計を、オーナー承認（2026-09-18）のもと次の 3 条件を**全て**満たす場合に限り緩和し、候補スロットへ `on_visible_row`（マスク済みデコード＋ `matches_all` 再適用）を掛け直さずに候補集合をそのまま信頼する。

1. `classify_scalar_plan` が `PlainScan` 以外で、残余述語（索引非対応述語・式述語）が無い（静的判定）
2. `ScalarIndex::resolve_candidates` が `Use` を返す（`FallbackSelectivity`〔1/2〕による plain scan 縮退は従来どおり）
3. 索引↔スナップショット同一性ガード（行数＋テーブル世代）を通過する

索引は `(table, PolicyContext)` 可視スナップショットから構築される（Issue #473）ため、この緩和でテナント境界は変わらない。hybrid・HNSW `Subset` 形状・`HINT ORDER`・`COUNT(col)`（NULL 判定が必要）は従来経路のまま。

### 実装箇所

- `sql/exec.rs`: 信頼マスク分岐 `mask_trusted_defer`（候補スロットを直接 `ScalarSource::Deferred(DeferredScalars::Snapshot)` へ渡す。`SELECT id` 単独投影でも `EagerSubset` へ落ちない）
- `sql/aggregate.rs::count_star_only`／`Accumulator::observe_present_n`（`COUNT(*)` のみの集計で候補件数を直読み。非 Count 累算器へ到達した場合は `accumulator_bug` として fail-closed）
- `sql/group_by.rs::observe_group_count_only`（列挙形・`COUNT(*)` のみ・`WHERE` なしのとき辞書 posting 長をグループ件数として直読み。`MAX_GROUPS`・キーバイト上限は維持）
- `sql/scalar_index.rs::ScalarIndexCacheStats::index_trusted_mask_scans`（非 vacuous 証跡用カウンタ。`SqlArenaCacheStats::fast_path_borrows`／`full_rebuild_copies` と合わせて pub フィールド 3 件を追加）

### 差し戻し手順

設計方針を元に戻す場合は次の 3 箇所を撤去すれば Issue #474 の候補再適用経路へ戻る（他の変更はそのまま残せる）。

1. `sql/exec.rs` の `mask_trusted_defer` 分岐（`hnsw_subset_eligible` 判定の直後）
2. `sql/aggregate.rs::try_scalar_index_aggregate` の `count_star_only` 早期リターン
3. `sql/group_by.rs::observe_group_count_only` の呼び出し

### 検証

`crates/engine/tests/scalar_index_mask_search.rs`（5 述語形状で plain-scan オラクル〔`AND id + 0 > 0` で索引を無効化〕と結果一致・他テナント private 行の非漏えい・世代 bump 後の不使用）・`tests/scalar_index_aggregate.rs`（NULL グループ・TABLE-12 重複 id を含む）。A/B 実測は `docs/design/crossdb-loss-analysis-20260918.md` §4.4。

## Issue #893: 新スカラー型（INTEGER/BIGINT/REAL/DOUBLE/DATE/TIMESTAMP/NUMERIC/UUID）のスカラー列二次索引対応

Phase 2（Issue #881〜#890）で追加した新スカラー型は、`ScalarIndex::build` が
一律 `per_column.push(None)`（未索引）へ縮退させたままだった。本 Issue で
`sql::scalar_index::OrderedColumnIndex`（`TextColumnIndex` の順序キー版）と、
`ScalarIndex::typed_columns`（`columns` と排他な並行フィールド）・
`ScalarIndex::candidates_typed_range` を追加し、これらの型を等価・範囲述語で
候補削減できる索引データ構造・照会 API を実装した。

### データ構造とキー導出

- `INTEGER`／`BIGINT`／`DATE`／`TIMESTAMP` → `i64` へ昇格した `OrderedColumnIndex::I64`
- `REAL`／`DOUBLE` → `f64_to_sortable_bits`（IEEE 754 ビットパターンを全順序
  比較可能な `u64` へ写像する標準変換）による `OrderedColumnIndex::F64Sortable`
- `NUMERIC(p, s)` → 列固定の `scale` のもとで `unscaled`（`i128`）の大小が
  そのまま値の大小になることを利用した `OrderedColumnIndex::I128`
- `UUID` → ネットワークバイトオーダーのバイト列を `u128`（ビッグエンディアン）
  として扱う `OrderedColumnIndex::U128`（`crate::uuid::Uuid` の `Ord` 導出
  ——バイト列辞書順——と同じ大小関係）

`BOOLEAN`・`BYTEA`・`JSON`・`JSONB`・`ARRAY`・`VECTOR` 列は引き続き索引対象外
（`typed_columns[i] = None`）。値域が 2 値（`BOOLEAN`）・等価/前方一致/範囲
述語を持たない（他）という Issue #883・#886・#888・#889 時点の判断を維持する。

`NULL` はいずれの typed 列索引にもエントリを作らない（`TextColumnIndex` と
同じ契約）。予算計上（`typed_column_reservation_bytes`）は固定長キーのため
`row_count` 件分の一括確保をアロケーション前に検証するだけでよく、`TEXT` 列の
ような重複排除後の再確保・`equality` テーブルは不要。

### `BIGINT`／`TIMESTAMP` の `2^53` ゲート

`#891`（新スカラー型の `WHERE` 述語・式評価対応）の評価器がリテラル比較を
f64 経由（損失あり）で行う可能性に備え、絶対値が `2^53`（`f64` の仮数部が
整数値を正確に表現できる上限。`id_index`・`sql::scalar_plan::MAX_EXACT_ID`
と同じ基準）を超える値を 1 件でも含む列は、列単位で `None`（索引対象外）へ
fail-closed に縮退する（`INTEGER`・`DATE` は値域が構造的にこの上限に収まる
ため対象外にしない）。ちょうど `2^53`（絶対値）は境界として索引に残る。

### `#891` 依存によるスコープ限定

本 Issue 着手時点（2026-09-25）で `#891`（新スカラー型の `WHERE` 述語・式
評価表現）は未マージだった。新スカラー型の列は `sql::udf_call::bind_expr_in`
がまだ列参照そのものを `22000` で拒否しており、`declarative_filter::FilterOp`
にも数値・日時型の範囲比較 variant が存在しないため、**SQL 表層（`WHERE`
句）から本索引の typed 経路へ到達する構文が現時点で存在しない**。

このため本 Issue は次のスコープに限定した:

- 索引データ構造・照会 API（`OrderedColumnIndex`・`candidates_typed_range`）は
  完全実装し、brute-force オラクル対照の単体テストで正しさを固定した。
- `sql::scalar_plan::TypedRangePredicate`（`#891` の述語表現から
  `candidates_typed_range` の引数形へ正規化した表現。`IdPredicate`／
  `id_bounds` の typed 版に相当）は型定義のみ用意し、`BoundExpr` から本型への
  変換アダプタ（`id_predicate_from_expr` の typed 版に相当）は実装していない
  （`#891` の述語表現が確定していないため実装すると `#891` と重複・競合する）。
- `ScalarIndex::resolve_candidates` は `typed_preds: &[TypedRangePredicate]`
  引数を追加済みで、`sql::exec`・`sql::aggregate`・`sql::group_by` の 3 呼び
  出し元はすべて配線済みだが、アダプタ未接続のため常に空スライス（no-op）を
  渡す。
- `sql::scalar_plan::ScalarPlan`（`classify_scalar_plan` の分類結果）・
  `EXPLAIN` の `scalar_plan:` トークンはいずれも無変更——typed 述語を分類する
  `IndexRange` 相当の variant は、それを実際に返す経路（`#891` アダプタ）が
  無い状態で追加すると「一度も構築されない enum variant」になり、
  `-D warnings` 下の `dead_code` 検出（`clippy --all-targets`）に抵触する
  ため見送った。

`#891` 接続後の残作業（別 Issue の担当）:

1. `BoundExpr`／`declarative_filter::WherePredicate` 側に新スカラー型の
   範囲比較表現が確定した時点で、`typed_predicate_from_expr`（`id_predicate_from_expr`
   に相当）を実装し `TypedRangePredicate` へ正規化する。
2. `classify_scalar_plan` を `typed_preds` を受け取る形へ拡張し
   （既存 `classify_scalar_plan` は空スライスで委譲する後方互換シグネチャを
   維持できる）、`ScalarPlan::IndexRange`（もしくは `IndexConjunction` への
   合流）を追加する。
3. `EXPLAIN` の `scalar_plan:` トークンへ `index_range` を追記する。
4. 索引で完全被覆された typed 述語の信頼マスク（Issue #844 の `mask_trusted_defer`
   等）への昇格は、索引と評価器の同値性を性質テストで固定したうえで別途判断する
   （本 Issue 時点では意図的に対象外のまま——`resolve_candidates` は typed
   述語の候補も従来どおり `on_visible_row` の多層防御を経由する）。

### 対象ファイル（本 Issue 分）

| パス | 変更 |
| --- | --- |
| `crates/engine/src/sql/scalar_index.rs` | `OrderedColumnIndex`・`TypedKey`・`typed_columns`・`push_typed_value`・`candidates_typed_range`・`resolve_candidates` の `typed_preds` 引数・予算計上関数・単体テスト |
| `crates/engine/src/sql/scalar_plan.rs` | `TypedRangePredicate`（アダプタ未実装。`#[cfg_attr(not(test), allow(dead_code))]`） |
| `crates/engine/src/sql/exec.rs`・`aggregate.rs`・`group_by.rs` | `resolve_candidates` 呼び出しへ `&[]` を追加（配線のみ・挙動不変） |

依存追加なし・`unsafe` なし・spec 本文転記なし。既存クエリの挙動・`EXPLAIN`
出力・Recall ゲートはいずれも本 Issue の前後で完全に不変（typed 述語が実際に
発生する経路が無いため）。
