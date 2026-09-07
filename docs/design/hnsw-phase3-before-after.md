# Phase 3（#458 ツリー）通し前後比較

- **ステータス**: Recorded（記録専用・参考値。共有 QEMU 環境の数値は採否根拠に
  しない。`docs/design/benchmark-judgement-policy.md`〔Issue #462〕準拠）
- **対応 Issue**: #507（本書）・Phase 3 親: #458／ルート: #455
- **関連ポインタ（spec・本文は転記しない）**: `docs/spec/05-tasks.md` の
  TASK-132・CORE-9・CORE-10 系（`docs/design/ann-index-adoption.md` ADR
  ポインタ）

## 1. 背景・変更一覧

用語注意: `docs/design/hnsw-index.md` §1 は #404〜#412（#402 ツリー）を
「Phase 3」と呼んでいる。本書の「Phase 3」は #455 ルート配下の **#458 ツリー**
（ANN／HNSW の構築並列化・探索メモリ局所性・フィルタ付き探索）を指す。

Phase 3（#458 ツリー）の production 変更は各実装 PR に付随する変更単位の局所
比較（下表「既存の局所比較」列）しか持たない。本書は Phase 5
（`docs/design/gpu-batch-phase5-before-after.md`・#544）・Phase 6
（`docs/design/hybrid-rrf-phase6-before-after.md`・#575）と同型の「通し前後
比較 doc」を Phase 3 について作る。

| Issue | 実装 PR / commit | 変更 | 既存の局所比較 |
| --- | --- | --- | --- |
| #489（#490） | #574 / `eabff3a` | `search_layer` の受理判定後 prefetch | #491（`hnsw-search.md`「Issue #491」節。保留・production 無変更） |
| #492（#494） | #585 / `cadf6c3` | 凍結時 CSR 平坦化・`search_layer` の CSR 参照 | #495（`hnsw-index.md` §14.13・`hnsw-parallel-build.md`） |
| #486（#488） | #606 / `2dcade0` | per-query 写像コスト削減（早期打ち切り・BFS 遅延）。`full_scan_ratio` 既定は 1/10 のまま | `hnsw-rls-cardinality-switch.md`「Issue #488」節 |
| #496（#497） | #608 / `3fc7e7c` | `VisitedSparse`（HashSet）と `sparse_visited_max` 切替（既定 0＝常に dense） | #498（`hnsw-search.md`「Issue #498」節。既定値 0 暫定維持） |
| #499（#501） | #616 / `25ba6db` | ACORN-1 2-hop 展開（`acorn_max_visible_ratio` 既定 `None`＝無効） | #502（`hnsw-rls-cardinality-switch.md`「Issue #502」節） |
| #503（#505） | #619 / `4ceb6b5` | `HnswDenseProvider` 破棄候補ヒープ保持の再開型探索 | #506（`hnsw-hybrid-iterative-scan.md`「前後比較実測（Issue #506）」節） |

補助（#446 ツリー・同じ #458 配下だが本 Issue の依存には非列挙。構築時間に
影響するため §2 の交絡表へ含める）: #594〔`cbf1d34`〕・#596〔`6afff03`〕・#600
〔`7382b35`〕（`repair_reachability` 観測・上位層リンク保証・並列化）。

## 2. 計測条件

### 2.1 状態の固定

| 状態 | commit | 根拠 |
| --- | --- | --- |
| before | `4d2bd23`（`perf(wire): DataRow 群を…(#573)`） | #458 ツリー最初の production マージ `eabff3a`（PR #574）の親 |
| after | `6184491`（`test(engine): dim 768／1536 での前後比較と閾値の確定 (#614)`。実装着手時点の `origin/main`） | 全 Phase 3 実装 PR（#574・#585・#606・#608・#616・#619）を含む |

実装着手時点で `origin/main` は計画作成時点（`799a7d8`）からさらに 1 コミット
（#614）進んでいたため、計画の再確認手順（本節・§2.2）に従い `6184491` を
after として再固定した。

### 2.2 ビルド条件の同一性

