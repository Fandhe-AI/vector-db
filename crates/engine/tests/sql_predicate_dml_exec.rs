//! 述語つき `UPDATE ... WHERE`／`DELETE ... WHERE`（SQL-19・TASK-192、Issue #871）
//! の実行結線の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-192・
//! `docs/spec/04-behavior/sql-surface.md` SQL-19・`docs/spec/04-behavior/
//! recovery.md` RECOVER-11（検討中）・RECOVER-9・RECOVER-10・
//! `docs/spec/04-behavior/rls.md` RLS-7・RLS-9・RLS-10・TABLE-12。
//!
//! `EngineCore::execute_sql_in_session` を production 経路として検証する
//! （`sql_delete_single_row.rs`・`truncate_table.rs` と同じ流儀。実 `Storage`
//! ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。ADR
//! `docs/design/multi-row-dml-operation-id.md`（Issue #868。ステータス
//! Proposed・オーナー承認待ち）を作業前提として実行結線した旨を
//! `docs/design/predicate-dml-exec.md` に記録済み。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core_with_table() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-predicate-dml-exec");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn insert_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    table: &str,
    id: u64,
    lang: &str,
    body: &str,
    seq: &str,
) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        ctx,
        &mut session,
        &format!(
            "INSERT INTO {table} (id, embedding, lang, body) VALUES \
             ({id}, '[0.1,0.2]', '{lang}', '{body}') USING OPERATION_ID 'seed-{table}-{id}-{seq}'"
        ),
    )
    .expect("insert row");
}

fn count_star(core: &EngineCore, ctx: &PolicyContext, table: &str) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {table}"))
        .expect("count(*) should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn scan_lang_rows(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<(u64, String)> {
    core.execute_sql(ctx, &format!("SELECT id, lang FROM {table} LIMIT 100"))
        .expect("scan should succeed")
        .rows
        .iter()
        .map(|r| {
            let lang = match &r.cells[1] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            };
            (r.id, lang)
        })
        .collect()
}

fn execute(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(ctx, &mut session, sql)
}

/// 候補列挙・応答件数の同値性: `SELECT` の候補集合と `DELETE ... WHERE` の
/// `rows_affected`・削除後残存集合が一致する（第 2 の述語評価器を作らない
/// 契約の外部観測）。
#[test]
fn predicate_delete_removes_exactly_the_matching_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "b", "2");
    insert_row(&core, &alice, TABLE, 3, "ja", "c", "3");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-del-1'"),
    )
    .expect("predicate DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }

    let remaining = scan_lang_rows(&core, &alice, TABLE);
    assert_eq!(remaining, vec![(2, "en".to_string())]);
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// 候補列挙・SET 反映の同値性: `UPDATE ... WHERE` は一致行のみ SET 対象列を
/// 更新し、非対象列・他行は不変のまま。
#[test]
fn predicate_update_changes_exactly_the_matching_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "b", "2");
    insert_row(&core, &alice, TABLE, 3, "ja", "c", "3");

    let outcome = execute(
        &core,
        &alice,
        &format!("UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-1'"),
    )
    .expect("predicate UPDATE should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }

    let mut rows = scan_lang_rows(&core, &alice, TABLE);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            (1, "fr".to_string()),
            (2, "en".to_string()),
            (3, "fr".to_string()),
        ]
    );
}

/// RLS 境界（RLS-9・RLS-10）: alice が bob の `Public` 行と一致する述語で
/// DELETE/UPDATE しても bob 行は不変。応答（成否・件数）は「bob 行が存在
/// しない DB」で同じ文を実行した場合と一致する（bob 行の有無が alice の
/// 応答へ一切影響しない）。
#[test]
fn predicate_delete_and_update_do_not_affect_other_tenant_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &bob, TABLE, 2, "ja", "b", "1");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-del-cross'"),
    )
    .expect("predicate DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    // bob 行は不変（bob 自身の視点で確認）。
    assert_eq!(count_star(&core, &bob, TABLE), 1);
    // alice の視点では自テナント行のみが対象になり、bob 行は候補にすら
    // ならない（`rows_affected` が bob 行の有無に依存しない）。
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    let outcome = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-upd-cross'"
        ),
    )
    .expect("predicate UPDATE should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    let bob_rows = scan_lang_rows(&core, &bob, TABLE);
    assert_eq!(bob_rows, vec![(2, "ja".to_string())]);
}

/// 0 行一致: 台帳は commit され（世代は進まない）、同一 `operation_id` の
/// 再送は対象行の有無に依存せず `23505` へ収束する（RECOVER-10・RECOVER-11）。
#[test]
fn predicate_delete_zero_match_still_records_ledger_and_rejects_resend() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "en", "a", "1");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-zero'"),
    )
    .expect("zero-match predicate DELETE should still succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-zero'"),
    )
    .expect_err("resend of a recorded operation_id must be rejected");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// 内容照合ハッシュ（RECOVER-11）: `WHERE` 述語の宣言順を入れ替えた再送は
