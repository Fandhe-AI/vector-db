//! ERR-4 分類境界 (a)〜(f) の全類型について、`wire_code` と HTTP ステータス
//! の射影（[`wire_server::http::status::http_status`]）が実要求から誘発した
//! 応答と一致することを 1 類型 1 テストで網羅検証する層 A 結合テスト
//! （Issue #775・TASK-180。対象ビヘイビア ERR-4。ポインタ:
//! `docs/spec/05-tasks.md` TASK-180・`docs/spec/04-behavior/error-format.md`
//! ERR-4）。関連: HTTP-2/3/11/12（TASK-173）・HTTP-4〜8（TASK-174）・
//! NOSQL-1〜9（TASK-175〜179）・ERR-5（TASK-153）。
//!
//! 個々の類型はこれまで各 Issue の層 A テスト（`http2_framing.rs`・
//! `http11_limits.rs`・`http4_session*.rs`・`nosql2_search.rs` 等）に散在し、
//! 一部は `wire_code` のみを検証し HTTP ステータスまでは検証していなかった。
//! 本ファイルは「実要求で誘発 → `wire_code` と HTTP ステータスの組が射影表
//! （`status.rs::EXPECTED` と同値の [`EXPECTED_STATUS`]）と一致」を 1 箇所に
//! 集約し、あわせて「新規 `wire_code` が増えていない」ことを
//! [`err4_projection_table_is_closed_over_all_error_classes`] で機械的に
//! 固定する。production コードは変更しない（テスト専任）。
//!
//! 到達不能類型（`42501`・`P0002`）の扱い: NoSQL 表層はテナントをセッション
//! （`SessionPrincipal::policy_context()`）からのみ導出し、クライアント自己
//! 申告の `tenant_id` 相当値は JSON／ヘッダ／パスいずれの位置でも `42601` で
//! 先に拒否する（`gate.rs`・`session/middleware.rs`・`router.rs`）ため、
//! `ForbiddenTenantMismatch`（`42501`）を実要求から誘発する経路が構造的に
//! 存在しない。`RowNotFound`（`P0002`）に対応する op（更新・削除系）も
//! NoSQL 表層の許可リストに無い。これらはテナント境界の検査を緩める・
//! バイパスする production 経路を新設せず（`.claude/rules/security.md`
//! P0）、production の応答エンコーダ（`http::response::encode_error`。
//! ルータ・各 op ハンドラが実際に使う関数）を通したバイト列を実応答と同じ
//! パーサ（`http_common::parse_single_response`）で解析し、射影のみを検証
//! する（[`err4_f_unreachable_classes_project_via_production_encoder`]）。
//!
//! `data` キー付き `XX000`（緊急応答。`RECOVER-5` (3)・ERR-5 ポインタ）も
//! 同様に実要求からは到達不能: `http::response::encode_error_may_be_committed`
//! を呼び出す production 経路が `http/` 配下に存在せず、commit 後 panic は
//! RECOVER-5 の既存ガードが abort に倒すため（`http_insert_response_boundary.rs`
//! で検証済み）。通常応答が `data` キーを含まないことは
//! [`assert_projected`] が全テストで機械的に固定する。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::ErrorClass;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;
use wire_server::http::status::http_status;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            // `path`／`body` は `EngineCore::dictionary_snapshot`
            // （TASK-109・PLAN-5。`USING PLAN`／`plan` 検索が内部で要求する
            // 辞書スナップショット抽出）の non-nullable text 列要件を満たす
            // ために必要（`err4_f_internal_error_projects_xx000_to_500` が
            // `plan` 検索を誘発するため）。
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// 空の `docs` テーブルを持つスローアウェイ `EngineCore`。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("err4-http-projection");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `id` 桁あふれ（`22003`）専用フィクスチャ（`nosql4_aggregate.rs::
/// new_core_overflow` と同型）。
fn new_core_overflow() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("err4-http-projection-overflow");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant ctx");
    for id in [u64::MAX, 1] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("overflow-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text("ja".to_string()),
                Value::Text(format!("docs/{id}.md")),
                Value::Text("overflow content".to_string()),
            ],
            &op_id,
        )
        .expect("insert overflow row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `insert` の件数上限（INDEX-4）を小さく固定したスローアウェイ `EngineCore`
/// （環境変数の実値に依存せず非 vacuous に誘発するため）。
fn new_core_with_small_batch_limit(max_files: usize) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("err4-http-projection-batch-limit");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(
        BatchLimits {
            max_files_per_batch: max_files,
            ..Default::default()
        },
    );
    (Arc::new(core), guard)
}

fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core)
}

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

fn post(addr: SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
    let auth = format!("Bearer {token}");
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// `alice`（tenant-a）で都度ログインしてから要求を送る便宜 API。
fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    let token = login(addr, "alice", "pw-alice");
    post(addr, &token, body)
}

