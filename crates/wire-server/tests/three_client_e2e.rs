//! 無改造の実クライアント 3 種（`psql`／Python `psycopg`／Node.js `pg`）から
//! `wire-server` バイナリへ実接続し、C1〜C4（定義は TASK-73／ビヘイビア
//! WIRE-1、`crates/engine/src/sql/parser.rs` 参照）の実行・誤りパスワードの
//! 拒否を検証する層 B の統合テスト（codex-review P2 指摘・PR #210: 各ドライバ
//! での挙動差異を独立オラクルと照合し保証する）。
//!
//! 責務境界: 層 A（`tests/wire1_simple_query.rs`）が生バイトの wire クライアント
//! で常時（`make ci`）回帰保護する契約と同じバイト列を、実クライアント経由で
//! 追加検証する。ローカル・Docker 開発コンテナには `psql`／`psycopg`／`pg` が
//! 導入されていないため `#[ignore]` とし、`make e2e-three-client`
//! （`cargo test -p fandhe-vector-db-wire-server --test three_client_e2e -- --ignored`）から
//! 明示的に実行する（CI の必須チェックには含めない。psql・psycopg・pg の並
//! 導入をローカル環境へ強制すると `make ci` 自体が壊れるため。ADR:
//! `docs/design/three-client-e2e-harness.md`）。
//!
//! TASK-165（SQL-12／SEARCH-9）: `USING MODE`／`SET search_mode` の優先順位・
//! 確信度ゲートは層 A（`tests/wire_search_mode.rs`、常時 `make ci`）が主たる
//! 回帰保護を担う。本ファイルは `run_*_session` 系ヘルパー（`WIRE_SQL_PRELUDE`
//! で同一接続に複数文を送る）を使い、無改造クライアント経由でも同じ契約を
//! 最小限確認する（3 クライアントの子プロセス実行は本環境未導入のため
//! コンパイル通過とスクリプト構文確認のみで検証済み。詳細は PR 本文）。
//!
//! TASK-168（SQL-13／SQL-14）: 集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`）・
//! `GROUP BY`/`HAVING` の拒否形状・NULL 契約の回帰保護は層 A
//! （`tests/wire_aggregate.rs`、常時 `make ci`）が主として担う。本ファイルは
//! `seed_aggregate_three_tenant_db` の同一コーパスに対する代表ケース（単一行
//! 集計・`GROUP BY`/`HAVING`・RLS 不変・拒否経路 2 種）のみを 3 クライアント
//! 経由で確認する（3 クライアントの子プロセス実行は本環境未導入のため層 A・
//! 層 B いずれもコンパイル通過とスクリプト構文確認のみで検証済み。詳細は
//! PR 本文）。
//!
//! ツール未検出・クライアントスクリプトの非 0 終了はいずれも `panic!` で
//! 失敗させ、silent skip はしない（`.claude/rules/coding-rust.md`・実行規約
//! 「テストの skip・ignore・アサーション弱体化で CI を通さない」の精神を、
//! 明示的に選択実行するこの導線でも維持する）。
//!
//! TASK-187（SQL-11）: `docs` 以外の任意テーブル（`kb_articles`）でも C1 相当の
//! SELECT・`INSERT ... USING OPERATION_ID` が `docs` と同じ契約（成否・
//! `wire_code`・RLS 暗黙適用）で通ることを 3 クライアント経由で確認する。
//! 評価順序・台帳スコープ・複数次元共存・`42P01` は engine 側
//! `crates/engine/tests/arbitrary_table.rs`（TASK-81・SQL-11 確定化の根拠）が
//! 既に機械検証済みのため、本ファイルでは重複網羅しない。
//!
//! TASK-97・TASK-153・ERR-5（Issue #706）: commit 成功境界を跨いだ panic 時の
//! 緊急応答（`S`=`ERROR`・`C`=`XX000`・`D`=`state=may_be_committed`）の
//! バイト列契約そのものは層 A（`tests/wire_emergency_response.rs`）が固定し、
//! テスト専用注入フラグ `--fault-inject post-commit-panic`（feature
//! `fault-injection`・Issue #705）の CLI 受理・発火・abort は層 A
//! （`tests/wire_fault_injection_cli.rs`）が検証済み。本ファイルはその先
//! ——無改造の実クライアント 3 種が、自身のドライバ API から実際に `detail`
//! 値へ到達できるか（psql の `DETAIL:` 行、psycopg の
//! `e.diag.message_detail`、node `pg` の `err.detail`）——を検証する
//! （`three_clients_receive_emergency_response_detail_after_post_commit_panic`）。
//! `--fault-inject` は 1 プロセスにつき 1 回しか発火しない take-once 契約
//! （Issue #705）のため、クライアントごとに独立したサーバー・DB を起動する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

/// 環境変数（`PSQL_BIN`/`PYTHON_BIN`/`NODE_BIN`）で上書きできるツール解決。
/// 未指定時は `PATH` 上のデフォルト名を使う。ツール自体の存在確認はしない
/// （`Command::spawn` の失敗として顕在化させ、呼び出し元が案内メッセージ付きで
/// panic する）。
fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

