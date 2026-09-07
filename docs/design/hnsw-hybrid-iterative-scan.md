# フィルタ付き ANN の境界再取得（iterative scan 型）と fail-closed 検証（Issue #410）

親 Issue #402（Phase 3・ANN opt-in 採用）。前提 #408（`sql::hnsw_cache::
HnswIndexCache`）・#409（`docs/design/hnsw-rls-cardinality-switch.md`）。対象
ビヘイビア（ポインタのみ）: CORE-9・CORE-10・TASK-132・SEARCH-1・SEARCH-3・
RLS-1〜4・TASK-138・TASK-139。ADR: `docs/design/ann-index-adoption.md`
（Accepted・B 案）。本ドキュメントが定める閾値・ラウンド上限・統計名はいずれも
本リポの実装既定値（非規範）であり、`wire_code` の新設・`EXPLAIN` への露出
（#411）は行わない（#411 は実装済みだが、ラウンド数・実行時縮退結果は
引き続き非露出。`docs/design/explain-search-engine-exposure.md` 参照）。

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
と合わせると、`mask_splits_graph == false` のとき、層 0 探索の**切り詰め前**
の結果 `results`（`search_layer` の戻り値。`hnsw.rs::search_masked`）について

```text
results.len() >= min(ef_eff, visible_in_index)
```

が常に成立する（`ef_eff = effective_ef(k) = ef_search.max(k).min(MAX_EF)`
が `k` 以上であり、`min(x, visible_in_index)` は `x` に関して単調非減少の
ため `min(ef_eff, visible_in_index) >= min(k, visible_in_index) = expected`
も成立する）。`search_masked` は最後にこれを `take(k)` で切り詰めて返す
（`hnsw.rs::search_masked` 末尾）ため、呼び出し元
（`sql::hnsw_cache::search_with_overlay`）が受け取るのは
`index_hits.len() = min(results.len(), k)` である。`results.len() >=
expected` かつ `expected <= k` （`expected = min(k, visible_in_index)`）
なので

```text
index_hits.len() = min(results.len(), k) >= min(expected, k) = expected
```

が成立する。`search_with_overlay` の `masked_short` 分岐
（`index_hits.len() < expected`）はこの不等式と矛盾するため、
`mask_splits_graph == false` の間は到達しない。`mask_splits_graph ==
true` のときは `search_masked` 自体を呼ばず別分岐（統計
`mask_splits_graph`）で plain scan するため、こちらも `masked_short` へは
到達しない。

結論: 現行実装では `masked_short` は防御的分岐（想定外の不整合に対する
fail-closed の最終防御）に留まり、「`ef` 拡張で結果不足を解消する」という
狙いの対象にはならない。`docs/design/hnsw-rls-cardinality-switch.md`
「切替規則」節・`sql/hnsw_cache.rs` の `masked_short` コメントへこの結論を
反映済み。

## hybrid 密側再取得ループへの結線

### 現状整理

`hybrid.rs::hybrid_search_boosted` の密側再取得ループは、境界同点グループが
未確定（`TieBoundary::Undetermined`）の間 `dense_fetch_k` を
`pool_depth * 2` から倍増しつつ `provider.search` を呼び直す（Issue #310・#320。
`docs/design/hybrid-recall-regression.md`「Issue #310」節・
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
  provider の実装に依らず保証する。初期 `dense_fetch_k` は
  `min(2·pool_depth, dense_cap)` であり（実装は `hybrid.rs::
  hybrid_search_boosted` の `dense_fetch_k` 初期化を参照）、これが既に
  `dense_cap` に達している場合（`dense_cap < 2·pool_depth`。例:
  `dense_cap = 100`・`pool_depth = 200`）は倍増の余地がなく 1 ラウンドで
  確定する。ラウンド数は高々
  `⌈log2(dense_cap / min(2 · pool_depth, dense_cap))⌉ + 1`（分母を
  `min(2·pool_depth, dense_cap)` に補正した式。既定 `pool_depth = 200` の
  小〜中規模コーパス〔`dense_cap >= 2·pool_depth`〕では 8 以下）。
  `dense_cap = 0`（可視候補 0 件）の場合は `dense_fetch_k = 0` のまま
  `provider.search` を 1 回呼ぶだけ（式の分母が 0 になり定義できないため、
  この境界は式の対象外として明示する）で、空の結果を確定させて終了する。
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

- ~~`EXPLAIN` へのエンジン種別・ラウンド数・縮退有無の露出（#411）~~ エンジン種別・
  静的な ANN 適用判定のみ実装済み（ラウンド数・実行時縮退結果は引き続き非露出。
  `docs/design/explain-search-engine-exposure.md` 参照）
- ~~Recall 3 ゲートの ANN 同一閾値検証（#412）~~ 実装済み。
  `docs/design/ann-recall-gate-verification.md` 参照。前後比較と
  `full_scan_ratio`／`MAX_EF` 既定値の再調整（#413）は継続
- `precision` モード hybrid の ANN 化（確信度ゲート契約の再設計が前提）
- `SearchTimeFilter` 経路・Rust API `hybrid` 相当 API の結線
- ~~Phase B（再開型スキャン）の再検討（実測に基づく必要性の確認後）~~ 状態保持・
  決定性・停止性の契約設計は完了（Issue #504。上記「Phase B 再検討
  （Issue #504）」節参照）。実装（#505）・前後比較実測（#506）は継続

## Phase B 再検討（Issue #504）: 再開型探索の状態保持と決定性・停止性契約

親 Issue #503（`docs/design/hotpath-implementation-survey.md` §5・§9-#3c で
「条件付き採用」と評価された pgvector `hnswscan.c` 型の破棄候補ヒープ保持）。
Phase 3 親 #458／ルート #455。依存 #465（CLOSED。最新基線は
`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線（2026-09-06・
Issue #465）」節）。実装は #505、前後比較実測は #506（本 Issue のスコープ外）。
ステータス: **Proposed**（採否はオーナー判断。#505 のマージ根拠は本節が定める
正しさ契約——bit 同一ゲート・Recall 非劣化——であり、性能面の採否は #506・
専有環境実測に委ねる。共有 QEMU 環境の数値は採否根拠にしない。
`docs/design/benchmark-judgement-policy.md` §5〜6）。

