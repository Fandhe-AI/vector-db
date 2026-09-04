# フィルタ付き ANN の境界再取得（iterative scan 型）と fail-closed 検証（Issue #410）

親 Issue #402（Phase 3・ANN opt-in 採用）。前提 #408（`sql::hnsw_cache::
HnswIndexCache`）・#409（`docs/design/hnsw-rls-cardinality-switch.md`）。対象
ビヘイビア（ポインタのみ）: CORE-9・CORE-10・TASK-132・SEARCH-1・SEARCH-3・
RLS-1〜4・TASK-138・TASK-139。ADR: `docs/design/ann-index-adoption.md`
（Accepted・B 案）。本ドキュメントが定める閾値・ラウンド上限・統計名はいずれも
本リポの実装既定値（非規範）であり、`wire_code` の新設・`EXPLAIN` への露出
（#411）は行わない。

## 背景・目的

Issue #410 は「フィルタ付き ANN が結果不足・境界同点未確定のとき `ef` を段階的
に拡張して再探索する（iterative scan）」ことを目的として起票された。調査の
結果、この目的を満たす経路は当初想定と異なることが判明した:

- **DISTANCE 経路（`sql::hnsw_cache::search_with_overlay`）の「結果不足」
  （`masked_short`）は #409（PR #435）以降、構造的に到達不能**（下記「証明」
  節）。この分岐へ `ef` 倍増ループを追加すると、本リポが避ける方針の
  到達不能分岐（`hnsw.rs::select_neighbors_heuristic` の `extend_candidates`
  注記と同じ方針）になるため、**追加しなかった**。
- **iterative scan が非 vacuous に効くのは hybrid 密側の再取得ラウンド**
  （`hybrid.rs::hybrid_search_boosted` の `dense_fetch_k` 倍増ループ）。
  このループは元々 `sql::hnsw_cache` を経由せず生の `&dyn SearchProvider`
  （全件 brute-force）を使っていたため、ここへ HNSW を結線することが
  #410 の実質的なスコープになった。

## DISTANCE 経路の `masked_short` 到達不能性（証明）

`search_masked`（`hnsw.rs`）は次の 2 点を保証する:

1. 層 0 の初期候補集合に、検査済み起点（`search_entry_for_mask` の戻り値。
   `is_mask_fully_reachable` と同じ起点選択）を必ず含める（降下後ノードと
   異なる場合。PR #435）。
2. `overlay.mask_splits_graph == false`（`Overlay::compute` が世代毎に 1 回
   判定）のとき、`is_mask_fully_reachable` は検査済み起点から**受理ノード
   全体**への到達可能性を保証済み。

`search_layer` の受理判定は「`results.len() >= ef && strictly_farther` の
場合のみ打ち切る」規約であり、それ以外は候補ヒープが空になるまで受理ノードを
`results` へ積み続ける（`worst_ok = results.len() < ef` の間は常に真）。1・2
と合わせると、`mask_splits_graph == false` のとき

```text
index_hits.len() >= min(ef_eff, visible_in_index) >= min(k, visible_in_index)
                                                     = expected