/// `wire-server` バイナリを子プロセスとして起動し、stderr の `listening on
/// 127.0.0.1:<port>` 行から実際に bind されたポートを取得する。
/// 呼び出し元が `Drop` 相当で必ず kill する（[`ServerGuard`]）。
///
/// `startup_lines` は listen 行到達までに観測した stderr の全行（トリム済み）
/// を保持する（Issue #706。`--fault-inject`〔Issue #705〕を渡して起動する
/// 場合の `fault injection armed` 行の観測に使う。`ServerGuard` の生存中に
/// 子プロセスが自発的に終了することがある（Issue #706 の commit 後 panic
/// 注入。`Drop` の `kill`／`wait` は既終了プロセスに対しても安全に no-op と
/// なる）ため、`wait_for_exit` で明示的に終了を待ち受けられるようにする。
struct ServerGuard {
    child: Child,
    port: u16,
    startup_lines: Vec<String>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ServerGuard {
    /// 子プロセスの終了（緊急応答送出後の `fail_fast` による abort を含む。
    /// TASK-97・RECOVER-6・TASK-99・RECOVER-8）を `timeout` まで待ち受ける
    /// （`crates/wire-server/tests/wire_fault_injection_cli.rs::wait_for_exit`
    /// と同型）。超過した場合は kill してから panic する。
    fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if start.elapsed() > timeout {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("subprocess did not terminate within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// `extra_args`（`extended_syntax_e2e.rs::spawn_wire_server` と同型）で
/// `--fault-inject post-commit-panic`（Issue #705・#706）等の追加 CLI を
/// まとめて渡せるようにする。
fn spawn_wire_server(users_path: &Path, db_path: &Path, extra_args: &[String]) -> ServerGuard {
    let mut args: Vec<String> = vec![
        "--users".into(),
        users_path.to_str().expect("utf-8 path").into(),
        "--db".into(),
        db_path.to_str().expect("utf-8 path").into(),
        "--bind".into(),
        "127.0.0.1:0".into(),
    ];
    args.extend_from_slice(extra_args);

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server binary (built by `cargo test`)");

    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 || tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut port: Option<u16> = None;
    let mut startup_lines: Vec<String> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let trimmed = line.trim().to_string();
                if let Some(addr_str) = trimmed.strip_prefix("wire-server: listening on ") {
                    if let Ok(addr) = addr_str.parse::<std::net::SocketAddr>() {
                        port = Some(addr.port());
                        startup_lines.push(trimmed);
                        break;
                    }
                }
                startup_lines.push(trimmed);
            }
            Err(_) => break,
        }
    }

    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "wire-server did not report a listening port within the deadline \
             (run via `make e2e-three-client` which builds with `--features \
             fault-injection` when `--fault-inject` is passed); lines so far: \
             {startup_lines:?}"
        );
    };

    ServerGuard {
        child,
        port,
        startup_lines,
    }
}

/// 3 テナント（alice/bob/carol）に Public 行 1 件ずつを投入した `docs`
/// テーブルを持つ一時 DB を用意する（層 A の
/// `wire1_three_tenant_visibility_public_shared_private_hidden` と同じ seed
/// 方針。可視性の非対称は同テストのドキュメンテーションコメント参照）。
/// C1〜C4（TASK-73／WIRE-1）すべてを同じ 3 行のコーパスで検証できるよう
/// 列を構成する（codex-review P2 指摘・PR #210）。
fn seed_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let tenants: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in tenants {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    (path, guard)
}

/// `docs` とは別名の任意テーブル（`kb_articles`）に `seed_three_tenant_db` と
/// 同じ Public 3 行を投入したうえで、tenant-a の Private 行（id=11,
/// lang="xx"）も追加した一時 DB を用意する（TASK-187・SQL-11）。`docs` と
/// 別名のテーブルでも wire 経由の C1 相当・RLS 暗黙適用が同一契約で成立する
/// ことを、Private 行の非漏洩という非自明な形で検証するための seed
/// （engine 側 `crates/engine/tests/arbitrary_table.rs` の対照検証と同じ
/// `kb_articles` という命名を踏襲する）。
fn seed_arbitrary_table_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-kb-articles");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "kb_articles",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let public_rows: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in public_rows {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "kb_articles",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    // TASK-101（RECOVER-10）: 上の public_rows ループで tenant-a が既に
    // "test-op" を使用しているため、別内容の再利用は OperationIdContentMismatch
    // になる。Private 行専用の別 operation_id を使う。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "kb_articles",
        &ctx,
        11,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("xx".to_string()),
            Value::Text("private body".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op-private-kb")
            .expect("valid operation_id"),
    )
    .expect("insert private row");
    (path, guard)
}

