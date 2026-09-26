//! サブクエリ（`IN (SELECT ...)`・`EXISTS (SELECT ...)`）の結合テスト
//! （Issue #927・SQL-29 (a)・RLS-10 (b)・TASK-213）。`tests/sql_where_or.rs`
//! と同じ流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`＋
//! `CpuScalarProvider`、`EngineCore::execute_sql`／`execute_sql_in_session` を
//! production 経路として検証）。
//!
//! スコープ（実装既定値。`docs/design/sql-subquery.md` 参照）:
//! - 対応: WHERE の `<col> IN (SELECT ...)`・`EXISTS (SELECT ...)`。内側は
//!   `SELECT ... FROM <table> [WHERE ...] LIMIT <n>`（広域取得）のみ。
//! - 対象外（このファイルでは拒否の確認のみ）: スカラー比較サブクエリ・投影
//!   位置のサブクエリ・内側が集計／ランキング付き検索 SELECT・内側 `LIMIT`
//!   省略・相関サブクエリ・拡張クエリプロトコル（Parse/Bind）経由・
//!   `NOT IN`/`NOT EXISTS`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DOCS: &str = "docs";
const ALLOWED_LANGS: &str = "allowed_langs";
const VISITS: &str = "visits";

fn docs_schema() -> TableSchema {
    TableSchema::new(
        DOCS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            // INTEGER/BIGINT 列を対象にした `IN (SELECT ...)`（レビュー指摘対応。
            // `sql::subquery::cell_to_equality_predicate` の `Cell::SignedInteger`
            // 分岐が `WherePredicate::Equality` 経由で常に失敗していたバグの
            // 回帰テスト用。`insert_doc` は指定しないため NULL 許容にする）。
            ColumnDef::new("priority", ColumnType::BigInt, true),
        ],
    )
}

fn allowed_langs_schema() -> TableSchema {
    TableSchema::new(
        ALLOWED_LANGS,
        vec![ColumnDef::new("lang", ColumnType::Text, false)],
    )
}

const PRIORITIES: &str = "priorities";

fn priorities_schema() -> TableSchema {
    TableSchema::new(
        PRIORITIES,
        vec![ColumnDef::new("priority", ColumnType::BigInt, false)],
    )
}

fn visits_schema() -> TableSchema {
    TableSchema::new(
        VISITS,
        vec![ColumnDef::new("note", ColumnType::Text, false)],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql29-subquery");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&docs_schema()).expect("create docs");
    storage
        .create_table(&allowed_langs_schema())
        .expect("create allowed_langs");
    storage
        .create_table(&visits_schema())
        .expect("create visits");
    storage
        .create_table(&priorities_schema())
        .expect("create priorities");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_doc(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {DOCS} (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', '{lang}') \
             USING OPERATION_ID 'doc-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert doc id={id} should succeed: {e:?}"));
}

fn insert_doc_with_priority(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    priority: i64,
) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {DOCS} (id, embedding, lang, priority) VALUES \
             ({id}, '[0.{id},0.1]', '{lang}', {priority}) USING OPERATION_ID 'doc-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert doc id={id} should succeed: {e:?}"));
}

fn insert_priority(core: &EngineCore, ctx: &PolicyContext, id: u64, priority: i64) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {PRIORITIES} (id, priority) VALUES ({id}, {priority}) \
             USING OPERATION_ID 'priority-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert priority id={id} should succeed: {e:?}"));
}

fn insert_allowed_lang(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {ALLOWED_LANGS} (id, lang) VALUES ({id}, '{lang}') \
             USING OPERATION_ID 'lang-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert allowed_lang id={id} should succeed: {e:?}"));
}

fn insert_visit(core: &EngineCore, ctx: &PolicyContext, id: u64, note: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {VISITS} (id, note) VALUES ({id}, '{note}') USING OPERATION_ID 'visit-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert visit id={id} should succeed: {e:?}"));
}

fn seed_docs(core: &EngineCore, ctx: &PolicyContext) {
    insert_doc(core, ctx, 1, "ja");
    insert_doc(core, ctx, 2, "en");
    insert_doc(core, ctx, 3, "fr");
    insert_doc(core, ctx, 4, "de");
}

fn select_ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn expect_error_code(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> engine::sql::allowlist::SqlSurfaceError {
    core.execute_sql(ctx, sql)
        .expect_err(&format!("sql={sql:?} should be rejected"))
}

// --- IN (SELECT ...) -------------------------------------------------------

#[test]
fn in_subquery_text_column_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");
    insert_allowed_lang(&core, &ctx, 2, "fr");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn in_subquery_bigint_column_in_target_is_rejected() {
    // レビュー指摘の回帰テスト（Issue #927 push 前 Review）: `sql::subquery::
    // cell_to_equality_predicate` が以前は `BIGINT`/`INTEGER` 列（`Cell::
    // SignedInteger`）を `WherePredicate::Equality`（`TEXT`/`ENUM` 列専用）へ
    // 変換していたため、`<BIGINT/INTEGER 列> IN (SELECT ...)` は常に「TEXT
    // 列でない」で拒否されていた（ドキュメント上の「対応済み」表明と実装が
    // 矛盾する機能バグ）。`INTEGER`/`BIGINT` 列の等価比較自体がこのリポでは
    // まだ実装されていない（レーン A。`sql::udf_call::bind_expr_in` 参照）
    // ため、実装を追加するのではなく `IN` 対象値の対応型を `TEXT`/`BOOLEAN`
    // のみへ縮小し、`INTEGER`/`BIGINT` は明示的に `22000` へ倒したことを
    // 検証する（`docs/design/sql-subquery.md`「`IN` 対象値の型」節参照）。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert_doc_with_priority(&core, &ctx, 1, "ja", 10);
    insert_priority(&core, &ctx, 1, 10);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE priority IN (SELECT priority FROM {PRIORITIES} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::InvalidInput { .. }
    ));
}

// 疑似列 `id` を対象にした `IN`／`EXISTS`（`id IN (SELECT id FROM ...)`）は
// 対象外（このリポの既存 WHERE 等価述語〔`WherePredicate::Equality`〕自体が
// 疑似列 `id` を対象にしていないため。`sql::subquery::cell_to_equality_predicate`
// の `Cell::Integer` 分岐は将来の `id` 対応拡張に備えた到達可能コードとして
// 残す）。内側の投影に `id` を書くこと自体は他列と同様に受理される
// （`in_subquery_multi_column_projection_is_rejected` 参照）。

#[test]
fn in_subquery_empty_result_matches_no_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // allowed_langs は空のまま。

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(ids.is_empty());
}

