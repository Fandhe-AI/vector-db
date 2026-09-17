//! `POST /v1/query` の op 許可リスト（`search`／`scan`／`aggregate`／
//! `insert` の閉じた 4 値）を、production ルータ経由（生バイトクライアント）
//! で検証する層 A 結合テスト（Issue #759・TASK-179。対象ビヘイビア
//! NOSQL-1・NOSQL-9。ポインタ: `docs/spec/05-tasks.md` TASK-179・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1・NOSQL-9）。
//!
//! `wire_server::http::query::op` の単体テストは crate 内部から `Op::parse`
//! を直接叩くが、本ファイルは `nosql1_endpoint_routing.rs` と同じ流儀で
//! production ルータ（[`wire_server::http::router::Router`]）へ実際に
//! `POST /v1/query` を送り、HTTP 応答（ステータス・`wire_code`・
//! エラーコード・メッセージ）を確認する。
//!
//! 受理（4 op）と語彙外拒否はいずれも HTTP 501・`wire_code` `0A000`・
//! `code` `FEATURE_NOT_SUPPORTED` で status だけでは区別できないため、
//! 必ず `error_message_of` で [`wire_server::http::query::gate::
//! PLACEHOLDER_MESSAGE`]（受理側）／[`wire_server::http::query::gate::
//! UNSUPPORTED_OP_MESSAGE`]（拒否側）を突き合わせる。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::net::SocketAddr;

use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::gate::{PLACEHOLDER_MESSAGE, UNSUPPORTED_OP_MESSAGE};
use wire_server::http::session::store::SessionStore;

/// `POST /v1/session` へログインしてトークン（base64url 表現）を取り出す。
fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match engine::json::parse_json(&String::from_utf8_lossy(&resp.body))
        .expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

/// 正常形の `POST <target>`（`Content-Type: application/json`・宣言済み
/// `Content-Length`）を送って解析済み応答を返す便宜 API。
fn post(addr: SocketAddr, target: &str, auth: Option<&str>, body: &[u8]) -> HttpResponse {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let auth_header;
    if let Some(auth) = auth {
        auth_header = format!("Bearer {auth}");
        headers.push(("Authorization", &auth_header));
    }
    let content_length = body.len().to_string();
    headers.push(("Content-Type", "application/json"));
    headers.push(("Content-Length", &content_length));
    let request = http_common::build_request(target, &headers, body);
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// テナントの Bearer トークンを都度発行し `/v1/query` へ本文を送る便宜 API
/// （セッション枠を使い切らないよう毎回新規ログインする）。
fn query(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, "/v1/query", Some(&token), body)
}

fn spawn() -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener(&users_path, SessionStore::new())
}

// --- 受理 4 op: いずれも 501・0A000・PLACEHOLDER_MESSAGE ------------------

#[test]
fn four_allowlisted_ops_reach_placeholder_response() {
    let addr = spawn();
    let bodies: [&[u8]; 4] = [
        br#"{"op":"search","table":"docs","limit":1}"#,
        br#"{"op":"scan","table":"docs","limit":1}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        br#"{"op":"insert","table":"docs","rows":[]}"#,
    ];
    for body in bodies {
        let resp = query(addr, body);
        assert_eq!(resp.status, 501, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "0A000");
        assert_eq!(http_common::error_message_of(&resp), PLACEHOLDER_MESSAGE);
    }
}

// --- 語彙外 op: DDL・UDF・トランザクション・UPDATE/DELETE・表記揺れ -------

