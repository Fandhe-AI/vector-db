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
から commit 後 panic させる手段が無かった。#706（3 クライアント e2e で `D`
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
でも常にコンパイル・実行され、feature コードの腐敗を防ぐ。既定ビルド
（feature 無効）側の `--fault-inject` 拒否契約は `make test-default-build`
（`make ci` に含む）・`.github/workflows/ci.yml` の `test-default-build`
ジョブが担う（Issue #715・#716）。

**検証層**: 単体テスト（`crates/wire-server/src/fault_injection.rs`）は
`is_committed_insert`／take-once の判定純関数のみを検証する（実発火は
`ResponseBoundaryGuard` の `Drop` と `fail_fast::install` により必ず
SIGABRT に至るためプロセス内テスト不可）。結合テスト
（`crates/wire-server/tests/wire_fault_injection_cli.rs`）は
`CARGO_BIN_EXE_wire-server` の実子プロセスとして起動し、CLI 引数の拒否・
未 arm 時の通常応答・実際の発火（緊急応答の `S`/`C`/`M`/`D` フィールド・
abort・再オープン後の可視性）・arm 維持（SELECT・拒否された INSERT を
挟んでも消費されない）を検証する。e2e 層（#706・`three_client_e2e.rs`）は
本フラグを使って 3 クライアント経由の `D` 到達を確認する担当。

**既定ビルド側の検査経路**: `make test`／`rust-ci` は `--all-features` の
ため `cfg(not(feature = "fault-injection"))` の既定ビルド拒否テスト
（`default_build_rejects_fault_inject_flag_as_unknown_argument`）はこの経路
には入らない。この既定ビルド側の拒否契約は `make test-default-build`
（`make ci` に含む）・`.github/workflows/ci.yml` の独立ジョブ
`test-default-build` が常時実行して検査する（Issue #715・#716・PR #718）。

### Issue #706: 3 クライアントでの緊急応答 detail 到達検証

**目的**: ERR-5（`docs/spec/04-behavior/error-format.md`）の期待欄が定める
「無改造クライアントから `detail` へ到達できる」ことを、Issue #705 の注入
フラグを使って実クライアント経由で機械検証する。Issue #701 で `D` フィールド
自体のバイト列契約は層 A で固定済みだったが、psql・psycopg・node `pg` それぞれ
の**ドライバ API から**その値が実際に読めるかは未検証のまま残っていた。

**到達フィールド（実測で確認）**:

| クライアント | 到達フィールド | stderr 出力例 |
| --- | --- | --- |
| psql | `DETAIL:` 行（`-v VERBOSITY=verbose` 必須） | `DETAIL:  state=may_be_committed` |
| psycopg | `e.diag.message_detail` | `[DETAIL=state=may_be_committed]` |
| node `pg` | `err.detail` | `[DETAIL=state=may_be_committed]` |

**1 クライアント 1 サーバーの理由**: `--fault-inject post-commit-panic`
（Issue #705）は 1 プロセスにつき高々 1 回しか発火しない take-once 契約
（同プロセスへ 2 回目の成功 INSERT を送っても発火しない）。3 クライアントを
同一サーバーへ順に接続すると 2 クライアント目以降が発火を観測できないため、
`three_clients_receive_emergency_response_detail_after_post_commit_panic`
はクライアント（psql→alice、psycopg→bob、pg→carol）ごとに独立した
`ServerGuard`・一時 DB・テナントを用意する。

**psql の終了コードと `LC_ALL`**: psql は `ErrorResponse` 受信直後の接続断を
「connection to server was lost」として終了コード 2 で報告する（通常の拒否
経路——`ReadyForQuery` まで到達してからのエラー——の終了コード 1 とは異なる）
ため、検証は `!status.success()` のみで行う（`run_psql_session_expect_sqlstate`
と同じ判定方針）。`DETAIL:` ラベル自体は libpq の gettext 翻訳対象になり
得るため `LC_ALL=C` を渡すが、`state=may_be_committed` という生値そのものは
翻訳対象ではないためこのガードで十分。

**psycopg の接続断吸収挙動**: psycopg 3.x の内部ジェネレータは
`ErrorResponse` を受け取った直後の接続断由来の例外を、既に受け取った
FATAL エラー結果の陰に隠して送出する。結果として `cur.execute()` は素直に
`XX000` の `InternalError`（`e.diag.message_detail` 込み）を送出し、
呼び出し側で接続断由来の二重例外を個別にハンドルする必要はない。

**node `pg` の追加防御**: `client.query()` が reject した後に接続断由来の
後追い `error` イベントを `pg.Client` が emit することがあり、リスナー未登録
だと Node プロセス全体が uncaught 例外で異常終了する。`client.on("error",
...)` を登録し stderr へログするだけに留め、成否判定は既存の `.catch()` に
一本化した（`Issue #706` で追加。`WIRE_SQL_PRELUDE` を使わない通常の拒否
経路テストの挙動・出力形式は変えていない）。

