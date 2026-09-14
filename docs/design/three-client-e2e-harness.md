# ADR: 3 クライアント統合検証ハーネスの層 A/層 B 分割（WIRE-1）

- ステータス: Accepted
- 対応: TASK-73（WIRE-1）、TASK-165（SQL-12・SEARCH-9）、TASK-168（SQL-13・SQL-14）、
  TASK-82（SQL-5〜7・9・10）
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

### TASK-82: 拡張構文（SQL-5〜7・9・10）の検証範囲・`extended_syntax_e2e.rs` の採用理由

`USING PLAN`（SQL-5）・`EXPLAIN`（SQL-6）・`HINT ORDER`（SQL-7）・宣言的 UDF
呼び出し（SQL-9）・`USING OPERATION_ID` 付き `INSERT`（SQL-10）の wire 経由
検証も同じ層分割に従う。spec（TASK-82）の定義に基づき、層 B は
`three_client_e2e.rs` とは別ファイル（`extended_syntax_e2e.rs`）に分離した。
層 A はそれぞれ既存の流儀（`wire_using_plan.rs`（TASK-117・PLAN-9 で
先行実装済み）・`wire_explain.rs`・新規 `wire_hint_order.rs`・
`wire_udf_call.rs`・`wire_insert_operation_id.rs`）に従う。

- **層 A**: 各構文の規則自体は engine 側 in-process 結合テスト（`sql_using_plan.rs`
  等）が確定オラクルとして検証済みのため、同じ規則が wire フレーミング越しに
  観測できることの確認に徹する。常時（`make ci`）実行される。
- **層 B**（`crates/wire-server/tests/extended_syntax_e2e.rs`）: `USING PLAN`・
  `EXPLAIN` は `wire-server` バイナリへの `--planner-endpoint`／
  `--planner-model`／`--embedder-hashing-dim`（TASK-117）注入を要するため、
  子プロセス起動前にプロセス内 HTTP スタブ（Ollama `/api/generate` 互換の
  最小応答。`engine::query_planner` の単体テスト `spawn_stub_server` と同型）を
  立て、`--planner-endpoint 127.0.0.1:<stub port>` として渡す。`HINT ORDER`・
  `CREATE FUNCTION`・`INSERT` は追加注入を要しないため素の `wire-server` 起動で
  足りる。各構文 1 本の代表ケースのみを 3 クライアントで確認する（層 A で
  確定済みの拒否経路・順列網羅を層 B へ複製しない方針は TASK-165・TASK-168 と
  同じ）。

**INSERT の wire 受理（判断の記録）**: TASK-82（SQL-10）の定義に基づき、本タスク
で wire 経由の `INSERT` 受理へ切り替えた（下記「スコープ外」の旧項目を参照。
判断の詳細は `simple_query.rs` モジュールコメント）。読み取り可視性の既定
（`Public` のみ）は拡大していない。

### Issue #454: 広域取得（ソートなしのフィルタ取得）の検証範囲

`ORDER BY`／`USING PLAN` を伴わない `SELECT ... [WHERE ...] LIMIT n`（広域取得。
契約の詳細は `docs/design/wide-retrieval-scan.md`）の wire 経由検証も同じ層分割に
従う。

- **層 A**（`crates/wire-server/tests/wire_scan.rs`）: 実行契約自体は engine 側
  in-process 結合テスト（`sql_scan.rs`）が確定オラクルとして検証済みのため、
  同じ規則（複数行の返却・`WHERE` 絞り込み・RLS 非漏えい・`USING MODE`／
  `EXPLAIN` 前置の拒否・取得モードからの独立性）が wire フレーミング越しに
  観測できることの確認に徹する。常時（`make ci`）実行される。
- **層 B**（`crates/wire-server/tests/extended_syntax_e2e.rs::
  three_clients_run_scan_where_nosort`）: 追加注入を要しないため素の
  `wire-server` 起動で足りる。`seed_plain_docs`（単一テナント）に対する
  代表ケース 1 本（bare `LIMIT` と、可視総数を超える `LIMIT` の 2 パターン）を
  3 クライアントで確認する（層 A で確定済みの拒否経路・順列網羅を層 B へ
  複製しない方針は TASK-165・TASK-168・TASK-82 と同じ）。

### Issue #705: テスト専用 commit 後 panic 注入フラグ（`fault-injection` feature）

**目的**: commit 成功境界を跨いだ panic → 緊急応答の同期送出 → abort
（TASK-97・RECOVER-6。応答本体は `D`=`state=may_be_committed`。ERR-5）は、
これまで engine のプロセス内テストと wire-server 層 A
（`tests/wire_emergency_response.rs`。テストバイナリ自身が登録・commit・
panic を再現する）でしか観測できず、`wire-server` バイナリを外部クライアント
から commit 後 panic させる手段が無かった。#702（3 クライアント e2e で `D`
到達を確認する）の前提として、テスト専用・feature gate 付きの注入フラグを
バイナリへ追加した。

**feature gate と CLI 契約**: `crates/wire-server/Cargo.toml` の
`fault-injection` feature（default に含めない・依存追加なし）を有効化した
ビルドでのみ `--fault-inject post-commit-panic` を受理する。既定ビルドでは
このフラグ自体が存在せず `main.rs` の `other =>` 分岐で未知引数として拒否
される。値欠落・不正値（閉じた語彙 `post-commit-panic` の厳密一致のみ受理）・
2 回目以降の重複指定はいずれも fail-closed で起動エラー（`--search-engine`
等・Issue #656 と同じ判断枠組み）。

