# `precision` モード評価基準の実測: 設計判断記録

- ステータス: Proposed（目標値の確定はユーザー判断待ち。確定後に Accepted へ更新する）
- 対応: TASK-163（Issue #112。ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-104（Recall 回帰基盤。Issue #37 / PR #147）・TASK-158（性能計測プロトコル
  基盤。Issue #106 / PR #136）・TASK-162（`precision` モードの実行契約。Issue #111 /
  PR #211。`docs/design/precision-confidence-gate.md`）
- 関連ビヘイビア: SEARCH-10（ポインタ: `docs/spec/04-behavior/search.md`）
- 実装: `crates/engine/tests/precision_eval.rs`（評価ハーネス。production コード
  `crates/engine/src/` は変更しない）

## 目的

TASK-162 で `precision` モードの実行契約（確信度ゲート・空集合 fail-closed）が
実装されたが、その既定閾値（`precision.rs` の `DEFAULT_*` 定数）と SEARCH-10 の
評価基準はいずれも実測未了だった。本タスクは `tests/hybrid_recall.rs`
（TASK-104）・`tests/rerank_recall.rs`（TASK-108）と同じ決定的合成コーパス方式で
`precision` 専用の評価ハーネスを追加し、目標値確定の判断材料を提示することが目的。
2 層構成は「層 A 構造不変条件・決定性／層 B 環境変数閾値ゲート」とする。

**目標値の確定そのもの・SEARCH-10 期待欄の更新は本タスクのスコープ外**
（下記「申し送り」参照）。指標の正式な定義・実測値・パラメータ感度は spec 側
（上記ポインタ）に記録する（`.claude/rules/spec-confidentiality.md`）。

## 測定経路

`precision::apply_gate` を直接呼ばず、production の SQL 経路
（`EngineCore::execute_sql`。実 `Storage`＋`CpuScalarProvider`。`tests/
sql_precision_mode.rs` と同じ流儀）のみで測定する。直接呼ぶと `sql/exec.rs` の配線
（`k_eff` の拡張・正規化 RRF の適用等）をテスト側で再実装することになり production
と乖離しうるため。

3 指標（Top-1 Accuracy・MRR@10・誤返却率）はいずれも `USING MODE 'precision'` の
クエリが返した行から算出する。MRR@10 を `recall` モードの結果で測ると確信度ゲート・
`PrecisionPolicy` が指標に反映されず `PRECISION_EVAL_MIN_MRR10` が別モードの品質を
判定してしまうため（PR #212 codex-review P1）。ゲートが空集合へ倒したクエリは
寄与 0 として Q+ 件数の分母に算入する。返却行数は `PrecisionPolicy::max_results`
で上限が決まるため、順位の広がりは感度スイープで `max_results` を広げたときにのみ
現れ、既定値（1 件）では MRR@10 は Top-1 Accuracy と構造的に一致する。

ランキングは **hybrid**（`hybrid_rrf(embedding, ..., body, ...)`）を主、**dense**
（`embedding <=> ...`）を副として同一コーパスで併測する（`PrecisionPolicy` が
dense/hybrid で別々の閾値を持つため、両方の既定値の妥当性判断材料になる）。

## 評価セット拡張（正解不在クエリ）

`hybrid_recall.rs` の決定的合成コーパス生成（潜在トピック集合 `keywords` → 疎/密
lossy view・Zipf 語彙・低頻度 2 語 AND クエリ）を複製・踏襲し、以下を追加した:

- **正解不在クエリ（hard negative。本命ケース）**: 語彙中に単独では出現する 2 語
  `(a, b)` で、inverted index の交差が空（同一文書で共起しない）ペア。各語は単独で
  部分一致文書を持つため BM25／密チャネルとも「もっともらしい候補」を返しうる。
  ゲートが空集合へ倒せるかを問う。正規化ペアの重複除外・生成試行回数の上限
  （fail-closed の `assert!`）を設けた。
- **語彙外クエリ（自明ケース。少数）**: 未知トークン＋零ベクトル。密チャネルは
  cosine が定義できず常に空集合。比率は小さく保つ。

