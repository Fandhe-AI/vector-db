//! wire-server の結合テスト（TASK-71・対象ビヘイビア WIRE-8）。
//!
//! 認証成功後のセッションが拡張クエリプロトコル系メッセージ（Parse/Bind/
//! Describe/Execute/Sync/Close/Flush）を受信した場合に、SQLSTATE `0A000` の
//! ErrorResponse を返したうえで接続を閉じること（黙って無視しない・
//! ReadyForQuery を返さない・エラー後に接続を維持しない）を確認する。

mod common;

use std::io::Write;
use std::net::Shutdown;
use std::time::Duration;

use common::{
    authenticate_to_ready_for_query, expect_connection_closed, expect_error_response_with_sqlstate,
    send_length_prefixed_message, spawn_server_accepting_one, spawn_server_with_accept_loop,
    write_user_store_file,
};

/// ポインタ: TASK-71・WIRE-8。Parse+Bind+Describe+Execute+Sync をパイプライン
/// 送信しても、最初の 1 通に対してのみ `0A000` が返り、以降は ReadyForQuery も
/// 2 通目の ErrorResponse も来ず EOF になること（同期回復を実装しない MVP の
/// 契約: エラー後は即クローズ）。
#[test]
fn wire8_parse_bind_execute_sync_pipeline_gets_0a000_then_eof() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // Parse('P') + Bind('B') + Describe('D') + Execute('E') + Sync('S') を
    // 1 回の write_all でまとめて送る（クライアントのパイプライン送信を模す）。
    let mut pipeline = Vec::new();
    for type_byte in [b'P', b'B', b'D', b'E', b'S'] {
        let total_len = 4i32; // 空 body
        pipeline.push(type_byte);
        pipeline.extend_from_slice(&total_len.to_be_bytes());
    }
    stream.write_all(&pipeline).expect("send pipeline");
    stream.shutdown(Shutdown::Write).ok();

    expect_error_response_with_sqlstate(&mut stream, "0A000");
    expect_connection_closed(&mut stream);
}

/// ポインタ: TASK-71・WIRE-8。拡張クエリプロトコル系の型それぞれを単独送信しても
/// 同様に `0A000` + 切断となること。
#[test]
fn wire8_each_extended_message_alone_is_rejected_and_closed() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);

    for type_byte in [b'P', b'B', b'D', b'E', b'S', b'C', b'H'] {
        let addr = spawn_server_accepting_one(&users_path);
        let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

        send_length_prefixed_message(&mut stream, type_byte, b"");
        stream.shutdown(Shutdown::Write).ok();

        expect_error_response_with_sqlstate(&mut stream, "0A000");
        expect_connection_closed(&mut stream);
    }
}

/// 対照テスト（過剰クローズの回帰防止）: 簡易クエリ（'Q'）は従来どおり
/// ErrorResponse ('E') → ReadyForQuery ('Z') を返し、接続は維持される
/// （WIRE-8 の対象外であることの確認）。
#[test]
fn simple_query_still_keeps_connection_open() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // 簡易クエリの構造検証（`handshake.rs`）を満たす最小 body（終端 NUL のみ）。
    send_length_prefixed_message(&mut stream, b'Q', b"\0");

    expect_error_response_with_sqlstate(&mut stream, "0A000");

    // ReadyForQuery('Z') を読めること（接続が閉じられていないこと）。
    use std::io::Read;
    let mut header = [0u8; 1];
    stream
        .read_exact(&mut header)
        .expect("read ReadyForQuery type");
    assert_eq!(
        header[0], b'Z',
        "expected ReadyForQuery after simple query error"
    );
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
}

/// ポインタ: TASK-71・WIRE-8。未知の型バイトも従来どおり拒否＋切断される
/// （`protocol_dispatch::classify` の `Unknown` 分類が回帰しないことの確認）。
#[test]
fn wire8_unknown_message_type_is_rejected_and_closed() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'?', b"");
    stream.shutdown(Shutdown::Write).ok();

    expect_error_response_with_sqlstate(&mut stream, "0A000");
    expect_connection_closed(&mut stream);
}

/// ポインタ: TASK-71・WIRE-8。拒否後に接続スロットが解放され、次の接続が
/// 受理されること（有界 lingering close が枠を永続占有しないことの確認）。
#[test]
fn wire8_rejection_releases_connection_slot() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_accept_loop(
        &users_path,
        1,
        Duration::from_secs(5),
        Duration::from_secs(300),
    );

    let mut rejected = common::authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_length_prefixed_message(&mut rejected, b'P', b"");
    rejected.shutdown(Shutdown::Write).ok();
    expect_error_response_with_sqlstate(&mut rejected, "0A000");
    expect_connection_closed(&mut rejected);

    // 枠(1)が解放されたはずなので、次の接続はすぐに閉じられない
    // （即座に EOF ではなく、認証応答まで進められること）。
    // `send_ssl_request_and_startup` は内部で SSLRequest の 'N' 応答を読み切って
    // から StartupMessage を送るため、続けて AuthenticationCleartextPassword
    // （'R', code=3）が読めることまで確認する。
    let mut second = std::net::TcpStream::connect(addr).expect("connect second");
    second
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set read timeout");
    common::send_ssl_request_and_startup(&mut second, "alice", "db");
    let auth_code = common::read_auth_request_type(&mut second);
    assert_eq!(
        auth_code, 3,
        "slot must be free for the next connection to reach AuthenticationCleartextPassword"
    );
}