**注入点の位置と理由**: 実際の panic 注入は
`crate::simple_query::execute_and_respond` の「登録ブロック」
（`_emergency_registration` が生存する区間。同関数のコメント参照）の**内側**、
`engine.execute_sql_in_session` の呼び出し直後に置く
（`crate::fault_injection::maybe_panic_after_commit`）。この区間の外
（ブロック終端後の `match outcome { .. }` 側）で panic しても、登録は既に
drop 済みで緊急応答は送られず接続断（RECOVER-5 の abort バックストップ）に
なるだけのため、注入点をここより後ろへ移動してはならない。登録ブロックの
境界自体は変更していない。

**arm-once と発火条件**: `main.rs::run_server` が bind 成功後・`listening on`
出力前に `fault_injection::arm` を高々 1 回呼ぶ（テストが両行の出力順に
依存できるようにするため）。発火は `Ok(SqlOutcome::Insert(_))`（commit 成功
を意味する）のときだけ arm を消費する（`compare_exchange` による take-once）。
`Err(_)` や他の読み取り専用 variant（`SELECT`・`EXPLAIN` 等）では arm は
一切触らず据え置くため、失敗した INSERT・SELECT を挟んでも後続の成功
INSERT で確実に発火する。

**安全性の判断**: default features に含めないため既定ビルド・crates.io
公開の既定構成にはシンボルもフラグも存在しない。feature を有効化しても
露出するのは「CLI を握る者が自プロセスを 1 回だけ commit 後 panic で終了
させる」能力のみで、テナント境界・RLS・認証・fail-closed 経路を迂回する
API は一切露出しない（engine の `bench-internals` feature と同じ判断
枠組み）。発火経路自体は production の RECOVER-6 経路そのもので、追加する
のはトリガーだけ。`make lint`／`make test` は `--all-features` のため CI
でも常にコンパイル・実行され、feature コードの腐敗を防ぐ。

**検証層**: 単体テスト（`crates/wire-server/src/fault_injection.rs`）は
`is_committed_insert`／take-once の判定純関数のみを検証する（実発火は
`ResponseBoundaryGuard` の `Drop` と `fail_fast::install` により必ず
SIGABRT に至るためプロセス内テスト不可）。結合テスト
（`crates/wire-server/tests/wire_fault_injection_cli.rs`）は
`CARGO_BIN_EXE_wire-server` の実子プロセスとして起動し、CLI 引数の拒否・
未 arm 時の通常応答・実際の発火（緊急応答の `S`/`C`/`M`/`D` フィールド・
abort・再オープン後の可視性）・arm 維持（SELECT・拒否された INSERT を
挟んでも消費されない）を検証する。e2e 層（#702・`three_client_e2e.rs`）は
本フラグを使って 3 クライアント経由の `D` 到達を確認する担当。

**既知の制約**: `make test`／CI は `--all-features` のため
`cfg(not(feature = "fault-injection"))` の既定ビルド拒否テスト
（`default_build_rejects_fault_inject_flag_as_unknown_argument`）は CI では
実行されない。`cargo test -p fandhe-vector-db-wire-server`（feature 無し）
を別途ローカルで実行することが唯一の検査手段。

## 影響

- `crates/wire-server/src/{simple_query,result_encoder}.rs`（新規）・
  `handshake.rs`／`server.rs`／`main.rs`（拡張）により、簡易クエリが
  `engine::core::EngineCore` の SQL 表層へ到達する（TASK-73 本体）。
- `wire-server --db <path>` が必須化された（省略時は fail-closed で
  起動拒否。匿名・揮発 DB の暗黙生成はしない）。
- `Makefile` に `e2e-three-client`（opt-in・`ci` には含めない）を追加した。
- `crates/wire-server/Cargo.toml` に `fault-injection` feature（default 外）
  を追加し、`e2e-three-client` の `three_client_e2e` 行はこの feature 付きで
  ビルドするよう変更した（Issue #705）。

## スコープ外

- `psql`・`psycopg`・`pg` の CI 自動導入ジョブ（バージョン確認・pin の確定は
  別途ユーザー承認を要する）
- Docker 開発コンテナへの `psql`／`psycopg` 追加
- SQL `INSERT` が書き込む行の可視性（`Visibility::Private` 固定）と wire 認証
  経由の `PolicyContext`（`Public` のみ許可）の非対称の解消（wire セッションへの
  自テナント `Private` 行の読み戻し可視性付与）。TASK-82（SQL-10）で `INSERT`
  自体は wire 経由で受理するよう切り替えたが（旧: 当面 `INSERT` 自体を
  公開しない方針だった。codex-review P1・PR #210 指摘の検討過程の判断）、
  `Private` 許可を wire 認証側へ広げる案は
  `wire1_three_tenant_visibility_public_shared_private_hidden`
  （自テナント自身の `Private` 行も含め wire 越しには不可視、という既存の
  最小権限境界）を壊すため引き続き不採用とし、非対称（書いた本人も同一
  セッションでは読み戻せない）はそのまま残した。本項目は「wire セッションへの
  読み戻し可視性付与」の設計が定まるまで引き続きスコープ外
- `EXPLAIN` 応答での実効モード・指定元の可視化（SQL-12 が SQL-6 と併せて
  期待する項目）: engine に `EXPLAIN` 自体が未実装のため対象外（SQL-6 の
  確定化で扱う）
- 拡張クエリプロトコル経由の `USING MODE $n`: WIRE-8 で拡張クエリ自体を
  拒否しているため、MVP は簡易クエリの `42601` 拒否のみを検証する
