//! `POST /v1/query`（`op: "create_table"｜"alter_table"｜"drop_table"`）を
//! production ルータ（生バイトクライアント）経由で固定する層 A 結合テスト
//! （Issue #910・NOSQL-13・TASK-207。ポインタ: `docs/spec/05-tasks.md`
//! TASK-207・`docs/spec/04-behavior/nosql-surface.md` NOSQL-13・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・
//! `docs/spec/04-behavior/error-format.md` ERR-4）。
//!
//! `crates/wire-server/src/http/query/ddl.rs` 内の unit tests がトークン列
//! 写像の境界値を検証済みのため、本ファイルは「HTTP フレーミング越しに
//! SQL 表層の DDL と同一の実行結果（成功・エラー分類）が観測できること」
//! （DDL 実行権限ゲート・カタログ照会・SQL/NoSQL 間のスキーマパリティを含む）
//! に絞る。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::auth::UserStore;
use wire_server::http::session::store::SessionStore;
use wire_server::limits::ConnectionLimiter;

const TENANT_A: &str = "tenant-a";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql13-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql13-ddl-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// [`http_common::spawn_router_listener_with_engine`] と同一の production 入口
/// だが、呼び出し元が組み立てた `UserStore`（`--ddl-allowed-users` 適用済み）を
/// 使う（`wire_ddl_add_column.rs::spawn_server_with_engine_and_store` の HTTP 版）。
fn spawn_with_store(store: UserStore, engine: Arc<EngineCore>) -> std::net::SocketAddr {
    let store = Arc::new(store);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
    let router = wire_server::http::router::Router::with_engine(store, SessionStore::new(), engine);

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_router(
            listener,
            limiter,
            wire_server::limits::READ_TIMEOUT,
            router,
        );
    });

    addr
}

struct Session {
    addr: std::net::SocketAddr,
    token: String,
}

fn login(addr: std::net::SocketAddr, user: &str, password: &str) -> Session {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#).into_bytes();
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        &body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let token = match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };
    Session { addr, token }
}

fn query(session: &Session, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {}", session.token)),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        session.addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn ddl_session(core: Arc<EngineCore>) -> Session {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("alice is a known username");
    let addr = spawn_with_store(store, core);
    login(addr, "alice", "pw-alice")
}

fn non_ddl_session(core: Arc<EngineCore>) -> Session {
    let users_path = common::write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_with_store(store, core);
    login(addr, "alice", "pw-alice")
}

// --- create_table ---------------------------------------------------------

#[test]
fn create_table_succeeds_and_is_visible_to_insert_and_scan() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let body = br#"{"op":"create_table","table":"docs","columns":[
        {"name":"embedding","type":"vector","dim":3},
        {"name":"lang","type":"text","nullable":true}
    ]}"#;
    let resp = query(&session, body);
    assert_eq!(resp.status, 200, "got: {resp:?}");
    assert_eq!(String::from_utf8_lossy(&resp.body).trim(), r#"{"ok":true}"#);

    let insert_body =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3],"lang":"ja"}],"operation_id":"op-1"}"#;
    let insert_resp = query(&session, insert_body);
    assert_eq!(insert_resp.status, 200, "got: {insert_resp:?}");

    let scan_body = br#"{"op":"scan","table":"docs","limit":10}"#;
    let scan_resp = query(&session, scan_body);
    assert_eq!(scan_resp.status, 200, "got: {scan_resp:?}");
}

#[test]
fn create_table_with_primary_key_unique_and_foreign_key_succeeds() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);

    let parent = br#"{"op":"create_table","table":"parent","columns":[
        {"name":"code","type":"integer"}
    ],"constraints":[{"kind":"primary_key","columns":["code"]}]}"#;
    assert_eq!(query(&session, parent).status, 200);

    let child = br#"{"op":"create_table","table":"child","columns":[
        {"name":"embedding","type":"vector","dim":2},
        {"name":"parent_code","type":"integer"},
        {"name":"tag","type":"text"}
    ],"constraints":[
        {"kind":"unique","columns":["tag"]},
        {"kind":"foreign_key","columns":["parent_code"],"references":{"table":"parent","columns":["code"]}}
    ]}"#;
    let resp = query(&session, child);
    assert_eq!(resp.status, 200, "got: {resp:?}");
}

