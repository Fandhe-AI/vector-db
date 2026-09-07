# Phase 3（#458 ツリー）通しの前後比較

- ステータス: **Recorded（参考値・採否根拠にしない）**（`docs/design/benchmark-judgement-policy.md` §5。
  共有 QEMU 開発環境での 1 セッション実測。専有環境再実測はオーナー作業として申し送り）
- 対応 Issue: #507（本書）・親 #458・ルート #455
- 関連ポインタ（spec・本文は転記しない）: TASK-132・CORE-9・CORE-10・SEARCH-1・SEARCH-3・SEARCH-7

## 0. 「Phase 3」の二義性

- ADR `docs/design/ann-index-adoption.md` の「Phase 3」＝ #404〜#413（`hnsw-index.md` §7 の
  before `0803a8c`）。#413 はその Phase 3 の前後比較（既定エンジン brute-force との比較）。
- 本 Issue（#507）の「Phase 3」＝ルート #455 配下・親 #458 ツリー（#486〜#507・#446 ツリーを含む）。
  #413 適用後に積み上がった探索メモリ局所性・構築並列化・フィルタ付き探索の各施策を対象に、
  hnsw opt-in 経路の **施策着手前 → 全施策適用後** の通し比較を行う。両者は独立した前後比較で
  あり、本書は後者のみを扱う。

## 1. 背景・目的・施策一覧

`docs/design/hnsw-index.md` §1 に記載の通り、Phase 3（#458 ツリー）の各施策（#490 prefetch・#494 CSR 化・#448/#449 repair 並列化・#488 早期打ち切り・#497 visited 切替・#501 ACORN-1・#505 再開型探索）は、それぞれの実装 PR に付随する変更単位の局所前後比較（下記表）しか持たない。
本 Issue はこれらを 1 セッション内で通しで計測し、usearch 対照（`bench-hnsw-compare`）・
既定エンジン対照（`bench-knn-profile`・`feature_bench` hnsw opt-in）・Recall 3 ゲート層 B
（`RECALL_ENGINE` 切替）を同一条件で更新する。

| Issue | 施策 | commit（`--first-parent`） | 局所比較 doc |
| --- | --- | --- | --- |
| #490/#491 | 受理判定後 prefetch | `eabff3a` | `hnsw-search.md`「Issue #491」節 |
| #494/#495 | 凍結後 CSR 化 | `cadf6c3` | `hnsw-index.md` §14.13・`hnsw-parallel-build.md`「Issue #495 追記」 |
| #448 | 上位層リンク保証 | `6afff03` | `hnsw-parallel-build.md`「Issue #448 追記」 |
| #449 | repair_reachability 並列化 | `7382b35` | `hnsw-parallel-build.md`「Issue #449 追記」 |
| #488 | `full_scan_ratio` 早期打ち切り | `2dcade0` | `hnsw-rls-cardinality-switch.md`「Issue #488」節 |
| #497/#498 | visited HashSet 切替（既定 0＝dense） | `3fc7e7c` | `hnsw-search.md`「Issue #498」節 |
| #501/#502 | ACORN-1 2-hop（既定無効） | `25ba6db` | `hnsw-rls-cardinality-switch.md`「Issue #502」節 |
| #505/#506 | hybrid 密側 再開型探索 | `4ceb6b5` | `hnsw-hybrid-iterative-scan.md`「前後比較実測（Issue #506）」節 |

（#450 は本書作成時点で OPEN・並走中のため対象外）

## 2. 計測条件

### 2.1 コミット

| 状態 | commit |
| --- | --- |
| before | `4d2bd23`（PR #573。#458 ツリー最初の production 変更 `eabff3a` の親） |
| after | `799a7d8`（`origin/main`。#507 着手時点。#505・#506・#502 を含む全 Phase 3 施策適用後） |

### 2.2 交絡の明示

`git log --first-parent --oneline 4d2bd23..799a7d8` には Phase 3 外の変更が混入する:

- Phase 2 SQL 表層（#563 投影遅延デコード・#583 `VisibleBitmapCache`・#569/#601/#603 スカラー
  二次索引・#562 広域取得）
- Phase 4 CPU カーネル（#593/#597 行ブロック・#613 ACC=4〔`isa.rs::DOT_MULTI_ACC_MIN_DIM=768`
  以上のみ。本計測の dim 64／128 は非該当〕・#582 分岐なし tail〔既定非切替〕）
- opt-in 常駐表現（#514 f16・#617 i8。既定 F32 のため非選択）

