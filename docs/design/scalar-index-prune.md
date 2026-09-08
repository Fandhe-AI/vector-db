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

## 後続 Issue #654

候補削減が実際に消費された `VectorArena` の構築（`build_from_cached_rls_rows_subset`）
は候補行を新規アリーナへ複製していたため、Issue #654 で候補スロットを直接
マスクとして探索する経路（`arena.rs::filter_cached_rls_rows_subset`・
`kernel.rs::SearchProvider::search_subset`）を追加し、hybrid・HNSW `Subset`
形状を除く DISTANCE 経路で複製を回避した。詳細は
`docs/design/scalar-index-mask-search.md` 参照。
