//! TLS 上の pg wire 接続の結合テスト（Issue #966・TASK-228・WIRE-9 ポインタ。
//! `wire_server::server::accept_loop_with_tls`／`handshake::
//! handle_connection_with_options` の受入基準を検証する）。
//!
//! `tests/common/tls_client.rs`（`tests/tls_server_handshake.rs` の
//! `TestClient`／`drive_client_handshake_over_socket` と同じ構成要素を
//! 独立に持つ最小 TLS 1.3 クライアント）で実ハンドシェイクを駆動し、
//! StartupMessage・認証・簡易クエリを TLS レコード越しに送受信する。
//! 実クライアント（psql／openssl s_client）での疎通は #969 の担当。

#[path = "common/mod.rs"]
mod common;
#[path = "common/tls_client.rs"]
mod tls_client;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const GSSENC_REQUEST_CODE: i32 = 80_877_104;

fn write_ssl_request(stream: &mut TcpStream) {
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&msg).expect("send SSLRequest");
}

fn write_startup_message(stream: &mut impl Write, username: &str, database: &str) {
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0");
    params.extend_from_slice(username.as_bytes());
    params.push(0);
    params.extend_from_slice(b"database\0");
    params.extend_from_slice(database.as_bytes());
    params.push(0);
    params.push(0);
    let total_len = (4 + 4 + params.len()) as i32;
    let mut startup = Vec::new();
    startup.extend_from_slice(&total_len.to_be_bytes());
    startup.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    startup.extend_from_slice(&params);
    stream.write_all(&startup).expect("send StartupMessage");
}

fn write_typed_message(stream: &mut impl Write, type_byte: u8, body: &[u8]) {
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::with_capacity(1 + 4 + body.len());
    msg.push(type_byte);
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(body);
    stream.write_all(&msg).expect("write typed message");
}

fn read_exact_n(stream: &mut impl Read, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).expect("read exact");
    buf
}

/// 型バイト付きメッセージ 1 個を読む（`(type_byte, body)`）。
fn read_typed_message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let type_byte = read_exact_n(stream, 1)[0];
    let len_bytes = read_exact_n(stream, 4);
    let len = i32::from_be_bytes(len_bytes.try_into().expect("4 bytes")) as usize;
    let body_len = len.checked_sub(4).expect("length includes itself");
    let body = read_exact_n(stream, body_len);
    (type_byte, body)
}

/// AuthenticationOk 以降（BackendKeyData・ParameterStatus*）を読み飛ばし、
/// ReadyForQuery（'Z'）に到達するまで進める。
fn drain_to_ready_for_query(stream: &mut impl Read) {
    loop {
        let (type_byte, _body) = read_typed_message(stream);
        if type_byte == b'Z' {
            return;
        }
    }
}

