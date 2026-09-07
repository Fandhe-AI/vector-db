# HNSW 構築の並列化（要素単位ロック・エントリポイント更新のみ排他）

- ステータス: **実装済み**（`crates/engine/src/hnsw.rs::HnswIndex::build_with_threads`／
  `build_parallel`・`crates/engine/src/hnsw/parallel_build.rs`）
- 対応: Issue #406（`perf(engine): HNSW 構築の並列化（要素単位ロック・エントリ
  ポイント更新のみ排他）`）
- 前提: Issue #404（`docs/design/hnsw-graph-construction.md`。グラフ構築
  Algorithm 1〜4）・Issue #405（`docs/design/hnsw-search.md`。探索 API）
- 親: Issue #402（Phase 3: ANN 索引の opt-in 採用）・Issue #403
  （`docs/design/ann-index-adoption.md` を Accepted 化）

## 背景・範囲

`HnswIndex::build`（#404）は単一スレッド逐次挿入で、実測
（`docs/design/hnsw-graph-construction.md`）では dim=64・32,000 行で約 2.2 秒、
`N log N` 相当で伸びるため 100k 点では数秒〜10 秒級になる。#408（世代整合
キャッシュ）では世代更新後の再構築がクエリ経路上で発生するため、構築時間の
短縮が必要になる。

本タスクは**構築の並列化のみ**を扱う。`search_engine.rs::SearchEngineKind`
への結線（#407・実装済み。`docs/design/hnsw-search-engine-wiring.md`）、
世代整合キャッシュ（#408）、RLS 統合・切替（#409／#410）、
`EXPLAIN` 露出（#411・実装済み。`docs/design/explain-search-engine-exposure.md`
参照）、Recall ゲート接続（#412）、前後比較（#413）、永続化は
いずれも別タスクの担当であり、本タスクは `hnsw.rs`／`hnsw/parallel_build.rs`
内部に閉じた実装（wire／SQL に露出しない・`wire_code` を新設しない）に留める。

参考にした外部実装は手法名・ライセンスのみ（コード転記なし）: pgvector
`hnswbuild.c`（要素単位ロック・エントリポイント更新のみ排他というロック粒度
設計。PostgreSQL License）・qdrant（先頭少数点を単一スレッドで構築し孤立成分
の発生を防ぐ逐次プレフィックス方式。Apache-2.0）。

## 公開 API

```rust
pub const SEQUENTIAL_PREFIX_NODES: usize = 256;
pub const MAX_BUILD_THREADS: usize = 16;

impl HnswIndex {
    // 既存（#404）。単一スレッド・完全決定的。
    pub fn build(params: HnswParams, dim: u32, vectors: &[f32], seed: u64) -> Result<Self, HnswError>;

    // threads を明示。threads==1 または n<=SEQUENTIAL_PREFIX_NODES では
    // build と完全に同一のグラフを返す。
    pub fn build_with_threads(
        params: HnswParams, dim: u32, vectors: &[f32], seed: u64, threads: usize,
    ) -> Result<Self, HnswError>;

