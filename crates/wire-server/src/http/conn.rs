//! HTTP 接続 1 本ぶんの受理後処理（Issue #743・TASK-173／HTTP-11。対象
//! ポインタ: `docs/spec/05-tasks.md` TASK-69・WIRE-5, WIRE-6）。
//!
//! `http::listener::accept_loop_with_limiter` から呼ばれる、接続単位の 2 経路:
//! - [`handle_connection_interim`][]: 同時接続数の枠を確保できた接続の
//!   **暫定**ハンドラ。要求の解釈・ルーティング・応答生成は本 Issue の
//!   スコープ外（Issue #747 が置き換える）ため、固定長スタックバッファへの
//!   **有界 1 回 read** だけを行い、タイムアウト・EOF・データ到着の
//!   いずれでも応答を書かずにクローズする。「読む」ことを要求する
//!   `read_timeout`（[`crate::limits::READ_TIMEOUT`]）の受け入れ条件を
//!   反証可能にすることが本関数の存在理由であり、要求内容の解釈はまだ
//!   行わない
//! - [`reject_too_many_connections`][]: 同時接続数の枠を確保できなかった
//!   接続へ HTTP 503 ＋ JSON 本文（`wire_code`＝`53300`）を返してから
//!   クローズする拒否経路。`crate::server::accept_loop_inner` の拒否経路
//!   （[`crate::limits::reject_too_many_connections`]）の HTTP 版に相当する
//!
//! いずれも `pub(crate)`（#747 が接続ハンドラ本体を置き換える前提のため、
//! `pub` 昇格に伴う `#[deprecated]` 残置義務〔P1 互換方針〕を負わない）。
//! 結合テストが到達する公開 API は
//! [`crate::http::listener::accept_loop_with_limiter`] のみ。
//!
//! 受信データ経路（[`handle_connection_interim`] の read バッファ）のため
//! `unwrap`／`expect`／添字アクセス（`[]`）を用いない。

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};

use engine::error_format::ErrorClass;

use crate::http::{error_body, status};
use crate::limits::REJECT_WRITE_TIMEOUT;

/// `handle_connection_interim` が 1 回だけ読む read バッファ長。本 Issue では
/// 要求内容を解釈しないため、タイムアウト／EOF／データ到着の 3 状態を
/// 区別できる最小長（1 バイト）で足りる。
const INTERIM_READ_BUF_LEN: usize = 1;

/// 同時接続数の枠を確保できた接続の暫定ハンドラ。
///
/// 固定長スタックバッファへの有界 read を 1 回行うだけで、結果（タイムアウト
/// 〔`WouldBlock`／`TimedOut`〕・EOF・データ到着のいずれか）を問わず応答を
/// 書かずに `shutdown` する。呼び出し元（`listener::accept_loop_with_limiter`）
/// が受理直後に一度だけ `read_timeout` を設定した後のソケットを渡す前提。
///
/// 意図的な暫定挙動: 08P01 ルーティング・要求パース・応答生成は Issue #747
/// の担当であり、本関数をその設計の完成形と誤解しないこと。
pub(crate) fn handle_connection_interim(mut stream: TcpStream) {
    let mut buf = [0u8; INTERIM_READ_BUF_LEN];
    // 読み取り結果（Ok/Err いずれも）は本 Issue の時点では一切解釈しない。
    // 「読む」動作そのものが `read_timeout` の受け入れ条件を反証可能にする
    // ために必要（stub のように無読みで閉じると、タイムアウト超過の検証が
    // 空虚になる）。
    let _ = stream.read(&mut buf);
    let _ = stream.shutdown(Shutdown::Both);
}

/// 同時接続数の枠を確保できなかった接続へ HTTP 503 ＋ JSON 本文
/// （`wire_code`＝`53300`）を書き込み、接続を閉じる。
///
/// `crate::limits::reject_too_many_connections`（SQL 表層）の HTTP 版。書き込み
/// タイムアウトは同じ [`REJECT_WRITE_TIMEOUT`] を使う（拒否応答自体が
/// accept ループのブロッキング点にならないよう小さく設定する契約を共有）。
/// 書き込み失敗は無視する（拒否経路で新たなブロッキング点・panic を作らない
/// ため。クライアントが応答を受け取れなくても、最終的に `shutdown` で接続は
/// 閉じる）。
pub(crate) fn reject_too_many_connections(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));
    let response = encode_reject_response();
    let _ = stream.write_all(&response);
    let _ = stream.shutdown(Shutdown::Both);
}

