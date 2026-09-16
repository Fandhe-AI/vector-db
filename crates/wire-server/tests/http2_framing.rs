//! HTTP-2（要求行・ヘッダのフレーミング）の層 A 結合テスト（Issue #748・
//! TASK-173）。`wire_server::http::listener::accept_loop_with_limiter`
//! （production 入口。内部で `wire_server::http::conn::PlaceholderRouter` を
//! 固定で使う）をリスナーとして起動し、`std::net::TcpStream` で生バイトを
//! 送受信して外形契約を検証する。
//!
//! 各パーサの内部契約（境界値・個々の拒否理由）は
//! `crates/wire-server/src/http/request.rs`・`headers.rs` 自身の `#[cfg(test)]`
//! が固定済み（`pub(crate)` の内部 API を直接呼べる）。本ファイルはそれを
//! 「TCP 経由で 1 往復すると HTTP ステータス＋`wire_code` の JSON 応答が
//! 返り接続が閉じる」という外形まで含めて検証する（`tests/http_common/mod.rs`
//! のモジュール doc 参照）。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-2,
//! HTTP-11）。

use std::time::Duration;

#[path = "http_common/mod.rs"]
mod http_common;

use http_common::{assert_reached_router, SERVER_READ_TIMEOUT};
use http_common::{
    assert_rejected, build_request, parse_single_response, send_raw, spawn_http_listener,
    well_formed_request, AfterWrite,
};

const MAX_CONNECTIONS_FOR_TEST: usize = 4;

fn listener() -> std::net::SocketAddr {
    let (addr, _limiter) = spawn_http_listener(MAX_CONNECTIONS_FOR_TEST, SERVER_READ_TIMEOUT);
    addr
}

// --- 対照（受理側） ---------------------------------------------------

#[test]
fn accepts_well_formed_post_request() {
    let addr = listener();
    let request = well_formed_request(b"{}");
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

#[test]
fn accepts_empty_body_with_content_length_zero() {
    let addr = listener();
    let request = well_formed_request(b"");
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

// --- 要求行の拒否契約（すべて 400／08P01） ------------------------------

fn assert_malformed_request_line(raw: &[u8]) {
    let addr = listener();
    let received = send_raw(addr, raw, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

#[test]
fn rejects_get_method() {
    assert_malformed_request_line(
        b"GET /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_lowercase_post_method() {
    assert_malformed_request_line(
        b"post /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_http_1_0() {
    assert_malformed_request_line(
        b"POST /v1/query HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_http_2_0() {
    assert_malformed_request_line(
        b"POST /v1/query HTTP/2.0\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_bare_lf_line_terminator() {
    assert_malformed_request_line(b"POST /v1/query HTTP/1.1\n");
}

#[test]
fn rejects_leading_blank_line() {
    assert_malformed_request_line(b"\r\nPOST /v1/query HTTP/1.1\r\n");
}

#[test]
fn rejects_consecutive_spaces_in_request_line() {
    assert_malformed_request_line(b"POST  /v1/query HTTP/1.1\r\n");
}

#[test]
fn rejects_too_many_request_line_tokens() {
    assert_malformed_request_line(b"POST /v1/query HTTP/1.1 extra\r\n");
}

#[test]
fn rejects_too_few_request_line_tokens() {
    assert_malformed_request_line(b"POST /v1/query\r\n");
}

#[test]
fn rejects_absolute_form_target() {
    assert_malformed_request_line(b"POST http://host/v1/query HTTP/1.1\r\n");
}

#[test]
fn rejects_control_byte_in_target() {
    assert_malformed_request_line(b"POST /v1/\x01query HTTP/1.1\r\n");
}

#[test]
fn rejects_non_ascii_byte_in_target() {
    assert_malformed_request_line(b"POST /v1/\xc3query HTTP/1.1\r\n");
}

// --- ヘッダの拒否契約（すべて 400／08P01） -----------------------------

fn assert_malformed_headers(raw: &[u8]) {
    let addr = listener();
    let received = send_raw(addr, raw, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

#[test]
fn rejects_missing_content_length_header() {
    assert_malformed_headers(b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\n\r\n");
}

#[test]
fn rejects_negative_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: -1\r\n\r\n",
    );
}

#[test]
fn rejects_non_numeric_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: abc\r\n\r\n",
    );
}

#[test]
fn rejects_leading_plus_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: +1\r\n\r\n",
    );
}

#[test]
fn rejects_duplicate_content_length_same_value() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\nContent-Type: application/json\r\n\r\n",
    );
}

#[test]
fn rejects_duplicate_content_length_different_value() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 1\r\nContent-Type: application/json\r\n\r\n",
    );
}

#[test]
fn rejects_transfer_encoding_chunked_even_with_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
}

