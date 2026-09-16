//! `--surface` opt-in（Issue #734・#735・TASK-171／HTTP-1・HTTP-9）をバイナリ
//! 子プロセスとして起動し、CLI 引数の受理・拒否（fail-closed）・選択表層に
//! 応じたリスナー分岐を外形的に検証する結合テスト。`tests/wire_search_engine_cli.rs`
//! と同じ流儀（実バイナリを `Command::new(env!("CARGO_BIN_EXE_wire-server"))`
//! で起動し、stderr の `listening on` 行または非 0 終了・エラーメッセージを
//! 外形的に確認する）。
//!
//! - 未指定／`--surface sql` の 2 プロセスは `listening on` に到達し、その
//!   行がちょうど 1 行だけであること（表層表示行が混じらないこと）
//! - `bogus`・大文字小文字違い・値欠落・重複指定はいずれも非 0 終了・
//!   stderr に `--surface` を含む説明が出ること
//! - `--surface nosql` は SQL wire リスナーを一切 bind せず、HTTP/1.1
//!   リスナー（Issue #735・#743。読み取りタイムアウト・接続数リミッター
//!   適用済みだが要求の解釈・応答生成はまだ行わない暫定ハンドラ）だけを
//!   1 本 bind すること（`listening on` に到達し、`listening on` はちょうど
//!   1 行、かつ表層表示行を含む）
//! - `--surface nosql --bind 0.0.0.0:...` は既存 WIRE-7 と同じ理由（TLS 未構成
//!   時の非ループバック拒否）で起動拒否されること（HTTP-9）

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
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

/// 子プロセスの stderr を専用スレッドで読み続け、行を `mpsc::Receiver` 経由で
/// 届ける（`BufReader::read_line` はデッドラインを持たないブロッキング呼び出し
/// のため、呼び出し元は `recv_timeout` で確実に打ち切れるようにする）。
fn spawn_stderr_reader(child: &mut Child) -> mpsc::Receiver<String> {
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
    rx
}

/// `listening on` を含む行が来るまで待ち、読み取った行を `seen` へ積む
/// （`recv_timeout` は一度読んだ行を再度読めないため、`listening on` 行数の
/// 厳密検証をする呼び出し元は本関数が読んだ行を `drain_remaining` の結果と
/// 合算する必要がある）。
fn wait_for_listening(
    rx: &mpsc::Receiver<String>,
    deadline: Instant,
    seen: &mut Vec<String>,
) -> bool {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let is_listening = line.contains("listening on");
                seen.push(line);
                if is_listening {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
}

/// 子プロセス停止（`kill`＋`wait`）後、stderr 読み取りスレッドが送信済みの
/// 残り行をすべて回収する（送信側スレッドはパイプが閉じれば終了し
/// `recv`/`recv_timeout` は `Err` を返すため、有限時間で必ず終わる）。
fn drain_remaining(rx: &mpsc::Receiver<String>, deadline: Instant) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => lines.push(line),
            Err(_) => break,
        }
    }
    lines
}

