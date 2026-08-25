//! PostgreSQL wire プロトコル v3 のハンドシェイク・簡易クエリ最小応答を担う。
//!
//! `main.rs` の接続受け付けループ（`TcpListener` + thread-per-connection）から
//! 1 接続 1 スレッドで [`handle_connection`] が呼ばれる。認証の実照合は
//! `auth::verify` に委譲し、本モジュールはメッセージのフレーミング（読み書き・
//! 長さ検証）と応答メッセージの組み立てに専念する。
//!
//! 受信データ（SSLRequest/StartupMessage/PasswordMessage/簡易クエリ）はすべて
//! untrusted 入力として扱い、`unwrap`/`expect`/添字アクセスを用いず `get()`・
//! `checked_*` で処理する（`.claude/rules/coding-rust.md` P0）。
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）。
//! メッセージ長の上限は本タスクでは暫定値であり、正式な上限体系は TASK-68 で
//! 置換される（ポインタ: `docs/spec/05-tasks.md` TASK-68）。

use std::io::{self, Read, Write};
use std::net::TcpStream;

use crate::auth::{self, UserStore};

/// StartupMessage・SSLRequest 等、認証前の最初のパケットに許す暫定上限バイト数。
/// 実際の StartupMessage は数百バイト程度で足りるため、通常利用を妨げない範囲で
/// 小さく設定する（正式な上限は TASK-68）。
const PROVISIONAL_STARTUP_MAX_LEN: usize = 32 * 1024;

/// PasswordMessage・簡易クエリなど認証後の一般メッセージに許す暫定上限バイト数。
const PROVISIONAL_MESSAGE_MAX_LEN: usize = 64 * 1024;

/// StartupMessage が名乗るべきプロトコルバージョン（3.0 = major 3, minor 0）。
const PROTOCOL_VERSION_3_0: i32 = 0x0003_0000;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const GSSENC_REQUEST_CODE: i32 = 80_877_104;
const CANCEL_REQUEST_CODE: i32 = 80_877_102;

/// SQLSTATE `0A000`（feature_not_supported）。簡易クエリ実行未実装・拡張クエリ
/// プロトコル受信時の応答に用いる。
const SQLSTATE_FEATURE_NOT_SUPPORTED: &str = "0A000";
/// SQLSTATE `08P01`（protocol_violation）。StartupMessage の構文・バージョン不正に
/// 用いる。
const SQLSTATE_PROTOCOL_VIOLATION: &str = "08P01";

#[derive(Debug)]
enum HandshakeError {
    Io(io::Error),
    /// fail-closed に倒すプロトコル違反（詳細は理由をログ用途にのみ保持し、
    /// クライアントへは SQLSTATE 経由の定型メッセージのみ返す）。
    Protocol(&'static str),
}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        HandshakeError::Io(e)
    }
}

/// `handle_connection` の戻り値型（`io::Result<()>`）へ `?` で直接畳み込めるようにする
/// 変換。`Protocol` 側は `io::ErrorKind::InvalidData` に写像し、呼び出し元（`main.rs`）
/// にはログ用途の文字列のみを残す（詳細な違反理由をクライアントへ返すことはない）。
impl From<HandshakeError> for io::Error {
    fn from(e: HandshakeError) -> Self {
        match e {
            HandshakeError::Io(io_err) => io_err,
            HandshakeError::Protocol(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
        }
    }
}

type Result<T> = std::result::Result<T, HandshakeError>;

// ---------------------------------------------------------------------------
// 低レベル読み書きプリミティブ（長さ上限検証つき）
// ---------------------------------------------------------------------------

fn read_i32_be(stream: &mut TcpStream) -> Result<i32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    Ok(i32::from_be_bytes(buf))
}

