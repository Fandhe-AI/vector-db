//! TCP 接続の受け付け・bind アドレスの loopback 検証を担う。
//!
//! `main.rs::run_server` から呼ばれる（`tests/` からの結合テストが同じ挙動を
//! 直接検証できるよう lib.rs 経由で公開する）。responsibility はソケットレベルの
//! 防御（loopback 限定・接続の受理／拒否ループ）に限り、同時接続数の有界化・
//! 読み取りタイムアウトの契約値は [`crate::limits`]（TASK-69・WIRE-5, WIRE-6）へ、
//! wire プロトコルそのものの解釈は [`crate::handshake::handle_connection`] へ
//! それぞれ委譲する。
//!
//! 対応: TASK-67 の review 是正（loopback bind の TOCTOU 排除）。TLS（TASK-72・
//! WIRE-9）が実装されるまで非ループバック bind を拒否する（[`bind_loopback`]）。

use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use crate::auth::UserStore;
use crate::limits::{self, ConnectionLimiter};

/// `bind_addr` を解決し、すべてのアドレスが loopback であることを検証したうえで
/// 解決済みの [`SocketAddr`] 列を返す。TLS（TASK-72・WIRE-9）が実装されるまで、
/// cleartext password 認証の平文パスワードを非ループバックの通信路へ公開しない
/// （fail-closed）。
///
/// 呼び出し元は返り値の `SocketAddr`（数値アドレス）へ直接 bind すること。
/// `bind_addr`（文字列）を検証後に再度 `TcpListener::bind` へ渡すと、ホスト名の
/// 場合は DNS 解決がもう一度走り、検証時と bind 時で異なるアドレスへ解決される
/// TOCTOU（検証時 loopback・bind 時に外部アドレス）が成立しうる（review 指摘）。
/// [`bind_loopback`] がこの契約を守った単一の入口を提供する。
fn validate_loopback_bind(bind_addr: &str) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = bind_addr
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve bind address {bind_addr}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!(
            "bind address {bind_addr} did not resolve to any socket address"
        ));
    }
    for addr in &addrs {
        if !addr.ip().is_loopback() {
            return Err(format!(
                "refusing to bind non-loopback address {addr}: cleartext password \
                 authentication is not yet protected by TLS (TASK-72/WIRE-9); \
                 bind to a loopback address (e.g. 127.0.0.1) or place a trusted TLS \
                 terminator in front of this listener"
            ));
        }
    }
    Ok(addrs)
}

/// `bind_addr` の loopback 検証と実際の bind を 1 つの入口にまとめる。
/// `main.rs::run_server` はこの関数だけを呼び、`TcpListener::bind` を直接
/// 呼ばない（[`validate_loopback_bind`] のドキュメント参照。文字列の再解決による
/// TOCTOU を構造的に作らないための唯一の bind 経路とする）。
pub fn bind_loopback(bind_addr: &str) -> Result<TcpListener, String> {
    let addrs = validate_loopback_bind(bind_addr)?;
    // `&[SocketAddr]` も `ToSocketAddrs` を実装するが、これは単に列挙を返すだけで
    // DNS 解決は発生しない（数値アドレスなので再解決の余地がない）。
    TcpListener::bind(addrs.as_slice())
        .map_err(|e| format!("failed to bind {bind_addr} ({addrs:?}): {e}"))
}