/// `status.rs::EXPECTED`（`#[cfg(test)]` 内で外部から参照不可）と同値の
/// 期待表。両者の乖離は [`err4_projection_table_is_closed_over_all_error_classes`]
/// が `http_status` 経由で検出する。
const EXPECTED_STATUS: [(&str, u16); 16] = [
    ("22000", 400),
    ("28P01", 401),
    ("28000", 401),
    ("42501", 403),
    ("42P01", 404),
    ("P0002", 404),
    ("23505", 409),
    ("23502", 400),
    ("54000", 413),
    ("53300", 503),
    ("0A000", 501),
    ("42601", 400),
    ("08P01", 400),
    ("XX000", 500),
    ("22003", 400),
    ("22023", 400),
];

/// (a)〜(f) 全類型の共通アサーション: `wire_code` が逆引き可能・射影ステータス
/// と実応答ステータスが一致・`EXPECTED_STATUS`（射影関数とは独立の期待表）
/// とも一致・`code` ラベルが分類と一致・`message` が非空・トップレベル
/// `error` オブジェクトに `data` キーが無いことを固定する。
fn assert_projected(resp: &HttpResponse, expected_wire_code: &str) {
    let wire_code = http_common::wire_code_of(resp);
    assert_eq!(
        wire_code, expected_wire_code,
        "unexpected wire_code (resp={resp:?})"
    );
    let class = ErrorClass::from_wire_code(&wire_code)
        .unwrap_or_else(|| panic!("wire_code {wire_code:?} must be a known ErrorClass"));
    assert_eq!(
        resp.status,
        http_status(class),
        "response status must match the http_status projection for {wire_code:?}"
    );
    let (_, expected_status) = EXPECTED_STATUS
        .iter()
        .find(|(code, _)| *code == expected_wire_code)
        .unwrap_or_else(|| panic!("{expected_wire_code:?} missing from EXPECTED_STATUS"));
    assert_eq!(
        resp.status, *expected_status,
        "response status must match the independent EXPECTED_STATUS table for {wire_code:?}"
    );
    assert_eq!(
        http_common::error_code_of(resp),
        class.label(),
        "error.code must match the ErrorClass label"
    );
    assert!(
        !http_common::error_message_of(resp).is_empty(),
        "error.message must not be empty"
    );
    let body_str = std::str::from_utf8(&resp.body).expect("body must be utf-8");
    let parsed = engine::json::parse_json(body_str).expect("body must be valid json");
    let engine::json::JsonValue::Object(mut top) = parsed else {
        panic!("top level must be an object: {body_str}");
    };
    let error_value = top.remove("error").expect("missing error key");
    let engine::json::JsonValue::Object(error_obj) = error_value else {
        panic!("error value must be an object: {body_str}");
    };
    assert!(
        !error_obj.contains_key("data"),
        "normal error response must not carry a data key: {body_str}"
    );
    assert_eq!(
        resp.header("content-type"),
        Some(wire_server::http::response::CONTENT_TYPE_JSON_UTF8)
    );
    assert_eq!(resp.header("connection"), Some("close"));
}

// --- R7: 射影表が ErrorClass::ALL 全体を閉じて覆うことの機械検証 -----------

const _: () = assert!(ErrorClass::ALL.len() == 16);

#[test]
fn err4_projection_table_is_closed_over_all_error_classes() {
    assert_eq!(EXPECTED_STATUS.len(), ErrorClass::ALL.len());
    for class in ErrorClass::ALL {
        let wire_code = class.wire_code();
        let (_, expected_status) = EXPECTED_STATUS
            .iter()
            .find(|(code, _)| *code == wire_code)
            .unwrap_or_else(|| panic!("{wire_code:?} missing from EXPECTED_STATUS"));
        assert_eq!(
            http_status(class),
            *expected_status,
            "http_status(class) must match EXPECTED_STATUS for {wire_code:?}"
        );
        let round_tripped = ErrorClass::from_wire_code(wire_code)
            .unwrap_or_else(|| panic!("{wire_code:?} must round-trip via from_wire_code"));
        assert_eq!(round_tripped, class);
    }
}

// --- (a) HTTP フレーミング不正 → 08P01／400 --------------------------------

#[test]
fn err4_a_malformed_framing_projects_08p01_to_400() {
    let (addr, _limiter) = http_common::spawn_http_listener(4, http_common::SERVER_READ_TIMEOUT);
    let request =
        b"GET /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n";
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        request,
        AfterWrite::HalfClose,
    ));
    http_common::assert_rejected(&resp, 400, "08P01");
    assert_projected(&resp, "08P01");
}

