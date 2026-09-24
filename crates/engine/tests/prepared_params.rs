//! 拡張クエリプロトコルの `$n` パラメータ束縛（Issue #935・WIRE-12・TASK-217）の
//! engine 層結合テスト。`EngineCore::parse_sql_prepared`（Parse）・
//! `EngineCore::bind_prepared`（Bind）・`EngineCore::describe_prepared_in_session`
//! （Describe）が、SQL テキスト経由の簡易クエリ（`parse_sql`／`execute_sql_in_session`／
//! `describe_parsed_in_session`）と完全に同一の判定順序・実行結果・RLS 境界を
//! 持つことを固定する（第 2 の実行器を作らない設計の証明。詳細・受理位置・
//! スコープ外は `docs/design/wire-extended-query-param-binding.md` 参照）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

// --- リテラル同値性（束縛結果が「同じ値を正しくエスケープしたリテラルで
// 書いた SQL」の `parse_sql` 結果と完全に一致する） --------------------------

#[test]
fn bind_prepared_where_equality_matches_literal_form() {
    let path = unique_db_path("prepared-where-equality-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id, body FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    assert_eq!(prepared.param_count(), 1);

    let bound = core
        .bind_prepared(&prepared, &[some("ja")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("SELECT id, body FROM documents WHERE lang = 'ja' LIMIT 5")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

#[test]
fn bind_prepared_vector_distance_matches_literal_form() {
    let path = unique_db_path("prepared-vector-distance-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents ORDER BY embedding <=> $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some("[0.1,0.2,0.3]")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

#[test]
fn bind_prepared_insert_values_matches_literal_form() {
    let path = unique_db_path("prepared-insert-values-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, $1, $2, $3) USING OPERATION_ID $4",
        )
        .expect("parse_sql_prepared should succeed");
    assert_eq!(prepared.param_count(), 4);
    let bound = core
        .bind_prepared(
            &prepared,
            &[
                some("[0.1,0.2,0.3]"),
                some("hello"),
                some("en"),
                some("op-0001"),
            ],
        )
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'en') USING OPERATION_ID 'op-0001'",
        )
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

#[test]
fn bind_prepared_using_plan_matches_literal_form() {
    let path = unique_db_path("prepared-using-plan-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents USING PLAN($1) LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some("find the handler")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("SELECT id FROM documents USING PLAN('find the handler') LIMIT 5")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

#[test]
fn bind_prepared_using_operation_id_matches_literal_form_for_truncate() {
    let path = unique_db_path("prepared-truncate-operation-id-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("TRUNCATE TABLE documents USING OPERATION_ID $1")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some("op-trunc-0001")])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql("TRUNCATE TABLE documents USING OPERATION_ID 'op-trunc-0001'")
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

// --- インジェクション耐性 ----------------------------------------------------

#[test]
fn bind_prepared_treats_sql_metacharacters_as_a_single_opaque_literal() {
    let path = unique_db_path("prepared-injection-resistance");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    // 挿入した自分自身の行を読み戻すため Private も許可する（RLS-11・TASK-195。
    // `PolicyContext::new` の既定 `Public` のみでは自テナントの書き込みも
    // 読み戻せない）。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'ja') USING OPERATION_ID 'seed-0001'",
    )
    .expect("seed insert should succeed");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO documents (id, embedding, body, lang) VALUES (2, '[0.4,0.5,0.6]', 'world', 'en') USING OPERATION_ID 'seed-0002'",
    )
    .expect("seed insert should succeed");

    let malicious = "ja' OR '1'='1";
    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound = core
        .bind_prepared(&prepared, &[some(malicious)])
        .expect("bind_prepared should succeed");
    let outcome = core
        .execute_parsed_in_session(&ctx, &mut session, &bound)
        .expect("execute should succeed");
    match outcome {
        SqlOutcome::Query(result) => {
            // 値は 1 個の文字列リテラルとしてのみ扱われるため、`lang = "ja' OR
            // '1'='1"` に完全一致する行は無く、文の形も変わらない
            // （両方の行を返す `OR` インジェクションが成立していないことの証明）。
            assert_eq!(result.rows.len(), 0);
        }
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }

    // 改行・カンマ・`--`・別の `$n` らしき文字列を含む値でも、単一リテラルの
    // ままであることを重ねて確認する。
    let tricky_values = [
        "'); DROP TABLE documents; --",
        "a\nb",
        "$2, $3",
        "line1\r\nline2",
    ];
    for value in tricky_values {
        let prepared = core
            .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
            .expect("parse_sql_prepared should succeed");
        let bound = core
            .bind_prepared(&prepared, &[some(value)])
            .expect("bind_prepared should succeed");
        let outcome = core
            .execute_parsed_in_session(&ctx, &mut session, &bound)
            .expect("execute should succeed for opaque literal value");
        match outcome {
            SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 0),
            other => panic!("expected SqlOutcome::Query, got {other:?}"),
        }
    }

    // テーブルは無傷（`DROP TABLE` が実行されていれば以降の SELECT が
    // `42P01` になる）。
    let sanity = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM documents WHERE lang = 'ja' LIMIT 5",
        )
        .expect("table must still exist and be queryable");
    match sanity {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 1),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- 許可位置外の拒否（`42601`） ---------------------------------------------

#[test]
fn parse_sql_prepared_rejects_limit_placeholder() {
    let path = unique_db_path("prepared-rejects-limit");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared(
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT $1",
        )
        .expect_err("LIMIT $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn parse_sql_prepared_rejects_using_mode_placeholder() {
    let path = unique_db_path("prepared-rejects-using-mode");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared(
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE $1",
        )
        .expect_err("USING MODE $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn parse_sql_prepared_rejects_set_search_mode_placeholder() {
    let path = unique_db_path("prepared-rejects-set-search-mode");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared("SET search_mode = $1")
        .expect_err("SET search_mode = $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn parse_sql_prepared_rejects_hybrid_function_argument_placeholder() {
    let path = unique_db_path("prepared-rejects-hybrid-arg");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared(
            "SELECT id FROM documents ORDER BY HYBRID_RRF(embedding, $1, body, 'q') LIMIT 5",
        )
        .expect_err("hybrid function argument $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- 字句・パラメータ番号上限 -------------------------------------------------

#[test]
fn parse_sql_prepared_rejects_zero_and_leading_zero_and_ambiguous_suffix() {
    let path = unique_db_path("prepared-rejects-malformed-lexemes");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    for sql in [
        "SELECT id FROM documents WHERE lang = $0 LIMIT 5",
        "SELECT id FROM documents WHERE lang = $01 LIMIT 5",
        "SELECT id FROM documents WHERE lang = $1a LIMIT 5",
    ] {
        let err = core
            .parse_sql_prepared(sql)
            .expect_err("malformed placeholder must be rejected");
        assert_eq!(err.wire_code(), "42601", "sql = {sql}");
    }
}

#[test]
fn parse_sql_prepared_rejects_parameter_number_over_max() {
    let path = unique_db_path("prepared-rejects-over-max-param");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let err = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $65 LIMIT 5")
        .expect_err("parameter number over MAX_PARAMS must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- 値検証（`22000`） -------------------------------------------------------

#[test]
fn bind_prepared_rejects_null_value() {
    let path = unique_db_path("prepared-rejects-null-value");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[None])
        .expect_err("NULL bind value must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn bind_prepared_rejects_non_utf8_value() {
    let path = unique_db_path("prepared-rejects-non-utf8-value");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[Some(vec![0xff, 0xfe])])
        .expect_err("non-UTF-8 bind value must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn bind_prepared_rejects_missing_operation_id_via_empty_string_value() {
    let path = unique_db_path("prepared-rejects-empty-operation-id");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("TRUNCATE TABLE documents USING OPERATION_ID $1")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[some("")])
        .expect_err("empty operation_id value must be rejected");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn bind_prepared_rejects_value_count_mismatch() {
    let path = unique_db_path("prepared-rejects-value-count-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let err = core
        .bind_prepared(&prepared, &[])
        .expect_err("value count mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .bind_prepared(&prepared, &[some("ja"), some("en")])
        .expect_err("value count mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- 簡易クエリ経路は引き続き `$n` を拒否する --------------------------------

#[test]
fn execute_sql_in_session_still_rejects_dollar_placeholder_in_simple_query() {
    let path = unique_db_path("prepared-simple-query-still-rejects-dollar");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM documents WHERE lang = $1",
        )
        .expect_err("simple query must still reject $n");
    assert_eq!(err.wire_code(), "42601");
}

// --- RLS 暗黙適用（第 2 の実行器を作らない設計の証明） ------------------------

#[test]
fn bind_prepared_search_applies_rls_implicitly_across_tenants() {
    let path = unique_db_path("prepared-rls-implicit");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    // ctx_a は自テナントの書き込みを読み戻すため Private も許可する
    // （RLS-11・TASK-195）。ctx_b は既定 Public のみで十分（他テナント境界の
    // 確認が目的）。
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx_a,
        &mut session,
        "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'secret-a', 'ja') USING OPERATION_ID 'rls-0001'",
    )
    .expect("tenant-a insert should succeed");

    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let bound_for_b = core
        .bind_prepared(&prepared, &[some("ja")])
        .expect("bind_prepared should succeed");
    let outcome = core
        .execute_parsed_in_session(&ctx_b, &mut session, &bound_for_b)
        .expect("execute should succeed");
    match outcome {
        SqlOutcome::Query(result) => {
            assert_eq!(result.rows.len(), 0, "tenant-b must not see tenant-a's row");
        }
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }

    let bound_for_a = core
        .bind_prepared(&prepared, &[some("ja")])
        .expect("bind_prepared should succeed");
    let outcome = core
        .execute_parsed_in_session(&ctx_a, &mut session, &bound_for_a)
        .expect("execute should succeed");
    match outcome {
        SqlOutcome::Query(result) => assert_eq!(result.rows.len(), 1),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- Describe（Bind 前・値未確定でも呼べる） ----------------------------------

#[test]
fn describe_prepared_matches_literal_form_describe() {
    let path = unique_db_path("prepared-describe-parity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared("SELECT id, body FROM documents WHERE lang = $1 LIMIT 5")
        .expect("parse_sql_prepared should succeed");
    let described = core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe_prepared_in_session should succeed");

    let literal = core
        .parse_sql("SELECT id, body FROM documents WHERE lang = 'ja' LIMIT 5")
        .expect("parse_sql should succeed");
    let literal_described = core
        .describe_parsed_in_session(&session, &literal)
        .expect("describe_parsed_in_session should succeed");

    assert_eq!(described, literal_described);
}

// --- 副作用ゼロ（Parse／Describe は行・台帳・世代に触れない） -----------------

#[test]
fn parse_and_describe_prepared_insert_have_no_side_effects() {
    let path = unique_db_path("prepared-insert-no-side-effects");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let session = SessionState::default();

    let prepared = core
        .parse_sql_prepared(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, $1, $2, $3) USING OPERATION_ID $4",
        )
        .expect("parse_sql_prepared should succeed");
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe_prepared_in_session should succeed");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents WHERE lang = 'en' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(
        result.rows.len(),
        0,
        "Parse/Describe must not insert any row"
    );
    let _ = ctx; // ctx は本テストでは未使用（read_ctx のみ使う）だが、
                 // 意図を明示するために保持する。
}
