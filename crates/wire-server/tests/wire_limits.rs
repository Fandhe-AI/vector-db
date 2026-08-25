//! wire-server の結合テスト（TASK-69・対象ビヘイビア WIRE-5, WIRE-6）。
//!
//! ephemeral port（`127.0.0.1:0`）で `wire_server::server::accept_loop_with_limiter` を起動し、
//! `std::net::TcpStream` で生バイトを送受信する自作クライアントを用いる
//! （`tests/wire_auth.rs` と同じ流儀。結合テスト間でモジュールを共有しないため
//! ヘルパーはこのファイル内に閉じる）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::{argon2id, UserStore};
use wire_server::limits::ConnectionLimiter;

/// `UserStore::load_from_file` は Argon2id パラメータが `RECOMMENDED_PARAMS` と
/// 完全一致するレコードのみを受理するため、フィクスチャも本番既定値を使う
/// （`tests/wire_auth.rs` と同じ理由）。
const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

fn write_user_store_file(records: &[(&str, &str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-limits-test-{}-{}",
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

/// `wire_server::server::accept_loop_with_limiter` をサーバースレッドで起動し、
/// `(接続先アドレス, リミッターのクローン)` を返す。呼び出し元はリミッターの
/// `active()` を観測して、拒否時に枠が消費されていないことを間接確認できる。
fn spawn_server(
    users_path: &std::path::Path,
    max_connections: usize,
    read_timeout: Duration,
) -> (std::net::SocketAddr, ConnectionLimiter) {
    let store = Arc::new(UserStore::load_from_file(users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);
    let limiter_for_loop = limiter.clone();

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_limiter(
            listener,
            store,
            limiter_for_loop,
            read_timeout,
        );
    });

    (addr, limiter)
}

/// SSLRequest → 'N' → StartupMessage(protocol 3.0, user=...) を送るクライアント側の
/// 共通シーケンス（`tests/wire_auth.rs` と同じ内容）。
fn send_ssl_request_and_startup(stream: &mut TcpStream, username: &str, database: &str) {
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

/// 認証完了まで進め、`ReadyForQuery` に到達させる（WIRE-6 の「認証済みでも枠は
/// 占有し続ける」ことの検証で使う）。
fn complete_authentication(stream: &mut TcpStream, username: &str, password: &str) {
    send_ssl_request_and_startup(stream, username, "db");
    let _ = read_auth_request_type(stream);
    send_password_message(stream, password);

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read auth ok type");
    assert_eq!(header[0], b'R', "expected AuthenticationOk");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read code");
    assert_eq!(i32::from_be_bytes(code_buf), 0, "AuthenticationOk expected");

    let mut msg_type = read_message_type_discarding_body(stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(stream);
    }
}

/// 接続が受理されて待機している（即座に EOF ではない）ことを、短いプローブ猶予
/// 内の `WouldBlock` で確認する。
fn assert_connection_accepted_and_idle(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set client probe timeout");
    let mut buf = [0u8; 1];
    let err = stream.read(&mut buf).expect_err("no data sent yet");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::WouldBlock,
        "connection must be accepted and left open (not immediately closed)"
    );
}

// ---------------------------------------------------------------------------
// WIRE-5: 読み取りタイムアウト
// ---------------------------------------------------------------------------

/// 接続後何も送らずアイドルすると、応答なし（`'E'` 等のバイトが一切ない）で
/// EOF になること。
#[test]
fn wire5_pre_auth_idle_is_closed_without_response() {
    let read_timeout = Duration::from_millis(300);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server(&users_path, 4, read_timeout);

    let mut stream = TcpStream::connect(addr).expect("connect");
    std::thread::sleep(read_timeout * 3);

    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "idle connection must be closed by the read timeout without any response bytes"
    );
}

/// StartupMessage の length フィールドだけ送って停止した部分フレームも、
/// 応答なしで EOF になること（WIRE-1 の SSL ネゴシエーション後の部分フレーム）。
#[test]
fn wire5_partial_frame_is_closed_without_response() {
    let read_timeout = Duration::from_millis(300);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server(&users_path, 4, read_timeout);

    let mut stream = TcpStream::connect(addr).expect("connect");

    let mut ssl_req = Vec::new();
    ssl_req.extend_from_slice(&8i32.to_be_bytes());
    ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl_req).expect("send SSLRequest");
    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(&resp, b"N");

    // StartupMessage の length(4 バイト)だけ送って停止する（body を送らない）。
    let total_len = 4i32 + 4 + 16; // 適当な妥当長。中身は送らない。
    stream
        .write_all(&total_len.to_be_bytes())
        .expect("send partial length prefix only");

    std::thread::sleep(read_timeout * 3);
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "partial frame must be closed by the read timeout without any response bytes"
    );
}

