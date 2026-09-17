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
//!    parse_json` → [`crate::http::query::schema::extract_op`]（本文の構文・
//!    `op` フィールドの形の検証。`42601`）
//! 3. [`crate::http::query::op::classify_op`]（`op` 名を閉じた語彙 4 値へ
//!    分類する許可リスト。DDL・UDF 呼び出し・トランザクション制御・
//!    UPDATE／DELETE 相当を含む語彙外はすべて `0A000`。
//!    Issue #759・TASK-179・NOSQL-1・NOSQL-9）
//! 4. `Op::schema().validate(...)`（必須欠落・未知キー・型不一致 →
//!    `42601`。`tenant_id` の JSON 自己申告・`HINT ORDER`／
//!    `SET search_mode` 相当フィールドはここで未知キーとして拒否される）
//! 5. `op` と `engine`（接続済み `EngineCore`。`Router::new` 経由では
//!    `None`・`Router::with_engine` 経由でのみ `Some`）の組でディスパッチ
//!    する。`(Op::Search, Some(engine))` かつ `explain: true` は
//!    [`super::explain::handle`]（TASK-186・NOSQL-10・Issue #765。検索本体を
//!    実行せず `QUERY PLAN` を返す。通常の `search` 実行より必ず先に
//!    判定される）、`(Op::Scan, Some(engine))` は [`crate::http::query::scan::
//!    handle`]（TASK-186・NOSQL-3・Issue #766）、`(Op::Aggregate,
//!    Some(engine))` は [`super::aggregate::handle`]（Issue #768・
//!    TASK-177・NOSQL-4）、`explain` なしの `(Op::Search, Some(engine))` は
//!    [`super::search::handle`]（TASK-186・NOSQL-2・Issue #764）、
//!    `(Op::Insert, Some(engine))` は [`super::insert::handle`]（Issue
//!    #772・TASK-178・NOSQL-6）へそれぞれ束縛・実行を委譲する。`engine`
//!    未接続時の `Op::Scan`／`Op::Aggregate`／`Op::Insert`／`Op::Search` は
//!    暫定の `0A000`／501（[`PLACEHOLDER_MESSAGE`]）を返す（4 op すべてが
//!    実行結線済みのため、この応答は `engine` 未接続時にのみ到達する）
//!
//! 手順 3（op 許可リスト）は手順 4（スキーマ検証）より前に行う。語彙外の
//! `op` にスキーマ検証由来の情報（未知キー等）が先に返ることはない
//! （fail-closed の判定順序も契約の一部）。
//!
//! 応答本文・ログにテナント ID・トークンを含めない
//! （`.claude/rules/security.md`「エラー・ログ経由で他テナントのデータ・
//! 存在情報を漏らさない」）。

use std::time::SystemTime;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::parse_json;

use crate::http::query::op::{classify_op, Op};
use crate::http::query::schema::Validated;
use crate::http::query::{insert, scan};
use crate::http::session::middleware::{self, SessionPrincipal};
use crate::http::{body, response};

pub use crate::http::query::op::UNSUPPORTED_OP_MESSAGE;

/// 検証を通過したが実行結線が未接続（`engine` 未接続）の要求に返す暫定応答
/// の文言。`scan`・`aggregate`・`insert`・`search`（`explain: true` の
/// `search` を含む）の 4 op すべてが実行結線済み（Issue #766・#768・
/// #772・#764・#765）のため、この応答は `Router::new` 経由（`engine`
/// 未接続）の場合にのみ到達する。
pub const PLACEHOLDER_MESSAGE: &str = "query execution not yet available";