**`pg` の一時導入**: 本リポは `pg` を `package.json` として常設していないため
（Node 依存の管理方針は本 ADR のスコープ外）、実行確認はリポ外のスクラッチ
ディレクトリへ `npm install pg@8.23.0` した上で `NODE_PATH` 環境変数経由で
`pg_client.js` に解決させた（PR #708 と同じ一時導入方法）。

**実行記録**: `make e2e-three-client`（`three_client_e2e` 5 件・
`extended_syntax_e2e` 6 件、計 11 件）で本テストを含め全件成功することを
確認した。個別実行時の観測 `[e2e-record]` 出力（実行環境固有の文言・
接続情報は含まない）は PR 本文の Test plan に転記した。

### TASK-183／HTTP-13: NoSQL 表層の 3 クライアント統合ハーネス（Issue #776）

NoSQL 表層（`--surface nosql`。HTTP/1.1 自作リスナー・`/v1/session`／
`/v1/session/close`／`/v1/query`）は Issue #734〜#772 で production 結線まで
実装済みだが、無改造の外部 HTTP クライアントから実バイナリへ接続する層 B
統合テストは未整備だった。SQL 表層側の層 A/層 B 分割方針（本 ADR 冒頭「決定」
節）をそのまま踏襲し、以下の形で追加した。

- **層 A**（既存。`tests/http4_session_issue.rs`・`tests/http5_query_bearer.rs`・
  `tests/http8_session_close.rs`・`tests/nosql1_*`〜`tests/nosql11_*`）:
  in-process サーバースレッド・生 HTTP/1.1 バイトの自作クライアントによる
  常時回帰テスト。NoSQL 表層自体の契約（`op` 許可リスト・束縛規則・
  `operation_id` 必須化・precision fail-closed・RLS 暗黙適用等）はこちらが
  主として担う。
- **層 B**（新規。`tests/three_client_http_e2e.rs`）: 実 `wire-server --surface
  nosql` を子プロセスとして起動し、無改造の `curl` から `POST /v1/session`
  （発行）→ `POST /v1/query`（`op: search`）→ `POST /v1/session/close`
  （失効）→ 失効後の同一トークン再送が `401`／`wire_code 28000` で
  拒否されることまでを確認するスモークテスト。あわせて stderr に
  `--surface nosql` 起動時の告知行が含まれること（SQL 表層が誤って
  起動していないことの非 vacuous な証跡）も検証する。`#[ignore]` とし
  `make e2e-three-client-http` から明示的に実行する（`ci` には含めない）。

**起動・ポート取得**: `tests/common/mod.rs::SpawnedServer`（`Drop` ガード付き。
`tests/http4_session_issue.rs::spawned_binary_accepts_valid_login_over_nosql_surface`
の前例と同型）をそのまま再利用した。SQL 表層側の `three_client_e2e.rs` が持つ
`spawn_wire_server`／`ServerGuard` の第 3 コピーは作らない判断とした。

**seed の複製**: `three_client_e2e.rs::seed_three_tenant_db`（`docs` テーブル・
3 テナント × Public 1 行）は private 関数のため import できず、
`extended_syntax_e2e.rs` の前例と同じ方式で `three_client_http_e2e.rs` 専用に
複製した。3 行とも Public のため、wire 認証が導出する `PolicyContext`
（Public のみ）でも alice（tenant-a）から全 3 行が可視という既存オラクルを
そのまま流用できる。

**curl 起動の設計**: `Command` の引数配列で `curl` を起動しシェルを介さない。
要求 JSON は固定 `const`／リテラルで、未検証の文字列連結は行わない。
`Content-Type: application/json` を明示し、`Expect:` を空値で送って
100-continue の余地を消した。NoSQL 表層は `Connection: close` 固定のため
1 要求 = 1 curl プロセスとし `--next` 連結はしない。取得したセッション
トークンは形状検証（43 文字・base64url アルファベット限定）を経てから
`Authorization` ヘッダへ載せる（untrusted な外部プロセス応答をそのまま
信頼しない）。curl 未検出・非 0 終了・応答形状不一致はいずれも `panic!` で
失敗させ、silent skip はしない。

**本 Issue のスコープ**: ハーネス＋curl ランナー＋curl での
session→search→close→失効後再送（`401`／`28000`）スモーク 1 本＋
`make e2e-three-client-http` に限定した。urllib／fetch ランナー・
3 クライアント一連手順の実行記録の整備・psql（SQL 経路）との結果一致比較は
後続 Issue（#777〜#779）へ申し送る。

### urllib／fetch ランナー（Issue #777）

Issue #776 で curl のみだった `three_client_http_e2e.rs` へ、Python 標準
ライブラリ `urllib.request`・Node.js 組み込み `fetch` の 2 クライアントを
追加した。SQL 表層側の `tests/three_client/{psycopg_client.py,pg_client.js}`
と同じ配置・起動方式（`tests/three_client_http/{urllib_client.py,
fetch_client.js}`・`PYTHON_BIN`／`NODE_BIN` 環境変数でインタプリタを解決）を
踏襲し、外部パッケージ（pip／npm）には一切依存しない
（`.claude/rules/dependency-policy.md`）。

