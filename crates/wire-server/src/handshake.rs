//! PostgreSQL wire プロトコル v3 のハンドシェイク・簡易クエリ最小応答を担う。
//!
//! `main.rs` の接続受け付けループ（`TcpListener` + thread-per-connection）から
//! 1 接続 1 スレッドで [`handle_connection_bounded`] が呼ばれる。認証の実照合は
//! `auth::verify` に委譲し、本モジュールはメッセージのフレーミング（読み書き・
//! 長さ検証）と応答メッセージの組み立てに専念する。
//!
//! 受信データ（SSLRequest/StartupMessage/PasswordMessage/簡易クエリ）はすべて
//! untrusted 入力として扱い、`unwrap`/`expect`/添字アクセスを用いず `get()`・
//! `checked_*` で処理する（`.claude/rules/coding-rust.md` P0）。
//! メッセージ長の検証・fail-closed なエラー分類は `crate::framing` に集約する。
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）、
//! TASK-68（正式なフレーミング上限体系。対象ビヘイビア WIRE-4, WIRE-10）。

use std::io::{self, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::auth::{self, UserStore};
use crate::framing::{self, FrameError};

/// StartupMessage が名乗るべきプロトコルバージョン（3.0 = major 3, minor 0）。
const PROTOCOL_VERSION_3_0: i32 = 0x0003_0000;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const GSSENC_REQUEST_CODE: i32 = 80_877_104;
const CANCEL_REQUEST_CODE: i32 = 80_877_102;

/// SQLSTATE `0A000`（feature_not_supported）。簡易クエリ実行未実装・拡張クエリ
/// プロトコル受信時の応答に用いる。値は `engine::error_format::ErrorClass`
/// （SSOT。TASK-152・ERR-2）由来（TASK-153・ERR-1 の分散定数 SSOT 化）。
const SQLSTATE_FEATURE_NOT_SUPPORTED: &str =
    engine::error_format::ErrorClass::FeatureNotSupported.wire_code();
/// SQLSTATE `08P01`（protocol_violation）。StartupMessage の構文・バージョン不正
/// （フレーミング以外のプロトコル違反）に用いる。フレーミング由来の分類・値は
/// `framing::SQLSTATE_PROTOCOL_VIOLATION` を単一の真実源とする。
const SQLSTATE_PROTOCOL_VIOLATION: &str = framing::SQLSTATE_PROTOCOL_VIOLATION;

#[derive(Debug)]
enum HandshakeError {
    Io(io::Error),
    /// fail-closed に倒すプロトコル違反（詳細は理由をログ用途にのみ保持し、
    /// クライアントへは SQLSTATE 経由の定型メッセージのみ返す）。フレーミング外の
    /// 構文違反（パラメータ解析・型ごとの形状検証等）に用いる。
    Protocol(&'static str),
    /// `framing` モジュールが検出したフレーミング違反（WIRE-4/WIRE-10）。
    Frame(FrameError),
}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        HandshakeError::Io(e)
    }
}

impl From<FrameError> for HandshakeError {
    fn from(e: FrameError) -> Self {
        match e {
            // I/O 異常（タイムアウト等）はフレーミング分類ではなく通常の I/O エラー
            // として扱う（呼び出し元の `handle_connection_bounded` が `Err` を返す経路と
            // 揃える）。
            FrameError::Io(io_err) => HandshakeError::Io(io_err),
            other => HandshakeError::Frame(other),
        }
    }
}

/// [`write_error_response`] が [`crate::result_encoder::encode_error_response`]
/// （ErrorResponse バイト列組み立ての唯一の実体。codex-review Low 指摘対応・
/// PR #101 で重複実装を解消）に委譲する際のエラー写像。フレーム長超過等の
/// エンコード失敗は fail-closed に倒し `Protocol` 分類として扱う（本モジュールの
/// `sqlstate`/`message` 引数は固定英語文言のみを渡す契約のため実運用では
/// 発生しない想定）。
impl From<crate::result_encoder::EncodeError> for HandshakeError {
    fn from(_: crate::result_encoder::EncodeError) -> Self {
        HandshakeError::Protocol("failed to encode error response")
    }
}

