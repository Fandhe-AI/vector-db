//! 広域取得（ソートなしのフィルタ取得。`SELECT ... [WHERE ...] LIMIT n`。
//! Issue #454）が PostgreSQL wire プロトコル v3 の簡易クエリ経路（生バイト
//! クライアント）で契約どおりの応答（`RowDescription`／`DataRow`／
//! `CommandComplete`／`ErrorResponse` の SQLSTATE）として観測できることを検証する
//! 結合テスト（層 A。`docs/design/three-client-e2e-harness.md` 参照）。
//!
//! 実行契約そのもの（早期終了・投影種別・`LIMIT` 範囲・取得モード非依存）は
//! `crates/engine/tests/sql_scan.rs`（in-process）が既に確定オラクルとして検証
//! 済みのため、本ファイルは同じ規則を **wire フレーミング** 越しに再確認する
//! ことに徹する（`wire_aggregate.rs`・`wire_search_mode.rs` と同方針）。
//! 無改造の実クライアント（psql／psycopg／pg）を使う 3 クライアント統合検証は
//! `tests/extended_syntax_e2e.rs`（`#[ignore]`）が層 B として担う。

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
use engine::storage::{Storage, Visibility};

use common::*;

/// `docs(embedding VECTOR(2), lang TEXT)` を持つ `EngineCore` を新設する。
/// 可視行（wire 認証経路では Public のみ）: id=1 (tenant-a, "ja") /
/// id=2 (tenant-b, "en") / id=3 (tenant-c, "ja")。`lang="xx"` の Private 行
/// （id=11, tenant-a）は wire 越しには不可視で、RLS 非漏えいの対照に使う。
fn new_core_scan_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-scan-docs");
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
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        11,
        Visibility::Private,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("xx".to_string())],
        &engine::recovery::required_op_id::OperationId::parse("test-op-11")
            .expect("valid operation_id"),
    )
    .expect("insert private row");

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

/// Issue #454: `ORDER BY`／`USING PLAN` を伴わない `SELECT ... LIMIT n` が
/// `RowDescription`／`DataRow`（複数行）／`CommandComplete` として返る。RLS
/// 非漏えい（`lang="xx"` の Private 行が現れない）もあわせて確認する。
#[test]
fn bare_limit_scan_returns_rows_over_wire_without_leaking_private_rows() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id, lang FROM docs LIMIT 10");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "lang"]);

    let mut seen_ids: Vec<String> = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        seen_ids.push(row[0].clone().expect("id must not be NULL"));
        let lang = row[1].as_deref().expect("lang must not be NULL");
        assert_ne!(
            lang, "xx",
            "Private row's lang value must not leak over wire"
        );
    }
    let mut sorted = seen_ids.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["1", "2", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `WHERE` による絞り込みが early-termination の対象行にも適用される。
#[test]
fn scan_with_where_filters_rows_over_wire() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs WHERE lang = 'ja' LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..2 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].clone().expect("id must not be NULL"));
    }
    ids.sort();
    assert_eq!(ids, vec!["1", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `LIMIT` 未満の行数しか可視集合に無い場合、その件数だけを返す
/// （早期終了は行わない=全件読み切って `LIMIT` に満たない、という契約側の確認）。
#[test]
fn scan_limit_larger_than_visible_set_returns_all_visible_rows() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10000");
    let _columns = read_row_description(&mut stream);
    for _ in 0..3 {
        let _ = read_data_row(&mut stream);
    }
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `LIMIT` 直後に `USING MODE` を付けた形は許可リスト外
/// （`42601`）。取得モードは広域取得に適用対象を持たない。
#[test]
fn scan_rejects_using_mode_suffix_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 10 USING MODE 'precision'",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `ORDER BY`／`USING PLAN`／`LIMIT` のいずれも無い形は許可リスト外
/// （`42601`）のまま（広域取得の追加が既存の必須句判定を緩めていないことの
/// 回帰確認）。
#[test]
fn plain_select_without_order_by_or_limit_still_rejected_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `EXPLAIN` は広域取得の前置として許可しない（`USING PLAN` を伴う
/// 検索 SELECT のみを受理する既存契約を維持）。
#[test]
fn explain_rejects_bare_limit_scan_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "EXPLAIN SELECT id FROM docs LIMIT 10");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `SET search_mode` の直後でも広域取得の結果は変わらない
/// （取得モードからの独立性の wire 越し確認）。
#[test]
fn scan_result_unaffected_by_search_mode_over_wire() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SET search_mode = 'precision'");
    assert_eq!(read_command_complete(&mut stream), "SET");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10000");
    let _columns = read_row_description(&mut stream);
    for _ in 0..3 {
        let _ = read_data_row(&mut stream);
    }
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}
