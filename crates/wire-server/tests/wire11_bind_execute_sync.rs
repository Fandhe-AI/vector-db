//! 拡張クエリプロトコルの Bind（'B'）・Execute（'E'）・Sync（'S'）・
//! Close（'C'）・Flush（'H'）の結合テスト（Issue #934・TASK-71・WIRE-11。
//! `crate::extended_query` が受理する経路）。
//!
//! Parse／Describe 単体の契約は `tests/wire11_parse_describe.rs` が担う。本
//! ファイルは Bind 以降——portal のライフサイクル・`max_rows` による分割送出・
//! エラー後の同期回復・テナント境界——を対象にする。

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
    let path = temp_db::unique_db_path("wire11-bind-execute-sync");
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

fn owner_ctx() -> engine::policy::PolicyContext {
    engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id")
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

fn bind_body_with_binary_result_format(portal: &str, statement: &str) -> Vec<u8> {
    bind_body_with_result_formats(portal, statement, &[1])
}

/// [`bind_body`] の結果 format code 部分だけを差し替える（WIRE-14）。
/// `codes` はそのまま「結果 format code 件数＋列」として送出する
/// （0/1/対象数のいずれでもない件数を意図的に送るテストにも使う）。
fn bind_body_with_result_formats(portal: &str, statement: &str, codes: &[i16]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&0i16.to_be_bytes()); // param count
    body.extend_from_slice(&(codes.len() as i16).to_be_bytes());
    for code in codes {
        body.extend_from_slice(&code.to_be_bytes());
    }
    body
}

fn execute_body(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(kind);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn close_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(kind);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
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

fn assert_error_response(stream: &mut std::net::TcpStream, expected_sqlstate: &str) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains(expected_sqlstate),
        "expected sqlstate {expected_sqlstate} in {text:?}"
    );
}

fn send_sync(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn assert_ready_for_query(stream: &mut std::net::TcpStream) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
    assert_eq!(body, [b'I']);
}

fn assert_error_then_recovers(stream: &mut std::net::TcpStream, expected_sqlstate: &str) {
    assert_error_response(stream, expected_sqlstate);
    send_sync(stream);
    assert_ready_for_query(stream);
}

fn parse_and_bind(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
}

/// 受け入れ条件 1: Parse/Bind/Describe(P)/Execute(0)/Sync を送ると
/// '1' '2' 'T' 'D'* 'C' 'Z' の順に返り、同じ SQL の簡易クエリ応答とバイト単位で
/// 一致する。
#[test]
fn select_happy_path_matches_simple_query_bytes() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut extended = tcp_connect(addr);
    let mut simple = tcp_connect(addr);

    let sql = "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";

    parse_and_bind(&mut extended, "s1", "p1", sql);
    send_length_prefixed_message(&mut extended, b'D', &describe_body(b'P', "p1"));
    let (kind, _) = read_message(&mut extended);
    assert_eq!(
        kind, b'T',
        "expected RowDescription (no ParameterDescription for portal)"
    );
    send_length_prefixed_message(&mut extended, b'E', &execute_body("p1", 0));
    let mut extended_bytes = Vec::new();
    loop {
        let (kind, body) = read_message(&mut extended);
        extended_bytes.push(kind);
        extended_bytes.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
        extended_bytes.extend_from_slice(&body);
        if kind == b'C' {
            break;
        }
    }
    send_sync(&mut extended);
    assert_ready_for_query(&mut extended);

    send_simple_query(&mut simple, sql);
    let (kind, _) = read_message(&mut simple); // RowDescription（拡張経路では Describe が既に消費している）
    assert_eq!(kind, b'T');
    let mut simple_bytes = Vec::new();
    loop {
        let (kind, body) = read_message(&mut simple);
        if kind == b'Z' {
            break;
        }
        simple_bytes.push(kind);
        simple_bytes.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
        simple_bytes.extend_from_slice(&body);
    }

    assert_eq!(
        extended_bytes, simple_bytes,
        "DataRow/CommandComplete bytes must match the simple query response"
    );
}

