# ADR: クエリ展開の受け入れ基準回帰ハーネス（TASK-112）

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-112（`docs/spec/05-tasks.md`・`docs/spec/06-roadmap.md` 参照）
- 対象ビヘイビア: PLAN-1, PLAN-2（`docs/spec/04-behavior/query-planning.md`）
- 前提: TASK-110（クエリ展開クライアント `crates/engine/src/query_planner.rs`。
  常駐 LLM プロセス〔Ollama〕への注入型 `LlmClient`、`render_full_prompt` →
  `LlmClient::complete` → `parse_expansion` の一連）・TASK-111（ソフトブースト
  `crates/engine/src/hybrid.rs::hybrid_search_boosted`）はいずれもマージ済み
- 関連: TASK-104（`docs/design/hybrid-recall-regression.md`）・TASK-108
  （`docs/design/rerank-recall-regression.md`）の決定的合成コーパス生成・2 層構成・
  strict モードによる誤 green 防止を複製・踏襲する

## 背景

クエリ展開クライアント（TASK-110）・ソフトブースト機構（TASK-111）はいずれも
実装・マージ済みだが、クエリ展開が実際に検索 Recall@20 を改善するかどうかを自動
チェックする回帰テストが存在しなかった。本 ADR は TASK-112 に対応し、展開なし
（baseline）・展開あり（after）の Recall@20 を「言い換え語彙のみのクエリ（intent
カテゴリ）」「コーパス語彙と一致するクエリ（direct カテゴリ）」の 2 カテゴリで
比較する回帰テスト＋CI workflow 追加を記録する。

**位置づけ**: TASK-104/TASK-108 の先例に倣い、本タスクは production コード
（`crates/engine/src/`）を変更せず、既存 API（`hybrid::hybrid_search`・
`query_planner::render_full_prompt`／`parse_expansion`）に対する実測ハーネス
（回帰テスト）・本レポート・CI workflow の追加に限定した。

## 検証設計

### 2 カテゴリの QA

同一の潜在キーワードペア `(a, b)`（コーパス生成時に決定的に選出。正解集合
`correct` を共有）から、難易度以外の条件を揃えた 2 種類のクエリを構成する:

- **direct**: クエリ語がコーパスの内容語トークンと一致する（`hybrid_recall.rs`/
  `rerank_recall.rs` の QA と同型）
- **intent**: クエリ語がコーパス語彙と重ならない「言い換え語彙」のみで構成される。
  疎チャネル（トークン不一致）・密チャネル（対応するベクトル信号を持たない = ゼロ
  ベクトル）のいずれからも baseline は手がかりを得られない構成にし、「クエリ展開
  なしでは初見の言い換えに対応できない」状況を最小限にモデル化する

### 比較対象

- **baseline（展開なし）**: 各カテゴリのクエリ語をそのまま `hybrid::hybrid_search`
  （`RrfConfig::default()`）へ渡した Recall@20
- **after（展開あり）**: 決定的スタブ `LlmClient`（プロンプト中の質問語を、決定的な
  同義語対応表で言い換え語彙 → コーパス内容語へ写像し、他の語はそのまま通す。実
  Ollama へは接続しない）を `query_planner::render_full_prompt`（固定接頭辞は空
  文字列）→ `LlmClient::complete` → `query_planner::parse_expansion` の一連
  （production API・LLM 出力の fail-closed 検証経路を含む）に通し、得られた
  `QueryExpansion::search_terms` から再構成したクエリの Recall@20

`path_hint`/`kind_hint` を用いたソフトブースト経路（TASK-111）は
`crates/engine/tests/soft_boost.rs` が別途 end-to-end で検証済みのため、本ハーネス
の対象外とした（`search_terms` の展開効果測定に対象を絞ることで、比較対象の変数を
1 つに限定している）。

### 2 層構成（PR CI と閾値ゲートの分離。TASK-104/TASK-108 と同方式）

- **層 A**（`#[test]`。常時 `cargo test` 対象）: 両カテゴリの baseline/after の
  hits20・`ceil20`・`total_correct` を、カテゴリ間・before/after 間の相対関係
  （不等号・等号）のみで回帰トラッキングする（絶対数値の固定値アサーションは
  行わない。検索カーネル・クエリ展開パーサ・フィクスチャの変更でこれらの関係が
  崩れた場合にこのテストが失敗する）。あわせて「intent は after が baseline を
  上回る」（PLAN-1）「direct は after が baseline を下回らない」（PLAN-2。展開が
  既存の強みを破壊しないことの最小保証）ことを独立にアサートする。spec の数値
  基準は使わないため public 資産に閾値を持ち込まない
- **層 B**（`#[ignore]`。`make query-planning-regression` 経由）: spec 由来の下限
  （`QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT`＝intent カテゴリの改善幅下限・
  `QUERY_PLANNING_RECALL_MIN_R20_DIRECT`＝direct カテゴリの after Recall@20 絶対
  下限。`.github/workflows/recall.yml` の同一 job・同一 environment `recall-gate`
  から注入）と実測値を比較する閾値ゲート。TASK-104/TASK-108 と同じ opt-in
  （未設定＝対象外）・strict モード（`QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS=1`
  で未設定を fail-closed 化）を持つ。ゲート本体（`run_threshold_gate`）は
  `MockLlmClient`（完全 oracle 写像）と `NoisyLlmClient`（非 oracle・劣化展開品質）
  の両方に対して独立に実行し、同じ spec 閾値をどちらの応答品質でも満たすことを
  要求する（下記「展開品質の劣化検出」参照）