/// 長さフィールドを検証してからアロケーションする（無制限 `Vec::with_capacity`
/// 禁止。coding-rust.md 準拠）。`len` は呼び出し元が `max` 以内であることを確認済み
/// の値のみを渡す契約。
fn read_exact_bytes(stream: &mut TcpStream, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_all(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    stream.write_all(data)?;
    Ok(())
}

/// 先頭の length(4 バイト, 自身を含む) を読み、`min_total..=max_total` の範囲か
/// 検証したうえで残りのボディを読み取る。SSLRequest/GSSENCRequest/CancelRequest/
/// StartupMessage はいずれもこの共通フレームに従う。
fn read_length_prefixed_body(
    stream: &mut TcpStream,
    min_total: usize,
    max_total: usize,
) -> Result<Vec<u8>> {
    let total_len = read_i32_be(stream)?;
    if total_len < 0 {
        return Err(HandshakeError::Protocol("negative message length"));
    }
    let total_len = total_len as usize;
    if total_len < min_total || total_len > max_total {
        return Err(HandshakeError::Protocol("message length out of bounds"));
    }
    // total_len は length フィールド自身の 4 バイトを含む。
    let body_len = total_len - 4;
    read_exact_bytes(stream, body_len)
}

/// null 終端 C 文字列を `body[*pos..]` から読み取り、UTF-8 として検証する
/// （不正 UTF-8・未終端はいずれも `Err`。添字アクセスは行わず `get()` のみ使う）。
fn read_c_string<'a>(body: &'a [u8], pos: &mut usize) -> Result<&'a str> {
    let start = *pos;
    let rest = body
        .get(start..)
        .ok_or(HandshakeError::Protocol("truncated frame"))?;
    let nul_offset = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(HandshakeError::Protocol("unterminated C string"))?;
    let s = std::str::from_utf8(&rest[..nul_offset])
        .map_err(|_| HandshakeError::Protocol("invalid UTF-8 in C string"))?;
    *pos = start + nul_offset + 1;
    Ok(s)
}

// ---------------------------------------------------------------------------
// 応答メッセージ組み立て
// ---------------------------------------------------------------------------

fn write_ssl_no_response(stream: &mut TcpStream) -> Result<()> {
    write_all(stream, b"N")
}

fn write_authentication_cleartext_password(stream: &mut TcpStream) -> Result<()> {
    // 'R' + length(4) + AuthenticationCleartextPassword コード(4) = 3
    let mut msg = Vec::with_capacity(9);
    msg.push(b'R');
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&3i32.to_be_bytes());
    write_all(stream, &msg)
}

fn write_authentication_ok(stream: &mut TcpStream) -> Result<()> {
    let mut msg = Vec::with_capacity(9);
    msg.push(b'R');
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&0i32.to_be_bytes());
    write_all(stream, &msg)
}

/// BackendKeyData（'K'）: pid・secret key を通知する。CancelRequest 経路は本タスクの
/// スコープ外だが、クライアント実装（psql 等）が本メッセージの到達を前提に
/// StartupMessage 後続シーケンスを進めるため送出する。
fn write_backend_key_data(stream: &mut TcpStream, pid: i32, secret: i32) -> Result<()> {
    let mut msg = Vec::with_capacity(13);
    msg.push(b'K');
    msg.extend_from_slice(&12i32.to_be_bytes());
    msg.extend_from_slice(&pid.to_be_bytes());
    msg.extend_from_slice(&secret.to_be_bytes());
    write_all(stream, &msg)
}

fn write_parameter_status(stream: &mut TcpStream, name: &str, value: &str) -> Result<()> {
    let mut body = Vec::with_capacity(name.len() + value.len() + 2);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'S');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    write_all(stream, &msg)
}

fn write_ready_for_query(stream: &mut TcpStream) -> Result<()> {
    let mut msg = Vec::with_capacity(6);
    msg.push(b'Z');
    msg.extend_from_slice(&5i32.to_be_bytes());
    msg.push(b'I'); // idle（トランザクション外）
    write_all(stream, &msg)
}

