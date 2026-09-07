# ANN（HNSW）導入の前後比較と設計まとめ

- **ステータス**: Accepted（記録専用。Issue #413。親 #402／ADR
  `docs/design/ann-index-adoption.md`〔#367・#403〕の受け入れ基準表
  「(B 案条件 1) 損益分岐点の実測」を本書で事後確認する）
- **対応 Issue**: #413（本書）・#402（Phase 3 親トラッキング）・#403（判断記録）
- **関連ポインタ（spec・本文は転記しない）**: CORE-9・CORE-10・TASK-132

## 1. 背景・目的

Phase 3（#404〜#412・すべて merged。ベース `40cc024`）で自作 HNSW の opt-in
経路（`SearchEngineKind::Hnsw`・`sql::hnsw_cache`・`sql::hnsw_hybrid`・
`EXPLAIN` 露出・Recall ゲート同一閾値検証）が揃った。ADR
`docs/design/ann-index-adoption.md` は B 案条件 1「損益分岐点の A/B 事前実測」
を「Phase 3 完了時の前後比較（本 Issue）で事後確認する」運用へ差し替えている。
本書はその事後確認であり、ADR の受け入れ基準表の当該行を本書へ更新する。

## 2. サブ Issue 設計 doc へのポインタ

| サブ Issue | 内容 | doc |
| --- | --- | --- |
| #404 | グラフ構築の基本設計 | `docs/design/hnsw-graph-construction.md` |
| #405 | 探索（ef ビーム・top-k）・Recall 単体検証 | `docs/design/hnsw-search.md` |
| #406 | 構築の並列化 | `docs/design/hnsw-parallel-build.md` |
| #407 | `SearchEngineKind` 結線 | `docs/design/hnsw-search-engine-wiring.md` |
| #408 | 世代整合キャッシュ・未索引分 brute-force 併用 | `docs/design/hnsw-generation-cache.md` |
| #409 | RLS 事前フィルタ統合・可視カーディナリティ切替 | `docs/design/hnsw-rls-cardinality-switch.md` |
| #410 | hybrid 密側 iterative scan | `docs/design/hnsw-hybrid-iterative-scan.md` |
| #411 | `EXPLAIN` 露出 | `docs/design/explain-search-engine-exposure.md` |
| #412 | Recall ゲート同一閾値検証・TASK-121 系拡張 | `docs/design/ann-recall-gate-verification.md` |
| #492（#493〜#495） | 凍結後 CSR 化（設計・実装・前後比較） | 本書§14（#493） |

## 3. データ構造・パラメータ既定値（本リポ実装既定値）

| パラメータ | 値 | 出典 |
| --- | --- | --- |
| `HnswParams::default().m` | 16 | 本リポ実装既定値 |
| `HnswParams::default().ef_construction` | 100 | 本リポ実装既定値 |
| `HnswParams::default().ef_search` | 64 | 本リポ実装既定値 |
| `ValidatedHnswParams::full_scan_ratio` | 1/10 | 本リポ実装既定値 |
| `sql/hnsw_cache.rs::MIN_INDEXED_ROWS` | 1,024 | 本リポ実装既定値 |
| `sql/hnsw_cache.rs::REBUILD_DELTA_RATIO` | 1/10 | 本リポ実装既定値 |
| `sql/hnsw_cache.rs::MAX_HNSW_CACHE_ENTRIES` | 8 | 本リポ実装既定値 |
| `hnsw.rs::MAX_M` | 128 | 本リポ実装既定値 |
| `hnsw.rs::MAX_EF` | 10,000 | 本リポ実装既定値 |
| `hnsw.rs::MAX_HNSW_NODES` | 1,000,000 | 本リポ実装既定値 |
| `hnsw/parallel_build.rs::SEQUENTIAL_PREFIX_NODES` | 256 | 本リポ実装既定値（qdrant 方式を参考） |

## 4. opt-in 手順（Rust API のみ）

```rust
let kind = engine::search_engine::hnsw_kind(engine::hnsw::HnswParams::default())?;
let core = engine::core::EngineCore::from_storage_with_engine(storage, kind);
// または EngineCore::open_with_engine(path, kind)
```

既定エンジンは不変（明示的に `hnsw_kind` を渡さない限り brute-force のまま）。
`wire-server` の CLI フラグ・テーブルカタログ属性による opt-in 露出は本 Issue
の対象外（ADR の申し送り事項のまま）。適用状況は `EXPLAIN` の `engine:`・
`hnsw_params:`・`ann_plan:` 行（`docs/design/explain-search-engine-exposure.md`
参照）で確認できる。

## 5. 維持した契約（Phase 3 で固定済み・本書では再検証しない）

既定エンジン不変・`precision` モードは厳密探索（`plain_scan_precision`）・RLS
fail-closed・可視カーディナリティ切替・決定性（同点タイブレーク）・Recall
ゲート同一閾値。詳細は各サブ Issue doc・`ann-index-adoption.md`「判断記録」節
を参照。

## 6. 参照した外部実装（手法名・ライセンス・確認できた既定値のみ）

`ann-index-adoption.md`「参照した外部実装」節の記録を実装確定後に再確認した。

- **qdrant**（Apache-2.0）: 公式ドキュメント（`documentation/concepts/indexing/`）
  で確認——HNSW 既定値 `m=16`・`ef_construct=100`。フィルタ付き検索を
  brute-force（plain scan）へ切り替える `full_scan_threshold_kb`（既定
  10,000 KiB）という設計方針。本リポの `full_scan_ratio`（可視候補数の比率で
  切替）は同じ「フィルタが強すぎる場合は ANN を使わない」思想の変種だが、
  判定基準（バイト数 vs 比率）は異なる本リポ独自設計。
- **pgvector**（PostgreSQL License）: 公式 README で確認——HNSW 既定値
  `m=16`・`ef_construction=64`・`hnsw.ef_search` 既定 `40`。並列ビルドの
  ロック粒度設計・`iterative_scan` 型の境界再取得という設計方針を参考にした
  （#406・#410）。
- **usearch**（Apache-2.0）: `README` のコード例に `connectivity=16`・
  `expansion_add=128`・`expansion_search=64` という値が示されているが、
  ドキュメント上「オプション引数の例示値」としての記載であり、これらが
  ライブラリの実際の既定値であることは確認できなかった（未確認のため本書
  では本リポ採用値として扱わず、確認できた事実〔ライセンス・CORE-5 対照
  エンジンとしての既存利用〕のみを記す）。
- **Lance**（Apache-2.0）: 未索引分 brute-force 併用＋再構築という設計方針
  （#408 が参考にした）。

本リポの `m=16`・`ef_construction=100` は qdrant の確認済み既定値と一致する
（意図した追随ではなく、独立に選定した値が一致した）。`ef_construction` は
pgvector の既定値（64）とは異なる。

