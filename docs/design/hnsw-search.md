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
RLS 統合・切替（#409／#410）、`EXPLAIN` 露出
（#411・実装済み。`docs/design/explain-search-engine-exposure.md` 参照）、Recall ゲート接続
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

## 受理判定後 prefetch（Issue #490）

`search_layer` の隣接ループへ、hnswlib `searchBaseLayerST` に倣うソフトウェア
パイプライン先読みを追加した（`hnsw/prefetch.rs`）。隣接リストの先頭要素を
ループ開始前に、以降は各反復で次の要素を先読みする（距離 1）。Issue #431
是正で確立した契約は「非受理（マスク外）ノードのベクトルには一切触れない
（`self.score` を呼ばない）」であり、visited スロットへの訪問済みマークは
この契約の対象外（`search_layer_with` は受理判定より前に非受理ノードへも
通常どおりマークを付ける。既存の探索契約を変更しない）。本 Issue で追加した
先読み処理（ベクトル・visited スロットいずれの読み出しも）はこれとは別に、
非受理ノードへは一切発行しない契約を持つ——先読みは `is_accepted` 判定を
通過した後にのみ行う。

### stable での制約

新規 `unsafe` を追加しない制約下では、真の prefetch 命令
（`core::arch::{x86_64,aarch64}` の `_mm_prefetch`／`_prefetch`）は
`#[target_feature]` 付き関数の内側でのみ safe に呼べる（通常の fn から
呼ぶと E0133）ため発行できない。`core::hint::prefetch_read`
（`hint_prefetch` feature）も stable では未安定化。本実装は
`core::hint::black_box` による早期 load（best-effort。真の prefetch より
弱い保証）で代替し、差し替え箇所を `hnsw/prefetch.rs` の 2 関数
（`touch_node_vector`・`touch_word`）に隔離した。

### 検証構成

`search_layer` の本体を `search_layer_with<V, P: PrefetchPolicy>` へ分離し
（`search_layer` は production 用 `PipelinePrefetch` を渡す薄いラッパ）、
`#[cfg(test)]` の `NoPrefetch`／`RecordingPrefetch` で以下を機械検証する
（`hnsw.rs::tests`）:

- prefetch の有無で `search_layer` の結果がビット同一（クラスタコーパス・
  重複ヘビーコーパス・小 dim の 3 フィクスチャ × `ef ∈ {1, 10, 40}`）
- `NodeMask` 付き探索（`Subset` 形状）でも同様にビット同一
- 先読み要求されたノード id がすべて `NodeMask` の受理ノードであること
  （P0 契約の直接検証。記録が非空であることも固定し vacuous pass を防止）

`parallel_build.rs::search_layer_locked`（並列構築のロック対応版）・
`greedy_descend`／`greedy_descend_masked`（上位層貪欲降下）・エントリ
ポイントループへの先読みは本 Issue では未適用（#491 で効果確認後に別
Issue で検討）。効果の前後比較・採否は #491 の担当。

## Issue #491: 受理判定後 prefetch の前後比較と採否

### 対象・方法

- before: `4d2bd23`（`eabff3a` の親。prefetch 導入前）
- after: `eabff3a`（`perf(engine): search_layer に受理判定後の隣接ベクトル・
  visited prefetch を追加する (#574)`。#490 の実装）
- `git diff 4d2bd23 eabff3a -- Cargo.lock crates/engine/Cargo.toml` は空
  （同一 `Cargo.lock` で before/after をビルド。`docs/design/
  benchmark-judgement-policy.md` §3 の要件）
- 新設ベンチ 3 ファイル（`benches/hnsw_search_bench.rs`・
  `benches/harness/hnsw_search_latency.rs`・`harness/mod.rs` の 1 行・
  `Cargo.toml` の `[[bench]]`）だけを `git archive` した各コミットのツリー
  へ個別に追加し、`CARGO_TARGET_DIR` を分離して `cargo bench --no-run` で
  ビルドした 2 バイナリを、8 規模点（`{10k, 100k} 行 × {128, 768} 次元 ×
  {マスクなし, 可視率 50%}`）それぞれについて交互 5 ペア（before→after を
  1 ペアとして 5 回）起動した
