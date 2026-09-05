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

### 上位層の到達性

並列時、エントリ更新の競合（複数ワーカーが同時に新最大層を持つノードを
挿入）で上位層に到達不能ノードが残り得るが、既存の `repair_reachability`
（凍結後・単一スレッド）がそのまま閉じる。追加の修復ロジックは書いていない。

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

### Issue #406 追記（2026-09-05）: 8→12 スレッド頭打ちの段別内訳

「受け入れ条件 (b)」で観測した 8→12 スレッドの伸び悩みについて、構築の
各段（レベル割当・逐次プレフィックス・並列挿入・凍結・`repair_reachability`）
を計測できる観測用フック `HnswIndex::build_with_threads_observed`
（本 PR で追加。`build_with_threads` 自体は変更していない。計装は観測版
のみが持つ）を用いて段別内訳を実測した。

計測条件: `BENCH_HNSW_PARALLEL_THREADS=1,2,4,8,12
make bench-hnsw-parallel-build`、rows=100,000・dim=64・既定パラメータ、
各点 warmup 20 回・計測 20 回の中央値を 2 回実測（2026-09-05）。環境は
「受け入れ条件 (b)」と同一の QEMU ゲスト（`lscpu`: `Thread(s) per core: 1`・
1 ソケット 12 コア・L3 16 MiB・NUMA 1 ノード、x86_64 AVX2+FMA）。

#### 段別内訳（1 回目）

| threads | total | level | prefix | parallel | freeze | repair | serial_share | parallel_speedup | total_speedup |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 9,982.0 ms | 0.000 ms | 9,976.3 ms | 0.000 ms | 0.000 ms | 0.000 ms | 99.94% | 1.000x | 1.000x |
| 2 | 5,279.6 ms | 1.040 ms | 6.084 ms | 5,011.9 ms | 0.699 ms | 236.070 ms | 4.62% | 1.000x | 1.891x |
| 4 | 3,021.0 ms | 1.040 ms | 6.087 ms | 2,594.7 ms | 0.603 ms | 412.699 ms | 13.92% | 1.932x | 3.304x |
| 8 | 2,080.2 ms | 1.040 ms | 6.091 ms | 1,435.5 ms | 0.564 ms | 631.937 ms | 30.75% | 3.491x | 4.799x |
| 12 | 2,089.6 ms | 1.042 ms | 6.110 ms | 1,236.3 ms | 0.723 ms | 814.881 ms | 39.37% | 4.054x | 4.777x |

#### 段別内訳（2 回目）

| threads | total | level | prefix | parallel | freeze | repair | serial_share | parallel_speedup | total_speedup |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 12,149.4 ms | 0.000 ms | 11,943.0 ms | 0.000 ms | 0.000 ms | 0.000 ms | 98.30% | 1.000x | 1.000x |
| 2 | 6,604.4 ms | 1.045 ms | 6.034 ms | 5,844.6 ms | 0.694 ms | 290.534 ms | 4.52% | 1.000x | 1.840x |
| 4 | 3,784.9 ms | 1.046 ms | 6.042 ms | 3,138.5 ms | 0.610 ms | 570.384 ms | 15.27% | 1.862x | 3.210x |
| 8 | 2,630.4 ms | 1.045 ms | 6.036 ms | 1,842.0 ms | 0.611 ms | 880.675 ms | 33.77% | 3.173x | 4.619x |
| 12 | 2,138.9 ms | 1.044 ms | 6.026 ms | 1,467.2 ms | 0.575 ms | 896.327 ms | 42.26% | 3.983x | 5.680x |

`parallel_speedup` は `parallel_base_threads=2` を基準（threads=2 が
1.000x）とした並列挿入段のみの speedup（`level`／`prefix`／`freeze` は
スレッド数に依らずほぼ一定のため対象外）。

#### ワーカー統計

