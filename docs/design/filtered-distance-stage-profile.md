# ADR: SCALAR 事前フィルタ付き DISTANCE の段別プロファイル（選択率 33%）

- ステータス: Accepted
- 対応 Issue: #653（親 #650・ルート #649）
- 関連ポインタ: TASK-83（SQL 表層性能受け入れ基準）・SQL-6（`WHERE`／`ORDER BY` DISTANCE）・Issue #464／`docs/design/scan-stage-profile.md`（先行の段別プロファイル。選択率 20%・索引導入前の経路）・Issue #474／`docs/design/scalar-index-prune.md`（`ScalarIndex` 候補削減の結線）・`docs/design/benchmark-judgement-policy.md`（計測規約）・`docs/design/crossdb-bench.md`（本 Issue の動機となった専有環境実測）

## 背景

2026-09-08 の専有環境 crossdb 実測（`docs/design/crossdb-bench.md`・ルート #649）で `vector_knn`（フィルタなし）約 695µs に対し `vector_knn_where`（`WHERE lang = 'ja' ORDER BY embedding <=> q LIMIT 10`）が約 2,035µs と +1,340µs 劣後し、Qdrant は同条件で悪化しない。

既存の段別プロファイル `crates/engine/benches/scan_stage_profile_bench.rs`（Issue #464）の W 系列は、次の 2 点で今回の疑問に直接は答えられない:

1. `ScalarIndex` 候補削減（Issue #474・`sql/exec.rs::execute_statement_with_cache` の `build_from_cached_rls_rows_subset` 結線）**導入前**の経路（全可視行 `scan_scalar_columns` → 述語 → 一致行複製）を再現した値であり、現行の索引経路の内訳ではない。
2. fixture の選択率が `lang` 5 値輪番＝20% で固定されており、crossdb fixture（`lang='ja'` 8,309/25,000 ≒ 33%）と一致しない。

本 Issue は、現行経路（索引候補削減あり）で選択率 33% のとき「どの区分が何 µs・何 % を占めるか」を実測し、後続 #654（候補 id マスク経路で arena 複製回避）が狙う削減余地の上限を数値で確定する。

**production コード（`crates/engine/src/`）は無変更**。テスト・ベンチ・docs 専任。

## 測定対象の実行経路（索引経路・plain 経路の分岐）

`sql/exec.rs::execute_statement_with_cache` は `SqlArenaCache` ヒット時、`sql::scalar_plan::classify_scalar_plan`（`bound.metadata_filters`・`bound.expr_filters` の静的形状判定）の結果に応じて分岐する:

- `PlainScan`（メタデータフィルタ・式フィルタが空、または式フィルタに `id` 単純比較以外の残余述語を含む）: `ScalarIndex` を一切参照せず、キャッシュ済みスナップショットの全可視行へ `on_visible_row`（`scan_scalar_columns` → `matches_all` → 一致行のみ embedding 複製）を適用する。
- それ以外（`IndexEquality`／`IndexPrefix`／`IndexIdRange`／`IndexConjunction`）: `ScalarIndexCache::lookup` → 索引↔スナップショット同一性ガード → `ScalarIndex::resolve_candidates` の順で候補スロットを絞り込み、`Use(slots)` なら候補行のみへ `on_visible_row` を適用する（`build_from_cached_rls_rows_subset`）。選択度が閾値（既定 1/2）を超える、または索引未構築・同一性不一致の場合は `FallbackNoIndex`／`FallbackSelectivity` として全可視行走査へ縮退する。

いずれの分岐も `sql::scalar_index::ScalarIndexCache`（`EngineCore::scalar_index_cache_stats()` 経由で観測可能。`index_scans`／`plain_scan_fallbacks`／`builds` を持つ。テナント ID・行 ID を含まない集計カウンタ）に統計を記録する。ただし **`PlainScan` 分類のクエリはこの分岐へ一切入らないため、`index_scans`／`plain_scan_fallbacks` のいずれも増分しない**——`FallbackNoIndex`／`FallbackSelectivity`（索引対応述語を持つが lookup/resolve に失敗）とは区別される契約であることに注意（後述「plain アームの非 vacuous 確認」参照）。

## 設計

### 選択率 opt-in（`BENCH_SCAN_PROFILE_SELECTIVITY`）

`benches/harness/scan_stage_profile.rs::parse_selectivity` が `BENCH_SCAN_PROFILE_SELECTIVITY=1/<N>`（`N` は 2..=100 の整数。未設定・空文字は既定 `1/5`）を fail-closed にパースする。`lang_for_id(id, denominator)` が id → `lang` 値の割当規則の単一情報源で、`id % denominator == 0` のみ `"ja"`、それ以外は `en`/`fr`/`de`/`es` を輪番する。既定 `1/5` は導入前の `LANGS[(id) % 5]` 輪番とビット同一であることを `tests/scan_stage_profile_accept.rs::lang_for_id_default_denominator_matches_legacy_langs_rotation`（id 0..1000 全件）で固定した。crossdb fixture の実選択率（33%）に合わせる場合は `1/3` を指定する。