```

が常に成立する（`ef_eff = effective_ef(k) = ef_search.max(k).min(MAX_EF)`
が `k` 以上のため）。`search_with_overlay` の `masked_short` 分岐
（`index_hits.len() < expected`）はこの不等式と矛盾するため、
`mask_splits_graph == false` の間は到達しない。`mask_splits_graph == true`
のときは `search_masked` 自体を呼ばず別分岐（統計 `mask_splits_graph`）で
plain scan するため、こちらも `masked_short` へは到達しない。

結論: 現行実装では `masked_short` は防御的分岐（想定外の不整合に対する
fail-closed の最終防御）に留まり、「`ef` 拡張で結果不足を解消する」という
狙いの対象にはならない。`docs/design/hnsw-rls-cardinality-switch.md`
「切替規則」節・`sql/hnsw_cache.rs` の `masked_short` コメントへこの結論を
反映済み。

## hybrid 密側再取得ループへの結線

### 現状整理

`hybrid.rs::hybrid_search_boosted` の密側再取得ループは、境界同点グループが
未確定（`TieBoundary::Undetermined`）の間 `dense_fetch_k` を
`pool_depth * 2` から倍増しつつ `provider.search` を呼び直す（Issue #310・
#320。`docs/design/hybrid-recall-regression.md`「Issue #310」節・
`docs/design/rrf-tie-break-determinism.md` 参照）。この `dense_fetch_k` の
単調倍増そのものが iterative scan の実体であり、Issue #410 の役割は
「各ラウンドの `provider.search` を HNSW 索引経由にする」ことに絞られる。

### 設計: 準備／探索の分離（`sql/hnsw_cache.rs`）

`search_or_fallback`／`search_subset_or_fallback`（#408・#409）は「索引・
オーバーレイの解決」（`IndexedBase::build`・`Overlay::compute` を伴う O(N)
相当の重い処理。`k` に依存しない）と「Top-k 探索」（`k` に依存し軽い）が
1 関数に混在していた。hybrid の複数ラウンドで毎回呼ぶと重い解決を毎ラウンド
やり直すことになるため、次の中間表現へ分離した:

- `PreparedHnswSearch`（`pub(crate) enum`）: `Indexed { base, overlay,
  success_stat }` または `FullScan`。
- `prepare_full_visible`／`prepare_subset`: 解決のみを行い
  `PreparedHnswSearch` を返す（`search_or_fallback`／`search_subset_or_fallback`
  から抽出。統計計上位置・縮退分岐・fail-closed 契約は移動していない）。
- `search_prepared`: `PreparedHnswSearch` を使って 1 ラウンド分の Top-k
  探索を行う（`Indexed` なら既存 `search_with_overlay` をそのまま呼ぶ——
  この関数自体は無変更。`FullScan` なら `slot_ids` を使う全件 brute-force）。

既存の `search_or_fallback`／`search_subset_or_fallback`（DISTANCE 単発
クエリ向け）は「`prepare_*` を 1 回呼び `search_prepared` を 1 回呼ぶだけの
薄いラッパー」へ書き換えた。単発クエリでは解決・探索をそれぞれ 1 回ずつ
呼ぶだけであり、統計・戻り値・エラー契約は分離前とビット単位で同一
（`crates/engine/src/sql/hnsw_cache.rs` の既存 in-module テスト・
`crates/engine/tests/hnsw_cache.rs` が無変更のまま green であることで確認
済み）。

### アダプタ（`sql/hnsw_hybrid.rs`。新設）

`HnswDenseProvider<'a>` は `sql::exec::execute_statement_with_cache` の
`Ranking::Hybrid` 分岐がクエリ開始時に一度だけ構築する `SearchProvider`
アダプタ。`access`（`HnswCacheAccess`）・`arena`・`slot_ids`・`inner`
（索引を使わない場合の委譲先。既存の hybrid 経路がそのまま使っていた
provider）・`prepared`（`prepare_full_visible`／`prepare_subset` の結果）を
保持する。

**fail-closed な受理条件**: `search(input)` は `input.vectors`／`input.ids`
が構築時に捕捉した `arena.vectors()`／`slot_ids` と**同一バッファ**（ポインタ
・長さが一致）を指している場合に限り `search_prepared` を呼ぶ。1 つでも
外れれば `inner` へそのまま委譲する。`hybrid.rs::hybrid_search_boosted` の
密側再取得ループは `dense_input.ids = input.ids`／`dense_input.vectors =
input.vectors`（`k` のみ変更）で `provider.search` を複数ラウンド呼ぶ契約
（コード事実。`hybrid_search_boosted` 本体で確認済み）であり、`sql/exec.rs`
が構築する `SearchInput` も常に `&slot_ids`／`arena.vectors()` を渡すため、
この受理条件は同一クエリの全ラウンドで一貫して成立する（アダプタの
in-module テスト `search_reuses_prepared_base_across_rounds_for_the_same_buffer`
で固定）。別クエリ・別バッファに対して索引経由で答えてしまう構造的リスクを
この ptr-eq 判定で塞ぐ。

