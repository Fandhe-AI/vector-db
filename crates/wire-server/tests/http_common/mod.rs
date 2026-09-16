//! HTTP 表層（`--surface nosql`）の結合テスト共通ヘルパー（Issue #748・
//! TASK-173／HTTP-2, HTTP-3, HTTP-11）。
//!
//! `tests/common/mod.rs` は pg wire（SSLRequest／Startup）を前提にした
//! ヘルパー群であり、HTTP 表層のテスト（本ファイル・`http2_framing.rs`・
//! `http3_content_type.rs`・`http11_limits.rs`、および以後 HTTP 表層を扱う
//! テスト全般）には流用できない。本モジュールはそれらが共通して必要とする
//! 「リスナー起動・生バイト送受信・応答エンベロープ解析・`wire_code`／
//! `message` 抽出・拒否応答のアサーション」を 1 箇所へ集約する
//! （`#[path = "http_common/mod.rs"] mod http_common;` で include する）。
//!
//! 到達できる唯一の production 入口は
//! [`wire_server::http::listener::accept_loop_with_limiter`] であり、これは
//! 内部で [`wire_server::http::conn::PlaceholderRouter`]（`pub(crate)`。
//! 本 Issue 時点は全パスを `08P01`／`"unknown request target"` で拒否する
//! 実ルータ未実装の placeholder。実ルータは Issue #758）を固定で使う。
//! したがって「要求が受理されパースを通ってハンドラへ到達した」ことは、
//! 現状では常にこの固定応答としてしか観測できない。[`assert_reached_router`]
//! はその観測境界を 1 箇所へ閉じ込めており、実ルータ置き換え時に更新する
//! 箇所はここだけになる。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-2,
//! HTTP-3, HTTP-11）。
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use engine::json::{parse_json, JsonValue};
use wire_server::limits::ConnectionLimiter;

/// クライアント側ソケットの読み取り／書き込みタイムアウト。
///
/// production の既定（`crate::limits::READ_TIMEOUT`＝30 秒）を待つ必要はなく、
/// テストの意図（不正フレーム・上限超過の即時拒否）に対して十分に大きい
/// 短縮値を使う。
pub const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// サーバー側（[`spawn_http_listener`] へ渡す `read_timeout`）の既定値。
/// [`crate::http::conn::handle_connection_with`] の `request_read_deadline`
/// としても使われる（`accept_loop_with_limiter` が同じ値を両方へ渡す契約）。
pub const SERVER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// [`send_raw`] が読み取る応答バイト列の総量上限。無制限 `Vec` 伸長を避ける
/// （untrusted な相手からの応答を読むテストヘルパー自身も無制限確保をしない）。
const MAX_RECEIVE_BYTES: usize = 2 * 1024 * 1024;

/// `wire_server::http::listener::accept_loop_with_limiter`（production 入口。
/// 内部で [`wire_server::http::conn::PlaceholderRouter`] を固定で使う）を
/// サーバースレッドで起動し、`(接続先アドレス, リミッターのクローン)` を返す。
///
/// `tests/http_limits.rs::spawn_http_server` と同じ流儀（Issue #743）。
pub fn spawn_http_listener(
    max_connections: usize,
    read_timeout: Duration,
) -> (SocketAddr, ConnectionLimiter) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);
    let limiter_for_loop = limiter.clone();

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_limiter(
            listener,
            limiter_for_loop,
            read_timeout,
        );
    });

    (addr, limiter)
}

/// 応答受信後にクライアントがソケットへ対して何をするか。
///
/// `HalfClose`（既定・推奨）は書き込み側を即座に閉じ、サーバーの
/// `drain_and_close` が即座に EOF へ到達できるようにする（drain の
/// `LINGER_DRAIN_TIMEOUT`＝1 秒待ちを避け、テストを高速に保つ）。
/// `KeepOpen` は「クライアントが接続を切らなくてもサーバー側の判断だけで
/// 応答＋クローズへ到達すること」を確認したいケース（本文読み取り前の
/// 上限拒否・パイプライン・本文長超過等）でのみ使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterWrite {
    HalfClose,
    KeepOpen,
}

