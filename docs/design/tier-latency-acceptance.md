# ティア別レイテンシ受け入れ基準の検証

- ステータス: Proposed（計測ハーネス・ゲート・opt-in 配線は整備済み。Mac の
  承認済み計測環境での初回実測は 2026-08-29 に実施済み〔下記「実測状態」〕だが、
  複数回の実行のうち判定前に中断した回があり、中断の扱いが spec 側で未確定〔#316〕のため
  Accepted 化は保留）
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

ハーネス整備時（PR #269 時点）は、常駐 Ollama への実接続を要する実測（`make bench-tier`
の opt-in 経路）を実行していなかった（当時のホストは常駐 Ollama を持たない共有開発
環境であり、TASK-110 の既存契約〔実 Ollama への疎通確認は対象外〕と同じ制約を
引き継いだため）。その後 2026-08-29 に Mac の承認済み計測環境で初回実測を実施した
（結果は「実測状態」節）。

ハーネス整備時に検証した項目は以下のとおり:

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
結果は下記「実測状態」表に記録する。p95 の数値そのものは本ドキュメント（public）へは
記録しない。実測値は非公開記録先へ保存し、本ドキュメントには「実施済み/未実施」
「pass/fail」「routing 一致/不一致」の非数値の状態のみを記録する。

## 実測状態

| 判定 | 実施状況（Mac・2026-08-29） | 判定到達 | pass/fail（判定到達回） | routing 一致 | 実施状況（DGX Spark） |
| ---- | -------------------------- | -------- | ----------------------- | ------------- | --------------------- |
| 対話ティア クエリ展開 p95（PLAN-4） | 実施済み・中断あり | 到達回あり・未到達回あり | pass | 一致 | 未実施 |
| 対話ティア e2e p95（PLAN-6） | 実施済み・中断あり | 到達回あり・未到達回あり | pass | - | 未実施 |
| 高精度ティア クエリ展開 p95（PLAN-4） | 実施済み・中断あり | 到達回あり・未到達回あり | pass | 一致 | 未実施 |
| 高精度ティア e2e p95（PLAN-7） | 実施済み・中断あり | 到達回あり・未到達回あり | pass | - | 未実施 |

「判定到達」は `make bench-tier` の 1 回の実行が 4 判定の算出まで到達したかを示す
（中断した実行は 4 判定すべてが未算出。中断は対話ティア展開段で発生したが、後続の
3 判定も同じ実行内では算出されないため全行に反映している）。実行回数・到達回数・
中断回数は非公開記録先に保存し、本ドキュメントには「到達回あり／未到達回あり」の
状態のみを記録する。

Mac（Apple Silicon・常駐 Ollama）で `make bench-tier` を複数回連続実行した。判定に
到達した回はすべての判定が `pass`、routing 一致は記録対象の判定で `一致` だった
（routing 一致の記録対象は `crates/engine/benches/tier_latency_bench.rs` が
`routing_matched` を出力する判定に限る。出力の無い判定は表で `-`）。一方で一部の実行では対話ティアの
LLM 応答が展開結果として解釈できない形式（`InvalidResponse`）で返り、判定前に
ベンチが中断した（レイテンシ超過ではなく LLM 出力形式の非決定性による環境要因。
`docs/design/query-planning-recall-regression.md` の展開失敗時の扱いと同種）。
安定して判定に到達しないため本ドキュメントのステータスは Proposed のまま据え置く
（今回の実測に対する暫定判断。下記「PLAN-4/6/7 の判定」節）。
p95 の実測値・閾値・使用モデル名は非公開記録先に保存し、本ドキュメントへは
転記しない。DGX Spark での実測は未実施。

## PLAN-4/6/7 の判定: 判定保留（判定到達回は pass・中断の扱いが spec 側で未確定）

Mac の承認済み計測環境での初回実測では、判定に到達した回はすべて `pass`・記録対象の
routing は `一致` だったが、連続実行の一部は LLM 応答形式の非決定性により判定前に
中断している（「実測状態」節）。受け入れ条件は spec（PLAN-4/6/7）が SSOT であり本
ドキュメントでは変更・具体化しない。判定前に中断した実行を受け入れ判定上どう扱うかは
spec 側で未確定（#316）のため、**今回の実測に限った暫定判断として** PLAN-4/6/7 を
**確定せず判定保留**とする。#316 でオーナーが確定した後に再実測し、spec の条件で
充足と判断できれば本ドキュメントを Accepted に更新する（TASK-83 の ADR と同じ流儀）。

## 実測手順（オーナー作業）

具体的な env 名・実行コマンドは README「ティア別レイテンシ受け入れ基準の実測手順」
を参照すること。数値基準（p95 上限）は spec 側の値をコマンドラインの環境変数として
のみ渡し、値をファイル・コミット・PR 本文・本ドキュメントに書かない。

実測後、標準出力に記録された実測値・pass/fail・routing 結果を確認する。**実測値
（p95 の数値）そのものは本ドキュメント（public）へ転記しない**。実測値は非公開記録先
へ保存し、本ドキュメントの「実測状態」表には各判定の「実施済み/未実施」「pass/fail」
「routing 一致/不一致」のみを更新する（routing 一致は `tier_latency_bench.rs` が
`routing_matched` を出力する判定のみ記録し、出力の無い判定は `-`）。すべて `pass` かつ
`routing 一致` であれば、「実測状態」表を「実施済み」「pass」「一致」へ更新したうえで
本ドキュメントのステータスを Accepted に更新し、PLAN-4/6/7 を「充足」へ書き換える。
いずれか `fail` または `routing 不一致` の場合も、数値は書かず「実施済み」「fail」等の
状態のみを表へ反映する。

実行が判定前に中断した場合（`InvalidResponse` 等の LLM 応答不正による panic）の
扱いは spec 側で未確定（#316）のため、中断の有無・要因を非数値の状態（「中断あり」
「到達回あり／未到達回あり」）として表・本文へ記録するに留め、Accepted 更新は行わず
オーナー判断（#316）へ回す（実行回数・中断回数は非公開記録先へ）。
`.github/workflows/bench.yml` に `bench-tier` ジョブは存在しない（GitHub ホステッド
runner に常駐 Ollama が無く、self-hosted は組織承認済み例外の範囲外のため。PR #269
Codex 指摘）。

## 制約・スコープ外

- 常駐 Ollama を用意した環境での実測はオーナー作業（Mac は初回実施済み。中断の扱い
  確定後の再実測・DGX Spark での実測は未実施）
- ベンチ側での LLM 不正応答（`InvalidResponse`）時の扱い（リトライ・除外・判定項目化）
  の定義は #316 の管轄
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
