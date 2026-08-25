//! TCP 接続の受け付け・同時接続数の有界化を担う。
//!
//! `main.rs::run_server` から呼ばれる（`tests/` からの結合テストが同じ挙動を
//! 直接検証できるよう lib.rs 経由で公開する）。responsibility はソケットレベルの
//! 防御（同時接続数上限・I/O タイムアウト）に限る。bind アドレスの loopback 検証
//! （TASK-70・WIRE-7）は [`crate::bind_guard`] が担い、本モジュールは検証済みの
//! `TcpListener` を受け取って accept ループを回すだけである。wire プロトコル
//! そのものの解釈は [`crate::handshake::handle_connection`] に委譲する。
//!
//! 対応: TASK-67 の review 是正。Slowloris 対策として認証前フェーズに限り
//! 接続数上限・I/O タイムアウトを課す（[`accept_loop`]）。認証後は緩い
//! [`POST_AUTH_IDLE_TIMEOUT`] へ切り替え、有効資格情報を持つクライアントが
//! 何も送らずに接続枠を永久占有するのを防ぐ（暫定防御）。本格的な接続ライフサイクル
//! 管理（段階的タイムアウト・ヘルスチェック・keepalive 等）は TASK-69（WIRE-5, WIRE-6）の
//! 管轄であり、本モジュールは暫定の防御的デフォルトに留める。

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::auth::UserStore;
use crate::bind_guard::{GuardedBindAddrs, TransportSecurity};

/// 旧 TASK-67 review 是正時点の公開 API との後方互換ラッパー。
///
/// TASK-70（WIRE-7）で本体の実装は [`crate::bind_guard::GuardedBindAddrs`]
/// （TLS 有無に応じた通信路保護要件を型で表現できる形）へ移設・拡張したが、
/// 本関数はすでに公開 API（`pub fn bind_loopback`）として利用側に届いている
/// ため、AGENTS.md の「公開 API・エラー契約の互換性（P1）」に従い削除せず
/// 残す。内部では [`GuardedBindAddrs::resolve`]（[`TransportSecurity::Cleartext`]
/// 固定 = 従来どおり loopback 限定）を呼ぶだけの薄いラッパーであり、挙動・
/// エラーメッセージ文言は旧実装と同一（[`crate::bind_guard::BindGuardError`]
/// の `Display` が旧 `validate_loopback_bind` と同じ文言を維持している）。
///
/// 新規コードは `GuardedBindAddrs::resolve` を直接呼ぶこと（TLS 導入時に
/// `TransportSecurity` へ variant が増えても、本ラッパーは cleartext 固定の
/// 意味論を変えない）。
///
/// 対応: PR #182 レビュー是正。[`crate::bind_guard::BindGuardError`] の
/// `Display` は新 API 向けに `bind_addr` を含む文脈情報を足しており
/// （`NonLoopback` に `(from {bind_addr})` 等）、旧 `validate_loopback_bind` /
/// `bind_loopback` のエラー文言とは異なる。本ラッパーは呼び出し側が診断・
/// 照合に利用しうる旧文言（`failed to bind {bind_addr} ({addrs:?}): {e}` /
/// `refusing to bind non-loopback address {addr}: ...`）を `BindGuardError`
/// の `Display` に委譲せず個別に再現し、公開 API の互換性を保つ。
#[deprecated(
    since = "0.1.0",
    note = "use crate::bind_guard::GuardedBindAddrs::resolve(..., TransportSecurity::Cleartext) instead"
)]
pub fn bind_loopback(bind_addr: &str) -> Result<TcpListener, String> {
    use crate::bind_guard::BindGuardError;

    let guarded = GuardedBindAddrs::resolve(bind_addr, TransportSecurity::Cleartext).map_err(
        |e| match e {
            BindGuardError::Resolve { bind_addr, source } => {
                format!("cannot resolve bind address {bind_addr}: {source}")
            }
            BindGuardError::NoAddress { bind_addr } => {
                format!("bind address {bind_addr} did not resolve to any socket address")
            }
            BindGuardError::NonLoopback { addr, .. } => format!(
                "refusing to bind non-loopback address {addr}: cleartext password \
                 authentication is not yet protected by TLS (TASK-72/WIRE-9); \
                 bind to a loopback address (e.g. 127.0.0.1) or place a trusted TLS \
                 terminator in front of this listener"
            ),
        },
    )?;
    let addrs = guarded.addrs().to_vec();
    guarded
        .bind()
        .map_err(|e| format!("failed to bind {bind_addr} ({addrs:?}): {e}"))
}

