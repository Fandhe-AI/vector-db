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
分岐は検証されないままコードに残り将来のバグ源になるため、既定を見直す
場合は #405 の Recall 実測を踏まえて別途実装する。

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
  していることを確認済み。上位層が偶然構築されないまま green になる回帰を
  防ぐため `max_level >= 1` を明示的にアサートする）
- ランダム構成・重複ヘビーコーパスでの連結性（下記「逆方向リンクの到達性
  保証」節参照）: 複数 seed（10 個）× 複数 `(dim, rows, m)` 構成、および
  完全同点スコアを誘発する重複ヘビーコーパス（少数のクラスタ中心の完全
  複製）でも最終グラフの全層連結性が保たれることを検証する
  （`tests/hnsw.rs::randomized_configs_and_duplicate_heavy_corpus_stay_fully_connected`）。
  固定 1 フィクスチャでは顕在化しない入力依存のバグ（下記参照）が実際に
  存在したため追加した

## `search_layer` の停止・受理判定: 順序規約の使い分け（PR #423 是正）

`search_layer`（Algorithm 2）の候補ヒープの並び替え・最終出力ソートは、
モジュール冒頭の順序規約どおり `ScoredNode::cmp`（スコア `total_cmp` 降順・
同点は id 昇順の複合順序）を使う。だが**停止判定**（候補集合の最良要素が
結果集合の最悪要素より劣るなら打ち切る）と**受理判定**（新規候補を結果
集合へ加えるか）は、この複合順序ではなく**スコアのみ**の比較に限定する。

複合順序をそのまま使うと、スコアが同点で id が大きいだけの候補まで
「より遠い」と誤判定して打ち切ってしまい、その候補の未訪問隣接ノードが
より近い可能性を探索し損なう（cursor Bugbot 指摘 PR #423。重複 embedding
で同点候補が生じやすく顕在化する）。id 順の複合順序は結果集合の内容
（`results.pop()` によるヒープ内での追い出し順）・最終出力の安定ソートで
のみ使い、探索を続けるか否かの判定には使わない。

なお `greedy_descend`（上位層のナビゲーション用 `ef=1` 特殊形）は、この
使い分けの対象**外**とする。`cand_scored > current_best` は厳密な改善判定
であり、有限ノード集合上の全順序で単調に進むことがループの終了を保証する。
これをスコアのみの `>=` 判定へ緩めると、同点重複ノード間で A→B→A の
無限ループになり得るため、意図的に複合順序のまま据え置く。

## 逆方向リンクの到達性保証（PR #423 是正）

`shrink_links`（次数上限超過時のヒューリスティック再選択、Algorithm 1）は、
呼び出し直前に張ったばかりの逆方向リンク先（`protect`）を枝刈りしてしまうと、
対象ノードへの唯一の入口だった逆方向リンクが失われ、エントリポイントから
の到達路が残らないまま孤立し得る（codex-review P1 指摘 PR #423。挿入ノード
は自身の外向きリンクは持つが、探索はエントリポイントから既存ノードの隣接
リストを辿って到達するため入方向のリンクが要る）。

- **`shrink_links` の `protect` 引数**: ヒューリスティック選択後に `protect`
  が漏れていれば、選択済み集合中で最もスコアが低い要素と差し替えて強制的
  に残す。これは「`protect` の挿入時点で選ばれた各近傍が `protect` への
  逆方向リンクを保持する」ことのみを担保する**呼び出し時点の不変条件**
  であり、後続の別ノード挿入がこれらの近傍を再度 `shrink_links` する際に
  `protect` が漏れる余地までは塞がない（グローバルな到達性の恒久保証では
  ない）。
- **`repair_reachability`（全ノード挿入後の後始末パス）**: 上記の残差
  ケースを閉じるため、`HnswIndex::build` の末尾で各層ごとにエントリ
  ポイントからの BFS を行い、到達できないノードが残っていれば最も近い
  到達済みノードへ双方向リンクを追加して修復する。検証の過程で、修復
  自体が呼ぶ `shrink_links` の枝刈りが**無関係な別の**既存ノードの唯一の
  到達経路を巻き込んで壊し、新たな到達不能ノードを生む whack-a-mole が
  起こり得ることが分かったため、1 ノードずつ確定的に修復し直後に BFS を
  やり直して次の未到達ノード（新たに生まれたものを含む）を選ぶワーク
  リスト方式にした（フェーズ 1）。反復回数の上限は入力規模に依存しない
  小さな絶対上限 `PRECISE_REPAIR_CAP`（64。層のメンバ数に比例させると
  残差の多い adversarial な入力で計算量 DoS になり得るため、意図的に
  定数へ固定している。codex-review #423 P1 指摘）までは次数上限を維持する
  `shrink_links` 経由で厳密に修復する。この上限に達してもなお残る未到達
  ノードはフェーズ 2 が閉じる：残存ノードを id 昇順の片方向チェーン
  （`entry -> remaining[0] -> remaining[1] -> ...`）として結線したうえで、
  各結線の直後に `shrink_links` を適用し次数上限を維持する（`connect` の
  みで結線し次数超過を許容する方式は、エントリポイントの隣接リストが
  残存ノード数に比例して伸びる・次数上限を大幅に超え得るという 2 つの
  問題を持つため採らない。詳細は `repair_reachability` のドキュメンテー
  ションコメント参照）。安全側 = 全ノードの到達性を最終的に必ず保証し、
  かつ次数上限もフェーズ 1・フェーズ 2 のいずれの結線でも維持する。