/// 内容不一致として `22023` を返す（`AND` 平坦列挙・並べ替えない契約）。
#[test]
fn predicate_delete_resend_with_reordered_predicates_is_content_mismatch() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' AND id > 0 USING OPERATION_ID 'op-reorder'"
        ),
    )
    .expect("first DELETE should succeed");

    // 同じ operation_id・述語順を入れ替えた文を再送する（行は既に削除済み
    // だが、台帳照合は列挙より先に行われるため実削除の有無に依存しない）。
    let err = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE id > 0 AND lang = 'ja' USING OPERATION_ID 'op-reorder'"
        ),
    )
    .expect_err("reordered predicate resend must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

/// `WHERE visible()` のみの述語つき DELETE は自テナント全行を候補にする
/// （#870 の決定を継承。歯止めは影響行数上限のみ。本テストは #870 の
/// 決定を再決定しない）。
#[test]
fn predicate_delete_visible_only_matches_all_own_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");
    insert_row(&core, &alice, TABLE, 2, "en", "b", "2");
    insert_row(&core, &alice, TABLE, 3, "fr", "c", "3");

    let outcome = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE visible() USING OPERATION_ID 'op-visible'"),
    )
    .expect("visible()-only predicate DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 3),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// fail-closed: `operation_id` 欠落は `23502`（構造検証・束縛のいずれよりも
/// 先に短絡する。TASK-92・RECOVER-1）。
#[test]
fn predicate_delete_missing_operation_id_is_rejected() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja'"),
    )
    .expect_err("missing operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// fail-closed: `WHERE visible()` のみの述語つき UPDATE は束縛段で拒否する
/// （#869 の既存契約。実行に到達しない）。
#[test]
fn predicate_update_visible_only_is_rejected_at_bind() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "ja", "a", "1");

    let err = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE visible() USING OPERATION_ID 'op-visible-upd'"
        ),
    )
    .expect_err("visible()-only predicate UPDATE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// `execute_sql`（セッション無し）は引き続き `DELETE`／`UPDATE` を拒否する
/// （既存契約は本 Issue で変更しない）。
#[test]
fn execute_sql_without_session_still_rejects_predicate_delete_and_update() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);

    let err = core
        .execute_sql(
            &alice,
            &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-x'"),
        )
        .expect_err("execute_sql must reject DELETE");
    assert_eq!(err.wire_code(), "42601");

    let err = core
        .execute_sql(
            &alice,
            &format!("UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'op-y'"),
        )
        .expect_err("execute_sql must reject UPDATE");
    assert_eq!(err.wire_code(), "42601");
}

/// `MAX_DML_AFFECTED_ROWS`（1,000）件を超える一致は `54000` で拒否され、
/// 副作用がゼロのまま（行不変・台帳未記録＝再送しても再び `54000`）となる
/// （ADR §6「4.」。`engine::sql::parser::MAX_DML_AFFECTED_ROWS` を直接参照し、
/// 本リポ独自の実装既定値が変わっても追随する）。多数行の投入は
/// `MAX_INSERT_ROWS_PER_STATEMENT`（1,000）に収まるよう複数行 `VALUES`
/// （SQL-16・TASK-190）2 文に分割する。
#[test]
fn predicate_delete_over_limit_is_rejected_with_no_side_effects() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);

    let limit = engine::sql::parser::MAX_DML_AFFECTED_ROWS;
    let over = limit + 1; // 1,001 件（1,000 件ちょうどは成功する対照として別テストへ）

    // 複数行 `VALUES`（SQL-16・TASK-190）の実行結線は INDEX-4 の一括投入上限
    // （既定 64 ファイル相当）を経由するため、本テストの目的（DML 影響行数
    // 上限 `MAX_DML_AFFECTED_ROWS`＝1,000 の検証）には使えない。単一行 `INSERT`
    // をループで投入する（`insert_row` ヘルパーと同じ経路）。
    for id in 1..=over {
        insert_row(
            &core,
            &alice,
            TABLE,
            id as u64,
            "ja",
            "b",
            &format!("over-{id}"),
        );
    }
    assert_eq!(count_star(&core, &alice, TABLE), over as u64);

    let err = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-over-limit'"),
    )
    .expect_err("over-limit predicate DELETE must be rejected");
    assert_eq!(err.wire_code(), "54000");
    // 副作用ゼロ: 行数不変・台帳未記録（同一 operation_id は再送しても
    // 再び 54000 になる。23505/22023 にはならない＝台帳に痕跡が残っていない）。
    assert_eq!(count_star(&core, &alice, TABLE), over as u64);
    let err_again = execute(
        &core,
        &alice,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-over-limit'"),
    )
    .expect_err("resend of an unrecorded operation_id must be the same over-limit rejection");
    assert_eq!(err_again.wire_code(), "54000");
}
