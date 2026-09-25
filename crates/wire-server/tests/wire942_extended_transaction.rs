//! 拡張クエリプロトコル経由の明示トランザクション `BEGIN`/`COMMIT`/`ROLLBACK`
//! （SQL-31・TASK-221）の結合テスト（Issue #942 codex-review 指摘対応）。
//!
//! 以前は `extended_query::handle_execute`/`execute_portal` が
//! `engine::execute_parsed_in_session`（トランザクション文脈を持たない
//! autocommit 専用の入口）を呼んでいたため、拡張クエリプロトコル経由で
//! `BEGIN` を Parse/Bind/Execute しても `Idle` から進めず常に `0A000`
//! （`transaction_feature_not_supported`）で拒否されていた——コミット
//! メッセージ・`docs/design/explicit-transaction.md`・
//! `crates/engine/src/sql/transaction.rs` のモジュールドキュメントはいずれも
//! 「簡易クエリ・拡張クエリの両経路で受理する」と述べていたが、実装は
//! 簡易クエリ（`crate::simple_query`）経由のみ結線済みで、拡張クエリ経由は
//! 未結線のまま不一致だった。本ファイルは `handshake::post_auth_loop` が
//! 保持する接続単位の `SessionTransaction` を拡張クエリ経路でも共有する
//! ように結線した後の受理を固定する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_documents_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire942-extended-transaction");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

fn parse_body(name: &str, query: &str, num_param_types: i16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(query.as_bytes());
    body.push(0);
    body.extend_from_slice(&num_param_types.to_be_bytes());
    body
}

fn bind_body(portal: &str, statement: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&0i16.to_be_bytes()); // param count
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    body
}

fn execute_body(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

fn read_message(stream: &mut std::net::TcpStream) -> (u8, Vec<u8>) {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte).expect("read type byte");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let body_len = len.checked_sub(4).expect("length must be >= 4");
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body).expect("read body");
    (type_byte[0], body)
}

fn send_sync(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn assert_ready_for_query(stream: &mut std::net::TcpStream) {
    let (kind, _body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
}

fn parse_and_bind(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
}

/// Execute し `CommandComplete` タグを読み取る（Describe を伴わない
/// トランザクション制御文・`INSERT` 用。`RowDescription` を持たない）。
fn execute_and_read_command_complete(stream: &mut std::net::TcpStream, portal: &str) -> String {
    send_length_prefixed_message(stream, b'E', &execute_body(portal, 0));
    let (kind, tag) = read_message(stream);
    assert_eq!(kind, b'C', "expected CommandComplete");
    String::from_utf8_lossy(&tag[..tag.len() - 1]).into_owned()
}

/// Issue #942 codex-review 指摘対応の中心テスト: Parse/Bind/Execute で
/// `BEGIN` すると（以前のように `0A000` へ落ちず）`CommandComplete BEGIN` が
/// 返ること、続けて同一トランザクション内で `INSERT` を Execute し
/// `COMMIT` すると commit されて可視になること、`ROLLBACK` すると破棄される
/// ことを固定する。
#[test]
fn begin_insert_commit_over_extended_protocol_is_accepted_and_visible() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // BEGIN。以前は `execute_parsed_in_session`（autocommit 専用）が
    // `ParsedSql::Transaction` を無条件に `0A000` で拒否していたため、拡張
    // クエリプロトコル経由では `CommandComplete` に到達できなかった。
    parse_and_bind(&mut stream, "begin1", "pb1", "BEGIN");
    let tag = execute_and_read_command_complete(&mut stream, "pb1");
    assert_eq!(
        tag, "BEGIN",
        "BEGIN must succeed over the extended protocol"
    );
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 同一トランザクション内で INSERT。
    let insert_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-942-1'";
    parse_and_bind(&mut stream, "ins1", "pi1", insert_sql);
    let tag = execute_and_read_command_complete(&mut stream, "pi1");
    assert_eq!(tag, "INSERT 0 1");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // COMMIT。
    parse_and_bind(&mut stream, "commit1", "pc1", "COMMIT");
    let tag = execute_and_read_command_complete(&mut stream, "pc1");
    assert_eq!(tag, "COMMIT");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // commit 済みの行が簡易クエリ経由で読み戻せる（read-your-writes）。
    send_simple_query(&mut stream, "SELECT id FROM documents WHERE id = 1 LIMIT 1");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string())]);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// `BEGIN` → `INSERT` → `ROLLBACK` を拡張クエリプロトコル経由で行うと、行が
