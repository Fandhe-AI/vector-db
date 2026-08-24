# ADR: ハイブリッド検索 Recall 回帰ハーネス（TASK-104）

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-104（`docs/spec/05-tasks.md`・`docs/spec/06-roadmap.md` 参照）
- 対象ビヘイビア: SEARCH-1, SEARCH-2（`docs/spec/04-behavior/search.md`）
- 前提: TASK-102（BM25 疎検索カーネル・`crates/engine/src/sparse.rs`）・TASK-103
  （RRF 融合・`crates/engine/src/hybrid.rs`）・TASK-105（CJK ストップワード除去）は
  いずれもマージ済み（PR #138・#142・#144）
- 関連: TASK-106（`docs/design/cjk-tokenizer-impact-ja-corpus.md`。決定的合成コーパス
  生成・固定値回帰トラッキング方式の先行実装）・TASK-127（`crates/engine/benches/
  parallel_bench.rs`。spec 閾値の Actions variables 注入パターンの先行実装）

## 背景

ハイブリッド検索カーネル（疎 BM25・RRF 融合・CJK ストップワード除去）は実装・マージ
済みだが、検索品質（Recall）の受け入れ基準を自動チェックする回帰テストが存在しな
かった。本 ADR は TASK-104 に対応し、小規模・大規模の 2 段のスケール条件付き Recall
基準を自動チェックする回帰テスト＋CI workflow を追加した設計を記録する。

**位置づけ**: TASK-106 の先例（`docs/design/cjk-tokenizer-impact-ja-corpus.md`）に
倣い、本タスクは production コード（`crates/engine/src/`）を変更せず、既存 API に
対する実測ハーネス（回帰テスト）・本レポート・CI workflow の追加に限定した。

## 検証設計

### 2 層構成（PR CI と閾値ゲートの分離）

- **層 A**（`#[test]`。常時 `cargo test` 対象）: 決定的コーパスでのヒット数を固定値
  アサーションで回帰トラッキングする（TASK-106 と同方式）。spec の数値基準を使わない
  ため public 資産に閾値を持ち込まない
- **層 B**（`#[ignore]`。`make recall-regression` 経由）: spec 由来の Recall 下限
  （`HYBRID_RECALL_MIN_R20_SMALL`・`HYBRID_RECALL_MIN_R20_LARGE`・
  `HYBRID_RECALL_MIN_R100_LARGE`。`.github/workflows/recall.yml` が Actions
  variables から注入）と実測値を比較する閾値ゲート。未設定・非数値・範囲外は
  fail-closed でテスト失敗とし、ログには実測値と pass/fail のみを出力する
  （`crates/engine/benches/parallel_bench.rs` と同方針。README「Recall 回帰ハーネス
  の repo variables」参照）

### コーパス・QA セットの生成

`crates/engine/tests/hybrid_recall.rs` は決定的シード付き擬似乱数（xorshift64*。
外部クレート不使用）で合成コーパスと QA セットを生成する（TASK-106 の設計を踏襲）。

- 文書のキーワードは Zipf 近似分布（重み `1/(i+1)`）で語彙プールから 3〜6 語を選ぶ
- QA セットは各文書から出現頻度が最も低いキーワード 2 語を選び、その AND 一致
  文書集合を正解集合とする `direct` カテゴリ相当のクエリ
- 語彙数（`vocab_size`）はコーパス規模に応じて可変にする: 固定語彙表を使うと大規模
  段で正解集合が肥大化し、Recall@k の理論上限（Σmin(k,\|correct_q\|)）に対して
  文書規模とほぼ独立な比較ができなくなるため、規模に応じた語彙数（小規模: 60 語・
  大規模: 800 語）で正解集合の絞り込み度を揃えている
- `sparse.rs` の各上限（`MAX_CORPUS_DOCS`・`MAX_DOC_BYTES`・`MAX_CORPUS_BYTES`）に
  十分収まる規模で設計した（コーパス規模は環境変数から受け取らず、テスト内定数のみで
  決める。無制限アロケーション防止）

### 密ベクトルの合成（one-hot AND 信号）

