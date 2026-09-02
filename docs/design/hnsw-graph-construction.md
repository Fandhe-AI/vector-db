# HNSW グラフ構築の設計記録

- ステータス: **実装済み**（`crates/engine/src/hnsw.rs`）
- 対応: Issue #404（`feat(engine): HNSW グラフ構築（層割当・近傍選択ヒューリスティック・単一スレッド）`）
- 親: Issue #402（Phase 3: ANN 索引の opt-in 採用）
- 前提: Issue #403（`docs/design/ann-index-adoption.md` を Accepted 化。ADR
  「実装ガイド（B 案）」節により、構築（本タスク）・探索・並列化は「非契約的な
  実装詳細」として spec 側の確定を待たずに着手可能と明記されている）

## 背景・範囲

`docs/design/ann-index-adoption.md`（Issue #367・CORE-9・CORE-10・TASK-132）で
採用が判断された B 案（条件付き opt-in・自作 HNSW・依存追加なし）の実装分解の
うち、本タスクは**グラフ構築のみ**（Malkov & Yashunin 2016 の Algorithm 1〜4
相当）を扱う。探索 API（`ef_search` を使った top-k 探索）は #405、並列構築は #406、
`SearchEngineKind::Hnsw` の結線は #407、世代整合キャッシュは #408、RLS 統合・
`EXPLAIN` 露出・Recall ゲート・前後比較 doc は #409〜#413 の担当であり、本タスク
では `search_engine.rs`・`core.rs`・`sql/` を変更していない。

## データ構造・API

`crates/engine/src/hnsw.rs::HnswIndex` は、ノードごとの層別隣接リスト
（`links: Vec<Vec<u32>>`。長さ `level+1`）・レベル・エントリポイントのみを
保持する。層ごとの次数上限は層 0 が `2*m`、層 1 以上が `m`（`max_degree`
アクセサで公開）。

```rust
pub struct HnswParams { pub m: usize, pub ef_construction: usize, pub ef_search: usize }
// 既定値（本リポ採用値。ADR 起票 Issue #403 に記載の非規範的な実装既定値）:
// m=16, ef_construction=100, ef_search=64

pub struct HnswIndex { /* private */ }
impl HnswIndex {
    pub fn build(params: HnswParams, dim: u32, vectors: &[f32], seed: u64) -> Result<Self, HnswError>;
    pub fn params(&self) -> &HnswParams;
    pub fn dim(&self) -> u32;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn max_level(&self) -> Option<usize>;
    pub fn entry_point(&self) -> Option<u32>;
    pub fn level_of(&self, node: u32) -> Option<usize>;
    pub fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]>;
    pub fn max_degree(&self, level: usize) -> usize;
}
```

## ベクトルの所有方針（#405・#408 への申し送り）

`HnswIndex` はベクトル本体を複製しない。呼び出し元（`arena.rs::VectorArena`
や #408 の世代整合キャッシュ）が row-major 連続バッファとして所有し続け、
`build` へは `&[f32]` で借用のみ渡す契約とする。768 次元 × 100 万行では
複製だけで 3 GB 級になるため、この方針はメモリ効率上の要請である。探索 API
（#405）も同じ `vectors: &[f32]` を受け取り、`len() * dim() == vectors.len()`
を毎回検証する（fail-closed。呼び出し元がビルド後にベクトル集合を差し替えて
しまう事故を検出する唯一の手段が長さ照合であるため）。

## 近傍選択ヒューリスティック（Algorithm 4）の既定

`extend_candidates=false`・`keep_pruned_connections=true`（余った枠を枝刈り
済み候補で埋め、次数を確保する）を既定とした。`extend_candidates=true`
（候補の隣接をさらに候補集合へ加える拡張）は実装していない——到達しない
分岐をコードへ残すこと自体がレビュー時のバグ源になる（advisor レビュー
指摘）ため、既定を見直す場合は #405 の Recall 実測を踏まえて別途実装する。

## 決定性・非暗号 PRNG

レベル割当は `benches/harness/rng.rs::DeterministicRng` と同アルゴリズムの
xorshift64* を `hnsw.rs` 内へ独立に複製し（`src/` からは bench harness を
参照できないため）、`build` の `seed` 引数で初期化する。同一 `seed`・同一
入力なら完全に同一のグラフを構築する（`tests/hnsw.rs::
same_seed_produces_identical_graph`／`different_seed_yields_a_different_graph`
で検証済み）。**非暗号 PRNG であり、鍵・トークン等のセキュリティ用途に
転用してはならない**（OWASP A02）。

## 距離カーネル・順序規約

`kernel::dot`（`isa.rs` の実行時検出 SIMD カーネルへの唯一の委譲経路）を
再利用する。スコアは内積で「大きいほど近い」とし、候補ヒープの同点タイ
ブレークは `kernel.rs::MinHeapItem` と同じ「スコア `total_cmp` 降順・同点は
id 昇順」を踏襲する。ソートは安定ソート（`sort_by`）のみを使い、
`scripts/check_sort_determinism.sh` の対象（`sort_unstable_by` 系）を使わない。

