//! NoSQL 表層の production ルータ（Issue #752 の範囲。TASK-171／HTTP-1。
//! ポインタ: `docs/spec/05-tasks.md` TASK-171・
//! `docs/spec/04-behavior/http-transport.md` HTTP-1, HTTP-6）。
//!
//! [`crate::http::conn::handle_connection_with`]（Issue #747）へ注入する
//! [`crate::http::conn::RequestHandler`] 実装。`target` の厳密一致のみで
//! ディスパッチし、`/v1/session` 以外は従来どおり [`crate::http::conn::
//! PlaceholderRouter`] と同じバイト列（`08P01`）で拒否する（`/v1/session/close`・
//! `/v1/query` の実ディスパッチは Issue #753・#758 が本ルータへ追記する）。
//!
//! メソッド（`POST` 以外を拒否）は [`crate::http::conn`] が要求行パース時点で
//! 既に絞り込み済み（[`crate::http::request::Method`] は `Post` の 1 variant
//! のみを持つ閉じた語彙）のため、本ルータでは再検査しない。

use std::time::{Instant, SystemTime};

use std::sync::Arc;

use crate::auth::UserStore;
use crate::http::conn::{Request, RequestHandler};
use crate::http::response;
use crate::http::session::issue as session_issue;
use crate::http::session::store::SessionStore;
use engine::error_format::ErrorClass;

/// `/v1/session` の要求ターゲット（クエリ文字列付き・末尾スラッシュ違いは
/// 不一致として `08P01` へ落ちる。厳密一致のみを受理する方針は #758 が
/// 実ルータ全体へ引き継ぐ）。
const SESSION_TARGET: &str = "/v1/session";

/// production 入口のルータ。`users`（ユーザーストアの共有ハンドル）・
/// `sessions`（[`SessionStore`]。`Clone` で内部状態を共有する型のため、
/// `Router` 自身は `Arc` で包まず値として保持する）を束ねる。
pub struct Router {
    users: Arc<UserStore>,
    sessions: SessionStore,
}

impl Router {
    /// `main.rs::run_server` の nosql 分岐から呼ばれる唯一の構築経路。
    pub fn new(users: Arc<UserStore>, sessions: SessionStore) -> Router {
        Router { users, sessions }
    }
}

impl RequestHandler for Router {
    fn handle(&self, req: &Request<'_>) -> Vec<u8> {
        if req.line.target == SESSION_TARGET {
            return session_issue::handle(
                &self.users,
                &self.sessions,
                req.body,
                Instant::now,
                SystemTime::now(),
            );
        }

        // `/v1/session/close`・`/v1/query` を含む未対応パスは、Issue #747 時点の
        // `PlaceholderRouter` と同一のバイト列で拒否する（実ルータの段階的な
        // 置き換えは #753・#758 が担う）。
        response::encode_error(
            ErrorClass::ProtocolViolation,
            "unknown request target",
            SystemTime::now(),
        )
    }
}
