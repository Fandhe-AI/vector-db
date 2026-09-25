//! カーソル（`DECLARE`/`FETCH`/`CLOSE`。WIRE-15・TASK-218）の wire 経由結合
//! テスト。ポインタ: `docs/spec/05-tasks.md` TASK-218・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-15・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `wire942_extended_transaction.rs`・`wire16_multi_statement.rs` と同じ流儀
//! （生バイトの wire クライアント＋in-process サーバー）で、簡易クエリ
//! プロトコル経由の `DECLARE`／`FETCH`／`CLOSE` の受理・エラー分類・RLS 分離を
//! 固定する。

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

const TABLE: &str = "docs";

/// 次に届くメッセージの種別バイトを消費せずに覗き見る（TCP の `peek` を使う。
/// 後続の通常の読み取り〔`read_data_row`／`read_command_complete` 等〕が同じ
/// バイト列を改めて読み取れる）。`FETCH` の 1 ページに含まれる `DataRow` の
/// 件数は事前に分からない（末尾ページは要求件数未満・0 件になりうる）ため、
/// `DataRow`（`'D'`）と `CommandComplete`（`'C'`）のどちらが届いたかを見て
/// 分岐するために使う。
fn peek_message_type(stream: &mut std::net::TcpStream) -> u8 {
    let mut buf = [0u8; 1];
    loop {
        match stream.peek(&mut buf) {
            Ok(0) => panic!("connection closed while peeking message type"),
            Ok(_) => return buf[0],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("peek failed: {e}"),
        }
    }
}

/// `docs` テーブル（`embedding VECTOR(3)` + `lang TEXT`）を持つ `EngineCore` を
/// 新設し、決定的な小規模コーパスを `tenant-a` に投入する。
fn new_core_with_rows() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire15-cursor");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for id in 1..=5u64 {
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![1.0, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &op_id,
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `BEGIN` → `DECLARE`（広域取得 `SELECT`）→ 複数回 `FETCH` → `CLOSE` →
/// `COMMIT` の一連が、pg 互換のタグ・`RowDescription`／`DataRow` で応答する
/// ことを固定する。
#[test]
fn wire15_declare_fetch_close_over_simple_query() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 100",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);

    let mut fetched_ids: Vec<String> = Vec::new();
    loop {
        send_simple_query(&mut stream, "FETCH 2 FROM c");
        let columns = read_row_description(&mut stream);
        assert_eq!(columns, vec!["id"]);
        let mut page_rows = Vec::new();
        // このページの行数はタグの数値部分から分かるが、先に `CommandComplete`
        // を読むと `DataRow` と混同するため、`DataRow` を上限 2 件まで読み、
        // 3 件目の読み取りを試みる代わりにタグを直接読む（`read_command_complete`
        // は内部で `C` メッセージのみを期待するため、`DataRow` が尽きた時点で
        // 呼び出す）。
        for _ in 0..2 {
            match peek_message_type(&mut stream) {
                b'D' => page_rows.push(read_data_row(&mut stream)),
                b'C' => break,
                other => panic!("unexpected message type: {other}"),
            }
        }
        let tag = read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
        assert_eq!(tag, format!("FETCH {}", page_rows.len()));
        if page_rows.is_empty() {
            break;
        }
        for row in page_rows {
            fetched_ids.push(row[0].clone().expect("id is not null"));
        }
    }
    fetched_ids.sort();
    assert_eq!(fetched_ids, vec!["1", "2", "3", "4", "5"]);

    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);
}

/// トランザクション外の `DECLARE` は `25P01`、`FETCH`／`CLOSE` は `34000`。
/// エラー後も接続は維持される（簡易クエリのエラー契約）。
#[test]
fn wire15_cursor_statements_outside_transaction_are_rejected() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "FETCH 1 FROM c");
    expect_error_response_with_sqlstate(&mut stream, "34000");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CLOSE c");
    expect_error_response_with_sqlstate(&mut stream, "34000");
    read_ready_for_query(&mut stream);

    // 接続が維持されていることを確認する（簡易クエリの通常応答が返る）。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 1");
    let _ = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// ベクトル順位付けの検索 `SELECT`（`ORDER BY <=>`）を `DECLARE` の内側として
/// 指定すると `42601`。`Active` なトランザクションは `Failed` へ遷移する。
#[test]
fn wire15_declare_rejects_vector_ranking_select_and_fails_transaction() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    // `Failed` へ遷移しているため `ReadyForQuery` の状態バイトは `'E'`。
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "FETCH 1 FROM c");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

/// RLS: 他テナントのカーソル名は不在と同一の応答（`34000`）になり、存在情報を
/// 漏らさない。
#[test]
fn wire15_cursor_names_do_not_leak_across_tenants() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));

    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut alice, "BEGIN");
    assert_eq!(read_command_complete(&mut alice), "BEGIN");
    read_ready_for_query(&mut alice);
    send_simple_query(
        &mut alice,
        "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 10",
    );
    assert_eq!(read_command_complete(&mut alice), "DECLARE CURSOR");
    read_ready_for_query(&mut alice);

    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    send_simple_query(&mut bob, "BEGIN");
    assert_eq!(read_command_complete(&mut bob), "BEGIN");
    read_ready_for_query(&mut bob);
    send_simple_query(&mut bob, "FETCH 1 FROM c");
    expect_error_response_with_sqlstate(&mut bob, "34000");
    read_ready_for_query(&mut bob);
    send_simple_query(&mut bob, "ROLLBACK");
    assert_eq!(read_command_complete(&mut bob), "ROLLBACK");
    read_ready_for_query(&mut bob);
}
