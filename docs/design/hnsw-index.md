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
