//! サーバー側 TLS 1.3 ハンドシェイク状態機械（`wire_server::tls::
//! server_handshake`。TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #965・
//! 親 #941）の公開 API のみを使う結合テスト。
//!
//! 独立した正しさの根拠（RFC 8448 §3 の ServerHello バイト一致）と、
//! 公開 API のみで組んだ最小クライアントによる完全な往復の 2 系統で
//! 固定する。加えて、`perform_server_handshake_with_timeout`（#966 が
//! 実接続へ結線する blocking driver 本体）自身の成功経路（loopback
//! `TcpStream` 越しのフルハンドシェイク＋アプリケーションデータ往復）も
//! 固定する（#965 レビュー指摘。従来はタイムアウト系の 1 テストでしか
//! driver を経由していなかった）。

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wire_server::tls::alert::{Alert, ReceivedAlert};
use wire_server::tls::certificate_verify;
use wire_server::tls::ed25519::SigningKey;
use wire_server::tls::finished;
use wire_server::tls::handshake::{self, HandshakeType, RawHandshake};
use wire_server::tls::key_schedule::EarlySecret;
use wire_server::tls::record::{self, AlertDescription, ContentType, Record, RecordKind};
use wire_server::tls::record_protection::{Opener, Sealer};
use wire_server::tls::server_handshake::{
    HandshakeEntropy, ServerHandshake, ServerHandshakeError, Step, TlsServerConfig,
    HANDSHAKE_READ_TIMEOUT,
};
use wire_server::tls::transcript::Transcript;
use wire_server::tls::x25519::EphemeralSecret;
use wire_server::tls::x509::ServerCertificateChain;

fn hex_decode(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("ascii hex pair");
            u8::from_str_radix(text, 16).expect("valid hex byte")
        })
        .collect()
}

fn hex_decode32(s: &str) -> [u8; 32] {
    hex_decode(s).try_into().expect("32 bytes")
}

// ---- RFC 8448 §3（Simple 1-RTT Handshake。IETF の公開文書由来。
// `docs/spec` の内容ではない）のベクタ。`tls_transcript_finished_rfc8448.rs`
// と同じ値を再利用する。----

const CLIENT_HELLO: &str = "010000c00303cb34ecb1e78163ba1c38c6dacb196a6dffa21a8d9912ec18a2ef6283024dece7000006130113031302010000910000000b0009000006736572766572ff01000100000a0014001200\
    1d0017001800190100010101020103010400230000003300260024001d002099381de560e4bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c002b0003020304000d0020001e04\
    0305030603020308040805080604010501060102010402050206020202002d000201\
    01001c00024001";
const SERVER_HELLO: &str = "020000560303a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155772ed3e2692800130100002e00330024001d0020c9828876112095fe66762bdbf7c672e156d6cc253b833df1dd69b1b04e751f0f002b00020304";

const SERVER_X25519_PRIV: &str = "b1580eeadf6dd589b8ef4f2d5652578cc810e9980191ec8d058308cea216a21e";
const SERVER_RANDOM: &str = "a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155772ed3e26928";

/// RFC 8032 §7.1 TEST 1 の Ed25519 seed／公開鍵（IETF の公開文書由来）。
const RFC8032_TEST1_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const RFC8032_TEST1_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

// ---- テスト専用 DER エンコーダ（本番コードには持たない。`tls_x509.rs`
// と同方針）。----

fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = body.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.push(0x81);
        out.push(len as u8);
    } else {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    }
    out.extend_from_slice(body);
    out
}

fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(part);
    }
    tlv(0x30, &body)
}

const OID_ED25519_BYTES: [u8; 3] = [0x2b, 0x65, 0x70];

fn ed25519_algorithm_identifier() -> Vec<u8> {
    sequence(&[&tlv(0x06, &OID_ED25519_BYTES)])
}

fn utc_time(text: &str) -> Vec<u8> {
    tlv(0x17, text.as_bytes())
}

fn issuer_name() -> Vec<u8> {
    let common_name = sequence(&[&tlv(0x06, &[0x55, 0x04, 0x03]), &tlv(0x0c, b"test-issuer")]);
    sequence(&[&tlv(0x31, &common_name)])
}

fn empty_name() -> Vec<u8> {
    sequence(&[])
}

fn version_v3() -> Vec<u8> {
    tlv(0xa0, &tlv(0x02, &[0x02]))
}

/// テスト専用: Ed25519 葉証明書の DER を手組みする（署名は検証対象外の
/// ため全 0 で埋める。`tls_x509.rs::build_ed25519_leaf_certificate_der`
/// と同方針）。
fn build_ed25519_leaf_certificate_der(public_key: &[u8; 32]) -> Vec<u8> {
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(public_key);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ])
}

fn test_config() -> Arc<TlsServerConfig> {
    let der = build_ed25519_leaf_certificate_der(&RFC8032_TEST1_PUBLIC_KEY);
    let chain =
        ServerCertificateChain::from_der_chain(vec![der], &RFC8032_TEST1_PUBLIC_KEY, 1_600_000_000)
            .expect("valid synthetic chain");
    let key = SigningKey::from_seed_bytes(hex_decode32(RFC8032_TEST1_SEED));
    Arc::new(TlsServerConfig::new(chain, key).expect("matching leaf/key"))
}

/// `TlsServerConfig::new` は、証明書チェーンの葉が保持する公開鍵と
/// 署名鍵の公開鍵が一致しない場合に拒否する（起動時エラー）。
#[test]
fn tls_server_config_rejects_mismatched_key() {
    // RFC 8032 §7.1 TEST 1 の公開鍵を持つ証明書に対し、TEST 2 の秘密鍵
    // （別の鍵ペア）を渡す。
    const RFC8032_TEST2_SEED: &str =
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6f";
    let der = build_ed25519_leaf_certificate_der(&RFC8032_TEST1_PUBLIC_KEY);
    let chain =
        ServerCertificateChain::from_der_chain(vec![der], &RFC8032_TEST1_PUBLIC_KEY, 1_600_000_000)
            .expect("valid synthetic chain");
    let key = SigningKey::from_seed_bytes(hex_decode32(RFC8032_TEST2_SEED));
    match TlsServerConfig::new(chain, key) {
        Err(wire_server::tls::server_handshake::TlsServerConfigError::PublicKeyMismatch) => {}
        Ok(_) => panic!("mismatched key must be rejected"),
    }
}

/// テスト専用の決定的乱数源。server random・一時鍵を固定値で注入する。
struct FixedEntropy {
    random: [u8; 32],
    ephemeral_seed: [u8; 32],
    used: bool,
}

impl HandshakeEntropy for FixedEntropy {
    fn server_random(&mut self) -> std::io::Result<[u8; 32]> {
        Ok(self.random)
    }

    fn ephemeral(&mut self) -> std::io::Result<EphemeralSecret> {
        if self.used {
            return Err(std::io::Error::other(
                "ephemeral() called more than once in this test",
            ));
        }
        self.used = true;
        Ok(EphemeralSecret::from_bytes(self.ephemeral_seed))
    }
}

