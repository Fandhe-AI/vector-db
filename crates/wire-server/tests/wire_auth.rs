//! wire-server の結合テスト（TASK-67・対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）。
//!
//! ephemeral port（`127.0.0.1:0`）でサーバースレッドを起動し、`std::net::TcpStream`
//! で生バイトを送受信する自作クライアントを用いる（`psql` 等の外部プロセスは CI に
//! 存在しないため対象外。実機確認は手元検証に委ねる）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wire_server::auth::{argon2id, UserStore};

/// `UserStore::load_from_file` は P0 review 是正により Argon2id パラメータが
/// `argon2id::RECOMMENDED_PARAMS` と完全一致するレコードのみを受理する（既知・
/// 未知ユーザーの KDF コストを常に一致させ、タイミング側チャネルで存在情報が
/// 漏れないようにするため）。したがって結合テストの user store フィクスチャも
/// 軽量パラメータではなく本番既定値をそのまま使う必要がある。
const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

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
            // このヘルパーを使うテストは post-auth アイドルの挙動を検証しないため、
            // 本番既定値をそのまま使う。
            let _ = wire_server::handshake::handle_connection(
                stream,
                &store,
                wire_server::server::POST_AUTH_IDLE_TIMEOUT,
            );
        }
    });

    addr
}

/// `wire_server::server::accept_loop` 経由でサーバースレッドを起動し、接続先アドレス
/// を返す。`spawn_server_accepting_one` と異なり、認証前フェーズ・認証後フェーズの
/// Slowloris 対策（同時接続数上限・I/O タイムアウト）を実際に適用した状態で
/// ハンドシェイクを検証するために使う（review 指摘: 認証前タイムアウトが認証後
/// セッションへ引き継がれないこと・認証後にも別の緩いアイドル期限が働くことの
/// 回帰確認）。
fn spawn_server_with_accept_loop(
    users_path: &std::path::Path,
    max_connections: usize,
    io_timeout: Duration,
    post_auth_idle_timeout: Duration,
) -> std::net::SocketAddr {
    let store = Arc::new(UserStore::load_from_file(users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        wire_server::server::accept_loop(
            listener,
            store,
            max_connections,
            io_timeout,
            post_auth_idle_timeout,
        );
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
    if header[0] != b'R' {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        stream.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        panic!(
            "expected AuthenticationOk ('R'); got message type {:?}, body: {body_str:?}",
            header[0] as char
        );
    }
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
    if header[0] != b'R' {
        // 失敗時は SQLSTATE を含めて診断する（`28P01` なら認証そのものの不一致、
        // `08P01` ならテストクライアント側のフレーミング齟齬で、原因が全く異なる。
        // 後続の flake 調査を「もう一度落として見る」から「1 回のログで切り分ける」
        // に変えるための最小限の追加読み取り）。
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len.saturating_sub(4)];
        stream.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        panic!(
            "authentication must succeed regardless of the self-reported database parameter; \
             got message type {:?}, body: {body_str:?}",
            header[0] as char
        );
    }
}

/// レビュー指摘の再現ケース: `server::accept_loop` の認証前 I/O タイムアウトは
/// StartupMessage を送らない接続には引き続き働くが、認証成功後のセッションには
/// 適用され続けないこと（対話的クライアント・コネクションプールの正当なアイドルを
/// 切断しない）。
#[test]
fn wire_pre_auth_timeout_still_applies_but_post_auth_session_survives_idle() {
    // 認証ハンドシェイク自体がこの区間内（`short_timeout` 未満）で完了する前提の
    // pre-auth タイムアウト。CI・高負荷環境でのスケジューリング遅延を吸収できる
    // 余裕を持たせる（150ms は高負荷時に地の文の read/write が間に合わず
    // 誤ってタイムアウトする flake の実例があったため 300ms に引き上げた）。
    let short_timeout = Duration::from_millis(300);
    // post-auth 用のタイムアウトは、このテストが使うアイドル区間（`short_timeout * 3`）
    // より十分に長く取り、pre-auth タイムアウトが引き継がれていないことだけを検証する
    // （post-auth タイムアウト自体の発火は別テストで検証する）。
    let post_auth_idle_timeout = short_timeout * 10;
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_accept_loop(&users_path, 4, short_timeout, post_auth_idle_timeout);

    // 1) 認証前タイムアウトは引き続き働く: 何も送らない接続は
    //    `short_timeout` 経過後にサーバー側から切断される。
    {
        let mut stalled = TcpStream::connect(addr).expect("connect stalled");
        std::thread::sleep(short_timeout * 3);
        let mut buf = [0u8; 1];
        let n = stalled.read(&mut buf).unwrap_or(0);
        assert_eq!(
            n, 0,
            "pre-auth idle connection must still be closed by the server-side timeout"
        );
    }

    // 2) 認証成功後のセッションは、認証前タイムアウトの何倍もアイドルしても
    //    切断されないこと。アイドル後に簡易クエリを送っても正常に応答が返る
    //    （EOF にならない）ことで生存を確認する。
    {
        let mut stream = TcpStream::connect(addr).expect("connect for auth");
        send_ssl_request_and_startup(&mut stream, "alice", "db");
        let _ = read_auth_request_type(&mut stream);
        send_password_message(&mut stream, "correct-horse");

        let mut header = [0u8; 1];
        stream.read_exact(&mut header).expect("read auth ok type");
        if header[0] != b'R' {
            // `wire2_...` と同様、失敗時は SQLSTATE・本文を含めて診断する
            // （認証タイムアウトの誤発火なら `08P01`、認証そのものの不一致なら
            // `28P01` になるはずで、原因の切り分けに使う）。
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).expect("read len");
            let len = i32::from_be_bytes(len_buf) as usize;
            let mut body = vec![0u8; len.saturating_sub(4)];
            stream.read_exact(&mut body).expect("read body");
            let body_str = String::from_utf8_lossy(&body);
            panic!(
                "expected AuthenticationOk ('R'); got message type {:?}, body: {body_str:?}",
                header[0] as char
            );
        }
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let mut code_buf = [0u8; 4];
        stream.read_exact(&mut code_buf).expect("read code");
        assert_eq!(i32::from_be_bytes(code_buf), 0, "AuthenticationOk expected");

        // ReadyForQuery まで読み飛ばす。
        let mut msg_type = read_message_type_discarding_body(&mut stream);
        let mut safety = 0;
        while msg_type != b'Z' {
            safety += 1;
            assert!(safety < 20, "too many messages before ReadyForQuery");
            msg_type = read_message_type_discarding_body(&mut stream);
        }

        // 認証前タイムアウトの何倍もアイドルする（クリアされていなければ、
        // ここで接続が切断されているはず）。
        std::thread::sleep(short_timeout * 3);

        // アイドル後に簡易クエリを送り、EOF ではなく正常な応答
        // （未実装 ErrorResponse）が返ることで接続が生きていることを確認する。
        let mut query = Vec::new();
        query.push(b'Q');
        let body = b"SELECT 1\0";
        let total_len = (4 + body.len()) as i32;
        query.extend_from_slice(&total_len.to_be_bytes());
        query.extend_from_slice(body);
        stream
            .write_all(&query)
            .expect("send simple query after idling past the pre-auth timeout window");

        let mut resp_header = [0u8; 1];
        stream.read_exact(&mut resp_header).expect(
            "post-auth session must still be open after idling past the pre-auth timeout window",
        );
        assert_eq!(
            resp_header[0], b'E',
            "expected ErrorResponse (simple query not yet implemented), not a closed connection"
        );
    }
}

