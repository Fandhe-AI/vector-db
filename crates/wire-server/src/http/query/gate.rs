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
//! 5. `op` ごとにディスパッチする（TASK-186・NOSQL-3。Issue #766）:
//!    `Op::Scan` は [`crate::http::query::scan::handle`] へ束縛・実行を
//!    委譲する。`Op::Search`／`Op::Aggregate`／`Op::Insert` は引き続き暫定
//!    `0A000`／501（[`PLACEHOLDER_MESSAGE`]）を返す（束縛・実行の結線は
//!    #763・#768・#771 が本 seam を置き換える）
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
use crate::http::query::scan;
use crate::http::session::middleware::{self, SessionPrincipal};
use crate::http::{body, response};

pub use crate::http::query::op::UNSUPPORTED_OP_MESSAGE;

/// 検証を通過した要求に返す暫定応答の文言（束縛・実行は #763
/// 以降の担当。本 Issue 時点は seam のみ）。
pub const PLACEHOLDER_MESSAGE: &str = "query execution not yet available";

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
/// `core` は束縛済み計画の実行先（`Op::Scan` のみが使う。TASK-186・NOSQL-3）。
/// `principal` は [`crate::http::session::middleware::authenticate`] を通過
/// した要求のテナント文脈（唯一の入口）。`now_wall` は応答の `Date` ヘッダ
/// （壁時計）に使う。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    headers: &crate::http::headers::Headers<'_>,
    body: &[u8],
    now_wall: SystemTime,
) -> Vec<u8> {
    match handle_inner(core, principal, headers, body, now_wall) {
        Ok(bytes) => bytes,
        Err(e) => response::encode_error(e.class, &e.message, now_wall),
    }
}

/// [`handle`] の本体。手順 1〜4 の検証を通過した要求のみ手順 5 の op 別
/// ディスパッチへ進む（`Op::Scan` は実行結果の応答、他 op は暫定
/// placeholder 応答をそれぞれ完成させたバイト列として返す）。
fn handle_inner(
    core: &EngineCore,
    principal: &SessionPrincipal,
    headers: &crate::http::headers::Headers<'_>,
    raw_body: &[u8],
    now_wall: SystemTime,
) -> Result<Vec<u8>, HandleError> {
    // 手順 1: テナント指定ヘッダの拒否（本文検証より先）。
    middleware::reject_tenant_headers(headers)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // 手順 2: 本文検証（構文・`op` フィールドの形）。
    let text = body::body_as_utf8(raw_body)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;
    let value =
        parse_json(text).map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;
    let raw_op = crate::http::query::schema::extract_op(&value)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // 手順 3: op 許可リスト判定（スキーマ検証より前。語彙外は 0A000）。
    let op =
        classify_op(raw_op).map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // 手順 4: スキーマ検証（必須欠落・未知キー・型不一致 → 42601）。
    let validated = op
        .schema()
        .validate(&value)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // 手順 5: op 別ディスパッチ（TASK-186・NOSQL-3）。`Op::Scan` のみ
    // 実行結線済み。他 op は引き続き暫定 placeholder 応答（#763・#768・#771
    // が本 seam を置き換える）。
    Ok(match op {
        Op::Scan => scan::handle(core, principal, &validated, now_wall),
        Op::Search | Op::Aggregate | Op::Insert => response::encode_error(
            ErrorClass::FeatureNotSupported,
            PLACEHOLDER_MESSAGE,
            now_wall,
        ),
    })
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

    fn run(core: &EngineCore, body: &[u8], extra_headers: &[&str]) -> Vec<u8> {
        let p = principal();
        let headers = headers_with_body(body, extra_headers);
        handle(core, &p, &headers, body, std::time::SystemTime::now())
    }

    #[test]
    fn valid_scan_is_dispatched_to_the_engine_and_reports_undefined_table() {
        // `scan` は TASK-186・NOSQL-3 で実行結線済みのため、他 3 op のような
        // 暫定 placeholder（`0A000`／501）はもう返らない。存在しないテーブル
        // への `scan` が `42P01`／404（SQL 経路と同一分類）になることで、
        // 「認証 → op 許可リスト → スキーマ検証 → engine 呼び出し」が
        // すべて走ったことを非 vacuous に確認する。
        let (core, _guard) = empty_core();
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
        assert!(text.contains("42P01"), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_search_reaches_placeholder_response() {
        let (core, _guard) = empty_core();
        let body = br#"{"op":"search","table":"docs","limit":1}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_aggregate_reaches_placeholder_response() {
        let (core, _guard) = empty_core();
        let body =
            br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_insert_reaches_placeholder_response() {
        let (core, _guard) = empty_core();
        let body = br#"{"op":"insert","table":"docs","rows":[]}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn tenant_id_in_json_is_rejected_for_all_four_ops() {
        let (core, _guard) = empty_core();
        let cases: [&[u8]; 4] = [
            br#"{"op":"search","table":"docs","limit":1,"tenant_id":"evil"}"#,
            br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#,
            br#"{"op":"aggregate","table":"docs","aggregates":[],"tenant_id":"evil"}"#,
            br#"{"op":"insert","table":"docs","rows":[],"tenant_id":"evil"}"#,
        ];
        for body in cases {
            let response = run(&core, body, &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
            assert!(text.contains("42601"), "got: {text}");
        }
    }

    #[test]
    fn unsupported_op_rejects_with_0a000() {
        let (core, _guard) = empty_core();
        let body = br#"{"op":"select","table":"docs"}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(UNSUPPORTED_OP_MESSAGE), "got: {text}");
    }

    #[test]
    fn ddl_udf_transaction_update_delete_ops_reject_with_0a000() {
        let (core, _guard) = empty_core();
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
            let response = run(&core, body.as_bytes(), &[]);
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
        let (core, _guard) = empty_core();
        // 語彙外 op に未知キー（本来ならスキーマ検証で 42601）が同時に
        // 付与されていても、op 許可リスト判定（0A000）が先に効く
        // （手順の順序が契約であることの回帰確認）。
        let body = br#"{"op":"drop_table","table":"docs","hint_order":["path"]}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(UNSUPPORTED_OP_MESSAGE), "got: {text}");
        assert!(!text.contains("42601"), "got: {text}");
    }

    #[test]
    fn missing_op_rejects_with_42601() {
        let (core, _guard) = empty_core();
        let body = br#"{"table":"docs"}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn non_object_body_rejects_with_42601() {
        let (core, _guard) = empty_core();
        let body = br#"[]"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn invalid_json_rejects_with_42601() {
        let (core, _guard) = empty_core();
        let body = b"not json";
        let response = run(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn non_utf8_body_rejects_with_42601() {
        let (core, _guard) = empty_core();
        let body: &[u8] = &[0xff, 0xfe, 0xfd];
        let response = run(&core, body, &[]);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn tenant_header_rejects_before_body_validation() {
        let (core, _guard) = empty_core();
        // 本文が不正でも tenant ヘッダの拒否文言が先に返る（順序: ヘッダ検査
        // が本文検証より前）。
        let body = b"not json at all";
        let response = run(&core, body, &["X-Tenant-Id: other\r\n"]);
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
        let (core, _guard) = empty_core();
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run(&core, body, &[]);
        let text = String::from_utf8_lossy(&response);
        assert!(!text.contains("tenant-a"));
    }
}