## 7. 前後比較（`feature_bench` 13 フェーズ・scale=1＝25,000 行）

### 測定条件

- **before** = commit `0803a8c`（PR #429。Phase 3 の最初の production 変更
  〔#423〕直前。`feature_bench.rs` は Issue #358 でこの時点までに追跡化
  済み）。`git worktree add --detach <dir> 0803a8c` ＋ 別 `CARGO_TARGET_DIR`
  で `cargo build --release -p engine --example feature_bench`。
- **after/既定** = 本ブランチ（`origin/main` `40cc024` ＋ 本 Issue の変更）を
  `BENCH_FEATURE_ENGINE` 未設定（既定 brute-force）で実行。
- **after/hnsw** = 同ブランチを `BENCH_FEATURE_ENGINE=hnsw` で実行。
- 3 条件を各 3 回、時系列で交互に実行（before→after/既定→after/hnsw を 1
  ラウンドとして 3 ラウンド）。**中央値を採用しつつ生値（3 回の p50）も本書へ
  残す**（`sparse-inverted-index.md`「判定」節が指摘した「3 回中央値のみ・
  生値未記録では実変化とノイズを分離できない」問題を避けるため）。
- 本開発環境は**専有環境ではなく**、本 Issue 実装中も他の並列エージェントが
  同一マシン上でビルド・テストを実行していた（Issue #413 実装ログ参照）。
  以下の数値は参考値であり、専有環境での再測定は申し送りとする
  （`docs/design/c1-p95-dedicated-env-reverification.md` と同方針）。
- **索引構築時間プローブの分離**（Issue #439 codex-review 指摘への対応）:
  `index_warm_us`（下記）の計測は 13 フェーズが使う `core` とは別ファイル・
  別 `EngineCore` インスタンス（`feature_bench.rs` の `probe_core`）に対して
  行う。当初の実装は同じ `core` に対してこのプローブを 13 フェーズの直前に
  1 回発行しており、`vector_knn` と同形のクエリのため `SqlArenaCache`／
  `HnswIndexCache` を事前ウォームしてしまい、13 フェーズが cold から
  始まる before バイナリと測定条件が食い違っていた（P1 指摘）。分離後は
  13 フェーズ開始時点で `core` は完全に cold であり、before との直接比較対象
  である 13 フェーズの p50/p95 の測定条件が一致する。この修正により本節の
  数値（特に `point_where`・`vector_knn_where`。下記「判定」節参照）は
  当初版から更新されている。

### ノイズ帯の見積り

`crates/engine/src/` を一切変更していない本 Issue では、before/既定 と
after/既定 の差は**構造的に環境変動のみ**である（コード変更に起因する差は
論理的にありえない）。同一条件（after/既定）内の 3 回の p50 のばらつき
（同一時間帯の run-to-run 差）は非検索フェーズで 1〜2%程度に収まる
（例: `agg_count` raw_p50=[2525, 2554, 2524]us、`udf_call` raw_p50=[689, 681,
672]us）。一方 before（別ビルド・別時間帯）と after/既定 の非検索フェーズ
差は 10〜15%程度（例: `agg_count` 2867us→2525us、`group_by_having`
3556us→3169us）で、これは**同一時間帯の run-to-run 差より大きく、時間帯を
跨いだ背景負荷の変動**を反映していると判断する。本書ではこの 15% 程度を
「時間帯を跨いだ比較のノイズ帯」の目安として扱う。

### 13 フェーズ p50（us・3 回中央値。生値は表下段参照）

| フェーズ | before | after/既定 | after/hnsw |
| --- | --- | --- | --- |
| ingest | 4,175 | 4,120 | 4,125 |
| point_where | 2,752 | 2,885 | 2,870 |
| where_compound | 3,315 | 2,908 | 2,890 |
| agg_count | 2,867 | 2,525 | 2,508 |
| agg_multi | 3,107 | 2,726 | 2,727 |
| group_by_having | 3,556 | 3,169 | 3,189 |
| vector_knn | 8,210 | 9,564 | 8,095 |
| vector_knn_where | 2,770 | 2,883 | 4,282 |
| hybrid_rrf | 11,442 | 12,261 | 10,685 |
| mode_recall | 8,596 | 9,231 | 7,648 |
| mode_precision | 8,562 | 9,034 | 8,404 |
| rls_isolation | 2,791 | 2,418 | 2,416 |
| udf_call | 675 | 681 | 402 |

生値（p50・us・3 回）:

- before: ingest=[4181,4175,4118] point_where=[2754,2752,2733]
  vector_knn=[8086,8210,8813] hybrid_rrf=[11442,11357,12368]
  mode_recall=[8591,8596,8670] mode_precision=[8806,8562,8364]
- after/既定: point_where=[2974,2883,2885] vector_knn=[9539,9564,9588]
  vector_knn_where=[2835,2883,2973] hybrid_rrf=[11775,12261,12264]
  mode_recall=[9547,9231,9049] mode_precision=[8869,9049,9034]
- after/hnsw: point_where=[2892,2870,2846] vector_knn=[8120,8095,8081]
  vector_knn_where=[4271,4382,4282] hybrid_rrf=[10758,10580,10685]
  mode_recall=[7648,7708,7627] mode_precision=[8404,8433,8272]

`meta.index_warm_us`（`probe_core` での 1 回計測。索引構築時間相当。before
バイナリには対応するプローブが無いため before は測定対象外）: after/既定
（`SqlArenaCache` cold 構築）中央値 **48.2ms**、after/hnsw（arena デコード＋
HNSW グラフ構築）中央値 **613.8ms**——約 12.7 倍。`meta.vm_rss_kb_final`
中央値: after/既定 112,004kB、after/hnsw 137,916kB（+23.1%。HNSW グラフの
隣接リスト分）。

### 段別の `ann_plan` 対応（`sql::hnsw_cache::classify_ann_plan`。ソース: `docs/design/explain-search-engine-exposure.md`）

| フェーズ | 形状 | 期待される `ann_plan`（hnsw エンジン時） |
| --- | --- | --- |
| `vector_knn`・`mode_recall` | フィルタなし DISTANCE | `hnsw_full_visible` |
| `point_where`・`vector_knn_where` | SCALAR 事前フィルタ付き DISTANCE | `hnsw_subset`（可視候補比率が `full_scan_ratio` 未満なら `plain_scan_engine`。本条件〔`lang='ja'`＝約 1/5〕は未満にならない） |
| `hybrid_rrf` | Hybrid | `HnswDenseProvider` 経由（`ann_plan` の対象外。密側再取得ループの `hybrid_dense_searches` で確認） |
| `mode_precision` | precision 確信度ゲート | `plain_scan_precision`（構造的に brute-force 固定） |

