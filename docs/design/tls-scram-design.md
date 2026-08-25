# ADR: TLS・SCRAM 実運用に関する設計調査（WIRE-9）

- ステータス: Proposed
- 対応: TASK-72（WIRE-9）
- 関連: TASK-70（WIRE-7）

## 背景

wire プロトコル層は PostgreSQL wire プロトコル v3 互換の自作実装であり、
psql・psycopg・node pg が無改造で接続可能なことを PoC-8 で実測済みである
（README.md「実装方針（要点）」）。今後、非 loopback な環境へ配置する運用を
見据えると、通信路の暗号化（TLS）と認証方式（SCRAM）の扱いを事前に整理して
おく必要がある。

現状の `crates/wire-server` は TASK-66 時点の stub（`src/main.rs` のみ）であり、
TCP 待ち受け・SSLRequest ハンドシェイク・認証フローは未実装である。

## 論点

決定内容・採否・ライブラリ選定は private spec 側を参照する
（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）。

- TLS 証明書管理方式: `docs/spec/05-tasks.md` TASK-72、
  `docs/spec/04-behavior/wire-protocol.md` WIRE-9
- TLS ライブラリ選定: 依存最小方針
  （[dependency-policy](../../.claude/rules/dependency-policy.md)）に従い
  ユーザー承認を経て確定する
- SCRAM 採否: 上記 spec 参照先を参照
- WIRE-7（TASK-70）との関係: 上記 spec 参照先を参照

## 影響

具体的な影響範囲・後続タスクの割り当ては private spec 側を参照する。

## スコープ外

- TLS・SCRAM の実装コード（SSLRequest ハンドシェイク・証明書ロード・SCRAM
  チャレンジ・レスポンス）
- TLS ライブラリの依存追加
- 証明書管理方式・SCRAM 採否・TLS ライブラリ選定の確定内容そのもの
- `docs/spec` 側ドキュメントへの反映（spec リポジトリ側の作業）

## 参照

- `docs/spec/05-tasks.md`（TASK-72・TASK-70）
- `docs/spec/04-behavior/wire-protocol.md`（WIRE-9・WIRE-7）
- `docs/spec/06-roadmap.md`（MS-3）
