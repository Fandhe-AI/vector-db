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

## コミットメッセージ・PR 本文での運用

- コミットメッセージ（件名・本文）・PR 本文・レビュー返信も public 資産であり、上記の禁止事項・ポインタ表記が同様に適用される
- squash merge 後もブランチ上の中間コミットは GitHub の PR refs（`refs/pull/<n>/head`）から参照可能であり、
  main の history rewrite では除去できない（rewrite 自体も branch ruleset で禁止されている）。**マージ前の検査で防ぐ**ことを原則とする
- レビュー指摘（spec 転記）への対応コミットでは、「何を削除したか」の説明に spec 規則の内容・規則が定める対象を
  再転記しない。「private spec 由来の記述を削除（ビヘイビア ID のポインタのみ残置）」のように ID ポインタと削除の事実だけを書く
- squash merge の件名・本文は PR 本文から生成される。PR 本文（Summary・対象外・自動生成の要約を含む）にも
  spec 本文・内部判断の転記が無いことを PR 作成前チェック（create-pr の OWASP 観点）に含める
- マージ後に発見した場合は Issue で扱いを決め、history rewrite は原則行わない（判断記録: `docs/design/spec-pointer-in-commit-messages.md`）
