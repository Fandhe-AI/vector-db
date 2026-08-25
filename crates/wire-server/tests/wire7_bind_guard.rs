//! バイナリ `wire-server` を実プロセスとして起動し、非ループバック bind が
//! 起動時に非 0 終了で拒否されること・loopback bind は正常に起動へ進むことを
//! 検証する結合テスト（対応: TASK-70。対象ビヘイビア WIRE-7）。
//!
//! `crates/wire-server/src/bind_guard.rs` の単体テストは `GuardedBindAddrs` の
//! 判定ロジックのみを検証するため、ここでは `main.rs::run_server` が実際に
//! プロセスを非 0 終了させること・stderr に拒否理由を出力することを外形的に
//! 確認する（ユーザーストアの内容には依存しないよう空ファイルで固定する）。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
/// プロセス内単調カウンタ（`wire_auth.rs` と同一クラスの競合対策。Issue #172）。
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ユーザーストアディレクトリ／ファイルを保持し、
/// `Drop` でディレクトリごと確実に削除するガード（対応: TASK-70 review 指摘。
/// 空ファイル（ユーザー登録なし）で十分（bind ガードはユーザーストア読込より
/// 前に実行される）。`assert!` の panic 経路でも `Drop` によりクリーンアップが
/// 走るため、ファイル削除のみに頼っていた旧実装のような temp ディレクトリの
/// 残留を起こさない。
struct TempUserStore {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl TempUserStore {
    fn new() -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-wire7-test-{}-{}-{}",
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
        Self { dir, path }
    }

    fn path_str(&self) -> &str {
        self.path.to_str().expect("utf-8 path")
    }
}

impl Drop for TempUserStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 非ループバックアドレス（`0.0.0.0` / `[::]`）を指定すると、TLS 未構成
/// （TASK-72/WIRE-9）のため起動が非 0 終了で拒否されること。
#[test]
fn non_loopback_bind_exits_non_zero() {
    let users_store = TempUserStore::new();

    for bind_addr in ["0.0.0.0:0", "[::]:0"] {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(["--users", users_store.path_str(), "--bind", bind_addr])
            .output()
            .expect("spawn wire-server");

        assert!(
            !output.status.success(),
            "non-loopback bind {bind_addr} must exit non-zero"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("refusing to bind non-loopback") && stderr.contains("TLS"),
            "stderr for {bind_addr} should explain the TLS-related refusal, got: {stderr}"
        );
    }
}

/// loopback アドレス（`127.0.0.1`）は起動拒否されず、accept ループへ進むこと
/// （stderr に `listening on` が出力されるまで待って確認する）。
#[test]
fn loopback_bind_starts_listening() {
    let users_store = TempUserStore::new();

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(["--users", users_store.path_str(), "--bind", "127.0.0.1:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let stderr = child.stderr.take().expect("piped stderr");

    // `BufReader::read_line` は子プロセスの stdout/stderr パイプに対する read
    // タイムアウトを持たないブロッキング呼び出しであるため、デッドラインを
    // ループの「間」でチェックするだけでは子プロセスが行を出力も終了もせず
    // 停止した場合にハングしうる（TASK-70 review 指摘）。専用スレッドで
    // 行読み取りを行い、`mpsc::Receiver::recv_timeout` で待つことで、
    // 呼び出し元スレッドが `read_line` のブロッキングに巻き込まれず確実に
    // デッドラインで打ち切れるようにする。
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 || tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_listening = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains("listening on") => {
                saw_listening = true;
                break;
            }
            Ok(_) => continue,
            // 送信側スレッドが終了した（プロセスが早期終了・拒否された）場合、
            // またはタイムアウトした場合はここで抜ける。
            Err(_) => break,
        }
    }

    // 起動を確認できたら子プロセスを終了させ、ゾンビを残さないよう wait する。
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        saw_listening,
        "loopback bind must not be rejected and must reach the listening state"
    );
}