/// 同時接続数上限超過時の HTTP 応答バイト列を組み立てる純関数。
///
/// `crate::http::response`（Issue #746。本 Issue 時点では未マージ）が入れば
/// そちらへ委譲する形に差し替える想定の最小エンベロープ。ステータス行
/// （`http::status::http_status(ErrorClass::ConnectionLimitExceeded)` ＝
/// 503 固定のため reason phrase も固定表記）・`Connection: close`・
/// `Content-Type: application/json; charset=utf-8`・`Content-Length`・
/// 空行・本文（`http::error_body::encode`）の順に組み立てる。
fn encode_reject_response() -> Vec<u8> {
    let class = ErrorClass::ConnectionLimitExceeded;
    debug_assert_eq!(status::http_status(class), 503);
    let body = error_body::encode(class, "too many connections");
    let body_bytes = body.as_bytes();

    let mut out = Vec::with_capacity(128 + body_bytes.len());
    out.extend_from_slice(b"HTTP/1.1 503 Service Unavailable\r\n");
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"Content-Type: application/json; charset=utf-8\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body_bytes.len()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body_bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Duration;

    /// `stream.read` が実際に EOF（`Ok(0)`）で終わったことを確認する。
    ///
    /// `read(...).unwrap_or(0)` は、クライアント側の read が `WouldBlock`／
    /// `TimedOut` で終わった場合も `Ok(0)`（EOF）と同一視してしまい、
    /// 「サーバーの `read_timeout` 超過後に接続が閉じる」という検証対象の
    /// 契約が破れていてもテストを通してしまう（codex-review 指摘）。
    /// ここでは `Ok(0)` のみを合格とし、それ以外（`WouldBlock`／`TimedOut`
    /// を含む）はテスト失敗として明示する。
    fn assert_eof(stream: &mut TcpStream) {
        let mut buf = [0u8; 8];
        match stream.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => panic!("expected EOF without any response bytes, got {n} bytes"),
            Err(e) => panic!("expected EOF (Ok(0)), got read error: {e:?}"),
        }
    }

    /// `encode_reject_response` の構造（ステータス行・ヘッダ・
    /// `Content-Length` の一致・ヘッダ終端がちょうど 1 箇所・本文が
    /// `error.wire_code == "53300"` で `data` キーを含まない）を固定する。
    #[test]
    fn encode_reject_response_has_53300_body_and_matching_content_length() {
        let response = encode_reject_response();
        let text = String::from_utf8(response.clone()).expect("response must be valid utf-8");

        assert!(
            text.starts_with("HTTP/1.1 503 "),
            "unexpected status line: {text:?}"
        );
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains("Content-Type: application/json; charset=utf-8\r\n"));

        let header_end = text
            .find("\r\n\r\n")
            .expect("must contain exactly one header terminator");
        assert_eq!(
            text.matches("\r\n\r\n").count(),
            1,
            "header terminator must appear exactly once"
        );
        let body = &text[header_end + 4..];

        let content_length_line = text
            .lines()
            .find(|l| l.starts_with("Content-Length:"))
            .expect("Content-Length header present");
        let declared_len: usize = content_length_line
            .trim_start_matches("Content-Length:")
            .trim()
            .parse()
            .expect("Content-Length must be a valid integer");
        assert_eq!(declared_len, body.len());

        let parsed = engine::json::parse_json(body).expect("body must be valid JSON");
        let engine::json::JsonValue::Object(top) = parsed else {
            panic!("top level must be an object");
        };
        let error_value = top.get("error").expect("error key present");
        let engine::json::JsonValue::Object(error_obj) = error_value else {
            panic!("error value must be an object");
        };
        let wire_code = error_obj.get("wire_code").expect("wire_code present");
        assert_eq!(
            wire_code,
            &engine::json::JsonValue::String(
                crate::limits::SQLSTATE_TOO_MANY_CONNECTIONS.to_string()
            )
        );
        assert!(
            !error_obj.contains_key("data"),
            "reject response must not carry the emergency-response-only data key"
        );
    }

    /// `handle_connection_interim` はデータを送らないクライアントに対して、
    /// 短縮タイムアウト超過後も一切応答を書かずに EOF（0 バイト）へ倒れる。
    #[test]
    fn handle_connection_interim_closes_without_response_after_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");
        server_stream
            .set_read_timeout(Some(Duration::from_millis(150)))
            .expect("set read timeout");

        std::thread::spawn(move || {
            handle_connection_interim(server_stream);
        });

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set client read timeout");
        assert_eof(&mut client);
    }

    /// `handle_connection_interim` はデータを送ったクライアントに対しても
    /// 応答を返さず EOF になる（要求解釈は本 Issue のスコープ外）。
    #[test]
    fn handle_connection_interim_closes_without_response_after_data_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");
        server_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        std::thread::spawn(move || {
            handle_connection_interim(server_stream);
        });

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set client read timeout");
        let _ = client.write_all(b"GET / HTTP/1.1\r\n\r\n");
        assert_eof(&mut client);
    }

    /// `reject_too_many_connections` は 503 応答を書き込んでから EOF になる。
    #[test]
    fn reject_too_many_connections_writes_response_and_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");

        std::thread::spawn(move || {
            reject_too_many_connections(server_stream);
        });

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut received = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&received);
        assert!(text.starts_with("HTTP/1.1 503 "), "got: {text:?}");
        assert!(text.contains("53300"), "got: {text:?}");
    }
}