`.github/workflows/recall.yml` の TASK-104 由来の設計判断（`pull_request` トリガを
持たない・`workflow_dispatch` のみ・`if: github.ref == 'refs/heads/main'`・
`checkout ref: main`・environment `recall-gate` の deployment branch policy による
実行境界）はそのまま踏襲する（`docs/design/hybrid-recall-regression.md`「2 層構成」
参照）。

## 既知の制約・スコープ外

- 実 Ollama 接続での実測は対象外（TASK-110 時点からの継続制約）。本ハーネスの
  スタブ `LlmClient` は「LLM 出力の受理契約（`parse_expansion` の fail-closed
  検証）を通した上でクエリ展開の効果を測定する」という設計目的のみを満たす
- 数万チャンク規模ケース（`docs/spec/04-behavior/search.md` のスケール条件付き
  基準）の追加は TASK-113 が本ファイルへ後続で追加する
- `search_query:` プレフィックス再埋め込み（TASK-114）は未実装のため、本テストの
  再構成クエリは埋め込みの使い回しではなく合成 one-hot ベクトルの再合成で代替する
- 合成コーパスによる暫定測定であり、実コーパスでの評価は未了（TASK-104/TASK-108
  と同種の制約）
- **層 A の独立アサーションが検出できる範囲は限定的**: `MockLlmClient` は
  `direct` カテゴリの語（すでに内容語トークン形式）を無変換で通すため、`direct`
  の after クエリは baseline と構成上バイト同一になり、`direct.after_hits20 >=
  direct.baseline_hits20` は構造的に常に真になる（展開によるスコア劣化を検出する
  アサーションではない。PLAN-2 の実質的な検証は層 B の絶対下限ゲートが担う）。
  同様に `intent` の baseline はゼロベクトル（対応する密信号を持たない）という
  最も不利な構成であるため、改善量のアサーションは「構成上ほぼ自明に成立する」
  比較である。層 A は「回帰（数値の意図しない変化）の検出」を目的とし、
  「絶対水準としての受け入れ基準の判定」は層 B（spec 閾値ゲート）が担うという
  役割分担は TASK-104/TASK-108 と同一
- **展開品質の劣化検出**（codex-review・PR #265・P2 指摘への追補）: 上記の制約は
  「層 A の相対関係アサーションが `MockLlmClient` の完全 oracle 写像を前提に構造上
  自明に成立する」ことを述べたものであり、「本ハーネス全体が完全 oracle 写像でしか
  評価できない」ことを意味しない。`crates/engine/tests/query_planning_recall.rs::
  query_planning_recall_detects_degraded_expansion_quality`（層 A・PR CI 常時実行）
  が、言い換え語彙の半数のみを正しく写像し残り半数を未写像のまま通す
  `NoisyLlmClient`（劣化した production LLM 応答を模する決定的スタブ）を追加し、
  完全 oracle 写像（`MockLlmClient`）との Recall@20 差を独立にアサートすることで、
  展開戦略の劣化そのものを検出できることを回帰保証する
- **層 B ゲート自体の oracle 依存**（codex-review・PR #265・P2 再指摘への追補）:
  上記の `NoisyLlmClient` 追加は層 A（相対比較によるハーネスの検出感度の回帰保証）
  にとどまり、実際に spec 閾値と比較する層 B の受け入れゲート
  （`query_planning_recall_threshold_gate`）自体は `MockLlmClient` 固定のままだった
  ため、production の展開品質が劣化してもゲートの pass/fail は変化しないという
  指摘を受けた。これに対応し、層 B のゲート本体を `run_threshold_gate(client,
  gate_name)` として `LlmClient` 差し替え可能に切り出し、`MockLlmClient` 版
  （`query_planning_recall_threshold_gate`）に加えて `NoisyLlmClient` 版
  （`query_planning_recall_threshold_gate_degraded_expansion`）を追加した。「固定した
  非 oracle 応答コーパスを production と同じ展開処理（`render_full_prompt` →
  `LlmClient::complete` → `parse_expansion` → `hybrid_search`）へ入力し、spec 閾値を
  適用する評価」という位置づけであり、実 Ollama への疎通確認（引き続き対象外）を
  代替するものではない。**リスク**: `query_planning_recall_threshold_gate_degraded_expansion`
  は既存の `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT`／
  `QUERY_PLANNING_RECALL_MIN_R20_DIRECT`（`recall-gate` environment の同一 Actions
  variables）をそのまま共用する。`NoisyLlmClient` は `MockLlmClient` より改善幅が
  小さいため、オーナーが完全 oracle 写像の実測を基準に閾値を設定していた場合、次回
  `recall.yml` 実行で本ゲートが新規に fail する可能性がある（実測により確認済み:
  両ゲートの pass/fail が分かれる閾値域が存在する）。閾値の再調整はオーナー・spec
  リポ側の判断事項であり、本 ADR の範囲外
