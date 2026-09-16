//! `POST /v1/session` の発行パイプライン本体（Issue #752・TASK-174・
//! HTTP-4・HTTP-6。関連 HTTP-5・HTTP-7。ポインタ: `docs/spec/05-tasks.md`
//! TASK-174・`docs/spec/04-behavior/http-transport.md` HTTP-4・HTTP-6）。
//!
//! 呼び出し文脈: [`crate::http::router::Router`]（新設ルータ）が `target ==
//! "/v1/session"` を一致させたときにのみ [`handle`] を呼ぶ。本モジュールは
//! ソケット I/O を一切持たず、本文バイト列（[`crate::http::conn::Request::body`]
//! 由来）と時刻 2 種（単調時計・壁時計）だけを受け取り、応答バイト列
//! （[`Vec<u8>`]）を返す純粋な関数として実装する（`handle` を直接単体テストできる
//! ようにするための設計）。
//!
//! 手順（fail-closed。順序は契約の一部）:
//! 1. [`crate::http::body::body_as_utf8`] で本文を UTF-8 文字列へ昇格（不正なら
//!    `42601`）
//! 2. [`engine::json::parse_json`] で構文解析（不正なら `42601`）
//! 3. [`SESSION_REQUEST_SCHEMA`]（`user`・`password` の 2 つの必須文字列
//!    フィールドのみを許可する [`crate::http::query::schema::ObjectSchema`]）で
//!    意味検証。必須欠落・未知キー・型不一致はいずれも `42601`（未知キーの拒否
//!    により、クライアント自己申告の `tenant_id` 等も無視せず拒否する）
//! 4. [`crate::auth::verify`] で Argon2id 照合。失敗は `28P01`（HTTP 401）で、
//!    固定遅延・ダミー KDF による対称性は `verify` 内部が担う（本モジュールは
//!    追加の `sleep` を行わない）
//! 5. 認証成功後にのみ [`SessionStore::issue`] を呼びトークンを発行する
//!    （認証成功より前にセッション上限の判定を行わない。未認証クライアントへ
//!    KDF を経ない高速経路・セッション数のオラクルを与えないための順序）。
//!    上限超過等は [`IssueError::error_class`] が返す分類（`53300`／`XX000`）
//!    へ写像する
//! 6. 成功時は `{"token":"<...>","expires_in":<SESSION_TTL の秒数>}` を
//!    `200 OK` で返す
//!
//! いずれの応答（成功・失敗）にもテナント ID・ユーザー名を含めない
//! （security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」）。

use std::time::{Instant, SystemTime};

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::{parse_json, JsonValue};

use crate::auth::{self, UserStore};
use crate::http::error_body::escape_json_string_into;
use crate::http::query::schema::{FieldSpec, FieldType, ObjectSchema, Presence};
use crate::http::response;
use crate::http::session::store::SessionStore;
use crate::http::{body, session};
use crate::limits::SESSION_TTL;

