//! `engine::recovery::panic_hook`（TASK-97、対象ビヘイビア: RECOVER-6・ERR-1）の
//! 結合テスト。commit 成功境界を跨いだ panic の観測可能性側（緊急応答の同期送出→
//! abort）を、実 TCP 越し・実プロセス強制終了込みで検証する。
//!
//! 収録範囲: 本ファイルは `pub` API（[`engine::recovery::panic_hook::
//! install_panic_hook`]・[`EmergencyResponseRegistration::register`]・
//! [`engine::recovery::commit_boundary::ResponseBoundaryGuard`]・
//! [`EngineCore::insert_row`]）のみを使う。`emergency_send_decision`・
//! `try_send_emergency_response` 等の `pub(crate)`／private 項目は
//! `crates/engine/src/recovery/panic_hook.rs` の `#[cfg(test)] mod tests`
//! （同一クレート内ユニットテスト）で検証済み（`commit_boundary.rs` と同じ
//! 「結合テストは公開 API のみ・内部状態はユニットテストで検証する」方針。
//! テスト専用の feature ゲート API は新設しない）。
//!
//! プロセス外検証は `crates/engine/src/recovery/commit_boundary.rs` の
//! `subprocess_*` テスト群と同型（自己再帰: `std::env::current_exe()` を
//! 環境変数付きで再実行する）。子の listen アドレス受け渡しは標準出力を使わない
//! （`cargo test` のテストハーネスが `--list` 目的で同一バイナリを再度起動する
//! 際にパイプ・fd の扱いが競合し flaky になったため、ファイル経由のハンド
//! シェイクへ切り替えた）。

use std::io::Read as _;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::commit_boundary::ResponseBoundaryGuard;
use engine::recovery::panic_hook::{install_panic_hook, EmergencyResponseRegistration};
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const DIM: u32 = 3;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

fn open_storage_with_table(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
}

/// 緊急応答チャネルに登録する固定バイト列（テストの都合上、wire-server の実際の
/// ErrorResponse エンコード規約〔`crates/wire-server/src/result_encoder.rs`〕とは
/// 独立に決めたマーカー列。`panic_hook` は登録バイト列の中身を解釈せず、そのまま
/// 送出するだけなので、フォーマット自体は本テストの関心事ではない）。
const EMERGENCY_MARKER: &[u8] = b"RECOVER6_EMERGENCY_MAY_BE_COMMITTED";

const CHILD_ROLE_ENV: &str = "ENGINE_RECOVER6_CHILD_ROLE";
const CHILD_DB_ENV: &str = "ENGINE_RECOVER6_CHILD_DB";
/// 子（サーバー役）が listen アドレスを書き出すファイルのパスを渡す環境変数。
/// 標準出力ではなくファイルを使う理由はモジュール冒頭コメント参照。
const CHILD_ADDR_FILE_ENV: &str = "ENGINE_RECOVER6_CHILD_ADDR_FILE";