/// ErrorResponse（'E'）。SQLSTATE と英語メッセージのみを含む最小フィールド構成
/// （severity 'S'・code 'C'・message 'M' のみ。他テナント・存在情報は含めない）。
fn write_error_response(stream: &mut TcpStream, sqlstate: &str, message: &str) -> Result<()> {
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"ERROR");
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(sqlstate.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message.as_bytes());
    body.push(0);
    body.push(0); // フィールド終端
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'E');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    write_all(stream, &msg)
}

// ---------------------------------------------------------------------------
// StartupMessage 受理・認証・最小クエリループ
// ---------------------------------------------------------------------------

/// SSLRequest/GSSENCRequest には 'N'（非対応）で応答して次のパケットを待ち、
/// CancelRequest は即座に接続を閉じ、それ以外はプロトコルバージョン 3.0 の
/// StartupMessage として `user` パラメータを取り出す（WIRE-1）。
///
/// SSLRequest/GSSENCRequest への応答ループは無制限に繰り返させない
/// （各コードにつき応答は高々 1 回。2 回目以降は fail-closed で拒否し、
/// 無限ループでスレッドを占有させる経路を作らない）。
fn negotiate_startup(stream: &mut TcpStream) -> Result<String> {
    let mut ssl_seen = false;
    let mut gssenc_seen = false;
    loop {
        let body = read_length_prefixed_body(stream, 8, PROVISIONAL_STARTUP_MAX_LEN)?;
        let code_bytes: [u8; 4] = body
            .get(0..4)
            .and_then(|s| s.try_into().ok())
            .ok_or(HandshakeError::Protocol("truncated startup code"))?;
        let code = i32::from_be_bytes(code_bytes);

        match code {
            SSL_REQUEST_CODE if !ssl_seen => {
                ssl_seen = true;
                write_ssl_no_response(stream)?;
                continue;
            }
            GSSENC_REQUEST_CODE if !gssenc_seen => {
                gssenc_seen = true;
                write_ssl_no_response(stream)?;
                continue;
            }
            SSL_REQUEST_CODE | GSSENC_REQUEST_CODE => {
                return Err(HandshakeError::Protocol("repeated SSL/GSSENC negotiation"));
            }
            CANCEL_REQUEST_CODE => {
                return Err(HandshakeError::Protocol(
                    "cancel request (not supported on this path)",
                ));
            }
            PROTOCOL_VERSION_3_0 => {
                return parse_startup_params(&body[4..]);
            }
            _ => {
                return Err(HandshakeError::Protocol("unsupported protocol version"));
            }
        }
    }
}

/// StartupMessage のパラメータ列（null 終端キー・値ペアの繰り返し、空文字列で終端）
/// から `user` を取り出す。`database`/`dbname` を含む他パラメータはテナント決定に
/// 一切使わない（WIRE-2: テナントはユーザーストアからのみ導出する）。
fn parse_startup_params(params_body: &[u8]) -> Result<String> {
    let mut pos = 0usize;
    let mut user: Option<String> = None;
    loop {
        let key = read_c_string(params_body, &mut pos)?;
        if key.is_empty() {
            break;
        }
        let value = read_c_string(params_body, &mut pos)?;
        if key == "user" {
            user = Some(value.to_string());
        }
        // `database`/`dbname` 等その他のパラメータは意図的に読み捨てる
        // （WIRE-2: クライアント自己申告値をテナント決定に使わない）。
    }
    user.ok_or(HandshakeError::Protocol("missing required parameter: user"))
}

