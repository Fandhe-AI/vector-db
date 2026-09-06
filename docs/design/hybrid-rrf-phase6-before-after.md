# hybrid_rrf Phase 6 通し前後比較（feature_bench・bench-hybrid-profile・crossdb・Recall 3 ゲート）

- **ステータス**: Recorded（記録専用・参考値。共有 QEMU 環境での計測のため
  `docs/design/benchmark-judgement-policy.md` §5 により Accepted/Rejected の
  採否根拠にはしない）
- **対応 Issue**: #550（本書）・親 #548（Phase 6 トラッキング）
- **関連ポインタ（spec・本文は転記しない）**: SEARCH-1・SEARCH-3・TASK-104・
  TASK-108・TASK-112・TASK-113

## 1. 背景・目的

Phase 6（親 #461 → #548）で hybrid_rrf 経路に入った production 変更は次の 2 件。

| Issue | PR / commit | 変更 |
| --- | --- | --- |
| #546 | #565 / `b161d5b` | `sparse.rs::score_pass` のスコアアキュムレータを `SparseIndex` 寿命内の有界スクラッチプールへ置換 |
| #549 | #572 / `c86c683` | `hybrid.rs::rrf_fuse_with_limits` の融合コアを `BTreeMap` 累積から位置索引方式へ置換、`has_duplicate_id` の `Vec` 化、`apply_soft_boost` の整列済み時再ソート省略 |

いずれも「スコア・順序はビット同一」を契約とし、実装 PR 単体では共有計測環境の
制約により採否を確定していない。本書はこれらを **Phase 6 通しで** 計測し、
`feature_bench`・`bench-hybrid-profile`・crossdb self（wire 経由）・Recall 3
ゲート（hybrid・rerank・query-planning、`RECALL_ENGINE=brute_force`/`hnsw`）の
before/after を記録する。

## 2. 計測条件

| 状態 | commit | 内容 |
| --- | --- | --- |
| A（before・Phase 6 着手前） | `91f6a18` | #546 直前 |
| C（after） | `c86c683`（PR #572 squash commit。#549 適用後） | #546＋#549 適用 |

