# 委譲ルール（作成・編集フェーズ）

## 原則

コードの作成・編集は担当レイヤの builder Agent へ委譲し、main は計画・レビュー・統合に徹する。

## パスベース委譲マッピング（実装）

| 対象パス | 委譲先 Agent | model |
| -------- | ------------ | ----- |
| `crates/engine/`（コアロジック・検索カーネル・認証・RLS・redb 永続化） | engine-builder | sonnet |
| `crates/wire-server/`（pg wire v3 自作実装・接続ハンドリング） | wire-builder | sonnet |
| テスト実行・失敗解析（`cargo test` / `cargo clippy`） | test-runner | sonnet |
| コードレビュー（AGENTS.md P0/P1/P2 観点） | reviewer | sonnet |
| セキュリティ監査（テナント境界・wire 入力・spec 漏えい） | security-auditor | sonnet |
| lint・整形の機械的確認 | linter | haiku |
| README・CLAUDE.md・ドキュメント更新 | docs-writer | haiku |

## 実装フローの標準形

1. 計画（main。必要に応じて explorer で事前調査）
2. 実装（builder へ委譲。engine と wire-server は独立なら並列可）
3. 検証（test-runner → 失敗があれば builder へ差し戻し）
4. レビュー（reviewer / security-auditor）
5. コミット（create-commit スキル。Conventional Commits・`--no-verify` 禁止）

## 注意

- 依存（Cargo.toml の dependencies）の追加・更新は builder に委譲せず、必ずユーザー承認を経る（[dependency-policy](./dependency-policy.md)）
- スコープ外の発見事項は放置せず [out-of-scope-tracking](./out-of-scope-tracking.md) に従い追跡する
