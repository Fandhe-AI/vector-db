# ティア別レイテンシ受け入れ基準の検証

- ステータス: Proposed（本 PR は計測ハーネス・ゲート・opt-in 配線の整備までが範囲。
  常駐 Ollama を用意した環境での実測・Accepted 反映はオーナー作業）
- 対応: TASK-116（Issue #78。ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-115（PLAN-8。`tiering.rs`・`EngineCore::with_tiered_query_planner`／
  `plan_query_with_classification`）・TASK-110（PLAN-1。`query_planner.rs`）・
  TASK-77（SQL-5。`USING PLAN('<query>')` の一意ディスパッチ）・TASK-158
  （性能計測プロトコル基盤）
- 関連: TASK-83（同型の p95 再測定 ADR の前例。`docs/design/
  c1-p95-dedicated-env-reverification.md`）・TASK-117（PLAN-9。wire 経由・3 クライアント
  でのレイテンシ検証。本タスクの範囲外）
- 対象ビヘイビア: `docs/spec/04-behavior/query-planning.md` PLAN-4・PLAN-6・PLAN-7
  （ポインタ表記のみ。判定内容・測定段階・数値基準は spec 側が SSOT であり、
  本ドキュメントへは転記しない。[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）

## 背景

TASK-115（PLAN-8）でティアリング機構（`tiering::classify`・`TieredPlanner`）と
`EngineCore::with_tiered_query_planner`／`plan_query_with_classification` が実装済みだが、
PLAN-4・PLAN-6・PLAN-7 の受け入れ基準を実測して回帰テスト化する仕組みが存在しなかった。
本タスクはその計測ハーネス・判定ロジック・CI 配線を整備する。判定内容・測定段階の
詳細は spec（上記ポインタ）を参照し、本ドキュメントには記載しない。

計測は TASK-158（性能計測プロトコル基盤 `crates/engine/benches/harness/`）の契約に
従い、既存の TASK-83（C1 p95 専有環境再測定）と同じ「実測層（bench・時間依存・ci
対象外）／判定ロジック層（純関数・`make ci` 対象）」の 2 層構成を踏襲する。

## 検証設計（実装配置のみ。判定内容は spec 側を参照）

| 項目 | 内容 |
| ---- | ---- |
| 実測入口 | `crates/engine/benches/tier_latency_bench.rs`（`make bench-tier`） |
| 判定ロジック | `crates/engine/benches/harness/tier.rs`（時間非依存の純関数群） |
| CI 回帰 | `crates/engine/tests/tier_latency_accept.rs`（`make ci` 対象。判定ロジックの単体テスト） |
| CI 配線 | なし。`.github/workflows/bench.yml` に `bench-tier` ジョブは置かない（GitHub ホステッド runner に常駐 Ollama が無く、self-hosted は組織承認済み例外の範囲外のため、opt-in の有無を問わず実測を成功させる CI 経路が存在しない。PR #269 Codex 指摘）。実測は README「ティア別レイテンシ受け入れ基準の実測手順」記載の Actions 外の承認済み手順で運用者が直接実行する |

判定基準・測定対象・opt-in env 名の一覧・数値基準は spec（`docs/spec/04-behavior/
query-planning.md` PLAN-4・PLAN-6・PLAN-7）が SSOT であり、実行に必要な env 名は
README「ティア別レイテンシ受け入れ基準の実測手順」を参照すること。

## 実測状況

本 PR のコミット時点では、常駐 Ollama への実接続を要する実測（`make bench-tier` の
opt-in 経路）は実行していない。理由: 本ホストは常駐 Ollama を持たない共有開発環境で
あり、TASK-110 の既存契約（実 Ollama への疎通確認は対象外。プロセス内 TCP スタブで
契約を固定）と同じ制約を引き継ぐ。

代わりに、以下を本 PR 内で検証した:

1. **opt-in ゲートの動作確認**: opt-in env 未設定で `make bench-tier`（相当のバイナリ
   実行）を行い、「測定不能」の明示ログ出力とともに正常終了（exit 0）することを確認した。
2. **fail-closed の動作確認**: opt-in しつつ接続・閾値 env 未設定で実行し、明示エラー
   とともに非ゼロ終了（exit 1）することを確認した。
3. **ビルド確認**: `cargo bench --bench tier_latency_bench -p engine --no-run` が
   警告なしで成功することを確認した（`cargo clippy --workspace --all-targets
   --all-features -- -D warnings` にも本ベンチ・テストを含めて通過済み）。
4. **判定ロジック・routing の回帰確認**: `cargo test -p engine --test
   tier_latency_accept` を実行し、`harness::tier` の判定ロジックがすべて pass する
   ことを確認した。

4 判定（対話ティア展開・対話ティア e2e・高精度ティア展開・高精度ティア e2e）の実測
そのものは本 PR では未実施。実測を行った場合も、p95 の数値そのものは本ドキュメント
（public）へは記録しない。実測値は非公開記録先へ保存し、本ドキュメントには
「実施済み/未実施」「pass/fail」「routing 一致/不一致」の非数値の状態のみを記録する
（下記「実測状態」表参照）。

## 実測状態

| 判定 | 実施状況 | pass/fail | routing 一致 |
| ---- | -------- | --------- | ------------- |
| 対話ティア クエリ展開 p95（PLAN-4） | 未実施 | - | - |
| 対話ティア e2e p95（PLAN-6） | 未実施 | - | - |
| 高精度ティア クエリ展開 p95（PLAN-4） | 未実施 | - | - |
| 高精度ティア e2e p95（PLAN-7） | 未実施 | - | - |

## PLAN-4/6/7 の判定: 未充足（判定保留）

常駐 Ollama への実接続を要する実測を本 PR では行っていないため、PLAN-4/6/7 は
本 PR では**確定せず**、オーナーが常駐 Ollama を用意した環境で実測を行ったうえで
本ドキュメントを Accepted に更新する運用とする（TASK-83 の ADR と同じ流儀）。

## 実測手順（オーナー作業）

具体的な env 名・実行コマンドは README「ティア別レイテンシ受け入れ基準の実測手順」
を参照すること。数値基準（p95 上限）は spec 側の値をコマンドラインの環境変数として
のみ渡し、値をファイル・コミット・PR 本文・本ドキュメントに書かない。

実測後、標準出力に記録された実測値・pass/fail・routing 結果を確認する。**実測値
（p95 の数値）そのものは本ドキュメント（public）へ転記しない**。実測値は非公開記録先
へ保存し、本ドキュメントの「実測状態」表には各判定の「実施済み/未実施」「pass/fail」
「routing 一致/不一致」のみを更新する。すべて `pass` かつ `routing 一致` であれば、
「実測状態」表を「実施済み」「pass」「一致」へ更新したうえで本ドキュメントのステータス
を Accepted に更新し、PLAN-4/6/7 を「充足」へ書き換える。いずれか `fail` または
`routing 不一致` の場合も、数値は書かず「実施済み」「fail」等の状態のみを表へ反映する。
`.github/workflows/bench.yml` に `bench-tier` ジョブは存在しない（GitHub ホステッド
runner に常駐 Ollama が無く、self-hosted は組織承認済み例外の範囲外のため。PR #269
Codex 指摘）。

## 制約・スコープ外

- 常駐 Ollama を用意した環境での本実測はオーナー作業（本 PR には含まれない）
- wire 経由・3 クライアントでのレイテンシ検証は TASK-117（PLAN-9）の管轄
- 実測値が基準を満たさない場合のチューニング（プロンプト・モデル選定・タイムアウト
  調整等）は別 Issue の管轄
- spec 側のステータス更新（PLAN-4/6/7 の Accepted 反映）はオーナー作業

## 参照

- `docs/spec/05-tasks.md` TASK-116（ポインタ）
- `docs/spec/04-behavior/query-planning.md` PLAN-4, PLAN-6, PLAN-7（ポインタ）
- `docs/design/query-tiering-criteria.md`（TASK-115・ティア判定基準）
- `crates/engine/benches/tier_latency_bench.rs`・`crates/engine/benches/harness/tier.rs`
- `crates/engine/tests/tier_latency_accept.rs`
- `docs/design/c1-p95-dedicated-env-reverification.md`（同型の再測定・判断記録 ADR の前例）