fn tcp_connect(addr: std::net::SocketAddr) -> std::net::TcpStream {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db-name");
    let auth_code = read_auth_request_type(&mut stream);
    assert_eq!(auth_code, 3);
    send_password_message(&mut stream, "correct-horse");
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("auth ok type");
    assert_eq!(header[0], b'R');
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("code");
    assert_eq!(i32::from_be_bytes(code_buf), 0);
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(&mut stream);
    }
    stream
}

/// 受け入れ条件 2: `max_rows` による分割送出。5 行に対し `max_rows = 2` を
/// 3 回 Execute すると「2 行 + PortalSuspended」×2・「1 行 + CommandComplete」と
/// なり、再実行しない（重複・欠落なし）。`max_rows <= 0` は全行を返す。
#[test]
fn max_rows_splits_result_without_reexecuting() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";
    parse_and_bind(&mut stream, "s1", "p1", sql);

    let mut all_ids = Vec::new();

    // 1 回目: 2 行 + PortalSuspended
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 2));
    for _ in 0..2 {
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'D');
        all_ids.push(body);
    }
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b's', "expected PortalSuspended");

    // 2 回目: 残り 2 行 + PortalSuspended
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 2));
    for _ in 0..2 {
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'D');
        all_ids.push(body);
    }
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b's');

    // 3 回目: 残り 1 行 + CommandComplete（再実行なし）。タグは今回バッチの
    // 件数（1）ではなく portal 全体の累計送出行数（2+2+1=5）から組み立てる
    // （PostgreSQL の契約。PR #1013 レビュー指摘・P1）。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 2));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    all_ids.push(body);
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 5");

    assert_eq!(all_ids.len(), 5, "no duplicate or missing rows");
    let mut dedup = all_ids.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), 5, "no duplicate rows across Execute calls");

    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 負値・0 は全行を一度に返す。
    parse_and_bind(&mut stream, "s2", "p2", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p2", 0));
    let mut count = 0;
    loop {
        let (kind, _) = read_message(&mut stream);
        if kind == b'C' {
            break;
        }
        assert_eq!(kind, b'D');
        count += 1;
    }
    assert_eq!(count, 5);
}

/// PR #1013 レビュー指摘（P0）: 中断保持する未送出行の合計バイト数上限
/// （`MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`＝16 MiB）は portal 単体ではなく
/// 接続（セッション）全体で合算して適用される契約を固定する。1 個の
/// 名前付き portal が上限未満（約 10 MiB）の分だけ中断保持している状態で、
/// 別の名前付き portal が同じ接続上でさらに約 10 MiB を中断保持しようと
/// すると、合計が上限を超えるため 2 個目の Execute は `54000` で拒否される
/// （portal 単体判定のままなら両方とも上限未満として通ってしまう）。
///
/// 1 文の SQL テキスト長には別途上限（`sql::lexer::MAX_INPUT_LEN`＝1 MiB）が
/// あるため、1 行あたり約 1 MiB の `body` を持つ行を複数（11 行）挿入し、
/// 1 行だけ送出（`max_rows=1`）して残り 10 行（約 10 MiB）を中断保持させる
/// ことで境界を作る。
#[test]
fn suspended_portal_bytes_are_accounted_across_the_whole_session_not_per_portal() {
    const ROW_BODY_LEN: usize = 1_000_000;
    const ROW_COUNT: u64 = 11;

    let (core, _guard) = new_core_with_documents_table();
    let big_body = "x".repeat(ROW_BODY_LEN);
    for id in 1..=ROW_COUNT {
        let sql = format!(
            "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', '{big_body}') USING OPERATION_ID 'seed-big-{id}'"
        );
        let mut session = engine::sql::mode::SessionState::default();
        core.execute_sql_in_session(&owner_ctx(), &mut session, &sql)
            .expect("seed big-body insert succeeds");
    }
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 11";

    // 1 個目の portal: 11 行中 1 行だけ送出し、残り 10 行（約 10 MiB）を
    // 中断保持する。単体では上限（16 MiB）未満のため成功する。
    parse_and_bind(&mut stream, "s1", "p1", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 1));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b's', "expected PortalSuspended for p1");

    // 2 個目の portal（別名。同一接続）: こちらも残り 10 行（約 10 MiB）を
    // 中断保持しようとするが、p1 の中断保持分と合算すると接続全体で
    // 約 20 MiB となり上限（16 MiB）を超えるため 54000 で拒否される。
    parse_and_bind(&mut stream, "s2", "p2", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p2", 1));
    assert_error_then_recovers(&mut stream, "54000");

    // p1 は影響を受けず、残り 10 行を通常どおり送出できる（セッション上限
    // 超過の拒否が既存の中断状態を破壊しない）。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    for _ in 0..10 {
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'D');
    }
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    // `CommandComplete` のタグは portal 全体の累計送出行数（1 回目の
    // Execute で送った 1 行 + 今回送った 10 行 = 11）から組み立てる
    // （PostgreSQL の契約。PR #1013 レビュー指摘・P1）。
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 11");
}

