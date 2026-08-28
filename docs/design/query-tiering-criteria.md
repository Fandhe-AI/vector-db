# ADR: 質問類型推定・ティアリングの判定基準（PLAN-8）

- ステータス: Accepted（判定基準の具体値はオーナー判断待ちの仮置き。下記「判定基準の
  最終確定について」参照）
- 対応: Issue #77（TASK-115）
- 関連: `docs/spec/05-tasks.md`（TASK-115）

## 概要

Issue #77（TASK-115）は、LLM クエリプランニング（TASK-110・`query_planner.rs`）の
対話ティア／高精度ティアの振り分けに使う質問類型推定を実装する対応である。検討内容・
判断の根拠は private spec 側で管理する。本ドキュメントは、対応する公開コード上の参照先と、
本実装が採用した判定方式の要点（既にコードで公開済みの範囲）を記録するためのポインタである。
本リポジトリで公開している設計方針の範囲は README.md「実装方針（要点）」の通りであり、
それを超える内容はここに記載しない。

## 実装の要点（コードで公開済みの範囲）

- 質問類型（`crate::tiering::QuestionClass`: `Direct`／`Intent`／`Abstraction`）と
  ティア（`crate::tiering::Tier`: `Dialogue`／`HighPrecision`）への既定割り当ては
  `crate::tiering::tier_for_class` に実装済み（`Direct → Dialogue`、`Intent`・
  `Abstraction → HighPrecision`）。
- 判定は決定的・線形時間のルールベース（`crate::tiering::classify`）で、辞書的情報源
  （TASK-109・`dictionary.rs`）から得たシンボル名・パスへの一致を優先し、次に手掛かり語
  一致、いずれにも一致しなければ意図型（`Intent`）とする。優先順・各シグナルの詳細は
  `crate::tiering::classify` のドキュメンテーションコメントを参照。
- fail-safe の方向: 空入力・上限超過等の縮退時は `Intent`（＝高精度ティア）へ倒す
  （品質を優先する側を安全側とする）。
- 判定基準の具体値（手掛かり語・拡張子リスト等）は `crate::tiering::TieringCriteria`
  として構成可能にし、ハードコードしていない。

## 判定基準の最終確定について

判定基準そのもの（`TieringCriteria` の既定値）の設計は、TASK-115 の分担上オーナーとの
共同タスクである。本実装は差し替え可能な既定値までを実装範囲とし、既定値自体の最終確定は
オーナー判断待ちとする（TASK-163 の「評価ハーネス実装済み・目標値の確定はユーザー判断待ち」
と同じ運用に倣う）。

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
