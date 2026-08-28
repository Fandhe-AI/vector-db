//! TASK-82（対象ビヘイビア: SQL-5, SQL-6, SQL-7, SQL-9, SQL-10）の層 B: 無改造の
//! 実クライアント 3 種（psql／Python `psycopg`／Node.js `pg`）から `wire-server`
//! バイナリへ実接続し、拡張構文（`USING PLAN`／`EXPLAIN`／`HINT ORDER`／
//! `CREATE FUNCTION` 呼び出し／`USING OPERATION_ID` 付き `INSERT`）を実行する
//! 統合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-82・
//! `docs/spec/04-behavior/sql-surface.md` SQL-5〜7, SQL-9, SQL-10。
//!
//! 責務境界: 各構文の規則そのものは engine 側の in-process 結合テスト
//! （`crates/engine/tests/sql_using_plan.rs`・`sql_explain.rs`・
//! `sql_evaluation_order.rs`・`sql_udf_call.rs`・`sql_operation_id.rs`）と wire
//! 層 A（`tests/wire_using_plan.rs`・`wire_explain.rs`・`wire_hint_order.rs`・
//! `wire_udf_call.rs`・`wire_insert_operation_id.rs`、いずれも常時 `make ci`）が
//! 確定オラクルとして検証済みのため、本ファイルは同じバイト列が無改造クライアント
//! 経由でも観測できることの代表ケース確認に徹する（`tests/three_client_e2e.rs`
//! と同じ流儀・同じヘルパー構成。ローカル・Docker 開発コンテナには
//! `psql`／`psycopg`／`pg` が導入されていないため `#[ignore]` とし、
//! `make e2e-three-client` から明示的に実行する。ADR:
//! `docs/design/three-client-e2e-harness.md`）。
//!
//! `USING PLAN`／`EXPLAIN`（SQL-5・SQL-6）は `wire-server` バイナリへの
//! `--planner-endpoint`／`--planner-model`／`--embedder-hashing-dim` opt-in
//! 注入（TASK-117）を要する。本ファイルはプロセス内 HTTP スタブ
//! （Ollama `/api/generate` 互換の最小応答）を子プロセスの起動前に立て、
//! `--planner-endpoint 127.0.0.1:<stub port>` として渡す（実 Ollama への疎通は
//! 対象外。`crates/engine/src/query_planner.rs` の `spawn_stub_server` 単体テスト
//! と同型の構成）。埋め込みは `engine::embedding::HashingEmbedder`
//! （`--embedder-hashing-dim`。決定的・ネットワーク不要な検証用参照実装）を使う。
//!
//! `HINT ORDER`／`CREATE FUNCTION`／`INSERT`（SQL-7・SQL-9・SQL-10）は追加の
//! 注入を要さないため、`tests/three_client_e2e.rs::spawn_wire_server` と同型の
//! 素の `wire-server` 起動で足りる。
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
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

/// 環境変数（`PSQL_BIN`/`PYTHON_BIN`/`NODE_BIN`）で上書きできるツール解決
/// （`tests/three_client_e2e.rs::resolve_tool` と同型）。
fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

/// `wire-server` バイナリの子プロセスハンドル（`tests/three_client_e2e.rs::
/// ServerGuard` と同型。`extra_args` で `--planner-endpoint` 等の追加 CLI を
/// 渡せるよう `spawn` 時にまとめて指定する）。
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
    thread::spawn(move || {
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

/// Ollama `/api/generate`（`stream: false`）互換の最小 HTTP スタブ。接続を
/// 継続的に受理し（`USING PLAN`／`EXPLAIN` が実行されるたびに `wire-server`
/// 子プロセスが新規 TCP 接続を張るため。`query_planner.rs` の
/// `http_post_json` は `Connection: close` を送り毎回再接続する）、常に同じ
/// 展開結果を返す。`crate::query_planner` の `spawn_stub_server` 単体テスト
/// （1 接続限定）と同じ応答形状を、複数接続に対応させて再利用する。
struct OllamaStubGuard {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    listener_addr: std::net::SocketAddr,
}

impl Drop for OllamaStubGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // accept() をブロック解除するためのダミー接続。
        let _ = TcpStream::connect(self.listener_addr);
    }
}