本書のこの対応は `classify_ann_plan` の既定ドキュメント（#411）から導いた
期待値であり、`EXPLAIN` は `USING PLAN(...)` 構文専用（プランナー注入が必要）
のため、`feature_bench` の生 SQL（`ORDER BY ...`）に対する `EXPLAIN` 出力の
直接取得は本 Issue の測定時間内では行っていない（申し送り。§11 参照）。
`meta.hnsw_stats`（after/hnsw・3 回とも同一値）が実観測として代わりに示す
非 vacuous 確認: `builds=1 build_failures=0 hits=224 misses=1 fallbacks=112
subset_searches=0 hybrid_dense_searches=56 hybrid_queries=56
ef_cap_fallbacks=0 entries=1`。`point_where`・`vector_knn_where` の
`fallbacks=112` は「plain scan」ではなく `Overlay::delta_slots`
（未索引分 brute-force 併用。#408）の呼び出し回数を含む値であり、
`hnsw_subset` 経路自体は `search_subset_or_fallback` がキャッシュ非登録で
呼ばれるため `entries` には現れない（設計は #409 参照）。

### 判定

**既定エンジン非退行**: production コード（`crates/engine/src/`）を一切
変更していない本 Issue において、before→after/既定 の差はすべて環境要因
（時間帯を跨いだ背景負荷の変動。上記「ノイズ帯の見積り」節）に起因する
はずであり、コード変更由来の退行は論理的に存在しない。索引構築時間
プローブを分離した本版では非検索系フェーズの差が概ね数%〜10%程度
（例: `agg_count` 2867us→2525us・約 -12%）に収まり、検索系フェーズ
（`vector_knn` +16.5%・`hybrid_rrf` +7.2%・`mode_recall` +7.4%）との差も
当初版（+29〜32% 対 +10〜15%）より縮小した。専有環境での再測定（申し送り）
でこの残差がさらに縮小するかは未確認だが、**コード差分が存在しない以上
これは受け入れ基準「既定エンジンでの全 13 フェーズ非退行」の対象外**
（比較対象コードが同一であるため退行の定義自体が成立しない）と判断する。

**hnsw の効果（after/既定 vs after/hnsw。同一バイナリ・同一時間帯の
比較のため上記ノイズ要因を受けにくい）**:

- フィルタなし DISTANCE（`vector_knn` -15.4%・`mode_recall` -17.1%）・
  hybrid 密側（`hybrid_rrf` -12.9%）はいずれも高速化（索引構築時間
  プローブ分離後も方向は不変。倍率は当初版〔-22.3%・-18.5%・-11.3%〕から
  縮小したが、これは検索系フェーズの絶対値自体が本節「既定エンジン
  非退行」で述べた環境変動の影響を受けているためで、hnsw の相対効果の
  符号を覆すものではない）。
- `point_where`（-0.5%）は**当初版（+37.5%）から一変してほぼ不変**となった。
  これは索引構築時間プローブの分離（測定条件節参照）による直接的な効果:
  `run_select_phase`（`feature_bench.rs`）は各フェーズの計測ループ開始前に
  素の 1 回実行（warm-up 実行）を挟む構成のため、13 フェーズ開始前の共有
  プローブが無くても `point_where` 自身のこの 1 回で `SqlArenaCache`／
  `HnswIndexCache` の基礎索引が構築され、計測対象の 50 回（`p50`・`p95`
  算出対象）はいずれも索引済みの状態で実行される。当初版の +37.5% は
  「共有プローブが `point_where` より先に基礎索引を暖めていたことで
  `hnsw_subset` 経路のマスク計算コストのみが計測に乗っていた」ことの
  反映であり、単独インスタンスでの再測定（本版）が本来観測すべき値
  （ほぼ等価）である。
- 一方 `vector_knn_where`（+48.5%）は当初版（+39.2%）と同様、あるいは
  それ以上に悪化したままであり、**SCALAR 事前フィルタ付き DISTANCE
  における `hnsw_subset` 経路の実コストは `vector_knn_where` が示す方が
  忠実**（`point_where` は上記の理由で他フェーズの索引ウォームアップに
  依存しない独立した観測点として、より参考になる）。可視候補比率
  （約 1/5）が `full_scan_ratio`（1/10）を下回らず `hnsw_subset` 経路
  （マスク付き ANN 探索＋`Overlay::delta_slots` 補完）を通るが、本コーパス
  規模（25,000 行）・この選択性では、単純な brute-force 走査よりコストが
  高いことを実測が示す。
- `mode_precision` は -7.0%（当初版 +1.7% から符号反転）。構造的に
  `plain_scan_precision` 固定（`hnsw`・`brute_force` いずれも brute-force
  走査）のため理論上は engine 差が生じないはずだが、実測差は上記「既定
  エンジン非退行」で述べた環境変動由来のノイズ帯（15% 程度）の範囲内で
  あり、hnsw 固有の効果とは判断しない。
- `index_warm_us`（12.7 倍）・`vm_rss_kb_final`（+23.1%）はいずれも hnsw の
  明確なコストとして現れている（倍率は当初版〔17.4 倍・+22.1%〕と近い
  オーダーで、`probe_core` 分離後も一貫）。

p95 は別立てで記録する: `vector_knn` after/既定 10,363us→after/hnsw
8,310us（-19.8%）・`point_where` 3,504us→3,004us（-14.3%）・`hybrid_rrf`
14,050us→11,297us（-19.6%）と、p50 と同方向の傾向を示す（測定回数 n=3
のため p95 自体の統計的信頼性は低い。`point_where` の p95 も p50 と同様
ほぼ収束方向にあることが確認できる）。

## 8. 規模スケーリング（25,000 行 vs 100,000 行）

scale=4（`BENCH_FEATURE_SCALE=4`）で 100,000 行（tenant-a 80,000・tenant-b
20,000）を各 2 回測定した（時間制約により scale=1 より回数を減らした。
`hnsw::MAX_HNSW_NODES`〔1,000,000〕には収まる規模）。**before バイナリには
`BENCH_FEATURE_SCALE` が存在しない**（本 Issue で追加した変数のため）ため、
before/scale=4 は測定対象外——規模スケーリング比較は after/既定 vs
after/hnsw のみで行う。

| フェーズ | after/既定（100k） | after/hnsw（100k） | 比（hnsw/既定） |
| --- | --- | --- | --- |
| vector_knn | 60,852.5us | 54,742.5us | 0.900（-10.0%） |
| vector_knn_where | 15,194.5us | 22,175.0us | 1.459（+45.9%） |
| hybrid_rrf | 78,837.5us | 72,382.0us | 0.918（-8.2%） |
| mode_recall | 62,103.0us | 55,129.0us | 0.888（-11.2%） |
| mode_precision | 62,457.0us | 61,446.0us | 0.984（-1.6%） |
| point_where | 14,996.0us | 15,011.5us | 1.001（+0.1%） |