/// 同時接続数の暫定上限（防御的な小さめの定数）。本格的な運用上限の決定・
/// テナント単位の細分化は TASK-69 の管轄。
pub const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// **認証前**（StartupMessage 受信〜認証応答完了まで）にのみ課す読み取り・書き込みの
/// I/O 期限（Slowloris 対策: StartupMessage 等を送らない/受け取らない接続がスレッドと
/// 接続枠を無期限に占有するのを防ぐ）。認証成功後は
/// [`crate::handshake::handle_connection`] が [`POST_AUTH_IDLE_TIMEOUT`] へ切り替える
/// （review 指摘）。
pub const CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// **認証後**のセッションに課す緩いアイドル期限。対話的クライアント・コネクション
/// プールの正当な長アイドル（数分〜十数分）を切断しない値を確保しつつ、有効な
/// 資格情報を持つクライアントが接続を張ったまま何も送らないことで
/// `MAX_CONCURRENT_CONNECTIONS` の枠を永久に占有し続けるのを防ぐ（review 指摘）。
/// 期限超過時は接続を正常にクローズし、接続枠を解放する（`server::accept_loop` の
/// `ConnectionSlot` の Drop 経由）。あくまで暫定防御であり、本格的なセッション生存
/// 期間管理（keepalive・利用パターンに応じた期限調整等）は TASK-69（WIRE-5, WIRE-6）の管轄。
pub const POST_AUTH_IDLE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// 同時接続数の枠 1 つぶんの所有権。`Drop` で確実に解放する（RAII。早期 return や
/// panic があっても枠解放漏れが起きないようにする）。
struct ConnectionSlot {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// `active` が `max` 未満なら枠を 1 つ確保して `Some` を返す。CAS ループで
/// 「読み取り→上限比較→加算」の間の競合を許さず、複数スレッドが同時に accept
/// しても上限を超えて確保できないようにする。
fn try_acquire_slot(active: &Arc<AtomicUsize>, max: usize) -> Option<ConnectionSlot> {
    let mut current = active.load(Ordering::Acquire);
    loop {
        if current >= max {
            return None;
        }
        match active.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                return Some(ConnectionSlot {
                    active: Arc::clone(active),
                })
            }
            Err(actual) => current = actual,
        }
    }
}

/// stream に読み取り・書き込み双方のタイムアウトを設定する。設定自体の失敗
/// （OS レベルのソケットオプション設定エラー）は `Err` を返し、呼び出し元は
/// タイムアウトなしで扱い続けるより安全側（接続を破棄する）に倒す。
fn apply_io_timeout(stream: &TcpStream, timeout: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(())
}