- 状態 B（`c636a81`。#546 適用済み・#549 未適用）は**未計測**（自動運転での
  時間配分により A→C の主比較を優先。`docs/design/benchmark-judgement-policy.md`
  が許容する縮退構成）。#549 単独の寄与は `docs/design/
  hybrid-rrf-latency-breakdown.md`「Issue #549」節の単一バイナリ内 B7 参考値を
  参照
- 状態 A・C は明示的なコミットハッシュ（`91f6a18`／`c86c683`）を `git archive`
  で個別ディレクトリへ書き出し、それぞれ独立した `CARGO_TARGET_DIR` でビルドする
  （PR #575 codex-review 指摘対応で「実装ブランチ HEAD」参照から明示コミットへ
  切替。ブランチ HEAD はレビュー対応の追加コミットで動くため before/after の
  対象を固定するには不向き）。`git diff --stat 91f6a18 c86c683 -- Cargo.lock
  Cargo.toml crates/engine/Cargo.toml crates/wire-server/Cargo.toml` は差分なしを
  確認済み
- ツールチェーン: `rustc 1.96.0`・`cargo 1.96.0`（両状態で共通の
  `rust-toolchain.toml` を使用）
- 環境: `lscpu` Model name `QEMU Virtual CPU version 2.5+`・ISA フラグ
  `avx2`／`fma`／`f16c` 検出（`avx512*`・`neon` は無し。`hybrid_profile_bench`
  実行時の自己申告 `isa=Avx2Fma` と整合）・`nproc`=12・`x86_64`・
  他 worktree（他 Issue のエージェント）多数が並行稼働・`loadavg` 概ね
  2.4〜6.7（§3〜§5 の各 run 直前に記録。外れ値回の詳細は §5・§8 参照）・
  `BENCH_DEDICATED_ENV` 未設定。**専有環境ではないため
  Accepted/Rejected の採否根拠にしない**（`docs/design/
  benchmark-judgement-policy.md` §5）
- ビルド: 状態 A・C それぞれ独立したディレクトリ・`CARGO_TARGET_DIR` で
  `cargo build --release`（`feature_bench` example・`wire-server` バイナリ・
  `hybrid_profile_bench` ベンチバイナリ〔`--features bench-internals`〕）
- **各 run の生データの保存先**: 本 doc の §3〜§5 の各表直下に「各 run の
  生データ」小節としてインライン記載する（`docs/design/ingest-write-path.md`
  の記録方式を踏襲。ベンチ実行時の一時出力先はホスト環境のスクラッチ
  ディレクトリであり本リポの追跡対象外のため、事後再判定に必要な値は
  この doc 自体に残す）

## 3. `feature_bench` 13 フェーズ（交互 5 ペア・min-of-5／median-of-5）

`./target/release/examples/feature_bench` を A→C の順で 5 ペア交互実行し
（各 run 前に `loadavg` を記録）、p50 の min-of-5・median-of-5 を集計した。

| phase | A min (µs) | C min (µs) | ratio(min) | A median | C median | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| ingest | 4093 | 4081 | 0.997 | 4129 | 4122 | 0.998 |
| point_where | 2749 | 2787 | 1.014 | 2861 | 2814 | 0.984 |
| where_compound | 2882 | 2892 | 1.003 | 2899 | 2903 | 1.001 |
| agg_count | 2449 | 2488 | 1.016 | 2521 | 2503 | 0.993 |
| agg_multi | 2663 | 2746 | 1.031 | 2708 | 2760 | 1.019 |
| group_by_having | 3100 | 3109 | 1.003 | 3117 | 3143 | 1.008 |
| vector_knn | 8589 | 8592 | 1.000 | 8796 | 8625 | 0.981 |
| vector_knn_where | 2804 | 2781 | 0.992 | 2816 | 2812 | 0.999 |
| **hybrid_rrf** | **11741** | **11100** | **0.945** | **11884** | **11218** | **0.944** |
| mode_recall | 8628 | 8696 | 1.008 | 8734 | 8761 | 1.003 |
| mode_precision | 8555 | 8613 | 1.007 | 8755 | 8729 | 0.997 |
| rls_isolation | 2357 | 2421 | 1.027 | 2392 | 2424 | 1.013 |
| udf_call | 640 | 668 | 1.044 | 694 | 701 | 1.010 |

**各 run の p50 生データ（µs、run1..5。PR #575 codex-review P1 指摘対応で
再判定用に追記）**:

| phase | A | C |
| --- | --- | --- |
| ingest | [4144, 4096, 4129, 4131, 4093] | [4121, 4124, 4140, 4122, 4081] |
| point_where | [2772, 2887, 2861, 2891, 2749] | [2794, 2814, 2896, 2787, 2837] |
| where_compound | [2882, 2889, 2902, 3000, 2899] | [2896, 2977, 2903, 2989, 2892] |
| agg_count | [2492, 2449, 2551, 2521, 2548] | [2503, 2513, 2496, 2521, 2488] |
| agg_multi | [2663, 2708, 2666, 2777, 2759] | [2805, 2746, 2928, 2760, 2749] |
| group_by_having | [3100, 3117, 3112, 3177, 3157] | [3158, 3143, 3143, 3109, 3114] |
| vector_knn | [8679, 9429, 9356, 8796, 8589] | [8625, 8592, 8957, 8604, 8813] |
| vector_knn_where | [2814, 3058, 2816, 2862, 2804] | [2812, 2781, 2809, 2906, 2823] |
| hybrid_rrf | [11741, 12085, 11821, 11935, 11884] | [11474, 11218, 11262, 11100, 11110] |
| mode_recall | [8734, 8958, 8706, 8862, 8628] | [9468, 8701, 8849, 8696, 8761] |
| mode_precision | [8793, 9214, 8597, 8755, 8555] | [8967, 8669, 8872, 8613, 8729] |
| rls_isolation | [2396, 2357, 2392, 2369, 2588] | [2421, 2424, 2929, 2491, 2424] |
| udf_call | [694, 784, 749, 692, 640] | [701, 704, 748, 685, 668] |

参照区間の実測ノイズ帯（run-to-run 幅 / min）: `vector_knn` A=9.8%・C=4.2%、
`agg_count` A=4.2%・C=1.3%。対象区間 `hybrid_rrf` の改善（min 比 0.945・
median 比 0.944）は参照区間のノイズ帯（4〜10%）と同程度かそれ未満であり、
固定 ±5% 判定では `hybrid_rrf` は境界付近（`Improved` 寄り）だが、**参照区間の
ノイズ帯を明確に超えているとは言えない**。他 12 フェーズはいずれも比
0.98〜1.04x でノイズ帯内（非退行）。

## 4. `bench-hybrid-profile` 段別（B0s〜B8。プロセス 5 回起動 × 内部 5 ラウンド）

`BENCH_HYBRID_PROFILE_ROUNDS=5` でプロセスを A→C 交互に **5 回**起動し
（PR #575 codex-review P1 指摘対応で 3→5 に補完。1 プロセス内で
`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線」節と同じ内部 5
ラウンド交互計測を実施）、各プロセスの `baseline_summary` min をそのプロセスの
「1 run」の値として扱い、5 プロセス間で min-of-5・median-of-5 を集計する。