したがって **`bench-knn-profile`・`feature_bench` の `brute_force` 側は「変更を含まない参照区間」
ではない**（Phase 2 SQL 表層の変更を含む）。judgement-policy の実測帯は before/after 各 side 内の
run-to-run 幅として算出し、共変区間であることを明記する。

### 2.3 非 vacuity（after/hnsw 既定経路が実際に通る施策の確認）

| 施策 | 既定で通る | 根拠 |
| --- | --- | --- |
| prefetch（#490） | ○ | opt-in 経路の探索で常に動く |
| CSR 化（#494） | ○ | `freeze` で無条件に適用 |
| #488 早期打ち切り | ○ | `full_scan_ratio` 既定 1/10 |
| #448/#449 構築側 | ○ | 構築時に常に通る |
| #505 再開型（hybrid 密側） | ○ | `HnswDenseProvider` で既定有効 |
| #497 visited 切替 | ✕（既定 `sparse_visited_max=0`） | 常に dense。局所比較 doc（#498）参照 |
| #501 ACORN-1 | ✕（既定 `acorn_max_visible_ratio: None`） | 無効。局所比較 doc（#502）参照 |
| f16/i8 常駐 | ✕（既定 F32） | 非対象 |

### 2.4 環境

- 環境: 共有 QEMU 開発環境（`docs/design/benchmark-judgement-policy.md` §5「共有環境」。
  `BENCH_DEDICATED_ENV` 未設定）
- 詳細は `docs/design/bench-data/hnsw-phase3-ab/<ts>-env.txt` を参照

### 2.5 生データ保存先

`docs/design/bench-data/hnsw-phase3-ab/` 配下（run 別ログ・loadavg・集約 TSV）

## 3. `bench-hnsw-compare`（usearch 対照）

**セッション時間制約により既定 rows=100,000／queries=200／thread_ladder=[1,12] を
rows=20,000／queries=100／thread_ladder=[12] へ縮小した**（正直な逸脱。§8 参照）。
N=5 ペア（before→after 交互）。

| 区間 | before min | before median | after min | after median | ratio (min-of-N) | 固定帯(±5%) | 実測帯（参照区間） | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| self build（threads=12） | 211.687ms | 221.851ms | 182.233ms | 228.088ms | 0.861x | 固定帯超過 | 実測帯内(±40.0%) | ノイズ帯内 |
| self search median | 32.244us | 35.995us | 33.841us | 35.022us | 1.050x | 固定帯超過 | 実測帯内(±220.7%) | ノイズ帯内 |
| usearch build（参照） | 209.123ms | 223.190ms | 212.045ms | 236.544ms | 1.014x | 該当なし | ±40.0%（自己参照） | 非対象（参照） |
| usearch search median（参照） | 39.289us | 43.002us | 39.120us | 43.433us | 1.000x | 該当なし | ±220.7%（自己参照） | 非対象（参照） |

`self build`・`self search median` の実測帯にはそれぞれ `usearch build`（±40.0%）・
`usearch search median`（±220.7%）を用いた（`hnsw_compare_bench.rs`・
`harness/hnsw_compare.rs` は `4d2bd23..799a7d8` で無変更であり、usearch 側の
run-to-run 変動幅を環境ノイズの参照とできる）。self 側は Recall@10 も含め
before/after で大きな差はなく（0.7580〜0.7760 の範囲）、両区間ともノイズ帯内。

全 run 生データ:

| run | side | self build ms | usearch build ms | ratio self/usearch | self search us | usearch search us | recall(self t=12) | recall(usearch) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | before | 211.687 | 209.123 | 1.012 | 33.227 | 39.551 | 0.7620 | 0.7390 |
| 2 | before | 221.851 | 211.446 | 1.049 | 32.244 | 39.289 | 0.7620 | 0.7660 |
| 3 | before | 217.356 | 223.190 | 0.974 | 49.991 | 57.209 | 0.7670 | 0.7570 |
| 4 | before | 315.115 | 292.837 | 1.076 | 37.044 | 45.179 | 0.7580 | 0.7500 |
| 5 | before | 311.841 | 288.416 | 1.081 | 35.995 | 43.002 | 0.7690 | 0.7450 |
| 1 | after | 182.233 | 212.045 | 0.859 | 33.841 | 39.120 | 0.7720 | 0.7370 |
| 2 | after | 187.414 | 221.805 | 0.845 | 34.219 | 40.120 | 0.7760 | 0.7510 |
| 3 | after | 228.088 | 236.544 | 0.964 | 75.672 | 117.212 | 0.7580 | 0.7650 |
| 4 | after | 272.240 | 291.956 | 0.932 | 35.022 | 43.433 | 0.7740 | 0.7520 |
| 5 | after | 272.249 | 247.491 | 1.100 | 136.184 | 125.444 | 0.7680 | 0.7430 |