- 索引構築は逐次 `HnswIndex::build`（`build_with_threads` は使わない）。
  同一シードなら before/after で完全に同一のグラフになるため、探索
  レイテンシの差分が「同じグラフに対する prefetch の有無」だけに帰属する
- コーパス・クエリは決定的 PRNG（`DeterministicRng`）で生成し L2 正規化
  （`harness::hnsw_compare::l2_normalize_corpus` を再利用）。マスクは
  `NodeMask` を可視率どおりベルヌーイ試行で決定的に生成（`Subset` 形状を
  模す。RLS 事前フィルタ統合〔Issue #409〕の実運用条件）
- 参照区間（変更〔prefetch〕を含まない区間）: 探索本体と同一のクエリ
  サイクル・同一シードで回した brute-force Top-k（`engine::kernel::
  CpuScalarProvider`。`kernel.rs` は #490 で無変更）。**ノイズ帯（実測帯）
  は単一プロセス内では算出しない**——単一プロセス内の分布には同じクエリ
  サイクルに含まれる各クエリ間の所要時間差が混入し、`docs/design/
  benchmark-judgement-policy.md` §4 が求める run-to-run（プロセス実行間）
  幅にならないため（codex-review 指摘・Issue #491）。各プロセスは
  `min_us`／`median_us` という代表値のみを出力し（`hnsw_search_bench.rs`
  の `render_reference_line`）、実測帯はこの代表値を交互起動した複数
  プロセス（本 Issue では 1 規模点あたり before 5 回＋after 5 回＝10 回）
  から集めた値列に対して `harness::hnsw_search_latency::reference_band`
  を適用して算出する。before/after で参照区間側（brute-force 経路）は
  変更されていないため（`kernel.rs` 無変更）、本 doc では before・after
  10 プロセス分の代表値を 1 つの値列にプールして算出する（pooled 方式。
  before 側単独・after 側単独に分けた帯より母数が大きく、ノイズ推定が
  より安定するため採用）
- min-of-N（N=5）・median を両方記録し、参照区間の実測ノイズ帯（pooled・
  `(max − min) / min`）を判定材料として併記する
  （`docs/design/benchmark-judgement-policy.md` §4）

### 環境（policy §3）

- CPU: `QEMU Virtual CPU version 2.5+`（KVM）・12 vCPU
- 命令セットフラグ: `avx2` `fma` `f16c` あり・`avx512*` 無し（`lscpu` 全文で確認）
- 負荷: 各 run 直前の `loadavg` は概ね 3〜9（別プロセスと共有・非専有。
  `BENCH_DEDICATED_ENV` 未設定）
- **判定不能な施策種別**: `docs/design/benchmark-judgement-policy.md` §6 は
  「キャッシュ規模依存のレイアウト最適化（CSR 化・prefetch・チャンク連続
  格納）」を本開発環境で構造的に判定不能な種別として既に列挙している
  （関連 Issue #364・#489・#492）。本 Issue（#491）の対象（`search_layer`
  への prefetch。Issue #490）も同一種別に該当する

### 実測表（8 規模点。単位 µs。`ratio = after / before`）

#### 表 1: 実測値・固定 ±5% 帯による判定クラス

`判定クラス` は `docs/design/benchmark-judgement-policy.md` §2・§4 の
`classify_change` に相当する固定相対帯（±5%）のみによる分類であり、
この帯だけを根拠に採否を決めない（表 2 の「両帯超過」で採否根拠の
可否を別途判定する）。