#[test]
fn in_subquery_inside_or_branch_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang = 'de' OR lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1, 4]);
}

#[test]
fn in_subquery_multi_column_projection_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE id IN (SELECT id, lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

// --- EXISTS (SELECT ...) ----------------------------------------------------

#[test]
fn exists_subquery_true_keeps_all_visible_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_visit(&core, &ctx, 1, "hit");

    let ids = select_ids(
        &core,
        &ctx,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert_eq!(ids, vec![1, 2, 3, 4]);
}

#[test]
fn exists_subquery_false_excludes_all_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    // visits は空のまま。

    let ids = select_ids(
        &core,
        &ctx,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert!(ids.is_empty());
}

#[test]
fn exists_subquery_combined_with_and_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_visit(&core, &ctx, 1, "hit");

    let ids = select_ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang = 'ja' AND EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
        ),
    );
    assert_eq!(ids, vec![1]);
}

// --- RLS 境界（RECOVER-4 と同型: テナント越境なし） -------------------------

#[test]
fn in_subquery_only_sees_own_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    seed_docs(&core, &ctx_a);
    seed_docs(&core, &ctx_b);
    // tenant-b だけが 'ja' を allowed_langs に持つ。
    insert_allowed_lang(&core, &ctx_b, 1, "ja");

    let ids_a = select_ids(
        &core,
        &ctx_a,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert!(
        ids_a.is_empty(),
        "tenant-a must not see tenant-b's allowed_langs rows via IN subquery"
    );

    let ids_b = select_ids(
        &core,
        &ctx_b,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100"
        ),
    );
    assert_eq!(ids_b, vec![1]);
}

#[test]
fn exists_subquery_only_sees_own_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    seed_docs(&core, &ctx_a);
    seed_docs(&core, &ctx_b);
    // tenant-b だけが visits を持つ。
    insert_visit(&core, &ctx_b, 1, "hit");

    let ids_a = select_ids(
        &core,
        &ctx_a,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert!(
        ids_a.is_empty(),
        "tenant-a must not observe tenant-b's visits row via EXISTS"
    );

    let ids_b = select_ids(
        &core,
        &ctx_b,
        &format!("SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"),
    );
    assert_eq!(ids_b, vec![1, 2, 3, 4]);
}

// --- 上限（ネスト深さ） ------------------------------------------------------

#[test]
fn subquery_nesting_within_limit_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);
    insert_allowed_lang(&core, &ctx, 1, "ja");

    // 深さ 2（最外側 SELECT=1、その WHERE の IN サブクエリ=2）。
    let sql = format!(
        "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS} LIMIT 100) LIMIT 100) LIMIT 100"
    );
    let ids = select_ids(&core, &ctx, &sql);
    assert_eq!(ids, vec![1]);
}

#[test]
fn subquery_nesting_beyond_limit_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    // 深さ 6（`MAX_SUBQUERY_DEPTH` = 4 を超える）。
    let mut sql = format!("SELECT lang FROM {ALLOWED_LANGS} LIMIT 100");
    for _ in 0..6 {
        sql = format!("SELECT lang FROM {ALLOWED_LANGS} WHERE lang IN ({sql}) LIMIT 100");
    }
    sql = format!("SELECT id FROM {DOCS} WHERE lang IN ({sql}) LIMIT 100");

    let err = expect_error_code(&core, &ctx, &sql);
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::PayloadTooLarge { .. }
    ));
}

// --- 文脈の拒否（fail-closed） -----------------------------------------------

#[test]
fn exists_subquery_in_ranked_select_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) \
             ORDER BY embedding <=> '[0.1,0.1]' LIMIT 10"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

#[test]
fn in_subquery_in_update_where_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "UPDATE {DOCS} SET lang = 'xx' WHERE id IN (SELECT id FROM {ALLOWED_LANGS} LIMIT 100) \
                 USING OPERATION_ID 'update-subquery'"
            ),
        )
        .expect_err("subquery in UPDATE WHERE must be rejected");
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

#[test]
fn exists_subquery_over_extended_query_protocol_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed_docs(&core, &ctx_for("tenant-a"));

    let err = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {DOCS} WHERE EXISTS (SELECT id FROM {VISITS} LIMIT 1) LIMIT 100"
        ))
        .expect_err("subquery over the extended query protocol must be rejected");
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}

#[test]
fn inner_subquery_without_limit_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_docs(&core, &ctx);

    let err = expect_error_code(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM {DOCS} WHERE lang IN (SELECT lang FROM {ALLOWED_LANGS}) LIMIT 100"
        ),
    );
    assert!(matches!(
        err,
        engine::sql::allowlist::SqlSurfaceError::UnsupportedSyntax { .. }
    ));
}
