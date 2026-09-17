//! セッション発行・失効・上限のライフサイクル結合テスト（Issue #756・
//! TASK-174。対象ビヘイビア HTTP-4・HTTP-5・HTTP-8。ポインタ:
//! `docs/spec/05-tasks.md` TASK-174・`docs/spec/04-behavior/http-transport.md`
//! HTTP-4, HTTP-5, HTTP-8）。
//!
//! `http4_session_issue.rs`（`POST /v1/session` 単体の入力検証・#752）・
//! `http5_query_bearer.rs`（`/v1/query` 前段 Bearer 検証・#754）・
//! `http8_session_close.rs`（`POST /v1/session/close` 単体・#753）とは
//! 対象が異なる: 本ファイルは発行トークンの**形式**（43 文字・base64url
//! アルファベット限定・32 バイト復号）・**TTL 失効**（wire 経由の `28000`
//! 収束・`issue` 時の一括回収）・**同時有効数 256 上限**（接続数上限とは
//! 独立なセッション数カウンタ）・**close ライフサイクル**（login→query→
//! close→再クエリ拒否→再 close 拒否→再 login）という、上記 3 ファイルには
//! 無い横断的なライフサイクル契約を層 A（`cargo test`・`make ci` 対象）で
//! 固定する。
//!
//! `wire_server::http::router::Router` は `Instant::now`（単調時計）を
//! 呼び出しのたびに固定で渡す（時計注入 seam を持たない）ため、TTL 系の
//! テストは `SessionStore::with_limits` の短い TTL と実際の `sleep` で
//! 観測する（production コードへの時計注入 seam 追加はスコープ外。
//! CLAUDE.md の対象外一覧参照）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::time::{Duration, Instant};

use engine::json::JsonValue;
use engine::policy::PolicyContext;
use wire_server::http::session::store::SessionStore;
use wire_server::http::session::token::{decode_base64url, SessionToken, TOKEN_BYTES};
use wire_server::limits::{MAX_CONNECTIONS, MAX_SESSIONS};

// --- 送受信ヘルパー（http4/5/8 と同じ流儀。第 4 の複製を避けるため
// http_common::{build_request, send_raw, AfterWrite, parse_single_response,
// wire_code_of, error_message_of, assert_message_does_not_echo} を再利用し、
// このファイルではログイン専用の薄いラッパーのみを追加する） --------------

/// `POST /v1/session` へログインし、成功したトークン文字列を返す
/// （失敗時は panic。ログインが成功する前提のケース向け）。
fn login(addr: std::net::SocketAddr, user: &str, password: &str) -> String {
    let resp = login_raw(addr, user, password);
    assert_eq!(
        resp.status,
        200,
        "login must succeed: body={:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let obj = json_object(&resp.body);
    match obj.get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    }
}

/// [`login`] のステータスを断定しない版（`53300` 等の拒否ケース検証用）。
fn login_raw(addr: std::net::SocketAddr, user: &str, password: &str) -> http_common::HttpResponse {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let response = http_common::send_raw(addr, &request, http_common::AfterWrite::HalfClose);
    http_common::parse_single_response(&response)
}

/// `Authorization: Bearer <token>` 付きで `/v1/query` へ最小限の `scan` を送る。
fn query_with_bearer(addr: std::net::SocketAddr, token: &str) -> http_common::HttpResponse {
    let body = br#"{"op":"scan","table":"docs","limit":1}"#;
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    let response = http_common::send_raw(addr, &request, http_common::AfterWrite::HalfClose);
    http_common::parse_single_response(&response)
}

