//! `USING PLAN('<query>')` の実行時ディスパッチ受け入れテスト（TASK-77、対象
//! ビヘイビア: SQL-5。ポインタ: `docs/spec/05-tasks.md` TASK-77・
//! `docs/spec/04-behavior/sql-surface.md` SQL-5）。
//!
//! `tests/sql_allowlist.rs`（存在すれば構文層）とは独立に、実行時の一意ディスパッチ
//! （`sql::allowlist` の構造受理 → `core.rs::EngineCore::plan_query`（TASK-110）→
//! 展開後テキストの再埋め込み → `sql::using_plan::bind_expansion` → 既存 C4
//! ハイブリッド実行形 → `sql::exec::execute_statement`）を、スタブ `LlmClient`・
//! 決定的なテスト `Embedder` を注入して検証する。RLS 暗黙適用の検証は
//! `tests/rls_generalized.rs::using_plan_dispatch_implicitly_applies_rls` が担う
//! （本ファイルでは重複させない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore};
use engine::embedding::{EmbedError, Embedder};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
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
    let op_id = OperationId::parse(&format!("plan-test-op-{id}")).expect("valid operation_id");
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

/// 展開後テキストごとに異なる決定的ベクトルを返す（`text.len()` を成分へ埋め込む
/// だけの単純な写像）。「原質問の埋め込みを使い回していない」ことをテストが
/// 区別できるよう、`embed_batch` に渡された **実際のテキスト** を記録する。
struct RecordingEmbedder {
    dim: u32,
    seen: std::sync::Mutex<Vec<String>>,
}

impl RecordingEmbedder {
    fn new(dim: u32) -> Self {
        Self {
            dim,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn seen_texts(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }
}

impl Embedder for RecordingEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut seen = self.seen.lock().expect("lock");
        let mut out = Vec::with_capacity(texts.len());
        for &t in texts {
            seen.push(t.to_string());
            let base = t.len() as f32 * 0.01;
            out.push(vec![base; self.dim as usize]);
        }
        Ok(out)
    }
}

/// 固定の展開結果を返すスタブ（実 Ollama 疎通は TASK-110 と同じくスコープ外）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

struct FailingLlmClient;

impl LlmClient for FailingLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Err(PlanError::Unavailable)
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#;

fn seeded_storage(path: &std::path::Path) -> Storage {
    let storage = open_storage_with_table(path);
    insert_row(
        &storage,
        1,
        vec![0.1, 0.2, 0.3, 0.4],
        "docs/a.md",
        "alpha content in english",
    );
    insert_row(
        &storage,
        2,
        vec![0.4, 0.3, 0.2, 0.1],
        "docs/b.md",
        "beta content in english",
    );
    storage
}

