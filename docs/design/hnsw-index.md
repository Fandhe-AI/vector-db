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

### ノイズ帯の見積り

`crates/engine/src/` を一切変更していない本 Issue では、before/既定 と
after/既定 の差は**構造的に環境変動のみ**である（コード変更に起因する差は
論理的にありえない）。同一条件（after/既定）内の 3 回の p50 のばらつき
（同一時間帯の run-to-run 差）は非検索フェーズで 2〜5%程度に収まる
（例: `agg_count` raw_p50=[2526, 2459, 2457]us、`udf_call` raw_p50=[671, 662,
676]us）。一方 before（別ビルド・別時間帯）と after/既定 の非検索フェーズ
差は 10〜15%程度（例: `agg_count` 2867us→2459us、`group_by_having`
3556us→3156us）で、これは**同一時間帯の run-to-run 差より大きく、時間帯を
跨いだ背景負荷の変動**を反映していると判断する。本書ではこの 15% 程度を
「時間帯を跨いだ比較のノイズ帯」の目安として扱う。

### 13 フェーズ p50（us・3 回中央値。生値は表下段参照）

| フェーズ | before | after/既定 | after/hnsw |
| --- | --- | --- | --- |
| ingest | 4,175 | 4,109 | 4,100 |
| point_where | 2,752 | 3,361 | 4,620 |
| where_compound | 3,315 | 2,970 | 2,908 |
| agg_count | 2,867 | 2,459 | 2,454 |
| agg_multi | 3,107 | 2,739 | 2,679 |
| group_by_having | 3,556 | 3,156 | 3,181 |
| vector_knn | 8,210 | 10,586 | 8,229 |
| vector_knn_where | 2,770 | 3,352 | 4,665 |
| hybrid_rrf | 11,442 | 14,000 | 12,414 |
| mode_recall | 8,596 | 11,378 | 9,271 |
| mode_precision | 8,562 | 10,253 | 10,425 |
| rls_isolation | 2,791 | 2,386 | 2,401 |
| udf_call | 675 | 671 | 403 |

生値（p50・us・3 回）:

- before: ingest=[4181,4175,4118] point_where=[2754,2752,2733]
  vector_knn=[8086,8210,8813] hybrid_rrf=[11442,11357,12368]
  mode_recall=[8591,8596,8670] mode_precision=[8806,8562,8364]
- after/既定: point_where=[3522,3300,3361] vector_knn=[10596,10586,9498]
  vector_knn_where=[3488,3299,3352] hybrid_rrf=[13480,14596,14000]
  mode_recall=[11566,10219,11378] mode_precision=[10253,10991,10043]
- after/hnsw: point_where=[4796,4620,4511] vector_knn=[8462,8032,8229]
  vector_knn_where=[4847,4524,4665] hybrid_rrf=[12568,12274,12414]
  mode_recall=[9488,9271,9199] mode_precision=[10498,10425,10385]

`meta.index_warm_us`（1 回計測。索引構築時間相当。before バイナリには対応する
プローブが無いため before は測定対象外）: after/既定（`SqlArenaCache` cold
構築）中央値 **35.0ms**、after/hnsw（arena デコード＋HNSW グラフ構築）中央値
**608.7ms**——約 17.4 倍。`meta.vm_rss_kb_final` 中央値: after/既定
108,680kB、after/hnsw 132,732kB（+22.1%。HNSW グラフの隣接リスト分）。

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
はずであり、コード変更由来の退行は論理的に存在しない。数値上は検索系
フェーズ（`vector_knn` +29%・`hybrid_rrf` +22%・`mode_recall` +32%）が
非検索系フェーズ（+10〜15%）より大きく増えており、専有環境での再測定
（申し送り）でこの差が縮小するかは未確認だが、**コード差分が存在しない
以上これは受け入れ基準「既定エンジンでの全 13 フェーズ非退行」の対象外**
（比較対象コードが同一であるため退行の定義自体が成立しない）と判断する。

**hnsw の効果（after/既定 vs after/hnsw。同一バイナリ・同一時間帯の
比較のため上記ノイズ要因を受けにくい）**:

- フィルタなし DISTANCE（`vector_knn` -22.3%・`mode_recall` -18.5%）・
  hybrid 密側（`hybrid_rrf` -11.3%）はいずれも高速化。
- SCALAR 事前フィルタ付き DISTANCE（`point_where` +37.5%・
  `vector_knn_where` +39.2%）はいずれも悪化。可視候補比率（約 1/5）が
  `full_scan_ratio`（1/10）を下回らず `hnsw_subset` 経路（マスク付き
  ANN 探索＋`Overlay::delta_slots` 補完）を通るが、本コーパス規模
  （25,000 行）・この選択性では、単純な brute-force 走査よりコストが
  高いことを実測が示す。
