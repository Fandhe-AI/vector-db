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
//! （`cargo test -p wire-server --test three_client_e2e -- --ignored`）から
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
//! ツール未検出・クライアントスクリプトの非 0 終了はいずれも `panic!` で
//! 失敗させ、silent skip はしない（`.claude/rules/coding-rust.md`・実行規約
//! 「テストの skip・ignore・アサーション弱体化で CI を通さない」の精神を、
//! 明示的に選択実行するこの導線でも維持する）。

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
struct ServerGuard {
    child: Child,
    port: u16,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_wire_server(users_path: &Path, db_path: &Path) -> ServerGuard {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_path.to_str().expect("utf-8 path"),
            "--db",
            db_path.to_str().expect("utf-8 path"),
            "--bind",
            "127.0.0.1:0",
        ])
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
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(addr_str) = line.trim().strip_prefix("wire-server: listening on ") {
                    if let Ok(addr) = addr_str.parse::<std::net::SocketAddr>() {
                        port = Some(addr.port());
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("wire-server did not report a listening port within the deadline");
    };

    ServerGuard { child, port }
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
        )
        .expect("insert row");
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

    let server = spawn_wire_server(&users_path, &db_path);
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

    let server = spawn_wire_server(&users_path, &db_path);
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