/// `Authorization: Bearer <token>` 付きで `/v1/session/close` を送る。
fn close_with_bearer(addr: std::net::SocketAddr, token: &str) -> http_common::HttpResponse {
    let request = http_common::build_request(
        "/v1/session/close",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let response = http_common::send_raw(addr, &request, http_common::AfterWrite::HalfClose);
    http_common::parse_single_response(&response)
}

/// 有効な Bearer で到達した `/v1/query` の現時点の期待値を 1 箇所へ集約する。
/// 本 Issue 時点は `session::middleware::authenticate` を通過した要求が
/// `query::gate::handle` の暫定ゲート応答（`501`／`0A000`）へ到達する
/// （`http5_query_bearer.rs::valid_bearer_and_valid_json_reaches_placeholder_response`
/// と同じ観測点）。op 許可リストの正式化・実行結線（Issue #758・#759）で
/// この期待値が変わった際、更新箇所をこの関数だけに閉じ込める。
fn assert_query_accepted(resp: &http_common::HttpResponse) {
    assert_eq!(
        resp.status,
        501,
        "expected placeholder gate response, got body={:?}",
        String::from_utf8_lossy(&resp.body)
    );
    assert_eq!(http_common::wire_code_of(resp), "0A000");
}

fn json_object(body: &[u8]) -> std::collections::BTreeMap<String, JsonValue> {
    let text = std::str::from_utf8(body).expect("response body must be utf-8");
    match engine::json::parse_json(text).expect("response body must be valid JSON") {
        JsonValue::Object(map) => map,
        other => panic!("expected JSON object, got {other:?}"),
    }
}

/// 応答から `Date` ヘッダを除いた文字列（時刻に依存する行だけを除外した
/// バイト同一性比較のため。`http4_session_issue.rs`／`http5_query_bearer.rs`／
/// `http8_session_close.rs` と同じパターンの、解析済み [`http_common::
/// HttpResponse`] 向けの版）。
fn strip_date(resp: &http_common::HttpResponse) -> String {
    let mut out = format!("{} {}\n", resp.status, resp.reason);
    for (name, value) in &resp.headers {
        if !name.eq_ignore_ascii_case("date") {
            out.push_str(&format!("{name}: {value}\n"));
        }
    }
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&resp.body));
    out
}

/// TTL 系テストで使う短い有効期限（1.5 秒）。要求処理そのものが TTL を
/// 跨がない程度に十分短く、CI 環境のスケジューリング遅延を吸収できる
/// 程度に長い値として選んだ。
const SHORT_TTL: Duration = Duration::from_millis(1_500);
/// [`SHORT_TTL`] の 2 倍以上の余裕を持つ待機時間（期限切れを確実に観測する）。
const EXPIRY_WAIT: Duration = Duration::from_millis(3_000);

// === (A) トークン形式（HTTP-4） =============================================

#[test]
fn issued_token_is_43_char_padless_base64url_decoding_to_32_bytes() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let resp = login_raw(addr, "alice", "pw-alice");
    assert_eq!(resp.status, 200);
    let obj = json_object(&resp.body);

    // 応答本文のキーは token・expires_in の 2 つのみ（`issue.rs::success_body`
    // の契約）。
    assert_eq!(obj.len(), 2, "unexpected keys in success body: {obj:?}");

    let token = match obj.get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    assert_eq!(
        token.len(),
        43,
        "token must be exactly 43 characters: {token:?}"
    );
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "token must use only the base64url alphabet: {token:?}"
    );
    assert!(!token.contains('='), "token must not be padded: {token:?}");
    assert!(
        !token.contains('+'),
        "token must use url-safe alphabet, not '+': {token:?}"
    );
    assert!(
        !token.contains('/'),
        "token must use url-safe alphabet, not '/': {token:?}"
    );

    let decoded = decode_base64url(&token).expect("token must be valid base64url");
    assert_eq!(
        decoded.len(),
        TOKEN_BYTES,
        "decoded token must be exactly 32 bytes"
    );
    assert!(
        SessionToken::parse(&token).is_ok(),
        "token must round-trip through SessionToken::parse"
    );

    match obj.get("expires_in") {
        Some(JsonValue::Number(n)) => {
            assert_eq!(*n, wire_server::limits::SESSION_TTL.as_secs() as f64)
        }
        other => panic!("expected numeric expires_in field, got {other:?}"),
    }
}

#[test]
fn reissued_tokens_are_distinct_and_independently_valid() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let token_1 = login(addr, "alice", "pw-alice");
    let token_2 = login(addr, "alice", "pw-alice");
    assert_ne!(token_1, token_2, "reissued tokens must differ");

    // 2 回目の発行が 1 回目のトークンを無効化しない（それぞれ独立に有効）。
    assert_query_accepted(&query_with_bearer(addr, &token_1));
    assert_query_accepted(&query_with_bearer(addr, &token_2));
}