fn spawn_ollama_stub(expansion_response_json: &'static str) -> OllamaStubGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
    let addr = listener.local_addr().expect("local addr");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_thread = std::sync::Arc::clone(&stop);
    thread::spawn(move || {
        for stream in listener.incoming() {
            if stop_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            let Ok(mut socket) = stream else { continue };
            let mut reader = BufReader::new(socket.try_clone().expect("clone socket"));
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            // Ollama `/api/generate` 応答の `response` フィールド値は生成テキスト
            // （本テストでは展開結果 JSON をそのまま文字列化したもの）を運ぶ。
            // 二重引用符をエスケープして埋め込む。
            let escaped = expansion_response_json.replace('"', "\\\"");
            let body = format!(r#"{{"model":"stub-model","response":"{escaped}","done":true}}"#);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
        }
    });
    OllamaStubGuard {
        port: addr.port(),
        stop,
        listener_addr: addr,
    }
}

/// `USING PLAN`／`EXPLAIN`（SQL-5・SQL-6）用の `docs` テーブル
/// （`embedding VECTOR(2)` + `path TEXT` + `body TEXT`。`path`/`body` は
/// TASK-110 の辞書スナップショット必須列）。`wire_using_plan.rs`・
/// `wire_explain.rs` と同一の投入方針。
fn seed_using_plan_docs() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("extended-syntax-e2e-using-plan-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("docs/a.md".to_string()),
            Value::Text("alpha content".to_string()),
        ],
        &OperationId::parse("extended-e2e-using-plan-op-1").expect("valid operation_id"),
    )
    .expect("insert row 1");
    // 2 行目（`crates/engine/tests/zzz_scratch_using_plan_check.rs` 相当の構成で
    // 事前に確認済み: `EXPANSION_RESPONSE`・`HashingEmbedder::new(2)` の同一構成で
    // `USING PLAN` 実行結果が `[1, 2]` の順で両方返ることを in-process オラクルで
    // 確認してから、このテストの期待値へ反映した。単一行テーブルでは
    // `USING PLAN` の展開・実行が実際に走らなくても偶然一致してしまうため、
    // 2 行構成にして非自明な検証にする）。
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        2,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0, 1.0]),
            Value::Text("docs/b.md".to_string()),
            Value::Text("unrelated beta content".to_string()),
        ],
        &OperationId::parse("extended-e2e-using-plan-op-2").expect("valid operation_id"),
    )
    .expect("insert row 2");
    (path, guard)
}

/// `HINT ORDER`／`CREATE FUNCTION`／`INSERT`（SQL-7・SQL-9・SQL-10）用の
/// `docs` テーブル（`embedding VECTOR(2)`）。追加の注入を要しないため
/// `path`/`body` 列は持たない。
fn seed_plain_docs() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("extended-syntax-e2e-plain-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, emb) in [(1u64, [0.9f32, 0.1]), (2, [0.0, 1.0]), (3, [0.1, 0.9])] {
        let op_id =
            OperationId::parse(&format!("extended-e2e-plain-op-{id}")).expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &op_id,
        )
        .expect("insert row");
    }
    (path, guard)
}

fn write_users_file(path: &Path) {
    use wire_server::auth::argon2id;
    let salt = b"0123456789abcdef";
    let phc = argon2id::encode_phc(b"pw-alice", salt, &argon2id::RECOMMENDED_PARAMS)
        .expect("valid phc encoding");
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write users file");
}

// --- クライアント実行ヘルパー（`tests/three_client_e2e.rs` と同型） -----------------

fn run_psql(port: u16, sql: &str) -> Vec<String> {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .env("PGPASSWORD", "pw-alice")
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            "alice",
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-q",
            "-At",
            "-F",
            "|",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {psql} (install libpq-client tools or set PSQL_BIN): {e}")
        });
    assert!(
        output.status.success(),
        "psql exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn spawn_psycopg_client(port: u16, sql: &str) -> std::process::Output {
    let python = resolve_tool("PYTHON_BIN", "python3");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/psycopg_client.py");
    Command::new(&python)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", "alice")
        .env("WIRE_PASSWORD", "pw-alice")
        .env("WIRE_SQL", sql)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {python}: {e}"))
}

