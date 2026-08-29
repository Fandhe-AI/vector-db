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

- **層 A**（`#[test]`。常時 `cargo test` 対象）: baseline/after の hits20・改善量を
  数値リテラルを含まない関係アサーション（会計整合・上限以下・単調性・非空）で
  回帰トラッキングする。あわせて「after が baseline を厳密に上回る」ことを独立に
  アサートする（`>=`（同値許容）ではリランカーが完全な no-op でも通過してしまうため、
  リランキング層が実際に Recall へ寄与していることを最小限保証する形へ強化した。
  改善幅そのものの下限判定は層 B が担う。Issue #312・PR #319 codex-review P1
  対応）。spec の数値基準は使わないため public 資産に閾値を持ち込まない
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

### 出力方針

`crates/engine/tests/rerank_recall.rs` の標準出力方針（既定は対象名と pass/fail
のみ・`RECALL_VERBOSE=1` opt-in・`GITHUB_ACTIONS` 下は fail-closed 拒否・
`recall.yml` へは注入しない）は `hybrid_recall.rs` と同一実装を複製している
（`docs/design/hybrid-recall-regression.md`「出力方針（実測値の既定非出力・
Issue #303）」参照）。

### コーパス・QA セット・測定経路

`crates/engine/tests/rerank_recall.rs` は `hybrid_recall.rs` の決定的合成コーパス
生成（xorshift64*・Zipf 近似分布・疎密チャネルの lossy view）・QA セット生成
（`direct` カテゴリ相当）をそのまま複製する（`cjk_tokenizer_impact.rs` →
`hybrid_recall.rs` と同じ「複製・踏襲」方式。既存の `hybrid_recall.rs` の関係
アサーション方式へは手を入れない）。シードは本ファイル専用の値を使う（`hybrid_recall.rs`
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

## 測定方法と層 A が検証する関係（Issue #312）

（`crates/engine/tests/rerank_recall.rs`、層 A 1/1 pass。決定的コーパス・QA
セットのため実測値は再現可能。Issue #312 以前は本節に hit 数・理論上限・改善量の
実測表を記録していたが、層 B の非公開閾値ゲートの pass/fail と組み合わせた閾値の
逆算材料になりうるため削除した。以下は測定方法と層 A が検証する関係のみを記す）

文書数・QA 件数の規模（`LARGE_NUM_DOCS`/`LARGE_NUM_QUERIES`）は
`hybrid_recall.rs` の大規模段と同一オーダを使う。

層 A が数値リテラルを含まずに検証する関係（`qa_accounting` ヘルパ・
`RerankRecallResult`）:

- **会計整合**: `total_correct`/`ceil20`/`ceil100`/`ceil200` が QA セット
  （`QaCase::correct`）からの独立な再計算と一致する
- **QA 件数**: 重複除外後の QA 件数がフィクスチャ定数（`LARGE_NUM_QUERIES`）と
  一致する（フィクスチャ縮退の回帰検知を兼ねる）
- **上限以下**: `baseline_hits20 <= ceil20`・`after_hits20 <= ceil20`・
  `pool_hits100 <= ceil100`・`pool_hits200 <= ceil200`
- **プール構造の単調性**: 候補プールの先頭 20 件 ⊆ 先頭 100 件 ⊆ プール全体
  （`pool_depth` 200）であり、`baseline_hits20 <= pool_hits100 <= pool_hits200`
- **リランキングの範囲制約**: リランキングはプール内の並べ替えのみでありプール外
  から正解を持ち込めないため `after_hits20 <= pool_hits200`
- **改善（no-op を許さない）**: `after_hits20 > baseline_hits20`（「after が
  baseline を厳密に上回る」独立アサーション。`>=` では完全な no-op でも通過して
  しまうため厳密な不等号を使う。定性的な効果があること自体は本節で確認するが、
  改善幅そのものの下限判定は層 B `RERANK_RECALL_MIN_R20_IMPROVEMENT` が担う）
- **非空**（vacuous pass 防止）: `baseline_hits20 > 0`
- **ミスマッチ制御（chance level）比較**: `baseline_hits20 > control_baseline_hits20 * CONTROL_FACTOR`
  かつ `after_hits20 > control_after_hits20 * CONTROL_FACTOR`。
  `control_baseline_hits20`/`control_after_hits20` は各クエリの baseline/after
  出力を「1 つずらした別クエリの正解集合」に対しても採点した hit 数の合計で、
  `measure_rerank_recall` が同一ランで実測する対照値（`hybrid_recall.rs` の同型対照値と設計を揃える）。
  層 B（`RERANK_RECALL_MIN_R20_IMPROVEMENT` 等）は
  `workflow_dispatch`/`schedule` のみで PR の通常 CI では評価されないため、
  「非空」だけでは Recall が chance level 近くまで崩壊しても 1 hit で通過して
  しまう懸念があった（codex-review P1 指摘・Issue #312 フォローアップ）。この
  比較は非公開の絶対値を使わずに PR の通常 CI（層 A）自体で検知するために追加
  した