## 上限・untrusted 入力の扱い

`MAX_M`（128）・`MAX_EF`（10,000。`core.rs::MAX_SEARCH_K` と同値）・
`MAX_HNSW_NODES`（1,000,000。`arena::MAX_ARENA_ROWS` と同値）・`MAX_LEVEL`
（32）で上限を検証する。ノード id 変換は `u32::try_from`、オフセット計算は
`checked_mul`／`checked_add`、スライス添字は `get()` のみを使い
`unwrap`／`expect`／`[]` は使わない。構築入力に非有限値（NaN/Inf）を含む
行は `NonFiniteVector` として構築段で拒否する（構築後に順序を壊す経路を
作らないため）。`unsafe` は使わず、依存追加もない。環境変数・feature flag
による経路上書きは設けない（CORE-12 踏襲）。

## 受け入れ条件 (a): 層構造・次数上限・連結性の単体テスト

`crates/engine/tests/hnsw.rs`（crate 外の公開 API のみで検証する既存流儀。
`tests/isa.rs`・`tests/dispatch.rs` と同じ位置付け）で、決定的フィクスチャ
（seed 固定・N=800・dim=16）に対し以下を検証済み:

- パラメータ検証（`m`／`ef_construction`／`ef_search` の境界値・
  `MAX_HNSW_NODES` 超過・次元不整合・非有限値）
- 同一 seed → 完全に同一のグラフ、異 seed → 差分あり
- 層構造: エントリポイントのレベル == `max_level`、全ノードが層 0 に存在、
  レベル `l` のノードは `0..=l` 全層に隣接リストを持つ、`max_level <=
  MAX_LEVEL`
- 次数上限: 全層・全ノードで隣接数が `max_degree` 以内、自己ループなし、
  重複なし、隣接先の id・層整合
- 連結性: 各層でエントリポイントからの BFS がその層の全メンバへ到達する
  （固定 seed フィクスチャで `max_level=3` まで実際に上位層を構築・検証
  していることを確認済み。advisor レビュー指摘対応: 上位層が偶然構築
  されないまま green になる回帰を防ぐため `max_level >= 1` を明示的に
  アサートする）

## 受け入れ条件 (b): N log N スケーリング確認ベンチ

`crates/engine/benches/hnsw_build_bench.rs`（`make bench-hnsw-build` から
実行する手動専用ベンチ。`GITHUB_ACTIONS` 下拒否・CI 非配線。spec 由来の
合否閾値は持たない情報提供専用）で、`HnswIndex::build`（dim=64・既定
パラメータ `M=16`／`ef_construction=100`）を `rows ∈ {2,000, 8,000, 32,000}`
で計測し、隣接規模点対の log-log 傾き（実効指数）を出す。実効指数が 1.6
以上（本ベンチ実装上の目安値。spec 由来の閾値ではない）の場合は
`SuperLinear` と明示する。

### 実測結果（本開発環境・単発計測）

| rows | dim | 構築時間中央値 | t / (N ln N) |
| --- | --- | --- | --- |
| 2,000 | 64 | 78.5 ms | 5.16e-6 |
| 8,000 | 64 | 415.6 ms | 5.78e-6 |
| 32,000 | 64 | 2,084.7 ms | 6.28e-6 |

| 規模点対 | 実効指数 | 分類 |
| --- | --- | --- |
| 2,000 → 8,000 | 1.20 | NearNLogN |
| 8,000 → 32,000 | 1.16 | NearNLogN |

いずれも `1.0`（線形）よりわずかに大きく `2.0`（二乗）から明確に離れており、
`N log N` 相当の伸び方であることを確認した。単発計測（複数試行の前後比較で
はない）のため、大きな実装変更（#406 の並列化等）の前後比較を行う場合は
`docs/design/dot-kernel-multi-accumulator.md` 等と同じ交互実行方式で
再測定することを推奨する。

## #405〜#408 への申し送り

- ベクトルは借用のみで複製しない契約（上記「ベクトルの所有方針」節）を
  探索 API（#405）・世代整合キャッシュ（#408）でも維持すること
- `search_layer`（Algorithm 2 相当）は `pub(crate)` で実装済みのため、
  #405 は `ef_search` を渡してそのまま再利用できる。ただし現状の visited
  配列は呼び出しごとに新規確保する構築段向けの実装であり、クエリ多発経路
  （#405）ではエポック方式（`Vec<u32>` 再利用）への差し替えを検討すること
- `insert_node` は探索段（不変参照）と結線段（可変）を関数分離してある。
  #406（並列構築）が要素単位ロックへ差し替える際にこの境界を流用できる
- 近傍選択ヒューリスティックの既定（`extend_candidates=false`）は #405 の
  Recall 実測に基づいて見直す余地がある（上記「近傍選択ヒューリスティック」節）