/// `seed_three_tenant_db` と同じ Public 3 行（`embedding`/`lang`/`body`）に加え、
/// tenant-a の Private 行（id=11, lang="xx"）・tenant-b の Private 行
/// （id=12, lang="ja"）を投入した一時 DB を用意する（TASK-168・SQL-13/14）。
/// wire 認証経路の `PolicyContext` は Public のみ許可のため、Private 行は
/// どのユーザーの接続からも不可視。既存 C1〜C4 テストの seed
/// （`seed_three_tenant_db`）はこの関数の追加では変更しない。
fn seed_aggregate_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-aggregate-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let public_rows: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in public_rows {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    let private_rows: [(&str, u64, [f32; 2], &str); 2] = [
        ("tenant-a", 11, [1.0, 0.0], "xx"),
        ("tenant-b", 12, [0.0, 1.0], "ja"),
    ];
    for (tenant, id, dir, lang) in private_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        // TASK-101（RECOVER-10）: 上の public_rows ループで同一テナントが既に
        // "test-op" を使用しているため、別内容の再利用は OperationIdContentMismatch
        // になる。別の operation_id を使う。
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text("private body".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op-private")
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }
    (path, guard)
}

fn write_users_file(path: &Path) {
    use wire_server::auth::argon2id;
    let salt = b"0123456789abcdef";
    let mut content = String::new();
    for (user, tenant, pw) in [
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ] {
        let phc = argon2id::encode_phc(pw.as_bytes(), salt, &argon2id::RECOMMENDED_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{user}:{tenant}:{phc}\n"));
    }
    std::fs::write(path, content).expect("write users file");
}

/// C1（TASK-73／WIRE-1）。
const C1_SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
/// C2（TASK-73／WIRE-1）。各ドライバでの型変換も合わせて検証する。
const C2_SQL: &str =
    "SELECT id, lang FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
/// C3（TASK-73／WIRE-1。`crates/engine/tests/sql_surface.rs`
/// `sql3_rls_is_enforced_regardless_of_visible_predicate_presence` と同じ契約）。
const C3_SQL: &str =
    "SELECT id FROM docs WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
/// C4（TASK-73／WIRE-1。`crates/engine/tests/sql_surface.rs`
/// `sql4_hybrid_degrades_to_dense_only_when_no_visible_body_text` と同じ契約）。
const C4_SQL: &str = "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'zzz-term-absent-from-any-seed-body') LIMIT 3";

/// TASK-187（SQL-11）: `docs` 以外の任意テーブル（`kb_articles`）での C1 相当。
/// `LIMIT` を `docs` 版（3）より大きい 5 にし、Public 行（3 件）しか無い
/// コーパスで万一 RLS が破綻し Private 行（id=11）が漏れても `LIMIT` で
/// 隠れず必ず観測できる形にする（非 vacuous な RLS チェック）。
const C1_SQL_ARBITRARY_TABLE: &str =
    "SELECT id FROM kb_articles ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5";

/// psql（無改造）で任意の SQL を実行し、返却された各行を `|` 区切りで結合した
/// 文字列の集合として返す（単一列なら値そのもの）。`-F '|'` で区切り文字を
/// 明示指定し（`-X` で `~/.psqlrc` 経由の `\pset fieldsep` 上書きも遮断する
/// ため、環境差異で暗黙に変わらない）、`run_psycopg`／`run_pg` 側も同じ区切りで
/// 出力を揃える。
fn run_psql(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    run_psql_session(port, user, password, &[], sql)
}

/// psql（無改造）で `prelude` の各文を先に実行してから `sql` を実行し、`sql` の
/// 結果行を `run_psql` と同じ `|` 区切りの集合として返す（TASK-165・SQL-12。
/// 同一接続で `SET search_mode = ...` を先行実行してから `SELECT` を送る、
/// セッション複数文の検証に使う）。`-q`（quiet）で prelude の `SET` タグが
/// stdout の結果集合へ混入するのを防ぎ、複数の `-c` は psql が同一セッションで
/// 順次送信する（`run_psql` は本関数の prelude 無しの薄いラッパー）。
fn run_psql_session(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> Vec<String> {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let mut args: Vec<String> = vec![
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        port.to_string(),
        "-U".into(),
        user.into(),
        "-d".into(),
        "irrelevant-db-name".into(),
        "-X".into(),
        "-w".into(),
        "-q".into(),
        "-At".into(),
        "-F".into(),
        "|".into(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
    ];
    for stmt in prelude {
        args.push("-c".into());
        args.push((*stmt).into());
    }
    args.push("-c".into());
    args.push(sql.into());

    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args(&args)
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {psql} (install libpq-client tools or set PSQL_BIN): {e}")
        });
    assert!(
        output.status.success(),
        "psql exited non-zero for user {user}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// psql で `prelude` を先行実行後、最終文が非 0 終了かつ stderr に期待
/// SQLSTATE を含めて拒否されることを確認する（TASK-165 の拒否経路検証。
/// `-v VERBOSITY=verbose` で SQLSTATE を stderr へ出させる）。
fn run_psql_session_expect_sqlstate(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
    expected_sqlstate: &str,
) {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let mut args: Vec<String> = vec![
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        port.to_string(),
        "-U".into(),
        user.into(),
        "-d".into(),
        "irrelevant-db-name".into(),
        "-X".into(),
        "-w".into(),
        "-q".into(),
        "-At".into(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
        "-v".into(),
        "VERBOSITY=verbose".into(),
    ];
    for stmt in prelude {
        args.push("-c".into());
        args.push((*stmt).into());
    }
    args.push("-c".into());
    args.push(sql.into());

    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args(&args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        !output.status.success(),
        "psql must exit non-zero for a rejected statement (user {user})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate} in psql stderr, got: {stderr}"
    );
}

/// psql で誤りパスワードを送り、非 0 終了・`28P01`／認証失敗の文言が出ることを
/// 確認する。
fn run_psql_wrong_password(port: u16, user: &str) {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .env("PGPASSWORD", "definitely-not-the-password")
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            user,
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-At",
            "-c",
            "SELECT 1",
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        !output.status.success(),
        "psql must exit non-zero on wrong password"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("password") || stderr.contains("28P01"),
        "expected password-authentication failure text, got: {stderr}"
    );
}

/// Python `psycopg`（無改造）で任意の SQL を実行し、各行を `|` 区切りで
/// 結合した文字列の集合を返す（`run_psql` と同じ区切り規約。複数列を返す
/// C2 の型変換検証に対応する）。
fn run_psycopg(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    run_psycopg_session(port, user, password, &[], sql)
}

/// `psycopg_client.py` に `WIRE_SQL_PRELUDE`（JSON 配列）を渡し、`prelude` の
/// 各文を同一接続で先行実行してから `sql` を実行する（TASK-165・SQL-12。
/// `run_psycopg` は本関数の prelude 無しの薄いラッパー）。
fn run_psycopg_session(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> Vec<String> {
    let output = spawn_psycopg_client(port, user, password, prelude, sql);
    assert!(
        output.status.success(),
        "psycopg_client.py failed for user {user} (install psycopg via \
         `pip install psycopg[binary]` or set PYTHON_BIN): stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// psycopg で `prelude` を先行実行後、最終文が非 0 終了かつ stderr に期待
/// SQLSTATE（`psycopg_client.py` の `[SQLSTATE=<code>]` 表記）を含めて拒否
/// されることを確認する（TASK-165 の拒否経路検証）。
fn run_psycopg_session_expect_sqlstate(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
    expected_sqlstate: &str,
) {
    let output = spawn_psycopg_client(port, user, password, prelude, sql);
    assert!(
        !output.status.success(),
        "psycopg_client.py must exit non-zero for a rejected statement (user {user})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate} in psycopg_client.py stderr, got: {stderr}"
    );
}

fn spawn_psycopg_client(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> std::process::Output {
    let python = resolve_tool("PYTHON_BIN", "python3");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/psycopg_client.py");
    let mut cmd = Command::new(&python);
    cmd.arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql);
    if !prelude.is_empty() {
        let prelude_json =
            serde_json_prelude(prelude).expect("prelude statements must encode as a JSON array");
        cmd.env("WIRE_SQL_PRELUDE", prelude_json);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn {python}: {e}"))
}

/// `["a","b"]` 形式の最小 JSON エンコーダ（依存追加なしで `WIRE_SQL_PRELUDE` を
/// 組み立てる。`prelude` は本ファイル内の定数リテラルのみを渡す前提で、
/// 制御文字・バックスラッシュを含まない SQL 文だけを扱う。`\`・制御文字を
/// 含む文字列を渡した場合は panic して不正なエンコードを未然に防ぐ）。
fn serde_json_prelude(statements: &[&str]) -> Option<String> {
    let mut out = String::from("[");
    for (i, stmt) in statements.iter().enumerate() {
        if stmt.contains('\\') || stmt.chars().any(|c| c.is_control()) {
            return None;
        }
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&stmt.replace('"', "\\\""));
        out.push('"');
    }
    out.push(']');
    Some(out)
}

/// Node.js `pg`（無改造）で任意の SQL を実行し、各行を `|` 区切りで結合
/// した文字列の集合を返す（`run_psql` と同じ区切り規約）。
fn run_pg(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    run_pg_session(port, user, password, &[], sql)
}

/// `pg_client.js` に `WIRE_SQL_PRELUDE`（JSON 配列）を渡し、`prelude` の各文を
/// 同一接続で先行実行してから `sql` を実行する（TASK-165・SQL-12。`run_pg` は
/// 本関数の prelude 無しの薄いラッパー）。
fn run_pg_session(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> Vec<String> {
    let output = spawn_pg_client(port, user, password, prelude, sql);
    assert!(
        output.status.success(),
        "pg_client.js failed for user {user} (install pg via \
         `npm install pg` or set NODE_BIN): stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// pg で `prelude` を先行実行後、最終文が非 0 終了かつ stderr に期待
/// SQLSTATE（`pg_client.js` の `[SQLSTATE=<code>]` 表記）を含めて拒否される
/// ことを確認する（TASK-165 の拒否経路検証）。
fn run_pg_session_expect_sqlstate(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
    expected_sqlstate: &str,
) {
    let output = spawn_pg_client(port, user, password, prelude, sql);
    assert!(
        !output.status.success(),
        "pg_client.js must exit non-zero for a rejected statement (user {user})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate} in pg_client.js stderr, got: {stderr}"
    );
}

fn spawn_pg_client(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> std::process::Output {
    let node = resolve_tool("NODE_BIN", "node");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_client.js");
    let mut cmd = Command::new(&node);
    cmd.arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql);
    if !prelude.is_empty() {
        let prelude_json =
            serde_json_prelude(prelude).expect("prelude statements must encode as a JSON array");
        cmd.env("WIRE_SQL_PRELUDE", prelude_json);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn {node}: {e}"))
}

/// 3 クライアント（psql / psycopg / pg）それぞれで、3 テナントいずれの
/// ユーザーで接続しても C1〜C4（TASK-73／WIRE-1）の結果が独立オラクルと一致
/// すること・誤りパスワードが拒否されることを検証する（可視性契約は層 A の
/// `wire1_three_tenant_visibility_public_shared_private_hidden` と同じ。
/// codex-review P2 指摘・PR #210）。ツール未導入・スクリプト失敗は silent
/// skip せず `panic!` で失敗させる（本ファイル先頭のドキュメンテーション
/// コメント参照）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_c1_through_c4_and_reject_wrong_password() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    // 独立オラクル（TASK-73／WIRE-1。各定数のドキュメンテーションコメント
    // 参照）。
    let expected_c1 = vec!["1".to_string(), "2".to_string(), "3".to_string()];
    let expected_c2 = vec!["1|ja".to_string(), "3|ja".to_string()];
    let expected_c3 = expected_c1.clone();
    let expected_c4 = expected_c1.clone();

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        for (label, sql, expected) in [
            ("C1", C1_SQL, &expected_c1),
            ("C2", C2_SQL, &expected_c2),
            ("C3", C3_SQL, &expected_c3),
            ("C4", C4_SQL, &expected_c4),
        ] {
            let psql_rows = run_psql(port, user, pw, sql);
            assert_eq!(
                &psql_rows, expected,
                "psql: unexpected {label} result for user {user}"
            );

            let psycopg_rows = run_psycopg(port, user, pw, sql);
            assert_eq!(
                &psycopg_rows, expected,
                "psycopg: unexpected {label} result for user {user}"
            );

            let pg_rows = run_pg(port, user, pw, sql);
            assert_eq!(
                &pg_rows, expected,
                "pg: unexpected {label} result for user {user}"
            );
        }
    }

    run_psql_wrong_password(port, "alice");

    drop(server);
    let _ = std::io::stdout().flush();
}

/// TASK-165（SQL-12／SEARCH-9）: `USING MODE` 句・`SET search_mode` セッション
/// 変数・未知モード値の拒否を無改造クライアント経由で最小限確認する。閾値
/// そのものの回帰保護は層 A（`tests/wire_search_mode.rs`、常時 `make ci`）が
/// 担うため、ここでは同じオラクル（`seed_three_tenant_db` の
/// `[1,0]`／`[0,1]`／`[-1,0]` コーパス）に対する代表ケースのみを 3 クライアントで
/// 確認する（M2: クエリ句 precision が Top-1 のみを返す／M5: `SET
/// search_mode='precision'` が後続の句なし SELECT に適用される／R1: クエリ句の
/// 未知モード値が拒否される）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_verify_search_mode_switch_and_precision_contract() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-search-mode-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    const PRECISION_CLAUSE_SQL: &str =
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'precision'";
    const UNKNOWN_MODE_SQL: &str =
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'fuzzy'";
    let expected_top1_only = vec!["1".to_string()];

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        // M2: クエリ句 precision（明確な勝者 → Top-1 のみ）。
        assert_eq!(
            run_psql_session(port, user, pw, &[], PRECISION_CLAUSE_SQL),
            expected_top1_only,
            "psql: USING MODE 'precision' must return only id=1 for user {user}"
        );
        assert_eq!(
            run_psycopg_session(port, user, pw, &[], PRECISION_CLAUSE_SQL),
            expected_top1_only,
            "psycopg: USING MODE 'precision' must return only id=1 for user {user}"
        );
        assert_eq!(
            run_pg_session(port, user, pw, &[], PRECISION_CLAUSE_SQL),
            expected_top1_only,
            "pg: USING MODE 'precision' must return only id=1 for user {user}"
        );

        // M5: SET search_mode='precision' → 句なし SELECT が Top-1 のみ返る
        // （セッション複数文の同一接続内適用。`WIRE_SQL_PRELUDE`／複数 `-c` 経由）。
        let prelude = ["SET search_mode = 'precision'"];
        assert_eq!(
            run_psql_session(port, user, pw, &prelude, C1_SQL),
            expected_top1_only,
            "psql: SET search_mode='precision' must apply to the subsequent SELECT for user {user}"
        );
        assert_eq!(
            run_psycopg_session(port, user, pw, &prelude, C1_SQL),
            expected_top1_only,
            "psycopg: SET search_mode='precision' must apply to the subsequent SELECT for user {user}"
        );
        assert_eq!(
            run_pg_session(port, user, pw, &prelude, C1_SQL),
            expected_top1_only,
            "pg: SET search_mode='precision' must apply to the subsequent SELECT for user {user}"
        );

        // R1: クエリ句の未知モード値は 22000 で拒否される。
        run_psql_session_expect_sqlstate(port, user, pw, &[], UNKNOWN_MODE_SQL, "22000");
        run_psycopg_session_expect_sqlstate(port, user, pw, &[], UNKNOWN_MODE_SQL, "22000");
        run_pg_session_expect_sqlstate(port, user, pw, &[], UNKNOWN_MODE_SQL, "22000");
    }

    drop(server);
    let _ = std::io::stdout().flush();
}

/// TASK-168（SQL-13／SQL-14）: 集計クエリ（単一行の `COUNT`/`SUM`/`AVG`/`MIN`/
/// `MAX`・`GROUP BY`/`HAVING`）と RLS 不変性の代表ケースを無改造クライアント
/// 経由で確認する。閾値・拒否形状そのものの回帰保護は層 A
/// （`tests/wire_aggregate.rs`、常時 `make ci`）が担う。
///
/// すべての SELECT に一意の `AS` 別名を付け、NULL を返す SQL は使わない
/// （Node `pg` は `Object.values(row)` で行を出力するため同名列が潰れ、NULL の
/// 描画も psql（空文字）／pg（`Array.join` で空文字）／psycopg（`str(None)`=
/// "None"）で異なる。NULL 契約の検証は層 A に閉じる）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_verify_aggregate_queries_and_rls_invariance() {
    let (db_path, _db_guard) = seed_aggregate_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-aggregate-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    // 独立オラクル（可視行は id=1/2/3 の Public 3 行のみ。Private 行
    // （lang="xx"）は wire 認証経路では不可視。`seed_aggregate_three_tenant_db`
    // のドキュメンテーションコメント参照）。
    const AGG1_SQL: &str = "SELECT COUNT(*) AS n, SUM(id) AS s, AVG(id) AS a, MIN(lang) AS l_min, MAX(lang) AS l_max FROM docs";
    let expected_agg1 = vec!["3|6|2|en|ja".to_string()];

    const AGG2_SQL: &str = "SELECT COUNT(*) AS n FROM docs WHERE lang = 'ja'";
    let expected_agg2 = vec!["2".to_string()];

    const AGG3_SQL: &str = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY n DESC";
    let expected_agg3 = vec!["ja|2".to_string(), "en|1".to_string()];

    const AGG4_SQL: &str = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n >= 2";
    let expected_agg4 = vec!["ja|2".to_string()];

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        for (label, sql, expected) in [
            ("AGG1", AGG1_SQL, &expected_agg1),
            ("AGG2", AGG2_SQL, &expected_agg2),
            ("AGG3", AGG3_SQL, &expected_agg3),
            ("AGG4", AGG4_SQL, &expected_agg4),
        ] {
            let psql_rows = run_psql(port, user, pw, sql);
            assert_eq!(
                &psql_rows, expected,
                "psql: unexpected {label} result for user {user} (must not reveal the \
                 Private-only \"xx\" group of another tenant)"
            );

            let psycopg_rows = run_psycopg(port, user, pw, sql);
            assert_eq!(
                &psycopg_rows, expected,
                "psycopg: unexpected {label} result for user {user}"
            );

            let pg_rows = run_pg(port, user, pw, sql);
            assert_eq!(
                &pg_rows, expected,
                "pg: unexpected {label} result for user {user}"
            );
        }

        // 拒否経路: 型不整合（VECTOR 列への SUM）・許可形状外
        // （集計と裸の列の混在）はいずれも接続を破棄せず拒否される。
        const REJECT_TYPE_MISMATCH_SQL: &str = "SELECT SUM(embedding) FROM docs";
        run_psql_session_expect_sqlstate(port, user, pw, &[], REJECT_TYPE_MISMATCH_SQL, "22000");
        run_psycopg_session_expect_sqlstate(port, user, pw, &[], REJECT_TYPE_MISMATCH_SQL, "22000");
        run_pg_session_expect_sqlstate(port, user, pw, &[], REJECT_TYPE_MISMATCH_SQL, "22000");

        const REJECT_MIXED_SHAPE_SQL: &str = "SELECT COUNT(*), lang FROM docs";
        run_psql_session_expect_sqlstate(port, user, pw, &[], REJECT_MIXED_SHAPE_SQL, "42601");
        run_psycopg_session_expect_sqlstate(port, user, pw, &[], REJECT_MIXED_SHAPE_SQL, "42601");
        run_pg_session_expect_sqlstate(port, user, pw, &[], REJECT_MIXED_SHAPE_SQL, "42601");
    }

    drop(server);
    let _ = std::io::stdout().flush();
}

/// TASK-187（SQL-11）: `docs` とは別名の任意テーブル（`kb_articles`）でも、
/// wire 経由・3 クライアントで C1 相当 SELECT と `INSERT ... USING
/// OPERATION_ID` が `docs` と同じ契約（成否・RLS 暗黙適用）で通ることを
/// 確認する。評価順序・台帳スコープ・複数次元共存・`42P01` は engine 側
/// `crates/engine/tests/arbitrary_table.rs`（TASK-81）が既に機械検証済みの
/// ため、本テストは wire 経由の C1 相当 SELECT・`INSERT` の成否契約確認に
/// 限定する。TASK-82／`docs/design/three-client-e2e-harness.md` の既定契約
/// （wire 認証経路の `PolicyContext` は Public のみ・書いた本人も同一 wire
/// セッションでは読み戻せない非対称）はここでも維持されるため、`INSERT`
/// 後に C1 を再実行して可視性を確認することはしない（`docs` の
/// `three_clients_run_insert_with_operation_id` 相当と同じく「成功したこと」
/// のみを確認する）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_c1_and_insert_on_arbitrary_table() {
    let (db_path, _db_guard) = seed_arbitrary_table_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-arbitrary-table-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    // 独立オラクル（Public 3 行のみ。tenant-a の Private 行 id=11 は wire
    // 認証経路では不可視のため、現れれば RLS 暗黙適用の破綻として検出される）。
    let expected_c1 = vec!["1".to_string(), "2".to_string(), "3".to_string()];

    let insert_sql = |id: u64, op: &str| -> String {
        format!(
            "INSERT INTO kb_articles (id, embedding, lang, body) VALUES \
             ({id}, '[0.5,0.5]', 'en', 'inserted via three-client e2e') \
             USING OPERATION_ID '{op}'"
        )
    };

    // tenant ごとに id・operation_id のブロックを分け、台帳（TASK-93・
    // RECOVER-2）の内容照合ハッシュ（TASK-101・RECOVER-10）が誤って
    // 別テナント・別クライアントの再送と衝突判定しないようにする。
    for (i, (user, pw)) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ]
    .into_iter()
    .enumerate()
    {
        // C1 相当 SELECT。
        assert_eq!(
            run_psql(port, user, pw, C1_SQL_ARBITRARY_TABLE),
            expected_c1,
            "psql: unexpected C1 result on kb_articles for user {user}"
        );
        assert_eq!(
            run_psycopg(port, user, pw, C1_SQL_ARBITRARY_TABLE),
            expected_c1,
            "psycopg: unexpected C1 result on kb_articles for user {user}"
        );
        assert_eq!(
            run_pg(port, user, pw, C1_SQL_ARBITRARY_TABLE),
            expected_c1,
            "pg: unexpected C1 result on kb_articles for user {user}"
        );

        // INSERT ... USING OPERATION_ID（3 クライアントとも成功のみ確認）。
        let base_id: u64 = 200 + (i as u64) * 10;
        run_psql_session(
            port,
            user,
            pw,
            &[],
            &insert_sql(base_id, &format!("arbitrary-table-e2e-insert-psql-{user}")),
        );
        run_psycopg_session(
            port,
            user,
            pw,
            &[],
            &insert_sql(
                base_id + 1,
                &format!("arbitrary-table-e2e-insert-psycopg-{user}"),
            ),
        );
        run_pg_session(
            port,
            user,
            pw,
            &[],
            &insert_sql(
                base_id + 2,
                &format!("arbitrary-table-e2e-insert-pg-{user}"),
            ),
        );
    }

    drop(server);
    let _ = std::io::stdout().flush();
}
// -----------------------------------------------------------------------
// Issue #706: 緊急応答（TASK-97・TASK-153・ERR-5）の 3 クライアント detail
// 到達検証。
// -----------------------------------------------------------------------

/// psql（無改造）で `sql` を送り、commit 後 panic の緊急応答（TASK-97・
/// TASK-153・ERR-5）を検証する。psql は `ErrorResponse` 受信直後の接続断を
/// 「connection to server was lost」として終了コード 2 で報告するため
/// （通常の拒否経路の終了コード 1 とは異なる）、`run_psql_session_expect_sqlstate`
/// と同じ `!success()` のみで判定する。`-v VERBOSITY=verbose` で `DETAIL:`
/// 行を出力させ、`LC_ALL=C` で libpq の gettext 翻訳によるラベル文言差を
/// 避ける（`DETAIL:` 自体は翻訳されうるが、`state=may_be_committed` の
/// 生値は翻訳対象外）。観測した stderr 全文を返す（実行記録用）。
fn run_psql_expect_emergency(port: u16, user: &str, password: &str, sql: &str) -> String {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .env("LC_ALL", "C")
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            user,
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-q",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            "VERBOSITY=verbose",
            "-c",
            sql,
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        !output.status.success(),
        "psql must exit non-zero after the emergency response / connection loss"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("XX000"),
        "expected SQLSTATE XX000 in psql stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("state=may_be_committed"),
        "expected ERR-5 detail 'state=may_be_committed' in psql stderr, got: {stderr}"
    );
    stderr
}

/// psycopg（無改造）で `sql` を送り、commit 後 panic の緊急応答を検証する
/// （`psycopg_client.py` が `e.diag.message_detail` を `[DETAIL=...]` として
/// stderr へ出力する。Issue #706 で追加）。
fn run_psycopg_expect_emergency(port: u16, user: &str, password: &str, sql: &str) -> String {
    let output = spawn_psycopg_client(port, user, password, &[], sql);
    assert!(
        !output.status.success(),
        "psycopg_client.py must exit non-zero for the emergency response"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("[SQLSTATE=XX000]"),
        "expected [SQLSTATE=XX000] in psycopg_client.py stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("[DETAIL=state=may_be_committed]"),
        "expected ERR-5 [DETAIL=state=may_be_committed] in psycopg_client.py stderr, got: {stderr}"
    );
    stderr
}

/// node `pg`（無改造）で `sql` を送り、commit 後 panic の緊急応答を検証する
/// （`pg_client.js` が `err.detail` を `[DETAIL=...]` として stderr へ出力する。
/// Issue #706 で追加）。
fn run_pg_expect_emergency(port: u16, user: &str, password: &str, sql: &str) -> String {
    let output = spawn_pg_client(port, user, password, &[], sql);
    assert!(
        !output.status.success(),
        "pg_client.js must exit non-zero for the emergency response"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("[SQLSTATE=XX000]"),
        "expected [SQLSTATE=XX000] in pg_client.js stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("[DETAIL=state=may_be_committed]"),
        "expected ERR-5 [DETAIL=state=may_be_committed] in pg_client.js stderr, got: {stderr}"
    );
    stderr
}

/// commit 成功境界を跨いだ panic（TASK-97・RECOVER-6）の緊急応答が、無改造の
/// psql／psycopg／node `pg` それぞれのドライバ API から観測できる `detail`
/// フィールド（ERR-5・`state=may_be_committed`）として実際に到達することを
/// 検証する（Issue #706）。`--fault-inject post-commit-panic`（Issue #705）は
/// 1 プロセスにつき 1 回のみ発火する take-once 契約のため、クライアントごとに
/// 独立したサーバー・DB・テナントを用意する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg, and a `--features fault-injection` \
            build; run via `make e2e-three-client`"]
fn three_clients_receive_emergency_response_detail_after_post_commit_panic() {
    struct Case {
        client: &'static str,
        user: &'static str,
        password: &'static str,
        tenant: &'static str,
        id: u64,
    }

    let cases = [
        Case {
            client: "psql",
            user: "alice",
            password: "pw-alice",
            tenant: "tenant-a",
            id: 901,
        },
        Case {
            client: "psycopg",
            user: "bob",
            password: "pw-bob",
            tenant: "tenant-b",
            id: 902,
        },
        Case {
            client: "pg",
            user: "carol",
            password: "pw-carol",
            tenant: "tenant-c",
            id: 903,
        },
    ];

    for case in cases {
        let (db_path, _db_guard) = seed_three_tenant_db();
        let users_dir = temp_db::TempDir::new("three-client-e2e-emergency-users");
        let users_path = users_dir.path().join("users.txt");
        write_users_file(&users_path);

        let mut server = spawn_wire_server(
            &users_path,
            &db_path,
            &["--fault-inject".into(), "post-commit-panic".into()],
        );
        // 「fault injection armed」が listen 到達前に観測されること（非
        // vacuous な arm 確認。`wire_fault_injection_cli.rs::
        // armed_post_commit_panic_sends_emergency_response_then_aborts` と
        // 同じ検査方針）。feature 無効ビルドで実行された場合は `--fault-inject`
        // が未知引数として拒否され listen 行に到達できないため、この時点で
        // `spawn_wire_server` 側の panic として明示的に失敗する。
        assert!(
            server
                .startup_lines
                .iter()
                .any(|l| l.contains("fault injection armed")),
            "client={}: expected 'fault injection armed' before listen; lines={:?}",
            case.client,
            server.startup_lines,
        );

        let insert_sql = format!(
            "INSERT INTO docs (id, embedding, lang, body) VALUES \
             ({}, '[0.5,0.5]', 'en', 'emergency e2e') \
             USING OPERATION_ID 'emergency-e2e-{}'",
            case.id, case.client
        );

        let stderr = match case.client {
            "psql" => run_psql_expect_emergency(server.port, case.user, case.password, &insert_sql),
            "psycopg" => {
                run_psycopg_expect_emergency(server.port, case.user, case.password, &insert_sql)
            }
            "pg" => run_pg_expect_emergency(server.port, case.user, case.password, &insert_sql),
            other => panic!("unknown client label: {other}"),
        };
        eprintln!("[e2e-record] {}: {stderr}", case.client);

        let status = server.wait_for_exit(Duration::from_secs(30));
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert!(
                !status.success(),
                "client={}: server must not exit successfully; status={status:?}",
                case.client
            );
            assert_eq!(
                status.signal(),
                Some(6),
                "client={}: server must be terminated by SIGABRT \
                 (std::process::abort); status={status:?}",
                case.client
            );
        }

        // commit 自体は成功しているため、再オープン後も投入行が可視のまま
        // であること（`state=may_be_committed` が実際に committed だった
        // ことの確認。他テナントの Public 行 3 件と合わせて 4 件になる）。
        drop(server);
        let storage = Storage::open(&db_path).expect("reopen storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let read_ctx = PolicyContext::with_visibilities(
            case.tenant,
            [Visibility::Public, Visibility::Private],
        )
        .expect("valid tenant");
        let result = core
            .execute_sql(
                &read_ctx,
                "SELECT id FROM docs ORDER BY embedding <=> '[0.5,0.5]' LIMIT 10",
            )
            .expect("select should succeed after the emergency-abort path");
        assert_eq!(
            result.rows.len(),
            4,
            "client={}: expected 3 seeded Public rows + 1 committed Private row",
            case.client
        );
        assert!(
            result.rows.iter().any(|row| row.id == case.id),
            "client={}: expected the committed row (id={}) to remain visible; rows={:?}",
            case.client,
            case.id,
            result.rows,
        );
    }
}