/// 接続受け付けループ本体。1 接続 1 スレッドで処理するが、以下の防御を課す
/// （WIRE-5, WIRE-6。契約値・実装は [`crate::limits`] に集約）:
///
/// - `limiter` の枠を確保できない接続は [`crate::handshake::handle_connection`] へ
///   進ませず、スレッドも生成せずに [`limits::reject_too_many_connections`] で
///   `53300`（too_many_connections）の ErrorResponse を返してから即座にクローズする
///   （枠を使い切った状態でスレッドを積み増さない）
/// - 受理した接続には読み取り・書き込み双方に `read_timeout` を一度だけ設定してから
///   ハンドシェイクへ渡す（認証前後を問わず同一値を維持する。WIRE-5）。超過時は
///   `handle_connection` が応答を書かずに `Err` を返し、スレッド終了で枠が解放される
///
/// 各スレッドの panic は `std::thread::spawn` の join ハンドルを無視することで
/// プロセス全体へは波及させない（他接続の継続稼働を優先する）。
pub fn accept_loop(
    listener: TcpListener,
    store: Arc<UserStore>,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) {
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wire-server: accept error: {e}");
                continue;
            }
        };

        let Some(permit) = limiter.try_acquire() else {
            // 上限超過: ハンドシェイクへ進ませず、スレッドを生成せずに `53300` を
            // 返してから即座にクローズする（WIRE-6）。ピアアドレス等の識別情報は
            // ログに出さない。
            eprintln!(
                "wire-server: rejecting connection: too many connections (active={}, max={})",
                limiter.active(),
                limiter.max()
            );
            limits::reject_too_many_connections(stream, limiter.max());
            continue;
        };

        if let Err(e) = limits::apply_read_timeout(&stream, read_timeout) {
            eprintln!("wire-server: failed to configure connection timeouts: {e}");
            // `permit` はここでスコープを抜けて解放される。
            continue;
        }

        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            // 接続処理中は `permit` を保持し続け、スレッド終了時（正常終了・panic
            // いずれも）に Drop で確実に枠を解放する。
            let _permit = permit;
            if let Err(e) = crate::handshake::handle_connection(stream, &store) {
                eprintln!("wire-server: connection error: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    #[test]
    fn validate_loopback_bind_accepts_loopback_addresses() {
        let addrs = validate_loopback_bind("127.0.0.1:0").expect("loopback address accepted");
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
        assert!(validate_loopback_bind("localhost:0").is_ok());
    }

    #[test]
    fn validate_loopback_bind_rejects_wildcard_address() {
        let result = validate_loopback_bind("0.0.0.0:5432");
        assert!(result.is_err());
        let msg = result.expect_err("must be rejected");
        assert!(msg.contains("TLS"), "message should explain why: {msg}");
    }

    /// レビュー指摘の再現ケース（TOCTOU）: `bind_loopback` は検証済みの数値
    /// `SocketAddr` へ直接 bind し、返された listener は実際に接続を受理できる
    /// こと（`bind_addr` 文字列を再度 `TcpListener::bind` へ渡す経路が残っていない
    /// ことの外形的確認）。
    #[test]
    fn bind_loopback_binds_to_validated_addr_and_accepts_connections() {
        let listener = bind_loopback("127.0.0.1:0").expect("loopback bind must succeed");
        let addr = listener.local_addr().expect("local addr");
        assert!(addr.ip().is_loopback());

        let client = TcpStream::connect(addr).expect("connect to bound listener");
        let (_accepted, _peer) = listener.accept().expect("listener must accept connection");
        drop(client);
    }

    /// レビュー指摘の再現ケース: 非ループバックアドレスは bind 自体が行われず
    /// `Err` になること（`validate_loopback_bind` 単体テストの外形確認）。
    #[test]
    fn bind_loopback_rejects_non_loopback_address() {
        let result = bind_loopback("0.0.0.0:0");
        assert!(result.is_err());
    }

    /// レビュー指摘の再現ケース: 同時接続数の上限を超える接続は、ハンドシェイクへ
    /// 進ませずに `53300` の ErrorResponse を受け取った後クローズされること
    /// （Slowloris 対策・WIRE-6）。
    #[test]
    fn accept_loop_rejects_connection_over_capacity_with_53300() {
        let dir = std::env::temp_dir().join(format!(
            "wire-server-server-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users.txt");
        std::fs::write(&path, "").expect("write empty user store");
        let store = Arc::new(UserStore::load_from_file(&path).expect("valid empty store"));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let limiter = ConnectionLimiter::new(1);

        std::thread::spawn(move || {
            accept_loop(listener, store, limiter, Duration::from_secs(5));
        });

        // 1 本目: 上限(1)に達する接続。ハンドシェイクを進めず接続だけ保持する。
        let _held = TcpStream::connect(addr).expect("connect first");
        std::thread::sleep(Duration::from_millis(100));

        // 2 本目: 上限超過のため `'E'` / `53300` を受け取った後クローズされること。
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
            body_str.contains(limits::SQLSTATE_TOO_MANY_CONNECTIONS),
            "ErrorResponse must carry SQLSTATE 53300, got: {body_str:?}"
        );
        let mut extra = [0u8; 1];
        let n = second.read(&mut extra).unwrap_or(0);
        assert_eq!(
            n, 0,
            "connection over the concurrency limit must be closed after the rejection"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: StartupMessage を送らない接続は読み取りタイムアウト
    /// で応答なしに切断され、接続枠が解放されて後続接続を受け付けられること（WIRE-5）。
    #[test]
    fn accept_loop_releases_permit_after_read_timeout() {
        let dir = std::env::temp_dir().join(format!(
            "wire-server-server-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("users.txt");
        std::fs::write(&path, "").expect("write empty user store");
        let store = Arc::new(UserStore::load_from_file(&path).expect("valid empty store"));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let short_timeout = Duration::from_millis(150);
        let client_probe_timeout = Duration::from_millis(50);
        assert!(
            client_probe_timeout < short_timeout,
            "probe window must be shorter than the server timeout to distinguish before/after"
        );
        let limiter = ConnectionLimiter::new(1);

        std::thread::spawn(move || {
            accept_loop(listener, store, limiter, short_timeout);
        });

        // 1 本目: 何も送らず保持する（クライアント側では明示的に close しない。
        // サーバー側の read タイムアウトだけで枠が解放されることを確認するため）。
        let stalled = TcpStream::connect(addr).expect("connect stalled");

        // タイムアウト前: 枠(1)を stalled が占有しているため、2 本目は
        // `53300` を受けた後クローズされること。
        std::thread::sleep(Duration::from_millis(50));
        let mut before = TcpStream::connect(addr).expect("connect before timeout");
        let mut header = [0u8; 1];
        before.read_exact(&mut header).expect("read message type");
        assert_eq!(header[0], b'E', "slot must still be held before timeout");

        // サーバー側の read タイムアウトが経過するのを待つ（stalled はクライアント側で
        // close していないため、枠が解放されるのはサーバーのタイムアウトのみが理由）。
        std::thread::sleep(short_timeout * 3);

        // タイムアウト後: 枠が解放されているため、3 本目は即座には閉じられない
        // （読み取りが `client_probe_timeout` 以内に WouldBlock することで、
        // 「即座に EOF」ではなく「接続を受理して待機している」ことを確認する）。
        let mut after = TcpStream::connect(addr).expect("connect after timeout");
        after
            .set_read_timeout(Some(client_probe_timeout))
            .expect("set client read timeout");
        let mut buf = [0u8; 1];
        let err = after.read(&mut buf).expect_err("no data sent yet");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::WouldBlock,
            "connection after the server-side read timeout must not be closed immediately"
        );

        drop(stalled);
        let _ = std::fs::remove_file(&path);
    }
}