```
$ git diff --stat 4d2bd23 6184491 -- Cargo.lock Cargo.toml rust-toolchain.toml crates/engine/Cargo.toml crates/wire-server/Cargo.toml deny.toml
 crates/engine/Cargo.toml      | 17 +++++++++++++++++
 crates/wire-server/Cargo.toml | 10 ++++++++++
 2 files changed, 27 insertions(+)
```

`Cargo.lock` は不変。追加分はいずれも `[[bench]]` エントリ（`hnsw_search_bench`・
`ingest_wire_profile_bench`）の追記のみで、依存追加・バージョン変更はない。

参照フェーズ候補（`feature_bench` の非退行対照）として想定していた `udf_call`
フェーズの実装ファイルも不変を確認済み:

```
$ git log 4d2bd23..6184491 -- crates/engine/src/sql/udf_call.rs crates/engine/src/sql/expr_program.rs
（空。コミットなし）
```

### 2.3 交絡の明示（必須）

`4d2bd23..6184491` は Phase 3 のみの差分ではない。非 Phase 3 の production 変更
（PR 番号付き）:

| 領域 | 代表 PR | 影響 |
| --- | --- | --- |
| HNSW f16 常駐（Issue #514） | `#595` 系 | `hnsw` opt-in 経路の候補生成カーネルに影響しうる（本書の対象は F32 常駐 `hnsw` のみのため直接の対象外だが、`hnsw.rs`・`hnsw_cache.rs` の共有コード経由の可能性を排除しない） |
| HNSW SQ8（i8）常駐・整数カーネル（Issue #521・#522・#525） | `#617`・`#620`・`#623` | 同上 |
| スカラー二次索引の集計・GROUP BY 結線（Issue #473〜#475） | `#569`・`#601`・`#603` | `feature_bench` の `where_compound`／`group_by_having`／`point_where` フェーズに影響（参照フェーズとして使わない） |
| `search_range` 行ブロックカーネルの AVX2/AVX-512/NEON 版（Issue #510・#511） | `#593`・`#597` | brute-force 経路の距離計算に影響 |
| dot カーネル ACC=4 ディスパッチ（`isa.rs::DOT_MULTI_ACC_MIN_DIM=768`） | `#613` | dim=128 の計測では非発火（閾値未満） |
| 投影の遅延デコード（Issue #453） | `#563` | `feature_bench` のスカラー列投影フェーズに影響 |
| `VisibleBitmapCache`（Issue #478） | `#583` | `agg_count`／`rls_isolation` フェーズに影響（参照フェーズとして使わない） |
| #446 ツリー（`repair_reachability` 並列化） | `#594`・`#596`・`#600` | Phase 3（#458）の一部でもあり、HNSW 構築時間（`bench-hnsw-compare` の self build）に寄与 |

**帰結**: `brute_force`（S0-hot／`feature_bench` 既定エンジン）は行ブロック
カーネル（#593・#597）・SQL 表層変更（#563・#583・#601・#603）により
「変更を含まない参照区間」ではない。ノイズ帯は `docs/design/
gpu-batch-phase5-before-after.md` §2「交絡の明示」と同じ **side 別方式**
（before のみ N 件・after のみ N 件の run-to-run 幅を両方併記し大きい方で
判定）を使う。`usearch`（`bench-hnsw-compare`）は外部エンジン・`Cargo.lock`
不変のため、こちらは pooled 参照区間として扱う。

**交絡免疫の主指標**: 各 side の**同一バイナリ内**比 `hnsw ÷ brute_force`
（`hnsw-index.md` §7／§9 と同じ考え方）を主表の列として持ち、その比の
before→after 変化（比の比）を併記する。交絡が両エンジンへ同程度に効くなら
この比は変化しにくい。

### 2.4 既定パラメータで実際に通る経路（非 vacuity 表）

| 変更 | 既定ベンチで通るか | 根拠・代替 |
| --- | --- | --- |
| #489 prefetch | 通る（全 HNSW 探索） | — |
| #492 CSR | 通る（全 HNSW 探索・構築 `flatten`） | — |
| #486 per-query 写像 | `Subset` 形状のみ（`feature_bench` の `vector_knn_where`）。`full_scan_ratio=1/10` 不変 | `hnsw_stats.subset_searches>0` で確認（下記実測参照） |
| #496 `VisitedSparse` | **通らない**（既定 `sparse_visited_max=0`） | #498 の実測を引用。再計測しない |
| #499 ACORN-1 | **通らない**（既定 `None`） | #502 の実測を引用。再計測しない |
| #503 再開型探索 | hybrid 密側が複数ラウンド発火した場合のみ。`hnsw_stats` JSON は `hybrid_resumed_rounds` を露出しないため通し計測で発火を直接確認できない | #506 の実測（`hnsw_410shape_tie2` 約 19% 改善・`hnsw_large_uniform` 約 8.6% 悪化）を引用 |