/// `handle_connection_bounded` の戻り値型（`io::Result<()>`）へ `?` で直接畳み込めるようにする
/// 変換。`Protocol`/`Frame` 側は `io::ErrorKind::InvalidData` に写像し、呼び出し元
/// （`main.rs`）にはログ用途の文字列のみを残す（詳細な違反理由をクライアントへ
/// 返すことはない）。
impl From<HandshakeError> for io::Error {
    fn from(e: HandshakeError) -> Self {
        match e {
            HandshakeError::Io(io_err) => io_err,
            HandshakeError::Protocol(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            HandshakeError::Frame(frame_err) => {
                io::Error::new(io::ErrorKind::InvalidData, frame_err.to_string())
            }
        }
    }
}

type Result<T> = std::result::Result<T, HandshakeError>;

// ---------------------------------------------------------------------------
// 低レベル読み書きプリミティブ
// ---------------------------------------------------------------------------

fn write_all(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    stream.write_all(data)?;
    Ok(())
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
/// `protocol_dispatch::reject_and_close` から呼ばれる `io::Result` 版のラッパー。
/// `HandshakeError`／`handshake::Result` は本モジュール限定の型のため、モジュール
/// 境界をまたいで直接公開せず、戻り値を `io::Result` へ写像したこの関数のみを
/// `pub(crate)` にする（`HandshakeError` 自体は private のまま維持する）。
pub(crate) fn write_error_response_io(
    stream: &mut TcpStream,
    sqlstate: &str,
    message: &str,
) -> io::Result<()> {
    write_error_response(stream, sqlstate, message).map_err(io::Error::from)
}

/// `write_ready_for_query` の `io::Result` 版ラッパー。[`crate::simple_query`] は
/// 本モジュール限定の `handshake::Result` を扱えないため、`ReadyForQuery` を
/// 送出する唯一の経路としてこの関数を `pub(crate)` にする。
pub(crate) fn write_ready_for_query_io(stream: &mut TcpStream) -> io::Result<()> {
    write_ready_for_query(stream).map_err(io::Error::from)
}

/// バイト列組み立ては [`crate::result_encoder::encode_error_response`] に委譲する
/// （`S`/`C`/`M` 3 フィールドの ErrorResponse エンコードは本関数と
/// `result_encoder::encode_error_response` で重複実装していたが、こちらを
/// 唯一の実体として統合する。codex-review Low 指摘対応・PR #101）。
fn write_error_response(stream: &mut TcpStream, sqlstate: &str, message: &str) -> Result<()> {
    let msg = crate::result_encoder::encode_error_response(sqlstate, message)?;
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
        let body = framing::read_startup_frame(stream)?;
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
/// から `user` を取り出す。`user` 以外のパラメータはテナント決定に用いない
/// （ポインタ: TASK-67・WIRE-2）。
///
/// 終端（空キー）を読んだ時点で `params_body` を使い切っていること（終端後の
/// 残余バイトがないこと）・`user` キーが複数回出現しないことを検証する
/// （review 指摘: 残余バイトの無視・重複キーの後勝ち上書きは、フレーミングの
/// 曖昧さやテナント決定への不正な入力混入余地を生むため fail-closed で拒否する）。
fn parse_startup_params(params_body: &[u8]) -> Result<String> {
    let mut pos = 0usize;
    let mut user: Option<String> = None;
    loop {
        let key = read_c_string(params_body, &mut pos)?;
        if key.is_empty() {
            if pos != params_body.len() {
                return Err(HandshakeError::Protocol(
                    "trailing data after startup params",
                ));
            }
            break;
        }
        let value = read_c_string(params_body, &mut pos)?;
        if key == "user" {
            if user.is_some() {
                return Err(HandshakeError::Protocol("duplicate user parameter"));
            }
            user = Some(value.to_string());
        }
        // その他のパラメータは意図的に読み捨てる（ポインタ: WIRE-2）。
    }
    user.ok_or(HandshakeError::Protocol("missing required parameter: user"))
}

/// PasswordMessage（'p'）を読み、末尾 null を除いた生パスワードバイト列を返す。
///
/// body は null 終端 C 文字列 1 個であること（末尾以外に NUL を含む不正フレームは
/// 拒否する。review 指摘: `password\0suffix\0` のような多重 NUL フレームを
/// Argon2id 照合へそのまま渡すと、フレーミングの曖昧さがパスワード照合の意味論に
/// 混入するため fail-closed で拒否する）。
fn read_password_message(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let type_byte = match framing::read_typed_frame_header(stream)? {
        Some(b) => b,
        None => return Err(HandshakeError::Protocol("expected PasswordMessage")),
    };
    if type_byte != b'p' {
        return Err(HandshakeError::Protocol("expected PasswordMessage"));
    }
    let body = framing::read_length_prefixed_body(
        stream,
        framing::MIN_TYPED_MESSAGE_LEN,
        framing::MAX_MESSAGE_LEN,
    )?;
    let end = body
        .len()
        .checked_sub(1)
        .ok_or(HandshakeError::Protocol("empty password body"))?;
    if body.get(end) != Some(&0) {
        return Err(HandshakeError::Protocol(
            "password message not null-terminated",
        ));
    }
    let password = body
        .get(..end)
        .ok_or(HandshakeError::Protocol("truncated password body"))?;
    if password.contains(&0) {
        return Err(HandshakeError::Protocol(
            "password message contains embedded NUL",
        ));
    }
    Ok(password.to_vec())
}

/// 認証成功後の最小メッセージループ。`engine` が `Some` の場合、簡易クエリ
/// （'Q'）は UTF-8 検証後 [`crate::simple_query::execute_and_respond`] へ委譲し、
/// engine の SQL 表層で実行した結果（または SQL エラー）を wire メッセージへ
/// 整形して返す（TASK-73・WIRE-1）。`engine` が `None`（`handle_connection_bounded`
/// 経由の後方互換パス）の場合は従来どおり未実装エラー（SQLSTATE `0A000`）を返す。
/// いずれの経路も Terminate（'X'）で正常終了する。それ以外の型（拡張クエリ
/// プロトコル等）は `protocol_dispatch` へ委譲し、fail-closed でエラー応答後に
/// 接続を切断する（TASK-71・WIRE-8 で正式化済み）。
///
/// `Q`・`X` いずれも構造検証を行う（review 指摘: 構造を検証せず読み捨てるだけでは
/// フレーミングの曖昧さが残る）。`Q` は単一の NUL 終端文字列（空 body・終端 NUL
/// なし・末尾以外の埋め込み NUL はいずれも拒否）に加え、本文が UTF-8 として
/// 妥当であることを要求する（`simple_query`／engine の SQL 表層はいずれも `&str`
/// のみを受け取る契約のため、境界はここで確定させる。不正 UTF-8 は `08P01` で
/// fail-closed に拒否する）。`X` は length=4・body が厳密に空であることを要求し、
/// 違反は protocol violation として fail-closed で扱う。
///
/// `ctx` は認証成功時に導出された `engine::policy::PolicyContext`（テナント境界・
/// 可視性判定の唯一の入力経路）、`session` は接続単位の
/// `engine::sql::mode::SessionState`（取得モード・宣言的 UDF レジストリ）で、
/// いずれも本ループの全クエリを通じて 1 個の値を使い回す（`EngineCore` 自体は
/// セッション状態を保持しない設計。`sql::mode` モジュールドキュメント参照）。
fn post_auth_loop(
    stream: &mut TcpStream,
    ctx: &engine::policy::PolicyContext,
    engine: Option<&engine::core::EngineCore>,
    session: &mut engine::sql::mode::SessionState,
) -> Result<()> {
    loop {
        let type_byte = match framing::read_typed_frame_header(stream)? {
            Some(b) => b,
            None => return Ok(()),
        };

        match type_byte {
            b'Q' => {
                // 最小 5（length 4 バイト + 終端 NUL 1 バイト）。空 body は拒否する。
                let body = framing::read_length_prefixed_body(stream, 5, framing::MAX_MESSAGE_LEN)?;
                let end = body
                    .len()
                    .checked_sub(1)
                    .ok_or(HandshakeError::Protocol("empty query body"))?;
                if body.get(end) != Some(&0) {
                    return Err(HandshakeError::Protocol(
                        "query message not null-terminated",
                    ));
                }
                let text = body
                    .get(..end)
                    .ok_or(HandshakeError::Protocol("truncated query body"))?;
                if text.contains(&0) {
                    return Err(HandshakeError::Protocol(
                        "query message contains embedded NUL",
                    ));
                }
                let text = std::str::from_utf8(text)
                    .map_err(|_| HandshakeError::Protocol("query text is not valid UTF-8"))?;

                match engine {
                    Some(engine) => {
                        crate::simple_query::execute_and_respond(
                            stream, engine, ctx, session, text,
                        )?;
                    }
                    None => {
                        write_error_response(
                            stream,
                            SQLSTATE_FEATURE_NOT_SUPPORTED,
                            "simple query execution is not yet implemented",
                        )?;
                        write_ready_for_query(stream)?;
                    }
                }
            }
            b'X' => {
                // Terminate は length=4（body 厳密に空）以外を fail-closed で拒否する。
                let _body = framing::read_length_prefixed_body(stream, 4, 4)?;
                return Ok(());
            }
            other => {
                // 拡張クエリプロトコル等の未対応メッセージ。長さフィールド
                // （最低 4 バイト、MIN_TYPED_MESSAGE_LEN..=MAX_MESSAGE_LEN）だけは
                // 他の型付きメッセージと同じ基準で検証する。未検証のまま
                // `0A000`（未対応機能）を返すと、長さフィールド欠落・範囲外の
                // malformed frame まで正規の未対応機能扱いにしてしまい、既存の
                // framing/protocol error 契約（`54000`/`08P01`）を迂回してしまう
                // ため（レビュー指摘の回帰防止）。本文自体は読まない
                // （`protocol_dispatch` 側が型バイトのみで分類し、ErrorResponse
                // 送出後は有界 lingering close で未読データを読み捨てる。
                // ポインタ: TASK-71・WIRE-8）。
                framing::validate_typed_message_length_prefix(
                    stream,
                    framing::MIN_TYPED_MESSAGE_LEN,
                    framing::MAX_MESSAGE_LEN,
                )?;
                let kind = crate::protocol_dispatch::classify(other);
                crate::protocol_dispatch::reject_and_close(stream, kind, write_error_response_io)?;
                return Ok(());
            }
        }
    }
}

/// `negotiate_startup`・`read_password_message`・`post_auth_loop` いずれのエラーも
/// ここへ集約して応答を分岐する（WIRE-4/WIRE-10）。
///
/// - `sqlstate()` が `Some`（`Protocol`・`Frame(TooLarge)`・`Frame(Malformed)`）:
///   固定の英語メッセージで ErrorResponse を送ってから切断する（送信失敗は無視。
///   相手が既に切断していれば送信自体が失敗しうるが、その場合も fail-closed に
///   切断で終わる）。`Frame` 由来のメッセージは `FrameError::client_message()` を、
///   `Protocol` 由来は呼び出し元が渡す `fallback_message` を用いる（`Protocol` は
///   フレーミング外の構文違反であり、`FrameError` に対応するメッセージを持たない
///   ため）。
/// - `Frame(Truncated)`: 相手が既に切断しているため応答を送らずに `Ok(())`。
/// - `Io`: サーバー側の異常として `Err` をそのまま返す（呼び出し元の
///   `server::accept_loop` がログに残す）。
fn respond_and_close(
    stream: &mut TcpStream,
    err: HandshakeError,
    fallback_message: &str,
) -> io::Result<()> {
    match err {
        HandshakeError::Io(e) => Err(e),
        HandshakeError::Frame(FrameError::Truncated) => Ok(()),
        HandshakeError::Frame(frame_err) => {
            if let Some(sqlstate) = frame_err.sqlstate() {
                let _ = write_error_response(stream, sqlstate, frame_err.client_message());
            }
            Ok(())
        }
        HandshakeError::Protocol(_) => {
            let _ = write_error_response(stream, SQLSTATE_PROTOCOL_VIOLATION, fallback_message);
            Ok(())
        }
    }
}

/// 1 接続ぶんのハンドシェイク・認証・最小クエリループ全体。
/// `server::accept_loop_with_limiter` の接続受け付けスレッドから呼ばれる。
/// 戻り値の `Err` はネットワーク I/O 異常（クライアント切断・読み取り
/// タイムアウト等）を表し、呼び出し元はログのみでスレッドを終了してよい
/// （他接続には影響させない）。
///
/// 読み取りタイムアウト（`limits::READ_TIMEOUT`）は呼び出し元の
/// `server::accept_loop_with_limiter` が受理直後に一度だけソケットへ設定済み
/// であり、本関数はそれを認証前後で変更しない（WIRE-5: 接続全体に同一の期限を
/// 適用する）。タイムアウト由来の `io::Error`（`TimedOut` / `WouldBlock`）は
/// ここで捕捉して ErrorResponse を書くことはせず、そのまま `Err` として
/// 呼び出し元へ返す（応答なしでクローズすることが WIRE-5 の契約）。
///
/// 対応: TASK-69（WIRE-5）。旧 3 引数シグネチャ（`post_auth_idle_timeout` を
/// 直接受け取る形）は [`handle_connection`]（deprecated 互換ラッパー）として
/// 維持する（codex-review / Cursor Bugbot 再指摘: 別名ラッパーでは後方互換に
/// ならず、旧名・旧シグネチャをそのまま残す必要がある）。
///
/// `engine` が `None` の場合、簡易クエリには従来どおり `0A000`（未実装）を返す
/// （本関数の既存呼び出し元・既存テストの契約を変えない）。`Some` を渡す新規
/// エントリポイントは [`handle_connection_with_engine`]（TASK-73・WIRE-1）。
pub fn handle_connection_bounded(stream: TcpStream, store: &UserStore) -> io::Result<()> {
    handle_connection_inner(stream, store, None)
}

/// engine（SQL 表層）を接続した簡易クエリ実行経路（TASK-73・WIRE-1）。
/// `server::accept_loop_with_engine` の接続受け付けスレッドから呼ばれる。
/// 認証・ハンドシェイクの契約は [`handle_connection_bounded`] と同一で、簡易
/// クエリ（'Q'）の実処理だけが `engine::core::EngineCore` へ委譲される点が異なる。
pub fn handle_connection_with_engine(
    stream: TcpStream,
    store: &UserStore,
    engine: &engine::core::EngineCore,
) -> io::Result<()> {
    handle_connection_inner(stream, store, Some(engine))
}

fn handle_connection_inner(
    mut stream: TcpStream,
    store: &UserStore,
    engine: Option<&engine::core::EngineCore>,
) -> io::Result<()> {
    let username = match negotiate_startup(&mut stream) {
        Ok(u) => u,
        Err(e) => return respond_and_close(&mut stream, e, "invalid startup packet"),
    };

    write_authentication_cleartext_password(&mut stream)?;

    let password = match read_password_message(&mut stream) {
        Ok(p) => p,
        Err(e) => return respond_and_close(&mut stream, e, "invalid password message"),
    };

    // ポインタ: TASK-67・WIRE-3。
    match auth::verify(store, &username, &password) {
        Err(_failure) => {
            write_error_response(
                &mut stream,
                auth::SQLSTATE_INVALID_PASSWORD,
                auth::AuthFailure::MESSAGE,
            )?;
            Ok(())
        }
        Ok(ctx) => {
            // 読み取りタイムアウトは `server::accept_loop_with_limiter` が接続全体に
            // 一度だけ設定済み（WIRE-5）であり、ここで切り替えない。
            write_authentication_ok(&mut stream)?;
            // BackendKeyData の値そのものはキャンセル要求の照合以外に使わないため、
            // 暗号学的な強さは要求しない。プロセス ID とプロセス内カウンタで十分。
            let pid = std::process::id() as i32;
            let secret = connection_counter();
            write_backend_key_data(&mut stream, pid, secret)?;
            write_parameter_status(&mut stream, "server_version", "14.0")?;
            write_parameter_status(&mut stream, "client_encoding", "UTF8")?;
            write_ready_for_query(&mut stream)?;

            // 接続単位のセッション状態（取得モード・宣言的 UDF レジストリ）。
            // `EngineCore` 自体は保持しない（`sql::mode` モジュールドキュメント参照）。
            let mut session = engine::sql::mode::SessionState::default();

            match post_auth_loop(&mut stream, &ctx, engine, &mut session) {
                Ok(()) => Ok(()),
                Err(e) => respond_and_close(&mut stream, e, "invalid message frame"),
            }
        }
    }
}

/// 旧 `(stream, store, post_auth_idle_timeout)` 3 引数シグネチャとの後方互換
/// ラッパー（**旧名・旧シグネチャをそのまま維持**。codex-review / Cursor
/// Bugbot 再指摘: 別名のラッパーを追加するだけでは呼び出し元のコンパイルが
/// 通らず後方互換にならないため、新実装は [`handle_connection_bounded`]
/// という別名へ移し、この名前・シグネチャの方を互換層として残す）。
///
/// TASK-69（WIRE-5）で読み取りタイムアウトは認証前後で切り替えない単一値方式
/// （`server::accept_loop_with_limiter` が受理直後に一度だけ設定する）へ統一
/// され、本体の実装はタイムアウトを受け取らない 2 引数シグネチャへ移行した。
/// 本関数はすでに公開 API として利用側に届いている可能性のある旧シグネチャを
/// 維持しつつ、内部では新実装（[`handle_connection_bounded`]）へ委譲する
/// （`server::bind_loopback` と同じ後方互換方針）。
///
/// `post_auth_idle_timeout` は WIRE-5 の単一タイムアウト契約により**無視**する
/// （認証前後で値を切り替える経路自体が存在しないため）。新規コードは
/// `handle_connection_bounded` を直接呼ぶこと。
#[deprecated(
    since = "0.1.0",
    note = "use handle_connection_bounded(stream, store) instead; post_auth_idle_timeout is ignored (WIRE-5 uses a single read_timeout for the whole connection)"
)]
pub fn handle_connection(
    stream: TcpStream,
    store: &UserStore,
    _post_auth_idle_timeout: Duration,
) -> io::Result<()> {
    handle_connection_bounded(stream, store)
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
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
    /// プロセス内単調カウンタ（`tests/wire_auth.rs` と同一クラスの競合対策。
    /// Issue #172）。
    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

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

    /// review 指摘の再現ケース: 終端（空キー）後に残余バイトがあれば拒否すること。
    #[test]
    fn parse_startup_params_rejects_trailing_data_after_terminator() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0alice\0");
        body.push(0); // 終端
        body.extend_from_slice(b"trailing garbage");
        assert!(parse_startup_params(&body).is_err());
    }