| 規模点 | before min | before median | after min | after median | ratio(min) | ratio(median) | 判定クラス(min) | 判定クラス(median) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 10k × dim128・マスクなし | 31.730 | 36.737 | 31.317 | 35.472 | 0.9870 | 0.9656 | Neutral | Neutral |
| 10k × dim128・可視率50% | 54.996 | 61.045 | 58.754 | 65.916 | 1.0683 | 1.0798 | Regressed | Regressed |
| 10k × dim768・マスクなし | 167.345 | 207.130 | 165.395 | 195.513 | 0.9883 | 0.9439 | Neutral | Improved |
| 10k × dim768・可視率50% | 105.274 | 120.982 | 105.527 | 122.124 | 1.0024 | 1.0094 | Neutral | Neutral |
| 100k × dim128・マスクなし | 97.536 | 121.216 | 92.330 | 113.062 | 0.9466 | 0.9327 | Improved | Improved |
| 100k × dim128・可視率50% | 427.997 | 449.890 | 431.991 | 457.543 | 1.0093 | 1.0170 | Neutral | Neutral |
| 100k × dim768・マスクなし | 355.731 | 476.746 | 364.344 | 497.237 | 1.0242 | 1.0430 | Neutral | Neutral |
| 100k × dim768・可視率50% | 563.955 | 637.557 | 554.073 | 618.216 | 0.9825 | 0.9697 | Neutral | Neutral |

`before min`／`after min` は各点 5 プロセス（ペア）の `min_us` の
min-of-5、`before median`／`after median` は各点 5 プロセスの
`median_us` の median-of-5（下記「per-run 生値」から再計算可能）。

#### 表 2: 参照区間の実測ノイズ帯・両帯超過（採否根拠として使える変化）の判定

`参照帯` は pooled 方式（before 5 プロセス＋after 5 プロセス＝10 プロセス
の代表値を 1 つの値列として `reference_band` へ渡した算出値。単位は
`min`／`median` いずれも百分率）。`両帯超過` は
`|ratio − 1.0| > 0.05` かつ `|ratio − 1.0| > 参照帯/100` の両方を満たす
場合のみ `Yes`（benchmark-judgement-policy.md §4「両ノイズ帯を超えるとは」
の定義どおり、`Improved`／`Regressed` いずれの方向にも対称に適用する。
表 1 の「判定クラス」が `Improved`／`Regressed` でも参照帯が広く
`両帯超過` に至らない点（例: `100k×dim128・マスクなし` は min・median
とも `Improved` だが参照帯 20.67%／42.34% には収まらず `No`）と、固定帯
・参照帯の双方を超えて残る点（`10k×dim128・可視率50%` のみ）とが
併存することが、「固定帯の判定クラス」と「採否に使える変化」を区別する
具体例）。

| 規模点 | 参照帯(min) | 参照帯(median) | 両帯超過(min) | 両帯超過(median) |
| --- | --- | --- | --- | --- |
| 10k × dim128・マスクなし | 23.36% | 19.77% | No | No |
| 10k × dim128・可視率50% | 2.66% | 2.67% | **Yes** | **Yes** |
| 10k × dim768・マスクなし | 7.84% | 10.26% | No | No |
| 10k × dim768・可視率50% | 14.55% | 26.74% | No | No |
| 100k × dim128・マスクなし | 20.67% | 42.34% | No | No |
| 100k × dim128・可視率50% | 3.21% | 6.76% | No | No |
| 100k × dim768・マスクなし | 16.48% | 42.26% | No | No |
| 100k × dim768・可視率50% | 4.00% | 12.71% | No | No |

per-run 生値（`target=hnsw_search`・`reference=brute_force` それぞれの
`min_us`／`median_us`。5 ペア分。表 1・表 2 の集計元データ）:

```text
10k_d128_none:
  target_min:    before=[33.134, 32.822, 32.568, 33.264, 31.730] after=[32.858, 31.602, 31.647, 31.669, 31.317]
  target_median: before=[37.193, 36.294, 36.737, 37.744, 36.417] after=[39.049, 35.631, 35.232, 35.472, 35.288]
  ref_min:       before=[98.992, 81.499, 80.244, 81.525, 81.711] after=[82.330, 81.456, 81.612, 94.333, 81.469]
  ref_median:    before=[101.706, 86.087, 84.916, 86.639, 86.384] after=[88.193, 85.990, 85.949, 97.010, 86.173]

10k_d128_mask50:
  target_min:    before=[55.590, 54.997, 54.996, 55.513, 56.108] after=[59.837, 58.754, 60.686, 59.293, 59.791]
  target_median: before=[61.313, 61.141, 61.045, 60.928, 60.991] after=[65.916, 65.435, 66.052, 65.807, 65.968]
  ref_min:       before=[81.073, 80.918, 79.970, 81.165, 81.581] after=[81.154, 81.119, 81.203, 81.417, 82.097]
  ref_median:    before=[85.843, 85.666, 84.393, 85.935, 86.414] after=[85.854, 85.772, 85.995, 86.179, 86.648]

10k_d768_none:
  target_min:    before=[171.106, 175.597, 171.472, 167.345, 169.029] after=[165.395, 169.019, 167.451, 165.831, 168.793]
  target_median: before=[201.883, 211.133, 207.130, 207.109, 209.195] after=[193.191, 194.602, 195.903, 195.513, 197.576]
  ref_min:       before=[760.089, 818.352, 768.997, 809.860, 789.198] after=[773.809, 763.170, 773.620, 819.717, 786.236]
  ref_median:    before=[787.814, 848.979, 798.762, 848.204, 825.625] after=[868.619, 791.809, 801.891, 852.516, 814.923]

10k_d768_mask50:
  target_min:    before=[105.274, 106.151, 110.749, 106.025, 116.574] after=[105.838, 116.423, 127.250, 105.527, 106.907]
  target_median: before=[118.785, 120.982, 127.513, 120.358, 130.642] after=[120.900, 144.775, 148.302, 120.283, 122.124]
  ref_min:       before=[811.545, 771.381, 838.155, 821.559, 762.721] after=[817.028, 873.718, 781.605, 811.556, 793.988]
  ref_median:    before=[837.063, 815.996, 891.043, 850.213, 789.735] after=[865.041, 1000.887, 829.038, 841.311, 825.409]

100k_d128_none:
  target_min:    before=[97.536, 99.745, 99.311, 114.226, 99.281] after=[93.154, 92.709, 92.330, 92.520, 95.429]
  target_median: before=[117.653, 121.809, 121.216, 145.008, 117.695] after=[113.238, 112.833, 110.050, 113.062, 113.532]
  ref_min:       before=[1796.346, 2009.329, 1979.172, 2167.659, 2009.626] after=[1951.177, 1934.946, 1995.044, 2010.843, 1987.943]
  ref_median:    before=[1880.079, 2125.078, 2034.358, 2676.177, 2074.485] after=[2320.033, 2091.521, 2058.801, 2067.197, 2051.366]

100k_d128_mask50:
  target_min:    before=[430.216, 427.997, 429.438, 429.140, 431.117] after=[439.690, 433.967, 442.308, 431.991, 435.479]
  target_median: before=[449.890, 448.879, 448.532, 452.132, 450.387] after=[463.381, 457.543, 464.103, 453.781, 453.870]
  ref_min:       before=[1980.843, 1976.598, 1977.827, 1987.810, 2017.942] after=[2035.405, 1983.474, 2040.087, 2011.972, 1996.406]
  ref_median:    before=[2037.215, 2048.594, 2059.394, 2039.337, 2088.492] after=[2174.837, 2056.751, 2169.444, 2086.112, 2049.812]

100k_d768_none:
  target_min:    before=[355.731, 371.497, 573.918, 375.548, 391.831] after=[430.900, 364.344, 376.173, 393.926, 477.322]
  target_median: before=[434.800, 455.147, 760.078, 476.746, 498.414] after=[551.651, 449.520, 455.432, 497.237, 604.914]
  ref_min:       before=[12224.855, 13708.181, 13661.991, 13928.550, 14239.645] after=[12368.938, 12857.951, 13929.325, 14229.091, 14031.782]
  ref_median:    before=[12457.964, 14092.931, 14285.526, 14251.558, 17722.918] after=[12783.316, 13099.831, 14253.119, 14503.378, 14494.014]

100k_d768_mask50:
  target_min:    before=[563.955, 630.958, 570.585, 570.674, 590.570] after=[556.195, 635.612, 554.073, 562.234, 556.211]
  target_median: before=[622.606, 715.489, 637.557, 629.691, 651.865] after=[610.840, 734.747, 618.216, 624.436, 610.902]
  ref_min:       before=[14021.071, 14495.996, 14076.967, 14000.292, 14055.228] after=[13959.821, 14397.984, 14087.647, 13946.255, 13938.387]
  ref_median:    before=[14238.742, 15877.196, 14637.363, 14255.219, 14289.322] after=[14256.561, 15992.073, 14706.054, 14189.190, 14269.151]
```