    // parallel_search::thread_count_for + WorkerBudgetGuard と同じ決定方法・
    // 予算調停でスレッド数を決めて build_with_threads を呼ぶ。
    pub fn build_parallel(params: HnswParams, dim: u32, vectors: &[f32], seed: u64) -> Result<Self, HnswError>;
}
```

`HnswError` に `WorkerPanicked`（構築ワーカーの panic・ロック poison。
fail-closed）を追加した。既存の `search`・`neighbors` 等の公開シグネチャは
変更していない。

## 設計

### 方針の要約

| 項目 | 決定 |
| --- | --- |
| 層割当 | 並列化の前に `seed` から全ノードのレベルを**逐次**確定（`levels: Vec<usize>`）。スレッド数に依らず不変 |
| 挿入順 | 先頭 `SEQUENTIAL_PREFIX_NODES`（256）件は逐次挿入。残りは `AtomicUsize::fetch_add` によるワークスティール方式（挿入順は非決定的） |
| ロック粒度 | ノード単位 `RwLock<Vec<Vec<u32>>>`（層別隣接リストをまとめて 1 ロック）。読み取りは read ロック下で隣接 id をスクラッチ `Vec<u32>` へコピーして即解放し、スコア計算はロック外で行う。書き込みは当該ノード 1 件の write ロック下で「読み→再選択→書き戻し」を原子的に行う |
| エントリポイント | `RwLock<Option<u32>>`。`level > 現在のエントリレベル` の場合のみ書き込みロックを取り、**再読込のうえ現在のエントリレベルより高い場合だけ**更新する（pgvector 由来のロック粒度） |
| デッドロック回避 | 同時に 2 つ以上のノードロックを保持しない。`connect(a,b)` は `a` のみ、`shrink_links(x)` は `x` のみを個別にロック・解放する |
| スレッド数 | `parallel_search::thread_count_for`・`WorkerBudgetGuard`・`MAX_THREADS_PER_QUERY` を `pub(crate)` 化して再利用（single source of truth。構築側は別名 `MAX_BUILD_THREADS` で同値を持つ） |
| 決定性契約 | `build`（既存 API）は従来どおり単一スレッド・完全決定的のまま維持。`build_with_threads(threads>=2)`／`build_parallel` は挿入順序が非決定的になるためグラフの**形状**は run-to-run で変わり得る（探索の決定性契約「同一索引・同一クエリで再現」は構築方式に依らず不変） |
| 失敗契約 | ワーカーの `Err` は最初の 1 件を保存し `AtomicBool` 停止フラグで全ワーカーを早期終了 → 全ハンドル join 後に `Err` を返す。ワーカー panic・`RwLock` poison は `HnswError::WorkerPanicked` へ fail-closed（部分索引を返さない） |

### コード共有（`LinkStore` 抽象の代わりに純粋関数を共有）

計画段階では `LinkStore` trait による `insert_node`／`search_layer` 等の
ジェネリック共有を検討したが、実装時に次の事実に気づいた: `HnswIndex::score`・
`select_neighbors_heuristic`・`shrink_links` の計算本体はいずれも `self` の
フィールドを実質参照しない（`score`・`select_neighbors_heuristic` は完全に
無関係、`shrink_links` は「現在のリンクを読む→再選択を計算→書き戻す」のうち
中央の計算のみが共有可能）。そこで `hnsw.rs` へ以下の純粋関数を切り出し、
逐次経路（`HnswIndex` のメソッド。挙動は完全に不変であることを既存テストで
確認済み）と並列経路（`parallel_build::BuildGraph`）の双方から共有した:

- `score_of(vectors, dim, node, query) -> Result<f32, HnswError>`
- `select_neighbors_heuristic_free(candidates, m, dim, vectors) -> Result<Vec<u32>, HnswError>`
- `compute_shrink(current_links, node, dim, vectors, limit, protect) -> Result<Option<Vec<u32>>, HnswError>`
  （`None` は「変更不要」。読み書き分離のため、逐次経路は `&mut self.nodes`
  への読み書き 2 回、並列経路は 1 回の書き込みロック内で使う）

`greedy_descend`・`search_layer`・`insert_node` 自体は、逐次経路
（`self.neighbors()` を直接参照する既存実装。**完全に無変更**）と並列経路
（`BuildGraph::neighbors_copy`（ロック越しのコピー）を介する
`greedy_descend_locked`・`search_layer_locked`・`insert_node_locked`。
`parallel_build.rs`）とで、アルゴリズム（停止・受理判定・順序規約）を
完全に同一に保ちつつ実装を分離した。`build_with_threads(threads=1)` が
`build`（逐次経路）をそのまま呼ぶため、この分離によって単一スレッド経路の
挙動が変わることはない（`build_with_threads_one_matches_sequential_build_exactly`
で機械検証済み）。

### 並列固有のレース（実装中に発見・修正）

逐次版 `insert_node` は、挿入ノード自身の隣接リスト（`connect(node_id,
neighbor)` で構築される）を `select_neighbors_heuristic` の選択結果
（`<= params.m` 件）にそのまま委ね、自身へのシュリンクを呼ばない。これは
逐次実行では安全——挿入ノード自身の番が終わるまで他のどの挿入もそのノードの
リストへ触れないため。

並列実行では、ノード X の挿入処理が進行中でも、X が既にグラフへ部分的に
（上位層で）結線済みであれば、**別の**ノード Z の挿入がその瞬間に X を
発見して `connect(X, Z)`（X の逆方向リンク追加）→ `shrink_links(X,
protect=Z)` を行い得る。X 自身の挿入処理の残りの層でさらに `connect(X,
own_neighbor)` を呼ぶと、Z 由来のエントリが縮退で保護される保証がないまま
「X 自身が選んだ `m` 件」＋「Z からの逆方向リンク」が同時に存在し得る状態が
生まれ、`shrink_links` を挟まないと次数上限を超過する
（実装時に不変条件テストで再現・確認: `node 405 layer 1 exceeds degree
limit: 7 > 6`）。

対応として、`insert_node_locked` は各層の隣接構築ループの末尾で
`graph.shrink_links(node_id, l, node_id)` を追加で呼ぶ。`protect=node_id`
自身を渡すことで `compute_shrink` の `node != protect` 分岐が成立せず、
強制保護なしの純粋な「上位 `limit` 件を残す」縮退として働く。無競合時は
`current_links.len() <= limit` のため `compute_shrink` が `None` を返し
no-op（逐次経路との性能差は生じない）。

### 挿入時のリンク保証（Issue #448）

並列時、エントリ更新の競合（複数ワーカーが同時に新最大層を持つノードを
挿入）や下記 H-N の窓により上位層・層 0 に到達不能ノードが残り得るが、
`repair_reachability`（凍結後・単一スレッド）が最終的に閉じる。Issue
\#448 で `insert_node_locked` を `plan_links`（探索・選択・自ノードの
外向きリンク）／`publish_links`（逆方向リンク・shrink・エントリ昇格）の
2 パスへ分離し、挿入完了直前に選択近傍の少なくとも 1 つが逆方向リンクを
保持することを確認・必要なら再結線する保証（`ensure_reverse_link`）を
追加した——詳しくは下記「Issue #448 追記」節参照。これにより並列由来の
到達不能ノード発生自体を抑制し、`repair_reachability` の修復量（≒ 反復
回数）を減らす。到達不能ノードの発生を完全にゼロにする保証ではなく、
`repair_reachability` は引き続き必要。

## 対象ファイル

| パス | 内容 |
| --- | --- |
| `crates/engine/src/hnsw.rs` | `SEQUENTIAL_PREFIX_NODES`／`MAX_BUILD_THREADS` 定数、`HnswError::WorkerPanicked`、`build_with_threads`／`build_parallel`、`validate_build_input`／`score_of`／`select_neighbors_heuristic_free`／`compute_shrink`（逐次・並列で共有する純粋関数）、`mod parallel_build;` |
| `crates/engine/src/hnsw/parallel_build.rs`（新規） | `BuildGraph`（ノード単位 `RwLock`・エントリポイント `RwLock`）、`greedy_descend_locked`／`search_layer_locked`／`insert_node_locked`、`build_parallel_graph`（逐次プレフィックス→ワークスティール並列→凍結→`repair_reachability`）、ユニットテスト（poison→`WorkerPanicked`・`threads=1` と `build` の完全一致） |
| `crates/engine/src/parallel_search.rs` | `thread_count_for`・`WorkerBudgetGuard`・`MAX_THREADS_PER_QUERY` を `pub(crate)` 化（挙動は不変。`hnsw::parallel_build` と共有する旨をコメントに追記） |
| `crates/engine/tests/hnsw.rs` | 並列構築の不変条件テスト（`parallel_build_invariants` モジュール: レベル割当のスレッド数不変・次数上限/連結性/重複ヘビーコーパス・ワーカーエラー伝播・`threads` の範囲検証） |
| `crates/engine/tests/hnsw_search.rs` | 受け入れ条件 (a): 逐次 vs 並列（`threads=4`）の Recall@10 が `parallel >= sequential - 0.02` であることの層 A テスト、並列構築索引に対する探索決定性テスト |
| `crates/engine/benches/hnsw_parallel_build_bench.rs`（新規） | 受け入れ条件 (b): rows（既定 100,000・`BENCH_HNSW_PARALLEL_ROWS` で上書き可）× スレッド数ラダー（既定 `[1, 2, 4, .., available_parallelism]`・`BENCH_HNSW_PARALLEL_THREADS` で上書き可）で構築時間中央値・speedup を計測する手動専用ベンチ |
| `crates/engine/Cargo.toml`・`Makefile` | `[[bench]] name = "hnsw_parallel_build_bench"`（`harness = false`／`test = false`）・`make bench-hnsw-parallel-build` ターゲット（`ci` 非包含・CI ワークフロー非配線） |
| `crates/engine/src/hnsw.rs`（Issue #406 追記） | 段別プロファイル観測用フック `build_with_threads_observed`（`build_with_threads` 本体は無変更） |
| `crates/engine/benches/hnsw_parallel_build_bench.rs`（Issue #406 追記） | 段別内訳（`level`／`prefix`／`parallel`／`freeze`／`repair`）・ワーカー統計（`inserted`／`busy`／`lock_blocked_ratio`／`entry_promotions`）・対照負荷 `dot_scan` の計測を追加 |
| `crates/engine/benches/harness/hnsw_parallel_profile.rs`（新規・Issue #406 追記） | 段別内訳・ワーカー統計・対照負荷の計測ハーネス |
| `crates/engine/benches/hnsw_compare_bench.rs`（新規・Issue #406 追記） | usearch（`=2.26.1`）との構築時間・Recall@10・探索レイテンシ比較。手動専用（`make bench-hnsw-compare`） |
| `crates/engine/benches/harness/hnsw_compare.rs`（新規・Issue #406 追記） | usearch 比較の計測ハーネス（パラメータ等価表・並列 add 方式） |
| `crates/engine/tests/hnsw_parallel_profile_accept.rs`（新規・Issue #406 追記） | 段別プロファイル観測用フックの受け入れテスト |
| `crates/engine/tests/hnsw_compare_accept.rs`（新規・Issue #406 追記） | usearch 比較ハーネスの受け入れテスト |
| `Makefile`（Issue #406 追記） | `make bench-hnsw-compare` ターゲット（`ci` 非包含・CI ワークフロー非配線） |
| `crates/engine/Cargo.toml`（Issue #406 追記・2026-09-05） | `hnsw_rs` `=0.3.4` 追加は撤去済み（2026-09-05・理由: 実測で構築 3.4〜4.8 倍・探索約 5 倍遅く、対照は usearch で足りる。`deny.toml` ignore・計測時間倍増も解消） |
| `deny.toml`（Issue #406 追記・2026-09-05） | `RUSTSEC-2025-0141`（`bincode` 1.3.3・unmaintained）の ignore 追加は撤去済み（2026-09-05。`hnsw_rs` の dump/reload 経路のみが依存し本ベンチでは未使用だったため） |
| `crates/engine/benches/hnsw_compare_bench.rs`・`crates/engine/benches/harness/hnsw_compare.rs`（Issue #406 追記・2026-09-05） | 3 エンジン対比実装は撤去済み（2026-09-05）。現在は usearch のみを対照とし、L2 正規化コーパス方式は維持 |
| `crates/engine/src/hnsw.rs`（Issue #447 追記） | `HnswRepairLevelStats`／`HnswRepairStats`（`HnswBuildProfile.repair` フィールド追加）、`PRECISE_REPAIR_CAP` をモジュール `pub const` へ昇格、`repair_reachability_inner<const OBSERVE: bool>`（`repair_reachability`／`repair_reachability_observed` の共有本体）、`build_inner<const OBSERVE: bool>`（`build`／`build_observed` の共有本体）、`build_with_threads_observed` 縮退分岐の `profile.repair` 補完 |
| `crates/engine/src/hnsw/parallel_build.rs`（Issue #447 追記） | `build_parallel_graph_observed` の `repair_reachability_observed` 呼び出しへの置換・`HnswBuildProfile` リテラルへの `repair` 追記、ユニットテスト 2 本追加（段別 wall の入れ子整合・縮退経路の repair 補完） |
| `crates/engine/benches/harness/hnsw_parallel_profile.rs`（Issue #447 追記） | repair 統計の集計・整形関数群（`repair_phase_wall_sum`・`repair_wall_gap`・層横断合計 4 関数・`repair_unreachable_per_level_min_med_max`・`format_per_level`） |
| `crates/engine/benches/hnsw_parallel_build_bench.rs`（Issue #447 追記） | repair 統計行（層別到達不能ノード数・反復回数・段別壁時間の min/med/max）の出力追加 |
| `crates/engine/tests/hnsw_parallel_profile_accept.rs`（Issue #447 追記） | 上記 harness 新関数の回帰テスト |
| `crates/engine/tests/hnsw.rs`（Issue #447 追記） | 重複ヘビーコーパスでのフェーズ 2 非 vacuous 性・逐次経路での repair 統計の決定性を固定するテスト 2 本 |
| `crates/engine/src/hnsw/parallel_build.rs`（Issue #448 追記） | `insert_node_locked` の `plan_links`／`publish_links` 分離、`BuildGraph::has_link`・`ensure_reverse_link`（新設）、observe 限定診断カウンタ、単体テスト 4 本＋informational テスト 1 本 |
| `crates/engine/src/hnsw.rs`（Issue #448 追記） | `HnswWorkerStats` へ `degenerate_layer_searches`／`reverse_link_reconnects` フィールド追加 |
| `crates/engine/tests/hnsw_parallel_profile_accept.rs`（Issue #448 追記） | ヘルパの構造体リテラルへ新フィールド追随 |
| `crates/engine/tests/hnsw_search.rs`（Issue #448 追記） | `parallel_build_recall_at_10_matches_sequential_build_within_margin` へ threads=12 判定を追加 |

## 検証

### 不変条件（`tests/hnsw.rs::parallel_build_invariants`）

- `build_with_threads(.., 1)` は `build` と完全に同一のグラフ（`hnsw/
  parallel_build.rs` のユニットテストで直接検証、`tests/hnsw.rs` 側は
  `threads` 0 件・上限超過の拒否のみ）
- `n <= SEQUENTIAL_PREFIX_NODES` では `threads` を変えても `build` と同一
- `threads = 4`・複数 seed × 複数 `(dim, rows, m)`（`n > SEQUENTIAL_PREFIX_NODES`）
  で: 全ノードのレベルがスレッド数に依らず一致、次数上限・自己ループなし・
  重複なし・隣接先の層整合、エントリポイントからの全層連結性、重複ヘビー
  コーパスでも同様
- オーバーフロー誘発の `NonFiniteScore` を `threads=4` で構築 → panic せず
  `Err` が返る（ワーカーのエラー伝播・停止フラグ）

### 受け入れ条件 (a): Recall 同水準（`tests/hnsw_search.rs`）

層 A（常時実行・N=`SEQUENTIAL_PREFIX_NODES + 1,800`・dim=32・20 クラスタ）で
同一フィクスチャ・同一クエリ集合に対し `build`（逐次）と
`build_with_threads(.., 4)` の Recall@10（ef=64/256）を算出し、
`parallel >= sequential - 0.02` を確認した。実測は両者とも同水準
（クラスタ構造ありフィクスチャでは逐次側が既に高水準のため、並列側の
低下はマージン内に収まる）。並列構築索引に対する探索の決定性
（同一索引・同一クエリでの結果再現）もあわせて固定した。

### 受け入れ条件 (b): 100k 点のスレッド数ラダーベンチ

`BENCH_HNSW_PARALLEL_THREADS=1,4,8,12 make bench-hnsw-parallel-build`
（本開発環境・12 論理コア・x86_64 AVX2+FMA・CPU のみ。rows=100,000・
dim=64・既定パラメータ。各点 warmup 20 回・計測 20 回の中央値）を
2026-09-04 に 2 回実測した。実装時点（PR #431）では共有計測環境の負荷により
`threads=1` の基準点しか実測できず運用者作業として申し送っていた分の補完
である（1 回目は他ジョブ〔`cargo test`〕と並走した loadavg 約 4〜7 の
状態、2 回目は loadavg 約 2〜3 の比較的静かな状態で計測）:

| threads | 1 回目 median | 1 回目 speedup | 2 回目 median | 2 回目 speedup |
| --- | --- | --- | --- | --- |
| 1 | 10,804.2 ms | 1.000x | 10,064.8 ms | 1.000x |
| 4 | 3,060.5 ms | 3.530x | 3,240.1 ms | 3.106x |
| 8 | 2,112.5 ms | 5.114x | 2,547.5 ms | 3.951x |
| 12 | 2,095.6 ms | 5.156x | 2,102.0 ms | 4.788x |

100k 点でもスレッド数に応じて構築時間が短縮することを確認した（受け入れ
条件 (b)。実装時点の `threads=1` 基準点 11,707.7 ms とも同水準）。
speedup は 4 スレッドまでほぼ線形（3.1〜3.5x）で、8→12 スレッドでは
2 回とも約 2.1 s に収束し、伸びが頭打ちになる。頭打ちの要因の段別内訳は
下記「Issue #406 追記（2026-09-05）」節で実測した（`repair_reachability`
の単一スレッド後始末が支配的で、当初の推定にあった「12 論理コア
（物理コアは半数）での SMT」という記述は、ゲスト内 `lscpu` が
`Thread(s) per core: 1` を報告しており誤りだったため訂正する。ホスト側の
物理コア共有の有無はゲストから直接は観測できず、対照負荷の speedup 天井
からの間接推定に留まる）。
2 回の実測差（`threads=8` で 2,112 ms vs 2,548 ms）は共有環境の
run-to-run 変動の範囲として扱い、閾値判定には用いない（本ベンチは
情報提供専用で spec 閾値を持たない）。

小規模スモーク実測（rows=5,000・dim=64。実装時点の計測）:

| threads | median | speedup |
| --- | --- | --- |
| 1 | 263.6 ms | 1.000x |
| 2 | 158.1 ms | 1.668x |
| 4 | 88.0 ms | 2.995x |

### Issue #494 追記: 凍結後 CSR 化に伴う `flatten` 段の追加

凍結時の CSR 平坦化（`docs/design/hnsw-index.md` §14・Issue #493・#494）の
実装に伴い、`HnswBuildProfile` へ `flatten`（可変長ビルダー表現
`GraphBuilder` から CSR `csr::CsrGraph` への平坦化。`freeze`・
`repair_reachability` の両方が完了した後の最終段）を追加した。以下の
「Issue #406 追記」節の段別内訳表（`level`／`prefix`／`parallel`／
`freeze`／`repair`）は本 Issue 以前（`flatten` 段が存在しない時点）の実測
であり、`flatten` を含まない。`flatten` を含む前後比較の再実測は #495 の
担当（`benches/harness/hnsw_parallel_profile.rs::serial_share` への
`flatten` 加算も含む）。逐次縮退経路（`threads == 1` または
`n <= SEQUENTIAL_PREFIX_NODES`）はこの区切りが存在しないため `flatten` は
`Duration::ZERO` のまま（`sequential_prefix` へ全量を積む既存規約）。

### Issue #495 追記: `flatten` 段を含む段別内訳・前後比較実測

`docs/design/hnsw-index.md` §14.13 の前後比較実測（before `929c027`→after
`ad484e7`〔PR #590 マージコミット。`crates/engine/src/`・`Cargo.lock` は
`cadf6c3`〔#494 適用後〕と同一で production コードとしては #494 適用後の
状態を表す。詳細は §14.13「比較対象・環境」参照〕。N=5 ペア・共有 QEMU
環境の参考値。詳細な表・判定は同節参照）から、
`flatten` 段を含む after 側（CSR 化後）の段別内訳（rows=100,000・dim=64。
上記「Issue #406 追記」節の run 番号に続けて記録）を示す。各列は 5 run の
median を個別に集計した値（`docs/design/hnsw-index.md` §14.13 と同一の
ログから抽出。総和と `total` の差は測定区間外のオーバーヘッドを含む）:

| threads | total median | level median | prefix median | parallel median | freeze median | repair median | flatten median | serial_share（概算） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 9,891.3ms | 0.000ms | 9,889.3ms | 0.000ms | 0.000ms | 0.000ms | 0.000ms | 99.98% |
| 12 | 2,339.3ms | 1.05ms | 6.0ms | 1,461.4ms | 0.61ms | 853.3ms | 8.96ms | 37.19% |

`flatten` は threads=12 で median 約 9ms（total の約 0.4%）——「Issue #406
追記」の頭打ち要因分析（`repair_reachability` が支配的）を変える規模ではない。
before（`flatten` フィールド自体が存在しない旧アリティ）との比較は `total`・
`repair_reachability` の実測値のみで行う（`docs/design/hnsw-index.md`
§14.13 参照。固定帯 ±5%・実測帯〔参照区間 `dot_scan` の run-to-run 幅〕の
両方を判定基準とし、両者ともノイズ帯内で一貫した悪化・改善は観測されな
かった。`serial_share` は定義差のため before/after で生比較しない）。

### Issue #495 追記: 現行ベンチ（L2 正規化コーパス）での usearch 探索レイテンシ前後比較

`docs/design/hnsw-index.md` §14.13 の前後比較実測から、現行の
`make bench-hnsw-compare`（自作・usearch 2 エンジン・L2 正規化コーパス。
`run9`・`run10` と同一条件）での自作／usearch 探索レイテンシ中央値を
before／after（CSR 化前後）で記録する。**旧実測「自作 66〜67µs／usearch
76〜77µs」（`run7`・`run8`。非正規化コーパス時代）とは条件が異なるため
差分計算はしない**——本節は現行条件での CSR 化前後比較のみを目的とする。

| コミット | 自作 median | usearch median（参照。CSR 非依存） |
| --- | --- | --- |
| before（`929c027`） | 67.302µs | 80.917µs |
| after（`ad484e7`。production コードは `cadf6c3` と同一） | 71.035µs | 95.202µs |

自作・usearch とも after 側が高めに出ているが、`docs/design/hnsw-index.md`
§14.13 の判定基準（固定帯 ±5%・実測帯〔usearch 自身の run-to-run 幅。本
条件では ±164.7%〕の両方を超えて初めて有効な変化として扱う）を踏まえると
「ノイズ帯内」判定であり、CSR 化由来の系統的な探索レイテンシ悪化とは
判断できない（usearch 側〔CSR 非依存〕も同方向に上昇しており、環境側の
負荷変動が主要因と考えられる）。

### Issue #406 追記（2026-09-05）: 8→12 スレッド頭打ちの段別内訳

「受け入れ条件 (b)」で観測した 8→12 スレッドの伸び悩みについて、構築の
各段（レベル割当・逐次プレフィックス・並列挿入・凍結・`repair_reachability`）
を計測できる観測用フック `HnswIndex::build_with_threads_observed`
（本 PR で追加。`build_with_threads` の挙動・ロック取得方式は不変で、
計装分岐は `observe=false` の production 経路では無効）を用いて段別内訳を実測した。

**計測方法修正前の実測は破棄した**（codex-review 指摘対応。以下は
計測方法修正後の再実測のみを記載する）。

計測方法の修正点は次のとおり:

- 段別中央値・ワーカー統計を warmup 除外の計測 20 回のみから算出する
  （修正前は warmup を含めていた疑いがあり除外を明示化）
- 並列段（`parallel_speedup`）と対照負荷 `dot_scan` の speedup を同一基準
  （threads=2）で正規化し、`parallel_vs_control` として比較可能にする
  （修正前の「並列段の単一スレッド換算 speedup」は定義上 `dot_scan` 側と
  基準が揃っておらず比較の妥当性が不明瞭だった）
- usearch 比較（`make bench-hnsw-compare`）で索引の drop（解放）を
  計測区間外へ移し、構築時間のみを比較する
- ワーカー統計にロック累積待ち時間（`lock_wait[sum/max]`・
  `lock_wait_share`＝Σ待ち時間÷Σbusy）を追加し、`lock_blocked_ratio`
  （試行回数ベース）だけでは見えない競合の重みを可視化する
- 段別中央値（`level`／`prefix`／`parallel`／`freeze`／`repair`／`total`）の
  定義を `stats::summarize` と同じ線形補間方式へ統一する（codex-review
  追加指摘対応。修正前は段によって中央値の算出方法が揃っておらず、
  高負荷下では `prefix` の中央値が `total` の中央値を上回る逆転が
  起こり得た）
- `serial_share`（≒ `repair` の `total` に対する比率）・`total_speedup` の
  分母を、索引の drop（解放）を含まない `HnswBuildProfile.total` の
  中央値へ変更する（codex-review 追加指摘対応。外側 `protocol::run` が
  計測する壁時計値は索引 drop を含み構築本体の比較には適さないため、
  `wall_median_with_drop` として参考値に降格する）

計測条件: `BENCH_HNSW_PARALLEL_THREADS=1,2,4,8,12
make bench-hnsw-parallel-build`、rows=100,000・dim=64・既定パラメータ、
各点 warmup 20 回・計測 20 回の中央値を 2 回実測（run5・run6。
2026-09-05）。環境は「受け入れ条件 (b)」と同一の QEMU ゲスト
（`lscpu`: `Thread(s) per core: 1`・1 ソケット 12 コア・L3 16 MiB・
NUMA 1 ノード、x86_64 AVX2+FMA）。run5 は loadavg 約 2.0〜5.6 で推移し、
run6 は threads=1 計測時のみ loadavg 7.2 とやや高いがそれ以外は
2.0〜6.1 で推移した。中央値定義を統一した結果、`prefix <= total` が
両回とも成立する（threads=1 の `serial_share` は run5・run6 とも 99.98%）。

#### 段別内訳（run5）

| threads | total | level | prefix | parallel | freeze | repair | serial_share | parallel_speedup | total_speedup | wall_median_with_drop |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 9,900.758 ms | 0.000 ms | 9,898.685 ms | 0.000 ms | 0.000 ms | 0.000 ms | 99.98% | 1.000x | 1.000x | 9,907.346 ms |
| 2 | 5,242.254 ms | 1.043 ms | 6.035 ms | 5,010.945 ms | 0.701 ms | 222.199 ms | 4.39% | 1.000x | 1.889x | 5,247.828 ms |
| 4 | 3,051.402 ms | 1.044 ms | 6.051 ms | 2,606.770 ms | 0.586 ms | 426.828 ms | 14.24% | 1.922x | 3.245x | 3,057.302 ms |
| 8 | 2,087.887 ms | 1.044 ms | 6.031 ms | 1,440.116 ms | 0.537 ms | 635.401 ms | 30.80% | 3.480x | 4.742x | 2,093.610 ms |
| 12 | 2,069.589 ms | 1.045 ms | 6.056 ms | 1,245.159 ms | 0.581 ms | 808.091 ms | 39.42% | 4.024x | 4.784x | 2,075.526 ms |

#### 段別内訳（run6）

| threads | total | level | prefix | parallel | freeze | repair | serial_share | parallel_speedup | total_speedup | wall_median_with_drop |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 9,859.958 ms | 0.000 ms | 9,857.940 ms | 0.000 ms | 0.000 ms | 0.000 ms | 99.98% | 1.000x | 1.000x | 9,866.442 ms |
| 2 | 5,237.350 ms | 1.043 ms | 6.049 ms | 5,006.212 ms | 0.657 ms | 220.616 ms | 4.36% | 1.000x | 1.883x | 5,242.796 ms |
| 4 | 3,030.780 ms | 1.045 ms | 6.049 ms | 2,591.570 ms | 0.674 ms | 423.653 ms | 14.23% | 1.932x | 3.253x | 3,036.301 ms |
| 8 | 2,068.200 ms | 1.042 ms | 6.060 ms | 1,436.720 ms | 0.543 ms | 620.578 ms | 30.38% | 3.484x | 4.767x | 2,074.091 ms |
| 12 | 2,062.867 ms | 1.042 ms | 6.042 ms | 1,243.845 ms | 0.607 ms | 804.320 ms | 39.36% | 4.025x | 4.780x | 2,068.870 ms |

`parallel_speedup` は `parallel_base_threads=2` を基準（threads=2 が
1.000x）とした並列挿入段のみの speedup（`level`／`prefix`／`freeze` は
スレッド数に依らずほぼ一定のため対象外）。`wall_median_with_drop` は
外側 `protocol::run` が計測した壁時計中央値（索引 drop を含む）の
参考値であり、`serial_share`／`total_speedup` の算出には用いない。

#### ワーカー統計（run5）

| threads | inserted[min/med/max] | busy[min/med/max] | lock_blocked | lock_wait[sum/max] | lock_wait_share |
| --- | --- | --- | --- | --- | --- |
| 2 | 49,689/49,872/50,055 | 5,004.098/5,004.142/5,004.186 ms | 0.00% | 0.462/0.306 ms | 0.00% |
| 4 | 24,750/24,935/25,124 | 2,609.740/2,609.814/2,609.878 ms | 0.01% | 1.258/0.367 ms | 0.01% |
| 8 | 7,217/13,196/13,367 | 1,442.451/1,442.552/1,442.857 ms | 0.01% | 6.958/1.566 ms | 0.06% |
| 12 | 7,635/8,270/9,152 | 1,255.706/1,256.101/1,256.438 ms | 0.02% | 507.774/67.137 ms | 3.37% |

#### ワーカー統計（run6）

| threads | inserted[min/med/max] | busy[min/med/max] | lock_blocked | lock_wait[sum/max] | lock_wait_share |
| --- | --- | --- | --- | --- | --- |
| 2 | 49,788/49,872/49,956 | 5,006.804/5,006.865/5,006.926 ms | 0.00% | 0.406/0.222 ms | 0.00% |
| 4 | 24,818/24,962/25,003 | 2,576.142/2,576.173/2,576.261 ms | 0.01% | 1.365/0.422 ms | 0.01% |
| 8 | 7,328/13,220/13,372 | 1,434.457/1,434.645/1,434.847 ms | 0.02% | 5.852/0.876 ms | 0.05% |
| 12 | 7,440/8,483/9,009 | 1,243.319/1,243.640/1,243.830 ms | 0.02% | 462.682/45.972 ms | 3.10% |

`lock_wait_share` は Σ`lock_wait` ÷ Σ`busy` で定義する
（`lock_blocked_ratio` の試行回数ベース比率とは異なる指標）。
`entry_promotions`（エントリポイント更新回数）は全点・両回とも 3 で不変だった。

#### 対照負荷（`dot_scan`）との比較

`dot_scan` は共有可変状態を持たない、コーパス全行とクエリ行の内積走査
（64 パス）で、ハードウェアの並列度天井の参考値として計測している。
`parallel_vs_control` は `parallel_speedup / (control_median(threads=2)
/ control_median(threads=T))` で定義し、並列挿入段と対照負荷の伸びを
同一基準（threads=2）に正規化した比（1.0 なら並列段が対照負荷と同じ
伸び方をしている）。`speedup_ref(basis=threads=1)` は参考値として
threads=1 基準の素の speedup も併記する。

| threads | run5 dot_scan median | run5 speedup_ref(基準 threads=1・参考) | run5 parallel_vs_control(基準 threads=2) | run6 dot_scan median | run6 speedup_ref(基準 threads=1・参考) | run6 parallel_vs_control(基準 threads=2) |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 49.487 ms | 1.000x | — | 46.491 ms | 1.000x | — |
| 2 | 26.775 ms | 1.848x | 1.000 | 25.582 ms | 1.817x | 1.000 |
| 4 | 12.837 ms | 3.855x | 0.922 | 11.430 ms | 4.067x | 0.863 |
| 8 | 8.977 ms | 5.513x | 1.167 | 6.921 ms | 6.717x | 0.943 |
| 12 | 6.006 ms | 8.240x | 0.903 | 6.051 ms | 7.683x | 0.952 |

threads=1 は並列段自体が存在しない（`build_with_threads(.., 1)` は
逐次経路そのもの）ため `parallel_vs_control` は算出しない。

#### 所見

1. **主因は `repair_reachability`（凍結後・単一スレッドの後始末）**。
   2→4→8→12 スレッドで 222→427→635→808 ms（run5）／
   221→424→621→804 ms（run6）と単調増加し、12 スレッドでは total の
   約 39.4%（`serial_share`）を占める。並列度が上がるほど上位層の
   到達不能ノードが増え修復量が増える構造（`hnsw.rs::repair_reachability`
   は各反復で BFS と到達済み全ノードとの dot 計算を伴う。反復上限
   `PRECISE_REPAIR_CAP` は本タスクの実装既定値）。8→12 の差分は
   run5・run6 とも並列段の短縮（約 −195 ms／−193 ms）を repair の増加
   （約 +173 ms／+184 ms）がほぼ相殺し、total は横ばい
   （run5: 2,088→2,070 ms・run6: 2,068→2,063 ms）。計測方法修正後の
   再実測 2 回とも「8→12 で伸びない」現象が再現した。
2. **逐次プレフィックス（256 件・約 6 ms）・レベル割当（約 1 ms）・
   凍結（1 ms 未満）はいずれも合計 8 ms 未満で無視できる**。旧来の
   「逐次プレフィックスが要因候補」という推定は否定できる。
3. **ロック競合は試行回数ベースでは軽微だが、累積待ち時間で見ると
   12 スレッドで無視できない水準になる**。`lock_blocked_ratio` は全点で
   0.00〜0.02% に留まるが、累積待ち時間で見ると 8 スレッドまでは
   Σ6〜7 ms（busy 比 0.05〜0.06%）に対し、12 スレッドでは
   Σ463〜508 ms（busy 比 3.10〜3.37%・ワーカー最大待ち時間 46〜67 ms）
   へ急増し、12 スレッドで初めて無視できない競合が現れる。ただし
   並列段全体（約 1,245〜1,256 ms）の 3% 程度に留まり、主因ではない。
4. **並列段は対照負荷にほぼ追随している**。同一基準（threads=2）で
   正規化した `parallel_vs_control` は 4/8/12 スレッドで
   0.92/1.17/0.90（run5）・0.86/0.94/0.95（run6）とおおむね 1.0 に
   収まる。参考値（threads=1 基準の `speedup_ref`）では、12 スレッドの
   対照負荷 speedup は run5 で 8.2x・run6 で 7.7x であり、12 vCPU の
   天井に対し並列段自体はほぼ追随していると言える。
5. **ワーカー間の挿入件数に偏りがあるが busy 時間はほぼ揃う**。
   8 スレッドで min 約 7.2〜7.3k・max 約 13.4k、12 スレッドで
   7.4k〜9.2k の範囲に偏るのに対し、busy はワーカー間でほぼ等しい
   （ワークスティール方式のため終了時点が揃う）。偏り自体は vCPU
   ごとの実効速度差（ホスト側スケジューリング）を示唆するが、ゲスト内
   からは要因を切り分けて検証できない。
6. 改善余地として `repair_reachability` の並列化、または挿入時の上位層
   リンク保証による到達不能ノード発生自体の抑制が考えられるが、本追記の
   スコープでは実装しない（別 Issue 起票の要否はオーナー判断）。

注記: run6 の threads=1 点は loadavg 7.2 とやや高い環境で計測している
が、`total` の中央値は静かな環境の run5（9,900.758 ms）とほぼ同値
（9,859.958 ms）であり、代表性に問題はない。

### Issue #447 追記（2026-09-07）: repair_reachability の修復対象ノード数・反復回数（run7・run8）

Issue #406 追記の所見 1（`repair_reachability` がスレッド数の増加に伴い
単調増加する）を、内訳（層ごとの到達不能ノード数・フェーズ 1 反復回数・
フェーズ 2 結線数・段別壁時間）まで踏み込んで検証した。

**観測フックの分離方針**: `hnsw.rs::repair_reachability_inner<const
OBSERVE: bool>` を新設し、`OBSERVE=false`（`build`・`build_with_threads`・
`parallel_build::freeze` が使う非観測経路）では計測分岐（`Instant::now()`・
カウンタ更新）が単相化により一切残らない設計とした。観測版
（`repair_reachability_observed`・`pub(crate)`）は
`HnswIndex::build_with_threads_observed` の並列経路（内部で
`parallel_build::build_parallel_graph_observed` を呼ぶ）へ結線。
**縮退経路（threads=1 または `n<=SEQUENTIAL_PREFIX_NODES`）**は
`HnswIndex::build_observed`（`build` と完全に同一のグラフを返す薄い
ラッパ。`build_inner<const OBSERVE: bool>` を `build` と共有）を新設して
threads=1 基線を取得し、既存フィールド（`repair_reachability`＝ゼロ・
`workers`＝空）の意味は変更せず、`profile.repair` のみを追加で埋める。

計測条件: `BENCH_HNSW_PARALLEL_THREADS=1,2,4,8,12
make bench-hnsw-parallel-build`、rows=100,000・dim=64・既定パラメータ、
各点 warmup 20 回・計測 20 回を 2 回実測（run7・run8・2026-09-07）。
環境は「受け入れ条件 (b)」・Issue #406 追記と同一の QEMU ゲスト
（12 vCPU）だが、他 Issue の並列実装が同時実行中で loadavg が
6.3〜23.1（run7）・7.4〜19.2（run8）とやや高い（`noise` 行に実測
loadavg・rss を記録。`total`／`repair` の絶対値は Issue #406 追記の
run5・run6（loadavg 2.0〜7.2）より全般に長いが、内訳の相対的な傾向
（後述の所見）は両回で再現しており、本 Issue の主目的（修復対象
ノード数・反復回数の内訳把握）には支障がない）。

#### 段別内訳・repair 内訳（run7）

| threads | repair(外側) | repair_stats_wall | unreachable[per-level med] | unreachable_sum[min/med/max] | phase1_iters[min/med/max] | cap_hits | phase2_nodes[min/med/max] | phase1_wall | phase2_wall |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 0.000 ms | 209.221 ms | L0:2,L1:7,L2:0,L3:0,L4:0 | 9/9/9 | 9/9/9 | 0 | 0/0/0 | 162.777 ms | 45.659 ms |
| 2 | 354.197 ms | 354.197 ms | L0:4,L1:7,L2:0,L3:0,L4:0 | 8/11/14 | 8/11/13 | 0 | 0/0/0 | 307.120 ms | 47.956 ms |
| 4 | 620.805 ms | 620.804 ms | L0:10,L1:7,L2:0,L3:0,L4:0 | 12/17/19 | 12/17/19 | 0 | 0/0/0 | 572.833 ms | 45.179 ms |
| 8 | 756.512 ms | 756.512 ms | L0:14,L1:7,L2:0,L3:0,L4:0 | 17/21/29 | 17/21/25 | 0 | 0/0/0 | 712.965 ms | 49.944 ms |
| 12 | 1,023.806 ms | 1,023.806 ms | L0:20,L1:8,L2:0,L3:0,L4:0 | 21/27/33 | 21/26/32 | 0 | 0/0/0 | 970.897 ms | 44.500 ms |

#### 段別内訳・repair 内訳（run8）

| threads | repair(外側) | repair_stats_wall | unreachable[per-level med] | unreachable_sum[min/med/max] | phase1_iters[min/med/max] | cap_hits | phase2_nodes[min/med/max] | phase1_wall | phase2_wall |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 0.000 ms | 216.904 ms | L0:2,L1:7,L2:0,L3:0,L4:0 | 9/9/9 | 9/9/9 | 0 | 0/0/0 | 172.972 ms | 48.215 ms |
| 2 | 343.305 ms | 343.305 ms | L0:5,L1:7,L2:0,L3:0,L4:0 | 8/12/15 | 8/11/15 | 0 | 0/0/0 | 292.378 ms | 49.814 ms |
| 4 | 626.021 ms | 626.020 ms | L0:8,L1:7,L2:0,L3:0,L4:0 | 13/16/20 | 13/16/20 | 0 | 0/0/0 | 570.007 ms | 53.545 ms |
| 8 | 771.882 ms | 771.882 ms | L0:13,L1:7,L2:0,L3:0,L4:0 | 17/20/25 | 17/20/25 | 0 | 0/0/0 | 726.874 ms | 48.167 ms |
| 12 | 844.153 ms | 844.152 ms | L0:16,L1:8,L2:0,L3:0,L4:0 | 16/25/33 | 16/24/29 | 0 | 0/0/0 | 797.991 ms | 42.664 ms |

`repair(外側)` は `HnswBuildProfile.repair_reachability`（従来からの
呼び出し元計測。threads=1 の縮退経路は既存契約どおりゼロのまま）、
`repair_stats_wall` は `profile.repair.wall`（観測版本体の壁時間）。
両回・全並列点（threads>=2）で一致（誤差 0.000〜0.001 ms）しており、
入れ子区間の整合（`Σ(phase1_wall+phase2_wall) <= repair.wall <=
repair_reachability`）が実測でも成立することを確認した。

#### 所見

1. **到達不能ノード数はスレッド数の増加に伴い単調増加する**（Issue #406
   追記所見 1 の仮説を内訳レベルで裏付け）。層 0 の中央値到達不能数は
   run7 で 2→4→10→14→20、run8 で 2→5→8→13→16 とスレッド数に対し
   概ね単調増加し、層横断合計（`unreachable_sum` 中央値）も
   9→11→17→21→27（run7）・9→12→16→20→25（run8）と同様の傾向を示す。
   並列度が上がるほど挿入順が非決定的になり逆方向リンクの枝刈りで
   一時的に到達不能になるノードが増える、という Issue #406 追記の
   仮説と整合する。
2. **発生層は層 0・層 1 に限られる**。層 2 以上（本コーパス・パラメータ
   では層 4 まで存在）は両回・全 threads 点で到達不能ノード数 0 のまま
   であり、修復対象は最下層とその直上層に集中する。
3. **フェーズ 2（チェーン結線）は本コーパス・規模では一度も発火しない**
   （`phase2_nodes` は全点で 0、`cap_hits` も全点で 0）。フェーズ 1 の
   反復上限 `PRECISE_REPAIR_CAP`（64）に対し実測の反復回数は最大でも
   32（run7 threads=12 の max）に留まり、上限には遠く及ばない。
   したがってフェーズ 2 のコストは本計測条件では観測されず、
   フェーズ 1（BFS＋厳密修復ループ）がほぼ全てを占める。
4. **フェーズ 1 の壁時間が repair 全体の支配的要因**。両回・全並列点で
   `phase1_wall` が `repair_stats_wall` の約 78〜92% を占め
   （例: run7 threads=12 は 970.897ms／1,023.806ms ≈ 94.8%）、
   `phase2_wall` は 42〜54 ms とスレッド数に依らずほぼ一定（層数×
   BFS 1 回分のコストに相当し、到達不能ノード数の影響を受けない）。
   反復回数（`phase1_iters`）とフェーズ 1 壁時間の増加が対応しており、
   「反復ごとの全体 BFS＋到達済み全ノードとの dot 計算」というフェーズ 1
   の計算量特性（モジュールコメント参照）が、到達不能ノード数の増加
   （所見 1）を通じてスレッド数依存の repair 時間増加へ直結していると
   分析できる。
5. これらの観測値は #448（発生抑制）・#449（修復並列化）の設計判断の
   入力とする——発生抑制であれば層 0・層 1 の挿入時上位層リンク保証、
   並列化であればフェーズ 1 の反復ループ自体（BFS が支配的）の並列化が
   候補になる、という所見の位置づけに留め、本 Issue では実装しない。

### Issue #448 追記（2026-09-07）: 並列挿入時のリンク保証による到達不能ノード発生の抑制

Issue #447 追記の実測（並列由来の到達不能ノード発生が層 0・層 1 に集中し、
スレッド数の増加に伴い単調増加する）を受け、発生自体を抑制する対策を
実装した。

#### 機構（H-N: 発見可能だが下位層の隣接リストが空の窓）

旧・単一パス `insert_node_locked` は層を上から下へ処理し、**各層ごとに**
`connect(node→sel)` → `connect(sel→node)` → `shrink_links(sel,
protect=node)` を行っていた。このため挿入中ノード Z は、ある層 l+1 の
逆方向リンクが張られた瞬間から他ワーカーに**発見可能**になる一方、
層 l 以下の Z 自身の隣接リストはまだ**空**である窓が生じる。この窓で
別ワーカー X が Z へ降下し層 l で `search_layer_locked(entry_points=
[Z])` を実行すると `neighbors_copy(l, Z)` が空のため結果が `[Z]` のみに
退化し、X は層 l 以下の全層で Z 1 本だけに結線される。この結線は Z 自身
の後続処理や後続挿入の枝刈りで容易に失われ、X が到達不能になる。

この仮説は Issue #447 の実測（(a) 層 0 集中——層 1 で同じ窓を作るには
「レベル ≥ 2 の挿入中ノード」が必要で約 1/m の頻度、(b) スレッド数に
比例した増加——同時挿入中ノード数に比例、(c) 発生規模が数十件程度に
留まる稀な競合、の 3 点と整合する）。

#### 対策

`crates/engine/src/hnsw/parallel_build.rs::insert_node_locked` を 2 パスへ
分離した:

- `plan_links`: 探索・選択・**自ノードの外向きリンクのみ**を層ごとに
  張る（逆方向リンクは張らない）。完了時点で対象ノードは entry から
  到達不能なまま。
- `publish_links`: 層を上から下へ処理し、逆方向リンク・`neighbor` 側
  shrink・自己 shrink（既存の防御）に加え、`ensure_reverse_link` で
  選択近傍の少なくとも 1 つが対象ノードへの逆方向リンクを保持している
  ことを確認し、全て失われていれば `selected[0]` へ再結線する（3.2:
  逆方向リンク保証。`compute_shrink` の `protect` 契約により次数上限を
  維持したまま必ず残る）。

`plan_links` 完了時点で対象ノードの全層の外向きリストは既に完成して
いるため、`publish_links` が逆方向リンクを張って対象ノードが発見可能に
なった瞬間には、その層以下の外向きリストは既に完成済みであり H-N の窓が
構造的に閉じる。無競合（単一スレッド・逐次プレフィックス）実行では
旧手順と完全に同一のグラフを生成することを
`crates/engine/src/hnsw/parallel_build.rs::tests::
plan_then_publish_matches_sequential_insert_without_contention`・
`build_with_threads_one_matches_sequential_build_exactly` で機械検証した。

エントリ昇格競合（挿入中に発見したエントリが古いスナップショットに基づく
ケース）の追加結線は、Issue #447 実測で層 2 以上の到達不能ノードが常に
0 だったこと（層 2 以上でのみこの経路が問題になる）を踏まえ、本 Issue
では実装を見送った（発生 0 のため対策の効果を実測で確認できず、追加の
複雑性に見合わないと判断）。

#### 診断カウンタと実測

observe 限定（production 経路には一切影響しない）の診断カウンタ
`degenerate_layer_searches`（`plan_links` の層探索で候補集合が
`<=1` に退化した回数。並列フェーズのみ計上）・`reverse_link_reconnects`
（`ensure_reverse_link` が実際に再結線した回数）を
`HnswWorkerStats` へ追加した。

導入後の単発実測（本開発環境・threads=12・rows=20,256・dim=32・
クラスタ構造ありコーパス。`cargo test --release -p engine --lib
hnsw::parallel_build::tests::
observed_build_at_high_thread_count_reports_low_degenerate_layer_searches
-- --ignored --nocapture`）:

```
degenerate_layer_searches=2 reverse_link_reconnects=0 repair_unreachable_sum=0
```

H-N の窓に起因する退化探索がほぼ消え（2 件のみ。うち残存分はエントリ
昇格競合など本対策の対象外の経路に起因する可能性がある）、この実測点
では `repair_reachability` の修復対象自体が 0 件（Issue #447 追記の
同規模条件では threads=12 で層横断合計 unreachable_sum が二桁台
発生していた）まで低下した。この 1 回の実測は導入後の絶対値のみを
確認するものであり、Issue #447 基線（100k 点・threads=12 で
unreachable_sum 中央値 26〜27）との**同一コーパス・同一規模での定量的な
前後比較**は行っていない——`make bench-hnsw-parallel-build` による
100k 点フルラダー実測を運用者作業として申し送る（受け入れ条件「並列由来
増分が導入前比 1/10 以下」の最終判定はこの実測を待つ）。

#### Recall・不変条件

`tests/hnsw_search.rs::parallel_build_recall_at_10_matches_sequential_
build_within_margin` へ threads=12 の判定を追加し（既存の threads=4 に
加える）、ef=64／256 いずれも `par >= seq - 0.02` を満たすことを確認した。
`tests/hnsw.rs::parallel_build_invariants`（次数上限・連結性・レベル
不変）・`graph_fingerprint_is_stable_across_representation_change`
（逐次グラフの固定値照合）は無変更のまま green。

#### 対象ファイル

| パス | 変更 |
| --- | --- |
| `crates/engine/src/hnsw/parallel_build.rs` | `insert_node_locked` を `plan_links`／`publish_links`（新設）の 2 パスへ分離する薄いラッパへ変更。`BuildGraph::has_link`（新設）・`ensure_reverse_link`（新設）。observe 限定 TLS カウンタ `DEGENERATE_LAYER_SEARCHES`／`REVERSE_LINK_RECONNECTS` の追加。単体テスト 4 本追加（外向きリンク完成の固定・探索非退化の固定・逆方向リンク保証の固定・パス分離の無競合等価性）、informational テスト 1 本（`#[ignore]`） |
| `crates/engine/src/hnsw.rs` | `HnswWorkerStats` へ `degenerate_layer_searches`／`reverse_link_reconnects`（`u64`）フィールド追加。既存フィールドの意味・値は不変 |
| `crates/engine/tests/hnsw_parallel_profile_accept.rs` | `worker()`／`worker_with_wait()` ヘルパの構造体リテラルへ新フィールド追随 |
| `crates/engine/tests/hnsw_search.rs` | `parallel_build_recall_at_10_matches_sequential_build_within_margin` を threads=4 に加え threads=12 でも判定するよう拡張 |

