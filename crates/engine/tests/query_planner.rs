//! `engine::core::EngineCore::plan_query` の結合テスト（TASK-110、対象ビヘイビア:
//! PLAN-1。ポインタ: `docs/spec/05-tasks.md` TASK-110・
//! `docs/spec/04-behavior/query-planning.md` PLAN-1）。
//!
//! `tests/dictionary.rs`（TASK-109）と同じ流儀（`unique_db_path` / `CleanupGuard`、
//! 実 `Storage` 上にテーブルを構築、`HashingEmbedder` による決定的埋め込み）で、
//! 固定 JSON を返すモック `LlmClient` を注入して `plan_query` の一連の流れ（辞書接頭辞が
//! 渡ること・展開結果が返ること・接頭辞のバイト同一性・planner 未注入時の fail-closed
//! 拒否・テナント境界での辞書分離）を検証する。`OllamaClient` 自体（HTTP/1.1・JSON の
//! 各経路）の単体検証は `crates/engine/src/query_planner.rs` 内の
//! `#[cfg(test)]` に併設済み（本ファイルでは扱わない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore};
use engine::embedding::HashingEmbedder;
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError, QueryExpansion};
use engine::storage::Storage;
use std::sync::{Arc, Mutex};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 32;

/// 固定 JSON を返すモック `LlmClient`（実 Ollama 非依存）。呼び出しごとに受け取った
/// プロンプトを記録し、テストから検証できるようにする。
struct MockLlmClient {
    response: String,
    seen_prompts: Arc<Mutex<Vec<String>>>,
}

impl LlmClient for MockLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.seen_prompts
            .lock()
            .expect("mock lock poisoned")
            .push(prompt.to_string());
        Ok(self.response.clone())
    }
}

/// 常にエラーを返すモック `LlmClient`（LLM 不達・不正応答経路の検証用）。
struct FailingLlmClient(PlanError);

impl LlmClient for FailingLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Err(self.0.clone())
    }
}

fn new_core_with_documents_table(
    path: &std::path::Path,
    planner: Box<dyn LlmClient>,
) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_query_planner(planner)
}

fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 4,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
}

/// ファイル形 `INSERT` はチャンク行を `Visibility::Private` で書き込む
/// （`tests/dictionary.rs` と同じ理由で `Public`/`Private` 両方を許可する）。
fn tenant_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant")
}

const FIXED_EXPANSION_JSON: &str =
    r#"{"search_terms": ["batch", "cache"], "path_hint": "src/", "kind_hint": "fn"}"#;

// --- 基本フロー: 辞書接頭辞が渡り、展開結果が返ること -----------------------------

#[test]
fn plan_query_returns_expansion_from_mock_llm() {
    let path = unique_db_path("plan-basic");
    let _guard = CleanupGuard(path.clone());
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    let body = "//! module doc about caching\npub fn run_batch() {}\nstruct Wrapper {}\n";
    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", body, "op-1"),
    )
    .expect("file insert should succeed");

    let expansion = core
        .plan_query(&ctx, "documents", "how does batching work?")
        .expect("plan_query should succeed");

    // `QueryExpansion` は TASK-164（PLAN-11）で `#[non_exhaustive]` を付与したため、
    // クレート外からは `default()` ＋フィールド代入でのみ構築できる。
    let mut expected = QueryExpansion::default();
    expected.search_terms = vec!["batch".to_string(), "cache".to_string()];
    expected.path_hint = Some("src/".to_string());
    expected.kind_hint = Some("fn".to_string());
    assert_eq!(expansion, expected);

    // 辞書接頭辞（シンボル・ファイルツリー）と質問文の両方が実際に LLM へ渡っていること。
    let prompts = seen_prompts.lock().expect("mock lock poisoned");
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("run_batch"));
    assert!(prompts[0].contains("src/x.rs"));
    assert!(prompts[0].contains("how does batching work?"));
    // TASK-164（PLAN-11）: 出力スキーマ指示に mode 推定フィールドが含まれること。
    assert!(prompts[0].contains("\"mode\""));
}

