# ANN opt-in 時の Recall ゲート同一閾値検証と TASK-121 系増分回帰の拡張（Issue #412）

## 背景

ADR `docs/design/ann-index-adoption.md`（Issue #403 で Accepted）の B 案
（条件付き opt-in 採用・自作 HNSW）の受け入れ基準のうち、以下 2 点が本 Issue
（#412）の担当:

- SEARCH 系 Recall ゲート（`crates/engine/tests/hybrid_recall.rs`・
  `rerank_recall.rs`・`query_planning_recall.rs` の層 B）を ANN 有効経路でも
  同一閾値で通過すること
- TASK-121 系増分回帰（`tests/incremental_recall.rs`）の ANN 対応

前提: `SearchEngineKind::Hnsw` opt-in（#407）・世代整合キャッシュと未索引分
brute-force 併用（#408。`docs/design/hnsw-generation-cache.md`）・RLS 事前
フィルタ統合（#409。`docs/design/hnsw-rls-cardinality-switch.md`）・hybrid 密側
境界再取得（#410。`docs/design/hnsw-hybrid-iterative-scan.md`）・`EXPLAIN` 露出
（#411。`docs/design/explain-search-engine-exposure.md`）はいずれも実装済み。

## 測定経路の設計判断

3 つの Recall ハーネスは従来 `engine::hybrid::hybrid_search` を in-memory 配列に
対して直接呼んでおり、ANN 経路（`sql::hnsw_cache`／`sql::hnsw_hybrid::
HnswDenseProvider`）を構造的に通らない。`hnsw::provider::HnswSearchProvider::
search` は常に brute-force へ委譲する契約であり、`SearchProvider` の差し替え
だけでは ANN は発火しない。ANN の実 seam はいずれも `pub(crate)` のため、結合
テストから ANN 経路へ到達できる唯一の production API は **SQL 表層**
（`EngineCore::from_storage_with_engine(storage, hnsw_kind(..))` ＋
`execute_sql` の `ORDER BY HYBRID(...)`）である。

そのため、3 ハーネス共通の fixture
`crates/engine/tests/fixtures/recall_engine.rs` を新設した:

- `RecallEngine`: `RECALL_ENGINE` 環境変数（未設定・空文字列・`brute_force` は
  既定〔既存 in-memory 経路をそのまま通す〕、`hnsw` は ANN opt-in。それ以外は
  fail-closed で panic）
- `SqlHybridFixture`: `docs(embedding VECTOR(dim), body TEXT)` へ行を投入し、
  `hybrid_top(query_vec, query_text, k)` で `ORDER BY HYBRID(...)` の
  `(id, score)` 列を返す。`score` は `sql/exec.rs` の hybrid 分岐が書き込む
  `ResultRow::score`（RRF 融合スコア）で、in-memory 版の `hybrid_search` が
  返すスコアと同じ意味
- `AnnStats`: `EngineCore::hnsw_index_cache_stats()` のフィールド値の複製
  （`HnswIndexCacheStats` 自体は `pub(crate)` モジュール配下のため型名を
  結合テストから綴れない。値は機微情報を含まないため出力可）
- `assert_ann_non_vacuous(expect_indexed)`: `expect_indexed` が真なら
  `builds >= 1 && build_failures == 0 && hybrid_dense_searches > 0` を、偽なら
  `builds == 0` を固定する（構築失敗→負のキャッシュ→黙って brute-force で
  「ANN pass」を誤報告する経路、および `MIN_INDEXED_ROWS` 未満の段を誤って
  ANN 通過と数える経路の両方を防ぐ）

既定経路（`RecallEngine::BruteForce`）は各ハーネスの既存 in-memory 測定コード
を一切変更せず素通しする——層 A・層 B とも既存の実測値・固定値アサーションは
無変更のまま green（`cargo test -p engine --test hybrid_recall` 等で確認済み）。

## 測定妥当性ガード（`tests/recall_engine_fixture.rs`）

fixture 自体の層 A テストとして以下を固定した:

1. `RecallEngine::from_env` の受理／拒否契約（純関数 `parse`。厳密一致のみ
   受理・trim 許容）
2. **「SQL 表層 + 既定エンジン」の hybrid クエリ結果（id 順序）が「in-memory
   `hybrid_search`」の結果と完全一致すること**（`sql_default_engine_hybrid_
   top_matches_in_memory_hybrid_search`）。ANN 有効時の Recall 差分が SQL 表層
   自体の違いではなく検索エンジンの違いにのみ起因することの前提を担保する
3. 非 vacuous ガードの正負両方（`MIN_INDEXED_ROWS`〔1,024〕以上で
   `builds >= 1`、未満で `builds == 0`）

## 3 ゲートへの結線

`hybrid_recall.rs`・`rerank_recall.rs`・`query_planning_recall.rs` の層 B
（`#[ignore]`）に `RecallEngine::from_env()` による分岐を追加した
（brute-force 側は既存コードを一切変更しない分岐構造）。hnsw 側は
`SqlHybridFixture` 経由で候補プール／Top-k を取得し、既存の集計ロジック
（`RecallResult`／`RerankRecallResult`／`CategoryRecallResult`）へそのまま
渡す。ゲート行には `engine=<brute_force|hnsw>` トークンを追加し、hnsw 実行時
は `AnnStats` の非機密カウンタ（`builds`／`build_failures`／`rebuilds`／
`hybrid_dense_searches`／`hybrid_queries`／`ef_cap_fallbacks`）を出力する。