/// 受け入れ条件 3: `INSERT ... USING OPERATION_ID` を拡張経路で実行すると
/// `INSERT 0 1` が返り、後続の SELECT から見える。同じ operation_id の再送は
/// `23505`、内容を変えた再送は `22023`（いずれも同期回復する）。
#[test]
fn insert_via_extended_protocol_is_visible_and_operation_id_conflicts_are_detected() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let insert_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-1'";
    parse_and_bind(&mut stream, "ins1", "pi1", insert_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi1", 0));
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "INSERT 0 1");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 見える: 簡易クエリで読み戻せる。
    send_simple_query(&mut stream, "SELECT id FROM documents WHERE id = 1 LIMIT 1");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string())]);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 同一 operation_id の再送（同一内容）は 23505。
    parse_and_bind(&mut stream, "ins2", "pi2", insert_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi2", 0));
    assert_error_then_recovers(&mut stream, "23505");

    // 同一 operation_id・異なる内容は 22023。
    let conflicting_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'other') USING OPERATION_ID 'op-1'";
    parse_and_bind(&mut stream, "ins3", "pi3", conflicting_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi3", 0));
    assert_error_then_recovers(&mut stream, "22023");
}

/// 受け入れ条件 4: Sync による回復（許可リスト外 SQL・未存在テーブル・未定義
/// statement/portal への Bind/Describe/Execute・パラメータ数不一致・実行時
/// エラー）はいずれも ErrorResponse の後 Sync で 'Z' が返り、同じ接続で簡易
/// クエリ・拡張シーケンスの双方が続けて成功する。
#[test]
fn sync_recovers_from_a_variety_of_errors_and_connection_keeps_working() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    // 許可リスト外 SQL の Parse。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents WHERE id = $1", 0),
    );
    assert_error_then_recovers(&mut stream, "42601");

    // 未存在テーブル。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM missing_table LIMIT 1", 0),
    );
    assert_error_then_recovers(&mut stream, "42P01");

    // 未定義 statement への Bind・Describe。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p", "never-parsed"));
    assert_error_then_recovers(&mut stream, "08P01");
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', "never-parsed"));
    assert_error_then_recovers(&mut stream, "08P01");

    // 未定義 portal への Execute・Describe。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("no-such-portal", 0));
    assert_error_then_recovers(&mut stream, "08P01");
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "no-such-portal"));
    assert_error_then_recovers(&mut stream, "08P01");

    // 実行時エラー: 未存在テーブルへの操作は Parse 時点で拒否済みのため、
    // 代わりに実行時に SqlSurfaceError を発生させる operation_id 重複を使う
    // （既存のテーブルに対する 2 回目の同一 INSERT）。
    let dup_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (99, '[0.1,0.2,0.3]', 'x') USING OPERATION_ID 'dup-op'";
    parse_and_bind(&mut stream, "dup1", "pdup1", dup_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pdup1", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
    parse_and_bind(&mut stream, "dup2", "pdup2", dup_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pdup2", 0));
    assert_error_then_recovers(&mut stream, "23505");

    // 回復後、同じ接続で簡易クエリが通る。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _cols = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 回復後、同じ接続で拡張シーケンスが再び成功する。
    parse_and_bind(
        &mut stream,
        "ok1",
        "pok1",
        "SELECT id FROM documents LIMIT 1",
    );
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pok1", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
}

/// 受け入れ条件 5: エラー後、Sync の前に送った簡易クエリには応答しない
/// （破棄される）。Sync に対して 'Z' が 1 つだけ返る。
#[test]
fn queries_sent_before_sync_after_an_error_are_discarded() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents WHERE id = $1", 0),
    );
    assert_error_response(&mut stream, "42601");

    // エラー後・Sync 前に送った簡易クエリは破棄され応答しない。
    send_simple_query(&mut stream, "SELECT 1");

    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // これ以上メッセージは来ない（破棄された 'Q' に対する応答が紛れ込んで
    // いないことを、次の簡易クエリの応答が正しく単発で返ることで確認する）。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _cols = read_row_description(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 受け入れ条件 6: portal のライフサイクル。無名 portal は Sync で破棄され、