/// レビュー指摘の再現ケース: 認証成功後のセッションにも `post_auth_idle_timeout`
/// が働き、期限超過で正常にクローズされ接続枠が解放されること（有効な資格情報を
/// 持つクライアントが接続を張ったまま何も送らないことで `max_connections` の枠を
/// 永久占有できないことの回帰確認）。
#[test]
fn wire_post_auth_session_is_closed_and_slot_released_after_idle_timeout() {
    let pre_auth_timeout = Duration::from_secs(5); // このテストでは発火させない
    let post_auth_idle_timeout = Duration::from_millis(150);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr =
        spawn_server_with_accept_loop(&users_path, 1, pre_auth_timeout, post_auth_idle_timeout);

    // 認証を完了させ、枠(1)を占有する。
    let mut authenticated = TcpStream::connect(addr).expect("connect for auth");
    send_ssl_request_and_startup(&mut authenticated, "alice", "db");
    let _ = read_auth_request_type(&mut authenticated);
    send_password_message(&mut authenticated, "correct-horse");

    let mut header = [0u8; 1];
    authenticated
        .read_exact(&mut header)
        .expect("read auth ok type");
    assert_eq!(header[0], b'R');
    let mut len_buf = [0u8; 4];
    authenticated.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    authenticated.read_exact(&mut code_buf).expect("read code");
    assert_eq!(i32::from_be_bytes(code_buf), 0, "AuthenticationOk expected");

    let mut msg_type = read_message_type_discarding_body(&mut authenticated);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(&mut authenticated);
    }

    // 枠(1)を占有した状態で 2 本目を張ると、上限超過で即座にクローズされること
    // （タイムアウト前の基準点。`accept_loop_closes_connection_immediately_when_over_capacity`
    // と同じ確認）。
    {
        let mut second = TcpStream::connect(addr).expect("connect second before timeout");
        let mut buf = [0u8; 1];
        let n = second.read(&mut buf).unwrap_or(0);
        assert_eq!(
            n, 0,
            "slot must still be held by the authenticated session before its idle timeout elapses"
        );
    }

    // 認証済み接続を何も送らずに保持し、post-auth アイドルタイムアウトの経過を待つ。
    std::thread::sleep(post_auth_idle_timeout * 3);
    let mut buf = [0u8; 1];
    let n = authenticated.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "authenticated session must be closed after its idle timeout elapses"
    );

    // 枠が解放されているため、3 本目は即座には閉じられないこと（読み取りが
    // 短いプローブ猶予以内に WouldBlock することで、「即座に EOF」ではなく
    // 「接続を受理して待機している」ことを確認する）。
    let mut third = TcpStream::connect(addr).expect("connect third after timeout");
    third
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set client read timeout");
    let mut buf3 = [0u8; 1];
    let err = third.read(&mut buf3).expect_err("no data sent yet");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::WouldBlock,
        "connection after the post-auth idle timeout must not be closed immediately (slot must be freed)"
    );
}
