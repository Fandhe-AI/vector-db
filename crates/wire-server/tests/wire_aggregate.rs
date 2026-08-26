//! 集計クエリ（TASK-168・SQL-13／SQL-14）が PostgreSQL wire プロトコル v3 の
//! 簡易クエリ経路（生バイトクライアント）で契約どおりの応答（`RowDescription`／
//! `DataRow`／`CommandComplete`／`ErrorResponse` の SQLSTATE）として観測できる
//! ことを検証する結合テスト（ポインタ: `docs/spec/05-tasks.md` TASK-168、
//! `docs/spec/04-behavior/sql-surface.md` SQL-13・SQL-14、
//! `docs/spec/04-behavior/rls.md` RLS-7・RLS-8）。
//!
//! 集計値・拒否形状そのものは `crates/engine/tests/sql_aggregate.rs`・
//! `sql_group_by.rs`（in-process）が既に確定オラクルとして検証済みのため、
//! 本ファイルは同じ規則を **wire フレーミング** 越しに再確認することに徹する
//! （オラクル値は手計算の固定値として本ファイル内に記述し、engine 実装を
//! 読み直して再計算しない）。無改造の実クライアント（psql／psycopg／pg）を
//! 使う 3 クライアント統合検証は `tests/three_client_e2e.rs`（`#[ignore]`）が
//! 代表ケースのみを担い、本ファイルが主たる回帰保護を担う（常時 `make ci`）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{RowInput, Storage, Visibility};

use common::*;

/// `docs(embedding VECTOR(2), lang TEXT)` を持つ `EngineCore` を新設し、
/// 3 テナントそれぞれの Public 行 1 件（可視集合 = {1,2,3}）に加え、
/// wire 越しには不可視な Private 行 2 件（tenant-a: id=11／tenant-b: id=12）を
/// 投入する。`"xx"` は Private 行にしか存在しない `lang` 値で、`GROUP BY` の
/// グループ値から他テナントの Private 行の存在が漏れないことの対照に使う。
///
/// 独立オラクル（手計算・固定値。`crates/engine/src/sql/aggregate.rs`・
/// `group_by.rs` の実装を読み直して再計算しない）。可視行（Public のみ、
/// wire 認証経路の `PolicyContext` は Public のみ許可）:
/// id=1 (tenant-a, [1,0], "ja") / id=2 (tenant-b, [0,1], "en") /
/// id=3 (tenant-c, [-1,0], "ja")。
/// `COUNT(*)=3`・`SUM(id)=6`・`AVG(id)=2`・`MIN(id)=1`・`MAX(id)=3`・
/// `MIN(lang)="en"`・`MAX(lang)="ja"`・各ノルムが 1 のため
/// `SUM(vec_norm(embedding))=3`。`GROUP BY lang` → `en:(n=1,s=2)`,
/// `ja:(n=2,s=4)`（キー昇順）。
fn new_core_aggregate_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-aggregate-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let public_rows: [(&str, u64, [f32; 2], &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja"),
        ("tenant-b", 2, [0.0, 1.0], "en"),
        ("tenant-c", 3, [-1.0, 0.0], "ja"),
    ];
    for (tenant, id, emb, lang) in public_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    // Private 行（wire 認証経路では全ユーザーに不可視。`lang="xx"` は Private
    // 行にしか存在しない値のため、`GROUP BY` に "xx" が現れれば RLS 違反）。
    let private_rows: [(&str, u64, [f32; 2]); 2] =
        [("tenant-a", 11, [1.0, 0.0]), ("tenant-b", 12, [0.0, 1.0])];
    for (tenant, id, emb) in private_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[Value::Vector(emb.to_vec()), Value::Text("xx".to_string())],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn connect_alice(addr: std::net::SocketAddr) -> std::net::TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn spawn_with_alice(core: Arc<EngineCore>) -> (std::net::TcpStream, std::path::PathBuf) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (connect_alice(addr), users_path)
}