| 段 | A min-of-5 (µs) | C min-of-5 (µs) | ratio(min) | A median-of-5 | C median-of-5 | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| B0s（密側 `CpuScalarProvider` 単線参照区間） | 315 | 316 | 1.003 | 322 | 325 | 1.009 |
| B0（密側 KNN・production provider） | 429 | 405 | 0.944 | 444 | 439 | 0.989 |
| B1（SQL 表層 hybrid） | 8898 | 6315 | 0.710 | 8946 | 6567 | 0.734 |
| B2（投影込み） | 10613 | 8049 | 0.758 | 11172 | 8156 | 0.730 |
| B3（密側対照・informational） | 639 | 643 | 1.006 | 650 | 648 | 0.997 |
| B4（融合込み対照） | 5684 | 3029 | 0.533 | 5742 | 3051 | 0.531 |
| B5（疎側再取得ループ） | 3374 | 1666 | 0.494 | 3408 | 1669 | 0.490 |
| B8（可視集合構築） | 124 | 122 | 0.984 | 126 | 123 | 0.976 |
| B7（融合コア下限近似） | （before 計測不能。B7 は #572 で新設のためハーネス非存在） | 10 | — | （before 計測不能） | 10 | — |

**各プロセスの min（µs、process1..5。PR #575 codex-review P1 指摘対応で
再判定用に追記）**:

| 段 | A | C |
| --- | --- | --- |
| B0s | [331, 347, 322, 317, 315] | [334, 342, 325, 321, 316] |
| B0 | [451, 489, 429, 434, 444] | [477, 439, 405, 461, 430] |
| B1 | [8898, 9711, 8912, 9322, 8946] | [6575, 6567, 6315, 6618, 6417] |
| B2 | [11427, 11388, 10613, 11172, 10621] | [8189, 8525, 8049, 8085, 8156] |
| B3 | [639, 661, 653, 639, 650] | [646, 745, 643, 655, 648] |
| B4 | [5772, 6025, 5684, 5742, 5689] | [3075, 3108, 3039, 3029, 3051] |
| B5 | [3411, 3418, 3408, 3374, 3407] | [1677, 1679, 1669, 1666, 1669] |
| B8 | [126, 127, 125, 124, 126] | [124, 123, 123, 122, 123] |
| B7 | （非存在） | [10, 10, 10, 10, 10] |

派生帰属（#465 と同区分。min-of-5 ベース）:

| 区分 | A (µs) | C (µs) |
| --- | --- | --- |
| sql_surface (B1−B4) | 3214 | 3286 |
| projection (B2−B1) | 1715 | 1734 |
| sparse (B5) | 3374 | 1666 |
| residual (B4−B0−B5) | 1881 | 958 |
| dense (B0) | 429 | 405 |