`sql/exec.rs` の `Ranking::Hybrid` 分岐は、`hnsw_hybrid_full_visible_eligible`
／`hnsw_hybrid_subset_eligible`（DISTANCE 版の `hnsw_full_visible_eligible`
／`hnsw_subset_eligible` と対称。`is_hybrid` を要求する点のみが異なる）の
いずれかが真かつ `hnsw_cache` が `Some` の場合のみアダプタを構築し、
密のみ縮退経路（疎コーパス 0 件）・`hybrid::hybrid_search`（疎索引あり）の
両方をこのアダプタ経由にする。`hybrid.rs`・`SearchProvider` trait は無変更。
`precision` モードは DISTANCE と同じ理由（`precision_policy.hybrid()` の
確信度ゲートは厳密順位を前提とするため。TASK-162・SEARCH-9）で対象外。

### 統計（`HnswIndexCacheStats`）

- `ef_cap_fallbacks`: `search_with_overlay` が `k > MAX_EF` を `search_masked`
  呼び出し前に検出し（`HnswError::InvalidParams` の文字列比較に依存しない
  よう検証順序を先取り）、直ちに plain scan へ縮退した回数（`fallbacks` の
  内数）。`hybrid.rs::MAX_FETCH_K`（`MAX_POOL_DEPTH * 4` = 40,000）は
  `crate::hnsw::MAX_EF`（10,000）の 4 倍のため、密側再取得ループが
  `fetch_k` を伸ばし切ると理論上到達しうる。
- `hybrid_dense_searches`: アダプタが受理条件を満たし `search_prepared` を
  呼んだ回数（1 クエリで複数ラウンドぶん加算されうる）。
- `hybrid_queries`／`hybrid_rounds_max`: `HnswDenseProvider::finish`
  （クエリ終了時に `sql/exec.rs` が明示的に呼ぶ。ロックを取る統計反映を
  `Drop` に持ち込まない設計）が、そのクエリで観測されたラウンド数を
  `hybrid_queries`（+1）・`hybrid_rounds_max`（CAS で最大値を更新）へ反映
  する。索引経路を一度も使わなかったクエリ（`rounds == 0`）は加算しない。

いずれもテナント ID・行 ID・スコアを含まない（`HnswIndexCacheStats` の既存
方針を踏襲）。

## 停止性・決定性

- 停止性は `hybrid.rs` 側の `dense_cap = MAX_FETCH_K.min(input.ids.len())`
  と `dense_fetch_k` の単調倍増（Issue #310・#320。本 Issue で無変更）が
  provider の実装に依らず保証する。ラウンド数は高々
  `⌈log2(dense_cap / (2 · pool_depth))⌉ + 1`（既定 `pool_depth = 200` なら
  小〜中規模コーパスで 8 以下）。
- `k > MAX_EF` のラウンド（`fetch_k > 10,000`）は `ef_cap_fallbacks` 経由で
  brute-force 縮退し厳密結果になる。ラウンドごとに近似（ANN）／厳密
  （brute-force）が混在しうるが、`hybrid.rs` は各ラウンドの `hits` を
  置き換える設計のため整合性は崩れない。
- 決定性（同一索引・同一クエリ・同一世代で同一結果）は、索引ヒットを常に
  `kernel::dot` で再計算する既存契約（#408）と `search_with_overlay` 自体が
  無変更であることから維持される。`crates/engine/tests/hnsw_hybrid_refetch.rs
  ::tie_inducing_corpus_hybrid_search_terminates_and_is_deterministic` が
  同点誘発コーパス（`quantize_levels`。`benches/harness/hybrid_latency.rs`）
  で同一クエリを 3 回実行し結果が完全一致すること・`hybrid_rounds_max <= 8`
  であることを固定する。