#[test]
fn non_canonical_token_forms_are_rejected_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");

    // `http8_session_close.rs::malformed_bearer_variants_reject_with_28000` は
    // 未知トークン・スキーム不一致等を扱う。本テストは「有効トークンに
    // 隣接する非正準な派生形」に絞る（重複回避）。
    let padded = format!("{token}=");
    let truncated = &token[1..];
    let mut plus_variant = token.clone();
    // 末尾から最初に置換可能な base64url 文字を '+' へ置き換える
    // （'+' は base64url アルファベット外であり、必ず拒否される）。
    plus_variant.replace_range(token.len() - 1..token.len(), "+");

    for auth_token in [padded.as_str(), truncated, plus_variant.as_str()] {
        let resp = query_with_bearer(addr, auth_token);
        assert_eq!(
            resp.status,
            401,
            "auth_token={auth_token:?}: got body={:?}",
            String::from_utf8_lossy(&resp.body)
        );
        assert_eq!(http_common::wire_code_of(&resp), "28000");
        assert_eq!(
            http_common::error_message_of(&resp),
            wire_server::http::session::bearer::MESSAGE
        );
    }

    // 元のトークンは不正提示によって消費・失効していない。
    assert_query_accepted(&query_with_bearer(addr, &token));
}

// === (B) TTL 失効（HTTP-4・HTTP-5・HTTP-8） =================================

#[test]
fn expired_token_is_rejected_on_query_and_close_with_28000_identically_to_unknown() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(
        &users_path,
        SessionStore::with_limits(MAX_SESSIONS, SHORT_TTL),
    );
    let token = login(addr, "alice", "pw-alice");

    // 発行直後は有効（TTL 1.5 秒に対し十分な余裕がある 1 回だけのプローブ）。
    assert_query_accepted(&query_with_bearer(addr, &token));

    std::thread::sleep(EXPIRY_WAIT);

    let expired_query = query_with_bearer(addr, &token);
    assert_eq!(expired_query.status, 401);
    assert_eq!(http_common::wire_code_of(&expired_query), "28000");

    let expired_close = close_with_bearer(addr, &token);
    assert_eq!(expired_close.status, 401);
    assert_eq!(http_common::wire_code_of(&expired_close), "28000");

    // 期限切れと未知トークンは `Date` を除きバイト同一（存在オラクル非公開）。
    let unknown_token = SessionToken::generate()
        .expect("generate unrelated token")
        .encoded();
    let unknown_query = query_with_bearer(addr, &unknown_token);
    let unknown_close = close_with_bearer(addr, &unknown_token);
    assert_eq!(strip_date(&expired_query), strip_date(&unknown_query));
    assert_eq!(strip_date(&expired_close), strip_date(&unknown_close));
}

#[test]
fn expired_sessions_are_swept_on_issue_without_lookup() {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let sessions = SessionStore::with_limits(1, SHORT_TTL);
    // production 経路（wire 越し）と並行して手元のハンドルで `active_sessions`
    // を観測する（`SessionStore::clone` は内部状態を共有するハンドル複製）。
    let sessions_handle = sessions.clone();
    let addr = http_common::spawn_router_listener(&users_path, sessions);

    let token_a = login(addr, "alice", "pw-alice");
    assert_eq!(sessions_handle.active_sessions(), 1);

    // 上限 1 のため、2 人目のログインは枠不足で拒否される。
    let second = login_raw(addr, "carol", "pw-carol");
    assert_eq!(second.status, 503);
    assert_eq!(http_common::wire_code_of(&second), "53300");
    assert_eq!(sessions_handle.active_sessions(), 1);

    // A のトークンには一切触れず（`lookup`／`close` を呼ばず）待機する。
    std::thread::sleep(EXPIRY_WAIT);

    // `issue` 時の枠不足回収により、A の期限切れエントリが回収されて
    // C の発行が成功する（`lookup` を経由しない一括回収の非 vacuous 証跡）。
    let token_c = login(addr, "carol", "pw-carol");
    assert_eq!(sessions_handle.active_sessions(), 1);

    assert_query_accepted(&query_with_bearer(addr, &token_c));

    let expired_a = query_with_bearer(addr, &token_a);
    assert_eq!(expired_a.status, 401);
    assert_eq!(http_common::wire_code_of(&expired_a), "28000");
}

// === (C) 同時有効数上限（HTTP-5） ===========================================