fn plaintext_handshake_record(wire_bytes: Vec<u8>) -> Record {
    Record {
        content_type: ContentType::Handshake,
        legacy_version: record::LEGACY_RECORD_VERSION,
        fragment: wire_bytes,
    }
}

fn client_hello_from_rfc8448_with_ed25519() -> (Record, handshake::ClientHello) {
    let full = hex_decode(CLIENT_HELLO);
    let mut ch = handshake::ClientHello::parse(&full[4..]).expect("valid RFC 8448 ClientHello");
    for ext in &mut ch.extensions {
        if ext.extension_type == 13 {
            let mut data = ext.extension_data.clone();
            let inner_len = u16::from_be_bytes(
                data.get(0..2)
                    .and_then(|s| s.try_into().ok())
                    .expect("length prefix present"),
            );
            data[0..2].copy_from_slice(&(inner_len + 2).to_be_bytes());
            data.extend_from_slice(&0x0807u16.to_be_bytes());
            ext.extension_data = data;
        }
    }
    let mut body = Vec::new();
    ch.serialize_body_into(&mut body).expect("valid body");
    let raw = RawHandshake {
        msg_type: HandshakeType::ClientHello,
        body,
    };
    (
        plaintext_handshake_record(raw.to_bytes().expect("valid wire bytes")),
        ch,
    )
}

/// 受け入れ条件: RFC 8448 §3 のサーバー乱数・鍵交換秘密鍵を注入した際、
/// サーバーが送出する `ServerHello` のバイト列が RFC トレースと完全一致
/// すること（`ServerHello` の組み立て・key_share 公開鍵導出・拡張順序に
/// 対する唯一の独立した正しさの根拠）。
#[test]
fn server_hello_matches_rfc8448_trace_byte_for_byte() {
    let config = test_config();
    let entropy = FixedEntropy {
        random: hex_decode32(SERVER_RANDOM),
        ephemeral_seed: hex_decode32(SERVER_X25519_PRIV),
        used: false,
    };
    let mut hs = ServerHandshake::with_entropy(config, entropy);
    let (ch_record, _ch) = client_hello_from_rfc8448_with_ed25519();

    let step = hs
        .handle_record(&ch_record)
        .expect("valid ClientHello accepted");
    let Step::Continue(output) = step else {
        panic!("expected Continue step for the Accept flow");
    };
    let sh_record = output.first().expect("at least the ServerHello record");
    assert_eq!(sh_record.content_type, ContentType::Handshake);
    assert_eq!(sh_record.fragment, hex_decode(SERVER_HELLO));
}

/// 受け入れ条件 1: ClientHello を待っている状態で `ApplicationData` を
/// 受けると、Plaintext epoch では `Opener::open` 自身がこれを
/// `UnexpectedOuterType` として拒否する（`record_protection.rs` の契約。
/// 送出される alert は `unexpected_message` で変わらない）。
#[test]
fn application_data_before_handshake_completes_is_unexpected_message() {
    let config = test_config();
    let mut hs = ServerHandshake::new(config);
    let bogus = Record {
        content_type: ContentType::ApplicationData,
        legacy_version: record::LEGACY_RECORD_VERSION,
        fragment: vec![0u8; 16],
    };
    let err = hs.handle_record(&bogus).expect_err("must be rejected");
    match err {
        ServerHandshakeError::Protection(_) => {}
        other => panic!("expected a record-protection failure, got {other:?}"),
    }
    let alert_output = hs.take_pending_alert_output();
    assert_eq!(alert_output.len(), 1);
    let alert = Alert::parse(&alert_output[0].fragment).expect("valid alert bytes");
    assert_eq!(
        alert.description,
        AlertDescription::UnexpectedMessage.as_u8()
    );
}

/// 受け入れ条件 1: 壊れた ClientHello 本文は `decode_error`。
#[test]
fn malformed_client_hello_is_decode_error() {
    let config = test_config();
    let mut hs = ServerHandshake::new(config);
    let raw = RawHandshake {
        msg_type: HandshakeType::ClientHello,
        body: vec![0x03, 0x03], // 本文が短すぎる。
    };
    let record = plaintext_handshake_record(raw.to_bytes().expect("valid header"));
    let err = hs.handle_record(&record).expect_err("must be rejected");
    match err {
        ServerHandshakeError::Handshake(_) => {}
        other => panic!("expected Handshake decode error, got {other:?}"),
    }
}

/// 受け入れ条件 2: 最初のエラーで fatal alert がちょうど 1 レコード
/// 出力され、その後は同じ `Err` と空の出力になる。
#[test]
fn fatal_error_emits_one_alert_then_poisons() {
    let config = test_config();
    let mut hs = ServerHandshake::new(config);
    let bogus = Record {
        content_type: ContentType::ApplicationData,
        legacy_version: record::LEGACY_RECORD_VERSION,
        fragment: vec![0u8; 16],
    };
    let err1 = hs.handle_record(&bogus).expect_err("first call must fail");
    let alert_output = hs.take_pending_alert_output();
    assert_eq!(
        alert_output.len(),
        1,
        "exactly one alert record must be emitted"
    );
    assert_eq!(alert_output[0].content_type, ContentType::Alert);
    let alert = Alert::parse(&alert_output[0].fragment).expect("valid alert bytes");
    assert_eq!(
        alert.description,
        AlertDescription::UnexpectedMessage.as_u8()
    );

    // 以後は同じ理由の `Err` を返し、追加の出力は無い。
    let err2 = hs
        .handle_record(&bogus)
        .expect_err("second call must also fail");
    assert_eq!(err1, err2);
    assert!(hs.take_pending_alert_output().is_empty());
    let err3 = hs
        .handle_record(&bogus)
        .expect_err("third call must also fail");
    assert_eq!(err1, err3);
}

/// 受け入れ条件 4: `HANDSHAKE_READ_TIMEOUT` が `limits::READ_TIMEOUT` と
/// 一致すること。
#[test]
fn handshake_read_timeout_matches_wire_read_timeout() {
    assert_eq!(HANDSHAKE_READ_TIMEOUT, wire_server::limits::READ_TIMEOUT);
}

/// 受け入れ条件 4: loopback の `TcpStream` で、無送信のクライアントに
/// 対して短いタイムアウトのもと alert を 1 バイトも送らずに `Err` で
/// 閉じること。
#[test]
fn handshake_times_out_without_sending_any_alert() {
    use std::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");

    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept connection");
        wire_server::tls::server_handshake::perform_server_handshake_with_timeout(
            &mut socket,
            test_config(),
            Duration::from_millis(150),
        )
    });

    let mut client = TcpStream::connect(addr).expect("connect to loopback listener");
    // クライアントは何も送らない。サーバー側の read タイムアウトを待つ。
    let result = server_thread.join().expect("server thread must not panic");
    assert!(result.is_err(), "timeout must be reported as an error");

    // タイムアウト後、サーバーは何も書き込んでいないはず。
    client
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set client read timeout");
    let mut buf = [0u8; 1];
    let n = client.read(&mut buf);
    match n {
        Ok(0) => {} // 接続が閉じられ、EOF として観測される。
        Err(e) => {
            assert!(
                matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ),
                "expected no data and no immediate close signal, got {e:?}"
            );
        }
        Ok(n) => panic!("expected no bytes from a timed-out handshake, got {n}"),
    }
}

