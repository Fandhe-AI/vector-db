//! 無改造の実クライアント 3 種（`psql`／Python `psycopg`／Node.js `pg`）から
//! `wire-server` バイナリへ実接続し、C1 クエリの実行・誤りパスワードの拒否を
//! 検証する層 B の統合テスト（TASK-73、対象ビヘイビア: WIRE-1）。
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
fn seed_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let tenants: [(&str, u64, [f32; 2]); 3] = [
        ("tenant-a", 1, [1.0, 0.0]),
        ("tenant-b", 2, [0.0, 1.0]),
        ("tenant-c", 3, [-1.0, 0.0]),
    ];
    for (tenant, id, dir) in tenants {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(dir.to_vec())],
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

const SELECT_OWN_ROW: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";

/// psql（無改造）で C1 を実行し、返却された id 集合を返す。
fn run_psql(port: u16, user: &str, password: &str) -> Vec<String> {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
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
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            SELECT_OWN_ROW,
        ])
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

/// Python `psycopg`（無改造）で C1 を実行し、id 集合を返す。
fn run_psycopg(port: u16, user: &str, password: &str) -> Vec<String> {
    let python = resolve_tool("PYTHON_BIN", "python3");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/psycopg_client.py");
    let output = Command::new(&python)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", SELECT_OWN_ROW)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {python}: {e}"));
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

/// Node.js `pg`（無改造）で C1 を実行し、id 集合を返す。
fn run_pg(port: u16, user: &str, password: &str) -> Vec<String> {
    let node = resolve_tool("NODE_BIN", "node");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_client.js");
    let output = Command::new(&node)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", SELECT_OWN_ROW)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {node}: {e}"));
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

/// 3 クライアント（psql / psycopg / pg）それぞれで、3 テナントいずれの
/// ユーザーで接続しても C1 の結果が独立オラクル（seed 表からの距離降順・
/// `Visibility::Public` はテナント跨ぎで可視。層 A の
/// `wire1_three_tenant_visibility_public_shared_private_hidden` と同じ可視性
/// 契約）と一致すること・誤りパスワードが拒否されることを検証する。
/// ツール未導入・スクリプト失敗は silent skip せず `panic!` で失敗させる
/// （本ファイル先頭のドキュメンテーションコメント参照）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_c1_and_reject_wrong_password() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;

    // クエリ `[1.0,0.0]` からの距離昇順オラクル（seed 表: tenant-a=(1,0)距離0・
    // tenant-b=(0,1)距離1・tenant-c=(-1,0)距離2）。`Public` 行は全テナントから
    // 可視のため、どのユーザーで接続してもこの 3 件が返る。
    let expected_ids = vec!["1".to_string(), "2".to_string(), "3".to_string()];

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        let psql_ids = run_psql(port, user, pw);
        assert_eq!(
            psql_ids, expected_ids,
            "psql: unexpected result for user {user}"
        );

        let psycopg_ids = run_psycopg(port, user, pw);
        assert_eq!(
            psycopg_ids, expected_ids,
            "psycopg: unexpected result for user {user}"
        );

        let pg_ids = run_pg(port, user, pw);
        assert_eq!(
            pg_ids, expected_ids,
            "pg: unexpected result for user {user}"
        );
    }

    run_psql_wrong_password(port, "alice");

    drop(server);
    let _ = std::io::stdout().flush();
}