`GraphBuilder::insert_node`／`shrink_links`／`compute_shrink`／
`repair_reachability_inner`（#449 の担当）・`SearchProvider` trait・
`sql::hnsw_cache`／`sql::hnsw_hybrid`・`Cargo.toml`（依存追加なし）・
`.github/workflows`（CI 非配線のまま）はいずれも無変更。

### Issue #449 追記（2026-09-07）: repair_reachability の到達不能ノード探索・再接続の並列化

Issue #447・#448 で明らかになった「凍結後に単一スレッドで走る
`repair_reachability` が並列構築の頭打ち要因の一つ」を受け、修復パスの
逐次コストを削減する。

#### 現状分析（着手前の実測から導いた設計判断）

Issue #447 追記の実測（run7・run8）を精査すると、`phase2_wall`（層数
（5）× BFS 1 回）はスレッド数に依存せず一定であり、`bfs_reachable`
（`HashSet<u32>` への挿入・`VecDeque` キュー）そのものの走査コストが
支配的だった。一方 Issue #448 適用後は残る到達不能ノードがほぼ 0（本
Issue の実測でも `repair_unreachable_sum=0`）まで低下しており、
Issue 本文が指定する「最近傍探索の並列化」（下記 C）だけでは修復対象
ノードが存在しないため効果が測れない。そこで、修復量に比例しない BFS の
定数コスト（下記 A・B）を先に削減したうえで C を実装する 3 段構成を
採った。A・B はいずれもグラフ出力を一切変えない（到達集合は集合として
同一）。