#[test]
fn err4_a_unknown_target_projects_08p01_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let token = login(addr, "alice", "pw-alice");
    let request = http_common::build_request(
        "/v1/unknown",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(
        http_common::error_message_of(&resp),
        http_common::ROUTER_PLACEHOLDER_MESSAGE,
        "resp={resp:?}"
    );
    assert_projected(&resp, "08P01");
}

// --- (b) JSON 構文不正・スキーマ違反 → 42601／400 --------------------------

#[test]
fn err4_b_json_syntax_error_projects_42601_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(addr, br#"{"op":"#);
    assert_projected(&resp, "42601");
}

#[test]
fn err4_b_schema_violation_projects_42601_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    // 未知キー。
    let resp = query_as_alice(addr, br#"{"op":"scan","table":"docs","limit":1,"extra":1}"#);
    assert_projected(&resp, "42601");

    // `tenant_id` キー付き（クライアント自己申告のテナント値は先頭で拒否）。
    let resp = query_as_alice(
        addr,
        br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"tenant-a"}"#,
    );
    assert_projected(&resp, "42601");
}

// --- (c) op 語彙外 → 0A000／501 ---------------------------------------------
//
// 「語彙内だが未対応」の従来サブケース（aggregate の explain: true）は
// NOSQL-10（Issue #765）の実装完了により `42601` へ写像されるようになり
// 本クラスの例として成立しなくなったため撤去した（base ブランチ取り込みに
// 伴う意味論変化。PR #832）。`engine` 未接続（`Router::new` 経由）時の
// 全 op プレースホルダー応答は `crates/wire-server/src/http/query/gate.rs`
// の単体テスト（`valid_scan_reaches_placeholder_response_when_engine_is_not_connected`
// 等）で別途固定済み。

#[test]
fn err4_c_unsupported_op_projects_0a000_to_501() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    // 語彙外 op。
    let resp = query_as_alice(addr, br#"{"op":"delete","table":"docs"}"#);
    assert_projected(&resp, "0A000");
}

// --- (d) 構造は受理されたが値不正 → 22000／22003／22023 ---------------------

#[test]
fn err4_d_invalid_value_projects_22000_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"mode":"fuzzy"}"#,
    );
    assert_projected(&resp, "22000");
}

#[test]
fn err4_d_numeric_overflow_projects_22003_to_400() {
    let (core, _guard) = new_core_overflow();
    let addr = spawn(core);
    let resp = query_as_alice(
        addr,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"id"}]}"#,
    );
    assert_projected(&resp, "22003");
}

#[test]
fn err4_d_operation_id_content_mismatch_projects_22023_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let first = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"}],"operation_id":"err4-op-d"}"#;
    let resp = query_as_alice(addr, first);
    assert_eq!(resp.status, 200, "first insert must succeed: {resp:?}");

    // 同一 operation_id・異なる内容での再送。
    let second = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.9,0.9],"lang":"ja","path":"docs/1.md","body":"changed content"}],"operation_id":"err4-op-d"}"#;
    let resp = query_as_alice(addr, second);
    assert_projected(&resp, "22023");
}

// --- (e) 上限超過 → 54000／413 ----------------------------------------------

#[test]
fn err4_e_body_over_limit_projects_54000_to_413() {
    let (addr, _limiter) = http_common::spawn_http_listener(4, http_common::SERVER_READ_TIMEOUT);
    let over_limit = wire_server::http::body::MAX_BODY_LEN + 1;
    let request = format!(
        "POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {over_limit}\r\n\r\n"
    )
    .into_bytes();
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::KeepOpen,
    ));
    http_common::assert_rejected(&resp, 413, "54000");
    assert_projected(&resp, "54000");
}

#[test]
fn err4_e_filter_count_over_limit_projects_54000_to_413() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let mut items_json = String::from("[");
    for i in 0..=engine::declarative_filter::MAX_METADATA_FILTERS {
        if i > 0 {
            items_json.push(',');
        }
        items_json.push_str(&format!(r#"{{"column":"lang","op":"eq","value":"v{i}"}}"#));
    }
    items_json.push(']');
    let body = format!(r#"{{"op":"scan","table":"docs","limit":10,"filter":{items_json}}}"#);
    let resp = query_as_alice(addr, body.as_bytes());
    assert_projected(&resp, "54000");
}

#[test]
fn err4_e_insert_batch_over_limit_projects_54000_to_413() {
    let (core, _guard) = new_core_with_small_batch_limit(2);
    let addr = spawn(core);

    let body = br#"{"op":"insert","table":"docs","rows":[
        {"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"},
        {"id":2,"embedding":[0.2,0.3],"lang":"ja","path":"docs/2.md","body":"beta content"},
        {"id":3,"embedding":[0.3,0.4],"lang":"ja","path":"docs/3.md","body":"gamma content"}
        ],"operation_id":"err4-op-e-batch"}"#;
    let resp = query_as_alice(addr, body);
    assert_projected(&resp, "54000");
}