`sparse(B5)` が約 51%（3374→1666µs）、`residual(B4−B0−B5)` が約 49%
（1881→958µs）に減少している。`sparse(B5)` の減少は #546（スコアアキュムレータ
のスクラッチプール化）、`residual` の減少は #549（RRF 融合コアの位置索引化）に
それぞれ帰属すると考えられるが、状態 B（#546 のみ）を計測していないため
**寄与の切り分けは確定できない**（申し送り）。`sql_surface`・`projection`・
`dense`・`B8` はノイズ帯程度の変動でほぼ不変。B7（融合コア下限近似）は after
のみ計測可能で、`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #549」節の
単一バイナリ内参考値（10µs）と整合する。参照区間 B0s の実測ノイズ帯（プロセス
min の run-to-run 幅）は A=10.16%・C=8.23%（5 プロセス）。

`crates/engine/benches/harness/hybrid_profile.rs` の stage 定義を確認したところ
（PR #575 codex-review P2 指摘対応）、B0s は `visible_id` 収集ではなく
`CpuScalarProvider::search` を単線・決定的に呼ぶ密側の参照区間
（`B0s_dense_scalar_ref`）であり、上表の見出しをそれに合わせて修正した。また
出力中の `hybrid_profile: memory stage=sparse_index_resident approx_heap_bytes=...`
行は B0s stage の一部ではなく、独立した `sparse_index_resident` という別 stage
のメモリ計測であり、`approx_heap_bytes` は #546 でプール上限分が加算表示される
設計のため、この前後比較では実 RSS の増減指標として扱わない
（`docs/design/hybrid-rrf-latency-breakdown.md` の既存注記のとおり）。

## 5. crossdb self（wire 経由・交互 5 ペア・min-of-5／median-of-5）

25,000 行・dim 128 の共通 fixture（`docs25k.redb`／`queries200.jsonl`／
`docs25k.jsonl`。他セッションが作成した既存 fixture を再利用）に対し、
状態 A・C それぞれ `91f6a18`／`c86c683` を `git archive` で書き出し独立に
`wire-server` をビルドしたうえで `scripts/crossdb_bench/run.py --db self
--config exact` を A→C 交互に **5 ペア**実行した（PR #575 codex-review P1
指摘対応で 3→5 に補完。`CROSSDB_SELF_PORT` を専用ポートへ変更しポート競合を
回避）。

| phase | A min (µs) | C min (µs) | ratio(min) | A median | C median | ratio(median) |
| --- | --- | --- | --- | --- | --- | --- |
| vector_knn | 697 | 698 | 1.000 | 738 | 737 | 0.998 |
| vector_knn_where | 2866 | 2828 | 0.987 | 2983 | 2937 | 0.985 |
| point_where | 2866 | 2828 | 0.987 | 2983 | 2937 | 0.985 |
| where_compound_count | 3974 | 3970 | 0.999 | 3990 | 3986 | 0.999 |
| agg_count | 3453 | 3440 | 0.996 | 3567 | 3585 | 1.005 |
| agg_multi | 3688 | 3733 | 1.012 | 3755 | 3877 | 1.033 |
| group_by_having | 4008 | 4029 | 1.005 | 4045 | 4168 | 1.030 |
| **hybrid_rrf** | **5839** | **5903** | **1.011** | **5961** | **6270** | **1.052** |
| mode_recall | 703 | 686 | 0.975 | 713 | 750 | 1.053 |
| mode_precision | 649 | 671 | 1.033 | 688 | 682 | 0.991 |
| bulk_knn_k200 | 7765 | 7767 | 1.000 | 7888 | 9042 | 1.146 |
| bulk_knn_k1000 | 11259 | 11356 | 1.009 | 11341 | 12545 | 1.106 |
| bulk_knn_where_k200 | 4593 | 4694 | 1.022 | 4730 | 4762 | 1.007 |
| bulk_hybrid_k200 | 9439 | 9311 | 0.986 | 9504 | 9507 | 1.000 |
| rls_isolation | 3354 | 3421 | 1.020 | 3551 | 3566 | 1.004 |
| udf_call | 701 | 744 | 1.061 | 748 | 765 | 1.022 |

**各 pair の p50 生データ（µs、pair1..5。PR #575 codex-review P1 指摘対応で
再判定用に追記）**:

| phase | A | C |
| --- | --- | --- |
| vector_knn | [754, 704, 738, 697, 835] | [714, 757, 698, 737, 743] |
| vector_knn_where | [3061, 2983, 2910, 2866, 3416] | [2922, 2937, 2970, 2828, 3162] |
| point_where | [3061, 2983, 2910, 2866, 3416] | [2922, 2937, 2970, 2828, 3162] |
| where_compound_count | [3979, 3974, 3990, 4001, 5257] | [3985, 3970, 3986, 4013, 4449] |
| agg_count | [3563, 3567, 3577, 3453, 4720] | [3585, 3503, 3440, 3699, 3939] |
| agg_multi | [3751, 3755, 3688, 3761, 4872] | [3877, 3733, 3836, 4151, 4145] |
| group_by_having | [4047, 4043, 4008, 4045, 5083] | [4051, 4362, 4029, 4168, 4363] |
| hybrid_rrf | [5951, 5981, 5961, 5839, 19740] | [6270, 6852, 5949, 5903, 6629] |
| mode_recall | [713, 703, 744, 711, 4039] | [750, 899, 690, 686, 757] |
| mode_precision | [668, 649, 735, 688, 5866] | [702, 699, 682, 682, 671] |
| bulk_knn_k200 | [7888, 7765, 7777, 7920, 25121] | [9042, 9228, 7767, 7882, 9867] |
| bulk_knn_k1000 | [11340, 11259, 11341, 11586, 13160] | [12789, 12545, 11356, 11401, 15668] |
| bulk_knn_where_k200 | [4753, 4736, 4716, 4730, 4593] | [5207, 4738, 4762, 4694, 10763] |
| bulk_hybrid_k200 | [9485, 9504, 9522, 9603, 9439] | [10747, 9311, 9507, 9428, 23412] |
| rls_isolation | [3661, 3602, 3551, 3380, 3354] | [3479, 3421, 3566, 3599, 3735] |
| udf_call | [847, 748, 751, 744, 701] | [823, 744, 757, 1135, 765] |

A の pair5 は全フェーズが一斉に大きく跳ねている（例: `hybrid_rrf` 19,740µs・
`bulk_knn_k200` 25,121µs）。`loadavg` は当該 run 直前で 2.99（他 pair と同水準）
であり、`loadavg` に現れない瞬間的な CPU steal・他 worktree のビルド等、
共有 QEMU 環境側の一過性の負荷スパイクによるものと考えられる（本 doc は
参考値の位置づけのためこの run を除外せずそのまま記録し、min-of-5・
median-of-5 で頑健化する）。

参照区間 `vector_knn` の実測ノイズ帯: A=19.7%・C=8.6%（A は上記 pair5 の
外れ値を含むため、3 ペア時点の 7.1% から拡大）。wire 経由の `hybrid_rrf` は
min 比 1.011・median 比 1.052 であり、5 ペアへの補完後も概ね 1 付近
（ノイズ帯内〜境界）にとどまる。engine 内部（§4）で観測された
`sparse`/`residual` の削減が wire レベルの p50 にはほぼ現れていない。これは
`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線」節が示す wire 内訳
（SQL 表層 T2−T1p が最大区分・67.1%）と整合し、engine 内 hybrid 経路
（T1p・28.6%）の改善分が SQL 表層・wire 側の固定コストに対して相対的に小さい
ため、と考えられる。self の `hybrid_rrf`（min 5,839〜5,903µs・median
5,961〜6,270µs）は `docs/design/crossdb-bench.md` 記載のスナップショット値
（`559b523` 時点 6,178µs）と近い水準にあり、大きな環境差は無い。

