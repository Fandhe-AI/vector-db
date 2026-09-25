//! 明示トランザクション（SQL-31・TASK-221）と autocommit 書き込みの間で
//! `redb` の単一ライタ（`TABLE-3`）を安全に共有するための待機付きゲート。
//!
//! `redb::WriteTransaction` は待機上限を持たない `Database::begin_write` でしか
//! 得られない。明示トランザクションが `BEGIN` から `COMMIT`/`ROLLBACK` まで
//! ライタを占有すると、autocommit 書き込みが無期限に止まってしまう。本モジュールは
//! 「ライタを握る前に必ずこのゲートを通す」という choke point を追加し、
//! - autocommit（[`crate::storage::Storage::begin_write_txn`]）はゲートを
//!   待機上限つきで取得し、`redb` のライタを得た直後にゲートを手放す
//!   （占有時間は redb 呼び出し 1 回分のみ）。
//! - 明示トランザクション（[`crate::storage::Storage::begin_explicit_write_txn`]）は
//!   ゲートを取得したまま [`WriterPermit`] として保持し、トランザクション終了まで
//!   手放さない。
//!
//! という非対称な保持方針で、待機の循環（デッドロック）を構造的に作らない
//! （ゲートは 1 段しかなく、redb 自身のロックへ待機付きで到達できるのは
//! ゲートを取得できたスレッドだけ）。
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

/// ゲート取得に失敗した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateError {
    /// 待機上限（呼び出し元が指定した `Duration`）を超過した。
    Timeout,
    /// 明示トランザクションを保持している当該スレッド自身が再度ゲートへ到達した
    /// （書き込み経路の配線漏れに対する多層防御。許可リスト側で本来は手前で
    /// 拒否される想定）。
    HeldByCurrentThread,
}

#[derive(Debug)]
struct GateState {
    /// 明示トランザクションがゲートを保持している間、そのスレッド ID を保持する。
    held_by: Option<ThreadId>,
}

/// 単一ライタを守るゲート本体。`Storage` が `Arc` で保持し、[`WriterPermit`] と
/// 寿命を共有する。
#[derive(Debug)]
pub struct WriterGate {
    state: Mutex<GateState>,
    cvar: Condvar,
}

impl WriterGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState { held_by: None }),
            cvar: Condvar::new(),
        })
    }

    /// autocommit 経路用: ゲートを取得したらすぐに解放するクロージャ実行の形。
    /// `redb::Database::begin_write` 呼び出しをゲートで挟む用途に使う。
    pub fn with_permit_timeout<T>(
        self: &Arc<Self>,
        timeout: Duration,
        f: impl FnOnce() -> T,
    ) -> Result<T, GateError> {
        let _permit = self.acquire(timeout)?;
        Ok(f())
        // `_permit` はここで drop され、直ちに解放・通知される。
    }

    /// 明示トランザクション経路用: ゲートを保持したまま `WriterPermit` を返す。
    pub fn acquire(self: &Arc<Self>, timeout: Duration) -> Result<WriterPermit, GateError> {
        let current = std::thread::current().id();
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if guard.held_by == Some(current) {
            return Err(GateError::HeldByCurrentThread);
        }
        let deadline = Instant::now() + timeout;
        while guard.held_by.is_some() {
            let now = Instant::now();
            if now >= deadline {
                return Err(GateError::Timeout);
            }
            let wait_for = deadline - now;
            let (g, timeout_result) = self
                .cvar
                .wait_timeout(guard, wait_for)
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
            if timeout_result.timed_out() && guard.held_by.is_some() {
                return Err(GateError::Timeout);
            }
        }
        guard.held_by = Some(current);
        Ok(WriterPermit {
            gate: Arc::clone(self),
        })
    }
}

/// ゲートの保持証跡（RAII）。drop 時に解放して待機者へ通知する。
#[derive(Debug)]
pub struct WriterPermit {
    gate: Arc<WriterGate>,
}

impl Drop for WriterPermit {
    fn drop(&mut self) {
        let mut guard = self.gate.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.held_by = None;
        drop(guard);
        self.gate.cvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Barrier;

    #[test]
    fn acquire_then_release_allows_next_acquirer() {
        let gate = WriterGate::new();
        let permit = gate.acquire(Duration::from_secs(1)).expect("acquire");
        drop(permit);
        let permit2 = gate.acquire(Duration::from_secs(1)).expect("acquire again");
        drop(permit2);
    }

    #[test]
    fn acquire_times_out_while_held_by_other_thread() {
        let gate = WriterGate::new();
        let permit = gate.acquire(Duration::from_secs(5)).expect("acquire");
        let gate2 = Arc::clone(&gate);
        let start = Instant::now();
        // 別スレッドから待機させる（同一スレッドからの再入は
        // `HeldByCurrentThread` になり、待機上限超過とは区別される）。
        let handle = std::thread::spawn(move || {
            gate2
                .acquire(Duration::from_millis(50))
                .expect_err("should time out")
        });
        let err = handle.join().unwrap();
        assert_eq!(err, GateError::Timeout);
        assert!(start.elapsed() >= Duration::from_millis(50));
        drop(permit);
    }

    #[test]
    fn acquire_by_current_holder_thread_is_rejected_immediately() {
        let gate = WriterGate::new();
        let permit = gate.acquire(Duration::from_secs(5)).expect("acquire");
        let start = Instant::now();
        let err = gate
            .acquire(Duration::from_secs(5))
            .expect_err("reentrant acquire must fail");
        assert_eq!(err, GateError::HeldByCurrentThread);
        // 待機せず即座に失敗すること（上限まで待たない）。
        assert!(start.elapsed() < Duration::from_millis(500));
        drop(permit);
    }

    #[test]
    fn release_notifies_waiting_thread_promptly() {
        let gate = WriterGate::new();
        let permit = gate.acquire(Duration::from_secs(5)).expect("acquire");
        let gate2 = Arc::clone(&gate);
        let barrier = Arc::new(Barrier::new(2));
        let barrier2 = Arc::clone(&barrier);
        let acquired = Arc::new(AtomicBool::new(false));
        let acquired2 = Arc::clone(&acquired);
        let handle = std::thread::spawn(move || {
            barrier2.wait();
            let p = gate2.acquire(Duration::from_secs(5)).expect("acquire");
            acquired2.store(true, Ordering::SeqCst);
            drop(p);
        });
        barrier.wait();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!acquired.load(Ordering::SeqCst));
        drop(permit);
        handle.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
    }

    #[test]
    fn with_permit_timeout_releases_after_closure() {
        let gate = WriterGate::new();
        let result = gate
            .with_permit_timeout(Duration::from_secs(1), || 42)
            .expect("closure runs");
        assert_eq!(result, 42);
        // 直後に再取得できること（closure 終了後に解放されている）。
        let permit = gate.acquire(Duration::from_millis(50)).expect("reacquire");
        drop(permit);
    }
}