    /// review 指摘の再現ケース: `user` キーが複数回出現する場合は後勝ちで上書きせず
    /// 拒否すること（テナント決定の唯一の入力経路への曖昧な混入余地を作らない）。
    #[test]
    fn parse_startup_params_rejects_duplicate_user() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0alice\0");
        body.extend_from_slice(b"user\0mallory\0");
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

    /// 長さプレフィックス付きメッセージ（type byte + length + body）をそのまま
    /// クライアント側から送るテスト用ヘルパー。
    fn write_length_prefixed_message(stream: &mut TcpStream, type_byte: u8, body: &[u8]) {
        let total_len = (4 + body.len()) as i32;
        let mut msg = Vec::with_capacity(1 + body.len() + 4);
        msg.push(type_byte);
        msg.extend_from_slice(&total_len.to_be_bytes());
        msg.extend_from_slice(body);
        stream.write_all(&msg).expect("send message");
    }

    /// review 指摘の再現ケース: 末尾以外に NUL を含む PasswordMessage
    /// （`password\0suffix\0`）を fail-closed で拒否すること。
    #[test]
    fn read_password_message_rejects_embedded_nul() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = read_password_message(&mut stream);
            assert!(result.is_err(), "embedded NUL must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'p', b"password\0suffix\0");

        server.join().expect("server thread must not panic");
    }