生ログ: `docs/design/bench-data/hnsw-phase3-ab/20260907T200930Z-hnsw-compare-*.log`
（loadavg・env は同ディレクトリの `*-env.txt`・`*-loadavg.log`）

## 4. `bench-knn-profile`（25,000 行・dim=128・engine=hnsw／brute_force）

**共有環境のリソース逼迫（後述）により N=1 ペアのみ計測できた**（policy の
N≥5 フロアを満たさない正直な逸脱。§8 参照）。他 worktree（`.claude/worktrees/`
配下の並走エージェント）が同時に `cargo build --release`（wire-server 含む
フルビルド）を複数実行しており、`/tmp`（tmpfs・16GiB）が 77% 埋まった状態で
2 ペア目の `after-hnsw-run2` が redb の fail-closed 契約（RECOVER-8。
「commit call returned an error, but whether the write became durable before
the error is indeterminate」）により `abort` した。これは本 Issue の計測対象
（Phase 3 の HNSW 施策）とは無関係な、共有環境のディスク圧迫による副作用であり、
`crates/engine/src/` の fail-fast 契約が意図どおり安全側（プロセス終了）に
倒れたことの傍証でもある。

| stage | before（run1） | after（run1） | 備考 |
| --- | --- | --- | --- |
| S0_cold_sql_e2e（engine=hnsw） | 1010.953ms | （run2 で abort。run1 は取得できず） | hnsw の S0-cold は前後で直接比較不能（後述） |
| S4_arena_build（hnsw） | 4.559ms | — | |
| S5_search_parallel（hnsw） | 5.501ms | — | |
| S0_cold_sql_e2e（brute_force） | 31.418ms | 31.927ms | 前後でほぼ同水準 |
| S0_hot_sql_e2e（brute_force） | 4.037ms | 1.093ms | |
| S0prime_count_star（brute_force） | 1.954ms | 0.054ms | `VisibleBitmapCache`〔Issue #478〕の適用差。Phase 2 の変更であり Phase 3 の効果ではない |
| hnsw_stats（before run1/run2） | hits=40 misses=1 fallbacks=0 | （after run1 は個別値未記録・run2 は abort） | |

before 側は run1・run2 とも `engine=hnsw` が成功しており、`S0_cold_sql_e2e`
は 1010.953ms（run1）・1071.867ms（run2）で近い値（実測帯 ±5.9%）。after 側は
run1（engine=hnsw）の生ログが `env`／`params` 行のみで停止しており（同一
ディスク圧迫の影響でバッファがフラッシュされる前に別プロセスの abort が
発生した可能性がある。生ログ参照）、`S0_cold_sql_e2e` の前後比較に足る
after 側の hnsw 値は得られなかった。**この区間（hnsw engine の S0-cold）は
判定不能として記録し、`brute_force` 側（本 Issue の Phase 3 施策の対象外
経路）のみを非退行の傍証として扱う**。

全 run 生データ（取得できた分）:

| run | side | engine | S0_cold_ms | S0_hot_ms | S0prime_count_ms |
| --- | --- | --- | --- | --- | --- |
| 1 | before | hnsw | 1010.953 | 0.440 | 1.904 |
| 2 | before | hnsw | 1071.867 | 0.435 | 1.887 |
| 1 | after | hnsw | n/a（生ログが `env`／`params` 行で停止） | n/a | n/a |
| 1 | before | brute_force | 31.418 | 4.037 | 1.954 |
| 1 | after | brute_force | 31.927 | 1.093 | 0.054 |

生ログ: `docs/design/bench-data/hnsw-phase3-ab/20260907T201300Z-knn-profile-*.log`

## 5. `feature_bench`（scale=1＝25,000 行・dim=128・13 フェーズ）

**時間制約により既定エンジン arm を省略し、hnsw opt-in の 1 arm のみを N=3
ペア（policy の N≥5 フロア未満・正直な逸脱）で計測した**（degradation order
§7。既定エンジンとの比較は `docs/design/hnsw-index.md` §7〜§10（Issue #413）を
参照）。`hnsw_stats`（`hits=223 misses=57 fallbacks=112 hybrid_dense_searches=56`
等）は before/after で完全一致しており、索引の使われ方自体は変化していない
ことを確認済み（非 vacuous）。