#[test]
fn create_table_duplicate_table_is_42p07() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text"}]}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P07", "got: {resp:?}");
}

#[test]
fn create_table_schema_parity_matches_sql_surface_reserved_columns() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    for reserved in ["id", "tenant_id", "visibility", "check", "constraint"] {
        let body = format!(
            r#"{{"op":"create_table","table":"docs","columns":[{{"name":"{reserved}","type":"text"}}]}}"#
        );
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "reserved column {reserved:?} got: {resp:?}"
        );
    }
}

#[test]
fn create_table_unknown_type_and_invalid_vector_shape_are_42601() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let cases = [
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"bogus"}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"vector"}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text","dim":3}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"vector","dim":3,"nullable":true}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text","default":true}]}"#,
        r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text","default":null}]}"#,
    ];
    for body in cases {
        let resp = query(&session, body.as_bytes());
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42601",
            "body={body} got: {resp:?}"
        );
    }
}

#[test]
fn create_table_check_constraint_is_0a000() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body = br#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"integer"}],
        "constraints":[{"kind":"check","columns":["a"]}]}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "0A000", "got: {resp:?}");
}

#[test]
fn create_table_tenant_id_self_declaration_is_42601() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body =
        br#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text"}],"tenant_id":"evil"}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "got: {resp:?}");
}

// --- alter_table -----------------------------------------------------------

#[test]
fn alter_table_add_column_succeeds() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"alter_table","table":"docs","add_column":{"name":"note","type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(resp.status, 200, "got: {resp:?}");
}

#[test]
fn alter_table_add_column_numeric_and_enum_succeed() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);

    let numeric = br#"{"op":"alter_table","table":"docs","add_column":{"name":"price","type":"numeric","precision":10,"scale":2}}"#;
    assert_eq!(query(&session, numeric).status, 200);

    // ENUM 型は事前登録が無いため、実行段（`Storage::get_enum_type`）が
    // 未定義の型名として `42601`（SQL 表層 `ALTER TABLE ADD COLUMN <col>
    // <未知の識別子>` と同一の分類。ENUM 型名の存在確認は engine 側の実行段
    // が担う）を返す。少なくとも `0A000` へ落ちず engine まで到達したことを
    // 固定する（構造検証段〔wire 側〕とカタログ照会段〔engine 側〕のどちらの
    // `42601` かは区別しないが、いずれも SQL 表層とパリティが取れている）。
    let enum_col = br#"{"op":"alter_table","table":"docs","add_column":{"name":"kind","type":"enum","enum_type":"my_enum"}}"#;
    let resp = query(&session, enum_col);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "got: {resp:?}");
}

#[test]
fn alter_table_add_column_undefined_table_is_42p01() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let body =
        br#"{"op":"alter_table","table":"nonexistent","add_column":{"name":"note","type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "got: {resp:?}");
}

#[test]
fn alter_table_add_column_duplicate_column_is_42701() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body =
        br#"{"op":"alter_table","table":"docs","add_column":{"name":"embedding","type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42701", "got: {resp:?}");
}

#[test]
fn alter_table_drop_column_is_0a000() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"alter_table","table":"docs","drop_column":{"name":"embedding"}}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "0A000", "got: {resp:?}");
}

#[test]
fn alter_table_both_and_neither_add_and_drop_column_is_42601() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);

    let both = br#"{"op":"alter_table","table":"docs","add_column":{"name":"a","type":"text"},"drop_column":{"name":"embedding"}}"#;
    assert_eq!(http_common::wire_code_of(&query(&session, both)), "42601");

    let neither = br#"{"op":"alter_table","table":"docs"}"#;
    assert_eq!(
        http_common::wire_code_of(&query(&session, neither)),
        "42601"
    );
}

#[test]
fn alter_table_add_column_reserved_enum_type_keyword_is_42601() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    let body = br#"{"op":"alter_table","table":"docs","add_column":{"name":"kind","type":"enum","enum_type":"text"}}"#;
    let resp = query(&session, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "got: {resp:?}");
}

// --- drop_table --------------------------------------------------------

