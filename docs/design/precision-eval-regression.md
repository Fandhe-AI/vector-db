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
評価基準（Top-1 Accuracy・MRR@10・正解不在クエリでの誤返却率）はいずれも実測未了
だった。本タスクは

- `tests/hybrid_recall.rs`（TASK-104）・`tests/rerank_recall.rs`（TASK-108）と同じ
  「決定的合成コーパス＋ 2 層構成（層 A 固定値回帰／層 B spec 閾値ゲート）」方式で
  `precision` 専用の評価ハーネスを追加し、
- 正解不在クエリを含む評価セットで 3 指標を実測してこの ADR に記録し、
- 目標値確定の判断材料（実測値・パラメータ感度）を提示する

ことが目的。**目標値の確定そのもの・SEARCH-10 期待欄の更新は本タスクのスコープ外**
（下記「申し送り」参照）。

## 測定経路

`precision::apply_gate` を直接呼ばず、production の SQL 経路
（`EngineCore::execute_sql`。実 `Storage`＋`CpuScalarProvider`。`tests/
sql_precision_mode.rs` と同じ流儀）のみで測定する。直接呼ぶと `sql/exec.rs` の配線
（`k_eff` の拡張・正規化 RRF の適用等）をテスト側で再実装することになり production
と乖離しうるため。

ランキングは **hybrid**（`hybrid_rrf(embedding, ..., body, ...)`）を主、**dense**
（`embedding <=> ...`）を副として同一コーパスで併測する（`PrecisionPolicy` が
dense/hybrid で別々の閾値を持つため、両方の既定値の妥当性判断材料になる）。

## 指標定義

QA セットを「正解あり Q+」「正解不在 Q0」に分ける。

| 指標 | 定義 | 分母 |
| ---- | ---- | ---- |
| Top-1 Accuracy | `precision` 出力が非空かつ先頭行 id が正解集合に含まれるクエリ数 / \|Q+\|。**空集合は不正解扱い**（fail-closed 側の保守的な定義） | Q+ |
| MRR@10 | `recall` モード `LIMIT 10`（候補生成段は precision と共通。SEARCH-9）の順位列で最初の正解の逆順位（10 位以内に無ければ 0）の平均 | Q+ |
| 誤返却率 | `precision` 出力が非空のクエリ数 / \|Q0\| | Q0 |

診断値（層 A のアサート対象外・`println!` のみ）: coverage（Q+ で非空を返した
割合）・条件付き Top-1 精度（非空のうち先頭が正解の割合）。

### MRR@10 を候補生成段で測る理由（ユーザー確認事項）

既定 `max_results`（1）の下では、`precision` 出力そのものの MRR@10 は Top-1
Accuracy と数学的に同値へ退化する（`precision` は非空なら常に 1 件しか返さないため、
「先頭が正解なら 1/1、そうでなければ 0」＝ Top-1 Accuracy と同じ値になる）。別指標
として意味を持たせるため、本ハーネスは `recall` モード `LIMIT 10`（precision と
候補生成段を共有する順位列）に対して MRR@10 を測る。この定義の妥当性——「precision
出力の MRR」ではなく「候補生成段の MRR」を基準に採用してよいか——はユーザー確認事項
の一つとする。

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

規模: `NUM_DOCS = 850`・`VOCAB_SIZE = 100`（密次元）・`|Q+| = 100`・
`|Q0| = 55`（hard negative 50 + 語彙外 5）。定数はテスト内固定・環境変数から受け
取らない。可視集合 850 件は `MAX_SEARCH_K`（10,000）に対し十分小さく、precision の
`k_eff` クランプに抵触しない。

## 2 層構成と spec 閾値の扱い

- **層 A**（`#[test]`。`make ci` 対象）: 決定的コーパスの Top-1 命中数・coverage 件数・
  MRR@10 命中クエリ数・誤返却件数を固定値アサーションで回帰トラッキングする。spec
  の数値基準は使わない。
- **層 B**（`#[ignore]`。`make precision-regression`）: `PRECISION_EVAL_MIN_TOP1_ACC`・
  `PRECISION_EVAL_MIN_MRR10`（`(0.0, 1.0]`）・`PRECISION_EVAL_MAX_FALSE_RETURN`
  （`[0.0, 1.0)`。`1.0` は常時 pass＝fail-open のため拒否）を環境変数から解決する。
  未設定は非 strict では「対象外」で成功終了、`PRECISION_EVAL_REQUIRE_THRESHOLDS=1`
  で fail-closed。非数値・範囲外は常に fail-closed。ログには実測値と pass/fail の
  みを出力し、閾値の数値は出力しない（`hybrid_recall.rs::resolve_gate_threshold`
  と同型）。
- **`.github/workflows/recall.yml` への接続は本 PR では行わない**: spec は「目標値
  確定まで `precision` をリリースゲートに含めない」としており、未確定の閾値変数を
  strict モードの週次 job へ追加すると、管理者に未確定値の設定を強いるか schedule
  を恒常的に red にする。層 B ＋ Makefile ターゲットは用意済みで、`recall.yml` への
  step 追加は**ユーザーの目標値確定後のフォローアップ**とする。