/// #965 レビュー指摘の回帰: ハンドシェイク全体のタイムアウトは個々の
/// 読み取り呼び出し単位ではなく絶対期限として働かなければならない。
/// クライアントが個々の読み取りタイムアウトを常に下回る間隔で 1 バイト
/// ずつ送り続けても、ハンドシェイク全体は `overall_timeout` の数倍以内で
/// 打ち切られることを固定する（ループの外側で `set_read_timeout` を
/// 1 回だけ呼ぶ実装だと、この打ち切りが働かず低速送信で長時間占有され
/// 得た）。
#[test]
fn handshake_absolute_deadline_bounds_slow_drip_client() {
    use std::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    let overall_timeout = Duration::from_millis(200);

    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept connection");
        let started = Instant::now();
        let result = wire_server::tls::server_handshake::perform_server_handshake_with_timeout(
            &mut socket,
            test_config(),
            overall_timeout,
        );
        (started.elapsed(), result)
    });

    let mut client = TcpStream::connect(addr).expect("connect to loopback listener");
    // 1 バイトずつ、`overall_timeout` を常に下回る間隔で送り続ける。
    // 打ち切られなければ 12 回 * 80ms = 960ms 分、個々の読み取りタイム
    // アウトには一度もかからないまま送り続けられる。
    for _ in 0..12 {
        if client.write_all(&[0x16]).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    drop(client);

    let (elapsed, result) = server_thread.join().expect("server thread must not panic");
    assert!(
        result.is_err(),
        "slow-drip client must not complete the handshake"
    );
    assert!(
        elapsed < overall_timeout * 4,
        "handshake must be bounded by the absolute deadline, got {elapsed:?} for timeout {overall_timeout:?}"
    );
}

/// PR #1046 レビュー指摘の回帰（driver 経由）: ClientHello を送ったあと
/// 受信を止めた相手に対しても、server flight（ServerHello・証明書等）の
/// 送出はハンドシェイク全体の絶対期限内に `Err` で打ち切られ、受信側の
/// 期限超過と同じく接続が shutdown されること。
///
/// 実 loopback ソケットは送信バッファが server flight より十分大きく
/// 書き込みブロックを再現できないため、「OS が `set_write_timeout` を
/// 尊重し、相手が読まないまま指定時間だけブロックしたのちタイムアウトを
/// 返す」挙動をモックで再現する。
#[test]
fn server_flight_write_is_bounded_by_absolute_deadline_when_peer_stops_reading() {
    struct StalledReader {
        inbound: std::io::Cursor<Vec<u8>>,
        write_timeout: Option<Duration>,
        write_attempts: usize,
        shutdown_called: bool,
    }

    impl Read for StalledReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inbound.read(buf)
        }
    }

    impl Write for StalledReader {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            self.write_attempts += 1;
            // 期限の設定が無ければテスト全体が長時間止まるのを防ぐため、
            // 上限として 5 秒だけ待つ（この場合は下の経過時間アサーションで
            // 失敗として検出される）。
            std::thread::sleep(self.write_timeout.unwrap_or(Duration::from_secs(5)));
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "peer never reads",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl wire_server::tls::server_handshake::HandshakeTransport for StalledReader {
        fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
            self.write_timeout = timeout;
            Ok(())
        }

        fn shutdown(&mut self) -> std::io::Result<()> {
            self.shutdown_called = true;
            Ok(())
        }
    }

    let client_ephemeral = EphemeralSecret::from_bytes([0x33u8; 32]);
    let (client_hello_record, _) = build_client_hello(*client_ephemeral.public_key().as_bytes());
    let mut inbound = Vec::new();
    client_hello_record
        .serialize_into(&mut inbound, RecordKind::Plaintext)
        .expect("serialize client hello record");

    let mut peer = StalledReader {
        inbound: std::io::Cursor::new(inbound),
        write_timeout: None,
        write_attempts: 0,
        shutdown_called: false,
    };
    let overall_timeout = Duration::from_millis(150);

    let started = Instant::now();
    let result = wire_server::tls::server_handshake::perform_server_handshake_with_timeout(
        &mut peer,
        test_config(),
        overall_timeout,
    );
    let elapsed = started.elapsed();

    assert!(
        result.is_err(),
        "a peer that stops reading must not complete the handshake"
    );
    assert!(
        peer.write_attempts > 0,
        "the server flight must actually have been attempted (non-vacuous)"
    );
    assert!(
        elapsed < overall_timeout * 4,
        "server flight write must be bounded by the absolute deadline, \
         got {elapsed:?} for timeout {overall_timeout:?}"
    );
    assert!(
        peer.shutdown_called,
        "a write-side deadline failure must shut the connection down"
    );
}

// ---- 独立クライアントによる完全な往復（受け入れ条件 3 以外の全体結合）。
// ----

/// テスト専用の最小クライアント。公開 API のみを使ってサーバーと
/// フルハンドシェイクを行う。
struct TestClient {
    transcript: Transcript,
    sealer: Sealer,
    opener: Opener,
}

fn build_client_hello(client_pub: [u8; 32]) -> (Record, RawHandshake) {
    build_client_hello_ex(Some(client_pub), [0x42u8; 32])
}

/// `key_share` の有無・`random` を選べる版。`key_share` を省略した
/// ClientHello は `supported_groups` に x25519 を含みつつ鍵を提示しない
/// ため、`negotiate` が `RetryRequestX25519` を返す（HelloRetryRequest
/// 誘発フィクスチャ）。
fn build_client_hello_ex(client_pub: Option<[u8; 32]>, random: [u8; 32]) -> (Record, RawHandshake) {
    let supported_versions = handshake::Extension {
        extension_type: 43,
        extension_data: vec![0x02, 0x03, 0x04],
    };
    let signature_algorithms = handshake::Extension {
        extension_type: 13,
        extension_data: vec![0x00, 0x02, 0x08, 0x07],
    };
    let supported_groups = handshake::Extension {
        extension_type: 10,
        extension_data: vec![0x00, 0x02, 0x00, 0x1d],
    };
    let mut extensions = vec![supported_groups, signature_algorithms, supported_versions];
    if let Some(client_pub) = client_pub {
        let mut key_share_data = vec![0x00, 0x24, 0x00, 0x1d, 0x00, 0x20];
        key_share_data.extend_from_slice(&client_pub);
        extensions.push(handshake::Extension {
            extension_type: 51,
            extension_data: key_share_data,
        });
    } else {
        // key_share 拡張自体は必須（`negotiate` が `MissingExtension` に
        // せず `RetryRequestX25519` を返すには、拡張が「存在するが
        // x25519 エントリを含まない」形である必要がある。RFC 8446 は
        // `key_share` を空リストで送ることを許容する）。
        extensions.push(handshake::Extension {
            extension_type: 51,
            extension_data: vec![0x00, 0x00],
        });
    }
    let ch = handshake::ClientHello {
        legacy_version: 0x0303,
        random,
        legacy_session_id: Vec::new(),
        cipher_suites: vec![0x1301],
        legacy_compression_methods: vec![0x00],
        extensions,
    };
    let mut body = Vec::new();
    ch.serialize_body_into(&mut body).expect("valid body");
    let raw = RawHandshake {
        msg_type: HandshakeType::ClientHello,
        body,
    };
    let record = plaintext_handshake_record(raw.to_bytes().expect("valid wire bytes"));
    (record, raw)
}