/// PasswordMessage（'p'）を読み、末尾 null を除いた生パスワードバイト列を返す。
fn read_password_message(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte)?;
    if type_byte[0] != b'p' {
        return Err(HandshakeError::Protocol("expected PasswordMessage"));
    }
    let body = read_length_prefixed_body(stream, 4, PROVISIONAL_MESSAGE_MAX_LEN)?;
    // body は null 終端 C 文字列 1 個（末尾の 0 を除く）。
    let end = body
        .len()
        .checked_sub(1)
        .ok_or(HandshakeError::Protocol("empty password body"))?;
    if body.get(end) != Some(&0) {
        return Err(HandshakeError::Protocol(
            "password message not null-terminated",
        ));
    }
    Ok(body[..end].to_vec())
}

/// 認証成功後の最小メッセージループ。簡易クエリ（'Q'）には未実装エラー
/// （SQLSTATE `0A000`）を返してループを継続し、Terminate（'X'）で正常終了する。
/// それ以外の型（拡張クエリプロトコル等、TASK-71 管轄）は fail-closed で
/// エラー応答後に接続を切断する。
fn post_auth_loop(stream: &mut TcpStream) -> Result<()> {
    loop {
        let mut type_byte = [0u8; 1];
        match stream.read_exact(&mut type_byte) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }

        match type_byte[0] {
            b'Q' => {
                let _body = read_length_prefixed_body(stream, 4, PROVISIONAL_MESSAGE_MAX_LEN)?;
                write_error_response(
                    stream,
                    SQLSTATE_FEATURE_NOT_SUPPORTED,
                    "simple query execution is not yet implemented",
                )?;
                write_ready_for_query(stream)?;
            }
            b'X' => {
                let _body = read_length_prefixed_body(stream, 4, PROVISIONAL_MESSAGE_MAX_LEN)?;
                return Ok(());
            }
            _ => {
                // 拡張クエリプロトコル等の未対応メッセージ。長さは検証のうえ読み捨て、
                // fail-closed でエラー応答後に切断する（TASK-71 で正式化予定）。
                let _body = read_length_prefixed_body(stream, 4, PROVISIONAL_MESSAGE_MAX_LEN)?;
                write_error_response(
                    stream,
                    SQLSTATE_FEATURE_NOT_SUPPORTED,
                    "message type is not supported on this connection",
                )?;
                return Ok(());
            }
        }
    }
}

/// 1 接続ぶんのハンドシェイク・認証・最小クエリループ全体。`main.rs` の
/// 接続受け付けスレッドから呼ばれる。戻り値の `Err` はネットワーク I/O 異常
/// （クライアント切断等）を表し、呼び出し元はログのみでスレッドを終了してよい
/// （他接続には影響させない）。
pub fn handle_connection(mut stream: TcpStream, store: &UserStore) -> io::Result<()> {
    let username = match negotiate_startup(&mut stream) {
        Ok(u) => u,
        Err(HandshakeError::Io(e)) => return Err(e),
        Err(HandshakeError::Protocol(_)) => {
            // StartupMessage 自体が不正な場合はまだ認証ラウンドに入っていないため、
            // fail-closed で接続を閉じる（詳細メッセージはクライアントへ返さない。
            // WIRE-10 の正式なフレーミング検証は TASK-68 の管轄）。
            let _ = write_error_response(
                &mut stream,
                SQLSTATE_PROTOCOL_VIOLATION,
                "invalid startup packet",
            );
            return Ok(());
        }
    };

    write_authentication_cleartext_password(&mut stream)?;

    let password = match read_password_message(&mut stream) {
        Ok(p) => p,
        Err(HandshakeError::Io(e)) => return Err(e),
        Err(HandshakeError::Protocol(_)) => {
            let _ = write_error_response(
                &mut stream,
                SQLSTATE_PROTOCOL_VIOLATION,
                "invalid password message",
            );
            return Ok(());
        }
    };

    // WIRE-3。verify は 1 回のみ呼び、結果に関わらずここでループへ戻さない。
    match auth::verify(store, &username, &password) {
        Err(_failure) => {
            write_error_response(
                &mut stream,
                auth::SQLSTATE_INVALID_PASSWORD,
                auth::AuthFailure::MESSAGE,
            )?;
            Ok(())
        }
        Ok(_ctx) => {
            // `server::accept_loop` が認証前フェーズの Slowloris 対策として設定した
            // I/O タイムアウトを解除する。認証成功後もタイムアウトを残すと、対話的
            // クライアント・コネクションプールの正当なアイドル（数十秒〜）が切断
            // される（review 指摘）。認証前の接続数上限・タイムアウトによる防御は
            // ここまでで役目を終えており、認証後のセッション生存期間管理
            // （keepalive・アイドルタイムアウト等）は TASK-69（WIRE-8）の管轄とする。
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;

            write_authentication_ok(&mut stream)?;
            // BackendKeyData の値そのものはキャンセル要求の照合以外に使わないため、
            // 暗号学的な強さは要求しない。プロセス ID とプロセス内カウンタで十分。
            let pid = std::process::id() as i32;
            let secret = connection_counter();
            write_backend_key_data(&mut stream, pid, secret)?;
            write_parameter_status(&mut stream, "server_version", "14.0")?;
            write_parameter_status(&mut stream, "client_encoding", "UTF8")?;
            write_ready_for_query(&mut stream)?;

            match post_auth_loop(&mut stream) {
                Ok(()) => Ok(()),
                Err(HandshakeError::Io(e)) => Err(e),
                Err(HandshakeError::Protocol(_)) => {
                    let _ = write_error_response(
                        &mut stream,
                        SQLSTATE_PROTOCOL_VIOLATION,
                        "invalid message frame",
                    );
                    Ok(())
                }
            }
        }
    }
}

