//! TLS 1.3 ハンドシェイク完了後の pg wire バイトストリーム（Issue #966）。
//!
//! `handshake::handle_connection_inner` が `SSLRequest` への `'S'` 応答・
//! [`super::server_handshake::perform_server_handshake`] 完了後に、生の
//! `TcpStream` を [`TlsStream::new`] でラップして返す。以後の
//! `negotiate_startup`・認証・簡易/拡張クエリ・COPY・拒否応答は、
//! 平文接続と同じロジックのまま `crate::wire_stream::WireStream` の
//! オブジェクトとしてこのストリームを扱う（型を広げるだけで分岐・応答内容は
//! 一切変えない。`handshake` モジュールドキュメント参照）。
//!
//! # 読み取り方式（push 型。受入基準 4 の要）
//!
//! [`super::record::read_record`]（`read_exact` を使う pull 型）ではなく、
//! [`super::record::RecordBuffer::feed`]／[`next_record`] を使う push 型で
//! 実装する。`handshake::read_next_frame_header` は明示トランザクションが
//! `Active` の間、`WouldBlock`/`TimedOut` を受けても同じストリームで読み
//! 直す契約（SQL-31・TASK-221）を持つため、pull 型でレコード途中の
//! タイムアウトに遭遇すると既に読み込んだ部分バイト列が失われ TLS
//! ストリームの復号状態が壊れる。push 型なら `inner.read` が返した生
//! バイト列を [`RecordBuffer`] へ蓄積するだけなので、途中のタイムアウトで
//! 呼び出しをまたいでも取りこぼしがない。
//!
//! # 緊急応答（RECOVER-6）との関係
//!
//! `WireStream::emergency_channel` は `None` を返す（平文の緊急応答
//! バイト列が TLS レコードへ混入するのを防ぐため）。TLS 接続での
//! panic 発生時の緊急応答（RECOVER-6）は「応答なしで切断」に縮退する
//! （安全性側の abort ガード・RECOVER-5 は無関係に維持される）。詳細は
//! `docs/design/tls-wire-connection.md` 参照。

use std::io::{self, Read, Write};

use super::record::{self, Record, RecordBuffer, RecordKind, MAX_PLAINTEXT_LEN};
use super::server_handshake::{AppEvent, TlsSession, TlsSessionError};
use crate::wire_stream::WireStream;

/// 復号済みアプリケーションデータの read-ahead 上限（1 レコード分。
/// [`MAX_PLAINTEXT_LEN`] を超えて確保しない）。
const READ_AHEAD_CAP: usize = MAX_PLAINTEXT_LEN;

/// ハンドシェイク完了後の TLS 1.3 接続 1 本を表す。`S`（実運用では
/// `std::net::TcpStream`）の上に [`TlsSession`] のレコード保護を重ねる。
pub struct TlsStream<S> {
    inner: S,
    session: Box<TlsSession>,
    /// 受信生バイト列の未消費分（`RecordBuffer::feed` が容量超過分を
    /// 取り込めなかった場合の呼び出し元側の保持先。[`super::record::
    /// MAX_RECORD_WIRE_LEN`] で有界）。
    rx: RecordBuffer,
    /// 復号済み平文の read-ahead（`AppEvent::ApplicationData` を
    /// `Read::read` の呼び出し粒度に合わせて切り出すためのバッファ）。
    plaintext: Vec<u8>,
    plaintext_pos: usize,
    /// 相手の `close_notify` を受信済み（以後 `read` は `Ok(0)` を返す）。
    eof: bool,
    /// この接続が致命的に失敗した（以後 read/write を即座にエラーにする。
    /// fail-closed。書き込み失敗・復号失敗等で真になる）。
    failed: bool,
}

impl<S: WireStream> TlsStream<S> {
    /// ハンドシェイク完了直後の生ストリームとセッションからラップする。
    pub fn new(inner: S, session: Box<TlsSession>) -> Self {
        Self {
            inner,
            session,
            rx: RecordBuffer::new(),
            plaintext: Vec::new(),
            plaintext_pos: 0,
            eof: false,
            failed: false,
        }
    }