**実測での訂正（本 Issue で判明）**:

- `#486 per-query 写像`: `feature_bench` の `vector_knn_where` フェーズで
  `hnsw_stats.subset_searches` を確認したところ**常に 0**（`hits=223
  misses=57 fallbacks=112 plain_scans=0 subset_searches=0`。§6 実測時点）
  だった。本フィクスチャ（`vector_knn_where` の `WHERE` 述語）は `Subset`
  形状（SCALAR 事前フィルタ付き DISTANCE）に到達せず、#486 の per-query
  写像コスト削減は本通し計測では発火していない。既存の局所比較（#488）を
  引用するに留める
- `#503 再開型探索`: Recall ゲート層 B（§7）の `query_planning_recall`
  large-scale テストで after 側のみ `hybrid_resumed_rounds` カウンタ
  （236〜598・4 測定点）が露出し、**複数ラウンドの再開型探索が実際に発火した
  ことを直接確認できた**（before 側バイナリにはこのカウンタ自体が存在しない
  ため比較不能）。`feature_bench` の `hnsw_stats` JSON には
  `hybrid_resumed_rounds` フィールドがなく（after 側ですら露出しない）、
  §6 の `hybrid_rrf`・`mode_recall`・`mode_precision` フェーズで発火したか
  どうかは直接確認できないままである

## 3. 環境

`bench-knn-profile`（makeup run。real disk 上の TMPDIR。§8 参照）:

```
timestamp_utc=20260907T202344Z
before_commit=4d2bd23f1f4f13f64ccbaf807e09b352ec6e31a2
after_commit=6184491389ca02d176e3518761525193520bd6c2
ab_pairs=5
nproc=12
cpu_model=QEMU Virtual CPU version 2.5+
cpu_flags_subset=fma f16c avx2
bench_dedicated_env=unset
```

`bench-hnsw-compare`・`feature_bench`（本番 run。tmpfs 上の TMPDIR。§8
参照）: 同一 CPU・同一 nproc・`timestamp_utc=20260907T200900Z`。

**同時実行プロセス（重要な限界）**: 計測中、本セッションとは別に同一ホスト
上で並行実行中の別セッション（`/home/fandhe/scratch-530` 配下で
`knn_profile_bench`・`chip_bench` 等の cargo bench を実行）が確認された。
`bench-hnsw-compare` の pair1〜3 実行中は `loadavg` が 15〜28（`nproc=12`
に対し最大 2.4 倍のオーバーサブスクリプション）に達し、pair4 以降は
その別セッションの終了とともに `loadavg` が 8〜10 まで下がった。各 run の
`# loadavg_before_run=` 行を §4〜§6 の生データに残す。**本書の数値は
非専有・重度に混雑した共有環境での参考値であり、通常の共有 QEMU 環境の
ノイズより大きい変動を含む**。

## 4. `bench-knn-profile`（S0-cold／S0-hot・25,000 行・dim=128）

makeup run（5 ペア・real disk TMPDIR・`GITHUB_ACTIONS` 非該当・全 20 run
成功・クオータ超過なし）の実測。`stage(S0_hot_sql_e2e)`（ホットパス・SQL
表層 e2e）・`stage(S0_cold_sql_e2e)`（コールドパス。`hnsw` 側は索引構築を
含む）。

| 段 | エンジン | before min/median (ms) | after min/median (ms) | 同一バイナリ内比 hnsw/bf（before→after） |
| --- | --- | --- | --- | --- |
| S0_hot_sql_e2e | brute_force | 3.390 / 3.994 | 3.210 / 4.419 | — |
| S0_hot_sql_e2e | hnsw | 0.455 / 0.590 | 0.452 / 0.550 | 0.148 → 0.124（比の比 0.84） |
| S0_cold_sql_e2e | brute_force | 28.151 / 35.713 | 33.370 / 44.632 | — |
| S0_cold_sql_e2e | hnsw | 1226.532 / 1285.723 | 815.261 / 1359.947 | 36.0 → 30.5（比の比 0.85） |

