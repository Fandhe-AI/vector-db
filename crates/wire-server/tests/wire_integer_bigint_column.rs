//! `INTEGER` / `BIGINT` 列型（Issue #881、対象ビヘイビア: TABLE-13・TABLE-6・
//! TASK-196）の wire プロトコル経由・簡易クエリ検証（層 A）。ポインタ:
//! `docs/spec/05-tasks.md` TASK-196。
//!
//! `crates/engine/tests/column_type_integer.rs` が engine API レベルの契約
//! （境界値・範囲外 `22003`・型不一致 `22000`・RLS・台帳照合）を確定オラクルとして
//! 検証済みのため、本ファイルは simple query プロトコル越しに観測できることの
//! 確認に絞る: `DataRow` のテキスト表現・範囲外リテラルの `ErrorResponse`
//! （`22003`）・NoSQL 表層 `insert` の JSON 数値束縛の暫定拒否（Issue #896 まで
//! `22000`）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;
use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

fn new_core_with_integer_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-integer-bigint-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// `DataRow` は `INTEGER`／`BIGINT` 列を 10 進テキストとしてそのまま送出する
/// （`RowDescription` の OID 写像は既存どおり `text` のまま。Issue #895）。
#[test]
fn wire_select_returns_integer_and_bigint_as_decimal_text() {
    let (core, _guard) = new_core_with_integer_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        &format!(
            "INSERT INTO docs (id, embedding, n, b) VALUES (1, '[0.1,0.2]', {}, {}) \
             USING OPERATION_ID 'op-1'",
            i32::MIN,
            i64::MAX
        ),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT n, b FROM docs WHERE id = 1 LIMIT 10");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(
        row,
        vec![Some(i32::MIN.to_string()), Some(i64::MAX.to_string()),]
    );
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 範囲外の `INTEGER` リテラルは wire 越しに `22003`（`ErrorResponse`）として
/// 観測できる。
#[test]
fn wire_insert_with_out_of_range_integer_literal_returns_22003() {
    let (core, _guard) = new_core_with_integer_table();
    let mut stream = spawn_with_alice(core);

    let over_i32 = i64::from(i32::MAX) + 1;
    send_simple_query(
        &mut stream,
        &format!(
            "INSERT INTO docs (id, embedding, n, b) VALUES (1, '[0.1,0.2]', {over_i32}, 0) \
             USING OPERATION_ID 'op-1'"
        ),
    );
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);
}

/// 小数リテラルを `INTEGER` 列へ渡した場合は `22000`。
#[test]
fn wire_insert_with_non_integer_literal_returns_22000() {
    let (core, _guard) = new_core_with_integer_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1.5, 0) \
         USING OPERATION_ID 'op-1'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);
}

/// NoSQL 表層 `POST /v1/query`（`op: insert`）で `INTEGER` 列へ JSON 数値を
/// 渡した場合、Issue #896（NoSQL の JSON 束縛）までは型不一致として `22000`
/// （HTTP 400）で拒否する（`sql::allowlist::ValidatedInsert` 経由の JSON→
/// `InsertLiteral` 変換が数値を文字列として素通しし、`INTEGER` 列は
/// `InsertLiteral::String` を受理しないため）。
#[test]
fn nosql_insert_with_json_number_for_integer_column_returns_22000() {
    let (core, _guard) = new_core_with_integer_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let http_addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);

    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let login_resp: HttpResponse = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &login_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(login_resp.status, 200, "login must succeed: {login_resp:?}");
    let login_text = String::from_utf8_lossy(&login_resp.body).into_owned();
    let token = match engine::json::parse_json(&login_text).expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };

    let insert_body =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"n":5,"b":1}],"operation_id":"op-1"}"#;
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &insert_body.len().to_string()),
        ],
        insert_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        http_addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 400, "unexpected response: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body);
    assert!(
        text.contains("22000"),
        "expected 22000 in error body, got: {text}"
    );
}