| threads | 1 回目 inserted[min/med/max] | 1 回目 busy[min/med/max] | 1 回目 lock_blocked | 2 回目 inserted[min/med/max] | 2 回目 busy[min/med/max] | 2 回目 lock_blocked |
| --- | --- | --- | --- | --- | --- | --- |
| 2 | 49,772/49,972/49,972 | 5,000.763/5,000.775/5,000.775 ms | 0.00% | 49,687/50,057/50,057 | 5,844.220/5,844.314/5,844.314 ms | 0.00% |
| 4 | 24,837/25,027/25,029 | 2,572.317/2,572.364/2,572.371 ms | 0.01% | 24,301/25,217/25,807 | 3,220.929/3,220.968/3,221.090 ms | 0.01% |
| 8 | 7,388/13,233/13,369 | 1,437.268/1,437.506/1,437.587 ms | 0.02% | 7,328/13,880/14,062 | 1,676.790/1,676.922/1,677.101 ms | 0.01% |
| 12 | 6,585/8,657/9,026 | 1,258.116/1,258.931/1,259.342 ms | 0.02% | 7,164/8,102/9,315 | 1,530.868/1,531.085/1,531.269 ms | 0.02% |

`entry_promotions`（エントリポイント更新回数）は全点・両回とも 3 で
不変だった。

#### 対照負荷（`dot_scan`）との比較

`dot_scan` は共有可変状態を持たない、コーパス全行とクエリ行の内積走査
（64 パス）で、ハードウェアの並列度天井の参考値として計測している。

| threads | 1 回目 dot_scan median | 1 回目 dot_scan speedup | 1 回目 並列段の単一スレッド換算 speedup | 2 回目 dot_scan median | 2 回目 dot_scan speedup | 2 回目 並列段の単一スレッド換算 speedup |
| --- | --- | --- | --- | --- | --- | --- |
| 2 | 27.108 ms | 1.939x | 2.000x | 31.468 ms | 1.558x | 2.000x |
| 4 | 11.366 ms | 4.625x | 3.863x | 25.902 ms | 1.893x | 3.725x |
| 8 | 8.115 ms | 6.477x | 6.983x | 10.143 ms | 4.833x | 6.346x |
| 12 | 6.031 ms | 8.716x | 8.107x | 6.013 ms | 8.153x | 7.968x |

「並列段の単一スレッド換算 speedup」は `parallel median(threads=2) × 2
/ parallel median(threads=T)` で定義する（`parallel_speedup` は
threads=2 基準のため、単一スレッド基準に揃えるための換算）。

#### 所見

1. **主因は `repair_reachability`（凍結後・単一スレッドの後始末）**。
   2→4→8→12 スレッドで 236→413→632→815 ms（1 回目）／
   291→570→881→896 ms（2 回目）と単調増加し、12 スレッドでは total の
   39〜42% を占める。並列度が上がるほど上位層の到達不能ノードが増え
   修復量が増える構造（`hnsw.rs::repair_reachability` は各反復で
   BFS と到達済み全ノードとの dot 計算を伴う。反復上限
   `PRECISE_REPAIR_CAP` は本タスクの実装既定値）。8→12 での並列段の
   短縮（1 回目 1,436→1,236 ms＝約 −199 ms、2 回目 1,842→1,467 ms＝
   約 −375 ms）を repair の増加（約 +183 ms／+16 ms）が相殺・吸収して
   おり、total の頭打ちとして観測される。
2. **逐次プレフィックス（256 件・約 6 ms）・レベル割当（約 1 ms）・
   凍結（1 ms 未満）はいずれも total の 0.4% 未満で無視できる**。旧来の
   「逐次プレフィックスが要因候補」という推定は否定できる。
3. **ロック競合は主因ではない**。`lock_blocked_ratio` は全点で
   0.00〜0.02% に留まる。
4. **並列段自体はハードウェアの並列度天井に概ね追随している**。対照
   `dot_scan` の speedup は 12 スレッドで 8.2〜8.7x（12 vCPU 中
   68〜73% の実効率）に留まり、並列段の単一スレッド換算 speedup
   （1 回目 8.1x・2 回目 8.0x）とほぼ一致する。VM のスケジューリング・
   メモリ帯域（コーパスは約 25.6 MiB で L3 16 MiB を上回る）による
   天井であり、8→12 での並列段自体の伸びしろは元々小さい。
5. **ワーカー間の挿入件数の偏り**（8 スレッドで min 約 7.3k・max
   約 13.4k、12 スレッドで min 約 6.6k・max 約 9.3k）に対し busy 時間は
   ワーカー間でほぼ等しい（ワークスティール方式のため終了時点が揃う）。
   偏り自体は vCPU ごとの実効速度差（ホスト側スケジューリング）を
   示唆するが、ゲスト内からは要因を切り分けて検証できない。