コーパス・QA セットの規模はテスト内固定・環境変数から受け取らない
（`crates/engine/tests/precision_eval.rs` 参照）。可視集合は `MAX_SEARCH_K`
（10,000）に対し十分小さく、precision の `k_eff` クランプに抵触しない。

## 2 層構成と spec 閾値の扱い

- **層 A**（`#[test]`。`make ci` 対象）: production の SQL 経路で評価を通しで実行し、
  構造不変条件（カウンタの上下関係・`PrecisionPolicy::max_results` の遵守・指標が
  `[0.0, 1.0]` に収まること）と測定の決定性のみを検査する。指標の実測値はアサート
  も出力もしない——public リポジトリ・public な Actions ログに実測値を残さないため
  （`.claude/rules/spec-confidentiality.md`）。品質の回帰判定は層 B だけが行う。
- **層 B**（`#[ignore]`。`make precision-regression`）: `PRECISION_EVAL_MIN_TOP1_ACC`・
  `PRECISION_EVAL_MIN_MRR10`（`(0.0, 1.0]`）・`PRECISION_EVAL_MAX_FALSE_RETURN`
  （`[0.0, 1.0)`。`1.0` は常時 pass＝fail-open のため拒否）を環境変数から解決する。
  未設定は非 strict では「評価は実行するが判定はスキップ（対象外）」として成功終了、
  `PRECISION_EVAL_REQUIRE_THRESHOLDS=1` で fail-closed。非数値・範囲外は常に fail-closed。ログには実測値と pass/fail の
  みを出力し、閾値の数値は出力しない（`hybrid_recall.rs::resolve_gate_threshold`
  と同型）。
- **`.github/workflows/recall.yml` への接続は本 PR では行わない**: TASK-163 の
  スコープは実測・判断材料の提示までであり目標値の確定は含まない。未確定の閾値変数を
  strict モードの週次 job へ追加すると、管理者に未確定値の設定を強いるか schedule
  を恒常的に red にする。層 B ＋ Makefile ターゲットは用意済みで、`recall.yml` への
  step 追加は**ユーザーの目標値確定後のフォローアップ**とする。
- **パラメータ感度スイープ**（`#[ignore]`。アサートなし）: `with_precision_policy`
  で hybrid の閾値パラメータを差し替え、指標の変化を表形式で出力する（判断材料の
  提示専用。production の既定値は変更しない）。

## 実測結果・パラメータ感度・目標値確定の判断材料

指標の実測値・感度スイープの結果・目標値確定のためのユーザー確認事項は
public リポジトリ（本ドキュメント・テストコード・PR 本文）へは一切転記しない
（`.claude/rules/spec-confidentiality.md`）。
実測は `cargo test -p engine --test precision_eval -- --nocapture`、感度スイープ
は `make precision-regression` の `precision_eval_policy_sweep`（`--ignored`）
でローカル再現できる。結果の記録・判断は spec 側（上記ポインタ）で管理する。

## 既知の制約

- 合成コーパスによる暫定測定であり、実コーパスでの評価は未了（TASK-104/108 と同種の
  制約）
- クエリ展開（PLAN-5 系）は未接続のため、本ハーネスは precision 単体（クエリ展開
  なし）の測定に留める
- wire 経由の検証（TASK-165）・`USING PLAN` モード推定（TASK-164）は本タスクの管轄
  外

## 申し送り

- **目標値の確定判断**（ユーザー）: 実測・感度スイープをもとに仮置き目標値の妥当性
  を確認する（判断材料は spec 側に記録する）
- **spec 側**: SEARCH-10 期待欄（目標値）の更新は spec リポ（`docs/spec`）の作業
- **`recall.yml` への層 B 接続**: 目標値確定後に `PRECISION_EVAL_*` を environment
  `recall-gate` へ設定し strict 注入付きの step を追加する（README 参照）
- **既定閾値（`precision.rs` の `DEFAULT_*`）の見直し**: 実測結果次第でユーザー判断
  のうえ別 PR
