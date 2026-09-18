//! NoSQL 表層（`--surface nosql`。HTTP/1.1 自作リスナー・`/v1/session`／
//! `/v1/session/close`／`/v1/query`）の層 B 統合テスト（Issue #776・
//! TASK-183・HTTP-13。ポインタ: `docs/spec/05-tasks.md` TASK-183・
//! `docs/spec/04-behavior/http-transport.md` HTTP-13）。
//!
//! 責務境界: SQL 表層側の `tests/three_client_e2e.rs`（psql／psycopg／pg。
//! `#[ignore]`・`make e2e-three-client`）が担う層 A/層 B 分割方針
//! （`docs/design/three-client-e2e-harness.md`）を NoSQL 表層へ踏襲する。
//! NoSQL 表層自体の契約（`op` 許可リスト・`vector`／`plan`／`mode`／`hybrid`
//! 束縛・`operation_id` 必須化・precision fail-closed・RLS 暗黙適用等）の
//! 回帰保護は既存の層 A（`http4_session_issue.rs`・`http5_query_bearer.rs`・
//! `http8_session_close.rs`・`nosql1_*`〜`nosql11_*`。常時 `make ci`）が担う。
//! 本ファイルはその上澄みとして、無改造の外部 HTTP クライアント（curl）が
//! 実バイナリへ接続して `session → search → close` を実行できることを
//! 外形的に確認する導入ハーネスであり、`#[ignore]` とし
//! `make e2e-three-client-http` から明示的に実行する（`ci` には含めない）。
//!
//! 起動・ポート取得は `common::SpawnedServer`（`http4_session_issue.rs` の
//! `spawned_binary_accepts_valid_login_over_nosql_surface` と同型）を再利用
//! する。seed（`docs` テーブル・3 テナント × Public 1 行）は
//! `tests/three_client_e2e.rs::seed_three_tenant_db` と同一内容をこのファイル
//! 専用に複製する（`extended_syntax_e2e.rs` の前例と同方針。private 関数の
//! ため import できない）。
//!
//! ツール未検出（`curl` が見つからない）・curl の非 0 終了・応答形状の不一致は
//! いずれも `panic!` で失敗させ、silent skip はしない
//! （`.claude/rules/coding-rust.md` の実行規約「テストの skip・ignore・
//! アサーション弱体化で CI を通さない」の精神を、明示的に選択実行するこの
//! 導線でも維持する）。
//!
//! スコープ外（後続 Issue への申し送り）:
//! - urllib.request／fetch のクライアントスクリプトとランナー: #777
//! - 3 クライアント一括実行・実行記録の整備: #778
//! - psql（SQL 経路）との結果一致比較（search／scan／aggregate）: #779

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::json::{parse_json, JsonValue};
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

/// 環境変数 `CURL_BIN` で上書きできるツール解決。未指定時は `PATH` 上の
/// `curl` を使う（`three_client_e2e.rs::resolve_tool` と同型）。ツール自体の
/// 存在確認はしない（`Command::spawn` の失敗として顕在化させ、呼び出し元が
/// 案内メッセージ付きで panic する）。
fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

/// `three_client_e2e.rs::seed_three_tenant_db` と同一内容（`docs` テーブル・
/// 3 テナント × Public 1 行）の一時 DB を用意する。呼び出し元がこの関数を
/// 抜けた時点で `Storage`（redb は単一ライター）は drop 済みのため、直後に
/// 子プロセス（同じ DB ファイルを開く `wire-server`）を安全に起動できる。
fn seed_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-http-e2e-docs");
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

/// `wire-server --surface nosql` を子プロセスとして起動し、stderr の
/// `listening on` 行から実 bind ポートを取得する（`common::SpawnedServer` を
/// 再利用。取得できなければ蓄積 stderr 行を添えて `panic!`）。
fn spawn_nosql_server(users_path: &str, db_path: &str) -> (common::SpawnedServer, u16) {
    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        users_path,
        "--db",
        db_path,
        "--bind",
        "127.0.0.1:0",
        "--surface",
        "nosql",
    ]);

    let deadline = Instant::now() + Duration::from_secs(10);
    let addr_str = server.wait_for_listening(deadline);
    let addr_str = match addr_str {
        Some(addr) => addr,
        None => {
            let seen = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
            panic!(
                "wire-server did not report a listening address within the deadline; \
                 stderr so far: {seen:?}"
            );
        }
    };
    let addr: std::net::SocketAddr = addr_str
        .parse()
        .unwrap_or_else(|e| panic!("invalid listening address {addr_str:?}: {e}"));
    (server, addr.port())
}

/// curl 応答の本文上限（untrusted な外部プロセス出力を無制限に読み込まない
/// ための固定上限。本テストが扱う応答は高々数百バイトで十分足りる）。
/// curl 自体にも `--max-filesize` として同じ上限を課し、上限超過時は
/// ディスクへ書き切ってから事後検査するのではなく curl の時点で転送を
/// 中断させる（fail-closed。curl は非 0 終了で応答し既存の非 0 終了検査で
/// `panic!` される）。
const MAX_RESPONSE_BODY_BYTES: u64 = 2 * 1024 * 1024;

