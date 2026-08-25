//! wire-server の結合テスト（TASK-67・対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）。
//!
//! ephemeral port（`127.0.0.1:0`）でサーバースレッドを起動し、`std::net::TcpStream`
//! で生バイトを送受信する自作クライアントを用いる（`psql` 等の外部プロセスは CI に
//! 存在しないため対象外。実機確認は手元検証に委ねる）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use wire_server::auth::{argon2id, UserStore};

/// テスト実行時間短縮のための軽量 Argon2id パラメータ。本番既定値は
/// `auth::DEFAULT_PARAMS`（`wire-server hash-password` サブコマンド）。
const TEST_PARAMS: argon2id::Params = argon2id::Params {
    m_cost_kib: 64,
    t_cost: 1,
    p_cost: 1,
};

fn write_user_store_file(records: &[(&str, &str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-auth-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("users.txt");

    let mut content = String::new();
    for (username, tenant_id, password) in records {
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(password.as_bytes(), salt, &TEST_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{username}:{tenant_id}:{phc}\n"));
    }
    std::fs::write(&path, content).expect("write user store fixture");
    path
}

/// サーバースレッドを起動し、`(接続先アドレス, 停止用ハンドル)` を返す。
/// 1 接続だけ受理してスレッドを終了する（テストのシーケンス制御を単純にするため）。
fn spawn_server_accepting_one(users_path: &std::path::Path) -> std::net::SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = wire_server::handshake::handle_connection(stream, &store);
        }
    });

    addr
}

/// SSLRequest → 'N' → StartupMessage(protocol 3.0, user=...) を送るクライアント側の
/// 共通シーケンス。
fn send_ssl_request_and_startup(stream: &mut TcpStream, username: &str, database: &str) {
    // SSLRequest: length=8, code=80877103
    let mut ssl_req = Vec::new();
    ssl_req.extend_from_slice(&8i32.to_be_bytes());
    ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl_req).expect("send SSLRequest");

    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(
        &resp, b"N",
        "server must decline SSL (WIRE-1: cleartext only)"
    );

    // StartupMessage: length + protocol(3.0) + "user\0<username>\0" +
    // "database\0<database>\0" + terminator 0x00
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

fn read_auth_request_type(stream: &mut TcpStream) -> i32 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    assert_eq!(header[0], b'R', "expected Authentication* message");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf);
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read auth code");
    // AuthenticationCleartextPassword はペイロードがコードのみ（length=8）。
    assert_eq!(len, 8);
    i32::from_be_bytes(code_buf)
}

fn send_password_message(stream: &mut TcpStream, password: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(password.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send PasswordMessage");
}

/// 次のメッセージの型バイトを返す（本文は破棄）。`ReadyForQuery` 到達確認・
/// `ErrorResponse` 検出の両方に使う。
fn read_message_type_discarding_body(stream: &mut TcpStream) -> u8 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    header[0]
}

/// WIRE-1 系: SSLRequest → 'N' → StartupMessage → AuthenticationCleartextPassword →
/// 正パスワード → AuthenticationOk → (BackendKeyData → ParameterStatus* →)
/// ReadyForQuery の順序どおり到達すること。
#[test]
fn wire1_successful_cleartext_auth_reaches_ready_for_query() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db-name");
    let auth_code = read_auth_request_type(&mut stream);
    assert_eq!(
        auth_code, 3,
        "AuthenticationCleartextPassword code must be 3"
    );

    send_password_message(&mut stream, "correct-horse");

    // AuthenticationOk ('R', code=0)
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read auth ok type");
    assert_eq!(header[0], b'R');
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read code");
    assert_eq!(
        i32::from_be_bytes(code_buf),
        0,
        "AuthenticationOk code must be 0"
    );

    // 後続メッセージ（BackendKeyData・ParameterStatus*）を読み飛ばし、
    // 最終的に ReadyForQuery('Z') へ到達することを確認する。
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(&mut stream);
    }
}

/// WIRE-3 系: 誤パスワードで `28P01` の ErrorResponse を受信し、ReadyForQuery が
/// 送られず接続が切断されること。応答までの経過時間が固定遅延の下限以上であること
/// （上限はフレーク回避のため検証しない）。
#[test]
fn wire3_wrong_password_returns_28p01_without_ready_for_query() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_auth_request_type(&mut stream);

    let start = Instant::now();
    send_password_message(&mut stream, "wrong-password");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    let elapsed = start.elapsed();
    assert_eq!(header[0], b'E', "expected ErrorResponse on wrong password");
    assert!(
        elapsed >= Duration::from_millis(200),
        "auth failure must incur the fixed delay (WIRE-3)"
    );

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("28P01"),
        "ErrorResponse must carry SQLSTATE 28P01, got: {body_str:?}"
    );

    // 接続は切断される（ReadyForQuery を送らない）: 追加の読み取りは EOF になる。
    let mut extra = [0u8; 1];
    let n = stream.read(&mut extra).unwrap_or(0);
    assert_eq!(
        n, 0,
        "connection must be closed after auth failure, not kept open"
    );
}

/// 未知ユーザーでも同一の応答（`28P01`）・同一の固定遅延であること（列挙対策）。
#[test]
fn wire3_unknown_user_returns_same_error_and_delay() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "no-such-user", "db");
    let _ = read_auth_request_type(&mut stream);

    let start = Instant::now();
    send_password_message(&mut stream, "anything");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    let elapsed = start.elapsed();
    assert_eq!(header[0], b'E');
    assert!(elapsed >= Duration::from_millis(200));
}

/// WIRE-2 系: StartupMessage の `database` に他テナント名を自己申告しても、接続の
/// 成否・応答シーケンスに影響しないこと（実際のテナント導出値そのものの検証は
/// `auth::verify` の単体テストで行う。ここでは「クライアント自己申告値を使わない」
/// ことの外形的帰結として、無関係な `database` 値でも正規ユーザーが認証成功する
/// ことを確認する）。
#[test]
fn wire2_database_param_does_not_affect_authentication_outcome() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "tenant-b-spoofed");
    let _ = read_auth_request_type(&mut stream);
    send_password_message(&mut stream, "correct-horse");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    assert_eq!(
        header[0], b'R',
        "authentication must succeed regardless of the self-reported database parameter"
    );
}