#### 設計

- **A: BFS のビットマップ化と層メンバの事前計算**（`hnsw.rs::
  GraphBuilder::bfs_reachable_mask`）: 到達集合の表現を `HashSet<u32>`
  （SipHash ハッシュコスト・エントリごとのヒープ確保）から
  [`NodeMask`]（1 ノード 1 bit のビットマップ。Issue #409 で導入済みの
  型を流用）＋ `VecDeque<u32>` キューへ置換した。層メンバ（`(0..len)
  .filter(level_of >= level)`）も層ごとに 1 回だけ構築し、毎反復・
  フェーズ 2 の全走査を削減した。
- **B: フェーズ 2 の冗長 BFS 省略**: フェーズ 1 が「未到達ノードが
  見つからず `break`」で終わった反復の BFS 結果は、グラフを一切変更
  していない状態のまま得られたものであり、フェーズ 2 が使う到達集合と
  完全に一致する。`mutated_since_bfs` フラグでこれを判定し、変更が
  無いと分かっている場合はフェーズ 2 の BFS 再実行を省略する。
- **C: 最近傍探索の並列化**（`hnsw.rs::nearest_reachable`・
  `repair_workers_for`）: フェーズ 1 の各反復が行う「到達済み集合内の
  最近傍探索」（`dot` を到達済みノード数だけ計算する読み取り専用の
  走査）を `std::thread::scope` で分割・並列実行する。方針（ワーカー数
  の決定。`repair_workers_for` が `parallel_search::thread_count_for`
  を経由）と機構（分割走査・縮約。`nearest_reachable`）を分離し、
  それぞれを独立にテストできるようにした。修復先の決定（`connect`／
  `shrink_links` の可変更新）自体は逐次のまま適用する 2 相構成
  （探索＝不変借用の読み取り専用ヘルパ、結線＝可変借用の逐次適用）。