暫定リランカー（[`LexicalOverlapReranker`]。字句一致順位と融合スコア順位を RRF 型で
再結合する参照実装）は本合成コーパス・QA セット上で最終 Recall@20 を改善している
ことを、上記「改善（劣化しない）」関係で確認している。プール自体の Recall@200 と
最終 Recall@20（after）の差が小さいこと（pool_depth 200 の候補プールがすでに正解の
大半を含んでおり、リランキングはその中の順位付けを改善する形で寄与している）は
定性的な考察であり、数値は記録しない（プールに入っていない正解＝Recall@200 の
未達分は、リランキング以前の候補生成段（`hybrid.rs`・`sparse.rs`）の課題であり
本タスクのスコープ外）。

なお本ハーネスは `LexicalOverlapReranker` の効果測定であり、方式確定前の暫定構成
（下記「既知の制約」参照）における測定であることに注意する。

## Issue #312: 層 A 固定値の除去（public 資産からの実測値排除）

`docs/design/hybrid-recall-regression.md`「Issue #312」節と同一方針・同一
理由で、本ファイルの「実測結果」節（hit 数・理論上限・改善量の実測表）を
削除し、`crates/engine/tests/rerank_recall.rs` の層 A 固定値アサーションを
関係アサーションへ置換した。

## PR #319: 層 A 検知力のミューテーション証明

`docs/design/hybrid-recall-regression.md`「PR #319」節と同一方針で、層 A
の chance level 比較（`baseline_hits20 > control_baseline_hits20 *
CONTROL_FACTOR` 等）が実際に劣化を検知できることを示すミューテーション
テスト（`rerank_recall_regression_detects_query_answer_mismatch`）を追加した。
クエリ・正解の対応を崩した状態で本体テストと同じ関係アサーションが実際に
失敗することを確認する。詳細・限界（緩やかな劣化までは保証しない点）は
リンク先の節を参照。

### PR #319 継続指摘: クエリ単位カバレッジの追加・ミューテーションテストの対照値衝突修正

`docs/design/hybrid-recall-regression.md`「PR #319 継続指摘」節と同一方針で
2 点を修正した。

1. **クエリ単位カバレッジ検査の追加**（codex-review P1 指摘）: 従来は合計
   hit 数の chance level 比較のみで、「一部クエリだけが多数 hit を稼ぎ残り
   大半が 0 hit」という部分的な劣化を素通りさせ得た。`RerankRecallResult` へ
   `baseline_queries_hit20`/`after_queries_hit20`（上位 20 件から正解を 1 件も
   拾えなかったクエリを除いた件数）を追加し、`qa.len()` に対する最低割合
   （`MIN_QUERY_COVERAGE_PERCENT`＝70%。`hybrid_recall.rs::
   LARGE_SCALE_MIN_QUERY_COVERAGE_PERCENT` と同一の設計値・同一の理由）を
   baseline・after それぞれで下回らないことをアサートする。
2. **ミューテーションテストの対照値がシフト衝突で真の Recall を測ってしまう**
   （Cursor Bugbot Medium 指摘）: `hybrid_recall.rs`「PR #319 継続指摘」節と
   同一の原因（ミューテーション側のクエリ差し替えシフトと `measure_rerank_recall`
   内部の対照値計算シフトがどちらも `+1` で衝突していた）。ミューテーション側の
   シフト量を `+2` へ変更し、対照値が実際に chance level を測るようにした。

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
- Actions secrets（`RERANK_RECALL_MIN_*`）の実値設定はマージ後のリポジトリ管理者
  作業（README「Recall 回帰ハーネスの repo secrets」参照。secret ↔ spec
  ポインタの対応表・設定手順は `docs/design/ci-gate-variables.md` に集約した。
  当初は Actions variables を使っていたが、`env:` ブロックのログ印字による
  漏えいを防ぐため secrets へ移行した（Issue #286）

[`SparseIndex::build`]: ../../crates/engine/src/sparse.rs
[`ParallelSearchProvider`]: ../../crates/engine/src/parallel_search.rs
[`hybrid_search`]: ../../crates/engine/src/hybrid.rs
[`rerank_candidates`]: ../../crates/engine/src/rerank.rs
[`LexicalOverlapReranker`]: ../../crates/engine/src/rerank.rs