上記「Phase B（訪問済みビットマップを引き継ぐ再開型スキャン）の採否」節の
**Rejected 判定を撤回するものではない**。当時は実測上の必要性が確認できず
見送ったのであり、本節はそれとは独立に「再開型にした場合、Issue #504 の
見出し主張（融合結果・境界同点グループ・`fetch_k` スケジュールが不変）が
どの範囲で成立するか」を #505 が機械検証できる粒度まで先に契約化する
（実装 GO を意味しない）。

### 現状の再実行型（コード事実の整理）

呼び出し系列は次のとおり（origin/main で確認済み。関数名・定数名は本節末尾の
すべてが対応するコードと一致することを実装前に再確認すること）:

1. `sql/exec.rs` の `Ranking::Hybrid` 分岐が `prepare_full_visible`／
   `prepare_subset`（`sql/hnsw_cache.rs`）をクエリ 1 回だけ呼び、
   `sql::hnsw_hybrid::HnswDenseProvider::new` を構築、終了時に `finish()`
2. `hybrid.rs::hybrid_search_boosted` の密側再取得ループが `dense_cap =
   MAX_FETCH_K.min(input.ids.len())` を上限に、初期 `min(2·pool_depth,
   dense_cap)` から `TieBoundary::Undetermined` の間 `dense_fetch_k` を
   倍増しながら `provider.search` を**毎ラウンド最初から呼び直す**
3. `HnswDenseProvider::search` は ptr-eq 受理条件（`input.vectors`／
   `input.ids` が捕捉済みバッファと同一の場合のみ）で `search_prepared` へ
   委譲し、`search_with_overlay`（`full_scan_ratio` 切替・
   `mask_splits_graph` 検査・`k > MAX_EF` の `ef_cap_fallbacks`・
   `search_masked`・`masked_short` ガード・スロット写像＋`kernel::dot`
   再計算・delta brute-force マージ・`sort_by(score desc, id asc)`・
   `dedup`・`truncate(k)`）を実行する

`prepare_*` は #410 で既にクエリ 1 回に償却済み。ラウンド毎に再評価される
のは `search_masked`（上位層貪欲降下・層 0 ビーム探索の全 visited ノードの
距離計算）・写像検証・delta brute-force の部分であり、再開型の狙いは
このラウンド間の重複探索を削減することにある。