/// 接続受け付けループ本体。1 接続 1 スレッドで処理するが、以下の防御を課す:
///
/// - `max_connections` を超える接続は [`crate::handshake::handle_connection`] へ
///   進ませず、スレッドも生成せずに即座にクローズする（枠を使い切った状態で
///   スレッドを積み増さない）
/// - 受理した接続には読み取り・書き込み双方に `io_timeout`（認証前フェーズ用）を
///   設定してからハンドシェイクへ渡す（StartupMessage 等を送らない/受け取らない
///   接続が無期限にスレッド・接続枠を占有するのを防ぐ）
/// - 認証成功後は `post_auth_idle_timeout`（[`POST_AUTH_IDLE_TIMEOUT`] 相当。
///   `handle_connection` が切り替える）へ緩和し、対話的クライアント・
///   コネクションプールの正当なアイドルを妨げないようにする
///
/// 各スレッドの panic は `std::thread::spawn` の join ハンドルを無視することで
/// プロセス全体へは波及させない（他接続の継続稼働を優先する）。
pub fn accept_loop(
    listener: TcpListener,
    store: Arc<UserStore>,
    max_connections: usize,
    io_timeout: Duration,
    post_auth_idle_timeout: Duration,
) {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wire-server: accept error: {e}");
                continue;
            }
        };

        let Some(slot) = try_acquire_slot(&active, max_connections) else {
            // 上限超過: ハンドシェイクへ進ませず即座にクローズする（スレッドを
            // 消費しない。drop(stream) は明示せずスコープを抜けるだけでよいが、
            // 意図を明確にするため書いておく）。
            drop(stream);
            eprintln!("wire-server: rejecting connection: too many concurrent connections");
            continue;
        };

        if let Err(e) = apply_io_timeout(&stream, io_timeout) {
            eprintln!("wire-server: failed to configure connection timeouts: {e}");
            // `slot` はここでスコープを抜けて解放される。
            continue;
        }

        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            // 接続処理中は `slot` を保持し続け、スレッド終了時（正常終了・panic
            // いずれも）に Drop で確実に枠を解放する。
            let _slot = slot;
            if let Err(e) =
                crate::handshake::handle_connection(stream, &store, post_auth_idle_timeout)
            {
                eprintln!("wire-server: connection error: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// P1 review 是正の再現ケース: 削除された公開 API との後方互換ラッパー
    /// `bind_loopback` が、新実装（`GuardedBindAddrs`）と同じ fail-closed 挙動
    /// （loopback は許可・非 loopback は拒否）を維持していること。
    #[test]
    #[allow(deprecated)]
    fn bind_loopback_compat_wrapper_accepts_loopback_and_rejects_non_loopback() {
        let listener = bind_loopback("127.0.0.1:0").expect("loopback bind must succeed");
        drop(listener);

        let err = bind_loopback("0.0.0.0:0").expect_err("non-loopback bind must be rejected");
        assert!(
            err.contains("refusing to bind non-loopback address"),
            "unexpected error message: {err}"
        );
    }

    /// 枠の取得・解放（RAII）がネットワークを介さず単体で検証できること。
    #[test]
    fn try_acquire_slot_enforces_max_and_releases_on_drop() {
        let active = Arc::new(AtomicUsize::new(0));
        let slot_a = try_acquire_slot(&active, 2).expect("first slot");
        let slot_b = try_acquire_slot(&active, 2).expect("second slot");
        assert!(
            try_acquire_slot(&active, 2).is_none(),
            "third slot must be rejected at max=2"
        );

        drop(slot_a);
        let slot_c = try_acquire_slot(&active, 2).expect("slot must be released on drop");

        drop(slot_b);
        drop(slot_c);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    /// レビュー指摘の再現ケース: 同時接続数の上限を超える接続は、ハンドシェイクへ
    /// 進ませずに即座にクローズされること（Slowloris 対策）。
    #[test]
    fn accept_loop_closes_connection_immediately_when_over_capacity() {
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

        std::thread::spawn(move || {
            // このテストは認証前フェーズのみを検証するため、post-auth タイムアウト
            // の値そのものは無関係（本番既定値をそのまま使う）。
            accept_loop(
                listener,
                store,
                1,
                Duration::from_secs(5),
                POST_AUTH_IDLE_TIMEOUT,
            );
        });

        // 1 本目: 上限(1)に達する接続。ハンドシェイクを進めず接続だけ保持する。
        let _held = TcpStream::connect(addr).expect("connect first");
        std::thread::sleep(Duration::from_millis(100));

        // 2 本目: 上限超過のため、応答を待たずに即座にクローズされること。
        let mut second = TcpStream::connect(addr).expect("connect second");
        let mut buf = [0u8; 1];
        let n = second.read(&mut buf).unwrap_or(0);
        assert_eq!(
            n, 0,
            "connection over the concurrency limit must be closed immediately"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: StartupMessage を送らない接続は I/O タイムアウトで
    /// 切断され、接続枠が解放されて後続接続を受け付けられること。
    #[test]
    fn accept_loop_releases_slot_after_read_timeout() {
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

        std::thread::spawn(move || {
            // このテストは認証前フェーズのみを検証するため、post-auth タイムアウト
            // の値そのものは無関係（本番既定値をそのまま使う）。
            accept_loop(listener, store, 1, short_timeout, POST_AUTH_IDLE_TIMEOUT);
        });

        // 1 本目: 何も送らず保持する（クライアント側では明示的に close しない。
        // サーバー側の read タイムアウトだけで枠が解放されることを確認するため）。
        let stalled = TcpStream::connect(addr).expect("connect stalled");

        // タイムアウト前: 枠(1)を stalled が占有しているため、2 本目は即座に
        // クローズされること（accept_loop_closes_connection_immediately_when_over_capacity
        // と同じ確認を「タイムアウト前」の基準点として取る）。
        std::thread::sleep(Duration::from_millis(50));
        let mut before = TcpStream::connect(addr).expect("connect before timeout");
        let mut buf = [0u8; 1];
        let n = before.read(&mut buf).unwrap_or(0);
        assert_eq!(
            n, 0,
            "slot must still be held by the stalled connection before its read timeout elapses"
        );

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
        let err = after.read(&mut buf).expect_err("no data sent yet");
        assert_eq!(
            err.kind(),
            io::ErrorKind::WouldBlock,
            "connection after the server-side read timeout must not be closed immediately"
        );

        drop(stalled);
        let _ = std::fs::remove_file(&path);
    }
}