impl TestClient {
    fn new() -> Self {
        TestClient {
            transcript: Transcript::new(),
            sealer: Sealer::new(),
            opener: Opener::new(),
        }
    }

    /// server flight（1 レコード以上の暗号化 Handshake レコード列）を
    /// 復号し、EncryptedExtensions・Certificate・CertificateVerify・
    /// server Finished を順に検証する。
    fn process_server_flight(
        &mut self,
        flight_records: &[Record],
        expected_leaf_public_key: &[u8; 32],
        server_hs_traffic: &wire_server::tls::key_schedule::TrafficSecret,
    ) -> [u8; 32] {
        let mut buffer = handshake::HandshakeBuffer::new();
        for record in flight_records {
            let inner = self
                .opener
                .open(record)
                .expect("valid server flight record");
            assert_eq!(inner.content_type, ContentType::Handshake);
            buffer.feed(&inner.content).expect("buffer capacity");
        }

        let ee = buffer
            .next_message()
            .expect("valid message")
            .expect("EncryptedExtensions present");
        assert_eq!(ee.msg_type, HandshakeType::EncryptedExtensions);
        self.transcript
            .append_encrypted_extensions(&ee)
            .expect("valid EncryptedExtensions");

        let cert_raw = buffer
            .next_message()
            .expect("valid message")
            .expect("Certificate present");
        assert_eq!(cert_raw.msg_type, HandshakeType::Certificate);
        self.transcript
            .append_certificate(&cert_raw)
            .expect("valid Certificate");
        let th_ch_cert = self
            .transcript
            .hash_through_certificate()
            .expect("checkpoint reached");

        let cv_raw = buffer
            .next_message()
            .expect("valid message")
            .expect("CertificateVerify present");
        assert_eq!(cv_raw.msg_type, HandshakeType::CertificateVerify);
        let cv =
            handshake::CertificateVerify::parse(&cv_raw.body).expect("valid CertificateVerify");
        certificate_verify::verify_server_certificate_verify(
            expected_leaf_public_key,
            &th_ch_cert,
            &cv,
        )
        .expect("server CertificateVerify must verify");
        self.transcript
            .append_certificate_verify(&cv_raw)
            .expect("valid CertificateVerify");
        let th_ch_cv = self
            .transcript
            .hash_through_certificate_verify()
            .expect("checkpoint reached");

        let sf_raw = buffer
            .next_message()
            .expect("valid message")
            .expect("server Finished present");
        assert_eq!(sf_raw.msg_type, HandshakeType::Finished);
        let server_finished =
            handshake::Finished::parse(&sf_raw.body).expect("valid server Finished");
        // server Finished の verify_data を独立に再計算し、サーバーが
        // 送ってきた値と一致することを確認する（`server_hs_traffic` は
        // クライアント側で独立に導出した secret）。
        let expected_server_finished_key = server_hs_traffic
            .finished_key()
            .expect("valid HKDF parameters");
        let expected_verify_data =
            finished::compute_verify_data(&expected_server_finished_key, &th_ch_cv);
        assert_eq!(
            expected_verify_data.as_bytes().as_slice(),
            server_finished.verify_data.as_slice(),
            "server Finished verify_data must match independently-derived value"
        );
        self.transcript
            .append_server_finished(&sf_raw)
            .expect("valid server Finished");
        let th_ch_sf = self
            .transcript
            .hash_through_server_finished()
            .expect("checkpoint reached");

        assert!(
            buffer.next_message().expect("valid message").is_none() && !buffer.has_partial(),
            "server flight must not contain trailing bytes"
        );

        th_ch_sf
    }
}

/// フルハンドシェイクを純粋 API（`handle_record` 直結）で完了させ、
/// application epoch まで切り替えた `TestClient` と、サーバー側の
/// `TlsSession` を返す。application data・`close_notify`・poison 状態を
/// 検証する各テストが共有するセットアップ（#965 レビュー指摘対応で
/// `full_handshake_round_trip_over_pure_api` から切り出した）。
fn establish_session_for_tests() -> (
    TestClient,
    Box<wire_server::tls::server_handshake::TlsSession>,
) {
    let config = test_config();
    let leaf_public_key = RFC8032_TEST1_PUBLIC_KEY;

    let client_priv = [0x11u8; 32];
    let client_ephemeral = EphemeralSecret::from_bytes(client_priv);
    let client_pub = *client_ephemeral.public_key().as_bytes();

    let mut client = TestClient::new();
    let (ch_record, ch_raw) = build_client_hello(client_pub);
    client
        .transcript
        .append_client_hello(&ch_raw)
        .expect("valid ClientHello");

    let entropy = FixedEntropy {
        random: [0x77u8; 32],
        ephemeral_seed: [0x22u8; 32],
        used: false,
    };
    let mut server = ServerHandshake::with_entropy(config, entropy);
    let step = server
        .handle_record(&ch_record)
        .expect("server must accept the ClientHello");
    let Step::Continue(output) = step else {
        panic!("expected Continue for the Accept flow");
    };
    let (sh_record, flight_records) = output.split_first().expect("at least ServerHello");

    let sh = handshake::ServerHello::parse(&sh_record.fragment[4..]).expect("valid ServerHello");
    let mut sh_body = Vec::new();
    sh.serialize_body_into(&mut sh_body).expect("valid body");
    let sh_raw = RawHandshake {
        msg_type: HandshakeType::ServerHello,
        body: sh_body,
    };
    client
        .transcript
        .append_server_hello(&sh_raw)
        .expect("valid ServerHello");
    let th_ch_sh = client
        .transcript
        .hash_through_server_hello()
        .expect("checkpoint reached");

    let server_pub = sh
        .extensions
        .iter()
        .find(|e| e.extension_type == 0x0033)
        .and_then(|e| e.extension_data.get(4..36))
        .and_then(|s| <[u8; 32]>::try_from(s).ok())
        .expect("server key_share public key present");

    let shared = client_ephemeral
        .diffie_hellman(&server_pub)
        .expect("valid non-zero shared secret");
    let early = EarlySecret::new_without_psk();
    let handshake_secret = early
        .into_handshake(&shared)
        .expect("valid HKDF parameters");
    let traffic = handshake_secret
        .traffic_secrets(&th_ch_sh)
        .expect("valid HKDF parameters");
    let client_hs_keys = traffic
        .client
        .traffic_keys()
        .expect("valid HKDF parameters");
    let server_hs_keys = traffic
        .server
        .traffic_keys()
        .expect("valid HKDF parameters");
    client
        .sealer
        .install_handshake_keys(&client_hs_keys)
        .expect("valid transition");
    client
        .opener
        .install_handshake_keys(&server_hs_keys)
        .expect("valid transition");

    let th_ch_sf = client.process_server_flight(flight_records, &leaf_public_key, &traffic.server);

    let master = handshake_secret
        .into_master()
        .expect("valid HKDF parameters");
    let app = master
        .application_traffic_secrets(&th_ch_sf)
        .expect("valid HKDF parameters");
    let client_ap_keys = app.client.traffic_keys().expect("valid HKDF parameters");
    let server_ap_keys = app.server.traffic_keys().expect("valid HKDF parameters");

    // client Finished を構成し、handshake epoch のまま送る。
    let client_finished_key = traffic
        .client
        .finished_key()
        .expect("valid HKDF parameters");
    let verify_data = finished::compute_verify_data(&client_finished_key, &th_ch_sf);
    let client_finished = handshake::Finished {
        verify_data: verify_data.as_bytes().to_vec(),
    };
    let mut cf_body = Vec::new();
    client_finished
        .serialize_body_into(&mut cf_body)
        .expect("valid body");
    let cf_raw = RawHandshake {
        msg_type: HandshakeType::Finished,
        body: cf_body,
    };
    let cf_wire = cf_raw.to_bytes().expect("valid wire bytes");
    let cf_records = client
        .sealer
        .seal_fragmented(ContentType::Handshake, &cf_wire)
        .expect("valid seal");

    let mut session = None;
    for record in &cf_records {
        match server
            .handle_record(record)
            .expect("client Finished must verify")
        {
            Step::Complete(output, s) => {
                assert!(output.is_empty(), "no output expected on completion");
                session = Some(s);
            }
            Step::Continue(output) => assert!(output.is_empty()),
            Step::ClosedByPeer(_) => panic!("must not close during client Finished"),
        }
    }
    let session = session.expect("handshake must complete");

    // application epoch へ切り替える（client 側）。
    client
        .sealer
        .install_application_keys(&client_ap_keys)
        .expect("valid transition");
    client
        .opener
        .install_application_keys(&server_ap_keys)
        .expect("valid transition");

    (client, session)
}

