# HNSW 探索（ef 探索・top-k）の設計記録

- ステータス: **実装済み**（`crates/engine/src/hnsw.rs::HnswIndex::search`）
- 対応: Issue #405（`feat(engine): HNSW 探索（ef 探索・top-k）と brute-force 対照 Recall 単体テスト`）
- 前提: Issue #404（`docs/design/hnsw-graph-construction.md`。グラフ構築 Algorithm 1〜4）
- 親: Issue #402（Phase 3: ANN 索引の opt-in 採用）・Issue #403
  （`docs/design/ann-index-adoption.md` を Accepted 化）

## 背景・範囲

`docs/design/ann-index-adoption.md`（Issue #367・CORE-9・CORE-10・TASK-132）で
採用が判断された B 案（条件付き opt-in・自作 HNSW・依存追加なし）の実装分解の
うち、本タスクは**探索 API のみ**（Malkov & Yashunin 2016 の Algorithm 5
相当）を扱う。`search_engine.rs::SearchEngineKind` への variant 追加・
`core.rs`／`sql/` 結線（#407・実装済み。`docs/design/hnsw-search-engine-wiring.md`）、
並列構築（#406）、世代整合キャッシュ（#408）、
RLS 統合・切替（#409／#410）、`EXPLAIN` 露出（#411）、Recall ゲート接続
（#412）、前後比較（#413）、永続化はいずれも別タスクの担当であり、本タスクは
`hnsw.rs` 内部に閉じた実装（wire／SQL に露出しない・`wire_code` を新設しない）
に留める。

## 公開 API

```rust
pub struct HnswSearchScratch { /* private: VisitedBitmap */ }

impl HnswIndex {
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        scratch: &mut HnswSearchScratch,
    ) -> Result<Vec<engine::kernel::CandidateHit>, HnswError>;
}
```

- 手順: エントリポイントから上位層（`max_level..=1`）を `ef=1` の
  `greedy_descend`（#404 で実装済み・private）で貪欲降下 → 層 0 を実効幅
  `ef.max(k)` で `search_layer`（Algorithm 2・`pub(crate)`）によりビーム探索
  → 上位 `k` 件を返す。
- 結果は `kernel.rs::CandidateHit` と同じ順序規約（スコア降順・同点は id
  昇順）。`id` は `build` 時に渡された `vectors` 上のノード番号（0 始まり）
  を `u64` 化したもの。
- 実効 ef を `ef.max(k)` へ引き上げるのは、`k > ef` のときに結果集合が `k`
  件に満たない事故を防ぐため（hnswlib 等の一般的慣行）。`ef`・`k` は共に
  呼び出し前に `MAX_EF` 以下と検証済みのため `ef_eff` も `MAX_EF` 以下。
- `ef` は `search` の明示引数であり、`HnswIndex::params().ef_search`
  （`HnswParams` に保持されたビルド時の既定値）を暗黙に読んで代用すること
  はしない。`params().ef_search` は呼び出し元が渡すべき「推奨値」の位置づけ
  に留め、実際に使う `ef` を選ぶ責務は呼び出し元（#407 の provider 結線。
  `hnsw/provider.rs::HnswSearchProvider::effective_ef`。実装済み）に残す。
- **ベクトルの所有方針（codex-review PR #430 P1 指摘への対応で変更）**:
  当初は `build` と同様 `search` にも `vectors: &[f32]` を渡す借用契約
  だったが、長さのみの照合ではサイズが同じまま内容を書き換えた・行順を
  入れ替えたバッファを正常入力として受理してしまう問題があった（初出時
  の対応であるサンプリング・フィンガープリント照合も、サンプリング対象
  外の位置への書き換えは検出できない構造的な穴が残ると指摘された）。
  `HnswIndex::build` が完了時に `vectors` の内容を `Arc<[f32]>` として
  1 回コピーし `HnswIndex` 自身に所有させる設計へ変更し、`search` は
  この不変スナップショットのみを参照する（呼び出し元からベクトルを
  受け取る経路自体を廃した）。結果として「別バッファが渡される」という
  入力のクラスが存在しなくなり、長さ・内容の不一致を照合する必要が
  なくなった。トレードオフとして `build` 呼び出しごとに `n * dim * 4`
  バイトの追加コピーが恒久的に発生する（旧設計が避けていたコスト）。
  `arena.rs::VectorArena` が `vectors: Vec<f32>` のまま（`Arc` 化して
  いない）ため、この追加コピーを `Arc::clone`（参照カウントの増分のみ）
  へ縮退できるかは #408（世代整合キャッシュ）・#406（並列構築）側の
  設計課題として申し送る（詳細は `hnsw.rs` モジュール冒頭コメント参照）。

