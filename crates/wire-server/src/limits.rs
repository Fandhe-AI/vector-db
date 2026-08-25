//! 接続資源保護（読み取りタイムアウト・同時接続数リミッター）を担う共有モジュール。
//!
//! `server::accept_loop` から呼ばれ、未認証クライアントの大量接続・Slowloris による
//! スレッド／メモリ枯渇を防ぐ。契約値（読み取りタイムアウト・同時接続数上限・
//! 上限超過時の SQLSTATE）をこのモジュールに集約し、`handshake.rs` や `server.rs`
//! に暫定値が分散しないようにする。
//!
//! 対応: TASK-69（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-5, WIRE-6）。
//! `SQLSTATE_TOO_MANY_CONNECTIONS` はポインタ: `docs/spec/04-behavior/error-format.md`
//! の `53300` 行を参照。

use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 接続全体（認証前後を問わず）に適用する読み取り・書き込みタイムアウト（WIRE-5）。
/// `server::accept_loop` が受理直後に一度だけ設定し、`handshake::handle_connection`
/// 側では変更しない（認証前後で値を切り替える経路を作らない）。
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 同時接続数の上限（WIRE-6）。
pub const MAX_CONNECTIONS: usize = 64;

/// 上限超過時に拒否応答（ErrorResponse）を書き込む際の書き込みタイムアウト。
/// 拒否応答自体が accept ループのブロッキング点にならないよう小さく設定する。
pub const REJECT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// 上限超過接続への拒否応答（[`reject_too_many_connections`]）を書き込むために
/// 同時に生成できるワーカースレッド数の上限（review 是正・WIRE-6）。
///
/// `server::accept_loop` は本体をブロックさせないため拒否応答の書き込みを
/// 使い捨てスレッドへ委譲するが、`MAX_CONNECTIONS` の枠管理外で無制限に
/// `std::thread::spawn` すると、攻撃者が上限到達後に接続を連続作成することで
/// スレッド・スタックなどの OS 資源を無制限に消費できてしまう（DoS）。
/// [`RejectWorkerLimiter`] でこのワーカー数自体を別枠の小さい上限に有界化し、
/// 上限に達した場合は応答を書かずに即座に接続をクローズする（fail-closed。
/// 応答が返らない方を、資源枯渇を許す方より安全側とする）。
pub const MAX_REJECT_WORKERS: usize = 16;

/// 拒否応答ワーカー 1 本ぶんの所有権。`Drop` で確実に解放する（`ConnectionPermit`
/// と同じ RAII パターン）。
pub struct RejectWorkerPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for RejectWorkerPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 拒否応答ワーカースレッド数を [`MAX_REJECT_WORKERS`] 以内に有界化する専用の
/// 小さいセマフォ。`ConnectionLimiter`（認証済み接続の枠）とは別枠で管理し、
/// 拒否経路が本来の接続枠を消費しないという既存の契約を変えない。
#[derive(Clone)]
pub struct RejectWorkerLimiter {
    active: Arc<AtomicUsize>,
    max: usize,
}

