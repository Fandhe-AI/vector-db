# ADR: 3 クライアント統合検証ハーネスの層 A/層 B 分割（WIRE-1）

- ステータス: Accepted
- 対応: TASK-73（WIRE-1）、TASK-165（SQL-12・SEARCH-9）、TASK-168（SQL-13・SQL-14）
- 関連: TASK-67・TASK-68・TASK-69・TASK-70・TASK-71（wire プロトコル層）、
  TASK-74・TASK-75・TASK-80・TASK-161・TASK-162・TASK-166・TASK-167（SQL 表層）、
  TASK-137（RLS 暗黙適用）

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

### TASK-165: セッション複数文の対応と検証範囲

`USING MODE` 句・`SET search_mode`（SQL-12）・`precision` の確信度ゲート
（SEARCH-9）の wire 経由検証も同じ層分割に従う。

- **層 A**（`crates/wire-server/tests/wire_search_mode.rs`）: 主たる回帰保護。
  クエリ句／セッション変数の優先順位、未知モード値・`$n` 形式の拒否、低確信度
  `precision` が空集合の通常応答（エラーではない）になること、テナント境界を
  越えて確信度ゲートへ他テナントの `Private` 行が混入しないことを、生バイトの
  wire クライアントで常時（`make ci`）検証する。
- **層 B**（`crates/wire-server/tests/three_client_e2e.rs`）: `SET
  search_mode = ...` を先行実行してから本体の `SELECT` を送る、同一接続内の
  セッション複数文を 3 クライアントで検証するため、既存の単文実行
  （`WIRE_SQL`）に加えて任意の `WIRE_SQL_PRELUDE`（実行順を保った SQL 文の
  JSON 配列）を追加した。`tests/three_client/{psycopg_client.py,pg_client.js}`
  は `WIRE_SQL_PRELUDE` を同一接続で逐次実行してから `WIRE_SQL` を実行し、
  psql 側は `run_psql_session`（複数 `-c` を同一セッションで送る）で同じ契約を
  実現する。拒否経路の検証では、各クライアントの失敗出力に SQLSTATE
  （`[SQLSTATE=<code>]`）を含めるようにし、`run_*_session_expect_sqlstate`
  ヘルパーで「非 0 終了 かつ期待 SQLSTATE を含む」ことを assert する。
  代表ケース（クエリ句 precision の Top-1 応答／`SET` 経由の precision 適用／
  未知モード値の拒否）のみを検証し、層 A で確定済みの全閾値パターンを層 B へ
  複製しない（層 B は無改造クライアント経由の受信確認が目的であり、閾値の
  網羅は層 A の責務）。

### TASK-168: 集計クエリ（SQL-13／SQL-14）の検証範囲

集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`。SQL-13）・`GROUP BY`/`HAVING`
集計（SQL-14）の wire 経由検証も同じ層分割に従う。

- **層 A**（`crates/wire-server/tests/wire_aggregate.rs`）: 主たる回帰保護。
  単一行集計・`GROUP BY` の既定順（キー昇順）・`HAVING`・`ORDER BY`／`LIMIT`、
  空集合契約（`COUNT`=0、他は NULL）、数値オーバーフロー（`22003`）・
  型不整合／許可形状外（`22000`／`42601`）の拒否経路、テナント境界（他
  テナントの `Private` 行を大量追加しても `COUNT`／`GROUP BY` の結果が
  不変であること）を、生バイトの wire クライアントで常時（`make ci`）検証
  する。
- **層 B**（`crates/wire-server/tests/three_client_e2e.rs`）: `docs` テーブルに
  Private 行（他テナントにのみ存在するグループ値を含む）を追加した専用 seed
  （`seed_aggregate_three_tenant_db`）に対し、単一行集計・`GROUP BY`/`HAVING`・
  RLS 不変（Private 専用グループが現れない）・拒否経路 2 種（型不整合・
  許可形状外）の代表ケースのみを 3 クライアントで検証する。Node `pg` の
  `Object.values(row)` 出力仕様（同名列の衝突）を避けるためすべての SELECT に
  一意の `AS` 別名を付け、NULL 描画の描画差異（psql／psycopg／pg で表現が
  異なる）を避けるため NULL を返す SQL は使わない（NULL 契約は層 A の
  DataRow -1 長検証に閉じる）。

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
- `EXPLAIN` 応答での実効モード・指定元の可視化（SQL-12 が SQL-6 と併せて
  期待する項目）: engine に `EXPLAIN` 自体が未実装のため対象外（SQL-6 の
  確定化で扱う）
- 拡張クエリプロトコル経由の `USING MODE $n`: WIRE-8 で拡張クエリ自体を
  拒否しているため、MVP は簡易クエリの `42601` 拒否のみを検証する