/// 子プロセスが [`CHILD_ADDR_FILE_ENV`] へ書き出したアドレス行が現れるまで
/// ポーリングして読み取る（存在しない・読み取り失敗はリトライ、タイムアウトで
/// panic）。子はアドレスの書き込みを一時ファイル＋リネームで行わない単純な
/// `write` だが、`listener.bind` 直後の 1 回書き込みのみでレースの余地が
/// 小さいため、内容が空でない・改行を含むまで待つことで部分書き込みを弾く。
fn wait_for_child_addr(addr_file: &std::path::Path, timeout: Duration) -> String {
    let start = std::time::Instant::now();
    loop {
        if let Ok(content) = std::fs::read_to_string(addr_file) {
            if let Some(line) = content.lines().next() {
                if !line.is_empty() {
                    return line.to_string();
                }
            }
        }
        if start.elapsed() > timeout {
            panic!("child did not announce its listen address within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("subprocess did not terminate within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn assert_aborted(status: std::process::ExitStatus) {
    use std::os::unix::process::ExitStatusExt as _;
    assert!(
        !status.success(),
        "child must not exit successfully; status={status:?}"
    );
    assert_eq!(
        status.signal(),
        Some(6),
        "child must be terminated by SIGABRT (std::process::abort); status={status:?}"
    );
}

// ---------------------------------------------------------------------------
// シナリオ 1: pending かつ登録あり → 緊急応答を送出してから abort する
// ---------------------------------------------------------------------------

#[test]
fn subprocess_commit_then_panic_sends_emergency_response_then_aborts() {
    if std::env::var(CHILD_ROLE_ENV).as_deref() == Ok("server") {
        // 子プロセス・サーバー役: TCP を listen し、接続を 1 本受理してから
        // commit → panic を注入する。
        let listener = TcpListener::bind("127.0.0.1:0").expect("child: bind");
        let addr = listener.local_addr().expect("child: local_addr");
        let addr_file = std::env::var(CHILD_ADDR_FILE_ENV).expect("child: addr file env");
        std::fs::write(&addr_file, format!("{addr}\n")).expect("child: write addr file");

        let (server_stream, _) = listener.accept().expect("child: accept");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("child: set write timeout");

        install_panic_hook();

        let db_path = std::env::var(CHILD_DB_ENV).expect("child: db path env");
        let storage = open_storage_with_table(std::path::Path::new(&db_path));
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let ctx = PolicyContext::new("tenant-a").expect("child: valid tenant");
        let op_id =
            OperationId::parse("op-recover6-commit-then-panic").expect("child: valid operation_id");
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[0.1, 0.2, 0.3],
            metadata: &[],
        };

        // wire-server::simple_query::execute_and_respond と同じ順序を再現する:
        // ResponseBoundaryGuard 生成 → 緊急応答登録 → commit を伴う処理 → panic。
        let _response_boundary = ResponseBoundaryGuard::new();
        let _registration = EmergencyResponseRegistration::register(
            EMERGENCY_MARKER.to_vec(),
            server_stream,
            Duration::from_secs(5),
        );

        core.insert_row(&ctx, TABLE, 1, &row, Some(&op_id))
            .expect("child: commit must succeed");

        panic!("injected panic after commit succeeded (RECOVER-6 emergency path)");
    }

    // 親プロセス側。DB ファイル・テーブルの作成は子プロセス側で 1 回だけ行う
    // （親が先に作ってしまうと、子の `open_storage_with_table` がテーブル重複で
    // 失敗し、緊急応答チャネルを登録する前に panic してしまう）。
    let path = unique_db_path("recover6-commit-then-panic");
    let _cleanup = CleanupGuard(path.clone());

    let addr_file = path.with_extension("addr");
    let _addr_file_cleanup = CleanupGuard(addr_file.clone());
    let _ = std::fs::remove_file(&addr_file);

    let exe = std::env::current_exe().expect("current_exe");
    let mut server_child = std::process::Command::new(&exe)
        .arg("--exact")
        .arg("subprocess_commit_then_panic_sends_emergency_response_then_aborts")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_ROLE_ENV, "server")
        .env(CHILD_DB_ENV, &path)
        .env(CHILD_ADDR_FILE_ENV, &addr_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn server child");

    let addr = wait_for_child_addr(&addr_file, Duration::from_secs(10));
    let mut client = TcpStream::connect(&addr).expect("connect to child server");
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set client read timeout");

    // 緊急応答マーカーの受信を待つ。
    let mut received = Vec::new();
    let mut buf = [0u8; 256];
    while received.len() < EMERGENCY_MARKER.len() {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    assert_eq!(
        received, EMERGENCY_MARKER,
        "client must observe exactly the registered emergency response bytes as the sole response"
    );

    // 追加バイトが来ないこと（唯一の応答であること）を確認する。
    client
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("set short read timeout");
    let mut extra = [0u8; 16];
    let n = client.read(&mut extra).unwrap_or(0);
    assert_eq!(
        n, 0,
        "no additional bytes must follow the emergency response (sole-response contract)"
    );

    let status = wait_for_exit(&mut server_child, Duration::from_secs(30));
    assert_aborted(status);

    // commit 自体は成功しているため、再オープン後も行が可視であること
    // （may_be_committed の名の通り、実際に commit されていたケース）。
    // `EngineCore::insert_row` はテーブル固有の行ストレージ
    // （`tenant::user_rows_table_name`）へ書き込むため、`Storage::get`（汎用
    // `ROWS_TABLE` 読み取り）ではなく SQL 経由で確認する
    // （`commit_boundary.rs` 結合テストの `precommit_failure_leaves_zero_side_effects_after_reopen`
    // と同じ確認方法）。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(
        result.rows.len(),
        1,
        "the committed row must remain visible after the emergency-abort path"
    );
}

// ---------------------------------------------------------------------------
// シナリオ 2: 登録はあるが commit していない（pending でない）panic
// → 緊急応答を送らず、既存の通常 unwind（プロセス終了だが SIGABRT ではない）
// ---------------------------------------------------------------------------

#[test]
fn subprocess_panic_without_commit_does_not_send_emergency_response() {
    if std::env::var(CHILD_ROLE_ENV).as_deref() == Ok("server") {
        let listener = TcpListener::bind("127.0.0.1:0").expect("child: bind");
        let addr = listener.local_addr().expect("child: local_addr");
        let addr_file = std::env::var(CHILD_ADDR_FILE_ENV).expect("child: addr file env");
        std::fs::write(&addr_file, format!("{addr}\n")).expect("child: write addr file");

        let (server_stream, _) = listener.accept().expect("child: accept");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("child: set write timeout");

        install_panic_hook();

        // commit を一切行わずに登録だけしてから panic する。
        let _response_boundary = ResponseBoundaryGuard::new();
        let _registration = EmergencyResponseRegistration::register(
            EMERGENCY_MARKER.to_vec(),
            server_stream,
            Duration::from_secs(5),
        );

        panic!("injected panic with a registration but no commit (must not send)");
    }

    let placeholder_path = unique_db_path("recover6-no-commit-panic-unused");
    let _cleanup = CleanupGuard(placeholder_path.clone());
    let addr_file = placeholder_path.with_extension("addr");
    let _addr_file_cleanup = CleanupGuard(addr_file.clone());
    let _ = std::fs::remove_file(&addr_file);

    let exe = std::env::current_exe().expect("current_exe");
    let mut server_child = std::process::Command::new(&exe)
        .arg("--exact")
        .arg("subprocess_panic_without_commit_does_not_send_emergency_response")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_ROLE_ENV, "server")
        .env(CHILD_ADDR_FILE_ENV, &addr_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn server child");

    let addr = wait_for_child_addr(&addr_file, Duration::from_secs(10));
    let mut client = TcpStream::connect(&addr).expect("connect to child server");
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set client read timeout");

    let mut received = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                if received.len() >= EMERGENCY_MARKER.len() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        received.is_empty(),
        "no emergency response must be sent when the thread never committed; got {received:?}"
    );

    let status = wait_for_exit(&mut server_child, Duration::from_secs(30));

    // pending でない panic は commit_boundary 側のガードも abort しないため、
    // 通常の Rust panic 終了（SIGABRT ではない）で終わるはず。
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert_ne!(
            status.signal(),
            Some(6),
            "a panic with no preceding commit must not abort the process via SIGABRT; \
             status={status:?}"
        );
    }
    assert!(
        !status.success(),
        "the injected panic must still make the child exit non-zero; status={status:?}"
    );
}