/// SQL-13: 単一行の集計関数呼び出し（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`）が、
/// 別名（`AS`）どおりの列名・独立オラクルどおりのテキスト値で返る。
#[test]
fn sql13_single_row_aggregates_are_returned_as_text_columns_over_wire() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT COUNT(*) AS n, COUNT(lang) AS c_lang, SUM(id) AS s, AVG(id) AS a, \
         MIN(id) AS lo, MAX(id) AS hi, MIN(lang) AS l_min, MAX(lang) AS l_max FROM docs",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(
        columns,
        vec!["n", "c_lang", "s", "a", "lo", "hi", "l_min", "l_max"],
        "column names must match the SELECT list AS aliases in order"
    );
    let row = read_data_row(&mut stream);
    let expected: Vec<Option<&str>> = vec![
        Some("3"),
        Some("3"),
        Some("6"),
        Some("2"),
        Some("1"),
        Some("3"),
        Some("en"),
        Some("ja"),
    ];
    let actual: Vec<Option<&str>> = row.iter().map(|c| c.as_deref()).collect();
    assert_eq!(actual, expected);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // 既定別名（`AS` 省略時は関数名の小文字）。
    send_simple_query(&mut stream, "SELECT COUNT(*) FROM docs");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["count"]);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("3"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-13: `WHERE` 述語での絞り込み・式（`vec_norm`）を引数に取る集計。
#[test]
fn sql13_where_filter_and_scalar_expression_aggregate() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT COUNT(*) AS n FROM docs WHERE lang = 'ja'",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("2"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT SUM(vec_norm(embedding)) AS s FROM docs",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("3"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-13: 空集合の契約（`COUNT` は `0`、それ以外は `NULL`＝DataRow の -1 長）。
#[test]
fn sql13_empty_set_contract_count_zero_and_others_null() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT COUNT(*) AS n, SUM(id) AS s, AVG(id) AS a, MIN(lang) AS m \
         FROM docs WHERE lang = 'zz'",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    let actual: Vec<Option<&str>> = row.iter().map(|c| c.as_deref()).collect();
    assert_eq!(actual, vec![Some("0"), None, None, None]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-14: `GROUP BY` 既定順（キー昇順）・`HAVING`・`ORDER BY` ＋ `LIMIT`。
#[test]
fn sql14_group_by_default_order_having_order_by_limit() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT lang, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["lang", "n", "s"]);
    let mut rows = Vec::new();
    for _ in 0..2 {
        let row = read_data_row(&mut stream);
        rows.push(
            row.iter()
                .map(|c| c.clone().expect("no NULL expected"))
                .collect::<Vec<_>>()
                .join("|"),
        );
    }
    assert_eq!(
        rows,
        vec!["en|1|2", "ja|2|4"],
        "groups must be key-ascending"
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT lang, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang HAVING n >= 2",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    let joined: Vec<Option<&str>> = row.iter().map(|c| c.as_deref()).collect();
    assert_eq!(joined, vec![Some("ja"), Some("2"), Some("4")]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT lang, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang ORDER BY n DESC LIMIT 1",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("ja"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// RLS-7・RLS-8: `COUNT(*)`・`GROUP BY` のグループ集合が他テナントの Private
/// 行の存在・件数を漏らさない。さらに、既存接続へ tenant-a の Private 行
/// （`lang="xx"`）を大量追加した前後で結果が不変であることを確認する
/// （`crates/engine/tests/sql_aggregate.rs`
/// `sql13_rls_count_is_invariant_to_other_tenants_private_rows` と同じ手順を
/// wire 越しに再現する）。
#[test]
fn rls_count_and_groups_never_reveal_other_tenants_private_rows() {
    let (core, _guard) = new_core_aggregate_docs();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        let mut stream = authenticate_to_ready_for_query(addr, user, pw);

        send_simple_query(&mut stream, "SELECT COUNT(*) AS n FROM docs");
        let _columns = read_row_description(&mut stream);
        let row = read_data_row(&mut stream);
        assert_eq!(row[0].as_deref(), Some("3"), "COUNT(*) for user {user}");
        assert_eq!(read_command_complete(&mut stream), "SELECT 1");
        read_ready_for_query(&mut stream);

        send_simple_query(
            &mut stream,
            "SELECT COUNT(*) AS n FROM docs WHERE visible()",
        );
        let _columns = read_row_description(&mut stream);
        let row = read_data_row(&mut stream);
        assert_eq!(
            row[0].as_deref(),
            Some("3"),
            "COUNT(*) WHERE visible() for user {user}"
        );
        assert_eq!(read_command_complete(&mut stream), "SELECT 1");
        read_ready_for_query(&mut stream);

        send_simple_query(
            &mut stream,
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang",
        );
        let _columns = read_row_description(&mut stream);
        let mut rows = Vec::new();
        for _ in 0..2 {
            let row = read_data_row(&mut stream);
            rows.push(row[0].clone().expect("lang must not be NULL"));
        }
        assert_eq!(
            rows,
            vec!["en".to_string(), "ja".to_string()],
            "GROUP BY lang must not surface the Private-only \"xx\" group for user {user}"
        );
        assert_eq!(read_command_complete(&mut stream), "SELECT 2");
        read_ready_for_query(&mut stream);
    }

    // tenant-a の Private 行（lang="xx"）を 50 件追加してから再クエリし、
    // COUNT(*)・GROUP BY の結果が不変であることを確認する（他テナントの
    // 存在・件数が集計値から推測できないことの検証）。
    let writer_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    );
    let metadata = engine::row_codec::encode_scalar_columns(
        &schema,
        &[Value::Null, Value::Text("xx".to_string())],
    )
    .expect("encode scalar columns");
    for i in 0..50u64 {
        let id = 20_000 + i;
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("wire-agg-rls-{i}"))
                .expect("valid operation_id");
        core.insert_row(
            &writer_ctx,
            "docs",
            id,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[1.0f32, 0.0f32],
                metadata: &metadata,
            },
            Some(&op_id),
        )
        .expect("insert additional private row");
    }

    let mut stream_bob = authenticate_to_ready_for_query(addr, "bob", "pw-bob");
    send_simple_query(&mut stream_bob, "SELECT COUNT(*) AS n FROM docs");
    let _columns = read_row_description(&mut stream_bob);
    let row = read_data_row(&mut stream_bob);
    assert_eq!(
        row[0].as_deref(),
        Some("3"),
        "COUNT(*) must stay invariant after adding 50 private rows to another tenant"
    );
    assert_eq!(read_command_complete(&mut stream_bob), "SELECT 1");
    read_ready_for_query(&mut stream_bob);

    send_simple_query(
        &mut stream_bob,
        "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang",
    );
    let _columns = read_row_description(&mut stream_bob);
    let mut rows = Vec::new();
    for _ in 0..2 {
        let row = read_data_row(&mut stream_bob);
        rows.push(row[0].clone().expect("lang must not be NULL"));
    }
    assert_eq!(
        rows,
        vec!["en".to_string(), "ja".to_string()],
        "GROUP BY must stay invariant after adding 50 private rows to another tenant"
    );
    assert_eq!(read_command_complete(&mut stream_bob), "SELECT 2");
    read_ready_for_query(&mut stream_bob);
}

