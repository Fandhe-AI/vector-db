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
- 参照区間（変更を含まない区間）: 同一プロセス内の brute-force Top-k
  （`engine::kernel::CpuScalarProvider`。`kernel.rs` は #490 で無変更）
- min-of-N（N=5）・median を両方記録し、参照区間の実測ノイズ帯
  （`(max − min) / min`）を判定材料として併記する
  （`docs/design/benchmark-judgement-policy.md` §4）

### 環境（policy §3）

- CPU: `QEMU Virtual CPU version 2.5+`（KVM）・12 vCPU
- 命令セットフラグ: `avx2` `fma` `f16c` あり・`avx512*` 無し（`lscpu` 全文で確認）
- 負荷: 各 run 直前の `loadavg` は概ね 1.5〜4（別プロセスと共有・非専有。
  `BENCH_DEDICATED_ENV` 未設定）
- **判定不能な施策種別**: `docs/design/benchmark-judgement-policy.md` §6 は
  「キャッシュ規模依存のレイアウト最適化（CSR 化・prefetch・チャンク連続
  格納）」を本開発環境で構造的に判定不能な種別として既に列挙している
  （関連 Issue #364・#489・#492）。本 Issue（#491）の対象（`search_layer`
  への prefetch。Issue #490）も同一種別に該当する

### 実測表（8 規模点。単位 µs。`ratio = after_min / before_min`）

| 規模点 | before min | before median | after min | after median | ratio(min) | ratio(median) | 判定クラス（±5%） | 参照区間帯(before) | 参照区間帯(after) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 10k × dim128・マスクなし | 32.010 | 35.962 | 31.488 | 35.061 | 0.9837 | 0.9749 | Neutral | 25.62% | 22.10% |
| 10k × dim128・可視率50% | 57.970 | 65.118 | 57.970 | 65.179 | 1.0000 | 1.0009 | Neutral | 27.34% | 120.53% |
| 10k × dim768・マスクなし | 165.312 | 197.651 | 153.538 | 208.413 | 0.9288 | 1.0544 | Improved(min)/Neutral(median) | 24.76% | 116.83% |
| 10k × dim768・可視率50% | 111.585 | 127.242 | 107.032 | 131.012 | 0.9592 | 1.0296 | Neutral | 54.92% | 44.44% |
| 100k × dim128・マスクなし | 97.167 | 119.059 | 92.060 | 113.835 | 0.9474 | 0.9561 | Improved | 31.64% | 27.87% |
| 100k × dim128・可視率50% | 433.367 | 464.756 | 426.396 | 459.998 | 0.9839 | 0.9898 | Neutral | 39.48% | 26.67% |
| 100k × dim768・マスクなし | 370.097 | 463.861 | 363.885 | 463.039 | 0.9832 | 0.9982 | Neutral | 38.40% | 31.51% |
| 100k × dim768・可視率50% | 573.329 | 633.558 | 555.634 | 612.037 | 0.9691 | 0.9660 | Neutral | 14.45% | 10.17% |

per-run 生値（min_us・5 ペア）:

```text
10k_d128_none:    before=[32.177, 32.010, 32.579, 32.590, 32.227] after=[31.632, 31.590, 31.703, 31.488, 31.559]
10k_d128_mask50:  before=[59.140, 59.080, 60.143, 60.376, 57.970] after=[58.705, 58.804, 58.437, 57.970, 59.143]
10k_d768_none:    before=[165.336, 165.668, 169.904, 165.312, 193.131] after=[153.538, 169.878, 165.261, 178.058, 196.701]
10k_d768_mask50:  before=[111.812, 127.515, 111.585, 165.583, 113.027] after=[124.302, 130.486, 113.255, 108.146, 107.032]
100k_d128_none:   before=[99.082, 97.867, 97.167, 97.611, 99.308] after=[95.197, 92.060, 94.425, 95.129, 93.049]
100k_d128_mask50: before=[435.929, 437.191, 433.367, 459.262, 441.618] after=[433.114, 426.396, 440.529, 439.705, 432.657]
100k_d768_none:   before=[485.168, 382.000, 380.467, 370.097, 419.052] after=[392.358, 363.885, 373.470, 375.437, 390.999]
100k_d768_mask50: before=[573.329, 578.636, 573.912, 576.224, 573.588] after=[571.055, 558.711, 555.634, 556.742, 556.348]
```

すべての可視率50%点で `masked_short_queries=0`（`k` 未満の返却は発生せず、
非 vacuous な計測であることを確認）。

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

実測は 8 点中 `Regressed`（固定 ±5% 帯かつ参照区間の実測帯を両方超える悪化）
が 0 点、`Improved` が 2 点（`10k×dim768マスクなし`〔min のみ〕・
`100k×dim128マスクなし`）、残り 6 点は `Neutral` で、悪化方向への一貫した
シグナルは観測されなかった。したがって:

- **Rejected（撤回）にはしない**: 悪化の一貫パターンが無く、撤回条件
  （過半の点で `Regressed` が min-of-N・median 双方で一貫）を満たさない
- **Accepted と断定もしない**: 「速そうなので Accepted」と書くことは
  policy §5 で明確に禁止されている。参照区間帯が最大 120.53%（`10k×dim128
  ・可視率50%` の after 側）に達するなど、この環境・この規模での run-to-run
  変動そのものが対象区間の観測差分（`ratio` はおおむね 0.93〜1.05x）と
  同程度かそれ以上あり、共有 QEMU 環境のノイズから prefetch の効果を
  切り分けて確認できたとは言えない
- **ステータス: 保留（production 無変更）。既にマージ済み・ビット同一性
  検証済みのコード（#490）を、切り分けられていない数値だけで撤回するのは
  非破壊側の判断ではないと判断した。専有実機（`BENCH_DEDICATED_ENV=1`）
  での再実測をオーナーへ申し送る**

### 申し送り

- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測手順: `make bench-hnsw-search`
  に `BENCH_HNSW_SEARCH_ROWS`／`BENCH_HNSW_SEARCH_DIM`／`BENCH_HNSW_SEARCH_MASK`
  を指定し、before/after バイナリ（`git archive <commit> | tar -x` で取り出し
  た作業ツリーへ本 Issue の新設 3 ファイルを追加コピーし `cargo bench
  --no-run` でビルドする）を交互 5 ペア以上で起動する。100k×768 の 1 点が
  最も時間を要する（1 run あたり約 110 秒）
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
CARGO_TARGET_DIR=/path/to/target-before cargo bench --bench hnsw_search_bench -p engine --no-run
CARGO_TARGET_DIR=/path/to/target-after  cargo bench --bench hnsw_search_bench -p engine --no-run
# 8 規模点 × 交互 5 ペアで両バイナリを起動（BENCH_HNSW_SEARCH_ROWS／
# BENCH_HNSW_SEARCH_DIM／BENCH_HNSW_SEARCH_MASK を指定）
```