/// `curl` を 1 要求 = 1 プロセスで起動し、HTTP ステータスコードと応答本文を
/// 返す（NoSQL 表層は `Connection: close` 固定のため `--next` 連結はしない）。
/// `Command` の引数配列で起動しシェルを介さない。curl の spawn 失敗・非 0
/// 終了はいずれも案内メッセージ付き `panic!`（silent skip しない）。
///
/// トークン（`Authorization` ヘッダ）・パスワードを含む要求本文は、
/// `ps`／プロセス一覧経由で一時的にでも観測されないよう `Command` の
/// 引数へ直接載せず、`out_dir` 配下の一時ファイル経由（ヘッダは `-H @file`・
/// 本文は `--data-binary @file`）で curl へ渡す。
fn curl_post(
    port: u16,
    target: &str,
    bearer: Option<&str>,
    json_body: &str,
    out_dir: &std::path::Path,
    seq: u32,
) -> (u16, String) {
    let out_path = out_dir.join(format!("{seq}.body"));
    let body_path = out_dir.join(format!("{seq}.req.json"));
    let headers_path = out_dir.join(format!("{seq}.headers"));
    let url = format!("http://127.0.0.1:{port}{target}");

    std::fs::write(&body_path, json_body)
        .unwrap_or_else(|e| panic!("failed to write curl request body {body_path:?}: {e}"));

    // 100-continue の余地を消す（本文が小さく即座に送れるため不要）。
    let mut headers = String::from("Content-Type: application/json\nExpect:\n");
    if let Some(token) = bearer {
        headers.push_str(&format!("Authorization: Bearer {token}\n"));
    }
    std::fs::write(&headers_path, &headers)
        .unwrap_or_else(|e| panic!("failed to write curl header file {headers_path:?}: {e}"));

    let args: Vec<String> = vec![
        "-sS".to_string(),
        "--max-time".to_string(),
        "10".to_string(),
        "--max-filesize".to_string(),
        MAX_RESPONSE_BODY_BYTES.to_string(),
        "-H".to_string(),
        format!("@{}", headers_path.to_str().expect("utf-8 headers path")),
        "--data-binary".to_string(),
        format!("@{}", body_path.to_str().expect("utf-8 body path")),
        "-o".to_string(),
        out_path.to_str().expect("utf-8 out path").to_string(),
        "-w".to_string(),
        "%{http_code}".to_string(),
        url,
    ];

    let curl_bin = resolve_tool("CURL_BIN", "curl");
    let output = Command::new(&curl_bin)
        .args(&args)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn curl (bin={curl_bin:?}): {e}; \
             install curl or set CURL_BIN to an alternative binary"
            )
        });
    if !output.status.success() {
        panic!(
            "curl exited non-zero (status={:?}) for target {target}: stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let status: u16 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|e| {
            panic!(
                "curl did not report a numeric http status for target {target}: {e} \
                 (stdout={:?})",
                String::from_utf8_lossy(&output.stdout)
            )
        });

    let meta = std::fs::metadata(&out_path)
        .unwrap_or_else(|e| panic!("curl response body missing at {out_path:?}: {e}"));
    assert!(
        meta.len() <= MAX_RESPONSE_BODY_BYTES,
        "curl response body for target {target} exceeds the test's size limit: {} bytes",
        meta.len()
    );
    let mut file = std::fs::File::open(&out_path)
        .unwrap_or_else(|e| panic!("failed to open curl response body {out_path:?}: {e}"));
    let mut body = String::new();
    file.read_to_string(&mut body)
        .unwrap_or_else(|e| panic!("curl response body for target {target} is not utf-8: {e}"));

    (status, body)
}

/// curl 応答・要求本文の一時保存先ディレクトリを `Drop` で確実に削除する
/// ガード（`temp_db::CleanupGuard` と同じ idiom。テスト内 `assert!` の
/// panic 経路でも解放されるようにする）。
struct CurlOutDirGuard(PathBuf);

impl Drop for CurlOutDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn json_object(body: &str) -> std::collections::BTreeMap<String, JsonValue> {
    match parse_json(body)
        .unwrap_or_else(|e| panic!("response body must be valid JSON: {e:?} (body={body:?})"))
    {
        JsonValue::Object(map) => map,
        other => panic!("expected JSON object, got {other:?}"),
    }
}

/// トークン形状（43 文字・base64url アルファベット限定）の検証（untrusted な
/// curl 応答由来の値を `Authorization` ヘッダへ流し込む前のガード。
/// `http4_session.rs` の同種検証と同じ基準）。
fn assert_valid_session_token(token: &str) {
    assert_eq!(token.len(), 43, "token must be 43 chars, got {token:?}");
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "token must be base64url alphabet only, got {token:?}"
    );
}