## 検証順序（fail-closed）

1. クエリ次元不一致 → `HnswError::QueryDimMismatch { expected, found }`
2. クエリの非有限値（NaN／Inf）→ `HnswError::NonFiniteQuery`
   （`kernel.rs::KernelError::NonFiniteQuery` と同じ理由。`total_cmp` は
   NaN を最大値扱いするため、事前に拒否しないと不正なクエリ 1 件が top-k を
   恒久的に占有し得る）
3. `ef == 0 || ef > MAX_EF || k > MAX_EF` → `HnswError::InvalidParams`
   （`MAX_EF` を上限に流用し、untrusted な呼び出し元が無制限の候補集合を
   要求できないようにする）
4. `k == 0` または空索引 → `Ok(Vec::new())`

`vectors` を呼び出し元から受け取らない設計（上記「ベクトルの所有方針」節）
のため、旧 `HnswError::VectorsLenMismatch`・`HnswError::VectorsContentMismatch`
は本タスク（#405）の対応で撤去した——検出すべき不一致の入力クラス自体が
存在しない。

## visited 集合の 2 実装

構築経路（`build`／`insert_node`）は世代カウンタ方式の `VisitedScratch`
（`epoch: Vec<u64>`。#404・codex-review #423 P1 指摘で O(N^2) 初期化を回避
する目的で導入）を維持する。探索経路（`HnswIndex::search`）にはビットマップ
方式の `VisitedBitmap`（`words: Vec<u64>`。1 ノード 1 bit）を新設した。

- 選定理由: 探索はクエリごとに繰り返し呼ばれ、`HnswSearchScratch` として
  呼び出し元（将来の provider）がスレッドごとに長期保持する想定である。
  ビットマップは epoch 方式（1 ノードあたり `u64` 8 バイト）の 8 分の 1
  のメモリ（1 ノードあたり 1 bit）で足り、100 万ノードでも 15,625 語
  （約 125 KB）の `memset` で `reset` できる。
- `search_layer`（Algorithm 2）は両実装を `pub(crate) trait VisitedSet {
  fn reset(&mut self, len: usize); fn mark_visited(&mut self, id: usize) ->
  Option<bool>; }` のジェネリックパラメータ `V: VisitedSet` として受け取る
  よう変更し、`VisitedScratch`・`VisitedBitmap` の双方に実装した。既存の
  構築経路の呼び出しは型推論により無変更のまま動作する。
- `VisitedBitmap::reset` は伸長のみで縮めない（呼び出し元が同一スクラッチ
  を異なる索引規模へ使い回す想定のため、再確保コストより多少の未使用
  メモリを許容する）。

## 決定性の保証範囲

同一索引・同一クエリ・任意のスクラッチ状態（新規／使い回し）で結果が
完全に再現することを保証する。総当たり経路（`kernel.rs`）が持つ「境界
同点グループの完全化」までは保証しない——`search_layer` の停止・受理判定は
`docs/design/hnsw-graph-construction.md`「順序規約」節のとおりスコアのみで
行われ、幅 `ef` を超えた時点の同点グループを全件拾い切る契約ではないため。
この保証範囲は本リポの実装既定値であり、spec 側の規範化は #405 の担当外
（`docs/design/ann-index-adoption.md`「非契約的な実装詳細」区分を踏襲）。

## フィクスチャ設計（`crates/engine/tests/hnsw_search.rs`）

- brute-force 対照: `engine::kernel::CpuScalarProvider`（production が使う
  総当たりカーネル）で正解 top-10 を求め、`Recall@10 = |HNSW top-10 ∩ brute
  top-10| / 10` をクエリ平均する。
- コーパス: 決定的シード・クラスタ中心 + ジッタ → L2 正規化（`hnsw.rs`
  冒頭の「cosine は正規化済みベクトルを渡して内積に一致させる」契約に
  合わせる）という、埋め込みらしい緩いクラスタ構造を採用した。クエリも
  同じクラスタ中心群から独立した乱数ストリームで生成し（コーパス外・
  連続値のため厳密に同一行を引く確率は無視できる）、完全な一様乱数の
  コーパス・クエリ（HNSW にとって最難条件）は受け入れ判定の対象にしない。
