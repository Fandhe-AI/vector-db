# ADR: TLS・SCRAM 実運用に関する設計調査（WIRE-9）

- ステータス: Proposed（確定前の調査・論点整理メモ。決定点はオーナー確定待ちであり、
  本ドキュメント単体では設計タスク完了を主張しない。確定後、別コミットで内容を
  更新し Accepted に遷移する）
- 対応: TASK-72（MS-3 / phase:3・wire プロトコル層・WIRE-9）
- 関連: TASK-70（未実装・後続タスク・WIRE-7）

## 背景

wire プロトコル層は PostgreSQL wire プロトコル v3 互換の自作実装であり、
psql・psycopg・node pg が無改造で接続可能なことを PoC-8 で実測済みである
（README.md「実装方針（要点）」）。今後、非 loopback な環境へ配置する運用を
見据えると、通信路の暗号化（TLS）と認証方式（SCRAM）の扱いを事前に整理して
おく必要がある。本タスクはコード実装ではなく、導入方式を検討するための論点
整理ドキュメントを作成するタスクである。具体的な決定内容・採否・ライブラリ
選定は private spec 側の該当タスクで確定するため、本ドキュメントでは記載しない
（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）。

現状の `crates/wire-server` は TASK-66 時点の stub（`src/main.rs` のみ）であり、
TCP 待ち受け・SSLRequest ハンドシェイク・認証フローは未実装である。本タスクは
その実装に着手する前段の論点整理であり、ランタイム挙動を変更しない。

## 論点（決定は別途行う）

以下はいずれも本ドキュメント単体では未確定であり、推奨案・採否・ライブラリ選定を
含む具体的な決定内容は private spec 側（`docs/spec/05-tasks.md` の TASK-72、
`docs/spec/04-behavior/wire-protocol.md` の WIRE-9）を参照する。

- **TLS 証明書管理方式**: pg wire v3 の `SSLRequest`／`S`・`N` 応答によるネゴシエー
  ションを自作 wire 実装のどこに組み込むか、証明書・秘密鍵の受け渡し方式をどう
  するか（決定は private spec 側）。
- **TLS ライブラリ選定**: 依存最小方針（[dependency-policy](../../.claude/rules/dependency-policy.md)）
  との整合を踏まえた選定。採用する場合は dependency-policy の承認情報 4 項目
  （クレート名・バージョン `=x.y.z` 完全固定・ライセンス・メンテナンス状況・
  推移的依存の概要）を提示しユーザー承認を得たうえで確定する。**本タスクでは
  依存を追加しない。**
- **SCRAM 採否**: 通信路暗号化（TLS）とは独立した認証方式の論点であり、採否・
  確定時期は private spec 側を参照する。
- **WIRE-7（TASK-70）との関係**: 関連する wire プロトコル層タスクであり、詳細な
  整合内容は private spec 側および WIRE-7 実装時に確定する。

## 影響

- TLS ハンドシェイク・証明書ロードの実装: 決定確定後の後続実装タスクで対応
  （具体的なタスク ID は spec 側で確定）
- SCRAM 認証フローの実装: 採否確定後の該当タスクで対応
- WIRE-7（TASK-70）: 決定確定後、実装時に本ドキュメントとの整合を反映する

## スコープ外

- TLS・SCRAM の実装コード（SSLRequest ハンドシェイク・証明書ロード・SCRAM
  チャレンジ・レスポンス）
- TLS ライブラリの依存追加（Cargo.toml への追加はユーザー承認後の後続タスク）
- 証明書管理方式・SCRAM 採否・TLS ライブラリ選定の確定内容そのもの（private spec
  側で確定し、本リポジトリの public ドキュメントへは転記しない）
- `docs/spec` 側ドキュメントへの本決定の反映（spec リポジトリ側の作業であり、本
  リポジトリから submodule を変更しない）

## 参照

- `docs/spec/05-tasks.md`（TASK-72・TASK-70）
- `docs/spec/04-behavior/wire-protocol.md`（WIRE-9・WIRE-7）
- `docs/spec/06-roadmap.md`（MS-3）