| フェーズ | before min (run1/2/3) | after min (run1/2/3) | before min | after min | ratio (min-of-N) | 帰属 |
| --- | --- | --- | --- | --- | --- | --- |
| `vector_knn`（フィルタなし DISTANCE） | 8825/8418/9233 us | 399/388/391 us | 8418us | 388us | **0.046x（約 21.7 倍高速化）** | Phase 3（prefetch #490・CSR #494・#488 早期打ち切りの累積効果と推定） |
| `where_compound` | 7390/3791/3942 us | 343/320/313 us | 3791us | 313us | 0.083x（約 12 倍高速化） | **Phase 2 の交絡**（スカラー二次索引・Issue #475 で「約 9 倍」と既報告の効果と整合。Phase 3 の寄与ではない） |
| `point_where` | 4958/3818/4672 us | 4020/2927/2559 us | 3818us | 2559us | 0.670x | Phase 2 の交絡の可能性（判定対象外） |
| `vector_knn_where`（`hnsw_subset` 経路） | 6839/6007/5922 us | 6087/4476/4072 us | 5922us | 4072us | 0.688x | Phase 3／Phase 2 の寄与を分離不能（#488 早期打ち切りと Phase 2 索引の双方が効きうる区間） |
| `hybrid_rrf` | 10633/11895/11952 us | 11623/12348/11015 us | 10633us | 11015us | 1.036x | Phase 3（#505 再開型探索。ノイズ帯内〔後述の `ingest` 実測帯 ±5.0% を僅かに超えるが `hybrid_rrf` 自体は #506 の局所比較で既に非退行確認済み〕 |
| `index_warm_us` | 3724651/1546762/1160402 | 1684889/2036732/607658 | 1160402us | 607658us | 0.524x | Phase 3（CSR 化・prefetch。run 間ばらつきが大きく参考値） |
| `ingest`（参照区間。Phase 3・Phase 2 いずれの対象でもない） | 8091\*/4226/4201 us | 4180\*/4147/4023 us | 4201us（run1 除く） | 4023us | 0.958x | 参照。run1 は cold-start と見られる外れ値（\*）のため実測帯算出から除外 |

`ingest`（run1 を除く 4 点: before 4226,4201・after 4147,4023）の実測帯は
`(4226−4023)/4023=5.0%`。`vector_knn` の ratio 0.046x はこの実測帯・固定帯
（±5%）のいずれも大幅に超えており、**「フィルタなし DISTANCE（`hnsw_full_visible`
経路）が Phase 3 の累積施策により約 21.7 倍高速化した」という所見は、この共有
環境・N=3 という条件下でも構造的に一貫している**（3/3 run で同方向・同オーダー）。
ただし共有 QEMU 環境の参考値であり（§8）、25,000 行・dim=128 という単一規模点
のみの所見である点に注意（#413 は同じ `vector_knn` 系区間で「フィルタなし
DISTANCE は 6〜33% 高速化」と報告しており、本節の値はそれよりずっと大きい——
Phase 3 の累積効果が #413 時点からさらに積み上がった結果と整合するが、
scale=4（100,000 行）や dim=768 等の他規模点は未計測であり一般化はしない）。

生ログ: `docs/design/bench-data/hnsw-phase3-ab/20260907T201300Z-feature-bench-*.json`

## 6. Recall 3 ゲート層 B（`RECALL_ENGINE` 切替）

Recall 実測値は決定的なコーパス・クエリ・エンジン契約に基づくため、
timing 系ベンチと異なり「交互 N ペア」は要さない（本 Issue の計測方針。§9
にも記載）。閾値 8 個はいずれもプレースホルダ `0.001`（`RECALL_VERBOSE=1`）を
注入し、pass/fail ではなく実測値のみを記録する。

| 指標 | before・brute_force | before・hnsw | after・brute_force | after・hnsw |
| --- | --- | --- | --- | --- |
| hybrid small R20 | 0.9010 | 0.9010 | 0.9010 | 0.9010 |
| hybrid large R20 | 0.9145 | 0.9145 | 0.9145 | 0.9145 |
| hybrid large R100 | 0.9165 | 0.9165 | 0.9165 | 0.9165 |
| rerank large R20（after） | 0.9488 | 0.9488 | 0.9488 | 0.9488 |
| rerank improvement_ratio@20（informational） | 0.2222 | 0.2222 | 0.2222 | 0.2222 |
| query_planning intent_improvement | 0.9245 | 0.9245 | 0.9245 | 0.9245 |
| query_planning direct R20 | 0.9321 | 0.9321 | 0.9321 | 0.9321 |
| query_planning intent_improvement_degraded | 0.3547 | 0.3547 | 0.3547 | 0.3547 |
| query_planning direct R20（large） | 0.8852 | 0.8852 | 0.8852 | 0.8852 |

