//! `POST /v1/session/close` のワンタイム失効パイプライン本体（Issue #753・
//! TASK-174・HTTP-8。関連 HTTP-5・HTTP-6。ポインタ: `docs/spec/05-tasks.md`
//! TASK-174・`docs/spec/04-behavior/http-transport.md` HTTP-8）。
//!
//! 呼び出し文脈: [`crate::http::router::Router`] が `target ==
//! "/v1/session/close"` を一致させたときにのみ [`handle`] を呼ぶ。
//! [`crate::http::session::issue::handle`] と同型の純関数として実装する
//! （ソケット I/O を持たず、ヘッダ・本文バイト列・時刻 2 種だけを受け取り
//! 応答バイト列を返す。直接単体テストできるようにするための設計）。
//!
//! 手順（fail-closed。順序は契約の一部）:
//! 1. [`crate::http::session::bearer::extract_bearer_token`] で
//!    `Authorization: Bearer` を検証し [`crate::http::session::token::
//!    SessionToken`] を取り出す（失敗はすべて `28000`）。**本文を読む前に
//!    行う**——本ハンドラ内の本文検証（UTF-8・JSON・[`CLOSE_REQUEST_SCHEMA`]
//!    のスキーマ）に限り、不正な本文を送りつけて `42601` に落とすことで
//!    トークンの有無を推測させない（AC 2 の「Bearer 欠落は常に `28000`」を
//!    本文の正しさに依存させない）。接続ハンドラ層（Issue #742。
//!    `Content-Type` 不一致は `08P01`・`Content-Length` 上限超過は
//!    `54000`）はこのハンドラより前段でルーティング前に確定するため、
//!    その場合は Bearer 欠落であっても `28000` にはならず別コードで
//!    先に落ちる契約であり、この保証の対象外
//! 2. 本文を検証する。空本文（`Content-Length: 0`）、または
//!    [`CLOSE_REQUEST_SCHEMA`]（フィールドを 1 つも持たない
//!    [`crate::http::query::schema::ObjectSchema`]）を満たす JSON
//!    オブジェクトのみを受理する（違反は `42601`）
//! 3. [`crate::http::session::store::SessionStore::close`] を 1 回だけ呼ぶ
//!    （`lookup` → `close` の 2 段にしない。ロック 2 回取得・TOCTOU の回避）。
//!    `false`（未知・期限切れ・二重 close のいずれも区別しない）は `28000`
//! 4. 成功時は `200 OK`（[`SUCCESS_BODY`]）
//!
//! 不正本文で誤ってトークンを消費しないよう、本文検証は `SessionStore::close`
//! より前に完了させる（手順 2 → 3 の順序）。
//!
//! いずれの応答にもテナント ID・トークンを含めない
//! （`.claude/rules/security.md`「エラー・ログ経由で他テナントのデータ・
//! 存在情報を漏らさない」）。

use std::time::{Instant, SystemTime};

use engine::error_format::ErrorClass;
use engine::json::parse_json;

use crate::http::query::schema::ObjectSchema;
use crate::http::session::bearer::{self, BearerError};
use crate::http::session::store::SessionStore;
use crate::http::{body, response};

/// `POST /v1/session/close` の要求本文スキーマ: フィールドを 1 つも持たない
/// （空オブジェクトのみ許可。未知キーはいずれも [`crate::http::query::
/// schema::ObjectSchema`] の一般則により拒否される）。
static CLOSE_REQUEST_SCHEMA: ObjectSchema = ObjectSchema {
    name: "session_close",
    fields: &[],
};

/// 成功応答の本文（フィールド名は本 Issue の実装既定値。spec 側への申し送り
/// 事項として PR 本文に記載する）。
const SUCCESS_BODY: &str = "{\"closed\":true}";

/// [`handle`] 内部の分類済みエラー（`ErrorClass` ＋ クライアント向け文言）。
/// [`crate::http::session::issue::handle`] と同じ設計（`Cow<'static, str>`
/// で固定文言・SSOT 由来の文言の双方を複製なしに扱う）。
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

/// `POST /v1/session/close` を処理し応答バイト列を返す。
/// [`crate::http::router::Router`] から呼ばれる唯一の入口。
///
/// `now_mono` は [`SessionStore::close`] の TTL 判定に使う単調時計のサンク。
/// `now_wall` は応答の `Date` ヘッダ（壁時計）に使う。production は
/// `Instant::now`（関数参照）／`SystemTime::now()` を渡し、決定的単体テストは
/// 固定時刻を注入できる。
pub fn handle(
    sessions: &SessionStore,
    headers: &crate::http::headers::Headers<'_>,
    body: &[u8],
    now_mono: impl Fn() -> Instant,
    now_wall: SystemTime,
) -> Vec<u8> {
    match handle_inner(sessions, headers, body, now_mono) {
        Ok(()) => response::encode_ok(SUCCESS_BODY, now_wall),
        Err(e) => response::encode_error(e.class, &e.message, now_wall),
    }
}