### 2 アーム（index／plain）の同一バイナリ内計測

| アーム | `WHERE` 節 | 経路 |
| --- | --- | --- |
| `index`（現行経路） | `WHERE lang = '<value>'` | `ScalarPlan::IndexEquality` → `resolve_candidates` → `build_from_cached_rls_rows_subset` |
| `plain`（Issue #474 以前相当の形状） | `WHERE lang = '<value>' AND vec_norm(embedding) > 0` | 残余述語（`VectorRef` を参照するため `id_predicate_from_expr` が `None`）により `PlainScan` → `build_from_cached_rls_rows`（全可視行 `on_visible_row`） |

`AND 1 = 1` は束縛時の定数畳み込み（Issue #353・`sql/expr_program.rs`）により消去され `PlainScan` を強制できなかった（実装中に実測で判明。`tests/scalar_index_prune.rs::residual_builtin_expr_never_consumes_index` と同じ「`vec_norm(embedding) > 0` という `VectorRef` 参照の恒真述語」形状を採用した）。

`plain` アームは「Issue #474 以前の実バイナリ」とビット同一ではなく、定数畳み込み済みステップ 1 個/行を余分に含む「Issue #474 以前の形状」であることに注意する（#474 以前の実バイナリとの交互 A/B は、旧ハーネスが選択率 opt-in を持たず同条件比較にならないため実施しなかった）。

### 索引経路の内訳（I 系列。pub API 再実装）

`sql::scalar_index::ScalarIndex`・`sql::exec::on_visible_row` はいずれも `pub(crate)` でベンチから直接呼べないため、pub API（`row_codec::scan_scalar_columns`・`declarative_filter::matches_all`・`ParallelSearchProvider::search`）で候補削減の内訳を再実装する（`knn_profile.rs::decode_row_reimpl` と同じドリフト対策方針。整合性は `expected_match_ids` との突き合わせで機械検証する）。

| 段 | 内容 |
| --- | --- |
| I1 `index_candidate_resolve` | 計測外で構築した値→候補スロット辞書（`HashMap<&str, Vec<usize>>`）からの `"ja"` lookup ＋ 複製（`ScalarIndex::candidates_for` 相当） |
| I2a `candidate_predicate` | I1 の候補のみへ `scan_scalar_columns` ＋ `lang = 'ja'` 判定（`build_from_cached_rls_rows_subset` の再適用契約に対応） |
| I2b `candidate_arena_copy` | I2a ＋ 一致行 embedding の連続 `Vec<f32>` 複製（**#654 の削減対象**。分母は候補行数） |
| I3 `provider_search` | 一致行のみへ `ParallelSearchProvider::search`（距離計算＋Top-k） |

SQL 表層固定コスト（4 区分目）は `e2e(index) − (I1 + I2b + I3)` の残差として `checked_sub` で算出する（I2b は I2a を包含する累積値のため、別途加算すると I1 分の候補述語コストを二重計上する。W 系列の `report_diff` と同じ理由。逆転時は測定ノイズとして `n/a` 表示）。

### 非 vacuous 確認（`scalar_index_cache_stats()` の増分）

- 索引アーム: `index_scans` が正に増分し、`plain_scan_fallbacks` は不変。
- plain アーム: `index_scans`／`plain_scan_fallbacks` のいずれも不変（上記「測定対象の実行経路」の契約どおり、`PlainScan` 分類は索引を消費する分岐そのものへ入らない）。

両アームは論理的に同一の `WHERE` 述語（`lang = 'ja'` かどうか）を持つため、索引経路・plain scan 経路のいずれで実行しても同一の Top-k id 集合を返すことも固定した。

### 計測規約（`benchmark-judgement-policy.md` への対応）

- 既存 A/W 系列（ラウンド輪番）に加え、index/plain 各アーム・I1〜I3 を `harness::protocol::run`（warmup 20・計測 20）で計測し、per-round 生データ（round[N] 行）・min-of-R／median-of-R を出力する。
- `R_dot`（変更を含まない参照区間）の複数ラウンド中央値から実測ノイズ帯（`reference_band`）を算出し、固定 ±5% 帯と併記する。

## 実測結果（25,000 行・`1/3`・共有 QEMU 環境）

- commit: `b3ae8912228aa3c76f605d61b9ee16ad4763b8ce`
- 環境: `os=linux arch=x86_64 logical_cpus=12 isa=Avx2Fma`（`lscpu` model name: `QEMU Virtual CPU version 2.5+`）・`loadavg=2.15 2.26 1.40`
- コマンド: `BENCH_SCAN_PROFILE_SELECTIVITY=1/3 BENCH_SCAN_PROFILE_ROUNDS=5 make bench-scan-stage-profile`
- 生ログ: `docs/design/bench-data/filtered-distance-stage-profile/1788877278-25k-1of3.log`

