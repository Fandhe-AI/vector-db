//! `--surface` opt-in（Issue #734・TASK-171／HTTP-1）をバイナリ子プロセスと
//! して起動し、CLI 引数の受理・拒否（fail-closed）を外形的に検証する結合
//! テスト。`tests/wire_search_engine_cli.rs` と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを外形的に確認する）。
//!
//! - 未指定／`--surface sql` の 2 プロセスは `listening on` に到達すること
//! - `bogus`・大文字小文字違い・値欠落・重複指定はいずれも非 0 終了・
//!   stderr に `--surface` を含む説明が出ること
//! - `--surface nosql` は**パーサとしては受理する**（`must be one of`／
//!   `specified more than once` を stderr に含まない）が、NoSQL リスナー本体
//!   の配線が Issue #735 の担当であるため非 0 終了・stderr に
//!   「not wired yet」を含み、`listening on` には到達しないこと（#735 で
//!   「HTTP リスナーのみ listen」へ置き換わる暫定契約）

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（ユーザーストア・DB ファイルの
/// 置き場）を確保し、`Drop` で確実に削除するガード
/// （`wire_search_engine_cli.rs::TempFixtureDir` と同型）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-surface-cli-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        Self { dir }
    }

    fn users_path_str(&self) -> String {
        self.dir
            .join("users.txt")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    fn db_path_str(&self) -> String {
        self.dir
            .join("db.redb")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 空のユーザーストア（本ファイルのテストは認証まで到達する必要がない）。
fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// 子プロセスの stderr を専用スレッドで読み、`listening on` を待つ
/// （`wire_search_engine_cli.rs::wait_for_listening` と同じ理由:
/// `BufReader::read_line` はデッドラインを持たないブロッキング呼び出しの
/// ため、`mpsc::Receiver::recv_timeout` で確実に打ち切れるようにする）。
fn wait_for_listening(child: &mut Child) -> bool {
    let stderr = child.stderr.take().expect("piped stderr");
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

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains("listening on") => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// 未指定／`--surface sql` はいずれも `listening on` に到達すること。
#[test]
fn unset_and_sql_token_start_listening() {
    for extra_args in [Vec::<&str>::new(), vec!["--surface", "sql"]] {
        let fixture = TempFixtureDir::new(&format!("ok-{}", extra_args.len()));
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
        let db_path = fixture.db_path_str();

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(&extra_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let listening = wait_for_listening(&mut child);
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            listening,
            "args={extra_args:?}: expected to reach listening state"
        );
    }
}

/// 不正値・値欠落・重複指定はいずれも fail-closed（非 0 終了・`--surface`
/// を含む stderr）。
#[test]
fn invalid_or_missing_or_duplicate_surface_arg_is_rejected() {
    let fixture = TempFixtureDir::new("reject");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let cases: Vec<Vec<&str>> = vec![
        vec!["--surface", "bogus"],
        vec!["--surface", "SQL"],
        vec!["--surface", "Nosql"],
        // 直後の既知フラグ名をそのまま値として食い、閉じた語彙のいずれとも
        // 一致しないため拒否されるケース（`--search-engine` と同じ「次
        // トークンを無条件で値とみなす」仕様の確認）。
        vec!["--surface", "--bind"],
        // 重複指定（同値・異値どちらも拒否）。
        vec!["--surface", "sql", "--surface", "sql"],
        vec!["--surface", "sql", "--surface", "nosql"],
    ];

    for extra_args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(&extra_args)
            .output()
            .expect("spawn wire-server");

        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--surface"),
            "args={extra_args:?}: expected stderr to mention --surface, got: {stderr}"
        );
    }

    // 値欠落単体（末尾に `--surface` だけを置くケース）。
    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--surface",
        ])
        .output()
        .expect("spawn wire-server");
    assert!(!output.status.success(), "missing value must be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--surface"),
        "expected stderr to mention --surface, got: {stderr}"
    );
}

/// `--surface nosql` はパーサとしては受理するが、NoSQL リスナー本体の配線が
/// Issue #735 の担当であるため非 0 終了・stderr に「not wired yet」を含み
/// `listening on` には到達しない（#735 で「HTTP リスナーのみ listen」へ
/// 置き換わる暫定契約。本テストはその置き換え時に更新が必要になる）。
#[test]
fn nosql_token_is_accepted_by_parser_but_listener_is_not_wired_yet() {
    let fixture = TempFixtureDir::new("nosql-stub");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--surface",
            "nosql",
        ])
        .output()
        .expect("spawn wire-server");

    assert!(
        !output.status.success(),
        "expected non-zero exit until Issue #735 wires the NoSQL listener"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("must be one of"),
        "unexpected parser rejection, got: {stderr}"
    );
    assert!(
        !stderr.contains("specified more than once"),
        "unexpected duplicate rejection, got: {stderr}"
    );
    assert!(
        stderr.contains("not wired yet"),
        "expected stderr to mention the pending listener wiring, got: {stderr}"
    );
    assert!(
        !stderr.contains("listening on"),
        "must not reach listening state before Issue #735, got: {stderr}"
    );
}
