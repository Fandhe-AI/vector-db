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

- **層 A**（`#[test]`。常時 `cargo test` 対象）: baseline/after の hits20・プールの
  hits100/hits200 を固定値アサーションで回帰トラッキングする。spec の数値基準は
  使わないため public 資産に閾値を持ち込まない。「after が baseline を下回らない」
  非劣化アサーション（`after_hits20 >= baseline_hits20`）も層 A に含む
  （`crates/engine/tests/rerank_recall.rs` の SEARCH-7 契約メモ参照。現在の
  実測状況は下記「実測結果」節）
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

（`crates/engine/tests/rerank_recall.rs`、層 A の固定値アサーションは実測値へ
更新済み。決定的コーパスのため再現可能。非劣化アサーション
`after_hits20 >= baseline_hits20` は Issue #310 対応の既定重み変更後、現在
green。詳細は下記）

| 指標 | 値 |
| ---- | -- |
| 文書数 | 20,000 |
| QA 件数 | 100 |
| total_correct | 1,049 |
| ceil20 | 410 |
| baseline hits20（リランキングなし） | 387 |
| after hits20（リランキングあり） | 389（Issue #330 対応後。対応前は 388） |
| baseline Recall@20 | 0.9439 |
| after Recall@20 | 0.9488（Issue #330 対応前は 0.9463） |
| 改善量（after − baseline） | +0.0049（Issue #330 対応前は +0.0024） |
| ceil100 / pool hits100 | 913 / 837（Recall@100 = 0.9168） |
| ceil200 / pool hits200 | 1,049 / 951（Recall@200 = 0.9066） |
| pool_ceiling_hits20（Issue #330 改訂で導入） | 396 |
| improvement_ratio（Issue #330 改訂。`(after − baseline) / (pool_ceiling_hits20 − baseline_hits20)`） | 2/9 ≈ 0.2222 |

上表は `hybrid.rs::complete_boundary_tie_group_by` の境界同点グループ完全化を
「終端未確定時は再取得ループ（`fetch_k` 倍増）で終端確定を試み、再取得の上限に
達してもなお未確定なら境界同点グループ全体を除外し厳密に上位の候補のみ
保持する」契約へ変更した後（Issue #320
codex-review P1 指摘対応・`docs/design/hybrid-recall-regression.md`「Issue #310:
engine 側改善」節参照）の、かつ `LexicalOverlapReranker` の既定重みを Issue #310
対応で変更した後の実測値である。

`rerank.rs::LexicalOverlapReranker` の `rank_fused` 算出は
`hybrid.rs::accumulate_ranked` の `TieRank::GroupEnd` 分岐と同じグループ末尾
順位へ揃えてある。