/// 拒否経路の fail-closed 確認: 各エラーが期待 SQLSTATE で返り、接続は破棄
/// されず（`ReadyForQuery` を都度読める）、セッションが汚染されない
/// （最後に成功クエリが通ることで確認する）。
#[test]
fn sql_aggregate_rejections_are_fail_closed_and_connection_survives() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    for (sql, expected_sqlstate) in [
        ("SELECT SUM(embedding) FROM docs", "22000"),
        ("SELECT COUNT(*) FROM docs GROUP BY embedding", "22000"),
        ("SELECT COUNT(*) FROM docs HAVING count > 1", "42601"),
        ("SELECT COUNT(*), lang FROM docs", "42601"),
        (
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING lang = 'ja'",
            "42601",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, expected_sqlstate);
        read_ready_for_query(&mut stream);
    }

    // セッションが汚染されず、直後の集計クエリが正常に成功する。
    send_simple_query(&mut stream, "SELECT COUNT(*) AS n FROM docs");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("3"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-13: `SUM(id)`/`AVG(id)` の `u64` 桁あふれは `22003` で拒否し、
/// `MAX(id)`（オーバーフローしない）は通常どおり成功する
/// （`crates/engine/tests/sql_aggregate.rs::sql13_sum_id_overflow_is_rejected`
/// と同じオラクルを wire 越しに再現）。
#[test]
fn sql13_numeric_overflow_is_rejected_with_22003_over_wire() {
    let path = temp_db::unique_db_path("wire-aggregate-overflow");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for (id, lang) in [(u64::MAX, "ja"), (1, "ja")] {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32, 0.0f32]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("overflow-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let (mut stream, _users_path) = spawn_with_alice(core);
    let _guard = guard;

    send_simple_query(&mut stream, "SELECT SUM(id) FROM docs");
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT AVG(id) FROM docs");
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT MAX(id) AS m FROM docs");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some(u64::MAX.to_string().as_str()));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}
