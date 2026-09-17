//! `POST /v1/query`（`op: search`）の実行結線（NOSQL-2。Issue #764）が、
//! SQL 表層と同一の precision fail-closed 契約（SEARCH-9）・RLS 暗黙適用
//! （RLS-7）で振る舞うことを検証する層 A 結合テスト（対象ビヘイビア
//! TASK-186・NOSQL-2・SEARCH-9・RLS-7。ポインタ: `docs/spec/05-tasks.md`
//! TASK-186・`docs/spec/04-behavior/nosql-surface.md` NOSQL-2・
//! `docs/spec/04-behavior/search.md` SEARCH-9・
//! `docs/spec/04-behavior/rls.md` RLS-7）。
//!
//! `nosql2_search_binding.rs`（Issue #763）との役割分担: 既存ファイルは
//! `bind_search` の束縛規則（schema 依存の投影・フィルタ・ランキング組み立て）
//! を SQL 表層の等価形とバイト単位で比較する単体テスト。本ファイルはそれを
//! 再検証せず、**実行結線後**の応答（`200 OK`／`row_count`／`row_code`）・
//! precision の空集合応答・RLS 非漏えいを、`nosql4_5_aggregate_wire_parity.rs`
//! と同じ方式（同一 `Arc<EngineCore>` 上の SQL テキスト実行
//! `execute_sql_in_session` → `response::encode` をオラクルとするバイト単位
//! パリティ）で固定する。
//!
//! `plan` 指定は `wire_using_plan.rs` と同じ決定的スタブ（`DeterministicEmbedder`・
//! `StubLlmClient`）を注入し、実 Ollama・実埋め込みサービスへの疎通は行わない
//! （TASK-110 と同じくスコープ外）。
//!
//! RLS 注意（`nosql4_5_aggregate_wire_parity.rs` と同じ誤コピー防止）: wire
//! ログインが導出する `PolicyContext` は常に `PolicyContext::new`（Public
//! のみ）であるため、SQL オラクルも `wire_scoped_ctx` を使う。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::response::encode as encode_query_result;
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";
const DIM: u32 = 2;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// wire ログインが導出する `PolicyContext` と同じ Public-only ctx
/// （`nosql4_5_aggregate_wire_parity.rs::wire_scoped_ctx` と同一の判断）。
fn wire_scoped_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant ctx (Public only, wire 既定)")
}

/// 3 テナント × Public 行 1 件（`wire_search_mode.rs` と同じクエリベクトル
/// `[1,0]` に対する cosine: id1=1.0／id2=0.0／id3=-1.0）＋ tenant-b の
/// `Private` 行（id=12。クエリベクトルと完全一致で RLS 境界の対照に使う）。
fn new_core_seed() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql2-search-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    // `(tenant, id, embedding, lang, path, body)`。`clippy::type_complexity`
    // 回避のため型エイリアスを使う。
    type SeedRow<'a> = (&'a str, u64, [f32; 2], &'a str, &'a str, &'a str);
    let public_rows: [SeedRow<'_>; 3] = [
        (
            "tenant-a",
            1,
            [1.0, 0.0],
            "ja",
            "docs/a.md",
            "alpha content",
        ),
        ("tenant-b", 2, [0.0, 1.0], "en", "docs/b.md", "beta content"),
        (
            "tenant-c",
            3,
            [-1.0, 0.0],
            "ja",
            "docs/c.md",
            "gamma content",
        ),
    ];
    for (tenant, id, emb, lang, path, body) in public_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(path.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql2-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    let private_ctx =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &private_ctx,
        12,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("xx".to_string()),
            Value::Text("docs/private.md".to_string()),
            Value::Text("private content".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("nosql2-op-12")
            .expect("valid operation_id"),
    )
    .expect("insert private row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// テキスト長だけを成分へ埋め込む決定的埋め込み（`wire_using_plan.rs::
/// DeterministicEmbedder` と同じ方針）。
struct DeterministicEmbedder {
    dim: u32,
}

impl Embedder for DeterministicEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| vec![t.len() as f32 * 0.01; self.dim as usize])
            .collect())
    }
}

