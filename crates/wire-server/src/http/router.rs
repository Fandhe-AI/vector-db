//! NoSQL 表層の production ルータ（Issue #752・#753 の範囲。TASK-171／
//! HTTP-1・HTTP-8。ポインタ: `docs/spec/05-tasks.md` TASK-171・
//! `docs/spec/04-behavior/http-transport.md` HTTP-1, HTTP-6, HTTP-8）。
//!
//! [`crate::http::conn::handle_connection_with`]（Issue #747）へ注入する
//! [`crate::http::conn::RequestHandler`] 実装。`target` の厳密一致のみで
//! ディスパッチし、`/v1/session`・`/v1/session/close`・`/v1/query` 以外は
//! 従来どおり [`crate::http::conn::PlaceholderRouter`] と同じバイト列
//! （`08P01`）で拒否する。
//!
//! `/v1/query` は [`crate::http::session::middleware::authenticate`]（Bearer
//! 検証。失敗は `28000`）を通過した要求のみ [`crate::http::query::gate::
//! handle`] へ渡す（Issue #754・HTTP-5・HTTP-6・HTTP-7）。op 許可リストの
//! 正式化・3 エンドポイント限定化（非 POST・クエリ文字列付き等の網羅）は
//! Issue #759・#758 が本ルータへ追記する。
//!
//! メソッド（`POST` 以外を拒否）は [`crate::http::conn`] が要求行パース時点で
//! 既に絞り込み済み（[`crate::http::request::Method`] は `Post` の 1 variant
//! のみを持つ閉じた語彙）のため、本ルータでは再検査しない。

use std::time::{Instant, SystemTime};

use std::sync::Arc;

use crate::auth::UserStore;
use crate::http::conn::{Request, RequestHandler};
use crate::http::query::gate as query_gate;
use crate::http::response;
use crate::http::session::close as session_close;
use crate::http::session::issue as session_issue;
use crate::http::session::middleware;
use crate::http::session::store::SessionStore;
use engine::error_format::ErrorClass;

/// `/v1/session` の要求ターゲット（クエリ文字列付き・末尾スラッシュ違いは
/// 不一致として `08P01` へ落ちる。厳密一致のみを受理する方針は #758 が
/// 実ルータ全体へ引き継ぐ）。
const SESSION_TARGET: &str = "/v1/session";

/// `/v1/session/close` の要求ターゲット（[`SESSION_TARGET`] と同じ厳密一致
/// 方針。クエリ文字列付き・末尾スラッシュ違いは不一致として `08P01` へ
/// 落ちる）。
const SESSION_CLOSE_TARGET: &str = "/v1/session/close";

/// `/v1/query` の要求ターゲット（[`SESSION_TARGET`] と同じ厳密一致方針）。
const QUERY_TARGET: &str = "/v1/query";

/// クライアント自己申告の `tenant_id` 相当がパス上に現れた場合の固定文言
/// （ヘッダの [`crate::http::session::middleware::TENANT_HEADER_MESSAGE`] と
/// 同じ設計意図。パス断片を echo しない）。
const QUERY_TARGET_TENANT_MARKER_MESSAGE: &str =
    "tenant context is derived from the session; per-request tenant path segments are not accepted";

/// `target` を [`QUERY_TARGET`] に対して 3 通りへ分類する純関数（Issue #754。
/// #758 が実ルータ全体を厳密化する際もこの判定をそのまま引き継ぐ想定）。
///
/// - `target == QUERY_TARGET`（厳密一致）→ [`QueryTargetKind::Exact`]
/// - `/v1/query?...`／`/v1/query/...` の残余部分（ASCII 小文字化・`_`→`-`
///   正規化後）に `tenant-id` または `tenantid` を部分文字列として含む
///   → [`QueryTargetKind::TenantMarker`]（`42601`。パス上の `tenant_id` は
///   要求自身の構文違反でありサーバー状態・テナント存在情報を含まないため、
///   認証前に判定してよい）
/// - それ以外（クエリ文字列付き・末尾スラッシュ違い等）→
///   [`QueryTargetKind::Other`]（従来どおり `08P01`）
fn query_target_kind(target: &str) -> QueryTargetKind {
    if target == QUERY_TARGET {
        return QueryTargetKind::Exact;
    }
    let rest = target
        .strip_prefix("/v1/query?")
        .or_else(|| target.strip_prefix("/v1/query/"));
    if let Some(rest) = rest {
        let normalized: String = rest
            .chars()
            .map(|c| {
                if c == '_' {
                    '-'
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if normalized.contains("tenant-id") || normalized.contains("tenantid") {
            return QueryTargetKind::TenantMarker;
        }
    }
    QueryTargetKind::Other
}

/// [`query_target_kind`] の分類結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryTargetKind {
    Exact,
    TenantMarker,
    Other,
}

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

        if req.line.target == SESSION_CLOSE_TARGET {
            return session_close::handle(
                &self.sessions,
                &req.headers,
                req.body,
                Instant::now,
                SystemTime::now(),
            );
        }

        match query_target_kind(req.line.target) {
            QueryTargetKind::Exact => {
                match middleware::authenticate(&self.sessions, &req.headers, Instant::now) {
                    Ok(principal) => {
                        query_gate::handle(&principal, &req.headers, req.body, SystemTime::now())
                    }
                    Err(e) => response::encode_error(
                        e.error_class(),
                        e.client_message(),
                        SystemTime::now(),
                    ),
                }
            }
            QueryTargetKind::TenantMarker => response::encode_error(
                ErrorClass::UnsupportedSqlSyntax,
                QUERY_TARGET_TENANT_MARKER_MESSAGE,
                SystemTime::now(),
            ),
            QueryTargetKind::Other => {
                // 未対応パスは、Issue #747 時点の `PlaceholderRouter` と
                // 同一のバイト列で拒否する（3 エンドポイント限定の網羅は
                // #758 が担う）。
                response::encode_error(
                    ErrorClass::ProtocolViolation,
                    "unknown request target",
                    SystemTime::now(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_target_kind_classifies_exact_and_variants() {
        assert_eq!(query_target_kind("/v1/query"), QueryTargetKind::Exact);
        assert_eq!(query_target_kind("/v1/query?x=1"), QueryTargetKind::Other);
        assert_eq!(query_target_kind("/v1/query/"), QueryTargetKind::Other);
        assert_eq!(query_target_kind("/other"), QueryTargetKind::Other);
        assert_eq!(
            query_target_kind("/v1/query?tenant_id=other"),
            QueryTargetKind::TenantMarker
        );
        assert_eq!(
            query_target_kind("/v1/query/tenant_id/other"),
            QueryTargetKind::TenantMarker
        );
        assert_eq!(
            query_target_kind("/v1/query?TENANT-ID=other"),
            QueryTargetKind::TenantMarker
        );
        assert_eq!(
            query_target_kind("/v1/query/tenantid/other"),
            QueryTargetKind::TenantMarker
        );
    }
}
