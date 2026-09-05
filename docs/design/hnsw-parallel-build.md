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
build 側の計測に混入していた疑いがあったため）。

計測方法修正後の再実測 2 回（run3・run4。2026-09-05。上記「Issue #406
追記」節と同一環境）:

| threads | run3 自作 build median | run3 usearch build median | run3 self/usearch | run4 自作 build median | run4 usearch build median | run4 self/usearch |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 10,392.7 ms | 11,434.1 ms | 0.909x | 10,347.8 ms | 11,409.3 ms | 0.907x |
| 2 | 5,530.1 ms | 5,830.0 ms | 0.949x | 5,505.3 ms | 5,839.5 ms | 0.943x |
| 4 | 4,721.1 ms | 3,416.6 ms | 1.382x | 3,148.6 ms | 2,957.3 ms | 1.065x |
| 8 | 2,473.1 ms | 2,548.5 ms | 0.970x | 2,118.3 ms | 2,464.1 ms | 0.860x |
| 12 | 2,449.8 ms | 1,882.9 ms | 1.301x | 2,126.1 ms | 1,714.1 ms | 1.240x |

run3 の 4 スレッド点は計測開始時 loadavg 8.22（他ジョブ並走）の高負荷下
にあり自作側が遅く計測されているため self/usearch 比 1.382x は
過大評価と見る（run4 の同条件では 1.065x）。

Recall@10（100,000 点・dim=64・クエリ 200 件。コーパスは
`harness/hnsw_build::generate_corpus` の一様乱数生成で、クラスタ構造を
持たない HNSW にとって最難条件の一つ）:

| engine | threads | recall@10 |
| --- | --- | --- |
| self | 1 | 0.5145（run3・run4 とも同値） |
| self | 12 | 0.5210（run3）／0.5270（run4） |
| usearch | 12 | 0.5080（run3）／0.4930（run4） |

探索レイテンシ中央値（threads=12 で構築した索引・ef_search=64）:
自作 91.058〜91.229 µs、usearch 97.163〜97.180 µs。

所見:

- 構築時間は自作／usearch でおおむね同水準（self/usearch 比
  0.86〜1.38 倍。高負荷下で計測された run3 の 4 スレッド点 1.382x を
  除けば 0.86〜1.30 倍）。1・2・8 スレッドでは自作が速く、4・12 スレッド
  では概ね usearch が速い。12 スレッドで usearch が 6.1〜6.7x まで
  speedup する一方、自作は 4.2〜4.9x で頭打ちになる差は、上記
  「Issue #406 追記」節で実測した `repair_reachability`（凍結後・
  単一スレッド）の相対比重増加で説明がつく（usearch 側に相当する
  凍結後の単一スレッド段があるかは未調査）。
- Recall@10 は自作 0.51〜0.53・usearch 0.49〜0.51 で同水準。この値は
  `docs/design/hnsw-search.md` に記録した一様乱数コーパスの informational
  参考値（10,000 点・ef=64 で 0.6410）と整合する低さであり、クラスタ
  構造ありフィクスチャでの受け入れ判定（Recall@10 ≥ 0.95〜0.99）とは
  別物である点に注意する。
- 探索レイテンシは自作・usearch とも同水準（91 µs vs 97 µs）。
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
