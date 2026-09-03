# 転置索引化後の Recall 非劣化・cold/hot 等価・決定性の回帰検証（Issue #393）

- ステータス: Implemented
- 対応 Issue: #393（親 Issue #386 Phase 1）
- 前提: #388（term インターニング）・#389（posting list）・#390（可視ビットマップ +
  posting 走査 1 パス `search_within`）・#391（文書長クラス表 + `select_nth_unstable_by`
  型 Top-k）・#392（疎側再取得ループの再スコアリング回避）
- 関連ビヘイビア（ポインタのみ・本文非転記）: SEARCH-1・SEARCH-2・SEARCH-3・SEARCH-7・
  PLAN-1〜3
- production コード（`crates/engine/src/`・`crates/wire-server/src/`）は無変更・
  テスト専任

## 背景・目的

Phase 1（#386）で `SparseIndex`（`crates/engine/src/sparse.rs`）を転置索引方式へ
再構成した。各タスクは単体テストでスコアのビット一致を固定したが、Issue #358
（`docs/design/sparse-index-cache-verification.md`）と同型の SQL 表層経由
cold/hot 等価性検証は、(a) RLS 部分可視・(b) 未知語のみクエリ・(c) 空クエリの
3 ケースが未整備だった。また `docs/design/rrf-tie-break-determinism.md` が固定した
同点タイブレーク契約（安定ソート・id 昇順・`total_cmp`）が、#391・#392 で導入した
不安定ソート API（`Candidate` の全順序を根拠にした `// sort-determinism: allow`
マーカー付き）の下でも維持されることの固定テストも未整備だった。本 Issue はこの
2 つの空白をテスト専任で埋め、あわせて 3 Recall ゲート（hybrid・rerank・
query-planning）の層 A 固定値アサーションが転置索引化後も不変であること・層 B
実測値が導入前（`8bfaaa4`）と一致することを記録する。

## 検証設計

### 1. cold/hot 等価性の追加ケース（SQL 表層経由）

`crates/engine/tests/sparse_cache_recall.rs` へ既存 2 ケース（小規模・同点境界）に
加えて以下を追加した（既存ケース・固定値 `hot_hits20 == 46` は無変更）。

| テスト | 検証内容 |
| ------ | -------- |
| `cold_and_hot_hybrid_results_match_under_partial_visibility_and_never_leak_invisible_rows` | RLS 部分可視。4 群（tenant-a Public/Private・tenant-b Public/Private、b-Private はほぼ全語彙対を高密度にカバー）を投入し、2 文脈（Public のみ／自テナント Private も可視）で cold/hot 完全一致・結果が可視集合の部分集合であること・文脈ごとに別キャッシュエントリ（`misses == 2`）になることを検証。さらに **統計縮約オラクル**（不可視行を一切含まない対照 DB で同一クエリを実行し、結果 id 列が完全一致すること）で df・N・avgdl の不可視行からの漏えいが無いことを固定 |
| `cold_and_hot_hybrid_results_match_for_unknown_terms_only_query` | コーパス語彙に存在しない ASCII トークンのみのクエリ文字列。cold/hot 完全一致・LIMIT 件数を密のみで満たすこと・純密クエリ（`ORDER BY embedding <=> ...`）の Top-20 と完全一致することを検証（疎チャネル無信号時の RRF 単一チャネル縮退が密順位の単調写像になる構造的性質） |
| `cold_and_hot_hybrid_results_match_for_empty_query_text` | `hybrid_rrf(embedding, '<vec>', body, '')`。実測した契約は `Ok`（密のみ縮退）。テストは `Ok` を必須契約として `expect` で固定し（`Err` は即座に失敗）、cold/hot 完全一致を検証 |

いずれも `sparse_index_cache_stats()` の `hits`/`misses` を同時にアサートし、
vacuous pass（結果が偶然一致するだけで実際にはキャッシュ非経由）を防ぐ既存方針
（Issue #281・#358 と同方針）を踏襲する。

### 2. 疎側決定性契約の固定（`crates/engine/tests/sparse_determinism.rs`、新設）

`docs/design/rrf-tie-break-determinism.md` の不変条件を、`SparseIndex`/
`hybrid_search` の公開 API のみを使って固定する（BM25 式自体の独立再実装はしない。
`tests/hybrid.rs` の既存方針と同じ）。

