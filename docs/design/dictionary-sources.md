# ADR: 辞書的情報源抽出パイプライン

- ステータス: Accepted（TASK-109 で実装済み）
- 対応: TASK-109（MS-4・対象ビヘイビア: PLAN-5）
- 実装: `crates/engine/src/dictionary.rs`・`crates/engine/src/core.rs::DictionaryCache`
- テスト: `crates/engine/tests/dictionary.rs`
- 関連: TASK-120（増分インデックス反映）・TASK-169（`PrefilterCache` の世代整合キャッシュ
  パターン）・TASK-110（後続タスク。本 ADR の対象外）

## 決定事項

### 1. 依存を追加せず手書きの抽出処理で実装する

dependency-policy（依存最小・ユーザー承認制）に従い、regex 等の新規クレートを追加せず、
`dictionary.rs` に閉じた手書きの抽出処理として実装した。

### 2. 増分インデックスとの連動は世代整合キャッシュで行う

`core::DictionaryCache` は `core::PrefilterCache`（TASK-169）と同一の失効パターンを
踏襲する。post-commit フックのような追加の結線は持たず、次回参照時に
`storage.current_generation()` との整合を判定して自己回復する構成とした。採用理由は
`PrefilterCache` と共通（レビュー・保守コストの低減、部分更新による不整合経路の排除）。

### 3. `path`/`body` 列を持たないテーブルへの適用は拒否する

新規 `CoreError` variant は追加せず、既存の `CatalogError::Invalid` を用いる
（`wire_code` 写像・`wire-server` 側の網羅的 match への影響を避ける）。エラーメッセージ
は固定の英語文言とし、他テナントのデータ・存在情報を含めない。

## 影響

- `crates/engine/src/dictionary.rs`（新規）
- `crates/engine/src/core.rs`: `DictionaryCache`・`EngineCore::dictionary_snapshot`・
  `EngineCore::with_dictionary_config` を追加（固有 API のため `core-api-check` の対象外）
- `crates/engine/tests/dictionary.rs`（新規）

## スコープ外

- Ollama 連携・クエリ展開（TASK-110）、ソフトブースト（TASK-111）、Recall 受け入れ検証
  （TASK-112〜113）は後続タスク
- 辞書のパス単位差分更新（再構築の最適化）・LLM プロンプトへの整形は TASK-110 側で
  必要になった時点で拡張する

## 参照

- `docs/spec/05-tasks.md`（TASK-109・TASK-110〜113・TASK-120・TASK-169）
- `docs/spec/04-behavior/query-planning.md`（PLAN-5）
- `docs/design/resend-semantics.md`（増分インデックスの置換セマンティクス）
