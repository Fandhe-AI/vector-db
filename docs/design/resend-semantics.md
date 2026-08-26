# ADR: 同一パスファイル再送セマンティクス

- ステータス: Accepted（TASK-120 が本決定に基づき実装済み。TASK-93 は未着手）
- 対応: TASK-123（MS-1 / phase:1・基盤・工程管理）
- 反映先（実装・テスト）: TASK-120（INDEX-1/2。実装済み）・TASK-93（RECOVER-2。未着手）
- 関連: TASK-92・TASK-80

## 背景

取り込み時の同一ファイル再送に関する処理方針を本実装がどう定めるかを規定する。
TASK-120・TASK-93 の実装はこの決定に従う。

## 決定事項

決定の詳細は private spec（`docs/spec`）側の該当タスクを参照する。本リポジトリ
で公開している設計方針の範囲は README.md「実装方針（要点）」の通りである。

## 影響

- TASK-120（INDEX-1/2）: 取り込み処理で本決定に基づく実装を行う（実装済み。
  `crates/engine/src/incremental.rs::index_file`・
  `crates/engine/src/tenant.rs::replace_typed_rows_by_text_key`・
  `crates/engine/tests/incremental_index.rs` が本決定に対応するコード・テスト）
- TASK-93（RECOVER-2）: リカバリ処理で本決定と整合する状態復元を行う（未着手）

## スコープ外

- TASK-93 側の実装・テスト追加（未着手）
- `docs/spec` 側への本決定の反映は spec リポジトリ側の作業であり、本リポジトリから
  は submodule を変更しない

## 参照

- `docs/spec/05-tasks.md`（TASK-123・TASK-120・TASK-93・TASK-92・TASK-80）
- `docs/spec/06-roadmap.md`（MS-1）