| テスト | 検証内容 |
| ------ | -------- |
| `search_within_and_score_within_top_are_bit_identical_across_k_and_repetitions` | 大きな同点グループ（300 件。64 の倍数境界を跨ぐ）を含む決定的コーパスで、複数の境界 `k`（同点グループの内側・末尾・+1・M・M+1）× 20 回反復にわたり `search_within` と `score_within().top(k)` がスコアのビット列含め完全一致すること |
| `tie_group_cut_by_k_yields_smallest_doc_ids_and_prefix_consistency` | 同点グループを `k` で切ると id 昇順の先頭 `k` 件のみが残ること・`top(k1)` が `top(k2)`（k1 ≤ k2）の前方一致になること・`top(k)` の反復呼び出しが冪等であること |
| `results_are_independent_of_build_order_and_visible_set_representation` | 投入順（整列順・決定的シャッフル・逆順）を変えても `search_within` の (doc_id, score) 列がビット一致すること。可視集合を部分（同点グループ中央域を不可視化）にした場合も、不可視域を飛び越えて id 昇順が保たれること |
| `hybrid_sparse_tie_group_across_pool_boundary_is_deterministic_over_multiple_refetch_rounds` | `pool_depth=8` で疎側再取得ループが複数ラウンド（16→32→64→…）発火する構成（全 300 件が同一融合スコアの同点グループ）で、`CpuScalarProvider`/`ParallelSearchProvider` 双方・20 回反復にわたり `hybrid_search` の結果がビット一致し、`TieRank::GroupEnd`（既定）により境界同点グループが分断されず id 昇順の先頭 k 件で安定すること |
| `hybrid_sparse_tie_group_boundary_completion_is_load_bearing_against_competing_dense_group` | 疎側のみで一致する巨大同点グループ（20 件・`pool_depth*2` を跨ぐ）と、密側のみで一致する競合グループ（14 件）を競合させる構成で、境界同点グループ完全化（`complete_boundary_tie_group`）が正しく機能すれば疎側グループの一律順位が競合グループに劣るため密側競合グループが top-k を独占することを、`CpuScalarProvider`/`ParallelSearchProvider` 双方で検証する。境界完全化を経由せず疎側が早期打ち切りされる退行が起きると、疎側同点グループが競合グループを押しのけて top-k を独占してしまい本テストが red になる（境界完全化が vacuous でなく load-bearing であることの証拠） |

`docs/design/rrf-tie-break-determinism.md` へも本検証へのポインタと、#391・#392 が
導入した `// sort-determinism: allow` マーカー付き不安定ソート API を「例外として
許容する箇所」へ追記した（`scripts/check_sort_determinism.sh` は無変更のまま
green）。

### 3. 3 Recall ゲートの層 A/層 B 前後比較

**層 A（`cargo test`。固定値アサーション）**: 転置索引化を含む現行 `origin/main`
（`89085aa`）+ 本 Issue のブランチで以下すべて green（固定値は無変更）。

```text
cargo test -p engine --test hybrid_recall --test rerank_recall \
  --test query_planning_recall --test precision_eval --test incremental_recall
```

hybrid_recall 10 passed・rerank_recall 13 passed・query_planning_recall 10 passed・
precision_eval 11 passed・incremental_recall 6 passed（いずれも 0 failed）。

**層 B（`--release --ignored`。`RECALL_VERBOSE=1` + `(0.0,1.0]` 内のプレースホルダ
閾値注入で早期 return を回避し実測値を取得）**: 導入前基線 `8bfaaa4`（#387 まで。
production コードは #388 直前と同一）と、導入後 `89085aa` + 本ブランチの両方を
同一プレースホルダ閾値で計測した。プレースホルダは spec 閾値ではないため
pass/fail 自体は記録の対象外とし、実測値の一致のみを比較する。

| ゲート | 指標 | before（`8bfaaa4`） | after（`89085aa`+本ブランチ） | 差分 |
| ------ | ---- | -------------------- | ------------------------------ | ---- |
| hybrid | 小規模 Recall@20 | 0.9010 | 0.9010 | 0 |
| hybrid | 大規模 Recall@20 | 0.9145 | 0.9145 | 0 |
| hybrid | 大規模 Recall@100 | 0.9165 | 0.9165 | 0 |
| rerank | after_recall@20 | 0.9488 | 0.9488 | 0 |
| rerank | non_degraded（after_hits20 ≥ baseline_hits20） | true | true | — |
| rerank | baseline_hits20 / after_hits20 / pool_ceiling_hits20 | 387 / 389 / 396 | 387 / 389 / 396 | 0 |
| rerank | improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0 |
| query-planning | intent_improvement | 0.9245 | 0.9245 | 0 |
| query-planning | direct_after_recall20 | 0.9321 | 0.9321 | 0 |
| query-planning | intent_improvement_degraded | 0.3547 | 0.3547 | 0 |
| query-planning | 大規模段 direct_after_recall20 | 0.8852 | 0.8852 | 0 |