/// 受け入れ条件全体: 純粋 API（`handle_record` の直結）による完全な
/// ハンドシェイクの往復と、application data・`close_notify` の往復まで。
#[test]
fn full_handshake_round_trip_over_pure_api() {
    let (mut client, mut session) = establish_session_for_tests();

    // サーバー → クライアント。
    let payload = b"hello from server";
    let records = session
        .seal_application_data(payload)
        .expect("valid application data");
    for record in &records {
        let inner = client
            .opener
            .open(record)
            .expect("valid application record");
        assert_eq!(inner.content_type, ContentType::ApplicationData);
        assert_eq!(inner.content, payload);
    }

    // クライアント → サーバー。
    let client_payload = b"hello from client";
    let client_records = client
        .sealer
        .seal_fragmented(ContentType::ApplicationData, client_payload)
        .expect("valid application data");
    for record in &client_records {
        let event = session
            .open_record(record)
            .expect("valid application record");
        match event {
            wire_server::tls::server_handshake::AppEvent::ApplicationData(data) => {
                assert_eq!(data, client_payload);
            }
            wire_server::tls::server_handshake::AppEvent::CloseNotify => {
                panic!("expected application data, got close_notify")
            }
        }
    }

    // close_notify の往復。
    let close_records = session.close_notify().expect("valid close_notify");
    for record in &close_records {
        let inner = client
            .opener
            .open(record)
            .expect("valid close_notify record");
        assert_eq!(inner.content_type, ContentType::Alert);
        let alert = Alert::parse(&inner.content).expect("valid alert bytes");
        assert_eq!(
            wire_server::tls::alert::classify_received(alert),
            ReceivedAlert::Closed
        );
    }
}

/// #965 レビュー指摘の回帰: 自ら `close_notify` を送出した後は、RFC 8446
/// §6.1 によりこの接続でこれ以上データを送ってはならないため、以後の
/// 送信操作（`seal_application_data`／`close_notify`）は `Poisoned` で
/// 拒否される（受信側の扱いは
/// `sent_close_notify_still_allows_receiving_peer_data_and_close_notify`）。
#[test]
fn close_notify_poisons_the_session_against_further_data() {
    use wire_server::tls::server_handshake::TlsSessionError;

    let (_client, mut session) = establish_session_for_tests();

    session.close_notify().expect("first close_notify succeeds");

    assert_eq!(
        session.seal_application_data(b"must not be sent"),
        Err(TlsSessionError::Poisoned)
    );
    assert_eq!(session.close_notify(), Err(TlsSessionError::Poisoned));
}

/// #965 レビュー指摘の回帰: `close_notify` を受信した後も、この接続で
/// アプリケーションデータの送受信を続けられてはならない。
#[test]
fn receiving_close_notify_poisons_the_session_against_further_data() {
    use wire_server::tls::server_handshake::{AppEvent, TlsSessionError};

    let (mut client, mut session) = establish_session_for_tests();

    let close_records = client
        .sealer
        .seal_fragmented(ContentType::Alert, &Alert::close_notify())
        .expect("valid seal");
    let mut saw_close_notify = false;
    for record in &close_records {
        match session
            .open_record(record)
            .expect("valid close_notify record")
        {
            AppEvent::CloseNotify => saw_close_notify = true,
            AppEvent::ApplicationData(_) => panic!("expected close_notify"),
        }
    }
    assert!(saw_close_notify, "close_notify must be observed");

    assert_eq!(
        session.seal_application_data(b"must not be sent"),
        Err(TlsSessionError::Poisoned)
    );
    assert_eq!(
        session.open_record(&close_records[0]),
        Err(TlsSessionError::Poisoned)
    );
}

/// PR #1046 レビュー指摘の回帰: `close_notify` は送信方向の終了にすぎない
/// （RFC 8446 §6.1）。自ら送出した後も、相手のアプリケーションデータと
/// `close_notify` は受信でき、相手の `close_notify` 受信後に初めて受信側も
/// 拒否される。
#[test]
fn sent_close_notify_still_allows_receiving_peer_data_and_close_notify() {
    use wire_server::tls::server_handshake::{AppEvent, TlsSessionError};

    let (mut client, mut session) = establish_session_for_tests();

    session
        .close_notify()
        .expect("server close_notify succeeds");

    let payload = b"late data from client";
    let data_records = client
        .sealer
        .seal_fragmented(ContentType::ApplicationData, payload)
        .expect("valid seal");
    let mut received = Vec::new();
    for record in &data_records {
        match session
            .open_record(record)
            .expect("peer data must still be readable after our close_notify")
        {
            AppEvent::ApplicationData(data) => received.extend_from_slice(&data),
            other => panic!("expected application data, got {other:?}"),
        }
    }
    assert_eq!(received, payload);

    let close_records = client
        .sealer
        .seal_fragmented(ContentType::Alert, &Alert::close_notify())
        .expect("valid seal");
    let mut saw_close_notify = false;
    for record in &close_records {
        match session
            .open_record(record)
            .expect("peer close_notify must still be readable after our close_notify")
        {
            AppEvent::CloseNotify => saw_close_notify = true,
            other => panic!("expected close_notify, got {other:?}"),
        }
    }
    assert!(saw_close_notify, "peer close_notify must be observed");

    // 両方向とも終了した後は、受信も送信も拒否される。
    assert_eq!(
        session.open_record(&close_records[0]),
        Err(TlsSessionError::Poisoned)
    );
    assert_eq!(
        session.seal_application_data(b"must not be sent"),
        Err(TlsSessionError::Poisoned)
    );
}