/// `POST /v1/query` を処理し応答バイト列を返す（認証済み要求のみ）。
///
/// `principal` は [`crate::http::session::middleware::authenticate`] を通過
/// した要求のテナント文脈（唯一の入口）。`engine` は接続済みの
/// `EngineCore`（`Router::new` 経由では `None`。`Router::with_engine` 経由
/// でのみ `Some`）。`now_wall` は応答の `Date` ヘッダ（壁時計）に使う。
pub fn handle(
    principal: &SessionPrincipal,
    headers: &crate::http::headers::Headers<'_>,
    raw_body: &[u8],
    engine: Option<&EngineCore>,
    now_wall: SystemTime,
) -> Vec<u8> {
    // 手順 1: テナント指定ヘッダの拒否（本文検証より先）。
    if let Err(e) = middleware::reject_tenant_headers(headers) {
        return response::encode_error(e.error_class(), e.client_message(), now_wall);
    }

    // 手順 2: 本文検証（構文・`op` フィールドの形）。
    let text = match body::body_as_utf8(raw_body) {
        Ok(text) => text,
        Err(e) => return response::encode_error(e.error_class(), e.client_message(), now_wall),
    };
    let value = match parse_json(text) {
        Ok(value) => value,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };
    let raw_op = match crate::http::query::schema::extract_op(&value) {
        Ok(raw_op) => raw_op,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };

    // 手順 3: op 許可リスト判定（スキーマ検証より前。語彙外は 0A000）。
    let op = match classify_op(raw_op) {
        Ok(op) => op,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };

    // 手順 4: スキーマ検証（必須欠落・未知キー・型不一致 → 42601）。
    let validated = match op.schema().validate(&value) {
        Ok(validated) => validated,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };

    // 手順 5: op と engine 接続有無の組でディスパッチする（Issue #766・
    // #768・#772・#764）。各アームは対応するモジュールへの 1 行委譲に留め、
    // 4 op すべてが実行結線済みのため `(_, _)` は `engine` 未接続時にのみ
    // 到達する。
    match (op, engine) {
        // `explain: true` は通常の `search` 実行（#764 が結線する
        // `(Op::Search, Some(engine)) => search::handle(...)` 相当）より
        // 必ず先に判定する（Issue #765・TASK-186・NOSQL-10）。`explain: true`
        // が構造的に実行経路へ落ちないことを match の腕の順序自体で保証する
        // （`aggregate.rs::reject_explain` と同じ fail-open 防止の思想）。
        (Op::Search, Some(engine)) if explain_requested(&validated) => {
            super::explain::handle(engine, principal, &validated, now_wall)
        }
        (Op::Scan, Some(engine)) => scan::handle(engine, principal, &validated, now_wall),
        (Op::Aggregate, Some(engine)) => {
            super::aggregate::handle(engine, principal, &validated, now_wall)
        }
        (Op::Insert, Some(engine)) => insert::handle(engine, principal, &validated, now_wall),
        (Op::Search, Some(engine)) => {
            super::search::handle(engine, principal, &validated, now_wall)
        }
        (_, _) => response::encode_error(
            ErrorClass::FeatureNotSupported,
            PLACEHOLDER_MESSAGE,
            now_wall,
        ),
    }
}

