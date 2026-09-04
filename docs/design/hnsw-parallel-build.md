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
2 回とも約 2.1 s に収束し、伸びが頭打ちになる。頭打ちの要因として、
凍結後に単一スレッドで走る `repair_reachability`（本タスクの範囲外）の
相対比重の増加と、`SEQUENTIAL_PREFIX_NODES` 分の逐次プレフィックス構築、
および 12 論理コア（物理コアは半数）での SMT 分の実効並列度の低さが
考えられるが、段別の内訳は計測していない（「スコープ外・申し送り」節）。
2 回の実測差（`threads=8` で 2,112 ms vs 2,548 ms）は共有環境の
run-to-run 変動の範囲として扱い、閾値判定には用いない（本ベンチは
情報提供専用で spec 閾値を持たない）。

小規模スモーク実測（rows=5,000・dim=64。実装時点の計測）:

| threads | median | speedup |
| --- | --- | --- |
| 1 | 263.6 ms | 1.000x |
| 2 | 158.1 ms | 1.668x |
| 4 | 88.0 ms | 2.995x |

### 単一スレッド経路の非退行

`build_with_threads(.., 1)` は `build` を直接呼ぶ薄いラッパのため、
逐次経路のコード自体は本タスクで変更していない（`score`・
`select_neighbors_heuristic`・`shrink_links` の計算本体を純粋関数へ
切り出したが、呼び出し元のメソッドは同じ計算を同じ順序で行うだけの
委譲になっており、既存の全ユニット・結合テストが無変更で green）。

## スコープ外・申し送り

- `VectorArena` の `Arc<[f32]>` 化によるコピー縮退（#408 の設計課題として
  引き続き申し送る。`hnsw.rs` モジュール冒頭「ベクトルの所有方針」節参照）
- `search_layer_locked` の隣接コピー方式（並列経路専用。逐次経路の
  `search_layer` は無変更）が並列構築時間へ与える影響の定量的な内訳は
  計測していない（本タスクの受け入れ条件は「スレッド数に応じた短縮」の
  確認までで、内訳分析は対象外）
- `build_parallel` を `HnswIndex::build` の既定にする判断・
  `SearchEngineKind::Hnsw` 結線（#407・実装済み）・世代整合キャッシュ（#408）