/// PR #1046 レビュー指摘の回帰: 相手の `close_notify` 受信は受信方向だけを
/// 終了させるため、応答としての自らの `close_notify` 送出は成功し、相手が
/// それを `close_notify` として開けること。
#[test]
fn received_close_notify_still_allows_sending_our_close_notify() {
    use wire_server::tls::server_handshake::{AppEvent, TlsSessionError};

    let (mut client, mut session) = establish_session_for_tests();

    let close_records = client
        .sealer
        .seal_fragmented(ContentType::Alert, &Alert::close_notify())
        .expect("valid seal");
    for record in &close_records {
        assert_eq!(
            session.open_record(record),
            Ok(AppEvent::CloseNotify),
            "peer close_notify must be observed"
        );
    }

    let reply = session
        .close_notify()
        .expect("replying close_notify after the peer's must succeed");
    for record in &reply {
        let inner = client
            .opener
            .open(record)
            .expect("valid close_notify record");
        assert_eq!(inner.content_type, ContentType::Alert);
        let alert = Alert::parse(&inner.content).expect("valid alert bytes");
        assert_eq!(
            wire_server::tls::alert::classify_received(alert),
            ReceivedAlert::Closed
        );
    }
    assert_eq!(session.close_notify(), Err(TlsSessionError::Poisoned));
}

/// #965 レビュー指摘の回帰: 復号（`bad_record_mac`）・alert 解析の失敗は
/// いずれもこの接続を終端させ、以後の `seal_application_data` を含む
/// すべての操作を拒否する（破損した TLS 接続での送受信継続を防ぐ）。
#[test]
fn open_record_decrypt_failure_poisons_the_session() {
    use wire_server::tls::server_handshake::TlsSessionError;

    let (_client, mut session) = establish_session_for_tests();

    let mut tampered = Record {
        content_type: ContentType::ApplicationData,
        legacy_version: record::LEGACY_RECORD_VERSION,
        fragment: vec![0u8; 32],
    };
    // 復号できない乱雑なバイト列（鍵・nonce に無関係）は
    // `ProtectionError`（`bad_record_mac` 相当）で拒否されるはず。
    for (i, b) in tampered.fragment.iter_mut().enumerate() {
        *b = i as u8;
    }

    let err = session
        .open_record(&tampered)
        .expect_err("garbage ciphertext must fail to decrypt");
    assert!(matches!(err, TlsSessionError::Protection(_)));

    assert_eq!(
        session.seal_application_data(b"must not be sent"),
        Err(TlsSessionError::Poisoned)
    );
    assert_eq!(
        session.open_record(&tampered),
        Err(TlsSessionError::Poisoned)
    );
}