`S0prime_count_star`（参照区間・COUNT(\*)。Issue #478
`VisibleBitmapCache` の非 Phase-3 変更を含むため純粋な参照区間ではない）:
brute_force median 2.328ms → 0.056ms（約 42 倍高速化。`agg_count` 系と同じ
`VisibleBitmapCache` 起因と考えられ、Phase 3 の効果ではない）。

生データ（5 ペア分の値）は
`docs/design/bench-data/hnsw-phase3-ab/20260907T200900Z-summary.txt`（§1 の
knn_profile セクション。makeup run の生ログは `target/bench-hnsw-phase3-ab-
knnretry/knn_profile/`。環境は `20260907T202344Z-knnretry-env.txt` 参照）。

## 5. `bench-hnsw-compare`（100,000 点・dim=64・12 threads）

本番 run（5 ペア。全 10 run 成功。`loadavg` 変動が大きい区間を含む——§3
参照）。self（自作 HNSW）・usearch 対照の構築時間・Recall@10・探索
レイテンシ。

| 指標 | before min/median | after min/median | 比較 |
| --- | --- | --- | --- |
| self build (ms) | 1892.6 / 1939.0 | 1281.0 / 1368.5 | median 比 0.71（改善方向。ただし §3 のオーバーサブスクリプション区間〔pair2・3〕の値〔3571・4644ms〕を含む） |
| usearch build (ms) | 1612.3 / 2102.4 | 1634.9 / 1654.6 | median 比 0.79（同上。usearch も同じ混雑区間の影響を受けている） |
| self/usearch 比 | 0.845 / 1.176 | 0.310 / 0.784 | 両 side とも変動が大きく（min-max 幅 0.85〜1.70 vs 0.31〜0.90）、片側性の結論は出せない |
| recall@10 self (t=12) | 0.4895 / 0.4970 | 0.4870 / 0.4955 | 実質不変（ノイズ帯内） |
| recall@10 usearch | 0.4860 / 0.4870 | 0.4825 / 0.4905 | 実質不変（ノイズ帯内） |
| search latency self (µs) | 71.7 / 73.6 | 64.4 / 77.7 | ノイズ帯内 |
| search latency usearch (µs) | 79.1 / 84.7 | 79.6 / 80.4 | ノイズ帯内 |

build 系（self・usearch とも）は before 側の pair2・3（`loadavg` 20〜29）で
外れ値（3571〜4644ms）を持ち、この区間の中央値は環境ノイズの影響を強く
受けている。min-of-N（オーバーサブスクリプションの影響が最も小さい値）で
見ると self build は 1892.6ms→1281.0ms（比 0.68）で改善方向の所見がある
が、専有環境での再実測なしに Phase 3（構築並列化系 #446 ツリーを含む）の
効果と断定はしない。

## 6. `feature_bench`（scale=1・25,000 行。本番 run・5 ペア・全 20 run 成功）

`BENCH_FEATURE_ENGINE` 未設定（既定・brute_force 相当）と `hnsw` の 2
経路を同一バイナリ内で計測。p50（µs・min-of-5／median-of-5）。

| フェーズ | before bf | before hnsw | after bf | after hnsw | 比 hnsw/bf（before→after。比の比） |
| --- | --- | --- | --- | --- | --- |
| vector_knn | 8675 / 8841 | 8061 / 8076 | 648 / 660 | 394 / 404 | 0.913 → 0.612（0.67） |
| vector_knn_where | 2780 / 2805 | 4210 / 4280 | 2059 / 2209 | 3122 / 3195 | 1.526 → 1.447（0.95。#486 は非発火——§2.4 参照） |
| hybrid_rrf | 11067 / 11081 | 9925 / 10040 | 11652 / 11756 | 10169 / 10321 | 0.906 → 0.878（0.97） |
| mode_recall | 8674 / 8835 | 7639 / 7650 | 647 / 665 | 398 / 400 | 0.866 → 0.602（0.70） |
| mode_precision | 8718 / 8746 | 8280 / 8356 | 638 / 653 | 623 / 638 | 0.955 → 0.977（1.02。ノイズ帯内） |
| udf_call（参照区間。ソースファイル不変を§2.2 で確認済み） | 660 / 665 | 401 / 409 | 647 / 659 | 407 / 408 | — |

