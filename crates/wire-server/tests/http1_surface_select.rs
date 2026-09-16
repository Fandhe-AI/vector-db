//! `--surface`（Issue #734・#735・TASK-171／HTTP-1・HTTP-9）を実バイナリの
//! 子プロセスとして起動し、CLI 引数の受理・拒否（fail-closed）・選択表層に
//! 応じたリスナー分岐を外形的に検証する層 A 結合テスト（Issue #736）。
//!
//! 子プロセス起動・stderr 読み取りの共通ヘルパーは
//! `tests/common/mod.rs`（Issue #736 節）を使う。
//!
//! ## 観測境界
//!
//! 「選択表層のリスナーが 1 本だけ起動する」ことを、本ファイルでは次の
//! 2 点の組み合わせでのみ観測する（`lsof`／`/proc/<pid>/net/tcp` による
//! ソケット列挙は macOS/Linux 間で非移植・CI 環境依存であり、「無ければ
//! skip」にすると vacuous pass になるため採用しない）:
//!
//! 1. stderr に現れる `wire-server: listening on` 行がちょうど 1 行
//!    （`bind()` が 1 回しか呼ばれないことの外形証跡）
//! 2. その唯一の addr で選択表層の挙動が判別できること
//!    （`sql`: SSLRequest → 先頭バイト `N`。`nosql`: 暫定ハンドラ
//!    〔Issue #743・`handle_connection_interim`〕が有界 1 回 read の後
//!    応答を書かずに閉じるため `N`/`E` を返さない）
//!
//! sql 側の bind ガード自体（loopback 以外の拒否）は `tests/wire7_bind_guard.rs`
//! が担い、本ファイルは表層選択と分岐に焦点を当てる。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[path = "common/mod.rs"]
mod common;

use common::{run_wire_server_to_exit, write_empty_user_store, SpawnedServer, TempFixtureDir};

/// 選択された表層を SSLRequest（8 バイト）への応答で判別する。
enum Probe {
    /// sql wire: 認証前に SSLRequest へ応答する（`N`＝非対応）。
    PgWireAnswersN,
    /// nosql 暫定ハンドラ（Issue #735・#743。固定長 1 バイトの有界
    /// 1 回 read の後、応答を書かずに shutdown する）: 応答しない
    /// （EOF／`ConnectionReset`／`BrokenPipe` のいずれも許容）。
    StubDoesNotAnswer,
}

/// テーブルテストの 1 ケース（受理側）。
struct AcceptCase {
    label: &'static str,
    extra_args: &'static [&'static str],
    /// `nosql` のときだけ出る表層表示行の有無。
    surface_line: bool,
    probe: Probe,
}

/// 未指定／`--surface sql`／`--surface nosql` はいずれも `listening on` に
/// 到達し、リスナーがちょうど 1 本（表層表示行の有無・選択表層の挙動が
/// 期待どおり）であることを確認する。
#[test]
fn surface_selection_accepts_and_starts_single_listener() {
    let cases = [
        AcceptCase {
            label: "unset",
            extra_args: &[],
            surface_line: false,
            probe: Probe::PgWireAnswersN,
        },
        AcceptCase {
            label: "sql",
            extra_args: &["--surface", "sql"],
            surface_line: false,
            probe: Probe::PgWireAnswersN,
        },
        AcceptCase {
            label: "nosql",
            extra_args: &["--surface", "nosql"],
            surface_line: true,
            probe: Probe::StubDoesNotAnswer,
        },
    ];

    for case in cases {
        let fixture = TempFixtureDir::new(case.label);
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
        let db_path = fixture.db_path_str();

        let mut base_args = vec![
            "--users",
            users_path.as_str(),
            "--db",
            db_path.as_str(),
            "--bind",
            "127.0.0.1:0",
        ];
        base_args.extend_from_slice(case.extra_args);

        let mut server = SpawnedServer::spawn(&base_args);
        let deadline = Instant::now() + Duration::from_secs(10);
        let addr = server.wait_for_listening(deadline);
        assert!(
            addr.is_some(),
            "label={}: expected to reach listening state",
            case.label
        );
        let addr = addr.expect("checked above");

        probe_listener(&addr, &case.probe, case.label);

        let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
        let listening_lines = lines.iter().filter(|l| l.contains("listening on")).count();
        assert_eq!(
            listening_lines, 1,
            "label={}: expected exactly one 'listening on' line, got: {lines:?}",
            case.label
        );
        let has_surface_line = lines.iter().any(|l| l.contains("surface nosql"));
        assert_eq!(
            has_surface_line, case.surface_line,
            "label={}: unexpected surface-line presence, got: {lines:?}",
            case.label
        );
    }
}

