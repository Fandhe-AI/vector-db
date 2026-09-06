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
| C（after） | 実装ブランチ HEAD（production は `c86c683` と同一） | #546＋#549 適用 |

- 状態 B（`c636a81`。#546 適用済み・#549 未適用）は**未計測**（自動運転での
  時間配分により A→C の主比較を優先。`docs/design/benchmark-judgement-policy.md`
  が許容する縮退構成）。#549 単独の寄与は `docs/design/
  hybrid-rrf-latency-breakdown.md`「Issue #549」節の単一バイナリ内 B7 参考値を
  参照
- `git diff --stat c86c683 origin/main -- crates/` は `crates/engine/examples/
  gpu_adapter_info.rs`（新規追加・167 行）のみで、`crates/engine/src/`・
  `crates/wire-server/src/` は差分なしを確認済み。よって実装ブランチ HEAD の
  production コードは `c86c683` と同一
- ツールチェーン: `rustc 1.96.0`・`cargo 1.96.0`（両状態で共通の
  `rust-toolchain.toml` を使用。`Cargo.lock`・`Cargo.toml` は
  `91f6a18..origin/main` で無変更）
- 環境: `QEMU Virtual CPU version 2.5+` 相当・12 vCPU・`x86_64`・
  他 worktree（他 Issue のエージェント）多数が並行稼働・`loadavg` 概ね
  2.4〜3.9・`BENCH_DEDICATED_ENV` 未設定。**専有環境ではないため
  Accepted/Rejected の採否根拠にしない**（`docs/design/
  benchmark-judgement-policy.md` §5）
- ビルド: 状態 A・C それぞれ独立した worktree・`CARGO_TARGET_DIR` で
  `cargo build --release`（`feature_bench` example・`wire-server` バイナリ・
  `hybrid_profile_bench` ベンチバイナリ）

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

参照区間の実測ノイズ帯（run-to-run 幅 / min）: `vector_knn` A=9.8%・C=4.2%、
`agg_count` A=4.2%・C=1.3%。対象区間 `hybrid_rrf` の改善（min 比 0.945・
median 比 0.944）は参照区間のノイズ帯（4〜10%）と同程度かそれ未満であり、
固定 ±5% 判定では `hybrid_rrf` は境界付近（`Improved` 寄り）だが、**参照区間の
ノイズ帯を明確に超えているとは言えない**。他 12 フェーズはいずれも比
0.98〜1.04x でノイズ帯内（非退行）。

## 4. `bench-hybrid-profile` 段別（B0s〜B8。プロセス 3 回起動 × 内部 5 ラウンド）

`BENCH_HYBRID_PROFILE_ROUNDS=5` でプロセスを A→C 交互に 3 回起動し
（1 プロセス内で `docs/design/hybrid-rrf-latency-breakdown.md`「最新基線」節と
同じ内部 5 ラウンド交互計測を実施）、`baseline_summary` の min を
プロセス間でさらに min を取った値を記録する。

| 段 | A min (µs) | C min (µs) | ratio(min) |
| --- | --- | --- | --- |
| B0s（visible_id 収集） | 322 | 325 | 1.009 |
| B0（密側 KNN） | 429 | 405 | 0.944 |
| B1（SQL 表層 hybrid） | 8898 | 6315 | 0.710 |
| B2（投影込み） | 10613 | 8049 | 0.759 |
| B3（密側対照・informational） | 639 | 643 | 1.006 |
| B4（融合込み対照） | 5684 | 3039 | 0.535 |
| B5（疎側再取得ループ） | 3408 | 1669 | 0.490 |
| B8（可視集合構築） | 125 | 123 | 0.984 |
| B7（融合コア下限近似） | （before 計測不能。B7 は #572 で新設のためハーネス非存在） | 10 | — |

派生帰属（#465 と同区分）:

| 区分 | A (µs) | C (µs) |
| --- | --- | --- |
| sql_surface (B1−B4) | 3214 | 3276 |
| projection (B2−B1) | 1715 | 1734 |
| sparse (B5) | 3408 | 1669 |
| residual (B4−B0−B5) | 1847 | 965 |
| dense (B0) | 429 | 405 |

`sparse(B5)` が約 51%（3408→1669µs）、`residual(B4−B0−B5)` が約 48%
（1847→965µs）に減少している。`sparse(B5)` の減少は #546（スコアアキュムレータ
のスクラッチプール化）、`residual` の減少は #549（RRF 融合コアの位置索引化）に
それぞれ帰属すると考えられるが、状態 B（#546 のみ）を計測していないため
**寄与の切り分けは確定できない**（申し送り）。`sql_surface`・`projection`・
`dense`・`B8` はノイズ帯程度の変動でほぼ不変。B7（融合コア下限近似）は after
のみ計測可能で、`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #549」節の
単一バイナリ内参考値（10µs）と整合する。

`approx_heap_bytes`（B0s stage の memory 行）は #546 でプール上限分が加算表示
される設計のため、この前後比較では実 RSS の増減指標として扱わない
（同 doc の既存注記のとおり）。

## 5. crossdb self（wire 経由・交互 3 ペア・min-of-3）

25,000 行・dim 128 の共通 fixture（`docs25k.redb`／`queries200.jsonl`／
`docs25k.jsonl`。他セッションが作成した既存 fixture を再利用）に対し、
各状態の worktree で `wire-server` をビルドし直したうえで
`scripts/crossdb_bench/run.py --db self --config exact` を A→C 交互に 3 ペア
実行した（`CROSSDB_SELF_PORT` を専用ポートへ変更しポート競合を回避）。