/// 認証完了後にアイドルしても応答なしで EOF になり、接続枠が解放されて後続接続を
/// 受理できること（有効な資格情報を持つクライアントが接続を張ったまま何も送らない
/// ことで枠を永久占有できないことの回帰確認）。
#[test]
fn wire5_post_auth_idle_is_closed_without_response_and_permit_released() {
    let read_timeout = Duration::from_millis(300);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server(&users_path, 1, read_timeout);

    let mut stream = TcpStream::connect(addr).expect("connect");
    complete_authentication(&mut stream, "alice", "correct-horse");

    std::thread::sleep(read_timeout * 3);
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "authenticated session must be closed by the read timeout without any response"
    );

    // 枠が解放されるまで少し待ってから、後続接続が受理されることを確認する。
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(limiter.active(), 0, "permit must be released after close");

    let mut next = TcpStream::connect(addr).expect("connect after permit release");
    assert_connection_accepted_and_idle(&mut next);
}

// ---------------------------------------------------------------------------
// WIRE-6: 共有接続数リミッター
// ---------------------------------------------------------------------------

/// max=1 で 1 本保持している間、2 本目は `'E'` / `53300` を受けて EOF になること。
/// 保持中の 1 本目は影響を受けず認証を完了でき、1 本目を閉じた後の 3 本目は
/// 受理されること。
#[test]
fn wire6_over_limit_is_rejected_with_53300_in_accept_loop() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server(&users_path, 1, Duration::from_secs(5));

    let mut held = TcpStream::connect(addr).expect("connect first (holds the only slot)");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(limiter.active(), 1);

    let mut second = TcpStream::connect(addr).expect("connect second");
    let mut header = [0u8; 1];
    second.read_exact(&mut header).expect("read message type");
    assert_eq!(header[0], b'E', "expected ErrorResponse");
    let mut len_buf = [0u8; 4];
    second.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    second.read_exact(&mut body).expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains(wire_server::limits::SQLSTATE_TOO_MANY_CONNECTIONS),
        "ErrorResponse must carry SQLSTATE 53300, got: {body_str:?}"
    );
    let mut extra = [0u8; 1];
    let n = second.read(&mut extra).unwrap_or(0);
    assert_eq!(n, 0, "rejected connection must be closed");

    // 保持中の 1 本目は影響を受けず認証を完了できる。
    complete_authentication(&mut held, "alice", "correct-horse");

    drop(held);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(limiter.active(), 0, "permit must be released after close");

    let mut third = TcpStream::connect(addr).expect("connect third after release");
    assert_connection_accepted_and_idle(&mut third);
}

/// 認証済み接続が枠を占有していても、未認証の 2 本目は同じく `53300` で拒否される
/// こと（枠の占有は認証状態を問わない）。
#[test]
fn wire6_limit_applies_to_authenticated_and_unauthenticated_alike() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, _limiter) = spawn_server(&users_path, 1, Duration::from_secs(5));

    let mut authenticated = TcpStream::connect(addr).expect("connect first");
    complete_authentication(&mut authenticated, "alice", "correct-horse");

    let mut second = TcpStream::connect(addr).expect("connect second");
    let mut header = [0u8; 1];
    second.read_exact(&mut header).expect("read message type");
    assert_eq!(
        header[0], b'E',
        "unauthenticated connection must be rejected even though the occupying slot is authenticated"
    );
}

/// max=4 で 16 クライアントを同時 connect すると、受理 4・`53300` 拒否 12 を
/// ちょうど観測し、`limiter.active()` が 4 を超えないこと。
#[test]
fn wire6_concurrent_burst_never_exceeds_max() {
    const MAX: usize = 4;
    const BURST: usize = 16;

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server(&users_path, MAX, Duration::from_secs(5));

    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut held: Vec<TcpStream> = Vec::new();

    for _ in 0..BURST {
        let mut stream = TcpStream::connect(addr).expect("connect burst client");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("set probe timeout");
        let mut header = [0u8; 1];
        match stream.read(&mut header) {
            // WouldBlock: 受理されて待機中（枠を取得できた）。
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                accepted += 1;
                held.push(stream);
            }
            // 'E' が読めた: 上限超過で拒否された。
            Ok(1) if header[0] == b'E' => {
                rejected += 1;
            }
            other => panic!("unexpected read outcome: {other:?}"),
        }
    }

    assert_eq!(accepted, MAX, "exactly max connections must be accepted");
    assert_eq!(
        rejected,
        BURST - MAX,
        "the remaining connections must be rejected with 53300"
    );
    assert!(
        limiter.active() <= MAX,
        "active permits must never exceed max"
    );
}