/// `POST /v1/session` の要求本文スキーマ: `user`・`password` の 2 つの必須
/// 文字列フィールドのみを許可する（未知キーは [`crate::http::query::schema`]
/// の一般則により拒否される。`nullable: false` が既定のため `null` も拒否）。
static SESSION_REQUEST_SCHEMA: ObjectSchema = ObjectSchema {
    name: "session",
    fields: &[
        FieldSpec {
            key: "user",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "password",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// [`handle`] 内部の分類済みエラー（`ErrorClass` ＋ クライアント向け文言）。
/// `message` を `Cow<'static, str>` にするのは、[`engine::json::JsonError::
/// client_message`] のように文言を `String` で返す型（SSOT）をそのまま
/// 使い、固定文言の複製を作らないため（`Owned` は engine 側の SSOT 由来、
/// `Borrowed` は本モジュールが自前で決める固定英語文言）。[`handle`] が
/// 応答バイト列へ写像する唯一の消費者。
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

/// `POST /v1/session` を処理し応答バイト列を返す。[`crate::http::router::Router`]
/// から呼ばれる唯一の入口。
///
/// `now_mono` は [`SessionStore::issue`] の TTL 起点（単調時計）、`now_wall` は
/// 応答の `Date` ヘッダ（壁時計）に使う。呼び出し元が両方とも `Instant::now()`／
/// `SystemTime::now()` を渡す（本関数自身は時刻を参照しない。決定的単体テストの
/// ための注入 seam）。
pub fn handle(
    users: &UserStore,
    sessions: &SessionStore,
    body: &[u8],
    now_mono: Instant,
    now_wall: SystemTime,
) -> Vec<u8> {
    match handle_inner(users, sessions, body, now_mono) {
        Ok(token_encoded) => response::encode_ok(&success_body(&token_encoded), now_wall),
        Err(e) => response::encode_error(e.class, &e.message, now_wall),
    }
}

/// [`handle`] の本体。成功時はトークンの base64url 表現（`String`）のみを返し、
/// 応答バイト列の組み立て（`now_wall` を要する）は呼び出し元へ委ねる
/// （ソケット I/O・時刻参照を持たない純粋な処理段として単体テストしやすくする
/// ための分離）。
fn handle_inner(
    users: &UserStore,
    sessions: &SessionStore,
    raw_body: &[u8],
    now_mono: Instant,
) -> Result<String, HandleError> {
    let text = body::body_as_utf8(raw_body)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    let value: JsonValue =
        parse_json(text).map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // `SchemaError::client_message()`（`missing required field: <key>`／
    // `type mismatch for field: <key>` 等）はフィールド名を含むため、本
    // エンドポイントでは意図的に使わず固定の一般文言へ丸める（`user`／
    // `password` は認証情報に近く、どの必須フィールドが欠けている／型が
    // 違うかを未認証の呼び出し元へ細分化して開示しない判断。`wire_code`＝
    // `42601` は共有する）。
    let validated = SESSION_REQUEST_SCHEMA.validate(&value).map_err(|_| {
        HandleError::new(ErrorClass::UnsupportedSqlSyntax, "invalid session request")
    })?;

    let user = validated.required_str("user").map_err(|_| {
        HandleError::new(ErrorClass::UnsupportedSqlSyntax, "invalid session request")
    })?;
    let password = validated.required_str("password").map_err(|_| {
        HandleError::new(ErrorClass::UnsupportedSqlSyntax, "invalid session request")
    })?;

    // 認証（固定遅延・ダミー KDF による対称性は `auth::verify` 内部が担う。
    // ここでは早期 return・別 sleep を追加しない）。
    let ctx = auth::verify(users, user, password.as_bytes())
        .map_err(|_| HandleError::new(ErrorClass::AuthInvalid, auth::AuthFailure::MESSAGE))?;

    // セッション上限の判定は認証成功後にのみ行う（未認証クライアントへ
    // KDF を経ない高速経路・セッション数のオラクルを与えないための順序）。
    let token = sessions
        .issue(ctx, now_mono)
        .map_err(|e| HandleError::new(e.error_class(), issue_error_message(&e)))?;

    Ok(token.encoded())
}

/// [`session::store::IssueError`] → クライアント向け固定英語文言（内部詳細を
/// 反映しない。TokenGeneration の `io::Error` 詳細もここでは使わない）。
fn issue_error_message(e: &session::store::IssueError) -> &'static str {
    match e {
        session::store::IssueError::LimitExceeded => "session limit exceeded",
        session::store::IssueError::TokenGeneration(_) | session::store::IssueError::Internal => {
            "internal error"
        }
    }
}

/// 成功本文（`{"token":"...","expires_in":<秒数>}`）を組み立てる。トークンは
/// base64url アルファベットのみで JSON エスケープ不要な文字集合だが、
/// 将来の表現変更への防御として [`escape_json_string_into`] を通す。
fn success_body(token_encoded: &str) -> String {
    let mut out = String::with_capacity(64);
    out.push('{');
    out.push_str("\"token\":\"");
    escape_json_string_into(&mut out, token_encoded);
    out.push_str("\",\"expires_in\":");
    out.push_str(&SESSION_TTL.as_secs().to_string());
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::argon2id;

    fn store_with(records: &[(&str, &str, &str)]) -> UserStore {
        let mut content = String::new();
        for (username, tenant_id, password) in records {
            let salt = b"0123456789abcdef";
            let phc =
                argon2id::encode_phc(password.as_bytes(), salt, &argon2id::RECOMMENDED_PARAMS)
                    .expect("valid phc encoding");
            content.push_str(&format!("{username}:{tenant_id}:{phc}\n"));
        }
        let dir = std::env::temp_dir().join(format!(
            "wire-server-session-issue-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        let path = dir.join("users.txt");
        std::fs::write(&path, content).expect("write fixture");
        UserStore::load_from_file(&path).expect("valid user store")
    }

    #[test]
    fn valid_credentials_return_200_with_token_and_ttl() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"pw-alice"}"#;

        let response = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 200 "), "got: {text}");
        assert!(text.contains("\"expires_in\":3600"), "got: {text}");
        assert!(!text.contains("tenant-a"), "must not leak tenant id");
    }

    #[test]
    fn unknown_user_rejects_with_28p01() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"bob","password":"whatever"}"#;

        let response = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28P01"), "got: {text}");
    }

    #[test]
    fn wrong_password_rejects_with_28p01() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"wrong"}"#;

        let response = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28P01"), "got: {text}");
    }

    #[test]
    fn missing_password_field_rejects_with_42601() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice"}"#;

        let response = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn unknown_field_rejects_with_42601() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"pw-alice","tenant_id":"other"}"#;

        let response = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn invalid_json_rejects_with_42601() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = b"not json";

        let response = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn session_limit_exceeded_rejects_with_53300_only_after_successful_auth() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::with_limits(1, SESSION_TTL);
        let body = br#"{"user":"alice","password":"pw-alice"}"#;

        let first = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        assert!(String::from_utf8_lossy(&first).starts_with("HTTP/1.1 200 "));

        let second = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let text = String::from_utf8(second).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 503 "), "got: {text}");
        assert!(text.contains("53300"), "got: {text}");
    }

    #[test]
    fn two_successful_logins_yield_different_tokens() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"pw-alice"}"#;

        let first = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        let second = handle(&store, &sessions, body, Instant::now(), SystemTime::now());
        assert_ne!(first, second, "reissued tokens must differ");
    }
}