同点タイブレーク（スコア `total_cmp` 降順・同点 id 昇順。モジュール
冒頭「順序規約」）は全順序を成すため、到達済み集合をどう分割し・各
ワーカーの局所最良をどの順序で縮約しても最終的に選ばれる修復先ノードは
分割・縮約の順序に依存せず一意に定まる——`threads` の値によらず
`repair_reachability_inner` が返すグラフはビット同一になる
（`docs/design/rrf-tie-break-determinism.md`「維持すべき不変条件」と
同方針）。`WorkerBudgetGuard` の追加取得は行わない——呼び出し元
（`build_with_threads`／`build_parallel`）が構築全体（並列挿入フェーズ
を含む）にわたって保持済みの予算を `threads` としてそのまま引き継ぐ
契約とした（二重計上の回避。既存の並列挿入フェーズも同じ規約）。

BFS 本体そのものの並列化（frontier 同期方式）は Issue 本文の指示により
本タスクでは見送り、逐次のまま維持した（下記「実測・所見」参照）。

#### 決定性の検証

- `crates/engine/src/hnsw.rs` 内 `#[cfg(test)] mod tests`
  （`nearest_reachable_matches_across_worker_counts_and_actually_
  parallelizes`）: 5,000 件の候補集合に対し `workers=1`／`2`／`4`／`8`
  の結果がビット同一であることを固定し、`workers>=2` で並列分岐が
  実際に起動したこと（テスト専用カウンタ `REPAIR_PARALLEL_LAUNCHES`。
  `#[cfg(test)]` 限定で production バイナリには残らない）を確認する
  非 vacuous 検証をあわせて行う。
