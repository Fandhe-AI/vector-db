# ADR: 質問類型推定・ティアリングの判定基準（本リポの実装既定値）

- ステータス: Accepted
- 対応: Issue #77（TASK-115）

## 概要

Issue #77（TASK-115）は、LLM クエリプランニング（TASK-110・`query_planner.rs`）の
対話ティア／高精度ティアの振り分けに使う質問類型推定を実装する対応である。本ドキュメントは、
本リポジトリが実装した判定方式・既定値の要点を記録するためのポインタである。

## 実装の要点

- 質問類型（`crate::tiering::QuestionClass`: `Direct`／`Intent`／`Abstraction`）と
  ティア（`crate::tiering::Tier`: `Dialogue`／`HighPrecision`）への既定割り当ては
  `crate::tiering::tier_for_class` に実装済み（`Direct → Dialogue`、`Intent`・
  `Abstraction → HighPrecision`）。
- 判定は決定的・線形時間のルールベース（`crate::tiering::classify`）で、辞書的情報源
  （TASK-109・`dictionary.rs`）のパス様トークン一致を最優先とし、次に手掛かり語一致
  （抽象的な言い回し）、次に辞書シンボル名への完全一致、いずれにも一致しなければ
  意図型（`Intent`）とする。手掛かり語一致をシンボル名一致より先に判定するのは、
  一般英語と衝突しうるありふれた識別子（`new`・`main`・`read` 等）を辞書シンボル名が
  含みうるため、説明・意図の質問が対話ティアへ誤ってルーティングされるのを防ぐ
  fail-safe 上の判断（Bugbot 指摘対応・PR #261）。優先順・各シグナルの詳細は
  `crate::tiering::classify` のドキュメンテーションコメントを参照。
- fail-safe の方向: 空入力・上限超過等の縮退時は `Intent`（＝高精度ティア）へ倒す
  （品質を優先する側を安全側とする）。
- 判定基準の具体値（手掛かり語・拡張子リスト等）は `crate::tiering::TieringCriteria`
  として構成可能にし、ハードコードしていない。既定値は
  `crate::tiering::TieringCriteria::default` を参照。

## 影響を受けるコード

- `crates/engine/src/tiering.rs`（新規: `QuestionClass`・`Tier`・`Classification`・
  `TieringCriteria`・`classify`・`TieredPlanner`）
- `crates/engine/src/lib.rs`（`pub mod tiering;` 追加）
- `crates/engine/src/core.rs`（`PlannerBinding` enum の追加による
  `EngineCore::query_planner` フィールドの整理、`EngineCore::with_tiered_query_planner`・
  `EngineCore::plan_query_with_classification` の追加。`EngineCore::with_query_planner`・
  `EngineCore::plan_query` の既存シグネチャ・契約は不変）
- `crates/engine/tests/tiering.rs`（結合テスト）

## スコープ外

- ティアの EXPLAIN 露出（SQL-6・PLAN-11／TASK-164）
- ティア別レイテンシ検証（TASK-116）
- 実 Ollama への疎通確認（TASK-110 と同様、注入点までが対象）
- `sql/exec.rs` への結線

## 参照

- `docs/spec/05-tasks.md`（TASK-115）