すべての可視率50%点で `masked_short_queries=0`（`k` 未満の返却は発生せず、
非 vacuous な計測であることを確認。`hnsw_search_bench.rs` の
`call_index` ガードにより warmup フェーズの検索は計測に含まれない
——Bugbot 指摘・Issue #491）。

### `make hnsw-search-recall` 不変確認（Recall@10。ef=64／256）

| フィクスチャ | ef | before | after |
| --- | --- | --- | --- |
| クラスタ構造あり | 64 | 1.0000 | 1.0000 |
| クラスタ構造あり | 256 | 1.0000 | 1.0000 |
| 一様乱数（informational） | 64 | 0.6410 | 0.6410 |
| 一様乱数（informational） | 256 | 0.9535 | 0.9535 |

4 値とも完全一致（`hnsw.rs::tests::search_layer_prefetch_*` が固定するビット
同一契約と整合。prefetch が探索結果に影響しないことを実データ規模でも確認）。

### 判定と採否

Issue #491 が要求する 2 条件——(a) 8 点いずれもノイズ帯内なら Rejected・撤回、
(b) QEMU 共有環境の数値は採否根拠にしない——は本環境では同時に満たせない。
`docs/design/benchmark-judgement-policy.md` §5 は共有 QEMU 環境で
**Accepted を不可**、**Rejected は「両ノイズ帯（固定 ±5% 帯・参照区間実測帯）
を超える一貫した悪化＋静的解析の裏付け」がある場合のみ可**と定める。

参照ノイズ帯をプロセス実行間の代表値列から算出する方式（表 2）に修正した
結果、8 点中 1 点（`10k×dim128・可視率50%`）が min-of-N・median 双方で
固定 ±5% 帯・参照区間実測帯の**両方**を超える悪化（`両帯超過=Yes`）として
残った。この点は 5 ペアすべてで before の最大値 (56.108µs) より after の
最小値 (58.754µs) が大きく、区間が重ならない一貫した悪化であり、参照帯も
2.66〜2.67% と小さいためノイズでは説明しにくい。他の 7 点はいずれの方向にも
両帯超過に至らない（`Improved` 方向も `100k×dim128・マスクなし` が固定帯
のみ超過・参照帯 20.67%/42.34% には収まらず両帯超過は `No`）。

- **Rejected（撤回）にはしない**: `docs/design/benchmark-judgement-policy.md`
  §5 の撤回条件は「両ノイズ帯を超える一貫した悪化＋静的解析／実アセンブリの
  裏付けがある場合」である。`10k×dim128・可視率50%` は両帯超過の悪化という
  前半条件は満たすが、後半の静的解析／実アセンブリによる裏付けは本 Issue の
  対象外（#490 のビット同一性検証止まり）で未実施のため、条件を完全には
  満たさない
- **Accepted と断定もしない**: 「速そうなので Accepted」と書くことは
  policy §5 で明確に禁止されている。7 点は `Improved`／`Neutral` 方向で
  両帯超過に至らず、prefetch 導入の一貫した改善効果を主張できる根拠にも
  ならない
- **`10k×dim128・可視率50%` の悪化は申し送り事項として明記する**:
  1 点のみとはいえ両ノイズ帯を超える一貫した悪化であり、旧・誤った
  算出方式（単一プロセス内のクエリ間差をノイズ帯として扱っていたため
  この点の参照帯が 27.34%／120.53% と過大評価され `Neutral` に埋もれて
  いた）では見えていなかった signal である。本 doc の「判断」を Rejected
  へは倒さないが、この 1 点に限定した追加実測・原因調査は申し送る