/// [`handle`] の本体。応答バイト列の組み立て（`now_wall` を要する）は
/// 呼び出し元へ委ねる（ソケット I/O・時刻参照を持たない純粋な処理段として
/// 単体テストしやすくするための分離）。
fn handle_inner(
    sessions: &SessionStore,
    headers: &crate::http::headers::Headers<'_>,
    raw_body: &[u8],
    now_mono: impl Fn() -> Instant,
) -> Result<(), HandleError> {
    // 手順 1: Bearer 検証を本文検証より前に行う（本文の正否に Bearer 欠落の
    // 応答が影響されないようにする）。
    let token = bearer::extract_bearer_token(headers)
        .map_err(|e: BearerError| HandleError::new(e.error_class(), e.client_message()))?;

    // 手順 2: 本文検証。空本文はスキーマ検証を経由せず受理する
    // （`parse_json("")` は構文的に不正な入力として拒否されるため、空本文の
    // 分岐をパースより前に明示する必要がある）。
    if !raw_body.is_empty() {
        let text = body::body_as_utf8(raw_body)
            .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;
        let value = parse_json(text).map_err(|e| {
            use engine::error_format::ClassifiedError;
            HandleError::new(e.error_class(), e.client_message())
        })?;
        CLOSE_REQUEST_SCHEMA.validate(&value).map_err(|_| {
            HandleError::new(
                ErrorClass::UnsupportedSqlSyntax,
                "invalid session close request",
            )
        })?;
    }

    // 手順 3: `lookup` を経由せず `close` を 1 回だけ呼ぶ（ロック 2 回取得・
    // TOCTOU の回避）。未知・期限切れ・二重 close のいずれも区別せず
    // `28000` へ集約する（`bearer` モジュールと同じ存在オラクル非公開方針）。
    if sessions.close(&token, now_mono()) {
        Ok(())
    } else {
        Err(HandleError::new(ErrorClass::AuthRequired, bearer::MESSAGE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::headers::{parse_headers, HeaderParse};
    use crate::http::session::token::SessionToken;
    use engine::policy::PolicyContext;

    fn leak(v: Vec<u8>) -> &'static [u8] {
        Box::leak(v.into_boxed_slice())
    }

    /// `Authorization` ヘッダ（任意）＋ `body` から `Headers` を組み立てる。
    fn headers_with_auth(
        auth: Option<&str>,
        body: &[u8],
    ) -> crate::http::headers::Headers<'static> {
        let mut input = Vec::new();
        if let Some(auth) = auth {
            input.extend_from_slice(format!("Authorization: {auth}\r\n").as_bytes());
        }
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        match parse_headers(leak(input)).expect("header parse should succeed") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id in test")
    }

    fn run(sessions: &SessionStore, auth: Option<&str>, body: &[u8], now_mono: Instant) -> Vec<u8> {
        let headers = headers_with_auth(auth, body);
        handle(
            sessions,
            &headers,
            body,
            move || now_mono,
            SystemTime::now(),
        )
    }

    #[test]
    fn valid_token_closes_with_200_and_removes_session() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");

        let auth = format!("Bearer {}", token.encoded());
        let response = run(&sessions, Some(&auth), b"", now);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 200 "), "got: {text}");
        assert!(text.contains("\"closed\":true"), "got: {text}");
        assert!(sessions.lookup(&token, now).is_none());
        assert_eq!(sessions.active_sessions(), 0);
    }

    #[test]
    fn double_close_rejects_second_attempt_with_28000() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        let auth = format!("Bearer {}", token.encoded());

        let first = run(&sessions, Some(&auth), b"", now);
        assert!(String::from_utf8_lossy(&first).starts_with("HTTP/1.1 200 "));

        let second = run(&sessions, Some(&auth), b"", now);
        let text = String::from_utf8(second).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28000"), "got: {text}");
    }

    #[test]
    fn missing_authorization_rejects_with_28000() {
        let sessions = SessionStore::new();
        let now = Instant::now();

        let response = run(&sessions, None, b"", now);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28000"), "got: {text}");
    }

    #[test]
    fn unknown_token_rejects_with_28000() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let unknown = SessionToken::generate().expect("generate unrelated token");
        let auth = format!("Bearer {}", unknown.encoded());

        let response = run(&sessions, Some(&auth), b"", now);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28000"), "got: {text}");
    }

    #[test]
    fn expired_token_rejects_with_28000() {
        let sessions = SessionStore::with_limits(4, std::time::Duration::from_secs(60));
        let base = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), base).expect("issue");
        let auth = format!("Bearer {}", token.encoded());

        let after_ttl = base + std::time::Duration::from_secs(61);
        let response = run(&sessions, Some(&auth), b"", after_ttl);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28000"), "got: {text}");
    }

    #[test]
    fn missing_unknown_close_and_expired_responses_are_byte_identical_except_date() {
        let sessions = SessionStore::with_limits(4, std::time::Duration::from_secs(60));
        let base = Instant::now();

        let missing = run(&sessions, None, b"", base);

        let unknown_token = SessionToken::generate().expect("generate unrelated token");
        let unknown_auth = format!("Bearer {}", unknown_token.encoded());
        let unknown = run(&sessions, Some(&unknown_auth), b"", base);

        let closed_token = sessions.issue(ctx("tenant-a"), base).expect("issue");
        let closed_auth = format!("Bearer {}", closed_token.encoded());
        let first_close = run(&sessions, Some(&closed_auth), b"", base);
        assert!(String::from_utf8_lossy(&first_close).starts_with("HTTP/1.1 200 "));
        let double_close = run(&sessions, Some(&closed_auth), b"", base);

        let expiring_token = sessions.issue(ctx("tenant-a"), base).expect("issue");
        let expiring_auth = format!("Bearer {}", expiring_token.encoded());
        let after_ttl = base + std::time::Duration::from_secs(61);
        let expired = run(&sessions, Some(&expiring_auth), b"", after_ttl);

        fn strip_date(bytes: &[u8]) -> String {
            String::from_utf8_lossy(bytes)
                .lines()
                .filter(|line| !line.starts_with("Date: "))
                .collect::<Vec<_>>()
                .join("\n")
        }

        let baseline = strip_date(&missing);
        assert_eq!(baseline, strip_date(&unknown));
        assert_eq!(baseline, strip_date(&double_close));
        assert_eq!(baseline, strip_date(&expired));
    }

    #[test]
    fn empty_body_and_empty_object_are_both_accepted() {
        let sessions = SessionStore::new();
        let now = Instant::now();

        let token_a = sessions.issue(ctx("tenant-a"), now).expect("issue a");
        let auth_a = format!("Bearer {}", token_a.encoded());
        let empty_body_response = run(&sessions, Some(&auth_a), b"", now);
        assert!(String::from_utf8_lossy(&empty_body_response).starts_with("HTTP/1.1 200 "));

        let token_b = sessions.issue(ctx("tenant-a"), now).expect("issue b");
        let auth_b = format!("Bearer {}", token_b.encoded());
        let empty_object_response = run(&sessions, Some(&auth_b), b"{}", now);
        assert!(String::from_utf8_lossy(&empty_object_response).starts_with("HTTP/1.1 200 "));
    }

    #[test]
    fn non_empty_object_rejects_with_42601_without_consuming_token() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        let auth = format!("Bearer {}", token.encoded());

        let bad = run(&sessions, Some(&auth), br#"{"x":1}"#, now);
        let text = String::from_utf8(bad).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");

        // 不正本文でトークンを消費していないことを確認する（続けて正しい
        // close が 200 になる）。
        let good = run(&sessions, Some(&auth), b"", now);
        assert!(String::from_utf8_lossy(&good).starts_with("HTTP/1.1 200 "));
    }

    #[test]
    fn array_body_rejects_with_42601() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        let auth = format!("Bearer {}", token.encoded());

        let response = run(&sessions, Some(&auth), b"[]", now);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn non_json_body_rejects_with_42601() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        let auth = format!("Bearer {}", token.encoded());

        let response = run(&sessions, Some(&auth), b"not json", now);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn response_does_not_leak_tenant_id_or_token() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions
            .issue(ctx("super-secret-tenant"), now)
            .expect("issue");
        let encoded = token.encoded();
        let auth = format!("Bearer {encoded}");

        let response = run(&sessions, Some(&auth), b"", now);
        let text = String::from_utf8_lossy(&response);
        assert!(!text.contains("super-secret-tenant"));
        assert!(!text.contains(&encoded));
    }
}