- `repair_reachability_inner_is_thread_count_invariant_on_duplicate_
  heavy_graph`: 重複ヘビーコーパス（3,000 行・6 クラスタ。完全同点
  スコアを誘発しフェーズ 1・フェーズ 2 の双方を確実に発火させる）で、
  `threads=1`／`2`／`4` それぞれで修復した `GraphBuilder` の最終状態
  （id 昇順 → レベル → 各層リンク列の FNV-1a 64bit フィンガープリント）
  が完全一致することを固定する。フェーズ 1・フェーズ 2 が実際に発火した
  こと（`phase1_iterations > 0`・`phase2_nodes > 0`）もあわせて確認し、
  非 vacuous なテストにしている。
- `repair_workers_for_clamps_small_reachable_sets_and_respects_
  threads_cap`: 到達集合が `MIN_ROWS_PER_THREAD`（1,024）未満では
  `threads` の値によらず常に 1 に縮退することを固定する（小規模
  フィクスチャでの並列テストが実際にはワーカーを起動しない「見かけ上
  green」を避けるための重要な契約）。
- 既存の `tests/hnsw.rs::graph_fingerprint_is_stable_across_
  representation_change`（`build`／`threads=1` の固定値照合。値
  `0x5597_d0e9_0e1e_9898` は本 Issue の前後で不変）・
  `repair_stats_are_deterministic_on_sequential_path`・
  `repair_stats_on_duplicate_heavy_corpus_report_phase2_nodes` は
  無変更のまま green（受け入れ条件 1「build／threads==1 のグラフ不変」）。