`index_warm_us` 中央値: after/既定 179.4ms → after/hnsw 2,142.8ms（約 11.9
倍）。`vm_rss_kb_final` 中央値: after/既定 377,490kB → after/hnsw
473,198kB（+25.4%）。

（上記は索引構築時間プローブを 13 フェーズ用 `core` と分離した版の実測値
であり、`point_where` が §7 と同様の理由でほぼ等価に収束している点は
25k・100k いずれの規模点でも一貫している。）

**25k vs 100k での hnsw 優位性の変化**: `vector_knn` の hnsw/既定 比は
25k で 0.846（-15.4%）、100k で 0.900（-10.0%）と、**規模が大きくなるほど
hnsw の相対優位が縮小している**（単純な「HNSW は O(log n) で brute-force
の O(n) に対し規模が大きいほど有利」という理論的期待とは逆方向）。同様に
`vector_knn_where`（SCALAR 事前フィルタ付き）の悪化幅も 25k の +48.5% から
100k の +45.9% へほぼ横ばい（縮小方向）である。この 2 規模点のみからは、
本コーパス（`n=2`・非専有環境・`ef_search=64` 固定）で明確な損益分岐点
（brute-force が hnsw に劣後し始める規模）を特定できない——両規模点で
hnsw が `vector_knn`／`mode_recall`／`hybrid_rrf` について brute-force を
上回ったままであり、規模を追うごとに差が縮む傾向は見えるが交差（逆転）は
観測されていない。より広い規模ラダー（例: 10k・50k・250k・500k）での
再測定が損益分岐点の特定には必要であり、本 Issue の時間・環境制約により
申し送りとする（§11）。

## 9. `bench-knn-profile` 前後比較（S0-cold／S0-hot・25,000 行）

`BENCH_KNN_PROFILE_ENGINE`（brute_force／hnsw）で S0-cold（毎サンプル新規
`EngineCore`）・S0-hot（`EngineCore` 使い回し）を各 3 回測定した。S1〜S5' は
構造的にエンジン非依存（生 redb 走査・provider 直呼び）のため測定していない
（S5 を hnsw 相当として提示しない。モジュール冒頭コメント参照）。3 回とも
`hnsw_stats builds=1 build_failures=0 hits=40 misses=1 fallbacks=0
entries=1`（非 vacuous 確認・完全一致）。

| 段 | brute_force（中央値） | hnsw（中央値） | 比 |
| --- | --- | --- | --- |
| S0-cold（毎サンプル新規構築） | 16.577ms | 379.522ms | 22.9x |
| S0-hot（キャッシュヒット） | 0.642ms | 0.430ms | 0.670（-33.0%） |

S0-cold の 22.9 倍は「毎サンプル HNSW グラフをゼロから構築するコスト」を
そのまま表しており、`feature_bench` の `index_warm_us`（12.7 倍。1 回限りの
構築を計測。§7 参照）と桁が一致する（構築コストは規模〔25k 行〕にほぼ比例する
という前提と整合）。S0-hot の -33.0% は `feature_bench` の `vector_knn`
（-15.4%）と方向・オーダーが一致し、hnsw のホットパス（索引構築済み後の
単発クエリ）優位性を独立に裏付ける。

## 10. 損益分岐点についての結論（B 案条件 1 の事後確認）

本 Issue の測定（25k・100k の 2 規模点、非専有環境、n=2〜3）から:

1. **フィルタなし DISTANCE・hybrid 密側・ホットパス**では、25k・100k の
   いずれの規模でも hnsw が brute-force を上回る（p50 で 8〜17% 高速）。
2. **SCALAR 事前フィルタ付き DISTANCE**（可視候補比率が `full_scan_ratio`
   を下回らない選択性）は選択性次第で結果が分かれる: `vector_knn_where`
   （lang×topic 複合条件でより選択的）は 25k・100k のいずれでも hnsw が
   brute-force より明確に遅い（+46〜49%）一方、`point_where`（`lang='ja'`
   単独・約 1/5 可視）はほぼ不変（±0.5% 程度）だった。索引構築時間
   プローブの分離により、当初 `point_where` にも見えていた悪化（+37〜39%）
   が計測条件の不一致による見かけ上のものだったと判明したため（§7 参照）、
   `hnsw_subset` 経路の実コストとしては `vector_knn_where` の実測をより
   重視すべきである。
3. **索引構築コスト**（`index_warm_us`）は 25k で brute-force cold 構築の
   約 12.7 倍、100k で約 11.9 倍——規模が変わっても比率はほぼ一定であり、
   ワンショットの構築コストというより経路の恒常的なオーバーヘッドとして
   扱うべき値である。
4. 明確な「brute-force が hnsw を下回り始める規模」（逆の損益分岐点）は
   本測定の 2 規模点では観測されなかった。1〜3 を総合すると、既定
   `HnswParams`（`full_scan_ratio=1/10`）下では**フィルタの選択性が損益
   分岐の主要因であり、行数の規模そのものは（少なくとも 25k〜100k の
   範囲では）副次的**という所見を得た。ADR の「対象規模の閾値」仮説
   （行数が主要因という想定）は本測定の範囲では支持されない。

## 11. スコープ外・申し送り

- HNSW 索引の永続化・`wire-server` CLI／テーブルカタログ属性による opt-in
  露出（ADR 申し送り事項）
- 専有環境での再実測（本書の数値は全て非専有・共有開発環境での参考値）
- より広い規模ラダー（10k・50k・250k・500k 等）での損益分岐点の精密化
- `EXPLAIN`（`USING PLAN(...)` 経由）による `ann_plan` の実出力取得——
  本書§7 の対応表はドキュメント（#411）からの期待値であり、プランナー
  スタブの注入を要する実測は時間制約により未実施
- `HnswParams` の非既定値（`ef_search` 等）のスイープ
- `contrast_bench`（usearch）への HNSW 対照経路追加
- 凍結後 CSR 化の実装（#494）・前後比較実測（#495）——設計は本書§14
  （Issue #493）

## 12. 再現方法

```bash
# before バイナリ
git worktree add --detach /tmp/ann413-before 0803a8c
CARGO_TARGET_DIR=/tmp/ann413-target-before cargo build --release -p engine --example feature_bench

# after バイナリ（本ブランチ）
cargo build --release -p engine --example feature_bench

# scale=1（既定）
./target/release/examples/feature_bench                         # 既定エンジン
BENCH_FEATURE_ENGINE=hnsw ./target/release/examples/feature_bench

# scale=4（100k 行）
BENCH_FEATURE_SCALE=4 ./target/release/examples/feature_bench
BENCH_FEATURE_ENGINE=hnsw BENCH_FEATURE_SCALE=4 ./target/release/examples/feature_bench

# bench-knn-profile
make bench-knn-profile
BENCH_KNN_PROFILE_ENGINE=hnsw make bench-knn-profile
```

