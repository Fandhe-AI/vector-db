# ティア別レイテンシ受け入れ基準の検証

- ステータス: Proposed（本 PR は計測ハーネス・ゲート・opt-in 配線の整備までが範囲。
  常駐 Ollama を用意した環境での実測・Accepted 反映はオーナー作業）
- 対応: TASK-116（Issue #78。ポインタ: `docs/spec/05-tasks.md`）
- 前提: TASK-115（質問類型推定・ティアリング。PLAN-8。`tiering.rs`・
  `EngineCore::with_tiered_query_planner`／`plan_query_with_classification`）・
  TASK-110（LLM クエリプランニング。PLAN-1。`query_planner.rs`）・TASK-77（SQL-5。
  `USING PLAN('<query>')` の一意ディスパッチ）・TASK-158（性能計測プロトコル基盤）
- 関連: TASK-83（同型の p95 再測定 ADR の前例。`docs/design/
  c1-p95-dedicated-env-reverification.md`）・TASK-117（PLAN-9。wire 経由・3 クライアント
  でのレイテンシ検証。本タスクの範囲外）
- 対象ビヘイビア: `docs/spec/04-behavior/query-planning.md` PLAN-4・PLAN-6・PLAN-7
  （ポインタ表記のみ。定義・数値基準は spec 側を参照し本ドキュメントへは転記しない。
  `.claude/rules/spec-confidentiality.md`）

## 背景

TASK-115（PLAN-8）でティアリング機構（`tiering::classify`・`TieredPlanner`）と
`EngineCore::with_tiered_query_planner`／`plan_query_with_classification` が実装済みだが、
ティア別のレイテンシ受け入れ基準（PLAN-4・PLAN-6・PLAN-7）を実測して回帰テスト化する
仕組みが存在しなかった。本タスクはその計測ハーネス・判定ロジック・CI 配線を整備する。

計測は TASK-158（性能計測プロトコル基盤 `crates/engine/benches/harness/`）の契約
（warmup 20 回以上・計測 20 回以上・決定的シード RNG）に従う。既存の TASK-83（C1 p95
専有環境再測定）と同じ「実測層（bench・時間依存・ci 対象外）／判定ロジック層（純関数・
`make ci` 対象）」の 2 層構成を踏襲する。

## e2e の定義

`EngineCore::execute_sql` へ `SELECT id FROM <table> USING PLAN('<question>') LIMIT k`
を渡す既存の production ディスパッチ経路（TASK-77・SQL-5。`sql::using_plan` モジュール
ドキュメント参照）を、そのままエンドツーエンド計測区間（PLAN-6/7）として使う。この
経路は 1 回の呼び出しで以下をすべて実行する（内部の詳細な結線は `core.rs::
EngineCore::expand_query` が担う既存の共有フローで、本タスクは複製しない）:

1. 辞書スナップショット取得（`Self::dictionary_snapshot_with_index`）
2. ティア判定（`TieredPlanner::select`。`PlannerBinding::Tiered` 構成時のみ）
3. LLM 呼び出し・厳格パース（`query_planner::render_full_prompt` → `LlmClient::
   complete` → `query_planner::parse_expansion`）
4. 再埋め込み（`query_planner::reembed_expansion`）
5. 既存 C4 ハイブリッド実行形への束縛・実行（`sql::using_plan::bind_expansion` →
   `sql::exec::execute_statement`）

クエリ展開の追加処理時間（PLAN-4）は、上記 1〜3 のみを行う
`EngineCore::plan_query_with_classification` 単独の所要時間として個別に計測する
（`Self::plan_query`/`Self::plan_query_with_mode` とも共有する既存の展開フロー本体）。

wire プロトコル経由・3 クライアントでのレイテンシ検証は TASK-117（PLAN-9）の管轄であり、
本タスクは engine クレート内で完結する（wire-server は結線しない）。

## ティア routing の実証

計測に使う質問（`harness::tier::DIALOGUE_QUESTION`／`PRECISION_QUESTION`）は、
`tiering::classify` の判定優先順（パス様トークン一致 > 手掛かり語一致 > 辞書シンボル名
一致。`tiering.rs::classify` ドキュメンテーションコメント「優先順」参照）のうち、
**対象テーブルの辞書スナップショット内容に依存しない**判定経路のみで意図したティアへ
決定的に分類されるよう選定した:

| 質問 | 判定根拠 | 期待ティア |
| ---- | -------- | ---------- |
| `DIALOGUE_QUESTION`（`"open src/module.rs and check it"`） | パス拡張子（`.rs`）一致（`ClassificationSignal::PathMatch`） | `Tier::Dialogue` |
| `PRECISION_QUESTION`（`"explain the overall architecture"`） | 手掛かり語（`explain`／`architecture`）一致（`ClassificationSignal::AbstractionCue`） | `Tier::HighPrecision` |

この選定により、計測用コーパスの内容を変更しても routing 実証の結果が揺れない。
`tests/tier_latency_accept.rs::routing` が空辞書に対する `classify` 呼び出しで
この分類結果を時間非依存に固定する（PLAN-4 の「ティアを適用して実行する」前提の
層 A 相当）。`tier_latency_bench.rs` は実測のたび（warmup・計測の全反復）に実際の
`plan_query_with_classification` 呼び出しの分類結果を検証し、いずれかの反復で不一致が
あれば `TierJudgment::all_passed` を `false` にする（誤ったティアのモデルで基準判定
する false green/red を防ぐ）。

## 検証設計

| 項目 | 内容 |
| ---- | ---- |
| 実測入口 | `crates/engine/benches/tier_latency_bench.rs`（`cargo bench --bench tier_latency_bench -p engine` / `make bench-tier`） |
| 判定ロジック | `crates/engine/benches/harness/tier.rs`（時間非依存。env パース・p95 判定・routing 判定を集約した `judge`） |
| CI 回帰 | `crates/engine/tests/tier_latency_accept.rs`（`make ci` 対象。`harness/tier.rs` の単体テスト＋計測質問の production `classify` 結果固定） |
| 計測対象テーブル | `embedding VECTOR(32)`・`path TEXT`・`body TEXT`（TASK-120 のファイル形 `INSERT` 規約と同じ列名）。64 行の決定的合成コーパス（`harness::tier::build_corpus`。シード固定） |
| LLM クライアント | 対話ティア・高精度ティアそれぞれ独立の `OllamaClient`（`query_planner::OllamaConfig`。host/port 共有・モデル名のみ分離） |
| 埋め込み | `engine::embedding::HashingEmbedder`（決定的・ネットワーク不要の参照実装。TASK-120 が導入した production 型をそのまま使用） |
| 4 判定 | 対話ティア展開 p95・対話ティア e2e p95・高精度ティア展開 p95・高精度ティア e2e p95。いずれも `harness::accept::p95_from_samples`／`check_p95_within_limit` を再利用（複製しない） |
| routing 検証 | 上記「ティア routing の実証」参照。閾値を満たしていても不一致なら fail |
| 閾値・接続の注入 | `BENCH_TIER_DIALOGUE_MAX_EXPANSION_P95_MS`・`BENCH_TIER_DIALOGUE_MAX_P95_MS`・`BENCH_TIER_PRECISION_MAX_EXPANSION_P95_MS`・`BENCH_TIER_PRECISION_MAX_P95_MS`（正の整数・ms）・`BENCH_TIER_OLLAMA_HOST`／`BENCH_TIER_OLLAMA_PORT`／`BENCH_TIER_DIALOGUE_MODEL`／`BENCH_TIER_PRECISION_MODEL`。すべて env 経由で注入し本リポジトリにはハードコードしない |
| opt-in ゲート | `BENCH_TIER`（非空文字で opt-in。ローカル・外部計測環境で運用者が明示指定する env）。未 opt-in の既定 run は「測定不能」を明示ログ出力し正常終了（判定対象外）。opt-in 済みで接続・閾値が未設定・不正なら fail-closed で非ゼロ終了 |
| CI 配線 | なし。`.github/workflows/bench.yml` に `bench-tier` ジョブは置かない（GitHub ホステッド runner に常駐 Ollama が無く、self-hosted は組織承認済み例外の範囲外のため、opt-in の有無を問わず実測を成功させる CI 経路が存在しない。PR #269 Codex 指摘）。判定ロジック層 `crates/engine/tests/tier_latency_accept.rs` のみ `make ci` 対象。実測は README「ティア別レイテンシ受け入れ基準の実測手順」記載の Actions 外の承認済み手順で運用者が直接実行する |

## 実測状況