`vector_knn`／`mode_recall`（フィルタなし DISTANCE 経路）は before 側で
hnsw/bf 比 0.87〜0.91（HNSW がやや高速）だったのに対し after 側では
0.60〜0.61 まで下がり、同一バイナリ内比の変化（比の比 0.67〜0.70）は
`udf_call`（不変ソース）の run-to-run 変動から見積れる参照区間ノイズ帯
（後述）を上回る。ただし before/after 間の絶対値そのもの（bf: 8841µs→
660µs、hnsw: 8076µs→404µs）は非 Phase-3 の広範な変更（投影遅延デコード #563・
スカラー索引・行ブロックカーネル等）の影響が支配的で、Phase 3 固有の変化と
して断定できるのは「相対比が変わった」という所見までである。
`mode_precision`（`USING MODE('precision')` 相当）はほぼ変化なし（比の比
1.02）。`hybrid_rrf` も比の比 0.97 でノイズ帯内。

参照区間ノイズ帯（`udf_call`。ソース不変・side 別 min-max 幅 ÷ median）:
before side 660〜676µs（幅 2.4%）、after side 647〜702µs（幅 8.3%）。
`vector_knn`／`mode_recall` の比の比変化（30〜33%）は両 side の参照区間帯
（最大 8.3%）を明確に超える。

`agg_count`／`rls_isolation`／`where_compound`／`point_where`／
`group_by_having` は非 Phase-3 変更（`VisibleBitmapCache` #478・スカラー
二次索引 #473〜475）により劇的に変化（`agg_count` p50: 2848µs→54µs、
約 53 倍）しており、Phase 3 の効果ではないため参照区間・対象区間いずれに
も使わない。

`hnsw_stats` 非 vacuity（`after-hnsw` pair1。`vector_knn` 等フィルタなし
経路）: `builds=1 build_failures=0 hits=223 misses=57 fallbacks=112
plain_scans=0 subset_searches=0`。`subset_searches=0` は §2.4 の訂正の
根拠。

生データは
`docs/design/bench-data/hnsw-phase3-ab/20260907T200900Z-summary.txt`（feature_1
セクション。環境は `20260907T200900Z-env.txt` 参照）。

## 7. Recall 3 ゲート層 B（`RECALL_ENGINE=brute_force|hnsw`）

閾値は `(0.0,1.0]` 内のプレースホルダ（`0.001`）を注入（`docs/spec` 未
チェックアウトのため実閾値は不使用）。実測値・統計カウンタはオーナー判断
（2026-08-29 AGENTS.md）により記録可。

| 指標 | before bf | before hnsw | after bf | after hnsw |
| --- | --- | --- | --- | --- |
| hybrid 小規模 recall@20 | 0.9010 | 0.9010 | 0.9010 | 0.9010 |
| hybrid 大規模 recall@20 | 0.9145 | 0.9145 | 0.9145 | 0.9145 |
| hybrid 大規模 recall@100 | 0.9165 | 0.9165 | 0.9165 | 0.9165 |
| rerank 大規模 after_recall@20 | 0.9488 | 0.9488 | 0.9488 | 0.9488 |
| rerank non_degraded | true | true | true | true |
| rerank improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0.2222 | 0.2222 |
| query_planning intent_improvement | 0.9245 | 0.9245 | 0.9245 | 0.9245 |
| query_planning intent_improvement_degraded | 0.3547 | 0.3547 | 0.3547 | 0.3547 |
| query_planning direct r20（小規模） | 0.9321 | 0.9321 | 0.9321 | 0.9321 |
| query_planning direct r20（大規模） | 0.8852 | 0.8852 | 0.8852 | 0.8852 |