/// 固定の展開結果を返すスタブ `LlmClient`（`wire_using_plan.rs::
/// StubLlmClient` と同じ方針）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#;

/// `new_core_seed` に埋め込み・プランナースタブを注入した plan 検索向け
/// core を返す（`vector` 指定のテストはこの注入を使わない core でも動く
/// ため、注入なし版は [`new_core_seed`] を直接使う）。
fn new_core_with_plan_stubs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql2-search-plan-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = wire_scoped_ctx("tenant-a");
    let rows: [(u64, [f32; 2], &str, &str, &str); 2] = [
        (1, [0.1, 0.2], "ja", "docs/a.md", "alpha content in english"),
        (2, [0.4, 0.3], "en", "docs/b.md", "beta content in english"),
    ];
    for (id, emb, lang, path, body) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(path.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql2-plan-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder { dim: DIM }))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));
    (Arc::new(core), guard)
}

fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
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

fn post(addr: SocketAddr, auth: &str, body: &[u8]) -> HttpResponse {
    let auth_header = format!("Bearer {auth}");
    let content_length = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_header),
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn query_as(addr: SocketAddr, user: &str, password: &str, body: &[u8]) -> HttpResponse {
    let token = login(addr, user, password);
    post(addr, &token, body)
}

fn query_as_alice(addr: SocketAddr, body: &[u8]) -> HttpResponse {
    query_as(addr, "alice", "pw-alice", body)
}

/// `sql` を `tenant` の wire スコープ ctx（Public のみ）で SQL テキスト経由
/// 実行し、`response::encode` を通した JSON 本文（オラクル）を返す。
fn sql_oracle_body(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let ctx = wire_scoped_ctx(tenant);
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect("oracle SQL should succeed");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected SqlOutcome::Query for {sql:?}");
    };
    encode_query_result(&result).expect("oracle result should encode")
}

/// [`sql_oracle_body`] の拒否系版。SQL 経路の `wire_code()` を返す。
fn sql_oracle_err(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let ctx = wire_scoped_ctx(tenant);
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err("oracle SQL should be rejected");
    err.wire_code().to_string()
}

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

/// C1（`vector` のみ）: SQL `ORDER BY <=>` と応答本文がバイト一致する。
#[test]
fn vector_only_matches_sql_order_by_distance() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#""row_count":3"#),
        "{}",
        body_utf8(&resp)
    );
}

/// `filter`（`eq`）が SQL `WHERE` と一致する。
#[test]
fn filter_eq_matches_sql_where() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,
        "columns":["id"],"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

/// `hybrid` が SQL `ORDER BY HYBRID(...)` と一致する。
#[test]
fn hybrid_matches_sql_order_by_hybrid() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,
        "columns":["id"],"hybrid":{"text":"alpha"}}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'alpha') LIMIT 3",
    );
    assert_eq!(body_utf8(&resp), oracle);
}

/// SQL-12: `mode: recall`（明示）は SQL `USING MODE 'recall'` と一致する。
#[test]
fn mode_recall_explicit_matches_sql_using_mode() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body =
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"],"mode":"recall"}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'recall'",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#""row_count":3"#),
        "{}",
        body_utf8(&resp)
    );
}

/// SEARCH-9: `mode: precision` の明確な Top-1 は `row_count: 1`（クエリ
/// ベクトルと完全一致する tenant-a 自身の Public 行のみ）。
#[test]
fn mode_precision_clear_winner_returns_one_row() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body =
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"columns":["id"],"mode":"precision"}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'precision'",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#""row_count":1"#),
        "{}",
        body_utf8(&resp)
    );
    assert!(
        body_utf8(&resp).contains(r#"[[1]]"#),
        "{}",
        body_utf8(&resp)
    );
}