#### 実測・所見

`make bench-hnsw-parallel-build`（本開発環境・rows=30,000・dim=64・一様
乱数コーパス〔クラスタ構造なし〕・threads=1,4,8,12。時間制約により
100k 点フルラダーではなく縮小規模での 1 回実測。導入前（このコミットの
直前）／導入後の交互比較）:

| threads | repair（導入前） | repair（導入後） |
| --- | --- | --- |
| 1 | 16.027ms | 2.028ms |
| 4 | 16.813ms | 2.275ms |
| 8 | 20.554ms | 2.284ms |
| 12 | 17.706ms | 2.270ms |

このベンチの一様乱数コーパスは Issue #448 適用後 `repair_unreachable_
sum=0`（全 threads 点で修復対象ノードが 1 件も発生しない）であるため、
上記の改善は全面的に A（BFS のビットマップ化）・B（冗長 BFS 省略）に
よるものであり、C（最近傍探索の並列化）はこのベンチでは 1 度も並列分岐
を通っていない（`phase1_iterations=0`）。約 7〜9 倍の改善が確認できた
一方、この実測では repair 段の絶対値自体が既に数 ms 台まで縮小して
おり、「12 スレッドで並列構築全体の頭打ちに追随する」という当初の
受け入れ条件 2 の趣旨（並列フェーズと同程度の並列度天井への追随）は、
そもそも一様乱数コーパスでは修復対象ノードがほぼ発生しないため
測定不能であることが判明した——BFS が逐次のままである以上、この構造は
残る（下記「スコープ外・申し送り」参照）。

C（最近傍探索の並列化）自体の正しさ・非 vacuous 性は上記「決定性の
検証」の重複ヘビーコーパスによる単体テストで固定している。到達不能
ノードが実際に多数発生する条件（重複ヘビー・adversarial なコーパス）
での並列化の速度改善は、本ベンチのランダムコーパス方式では再現できず、
運用者による専有環境での追加実測（重複ヘビーコーパス対応の
`BENCH_HNSW_PARALLEL_*` 拡張を含む）へ申し送る。

#### 対象ファイル

| パス | 変更 |
| --- | --- |
| `crates/engine/src/hnsw.rs` | `bfs_reachable`（`HashSet`）→ `bfs_reachable_mask`（`NodeMask`）へ置換。`repair_reachability_inner` を層メンバ事前計算・`mutated_since_bfs` によるフェーズ 2 BFS 省略・`threads` 引数追加へ書き換え。`repair_workers_for`／`nearest_reachable`／`nearest_reachable_scan`／`better_repair_candidate`（新設）。`repair_reachability`／`repair_reachability_observed` に `threads: usize` を追加。`build_inner` は常に `threads=1` を渡す。テスト専用カウンタ `REPAIR_PARALLEL_LAUNCHES`（`#[cfg(test)]` 限定）。単体テスト 4 本追加 |
| `crates/engine/src/hnsw/parallel_build.rs` | `freeze`／`build_parallel_graph`／`build_parallel_graph_observed` へ `threads` を配線し `repair_reachability`／`repair_reachability_observed` へ引き継ぐ。既存テスト 2 本の `freeze(...)` 呼び出しへ `threads=1` を追随 |

`SearchProvider` trait・`sql::hnsw_cache`／`sql::hnsw_hybrid`・
`search_layer`／探索経路・`PRECISE_REPAIR_CAP` の値・`Cargo.toml`
（依存追加なし）・`.github/workflows`（CI 非配線のまま）はいずれも
無変更。

#### スコープ外・申し送り

- BFS 本体の並列化（frontier 同期方式・原子ビットマップ）: Issue の
  指示により本タスクでは逐次のまま維持した。上記実測のとおり、A・B
  適用後の残る BFS コストは既に小さいため、追加の並列化が正味の改善に
  つながるかは要実測——後続 Issue（#450 等）の前後比較・所見で採否を
  判断する材料として申し送る。
- 100k 点フルラダーでの導入前後比較・重複ヘビーコーパスでの C 単独の
  速度実測: 運用者による専有環境での追加実測へ申し送る。

### 外部フレームワークとの構築比較（usearch）

usearch（`=2.26.1`。承認済み optional 依存・`contrast-bench` feature、
Issue #176）を用いた `make bench-hnsw-compare`
（`crates/engine/benches/hnsw_compare_bench.rs`。`BENCH_HNSW_COMPARE_THREADS`
でスレッド数ラダーを上書き可）で、自作 `HnswIndex` との構築時間・
Recall@10・探索レイテンシを比較した。パラメータは可能な範囲で等価に
揃えている（自作 `m=16`／`ef_construction=100`／`ef_search=64` ↔
usearch `connectivity=16`／`expansion_add=100`／`expansion_search=64`、
いずれも内積・F32・`multi=false`）。usearch のパラメータの意味は
`usearch` クレート（`rust/lib.rs`）のドキュメンテーションコメントで
確認できた範囲に限る。usearch の並列構築は
`reserve_capacity_and_threads(rows, threads)` で容量・スレッド数を
確保したうえで `threads` 本のワーカーへ行を静的分割して `add` する
方式で、`Index` の生成（`reserve` を含む）を計測区間に含めている
（自作側の `build_with_threads` も呼び出しから返却までを計測しており
条件は揃っている）。**計測方法修正（codex-review 指摘対応）により、
索引の drop（解放）は計測区間外へ移した**（修正前は解放コストが
build 側の計測に混入していた疑いがあったため）。さらに探索レイテンシは
codex-review 指摘（従来は先頭 20 クエリのみを計測しておりクエリ集合の
一部にしか代表性が無かった）を受けて、warmup 後の本計測を全 200 クエリ
（`queries` の値）を均等に評価する方式（計測回数はクエリ数の整数倍）へ
修正している。

探索レイテンシ計測修正後の再実測 2 回（run7・run8。2026-09-05。上記
「Issue #406 追記」節と同一環境）:

| threads | run7 自作 build median | run7 usearch build median | run7 self/usearch | run8 自作 build median | run8 usearch build median | run8 self/usearch |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 10,420.7 ms | 11,360.4 ms | 0.917x | 10,408.2 ms | 11,378.4 ms | 0.915x |
| 2 | 5,493.1 ms | 5,822.8 ms | 0.943x | 5,509.6 ms | 5,847.6 ms | 0.942x |
| 4 | 3,188.9 ms | 2,954.8 ms | 1.079x | 3,170.4 ms | 2,963.3 ms | 1.070x |
| 8 | 2,103.2 ms | 2,462.1 ms | 0.854x | 2,128.4 ms | 2,408.4 ms | 0.884x |
| 12 | 2,148.3 ms | 1,707.8 ms | 1.258x | 2,136.8 ms | 1,721.7 ms | 1.241x |

run8 は計測開始時 loadavg 9.55（前ランの後始末が末尾で並走した影響と
見られる）とやや高い環境だったが、build median・比とも run7 とほぼ
一致しており代表性に問題はない。

Recall@10（100,000 点・dim=64・クエリ 200 件。コーパスは
`harness/hnsw_build::generate_corpus` の一様乱数生成で、クラスタ構造を
持たない HNSW にとって最難条件の一つ）:

| engine | threads | recall@10 |
| --- | --- | --- |
| self | 1 | 0.5145（run7・run8 とも同値） |
| self | 12 | 0.5165（run7）／0.5180（run8） |
| usearch | 12 | 0.5175（run7）／0.5220（run8） |