**共有 QEMU 環境の参考値のため、両ノイズ帯（固定 ±5%・`R_dot` 実測帯 21.14%）を超える判定には使わない。専有環境（`BENCH_DEDICATED_ENV=1`）での再実測はオーナー作業として申し送る。**

### 非 vacuous 確認

```
selectivity=1/3 expected_visible_hits=7667 (33.33%)
scalar_index(index,k=10): index_scans=+39 plain_scan_fallbacks=+0 builds=+1 arena_cache_hits=+39
scalar_index(plain,k=10): index_scans=+0 plain_scan_fallbacks=+0 builds=+0 arena_cache_hits=+0
```

索引アームは `index_scans` が計測イテレーション数（40 回中 39 回。1 回は cold で索引を構築する側に計上）ぶん増分し `plain_scan_fallbacks` は 0 のまま、plain アームは両カウンタとも 0（`PlainScan` 分類のため索引を消費する分岐へ入らない）——「索引経路が実際に発火している」ことを非 vacuous に確認できた。

### e2e（index／plain アーム）

```
e2e(index,k=10): median=6.301ms
e2e(plain,k=10): median=22.311ms
arm_ratio(index->plain,k=10): ratio=254.10%
```

同一選択率（33%）・同一クエリで、Issue #474 の候補削減（索引アーム）は Issue #474 以前相当の全可視行走査（plain アーム）比で約 3.5 分の 1（median 6.3ms 対 22.3ms）に短縮している。

### 索引経路の内訳（I 系列。us・% of e2e index arm）

```
bucket_share(I1_index_candidate_resolve): us=1.1 pct_of_e2e=0.02%
bucket_share(I2a_candidate_predicate): us=1133.8 pct_of_e2e=17.99%
bucket_share(I2b_candidate_arena_copy): us=2602.6 pct_of_e2e=41.31%
bucket_share(I3_provider_search): us=1164.0 pct_of_e2e=18.47%
bucket_share(sql_surface_fixed_cost_residual): us=2533.1 pct_of_e2e=40.20%
```

（`I2b` は `I2a` を包含する累積値なので、`I2b` 単独の複製コストは `I2b − I2a` ≈ 1,468.8µs ≈ e2e 比 23.3 ポイント分。）

## 所見

- I1（候補スロット lookup ＋ 複製）は e2e の 0.02% と無視できる大きさで、`ScalarIndex` の辞書 lookup 自体はボトルネックではない。
- I2b（候補行の arena 複製。#654 の削減対象）は I2a 込みで e2e の 41.31%、複製そのもの（I2b−I2a）は約 23.3 ポイント——本測定条件（選択率 33%・候補 7,667 行）での **#654 の削減余地の上限**として記録する。
- SQL 表層固定コスト（残差。I1+I2b+I3 に含まれない部分）は 40.20% で、I 系列の合計（59.80%）とほぼ拮抗する規模。crossdb で先行して特定済みの「投影・スキャン周りの k 非依存固定コスト」（Issue #453・#454）と整合する規模感であり、#654 単独では e2e 全体の半分弱までしか改善できない可能性を示唆する。
- 本測定は共有 QEMU 環境の 1 回実測（N=5 ラウンド）であり、round[2]/round[3] で他プロセス負荷由来と見られる外れ値（W1=5.634ms・A2=25.070ms 等）が混入し `R_dot` 実測ノイズ帯が 21.14% まで広がった。絶対値・比率とも参考値の位置づけとし、専有環境再実測で確定させる。

## crossdb との差異（申し送り）

- crossdb fixture: `lang='ja'` 8,309/25,000（≒ 33.24%）。本ベンチ fixture（`1/3`）: 7,667/23,000（≒ 33.33%）——近似だが完全一致ではない（分母がテナント総行数か tenant-a 可視行数かの違い、輪番方式の違いによる）。
- crossdb は `SELECT id, body ...`（`body` 列投影あり）・wire 経由往復を含むのに対し、本ベンチは `SELECT id ...`（投影なし）・`EngineCore::execute_sql` 直接呼び出し（wire 層を含まない）。`body` 列投影コストは対象外（Issue #453 遅延デコード・#661 の記録対象）。

## スコープ外・申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測はオーナー作業。本環境の値は参考値。
- `LIMIT 200` の crossdb 相当（`bulk_knn_where_k200`）の投影コスト（`id`+`body`）は本 Issue では計測しない。
- 100,000 行（`BENCH_SCAN_PROFILE_SCALE=4`）× `1/3` は未実施（1 プロセス = 1 規模点の方針上、時間許容時に追加実測）。
- #654（候補 id マスク経路で arena 複製回避）の実装後は本ベンチの `index` アームがそのまま after 側の計測点になる（#655 で before/after 交互実行）。
- production コード（`crates/engine/src/`）は無変更。