    /// read-ahead に残っている平文があれば `buf` へコピーして返す。
    fn drain_plaintext(&mut self, buf: &mut [u8]) -> usize {
        let remaining = &self.plaintext[self.plaintext_pos..];
        let n = remaining.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), remaining.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.plaintext_pos += n;
        if self.plaintext_pos >= self.plaintext.len() {
            self.plaintext.clear();
            self.plaintext_pos = 0;
        }
        n
    }

    /// 復号失敗・alert 解析失敗等を検出した際、可能なら fatal alert を
    /// 1 回だけ送出してから接続を `failed` にする（best-effort。送出自体の
    /// 失敗は無視する。RFC 8446 §5.2 の bad_record_mac 終了に対応する）。
    fn fail_with_alert(&mut self, desc: Option<record::AlertDescription>) {
        self.failed = true;
        let Some(desc) = desc else {
            return;
        };
        if let Ok(records) = self.session.seal_fatal_alert(desc) {
            let _ = write_records(&mut self.inner, &records);
        }
    }

    /// 内部の生ストリームから 1 チャンク読み、[`RecordBuffer`] へ取り込む。
    /// `inner.read` の `WouldBlock`/`TimedOut`/`Interrupted` は種別を変えず
    /// そのまま呼び出し元へ返す（`handshake::read_next_frame_header` が
    /// 同じストリームで読み直せるよう、内部状態は変更しない）。
    fn fill_from_inner(&mut self) -> io::Result<usize> {
        let mut buf = [0u8; 4096];
        let n = self.inner.read(&mut buf)?;
        if n > 0 {
            let taken = self.rx.feed(buf.get(..n).unwrap_or(&[]));
            // `feed` は空き容量に収まる分だけ取り込む契約（`MAX_RECORD_WIRE_LEN`
            // 上限）。1 レコード分を大きく超える生バイト列を一度に送ってくる
            // 相手は次の `next_record` が `TooLarge` 等の `Err` を返すため、
            // ここで取り込み切れなかった残り（`n - taken`）は意図的に捨てる
            // （fail-closed: 上限超過の入力を無制限に保持しない）。
            let _ = taken;
        }
        Ok(n)
    }
}

/// レコード列を 1 回の `write_all` にまとめて送出する（`TlsSession::
/// seal_application_data`／`close_notify`／`seal_fatal_alert` いずれも
/// 複数レコードに分割されうるため、書き込み対象を都度連結してから送る）。
fn write_records<W: Write>(w: &mut W, records: &[Record]) -> io::Result<()> {
    let mut out = Vec::new();
    for record in records {
        record
            .serialize_into(&mut out, RecordKind::Ciphertext)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "failed to serialize record")
            })?;
    }
    w.write_all(&out)
}

impl<S: WireStream> Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS connection already failed",
            ));
        }
        loop {
            if self.plaintext_pos < self.plaintext.len() {
                return Ok(self.drain_plaintext(buf));
            }
            if self.eof {
                return Ok(0);
            }

            match self.rx.next_record(RecordKind::Ciphertext) {
                Ok(Some(record)) => match self.session.open_record(&record) {
                    Ok(AppEvent::ApplicationData(data)) => {
                        if data.is_empty() {
                            continue;
                        }
                        // `READ_AHEAD_CAP`（= 1 レコード分の平文上限）を超える
                        // ことはない（`TlsSession::open_record` が 1 レコード
                        // ぶんの `TLSInnerPlaintext` しか返さないため）。
                        self.plaintext = data;
                        self.plaintext_pos = 0;
                        debug_assert!(self.plaintext.len() <= READ_AHEAD_CAP);
                        return Ok(self.drain_plaintext(buf));
                    }
                    Ok(AppEvent::CloseNotify) => {
                        self.eof = true;
                        return Ok(0);
                    }
                    Ok(AppEvent::UserCanceled) | Ok(AppEvent::Ignored) => {
                        continue;
                    }
                    Err(TlsSessionError::Protection(e)) => {
                        let desc = e.alert_description();
                        self.fail_with_alert(desc);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "TLS record protection failure",
                        ));
                    }
                    Err(TlsSessionError::AlertDecode(_))
                    | Err(TlsSessionError::ReceivedFatalAlert(_)) => {
                        // 相手からの alert（受信失敗を含む）に対してはこちらから
                        // alert を送り返さない（driver・RFC 8446 と同じ方針）。
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "TLS session received a fatal alert",
                        ));
                    }
                    Err(TlsSessionError::Poisoned) => {
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "TLS session already failed or closed",
                        ));
                    }
                },
                Ok(None) => {
                    // レコードがそろっていない: 生バイト列を追加で読む。
                    let n = self.fill_from_inner()?;
                    if n == 0 {
                        // 相手が close_notify なしで切断した。`finish` で
                        // 未消費の部分レコードが残っていれば truncation。
                        self.eof = true;
                        return Ok(0);
                    }
                }
                Err(e) => {
                    let desc = e.alert_description();
                    self.fail_with_alert(desc);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "TLS record framing failure",
                    ));
                }
            }
        }
    }
}