- **パラメータ感度スイープ**（`#[ignore]`。アサートなし）: `with_precision_policy`
  で hybrid の `min_top1`／`min_margin`／`max_results` を差し替え、3 指標の変化を
  表形式で出力する（判断材料の提示専用。production の既定値は変更しない）。

## 実測結果（層 A。決定的コーパス。2026-08-25 実測）

`cargo test -p engine --test precision_eval -- --nocapture` の実測値
（`|Q+| = 100`・`|Q0| = 55`）:

| ランキング | Top-1 Accuracy | MRR@10 | 誤返却率 | coverage | 条件付き Top-1 精度 |
| ---------- | -------------- | ------ | -------- | -------- | -------------------- |
| hybrid（主） | 0.6000 (60/100) | 0.8039 | 0.1273 (7/55) | 0.6500 (65/100) | 0.9231 (60/65) |
| dense（副） | 0.1000 (10/100) | 0.7805 | 0.0000 (0/55) | 0.1000 (10/100) | 1.0000 (10/10) |

観察（判断材料。数値評価は行わない）:

- hybrid は dense よりカバレッジ・Top-1 Accuracy とも高い一方、誤返却が 7 件
  （12.73%）発生している。既定 hybrid 閾値（0.98）が正解不在の hard negative
  クエリの一部を通してしまっている。
- dense は誤返却 0 件だが coverage が低く（10%）、既定 dense 閾値（0.80）が「答え
  られるはずのクエリ」の多くを空集合へ倒している可能性がある。
- MRR@10（候補生成段。precision 非依存）は両ランキングとも 0.78〜0.80 台で近く、
  候補生成自体の質は precision ゲートの通過率ほど大きく変わらない。

## パラメータ感度スイープ（hybrid。判断材料）

`cargo test -p engine --test precision_eval -- --ignored precision_eval_policy_sweep --nocapture` の出力
（dense 側は既定値 `DEFAULT_DENSE_MIN_TOP1`／`DEFAULT_DENSE_MIN_MARGIN` に固定し、
hybrid 側のみ格子で差し替え）:

| hybrid min_top1 | hybrid min_margin | max_results | Top-1 Accuracy | MRR@10 | 誤返却率 |
| ---------------- | ------------------ | ------------ | --------------- | ------ | -------- |
| 0.900 | 0.001 | 1/3 | 0.7000 | 0.8039 | 0.8364 |
| 0.900 | 0.005 | 1/3 | 0.7000 | 0.8039 | 0.8000 |
| 0.900 | 0.020 | 1/3 | 0.4600 | 0.8039 | 0.5273 |
| 0.980（既定） | 0.001 | 1/3 | 0.6000 | 0.8039 | 0.1273 |
| 0.980（既定） | 0.005（既定） | 1/3 | 0.6000 | 0.8039 | 0.1273 |
| 0.980（既定） | 0.020 | 1/3 | 0.4300 | 0.8039 | 0.1273 |
| 0.995 | 0.001 | 1/3 | 0.3500 | 0.8039 | 0.0182 |
| 0.995 | 0.005 | 1/3 | 0.3500 | 0.8039 | 0.0182 |
| 0.995 | 0.020 | 1/3 | 0.2900 | 0.8039 | 0.0182 |

観察: `max_results` を 1→3 に広げても本コーパスでは指標が変化しない（Q+ 側の
正解集合が小さく、precision 通過後の先頭連続一致件数が 1 件を超えるケースがほぼ
無いため）。`min_top1` を上げると誤返却率は単調に下がるが Top-1 Accuracy も下がる
トレードオフが明確に観測できる。`min_margin` の影響は `min_top1` ほど大きくない。

## ユーザー確認事項（目標値確定のための判断材料）

1. 上記実測 3 指標・感度表を踏まえた仮置き目標値の妥当性
2. Top-1 Accuracy の定義（空集合を不正解とするか、正解不在クエリの理想的な空集合
   応答を別枠でカウントすべきか）
3. MRR@10 の測定対象（候補生成段 vs. `precision` 出力。上記「MRR@10 を候補生成段で
   測る理由」参照）
4. hybrid 既定閾値（0.98 / 0.005）の見直し要否（誤返却率 12.73% を許容範囲とみなす
   かどうか）

## 既知の制約

- 合成コーパスによる暫定測定であり、実コーパスでの評価は未了（TASK-104/108 と同種の
  制約）
- クエリ展開（PLAN-5 系）は未接続のため、本ハーネスは precision 単体（クエリ展開
  なし）の測定に留める
- wire 経由の検証（TASK-165）・`USING PLAN` モード推定（TASK-164）は本タスクの管轄
  外

## 申し送り

- **目標値の確定判断**（ユーザー）: 上記実測・感度表・ユーザー確認事項をもとに
  仮置き目標値の妥当性を確認する
- **spec 側**: SEARCH-10 期待欄（目標値）の更新は spec リポ（`docs/spec`）の作業
- **`recall.yml` への層 B 接続**: 目標値確定後に `PRECISION_EVAL_*` を environment
  `recall-gate` へ設定し strict 注入付きの step を追加する（README 参照）
- **既定閾値（`precision.rs` の `DEFAULT_*`）の見直し**: 実測結果次第でユーザー判断
  のうえ別 PR
