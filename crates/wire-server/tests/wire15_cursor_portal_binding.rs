//! カーソル `FETCH`（WIRE-15・TASK-218）を拡張クエリプロトコルで実行した
//! portal が中断保持（`PortalState::Suspended`）へ回った場合に、その後の
//! `CLOSE`／`COMMIT`／`ROLLBACK`（いずれも同一 Sync サイクル内。named portal
//! は Sync でのみ破棄されるため、Sync を跨がなければ生き残る）を挟んでから
//! 再 Execute しても、保持済みの行を送出しないことを固定する
//! （PR #1049 レビュー指摘・codex P1「終了したカーソルの FETCH portal から
//! 行を送出できる」の回帰防止）。
//!
//! `wire942_extended_transaction.rs`（トランザクション期限切れ後の portal
//! 拒否）・`wire11_bind_execute_sync.rs`（`max_rows` 分割送出）と同じ流儀
//! （生バイトの wire クライアント＋in-process サーバー）を使う。

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
    let path = temp_db::unique_db_path("wire15-cursor-portal-binding");
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

fn owner_ctx() -> engine::policy::PolicyContext {
    engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id")
}

fn seed_rows(core: &EngineCore, n: u64) {
    for i in 1..=n {
        let sql = format!(
            "INSERT INTO documents (id, embedding, body) VALUES ({i}, '[0.1,0.2,0.3]', 'row-{i}') USING OPERATION_ID 'seed-{i}'"
        );
        let mut session = engine::sql::mode::SessionState::default();
        core.execute_sql_in_session(&owner_ctx(), &mut session, &sql)
            .expect("seed insert succeeds");
    }
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

fn parse_and_bind(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
}

/// `DECLARE`（内側 SELECT が `rows` 件を返す）→ 拡張クエリで `FETCH <rows> FROM
/// c` を named portal `pf` へ Bind → `max_rows=1` で Execute し、1 行 +
/// `PortalSuspended` を確認する（`rows >= 2` が前提）。
fn declare_and_suspend_fetch_portal(stream: &mut std::net::TcpStream, rows: u64) {
    send_simple_query(stream, "BEGIN");
    assert_eq!(read_command_complete(stream), "BEGIN");
    read_ready_for_query(stream);

    send_simple_query(
        stream,
        &format!("DECLARE c CURSOR FOR SELECT id FROM documents LIMIT {rows}"),
    );
    assert_eq!(read_command_complete(stream), "DECLARE CURSOR");
    read_ready_for_query(stream);

    parse_and_bind(stream, "sf", "pf", &format!("FETCH {rows} FROM c"));
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 1));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'D', "expected DataRow");
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b's', "expected PortalSuspended");
}

/// 中断保持中の `pf` を再 Execute すると `34000`（invalid cursor name）が
/// 返り、`DataRow` を一切送出しないこと、`Sync` で同期が回復することを
/// 検証する。
fn assert_resume_rejected_without_leaking_rows(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind, b'E',
        "a FETCH portal invalidated by CLOSE/COMMIT/ROLLBACK must not resume with buffered rows"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("34000"),
        "expected sqlstate 34000 (invalid cursor name), got {text:?}"
    );
    send_sync(stream);
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync recovery");
}

/// `CLOSE c` を挟んでから中断保持中の `pf`（`FETCH ... FROM c`）を再 Execute
/// しても、`CLOSE` 後の行は一切送出されない（PR #1049 レビュー指摘の再現
/// 条件そのもの）。
#[test]
fn suspended_fetch_portal_is_rejected_after_cursor_close() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    // 同一 Sync サイクル内で `CLOSE c`（simple query。named portal は
    // simple query では破棄されない）。
    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

/// `COMMIT` を挟んでから中断保持中の `pf` を再 Execute しても、カーソルが
/// 消えたトランザクション終了後の行は送出されない。
#[test]
fn suspended_fetch_portal_is_rejected_after_commit() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);
}

/// `ROLLBACK` を挟んでから中断保持中の `pf` を再 Execute しても、カーソルが
/// 消えたトランザクション終了後の行は送出されない。
#[test]
fn suspended_fetch_portal_is_rejected_after_rollback() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);
}