// --- (f) その他固定分類 ------------------------------------------------------

#[test]
fn err4_f_auth_required_projects_28000_to_401() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_projected(&resp, "28000");
}

#[test]
fn err4_f_auth_invalid_projects_28p01_to_401() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let body = br#"{"user":"alice","password":"wrong-password"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_projected(&resp, "28P01");
    // 誤資格の応答が実パスワードを反映しないこと（`.claude/rules/security.md`
    // の非漏えい方針）。
    http_common::assert_message_does_not_echo(&resp, "wrong-password");
}

#[test]
fn err4_f_undefined_table_projects_42p01_to_404() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(addr, br#"{"op":"scan","table":"missing","limit":1}"#);
    assert_projected(&resp, "42P01");
}

#[test]
fn err4_f_duplicate_operation_id_projects_23505_to_409() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"}],"operation_id":"err4-op-f-dup"}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "first insert must succeed: {resp:?}");

    // 同一 operation_id・同一内容での再送。
    let resp = query_as_alice(addr, body);
    assert_projected(&resp, "23505");
    http_common::assert_message_does_not_echo(&resp, "tenant-a");
}

#[test]
fn err4_f_missing_operation_id_projects_23502_to_400() {
    let (core, _guard) = new_core();
    let addr = spawn(core);

    let missing =
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2],"lang":"ja","path":"docs/1.md","body":"alpha content"}]}"#;
    let resp = query_as_alice(addr, missing);
    assert_projected(&resp, "23502");

    let null_id = br#"{"op":"insert","table":"docs","rows":[{"id":2,"embedding":[0.1,0.2],"lang":"ja","path":"docs/2.md","body":"beta content"}],"operation_id":null}"#;
    let resp = query_as_alice(addr, null_id);
    assert_projected(&resp, "23502");

    let empty_id = br#"{"op":"insert","table":"docs","rows":[{"id":3,"embedding":[0.1,0.2],"lang":"ja","path":"docs/3.md","body":"gamma content"}],"operation_id":""}"#;
    let resp = query_as_alice(addr, empty_id);
    assert_projected(&resp, "23502");
}

#[test]
fn err4_f_connection_limit_projects_53300_to_503() {
    let (addr, limiter) = http_common::spawn_http_listener(1, Duration::from_secs(5));

    let held = std::net::TcpStream::connect(addr).expect("connect within capacity");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while limiter.active() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the held connection to be accepted"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // 上限超過接続は要求を読まずに 503 を返して即座にクローズする
    // （`http/listener.rs::accept_loop_with_handler` の拒否経路）。要求を
    // 書き込むと、既に応答してクローズ済みのソケットへの書き込みが
    // カーネル RST を誘発し、実装によっては未読の応答バイト列が失われて
    // `send_raw` が panic しうる（`tests/http_limits.rs` と同じ理由で
    // 「接続するだけで書き込まない」形にする）。
    let mut extra =
        std::net::TcpStream::connect(addr).expect("connect the connection-over-capacity socket");
    extra
        .set_read_timeout(Some(http_common::CLIENT_READ_TIMEOUT))
        .expect("set client read timeout");
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match std::io::Read::read(&mut extra, &mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) => panic!("unexpected read error while waiting for 503 response: {e:?}"),
        }
    }
    let resp = http_common::parse_single_response(&received);
    assert_projected(&resp, "53300");
    drop(held);
}

#[test]
fn err4_f_session_limit_projects_53300_to_503() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(
        &users_path,
        SessionStore::with_limits(1, Duration::from_secs(3600)),
    );

    let first = login(addr, "alice", "pw-alice");
    assert!(!first.is_empty());

    let body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_projected(&resp, "53300");
}

#[test]
fn err4_f_internal_error_projects_xx000_to_500() {
    let (core, _guard) = new_core();
    let addr = spawn(core);
    let resp = query_as_alice(
        addr,
        br#"{"op":"search","table":"docs","plan":"find content","limit":10}"#,
    );
    assert_projected(&resp, "XX000");
}

/// `42501`（テナント越境）・`P0002`（行不在）は NoSQL 表層の実要求からは
/// 構造的に到達不能（本ファイル冒頭 doc 参照）。要求駆動ではなく、
/// production の応答エンコーダ（[`wire_server::http::response::encode_error`]。
/// ルータ・各 op ハンドラが実際に使う関数）を通したバイト列を実応答と同じ
/// パーサで解析し、射影のみを検証する。
#[test]
fn err4_f_unreachable_classes_project_via_production_encoder() {
    for class in [ErrorClass::ForbiddenTenantMismatch, ErrorClass::RowNotFound] {
        let raw =
            wire_server::http::response::encode_error(class, "test message", SystemTime::now());
        let resp = http_common::parse_single_response(&raw);
        assert_projected(&resp, class.wire_code());
    }
}
