# ADR: 3 クライアント統合検証ハーネスの層 A/層 B 分割（WIRE-1）

- ステータス: Accepted
- 対応: TASK-73（WIRE-1）
- 関連: TASK-67・TASK-68・TASK-69・TASK-70・TASK-71（wire プロトコル層）、
  TASK-74・TASK-75・TASK-80・TASK-161（SQL 表層）、TASK-137（RLS 暗黙適用）

## 背景

WIRE-1 は「無改造の実クライアント（psql・Python psycopg・Node.js pg）から
C1〜C4 を実行できる」ことを製品コードで統合検証するビヘイビアである。しかし
本リポジトリのローカル開発環境・Docker 開発コンテナ（`Dockerfile`）のいずれにも
`psql`／`psycopg`／`pg` は導入されておらず、`make ci` の必須経路へそのまま
組み込むと開発者のローカル検証（`make ci`）が壊れる。

## 決定

検証を 2 層に分割する。

- **層 A**（`crates/wire-server/tests/wire1_simple_query.rs`）: 生バイトの wire
  クライアント（`tests/common`）による常時回帰テスト。`make ci`・`cargo test`
  の通常経路で毎回実行される。3 クライアントが実際に送信するのと同一の簡易
  クエリバイト列を検証するため、層 B が実行できない環境でも WIRE-1 の中核
  契約（結果セット整形・エラー応答・接続維持）を回帰保護する。
- **層 B**（`crates/wire-server/tests/three_client_e2e.rs`）: 実 `psql`・
  Python `psycopg`・Node.js `pg` を子プロセスとして駆動する統合テスト。
  `#[ignore]` とし、`make e2e-three-client` から明示的に実行する。ツール
  未導入・クライアントスクリプトの失敗は silent skip せず `panic!` で
  失敗させる（実行された場合に「全クライアントが実際に成功した」ことを
  保証するため）。C1〜C4（TASK-73／ビヘイビア WIRE-1。定義は private spec
  参照）それぞれを 3 クライアント全てで実行し、各ドライバでの挙動が独立
  オラクルと一致することを検証する（codex-review P2 指摘・PR #210）。

層 B は `.github/workflows/ci.yml` の必須チェックには含めない（このリポジトリで
psql・psycopg・pg の導入自動化を確定させるには、pip/npm の実バージョン確認が
必要であり、依存最小・完全固定方針（[dependency-policy](../../.claude/rules/dependency-policy.md)）
の下でユーザー承認を経て別途整備する）。

## 影響

- `crates/wire-server/src/{simple_query,result_encoder}.rs`（新規）・
  `handshake.rs`／`server.rs`／`main.rs`（拡張）により、簡易クエリが
  `engine::core::EngineCore` の SQL 表層へ到達する（TASK-73 本体）。
- `wire-server --db <path>` が必須化された（省略時は fail-closed で
  起動拒否。匿名・揮発 DB の暗黙生成はしない）。
- `Makefile` に `e2e-three-client`（opt-in・`ci` には含めない）を追加した。

## スコープ外

- `psql`・`psycopg`・`pg` の CI 自動導入ジョブ（バージョン確認・pin の確定は
  別途ユーザー承認を要する）
- Docker 開発コンテナへの `psql`／`psycopg` 追加
- SQL `INSERT` が書き込む行の可視性（`Visibility::Private` 固定）と wire 認証
  経由の `PolicyContext`（`Public` のみ許可）の非対称の解消（挿入直後の行が
  同一セッションからも見えない現状の是非）。codex-review P1・PR #210 指摘の
  検討過程で、`Private` 許可を wire 認証側へ広げる案は
  `wire1_three_tenant_visibility_public_shared_private_hidden`
  （自テナント自身の `Private` 行も含め wire 越しには不可視、という既存の
  最小権限境界）を壊すため不採用と判断した。当面は wire の簡易クエリ経路
  から `INSERT` 自体を公開しない（`simple_query.rs` のモジュールコメント
  参照）ことで非対称を回避しており、本項目は「wire 経由の書き込み系 SQL」の
  設計が定まるまで引き続きスコープ外