#[test]
fn using_plan_dispatch_reaches_hybrid_execution_and_returns_seeded_rows() {
    let path = unique_db_path("sql-using-plan-dispatch");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let result = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("USING PLAN dispatch should succeed");
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn using_plan_reembeds_expanded_text_not_the_raw_question() {
    // PLAN-10 ポインタ: 密側の再埋め込み対象は「質問＋展開検索語」の決定的結合で
    // あり、原質問だけの埋め込みを使い回さない。`RecordingEmbedder` が実際に渡された
    // テキストを記録するため、原質問そのものとは異なることを直接確認する。
    let path = unique_db_path("sql-using-plan-reembed");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let embedder = std::sync::Arc::new(RecordingEmbedder::new(DIM));

    struct ArcEmbedder(std::sync::Arc<RecordingEmbedder>);
    impl Embedder for ArcEmbedder {
        fn dim(&self) -> u32 {
            self.0.dim()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.embed_batch(texts)
        }
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(ArcEmbedder(embedder.clone())))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let question = "find content";
    core.execute_sql(
        &ctx("tenant-a"),
        &format!("SELECT id FROM docs USING PLAN('{question}') LIMIT 10"),
    )
    .expect("USING PLAN dispatch should succeed");

    let seen = embedder.seen_texts();
    assert_eq!(
        seen.len(),
        1,
        "expected exactly one embed_batch call, got {seen:?}"
    );
    assert_ne!(
        seen[0], question,
        "USING PLAN must not re-embed the raw question verbatim (PLAN-10)"
    );
    assert_eq!(seen[0], "find content alpha beta");
}

#[test]
fn using_plan_fails_closed_without_query_planner() {
    let path = unique_db_path("sql-using-plan-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('q') LIMIT 5",
        )
        .expect_err("missing query planner must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn using_plan_fails_closed_without_embedder() {
    let path = unique_db_path("sql-using-plan-no-embedder");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('q') LIMIT 5",
        )
        .expect_err("missing embedder must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn using_plan_fails_closed_when_llm_response_is_unavailable() {
    let path = unique_db_path("sql-using-plan-llm-down");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(FailingLlmClient));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('q') LIMIT 5",
        )
        .expect_err("LLM failure must be rejected, not silently degraded");
    assert_eq!(err.wire_code(), "XX000");
    // 部分結果を返さない（呼び出し自体が Err）ことに加え、固定文言のみで構成
    // されることを確認する（プロンプト本文・LLM 応答本文を含めない、security.md P0）。
    assert_eq!(err.client_message(), "internal error");
}

#[test]
fn using_plan_fails_closed_without_body_column() {
    // `path` 列はあるが `body` 列を持たないテーブル。`USING PLAN` の LLM 展開
    // （`plan_query`、TASK-110）が辞書抽出の前提として `path`／`body` の両方を
    // 非 null `TEXT` 列に要求するため（`core.rs::EngineCore::dictionary_snapshot`）、
    // `body` 欠落はここで先に拒否される（`sql::using_plan::bind_expansion` 自身の
    // 本文列解決に到達する前）。`bind_expansion` 側の解決は多層防御として
    // `crates/engine/src/sql/using_plan.rs` の単体テストで別途固定する。
    let path = unique_db_path("sql-using-plan-no-body");
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM no_body USING PLAN('q') LIMIT 5",
        )
        .expect_err("missing body column must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn using_plan_respects_using_mode_precision() {
    let path = unique_db_path("sql-using-plan-mode");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    // `precision` モードは確信度ゲート（`crate::precision::apply_gate`）を通すため、
    // ダミーの定数ベクトル入力では 0 件へ収束しうる（fail-closed な低確信度拒否は
    // 意図した挙動。SEARCH-9 の管轄）。本テストの目的は「`USING MODE` の優先順位
    // 解決が `USING PLAN` 経路でも既存どおり効くこと」であり、`recall`（既定）
    // モードでは同じクエリが結果を返すことと対比して確認する。
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
        )
        .expect("USING MODE should be honored alongside USING PLAN");
    assert!(
        matches!(outcome, SqlOutcome::Query(_)),
        "expected Query outcome"
    );

    let recall_outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'recall'",
        )
        .expect("recall mode should succeed");
    match recall_outcome {
        SqlOutcome::Query(result) => {
            assert!(
                !result.rows.is_empty(),
                "recall mode should return the seeded rows (no confidence gate)"
            );
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

#[test]
fn using_plan_dispatch_error_variant_is_query_planning_or_dispatch_related() {
    // TASK-77 は既存の `CoreError`/`SqlSurfaceError` 分類のみを使い、`PlanError`
    // 用の新規 wire_code 分類を追加しない（ERR-2、TASK-152 の単一真実源を保つ）。
    // ここでは `plan_query` 単体の契約（TASK-110）を素通しで使っていることを、
    // `CoreError::QueryPlannerUnavailable` を直接発生させて確認する。
    let path = unique_db_path("sql-using-plan-core-error-shape");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .plan_query(&ctx("tenant-a"), TABLE, "q")
        .expect_err("plan_query without a configured planner must fail");
    assert!(matches!(err, CoreError::QueryPlannerUnavailable));
}
