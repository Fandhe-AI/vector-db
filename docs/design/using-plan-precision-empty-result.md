# `USING PLAN` が実 Ollama 経由で SQL エラーなしに 0 行を返す事象の調査

- ステータス: Accepted
- 対応: Issue #315
- 前提: TASK-117（PLAN-9 確定化の層 A。`crates/wire-server/tests/wire_using_plan.rs`）・
  TASK-77（SQL-5。`sql::using_plan::bind_expansion`）・TASK-111（PLAN-1。ソフトブースト
  機構）・TASK-162（SEARCH-9。`precision` モードの確信度ゲート）・TASK-164（PLAN-11。
  プランナーのモード推定）
- 関連ビヘイビア: SEARCH-3（融合が単体チャネルを下回らない）・SEARCH-9・PLAN-9・PLAN-11
  （ポインタ: `docs/spec/04-behavior/`）
- 検証コード: `crates/engine/tests/sql_using_plan.rs::using_plan_respects_using_mode_precision`・
  `crates/engine/tests/sql_using_plan.rs::using_plan_applies_planner_mode_hint_when_no_explicit_mode_is_set`・
  `crates/engine/tests/sql_explain.rs::explain_reports_mode_source_planner_estimate`・
  `crates/wire-server/tests/wire_using_plan.rs::using_plan_wire_precision_hint_returns_zero_rows_then_recall_override_returns_rows`

## 事象

`wire-server --planner-endpoint <実 Ollama> --planner-model <model> --embedder-hashing-dim 256`
を起動し、psql から `SELECT id FROM docs USING PLAN('...') LIMIT 10;` を複数回実行すると、
SQL エラーなしで毎回 0 行が返る。同一 DB に対する平易な `ORDER BY embedding <=> ...`
は seed 行を返す（行の不在・可視性の問題ではない）。planner 到達不能時の fail-closed
（`XX000`・接続継続）とは異なる経路である。

## 調査観点と読解結果

| # | 仮説 | 読解結果 | 判定 |
| - | ---- | -------- | ---- |
| H1 | プランナーの `mode_hint`（PLAN-11）が `precision` を返し、`precision` の確信度ゲート（SEARCH-9）が空集合へ倒している | `query_planner.rs` の展開結果パースは `mode` フィールド（`"precision"`／`"recall"`／`null`）を受理する。明示 `USING MODE`／`SET search_mode` が無い場合、`sql/mode.rs::resolve_mode_with_planner` はプランナー推定を採用する。`precision` モードの確信度ゲートは正規化 RRF スコアに下限を課すため、密・疎の 1 位が食い違うと最良スコアが下限を下回り **0 行**になりうる。この経路は SQL エラーを発生させず `CommandComplete("SELECT 0")` になる | **確認済み**（下記「検証結果」参照） |
| H2 | 疎チャネル 0 件のとき密のみへ縮退できていない | `hybrid_search`／`rrf_fuse_with_limits` は空スライスを許容し、疎 0 件時は密のみへ縮退する契約（SEARCH-3）。単独では 0 行の原因にならない | 棄却（H1 と独立） |
| H3 | `path_hint`/`kind_hint` がハードフィルタ化している | `sql::using_plan::bind_expansion` は両ヒントを読まず、`sql/exec.rs` へのソフトブースト結線も未実装（TASK-111 の対象外）。フィルタ化する経路は存在しない | 棄却 |
| H4 | 密プール境界の同点グループ除外（Issue #310・#320）で密が空になる | 小規模コーパス（可視行が `MAX_FETCH_K` 以下）では `complete_boundary_tie_group_by` が常に `Resolved` を返す。除外は非 exhaustive（大規模スケール）時のみ | 棄却（本事象の小規模条件には該当しない） |
| H5 | `HashingEmbedder` の展開後クエリと seed 側 embedding の意味的な不整合 | 展開後クエリと seed embedding が独立に生成されていれば密側の類似度は必ずしも高くない。`recall` モードでは順位付けされてそのまま返るが、`precision` では最良スコアが下限を下回りやすく H1 を助長する | 副次要因 |

## 検証結果（決定的フィクスチャによる再現）

`crates/engine/tests/sql_using_plan.rs::using_plan_respects_using_mode_precision` は、
seed 2 行に対して展開後テキストの文字数のみに依存する決定的な定数ベクトルを返す
`RecordingEmbedder` を注入し、以下を固定する（実行して確認済み）。

- `USING MODE 'precision'`（または `mode_hint: "precision"` の暗黙適用。
  `using_plan_applies_planner_mode_hint_when_no_explicit_mode_is_set` が対応）:
  `execute_sql_in_session` は `Ok(SqlOutcome::Query(result))` を返し、
  `result.rows.is_empty()` が真（**SQL エラーではなく空の正常応答**）