// --- 固定接頭辞の使い回し: 同一辞書世代での連続呼び出しはバイト同一 -----------------

#[test]
fn plan_query_prefix_is_byte_identical_across_calls_for_same_generation() {
    let path = unique_db_path("plan-prefix-stable");
    let _guard = CleanupGuard(path.clone());
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn hello() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    core.plan_query(&ctx, "documents", "question one")
        .expect("first plan_query should succeed");
    core.plan_query(&ctx, "documents", "question two")
        .expect("second plan_query should succeed");

    let prompts = seen_prompts.lock().expect("mock lock poisoned");
    assert_eq!(prompts.len(), 2);
    let prefix_a = prompts[0].split("# Question\n").next().unwrap();
    let prefix_b = prompts[1].split("# Question\n").next().unwrap();
    assert_eq!(
        prefix_a, prefix_b,
        "dictionary prefix must be byte-identical across calls within the same generation"
    );
}

// --- fail-closed: planner 未注入は拒否 ---------------------------------------------

#[test]
fn plan_query_rejects_when_no_planner_configured() {
    let path = unique_db_path("plan-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    // `with_query_planner` を呼ばない EngineCore（未注入）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .plan_query(&ctx, "documents", "anything")
        .expect_err("plan_query without a configured planner must fail-closed");
    assert!(matches!(err, CoreError::QueryPlannerUnavailable));
}

// --- fail-closed: LLM 不達・不正応答はエラーを伝播する -----------------------------

#[test]
fn plan_query_propagates_llm_unavailable_error() {
    let path = unique_db_path("plan-llm-unavailable");
    let _guard = CleanupGuard(path.clone());
    let core =
        new_core_with_documents_table(&path, Box::new(FailingLlmClient(PlanError::Unavailable)));
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn hello() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    let err = core
        .plan_query(&ctx, "documents", "anything")
        .expect_err("plan_query must propagate llm client errors");
    assert!(matches!(
        err,
        CoreError::QueryPlanning(PlanError::Unavailable)
    ));
}

#[test]
fn plan_query_rejects_invalid_llm_json_response() {
    let path = unique_db_path("plan-invalid-json");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: "not a json object at all".to_string(),
            seen_prompts: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn hello() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    let err = core
        .plan_query(&ctx, "documents", "anything")
        .expect_err("plan_query must reject an invalid llm response");
    assert!(matches!(
        err,
        CoreError::QueryPlanning(PlanError::InvalidResponse)
    ));
}

// --- テナント境界: 別テナントは辞書接頭辞（プロンプト内容）が分離される ------------

#[test]
fn plan_query_prefix_is_isolated_per_tenant() {
    let path = unique_db_path("plan-tenant-isolation");
    let _guard = CleanupGuard(path.clone());
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    let ctx_a = tenant_ctx("tenant-a");
    let ctx_b = tenant_ctx("tenant-b");

    core.execute_insert_sql(
        &ctx_a,
        &insert_file_sql(
            "documents",
            "src/only_in_a.rs",
            "pub fn only_in_tenant_a() {}\n",
            "op-a",
        ),
    )
    .expect("tenant-a file insert should succeed");
    core.execute_insert_sql(
        &ctx_b,
        &insert_file_sql(
            "documents",
            "src/only_in_b.rs",
            "pub fn only_in_tenant_b() {}\n",
            "op-b",
        ),
    )
    .expect("tenant-b file insert should succeed");

    core.plan_query(&ctx_a, "documents", "q")
        .expect("plan_query for tenant-a should succeed");
    core.plan_query(&ctx_b, "documents", "q")
        .expect("plan_query for tenant-b should succeed");

    let prompts = seen_prompts.lock().expect("mock lock poisoned");
    assert_eq!(prompts.len(), 2);
    let prompt_a = &prompts[0];
    let prompt_b = &prompts[1];

    assert!(prompt_a.contains("only_in_tenant_a"));
    assert!(!prompt_a.contains("only_in_tenant_b"));
    assert!(prompt_b.contains("only_in_tenant_b"));
    assert!(!prompt_b.contains("only_in_tenant_a"));
}