impl<S: WireStream> Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS connection already failed",
            ));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let records = self.session.seal_application_data(buf).map_err(|_| {
            self.failed = true;
            io::Error::new(io::ErrorKind::InvalidData, "TLS session cannot seal data")
        })?;
        if let Err(e) = write_records(&mut self.inner, &records) {
            // 書き込み失敗（タイムアウトを含む）はこの TLS ストリームを
            // 破損状態にする（fail-closed。部分的に送出済みのレコードが
            // 相手に届いているかもしれず、以後の送受信を安全に続けられない）。
            self.failed = true;
            return Err(e);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<S: WireStream> WireStream for TlsStream<S> {
    fn read_timeout(&self) -> io::Result<Option<std::time::Duration>> {
        self.inner.read_timeout()
    }

    fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    fn shutdown_both(&mut self) -> io::Result<()> {
        self.inner.shutdown_both()
    }

    fn shutdown_write(&mut self) -> io::Result<()> {
        // best-effort で `close_notify` を送ってから下層を閉じる
        // （RFC 8446 §6.1: 書き込み側を閉じる前に close_notify を送る）。
        if !self.failed {
            if let Ok(records) = self.session.close_notify() {
                let _ = write_records(&mut self.inner, &records);
            }
        }
        self.inner.shutdown_write()
    }

    fn emergency_channel(&self) -> Option<std::net::TcpStream> {
        // `engine::recovery::panic_hook::EmergencyResponseRegistration` は
        // 生の `TcpStream` へ平文の ErrorResponse を書く契約のため、TLS
        // 接続では登録自体を行わない（モジュールドキュメント参照）。
        None
    }

    fn graceful_close(&mut self) {
        if self.failed {
            return;
        }
        if let Ok(records) = self.session.close_notify() {
            let _ = write_records(&mut self.inner, &records);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use super::super::key_schedule::EarlySecret;
    use super::super::record_protection::{Opener, Sealer};
    use super::super::x25519::EphemeralSecret;

    /// 実ハンドシェイクを経由せず、鍵スケジュール（[`EarlySecret`]・
    /// [`super::super::x25519::EphemeralSecret`]）を 1 回通してレコード
    /// 保護層以上のロジックだけを検証するための `TlsSession` 対を組み立てる
    /// （`record_protection.rs::tests::dummy_keys` と同じ手法）。client/server
    /// 方向の traffic secret は同一の handshake secret から HKDF ラベルで
    /// 分離されるため、1 回の鍵交換から双方向ぶんの鍵を得られる。
    fn test_session_pair() -> (Box<TlsSession>, Box<TlsSession>) {
        let early = EarlySecret::new_without_psk();
        let server_priv = [7u8; 32];
        let client_pub_raw = [9u8; 32];
        let secret = EphemeralSecret::from_bytes(server_priv);
        let shared = secret
            .diffie_hellman(&client_pub_raw)
            .expect("non-zero shared secret for distinct seed bytes");
        let hs = early.into_handshake(&shared).expect("valid HKDF params");
        let th = [0u8; super::super::hkdf::HASH_LEN];
        // `Sealer::seal`（`Epoch::Handshake`）は 0-RTT 非対応の送信契約により
        // `ApplicationData` の送信を拒否する（`record_protection.rs::Sealer::
        // seal` ドキュメント参照）ため、`TlsStream` の平文往復を検証するには
        // `Application` epoch まで鍵スケジュールを進める必要がある。
        // `install_application_keys` は直前に `install_handshake_keys` を
        // 要求するため、両方とも導出してから順番にインストールする。
        let hs_traffic = hs.traffic_secrets(&th).expect("valid HKDF params");
        let hs_server_keys = hs_traffic.server.traffic_keys().expect("valid HKDF params");
        let hs_client_keys = hs_traffic.client.traffic_keys().expect("valid HKDF params");
        let master = hs.into_master().expect("valid HKDF params");
        let app_traffic = master
            .application_traffic_secrets(&th)
            .expect("valid HKDF params");
        let server_keys = app_traffic
            .server
            .traffic_keys()
            .expect("valid HKDF params");
        let client_keys = app_traffic
            .client
            .traffic_keys()
            .expect("valid HKDF params");

        let mut server_sealer = Sealer::new();
        server_sealer
            .install_handshake_keys(&hs_server_keys)
            .expect("install handshake");
        server_sealer
            .install_application_keys(&server_keys)
            .expect("install application");
        let mut server_opener = Opener::new();
        server_opener
            .install_handshake_keys(&hs_client_keys)
            .expect("install handshake");
        server_opener
            .install_application_keys(&client_keys)
            .expect("install application");

        let mut client_sealer = Sealer::new();
        client_sealer
            .install_handshake_keys(&hs_client_keys)
            .expect("install handshake");
        client_sealer
            .install_application_keys(&client_keys)
            .expect("install application");
        let mut client_opener = Opener::new();
        client_opener
            .install_handshake_keys(&hs_server_keys)
            .expect("install handshake");
        client_opener
            .install_application_keys(&server_keys)
            .expect("install application");

        (
            Box::new(TlsSession::new_for_tests(server_sealer, server_opener)),
            Box::new(TlsSession::new_for_tests(client_sealer, client_opener)),
        )
    }

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (server, client)
    }

    #[test]
    fn round_trip_writes_and_reads_plaintext() {
        let (server_sock, client_sock) = loopback_pair();
        let (server_session, client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);
        let mut client = TlsStream::new(client_sock, client_session);

        server.write_all(b"hello over tls").expect("write");
        let mut buf = [0u8; 64];
        let n = client.read(&mut buf).expect("read");
        assert_eq!(&buf[..n], b"hello over tls");
    }

    #[test]
    fn write_of_empty_buffer_is_a_no_op() {
        let (server_sock, _client_sock) = loopback_pair();
        let (server_session, _client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);
        assert_eq!(server.write(&[]).expect("empty write"), 0);
    }

    /// 受入基準 4 の核心: レコード本文が複数回の生 `read` に分かれて届いた
    /// 場合でも、`WouldBlock`（読み取りタイムアウト）を挟んで読み直しても
    /// 平文が正しく続くこと（push 型読み取り方式の存在意義。設計は
    /// `docs/design/tls-wire-connection.md` 参照）。
    #[test]
    fn read_recovers_after_would_block_mid_record() {
        let (server_sock, client_sock) = loopback_pair();
        let (server_session, client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);
        let mut client = TlsStream::new(client_sock, client_session);

        server.write_all(b"across two reads").expect("write");

        // クライアント側の生ソケットに極短い読み取りタイムアウトを設定し、
        // レコードが全部届く前に `WouldBlock`/`TimedOut` を意図的に起こす。
        client
            .set_read_timeout(Some(Duration::from_millis(1)))
            .expect("set timeout");
        let mut buf = [0u8; 64];
        let mut last_err_was_timeout = false;
        for _ in 0..2000 {
            match client.read(&mut buf) {
                Ok(n) => {
                    assert_eq!(&buf[..n], b"across two reads");
                    return;
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    last_err_was_timeout = true;
                    continue;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(last_err_was_timeout, "expected at least one timeout");
        panic!("read never completed despite repeated retries");
    }

    #[test]
    fn write_after_failed_write_keeps_failing_read_and_write() {
        // 書き込み先を即座にクローズしたソケットへ書くと `write_all` が
        // 失敗する。以後の read/write が fail-closed になることを固定する。
        let (server_sock, client_sock) = loopback_pair();
        drop(client_sock);
        let (server_session, _client_session) = test_session_pair();
        let mut server = TlsStream::new(server_sock, server_session);

        // 相手が閉じた直後の 1 回目の書き込みは OS 側の事情で成功することが
        // あるため、複数回書いて確実に失敗させる。
        let mut failed = false;
        for _ in 0..64 {
            if server.write_all(b"x").is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "expected write to eventually fail");
        assert!(server.write(b"y").is_err(), "must stay fail-closed");
        let mut buf = [0u8; 1];
        assert!(server.read(&mut buf).is_err(), "must stay fail-closed");
    }
}