- 同一クエリ・同一接続へ `USING MODE 'recall'` を付けると `result.rows` は seed 2 行を含む
  （positive control。空集合が embedder/query_planner の設定不備やテナント境界の
  取りこぼしでないことを示す）
- `EXPLAIN SELECT ... USING PLAN(...)`（`mode_hint: "precision"`、明示指定なし）は
  `mode: precision` / `mode_source: planner_estimate` を報告する
  （`sql_explain.rs::explain_reports_mode_source_planner_estimate`）

`crates/wire-server/tests/wire_using_plan.rs::
using_plan_wire_precision_hint_returns_zero_rows_then_recall_override_returns_rows` は
同じ構図を wire フレーミング越しに再確認する: `RowDescription` → `CommandComplete("SELECT 0")`
→ `ReadyForQuery`（エラー応答を経由しない・接続継続）→ 同一接続での `USING MODE 'recall'`
再送で `SELECT 2`。

この結果は Issue の観測（エラーなし・0 行・複数回とも同じ）と整合する。**0 行は
「プランナー推定 `precision` → 確信度ゲートによる空集合の通常応答」という既存契約
（SEARCH-9）どおりの挙動であり、SEARCH-3（融合が単体チャネルを下回らない）違反ではない。**

ただし、この確認はあくまで決定的スタブ `LlmClient`・決定的 `Embedder` を用いた
engine/wire レベルの再現であり、実 Ollama が実際に返した `mode` フィールドの値・
実埋め込みサービスの出力そのものは確認していない（受け入れ条件によりモデル名・
LLM 応答本文は記録しない）。したがって以下の再現手順で実環境側を切り分けることを推奨する。

## 実 Ollama 環境での再現・切り分け手順

1. ループバック限定の `--planner-endpoint`（`build_query_planner` の既存検証）で
   `wire-server` を起動する: `--planner-endpoint <loopback>:<port> --planner-model <model>`
   `--embedder-hashing-dim 256`
2. `INSERT`（`path`/`body` 列指定、TASK-120）で数件投入する
3. 同一クエリに対し `EXPLAIN SELECT id FROM docs USING PLAN('<question>') LIMIT 10;` を実行し、
   出力の末尾 2 行（`mode: ...` / `mode_source: ...`）を確認する
   - **`mode: precision` / `mode_source: planner_estimate`** の場合:
     確信度ゲートによる空集合が期待される既知経路（本ドキュメントの判断どおり）。
     `USING PLAN('<question>') LIMIT 10 USING MODE 'recall'` または
     セッション変数（`SET search_mode = 'recall'`）で明示上書きすれば行が返るかを確認する
   - **`mode: recall` かつ `SELECT ...USING PLAN(...)` が依然 0 行**の場合:
     本ドキュメントの H1 では説明できない別経路であり、上記の確信度ゲート契約に
     収まらない**未解明の欠陥**として扱う。再現条件（`EXPLAIN` の全行・行数・
     コーパス規模）を記録したうえで別 Issue として追跡する（本ドキュメントは
     この分岐の確定調査までは含まない）
4. モデル名・LLM 生応答本文・プロンプト内容は記録・転記しない
   （spec-confidentiality.md・Issue の受け入れ条件）

## 判断

- production コード（`crates/engine/src/**`・`crates/wire-server/src/**`）は変更しない。
  上記「検証結果」により、決定的フィクスチャの範囲では 0 行が SEARCH-9 の契約どおりの
  挙動であることを確認済み
- 「`EXPLAIN` が `mode: precision`」を再現手順の切り分け基準として明文化し、
  `mode: recall` でも 0 行が再現する場合は別経路の欠陥として扱う二分岐をドキュメント化した
  （本ドキュメントは前者の分岐を確定させるのみで、後者を否定するものではない）

## スコープ外（本 Issue では対応しない。起票はオーナー判断）

- 確信度ゲートで空集合になった際の wire `NoticeResponse` 等、`EXPLAIN` 以外の可観測化
- プランナー推定 `mode_hint` を既定で採用する現行方針の是非（PLAN-11 は spec 側 SSOT）
- `precision.rs` の仮置き閾値（`DEFAULT_HYBRID_MIN_TOP1`／`DEFAULT_HYBRID_MIN_MARGIN`
  等）の実測確定（TASK-163）
- 実 Ollama・実 3 クライアントでの PLAN-9 数値基準実測ハーネス（TASK-117 後続）
- ソフトブースト（`path_hint`/`kind_hint`）の `sql/exec.rs` 結線（TASK-111 後続）