各コーパス規模と `MIN_INDEXED_ROWS`（1,024）との関係:

| ゲート | 小規模段 | 大規模段 |
| ---- | ---- | ---- |
| hybrid | 400 docs（**`MIN_INDEXED_ROWS` 未満。構造的に brute-force のまま**） | 20,000 docs |
| rerank | （小規模段ゲートなし） | 20,000 docs |
| query-planning | 4,000 docs | 40,000 docs |

hybrid 小規模段以外はすべて `MIN_INDEXED_ROWS` を上回り、`RecallEngine::Hnsw`
指定時に実際に HNSW 索引を構築する。hybrid 小規模段は `RecallEngine::Hnsw`
を指定しても `assert_ann_non_vacuous(false)` により `builds == 0` を固定し、
ANN 通過とは数えない（測定経路自体は SQL 表層を経由するため無意味ではないが、
索引構築の検証にはならない）。

`recall.yml` は `recall-regression` job に `strategy.matrix.recall_engine:
[brute_force, hnsw]` を追加し、3 gate step の `env:` へ `RECALL_ENGINE` として
`matrix.recall_engine` を渡す（`run:` へ直接展開せず `env:` 経由。式インジェク
ション回避の定石）。当初は `workflow_dispatch.inputs.recall_engine`（`type:
choice`。既定 `brute_force`）の単一選択式で、`schedule` トリガでは `inputs`
が存在せず式が空文字列に解決 → `RecallEngine::from_env` の契約で既定
`brute_force` のまま評価される実装だった。これは週次 `schedule` 実行で HNSW
経路が一度も測定されないことを意味し、ADR の受け入れ条件「Recall ゲートを
ANN 有効経路でも同一閾値で通過」を継続的には保証できていなかった
（codex-review P1 指摘・Issue #412 PR #438）。`matrix` 化により
`workflow_dispatch`・`schedule` いずれのトリガでも `brute_force`/`hnsw` の
2 系列が独立 job として毎回両方ゲートされる（`fail-fast: false` で片方の
fail がもう片方の評価を止めない）。`RECALL_ENGINE` 自体は非機密の opt-in
フラグ（`BENCH_CORE6`等と同じ扱い）であり secrets 化しない
（`docs/design/ci-gate-variables.md` 参照）。

## 実測結果（ローカル `--release`。閾値は private spec から環境変数へ注入し値は本 doc に転記しない。ここに記載する Recall 実測値・統計カウンタはオーナー判断〔2026-08-29〕により公開可）

`RECALL_VERBOSE=1` opt-in で `brute_force` と `hnsw` を交互に 1 回ずつ実行し、
測定値を比較した。

### hybrid（`hybrid_recall.rs`）

| 段 | 指標 | brute_force | hnsw | 差分 |
| ---- | ---- | ---- | ---- | ---- |
| 小規模（400 docs） | recall@20 | 0.9010 | 0.9010 | 0（`builds=0`。構造的に brute-force のまま） |
| 大規模（20,000 docs） | recall@20 | 0.9145 | 0.9145 | 0 |
| 大規模（20,000 docs） | recall@100 | 0.9165 | 0.9165 | 0 |

大規模段の hnsw 実測での統計カウンタ（1 run）: `builds=1 build_failures=0
rebuilds=0 hybrid_dense_searches=420 hybrid_queries=100 ef_cap_fallbacks=80`
（`ef_cap_fallbacks` は `hybrid.rs::hybrid_search_boosted` の密側再取得ループが
`fetch_k` を `MAX_EF` 超まで倍増したラウンドで縮退した回数。production 契約
どおり brute-force 縮退により空集合誤返却を防いでおり、Recall には影響
していない）。

### rerank（`rerank_recall.rs`。大規模段のみ）

| 指標 | brute_force | hnsw | 差分 |
| ---- | ---- | ---- | ---- |
| after_recall@20 | 0.9488 | 0.9488 | 0 |
| non_degraded（after_hits20 >= baseline_hits20） | true | true | — |
| improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0 |

baseline_hits20=387・after_hits20=389・pool_ceiling_hits20=396 はいずれも
brute_force・hnsw で完全一致。hnsw 実測の統計（1 run）: `builds=1
build_failures=0 rebuilds=0 hybrid_dense_searches=492 hybrid_queries=100
ef_cap_fallbacks=106`。

### query-planning（`query_planning_recall.rs`）

| 段 | 指標 | brute_force | hnsw | 差分 |
| ---- | ---- | ---- | ---- | ---- |
| 小規模（4,000 docs） | intent_improvement | 0.9245 | 0.9245 | 0 |
| 小規模（4,000 docs） | direct_after_recall20 | 0.9321 | 0.9321 | 0 |
| 小規模（4,000 docs） | intent_improvement_degraded（NoisyLlmClient） | 0.3547 | 0.3547 | 0 |
| 大規模（40,000 docs） | direct_after_recall20 | 0.8852 | 0.8852 | 0 |

