# ADR: TLS・SCRAM 実運用に関する設計調査（WIRE-9）

- ステータス: Proposed
- 対応: TASK-72（WIRE-9）・TASK-174（HTTP-10。WIRE-9 と同一方針）
- 関連: TASK-70（WIRE-7）

## 背景

wire プロトコル層は PostgreSQL wire プロトコル v3 互換の自作実装であり、
psql・psycopg・node pg が無改造で接続可能なことを PoC-8 で実測済みである
（README.md「実装方針（要点）」）。今後、非 loopback な環境へ配置する運用を
見据えると、通信路の暗号化（TLS）と認証方式（SCRAM）の扱いを事前に整理して
おく必要がある。

`crates/wire-server` の bind ガード（`src/bind_guard.rs`。TASK-70・WIRE-7）が
`main.rs::run_server` の唯一の bind 経路であり、Issue #735 以降は `--surface
sql|nosql`（TASK-171・HTTP-1）で選択する SQL 表層・NoSQL 表層のいずれも同じ
経路を共有する（下記「NoSQL 表層（HTTP-10）」節参照）。

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

## NoSQL 表層（HTTP-10）

- NoSQL 表層（`--surface nosql`・HTTP/1.1 最小サブセット。TASK-171／HTTP-1・
  HTTP-9）も SQL 表層と同じ通信路保護状態
  [`wire_server::bind_guard::TransportSecurity`] を共有する。
  `main.rs::run_server` は両表層共通の `GuardedBindAddrs::resolve`／`bind()`
  を通した**後**に accept ループのみを分岐する構造であり（Issue #735・
  PR #795）、bind 経路自体は表層ごとに分岐しない。
- 非ループバック運用では TLS を必須とする設計要件を、WIRE-9 と同一のまま
  NoSQL 表層へ適用する（HTTP-10）。TLS 未構成の間は
  `TransportSecurity::Cleartext` により、SQL 表層・NoSQL 表層のいずれを
  選んでも非ループバックアドレスへの bind は起動時に fail-closed で拒否
  される（WIRE-7・HTTP-9 の既存契約）。
- NoSQL 表層側で独自の暗号化方式（別 TLS 経路・独自スキーム）を新設しない。
  TLS 導入時に `TransportSecurity` へ variant を追加すると、Rust の
  exhaustive match により `GuardedBindAddrs::resolve` の分岐更新が両表層
  共有の 1 箇所で強制される（`bind_guard.rs` の既存設計意図を踏襲する）。
  ただし、この型強制が保証するのは bind 可否判定（`resolve`）の更新の
  みであり、`bind()` 自体は通常の `TcpListener` を返す。TLS ハンドシェ
  イク・証明書ロードを含む実際の接続処理（SQL 表層・NoSQL 表層それぞれの
  accept ループ）への TLS 適用は本設計の対象外であり、各表層側で別途
  実装・確認する必要がある。
- 認証方式（SCRAM 採否）は通信路暗号化とは別軸として扱う点も WIRE-9 と
  同一とする。

決定内容・確定条件は private spec 側を参照する
（`docs/spec/04-behavior/http-transport.md` HTTP-10、
`docs/spec/05-tasks.md` TASK-174）。

## スコープ外

- TLS・SCRAM の実装コード（SSLRequest ハンドシェイク・証明書ロード・SCRAM
  チャレンジ・レスポンス）
- NoSQL 表層側の TLS 実装コード（HTTP リスナーへの TLS 終端・証明書ロード）
- TLS ライブラリの依存追加
- 証明書管理方式・SCRAM 採否・TLS ライブラリ選定の確定内容そのもの
- HTTP-10 の確定化判定そのもの（spec 側・TASK-174 系の後続タスク）
- `docs/spec` 側ドキュメントへの反映（spec リポジトリ側の作業）

## 参照

- `docs/spec/05-tasks.md`（TASK-72・TASK-70・TASK-174）
- `docs/spec/04-behavior/wire-protocol.md`（WIRE-9・WIRE-7）
- `docs/spec/04-behavior/http-transport.md`（HTTP-9・HTTP-10）
- `docs/spec/06-roadmap.md`（MS-3）
- `crates/wire-server/src/bind_guard.rs`
- `crates/wire-server/src/http/listener.rs`
