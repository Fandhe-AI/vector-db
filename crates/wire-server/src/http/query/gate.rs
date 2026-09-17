//! `POST /v1/query` の認証後ゲート本体（Issue #754・TASK-175。対象ビヘイビア
//! HTTP-5・HTTP-6・HTTP-7・NOSQL-1〜NOSQL-10。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`）。
//!
//! 呼び出し文脈: [`crate::http::router::Router`] が
//! [`crate::http::session::middleware::authenticate`] を通過させた要求
//! （[`crate::http::session::middleware::SessionPrincipal`]）に対してのみ
//! [`handle`] を呼ぶ。本モジュールはヘッダ・JSON からテナントを読む経路を
//! シグネチャ上持たない（`principal` が唯一のテナント文脈の入口）。
//!
//! ## 手順（fail-closed。順序は契約の一部）
//!
//! 1. [`crate::http::session::middleware::reject_tenant_headers`]（`tenant_id`
//!    相当ヘッダの拒否。`42601`）
//! 2. 本文検証: [`crate::http::body::body_as_utf8`] → `engine::json::
//!    parse_json` → [`crate::http::query::schema::extract_op`] →
//!    [`crate::http::query::schema::schema_for`] →
//!    `ObjectSchema::validate`（必須欠落・未知キー・型不一致 → `42601`。
//!    `tenant_id` の JSON 自己申告はここで未知キーとして自然に拒否される）
//! 3. 語彙外 op（`schema_for` が `None`）は暫定 `0A000`
//!    （[`UNSUPPORTED_OP_MESSAGE`]。正式な許可リスト分類は #759 の担当）
//! 4. ここまで通過した要求は暫定的に `0A000`／501
//!    （[`PLACEHOLDER_MESSAGE`]）を返す（束縛・実行の結線は #763 以降が
//!    本 seam を置き換える）
//!
//! 応答本文・ログにテナント ID・トークンを含めない
//! （`.claude/rules/security.md`「エラー・ログ経由で他テナントのデータ・
//! 存在情報を漏らさない」）。

use std::time::SystemTime;

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::parse_json;

use crate::http::query::schema::schema_for;
use crate::http::session::middleware::{self, SessionPrincipal};
use crate::http::{body, response};

/// 検証を通過した要求に返す暫定応答の文言（束縛・実行は #759／#763
/// 以降の担当。本 Issue 時点は seam のみ）。
pub const PLACEHOLDER_MESSAGE: &str = "query execution not yet available";

/// 語彙外の `op`（[`schema_for`] が `None`）に返す暫定文言。正式な op
/// 許可リスト・DDL／UDF／トランザクション相当の分類は #759 が本 seam を
/// 置き換える。
pub const UNSUPPORTED_OP_MESSAGE: &str = "unsupported op";

/// [`handle`] 内部の分類済みエラー。
struct HandleError {
    class: ErrorClass,
    message: std::borrow::Cow<'static, str>,
}