#[test]
fn drop_table_succeeds_and_subsequent_scan_reports_undefined_table() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);

    let resp = query(&session, br#"{"op":"drop_table","table":"docs"}"#);
    assert_eq!(resp.status, 200, "got: {resp:?}");
    assert_eq!(String::from_utf8_lossy(&resp.body).trim(), r#"{"ok":true}"#);

    let scan_resp = query(&session, br#"{"op":"scan","table":"docs","limit":1}"#);
    assert_eq!(http_common::wire_code_of(&scan_resp), "42P01");
}

#[test]
fn drop_table_undefined_table_is_42p01() {
    let (core, _guard) = new_core();
    let session = ddl_session(core);
    let resp = query(&session, br#"{"op":"drop_table","table":"nonexistent"}"#);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "got: {resp:?}");
}

// --- 権限（42501。存在オラクル非公開） -----------------------------------

#[test]
fn all_three_ddl_ops_reject_with_42501_without_ddl_permission() {
    let (core, _guard) = new_core_with_docs_table();
    let session = non_ddl_session(core);

    let cases: [&[u8]; 3] = [
        br#"{"op":"create_table","table":"new_table","columns":[{"name":"a","type":"text"}]}"#,
        br#"{"op":"alter_table","table":"docs","add_column":{"name":"note","type":"text"}}"#,
        br#"{"op":"drop_table","table":"docs"}"#,
    ];
    for body in cases {
        let resp = query(&session, body);
        assert_eq!(
            http_common::wire_code_of(&resp),
            "42501",
            "body={body:?} got: {resp:?}"
        );
    }
}

#[test]
fn permission_denial_is_byte_identical_regardless_of_table_existence() {
    // DDL 実行権限ゲートはカタログ照会より必ず先に判定する——権限の無い
    // 主体には対象テーブルの有無にかかわらず同一の応答を返す（存在オラクル
    // 非公開。security.md P0）。`Date` ヘッダを含む応答全体を比較する前に
    // `wire_code`／status のみ固定し、本文（`message`）も一致することを見る。
    let (core, _guard) = new_core_with_docs_table();
    let session = non_ddl_session(core);

    let existing = query(&session, br#"{"op":"drop_table","table":"docs"}"#);
    let missing = query(&session, br#"{"op":"drop_table","table":"does_not_exist"}"#);
    assert_eq!(existing.status, missing.status);
    assert_eq!(
        http_common::wire_code_of(&existing),
        http_common::wire_code_of(&missing)
    );
    assert_eq!(
        http_common::error_message_of(&existing),
        http_common::error_message_of(&missing)
    );
}

// --- 語彙外（NOSQL-13 対象外の DDL 相当） -------------------------------

#[test]
fn create_index_and_view_ddl_remain_unsupported_via_nosql_surface() {
    let (core, _guard) = new_core_with_docs_table();
    let session = ddl_session(core);
    for body in [
        &br#"{"op":"create_index","table":"docs"}"#[..],
        &br#"{"op":"drop_index","table":"docs"}"#[..],
        &br#"{"op":"create_view","table":"docs"}"#[..],
        &br#"{"op":"drop_view","table":"docs"}"#[..],
    ] {
        let resp = query(&session, body);
        assert_eq!(http_common::wire_code_of(&resp), "0A000", "got: {resp:?}");
    }
}

// --- untrusted 値の非 echo -------------------------------------------------
//
// `engine::sql::allowlist::SqlSurfaceError::undefined_table`（`42P01`）は
// SQL 表層と同じ設計判断で、切り詰め済みのテーブル名（クライアント自身が
// 送った識別子であり、他テナントの情報ではない）を文言に含める契約
// （`error_format.rs`「テーブル名は…エラーへ含める」参照）。そのため
// `create_table`／`alter_table`／`drop_table` の未定義テーブルエラーは
// テーブル名の非 echo 検査の対象外とし、代わりに権限拒否（`42501`。
// [`all_three_ddl_ops_reject_with_42501_without_ddl_permission`]・
// [`permission_denial_is_byte_identical_regardless_of_table_existence`]）が
// 固定文言のみを返すことで security.md の「存在情報を漏らさない」契約を
// 検証する。

#[test]
fn permission_denial_message_does_not_echo_untrusted_table_name() {
    let (core, _guard) = new_core();
    let session = non_ddl_session(core);
    let marker = "zzz_marker_value_zzz";
    let body = format!(r#"{{"op":"drop_table","table":"{marker}"}}"#);
    let resp = query(&session, body.as_bytes());
    assert_eq!(http_common::wire_code_of(&resp), "42501");
    http_common::assert_message_does_not_echo(&resp, marker);
}