## 6. Recall 3 ゲート層 B（`RECALL_ENGINE=brute_force`／`hnsw` × A／C）

`(0.0,1.0]` 内のプレースホルダ閾値（`0.001`）＋`RECALL_VERBOSE=1` を注入し、
`cargo test --release -p engine --test <name> -- --ignored --nocapture` を
状態 × エンジンの 4 通りで実行した（決定的ハーネスのため 1 回で十分。
プレースホルダは spec 閾値ではなく pass/fail は記録しない）。

| 指標 | A brute_force | A hnsw | C brute_force | C hnsw |
| --- | --- | --- | --- | --- |
| hybrid recall@20（小規模） | 0.9010 | 0.9010 | 0.9010 | 0.9010 |
| hybrid recall@20（大規模） | 0.9145 | 0.9145 | 0.9145 | 0.9145 |
| hybrid recall@100（大規模） | 0.9165 | 0.9165 | 0.9165 | 0.9165 |
| rerank after_recall@20 | 0.9488 | 0.9488 | 0.9488 | 0.9488 |
| rerank non_degraded | true | true | true | true |
| rerank hits20 (baseline/after/ceiling) | 387/389/396 | 387/389/396 | 387/389/396 | 387/389/396 |
| rerank improvement_ratio（informational） | 0.2222 | 0.2222 | 0.2222 | 0.2222 |
| query-planning intent_improvement | 0.9245 | 0.9245 | 0.9245 | 0.9245 |
| query-planning direct_after_recall20 | 0.9321 | 0.9321 | 0.9321 | 0.9321 |
| query-planning intent_improvement_degraded | 0.3547 | 0.3547 | 0.3547 | 0.3547 |
| query-planning direct_after_recall20（大規模） | 0.8852 | 0.8852 | 0.8852 | 0.8852 |

