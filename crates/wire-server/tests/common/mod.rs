//! `tests/wire_extended_query.rs`（TASK-71・WIRE-8）専用の結合テストヘルパー。
//!
//! `tests/wire_auth.rs` 側にも同種のヘルパーが既に存在するが、並列実装中の
//! 兄弟イシュー（TASK-68/69・framing/limits）が同ファイルを編集しうるため、
//! 本タスクでは移設・共通化せず独立に用意する（統合は別途フォローアップ）。
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::{argon2id, UserStore};
use wire_server::limits::ConnectionLimiter;

/// `UserStore::load_from_file` は Argon2id パラメータが
/// `argon2id::RECOMMENDED_PARAMS` と完全一致するレコードのみを受理するため、
/// フィクスチャも本番既定値をそのまま使う（`tests/wire_auth.rs` と同方針）。
const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

/// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
/// プロセス内単調カウンタ（`wire_auth.rs` と同一クラスの競合対策。Issue #172）。
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn write_user_store_file(records: &[(&str, &str, &str)]) -> std::path::PathBuf {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-extended-query-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
        seq
    ));
    // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる
    // （Issue #172）。
    std::fs::create_dir(&dir).expect("create unique fixture dir");
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

/// サーバースレッドを起動し、1 接続だけ受理してスレッドを終了する。
pub fn spawn_server_accepting_one(users_path: &std::path::Path) -> std::net::SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = wire_server::handshake::handle_connection_bounded(stream, &store);
        }
    });

    addr
}

/// `wire_server::server::accept_loop_with_limiter` 経由でサーバースレッドを起動する
/// （接続スロット解放の確認に使う）。
pub fn spawn_server_with_accept_loop(
    users_path: &std::path::Path,
    max_connections: usize,
    io_timeout: Duration,
) -> std::net::SocketAddr {
    let store = Arc::new(UserStore::load_from_file(users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_limiter(listener, store, limiter, io_timeout);
    });

    addr
}

/// SSLRequest → 'N' → StartupMessage(protocol 3.0, user=...) を送るクライアント側の
/// 共通シーケンス。
pub fn send_ssl_request_and_startup(stream: &mut TcpStream, username: &str, database: &str) {
    let mut ssl_req = Vec::new();
    ssl_req.extend_from_slice(&8i32.to_be_bytes());
    ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl_req).expect("send SSLRequest");

    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(&resp, b"N", "server must decline SSL");

    send_startup_message(stream, username, database);
}

