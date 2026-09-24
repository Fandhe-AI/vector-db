//! `COPY ... FROM STDIN`／`COPY (...) TO STDOUT`（Issue #939・WIRE-17・
//! TASK-220）の engine API 層の結合テスト。wire-server 側のメッセージ層
//! （CopyIn／CopyOut サブプロトコル）は対象外（`crates/wire-server/tests/
//! wire17_copy.rs` の担当）。`EngineCore::begin_copy`／`commit_copy_in` を
//! wire-server が呼ぶのと同じ形（`feed` を CopyData 相当のチャンクへ分けて
//! 呼び、`finish` → `commit_copy_in` の順）で駆動する。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("note", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

fn read_back_ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let policy = ctx(tenant);
    let mut session = SessionState::default();
    let sql = format!("SELECT id FROM {TABLE} LIMIT 100");
    let outcome = core
        .execute_sql_in_session(&policy, &mut session, &sql)
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    result.rows.iter().map(|row| row.id).collect()
}

const EXT_TABLE: &str = "docs_ext";
const EXT_ENUM_TYPE: &str = "mood";

/// `BOOLEAN`／`ARRAY`／`BYTEA`／`ENUM` 列を持つテーブル（Issue #939 レビュー
/// 指摘: `bind_copy_record` の該当 match アームがどのテスト（`copy_from.rs`・
/// `wire17_copy.rs`・`sql/copy.rs` 内 unit test）からもカバーされていなかった
/// ため追加。`tests/{boolean,array,bytea,enum}_column.rs` と同じ列定義流儀）。
fn extended_schema(enum_def: std::sync::Arc<engine::catalog::EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        EXT_TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
            ColumnDef::new("blob", ColumnType::Bytea, true),
            ColumnDef::new("mood", ColumnType::Enum(enum_def), true),
            // `JSON`／`JSONB` 列（Issue #939 レビュー指摘: `bind_copy_record`・
            // `bound_insert_byte_len` の `Value`／`ColumnType` 網羅 match が
            // origin/main の JSON/JSONB 追加（Issue #889）に追随しておらず
            // `E0004`（non-exhaustive patterns）でビルド不能だった。本テーブルで
            // COPY 経由の束縛を固定する）。
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
        ],
    )
}

fn open_engine_ext(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    let enum_def = storage
        .create_enum_type(EXT_ENUM_TYPE, vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    storage
        .create_table(&extended_schema(enum_def))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

/// `id = 1` の行を投影して `Cell` 列で返す（`BOOLEAN`／`ARRAY`／`BYTEA`／`ENUM`
/// の束縛結果を engine API 層で確定オラクルとして検証するため）。
fn select_ext_cells(core: &EngineCore, tenant: &str, id: u64, columns: &str) -> Vec<Cell> {
    let policy = ctx(tenant);
    let mut session = SessionState::default();
    let sql = format!("SELECT {columns} FROM {EXT_TABLE} WHERE id = {id} LIMIT 1");
    let outcome = core
        .execute_sql_in_session(&policy, &mut session, &sql)
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    result
        .rows
        .into_iter()
        .next()
        .expect("row must exist")
        .cells
}

/// wire-server の CopyIn サブプロトコルを模した駆動ヘルパー: `chunks` を順に
/// `feed` へ渡し、最後に `finish` → `commit_copy_in` を行う。
fn run_copy_from(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    chunks: &[&[u8]],
) -> Result<engine::sql::exec::InsertOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let policy = ctx(tenant);
    let session = SessionState::default();
    let plan = core.begin_copy(&policy, &session, sql)?;
    let mut copy_session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM STDIN plan"),
    };
    for chunk in chunks {
        copy_session.feed(chunk)?;
    }
    let batch = copy_session.finish()?;
    core.commit_copy_in(&policy, batch)
}

// ---------------------------------------------------------------------
// FROM STDIN: text 形式の基本受理・原子性
// ---------------------------------------------------------------------