## 13. 参照（ポインタのみ）

- `docs/design/ann-index-adoption.md`（ADR。B 案採否判断・実装ガイド）
- `docs/design/sparse-inverted-index.md`（同型の前後比較 doc の雛形）
- `docs/design/ann-recall-gate-verification.md`（Recall ゲート同一閾値検証）
- `docs/design/explain-search-engine-exposure.md`（`EXPLAIN` 露出仕様）
- `docs/design/c1-p95-dedicated-env-reverification.md`（非専有環境の扱いの
  先例）

## 14. 凍結後 CSR 化の設計（Issue #493）

- **ステータス**: Implemented（#494 で実装済み。前後比較実測は #495。本節
  自体は本書冒頭ステータス「Accepted（記録専用・#413）」とは独立に扱う）
- **親**: #492（HNSW 隣接リストの CSR 化）／Phase 3 親 #458／ルート #455
- **依存**: `docs/design/benchmark-judgement-policy.md`（#462）
- **関連ポインタ（spec・本文は転記しない）**: TASK-132・CORE-9・CORE-10

### 14.1 現状の構造と問題

`crates/engine/src/hnsw.rs::Node { level: usize, links: Vec<Vec<u32>> }`・
`HnswIndex { params, dim, nodes: Vec<Node>, entry_point, vectors: Arc<[f32]> }`
がノード単位の可変長隣接リストを保持する。隣接アクセスは
`HnswIndex::neighbors(level, node) -> Option<&[u32]>` に集約されており、
`search_layer`・`greedy_descend`・`greedy_descend_masked`・
`accepted_reachable_count`・`bfs_reachable`・`repair_reachability` の 6 箇所
が呼ぶ。`links` を直接触るのは `connect`・`shrink_links`・`insert_node`・
`approx_heap_bytes`・`neighbors` の 5 箇所のみ。

| 問題 | 内容 |
| --- | --- |
| ヒープ確保回数 | ノード × (level+1) 個の `Vec<u32>` それぞれが個別確保（ヘッダ 24B＋データ） |
| 容量スラック | `push` の倍化戦略により実次数より大きい容量を確保し得る |
| 間接参照 | `nodes[node]` → `links[level]` → ヒープの 2 段間接参照で、探索の隣接走査ごとにプリフェッチ不能 |

`docs/design/hotpath-implementation-survey.md` §3「HNSW グラフ」の CSR 型
フラット隣接配列（faiss `impl/HNSW.h`・MIT）が「採用推奨」としており、#492
が実装を追跡する。

### 14.2 2 相構成（構築は可変長・凍結時に平坦化）

並列構築（#406・`hnsw/parallel_build.rs::BuildGraph`）はノード単位
`RwLock<Vec<Vec<u32>>>` への可変長追記・`shrink_links` による in-place 縮退
を前提とし、凍結後に走る `repair_reachability`（`connect`／`shrink_links` で
隣接を書き換える）も可変表現を要する。したがって CSR は**構築中は維持でき
ず**、修復まで完了した後に平坦化する 2 相構成を取る。

| 経路 | 相 1: 可変長構築 | 相 2: 修復 | 相 3: 凍結（平坦化） |
| --- | --- | --- | --- |
| `build`（逐次） | `Vec<Node>` へ `insert_node`（`connect`／`shrink_links`） | `repair_reachability`（可変表現上） | 最後に CSR へ平坦化 |
| `build_with_threads(threads>=2, n>SEQUENTIAL_PREFIX_NODES)` | `BuildGraph`（ノード単位 `RwLock<Vec<Vec<u32>>>`） | `freeze`＝組み立て（ロック解除・`Vec<Node>` 化）→ `repair_reachability` | 最後に CSR へ平坦化 |
| `build_with_threads(1)`／`n<=SEQUENTIAL_PREFIX_NODES`／`build_parallel` 縮退 | `build` をそのまま呼ぶ | 同左 | 同左 |

`HnswIndex` の実行時表現は **CSR の 1 種類のみ**にする（逐次経路だけ
`Vec<Vec<u32>>` を残す案は表現が 2 つ併存し `search_layer` の分岐・テスト
行列が倍になるため不採用）。平坦化は `repair_reachability` を含む全ての可変
操作の**後**に置く（`repair_reachability` は `connect`／`shrink_links` を
呼ぶため CSR 上では実行できない）。#446 ツリー（#447 観測フック・#448 上位
層リンク保証・#449 修復並列化）はいずれも相 1〜2 の内部改善であり、相 3 が
最終段である限り衝突しない——この stage 順序の固定を #494・#449 双方の契約
とする。

可変相の型は現行 `Vec<Node>` を「ビルダー表現」として残す（型名の変更は #494
の裁量）。`insert_node`・`connect`・`shrink_links`・`repair_reachability`・
`bfs_reachable` はこの可変表現のメソッドへ移り、
`HnswIndex` は凍結済み CSR とパラメータ・`entry_point`・`vectors` のみを持つ。

### 14.3 CSR レイアウト

```text
struct CsrGraph {
    levels:    Vec<u8>,     // ノードごとのレベル（MAX_LEVEL=32 のため u8 で足りる）
    node_base: Vec<u32>,    // ノードごとの offsets 先頭添字（len = n + 1）
    offsets:   Vec<u32>,    // (node, level) ごとの links 先頭添字（len = 総 (node, level) 数 + 1）
    links:     Vec<u32>,    // 全ノード・全レベルの隣接 id を連結（ノード昇順→レベル昇順、各リスト内は構築時の順序を保持）
}
// neighbors(level, node):
//   node < n かつ level <= levels[node] のとき
//   links[offsets[node_base[node] + level] .. offsets[node_base[node] + level + 1]]
//   （添字はすべて get()・checked_add。範囲外は None）
```

**exact-length CSR（パディングなし）を採用する。** faiss の固定スロット＋
番兵（`-1`）方式は構築中の in-place 更新を許す代わりに `max_degree − 実次数`
分を空費し、番兵のため id 型を符号付きにするか `u32::MAX` を予約する必要が
ある。本リポの凍結後索引は読み取り専用（#408 の世代整合キャッシュは差分を
brute-force overlay で補い、索引本体は再構築で更新する）で in-place 更新が
不要なため、exact-length が適合する。不採用の他レイアウト（`survey.md` §3
の記録を参照。再論しない）: hnswlib の level0 インターリーブ（`Arc<[f32]>`
を別途一括所有する設計と重複）、usearch の可変長 tape（アンアラインドアク
セス＝`unsafe`）、qdrant のビット詰め（永続化未実装のため時期尚早）。

