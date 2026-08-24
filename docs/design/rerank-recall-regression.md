# ADR: リランキング効果測定回帰ハーネス（TASK-108）

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-108（`docs/spec/05-tasks.md`・`docs/spec/06-roadmap.md` 参照）
- 対象ビヘイビア: SEARCH-7（`docs/spec/04-behavior/search.md`）
- 前提: TASK-107（リランキング層本体・`crates/engine/src/rerank.rs`。`Reranker`
  trait と暫定リランカー 2 種＝`IdentityReranker`・`LexicalOverlapReranker`、
  出力検証 `rerank_candidates`）は PR #146 でマージ済み
- 関連: TASK-104（`docs/design/hybrid-recall-regression.md`。決定的合成コーパス
  生成・2 層構成・strict モードによる誤 green 防止の先行実装。本タスクは同方式を
  複製・踏襲する）

## 背景

リランキング層（TASK-107）は実装・マージ済みだが、リランキングが実際に最終
Recall@20 を改善するかどうかを自動チェックする回帰テストが存在しなかった。本 ADR
は TASK-108 に対応し、リランキング適用前後（baseline / after）の最終 Recall@20 を
比較する回帰テスト＋CI workflow 追加を記録する。

**位置づけ**: TASK-104 の先例に倣い、本タスクは production コード
（`crates/engine/src/`）を変更せず、既存 API（`hybrid::hybrid_search`・
`rerank::rerank_candidates`）に対する実測ハーネス（回帰テスト）・本レポート・CI
workflow の追加に限定した。

## 検証設計

### 比較対象

同一クエリ・同一候補プール（`hybrid_search` を 1 回だけ呼び出す）から baseline・
after を測定することで、コーパス・プール生成のばらつきが比較結果に混入しないように
している。

- **baseline（リランキングなし）**: `hybrid::hybrid_search`（`RrfConfig::default()`・
  pool_depth 200）で候補プールを 1 回取得し、その先頭 20 件をそのまま最終結果とした
  場合の Recall@20
- **after（リランキングあり）**: 同じ候補プール 200 件を `rerank::RerankCandidate`
  （id・fused_score・文書 text）へ変換し、`rerank_candidates(&LexicalOverlapReranker
  ::default(), …, RerankConfig::default())`（final_k 20）の出力の Recall@20
- **補助計測（原因分析用）**: 候補プール自体の Recall@100・Recall@200。プールの中に
  正解があるのにリランキングが上位 20 件へ引き上げられていないのか、そもそも
  プール（pool_depth 200）に入っていないのかを切り分けるための指標

### 2 層構成（PR CI と閾値ゲートの分離。TASK-104 と同方式）

- **層 A**（`#[test]`。常時 `cargo test` 対象）: baseline/after の hits20 と改善量を
  固定値アサーションで回帰トラッキングする。あわせて「after が baseline を下回らない」
  ことを独立にアサートする（リランキング層が Recall を悪化させていないことの最小
  保証）。spec の数値基準は使わないため public 資産に閾値を持ち込まない
- **層 B**（`#[ignore]`。`make rerank-regression` 経由）: spec 由来の Recall 下限
  （`RERANK_RECALL_MIN_R20_LARGE`＝リランキング後の最終 Recall@20 の絶対下限・
  `RERANK_RECALL_MIN_R20_IMPROVEMENT`＝baseline からの改善幅の下限。
  `.github/workflows/recall.yml` の同一 job・同一 environment `recall-gate` から
  注入）と実測値を比較する閾値ゲート。TASK-104 と同じ opt-in（未設定＝対象外）・
  strict モード（`RERANK_RECALL_REQUIRE_THRESHOLDS=1` で未設定を fail-closed 化）を
  持つ

`.github/workflows/recall.yml` の TASK-104 由来の設計判断（`pull_request` トリガを
持たない・`workflow_dispatch` のみ・`if: github.ref == 'refs/heads/main'`・
`checkout ref: main`・environment `recall-gate` の deployment branch policy による
実行境界）はそのまま踏襲する（`docs/design/hybrid-recall-regression.md`「2 層構成」
参照。spec 機密保持の理由も同一）。

### コーパス・QA セット・測定経路

`crates/engine/tests/rerank_recall.rs` は `hybrid_recall.rs` の決定的合成コーパス
生成（xorshift64*・Zipf 近似分布・疎密チャネルの lossy view）・QA セット生成
（`direct` カテゴリ相当）をそのまま複製する（`cjk_tokenizer_impact.rs` →
`hybrid_recall.rs` と同じ「複製・踏襲」方式。既存の `hybrid_recall.rs` の固定値
アサーションへは手を入れない）。シードは本ファイル専用の値を使う（`hybrid_recall.rs`
と依存関係を持たない自己完結を保つため）。