探索レイテンシ中央値（threads=12 で構築した索引・ef_search=64。全 200
クエリを均等評価した本計測値）: 自作 66.228〜67.080 µs、usearch
76.151〜77.136 µs。

所見:

- 構築時間は自作／usearch でおおむね同水準（self/usearch 比
  0.85〜1.26 倍）。1・2・8 スレッドでは自作が速く、4・12 スレッドでは
  usearch が速い。12 スレッドで usearch が 6.6x まで speedup する一方、
  自作は 4.9x で頭打ちになる差は、上記「Issue #406 追記」節で実測した
  `repair_reachability`（凍結後・単一スレッド）の相対比重増加で説明が
  つく（usearch 側に相当する凍結後の単一スレッド段があるかは未調査）。
- Recall@10 は自作 0.51〜0.52・usearch 0.52 で同水準。この値は
  `docs/design/hnsw-search.md` に記録した一様乱数コーパスの informational
  参考値（10,000 点・ef=64 で 0.6410）と整合する低さであり、クラスタ
  構造ありフィクスチャでの受け入れ判定（Recall@10 ≥ 0.95〜0.99）とは
  別物である点に注意する。
- 探索レイテンシ中央値は自作 66〜67 µs・usearch 76〜77 µs で自作が
  やや速いが、いずれも同水準の範囲にある。

上記 run7・run8 は usearch のみを対照とし、コーパスは正規化していない
（内積カーネルをそのまま用いる本リポの既定と揃えた条件）。以下の
run9・run10 はこの条件から変更し、hnsw_rs を追加した 3 エンジン比較の
実測である点に注意する（旧実測との直接比較はできない）。**現在の `make bench-hnsw-compare` は hnsw_rs を撤去済み（2026-09-05）で、自作・usearch 2 エンジン・L2 正規化コーパス方式を維持している**ため、本ドキュメントの run9・run10 実測値は参考値として扱い、現行ベンチとの直接比較には用いないこと。

#### hnsw_rs（`=0.3.4`）を加えた 3 エンジン比較（Issue #406 追記・2026-09-05）— 撤去済み（2026-09-05）

対照エンジンとして `hnsw_rs`（`=0.3.4`。MIT OR Apache-2.0・純 Rust・
`contrast-bench` feature 限定・optional 依存・オーナー承認済み
〔2026-09-05〕）を追加した。`instant-distance` はアーカイブ済み、
`faiss` はネイティブ C++ ビルドが必須なため引き続き候補から外している。
`hnsw_rs` は `simdeez_f` feature を有効化し、距離関数に `DistDot`
（`1 - dot`。単位ベクトル前提）を用いるため、自作・usearch・hnsw_rs の
3 エンジン共通で**コーパスを L2 正規化**する方式へ統一した（run7・run8
時点の非正規化コーパスとは条件が異なる）。並列構築はいずれも
`std::thread::scope` による静的分割ワーカーから `insert`／`add` を並行
呼び出しする方式で揃え、`hnsw_rs` は `Hnsw` が `Sync` のため `&self`
参照での挿入を用いる。パラメータ対応は自作 `m=16`／`ef_construction=100`
／`ef_search=64` ↔ usearch `connectivity=16`／`expansion_add=100`
／`expansion_search=64`（内積・F32・`multi=false`）↔ hnsw_rs
`max_nb_connection=16`／`ef_construction=100`／`ef_search=64`
／`max_layer=11`／`dist=DistDot`（`simdeez_f`）。

deny.toml へ `RUSTSEC-2025-0141`（`bincode` 1.3.3・unmaintained。
`hnsw_rs` の dump/reload 経路のみが依存し本ベンチでは未使用）の ignore
を追加（撤去済み。2026-09-05）。

100,000 点・dim=64 での構築時間中央値（QEMU ゲスト・12 vCPU・
AVX2+FMA。上記「Issue #406 追記」節と同一環境）を 2 回実測した
（run9・run10。2026-09-05）:

##### run9

| threads | self median (speedup) | usearch median (speedup) | hnsw_rs median (speedup) |
| --- | --- | --- | --- |
| 1 | 9,947.1 ms (1.000x) | 10,664.6 ms (1.000x) | 42,616.6 ms (1.000x) |
| 2 | 5,233.3 ms (1.901x) | 5,410.8 ms (1.971x) | 21,512.2 ms (1.981x) |
| 4 | 3,022.6 ms (3.291x) | 2,791.8 ms (3.820x) | 11,703.1 ms (3.641x) |
| 8 | 2,347.3 ms (4.238x) | 2,448.0 ms (4.356x) | 9,703.1 ms (4.392x) |
| 12 | 1,922.5 ms (5.174x) | 1,607.6 ms (6.634x) | 6,662.9 ms (6.396x) |

self/usearch 比: 0.933x／0.967x／1.083x／0.959x／1.196x（threads
1/2/4/8/12）。self/hnsw_rs 比: 0.233x／0.243x／0.258x／0.242x／0.289x。

Recall@10: self（threads=1）0.4905・self（12）0.4955・usearch（12）
0.4935・hnsw_rs（12）0.5460。探索レイテンシ中央値: self 73.1 µs・
usearch 76.0 µs・hnsw_rs 347.8 µs。

##### run10

| threads | self median (speedup) | usearch median (speedup) | hnsw_rs median (speedup) |
| --- | --- | --- | --- |
| 1 | 9,958.3 ms (1.000x) | 10,642.3 ms (1.000x) | 42,387.5 ms (1.000x) |
| 2 | 5,219.7 ms (1.908x) | 5,403.3 ms (1.970x) | 21,559.2 ms (1.966x) |
| 4 | 2,997.2 ms (3.323x) | 2,758.4 ms (3.858x) | 11,518.0 ms (3.680x) |
| 8 | 1,930.9 ms (5.157x) | 2,173.9 ms (4.895x) | 9,199.2 ms (4.608x) |
| 12 | 1,901.7 ms (5.237x) | 1,575.1 ms (6.757x) | 6,532.5 ms (6.489x) |

self/usearch 比: 0.936x／0.966x／1.087x／0.888x／1.207x。self/hnsw_rs
比: 0.235x／0.242x／0.260x／0.210x／0.291x。

Recall@10: self（1）0.4905・self（12）0.5090・usearch（12）0.4985・
hnsw_rs（12）0.5480。探索レイテンシ中央値: self 65.5 µs・usearch
76.5 µs・hnsw_rs 357.6 µs。

run10 は開始時 loadavg 8.96（run9 直後の連続実行）とやや高く、
threads=8 の self が run9 より速い等の run-to-run 差がある点に注意する。

所見:

- 構築時間は自作と usearch が同水準（self/usearch 比 0.89〜1.21x）で
  あるのに対し、hnsw_rs は 3.4〜4.8 倍遅い。
- スレッドスケーリングは 12 スレッドで usearch 6.6〜6.8x・hnsw_rs
  6.4〜6.5x に対し自作は 5.2x に留まり、8→12 の頭打ち（上記「Issue #406
  追記」節で実測した `repair_reachability` 起因）は自作に固有の傾向
  である。
- Recall@10 は hnsw_rs が自作・usearch より約 +0.05 高いが、探索
  レイテンシは自作・usearch の約 5 倍（348〜358 µs 対 66〜77 µs）。
- パラメータの厳密な等価性・差の深掘りは本追記のスコープ外とし、
  Issue #446（ルート）〜#450 のツリーへ申し送る。

### 単一スレッド経路の非退行

`build_with_threads(.., 1)` は `build` を直接呼ぶ薄いラッパのため、
逐次経路のコード自体は本タスクで変更していない（`score`・
`select_neighbors_heuristic`・`shrink_links` の計算本体を純粋関数へ
切り出したが、呼び出し元のメソッドは同じ計算を同じ順序で行うだけの
委譲になっており、既存の全ユニット・結合テストが無変更で green）。

## スコープ外・申し送り

- `VectorArena` の `Arc<[f32]>` 化によるコピー縮退（#408 の設計課題として
  引き続き申し送る。`hnsw.rs` モジュール冒頭「ベクトルの所有方針」節参照）
- ~~`search_layer_locked` の隣接コピー方式が並列構築時間へ与える影響の
  定量的な内訳は計測していない~~ → Issue #406 追記（2026-09-05）で
  段別内訳を実測済み（上記「Issue #406 追記」節）。支配的な要因は
  `search_layer_locked` の隣接コピー自体ではなく `repair_reachability`
  （凍結後・単一スレッドの後始末）と判明した
- 挿入時の上位層リンク保証による到達不能ノード発生自体の抑制は
  実装済み（Issue #448。上記「Issue #448 追記」節参照）。`repair_
  reachability` 本体の並列化（フェーズ 1 の BFS が支配的要因。Issue
  #447 追記の所見 4）は未実装のまま #449 へ申し送る——残る修復対象は
  逐次経路と同水準の残差（層 1 の数件前後）に留まる見込み（Issue #448
  実測点の `repair_unreachable_sum=0` を根拠とする所見であり、
  100k 点フルラダーでの確定は #448 追記に記載のとおり運用者実測待ち）
- ホスト側の物理コア共有（SMT・vCPU ピニング等）の有無はゲスト内から
  直接検証できない（Issue #406 追記の所見 5。対照負荷の speedup 天井
  からの間接推定に留まる）
- `hnsw_rs =0.3.4` を加えた 3 エンジン比較の実施・撤去済み（Issue #406 追記・2026-09-05。理由・実測値は本セクション「hnsw_rs（`=0.3.4`）を加えた 3 エンジン比較」節参照）
- `build_parallel` を `HnswIndex::build` の既定にする判断・
  `SearchEngineKind::Hnsw` 結線（#407・実装済み）・世代整合キャッシュ（#408）