/// StartupMessage（protocol 3.0, user=.../database=...）のみを送る
/// （SSLRequest フェーズを別途消費済みの接続向け。`connect_after_slot_available`
/// が SSLRequest の 'N' 応答まで進めた接続の続きとして使う）。
pub fn send_startup_message(stream: &mut TcpStream, username: &str, database: &str) {
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

/// 接続スロットが解放されるまで短い間隔でリトライし、解放後の TCP 接続を
/// 返す（SSLRequest の 'N' 応答まで到達した時点で解放済みと判定する）。
///
/// `server::accept_loop_with_limiter` は同時接続数の上限を超える接続を、ハンドシェイクへ
/// 進ませず `limits::ConnectionLimiter::try_acquire` 失敗時に `53300` の
/// ErrorResponse を返してから即座にクローズする（`limits::reject_too_many_connections`
/// 経由）。そのため SSLRequest の応答が 1 バイトも来ず EOF/タイムアウトになる
/// ことが「スロット未解放」の確実な合図になる。逆に 'N' が読めれば
/// `ConnectionLimiter` の枠を確保できたことの証拠であり、拒否された接続の
/// write-side shutdown（drain 開始時の EOF）を誤ってスロット解放の証拠として
/// 扱う競合を避けられる（`wire8_rejection_releases_connection_slot` のレビュー
/// 是正）。
pub fn connect_after_slot_available(
    addr: std::net::SocketAddr,
    total_timeout: Duration,
) -> TcpStream {
    let deadline = std::time::Instant::now() + total_timeout;
    loop {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set read timeout");

        let mut ssl_req = Vec::new();
        ssl_req.extend_from_slice(&8i32.to_be_bytes());
        ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
        stream.write_all(&ssl_req).expect("send SSLRequest");

        let mut resp = [0u8; 1];
        match stream.read_exact(&mut resp) {
            // 'N': ハンドシェイクへ進んだ = ConnectionLimiter の枠を確保できた。
            Ok(()) if &resp == b"N" => {
                stream.set_read_timeout(None).expect("clear read timeout");
                return stream;
            }
            // 'E': limits::reject_too_many_connections が 53300 の ErrorResponse
            // を書いてから切断した = まだ枠が空いていない。リトライする。
            Ok(()) => {
                assert_eq!(
                    &resp, b"E",
                    "unexpected first response byte while polling for a free slot"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "connection slot was not released within {total_timeout:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                // 拒否ワーカー自体が枯渇した場合の応答なし即時クローズ、または
                // まだ枠が空くのを待っている途中のタイムアウト。どちらも
                // 「未解放」としてリトライする。
                assert!(
                    std::time::Instant::now() < deadline,
                    "connection slot was not released within {total_timeout:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

pub fn read_auth_request_type(stream: &mut TcpStream) -> i32 {
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

pub fn send_password_message(stream: &mut TcpStream, password: &str) {
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

/// 次のメッセージの型バイトを返す（本文は破棄）。
pub fn read_message_type_discarding_body(stream: &mut TcpStream) -> u8 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    header[0]
}

/// AuthenticationCleartextPassword → 正しいパスワードで認証し、後続の
/// BackendKeyData・ParameterStatus* を読み飛ばして ReadyForQuery(`'Z'`) 到達まで
/// 進める。戻り値は接続済みの `TcpStream`（呼び出し元がそのまま拡張クエリ系
/// メッセージを送信できる状態）。
pub fn authenticate_to_ready_for_query(
    addr: std::net::SocketAddr,
    username: &str,
    password: &str,
) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, username, "irrelevant-db-name");
    let auth_code = read_auth_request_type(&mut stream);
    assert_eq!(auth_code, 3, "AuthenticationCleartextPassword expected");

    send_password_message(&mut stream, password);

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read auth ok type");
    assert_eq!(header[0], b'R', "expected AuthenticationOk");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read code");
    assert_eq!(i32::from_be_bytes(code_buf), 0, "AuthenticationOk code");

    let mut msg_type = read_message_type_discarding_body(&mut stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(&mut stream);
    }

    stream
}

/// 型バイト + 空 body の長さプレフィックス付きメッセージを組み立てて送る
/// （拡張クエリプロトコル系メッセージは本テストでは body の中身を解釈しないため
/// 空 body で十分。長さフィールドのみ正しく検証されればよい）。
pub fn send_length_prefixed_message(stream: &mut TcpStream, type_byte: u8, body: &[u8]) {
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(type_byte);
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(body);
    stream.write_all(&msg).expect("send message");
}

/// ErrorResponse（'E'）を読み取り、SQLSTATE が期待値と一致することを確認する。
pub fn expect_error_response_with_sqlstate(stream: &mut TcpStream, expected_sqlstate: &str) {
    expect_error_response_body(stream, expected_sqlstate, None);
}

/// ErrorResponse（'E'）を読み取り、SQLSTATE 及び（指定時）メッセージ本文の部分
/// 一致を確認する。分類ごとにメッセージ文言が分かれていること
/// （`protocol_dispatch::response_message`）の結合テスト側の回帰防止に使う。
pub fn expect_error_response_with_sqlstate_and_message(
    stream: &mut TcpStream,
    expected_sqlstate: &str,
    expected_message_substr: &str,
) {
    expect_error_response_body(stream, expected_sqlstate, Some(expected_message_substr));
}

fn expect_error_response_body(
    stream: &mut TcpStream,
    expected_sqlstate: &str,
    expected_message_substr: Option<&str>,
) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'E', "expected ErrorResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate}, got: {body_str:?}"
    );
    if let Some(expected_message_substr) = expected_message_substr {
        assert!(
            body_str.contains(expected_message_substr),
            "expected message containing {expected_message_substr:?}, got: {body_str:?}"
        );
    }
}

/// 応答読み取り後に接続が閉じられている（追加の読み取りが EOF になる）ことを
/// 確認する。読み取り前に書き込み方向を閉じ、サーバー側の lingering close が
/// 即座に EOF を検出できるようにする（drain のタイムアウト分だけテストが遅く
/// なるのを避けるため）。
pub fn expect_connection_closed(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut extra = [0u8; 1];
    let n = stream.read(&mut extra).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed after rejection");
    let _ = extra;
}

// ---------------------------------------------------------------------------
// engine 接続経路のヘルパー（TASK-73・WIRE-1。`tests/wire1_simple_query.rs` と
// `tests/three_client_e2e.rs` が共有する）。
// ---------------------------------------------------------------------------

/// サーバースレッドを `wire_server::server::accept_loop_with_engine` 経由で起動する
/// （簡易クエリが `engine::core::EngineCore` へ到達する経路。TASK-73）。
pub fn spawn_server_with_engine(
    users_path: &std::path::Path,
    engine: std::sync::Arc<engine::core::EngineCore>,
) -> std::net::SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// 簡易クエリ（'Q'）1 文を送る（UTF-8 テキストのみを想定。`sql` は呼び出し側の
/// リテラル文字列を渡す前提で、NUL 混入検証は wire-server 側のテストが別途担う）。
pub fn send_simple_query(stream: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_length_prefixed_message(stream, b'Q', &body);
}

/// `RowDescription`（'T'）を読み、列名のリストを返す。
pub fn read_row_description(stream: &mut TcpStream) -> Vec<String> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'T', "expected RowDescription");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut names = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let nul = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .expect("nul-terminated column name");
        let name = std::str::from_utf8(&body[pos..pos + nul])
            .expect("utf8 column name")
            .to_string();
        pos += nul + 1;
        pos += 4 + 2 + 4 + 2 + 4 + 2; // table oid, attnum, type oid, typlen, typmod, format
        names.push(name);
    }
    names
}

/// `DataRow`（'D'）を 1 行読み、各セルを `Option<String>`（`NULL` は `None`）として
/// 返す。
pub fn read_data_row(stream: &mut TcpStream) -> Vec<Option<String>> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'D', "expected DataRow");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut cells = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let cell_len = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        if cell_len < 0 {
            cells.push(None);
            continue;
        }
        let cell_len = cell_len as usize;
        let text = std::str::from_utf8(&body[pos..pos + cell_len])
            .expect("utf8 cell")
            .to_string();
        pos += cell_len;
        cells.push(Some(text));
    }
    cells
}

/// `CommandComplete`（'C'）のタグ文字列を読む。
pub fn read_command_complete(stream: &mut TcpStream) -> String {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'C', "expected CommandComplete");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let end = body.len().saturating_sub(1);
    String::from_utf8_lossy(&body[..end]).to_string()
}

/// `EmptyQueryResponse`（'I'）を読み、body が存在しないこと（length=4）を確認する。
pub fn expect_empty_query_response(stream: &mut TcpStream) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'I', "expected EmptyQueryResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    assert_eq!(
        i32::from_be_bytes(len_buf),
        4,
        "EmptyQueryResponse has no body"
    );
}

/// `ReadyForQuery`（'Z'）を読み切る（`status` バイトの中身は検証しない呼び出し側
/// 向けの簡便ヘルパー）。
pub fn read_ready_for_query(stream: &mut TcpStream) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'Z', "expected ReadyForQuery");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
}