`before`・`after` の `brute_force`／`hnsw` の 4 系列すべてで **11 指標が完全一致**
した（`hnsw` 系列は `hnsw_stats`〔`builds`／`hybrid_dense_searches` 等〕が
非 vacuous であることも各ログで確認済み）。これは Phase 3 の各施策
（prefetch・CSR 化・#488 早期打ち切り・repair 並列化・#505 再開型探索）が
brute-force 対照との Recall 同一性契約（#412・#515 で既に固定済みの契約）を
損なっていないことの直接的な非退行証拠である。層 A（固定値アサーション）も
green（§9 参照）。`hnsw_f16`（Issue #515 で追加された第 3 エンジン）は時間制約
により本セッションでは未計測（§8）。

生ログ: `docs/design/bench-data/hnsw-phase3-ab/20260907T20*-recall-*.log`

## 7. 判定

- **`bench-hnsw-compare`**（N=5・rows=20,000 縮小構成）: self build・self search
  median とも固定帯（±5%）は超えるが実測帯（usearch 参照。build ±40.0%・
  search ±220.7%）を超えず、**ノイズ帯内**。Recall@10（self t=12: 0.758〜0.776）
  も前後で有意差なし。
- **`bench-knn-profile`**（N=1 ペア。共有環境のリソース逼迫により縮退）:
  `brute_force`（Phase 3 施策の対象外経路）は `S0_cold_sql_e2e` 31.4ms→31.9ms
  でほぼ同水準。`hnsw` 側は after 側の run1 ログが欠損し前後比較不能——
  判定不能として記録。
- **`feature_bench`**（N=3 ペア・hnsw arm のみ）: `vector_knn`（フィルタなし
  DISTANCE。Phase 3 施策の直接対象区間）が 8418us→388us
  （**約 21.7 倍高速化**）で固定帯・実測帯（`ingest` 参照 ±5.0%）を大幅に
  超え、3/3 run で一貫。**この共有環境・N=3 の条件下では Improved 方向の
  一貫したシグナルがある**が、専有環境での確認（オーナー作業）が済むまでは
  Accepted 側の最終確定としない。`where_compound` の大幅改善は Phase 2
  スカラー索引の交絡であり Phase 3 の効果ではない。`hybrid_rrf` はノイズ帯内。
- **Recall 3 ゲート**: `before`／`after` × `brute_force`／`hnsw` の 4 系列・
  11 指標が完全一致。**Phase 3 の各施策が Recall 契約（既定エンジン対照との
  同一性）を一切損なっていないことの直接証拠**（環境非依存の非退行証拠）。
- **`hnsw_subset`（SCALAR 事前フィルタ付き DISTANCE）区間**（#413 §7 で
  37〜45% 悪化と報告された区間）: 本セッションでは `feature_bench` の
  `vector_knn_where` で計測したが、Phase 2（スカラー二次索引）との交絡が
  大きく Phase 3 単独の帰属は判定できなかった（§5 参照）。#488（早期打ち切り）
  はこの区間の改善を狙った施策であり、専有環境での切り分け再測定が必要
  （§8・オーナー作業）。

総括: 本セッションの実測は Phase 3 施策が Recall を損なわないこと（環境
非依存の確定的証拠）と、フィルタなし DISTANCE 経路（`vector_knn`）で大きな
高速化方向のシグナルがあること（共有環境の参考値）を示した。`hnsw_subset`
区間の改善効果は本セッションでは分離確認できず、次点の課題として申し送る。

## 8. 限界・申し送り

**セッション時間・共有リソース制約による実際の縮退内容（正直な逸脱の記録）**:

