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
use std::time::{Duration, Instant};

/// テストごとに衝突しない一時ユーザーストアファイルを作り、そのパスを返す。
/// 空ファイル（ユーザー登録なし）で十分（bind ガードはユーザーストア読込より
/// 前に実行される）。呼び出し元がテスト末尾で `remove_file` すること。
fn make_empty_user_store() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire7-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("users.txt");
    std::fs::write(&path, "").expect("write empty user store");
    path
}

/// 非ループバックアドレス（`0.0.0.0` / `[::]`）を指定すると、TLS 未構成
/// （TASK-72/WIRE-9）のため起動が非 0 終了で拒否されること。
#[test]
fn non_loopback_bind_exits_non_zero() {
    let users_path = make_empty_user_store();

    for bind_addr in ["0.0.0.0:0", "[::]:0"] {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                users_path.to_str().expect("utf-8 path"),
                "--bind",
                bind_addr,
            ])
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

    let _ = std::fs::remove_file(&users_path);
}

/// loopback アドレス（`127.0.0.1`）は起動拒否されず、accept ループへ進むこと
/// （stderr に `listening on` が出力されるまで待って確認する）。
#[test]
fn loopback_bind_starts_listening() {
    let users_path = make_empty_user_store();

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_path.to_str().expect("utf-8 path"),
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let stderr = child.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_listening = false;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            // プロセスが早期終了した（拒否された）場合はここで抜ける。
            break;
        }
        if line.contains("listening on") {
            saw_listening = true;
            break;
        }
    }

    // 起動を確認できたら子プロセスを終了させ、ゾンビを残さないよう wait する。
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        saw_listening,
        "loopback bind must not be rejected and must reach the listening state"
    );

    let _ = std::fs::remove_file(&users_path);
}