/// `CLOSE c` の後に同名で再 `DECLARE` しても、旧インスタンスに束縛された
/// 中断保持中の `pf` は再開できない（名前の一致だけでなくカーソル個体
/// 識別子まで検証していることの確認。`docs/design/sql-cursor.md`
/// 「wire-server 拡張クエリの portal 束縛」節参照）。
#[test]
fn suspended_fetch_portal_is_rejected_after_cursor_redeclare_with_same_name() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 5",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

/// 対照: `CLOSE`／`COMMIT`／`ROLLBACK` を挟まなければ、中断保持中の `pf` は
/// 従来どおり残り行を正しく送出できる（上記の拒否が過剰でないことの確認）。
#[test]
fn suspended_fetch_portal_still_resumes_without_intervening_close_or_commit() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    let mut remaining = 0usize;
    loop {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("pf", 1));
        let (kind, _) = read_message(&mut stream);
        if kind == b'C' {
            break;
        }
        assert_eq!(kind, b'D', "expected DataRow");
        remaining += 1;
        let (kind, _) = read_message(&mut stream);
        if kind == b'C' {
            break;
        }
        assert_eq!(kind, b's', "expected PortalSuspended");
    }
    assert_eq!(remaining, 4, "4 rows remain after the first suspended row");

    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

// --- 完了済み（Done）FETCH portal の再 Execute（PR #1049 レビュー指摘 Cursor
// Bugbot Low の回帰防止） ------------------------------------------------------
//
// 束縛検証は保持行を送出し得る `Suspended` のみが対象。全行送出済みの `Done`
// portal は `CLOSE`／`COMMIT`／`ROLLBACK` を挟んでも、`FETCH` 以外の portal と
// 同じく保持済みの完了タグだけを再送する（行は一切送出しない）。

/// `DECLARE`（内側 SELECT が `rows` 件を返す）→ 拡張クエリで `FETCH <rows> FROM
/// c` を named portal `pf` へ Bind → `max_rows=0` で Execute して全行＋
/// `CommandComplete` を受け取り、portal を `Done` にする。完了タグを返す。
fn declare_and_complete_fetch_portal(stream: &mut std::net::TcpStream, rows: u64) -> Vec<u8> {
    send_simple_query(stream, "BEGIN");
    assert_eq!(read_command_complete(stream), "BEGIN");
    read_ready_for_query(stream);

    send_simple_query(
        stream,
        &format!("DECLARE c CURSOR FOR SELECT id FROM documents LIMIT {rows}"),
    );
    assert_eq!(read_command_complete(stream), "DECLARE CURSOR");
    read_ready_for_query(stream);

    parse_and_bind(stream, "sf", "pf", &format!("FETCH {rows} FROM c"));
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 0));
    for _ in 0..rows {
        let (kind, _) = read_message(stream);
        assert_eq!(kind, b'D', "expected DataRow");
    }
    let (kind, tag) = read_message(stream);
    assert_eq!(kind, b'C', "expected CommandComplete after all rows");
    tag
}

/// 完了済み `pf` を再 Execute すると、エラーも `DataRow` も返さず初回と同じ
/// 完了タグの `CommandComplete` のみが返り、`Sync` で `ReadyForQuery` に戻る。
fn assert_done_replay_resends_tag_only(stream: &mut std::net::TcpStream, expected_tag: &[u8]) {
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'C',
        "a completed FETCH portal must replay CommandComplete, got {:?}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        body, expected_tag,
        "replayed completion tag must be unchanged"
    );
    send_sync(stream);
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");
}

#[test]
fn done_fetch_portal_replays_completion_after_cursor_close() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let tag = declare_and_complete_fetch_portal(&mut stream, 3);

    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    assert_done_replay_resends_tag_only(&mut stream, &tag);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

#[test]
fn done_fetch_portal_replays_completion_after_commit() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let tag = declare_and_complete_fetch_portal(&mut stream, 3);

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);

    assert_done_replay_resends_tag_only(&mut stream, &tag);
}

#[test]
fn done_fetch_portal_replays_completion_after_rollback() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let tag = declare_and_complete_fetch_portal(&mut stream, 3);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);

    assert_done_replay_resends_tag_only(&mut stream, &tag);
}