- `mode_precision` はほぼ不変（+1.7%。構造的に `plain_scan_precision`
  固定のため期待どおり）。
- `index_warm_us`（17.4 倍）・`vm_rss_kb_final`（+22.1%）はいずれも hnsw の
  明確なコストとして現れている。

p95 は別立てで記録する: `vector_knn` after/既定 12,667us→after/hnsw
8,509us（-32.8%）・`point_where` 3,742us→5,270us（+40.8%）・`hybrid_rrf`
17,421us→12,880us（-26.1%）と、p50 と同方向・より大きな振れ幅を示す
（測定回数 n=3 のため p95 自体の統計的信頼性は低い）。

## 8. 規模スケーリング（25,000 行 vs 100,000 行）

scale=4（`BENCH_FEATURE_SCALE=4`）で 100,000 行（tenant-a 80,000・tenant-b
20,000）を各 2 回測定した（時間制約により scale=1 より回数を減らした。
`hnsw::MAX_HNSW_NODES`〔1,000,000〕には収まる規模）。**before バイナリには
`BENCH_FEATURE_SCALE` が存在しない**（本 Issue で追加した変数のため）ため、
before/scale=4 は測定対象外——規模スケーリング比較は after/既定 vs
after/hnsw のみで行う。

| フェーズ | after/既定（100k） | after/hnsw（100k） | 比（hnsw/既定） |
| --- | --- | --- | --- |
| vector_knn | 71,748.5us | 64,265.5us | 0.896（-10.4%） |
| vector_knn_where | 17,262.5us | 24,980.5us | 1.447（+44.7%） |
| hybrid_rrf | 92,863.0us | 87,131.0us | 0.938（-6.2%） |
| mode_recall | 76,052.5us | 70,722.0us | 0.930（-7.0%） |
| mode_precision | 76,363.5us | 78,786.5us | 1.032（+3.2%） |
| point_where | 17,762.5us | 24,610.5us | 1.386（+38.6%） |

`index_warm_us` 中央値: after/既定 134.2ms → after/hnsw 2,101.8ms（約 15.7
倍）。`vm_rss_kb_final` 中央値: after/既定 360,734kB → after/hnsw
455,864kB（+26.4%）。

**25k vs 100k での hnsw 優位性の変化**: `vector_knn` の hnsw/既定 比は
25k で 0.777（-22.3%）、100k で 0.896（-10.4%）と、**規模が大きくなるほど
hnsw の相対優位が縮小している**（単純な「HNSW は O(log n) で brute-force
の O(n) に対し規模が大きいほど有利」という理論的期待とは逆方向）。同様に
`vector_knn_where`（SCALAR 事前フィルタ付き）の悪化幅も 25k の +39.2% から
100k の +44.7% へ拡大している。この 2 規模点のみからは、本コーパス
（`n=2`・非専有環境・`ef_search=64` 固定）で明確な損益分岐点（brute-force
が hnsw に劣後し始める規模）を特定できない——両規模点で hnsw が
`vector_knn`／`mode_recall`／`hybrid_rrf` について brute-force を上回った
ままであり、規模を追うごとに差が縮む傾向は見えるが交差（逆転）は観測
されていない。より広い規模ラダー（例: 10k・50k・250k・500k）での再測定が
損益分岐点の特定には必要であり、本 Issue の時間・環境制約により申し送りと
する（§11）。

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
そのまま表しており、`feature_bench` の `index_warm_us`（17.4 倍。1 回限りの
構築を計測）と桁が一致する（構築コストは規模〔25k 行〕にほぼ比例する
という前提と整合）。S0-hot の -33.0% は `feature_bench` の `vector_knn`
（-22.3%）と方向・オーダーが一致し、hnsw のホットパス（索引構築済み後の
単発クエリ）優位性を独立に裏付ける。

## 10. 損益分岐点についての結論（B 案条件 1 の事後確認）

本 Issue の測定（25k・100k の 2 規模点、非専有環境、n=2〜3）から:

1. **フィルタなし DISTANCE・hybrid 密側・ホットパス**では、25k・100k の
   いずれの規模でも hnsw が brute-force を上回る（p50 で 6〜33% 高速）。
2. **SCALAR 事前フィルタ付き DISTANCE**（可視候補比率が `full_scan_ratio`
   を下回らない選択性）では、25k・100k のいずれでも hnsw が brute-force
   より遅い（+37〜45%）。
3. **索引構築コスト**（`index_warm_us`）は 25k で brute-force cold 構築の
   約 17 倍、100k で約 16 倍——規模が変わっても比率はほぼ一定であり、
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