Issue #310 対応で `LexicalOverlapReranker` の既定重みを `fused_weight:lexical_weight
= 3.0:1.0`（fused 優位）へ変更した（`crates/engine/src/rerank.rs`
[`LexicalOverlapReranker::default`]）。変更後の大規模段実測は baseline 387 /
after 388 で、非劣化アサーション `after_hits20 >= baseline_hits20` を満たす。

Issue #320 の大規模段追加調査（非正スコア候補の順位付け除外。`hybrid.rs::
resolve_boundary_tie_group`・`trim_non_positive_score_tail`。`docs/design/
hybrid-recall-regression.md`「Issue #320 大規模段追加調査」節参照。exhaustive
かどうかで除外対象を分岐する契約への改訂を含む）の適用前後で、本ハーネスの
上表の数値（baseline hits20・pool hits100/hits200）はいずれも変化していない
（本フィクスチャでは非正スコア候補が測定対象クエリの結果へ影響しなかった）。

プール自体の Recall@200（0.9066）と最終 Recall@20（after: 0.9463）を比較すると、
pool_depth 200 の候補プールがすでに正解の大半を含んでおり、リランキングは
その中の順位付けを改善する形で寄与し続けている（プールに入っていない正解＝
Recall@200 の未達分は、リランキング以前の候補生成段（`hybrid.rs`・`sparse.rs`）
の課題であり本タスクのスコープ外）。

なお本ハーネスは `LexicalOverlapReranker` の効果測定であり、方式確定前の暫定構成
（下記「既知の制約」参照）における測定値であることに注意する。

## Issue #330: 改善幅未達の原因分析と engine 側改善

`recall.yml` の rerank 大規模段ゲートのうち、改善幅ゲート
（`RERANK_RECALL_MIN_R20_IMPROVEMENT`）が未達だった（Issue #310 対応後の
improvement@20 は +0.0024 相当）。絶対下限ゲート（after Recall@20 そのもの）は
達していた。本節はこの未達要因の分析と、その分析に基づく engine 側改善
（`crates/engine/src/rerank.rs::LexicalOverlapReranker`）を記録する。

### 未達要因の分析（診断内訳。上表と同一フィクスチャ）

理論上限 ceil20（410）と baseline hits20（387）の差 23 hit のうち、リランカーの
入力である候補プール自体に含まれない（プール外）ものが 14 hit、プール内に
含まれる（＝リランキングで到達しうる）ものが 9 hit だった。プール内 9 hit を
クエリ語と候補 text の字句重なり数（overlap）で内訳すると、overlap=2（クエリ 2
トークンとも一致）が 2 件、overlap=1 が 6 件、overlap=0 が 1 件だった。

一方、top20 中の不正解の overlap 分布は overlap=1 が大半（クエリ当たり ≒16 件）を
占め、overlap=2 は 0 件だった。本フィクスチャでは「text がクエリ 2 語とも含む
（overlap=2）⇒ ほぼ確実に正解」という構造がある一方、正解の大半は overlap=1 で
top20 中の不正解と字句・fused いずれの信号でも区別がつかない。したがって字句一致
信号で追加的に引き上げ可能なのは overlap=2 の 2 件のみであり、**字句信号での改善
到達上限は 389（+2 hit・改善幅 ≈ +0.0049）** と見積もられる（プール外 14 hit・
overlap=1/0 の 7 hit はリランカー単独では原理的に到達不能）。

### 現行実装が上限に届かなかった原因と対応

`LexicalOverlapReranker::rerank` の `rank_lexical`（字句一致順位）は、overlap
降順の安定ソート後の**位置順位**（`rank_idx + 1`）で算出していた。この方式では
overlap が同点の候補グループ内でも、ソートの安定性により元の並び順（＝融合スコア
降順）がそのまま順位差として残り、`contribution_fused`（融合順位の寄与）と
`contribution_lexical`（字句順位の寄与）の両方に融合スコア順位の情報が二重に
反映される非対称が生じていた。`rank_fused` 側は Issue #310 対応で
`hybrid.rs::TieRank::GroupEnd`（同点グループは末尾の共通順位を共有）へ統一済み
だったため、字句側だけがこの規約と異なるままだった。

対応として、`rank_lexical` の算出を `rank_fused_by_idx` と同じグループ末尾順位
（GroupEnd）へ変更した（overlap が同点のグループ内では全メンバーが同一の
`rank_lexical` を共有し、順位差は `rank_fused` 側のみに由来する）。フィクスチャ
パラメータ（`*_DROPOUT_PROB`・`VECTOR_DECOY_PROB`・規模・シード・`k`）は変更せず、
`crates/engine/src/rerank.rs::LexicalOverlapReranker`（既定重み 3.0:1.0 は不変）の
みを変更した結果、after hits20 は 388 → 389（改善幅 +0.0024 → +0.0049）となり、
字句信号の到達上限（389）に一致した。

### 重要な制約: 到達上限は字句信号の構造的上限

389（改善幅 +0.0049）は本フィクスチャにおける字句一致信号の構造的上限であり、
`recall.yml` の閾値（spec 由来・非公開）がこれを上回る場合、`LexicalOverlapReranker`
単独では改善幅ゲートに到達できない。その場合はプール外 14 hit・overlap=0/1 の
7 hit をカバーするための候補生成段（`hybrid.rs`・`sparse.rs`）の改善、フィクスチャ
自体の見直し、リランカー方式（クロスエンコーダ等）の確定のいずれかが必要であり、
オーナー判断事項となる。

### Issue #330 追記: 改善幅ゲートを候補プール上限に対する相対比率へ再定義（SEARCH-7 改訂）

上記の分析が示すとおり、baseline hits20（387）と ceil20（410）の差 23 hit のうち、
14 hit はプール外（候補生成段の課題・リランキング単独では原理的に到達不能）であり、
リランキングが到達しうる改善余地はプール内の 9 hit のみである。改善幅を
`ceil20` に対する絶対差（`after_recall20 − baseline_recall20`）で測る従来の
ゲート定義は、この「原理的に到達不能な範囲」を分母に含めてしまうため、暫定
リランカー（字句一致方式）の効果を過小評価する。

そこでオーナー承認済みの spec 改訂（vector-db-spec#7）に合わせ、`recall.yml` の
`RERANK_RECALL_MIN_R20_IMPROVEMENT` ゲートの判定基準を以下へ再定義した
（`crates/engine/tests/rerank_recall.rs::RerankRecallResult::improvement_ratio`）:

1. 非劣化: `after_hits20 >= baseline_hits20`（既存の層 A 固定値アサーションを維持）
2. 相対比率:
   `(after_hits20 − baseline_hits20) / (pool_ceiling_hits20 − baseline_hits20) >= RERANK_RECALL_MIN_R20_IMPROVEMENT`。
   ここで `pool_ceiling_hits20`
   （＝候補プール 200 件内に完璧な並び替えを施した場合に上位 20 件で回収しうる
   理論上限。クエリ単位の `min(20, プール内正解数)` の総和）は本フィクスチャで
   実測 396（プール内回収余地 9 hit のうち可能な最大＝ 387 + 9）
3. 改善余地（分母 `pool_ceiling_hits20 − baseline_hits20`）が `ceil20` の 1%
   未満（構造的にほぼ改善不可能）の場合は条件 2 を自動充足とし、条件 1 のみで
   判定する（分母 0 に近づく不安定さを避ける fail-closed 対策を兼ねる）

Issue #330 対応後の本フィクスチャでの実測比率は
`improvement_ratio = (389 − 387) / (396 − 387) = 2 / 9 ≈ 0.2222`
（字句信号の構造的上限に達している状態での実測値）。`RERANK_RECALL_MIN_R20_LARGE`
（絶対下限）の判定方式は変更していない。

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