/// SEARCH-9: 低確信度（45 度クエリ）の `mode: precision` は `ErrorResponse`
/// ではなく `200 OK`・`row_count: 0` の通常応答になる（`wire_search_mode.rs::
/// search9_precision_low_confidence_returns_empty_result_set_as_normal_response`
/// の NoSQL 版）。
#[test]
fn mode_precision_low_confidence_returns_zero_rows_as_normal_response() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[0.70710678,0.70710678],"limit":3,
        "columns":["id"],"mode":"precision"}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[0.70710678,0.70710678]' LIMIT 3 \
         USING MODE 'precision'",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#""row_count":0"#),
        "{}",
        body_utf8(&resp)
    );
}

/// `mode: "fuzzy"`（未知語彙）は SQL・NoSQL 双方とも `22000` で拒否される。
#[test]
fn unknown_mode_literal_rejects_with_22000_on_both_surfaces() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"mode":"fuzzy"}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "22000", "resp={resp:?}");
    let oracle_err = sql_oracle_err(
        &core,
        "tenant-a",
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'fuzzy'",
    );
    assert_eq!(oracle_err, "22000");
}

/// RLS-7: tenant-b の `Private` 行（id=12。クエリベクトルと完全一致）は
/// alice（tenant-a）の結果へ混入しない。`mode: precision` でも同様。
#[test]
fn private_row_of_another_tenant_never_leaks_into_result() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,"columns":["id"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let text = body_utf8(&resp);
    assert!(!text.contains("12"), "{text}");
    assert!(!text.contains("xx"), "{text}");
    assert!(!text.contains("tenant-"), "{text}");

    let precision_body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":10,
        "columns":["id"],"mode":"precision"}"#;
    let precision_resp = query_as_alice(addr, precision_body);
    assert_eq!(precision_resp.status, 200, "resp={precision_resp:?}");
    let precision_text = body_utf8(&precision_resp);
    assert!(!precision_text.contains("12"), "{precision_text}");
    assert!(
        precision_text.contains(r#""row_count":1"#),
        "{precision_text}"
    );
}

/// `plan` 指定が SQL `USING PLAN(...)` と一致する（決定的スタブ経由）。
#[test]
fn plan_matches_sql_using_plan() {
    let (core, _guard) = new_core_with_plan_stubs();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":10,
        "columns":["id"]}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    );
    assert_eq!(body_utf8(&resp), oracle);
    assert!(
        body_utf8(&resp).contains(r#""row_count":2"#),
        "{}",
        body_utf8(&resp)
    );
}

/// `plan` 指定・`mode_hint: precision` によるゲートで 0 行 → 明示
/// `mode: recall` で復帰する（`wire_using_plan.rs::
/// using_plan_wire_precision_hint_returns_zero_rows_then_recall_override_returns_rows`
/// の NoSQL 版）。
#[test]
fn plan_mode_hint_precision_returns_zero_rows_then_explicit_recall_overrides() {
    // `wire_using_plan.rs::
    // using_plan_wire_precision_hint_returns_zero_rows_then_recall_override_returns_rows`
    // と完全に同一のコーパス（次元 4・ベクトル値・埋め込みテキスト長）を使う
    // （DIM=2 の共通フィクスチャでは top1/top2 差が明確になりすぎて確信度
    // ゲートが `precision` を「明確な勝者あり」＝1 行として通してしまい、
    // 「ambiguous top1/top2」＝0 行という本テストの意図した対照にならない
    // ため、`lang` 列を持たない専用スキーマ・専用フィクスチャを使う）。
    const MODE_HINT_DIM: u32 = 4;
    let mode_hint_schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(MODE_HINT_DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    let path = http_common::temp_db::unique_db_path("nosql2-search-plan-mode-hint");
    let guard = http_common::temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&mode_hint_schema)
        .expect("create table");
    let ctx = wire_scoped_ctx("tenant-a");
    for (id, emb, path_v, body_v) in [
        (
            1u64,
            [0.1f32, 0.2, 0.3, 0.4],
            "docs/a.md",
            "alpha content in english",
        ),
        (
            2u64,
            [0.4f32, 0.3, 0.2, 0.1],
            "docs/b.md",
            "beta content in english",
        ),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(path_v.to_string()),
                Value::Text(body_v.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "nosql2-mode-hint-op-{id}"
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder {
            dim: MODE_HINT_DIM,
        }))
        .with_query_planner(Box::new(StubLlmClient {
            response: r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
        }));
    let core = Arc::new(core);
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":10}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    assert!(
        body_utf8(&resp).contains(r#""row_count":0"#),
        "{}",
        body_utf8(&resp)
    );

    let recall_body = br#"{"op":"search","table":"docs","plan":"find content","limit":10,
        "mode":"recall"}"#;
    let recall_resp = query_as_alice(addr, recall_body);
    assert_eq!(recall_resp.status, 200, "resp={recall_resp:?}");
    assert!(
        body_utf8(&recall_resp).contains(r#""row_count":2"#),
        "{}",
        body_utf8(&recall_resp)
    );
    drop(guard);
}

