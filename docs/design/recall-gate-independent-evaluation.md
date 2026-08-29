# ADR: recall.yml の 3 ゲートを独立評価・AND 集約する

- ステータス: Accepted
- 対応: Issue #311
- 親 Issue: #301（hybrid Recall@20 ゲート未達の調査）
- 関連: #303（実測値の非印字）、#286・`docs/design/ci-gate-variables.md`
  （閾値注入・Environment `recall-gate` の実行境界）

## 背景

`.github/workflows/recall.yml` の `recall-regression` job は `Run recall-regression`
（hybrid・TASK-104）→ `Run rerank-regression`（TASK-108）→
`Run query-planning-regression`（TASK-112/113）を直列 step として実行していた。
先頭の hybrid step が fail すると、後続 2 step は GitHub Actions の既定挙動により
`skipped` になる。

実測（run 33251403345・`workflow_dispatch`・main）でも
`Run recall-regression: failure` → `Run rerank-regression: skipped` →
`Run query-planning-regression: skipped` となっており、hybrid が red の間
（#301・#310）は rerank / query-planning ゲートが CI 上で一度も評価されない
状態が続いていた。

## 要件

1. hybrid step が fail でも rerank / query-planning step が実行されること。
2. 3 step のいずれかが fail なら job は failure（AND 集約）。「一度も評価して
   いない run」を green にしない strict モード（`*_REQUIRE_THRESHOLDS=1`）の
   挙動は維持する。
3. ログに実測値・閾値を印字しない（Issue #303 の抑止方針を維持。`RECALL_VERBOSE`
   は引き続き非注入）。
4. マージ後に main で `workflow_dispatch` し、3 ゲートそれぞれの pass/fail
   （数値なし）を親 Issue #301 に記録する（本 ADR のスコープ外・申し送り）。

## 採用案: 同一 job 内で `continue-on-error` + 最終判定 step

3 つの gate step（`Run recall-regression` / `Run rerank-regression` /
`Run query-planning-regression`）に `id:`（`hybrid` / `rerank` / `query_planning`）
と `continue-on-error: true` を付与し、job 末尾に最終判定 step
`Evaluate recall gates` を追加した。

- `steps.<id>.outcome` は `continue-on-error` 適用前の結果
  （`success` / `failure` / `cancelled` / `skipped`）であり、`conclusion` は
  適用後（常に `success`）になる。最終判定は **`outcome` を見る**
  （`conclusion` を見ると `continue-on-error` により常に green になり誤 green
  化する）。
- 判定は fail-closed: `success` 以外（`failure` だけでなく `skipped` /
  `cancelled` / 空文字列）はすべて fail 扱いにする。
- 最終判定 step は `if:` を付けない（既定 `success()`）。`continue-on-error`
  付き step は `conclusion` が `success` になるため gate step の fail では
  最終判定が skip されず、一方で Checkout / rustup が失敗した場合は最終判定も
  走らず job がそのまま red になる（いずれも誤 green 化しない）。
- 判定 step へ渡す値は `run:` へ `${{ }}` を直接埋め込まず `env:` 経由で渡す
  （式インジェクション回避の定石）。
- 出力は `gate=<name> outcome=<success|failure|...>` の行と最終 `pass=true|false`
  のみ。数値は一切扱わない（Issue #303 と整合）。

## 不採用案: job 分割（gate ごとに job）

各 job で checkout + `cargo test --release` のビルドが 3 回走り、所要時間・
runner コストが約 3 倍になる。Environment `recall-gate` を各 job に付け直す
必要もある。Issue の受け入れ条件は「job 全体の conclusion は全 step の AND」を
前提としているため、同一 job 内の独立評価で満たすほうがコストが小さい。

## strict モード・#303 抑止との関係

各 gate step 内部の strict モード（`HYBRID_RECALL_REQUIRE_THRESHOLDS=1` 等）は
無変更。閾値未設定時に該当 step 自体が fail-closed で `failure` になる挙動は
そのまま維持し、本 ADR の変更は「その `failure` を後続 step の実行・job の
最終判定にどう反映するか」のみを扱う。ログの非数値出力方針（Issue #303）も
維持し、最終判定 step は `outcome`（enum 文字列）のみを扱う。

## 既知の制約

- 3 step すべてが pass する run の所要時間は従来の直列実行と変わらない
  （独立評価後も 3 step は同一 job 内で順に実行される）。
- 各 gate の実測値・閾値の数値は本 ADR・関連ドキュメントに書かない
  （spec-confidentiality・Issue #303）。

## 申し送り

- hybrid Recall@20 ゲート未達そのものの解消（#301・#310）。
- query planning direct 系ゲート red の扱い（マージ後の実 run 結果に基づき
  別 Issue を起票するかはユーザー承認事項）。
- `DEGRADED` 副検査の非 strict 経路への分離（`ci-gate-variables.md` 既存の
  申し送り）。
- job 分割による gate ごとの UI 表示・secrets の最小権限化（コスト 3 倍のため
  本 ADR では不採用）。
- マージ後の `workflow_dispatch` 疎通確認・3 ゲート pass/fail の #301 への記録
  （`ci-gate-variables.md` の設定手順に準拠。数値は記録しない）。