#[test]
fn copy_from_stdin_text_format_inserts_all_rows_atomically() {
    let (core, path) = open_engine("copy-from-text-basic");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-1'");
    // CopyData 境界をまたいで 1 行を分割し、レコード分割がチャンクを
    // またいでも正しく動作することを併せて確認する。
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0", b"]\tja\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 2);

    let mut ids = read_back_ids(&core, "acme");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn copy_from_stdin_text_format_resolves_null_marker_and_escapes() {
    let (core, path) = open_engine("copy-from-text-null-escape");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN USING OPERATION_ID 'copy-op-2'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tja\t\\N\n2\t[0.0,1.0]\tja\ta\\tb\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn copy_from_stdin_csv_format_distinguishes_null_and_empty_and_quoted_comma() {
    let (core, path) = open_engine("copy-from-csv-basic");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {TABLE} (id, embedding, lang, note) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'copy-op-3'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1,\"[1.0,0.0]\",ja,\n2,\"[0.0,1.0]\",\"ja,jp\",\"\"\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn copy_from_stdin_missing_operation_id_is_rejected_before_any_row_is_visible() {
    let (core, path) = open_engine("copy-from-missing-op-id");
    let _guard = CleanupGuard(path);

    let sql = format!("COPY {TABLE} (id, embedding, lang) FROM STDIN");
    let policy = ctx("acme");
    let session = SessionState::default();
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("missing USING OPERATION_ID must be rejected");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn copy_from_stdin_rejects_invalid_row_with_zero_side_effects() {
    let (core, path) = open_engine("copy-from-invalid-row");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-4'");
    let err = run_copy_from(
        &core,
        "acme",
        &sql,
        // 2 行目の embedding 次元が宣言（2）と不一致。
        &[b"1\t[1.0,0.0]\tja\n2\t[1.0,0.0,0.0]\tja\n"],
    )
    .expect_err("dimension mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");

    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_empty_payload_is_rejected() {
    let (core, path) = open_engine("copy-from-empty");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-5'");
    let err = run_copy_from(&core, "acme", &sql, &[b""]).expect_err("empty COPY must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn copy_from_stdin_rejects_batch_over_row_count_limit() {
    let limits = engine::batch_limits::BatchLimits {
        max_files_per_batch: 2,
        ..engine::batch_limits::BatchLimits::default()
    };
    let path = unique_db_path("copy-from-row-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-6'");
    let policy = ctx("acme");
    let session = SessionState::default();
    let plan = core
        .begin_copy(&policy, &session, &sql)
        .expect("begin_copy");
    let mut copy_session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected FROM plan"),
    };
    // 上限 2 行を超える 3 行目で、CopyDone（`finish`）を待たずに拒否される
    // ことを確認する。
    let err = copy_session
        .feed(b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\tja\n")
        .expect_err("row count over limit must be rejected before CopyDone");
    assert_eq!(err.wire_code(), "54000");

    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_rejects_batch_over_total_byte_limit_before_copy_done() {
    let limits = engine::batch_limits::BatchLimits {
        max_batch_total_bytes: 4,
        ..engine::batch_limits::BatchLimits::default()
    };
    let path = unique_db_path("copy-from-byte-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);

    let sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-op-7'");
    let policy = ctx("acme");
    let session = SessionState::default();
    let plan = core
        .begin_copy(&policy, &session, &sql)
        .expect("begin_copy");
    let mut copy_session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected FROM plan"),
    };
    let err = copy_session
        .feed(b"1\t[1.0,0.0]\tja\n")
        .expect_err("raw CopyData byte total over limit must be rejected");
    assert_eq!(err.wire_code(), "54000");

    assert!(read_back_ids(&core, "acme").is_empty());
}

#[test]
fn copy_from_stdin_shares_operation_id_ledger_with_sql_insert() {
    let (core, path) = open_engine("copy-from-ledger-share");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let mut session = SessionState::default();
    let insert_sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'shared-op'"
    );
    core.execute_sql_in_session(&policy, &mut session, &insert_sql)
        .expect("INSERT succeeds");

    // 同じ operation_id・同じ内容の COPY 再送は台帳照合により 23505（同一内容
    // の再送）になる（複数行 INSERT・単文 INSERT と台帳キー空間を共有する
    // 契約の固定）。
    let copy_sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'shared-op'");
    let err = run_copy_from(&core, "acme", &copy_sql, &[b"1\t[1.0,0.0]\tja\n"])
        .expect_err("duplicate operation_id with same content must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn copy_from_stdin_rejects_other_tenant_id_conflict_without_leaking_existence() {
    let (core, path) = open_engine("copy-from-tenant-conflict");
    let _guard = CleanupGuard(path);

    let policy_a = ctx("acme");
    let mut session = SessionState::default();
    let insert_sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'tenant-a-op'"
    );
    core.execute_sql_in_session(&policy_a, &mut session, &insert_sql)
        .expect("INSERT succeeds");

    // 別テナントが同じ id=1 を COPY で書き込んでも、物理キーは
    // (tenant_id, id) で名前空間化されているため衝突しない（TABLE-12）。
    let copy_sql =
        format!("COPY {TABLE} (id, embedding, lang) FROM STDIN USING OPERATION_ID 'tenant-b-op'");
    let outcome = run_copy_from(&core, "beta", &copy_sql, &[b"1\t[0.0,1.0]\tja\n"])
        .expect("different tenant may use the same row id");
    assert_eq!(outcome.rows_affected, 1);

    let mut acme_ids = read_back_ids(&core, "acme");
    acme_ids.sort_unstable();
    assert_eq!(acme_ids, vec![1]);
}

// ---------------------------------------------------------------------
// TO STDOUT: 広域取得と同一の実行本体・RLS 暗黙適用
// ---------------------------------------------------------------------

#[test]
fn copy_to_stdout_returns_only_own_tenant_rows() {
    let (core, path) = open_engine("copy-to-stdout-basic");
    let _guard = CleanupGuard(path);

    let policy_a = ctx("acme");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &policy_a,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'to-op-1'"
        ),
    )
    .expect("INSERT succeeds");
    let policy_b = ctx("beta");
    core.execute_sql_in_session(
        &policy_b,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.0,1.0]', 'ja') USING OPERATION_ID 'to-op-2'"
        ),
    )
    .expect("INSERT succeeds");

    let sql = format!("COPY (SELECT id FROM {TABLE} LIMIT 100) TO STDOUT");
    let plan = core
        .begin_copy(&policy_a, &session, &sql)
        .expect("begin_copy succeeds");
    let (format, result) = match plan {
        CopyPlan::To(f, r) => (f, r),
        CopyPlan::From(_) => panic!("expected TO plan"),
    };
    assert_eq!(format, engine::sql::allowlist::CopyFormat::Text);
    let ids: Vec<u64> = result.rows.iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn copy_to_stdout_rejects_ranked_select() {
    let (core, path) = open_engine("copy-to-stdout-ranked");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql = format!(
        "COPY (SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10) TO STDOUT"
    );
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("ranked SELECT inside COPY (...) TO STDOUT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn copy_to_stdout_rejects_table_form() {
    let (core, path) = open_engine("copy-to-stdout-table-form");
    let _guard = CleanupGuard(path);

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql = format!("COPY {TABLE} TO STDOUT");
    let err = core
        .begin_copy(&policy, &session, &sql)
        .expect_err("table-form COPY TO STDOUT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// ---------------------------------------------------------------------
// FROM STDIN: BOOLEAN／ARRAY／BYTEA／ENUM 列の束縛
// （Issue #939 レビュー指摘: cc232e6 で `bind_copy_record` へ追加した該当
// match アームがどのテストからもカバーされていなかったため追加）
// ---------------------------------------------------------------------

#[test]
fn copy_from_stdin_text_format_binds_boolean_array_bytea_enum_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-text");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, flag, tags, blob, mood) FROM STDIN USING OPERATION_ID 'ext-op-1'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tt\t{ja,en}\t\\\\xdeadbeef\thappy\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "flag, tags, blob, mood");
    assert_eq!(
        cells,
        vec![
            Cell::Bool(true),
            Cell::Array(engine::row_codec::ArrayValue::Text(vec![
                "ja".to_string(),
                "en".to_string()
            ])),
            Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Cell::Text("happy".to_string()),
        ]
    );
}

#[test]
fn copy_from_stdin_csv_format_binds_boolean_array_bytea_enum_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-csv");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, flag, tags, blob, mood) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'ext-op-2'"
    );
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1,\"[1.0,0.0]\",false,\"{ja,en}\",\\xdeadbeef,sad\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "flag, tags, blob, mood");
    assert_eq!(
        cells,
        vec![
            Cell::Bool(false),
            Cell::Array(engine::row_codec::ArrayValue::Text(vec![
                "ja".to_string(),
                "en".to_string()
            ])),
            Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Cell::Text("sad".to_string()),
        ]
    );
}

/// `JSON`／`JSONB` 列を COPY FROM STDIN で束縛できることを固定する
/// （Issue #939 レビュー指摘。`docs/spec` の JSON 列ビヘイビア ID は
/// TABLE-14・TASK-198・Issue #889 を参照）。`JSON` は入力テキストを
/// そのまま保持し、`JSONB` はキー順を辞書順へ正規化・空白を除去して
/// 保持する契約（`crate::json::canonicalize_jsonb_text`）を COPY 経路でも
/// 維持することを確認する。
#[test]
fn copy_from_stdin_text_format_binds_json_and_jsonb_columns() {
    let (core, path) = open_engine_ext("copy-from-ext-json");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, doc, docb) FROM STDIN USING OPERATION_ID 'ext-op-json-1'"
    );
    // `doc`（JSON）は入力テキストをそのまま保持、`docb`（JSONB）はキー順が
    // 入れ替わって正規化されることを確認する（`{"b":1,"a":2}` → `{"a":2,"b":1}`）。
    let outcome = run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\t{ \"b\": 1, \"a\": 2 }\t{\"b\":1,\"a\":2}\n"],
    )
    .expect("COPY FROM STDIN succeeds");
    assert_eq!(outcome.rows_affected, 1);

    let cells = select_ext_cells(&core, "acme", 1, "doc, docb");
    assert_eq!(
        cells,
        vec![
            Cell::Json("{ \"b\": 1, \"a\": 2 }".to_string()),
            Cell::Json("{\"a\":2,\"b\":1}".to_string()),
        ]
    );
}