fn run_psycopg(port: u16, sql: &str) -> Vec<String> {
    let output = spawn_psycopg_client(port, sql);
    assert!(
        output.status.success(),
        "psycopg_client.py failed (install psycopg via `pip install psycopg[binary]` or set \
         PYTHON_BIN): stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn spawn_pg_client(port: u16, sql: &str) -> std::process::Output {
    let node = resolve_tool("NODE_BIN", "node");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_client.js");
    Command::new(&node)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", "alice")
        .env("WIRE_PASSWORD", "pw-alice")
        .env("WIRE_SQL", sql)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {node}: {e}"))
}

fn run_pg(port: u16, sql: &str) -> Vec<String> {
    let output = spawn_pg_client(port, sql);
    assert!(
        output.status.success(),
        "pg_client.js failed (install pg via `npm install pg` or set NODE_BIN): stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// 3 クライアントすべてで `sql` を実行し、結果集合が `expected` と一致することを
/// 確認する。
fn assert_all_clients_match(port: u16, label: &str, sql: &str, expected: &[String]) {
    assert_eq!(run_psql(port, sql), expected, "psql: unexpected {label}");
    assert_eq!(
        run_psycopg(port, sql),
        expected,
        "psycopg: unexpected {label}"
    );
    assert_eq!(run_pg(port, sql), expected, "pg: unexpected {label}");
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha"], "path_hint": "docs/", "kind_hint": null}"#;

/// SQL-5: `USING PLAN('<query>')` が 3 クライアントいずれからも実行でき、
/// 展開・再埋め込み後のハイブリッド検索結果（id=1）を返す。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_using_plan_search() {
    let (db_path, _db_guard) = seed_using_plan_docs();
    let users_dir = temp_db::TempDir::new("extended-syntax-e2e-using-plan-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let stub = spawn_ollama_stub(EXPANSION_RESPONSE);
    let server = spawn_wire_server(
        &users_path,
        &db_path,
        &[
            "--planner-endpoint".into(),
            format!("127.0.0.1:{}", stub.port),
            "--planner-model".into(),
            "stub-model".into(),
            "--embedder-hashing-dim".into(),
            "2".into(),
        ],
    );
    let port = server.port;

    // `USING PLAN(...)` は `ORDER BY` の代替（相互排他。SQL-5）であり、両立しない
    // （`crates/engine/tests/sql_using_plan.rs` と同一の規範形）。
    let sql = "SELECT id FROM docs USING PLAN('find alpha docs') LIMIT 3";
    assert_all_clients_match(
        port,
        "USING PLAN search",
        sql,
        &["1".to_string(), "2".to_string()],
    );
}

/// SQL-6: `EXPLAIN SELECT ... USING PLAN(...)` が 3 クライアントいずれからも
/// 実行でき、`QUERY PLAN` 単一列の展開結果行を返す（検索本体は実行しない）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_explain_using_plan() {
    let (db_path, _db_guard) = seed_using_plan_docs();
    let users_dir = temp_db::TempDir::new("extended-syntax-e2e-explain-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let stub = spawn_ollama_stub(EXPANSION_RESPONSE);
    let server = spawn_wire_server(
        &users_path,
        &db_path,
        &[
            "--planner-endpoint".into(),
            format!("127.0.0.1:{}", stub.port),
            "--planner-model".into(),
            "stub-model".into(),
            "--embedder-hashing-dim".into(),
            "2".into(),
        ],
    );
    let port = server.port;

    let sql = "EXPLAIN SELECT id FROM docs USING PLAN('find alpha docs') LIMIT 3";
    // 展開結果の可視化行が届くことのみを確認する（文言の詳細は engine 側
    // オラクル `tests/sql_explain.rs` の管轄）。
    let psql_rows = run_psql(port, sql);
    assert!(
        !psql_rows.is_empty(),
        "psql: EXPLAIN must return at least one QUERY PLAN row"
    );
    let psycopg_rows = run_psycopg(port, sql);
    assert!(
        !psycopg_rows.is_empty(),
        "psycopg: EXPLAIN must return at least one QUERY PLAN row"
    );
    let pg_rows = run_pg(port, sql);
    assert!(
        !pg_rows.is_empty(),
        "pg: EXPLAIN must return at least one QUERY PLAN row"
    );
}

/// SQL-7: `HINT ORDER(...)` の受理（既定順序との結果一致）を 3 クライアントで
/// 確認する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_hint_order() {
    let (db_path, _db_guard) = seed_plain_docs();
    let users_dir = temp_db::TempDir::new("extended-syntax-e2e-hint-order-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    let default_sql = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    let hinted_sql = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 \
                       HINT ORDER(RLS, SCALAR, DISTANCE)";
    // `[1, 3, 2]`: `seed_plain_docs` の同一コーパス（id=1 `[0.9,0.1]`／id=2
    // `[0.0,1.0]`／id=3 `[0.1,0.9]`、クエリ `[1.0,0.0]`）に対する in-process
    // engine オラクル（`EngineCore::execute_sql`）で事前に確認済みの順序。
    let expected = vec!["1".to_string(), "3".to_string(), "2".to_string()];

    assert_all_clients_match(port, "default order", default_sql, &expected);
    assert_all_clients_match(
        port,
        "HINT ORDER(RLS, SCALAR, DISTANCE)",
        hinted_sql,
        &expected,
    );
}

/// SQL-9: 同一接続セッション内で `CREATE FUNCTION` → 結果列位置からの UDF
/// 呼び出しが 3 クライアントいずれからも成功する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_create_function_then_call() {
    let (db_path, _db_guard) = seed_plain_docs();
    let users_dir = temp_db::TempDir::new("extended-syntax-e2e-udf-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    let create_sql = "CREATE FUNCTION udf_norm(v) AS vec_norm(v)";
    let call_sql = "SELECT id FROM docs WHERE udf_norm(embedding) < 1.5 \
                    ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    // 単一接続セッション内で複数文を送る（`run_*_session` 系の prelude 経由。
    // `three_client_e2e.rs` と同じ複数文セッション方式）。
    // `[1, 3, 2]`: 3 行とも `vec_norm(embedding) < 1.5` を満たす
    // （id=1≈0.906・id=2=1.0・id=3≈0.906）ため件数では判別できないが、順序
    // （`ORDER BY embedding <=> '[1.0,0.0]'`）は in-process engine オラクルで
    // 事前確認済み（`three_clients_run_hint_order` と同一コーパス・同一順序）。
    let expected: Vec<String> = vec!["1".to_string(), "3".to_string(), "2".to_string()];

    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .env("PGPASSWORD", "pw-alice")
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            "alice",
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-q",
            "-At",
            "-F",
            "|",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            create_sql,
            "-c",
            call_sql,
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        output.status.success(),
        "psql: CREATE FUNCTION + call session failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(rows, expected, "psql: unexpected UDF call result");
}

/// SQL-10: `INSERT ... USING OPERATION_ID '<id>'` が 3 クライアントいずれからも
/// `INSERT 0 1` として成功する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_insert_with_operation_id() {
    let (db_path, _db_guard) = seed_plain_docs();
    let users_dir = temp_db::TempDir::new("extended-syntax-e2e-insert-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    let insert_sql = |id: u64, op: &str| -> String {
        format!(
            "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.5,0.5]') \
             USING OPERATION_ID '{op}'"
        )
    };

    // psql は CommandComplete タグを標準の `\pset` では出力しないため、
    // `-c` 実行の終了コードのみで成功可否を判定する（`run_psql` はデータ行を
    // 前提とした薄いラッパーのため、ここでは直接 Command を組み立てる）。
    let psql = resolve_tool("PSQL_BIN", "psql");
    let status = Command::new(&psql)
        .env("PGPASSWORD", "pw-alice")
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            "alice",
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &insert_sql(100, "extended-e2e-insert-psql"),
        ])
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(status.success(), "psql: INSERT must succeed");

    let psycopg_out = spawn_psycopg_client(port, &insert_sql(101, "extended-e2e-insert-psycopg"));
    assert!(
        psycopg_out.status.success(),
        "psycopg: INSERT must succeed: stderr={}",
        String::from_utf8_lossy(&psycopg_out.stderr)
    );

    let pg_out = spawn_pg_client(port, &insert_sql(102, "extended-e2e-insert-pg"));
    assert!(
        pg_out.status.success(),
        "pg: INSERT must succeed: stderr={}",
        String::from_utf8_lossy(&pg_out.stderr)
    );
}