**入出力契約**: 3 クライアント共通で接続先・要求本文・bearer トークンは
すべて `HTTP_HOST`／`HTTP_PORT`／`HTTP_TARGET`／`HTTP_BODY`／`HTTP_BEARER`
環境変数経由（argv・stdin は使わない。security.md P0）。成功時は stdout
1 行目に HTTP ステータス（10 進数字のみ）、2 行目以降に応答本文をそのまま
出力し終了コード 0（4xx／5xx も「応答を受信できた」として扱う。失効後
トークン再送で `401` を確認するステップに必要な契約）。転送路・プロトコル
障害（接続不能・タイムアウト・応答本文の上限超過・UTF-8 デコード不正・
必須環境変数の欠落）はいずれも終了コード 1 とし、stderr には障害種別のみを
書く（要求本文・bearer 値・env の値は echo しない）。応答本文の上限は
curl 経路と同じ `2 MiB`（拒否閾値として）、タイムアウトは 10 秒に揃えた。
`urllib_client.py` は `read(LIMIT + 1)` で受信量そのものを上限に抑えるが、
`fetch_client.js` は `res.arrayBuffer()` で先に全量を読み切ってから長さを
検査する（Node 組み込み `fetch` の標準 API では読み取り量の事前制限が
できないため）。本テストの対象はいずれも自前サーバー（本文は高々数百
バイト）で上限超過は想定しない経路のため、ストリーミング読み取りの
複雑化は見送った。

**curl との差分**: `fetch` は既定でリダイレクトを追従するため
`redirect: "error"` を明示し、サーバーが 3xx を返さない契約
（`crates/wire-server/docs/nosql-api.md`）から外れた場合は fail-closed に
倒す（curl 側は追従しても到達しない想定でそのまま）。要求ヘッダは両者とも
`Content-Type: application/json` を明示する（urllib の既定
`application/x-www-form-urlencoded`・Node fetch の文字列本文既定
`text/plain;charset=UTF-8` のままだと `08P01` になるため）。

**Node.js ≥18 前提**: `fetch` は Node の組み込みグローバル
（`require` 不要）だが Node 18 未満には存在しない。`typeof fetch !==
"function"` を検査し、非搭載環境では案内メッセージ付きで終了コード 1 にする
（ローカル実行環境は Node v24 系で確認済み）。

**Rust ランナーの共有化**: `curl_post` と同じシグネチャの
`urllib_post`／`fetch_post` を追加し、共通の `spawn_script_client`
（`three_client_e2e.rs::spawn_psycopg_client` と同型。`Command` 引数配列で
起動しシェル非経由）へ委譲した。テスト本体は
`run_session_search_close_scenario(client: HttpClient)` へ抽出し、
`curl_runs_session_search_close_over_nosql_surface`（既存名を維持）・
`urllib_runs_session_search_close_over_nosql_surface`・
`fetch_runs_session_search_close_over_nosql_surface` の 3 本の
`#[test] #[ignore]` から呼ぶ。シナリオ内容（session 発行→トークン形状検証→
search 3 行→close→失効後再送→stderr 非漏えい検証）はクライアント種別に
依存しない。`make e2e-three-client-http` は 3 テストとも一括実行する。

production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
無変更（テスト・スクリプト専任）。

## 影響

- `crates/wire-server/src/{simple_query,result_encoder}.rs`（新規）・
  `handshake.rs`／`server.rs`／`main.rs`（拡張）により、簡易クエリが
  `engine::core::EngineCore` の SQL 表層へ到達する（TASK-73 本体）。
- `wire-server --db <path>` が必須化された（省略時は fail-closed で
  起動拒否。匿名・揮発 DB の暗黙生成はしない）。
- `Makefile` に `e2e-three-client`（opt-in・`ci` には含めない）を追加した。
- `Makefile` に `test-default-build`（`ci` に含む）・
  `.github/workflows/ci.yml` に同名の独立ジョブを追加し、既定ビルド拒否
  テストを常時検査する経路を整備した（Issue #715・#716・PR #718）。
- `crates/wire-server/Cargo.toml` に `fault-injection` feature（default 外）
  を追加し、`e2e-three-client` の `three_client_e2e` 行はこの feature 付きで
  ビルドするよう変更した（Issue #705）。
- `three_client_e2e.rs::spawn_wire_server` が `extra_args: &[String]` を
  受け取れるよう拡張され（`extended_syntax_e2e.rs` と同型）、`ServerGuard`
  が起動時 stderr の全行と `wait_for_exit` を保持するようになった。
  `tests/three_client/{psycopg_client.py,pg_client.js}` は失敗時に
  `[DETAIL=<detail>]` を stderr へ追記する（いずれも Issue #706）。

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