**すべて差分 0**。これは CLAUDE.md に記録済みの既知の構造的事実——
`tests/hybrid_recall.rs`・`tests/rerank_recall.rs`・`tests/query_planning_recall.rs`
は `SparseIndex::build` + `hybrid::hybrid_search` を直接呼び出し、SQL 表層の
`SparseIndexCache`（Issue #357）を経由しないため、SQL 表層専用の変更に対して
実測が構造的に不変であること——と整合する結果であり、転置索引化（#388〜#392）が
これらのハーネスが呼ぶ `SparseIndex`/`hybrid_search` の公開 API の出力（スコア・
順序）をビット単位で変えていないことの追加証拠になる（層 A の固定値アサーション
自体がこの不変性の機械的証拠であり、層 B 実測値の完全一致はそれを数値として
裏付ける）。

再現手順（本リポの `make` ターゲット。before 側は該当コミットの worktree +
別 `CARGO_TARGET_DIR` で同一コマンドを実行）:

```sh
RECALL_VERBOSE=1 HYBRID_RECALL_MIN_R20_SMALL=0.001 HYBRID_RECALL_MIN_R20_LARGE=0.001 \
  HYBRID_RECALL_MIN_R100_LARGE=0.001 make recall-regression
RECALL_VERBOSE=1 RERANK_RECALL_MIN_R20_LARGE=0.001 make rerank-regression
RECALL_VERBOSE=1 QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT=0.001 \
  QUERY_PLANNING_RECALL_MIN_R20_DIRECT=0.001 \
  QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED=0.001 \
  QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE=0.001 make query-planning-regression
```

### 4. 大規模段 cold/hot 等価性（`make sparse-cache-recall-large`）

既存の `#[ignore]` 大規模段テスト（20,000 件規模）を after 側（本ブランチ）で
再実行し green を確認した（`cargo test -p engine --release --test sparse_cache_recall
-- --ignored`。所要 約 7.8 秒）。

## 受け入れ条件の充足状況

- 3 ゲートすべて green（層 A・層 B とも）: 満たす
- 実測値の doc への記録: 満たす（上表。閾値そのものは非公開のため記載しない）
- 追加ケースのテスト green: 満たす（`sparse_cache_recall.rs` 全 5 本・
  `sparse_determinism.rs` 全 5 本）
- production コード無変更: 満たす（`git diff --stat origin/main -- crates/engine/src
  crates/wire-server/src` が空であることを確認済み）

## スコープ外・申し送り

- `recall.yml`（Environment `recall-gate`）でのマージ後 `workflow_dispatch` 実測・
  実閾値との pass/fail 記録: オーナー／管理者作業
- WHERE フィルタ付き hybrid（`SparseIndexCache` 非経由）・密側再取得ループの同型
  RLS/決定性検証: 対象外（`tests/sparse_cache.rs::where_filtered_hybrid_query_does_not_use_sparse_cache`
  が非経由であることのみ既存で固定済み）
- `feature_bench`・`bench-hybrid-profile` 等の性能前後比較: Issue #394 の範囲
- 3 クライアント wire e2e: 対象外

## 参照

- SEARCH-1・SEARCH-2・SEARCH-3・SEARCH-7・PLAN-1〜3（ポインタ:
  `docs/spec/04-behavior/search.md`・`docs/spec/04-behavior/query-planning.md`）
- `docs/design/sparse-index-cache-verification.md`（Issue #358・同型の検証設計）
- `docs/design/rrf-tie-break-determinism.md`（TASK-84・同点タイブレーク契約 ADR）
- `docs/design/hybrid-rrf-latency-breakdown.md`（#388〜#392 の性能前後比較）
- `docs/design/ci-gate-variables.md`（3 ゲートの閾値 ↔ spec ポインタ対応表）
- `crates/engine/tests/sparse_cache_recall.rs`・`crates/engine/tests/sparse_determinism.rs`