**全 10 指標 × 2 エンジンで A/C の差分がゼロ**。ただし Recall ハーネスは
正解 ID の hit 数（recall@k・improvement_ratio 等の集計指標）を比較するのみで
Top-k 内の順位入れ替え・スコアの微小なビット差はこの指標には現れない
（PR #575 codex-review P2 指摘対応でこの限界を明記）。よってこの差分ゼロが
直接裏付けるのは「A/C 間で集計指標（recall@k 等）が非退行」という事実であり、
「スコア・順序がビット同一」という Issue #546・#549 のより強い契約は、
`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」「Issue #549」節に
記載の機械検証（束縛済み式・融合コアの単体テストによる順序付き ID・スコア列の
直接比較）が根拠であり、本書の Recall 一致はそれを補強する状況証拠
（集計指標の非退行）にとどめる。12 run すべて `test result: ok`（`0 failed`）。

層 A（`cargo test --release -p engine --test hybrid_recall --test
rerank_recall --test query_planning_recall --test sparse_determinism`。状態 C）
は全件 green（23 テスト・0 failed）。

## 7. 判定

- **共有 QEMU 環境の計測のため、本書の数値は参考値であり Accepted/Rejected の
  採否根拠にしない**（`docs/design/benchmark-judgement-policy.md` §5・§7.1）
- Recall 3 ゲート（brute_force／hnsw）は A/C で集計指標（recall@k・
  improvement_ratio 等）が完全に一致し、実 hybrid 経路
  （`RECALL_ENGINE=hnsw` の ANN opt-in 経路を含む）で非退行であることを
  確認した。これは非退行の直接証拠であり、共有環境かどうかに依らない結論
  （決定的ハーネスの差分比較）。ただしこのハーネスは hit 数の集計比較であり
  Top-k の順位・スコアのビット列そのものを比較しないため、#546・#549 の
  「スコア・順序はビット同一」契約自体の裏付けは
  `docs/design/hybrid-rrf-latency-breakdown.md` 記載の機械検証（単体テスト）
  が担う
- `feature_bench` の `hybrid_rrf` は参照区間ノイズ帯と同程度の改善
  （比 0.944〜0.945）にとどまり、この計測だけでは「改善した」と断定しない
- `bench-hybrid-profile` の段別内訳では `sparse(B5)`・`residual(B4−B0−B5)` が
  大きく減少（約半分）しており、engine 内部の狙った経路では改善方向の変化が
  一貫して観測された。ただし専有環境での確定が必要
- crossdb self（wire 経由）の `hybrid_rrf` は min 比 1.011・median 比 1.052
  （5 ペアへの補完後。§5 参照区間ノイズ帯 A=19.7%・C=8.6% と同程度〜それ未満）
  で、engine 内部の改善が wire レベルの p50 にはほぼ現れていない。SQL 表層・
  wire の固定コストが支配的なため（`hybrid-rrf-latency-breakdown.md` の内訳と
  整合）
- 一貫した悪化（両ノイズ帯を超える）は観測されなかった（§5 の A pair5 外れ値は
  全フェーズ一斉のスパイクであり対象区間固有の退行ではない）。差し戻し候補
  には該当しない

## 8. 限界・申し送り

- #503（hybrid 密側再取得の再開型探索。sub-issue #504〜#506）は本計測時点で
  **未適用**（open）。#503 マージ後の再実測は別途（#507 の Phase 3 通し比較と
  合わせるのが自然）
- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測・Accepted/Rejected の確定は
  オーナー／運用者作業として申し送る
- 状態 B（`c636a81`。#546 単独）は未計測。§4 の `sparse`/`residual` 削減の
  #546/#549 間の寄与切り分けは状態 B の計測を要する
- B7（融合コア下限近似）の before 計測はハーネス非存在のため不能
- 他 DB（pgvector・Qdrant 等）の再計測は行っていない（`crossdb-bench.md` の
  `559b523` 時点値を目標値として参照）
- `feature_bench`／`bench-*` への交互ペア自動集計の組み込みは別 Issue 候補
  （`benchmark-judgement-policy.md` §9）
- `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #547」節（可視率別
  前後比較。担当 #547）とは別観点であり、本書は Phase 6 の通し比較に限定する