/// `validated`（`Op::Search` のスキーマ検証済み要求）が `explain: true` を
/// 伴うかを判定する（Issue #765）。`optional_bool` の `Err`（多層防御の域。
/// `SEARCH_SCHEMA` 検証を既に通過しているため通常到達しない）は `false`
/// 扱いにせず、後段のスキーマ検証と同じ判定経路（`(_, _)` アーム）へ
/// 委ねる意図で `matches!` により厳密に `Ok(Some(true))` のみを真とする。
fn explain_requested(validated: &Validated<'_>) -> bool {
    matches!(validated.optional_bool("explain"), Ok(Some(true)))
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

    /// テストごとに一意な一時 DB ファイルパスを払い出す（`EngineCore::open`
    /// に渡す前提。ファイル自体は作成しない）。`gate.rs` は `mod gate;` 経由
    /// で読み込まれる非ルートファイルのため、`#[path]` によるエンジン側
    /// `test_util/temp_db.rs` の取り込みは、ネストしたモジュール宣言に
    /// 対応する仮想ディレクトリ（`query/gate/`・`query/gate/tests/`）が
    /// 実ファイルシステム上に存在せず `..` を辿れない（OS のパス解決は
    /// 中間コンポーネントの実在を要求する）ため使えない。本ヘルパーは
    /// その代わりに最小限の一意名生成のみをその場で行う（`std` のみに依存。
    /// dependency-policy 準拠）。
    fn unique_temp_db_path(label: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vector-db-wire-server-query-gate-{label}-{}-{seq}.redb",
            std::process::id()
        ));
        path
    }

    /// [`unique_temp_db_path`] で払い出したパスのファイルを、値が drop
    /// されるタイミングで削除する RAII ガード（`engine::test_util::temp_db::
    /// CleanupGuard` と同じ意図の最小版）。
    struct CleanupGuard(std::path::PathBuf);

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// テーブルを一切作らないスローアウェイ `EngineCore`。`table: "docs"` への
    /// `scan` はスキーマ取得の時点で `SqlSurfaceError::UndefinedTable`
    /// （`42P01`／404）となり、「認証 → op 許可リスト → スキーマ検証 →
    /// engine 呼び出し」がすべて走ったことの非 vacuous な証跡になる
    /// （`http_common::assert_reached_query_gate` と同じ判断）。
    fn empty_core() -> (EngineCore, CleanupGuard) {
        let path = unique_temp_db_path("query-gate");
        let guard = CleanupGuard(path.clone());
        let core = EngineCore::open(&path).expect("open throwaway engine core");
        (core, guard)
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

    /// `engine` 未接続（`Router::new` 経由）の従来経路。全 op がプレース
    /// ホルダー応答へ落ちることを検証する既存テスト群が使う。
    fn run(body: &[u8], extra_headers: &[&str]) -> Vec<u8> {
        let p = principal();
        let headers = headers_with_body(body, extra_headers);
        handle(&p, &headers, body, None, std::time::SystemTime::now())
    }

    /// `engine` 接続済み（`Router::with_engine` 相当）の経路。`scan` op の
    /// 実行結線（TASK-186・NOSQL-3・Issue #766）を検証するテストが使う。
    fn run_with_engine(core: &EngineCore, body: &[u8], extra_headers: &[&str]) -> Vec<u8> {
        let p = principal();
        let headers = headers_with_body(body, extra_headers);
        handle(&p, &headers, body, Some(core), std::time::SystemTime::now())
    }

    #[test]
    fn valid_scan_is_dispatched_to_the_engine_and_reports_undefined_table() {
        // `scan` は TASK-186・NOSQL-3 で実行結線済みのため、`engine` 接続済み
        // であれば他 op のような暫定 placeholder（`0A000`／501）はもう
        // 返らない。存在しないテーブルへの `scan` が `42P01`／404（SQL 経路と
        // 同一分類）になることで、「認証 → op 許可リスト → スキーマ検証 →
        // engine 呼び出し」がすべて走ったことを非 vacuous に確認する。
        let (core, _guard) = empty_core();
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run_with_engine(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
        assert!(text.contains("42P01"), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_scan_reaches_placeholder_response_when_engine_is_not_connected() {
        // `engine` 未接続（`Router::new` 経由）では `scan` も従来どおり
        // placeholder のまま（実行器なしで応答を偽装しない。
        // `router.rs::Router::new` の既定・`nosql9_op_allowlist.rs` と
        // 同じ契約）。
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_search_reaches_placeholder_response_when_engine_is_not_connected() {
        // `engine` 未接続（`Router::new` 経由）では `search` も従来どおり
        // placeholder のまま（実行器なしで応答を偽装しない。`scan`・
        // `aggregate` と同じ契約）。
        let body = br#"{"op":"search","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_aggregate_reaches_placeholder_response_when_engine_is_not_connected() {
        // `engine` 未接続（`Router::new` 経由）では `aggregate` も従来どおり
        // placeholder のまま（実行器なしで応答を偽装しない。
        // `router.rs::Router::new` の既定・`nosql9_op_allowlist.rs` と
        // 同じ契約）。
        let body =
            br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_insert_reaches_placeholder_response() {
        let body = br#"{"op":"insert","table":"docs","rows":[]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_insert_is_dispatched_to_the_engine_and_reports_undefined_table() {
        // `insert` は Issue #772 で実行結線済みのため、`engine` 接続済み
        // であれば `scan`／`aggregate` と同様もう暫定 placeholder
        // （`0A000`／501）を返さない。存在しないテーブルへの `insert` が
        // `42P01`／404（SQL 経路と同一分類）になることで、「認証 → op
        // 許可リスト → スキーマ検証 → engine 呼び出し」がすべて走った
        // ことを非 vacuous に確認する（`valid_scan_is_dispatched_to_...`
        // と同型）。
        let (core, _guard) = empty_core();
        let body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0]}],"operation_id":"op-gate-1"}"#;
        let response = run_with_engine(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
        assert!(text.contains("42P01"), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_search_is_dispatched_to_the_engine_and_reports_undefined_table() {
        // `search` は TASK-186・NOSQL-2（Issue #764）で実行結線済みのため、
        // `engine` 接続済みであれば `scan`／`aggregate`／`insert` と同様もう
        // 暫定 placeholder（`0A000`／501）を返さない。存在しないテーブルへの
        // `search` が `42P01`／404（SQL 経路と同一分類）になることで、
        // 「認証 → op 許可リスト → スキーマ検証 → engine 呼び出し」が
        // すべて走ったことを非 vacuous に確認する
        // （`valid_scan_is_dispatched_to_...` と同型）。
        let (core, _guard) = empty_core();
        let body = br#"{"op":"search","table":"docs","limit":1}"#;
        let response = run_with_engine(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
        assert!(text.contains("42P01"), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
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
    fn ddl_udf_transaction_update_delete_ops_reject_with_0a000() {
        // DDL・UDF 呼び出し・トランザクション制御・UPDATE／DELETE 相当・
        // 表記揺れは、いずれも許可リスト（Op::parse の 4 値）に無いため
        // 0A000 に落ちる（拒否リストを別途持たない設計の回帰確認）。
        let ops = [
            "create_table",
            "alter_table",
            "drop_table",
            "call",
            "udf",
            "begin",
            "commit",
            "rollback",
            "update",
            "delete",
            "explain",
            "set",
            "SEARCH",
            " search",
            "",
        ];
        for op in ops {
            let body = format!(r#"{{"op":"{op}","table":"docs"}}"#);
            let response = run(body.as_bytes(), &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 501 "), "op {op:?} got: {text}");
            assert!(text.contains("0A000"), "op {op:?} got: {text}");
            assert!(
                text.contains(UNSUPPORTED_OP_MESSAGE),
                "op {op:?} got: {text}"
            );
        }
    }

    #[test]
    fn op_allowlist_check_precedes_schema_validation() {
        // 語彙外 op に未知キー（本来ならスキーマ検証で 42601）が同時に
        // 付与されていても、op 許可リスト判定（0A000）が先に効く
        // （手順の順序が契約であることの回帰確認）。
        let body = br#"{"op":"drop_table","table":"docs","hint_order":["path"]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(UNSUPPORTED_OP_MESSAGE), "got: {text}");
        assert!(!text.contains("42601"), "got: {text}");
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