**全 10 測定点で brute_force／hnsw・before／after の 4 通りが完全一致**
（ビット一致ではなく表示桁での一致。Issue #412・#515・#523 と同型の測定
経路）。非 vacuity: 全 hnsw 実行で `builds=1 build_failures=0 rebuilds=0`。
`query_planning_recall_large_scale_threshold_gate`（after-hnsw）:
`hybrid_dense_searches=456 hybrid_queries=100 ef_cap_fallbacks=120
hybrid_resumed_rounds=236`（#503 再開型探索の非 vacuous な発火根拠。§2.4
参照）。`query_planning_recall_threshold_gate`（direct/intent/
intent_degraded の 3 テスト。after-hnsw）: `hybrid_resumed_rounds=
502/571/598`。

## 8. 判定

- **共有・重度に混雑した環境の計測のため、本書の数値は参考値であり
  Accepted/Rejected の採否根拠にしない**（`docs/design/
  benchmark-judgement-policy.md` §5・§7.1）。§3 のとおり同時実行の別
  セッションによるオーバーサブスクリプション区間（`loadavg` 最大 28）を
  含み、Phase 5／Phase 6 の通し比較 doc より証拠力が低い
- **Recall 3 ゲート（§7）は非退行の直接証拠**（環境ノイズに依存しない
  決定的ハーネスの差分比較）: 10 測定点すべてで brute_force/hnsw・
  before/after が完全一致し、`docs/spec` の閾値注入なしでも「Phase 3 の
  全施策適用が既存の Recall 特性を変えていない」ことを確認した。#503
  再開型探索は `hybrid_resumed_rounds` により非 vacuous に発火している
  ことも確認済み
- `bench-knn-profile`（§4）は同一バイナリ内 hnsw/brute_force 比が
  before→after で縮小する方向（比の比 0.84〜0.85）の所見があるが、
  min-of-5 は環境ノイズの影響を強く受けており専有環境での確定が必要
- `feature_bench`（§6）は `vector_knn`／`mode_recall`（フィルタなし
  DISTANCE 経路）で同一バイナリ内比の変化（比の比 0.67〜0.70）が参照区間
  ノイズ帯（`udf_call`。最大 8.3%）を明確に超えており、HNSW が
  brute_force に対し相対的に高速化する方向の所見がある。ただし before/
  after 間の絶対値そのものは非 Phase-3 の広範な変更が支配的であり、
  Phase 3 固有の寄与を分離できていない
- `bench-hnsw-compare`（§5）は build 時間の外れ値（オーバーサブスクリプ
  ション区間）により median での判定が困難。min-of-5 では self build に
  改善方向の所見（比 0.68）があるが確定的ではない
- `vector_knn_where`（#486 の対象フェーズ）は `subset_searches=0`（§2.4）
  により本通し計測では #486 の効果を観測できていない
- 本結論は §4〜§7 の測定範囲に限定する。#496（`VisitedSparse`）・#499
  （ACORN-1）は既定パラメータで非発火のため通し計測では評価できず、
  既存の局所比較（#498・#502）の記録がそのまま成立すると判断する

## 9. 限界・申し送り

- `feature_4`（scale=4・100,000 行）は時間制約により未計測
- **専有環境（`BENCH_DEDICATED_ENV=1`）での再実測が強く推奨される**——
  §3 のとおり本計測は同時実行の別セッションによる最大 2.4 倍
  オーバーサブスクリプションを含む共有環境であり、Phase 5／Phase 6 の
  通し比較 doc より証拠力が低い
- #496／#499 は既定パラメータで非発火のため、通し計測では効果を主張せず
  各局所比較 doc（#498・#502）を引用するに留める。#503 は
  `hybrid_resumed_rounds`（Recall ゲート経由）で非 vacuous な発火を確認
  できたが、`feature_bench` 側では発火有無を直接確認できない
- #486（per-query 写像）は本フィクスチャで `subset_searches=0` のため
  未検証のまま。`Subset` 形状（SCALAR 事前フィルタ付き DISTANCE）に
  実際に到達するフィクスチャでの再測定が必要
- `docs/spec` 未チェックアウトのため §7 の閾値は本書のプレースホルダ
  （`(0.0,1.0]` 内の値）を注入した結果であり、実ゲートの pass/fail 判定その
  ものはこの数値では確定しない。実測値（Recall@k・統計カウンタ）はオーナー
  判断（2026-08-29 AGENTS.md）により記録・公開可