## `complete_boundary_tie_group` との相互作用

各ラウンドは独立した `provider.search` 呼び出しであり、
`resolve_boundary_tie_group`（Issue #310・#320）は**そのラウンドの返却列**
に対して境界同点グループの終端確定を行う。ANN 経由では返却列が近似候補列
であるため、同点グループの完全性は「ANN 候補順序に対する完全性」であり
真の総当たり順序に対する完全性ではない（`docs/design/hnsw-search.md`
「決定性の保証範囲」と同じ位置づけ）。決定性（同一索引・同一クエリ・同一
世代で同一結果）は維持されるが、Recall はブルートフォース対照からの近似
乖離を許容する（`hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`
が Recall@10 ≥ 0.9 の回帰基準で検証）。ラウンド間の前方一致（疎側
`SparseScored::top` が持つ性質。Issue #392）は密 ANN 側では保証しない
（`ef` 拡大でより良い候補が見つかれば prefix が入れ替わりうる）。`hybrid.rs`
はラウンドごとに `hits` を丸ごと置き換える設計のため契約上の問題はない。

## 検証

- 単体（`sql/hnsw_hybrid.rs` in-module）: 別バッファは `inner` へ委譲（統計
  非汚染）・同一バッファの複数ラウンドで `prepare_*` が 1 回のみ実行される
  こと（非 vacuous）・`k > MAX_EF` の brute-force 縮退（空集合の誤返却なし）
- 単体（`sql/hnsw_cache.rs` in-module）: 既存テストが分離後も無変更のまま
  green（`search_or_fallback`／`search_subset_or_fallback` の振る舞い・
  統計計上位置が不変であることの確認）
- 結合（`crates/engine/tests/hnsw_cache.rs`）:
  `filtered_distance_bypasses_full_visible_entries_and_matches_default_engine`
  （フィルタ付き DISTANCE は `FullVisible` エントリを占有しない契約を維持）・
  `hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`
  （新設。既定エンジン対照 Recall@10 ≥ 0.9・可視外テナント非混入・
  `hybrid_dense_searches > 0` の非 vacuous 検証。旧来「hybrid は常に
  `HnswIndexCache` を迂回する」としていた契約を反転した——旧テストは実際には
  hybrid SQL を一切実行しておらずその主張を検証していなかった点も含め、
  本 Issue で是正した）・
  `hybrid_queries_use_subset_shape_and_match_default_engine_recall`
  （新設。`hnsw_hybrid_subset_eligible`（`WHERE` 付き hybrid）は本テストでしか
  経由しない分岐のため、`subset_searches > 0` **かつ** `hybrid_dense_searches
  > 0` の両方で非 vacuous を固定する。可視カーディナリティ比・グラフ連結性は
  コーパスの乱数シードに依存し、シードによっては `mask_splits_graph` 経由の
  plain scan 縮退（fail-closed。誤りではない）に落ちて `subset_searches` が
  0 のままになりうることを実装中に確認したため、実測で `subset_searches > 0`
  になることを確認済みのシードを固定して使う）
- 結合（新設 `crates/engine/tests/hnsw_hybrid_refetch.rs`）: 同点誘発
  コーパスでの停止性（`hybrid_rounds_max <= 8`）・複数ラウンドの実発生
  （`hybrid_rounds_max >= 2`。`prepared` の再利用が SQL 表層経由でも実際に
  複数ラウンドにわたって働くことの確認）・決定性（3 回実行の完全一致）。
  `k > MAX_EF` の SQL 表層直接誘発は `LIMIT` の許容上限（`crate::hnsw::
  MAX_EF` と同値）により不可能なため、この経路は単体テストでのみ検証する