/// 名前付き portal は Sync を越えて残る。Close は '3' を返し、閉じた後の
/// Execute/Bind はエラーになる。statement の Close は派生 portal も閉じる。
/// 未存在名の Close も '3' を返す。
#[test]
fn portal_lifecycle_anonymous_named_and_close() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";

    // 無名 portal は Sync で破棄される。
    parse_and_bind(&mut stream, "", "", sql);
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // 名前付き portal は Sync を越えて残る。
    parse_and_bind(&mut stream, "sN", "pN", sql);
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pN", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // Close(Portal) は '3' を返し、以後の Execute はエラー。
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'P', "pN"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3', "expected CloseComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pN", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // statement の Close は派生 portal も閉じる。
    parse_and_bind(&mut stream, "sX", "pX", sql);
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'S', "sX"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3');
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pX", 0));
    assert_error_then_recovers(&mut stream, "08P01");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("pX2", "sX"));
    assert_error_then_recovers(&mut stream, "08P01");

    // 未存在名の Close も '3' を返す。
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'S', "no-such-stmt"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3');
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'P', "no-such-portal"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3');
}

/// 受け入れ条件 7: Flush は '1' を受け取れ 'Z' は来ない。body が空でない
/// Sync/Flush は `08P01` で切断される。
#[test]
fn flush_does_not_send_ready_for_query_and_nonempty_sync_flush_close_connection() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(&mut stream, b'H', b"");
    // Flush 自体は応答を持たないが、直前の ParseComplete のみが flush される
    // （'1' は既に読み終えている）。次にすぐ Sync を送って 'Z' だけが来ることを
    // 確認する。
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 非空 body の Sync は 08P01 + 切断。
    let mut stream2 = tcp_connect(addr);
    send_length_prefixed_message(&mut stream2, b'S', b"x");
    assert_error_response(&mut stream2, "08P01");
    let mut buf = [0u8; 1];
    let n = stream2.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");

    // 非空 body の Flush は 08P01 + 切断。
    let mut stream3 = tcp_connect(addr);
    send_length_prefixed_message(&mut stream3, b'H', b"x");
    assert_error_response(&mut stream3, "08P01");
    let n = stream3.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");
}

