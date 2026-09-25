//! `CREATE VIEW` / `DROP VIEW`（非マテリアライズド。TABLE-18・SQL-23・
//! TASK-205、Issue #909）の簡易クエリプロトコル経由（生バイトクライアント）
//! 検証（層 A）。
//!
//! ビューの意味論そのもの（許可リスト・ネスト深さ・循環拒否・RLS 暗黙適用・
//! 依存検査）は `crates/engine/tests/table18_view.rs` が確定オラクルとして
//! 検証済みのため、本ファイルは `wire_drop_table.rs` と同じ流儀で以下に徹する:
//! - DDL 実行権限（`--ddl-allowed-users`）が wire フレーミング越しに正しく
//!   反映されること
//! - `CommandComplete` タグ（`CREATE VIEW`／`DROP VIEW`）
//! - 2 接続（別テナント）でのビュー経由読み取りの RLS 暗黙適用
//! - `42P07`／`2BP01`／`42809` の `ErrorResponse` SQLSTATE
//! - 複数文メッセージで `CREATE VIEW` を最後以外に置くと `0A000`

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-create-view-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_server_with_engine_and_store(
    store: UserStore,
    engine: Arc<EngineCore>,
) -> std::net::SocketAddr {
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

fn allowed_store(users: &[(&str, &str, &str)], allowed: &[&str]) -> UserStore {
    let users_path = write_user_store_file(users);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    store
        .with_ddl_allowed_users(&allowed.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .expect("allowed usernames must be known")
}

/// DDL 実行権限を持たないユーザーは `CREATE VIEW`／`DROP VIEW` いずれも
/// `42501` で拒否され、接続は維持される。
#[test]
fn wire_create_view_without_permission_is_rejected_and_connection_stays_usable() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE VIEW v AS SELECT * FROM docs");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP VIEW v");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT COUNT(*) FROM docs");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 許可ユーザーは `CREATE VIEW`／`DROP VIEW` に成功し、それぞれ pg 互換の
/// `CommandComplete` タグを受け取る。
#[test]
fn wire_create_and_drop_view_succeed_for_allowed_user() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE VIEW v AS SELECT * FROM docs");
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "CREATE VIEW");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP VIEW v");
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "DROP VIEW");
    read_ready_for_query(&mut stream);

    // drop 済みのため参照は 42P01。
    send_simple_query(&mut stream, "SELECT * FROM v LIMIT 10");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);
}

/// ビュー経由の読み取りには、参照した**接続自身**の `PolicyContext` で RLS が
/// 暗黙適用される（作成者〔alice〕の可視性は参照者〔bob〕へ引き継がれない。
/// RLS-10 (b)）。
#[test]
fn wire_view_read_applies_reader_session_rls() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(
        &[
            ("alice", "tenant-a", "correct-horse"),
            ("bob", "tenant-b", "battery-staple"),
        ],
        &["alice"],
    );
    let addr = spawn_server_with_engine_and_store(store, core);

    let mut alice_stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(
        &mut alice_stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-alice-1'",
    );
    let _tag = read_command_complete(&mut alice_stream);
    read_ready_for_query(&mut alice_stream);

    send_simple_query(
        &mut alice_stream,
        "CREATE VIEW ja_docs AS SELECT id FROM docs WHERE lang = 'ja'",
    );
    let tag = read_command_complete(&mut alice_stream);
    assert_eq!(tag, "CREATE VIEW");
    read_ready_for_query(&mut alice_stream);

    let mut bob_stream = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    send_simple_query(
        &mut bob_stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (2, '[0.4,0.5,0.6]', 'ja') USING OPERATION_ID 'op-bob-1'",
    );
    let _tag = read_command_complete(&mut bob_stream);
    read_ready_for_query(&mut bob_stream);

    // bob はビュー経由でも自分の行のみを見る（alice の行は不可視）。
    send_simple_query(&mut bob_stream, "SELECT id FROM ja_docs LIMIT 100");
    let _columns = read_row_description(&mut bob_stream);
    let row = read_data_row(&mut bob_stream);
    assert_eq!(row, vec![Some("2".to_string())]);
    // 2 件目は無いはず（`COMMAND COMPLETE` が続けて読める）。
    let tag = read_command_complete(&mut bob_stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut bob_stream);
}

/// 既存テーブルと同名の `CREATE VIEW` は `42P07`。テーブルを参照するビューが
/// 残っている間の `DROP TABLE` は `2BP01`。`DROP TABLE`／`DROP VIEW` に誤った
/// 種別の名前を指定すると `42809`。
#[test]
fn wire_view_error_sqlstates() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE VIEW docs AS SELECT * FROM docs");
    expect_error_response_with_sqlstate(&mut stream, "42P07");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE VIEW v AS SELECT * FROM docs");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TABLE docs");
    expect_error_response_with_sqlstate(&mut stream, "2BP01");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TABLE v");
    expect_error_response_with_sqlstate(&mut stream, "42809");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP VIEW docs");
    expect_error_response_with_sqlstate(&mut stream, "42809");
    read_ready_for_query(&mut stream);
}

/// 複数文メッセージ（セミコロン区切り）で `CREATE VIEW` を最後以外に置くと
/// `0A000`（`statement_splitter::MultiStatementError::WriteNotLast`。DDL も
/// `INSERT` 等の書き込み系文と同じ分類を共有する）。
#[test]
fn wire_create_view_not_last_in_multi_statement_message_is_rejected() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "CREATE VIEW v AS SELECT * FROM docs; SELECT COUNT(*) FROM docs",
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);
}