- 層 A（常時 `#[test]`。N=2,000・dim=32・20 クラスタ・`HnswParams::default()`
  〔m=16／ef_construction=100／ef_search=64〕）: debug 実行で数秒以内の
  回帰保護。
- 層 B（`#[ignore]`・受け入れ条件の正本。N=10,000・dim=128・80 クラスタ・
  `HnswParams::default()`）: `make hnsw-search-recall` で release 実行する。
  debug では `HnswIndex::build`（10k×dim128）が約 110 秒かかるため常時 CI
  には含めない。層 A・層 B とも同一の既定パラメータで受け入れ判定を行い、
  クラスタ構造ありコーパス・クエリでは規模・次元によらず既定パラメータで
  条件を満たすことを確認している。
- 層 B には受け入れ判定と別に、完全な一様乱数のコーパス・クエリ（クラスタ
  構造を持たない。HNSW にとって最難条件の一つ）での Recall@10 も
  informational（アサーションなし・`println!` 出力のみ）として併記する。

## 実測 Recall（層 B・`make hnsw-search-recall`）

| コーパス | 規模 | ef | Recall@10 | 判定 |
| --- | --- | --- | --- | --- |
| クラスタ構造あり（既定パラメータ） | N=10,000・dim=128 | 64 | 1.0000 | 受け入れ判定（≥0.95） |
| クラスタ構造あり（既定パラメータ） | N=10,000・dim=128 | 256 | 1.0000 | 受け入れ判定（≥0.99） |
| 一様乱数のみ（既定パラメータ） | N=10,000・dim=128 | 64 | 0.6410 | informational（アサーションなし） |
| 一様乱数のみ（既定パラメータ） | N=10,000・dim=128 | 256 | 0.9535 | informational（アサーションなし） |

Issue #405 の受け入れ条件（ef=64 で ≥0.95、ef=256 で ≥0.99）はクラスタ構造
ありフィクスチャで既定パラメータのまま満たされたため、`ef_construction`／
`m` の引き上げやヒューリスティック（`extend_candidates`）の見直しは、この
フィクスチャに関する限り不要と判断した。一方で一様乱数のみのコーパスでは
同じ既定パラメータで Recall@10 が明確に低下する（ef=64 で 0.6410）ことを
実測しており、埋め込み分布がクラスタ構造から離れる場合に見直しが必要になる
可能性は残る——「不要」という判断はクラスタ構造ありフィクスチャの範囲に
限定される。実データ規模・実埋め込み分布での再評価は Issue #412〜#413
（Recall ゲート接続・前後比較）の担当。

## 決定性テストの構成

`crates/engine/tests/hnsw_search.rs` に以下を固定する:

1. 同一索引・同一スクラッチでの反復呼び出しが `Vec<CandidateHit>` 完全一致
2. 新規 `HnswSearchScratch` でも結果が同一（スクラッチ状態に非依存）
3. 同一 seed で再構築した索引でも結果が同一
4. 重複ヘビーコーパス（同点スコア多発）でも 1〜3 が成り立ち、結果内の同点
   が id 昇順であること

## #405〜#408 への申し送り

- `HnswSearchScratch` は呼び出し元（#407 の provider 結線。実装済みだが
  スクラッチの再利用自体は #408 の索引実利用結線で行う）がスレッドごとに
  1 つ所有し、クエリをまたいで再利用する契約
- `id` は `build` 時に渡した `vectors` 上のノード番号であり、呼び出し元が
  RLS 事前フィルタ後の縮約ベクトル集合を構築・渡す前提（`kernel.rs::
  SearchInput` と同じ境界。`PolicyContext::is_visible` 単一照合パスは
  #409／#410 が維持する）
- 決定性の保証範囲（上記節）は spec 側未確定のため、#409 以降で規範化する
  場合はこの記録を出発点にすること
- `HnswIndex` は `build` 完了時に `vectors` を `Arc<[f32]>` として所有する
  （「ベクトルの所有方針」節。codex-review PR #430 P1 指摘対応）。#408 の
  世代整合キャッシュ・#406 の並列構築を設計する際は、`arena.rs::
  VectorArena` 側が `vectors` を最初から `Arc<[f32]>` として持てるかを
  検討すること——`build` 時のコピーを `Arc::clone`（参照カウントの増分の
  み）へ縮退できる可能性がある
