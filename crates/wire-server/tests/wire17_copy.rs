//! `COPY ... FROM STDIN`／`COPY (...) TO STDOUT`（Issue #939・WIRE-17・
//! TASK-220）の簡易クエリプロトコル経由（生バイトクライアント）検証（層 A）。
//!
//! engine 側の意味論（レコード分割・エスケープ解決・束縛・INDEX-4 逐次判定・
//! commit の原子性）は `crates/engine/tests/copy_from.rs` が確定オラクルとして
//! 検証済みのため、本ファイルは同じ規則が **wire フレーミング**
//! （CopyInResponse／CopyData／CopyDone／CopyFail／CopyOutResponse）越しに
//! 観測できることの確認に集中する（`wire_insert_operation_id.rs` と同じ流儀）。

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

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire17-copy-docs");
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn spawn_with_bob(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "bob", "pw-bob")
}

/// `CopyInResponse`（'G'）を読み、公告された列数を返す。
fn read_copy_in_response(stream: &mut std::net::TcpStream) -> i16 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'G', "expected CopyInResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    i16::from_be_bytes([body[1], body[2]])
}

/// `CopyOutResponse`（'H'）を読み、公告された列数を返す。
fn read_copy_out_response(stream: &mut std::net::TcpStream) -> i16 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'H', "expected CopyOutResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    i16::from_be_bytes([body[1], body[2]])
}

/// `CopyData`（'d'）を 1 個読み、本文（改行込みの生バイト列）を返す。
fn read_copy_data(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'd', "expected CopyData");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    body
}

/// `CopyDone`（'c'）を読み、length=4（body 空）であることを確認する。
fn read_copy_done(stream: &mut std::net::TcpStream) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'c', "expected CopyDone");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    assert_eq!(i32::from_be_bytes(len_buf), 4, "CopyDone has no body");
}

fn send_copy_data(stream: &mut std::net::TcpStream, chunk: &[u8]) {
    send_length_prefixed_message(stream, b'd', chunk);
}

fn send_copy_done(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'c', b"");
}

fn send_copy_fail(stream: &mut std::net::TcpStream, reason: &str) {
    send_length_prefixed_message(stream, b'f', reason.as_bytes());
}

// ---------------------------------------------------------------------
// COPY FROM STDIN
// ---------------------------------------------------------------------

#[test]
fn wire17_copy_from_stdin_text_format_round_trips_through_select() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-1'",
    );
    let ncols = read_copy_in_response(&mut stream);
    assert_eq!(ncols, 3);

    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\tja\n");
    send_copy_done(&mut stream);

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 2");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 100");
    let _cols = read_row_description(&mut stream);
    let mut ids: Vec<u64> = Vec::new();
    for _ in 0..2 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].as_ref().unwrap().parse().unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_csv_format_is_accepted() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'wire-copy-2'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1,\"[1.0,0.0]\",ja\n");
    send_copy_done(&mut stream);

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_missing_operation_id_is_rejected_before_copy_in_response() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, "COPY docs (id, embedding, lang) FROM STDIN");
    expect_error_response_with_sqlstate(&mut stream, "23502");
    read_ready_for_query(&mut stream);

    // 通常のクエリが引き続き正常に動くこと（CopyInResponse を送っていない
    // ため接続状態が壊れていないことの確認）。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_invalid_row_is_rejected_with_zero_side_effects() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-3'",
    );
    let _ = read_copy_in_response(&mut stream);
    // 2 行目の embedding 次元が宣言（2）と不一致。
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n2\t[1.0,0.0,0.0]\tja\n");
    send_copy_done(&mut stream);

    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let cols = read_row_description(&mut stream);
    let _ = cols;
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0", "no row must have been committed");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_duplicate_operation_id_matches_multi_row_insert_ledger() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'wire-copy-shared'",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-shared'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);

    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_copy_fail_writes_zero_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-4'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_fail(&mut stream, "client aborted the copy locally");

    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0", "CopyFail must not write any row");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_rejects_unexpected_message_type_and_closes() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-5'",
    );
    let _ = read_copy_in_response(&mut stream);
    // 拡張クエリプロトコルの Parse メッセージは COPY サブプロトコル中は想定外。
    send_length_prefixed_message(&mut stream, b'P', b"\0select 1\0\0\0");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

// ---------------------------------------------------------------------
// COPY (...) TO STDOUT
// ---------------------------------------------------------------------

#[test]
fn wire17_copy_to_stdout_returns_only_own_tenant_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let mut alice = spawn_with_alice(Arc::clone(&core));

    send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'to-op-1'",
    );
    let _tag = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    let mut bob = spawn_with_bob(Arc::clone(&core));
    send_simple_query(
        &mut bob,
        "INSERT INTO docs (id, embedding, lang) VALUES (2, '[0.0,1.0]', 'ja') USING OPERATION_ID 'to-op-2'",
    );
    let _tag = read_command_complete(&mut bob);
    read_ready_for_query(&mut bob);

    send_simple_query(&mut alice, "COPY (SELECT id FROM docs LIMIT 100) TO STDOUT");
    let ncols = read_copy_out_response(&mut alice);
    assert_eq!(ncols, 1);
    let row = read_copy_data(&mut alice);
    assert_eq!(row, b"1\n");
    read_copy_done(&mut alice);
    let tag = read_command_complete(&mut alice);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut alice);
}

#[test]
fn wire17_copy_to_stdout_rejects_ranked_select() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY (SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10) TO STDOUT",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// COPY TO の出力を同じテーブルへ COPY FROM で再投入した際に値が往復する
/// こと（`docs/design/wire-copy-protocol.md`「対称性」節の固定）。
#[test]
fn wire17_copy_to_stdout_output_round_trips_through_copy_from_stdin() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'rt-op-1'",
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY (SELECT id, lang FROM docs LIMIT 10) TO STDOUT",
    );
    let _ = read_copy_out_response(&mut stream);
    let row = read_copy_data(&mut stream);
    read_copy_done(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // `id\tlang\n` 形式の出力を、`embedding` を補って別 id で再投入する。
    let text = String::from_utf8(row).expect("utf8 copy output");
    let mut parts = text.trim_end_matches('\n').splitn(2, '\t');
    let _id = parts.next().expect("id field");
    let lang = parts.next().expect("lang field");

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'rt-op-2'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, format!("2\t[0.0,1.0]\t{lang}\n").as_bytes());
    send_copy_done(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);
}