- `make core-api-check`（`SearchProvider`/`VectorCore` trait 差分ゼロ。
  シグネチャ無変更）・`make sort-determinism-check`（`sort_by` のみ使用）
- 既存の hybrid・RLS 統合テスト（`tests/hybrid.rs`・`tests/hybrid_recall.rs`
  層 A・`tests/sparse_cache_recall.rs`・`tests/sparse_determinism.rs`・
  `tests/plan_rls_boost.rs`・`tests/default_preset.rs`）は無変更のまま green

## Phase B（訪問済みビットマップを引き継ぐ再開型スキャン）の採否

Issue 起票時の作業内容に「訪問済みビットマップの引き継ぎ」があったが、上記
「停止性・決定性」節のとおり停止性・k 件充足は既存の `dense_cap`・
`ef_cap_fallbacks` で既に保証されており、受け入れ基準はいずれも満たされて
いる。`hnsw.rs::search_layer` を「seed → run(ef) → 再開」型へ再構成する
実装は、`hnsw.rs` の構築経路・探索経路が共有する `search_layer` の複製を
避けられるかの検証（bit 同一ゲート: `tests/hnsw.rs`・`tests/hnsw_search.rs`・
`tests/hnsw_cache.rs` 全件 green）にコストが見合うだけの実測上の必要性
（`ef` 拡張のたびに新規探索をやり直すコスト超過の実測）がこの時点では
確認できていないため、本 Issue のスコープでは**実装を見送った（Rejected）**。
将来、hybrid 密側の再取得ラウンド数・レイテンシが実運用上問題になった場合に
`make bench-hybrid-profile`（Issue #356・#387〜#392）で再開コストを実測した
うえで再検討する。

## セキュリティ考慮（OWASP Top 10 観点）

| 観点 | 対応 |
| ---- | ---- |
| アクセス制御の不備／テナント境界（P0） | 索引は `(table, ctx)` 可視アリーナのみから構築（不変）。アダプタは同一バッファ・同一 `slot_ids` の場合のみ索引経路を使い、それ以外は `inner` へ委譲（fail-closed）。索引ヒットはスロット写像・`(tenant_id, id)` キー照合・`kernel::dot` 再計算を経由し、`hybrid.rs` の可視 id 検証（`core::provider_result_is_valid`・`HybridError::ProviderResultRejected`）・`RlsSafetyNet` の多層防御は無変更。`PolicyContext::is_visible` に新規比較ロジックを追加していない |
| 存在情報の副次漏えい | 統計（`HnswIndexCacheStats`）にテナント ID・行 ID・スコアを含めない |
| 不安全な設計（DoS） | ラウンド数は `dense_cap`・`MAX_FETCH_K` で有界。`k > MAX_EF` は fail-closed に brute-force。`prepare_*` は準備 1 回に限定しラウンド数倍の O(N) を回避 |
| インジェクション | SQL 文字列の組み立てなし |
| untrusted 入力 | `k`／`fetch_k` は `HnswIndex::search_masked` の `MAX_EF` 検証と `hybrid.rs` の長さ検証を通る。`unwrap`/`expect`/添字アクセスは production コードに置いていない（`get()`／`checked_*` を使用） |
| 脆弱な依存 | 依存追加なし |
| private spec 漏えい（P0） | コメント・doc・コミット・PR は TASK／ビヘイビア ID のポインタ表記のみ |

## 将来の拡張・申し送り（本 Issue のスコープ外）

- `EXPLAIN` へのエンジン種別・ラウンド数・縮退有無の露出（#411）
- Recall 3 ゲートの ANN 同一閾値検証（#412）・前後比較と `full_scan_ratio`
  ／`MAX_EF` 既定値の再調整（#413）
- `precision` モード hybrid の ANN 化（確信度ゲート契約の再設計が前提）
- `SearchTimeFilter` 経路・Rust API `hybrid` 相当 API の結線
- Phase B（再開型スキャン）の再検討（実測に基づく必要性の確認後）
