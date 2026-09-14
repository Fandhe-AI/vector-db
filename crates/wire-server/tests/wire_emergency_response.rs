//! wire-server の結合テスト（TASK-97・TASK-153、対象ビヘイビア: RECOVER-6・
//! ERR-5。ポインタ: `docs/spec/05-tasks.md` TASK-97・TASK-153・
//! `docs/spec/04-behavior/recovery.md` RECOVER-6・
//! `docs/spec/04-behavior/error-format.md` ERR-5）。
//!
//! 責務分担: commit 成功境界を跨いだ panic → 緊急応答の同期送出 → abort という
//! 機構自体の契約（登録・送出判定・世代一致・abort シグナル）は
//! `crates/engine/tests/recover6_panic_hook.rs` が既に固定している
//! （子プロセスの自己再帰起動・`EmergencyResponseRegistration::register`・
//! `install_panic_hook` はいずれも同ファイルと同型のパターンをここでも使う）。
//! 本ファイルはその機構が実際に送出するバイト列の**内容**――wire-server が
//! `crate::simple_query::build_emergency_response_bytes` で組み立てる実
//! ErrorResponse（`S`=`ERROR`・`C`=`XX000`・`M`=`internal error`・`D`=
//! `crate::error_response::MAY_BE_COMMITTED_DETAIL`）――を実 TCP 越しに検証する
//! ことに専念する（`wire_server::simple_query::emergency_response_bytes()` の
//! 公開アクセサ経由で登録するバイト列を取得する点のみ、engine 側の結合テスト
//! （不透明なマーカー列を独自に登録する）と異なる）。
//!
//! `wire_error_response.rs::read_error_response_fields` の `!has_detail`
//! アサーションが固定する「通常応答は `D` を含まない」契約とは対照的に、本
//! ファイルは「緊急応答は `D` を含む」契約を固定する。

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

#[path = "../../engine/src/test_util/temp_db.rs"]
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

// `crates/engine/tests/recover6_panic_hook.rs` と衝突しない独自の環境変数名を
// 使う（同一プロセスツリーで両ファイルのサブプロセス起動が競合しないため）。
const CHILD_ROLE_ENV: &str = "WIRE_RECOVER6_CHILD_ROLE";
const CHILD_DB_ENV: &str = "WIRE_RECOVER6_CHILD_DB";
const CHILD_ADDR_FILE_ENV: &str = "WIRE_RECOVER6_CHILD_ADDR_FILE";

/// 子プロセスが [`CHILD_ADDR_FILE_ENV`] へ書き出した listen アドレスが現れるまで
/// ポーリングして読み取る（`recover6_panic_hook.rs::wait_for_child_addr` と同型。
/// 標準出力を使わない理由は同ファイルのモジュールコメント参照）。
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

/// body 中の 1 フィールド（タグ 1 バイト＋NUL 終端文字列）を機械的に抽出する
/// テスト専用ヘルパー（`error_response.rs::tests::find_field` と同型）。
/// 受信データ経路ではないテストコードのため `unwrap`/`expect`・添字アクセスは
/// 使わず `get`/`position` ベースで実装する。
fn find_field(body: &[u8], tag: u8) -> Option<String> {
    let mut idx = 0;
    while idx < body.len() {
        let this_tag = *body.get(idx)?;
        if this_tag == 0 {
            return None;
        }
        let value_start = idx + 1;
        let nul_offset = body.get(value_start..)?.iter().position(|&b| b == 0)?;
        let value_end = value_start + nul_offset;
        if this_tag == tag {
            let bytes = body.get(value_start..value_end)?;
            return std::str::from_utf8(bytes).ok().map(str::to_string);
        }
        idx = value_end + 1;
    }
    None
}

// ---------------------------------------------------------------------------
// commit 後 panic → 緊急応答（D=state=may_be_committed）の層 A 検証
// ---------------------------------------------------------------------------

#[test]
fn subprocess_commit_then_panic_emergency_response_carries_may_be_committed_detail() {
    if std::env::var(CHILD_ROLE_ENV).as_deref() == Ok("server") {
        // 子プロセス・サーバー役: TCP を listen し、接続を 1 本受理してから
        // wire-server が実際に組み立てる緊急応答バイト列
        // （`wire_server::simple_query::emergency_response_bytes()`）を登録し、
        // commit → panic を注入する（`execute_and_respond` の実運用経路と
        // 同じ `ResponseBoundaryGuard` → 登録 → commit → panic の順序）。
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
        let op_id = OperationId::parse("op-wire-recover6-commit-then-panic")
            .expect("child: valid operation_id");
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[0.1, 0.2, 0.3],
            metadata: &[],
        };

        let response_bytes = wire_server::simple_query::emergency_response_bytes()
            .expect("child: wire-server emergency response bytes must encode")
            .to_vec();

        let _response_boundary = ResponseBoundaryGuard::new();
        let _registration = EmergencyResponseRegistration::register(
            response_bytes,
            server_stream,
            Duration::from_secs(5),
        );

        core.insert_row(&ctx, TABLE, 1, &row, Some(&op_id))
            .expect("child: commit must succeed");

        panic!("injected panic after commit succeeded (wire emergency response path)");
    }

    // 親プロセス側。
    let path = unique_db_path("wire-recover6-commit-then-panic");
    let _cleanup = CleanupGuard(path.clone());

    let addr_file = path.with_extension("addr");
    let _addr_file_cleanup = CleanupGuard(addr_file.clone());
    let _ = std::fs::remove_file(&addr_file);

    let exe = std::env::current_exe().expect("current_exe");
    let mut server_child = std::process::Command::new(&exe)
        .arg("--exact")
        .arg("subprocess_commit_then_panic_emergency_response_carries_may_be_committed_detail")
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

    // ErrorResponse ('E') フレームを宣言長どおり読み切る。
    let mut header = [0u8; 1];
    client.read_exact(&mut header).expect("read type byte");
    assert_eq!(header[0], b'E', "expected ErrorResponse type byte");

    let mut len_buf = [0u8; 4];
    client.read_exact(&mut len_buf).expect("read length field");
    let declared_len = i32::from_be_bytes(len_buf);
    assert!(
        declared_len >= 4,
        "declared length must cover at least the length field itself; got {declared_len}"
    );
    let body_len = declared_len as usize - 4;
    let mut body = vec![0u8; body_len];
    client.read_exact(&mut body).expect("read body");

    assert_eq!(
        find_field(&body, b'S').as_deref(),
        Some("ERROR"),
        "severity"
    );
    assert_eq!(
        find_field(&body, b'C').as_deref(),
        Some("XX000"),
        "sqlstate (ERR-2 分類は D 追加後も不変)"
    );
    assert_eq!(
        find_field(&body, b'M').as_deref(),
        Some("internal error"),
        "message"
    );
    assert_eq!(
        find_field(&body, b'D').as_deref(),
        Some("state=may_be_committed"),
        "detail (ERR-5)"
    );
    assert_eq!(
        body.iter().filter(|&&b| b == b'D').count(),
        1,
        "D field must appear exactly once"
    );
    assert_eq!(body.last().copied(), Some(0), "field terminator");

    // 唯一の応答であること（追加バイトが来ない）を確認する。
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
    // （may_be_committed の名の通り、実際に commit されていたケース。
    // `recover6_panic_hook.rs` と同じ確認方法）。
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