/// `plan` 指定で `query_planner`／`embedder` が未注入だと fail-closed
/// （`XX000`）で拒否される（`wire_using_plan.rs` の同名契約の NoSQL 版）。
#[test]
fn plan_without_planner_or_embedder_fails_closed_with_xx000() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":10}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "XX000", "resp={resp:?}");
}

/// 未知テーブルは `vector`／`plan` の排他判定より先に `42P01` で拒否される
/// （§2.3 の判定順序）。
#[test]
fn undefined_table_rejects_with_42p01_before_exclusivity_check() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"missing","limit":1}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "resp={resp:?}");
}

/// `vector` と `plan` の両方指定は `42601`（SQL の `ORDER BY`／`USING PLAN`
/// 相互排他と同じ分類）。
#[test]
fn vector_and_plan_both_present_rejects_with_42601() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body =
        br#"{"op":"search","table":"docs","vector":[1.0,0.0],"plan":"find content","limit":10}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
}

/// `vector` + `explain: true` は `42601`（SQL-6 の `EXPLAIN SELECT ...
/// ORDER BY` 拒否と同じ分類）。
#[test]
fn explain_with_vector_rejects_with_42601() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"explain":true}"#;
    let resp = query_as_alice(addr, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
}

/// `plan` + `explain: true` は `0A000`（NOSQL-10・Issue #765 の未実装扱い。
/// LLM 呼び出しを一切行わないことをスタブ core で確認する——スタブ未注入の
/// core で `XX000` ではなく `0A000` が返ることが、実行前に拒否されている
/// 非 vacuous な証跡になる）。
#[test]
fn explain_with_plan_rejects_with_0a000_without_invoking_planner() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":10,"explain":true}"#;
    let resp = query_as_alice(addr, body);
    // `core` に `query_planner`／`embedder` を注入していないため、もし
    // `explain` 拒否より先に LLM 展開へ進んでいれば `XX000`
    // （プランナー未注入）になるはずである。`0A000` が返ることは、
    // `explain` 拒否が I/O より前に完結している証跡になる。
    assert_eq!(http_common::wire_code_of(&resp), "0A000", "resp={resp:?}");
}

/// 応答本文・エラー応答にテナント ID・ユーザー名・トークンを含まない。
#[test]
fn responses_do_not_leak_tenant_username_or_token() {
    let (core, _guard) = new_core_seed();
    let addr = spawn(Arc::clone(&core));
    let token = login(addr, "alice", "pw-alice");

    let ok_body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3}"#;
    let ok_resp = post(addr, &token, ok_body);
    let ok_text = body_utf8(&ok_resp);
    assert!(!ok_text.contains("alice"));
    assert!(!ok_text.contains("tenant-a"));
    assert!(!ok_text.contains(&token));

    let err_body = br#"{"op":"search","table":"docs","vector":[1.0,0.0],"limit":3,"mode":"fuzzy"}"#;
    let err_resp = post(addr, &token, err_body);
    let err_text = body_utf8(&err_resp);
    assert!(!err_text.contains("alice"));
    assert!(!err_text.contains("tenant-a"));
    assert!(!err_text.contains(&token));
}