本 PR のコミット時点では、常駐 Ollama への実接続を要する実測（`make bench-tier` の
`BENCH_TIER=1` 経路）は実行していない。理由: 本ホストは常駐 Ollama を持たない共有
開発環境であり、TASK-110 の既存契約（実 Ollama への疎通確認は対象外。プロセス内 TCP
スタブで契約を固定）と同じ制約を引き継ぐ。

代わりに、以下を本 PR 内で検証した:

1. **opt-in ゲートの動作確認**: `BENCH_TIER` 未設定で `make bench-tier`（相当のバイナリ
   実行）を行い、「測定不能」の明示ログ出力とともに正常終了（exit 0）することを確認した。
2. **fail-closed の動作確認**: `BENCH_TIER=1` かつ接続・閾値 env 未設定で実行し、
   `BENCH_TIER_DIALOGUE_MAX_EXPANSION_P95_MS is not set` 等の明示エラーとともに
   非ゼロ終了（exit 1）することを確認した。
3. **ビルド確認**: `cargo bench --bench tier_latency_bench -p engine --no-run` が
   警告なしで成功することを確認した（`cargo clippy --workspace --all-targets
   --all-features -- -D warnings` にも本ベンチ・テストを含めて通過済み）。
4. **判定ロジック・routing の回帰確認**: `cargo test -p engine --test
   tier_latency_accept` を実行し、`harness::tier` の env パース・`judge`（p95 判定・
   routing 判定）・計測質問の production `classify` 結果固定がすべて pass することを
   確認した。

実測値そのもの（4 判定の p95・pass/fail）は未取得のため、常駐 Ollama を用意した環境
での再実行時に記録するテンプレートとして扱う。

## PLAN-4/6/7 の判定: 未充足（判定保留）

理由:

- 常駐 Ollama への実接続を要する実測を本 PR では行っていない
- 4 判定（対話ティア展開・対話ティア e2e・高精度ティア展開・高精度ティア e2e）の
  実測値そのものを取得していない

したがって、PLAN-4/6/7 は本 PR では**確定せず**、オーナーが常駐 Ollama を用意した
環境で `BENCH_TIER=1` の実測を行ったうえで本ドキュメントを Accepted に更新する運用
とする（TASK-83 の ADR と同じ流儀）。

## 実測手順（オーナー作業）

1. 常駐 Ollama（対話ティア用・高精度ティア用の 2 モデル）を用意する。
2. spec（`docs/spec/04-behavior/query-planning.md` PLAN-4/6/7）の p95 上限をコマンド
   ラインの環境変数としてのみ渡す（値をファイル・コミット・PR 本文・本ドキュメントに
   書かない）:

   ```bash
   BENCH_TIER=1 \
   BENCH_TIER_OLLAMA_HOST=127.0.0.1 \
   BENCH_TIER_OLLAMA_PORT=11434 \
   BENCH_TIER_DIALOGUE_MODEL=<対話ティア用モデル名> \
   BENCH_TIER_PRECISION_MODEL=<高精度ティア用モデル名> \
   BENCH_TIER_DIALOGUE_MAX_EXPANSION_P95_MS=<spec 値> \
   BENCH_TIER_DIALOGUE_MAX_P95_MS=<spec 値> \
   BENCH_TIER_PRECISION_MAX_EXPANSION_P95_MS=<spec 値> \
   BENCH_TIER_PRECISION_MAX_P95_MS=<spec 値> \
   make bench-tier
   ```

3. 標準出力の `p95_latency(tier_dialogue_expansion)`・`p95_latency(tier_dialogue_e2e)`・
   `p95_latency(tier_precision_expansion)`・`p95_latency(tier_precision_e2e)` の各行
   （実測値・pass・routing_matched のみ。閾値の数値は含まれない）を記録する。
4. すべて `pass=true` かつ `routing_matched=true` であれば、本ドキュメントのステータス
   を Accepted に更新し、PLAN-4/6/7 を「充足」へ書き換える。
5. `.github/workflows/bench.yml` に `bench-tier` ジョブは存在しない（GitHub ホステッド
   runner に常駐 Ollama が無く、self-hosted は組織承認済み例外の範囲外のため、CI 経路
   では opt-in の有無を問わず実測を成功させられない。PR #269 Codex 指摘）。本手順は
   README「ティア別レイテンシ受け入れ基準の実測手順」に記載の Actions 外の承認済み
   計測環境で運用者が直接実行する。

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
