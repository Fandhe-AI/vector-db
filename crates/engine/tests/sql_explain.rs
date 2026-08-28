//! `EXPLAIN SELECT ... USING PLAN('<query>')` の受け入れテスト（TASK-78、対象
//! ビヘイビア: SQL-6。ポインタ: `docs/spec/05-tasks.md` TASK-78・
//! `docs/spec/04-behavior/sql-surface.md` SQL-6）。
//!
//! `tests/sql_using_plan.rs`（TASK-77・SQL-5）が固定する「実行時ディスパッチ」に
//! 対し、本ファイルは `EXPLAIN` が検索本体を一切実行せず、LLM クエリ展開・モード
//! 解決の結果（検索語・ソフトヒント・実効モードと指定元）のみを `QUERY PLAN` 単一
//! 列の応答として返すことを固定する。`USING PLAN` を伴わない構文・拒否系
//! （プランナー未注入・スキーマ不備・LIMIT/モードリテラル不正）が LLM 呼び出し
//! 前に完結することも確認する（`sql_using_plan.rs` と同じ判別テストの流儀）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const DIM: u32 = 4;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn open_storage_with_table(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, id: u64, embedding: Vec<f32>, path: &str, body: &str) {
    let op_id = OperationId::parse(&format!("explain-test-op-{id}")).expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx("tenant-a"),
        id,
        Visibility::Public,
        &[
            Value::Vector(embedding),
            Value::Text(path.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn seeded_storage(path: &std::path::Path) -> Storage {
    let storage = open_storage_with_table(path);
    insert_row(
        &storage,
        1,
        vec![0.1, 0.2, 0.3, 0.4],
        "docs/a.md",
        "alpha content in english",
    );
    storage
}

/// 固定の展開結果を返すスタブ（実 Ollama 疎通は TASK-110 と同じくスコープ外。
/// `sql_using_plan.rs::StubLlmClient` と同構成）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

/// 呼び出し回数を記録するスタブ（`sql_using_plan.rs::CountingLlmClient` と同構成。
/// LIMIT/モードリテラル不正が LLM 呼び出し前に拒否されることを直接確認するため）。
struct CountingLlmClient {
    response: String,
    calls: std::sync::atomic::AtomicUsize,
}

impl CountingLlmClient {
    fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl LlmClient for CountingLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.response.clone())
    }
}

struct ArcLlmClient(std::sync::Arc<CountingLlmClient>);

impl LlmClient for ArcLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.0.complete(prompt)
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": "docs/", "kind_hint": "fn"}"#;
const EXPANSION_RESPONSE_NO_HINTS: &str =
    r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#;

fn explain_result_lines(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(result) => {
            assert_eq!(
                result.columns.len(),
                1,
                "EXPLAIN must return a single column"
            );
            assert_eq!(
                result.columns[0],
                ColumnMeta::Computed {
                    name: "QUERY PLAN".to_string()
                }
            );
            result
                .rows
                .iter()
                .map(|row| match &row.cells[0] {
                    Cell::Text(s) => s.clone(),
                    other => panic!("expected Cell::Text, got {other:?}"),
                })
                .collect()
        }
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

#[test]
fn explain_reports_search_terms_and_hints_and_mode() {
    let path = unique_db_path("sql-explain-basic");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(
        lines,
        vec![
            "search_terms[0]: alpha".to_string(),
            "search_terms[1]: beta".to_string(),
            "path_hint: docs/".to_string(),
            "kind_hint: fn".to_string(),
            "mode: recall".to_string(),
            "mode_source: default".to_string(),
        ]
    );
}

#[test]
fn explain_uses_none_label_for_absent_hints() {
    let path = unique_db_path("sql-explain-no-hints");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(
        lines,
        vec![
            "path_hint: (none)".to_string(),
            "kind_hint: (none)".to_string(),
            "mode: recall".to_string(),
            "mode_source: default".to_string(),
        ]
    );
}

#[test]
fn explain_reports_mode_source_query_clause() {
    let path = unique_db_path("sql-explain-mode-query-clause");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 2], "mode: precision");
    assert_eq!(lines[lines.len() - 1], "mode_source: query_clause");
}

#[test]
fn explain_reports_mode_source_session_variable() {
    let path = unique_db_path("sql-explain-mode-session-variable");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }),
    );

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "SET search_mode = 'precision'",
    )
    .expect("SET search_mode should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 2], "mode: precision");
    assert_eq!(lines[lines.len() - 1], "mode_source: session_variable");
}

#[test]
fn explain_reports_mode_source_planner_estimate() {
    let path = unique_db_path("sql-explain-mode-planner-estimate");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: r#"{"search_terms": [], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 2], "mode: precision");
    assert_eq!(lines[lines.len() - 1], "mode_source: planner_estimate");
}