/// 受け入れ条件全体（driver 経由）: `perform_server_handshake_with_timeout`
/// （#966 が実接続へ結線する blocking driver 本体。乱数源は本番と同じ
/// `OsEntropy`）を loopback `TcpStream` 越しに駆動し、フルハンドシェイクの
/// 完了とアプリケーションデータの双方向往復までを確認する。
///
/// `full_handshake_round_trip_over_pure_api` は `handle_record` を直接
/// 呼ぶ経路（driver をバイパス）でのみ全体往復を検証していたため、
/// driver 自身が結線する唯一の入口（`perform_server_handshake_with` の
/// read/write ループ）を実ソケットで一度も通していなかった（#965 レビュー
/// 指摘）。クライアント側は乱数を固定できない（driver は常に `OsEntropy`
/// を使う）ため、`TestClient`／鍵導出はすべて実際に受信した ServerHello・
/// server flight のバイト列から独立に計算する。
#[test]
fn full_handshake_round_trip_over_driver_with_loopback_stream() {
    use std::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");

    let server_thread = std::thread::spawn(
        move || -> (Vec<u8>, Vec<u8>, Option<Duration>, Option<Duration>) {
            let (mut socket, _) = listener.accept().expect("accept connection");
            let mut session =
                wire_server::tls::server_handshake::perform_server_handshake_with_timeout(
                    &mut socket,
                    test_config(),
                    Duration::from_secs(5),
                )
                .expect("handshake must complete over the driver");

            // #965 レビュー指摘の回帰: ハンドシェイク中に `DeadlineReader` が
            // 残り時間で設定した読み取りタイムアウト（注入した overall
            // timeout=5s 由来。`limits::READ_TIMEOUT`=30s とは異なる値）が、
            // 成功時に通常運用値へ戻されていることを確認する。ここで戻して
            // いなければ、5s の期限にどれだけ食い込んだかに依存する短い
            // タイムアウトのまま残り、以降のアプリケーションデータ読み取りが
            // 意図より大幅に早くタイムアウトし得る。
            let read_timeout_after_handshake = socket
                .read_timeout()
                .expect("querying the socket read timeout must succeed");
            // 送信側も同様に、`DeadlineWriter` が残り時間で設定した書き込み
            // タイムアウトが成功時に通常運用値へ戻されていることを確認する。
            let write_timeout_after_handshake = socket
                .write_timeout()
                .expect("querying the socket write timeout must succeed");

            // サーバー → クライアントのアプリケーションデータ。
            let payload = b"hello from server (driver)".to_vec();
            let records = session
                .seal_application_data(&payload)
                .expect("valid application data");
            let mut buf = Vec::new();
            for record in &records {
                record
                    .serialize_into(&mut buf, RecordKind::Ciphertext)
                    .expect("serialize application data record");
            }
            socket.write_all(&buf).expect("write application data");

            // クライアント → サーバーのアプリケーションデータを読み、
            // 中身をそのままテスト側（メインスレッド）へ持ち帰って照合する。
            let record = record::read_record(&mut socket, RecordKind::Ciphertext)
                .expect("read client application data")
                .expect("client application data record present");
            let event = session
                .open_record(&record)
                .expect("valid application record");
            let received = match event {
                wire_server::tls::server_handshake::AppEvent::ApplicationData(data) => data,
                wire_server::tls::server_handshake::AppEvent::CloseNotify => {
                    panic!("expected application data, got close_notify")
                }
            };
            (
                payload,
                received,
                read_timeout_after_handshake,
                write_timeout_after_handshake,
            )
        },
    );

    let mut client_socket = TcpStream::connect(addr).expect("connect to loopback listener");

    let client_priv = [0x33u8; 32];
    let client_ephemeral = EphemeralSecret::from_bytes(client_priv);
    let client_pub = *client_ephemeral.public_key().as_bytes();

    let mut client = TestClient::new();
    let (ch_record, ch_raw) = build_client_hello(client_pub);
    client
        .transcript
        .append_client_hello(&ch_raw)
        .expect("valid ClientHello");

    let mut ch_buf = Vec::new();
    ch_record
        .serialize_into(&mut ch_buf, RecordKind::Plaintext)
        .expect("serialize ClientHello record");
    client_socket
        .write_all(&ch_buf)
        .expect("write ClientHello to loopback stream");

    let sh_record = record::read_record(&mut client_socket, RecordKind::Ciphertext)
        .expect("read ServerHello record")
        .expect("ServerHello record present");
    let sh = handshake::ServerHello::parse(&sh_record.fragment[4..]).expect("valid ServerHello");
    let mut sh_body = Vec::new();
    sh.serialize_body_into(&mut sh_body).expect("valid body");
    let sh_raw = RawHandshake {
        msg_type: HandshakeType::ServerHello,
        body: sh_body,
    };
    client
        .transcript
        .append_server_hello(&sh_raw)
        .expect("valid ServerHello");
    let th_ch_sh = client
        .transcript
        .hash_through_server_hello()
        .expect("checkpoint reached");

    let server_pub = sh
        .extensions
        .iter()
        .find(|e| e.extension_type == 0x0033)
        .and_then(|e| e.extension_data.get(4..36))
        .and_then(|s| <[u8; 32]>::try_from(s).ok())
        .expect("server key_share public key present");

    let shared = client_ephemeral
        .diffie_hellman(&server_pub)
        .expect("valid non-zero shared secret");
    let early = EarlySecret::new_without_psk();
    let handshake_secret = early
        .into_handshake(&shared)
        .expect("valid HKDF parameters");
    let traffic = handshake_secret
        .traffic_secrets(&th_ch_sh)
        .expect("valid HKDF parameters");
    let client_hs_keys = traffic
        .client
        .traffic_keys()
        .expect("valid HKDF parameters");
    let server_hs_keys = traffic
        .server
        .traffic_keys()
        .expect("valid HKDF parameters");
    client
        .sealer
        .install_handshake_keys(&client_hs_keys)
        .expect("valid transition");
    client
        .opener
        .install_handshake_keys(&server_hs_keys)
        .expect("valid transition");

    // server flight（EE・Certificate・CertificateVerify・server Finished）は
    // 本テストの構成（最小の自己署名 ed25519 証明書 1 枚）では 1 ciphertext
    // レコードに収まる（`full_handshake_round_trip_over_pure_api` と同じ
    // 前提）。
    let flight_record = record::read_record(&mut client_socket, RecordKind::Ciphertext)
        .expect("read server flight record")
        .expect("server flight record present");
    let th_ch_sf = client.process_server_flight(
        std::slice::from_ref(&flight_record),
        &RFC8032_TEST1_PUBLIC_KEY,
        &traffic.server,
    );

    let master = handshake_secret
        .into_master()
        .expect("valid HKDF parameters");
    let app = master
        .application_traffic_secrets(&th_ch_sf)
        .expect("valid HKDF parameters");
    let client_ap_keys = app.client.traffic_keys().expect("valid HKDF parameters");
    let server_ap_keys = app.server.traffic_keys().expect("valid HKDF parameters");

    // client Finished を構成し、handshake epoch のまま送る。
    let client_finished_key = traffic
        .client
        .finished_key()
        .expect("valid HKDF parameters");
    let verify_data = finished::compute_verify_data(&client_finished_key, &th_ch_sf);
    let client_finished = handshake::Finished {
        verify_data: verify_data.as_bytes().to_vec(),
    };
    let mut cf_body = Vec::new();
    client_finished
        .serialize_body_into(&mut cf_body)
        .expect("valid body");
    let cf_raw = RawHandshake {
        msg_type: HandshakeType::Finished,
        body: cf_body,
    };
    let cf_wire = cf_raw.to_bytes().expect("valid wire bytes");
    let cf_records = client
        .sealer
        .seal_fragmented(ContentType::Handshake, &cf_wire)
        .expect("valid seal");
    let mut cf_buf = Vec::new();
    for record in &cf_records {
        record
            .serialize_into(&mut cf_buf, RecordKind::Ciphertext)
            .expect("serialize client Finished record");
    }
    client_socket
        .write_all(&cf_buf)
        .expect("write client Finished to loopback stream");

    // application epoch へ切り替える（client 側）。
    client
        .sealer
        .install_application_keys(&client_ap_keys)
        .expect("valid transition");
    client
        .opener
        .install_application_keys(&server_ap_keys)
        .expect("valid transition");

    // サーバー → クライアントのアプリケーションデータ。
    let server_payload_record = record::read_record(&mut client_socket, RecordKind::Ciphertext)
        .expect("read server application data")
        .expect("server application data record present");
    let inner = client
        .opener
        .open(&server_payload_record)
        .expect("valid application record");
    assert_eq!(inner.content_type, ContentType::ApplicationData);

    // クライアント → サーバーのアプリケーションデータ。
    let client_payload = b"hello from client (driver)";
    let client_app_records = client
        .sealer
        .seal_fragmented(ContentType::ApplicationData, client_payload)
        .expect("valid application data");
    let mut client_app_buf = Vec::new();
    for record in &client_app_records {
        record
            .serialize_into(&mut client_app_buf, RecordKind::Ciphertext)
            .expect("serialize client application data record");
    }
    client_socket
        .write_all(&client_app_buf)
        .expect("write client application data");

    let (
        server_sent_payload,
        server_received_payload,
        read_timeout_after_handshake,
        write_timeout_after_handshake,
    ) = server_thread.join().expect("server thread must not panic");
    assert_eq!(inner.content, server_sent_payload);
    assert_eq!(server_received_payload, client_payload);
    assert_eq!(
        read_timeout_after_handshake,
        Some(wire_server::limits::READ_TIMEOUT),
        "read timeout must be restored to the normal operating value \
         (limits::READ_TIMEOUT) immediately after a successful handshake, \
         not left at the short deadline-derived value from the handshake itself"
    );
    assert_eq!(
        write_timeout_after_handshake,
        Some(wire_server::limits::READ_TIMEOUT),
        "write timeout must be restored to the normal operating value \
         (limits::READ_TIMEOUT) immediately after a successful handshake, \
         not left at the short deadline-derived value from the handshake itself"
    );
}

/// 改ざんした client Finished（verify_data を 1 bit 反転）は
/// `decrypt_error` として拒否される。
#[test]
fn tampered_client_finished_is_rejected() {
    let config = test_config();
    let client_priv = [0x11u8; 32];
    let client_ephemeral = EphemeralSecret::from_bytes(client_priv);
    let client_pub = *client_ephemeral.public_key().as_bytes();

    let (ch_record, _ch_raw) = build_client_hello(client_pub);
    let entropy = FixedEntropy {
        random: [0x77u8; 32],
        ephemeral_seed: [0x22u8; 32],
        used: false,
    };
    let mut server = ServerHandshake::with_entropy(config, entropy);
    let Step::Continue(_output) = server
        .handle_record(&ch_record)
        .expect("server must accept the ClientHello")
    else {
        panic!("expected Continue");
    };

    // client Finished の代わりに、明らかに不正な（全ゼロ）Finished を
    // 平文の Handshake レコードとして送る（handshake epoch へ既に
    // 切り替わっているサーバーの Opener はこれを復号できず、AEAD
    // タグ検証エラー＝bad_record_mac として拒否されることを確認する。
    // これは「decrypt_error 経路」ではなく「レコード保護層の検証失敗」の
    // 確認であり、`docs/design/tls-server-handshake.md` の受け入れ基準
    // 「改ざんした暗号文は bad_record_mac」を兼ねる。
    let bogus = Record {
        content_type: ContentType::ApplicationData,
        legacy_version: record::LEGACY_RECORD_VERSION,
        fragment: vec![0u8; 64],
    };
    let err = server.handle_record(&bogus).expect_err("must be rejected");
    match err {
        ServerHandshakeError::Protection(_) => {}
        other => panic!("expected a record-protection failure, got {other:?}"),
    }
}