疎検索（BM25）は「クエリ語を両方含む文書」を自然に上位へ置くが、密ベクトルを
トピック方向のランダムベクトルの平均として素朴に合成すると、無関係なトピック間の
交差項ノイズが AND 一致の信号を上回り得ることを実測で確認した（`RrfConfig::default()`
は密・疎を等重みで融合するため、密チャネルが弱く無相関だと疎チャネルの正しい順位が
押し流される）。

そのため本ハーネスは、語彙数と同じ次元数の one-hot 基底ベクトルを使い、文書の密
ベクトルを「その文書が含むキーワード集合の one-hot 和」として合成する
（`crates/engine/tests/hybrid_recall.rs::one_hot_sum`）。クエリの密ベクトルも同じ
2 キーワードの one-hot 和とすることで、内積が「クエリ語とのキーワード一致数」を
そのまま表す決定的な AND 信号になり、疎チャネルと同型の信号を密チャネルにも持たせ
られる。これは実務上の密ベクトル（埋め込みモデル出力）の忠実な模倣ではなく、
RRF 融合の回帰検出という目的に絞った簡略化である（「既知の制約」参照）。

### 検索経路

production API（[`SparseIndex::build`]・[`ParallelSearchProvider`]・
`hybrid::hybrid_search` ＋ `RrfConfig::default()`）のみを使用し、BM25/RRF の再実装は
行わない。`RrfConfig::default()` は spec 採用構成（等重み・pool_depth 200・
k_const 60）に一致する。

### 指標

Recall@20（小規模段）・Recall@20/Recall@100（大規模段）を、正解文書の総数
（`total_correct`）に対する hit 数として測定し、理論上限（`ceil` =
Σmin(k,\|correct_q\|)）に対する到達率（`hits == ceil` か）も併せて回帰トラッキング
する。

## 実測結果

（`crates/engine/tests/hybrid_recall.rs`、層 A 2/2 pass。決定的コーパスのため
再現可能。hit 数は同テストのアサーションに固定済み）

| 段 | 文書数 | クエリ数 | total_correct | ceil20 | hits20 | ceil100 | hits100 |
| -- | ------ | -------- | -------------- | ------ | ------ | ------- | ------- |
| 小規模 | 400 | 60 | 182 | 178 | 178 | - | - |
| 大規模 | 20,000 | 100 | 1,217 | 333 | 333 | 736 | 736 |

いずれの段も `hits == ceil`（理論上限に対して 100%）を達成している。`total_correct`
が `ceil` より大きいのは、一部クエリの正解集合が k（20/100）を超えるため
（TASK-106 と同じ天井効果。「Recall@k」を `hits/total_correct` として素朴に表示すると
1.0 未満になるが、達成可能な上限に対しては 100% である）。

大規模段のデバッグビルド実行時間は約 4.5 秒であり、PR CI（`cargo test`）に含めても
許容範囲と判断し、層 A の両テストとも `#[ignore]` にしていない。

## 既知の制約・スコープ外

- **合成コーパスによる暫定測定**: 実コーパスでの評価は未了（TASK-106 と同種の制約）
- **密ベクトルの簡略化**: 「密ベクトルの合成」で述べた通り、one-hot AND 信号は
  埋め込みモデルの類似度分布を模倣しないため、実際の埋め込み品質の回帰検出には
  使えない。あくまで RRF 融合パイプライン自体（密・疎の統合・pool_depth・k_const の
  挙動）の回帰検出が目的
- **クエリ展開との統合測定**: SEARCH-2 の前提にはクエリ展開（PLAN-5 系、TASK-109
  以降）が含まれるが未実装のため、本ハーネスはハイブリッド検索単体（クエリ展開なし）
  の測定に留める
- Actions variables（`HYBRID_RECALL_MIN_*`）の実値設定はマージ後のリポジトリ管理者
  作業（README「Recall 回帰ハーネスの repo variables」参照）

[`SparseIndex::build`]: ../../crates/engine/src/sparse.rs
[`ParallelSearchProvider`]: ../../crates/engine/src/parallel_search.rs