fn spawn_tls_server(users_path: &std::path::Path) -> std::net::SocketAddr {
    let store = Arc::new(UserStore::load_from_file(users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    let tls = tls_client::test_config();

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_tls(
            listener,
            store,
            None,
            Some(tls),
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// 受入基準 2: TLS 未設定時は既存どおり `'N'` を返し、平文接続がそのまま
/// 使えること（`accept_loop_with_limiter` と同じ経路。回帰確認）。
#[test]
fn tls_disabled_server_still_declines_ssl_with_n() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let store = Arc::new(UserStore::load_from_file(&users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_limiter(
            listener,
            store,
            limiter,
            Duration::from_secs(5),
        );
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut stream);
    let resp = read_exact_n(&mut stream, 1);
    assert_eq!(&resp, b"N", "TLS-disabled server must decline SSL with 'N'");
}

/// 受入基準 1・2: TLS opt-in 時に `SSLRequest` へ `'S'` を返し、ハンドシェイク
/// → TLS 上の StartupMessage → cleartext 認証 → 簡易クエリ往復まで完走する
/// こと。`engine` は未接続（`None`）のため簡易クエリ応答は `0A000` になるが、
/// これは平文接続の `engine: None` 経路と同一の契約であり、TLS レコード上で
/// 認証後シーケンス全体が正しく動くことの確認が本テストの目的。
#[test]
fn tls_enabled_server_returns_s_and_completes_query_round_trip_over_tls() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S", "TLS-enabled server must accept SSL with 'S'");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    write_startup_message(&mut channel, "alice", "irrelevant-db-name");

    // AuthenticationCleartextPassword（'R'/3）。
    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R');
    let auth_code = i32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
    assert_eq!(auth_code, 3, "AuthenticationCleartextPassword expected");

    let mut password_body = Vec::new();
    password_body.extend_from_slice(b"pw-alice");
    password_body.push(0);
    write_typed_message(&mut channel, b'p', &password_body);

    // AuthenticationOk（'R'/0）。
    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R');
    assert_eq!(
        i32::from_be_bytes(body[..4].try_into().expect("4 bytes")),
        0
    );

    drain_to_ready_for_query(&mut channel);

    // 簡易クエリ（'Q'）。engine 未接続のため `0A000` を期待する。
    let mut query_body = Vec::new();
    query_body.extend_from_slice(b"SELECT 1");
    query_body.push(0);
    write_typed_message(&mut channel, b'Q', &query_body);

    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("0A000"), "expected 0A000, got: {text}");
    drain_to_ready_for_query(&mut channel);

    // Terminate（'X'）で正常終了する。
    write_typed_message(&mut channel, b'X', &[]);
}

/// 受入基準 3: TLS 確立後の `SSLRequest`（初回であっても）は拒否・切断される
/// こと。
#[test]
fn ssl_request_after_tls_established_is_rejected() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    // TLS 確立後に SSLRequest を送る（StartupMessage 相当のフレームとして
    // TLS レコード内に含める）。
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    channel.write_all(&msg).expect("send SSLRequest over TLS");

    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(
        type_byte, b'E',
        "expected ErrorResponse rejecting SSLRequest after TLS"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("08P01") || text.contains("42601") || text.contains("invalid"),
        "unexpected error body: {text}"
    );

    // 接続はその後閉じられる。サーバーは ErrorResponse 送出後に
    // `graceful_close`（best-effort な `close_notify`）を経てから TCP を
    // 閉じるため、暗号化された `close_notify` バイト列そのものが先に届き
    // うる（読み取れること自体は「まだ開いている」ことを意味しない）。
    // 有界な時間内に最終的な EOF（`Ok(0)`）へ到達することだけを確認する。
    let mut socket = channel.into_socket();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut probe = [0u8; 256];
    let mut reached_eof = false;
    for _ in 0..64 {
        match socket.read(&mut probe) {
            Ok(0) => {
                reached_eof = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        reached_eof,
        "connection must eventually close after rejecting SSLRequest post-TLS"
    );
}

/// 受入基準 3: TLS 確立後の `GSSENCRequest`（初回であっても）も同様に拒否・
/// 切断されること。
#[test]
fn gssenc_request_after_tls_established_is_rejected() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&GSSENC_REQUEST_CODE.to_be_bytes());
    channel
        .write_all(&msg)
        .expect("send GSSENCRequest over TLS");

    let (type_byte, _body) = read_typed_message(&mut channel);
    assert_eq!(
        type_byte, b'E',
        "expected ErrorResponse rejecting GSSENCRequest after TLS"
    );
}

/// pipelining 対策の回帰確認（CVE-2021-23214 型）: `SSLRequest` と平文の
/// StartupMessage を同一 `write` で送っても、平文バイト列が StartupMessage
/// として処理されず、TLS ハンドシェイクの ClientHello として解釈されて
/// 失敗すること（応答なしで切断・あるいは fatal alert 後に切断）。
#[test]
fn pipelined_plaintext_after_ssl_request_is_not_processed_as_startup() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    let mut combined = Vec::new();
    combined.extend_from_slice(&8i32.to_be_bytes());
    combined.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    // StartupMessage 相当の平文バイト列を SSLRequest と同じ write で送る。
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0alice\0database\0db\0\0");
    let total_len = (4 + 4 + params.len()) as i32;
    combined.extend_from_slice(&total_len.to_be_bytes());
    combined.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    combined.extend_from_slice(&params);
    socket.write_all(&combined).expect("send pipelined bytes");

    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    // 平文の StartupMessage 相当バイト列は TLS レコードとして誤って解釈され、
    // ハンドシェイクは失敗する。届きうるのは失敗した TLS ハンドシェイクの
    // fatal alert（レコードヘッダの先頭バイトは ContentType::Alert = 0x15）
    // か EOF のみで、正規の pg wire 応答（`AuthenticationCleartextPassword`
    // の型バイト `'R'` = 0x52）が届くことは決してない。
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut buf = [0u8; 1];
    match socket.read(&mut buf) {
        Ok(0) => {}
        Ok(_) => assert_ne!(
            buf[0], b'R',
            "must not process pipelined plaintext as a valid pg wire AuthenticationCleartextPassword response"
        ),
        Err(_) => {}
    }
}