- `bench-knn-profile` の本番 5 ペアは tmpfs（`/tmp`）のユーザークオータ
  超過（同時実行セッションとの共有）により pair3〔`after-hnsw` 1 件が
  `Aborted (core dumped)`〕・pair4・pair5（全 run）が失敗したため、
  TMPDIR を実ディスクへ切り替えた**新規 5 ペアの makeup run**（§3・§4）を
  データとして採用した（元 5 ペアの失敗ログは環境要因の証跡として保持——
  `target/bench-hnsw-phase3-ab/20260907T200900Z/knn_profile/` に残置）。
  この障害は `recovery::fail_fast`（RECOVER-8）が redb コミット時の I/O
  エラーへ想定どおり fail-fast で応答したものであり、Phase 3 の検索経路
  自体の欠陥ではない

## 10. 再現手順

```
# 状態の固定
BEFORE=4d2bd23f1f4f13f64ccbaf807e09b352ec6e31a2
AFTER=6184491389ca02d176e3518761525193520bd6c2

# before/after を独立ディレクトリへ archive（同一 checkout に CARGO_TARGET_DIR
# だけ切り替える方式は両方が同一ソースになる罠があるため不可）
git archive "$BEFORE" | tar -x -C /path/to/before
git archive "$AFTER"  | tar -x -C /path/to/after

# 各ディレクトリで分離 CARGO_TARGET_DIR を指定して release ビルド
cd /path/to/before && CARGO_TARGET_DIR=/path/to/target-before \
  cargo build --release -p engine --bench knn_profile_bench \
  --bench hnsw_compare_bench --example feature_bench --features contrast-bench
cd /path/to/after  && CARGO_TARGET_DIR=/path/to/target-after \
  cargo build --release -p engine --bench knn_profile_bench \
  --bench hnsw_compare_bench --example feature_bench --features contrast-bench

# 交互計測ドライバ（N=5 ペア）
BEFORE_KNN_BIN=/path/to/target-before/release/deps/knn_profile_bench-<hash> \
AFTER_KNN_BIN=/path/to/target-after/release/deps/knn_profile_bench-<hash> \
BEFORE_COMPARE_BIN=/path/to/target-before/release/deps/hnsw_compare_bench-<hash> \
AFTER_COMPARE_BIN=/path/to/target-after/release/deps/hnsw_compare_bench-<hash> \
BEFORE_FEATURE_BIN=/path/to/target-before/release/examples/feature_bench \
AFTER_FEATURE_BIN=/path/to/target-after/release/examples/feature_bench \
BEFORE_COMMIT="$BEFORE" AFTER_COMMIT="$AFTER" \
AB_PAIRS=5 AB_WORKLOADS="knn_profile hnsw_compare feature_1" \
scripts/bench_hnsw_phase3_ab.sh

scripts/bench_hnsw_phase3_ab.sh --summarize target/bench-hnsw-phase3-ab/<ts>

# Recall 3 ゲート層 B（各ディレクトリ内・サブシェルで cd）
cd /path/to/before && RECALL_ENGINE=brute_force RECALL_VERBOSE=1 \
  HYBRID_RECALL_MIN_R20_SMALL=0.001 HYBRID_RECALL_MIN_R20_LARGE=0.001 \
  HYBRID_RECALL_MIN_R100_LARGE=0.001 \
  cargo test --release -p engine --test hybrid_recall -- --ignored --nocapture
# ... rerank_recall / query_planning_recall も同様。RECALL_ENGINE=hnsw・
# after 側ディレクトリでも同様に実行する。
```

## 11. 参照（ポインタのみ）

- `docs/design/hnsw-index.md`（#413。Phase 3〔#402 ツリー〕導入時の前後比較）
- `docs/design/gpu-batch-phase5-before-after.md`（#544。同型の通し前後比較 doc）
- `docs/design/hybrid-rrf-phase6-before-after.md`（#575。同型の通し前後比較 doc）
- `docs/design/benchmark-judgement-policy.md`（#462。計測規約 SSOT）
- `docs/design/ann-recall-gate-verification.md`（Recall 3 ゲートの測定経路）
- 各サブ Issue の局所比較 doc（§1 表の「既存の局所比較」列）