`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線（2026-09-06・
Issue #465）」節のとおり、既定エンジン `hybrid_rrf` の `dense(B0)` は
25,000 行・dim 128 で全体の約 4.7〜4.8% にとどまる。再開型の効果は
**hnsw opt-in かつ再取得ラウンドが実際に複数回発火するクエリ**（同点誘発
コーパス等。`tests/hnsw_hybrid_refetch.rs` の条件）に限定され、全体レイテンシ
への寄与は構造的に有界であることをここに明記する。

### 状態保持契約

- **所有者・寿命**: クエリ単位。`HnswDenseProvider` インスタンスが保持し
  `finish()`（クエリ終了）で破棄する。**`HnswIndexCache` には置かない**——
  同キャッシュは世代キー・複数クエリ間で共有される構造であり、再開状態は
  特定の `prepared`（base ＋ overlay）・クエリベクトル・マスクの組に一意に
  束縛されるため、キャッシュに混ぜるとテナント・クエリを跨いだ誤共有の
  リスクを生む
- **内容**（`hnsw.rs` の型で記述。#505 が新設する）: 独自の visited
  ビットマップ（既存の thread-local `SEARCH_SCRATCH` はラウンド間で状態が
  リセットされる前提のため使えず、専用スクラッチを新設する）・`expanded`
  ビットマップ（`candidates.pop()` で実際に展開した——隣接ノードを走査
  した——ノードを記録する。`visited` とは別物: `visited` は隣接ノードとして
  **発見**された時点で立つのに対し、`expanded` はそのノード自身が候補
  ヒープから**pop されて隣接走査された**時点で立つ。**`discarded`
  ／`candidates` のどちらの集合に属するかとは独立**——`results.pop()` に
  よる `discarded` への追い出しは「そのノードが現在の `results` の
  top-ef から外れた」ことのみを意味し、`candidates.pop()` による展開の
  有無とは無関係に起こり得る〔push 時点で `candidates`・`results` の
  両方へ同時に積まれるため、`candidates` に残ったまま＝未展開のまま
  `results` からだけ追い出されるケースがある〕）・`in_candidates`
  ビットマップ（そのノードが現在 `candidates` ヒープへ push 済みで未 pop
  ——エントリが在庫として残っている——ことを示す。push 時に立て、pop 時に
  下ろす。上記の「`candidates` に残ったまま `results` からだけ追い出され
  る」ノードを resume 時に再判定する際、`discarded` 側だけを見て
  無条件に再 push すると同一ノードが `BinaryHeap` に重複して積まれるため、
  この重複防止に用いる——下記「再開手順」参照）・`candidates`
  （未展開の受理済みノード。`BinaryHeap<ScoredNode>`）・`discarded`
  （**「まだ自己昇格していない」未受理ノード全体の集合**——`worst_ok`
  不受理で候補ヒープへ一度も積まれなかったノード、および `results.pop()`
  で `results` から追い出されたノード——後者は `expanded` の有無を問わない。
  **除去は自己昇格の成功時のみ**——`candidates` へ push された（＝
  `in_candidates` が立った）ことは `discarded` からの除去条件にしない。
  この 2 つの集合（`discarded`＝自己昇格の再評価対象／`candidates`＋
  `in_candidates`＝展開キューへの在庫）は独立な軸であり、1 ノードが
  同時に両方へ属し得る（自己昇格に失敗しつつ展開キューには積まれている、
  という状態が正常に存在する。下記「再開手順」参照）。`candidates` と
  同じ `ScoredNode::Ord`——スコア降順・id 昇順の全順序）・`admitted`
  （これまでに受理されたノードとスコア。ラウンド r の返却は `admitted`
  の上位 `k_r` 件）・直前ラウンドの `ef`
- **再開手順**: ラウンド r（`k_r > k_{r-1}`）では、まず `discarded` の
  **全ノード**（`expanded` の有無を問わない）について `ef_r =
  effective_ef(k_r)` の下で `worst_ok` を再評価し、満たすものは
  `results`（および `admitted`）へ**直接**挿入し、**このときに限り**
  `discarded` から除去する（自己昇格）。これは discovery 時と同じ
  自己昇格処理を明示的に行うものであり、`candidates` への合流だけでは
  代替できない——`search_layer_in` の while ループは `candidates.pop()`
  した候補**自身**を `results` へ追加しない（隣接ノードのうち
  `worst_ok` を満たすものだけを追加する）ため、discarded ノードを
  `candidates` へ戻して pop させるだけでは、それ自身が新たな `ef_r` の
  下で top-ef 相当になっていても `results` へは決して戻らない（例: 起点
  E（score=3）から A（2）、B（1）へ接続し、A/B の隣接が E のみの場合、
  `ef₁=k₁=2` で `worst_ok` 不受理となり `discarded` へ入った B を、
  自己昇格なしに `candidates` へ戻すだけで `ef₂=k₂=3` を再開しても、B の
  隣接はすべて visited 済みで新規発見が無いため `results` は `{E, A}` の
  2 件のまま——`k₂=3` の期待件数を満たせず `masked_short` へ縮退しうる。
  自己昇格を先に行えば `worst_ok` 再評価で B が `results` へ直接復帰し
  3 件になる）。

  **`discarded` からの除去条件は自己昇格の成功のみであり、`candidates`
  への push とは独立**（codex-review 指摘・PR #589）。自己昇格の成否に
  関わらず、discarded から見つかった各ノードのうち `expanded` が未設定
  であり、かつ `in_candidates` が未設定（＝`candidates` ヒープに在庫が
  無い）のものは `candidates` へ push し `in_candidates` を立てるが、
  **この push は `discarded` からの除去を伴わない**——自己昇格に**失敗**
  したノードは `expanded`／`in_candidates` の状態と無関係に `discarded`
  に残り続け、次ラウンド以降も `worst_ok` の再評価対象になり続ける（1
  ノードが `discarded`（自己昇格の再評価対象）と `candidates`＋
  `in_candidates`（展開キューの在庫）の両方に同時に属する状態が正常に
  存在する）。旧版はこの push を「discarded からの除去」と誤って結び
  付けており（自己昇格の成否によらず除去）、次の反例で結果件数の下限を
  満たせないことが判明した（codex-review・PR #589 指摘）: 起点 E
  （score=10）から葉 7 個 L1〜L7（score=9〜3。すべて E のみに接続する
  星型グラフ）へ接続し、`ef` を `k` として 2→4→8 と拡張する。ラウンド 1
  （`ef=2`）で E・L1 が受理され L2〜L7 は `discarded` へ入る。ラウンド 2
  （`ef=4`）で自己昇格を再評価すると L2・L3 が新たに受理されるが、
  L4〜L7 は依然 `worst_ok` 不受理のまま——旧版はここで L4〜L7 を
  （push 対象であることを理由に）`discarded` から除去してしまうため、
  ラウンド 3（`ef=8`）の開始時点で `discarded` が空になり、L4〜L7 は
  二度と自己昇格の再評価を受けられない。`candidates` に積まれた L4〜L7
  は葉ノードで隣接が E のみ（visited 済み）のため pop されても新規発見
  が無く、`results` は 4 件のまま `k₃=8` を満たせず `masked_short` へ
  不要に縮退する。修正版（除去を自己昇格成功時のみに限定）では L4〜L7
  は `discarded` に残ったままラウンド 3 を迎え、`ef₃=8` の下での
  `worst_ok` 再評価で全員が受理され 8 件そろう。

  **自己昇格したノードもこの push 対象に含める**——自己昇格は「そのノード
  を `results`／`admitted` へ復帰させる」操作であり、`search_layer_in` の
  while ループの構造上、「そのノード自身の隣接を展開する」操作を代替
  しない（`candidates.pop()` した候補自身は `results` へ追加されないのと
  対称に、`results`／`admitted` へ直接挿入された候補も自動では展開され
  ない）。自己昇格の成否に関わらず未展開ノードを候補へ戻さなければ、
  その隣接ノードは exhaustive 終了まで発見されないままになりうる（上記の
  例を延長: B の隣接に C（score=4）が新たに接続されているとする。
  自己昇格のみで `candidates` へ戻さない場合、B は `results` へ復帰し
  `{E, A, B}` の 3 件で `k₂=3` の期待件数自体は満たすが、C は永久に
  未発見のまま——候補・破棄ヒープが尽きて `exhaustive` 終了と誤判定
  されうる。B の隣接を展開して初めて C が発見され、C のスコア次第では
  真の Top-3 が入れ替わる。exhaustive 終了時に「マスク受理ノードの到達
  可能成分を全訪問した」という下記「決定性契約」の前提を満たすには、
  自己昇格の成否と無関係に未展開ノードを候補へ戻す必要がある）。
  `expanded` 済みのノードは自己昇格のみを行い `candidates` へは戻さない
  ——隣接ノードは既に走査済み（`visited` 済み）のため再度 pop して隣接
  走査しても新規の候補は一切生まれず、二重の展開コストにしかならない
  （自己昇格に失敗した場合は上記のとおり `discarded` に残り続ける）。
  続いて `search_layer_in` の while ループを**同じ停止条件・同じ受理
  判定（`worst_ok`）**のまま続行する（上位層の貪欲降下〔`ef=1`〕は
  再実行しない——層 0 の状態のみを再開する）。`candidates.pop()` 時にも
  防御的に `expanded` を再検査し、既に設定済みであれば隣接走査せず
  読み捨てる——`in_candidates` の更新漏れ等で万一重複エントリが紛れ
  込んでも二重展開にならないようにする多重の安全策であり、これにより
  **ノード 1 個あたりの実質的な隣接走査は同一クエリの全ラウンドを通じて
  高々 1 回**になり、下記「停止性契約」の「全ラウンド合計の展開数は
  高々 N」が成立する（`discarded` からの除去を自己昇格成功時のみに
  限定しても、`discarded` の要素数は依然として高々 N であり——除去
  されないノードも `expanded` が立てば再度 `candidates` へ push
  されない——下記「メモリ上限」の見積りは変わらない）
- **停止条件は `peek` で判定し、打ち切り候補を消失させない**: 現行の
  `search_layer_in` は `candidates.pop()` で候補を取り出してから
  `results.peek()` に対する停止条件（`worst_ok` の否定）を判定するため、
  停止が発火した時点で pop 済みの候補（`top_candidate`）は
  `candidates`・`discarded` のいずれにも属さないまま関数を抜ける。
  非再開の 1 回実行ではこの候補は元々 `results` へ追加されない候補
  なので影響しないが、再開型ではラウンド r+1 で `ef` が増え
  `top_candidate` 自身が受理対象になり得るにもかかわらず両ヒープから
  消失しているため二度と発見できない（例: 起点 E〔score=10〕から
  A〔9〕・B〔8〕・C〔7〕へ接続し、C の隣接に F〔20〕があるとする。
  `ef=2` で A・B を自己昇格・展開した後 C を `pop` して停止条件が成立し
  `break` すると、C は `candidates`・`discarded` のどちらにも残らない。
  `ef` を 3 へ増やす次ラウンドは `discarded` の再評価から始まるため C を
  再訪できず、F は永久に未発見のまま——上記「決定性契約」が要求する
  exhaustive 時の到達可能成分全訪問と矛盾し、`results` が真の Top-3 を
  欠いたまま返る）。この消失を防ぐため、再開型 `run(ef)` の停止条件判定
  は `candidates.pop()` ではなく **`candidates.peek()`** で行う——停止が
  成立する場合は候補を pop せず、そのまま `candidates`（および
  `in_candidates`）に残し次ラウンドの再開に委ねる。停止が成立しない
  場合のみ pop して展開する。この変更は非再開の 1 回実行の返却値
  （`results`）を変えない——`peek` で停止が成立するケースは元実装でも
  その候補を `results` へ追加しないまま `break` していたため、pop の
  タイミングを条件判定の前後どちらに置いても最終的な `results` の内容は
  同一であり、下記「実装方針」節の「ラウンド 1 の bit 同一性」契約は
  保たれる。相違が生じるのは関数終了後の `candidates` ヒープの残存状態
  のみであり、これは再開型のみが参照する。**peek で停止し `candidates`
  に残ったノードの自己昇格経路**（Cursor Bugbot 指摘・PR #589）: この
  ノードは「上記『再開手順』の訂正——`discarded` からの除去は自己昇格
  成功時のみ」により、`candidates`（展開キューの在庫）に残ることと
  `discarded`（自己昇格の再評価対象）に残ることが独立に両立する。
  したがって次ラウンド以降、このノードが `candidates` から pop されて
  展開されるかどうかに関わらず、ラウンド先頭の自己昇格ステップが
  `discarded` を経由して毎ラウンド `worst_ok` を再評価し続けるため、
  `results`／`admitted` への復帰経路が `candidates` の pop タイミングに
  依存して失われることはない
- **メモリ上限**: visited・`expanded`・`in_candidates` はいずれも
  `⌈N/64⌉` 語（N は索引ノード数）のビットマップ、`candidates`／
  `discarded` の要素数はいずれも高々 N、`admitted` も高々 N。
  `k`・`ef` は構築済み `HnswIndex::search_masked` の検証（`MAX_EF` =
  10,000 以下）を経由済みの値のみを受け取る。無制限確保はしない
  （`coding-rust.md`「untrusted 入力の扱い」）
- **並行性**: `SearchProvider` trait は `search(&self, ...)` を要求し
  `Send + Sync` が前提のため、状態は `Mutex<Option<ResumeState>>`
  （`RefCell` は `Sync` でないため不可）で保持する。lock poisoning は
  fail-closed に「状態を捨てて `inner`（brute-force）へ委譲」する側へ倒す
  ——poison から回復して不整合な状態を使い続けない
- **無効化条件**（該当ラウンドは状態を破棄し既存の縮退経路へ倒す）:
  - ptr-eq 受理条件が外れた（`input.vectors`／`input.ids` が別バッファ。
    `search_delegates_to_inner_for_a_different_buffer` の既存契約と同型）
  - `k_r > MAX_EF`（既存の `ef_cap_fallbacks` 経路）
  - 解決形状が `PreparedHnswSearch::FullScan`（plain scan。索引探索を
    経由しない）
  - 索引探索エラー（`HnswError` 系）
- **`FullScan`／`ef_cap_fallbacks` ラウンドの状態**: 不要（厳密
  brute-force のため再開する探索状態自体が存在しない）。任意拡張として
  「1 回の全件スコアリング結果を保持し以後は prefix を伸ばして提供する」
  （疎側 #392 の `SparseScored::top` と同型で厳密かつ決定的）を候補として
  記すが、**本 Issue では採否を決めない**（#505 のスコープ外）

### 決定性契約: 「不変」の正確な等価クラス

Issue の見出し「再開型にしても融合結果・境界同点グループ・`fetch_k`
スケジュールが不変」を「現行の再実行型との bit 一致」と読むと、pgvector 型の
破棄候補ヒープ再開では**一般には成立しない**。根拠は `search_layer_in` の
受理判定 `worst_ok = results.len() < ef || score >= worst` が `ef` に
依存することにある。`ef₁` で不受理になったノードは現行実装では visited
マークのみ付けて捨てられる。`ef₂ > ef₁` の新規探索であれば同じノードが
**受理され、さらにその隣接ノードが新たに展開される**。再開型は破棄ヒープ
からそのノードを後で拾い直せるが、拾い直した時点での展開順序・visited
集合の状態・`results.pop()` の追い出し対象が、`ef₂` からの新規探索と
一致する保証はない。

このため本節は次の等価クラスを契約として定める（弱体化ではなく、成立範囲を
正確に記述するもの）:

1. **provider 非依存で不変**（無条件）: `hybrid.rs` の `fetch_k` 生成規則・
   `validate_extended_pool`・可視 id 検証（`core::provider_result_is_valid`）・
   `resolve_boundary_tie_group`／`complete_boundary_tie_group_by`・
   `rrf_fuse_with_limits`（`TieRank::GroupEnd`）・出力順（score desc・id
   asc の安定ソート。`docs/design/rrf-tie-break-determinism.md`）。
   `SearchProvider` trait のシグネチャは無変更のまま
2. **再実行型と bit 一致するラウンド**: (a) ラウンド 1（同一クエリの
   `search_layer_in` 1 回実行。破棄候補を捨てずに保持するだけでは
   `results` の中身は変わらない）。(b) exhaustive 終了時（候補ヒープ ∪
   破棄ヒープが空——マスク受理ノードの到達可能成分を全訪問した状態。
   `mask_splits_graph == false` により到達可能集合 ＝ 受理ノード全体
   （上記「DISTANCE 経路の `masked_short` 到達不能性」節の証明と同じ前提）
   のため、この状態は厳密 Top-k と一致する）
3. **一致を保証しないラウンド**: 非 exhaustive な ラウンド ≥ 2。上記の
   `worst_ok` の `ef` 依存が根拠。結果は「ANN 候補順序に対する近似」であり、
   これは上記「密 ANN 側の前方一致非保証」節（`ef` 拡大で prefix が
   入れ替わりうる）と同じ位置づけの近似であって、新たに緩める契約ではない
4. **帰結**: 同一クエリでも再開型と再実行型で**実現するラウンド数**が
   異なりうる（各ラウンドの返却列が異なれば、そのラウンドでの
   `TieBoundary` 判定——`Resolved`／`Undetermined`——も異なりうるため）。
   「`fetch_k` スケジュールが不変」とは `fetch_k` の**生成規則**
   （`dense_cap`・倍増式）が不変であることを意味し、実現ラウンド数の一致を
   主張するものではない
5. **同一索引・同一クエリ・同一世代での再現性**（`docs/design/
   hnsw-search.md`「決定性の保証範囲」）は再開型でも維持する: 全ヒープが
   `ScoredNode::Ord`（score desc・id asc）の全順序を持つこと、visited・
   ヒープの初期化がクエリ開始時点で決定的であること、クエリ内は単一
   スレッドで実行されること（スレッド非依存）
6. **返却列の契約**（再開型でも不変）: 毎ラウンド、全ヒットに対して
   スロット写像・`(tenant_id, id)` 照合・`kernel::dot` 再計算を行う
   （差分更新はしない。O(k·dim) のコストより fail-closed な単純さを
   優先する既存方針を維持）→ delta brute-force マージ → `sort_by`・
   `dedup`・`truncate(k)`。`validate_extended_pool` を通過する
7. **代替案（bit 一致を構成的に保証する案）**: 距離メモ再実行型——ノード→
   スコアのメモをラウンド間で保持し、走査自体（展開・受理判定）は毎回
   やり直す。展開を再開しないため性能面の主張（合計展開数 ≤ N）は成立
   しないが、dot 積計算の重複だけは削減でき、結果は現行実装と bit 同一に
   なる。#506 の実測で Recall 劣化や決定性上の懸念が出た場合の
   フォールバックとして本節に記録するが、**採否はオーナー判断**とする

### 停止性契約

- ラウンド数上限は既存の `dense_cap`・`MAX_FETCH_K`（= 40,000）による
  provider 非依存の有界性（「停止性・決定性」節のとおり
  `⌈log2(dense_cap / min(2·pool_depth, dense_cap))⌉ + 1`。
  `dense_cap < 2·pool_depth`（初期取得が既に `dense_cap` にクランプされる
  小規模・強選択性フィルタの場合）は 1 ラウンドに縮退し、
  `pool_depth = 200`・`dense_cap >= 2·pool_depth` の小〜中規模では 8 以下）
  を再開型でも変更しない
- ラウンド内の停止性: 各ヒープ pop は「停止条件成立」か「未訪問ノードの
  展開」のいずれかであり、visited は単調増加かつ N で有界なため各
  `run(ef)` は高々 N 回の展開で必ず停止する。**全ラウンド合計の実質的な
  隣接走査回数は高々 N**（再実行型は各ラウンドが visited をリセットする
  ため Σ_r visited_r になり得る）——これが再開型の性能面の狙いであり、
  #506 で実測する対象。この上限は上記「状態保持契約」の `expanded`・
  `in_candidates` ビットマップによる重複排除が前提であり、それなしでは
  成立しない: `discarded`（`results.pop()` で追い出されたノード）は
  `expanded` の有無を問わず存在しうるため、これを区別せず無条件に
  `candidates` へ再合流させると、既に `candidates.pop()` を経て展開済み
  のノードが複数ラウンドで繰り返し pop・隣接走査され得るため上限が崩れる
  （`expanded` フラグにより、そのようなノードは `candidates` へ再合流
  させない。一方 `expanded` 未設定のノードは、自己昇格〔`worst_ok`
  再評価 → `results`／`admitted` への O(1) 直接挿入〕の成否に**関わらず**
  `candidates` へ合流させる——自己昇格は「復帰」であって「展開」の代替
  ではないため、これを怠ると自己昇格したノードの隣接が永久に未発見のまま
  残り、上記「決定性契約」が前提とする exhaustive 終了時の到達可能成分
  全訪問が成立しなくなる。展開一覧の重複排除〔`expanded`〕と、未展開
  ノードの `results` 復帰〔自己昇格〕は独立した 2 つの操作であり、
  どちらか一方だけでは正しくない）。同一ノードが `candidates` ヒープへ
  複数回 push されて重複エントリになることは `in_candidates` ビットマップ
  （push 済みかどうかの在庫判定）で防ぎ、`candidates.pop()` 時の
  `expanded` 再検査（既に展開済みなら読み捨てる）を多重の安全策として
  併用する
- `exhaustive` 推論の健全性: `hybrid.rs` は `hits.len() < dense_fetch_k`
  から `exhaustive` を推論する。再開型でも「候補 ∪ 破棄ヒープが空のとき
  にのみ `k` 未満を返す」（fail-closed。空でないのに `k` 未満を返しては
  ならない）契約を維持する。既存の `masked_short` ガード
  （`index_hits.len() < min(k, visible_in_index)` で plain scan へ縮退）も
  残置し、切り詰め前の層 0 探索結果 `results` に対する不等式
  `results.len() >= min(ef_eff, visible_in_index)`（上記「DISTANCE 経路の
  `masked_short` 到達不能性」節の証明）と、それを `take(k)` で切り詰めた
  `index_hits.len() = min(results.len(), k) >= min(k, visible_in_index)`
  が再開型でも成立することを #505 で示す（`mask_splits_graph == false` に
  より受理ノード全体が到達可能であるため、この不等式は既存証明の系として
  成立する。`index_hits` 自体への下限は `ef_eff` ではなく `k` に対する
  ものであることに注意——`ef_eff` に対する下限は切り詰め前の `results`
  についてのみ成立する）
- `k_r > MAX_EF`: 既存契約どおり状態を破棄し `ef_cap_fallbacks` を計上して
  厳密 brute-force へ縮退する（変更しない）

### 実装方針（#505 向け・列挙のみ。本 Issue では実装しない）

- `hnsw.rs::search_layer_in` を「状態構造体 `LayerScanState { candidates,
  discarded, admitted, visited, expanded, in_candidates }` に対する
  `run(ef)`」へ再構成し、ラウンド開始時に `discarded` 全ノードへ
  `worst_ok` 再評価 → 満たすものは `results`／`admitted` への自己昇格を
  行い、**このときに限り** `discarded` から除去する。**自己昇格の成否と
  `discarded` からの除去を混同しない**（codex-review 指摘・PR #589。旧版は
  push 対象になったことを理由に自己昇格の成否によらず除去しており、失敗
  ノードが以後の自己昇格再評価から永久に外れ、結果件数の下限を満たせなく
  なる反例〔星型グラフ・上記「再開手順」節〕があった）。自己昇格の成否
  によらず、見つかった各ノードのうち `expanded` 未設定かつ `in_candidates`
  未設定のものを `candidates` へ push し `in_candidates` を立てる——この
  push は `discarded` の状態を変えない（自己昇格に失敗したノードは
  `discarded` に残ったまま `candidates` にも同時に存在し得る）
  （**自己昇格したノードも push 対象に含める**——上記「状態保持契約」
  参照。自己昇格を欠くと `candidates` の pop 処理がノード自身を
  `results` へ追加しないため discarded ノードが恒久的に失われ、逆に
  自己昇格したノードを `candidates` へ戻さないとそのノード自身の隣接が
  展開されないまま残る——どちらか一方だけでは不十分）。while ループの
  停止条件判定は `candidates.peek()` で行い
  （上記「状態保持契約」の「停止条件は `peek` で判定し」参照）、停止が
  成立する場合は候補を pop せず `candidates`／`in_candidates` に残したまま
  ラウンドを終える。停止が成立しない場合のみ `candidates.pop()` して
  展開し、pop 時には `in_candidates` を下ろし `expanded` を再検査して
  既に設定済みなら読み捨てる（重複 push が万一紛れ込んでも二重展開に
  ならない安全策）。既存の
  `search_layer_in`（現行の公開シグネチャ・呼び出し元）はその 1 回実行の
  薄いラッパとして維持する。アルゴリズム本体を複製しない（#494 の
  `Adjacency` ジェネリック・#490 の `PrefetchPolicy` をそのまま利用する）。
  **ラウンド 1 の bit 同一性**（再開型 `run(ef₁)` と現行
  `search_layer_in(ef₁)` の完全一致。`search_masked_none_matches_search`・
  `tests/hnsw_search.rs`・`tests/hnsw_cache.rs` の全件無変更 green）が
  受け入れゲートとなる
- `discarded` の保持は既存の `search_masked`（通常呼び出し）では行わない
  ——コスト・挙動を変えないため。再開型 API（例:
  `HnswIndex::search_masked_resumable`）を別途 `pub(crate)` として追加し、
  既存の `search`／`search_masked` の公開 API・エラー契約は変更しない
- `sql/hnsw_cache.rs::search_with_overlay` の解決済み経路に「再開ハンドル
  付き探索」を追加する（`prepare_*`／`search_prepared` が既に分離済みで
  ある構成を活かす）。統計に `hybrid_resumed_rounds`（再開により完走した
  ラウンド数）等の追加候補を挙げるが、テナント ID・行 ID・スコアは
  含めない（既存の統計方針を維持）
- `sql/hnsw_hybrid.rs::HnswDenseProvider` が `Mutex<Option<ResumeState>>`
  を保持し、上記「状態保持契約」の無効化条件を判定する
- 新規 `unsafe` は追加しない。新規依存も追加しない（`Mutex` は std）

### #505／#506 向け検証計画（列挙のみ）

- 単体（`hnsw.rs`）: ラウンド 1 の bit 同一（再開型 `run(ef₁)` vs 現行
  `search_layer_in(ef₁)`）、exhaustive 終了時の厳密性（brute-force 対照と
  完全一致）、同一入力に対する再現性、`discarded` を合流しない実装との
  差分が非 vacuous であること（合流しないと結果が変わる入力が存在する
  ことの確認）、**`discarded` からの除去を自己昇格成功時のみに限定する
  契約の回帰テスト**（上記「再開手順」節の星型グラフ反例に対応する
  固定フィクスチャで `ef` を 2→4→8 と複数ラウンド拡張し、各ラウンド後の
  `results.len()` が `min(k_r, visible)` を満たすこと・最終ラウンドで
  `masked_short` へ誤縮退しないことを固定。codex-review 指摘・PR #589）
- 結合（`tests/hnsw_hybrid_refetch.rs`・`tests/hnsw_cache.rs`）: 既存の
  停止性（`hybrid_rounds_max <= 8`）・複数ラウンドの実発生
  （`hybrid_rounds_max >= 2`）・3 回実行の bit 一致・既定エンジン対照
  Recall@10 ≥ 0.9・可視外テナント非混入・`hybrid_dense_searches > 0` を
  **無変更のまま green** で維持する
- 「再実行型との一致」の定義: 単一ラウンドで完了するクエリ
  （`hybrid_rounds_max == 1` を assert）と exhaustive 完了クエリについては
  融合結果の bit 一致を検証する。複数ラウンドかつ非 exhaustive なクエリは
  bit 一致ではなく契約プロパティ（ソート順・一意性・`len <= k`・可視集合
  内であること）と既存の Recall 基準で検証する
- #506（本 Issue のスコープ外）: `make bench-hybrid`（同点誘発コーパスの
  A/B。Issue #324 の方式）を交互 min-of-N（N ≥ 5）・ノイズ帯併記で前後
  比較し、`RECALL_ENGINE=hnsw` の 3 Recall ゲート（hybrid・rerank・
  query-planning）が同一閾値で通ることを確認する。共有 QEMU 環境の数値は
  採否根拠にしない（`docs/design/benchmark-judgement-policy.md` §5〜6）
- ガード: `make core-api-check`（`SearchProvider`／`VectorCore` の trait
  差分ゼロ）・`make sort-determinism-check`（`sort_by` のみの使用）

### 外部実装の参照（手法名・ライセンスのみ）

pgvector（PostgreSQL License）の `hnsw.iterative_scan` は上流 README で
確認できる設定として `strict_order`（結果を厳密に距離順に保つ）・
`relaxed_order`（順序をわずかに緩めて Recall を優先する）の 2 モードを持ち、
`hnsw.max_scan_tuples`（既定 20,000。訪問タプル数の近似上限。初回スキャンには
影響しない）・`hnsw.scan_mem_multiplier`（既定 1。`work_mem` に対する倍数
としてのメモリ上限）で打ち切り条件を持つ。本リポの hybrid 密側再取得
ループは各ラウンドで `hits` を丸ごと置き換える設計であり、`strict_order`
型（後続で見つかった近い候補のために既に確定した順序を保つ機構）に相当する
仕組みは不要である——`hybrid.rs` はラウンドの結果をマージ元として保持する
だけで、途中経過の順序保証を提供する契約を持たないため。

### セキュリティ考慮（OWASP Top 10 観点）

| 観点 | 対応 |
| ---- | ---- |
| アクセス制御の不備／テナント境界（P0） | 索引は `(table, ctx)` 可視アリーナのみから構築する契約は不変。再開状態はクエリ単位で同一 `prepared`（同一 base・overlay・マスク）に束縛され、ptr-eq 受理条件が外れたラウンドは状態を破棄し `inner` へ委譲する（fail-closed）。`NodeMask` による非受理ノード非通過（#409 の P0 条件）は再開時も同一の受理判定を経由する。`hybrid.rs` の可視 id 検証・`RlsSafetyNet` の多層防御は無変更 |
| 存在情報の副次漏えい | 追加統計案（`hybrid_resumed_rounds` 等）にテナント ID・行 ID・スコアを含めない。`EXPLAIN` へラウンド数・再開有無を露出しない（#411 の既存方針を維持） |
| 不安全な設計（DoS） | ラウンド数は `dense_cap`・`MAX_FETCH_K` で有界のまま。再開状態のサイズは索引ノード数 N で有界。合計展開数は再実行型以下（≤ N）になる設計であり増加方向の変更ではない |
| インジェクション | SQL 文字列の組み立てを伴わない（docs 専任） |
| untrusted 入力 | `k`／`fetch_k` の検証順序（`MAX_EF` 検証 → `effective_ef`）を変更しない。`unwrap`／`expect`／添字アクセスを production コードに持ち込まない方針を #505 の実装制約として明記する |
| 脆弱な依存 | 依存追加なし（`Mutex` は標準ライブラリ） |
| private spec 漏えい（P0） | 本節・関連コミット・PR は TASK-nn／ビヘイビア ID のポインタ表記のみ。Issue 本文の逐語引用を行わない |