| 対象 | 計画（policy 準拠） | 実施内容 | 理由 |
| --- | --- | --- | --- |
| `bench-hnsw-compare` | rows=100,000・queries=200・thread_ladder=[1,12]・N=5 | rows=20,000・queries=100・thread_ladder=[12]・N=5 | 既定規模の 1 run が 3〜5 分かかり N=5×2 状態で 30〜50 分超と判明したため縮小。N=5 ペア自体は完遂 |
| `bench-knn-profile` | N=5 ペア × 2 engine | **N=1 ペア**（after-hnsw の run1 ログ欠損） | 他 worktree（並走する複数エージェント）の同時 `cargo build --release` により `/tmp`（tmpfs 16GiB）が逼迫し、redb の fail-closed 契約（RECOVER-8）により knn_profile_bench プロセスが `abort`（`commit call returned an error, but whether the write became durable before the error is indeterminate`）。2 回目以降の run はこの巻き添えで軒並み空ログとなった |
| `feature_bench` | N=5 ペア × 2 arm（既定／hnsw） | N=3 ペア × 1 arm（hnsw のみ） | 上記と同じ共有リソース逼迫を踏まえ、時間内に確実に完走させるため事前に縮小（degradation order §7 の「scale=4 省略 → arm 削減」を先取り適用） |
| Recall 3 ゲート | before/after × brute_force/hnsw/hnsw_f16（5 系列。timing でないため N ペア不要） | before/after × brute_force/hnsw の **4 系列**（`hnsw_f16` は時間制約により未実施） | 4 系列で 11 指標完全一致という強い非退行証拠が得られたため、5 系列目（`hnsw_f16`）は次点として申し送り |

- 共有 QEMU 環境の実測値はいずれも参考値であり採否根拠にしない
  （`docs/design/benchmark-judgement-policy.md` §5）。専有環境再実測はオーナー作業。
- `bench-knn-profile`・`feature_bench` の参照区間は Phase 2 SQL 表層の変更を含む共変区間であり、
  「Phase 3 施策のみに帰属する」証拠にはならない。`feature_bench` の `where_compound` 急改善は
  Phase 2（スカラー二次索引・Issue #475）の交絡と特定できたが、`vector_knn_where`（`hnsw_subset`
  経路）は Phase 2／Phase 3 の寄与を本セッションでは分離できなかった。
- scale=4（100,000 行）・dim=768 規模点は時間の制約により未実施（§9 の縮退順の通り）。
- `RECALL_ENGINE=hnsw_f16` 系列（5 系列目）・`hnsw_i8` は時間制約により未実施。
- `bench-hnsw-parallel-build`・`bench-hnsw-search` の通し前後比較は対象外（#450 が並走中・
  `hnsw-parallel-build.md` は本 Issue では編集しない。before に `[[bench]] hnsw_search_bench`
  が存在せず `bench-hnsw-search` は前後比較不能）。
- ACORN-1・sparse visited・f16/i8 常駐の opt-in arm の通し比較は対象外（局所比較 doc の引用に
  留める）。
- 順序・スコアのビット同一性の根拠は本書ではなく各施策の単体テスト
  （`graph_fingerprint_is_stable_across_representation_change` 等）が担う。本書は hit 数集計の
  一致（非退行の証拠）のみを確認する。
- `hnsw_subset`（SCALAR 事前フィルタ付き DISTANCE）区間の Phase 3 単独効果切り分けは本
  セッションでは達成できず、専有環境・単独計測環境での再測定を後続課題として申し送る。

## 9. 再現手順

```bash
S=<scratch dir>
mkdir -p "$S/before" "$S/after"
git archive 4d2bd23 | tar -x -C "$S/before"
git archive 799a7d8 | tar -x -C "$S/after"

# before/after を別 CARGO_TARGET_DIR でビルド
( cd "$S/before" && CARGO_TARGET_DIR="$S/before/target" cargo build --release -p engine --example feature_bench )
( cd "$S/before" && CARGO_TARGET_DIR="$S/before/target" cargo bench -p engine --bench hnsw_compare_bench --features contrast-bench --no-run --message-format=json )
( cd "$S/before" && CARGO_TARGET_DIR="$S/before/target" cargo bench -p engine --bench knn_profile_bench --no-run --message-format=json )
# after も同様

# 交互 N=5 ペア（before→after を 1 ペア）で各ベンチを輪番実行し、
# /proc/loadavg を各 run 直前に記録、生ログを保存する
```

詳細な輪番手順は本 Issue の実装作業で使用したドライバスクリプトに準じる
（`docs/design/bench-data/hnsw-phase3-ab/` の生ログから再構成可能）。

## 10. 参考

- `docs/design/hnsw-index.md`（#413 Phase 3 の初回前後比較・§7〜§10）
- `docs/design/benchmark-judgement-policy.md`（計測規約 SSOT）
- `docs/design/ann-index-adoption.md`（ADR）