#[test]
fn rejects_transfer_encoding_chunked_without_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
}

#[test]
fn rejects_obs_fold_continuation_line() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\n Content-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_header_name_with_space() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nX Foo: bar\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_header_value_with_control_byte() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nX-Foo: b\x00r\r\nContent-Length: 0\r\n\r\n",
    );
}

// --- 本文長不一致（`Content-Length` との齟齬） --------------------------

/// 宣言長より短い本文を送って半クローズ（クライアントが書き込み側を閉じ、
/// サーバーが本文読み取り中に EOF を検知する）。半クローズ必須の理由:
/// クライアントが接続を開いたままだと、サーバーはタイムアウトまで応答を
/// 書かずに待ち続ける（`fill_remaining_body` の deadline 分岐）ため、
/// 検証したい「本文不一致を 08P01 として即時検知する」契約を確認できない。
#[test]
fn rejects_body_shorter_than_declared_content_length() {
    let addr = listener();
    let mut request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "10"),
        ],
        b"",
    );
    request.extend_from_slice(b"abc"); // 宣言 10 バイトに対し 3 バイトのみ送信
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

/// 宣言長より長い本文（余剰バイト）を送る。読み取りの分割は OS 依存で
/// 決定的に予測できないため（`tests/http_common/mod.rs` モジュール doc・
/// 実装計画参照）、固定できるのは「`wire_code` が 08P01」「応答がちょうど
/// 1 個ぶんで、その後 EOF になる」ことのみ（`parse_single_response` が
/// 後者を保証する）。keep-open でも、サーバー側の判断だけで応答＋クローズへ
/// 到達することを確認する。
#[test]
fn rejects_body_longer_than_declared_content_length() {
    let addr = listener();
    let mut request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "2"),
        ],
        b"",
    );
    request.extend_from_slice(b"abcdef"); // 宣言 2 バイトに対し 6 バイト送信
    let received = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

// --- 接続契約 ------------------------------------------------------------

#[test]
fn response_is_connection_close_even_with_keep_alive_request_header() {
    let addr = listener();
    let request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
            ("Connection", "keep-alive"),
        ],
        b"",
    );
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    // parse_single_response 自体が Connection: close を検証する。
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

/// 1 回の書き込みへ整形済み要求を 2 本連結（パイプライン）しても、応答は
/// ちょうど 1 個（2 本目は解釈されない。非パイプライン設計の外形証跡）。
///
/// 2 本目の要求バイト列は 1 本目の `Content-Length` から見れば境界の無い
/// 余剰バイトそのものであり、`rejects_body_longer_than_declared_content_length`
/// と同じ「読み取りの分割が OS 依存で決定不能」な事情を持つ（1 本目の本文
/// ちょうどの直後に 2 本目の要求行が続くため、サーバーは高い確率で「本文が
/// 宣言長を超えている」と即時検知するが、`wire_code` の一致のみを固定し
/// `message` は固定しない。`tests/http_common/mod.rs` モジュール doc 参照）。
#[test]
fn pipelined_second_request_is_not_interpreted() {
    let addr = listener();
    let mut request = well_formed_request(b"{}");
    request.extend_from_slice(&well_formed_request(b"{}"));
    let received = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

/// `Expect` ヘッダ（値を問わず）は暫定応答を送る経路が無いため、最終エラー
/// 応答（`FeatureNotSupported`／`0A000`／501）へ倒れる（PR #810 レビュー
/// 対応の回帰固定）。
#[test]
fn rejects_expect_100_continue() {
    let addr = listener();
    let request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
            ("Expect", "100-continue"),
        ],
        b"",
    );
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 501, "0A000");
}

/// 起動ヘルパー自体が `SERVER_READ_TIMEOUT` を使っていることの健全性確認
/// （テストが恒久的にハングしない設計であることの明示）。
#[test]
fn server_read_timeout_constant_is_bounded() {
    assert!(SERVER_READ_TIMEOUT <= Duration::from_secs(30));
}