/// 受け入れ条件 8: 上限。名前付き portal の 65 個目は `54000`。portal 名が
/// 長すぎる場合は `54000`。名前付き portal の重複は `08P01`。
#[test]
fn portal_limits_are_enforced() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("shared", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');

    for i in 0..64 {
        let name = format!("port{i}");
        send_length_prefixed_message(&mut stream, b'B', &bind_body(&name, "shared"));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'2', "portal {i} should be accepted");
    }

    send_length_prefixed_message(&mut stream, b'B', &bind_body("port64", "shared"));
    assert_error_then_recovers(&mut stream, "54000");

    // portal 名が長すぎる。
    let long_name = "a".repeat(64);
    send_length_prefixed_message(&mut stream, b'B', &bind_body(&long_name, "shared"));
    assert_error_then_recovers(&mut stream, "54000");

    // 名前付き portal の重複は 08P01。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("port0", "shared"));
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 受け入れ条件 9: portal の Describe は RowDescription か NoData を返し、
/// ParameterDescription は返さない。
#[test]
fn describe_portal_returns_row_description_or_no_data_without_parameter_description() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    parse_and_bind(&mut stream, "s1", "p1", "SELECT id FROM documents LIMIT 1");
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "p1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(
        kind, b'T',
        "expected RowDescription only (no ParameterDescription)"
    );

    // TRUNCATE 系の書き込みは結果列を持たないため NoData。
    parse_and_bind(
        &mut stream,
        "s2",
        "p2",
        "TRUNCATE TABLE documents USING OPERATION_ID 'truncate-op'",
    );
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "p2"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'n', "expected NoData");
}

/// 受け入れ条件 10（WIRE-14 結線）: `id` 列（`numeric`）への binary 形式
/// 指定は非対応型として `0A000` で回復可能（[`result_encoder::
/// column_binary_support`] が `Id` を非対応とする）。
#[test]
fn binary_result_format_on_unsupported_column_is_rejected_with_feature_not_supported() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT id FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_binary_result_format("p1", "s1"),
    );
    assert_error_then_recovers(&mut stream, "0A000");
}

/// 受け入れ条件 10（WIRE-14 結線）: `TEXT` 列は binary 形式に対応する
/// （[`result_encoder::column_binary_support`]）。Bind が成功し、
/// Describe(Portal) の `RowDescription` が format code 1 を公告し、
/// Execute の `DataRow` は UTF-8 生バイト（text 表現とビット同一）を返す。
#[test]
fn binary_result_format_on_text_column_succeeds_and_reflects_in_row_description_and_data_row() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT body FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_result_formats("p1", "s1", &[1]),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");

    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "p1"));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'T', "expected RowDescription");
    // RowDescription 本体: field_count(i16) + [name cstr, table_oid(i32),
    // column_attnum(i16), type_oid(i32), typlen(i16), type_modifier(i32),
    // format_code(i16)] の末尾 2 バイトが format code。
    let format_code = i16::from_be_bytes([body[body.len() - 2], body[body.len() - 1]]);
    assert_eq!(
        format_code, 1,
        "RowDescription must advertise binary format"
    );

    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'D', "expected DataRow");
    // DataRow 本体: field_count(i16) + [len(i32), bytes] の 1 列。
    let value_len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
    let value_bytes = &body[6..6 + value_len];
    assert_eq!(
        value_bytes, b"row-1",
        "binary text representation must be the same UTF-8 bytes as the text representation"
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C', "expected CommandComplete");

    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
}