- PR #575 codex-review P1 指摘対応（本書 §3〜§5）で §4・§5 のペア数を
  3→5 に補完し、各 run の生データを §3〜§5 にインライン追記した。ただし
  §4・§5 の再測定は初回計測（同一 QEMU 環境・別セッション）とは別の作業
  セッションで実施したため、厳密な意味での「同一計測セッション」内の
  交互比較ではない（§4 の pair1〜3・§5 の pair1〜3 は初回セッション、
  pair4〜5 は本セッションの追加測定。ビルド条件〔同一コミット・同一
  `Cargo.lock`・同一プロファイル〕は統一しているが、セッション間で
  ホストの背景負荷が変化し得る点は申し送る）

## 9. 再現手順

```bash
# 状態 A・C を明示コミットから git archive で個別ディレクトリへ書き出し
# （ブランチ HEAD は追加コミットで動くため before/after の対象を固定するのに使わない）
mkdir -p <dir-a> <dir-c>
git archive 91f6a18 | tar -x -C <dir-a>
git archive c86c683 | tar -x -C <dir-c>

# 状態 A（before）をビルド
( cd <dir-a> && CARGO_TARGET_DIR=<dir-a>/target cargo build --release -p engine --example feature_bench --bench hybrid_profile_bench --features bench-internals -p wire-server --bin wire-server )

# 状態 C（after）をビルド
( cd <dir-c> && CARGO_TARGET_DIR=<dir-c>/target cargo build --release -p engine --example feature_bench --bench hybrid_profile_bench --features bench-internals -p wire-server --bin wire-server )

# feature_bench 交互 5 ペア（p50 min-of-5・median-of-5 を集計）
for n in 1 2 3 4 5; do
  <dir-a>/target/release/examples/feature_bench > A-$n.json
  <dir-c>/target/release/examples/feature_bench > C-$n.json
done

# bench-hybrid-profile はプロセスを A→C 交互に 5 回起動（BENCH_HYBRID_PROFILE_ROUNDS=5）
export BENCH_HYBRID_PROFILE_ROUNDS=5
for n in 1 2 3 4 5; do
  <dir-a>/target/release/deps/hybrid_profile_bench-* --bench > A-$n.out
  <dir-c>/target/release/deps/hybrid_profile_bench-* --bench > C-$n.out
done

# crossdb self は scripts/crossdb_bench/run.py --db self --config exact を A→C 交互に 5 ペア実行
# （run.py は <repo_root>/target/release/wire-server を既定で参照するため、
#   <dir-a>/target・<dir-c>/target 配下にそのパスでバイナリを配置してから実行する）

# Recall 3 ゲート（プレースホルダ閾値・RECALL_VERBOSE=1・RECALL_ENGINE=brute_force|hnsw）
RECALL_VERBOSE=1 RECALL_ENGINE=brute_force \
  HYBRID_RECALL_MIN_R20_SMALL=0.001 HYBRID_RECALL_MIN_R20_LARGE=0.001 HYBRID_RECALL_MIN_R100_LARGE=0.001 \
  cargo test --release -p engine --test hybrid_recall -- --ignored --nocapture
RECALL_VERBOSE=1 RECALL_ENGINE=brute_force RERANK_RECALL_MIN_R20_LARGE=0.001 \
  cargo test --release -p engine --test rerank_recall -- --ignored --nocapture
RECALL_VERBOSE=1 RECALL_ENGINE=brute_force \
  QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT=0.001 QUERY_PLANNING_RECALL_MIN_R20_DIRECT=0.001 \
  QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED=0.001 QUERY_PLANNING_RECALL_MIN_R20_DIRECT_LARGE=0.001 \
  cargo test --release -p engine --test query_planning_recall -- --ignored --nocapture
# RECALL_ENGINE=hnsw に差し替えて同様に実行
```

## 10. 参考

- `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」「Issue #549」
  「最新基線」節
- `docs/design/crossdb-bench.md`「`hybrid_rrf` 6,178µs の内訳（Issue #465）」節
- `docs/design/benchmark-judgement-policy.md`（計測規約 SSOT）