#[test]
fn session_limit_is_exactly_max_sessions_and_257th_login_is_rejected_with_53300() {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let sessions = SessionStore::new();
    // 手元のハンドルで 255 件を事前投入する（production 既定 256 の境界を
    // wire 経由の実ログイン 256 回・Argon2id 照合 256 回で埋めるのは
    // 過剰なため。`SessionStore::clone` は内部状態を共有するハンドル複製）。
    let now = Instant::now();
    for i in 0..(MAX_SESSIONS - 1) {
        sessions
            .issue(
                PolicyContext::new("tenant-a").expect("valid tenant id in test"),
                now,
            )
            .unwrap_or_else(|_| panic!("pre-seed issue #{i} must succeed"));
    }
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS - 1);

    let addr = http_common::spawn_router_listener(&users_path, sessions.clone());

    // 256 件目（wire 経由）は成功する。
    let token_256 = login(addr, "alice", "pw-alice");
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS);

    // 257 件目は枠不足で拒否され、拒否によって既存の枠が漏れない。
    let rejected = login_raw(addr, "bob", "pw-bob");
    assert_eq!(rejected.status, 503);
    assert_eq!(http_common::wire_code_of(&rejected), "53300");
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS);
    http_common::assert_message_does_not_echo(&rejected, "alice");
    http_common::assert_message_does_not_echo(&rejected, "bob");
    http_common::assert_message_does_not_echo(&rejected, "tenant-a");
    http_common::assert_message_does_not_echo(&rejected, "tenant-b");
    http_common::assert_message_does_not_echo(&rejected, &token_256);

    // 上限到達状態でも既存セッションは有効。
    assert_query_accepted(&query_with_bearer(addr, &token_256));

    // 256 件目を close すると枠が解放され、次のログインが成功する。
    let close_resp = close_with_bearer(addr, &token_256);
    assert_eq!(close_resp.status, 200);
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS - 1);

    let relogin = login_raw(addr, "bob", "pw-bob");
    assert_eq!(relogin.status, 200);
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS);

    // セッション上限（256）は接続数上限（64）より大きい別カウンタである
    // ことを固定する（`limits.rs` の production 定数。値はコンパイル時定数の
    // ため `const` ブロックで評価し clippy の
    // `assertions_on_constants` を回避する）。
    const _: () = assert!(MAX_SESSIONS == 256);
    const _: () = assert!(MAX_SESSIONS > MAX_CONNECTIONS);
    const _: () = assert!(MAX_CONNECTIONS == 64);
}

#[test]
fn session_counter_is_independent_of_connection_counter() {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let sessions = SessionStore::new();
    let now = Instant::now();
    for i in 0..MAX_SESSIONS {
        sessions
            .issue(
                PolicyContext::new("tenant-a").expect("valid tenant id in test"),
                now,
            )
            .unwrap_or_else(|_| panic!("pre-seed issue #{i} must succeed"));
    }
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS);

    let addr = http_common::spawn_router_listener(&users_path, sessions.clone());

    // このテストは各要求後に接続を閉じる（1 要求＝1 接続。`send_raw` の
    // `HalfClose` 契約）ため、接続 0 本のまま 256 セッションが共存している
    // ことがこの時点で成立している。
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS);

    // 失敗ログイン（誤パスワード。28P01）は枠を消費しない。
    let failed = login_raw(addr, "bob", "wrong-password");
    assert_eq!(failed.status, 401);
    assert_eq!(sessions.active_sessions(), MAX_SESSIONS);
}

// === (D) close ライフサイクル（HTTP-8） =====================================

#[test]
fn session_lifecycle_login_query_close_then_reject_and_relogin() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let sessions = SessionStore::new();
    let addr = http_common::spawn_router_listener(&users_path, sessions.clone());

    let token = login(addr, "alice", "pw-alice");
    assert_eq!(sessions.active_sessions(), 1);

    assert_query_accepted(&query_with_bearer(addr, &token));

    let close_resp = close_with_bearer(addr, &token);
    assert_eq!(close_resp.status, 200);
    assert_eq!(sessions.active_sessions(), 0);

    let rejected_query = query_with_bearer(addr, &token);
    assert_eq!(rejected_query.status, 401);
    assert_eq!(http_common::wire_code_of(&rejected_query), "28000");

    let rejected_close = close_with_bearer(addr, &token);
    assert_eq!(rejected_close.status, 401);
    assert_eq!(http_common::wire_code_of(&rejected_close), "28000");

    let new_token = login(addr, "alice", "pw-alice");
    assert_eq!(sessions.active_sessions(), 1);
    assert_ne!(
        new_token, token,
        "reissued token must differ from closed one"
    );

    // 旧トークンは close 後も一貫して拒否され続ける（再発行後も再消費されない）。
    let still_rejected = query_with_bearer(addr, &token);
    assert_eq!(still_rejected.status, 401);
    assert_eq!(http_common::wire_code_of(&still_rejected), "28000");

    assert_query_accepted(&query_with_bearer(addr, &new_token));
}