/// 一切 commit されない（`COMMIT` していない書き込みは破棄される契約。
/// `sql::transaction` モジュールドキュメント参照）ことを固定する。
#[test]
fn begin_insert_rollback_over_extended_protocol_discards_the_row() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    parse_and_bind(&mut stream, "begin1", "pb1", "BEGIN");
    let tag = execute_and_read_command_complete(&mut stream, "pb1");
    assert_eq!(tag, "BEGIN");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    let insert_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'discarded') USING OPERATION_ID 'op-942-2'";
    parse_and_bind(&mut stream, "ins1", "pi1", insert_sql);
    let tag = execute_and_read_command_complete(&mut stream, "pi1");
    assert_eq!(tag, "INSERT 0 1");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    parse_and_bind(&mut stream, "rollback1", "pr1", "ROLLBACK");
    let tag = execute_and_read_command_complete(&mut stream, "pr1");
    assert_eq!(tag, "ROLLBACK");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM documents WHERE id = 2 LIMIT 1");
    let _columns = read_row_description(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // ROLLBACK 済みの行を engine API から直接確認する（wire 経由の SELECT の
    // 結果件数 0 だけでは、書き込み自体が起きていないのか破棄されたのかを
    // 区別できないため）。
    let mut session = engine::sql::mode::SessionState::default();
    let ctx = engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id");
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM documents WHERE id = 2 LIMIT 1",
        )
        .expect("select via engine API");
    match outcome {
        engine::sql::SqlOutcome::Query(result) => assert_eq!(
            result.rows.len(),
            0,
            "id=2 は ROLLBACK により commit されていないはず"
        ),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// engine API から `id` の行が可視かどうかを数える（wire 経由の結果件数 0 だけ
/// では「書き込みが起きなかった」のか「破棄された」のかを区別できないため）。
fn visible_rows_with_id(core: &EngineCore, id: u64) -> usize {
    let mut session = engine::sql::mode::SessionState::default();
    let ctx = engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id");
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("SELECT id FROM documents WHERE id = {id} LIMIT 1"),
        )
        .expect("select via engine API");
    match outcome {
        engine::sql::SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'row') USING OPERATION_ID '{op}'"
    )
}

/// 簡易クエリで `BEGIN` → `INSERT` を送り、`Active` にする。
fn begin_and_insert_over_simple_query(stream: &mut std::net::TcpStream, id: u64, op: &str) {
    send_simple_query(stream, "BEGIN");
    assert_eq!(read_command_complete(stream), "BEGIN");
    read_ready_for_query(stream);
    send_simple_query(stream, &insert_sql(id, op));
    assert_eq!(read_command_complete(stream), "INSERT 0 1");
    read_ready_for_query(stream);
}

/// `COMMIT` が `25P02` で拒否され、`ROLLBACK` で `Idle` へ戻ることを確認する。
fn expect_commit_rejected_then_rollback(stream: &mut std::net::TcpStream) {
    send_simple_query(stream, "COMMIT");
    expect_error_response_with_sqlstate(stream, "25P02");
    read_ready_for_query(stream);
    send_simple_query(stream, "ROLLBACK");
    assert_eq!(read_command_complete(stream), "ROLLBACK");
    read_ready_for_query(stream);
}

/// 簡易クエリの構文エラーでもトランザクションが `Failed` へ遷移し、後続の
/// `COMMIT` が先行する `INSERT` を永続化しないこと（PR #1041 レビュー指摘）。
#[test]
fn simple_query_parse_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 30, "op-942-30");
    send_simple_query(&mut stream, "SELEC id FROM documents");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 30), 0);
}

/// 複数文メッセージの分割エラー（文数上限超過 `54000`）でもトランザクションが
/// `Failed` へ遷移すること（PR #1041 レビュー指摘の横断確認）。
#[test]
fn simple_query_split_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 31, "op-942-31");
    let too_many = "SELECT id FROM documents LIMIT 1;"
        .repeat(engine::sql::statement_splitter::MAX_STATEMENTS_PER_QUERY + 1);
    send_simple_query(&mut stream, &too_many);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 31), 0);
}

/// 拡張クエリプロトコルの Parse エラーでもトランザクションが `Failed` へ遷移し、
/// 後続の `COMMIT` が先行する `INSERT` を永続化しないこと（PR #1041 レビュー
/// 指摘の横断確認）。
#[test]
fn extended_parse_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 32, "op-942-32");
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("bad1", "SELEC id FROM documents", 0),
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 32), 0);
}

/// `Failed` 中の COPY が autocommit として実行されず `25P02` で拒否されること
/// （PR #1041 レビュー指摘）。CopyInResponse へ進まずに ErrorResponse が返る。
#[test]
fn copy_while_transaction_failed_is_rejected_with_in_failed_sql_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 33, "op-942-33");
    // 入れ子の BEGIN で Failed へ遷移させる。
    send_simple_query(&mut stream, "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY documents (id, embedding, body) FROM STDIN USING OPERATION_ID 'op-942-copy'",
    );
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 33), 0);
}