#[test]
fn explain_does_not_require_an_embedder() {
    // `EXPLAIN` は検索本体を実行しないため再埋め込み（`Embedder`）を呼ばない。
    // `with_embedder` を一切呼ばずに構築した `EngineCore` でも成功することを
    // 固定する（設計方針: 「embedder 未注入でも EXPLAIN 可能」）。
    let path = unique_db_path("sql-explain-no-embedder-needed");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    )
    .expect("EXPLAIN must succeed without a configured embedder");
}

#[test]
fn explain_rejects_plain_select_without_using_plan() {
    let path = unique_db_path("sql-explain-rejects-plain-select");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        )
        .expect_err("EXPLAIN without USING PLAN must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn explain_rejects_aggregate_select() {
    let path = unique_db_path("sql-explain-rejects-aggregate");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT COUNT(id) FROM docs",
        )
        .expect_err("EXPLAIN over an aggregate SELECT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn explain_rejects_set_search_mode() {
    let path = unique_db_path("sql-explain-rejects-set");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SET search_mode = 'recall'",
        )
        .expect_err("EXPLAIN over SET must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn explain_via_session_less_entry_point_is_rejected() {
    // `EngineCore::execute_sql`（セッションなし後方互換 API）は `EXPLAIN` を
    // 受理しない（`SET`・`CREATE FUNCTION` と同じ「セッション対応エントリ
    // ポイントを使う」契約）。
    let path = unique_db_path("sql-explain-rejects-sessionless-entry");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("EXPLAIN must require a session-aware entry point");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn explain_fails_closed_without_query_planner() {
    let path = unique_db_path("sql-explain-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("missing query planner must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn explain_fails_closed_without_body_column() {
    let path = unique_db_path("sql-explain-no-body");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "no_body",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM no_body USING PLAN('find content') LIMIT 10",
        )
        .expect_err("missing body column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn explain_rejects_invalid_limit_before_invoking_query_planner() {
    let path = unique_db_path("sql-explain-invalid-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 0",
        )
        .expect_err("LIMIT 0 must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "invalid LIMIT must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_invalid_using_mode_before_invoking_query_planner() {
    let path = unique_db_path("sql-explain-invalid-mode");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'invalid'",
        )
        .expect_err("unknown USING MODE literal must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "invalid USING MODE must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_unknown_projection_column_before_invoking_query_planner() {
    // codex-review P1・Cursor Bugbot 指摘（PR #267）の判別テスト:
    // `EXPLAIN` 経路も `Statement::Select` の `USING PLAN` 経路（PR #266・
    // `sql_using_plan.rs::using_plan_rejects_unknown_projection_column_before_invoking_query_planner`）
    // と同じく、投影列の束縛（`sql::parser::bind_projection`、`22000`）を
    // I/O 前の `pre_check_schema` で検証すべき。未知列は構文上は受理されるが
    // 束縛不能で必ず拒否される。
    let path = unique_db_path("sql-explain-unknown-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT no_such_column FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unknown projected column must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "unknown projection column must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_unknown_where_column_before_invoking_query_planner() {
    // 上のテストの対（`WHERE` 述語側）。`sql::parser::bind_where_predicates`
    // の未知列拒否（`22000`）も同じく I/O 前の `pre_check_schema` で検証される
    // べき（PR #267 の是正対応）。
    let path = unique_db_path("sql-explain-unknown-where");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE no_such_column = 'x' USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unknown WHERE column must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "unknown WHERE column must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_unregistered_udf_call_before_invoking_query_planner() {
    // codex-review P1・Cursor Bugbot 指摘（PR #267）の判別テスト:
    // 投影列・`WHERE` 述語内の未登録 UDF 呼び出し（`sql::udf_call`、`22000`）も
    // `EXPLAIN` 経路で I/O 前に拒否されるべき（`pre_check_bindable` が
    // `session.udfs()` を渡して同じ検証を再利用する）。
    let path = unique_db_path("sql-explain-unregistered-udf");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT no_such_udf(id) FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unregistered UDF call must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "unregistered UDF call must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_does_not_write_or_execute_search() {
    // `EXPLAIN` はテーブル行データを一切返さない・書き込まないことを、実行前後で
    // 行数（`SELECT COUNT(*)`）が変化しないことにより確認する（副作用なしの回帰）。
    let path = unique_db_path("sql-explain-no-side-effects");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    )
    .expect("EXPLAIN should succeed");

    let count_before = match core
        .execute_sql(&ctx("tenant-a"), "SELECT COUNT(id) FROM docs")
        .expect("count query should succeed")
        .rows
        .into_iter()
        .next()
        .expect("count row")
        .cells
        .into_iter()
        .next()
        .expect("count cell")
    {
        Cell::Integer(n) => n,
        other => panic!("expected Cell::Integer, got {other:?}"),
    };
    assert_eq!(count_before, 1, "EXPLAIN must not write or delete rows");
}