| phase | A min (µs) | C min (µs) | ratio(min) |
| --- | --- | --- | --- |
| vector_knn | 704 | 698 | 0.991 |
| vector_knn_where | 2910 | 2922 | 1.004 |
| point_where | 2910 | 2922 | 1.004 |
| where_compound_count | 3974 | 3970 | 0.999 |
| agg_count | 3563 | 3440 | 0.966 |
| agg_multi | 3688 | 3733 | 1.012 |
| group_by_having | 4008 | 4029 | 1.005 |
| **hybrid_rrf** | **5951** | **5949** | **1.000** |
| mode_recall | 703 | 690 | 0.981 |
| mode_precision | 649 | 682 | 1.050 |
| bulk_knn_k200 | 7765 | 7767 | 1.000 |
| bulk_knn_k1000 | 11259 | 11356 | 1.009 |
| bulk_knn_where_k200 | 4716 | 4738 | 1.005 |
| bulk_hybrid_k200 | 9485 | 9311 | 0.982 |
| rls_isolation | 3551 | 3421 | 0.963 |
| udf_call | 748 | 744 | 0.994 |

参照区間 `vector_knn` の実測ノイズ帯: A=7.1%・C=8.6%。wire 経由の `hybrid_rrf`
は before/after で比 1.000（ratio ほぼ 1）であり、engine 内部（§4）で観測された
`sparse`/`residual` の削減が wire レベルの p50 には現れていない。これは
`docs/design/hybrid-rrf-latency-breakdown.md`「最新基線」節が示す wire 内訳
（SQL 表層 T2−T1p が最大区分・67.1%）と整合し、engine 内 hybrid 経路
（T1p・28.6%）の改善分が SQL 表層・wire 側の固定コストに対して相対的に小さい
ため、と考えられる。self の `hybrid_rrf`（5,949〜5,951µs）は `docs/design/
crossdb-bench.md` 記載のスナップショット値（`559b523` 時点 6,178µs）と近い
水準にあり、大きな環境差は無い。

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

**全 10 指標 × 2 エンジンで A/C の差分がゼロ**（bit-identical）。これらの
ハーネスは `hybrid_search → rrf_fuse` を実際に通るため、この差分ゼロは
Issue #546・#549 双方の「スコア・順序はビット同一」契約の数値的裏付けである
（`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」「Issue #549」節の
機械検証を補強）。12 run すべて `test result: ok`（`0 failed`）。

層 A（`cargo test --release -p engine --test hybrid_recall --test
rerank_recall --test query_planning_recall --test sparse_determinism`。状態 C）
は全件 green（23 テスト・0 failed）。

## 7. 判定

- **共有 QEMU 環境の計測のため、本書の数値は参考値であり Accepted/Rejected の
  採否根拠にしない**（`docs/design/benchmark-judgement-policy.md` §5・§7.1）
- Recall 3 ゲート（brute_force／hnsw）は A/C で完全に一致し、#546・#549 の
  ビット同一契約が実 hybrid 経路（`RECALL_ENGINE=hnsw` の ANN opt-in 経路を
  含む）で崩れていないことを確認した。これは非退行の直接証拠であり、
  共有環境かどうかに依らない結論（決定的ハーネスの差分比較）
- `feature_bench` の `hybrid_rrf` は参照区間ノイズ帯と同程度の改善
  （比 0.944〜0.945）にとどまり、この計測だけでは「改善した」と断定しない
- `bench-hybrid-profile` の段別内訳では `sparse(B5)`・`residual(B4−B0−B5)` が
  大きく減少（約半分）しており、engine 内部の狙った経路では改善方向の変化が
  一貫して観測された。ただし専有環境での確定が必要
- crossdb self（wire 経由）の `hybrid_rrf` は比 1.000 で、engine 内部の改善が
  wire レベルの p50 には現れていない。SQL 表層・wire の固定コストが支配的な
  ため（`hybrid-rrf-latency-breakdown.md` の内訳と整合）
- 一貫した悪化（両ノイズ帯を超える）は観測されなかった。差し戻し候補には
  該当しない

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

## 9. 再現手順

```bash
# 状態 A（before）を別 worktree でビルド
git worktree add --detach <dir-a> 91f6a18
( cd <dir-a> && CARGO_TARGET_DIR=<dir-a>/target cargo build --release -p engine --example feature_bench \
    && CARGO_TARGET_DIR=<dir-a>/target cargo build --release -p wire-server \
    && CARGO_TARGET_DIR=<dir-a>/target cargo bench --bench hybrid_profile_bench -p engine --features bench-internals --no-run )

# 状態 C（after）は本ブランチ HEAD をそのままビルド
CARGO_TARGET_DIR=target cargo build --release -p engine --example feature_bench
CARGO_TARGET_DIR=target cargo build --release -p wire-server
CARGO_TARGET_DIR=target cargo bench --bench hybrid_profile_bench -p engine --features bench-internals --no-run

# feature_bench 交互 5 ペア（p50 min-of-5・median-of-5 を集計）
# bench-hybrid-profile はプロセスを A→C 交互に起動（BENCH_HYBRID_PROFILE_ROUNDS=5）
# crossdb self は各 worktree の scripts/crossdb_bench/run.py --db self --config exact を交互実行

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