/// 未指定／`--surface sql` はいずれも `listening on` に到達し、`listening on`
/// を含む行がちょうど 1 行（表層表示行は SQL では出さない契約。Issue #735）
/// であること。
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

        let rx = spawn_stderr_reader(&mut child);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut collected: Vec<String> = Vec::new();
        let listening = wait_for_listening(&rx, deadline, &mut collected);

        let _ = child.kill();
        let _ = child.wait();
        collected.extend(drain_remaining(
            &rx,
            Instant::now() + Duration::from_secs(5),
        ));
        let remaining = collected;

        assert!(
            listening,
            "args={extra_args:?}: expected to reach listening state"
        );

        let listening_lines = remaining
            .iter()
            .filter(|l| l.contains("listening on"))
            .count();
        assert_eq!(
            listening_lines, 1,
            "args={extra_args:?}: expected exactly one 'listening on' line, got: {remaining:?}"
        );
        assert!(
            !remaining.iter().any(|l| l.contains("surface nosql")),
            "args={extra_args:?}: sql surface must not print the nosql surface line, got: {remaining:?}"
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

/// `--surface nosql` は SQL wire リスナーを一切 bind せず、HTTP/1.1
/// リスナー（Issue #735・#743。読み取りタイムアウト・接続数リミッター
/// 適用済みの暫定ハンドラ。要求の解釈はまだ行わない）だけを 1 本 bind する。
/// `listening on` に到達し、かつちょうど 1 行であり、表層表示行を伴うことを
/// 確認する（HTTP-1 の排他方針）。
#[test]
fn nosql_starts_single_http_stub_listener_and_does_not_serve_pg_wire() {
    let fixture = TempFixtureDir::new("nosql-stub");
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
            "--surface",
            "nosql",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let rx = spawn_stderr_reader(&mut child);
    let deadline = Instant::now() + Duration::from_secs(10);

    // `listening on` に到達するまでの行を手元にも積んでおく（アドレス抽出用。
    // `wait_for_listening` は到達を判定するだけで行そのものは返さないため、
    // ここでは専用に読み切る）。
    let mut collected: Vec<String> = Vec::new();
    let addr = 'wait: loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for listening on");
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(addr) = line.trim_end().strip_prefix("wire-server: listening on ") {
                    let addr = addr.to_string();
                    collected.push(line);
                    break 'wait addr;
                }
                collected.push(line);
            }
            Err(_) => panic!("stderr reader stopped before listening on: {collected:?}"),
        }
    };

    // stub は要求を読まない: SSLRequest（8 バイト）を送っても、SQL wire の
    // ような応答（先頭バイト `N`／`E`）は返らず、接続は書き込み失敗
    // （リセット系）または読み取り側の即時 EOF/リセットに終わる。
    let mut stream = TcpStream::connect(&addr).expect("connect to nosql stub listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let ssl_request: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];
    match stream.write_all(&ssl_request) {
        Ok(()) => {
            let mut byte = [0u8; 1];
            match stream.read(&mut byte) {
                Ok(0) => {}
                Ok(_) => assert!(
                    byte[0] != b'N' && byte[0] != b'E',
                    "nosql stub must not answer like the SQL wire, got byte {:?}",
                    byte[0]
                ),
                Err(e) => {
                    let kind = e.kind();
                    assert!(
                        kind == std::io::ErrorKind::ConnectionReset
                            || kind == std::io::ErrorKind::BrokenPipe,
                        "unexpected read error from nosql stub: {e:?}"
                    );
                }
            }
        }
        Err(e) => {
            let kind = e.kind();
            assert!(
                kind == std::io::ErrorKind::ConnectionReset
                    || kind == std::io::ErrorKind::BrokenPipe,
                "unexpected write error to nosql stub: {kind:?}"
            );
        }
    }
    drop(stream);

    let _ = child.kill();
    let _ = child.wait();
    let remaining = drain_remaining(&rx, Instant::now() + Duration::from_secs(5));
    collected.extend(remaining);

    let listening_lines = collected
        .iter()
        .filter(|l| l.contains("listening on"))
        .count();
    assert_eq!(
        listening_lines, 1,
        "expected exactly one 'listening on' line, got: {collected:?}"
    );
    assert!(
        collected.iter().any(|l| l.contains("surface nosql")),
        "expected a nosql surface indicator line, got: {collected:?}"
    );
}

/// `--surface nosql --bind 0.0.0.0:...` は既存 WIRE-7 と同じ理由（TLS 未構成
/// 時の非ループバック拒否）で起動拒否される（HTTP-9: 両表層が同じ
/// `GuardedBindAddrs` を通ることの確認）。
#[test]
fn nosql_non_loopback_bind_exits_non_zero() {
    let fixture = TempFixtureDir::new("nosql-non-loopback");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    for bind_addr in ["0.0.0.0:0", "[::]:0"] {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                bind_addr,
                "--surface",
                "nosql",
            ])
            .output()
            .expect("spawn wire-server");

        assert!(
            !output.status.success(),
            "non-loopback bind {bind_addr} with --surface nosql must exit non-zero"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("refusing to bind non-loopback") && stderr.contains("TLS"),
            "stderr for {bind_addr} should explain the TLS-related refusal, got: {stderr}"
        );
    }
}