impl RejectWorkerLimiter {
    /// ワーカー数上限を `max` として新しいリミッターを作る。
    pub fn new(max: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// `active` が `max` 未満なら枠を 1 つ確保して `Some` を返す。`ConnectionLimiter`
    /// と同じ CAS ループで競合下でも `max` を超えて確保しない。
    pub fn try_acquire(&self) -> Option<RejectWorkerPermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.max {
                return None;
            }
            let next = current.checked_add(1)?;
            match self.active.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(RejectWorkerPermit {
                        active: Arc::clone(&self.active),
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }
}

/// SQLSTATE `53300`（too_many_connections）。ポインタ:
/// `docs/spec/04-behavior/error-format.md`。
pub const SQLSTATE_TOO_MANY_CONNECTIONS: &str = "53300";

/// 同時接続数の枠 1 つぶんの所有権。`Drop` で確実に解放する（RAII。早期 return や
/// panic があっても枠解放漏れが起きないようにする）。
pub struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 同時接続数の共有リミッター。`server::accept_loop` がクローンを保持し、
/// 受理のたびに [`ConnectionLimiter::try_acquire`] を呼ぶ。
#[derive(Clone)]
pub struct ConnectionLimiter {
    active: Arc<AtomicUsize>,
    max: usize,
}

impl ConnectionLimiter {
    /// 同時接続数の上限を `max` として新しいリミッターを作る。
    pub fn new(max: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// `active` が `max` 未満なら枠を 1 つ確保して `Some` を返す。CAS ループで
    /// 「読み取り→上限比較→加算」の間の競合を許さず、複数スレッドが同時に accept
    /// しても上限を超えて確保できないようにする。加算は `checked_add` で行い、
    /// カウンタのオーバーフローを未定義動作にしない（coding-rust.md 準拠）。
    pub fn try_acquire(&self) -> Option<ConnectionPermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.max {
                return None;
            }
            let next = current.checked_add(1)?;
            match self.active.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ConnectionPermit {
                        active: Arc::clone(&self.active),
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// 現在の使用中枠数（テスト・ログ用の観測）。
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// 上限値（テスト用の観測）。
    pub fn max(&self) -> usize {
        self.max
    }
}

/// `stream` に読み取り・書き込み双方のタイムアウトを設定する。設定自体の失敗
/// （OS レベルのソケットオプション設定エラー）は `Err` を返し、呼び出し元は
/// タイムアウトなしで扱い続けるより安全側（接続を破棄する）に倒す（fail-closed）。
pub fn apply_read_timeout(stream: &TcpStream, timeout: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(())
}

/// 同時接続数の上限超過を通知する ErrorResponse（'E', severity=FATAL,
/// code=`53300`）を書き込み、接続を閉じる。`handshake.rs` の private な
/// `write_error_response` には依存せず自己完結させる（TASK-68/71 との衝突回避）。
/// フィールドは S/C/M のみで、他テナント・存在情報・ピア識別情報は含めない。
///
/// 呼び出し元（`server::accept_loop`）はこの関数を呼ぶ時点でまだ
/// `std::thread::spawn` へ到達していない（スレッドを生成せずに拒否する）。
/// 書き込み失敗は無視する（拒否経路で新たなブロッキング点・panic を作らないため。
/// クライアントが応答を受け取れなくても、最終的に `shutdown` で接続は閉じる）。
pub fn reject_too_many_connections(mut stream: TcpStream, max: usize) {
    let _ = stream.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));

    let message = format!("too many connections (max {max})");
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"FATAL");
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(SQLSTATE_TOO_MANY_CONNECTIONS.as_bytes());
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

    use std::io::Write as _;
    let _ = stream.write_all(&msg);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::net::TcpListener;

    /// 枠の取得・解放（RAII）がネットワークを介さず単体で検証できること。
    #[test]
    fn try_acquire_enforces_max_and_releases_on_drop() {
        let limiter = ConnectionLimiter::new(2);
        let permit_a = limiter.try_acquire().expect("first permit");
        let permit_b = limiter.try_acquire().expect("second permit");
        assert!(
            limiter.try_acquire().is_none(),
            "third permit must be rejected at max=2"
        );
        assert_eq!(limiter.active(), 2);

        drop(permit_a);
        assert_eq!(limiter.active(), 1);
        let permit_c = limiter
            .try_acquire()
            .expect("permit must be released on drop");

        drop(permit_b);
        drop(permit_c);
        assert_eq!(limiter.active(), 0);
    }

    /// 並行 `try_acquire` を回しても、観測した使用中枠数が `max` を超えないこと
    /// （CAS ループの競合耐性の確認）。
    #[test]
    fn try_acquire_never_exceeds_max_under_concurrency() {
        const MAX: usize = 8;
        const THREADS: usize = 16;
        const ITERS: usize = 200;

        let limiter = ConnectionLimiter::new(MAX);
        let max_observed = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let limiter = limiter.clone();
                let max_observed = Arc::clone(&max_observed);
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        if let Some(permit) = limiter.try_acquire() {
                            let observed = limiter.active();
                            max_observed.fetch_max(observed, Ordering::AcqRel);
                            drop(permit);
                        }
                    }
                });
            }
        });

        assert!(
            max_observed.load(Ordering::Acquire) <= MAX,
            "observed active count must never exceed max={MAX}"
        );
        assert_eq!(limiter.active(), 0, "all permits must be released");
    }

    /// `apply_read_timeout` がソケットへ実際に反映されること。
    #[test]
    fn apply_read_timeout_sets_read_and_write_timeouts() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");

        let timeout = Duration::from_millis(250);
        apply_read_timeout(&server_stream, timeout).expect("apply timeout");

        assert_eq!(
            server_stream.read_timeout().expect("read timeout"),
            Some(timeout)
        );
        assert_eq!(
            server_stream.write_timeout().expect("write timeout"),
            Some(timeout)
        );
        drop(client);
    }

    /// `reject_too_many_connections` が `'E'` / SQLSTATE `53300` を含む
    /// ErrorResponse を書き込み、その後 EOF になること。
    #[test]
    fn reject_too_many_connections_writes_53300_error_and_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");

        std::thread::spawn(move || {
            reject_too_many_connections(server_stream, MAX_CONNECTIONS);
        });

        let mut header = [0u8; 1];
        client.read_exact(&mut header).expect("read message type");
        assert_eq!(header[0], b'E', "expected ErrorResponse");

        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).expect("read length");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len - 4];
        client.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains(SQLSTATE_TOO_MANY_CONNECTIONS),
            "ErrorResponse must carry SQLSTATE 53300, got: {body_str:?}"
        );

        let mut extra = [0u8; 1];
        let n = client.read(&mut extra).unwrap_or(0);
        assert_eq!(n, 0, "connection must be closed after the rejection");
    }
}