impl HandleError {
    fn new(class: ErrorClass, message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

/// `POST /v1/query` を処理し応答バイト列を返す（認証済み要求のみ）。
///
/// `principal` は [`crate::http::session::middleware::authenticate`] を通過
/// した要求のテナント文脈（唯一の入口）。`now_wall` は応答の `Date` ヘッダ
/// （壁時計）に使う。
pub fn handle(
    principal: &SessionPrincipal,
    headers: &crate::http::headers::Headers<'_>,
    body: &[u8],
    now_wall: SystemTime,
) -> Vec<u8> {
    match handle_inner(principal, headers, body) {
        Ok(message) => response::encode_error(ErrorClass::FeatureNotSupported, message, now_wall),
        Err(e) => response::encode_error(e.class, &e.message, now_wall),
    }
}

/// [`handle`] の本体。成功時も本 Issue 時点では常に `0A000`／501 の暫定応答
/// メッセージを返す（`Ok` の意味は「ここまでの検証を通過した」こと）。
fn handle_inner(
    _principal: &SessionPrincipal,
    headers: &crate::http::headers::Headers<'_>,
    raw_body: &[u8],
) -> Result<&'static str, HandleError> {
    // 手順 1: テナント指定ヘッダの拒否（本文検証より先）。
    middleware::reject_tenant_headers(headers)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // 手順 2: 本文検証。
    let text = body::body_as_utf8(raw_body)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;
    let value =
        parse_json(text).map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;
    let op = crate::http::query::schema::extract_op(&value)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    let Some(schema) = schema_for(op) else {
        // 手順 3: 語彙外 op。正式な許可リスト分類・0A000 判定は #759。
        return Err(HandleError::new(
            ErrorClass::FeatureNotSupported,
            UNSUPPORTED_OP_MESSAGE,
        ));
    };
    schema
        .validate(&value)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // 手順 4: 検証を通過した要求への暫定 placeholder 応答。
    Ok(PLACEHOLDER_MESSAGE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::headers::{parse_headers, HeaderParse};
    use crate::http::session::store::SessionStore;
    use engine::policy::PolicyContext;
    use std::time::Instant;

    fn leak(v: Vec<u8>) -> &'static [u8] {
        Box::leak(v.into_boxed_slice())
    }

    fn headers_with_body(body: &[u8], extra: &[&str]) -> crate::http::headers::Headers<'static> {
        let mut input = Vec::new();
        for line in extra {
            input.extend_from_slice(line.as_bytes());
        }
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        match parse_headers(leak(input)).expect("header parse should succeed") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn principal() -> SessionPrincipal {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let ctx = PolicyContext::new("tenant-a").expect("valid ctx");
        let token = sessions.issue(ctx, now).expect("issue");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let mut input = raw.into_bytes();
        input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        let headers = match parse_headers(leak(input)).expect("header parse") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        };
        middleware::authenticate(&sessions, &headers, move || now).expect("authenticate")
    }

    fn run(body: &[u8], extra_headers: &[&str]) -> Vec<u8> {
        let p = principal();
        let headers = headers_with_body(body, extra_headers);
        handle(&p, &headers, body, std::time::SystemTime::now())
    }

    #[test]
    fn valid_scan_reaches_placeholder_response() {
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_search_reaches_placeholder_response() {
        let body = br#"{"op":"search","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_aggregate_reaches_placeholder_response() {
        let body =
            br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_insert_reaches_placeholder_response() {
        let body = br#"{"op":"insert","table":"docs","rows":[]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn tenant_id_in_json_is_rejected_for_all_four_ops() {
        let cases: [&[u8]; 4] = [
            br#"{"op":"search","table":"docs","limit":1,"tenant_id":"evil"}"#,
            br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#,
            br#"{"op":"aggregate","table":"docs","aggregates":[],"tenant_id":"evil"}"#,
            br#"{"op":"insert","table":"docs","rows":[],"tenant_id":"evil"}"#,
        ];
        for body in cases {
            let response = run(body, &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
            assert!(text.contains("42601"), "got: {text}");
        }
    }

    #[test]
    fn unsupported_op_rejects_with_0a000() {
        let body = br#"{"op":"select","table":"docs"}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(UNSUPPORTED_OP_MESSAGE), "got: {text}");
    }

    #[test]
    fn missing_op_rejects_with_42601() {
        let body = br#"{"table":"docs"}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn non_object_body_rejects_with_42601() {
        let body = br#"[]"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn invalid_json_rejects_with_42601() {
        let body = b"not json";
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn non_utf8_body_rejects_with_42601() {
        let body: &[u8] = &[0xff, 0xfe, 0xfd];
        let response = run(body, &[]);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn tenant_header_rejects_before_body_validation() {
        // 本文が不正でも tenant ヘッダの拒否文言が先に返る（順序: ヘッダ検査
        // が本文検証より前）。
        let body = b"not json at all";
        let response = run(body, &["X-Tenant-Id: other\r\n"]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
        assert!(
            text.contains("tenant context is derived from the session"),
            "got: {text}"
        );
    }

    #[test]
    fn response_does_not_leak_tenant_id() {
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8_lossy(&response);
        assert!(!text.contains("tenant-a"));
    }
}