/// `addr` へ接続し `request` を書き込んでから、[`AfterWrite`] に応じて
/// 書き込み側を閉じ（またはそのまま）、EOF まで応答バイト列を読み取って返す。
///
/// 終端は `Ok(0)`（クリーンな EOF）を正とする。サーバー側が未読データを
/// 残したまま `close` する経路（drain 予算超過・タイムアウト）では OS が
/// close を RST として観測しうる（`crate::http::conn` 自身の単体テスト
/// `assert_closed_without_hanging` と同じ事情）ため、少なくとも 1 バイト
/// 受信済みであれば `ConnectionReset` も終端として許容する。`WouldBlock`／
/// `TimedOut`（クライアント側の読み取りタイムアウト超過）は許容せず
/// 明示的な失敗とする（「サーバーが応答してクローズする」契約が壊れていても
/// テストが通ってしまう事態を避けるため。`tests/http_limits.rs::assert_eof`
/// と同じ codex-review 由来の方針）。
pub fn send_raw(addr: SocketAddr, request: &[u8], after: AfterWrite) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect to http listener");
    stream
        .set_read_timeout(Some(CLIENT_READ_TIMEOUT))
        .expect("set client read timeout");
    stream
        .set_write_timeout(Some(CLIENT_READ_TIMEOUT))
        .expect("set client write timeout");
    stream.write_all(request).expect("write request bytes");
    if after == AfterWrite::HalfClose {
        stream
            .shutdown(Shutdown::Write)
            .expect("half-close write side after sending request");
    }

    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                assert!(
                    received.len() <= MAX_RECEIVE_BYTES,
                    "response exceeded test receive cap of {MAX_RECEIVE_BYTES} bytes"
                );
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset && !received.is_empty() => break,
            Err(e) => panic!("unexpected read error while waiting for response/EOF: {e:?}"),
        }
    }
    received
}

/// `POST <target> HTTP/1.1` の要求バイト列を組み立てる。`headers` は宣言順で
/// そのまま出力する（大文字小文字・重複の検証はしない。テストが意図的に
/// 不正なヘッダ列を組み立てられるようにするため）。
pub fn build_request(target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"POST ");
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// `/v1/query` 宛ての整形済み要求（`Content-Type: application/json`・
/// 実際の `body` 長に一致する `Content-Length`）を組み立てる便宜 API。
pub fn well_formed_request(body: &[u8]) -> Vec<u8> {
    let content_length = body.len().to_string();
    build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body,
    )
}

/// 解析済み HTTP 応答（1 個）。`Date` ヘッダは応答ごとに変わるため golden
/// bytes 比較ではなく、本構造体へ解析したうえで個別フィールドを検証する。
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// ヘッダ名を ASCII 大文字小文字非区別で照合し、最初に一致した値を返す。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// `bytes` の中から `needle` の最初の出現位置を返す（ヘッダ／本文の境界
/// `\r\n\r\n` を探すための小さな純関数。テストコードのため `unwrap`/`expect`
/// は許容するが、対象がユーザー入力ではなくテスト自身が受信したバイト列
/// であり、境界が見つからないのは応答の不整合＝テスト失敗として扱ってよい）。
fn find_subslice(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|w| w == needle)
}

/// `send_raw` が返した応答バイト列（ちょうど 1 応答ぶんであること・
/// エンベロープの妥当性を含む）を解析する。
///
/// 検証する不変条件（いずれか 1 つでも破れれば panic でテスト失敗にする）:
/// - `HTTP/1.1 <3桁> <reason>\r\n` で始まる
/// - ヘッダ部が `\r\n\r\n` で終端する
/// - `Content-Length` ヘッダが存在し、本文の残余バイト長と一致する
///   （＝本文の後に余剰バイトが無い。1 応答＝1 接続の証跡）
/// - `Connection: close` が存在する
/// - `Content-Type` が [`wire_server::http::response::CONTENT_TYPE_JSON_UTF8`]
///   と一致する
pub fn parse_single_response(bytes: &[u8]) -> HttpResponse {
    let sep = find_subslice(bytes, b"\r\n\r\n")
        .unwrap_or_else(|| panic!("response missing header/body terminator: {bytes:?}"));
    let head = bytes.get(..sep).expect("head slice in range");
    let body = bytes.get(sep + 4..).expect("body slice in range");

    let head_str = std::str::from_utf8(head).expect("response head must be valid utf-8");
    let mut lines = head_str.split("\r\n");
    let status_line = lines.next().expect("response missing status line");
    let mut parts = status_line.splitn(3, ' ');
    let http_version = parts.next().expect("status line missing http version");
    assert_eq!(
        http_version, "HTTP/1.1",
        "unexpected http version in status line: {status_line:?}"
    );
    let status: u16 = parts
        .next()
        .expect("status line missing status code")
        .parse()
        .expect("status code must be a 3-digit number");
    let reason = parts
        .next()
        .expect("status line missing reason phrase")
        .to_string();

    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("header line missing colon: {line:?}"));
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let response = HttpResponse {
        status,
        reason,
        headers,
        body: body.to_vec(),
    };

    let content_length: usize = response
        .header("content-length")
        .expect("response missing content-length header")
        .parse()
        .expect("content-length header must be numeric");
    assert_eq!(
        content_length,
        response.body.len(),
        "content-length does not match trailing byte count (no surplus bytes after body)"
    );
    assert_eq!(
        response.header("connection"),
        Some("close"),
        "response missing Connection: close"
    );
    assert_eq!(
        response.header("content-type"),
        Some(wire_server::http::response::CONTENT_TYPE_JSON_UTF8),
        "unexpected content-type header"
    );

    response
}