#[test]
fn vocabulary_outside_four_ops_rejects_with_0a000_and_unsupported_message() {
    let addr = spawn();
    let unsupported_ops = [
        // DDL 相当
        "create_table",
        "alter_table",
        "drop_table",
        // UDF 呼び出し相当
        "call",
        "udf",
        // トランザクション制御相当
        "begin",
        "commit",
        "rollback",
        // UPDATE/DELETE 相当
        "update",
        "delete",
        // SQL 系・表記揺れ
        "select",
        "explain",
        "set",
        "SEARCH",
        " search",
        "",
    ];
    for op in unsupported_ops {
        let body = format!(r#"{{"op":"{op}","table":"docs"}}"#);
        let resp = query(addr, body.as_bytes());
        assert_eq!(resp.status, 501, "op={op:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "0A000", "op={op:?}");
        assert_eq!(http_common::error_code_of(&resp), "FEATURE_NOT_SUPPORTED");
        assert_eq!(
            http_common::error_message_of(&resp),
            UNSUPPORTED_OP_MESSAGE,
            "op={op:?}"
        );
        // 語彙外 op 文字列そのものを応答本文へ echo しない。
        if !op.is_empty() {
            http_common::assert_message_does_not_echo(&resp, op);
        }
    }
}

// --- 順序契約: op 判定が schema 検証より先 ---------------------------------

#[test]
fn op_allowlist_check_precedes_schema_validation_over_wire() {
    let addr = spawn();
    // 語彙外 op に加え未知キー（`hint_order`）も同時に付与しても、
    // schema 検証由来の 42601 ではなく op 許可リスト由来の 0A000 が返る。
    let body = br#"{"op":"drop_table","table":"docs","hint_order":["path"]}"#;
    let resp = query(addr, body);
    assert_eq!(resp.status, 501, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "0A000");
    assert_eq!(http_common::error_message_of(&resp), UNSUPPORTED_OP_MESSAGE);
}

// --- 未知キー（HINT ORDER・セッション変数・トランザクション/UDF 相当） ----

#[test]
fn unknown_fields_on_valid_ops_reject_with_42601() {
    let addr = spawn();
    let cases: [(&str, &str); 15] = [
        (
            "search",
            r#"{"op":"search","table":"docs","limit":1,"hint_order":["path"]}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"hint_order":["path"]}"#,
        ),
        (
            "aggregate",
            r#"{"op":"aggregate","table":"docs","aggregates":[],"hint_order":["path"]}"#,
        ),
        (
            "insert",
            r#"{"op":"insert","table":"docs","rows":[],"hint_order":["path"]}"#,
        ),
        (
            "search",
            r#"{"op":"search","table":"docs","limit":1,"hint":"x"}"#,
        ),
        (
            "search",
            r#"{"op":"search","table":"docs","limit":1,"search_mode":"precision"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"search_mode":"precision"}"#,
        ),
        (
            "aggregate",
            r#"{"op":"aggregate","table":"docs","aggregates":[],"search_mode":"precision"}"#,
        ),
        (
            "insert",
            r#"{"op":"insert","table":"docs","rows":[],"search_mode":"precision"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"set":"x"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"session":"x"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"mode":"precision"}"#,
        ),
        (
            "aggregate",
            r#"{"op":"aggregate","table":"docs","aggregates":[],"mode":"precision"}"#,
        ),
        (
            "insert",
            r#"{"op":"insert","table":"docs","rows":[],"mode":"precision"}"#,
        ),
        (
            "scan",
            r#"{"op":"scan","table":"docs","limit":1,"transaction":"begin"}"#,
        ),
    ];
    for (label, body) in cases {
        let resp = query(addr, body.as_bytes());
        assert_eq!(resp.status, 400, "label={label} body={body} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601", "label={label}");
    }
}

// --- op 欠落／非文字列／ルート非オブジェクト（回帰） -----------------------

#[test]
fn missing_or_non_string_op_and_non_object_root_reject_with_42601() {
    let addr = spawn();
    let bodies: [&[u8]; 3] = [br#"{"table":"docs"}"#, br#"{"op":1}"#, br#"[]"#];
    for body in bodies {
        let resp = query(addr, body);
        assert_eq!(resp.status, 400, "body={body:?} resp={resp:?}");
        assert_eq!(http_common::wire_code_of(&resp), "42601");
    }
}

// --- 非漏えい: いずれの応答本文もテナント ID・トークンを含まない ----------

#[test]
fn responses_do_not_leak_tenant_or_token() {
    let addr = spawn();
    let resp = query(addr, br#"{"op":"begin","table":"docs"}"#);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(!text.contains("tenant-a"));
    assert!(!text.contains("pw-alice"));
}