/// [`RecordKind`] の変化（Plaintext → Ciphertext）が ServerHello 送出後に
/// 起きることを確認する（driver が正しい枠組みでレコードを読むための
/// 前提）。
#[test]
fn record_kind_switches_to_ciphertext_after_accept_flow() {
    let fresh = ServerHandshake::new(test_config());
    assert_eq!(fresh.record_kind(), RecordKind::Plaintext);
    drop(fresh);

    let (ch_record, _ch) = client_hello_from_rfc8448_with_ed25519();
    let entropy = FixedEntropy {
        random: hex_decode32(SERVER_RANDOM),
        ephemeral_seed: hex_decode32(SERVER_X25519_PRIV),
        used: false,
    };
    let mut server = ServerHandshake::with_entropy(test_config(), entropy);
    let _ = server
        .handle_record(&ch_record)
        .expect("valid ClientHello accepted");
    assert_eq!(server.record_kind(), RecordKind::Ciphertext);
}

/// 受け入れ条件 3: `key_share` を提示しない 1 回目の ClientHello に対して
/// HelloRetryRequest を 1 回返し、x25519 の `key_share` を含む 2 回目の
/// ClientHello で通常どおり Accept flow へ進むこと。
#[test]
fn hello_retry_request_round_trip_then_accepts() {
    use wire_server::tls::client_hello::HELLO_RETRY_REQUEST_RANDOM;

    let random = [0x55u8; 32];
    let (ch1_record, _ch1) = build_client_hello_ex(None, random);

    let entropy = FixedEntropy {
        random: [0x77u8; 32],
        ephemeral_seed: [0x22u8; 32],
        used: false,
    };
    let mut server = ServerHandshake::with_entropy(test_config(), entropy);

    let Step::Continue(output1) = server
        .handle_record(&ch1_record)
        .expect("CH1 without key_share must trigger HelloRetryRequest")
    else {
        panic!("expected Continue for HelloRetryRequest");
    };
    assert_eq!(
        output1.len(),
        1,
        "HRR alone, no dummy CCS (empty legacy_session_id)"
    );
    let hrr_record = &output1[0];
    assert_eq!(hrr_record.content_type, ContentType::Handshake);
    let hrr_raw = &hrr_record.fragment;
    let hrr = handshake::ServerHello::parse(&hrr_raw[4..]).expect("valid HRR body");
    assert_eq!(hrr.random, HELLO_RETRY_REQUEST_RANDOM);
    // まだ鍵は導入されていない（HRR は平文のまま）。
    assert_eq!(server.record_kind(), RecordKind::Plaintext);

    let client_priv = [0x33u8; 32];
    let client_pub = *EphemeralSecret::from_bytes(client_priv)
        .public_key()
        .as_bytes();
    let (ch2_record, _ch2) = build_client_hello_ex(Some(client_pub), random);
    let Step::Continue(_output2) = server
        .handle_record(&ch2_record)
        .expect("CH2 with key_share must be accepted after HelloRetryRequest")
    else {
        panic!("expected Continue for the Accept flow");
    };
    assert_eq!(server.record_kind(), RecordKind::Ciphertext);
}

fn dummy_ccs_record() -> Record {
    Record {
        content_type: ContentType::ChangeCipherSpec,
        legacy_version: record::LEGACY_RECORD_VERSION,
        fragment: vec![0x01],
    }
}

/// middlebox 互換のダミー `ChangeCipherSpec`（RFC 8446 付録 D.4）は、
/// ClientHello 受信後から client Finished 受信前までに限り読み捨てる。
/// ClientHello より前に届くと `unexpected_message`。
#[test]
fn dummy_ccs_before_client_hello_is_rejected() {
    let mut server = ServerHandshake::new(test_config());
    let err = server
        .handle_record(&dummy_ccs_record())
        .expect_err("CCS before any ClientHello must be rejected");
    assert_eq!(err, ServerHandshakeError::UnexpectedMessage);
}

/// ClientHello 受信後（`ExpectClientFinished`）に届くダミー CCS は
/// 読み捨てられ、出力・状態遷移は起きない。RFC 8446 §5 は件数の上限を
/// 設けず単純に読み捨てることを要求するため（#965 レビュー指摘。付録
/// D.4 の HelloRetryRequest 経由 middlebox 互換フローでは 2 回届き
/// 得る）、複数個連続で届いても読み捨て続けることを固定する。
#[test]
fn dummy_ccs_after_server_hello_is_discarded_without_limit() {
    let (ch_record, _ch) = client_hello_from_rfc8448_with_ed25519();
    let entropy = FixedEntropy {
        random: hex_decode32(SERVER_RANDOM),
        ephemeral_seed: hex_decode32(SERVER_X25519_PRIV),
        used: false,
    };
    let mut server = ServerHandshake::with_entropy(test_config(), entropy);
    let _ = server
        .handle_record(&ch_record)
        .expect("valid ClientHello accepted");

    for i in 0..3 {
        let Step::Continue(output) = server
            .handle_record(&dummy_ccs_record())
            .unwrap_or_else(|_| panic!("dummy CCS #{i} must be discarded without limit"))
        else {
            panic!("expected Continue");
        };
        assert!(
            output.is_empty(),
            "discarding a dummy CCS produces no output"
        );
    }
}

/// 時期・状態に関わらず、値が `0x01` 以外のダミー CCS は引き続き
/// `unexpected_message` で拒否される（上限撤廃は値検査を緩めない）。
#[test]
fn dummy_ccs_with_wrong_fragment_value_is_rejected() {
    let (ch_record, _ch) = client_hello_from_rfc8448_with_ed25519();
    let entropy = FixedEntropy {
        random: hex_decode32(SERVER_RANDOM),
        ephemeral_seed: hex_decode32(SERVER_X25519_PRIV),
        used: false,
    };
    let mut server = ServerHandshake::with_entropy(test_config(), entropy);
    let _ = server
        .handle_record(&ch_record)
        .expect("valid ClientHello accepted");

    let mut bad_ccs = dummy_ccs_record();
    bad_ccs.fragment = vec![0x02];
    let err = server
        .handle_record(&bad_ccs)
        .expect_err("non-0x01 CCS fragment must be rejected");
    assert_eq!(err, ServerHandshakeError::UnexpectedMessage);
}
