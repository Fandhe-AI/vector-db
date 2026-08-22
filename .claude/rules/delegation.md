# 委譲ルール（調査・設計フェーズ）

## 原則

main セッションはオーケストレーションに徹し、コンテキスト消費の大きい作業
（ファイルの大量読み込み・横断検索・外部仕様調査）は subagent へ委譲する。
main が直接ファイルを読むのは、委譲結果の確認や小さなピンポイント参照に限る。

## パスベース切り替え表（調査）

| 対象パス・内容 | 委譲先 Agent | model |
| -------------- | ------------ | ----- |
| `crates/` 配下のコード調査・構造把握 | explorer | sonnet |
| `docs/spec/`（private submodule）のタスク・ビヘイビア参照 | explorer（ポインタ表記で報告） | sonnet |
| pg wire v3・redb・SQLSTATE 等の外部仕様 | reference-researcher | sonnet |
| 依存候補クレートの調査 | reference-researcher | sonnet |
| lint・フォーマット状況の確認 | linter | haiku |

## model 配分

| 用途 | model |
| ---- | ----- |
| 複雑な横断判断・アーキテクチャ設計 | opus または fable（fable は特に大規模設計・横断判断の最上位 tier） |
| 調査・生成・実装・レビュー | sonnet |
| 機械的集計・lint・ドキュメント更新 | haiku |

## 注意

- 調査結果に `docs/spec` の本文を含めない（[spec-confidentiality](./spec-confidentiality.md)）
- 複数の独立した調査は 1 メッセージで並列に委譲する