- **ステータス: 保留（production 無変更）。既にマージ済み・ビット同一性
  検証済みのコード（#490）を、8 点中 1 点の悪化のみで撤回するのは
  非破壊側の判断ではないと判断した。専有実機（`BENCH_DEDICATED_ENV=1`）
  での再実測——特に `10k×dim128・可視率50%` の悪化が専有環境でも
  再現するかの確認——をオーナーへ申し送る**

### 申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測手順: `make bench-hnsw-search`
  に `BENCH_HNSW_SEARCH_ROWS`／`BENCH_HNSW_SEARCH_DIM`／`BENCH_HNSW_SEARCH_MASK`
  を指定し、before/after バイナリ（`git archive <commit> | tar -x` で取り出し
  た作業ツリーへ本 Issue の新設 3 ファイルを追加コピーし `cargo bench
  --no-run` でビルドする）を交互 5 ペア以上で起動する。100k×768 の 1 点が
  最も時間を要する（1 run あたり約 90〜115 秒。本開発環境の実測）
- **`10k×dim128・可視率50%` の悪化（表 1・表 2、上記「判定と採否」）は
  専有環境での優先再確認対象とする**: 8 点中唯一、両ノイズ帯を超える
  一貫した悪化が観測された規模点であり、専有環境で再現すれば prefetch
  導入（#490）の当該条件下（小規模・部分可視マスク）での撤回・条件付き
  適用を検討する材料になる
- `search_layer_locked`（並列構築のロック対応版）・`greedy_descend`／
  `greedy_descend_masked`（上位層貪欲降下）・エントリポイントループへの
  prefetch 適用検討は別 Issue（本 Issue の対象外のまま）
- 真の prefetch 命令（`_mm_prefetch`／`_prefetch`）への差し替えは新規
  `unsafe` 1 箇所を要するオーナー承認事項であり、本実測は「現状の
  `black_box` 方式に効果があるかどうか」の判断材料に留まる（効果を
  確実に測れなかったこと自体は、真の prefetch 命令への投資判断を積極的に
  後押しする根拠にはならない）

### 再現方法

```bash
git fetch origin main
git archive 4d2bd23 | tar -x -C /path/to/before
git archive eabff3a | tar -x -C /path/to/after
# 各ツリーへ benches/hnsw_search_bench.rs・benches/harness/hnsw_search_latency.rs・
# harness/mod.rs の `pub mod hnsw_search_latency;` 追記・Cargo.toml の
# [[bench]] 追記 を適用してから:
# BENCH_HNSW_SEARCH_COMMIT をビルド時に指定し、計測対象コミットをバイナリへ
# 焼き込む（`git archive` で取り出した作業ツリーには .git が無く、指定しない
# 場合の実行時フォールバック（git rev-parse HEAD）はカレントディレクトリの
# HEAD を返すため、同じ作業ディレクトリから before/after を交互起動すると
# 両方に同一値が記録されてしまう。codex-review 指摘・Issue #491）。
BENCH_HNSW_SEARCH_COMMIT=4d2bd23 CARGO_TARGET_DIR=/path/to/target-before cargo bench --manifest-path /path/to/before/Cargo.toml --bench hnsw_search_bench -p engine --no-run
BENCH_HNSW_SEARCH_COMMIT=eabff3a CARGO_TARGET_DIR=/path/to/target-after  cargo bench --manifest-path /path/to/after/Cargo.toml  --bench hnsw_search_bench -p engine --no-run
# 8 規模点 × 交互 5 ペアで両バイナリを起動（BENCH_HNSW_SEARCH_ROWS／
# BENCH_HNSW_SEARCH_DIM／BENCH_HNSW_SEARCH_MASK を指定）。各プロセスは
# target/reference いずれも代表値（min_us／median_us）のみを出力する。
# 参照区間の実測ノイズ帯（表 2）は単一プロセスの出力からは算出できず、
# 交互起動した複数プロセス（本 doc では 1 規模点あたり計 10 プロセス分）
# の代表値列を reference_band へ渡して別途算出する。
```