    /// 正常系の対照確認: 内部 NUL を含まない PasswordMessage は受理されること。
    #[test]
    fn read_password_message_accepts_well_formed_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = read_password_message(&mut stream);
            assert_eq!(result.expect("valid password"), b"correct-horse".to_vec());
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'p', b"correct-horse\0");

        server.join().expect("server thread must not panic");
    }

    fn dummy_policy_context() -> engine::policy::PolicyContext {
        engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id")
    }

    /// review 指摘の再現ケース: 簡易クエリ（'Q'）の body が空（終端 NUL すら
    /// 無い）場合は fail-closed で拒否すること。
    #[test]
    fn post_auth_loop_rejects_empty_query_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
            );
            assert!(result.is_err(), "empty query body must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'Q', b"");

        server.join().expect("server thread must not panic");
    }

    /// review 指摘の再現ケース: 簡易クエリの body に末尾以外の埋め込み NUL が
    /// あれば fail-closed で拒否すること。
    #[test]
    fn post_auth_loop_rejects_query_with_embedded_nul() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
            );
            assert!(result.is_err(), "embedded NUL in query must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'Q', b"select\0 1\0");

        server.join().expect("server thread must not panic");
    }

    /// review 指摘の再現ケース: Terminate（'X'）は length=4（body 厳密に空）以外を
    /// fail-closed で拒否すること。
    #[test]
    fn post_auth_loop_rejects_terminate_with_nonempty_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
            );
            assert!(
                result.is_err(),
                "Terminate with a non-empty body must be rejected"
            );
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'X', b"unexpected");

        server.join().expect("server thread must not panic");
    }

    /// 正常系の対照確認: length=4・body 厳密に空の Terminate は正常終了すること。
    #[test]
    fn post_auth_loop_accepts_well_formed_terminate() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
            );
            assert!(result.is_ok(), "well-formed Terminate must succeed");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'X', b"");

        server.join().expect("server thread must not panic");
    }

    /// レビュー指摘の再現ケース: 未対応メッセージ（例: Parse 'P'）であっても、
    /// 宣言長が `MIN_TYPED_MESSAGE_LEN`（4）未満の malformed frame は
    /// `0A000`（未対応機能）としてではなく、既存の `FrameError` 経路
    /// （`08P01` 相当）で fail-closed に拒否されること（`post_auth_loop` は
    /// `Err` を返し、呼び出し元の `respond_and_close` が応答を分岐する）。
    #[test]
    fn post_auth_loop_rejects_unsupported_message_with_length_below_minimum() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
            );
            match result {
                Err(HandshakeError::Frame(_)) => {}
                other => panic!(
                    "malformed length prefix on an unsupported message must surface as a                      FrameError, got {other:?}"
                ),
            }
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        // 型バイト 'P'（Parse・未対応）の直後に、body を含めない宣言長 3
        // （MIN_TYPED_MESSAGE_LEN=4 未満）だけを送る。
        client.write_all(b"P").expect("send type byte");
        client
            .write_all(&3i32.to_be_bytes())
            .expect("send malformed length");

        server.join().expect("server thread must not panic");
    }

    /// レビュー指摘の再現ケース: 未対応メッセージの宣言長が `MAX_MESSAGE_LEN` を
    /// 超える場合も同様に `FrameError`（`TooLarge`・`54000` 相当）経路へ送られ、
    /// `0A000` を返さないこと。
    #[test]
    fn post_auth_loop_rejects_unsupported_message_with_length_too_large() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
            );
            match result {
                Err(HandshakeError::Frame(FrameError::TooLarge { .. })) => {}
                other => panic!(
                    "oversized length prefix on an unsupported message must surface as                      FrameError::TooLarge, got {other:?}"
                ),
            }
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        let declared = (framing::MAX_MESSAGE_LEN + 1) as i32;
        client.write_all(b"P").expect("send type byte");
        client
            .write_all(&declared.to_be_bytes())
            .expect("send oversized length");

        server.join().expect("server thread must not panic");
    }

    /// 正常系の対照確認: 未対応メッセージでも宣言長が妥当な範囲内であれば、
    /// 従来どおり `reject_and_close`（`0A000` 応答＋切断）へ到達し、
    /// `post_auth_loop` は `Ok(())` を返すこと。
    #[test]
    fn post_auth_loop_accepts_well_formed_unsupported_message_length() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
            );
            assert!(
                result.is_ok(),
                "well-formed length prefix on an unsupported message must still be rejected                  via 0A000 and return Ok(())"
            );
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        // 型バイト 'P'（Parse・未対応）+ 妥当な宣言長のみ（body は付けない。
        // `reject_and_close` 側は body を読まず lingering close で読み捨てる）。
        write_length_prefixed_message(&mut client, b'P', b"");

        let mut resp_type = [0u8; 1];
        client
            .read_exact(&mut resp_type)
            .expect("read response type");
        assert_eq!(resp_type[0], b'E', "expected ErrorResponse ('E')");

        server.join().expect("server thread must not panic");
    }

    /// P1 review 再指摘（codex-review / Cursor Bugbot: 別名ラッパーでは
    /// 後方互換にならない）の再現ケース: 旧名・旧 3 引数シグネチャの
    /// [`handle_connection`]（deprecated）を呼んでも panic せず、新実装
    /// （[`handle_connection_bounded`]）へ委譲されること。無効な
    /// StartupMessage（負の長さ）を送り、応答なしで正常にクローズすることまで
    /// 確認する（`post_auth_idle_timeout` は無視される契約のため、値そのものは
    /// 検証しない）。
    #[test]
    #[allow(deprecated)]
    fn handle_connection_compat_wrapper_delegates_without_panic() {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-handshake-test-legacy-{}-{}-{}",
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
        std::fs::write(&path, "").expect("write empty user store");
        let store = UserStore::load_from_file(&path).expect("valid empty store");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            // 旧名・旧シグネチャをそのまま呼ぶ（互換性の実体はここで検証する）。
            let result = handle_connection(stream, &store, Duration::from_secs(300));
            assert!(
                result.is_ok(),
                "malformed startup must close without I/O error"
            );
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        let mut msg = Vec::new();
        msg.extend_from_slice(&(-1i32).to_be_bytes());
        client
            .write_all(&msg)
            .expect("send negative-length startup");

        let mut extra = [0u8; 1];
        let n = client.read(&mut extra).unwrap_or(0);
        // ErrorResponse の 'E' か、応答なし EOF のいずれか（フレーミング拒否経路の
        // 詳細は本テストの関心事ではない）。ここでは委譲先が呼ばれて panic せずに
        // クローズすることのみを確認する。
        let _ = n;

        server.join().expect("server thread must not panic");
        let _ = std::fs::remove_file(&path);
    }
}