/// 到達した addr へ SSLRequest を送り、選択表層に応じた応答パターンを
/// 検証する。
fn probe_listener(addr: &str, probe: &Probe, label: &str) {
    let mut stream = TcpStream::connect(addr).unwrap_or_else(|e| {
        panic!("label={label}: connect to listener {addr} failed: {e:?}");
    });
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let ssl_request: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];

    match probe {
        Probe::PgWireAnswersN => {
            stream.write_all(&ssl_request).expect("write SSLRequest");
            let mut byte = [0u8; 1];
            stream
                .read_exact(&mut byte)
                .expect("read SSLRequest response");
            assert_eq!(
                byte[0], b'N',
                "label={label}: expected SQL wire to answer SSLRequest with 'N'"
            );
        }
        Probe::StubDoesNotAnswer => match stream.write_all(&ssl_request) {
            Ok(()) => {
                let mut byte = [0u8; 1];
                match stream.read(&mut byte) {
                    Ok(0) => {}
                    Ok(_) => assert!(
                        byte[0] != b'N' && byte[0] != b'E',
                        "label={label}: nosql http listener must not answer like the SQL wire, got byte {:?}",
                        byte[0]
                    ),
                    Err(e) => {
                        let kind = e.kind();
                        assert!(
                            kind == std::io::ErrorKind::ConnectionReset
                                || kind == std::io::ErrorKind::BrokenPipe,
                            "label={label}: unexpected read error from nosql http listener: {e:?}"
                        );
                    }
                }
            }
            Err(e) => {
                let kind = e.kind();
                assert!(
                    kind == std::io::ErrorKind::ConnectionReset
                        || kind == std::io::ErrorKind::BrokenPipe,
                    "label={label}: unexpected write error to nosql http listener: {kind:?}"
                );
            }
        },
    }
}

/// テーブルテストの 1 ケース（拒否側）。
struct RejectCase {
    label: &'static str,
    /// `--bind` に渡す値（`--surface`／CLI 拒否ケースは常にループバック、
    /// bind ガードそのものを確認するケースだけ非ループバックにする）。
    bind: &'static str,
    extra_args: &'static [&'static str],
    /// 非 0 終了に加えて stderr が含むべき部分文字列（すべて満たす必要がある）。
    stderr_all_of: &'static [&'static str],
}

/// 不正値・値欠落・重複指定・非ループバック bind（nosql）はいずれも
/// fail-closed（非 0 終了・理由を含む stderr）で拒否される。
#[test]
fn surface_selection_rejects_invalid_configurations() {
    let cases = [
        RejectCase {
            label: "bogus-value",
            bind: "127.0.0.1:0",
            extra_args: &["--surface", "bogus"],
            stderr_all_of: &["--surface"],
        },
        RejectCase {
            label: "case-variant-upper",
            bind: "127.0.0.1:0",
            extra_args: &["--surface", "SQL"],
            stderr_all_of: &["--surface"],
        },
        RejectCase {
            label: "case-variant-mixed",
            bind: "127.0.0.1:0",
            extra_args: &["--surface", "Nosql"],
            stderr_all_of: &["--surface"],
        },
        RejectCase {
            label: "flag-name-as-value",
            bind: "127.0.0.1:0",
            // 直後の既知フラグ名をそのまま値として食い、閉じた語彙の
            // いずれとも一致しないため拒否される（`--search-engine` と同じ
            // 「次トークンを無条件で値とみなす」仕様の確認）。
            extra_args: &["--surface", "--bind"],
            stderr_all_of: &["--surface"],
        },
        RejectCase {
            label: "duplicate-same-value",
            bind: "127.0.0.1:0",
            extra_args: &["--surface", "sql", "--surface", "sql"],
            stderr_all_of: &["--surface"],
        },
        RejectCase {
            label: "duplicate-different-value",
            bind: "127.0.0.1:0",
            extra_args: &["--surface", "sql", "--surface", "nosql"],
            stderr_all_of: &["--surface"],
        },
        RejectCase {
            label: "missing-value-at-end",
            bind: "127.0.0.1:0",
            extra_args: &["--surface"],
            stderr_all_of: &["--surface"],
        },
        RejectCase {
            label: "nosql-non-loopback-ipv4",
            bind: "0.0.0.0:0",
            extra_args: &["--surface", "nosql"],
            stderr_all_of: &["refusing to bind non-loopback", "TLS"],
        },
        RejectCase {
            label: "nosql-non-loopback-ipv6",
            bind: "[::]:0",
            extra_args: &["--surface", "nosql"],
            stderr_all_of: &["refusing to bind non-loopback", "TLS"],
        },
    ];

    for case in cases {
        let fixture = TempFixtureDir::new(case.label);
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
        let db_path = fixture.db_path_str();

        let mut args = vec![
            "--users",
            users_path.as_str(),
            "--db",
            db_path.as_str(),
            "--bind",
            case.bind,
        ];
        args.extend_from_slice(case.extra_args);

        let output = run_wire_server_to_exit(&args);
        assert!(
            !output.status.success(),
            "label={}: expected non-zero exit",
            case.label
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        for needle in case.stderr_all_of {
            assert!(
                stderr.contains(needle),
                "label={}: expected stderr to contain {needle:?}, got: {stderr}",
                case.label
            );
        }
    }
}
