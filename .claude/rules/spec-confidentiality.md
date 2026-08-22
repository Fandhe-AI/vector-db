# private spec の機密保持規約（リポ固有・P0）

## 前提

- 本リポジトリは **public**
- 仕様・ビヘイビア定義の SSOT は **private** リポジトリ [vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec)（`docs/spec` submodule）にあり、**意図的に非公開を維持**する

## 禁止事項（AGENTS.md P0 準拠）

以下を public な資産（コード・コメント・ドキュメント・Issue・PR 本文・コミットメッセージ）へ持ち込まない:

- spec 本文の長文引用・ファイルコピー
- 非公開の内部判断・設計議論の転記
- spec の構成・内容を実質的に復元できる要約

## 許可される参照（ポインタ表記）

- spec 内のファイルパス（例: `docs/spec/04-behavior/...`）
- タスク ID（TASK-nn）・ビヘイビア ID・マイルストーン ID（MS-n）
- 1〜2 行程度の、README「実装方針（要点）」で**既にオーナーが公開済みの範囲**に収まる要約

## 運用

- 公開可否に迷った場合は**転記しない側**に倒し、ユーザーに確認する
- subagent への指示・subagent からの報告でも同じ規約を適用する（報告が PR 等へ転記されうるため）
- レビュー時（reviewer / security-auditor）は spec 漏えいを P0 として検査する