production API（[`SparseIndex::build`]・[`ParallelSearchProvider`]・
[`hybrid_search`]・[`rerank_candidates`]）のみを使用し、BM25/RRF/リランキングの
再実装はテスト内で行わない。`hybrid_search` が返す候補プールは融合スコア降順・
同点 id 昇順で整列済みであり、これは `rerank_candidates` が要求する入力順序契約と
同一であるため、変換以外の並べ替えは不要である。

### 指標

`hybrid_recall.rs::RecallResult` と同じ理由で、Recall@k は正解文書の総数
（`total_correct`）ではなく理論上限（`ceil` = Σmin(k,\|correct_q\|)）を分母とする
到達率（`hits / ceil`）として測定する。

## 実測結果

（`crates/engine/tests/rerank_recall.rs`、層 A 1/1 pass。決定的コーパスのため
再現可能。hit 数は同テストのアサーションに固定済み）

| 指標 | 値 |
| ---- | -- |
| 文書数 | 20,000 |
| QA 件数 | 100 |
| total_correct | 1,049 |
| ceil20 | 410 |
| baseline hits20（リランキングなし） | 343 |
| after hits20（リランキングあり） | 368 |
| baseline Recall@20 | 0.8366 |
| after Recall@20 | 0.8976 |
| 改善量（after − baseline） | +0.0610 |
| ceil100 / pool hits100 | 913 / 809（Recall@100 = 0.8861） |
| ceil200 / pool hits200 | 1,049 / 948（Recall@200 = 0.9037） |

暫定リランカー（[`LexicalOverlapReranker`]。字句一致順位と融合スコア順位を RRF 型で
再結合する参照実装）は本合成コーパス・QA セット上で最終 Recall@20 を有意に改善して
いる（343→368、+7.3pt）。プール自体の Recall@200（0.9037）と最終 Recall@20（after:
0.8976）の差は小さく、pool_depth 200 の候補プールがすでに正解の大半を含んでおり、
リランキングはその中の順位付けを改善する形で寄与していることを示唆する（プールに
入っていない正解＝Recall@200 の未達分は、リランキング以前の候補生成段
（`hybrid.rs`・`sparse.rs`）の課題であり本タスクのスコープ外）。

なお本ハーネスは `LexicalOverlapReranker` の効果測定であり、方式確定前の暫定構成
（下記「既知の制約」参照）における測定値であることに注意する。

## 既知の制約・スコープ外

- **暫定リランカーの効果測定である**: 同梱リランカー（[`LexicalOverlapReranker`]）は
  方式確定までの暫定実装であり、クロスエンコーダ等の本命方式は TASK-107 の残
  オーナー判断・依存承認（`.claude/rules/dependency-policy.md`）待ちである。本測定は
  「暫定構成でも Recall が悪化しないこと・字句一致順位による再順位付けが一定の改善を
  もたらすこと」の確認にとどまり、本命方式導入後の効果を保証しない。暫定構成での
  測定結果が spec 基準（層 B ゲート）に届かない場合の方式見直し・差し戻し判断は、
  本レポートの原因分析（プール活用状況・Recall@200 実測）を材料にオーナーへ報告する
- **合成コーパスによる暫定測定**: `hybrid_recall.rs`（TASK-104）と同種の制約。実
  コーパスでの評価は未了
- **`VectorCore` trait への統合・SQL 表層統合は後続タスクの管轄**: 本ハーネスは
  `hybrid_search`・`rerank_candidates` という純粋関数層の測定であり、上位層（RLS
  可視性判定・SQL 表層）との結合測定は対象外
- spec の基準充足判定（Recall@20 絶対下限・改善幅の下限）は environment
  `recall-gate` の閾値ゲート（層 B）で行う。値そのもの（spec 由来の数値基準）は
  本レポートには記載しない
- Actions variables（`RERANK_RECALL_MIN_*`）の実値設定はマージ後のリポジトリ管理者
  作業（README「Recall 回帰ハーネスの repo variables」参照）

[`SparseIndex::build`]: ../../crates/engine/src/sparse.rs
[`ParallelSearchProvider`]: ../../crates/engine/src/parallel_search.rs
[`hybrid_search`]: ../../crates/engine/src/hybrid.rs
[`rerank_candidates`]: ../../crates/engine/src/rerank.rs
[`LexicalOverlapReranker`]: ../../crates/engine/src/rerank.rs