/// curl（無改造の外部 HTTP クライアント）で `POST /v1/session`（発行）→
/// `POST /v1/query`（`op: search`）→ `POST /v1/session/close`（失効）→
/// 失効後の同一トークン再送が `401`／`28000` で拒否されることまでを
/// 確認するスモークテスト（Issue #776 の受け入れ条件 R1〜R3）。
///
/// 3 クライアント一括の実行記録・SQL 経路とのパリティは #778／#779 の
/// スコープ（本 Issue のスコープ外）。
#[test]
#[ignore = "requires curl; run via `make e2e-three-client-http`"]
fn curl_runs_session_search_close_over_nosql_surface() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let db_path_str = db_path.to_str().expect("utf-8 db path").to_string();

    let (server, port) =
        spawn_nosql_server(users_path.to_str().expect("utf-8 users path"), &db_path_str);

    let out_dir = std::env::temp_dir().join(format!(
        "wire-server-three-client-http-e2e-curl-out-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&out_dir).expect("create curl output dir");
    let _out_dir_guard = CurlOutDirGuard(out_dir.clone());

    // 1. session 発行。
    let (status, body) = curl_post(
        port,
        "/v1/session",
        None,
        r#"{"user":"alice","password":"pw-alice"}"#,
        &out_dir,
        1,
    );
    assert_eq!(status, 200, "session issue failed: {body}");
    let session_obj = json_object(&body);
    let token = match session_obj.get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    assert_valid_session_token(&token);
    match session_obj.get("expires_in") {
        Some(JsonValue::Number(_)) => {}
        other => panic!("expected numeric expires_in field, got {other:?}"),
    }

    // 2. search（alice は tenant-a・全行 Public のため RLS 暗黙適用でも
    //    3 行とも可視。クエリ [1,0] に対する cosine は id1=1.0／id2=0.0／
    //    id3=-1.0 のため id 昇順で [1,2,3] が返る想定。オラクルは engine 側
    //    テストが既に固定済みのため、本テストは「実バイナリ経由でも同じ
    //    行数・行集合が返る」非 vacuous な証跡に限定する）。
    let search_body =
        r#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"]}"#;
    let (status, body) = curl_post(port, "/v1/query", Some(&token), search_body, &out_dir, 2);
    assert_eq!(status, 200, "search query failed: {body}");
    let result_obj = json_object(&body);
    match result_obj.get("row_count") {
        Some(JsonValue::Number(n)) => assert_eq!(n.as_f64(), 3.0, "row_count: {body}"),
        other => panic!("expected numeric row_count field, got {other:?}"),
    }
    let rows = match result_obj.get("rows") {
        Some(JsonValue::Array(rows)) => rows.clone(),
        other => panic!("expected array rows field, got {other:?}"),
    };
    let mut ids: Vec<f64> = rows
        .iter()
        .map(|row| match row {
            JsonValue::Array(cells) => match cells.first() {
                Some(JsonValue::Number(n)) => n.as_f64(),
                other => panic!("expected numeric id cell, got {other:?}"),
            },
            other => panic!("expected array row, got {other:?}"),
        })
        .collect();
    ids.sort_by(|a, b| a.partial_cmp(b).expect("comparable ids"));
    assert_eq!(ids, vec![1.0, 2.0, 3.0], "unexpected id set: {body}");

    // 3. session close。
    let (status, body) = curl_post(port, "/v1/session/close", Some(&token), "{}", &out_dir, 3);
    assert_eq!(status, 200, "session close failed: {body}");
    assert!(
        body.contains("\"closed\":true"),
        "expected closed:true, got: {body}"
    );

    // 4. 失効後の同一トークン再送は `401`／`28000` で拒否される（Issue #776
    //    の受け入れ条件 R3。close が実際にトークンを失効させたことの
    //    非 vacuous な証跡。`http8_session_close.rs` の同種検証と同じ
    //    `wire_code` 判定基準）。
    let (status, body) = curl_post(port, "/v1/query", Some(&token), search_body, &out_dir, 4);
    assert_eq!(status, 401, "revoked token must be rejected: {body}");
    assert!(
        body.contains("\"wire_code\":\"28000\""),
        "expected wire_code 28000 for revoked token, got: {body}"
    );

    let seen = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let joined = seen.join("");
    // Issue #776 の受け入れ条件 R1: SQL 表層が誤って起動していないことの
    // 非 vacuous な証跡（`main.rs` は `--surface nosql` 選択時のみこの行を
    // `listening on` の直前に出す契約）。
    assert!(
        joined.contains("wire-server: surface nosql:"),
        "expected nosql surface banner in stderr, got: {joined:?}"
    );
    assert!(
        !joined.contains(&token),
        "stderr must not leak the session token"
    );
    assert!(
        !joined.contains("tenant-a"),
        "stderr must not leak tenant id"
    );
    assert!(!joined.contains("alice"), "stderr must not leak username");
}