6. 改善余地として `repair_reachability` の並列化、または挿入時の上位層
   リンク保証による到達不能ノード発生自体の抑制が考えられるが、本追記の
   スコープでは実装しない（別 Issue 起票の要否はオーナー判断）。

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
条件は揃っている）。

| threads | 1 回目 自作 build median | 1 回目 usearch build median | 1 回目 self/usearch | 2 回目 自作 build median | 2 回目 usearch build median | 2 回目 self/usearch |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 10,084.9 ms | 11,461.0 ms | 0.880x | 9,981.7 ms | 11,411.5 ms | 0.875x |
| 2 | 5,392.1 ms | 5,985.5 ms | 0.901x | 5,334.9 ms | 5,845.6 ms | 0.913x |
| 4 | 3,156.1 ms | 2,993.7 ms | 1.054x | 3,704.2 ms | 2,985.7 ms | 1.241x |
| 8 | 2,118.3 ms | 2,495.8 ms | 0.849x | 2,132.1 ms | 2,523.4 ms | 0.845x |
| 12 | 2,149.9 ms | 1,978.2 ms | 1.087x | 2,121.6 ms | 1,733.6 ms | 1.224x |

Recall@10（100,000 点・dim=64・クエリ 200 件。コーパスは
`harness/hnsw_build::generate_corpus` の一様乱数生成で、クラスタ構造を
持たない HNSW にとって最難条件の一つ）:

| engine | threads | recall@10 |
| --- | --- | --- |
| self | 1 | 0.5145（両回とも同値） |
| self | 12 | 0.5305（1 回目）／0.5370（2 回目） |
| usearch | 12 | 0.5185（1 回目）／0.5100（2 回目） |

探索レイテンシ中央値（threads=12 で構築した索引・ef_search=64）:
自作 90.8〜96.4 µs、usearch 93.5〜98.3 µs。

所見:

- 構築時間は自作／usearch でおおむね同水準（self/usearch 比 0.85〜1.24x）。
  1・2・8 スレッドでは自作が速く、4・12 スレッドでは usearch が速い。
  12 スレッドで usearch が 5.8〜6.6x まで speedup する一方、自作は
  4.7x 前後で頭打ちになる差は、上記「Issue #406 追記」節で実測した
  `repair_reachability`（凍結後・単一スレッド）の相対比重増加で説明が
  つく（usearch 側に相当する凍結後の単一スレッド段があるかは未調査）。
- Recall@10 は 0.51〜0.54 で自作・usearch とも同水準。この値は
  `docs/design/hnsw-search.md` に記録した一様乱数コーパスの informational
  参考値（10,000 点・ef=64 で 0.6410）と整合する低さであり、クラスタ
  構造ありフィクスチャでの受け入れ判定（Recall@10 ≥ 0.95〜0.99）とは
  別物である点に注意する。
- 探索レイテンシは自作・usearch とも同水準（91〜98 µs）。
- 追加の比較候補として `hnsw_rs`（`=0.3.4`。MIT OR Apache-2.0・純 Rust・
  `rayon` に依存）を第一候補として挙げる。`instant-distance` はアーカイブ
  済み、`faiss` はネイティブ C++ ビルドが必須なため候補から外した。
  依存追加はユーザー承認が必要なため、追加比較の実施はオーナー判断
  待ちとする。

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
- `repair_reachability` の並列化、または挿入時の上位層リンク保証による
  到達不能ノード発生自体の抑制は未実装（Issue #406 追記の所見 6。
  別 Issue 起票の要否はオーナー判断）
- ホスト側の物理コア共有（SMT・vCPU ピニング等）の有無はゲスト内から
  直接検証できない（Issue #406 追記の所見 5。対照負荷の speedup 天井
  からの間接推定に留まる）
- 外部フレームワーク比較の追加候補（`hnsw_rs =0.3.4` 等）は依存追加の
  ためユーザー承認待ち（Issue #406 追記「外部フレームワークとの構築
  比較」節）
- `build_parallel` を `HnswIndex::build` の既定にする判断・
  `SearchEngineKind::Hnsw` 結線（#407・実装済み）・世代整合キャッシュ（#408）