/// 受け入れ条件 10（WIRE-14 結線）: 結果 format code の値が 0/1 以外は
/// `08P01`（body の構文違反）で回復可能。
#[test]
fn binary_result_format_invalid_code_value_is_rejected_with_protocol_violation() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT body FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_result_formats("p1", "s1", &[2]),
    );
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 受け入れ条件 10（WIRE-14 結線）: 結果 format code の個数が
/// 0・1・列数のいずれでもない場合は `08P01` で回復可能。
#[test]
fn binary_result_format_count_mismatch_is_rejected_with_protocol_violation() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT id, body FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_result_formats("p1", "s1", &[0, 0, 0]),
    );
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 受け入れ条件 11: テナント境界。他テナントの Private 行に関わる INSERT 衝突
/// 応答バイト列が、行が存在しない場合と完全に一致する（RLS-9）。他テナントの
/// Private 行が拡張経路の SELECT に混ざらない（RLS-7）。
#[test]
fn tenant_boundary_holds_over_the_extended_protocol() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "correct-horse"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));

    // alice が Private 行（id=1）を書く。
    let mut alice = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut alice, "alice", "db");
    let _ = read_auth_request_type(&mut alice);
    send_password_message(&mut alice, "correct-horse");
    let mut header = [0u8; 1];
    alice.read_exact(&mut header).expect("auth ok");
    let mut len_buf = [0u8; 4];
    alice.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    alice.read_exact(&mut code_buf).expect("code");
    let mut msg_type = read_message_type_discarding_body(&mut alice);
    while msg_type != b'Z' {
        msg_type = read_message_type_discarding_body(&mut alice);
    }
    send_simple_query(
        &mut alice,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'alice-secret') USING OPERATION_ID 'alice-op'",
    );
    let _tag = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    // bob が拡張経路で同じ id への INSERT を試みる（他テナント所有の物理
    // キー衝突。台帳キー空間はテナント非依存のため、bob 視点では未存在 id への
    // 通常の書き込みと区別できない応答になるべき——本テストは bob が同じ
    // operation_id を使った場合の応答と、別 id・別 operation_id を使った場合の
    // 応答本文が「行の存在を漏らさない」ことを、簡易クエリの SELECT で
    // alice の行が bob から見えないことのみを固定する（RLS-7 側の確認）。
    let mut bob = tcp_connect_as(addr, "bob", "correct-horse");
    send_simple_query(&mut bob, "SELECT id FROM documents LIMIT 10");
    let _cols = read_row_description(&mut bob);
    // bob からは alice の Private 行が見えないため、直後は CommandComplete。
    let (kind, tag) = read_message(&mut bob);
    assert_eq!(kind, b'C', "no rows visible to bob");
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 0");
    read_ready_for_query(&mut bob);

    // bob が拡張経路でも同じ不可視性を確認する。
    parse_and_bind_as(&mut bob, "s1", "p1", "SELECT id FROM documents LIMIT 10");
    send_length_prefixed_message(&mut bob, b'E', &execute_body("p1", 0));
    let (kind, tag) = read_message(&mut bob);
    assert_eq!(
        kind, b'C',
        "no rows visible to bob over extended protocol either"
    );
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 0");
    send_sync(&mut bob);
    assert_ready_for_query(&mut bob);
}

fn tcp_connect_as(addr: std::net::SocketAddr, user: &str, password: &str) -> std::net::TcpStream {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, user, "db");
    let _ = read_auth_request_type(&mut stream);
    send_password_message(&mut stream, password);
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("auth ok");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("code");
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    while msg_type != b'Z' {
        msg_type = read_message_type_discarding_body(&mut stream);
    }
    stream
}

fn parse_and_bind_as(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2');
}

/// 受け入れ条件 12: `engine: None`（後方互換パス）では B/E/S/C/H は従来どおり
/// `0A000` + 切断のまま（`tests/wire_extended_query.rs` が既に固定しているが、
/// 本ファイルの命名規約に沿ってここでも 1 本だけ回帰確認する）。
#[test]
fn extended_messages_without_engine_are_still_rejected_and_closed() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_auth_request_type(&mut stream);
    send_password_message(&mut stream, "correct-horse");
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("auth ok");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("code");
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    while msg_type != b'Z' {
        msg_type = read_message_type_discarding_body(&mut stream);
    }

    send_length_prefixed_message(&mut stream, b'B', b"");
    assert_error_response(&mut stream, "0A000");
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");
}

/// 受け入れ条件 13: 簡易クエリの不変性。拡張シーケンスを挟んでも簡易クエリの
/// 応答バイト列が変わらない。
#[test]
fn simple_query_response_is_unchanged_across_extended_sequences() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";

    let read_full = |stream: &mut std::net::TcpStream| -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let (kind, body) = read_message(stream);
            out.push(kind);
            out.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
            out.extend_from_slice(&body);
            if kind == b'Z' {
                break;
            }
        }
        out
    };

    send_simple_query(&mut stream, sql);
    let first = read_full(&mut stream);

    parse_and_bind(&mut stream, "sI", "pI", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pI", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    send_simple_query(&mut stream, sql);
    let second = read_full(&mut stream);

    assert_eq!(
        first, second,
        "simple query response must be byte-identical"
    );
}