/// `resp.body` を `{"error":{"wire_code":...,"code":...,"message":...}}` として
/// 解析し `wire_code` を返す。
pub fn wire_code_of(resp: &HttpResponse) -> String {
    error_field(resp, "wire_code")
}

/// [`wire_code_of`] と同じ枠組みで `message` を取り出す。
pub fn error_message_of(resp: &HttpResponse) -> String {
    error_field(resp, "message")
}

fn error_field(resp: &HttpResponse, field: &str) -> String {
    let body_str = std::str::from_utf8(&resp.body).expect("error body must be valid utf-8");
    let parsed = parse_json(body_str)
        .unwrap_or_else(|_| panic!("error body must be valid json: {body_str:?}"));
    let JsonValue::Object(mut top) = parsed else {
        panic!("error body top level must be an object: {body_str:?}");
    };
    let error_value = top
        .remove("error")
        .unwrap_or_else(|| panic!("error body missing \"error\" key: {body_str:?}"));
    let JsonValue::Object(error_obj) = error_value else {
        panic!("error body \"error\" value must be an object: {body_str:?}");
    };
    match error_obj.get(field) {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("error body \"error.{field}\" must be a string, got {other:?}"),
    }
}

/// 応答が `expected_status`／`expected_wire_code` の拒否応答であることを
/// 検証する（[`parse_single_response`] の不変条件を含む）。
pub fn assert_rejected(resp: &HttpResponse, expected_status: u16, expected_wire_code: &str) {
    assert_eq!(
        resp.status,
        expected_status,
        "unexpected status (body: {:?})",
        String::from_utf8_lossy(&resp.body)
    );
    let wire_code = wire_code_of(resp);
    assert_eq!(
        wire_code,
        expected_wire_code,
        "unexpected wire_code (body: {:?})",
        String::from_utf8_lossy(&resp.body)
    );
}

/// 本 Issue 時点で唯一到達できるハンドラ（[`wire_server::http::conn::
/// PlaceholderRouter`]）の固定応答文言。実ルータ（Issue #758）が
/// `PlaceholderRouter` を置き換えた際、本文言に依存する箇所はここ 1 箇所だけ
/// 更新すればよい。
pub const ROUTER_PLACEHOLDER_MESSAGE: &str = "unknown request target";

/// 「要求がパースを通ってハンドラへ到達した」ことを検証する（本 Issue 時点は
/// [`ROUTER_PLACEHOLDER_MESSAGE`] を返す `PlaceholderRouter` のみが production
/// ハンドラのため、到達＝この固定応答になる。モジュール doc 参照）。
pub fn assert_reached_router(resp: &HttpResponse) {
    assert_rejected(resp, 400, "08P01");
    assert_eq!(error_message_of(resp), ROUTER_PLACEHOLDER_MESSAGE);
}

/// 応答本文（`message` フィールド・生バイト列の双方）に `marker` が含まれて
/// いないことを確認する。要求内容・内部詳細がそのままクライアントへ
/// 反映されないこと（`.claude/rules/security.md` の非漏えい方針）を機械的に
/// 断定するための補助。
pub fn assert_message_does_not_echo(resp: &HttpResponse, marker: &str) {
    let message = error_message_of(resp);
    assert!(
        !message.contains(marker),
        "response message unexpectedly echoed marker {marker:?}: {message:?}"
    );
    let body_str = String::from_utf8_lossy(&resp.body);
    assert!(
        !body_str.contains(marker),
        "response body unexpectedly echoed marker {marker:?}: {body_str:?}"
    );
}