小規模段 hnsw 実測の統計: direct `builds=1 hybrid_dense_searches=662
hybrid_queries=160 ef_cap_fallbacks=0`、intent `builds=1
hybrid_dense_searches=731 hybrid_queries=160 ef_cap_fallbacks=0`、
intent_degraded `builds=1 hybrid_dense_searches=758 hybrid_queries=160
ef_cap_fallbacks=0`（いずれも `build_failures=0 rebuilds=0`）。大規模段:
`builds=1 build_failures=0 rebuilds=0 hybrid_dense_searches=456
hybrid_queries=100 ef_cap_fallbacks=120`。

### 判断

**全 6 測定点（hybrid 大規模 2 指標・rerank 大規模 1 指標＋非劣化＋
improvement_ratio・query-planning 小規模 3 指標・大規模 1 指標）で
brute_force と hnsw の実測値が完全一致した。** 合成コーパス・本リポの
決定的フィクスチャ範囲では、ANN opt-in 経路は既定エンジンと同一の Recall
挙動を示す。閾値は既定エンジンで公開済みの値（例: hybrid 0.9010/0.9145/
0.9165）と同一のまま運用でき、ANN 有効化のために閾値を緩める必要はない。

未達・原因分析の記録は不要（全指標が非劣化どころか完全一致のため）。
production コード（`crates/engine/src/`）は本 Issue の範囲では無変更。

## TASK-121 系（`crates/engine/tests/incremental_index_hnsw.rs`）

`tests/incremental_recall.rs`（TASK-121 の性能回帰）とは別ファイルとして、
ANN opt-in 時の増分反映の状態遷移を検証する結合テストを追加した。
`documents(embedding VECTOR(64), path, body)` へ `HashingEmbedder`
（`lines_per_chunk=2`・本文ちょうど 2 行＝ 1 ファイル = 1 チャンク）で
1,100 ファイル（`[MIN_INDEXED_ROWS（1,024）, MIN_ROWS_PER_THREAD*2（2,048）)`
に収め、`HnswIndex::build_parallel` が逐次構築に留まり決定的であることを
保証する）を投入し、以下 3 状態を検証する:

1. **初回構築**: `execute_insert_sql_batch`（`BatchLimits::default()` の
   ファイル数上限で分割）で 1,100 ファイルを投入後、代表サンプル（約 30 件）
   の自己検索到達率（本文中の marker をクエリにしてその `path` 自身が
   `HYBRID(...)` の top-10 に現れる率）が ANN opt-in・既定エンジンの双方で
   0.9 以上であることを固定。`stats.builds == 1`。
2. **overlay**（1〜3 ファイルを同一パス置換。stale+delta 比 ≈ 0.27%）:
   置換後の新チャンク（新 marker）が `WHERE path = ..` の直接照会で確認
   でき、旧 marker が本文から消えていること（`body_for_path` によるランキング
   非依存の決定的検証）。`stats.builds == 1 && stats.rebuilds == 0`
   （再構築は起きない）かつ `delta_searches + plain_scans + fallbacks > 0`
   （overlay／縮退経路のいずれかが実際に発火したことの非 vacuous 検証。
   具体的にどのカウンタが増えるかは可視カーディナリティ・マスク連結性等の
   実装内部の判定順序に依存するため 3 カウンタの和で判定する）。未置換の
   既存チャンクの自己検索到達率は overlay 前後で非劣化。
3. **再構築**（約 12.5%＝137 ファイルをさらに置換。stale+delta 比が 1/10 を
   超える）: `stats.builds >= 2 && stats.rebuilds >= 1`。再構築後の新チャンク
   の自己検索到達率が 0.9 以上。overlay 済みチャンク（状態 2）は再構築後も
   引き続き到達可能。

あわせて `ann_replace_does_not_touch_other_tenants_same_path_rows` で、
ANN opt-in core でも tenant-a の同一パス置換が tenant-b の同一パス行を
変更しないこと（`tests/incremental_index.rs::
resend_does_not_touch_other_tenants_same_path_rows` と同方針）を固定した。

いずれも `cargo test -p engine --test incremental_index_hnsw` で green
（`--release` で約 0.5 秒）。production コード（`crates/engine/src/`）は
無変更・テスト専任。

## スコープ外・申し送り

- `precision` モード hybrid の ANN 化・`SearchTimeFilter` 経路・Rust API
  `hybrid` 相当 API の結線（#410 申し送りどおり継続）
- `full_scan_ratio`／`MAX_EF`／`ef_search` 既定値の再調整と前後比較（#413）
- `tests/rls_generalized.rs`／`tests/plan_rls_boost.rs` への HNSW variant
  追加（#409 申し送り）
- `recall.yml` の `hnsw` matrix job（`strategy.matrix.recall_engine: [brute_force, hnsw]`。PR #438 で `workflow_dispatch.inputs.recall_engine` から変更）の実 `workflow_dispatch`／`schedule` 疎通確認（マージ後の管理者作業）