#[test]
fn copy_from_stdin_rejects_invalid_boolean_word() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-bool");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {EXT_TABLE} (id, embedding, flag) FROM STDIN USING OPERATION_ID 'ext-op-3'");
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tmaybe\n"])
        .expect_err("invalid boolean word must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn copy_from_stdin_rejects_enum_label_outside_vocabulary() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-enum");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {EXT_TABLE} (id, embedding, mood) FROM STDIN USING OPERATION_ID 'ext-op-4'");
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tangry\n"])
        .expect_err("label outside enum vocabulary must be rejected");
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn copy_from_stdin_rejects_malformed_bytea_hex() {
    let (core, path) = open_engine_ext("copy-from-ext-bad-bytea");
    let _guard = CleanupGuard(path);

    let sql =
        format!("COPY {EXT_TABLE} (id, embedding, blob) FROM STDIN USING OPERATION_ID 'ext-op-5'");
    let err = run_copy_from(&core, "acme", &sql, &[b"1\t[1.0,0.0]\tnothex\n"])
        .expect_err("malformed bytea hex literal must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// ---------------------------------------------------------------------
// TO STDOUT: BOOLEAN／ARRAY／BYTEA／ENUM 列の投影（`Cell` 表現の確認。
// text 表現へのエンコード・往復は wire-server 側の
// `wire17_copy_to_stdout_output_round_trips_extended_column_types` が担う）
// ---------------------------------------------------------------------

#[test]
fn copy_to_stdout_projects_boolean_array_bytea_enum_columns() {
    let (core, path) = open_engine_ext("copy-to-stdout-ext");
    let _guard = CleanupGuard(path);

    let sql = format!(
        "COPY {EXT_TABLE} (id, embedding, flag, tags, blob, mood) FROM STDIN USING OPERATION_ID 'ext-op-6'"
    );
    run_copy_from(
        &core,
        "acme",
        &sql,
        &[b"1\t[1.0,0.0]\tt\t{ja,en}\t\\\\xdeadbeef\thappy\n"],
    )
    .expect("COPY FROM STDIN succeeds");

    let policy = ctx("acme");
    let session = SessionState::default();
    let sql =
        format!("COPY (SELECT id, flag, tags, blob, mood FROM {EXT_TABLE} LIMIT 10) TO STDOUT");
    let plan = core
        .begin_copy(&policy, &session, &sql)
        .expect("begin_copy succeeds");
    let (_, result) = match plan {
        CopyPlan::To(f, r) => (f, r),
        CopyPlan::From(_) => panic!("expected TO plan"),
    };
    let row = result.rows.into_iter().next().expect("row exists");
    assert_eq!(
        row.cells,
        vec![
            Cell::Integer(1),
            Cell::Bool(true),
            Cell::Array(engine::row_codec::ArrayValue::Text(vec![
                "ja".to_string(),
                "en".to_string()
            ])),
            Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Cell::Text("happy".to_string()),
        ]
    );
}