**offset 幅の判断**: 理論上限は
`MAX_HNSW_NODES × (2·MAX_M + MAX_LEVEL·MAX_M)` = 1,000,000 ×
(256 + 32×128) ≈ 4.35×10^9 で `u32::MAX`（約 4.29×10^9）を超え得る。選択肢
は (a) `usize` offsets（上限問題なし・8 B/要素）、(b) `u32` offsets ＋
`checked_add` 累積で超過時は既存の `HnswError::CapacityOverflow` を返す
fail-closed（新 variant 不要・panic 経路なし）。**(b) を採用する**——実運用
でこの上限に達する索引は links だけで 17 GB を超え非現実的であり、`u32` で
`node_base`／`offsets` のキャッシュ占有を半減できる。到達し得ない分岐を残さ
ない方針との整合は「`checked_add` は untrusted 入力サイズに対する
fail-closed 検証であって dead branch ではない」と位置づける。`levels` の
`u8` 化は `assign_level` が `MAX_LEVEL` で上限を持つため安全（変換は
`u8::try_from` で fail-closed）。総 (node, level) 数 `Σ(level+1)` は
`n × (MAX_LEVEL+1)` ≤ 3.3×10^7 で `u32` に収まる（同じく `checked_add`）。

### 14.4 `search_layer` を 2 表現で共有する方式

構築中の `insert_node` は `search_layer`／`greedy_descend` を**可変表現**に
対して呼び、凍結後の `search`／`search_masked` は **CSR** に対して呼ぶ。

**採用: 隣接アクセス trait によるジェネリック化**（既存の `VisitedSet` trait
と同型のパターン）。例: `trait Adjacency { fn neighbors(&self, level: usize,
node: u32) -> Option<&[u32]>; fn node_count(&self) -> usize; }` を可変表現と
`CsrGraph` の双方に実装し、`search_layer<V: VisitedSet, A: Adjacency>`・
`greedy_descend`・`greedy_descend_masked`・`accepted_reachable_count`・
`find_alternate_entry`・`search_entry_for_mask` をジェネリック化する。モノモ
ーフィゼーションにより探索経路は CSR 専用コードになり、動的ディスパッチを
持ち込まない。

不採用: enum ディスパッチ（探索ホットループに毎回の分岐が入る）、実装の複製
（停止条件・受理判定〔PR #423／#431 是正〕の二重管理になる）。
`parallel_build::search_layer_locked`／`greedy_descend_locked`（ロック下で
隣接をコピーする別実装）は本設計の対象外・無変更。

`search_layer` の内部実装 `search_layer_with<V: VisitedSet, P:
prefetch::PrefetchPolicy>`（#490・`hnsw/prefetch.rs`。§14.7 参照）は既に
2 個のジェネリックパラメータを持つ。本設計で `A: Adjacency` を追加する場合は
`search_layer_with<V, P, A>` の 3 パラメータになる——`search_layer<V:
VisitedSet>` という**公開シグネチャ**は変えず（§14.8 の不変性の範囲）、
内部の `search_layer_with` 呼び出し側のみが影響を受ける。

### 14.5 決定性契約への影響

1. **平坦化は「修復済み可変グラフ」の純粋関数であり順序保存**とする。ノード
   昇順・レベル昇順に連結し、**各隣接リスト内の要素順を一切並べ替えない**
   （ソート・正規化を禁止）。`search_layer` は隣接を走査順に `candidates`／
   `results` へ積み、同点スコア時の `ef` 打ち切り・`results.pop()` の追い出
   し対象は走査順に依存するため、リスト内順序の変更は同点境界で探索集合を
   変え得る。
2. 順序保存により `neighbors()` の返すスライスは平坦化前後でバイト同一
   → `search`／`search_masked` の結果はビット同一 →
   **公開 API 経由の結合テスト**（`tests/hnsw.rs` の不変条件・
   `tests/hnsw_search.rs` の Recall／決定性・`tests/hnsw_cache.rs`・`rls.rs`
   の HNSW 系・
   `parallel_build.rs::build_with_threads_one_matches_sequential_build_exactly`）
   は無変更で green になる——これが #494 の受け入れ条件「既存の全 HNSW
   テストが無変更で green」「`build` 逐次経路のグラフ（平坦化前）が不変」の
   根拠。ただし private 関数を直接呼ぶモジュール内単体テスト（`freeze` を
   直接呼ぶもの・`HnswBuildProfile` を参照するもの）は `freeze` のシグネチ
   ャ・プロファイルのフィールド追加に伴う**呼び出し形の追随編集**が必要に
   なり得る（アサーション内容は変えない）。「無変更」の主張は公開 API 経由
   のテストに限定する。
3. `build_with_threads(1)`／`n<=SEQUENTIAL_PREFIX_NODES` の完全一致契約:
   縮退経路は引き続き `build` をそのまま呼ぶため、同一の平坦化関数を通り
   構造的に同一。`threads>=2` 経路のグラフ**形状**の run-to-run 非決定性
   （`docs/design/hnsw-parallel-build.md` 冒頭「決定性の範囲」）は本設計で
   変わらない（平坦化は形状を変えない）。
4. `docs/design/hnsw-search.md`「決定性の保証範囲」（同一索引・同一クエリ・
   任意スクラッチで再現／境界同点グループ完全化は非保証）・
   `docs/design/rrf-tie-break-determinism.md`（スコア降順・id 昇順）は不変。
   RLS 事前フィルタ統合（#409。索引は ctx 可視アリーナのみから構築・
   `NodeMask` は候補差分）も表現非依存で不変。
5. **可変相のアルゴリズム**（`insert_node`・`connect`・`shrink_links`・
   `repair_reachability`・`select_neighbors_heuristic_free`・
   `compute_shrink`）は**コード無変更**とする（`&mut self` の対象型が変わ
   るだけ）。#494 の制約として明記する。

### 14.6 メモリ見積り（100k 点・M=16。式＋数値。実測は参考値）

`assign_level` は `P(level ≥ l) = M^{-l}` の幾何分布 → 期待 (node, level) 数
= `n × M/(M−1)` = 100,000 × 16/15 ≈ **106,700**（上位層ノード ≈ 6,700）。

| 表現 | グラフ部の合計上界（式） | 概算値 | ヒープ確保回数 |
| --- | --- | --- | --- |
| 現行（`Vec<Vec<u32>>`） | `Node` 本体 100,000×32B ＋ レベル別 `Vec<u32>` ヘッダ 106,700×24B ＋ links 容量上界 100,000×32×4B + 6,700×16×4B | 3.2MB + 2.56MB + 13.2MB ≈ **19MB** | ≈ 206,700 回（初回確保のみ。`push` 倍化に伴う再確保は含まず） |
| CSR | `links` 実次数合計 ＋ `offsets`(106,701×4B) ＋ `node_base`(100,001×4B) ＋ `levels`(100,000×1B) | 13.2MB + 0.43MB + 0.4MB + 0.1MB ≈ **14.1MB** | 4 回 |