## 受け入れ条件 (b): N log N スケーリング確認ベンチ

`crates/engine/benches/hnsw_build_bench.rs`（`make bench-hnsw-build` から
実行する手動専用ベンチ。`GITHUB_ACTIONS` 下拒否・CI 非配線。spec 由来の
合否閾値は持たない情報提供専用）で、`HnswIndex::build`（dim=64・既定
パラメータ `M=16`／`ef_construction=100`）を `rows ∈ {2,000, 8,000, 32,000}`
で計測し、隣接規模点対の log-log 傾き（実効指数）を出す。実効指数が 1.6
以上（本ベンチ実装上の目安値。spec 由来の閾値ではない）の場合は
`SuperLinear` と明示する。

### 実測結果（本開発環境・単発計測。#423 是正後の現 HEAD）

以下は「逆方向リンクの到達性保証」節・「`search_layer` の停止・受理判定」
節の是正（PR #423。`HnswIndex::build` 末尾へ `repair_reachability` を追加）
を含む現 HEAD での再測定値である。この修復パスは通常経路で BFS を 1 回
追加するため、是正前の基線（本節の旧版）よりわずかに `t / (N ln N)` が
増加しているが、実効指数はいずれの規模点対も引き続き `NearNLogN` 分類
（1.6 未満）のまま変化していない。

| rows | dim | 構築時間中央値 | t / (N ln N) |
| --- | --- | --- | --- |
| 2,000 | 64 | 82.9 ms | 5.46e-6 |
| 8,000 | 64 | 443.9 ms | 6.17e-6 |
| 32,000 | 64 | 2,213.2 ms | 6.67e-6 |

| 規模点対 | 実効指数 | 分類 |
| --- | --- | --- |
| 2,000 → 8,000 | 1.21 | NearNLogN |
| 8,000 → 32,000 | 1.16 | NearNLogN |

いずれも `1.0`（線形）よりわずかに大きく `2.0`（二乗）から明確に離れており、
`N log N` 相当の伸び方であることを確認した。単発計測（複数試行の前後比較で
はない）のため、大きな実装変更（#406 の並列化等）の前後比較を行う場合は
`docs/design/dot-kernel-multi-accumulator.md` 等と同じ交互実行方式で
再測定することを推奨する。

## #405 の実装状況・#406〜#408 への申し送り

- 探索 API（`ef_search` を使った top-k 探索）は #405 で実装済み。詳細は
  `docs/design/hnsw-search.md` 参照（`HnswIndex::search`・`HnswSearchScratch`・
  ビットマップ方式 visited 集合・実測 Recall 表）
- ベクトルは借用のみで複製しない契約（上記「ベクトルの所有方針」節）を
  世代整合キャッシュ（#408）でも維持すること
- `search_layer`（Algorithm 2 相当）は `pub(crate)` のまま、visited 集合の
  実装を `VisitedSet` trait でジェネリック化し、構築経路（`VisitedScratch`。
  世代カウンタ方式）・探索経路（`VisitedBitmap`。1 ノード 1 bit）の双方から
  共有する構成へ変更した（#405）
- `insert_node` は探索段（不変参照）と結線段（可変）を関数分離してある。
  #406（並列構築）が要素単位ロックへ差し替える際にこの境界を流用できる
- 近傍選択ヒューリスティックの既定（`extend_candidates=false`）は #405 の
  クラスタ構造ありフィクスチャでの実測（10k×dim128・ef=64/256 で
  Recall@10 = 1.0000）では見直しを要さなかった。同じ規模での一様乱数のみの
  コーパス（informational 参考値）では ef=64 で 0.6410 まで低下しており、
  埋め込み分布がクラスタ構造から離れる場合の見直し余地は引き続き残る
  （詳細は `docs/design/hnsw-search.md`「実測 Recall」節・上記「近傍選択
  ヒューリスティック」節）