/// BackendKeyData の secret フィールド用のプロセス内カウンタ（接続ごとに異なる値を
/// 割り当てるだけの用途で、暗号学的強度は不要）。
fn connection_counter() -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static COUNTER: AtomicI32 = AtomicI32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_startup_params_extracts_user_and_ignores_database() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0alice\0");
        body.extend_from_slice(b"database\0other-tenant\0");
        body.push(0);
        let user = parse_startup_params(&body).expect("valid params");
        assert_eq!(user, "alice");
    }

    #[test]
    fn parse_startup_params_rejects_missing_user() {
        let mut body = Vec::new();
        body.extend_from_slice(b"database\0d\0");
        body.push(0);
        assert!(parse_startup_params(&body).is_err());
    }

    #[test]
    fn parse_startup_params_rejects_unterminated_string() {
        let body = b"user\0alice".to_vec(); // 値が null 終端されていない
        assert!(parse_startup_params(&body).is_err());
    }

    #[test]
    fn parse_startup_params_rejects_invalid_utf8() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0");
        body.push(0xFF);
        body.push(0);
        body.push(0);
        assert!(parse_startup_params(&body).is_err());
    }

    fn write_ssl_request(stream: &mut TcpStream) {
        let mut msg = Vec::new();
        msg.extend_from_slice(&8i32.to_be_bytes());
        msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        stream.write_all(&msg).expect("send SSLRequest");
    }

    /// SSLRequest への応答は各コードにつき高々 1 回。2 回目は無限ループへ入らず
    /// fail-closed で拒否されること（無応答・スレッド占有の回帰確認）。
    #[test]
    fn negotiate_startup_rejects_repeated_ssl_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = negotiate_startup(&mut stream);
            assert!(result.is_err(), "second SSLRequest must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_ssl_request(&mut client);
        let mut resp = [0u8; 1];
        client.read_exact(&mut resp).expect("read first N");
        assert_eq!(&resp, b"N");

        // 2 回目の SSLRequest: 応答を待たず、サーバー側が拒否して接続を閉じる。
        write_ssl_request(&mut client);
        let mut extra = [0u8; 1];
        let n = client.read(&mut extra).unwrap_or(0);
        assert_eq!(
            n, 0,
            "server must close rather than answer a second SSLRequest"
        );

        server.join().expect("server thread must not panic");
    }
}