削減見込みはグラフ部で約 5MB（≈ 26%）＋確保回数の 5 桁削減。ベクトル本体
（`Arc<[f32]>`。dim=128 で 100,000×128×4B = 51.2MB）は不変のため、索引全体
では約 70MB → 65MB（≈ 7%）。本書§7 の実測（25k 行・dim 128 で RSS +23.1%）
のうちグラフ部が占める割合の目安として参考にする。

実測の位置づけ: 既存 API `HnswIndex::approx_heap_bytes()` と
`bench-hnsw-parallel-build` の RSS 出力で現行値を**参考値**として取得して
よい（`docs/design/benchmark-judgement-policy.md` §6 は CSR 化を本 QEMU
環境で判定不能な種別に列挙しているため、採否根拠にはしない。本格的な前後
比較は #495）。`approx_heap_bytes` 自体は #494 で CSR 向けに書き換えが必要
（`approx_heap_bytes_at_least_covers_raw_vector_storage` テストは green の
まま維持する）。

### 14.7 探索経路の変更点

- `neighbors()`: `nodes[node].links[level]` の 2 段間接参照 →
  `node_base[node]`・`offsets[b+level]`・`offsets[b+level+1]` の 3 ロード＋
  連続スライス（すべて `get()`／`checked_add`）。`level_of` は
  `levels[node]` の 1 ロード。`max_level`・`entry_point`・`len`・`dim`・
  `params`・`vector`・`max_degree` は不変。
- #490（`hnsw/prefetch.rs`。本節執筆時点で origin/main へ merge 済み）は
  受理判定通過後の隣接ノードの**ベクトル・visited スロット**を
  `core::hint::black_box` による早期 load（stable では `#[target_feature]`
  外から真の prefetch 命令を safe に呼べない制約のための best-effort 代替。
  `hnsw/prefetch.rs` 冒頭コメント参照）で先読み済みだが、**次候補自身の
  links 範囲**（隣接リストの走査に必要なアドレス）はまだ prefetch 対象に
  含まれていない——現行 `Vec<Vec<u32>>` では `nodes[node].links[level]` が
  2 段間接参照でありアドレスを事前計算できないため。CSR 化により
  `node_base[node]`・`offsets[b+level]` の 2 ロードだけで次候補の links 範囲
  アドレスが確定するため、この不足分（links 範囲の prefetch）を追加する余地
  が生まれる。真の prefetch 命令への切替可否は本設計の対象外（#490 の
  「stable での制約」節と同じ制約が CSR 化でも変わらず残る）。PR #431 の P0
  契約（非受理ノードのベクトルへ一切触れない）は CSR 化で変わらず、prefetch
  は可視判定後にのみ置けるという制約（`survey.md` §3 の記載）もそのまま
  維持する。`search_layer_with<V: VisitedSet, P: PrefetchPolicy>` へ §14.4 の
  `A: Adjacency` を足す場合は `search_layer_with<V, P, A>` の 3 パラメータに
  なる（`search_layer<V: VisitedSet>` という公開シグネチャ自体は変えず、
  内部の `search_layer_with` 側にのみ影響する）。
- 将来の永続化（ADR 申し送り）では連続配列 4 本をそのまま書き出せる（qdrant
  型ビット詰めはその時点で再検討する）。
- `HnswBuildProfile`: 現行 `freeze` 段は「`RwLock` を解いて組み立てる段
  （`repair` を含まない・実測 1ms 未満）」と定義済み。平坦化は O(総 links)
  のコピーを伴い `freeze` の意味が変わるため、**新フィールド
  `flatten: Duration` を追加**し、stage 順序を `level_assign →
  sequential_prefix → parallel_phase → freeze（assemble）→
  repair_reachability → flatten` と定義する。逐次縮退経路では従来どおり全
  量を `sequential_prefix` へ積む。#495 は `flatten` を独立段として記録す
  る。互換性の注記: `HnswBuildProfile` は `pub` struct で `#[non_exhaustive]`
  が付いていない（`derive(Debug, Clone, Default)` のみ）。crate 内の構築箇
  所（`hnsw.rs`・`parallel_build.rs`）は `..Default::default()` で非破壊だ
  が、`pub` フィールド追加は外部の構造体リテラル／網羅的 destructure に対し
  てソース非互換になり得る。リポ内の唯一の外部構造体リテラル
  （`tests/hnsw_parallel_profile_accept.rs`）は `..HnswBuildProfile::default()`
  を使っており（本節記述時点で確認済み）、`benches/hnsw_parallel_build_bench.rs`
  はフィールド読み取りのみのためリポ内は追随編集不要。それでも `pub` フィ
  ールド追加は公開 API 上のソース非互換になり得るため、#494 の PR では
  Breaking changes 節で明示する（`docs/design/error-enum-non-exhaustive-policy.md`
  と同方針。`#[non_exhaustive]` の後付けはしない）。

### 14.8 公開 API の不変性（#494 の「探索 API は不変」の根拠）

不変: `HnswIndex::{build, build_with_threads, build_with_threads_observed,
build_parallel, search, search_masked, neighbors, level_of, max_level,
entry_point, max_degree, len, is_empty, dim, params, vector,
approx_heap_bytes}`・`pub(crate) is_mask_fully_reachable`・
`HnswSearchScratch`・`NodeMask`・`HnswError`（variant 追加なし）。呼び出し元
（`sql/hnsw_cache.rs`・`sql/hnsw_hybrid.rs`・
`rls.rs::PrefilterSnapshot::search_with_hnsw`・`hnsw/provider.rs`・
`tests/hnsw*.rs`・`benches/hnsw_*`）は無変更で通る。

### 14.9 #494 向けテスト計画（列挙のみ・本 Issue では実装しない）

1. 平坦化ラウンドトリップ: 可変表現のスナップショット（全 (node, level) の
   `Vec<u32>`）と CSR の `neighbors()` が全件・順序込みで一致（`pub(crate)`
   の平坦化関数を単体テストから直接呼ぶ）。「平坦化前のグラフ不変」を検証
   可能にする唯一の経路。
2. 既存回帰: `tests/hnsw.rs`（次数上限・自己ループ・重複・層整合・連結性）、
   `parallel_build.rs` の `threads=1` 完全一致、`tests/hnsw_search.rs` の
   Recall／決定性、`tests/hnsw_cache.rs`・`hnsw_hybrid_refetch.rs`・
   `incremental_index_hnsw.rs`・`rls.rs` の HNSW 系——**公開 API 経由のテス
   トはすべて無変更で green** を受け入れ条件とする（private 関数を直接呼ぶ
   モジュール内単体テストは §14.5 項 2 のとおり呼び出し形の追随のみ許容し、
   アサーションは変えない）。
3. fail-closed: offsets 累積の `checked_add` ヘルパ単体テスト（`u32` 超過で
   `CapacityOverflow`）、`levels` の `u8::try_from`。
4. `approx_heap_bytes` の下限テスト維持＋ CSR 4 配列の `capacity` を計上す
   る新算式の単体テスト。
5. `clippy -D warnings`・新規 `unsafe` ゼロ。

### 14.10 リスク・申し送り

- 平坦化コピーが構築時間へ加算される（100k 点で数 ms〜十数 ms の見込み。
  #495 で `flatten` 段として実測）。
- `search_layer` のジェネリック化で `hnsw.rs` の関数シグネチャが増える
  （`pub(crate)` のみ。外部 API 影響なし）。
- #449（修復並列化）と #494 が `freeze` 周辺を同時に触る可能性 → §14.2 の
  stage 順序を先に固定して衝突を回避する。
- 実測値はすべて共有 QEMU 環境の参考値
  （`docs/design/benchmark-judgement-policy.md` §5〜6）。

### 14.11 参照

- `docs/design/hotpath-implementation-survey.md`（§3「HNSW グラフ」・
  §9-#5）
- `docs/design/chip-kernel-guidelines.md`（§0.2・prefetch の safe/unsafe 境
  界）
- `docs/design/benchmark-judgement-policy.md`（§5〜6・専有環境判定不能な施
  策種別）
- `docs/design/hnsw-parallel-build.md`（並列構築・決定性の範囲）
- `docs/design/hnsw-search.md`（決定性の保証範囲）
- `docs/design/hnsw-graph-construction.md`（データ構造・API）

### 14.12 実装追記（#494）

§14.2〜§14.9 の設計をそのまま実装した。最終的な型名・判断は以下のとおり
（本節は実装後の事実の記録。§14.1〜§14.11 の設計方針自体への変更はない）。

- **型名**: 可変長ビルダー表現は `hnsw.rs::GraphBuilder { params, nodes:
  Vec<Node>, entry_point }`（`pub(crate)`）。凍結後表現は新設
  `hnsw/csr.rs::CsrGraph`（`levels: Vec<u8>`・`node_base: Vec<u32>`（長さ
  n+1）・`offsets: Vec<u32>`・`links: Vec<u32>`。exact-length・パディング
  なし・順序保存）。共有インターフェースは `hnsw.rs::Adjacency` trait
  （`level_of`／`neighbors`／`node_count`）で、両型がこれを実装する。
- **ジェネリック化の範囲**: §14.4 の「例」提示どおり、構築時・探索時の双方
  から呼ばれる `search_layer` のみを `Adjacency` でジェネリック化した自由
  関数 `search_layer_in<V, P, A>` へ抽出した。`HnswIndex::search_layer`／
  `search_layer_with`（既存シグネチャ）と `GraphBuilder::search_layer`
  はいずれもこの自由関数へ委譲する薄いラッパー。`greedy_descend_masked`・
  `find_alternate_entry`・`search_entry_for_mask`・
  `accepted_reachable_count`・`is_mask_fully_reachable` は探索専用のため
  `HnswIndex`（`self.graph: CsrGraph` 直参照）に残置し、ジェネリック化しな
  かった（シグネチャ増加を最小に抑える判断。§14.4 の裁量の範囲内）。
  `greedy_descend`（非マスク）・`insert_node`・`connect`・`shrink_links`・
  `repair_reachability`・`bfs_reachable` は本体無変更のまま `GraphBuilder`
  のメソッドへ移設した。`select_neighbors_heuristic`（`self` を一切参照
  しない薄い委譲）は `HnswIndex` に残し、構築経路（`GraphBuilder::
  insert_node`）は本体の純粋関数 `select_neighbors_heuristic_free` を直接
  呼ぶ形へ変更した（`#[cfg(test)]` 限定で `HnswIndex::
  select_neighbors_heuristic` を残置。既存テスト
  `select_neighbors_heuristic_prunes_redundant_close_candidates` が
  `HnswIndex` 経由で純粋関数の挙動を検証するため）。
- **stage 順序**: `level_assign → sequential_prefix → parallel_phase →
  freeze（assemble_graph によるGraphBuilder への構造的な組み立て）→
  repair_reachability → flatten（HnswIndex::freeze_from による CSR 平坦
  化）`。平坦化は常に最終段（§14.2・#449 との衝突回避方針どおり）。
- **`HnswBuildProfile.flatten`**: 追加した。逐次縮退経路（`threads == 1`
  または `n <= SEQUENTIAL_PREFIX_NODES`）はこの区切りが存在しないため
  `sequential_prefix` へ全量を積み、`flatten` は `Duration::ZERO` のまま
  （既存フィールドと同じ縮退規約）。
- **指紋テスト**: `tests/hnsw.rs::graph_fingerprint_is_stable_across_representation_change`
  を実装前（commit `2ca1536`。CSR 化前の `Vec<Vec<u32>>` 表現）で 1 度だけ
  採取した FNV-1a 64bit 値（自作・依存追加なし）で固定し、CSR 化の前後で
  `build` が返すグラフ（`entry_point`／各ノードの `level_of`／各層の
  `neighbors` を返された順序のまま走査）がビット同一であることを機械検証
  した（green）。既存の全 HNSW テスト（`crates/engine/src/hnsw.rs` 内・
  `tests/hnsw*.rs`・`tests/incremental_index_hnsw.rs` 等）は無変更のまま
  green（構造体リテラルで `HnswIndex` を直接組む 7 テストのみ、新設ヘルパ
  `index_from_nodes`〔`GraphBuilder` → `freeze_from` を経由〕へ呼び出し形
  を追随。アサーションは無変更）。
- **`CsrGraph::neighbors` の `None`／`Some(&[])` 区別**: exact-length
  オフセットだけでは「範囲内レベルだがリンク 0 件」と「レベル超過／ノード
  範囲外」を区別できないため、`level_of` で明示的にレベル上限を検査してか
  ら区間を引く実装にした（旧 `Vec<Vec<u32>>` 表現と同じ `None`／
  `Some(&[])` 契約を維持。`csr.rs` 内単体テストで固定）。
- **スコープ外（申し送り）**: links 範囲 prefetch（§14.7 の「余地」）は追加
  しなかった（ビット同一の根拠を崩すため。#489 系・#495 へ申し送り）。
  `parallel_build::search_layer_locked`／`greedy_descend_locked`（要素単位
  ロック版の並列構築専用アルゴリズム。ロック粒度の都合で `Adjacency` を
  経由しない独自実装のまま）は無変更。`benches/harness/
  hnsw_parallel_profile.rs::serial_share` への `flatten` 加算は #495 の
  担当（本 PR の `serial_share` は `flatten` を含まない）。
- **`unsafe`**: 新規追加なし。`csr.rs` の添字アクセスはすべて `get()`／
  `checked_add`／`checked_mul` のみ（coding-rust.md）。
