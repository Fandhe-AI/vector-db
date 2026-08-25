//! 簡易クエリプロトコルが `engine::core::EngineCore` の SQL 表層へ実際に到達し、
//! wire v3 の結果セット／エラー応答として整形されることを検証する結合テスト
//! （TASK-73、対象ビヘイビア: WIRE-1。ポインタ: `docs/spec/05-tasks.md` TASK-73・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-1）。
//!
//! 生バイトの wire クライアント（`tests/common`）で、in-process サーバー
//! （`accept_loop_with_engine`）に対し C1 相当のクエリ・INSERT・SET・空クエリ・
//! 不正 UTF-8・エラー後の接続維持・3 テナント RLS 分離を検証する。実 `psql` 等の
//! 外部クライアントを使う 3 クライアント統合検証は `tests/three_client_e2e.rs`
//! （`#[ignore]`）が担い、本ファイルはその契約の中核（受信バイト列は同一）を
//! 常時（`make ci`）回帰保護する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// `docs` テーブル（`embedding VECTOR(3)` + `lang TEXT`）を持つ `EngineCore` を
/// 新設し、決定的な小規模コーパスを 1 テナント（`tenant-a`）分だけ投入する。
fn new_core_single_tenant() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire1-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let corpus: Vec<(u64, [f32; 3], &str)> = vec![
        (1, [1.0, 0.0, 0.0], "ja"),
        (2, [0.9, 0.1, 0.0], "en"),
        (3, [0.0, 1.0, 0.0], "ja"),
    ];
    for (id, emb, lang) in &corpus {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// C1 相当（`ORDER BY <=> LIMIT`）: `RowDescription`・`DataRow`・
/// `CommandComplete("SELECT n")` が返り、id 列（`int8`）が期待どおりであること。
#[test]
fn wire1_c1_query_returns_row_description_and_data_rows() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
    );

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "embedding", "lang"]);

    let mut ids = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        assert_eq!(row.len(), 3);
        ids.push(row[0].clone().expect("id is not null"));
    }
    // 最近傍は id=1（クエリと同一ベクトル）。
    assert_eq!(ids[0], "1");

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// `INSERT ... USING OPERATION_ID '<id>'` は `CommandComplete("INSERT 0 1")` を
/// 返し、行が実際に永続化されること（同一 `id` の再 INSERT が `23505`
/// `IdConflict` になることで確認する）。
///
/// SQL `INSERT` が書き込む行は常に `Visibility::Private`（`sql::exec::
/// execute_insert` の固定仕様。ポインタ: TASK-80・SQL-10）である一方、wire 認証
/// 経由の `PolicyContext`（`auth::verify` → `PolicyContext::new`）は `Public` の
/// みを許可可視性とするため、挿入直後の SELECT では自テナントの行であっても
/// 見えない（`policy.rs::PolicyContext::is_visible` の許可可視性チェックが先に
/// 弾く）。この非対称は本 Issue のスコープ外（wire 接続へ `Private` 可視性を
/// 付与する経路の要否は別途判断が必要）とし、本テストは可視性へは踏み込まない。
#[test]
fn wire1_insert_returns_command_complete_and_persists_row() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (99, '[0.0,0.0,1.0]', 'fr') USING OPERATION_ID 'op-wire1-insert'",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // 同一 id の再挿入は `23505`（テナント名前空間内の id 重複）で拒否される
    // ことで、直前の INSERT が実際に永続化されたことを確認する。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (99, '[0.0,0.0,1.0]', 'fr') USING OPERATION_ID 'op-wire1-insert-2'",
    );
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
}

/// `SET search_mode = '<literal>'` は `CommandComplete("SET")` を返す。
#[test]
fn wire1_set_search_mode_returns_set_tag() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "SET search_mode = 'recall'");
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SET");
    read_ready_for_query(&mut stream);
}

/// 空白のみのクエリは engine を呼ばず `EmptyQueryResponse` + `ReadyForQuery` を返す
/// （簡易クエリプロトコルの規定挙動）。
#[test]
fn wire1_empty_query_returns_empty_query_response() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "   ");
    expect_empty_query_response(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 不正 UTF-8 のクエリ本文は `08P01`（protocol_violation）で fail-closed に切断する
/// （`ReadyForQuery` は返らない）。
#[test]
fn wire1_non_utf8_query_is_rejected_and_connection_closes() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // 型バイト 'Q' + 不正 UTF-8 バイト列 + 終端 NUL。
    send_length_prefixed_message(&mut stream, b'Q', &[0xFFu8, 0x00]);
    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// 許可リスト外の構文（`42601`）はエラー応答後も接続を維持し、続くクエリが
/// 成功すること（簡易クエリのエラーは切断しない。拡張クエリプロトコルの
/// WIRE-8 切断契約とは独立）。
#[test]
fn wire1_sql_error_keeps_connection_and_next_query_succeeds() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "DROP TABLE docs");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
    );
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// Vector 列は `[v1,v2,...]` 形式の text、NULL は長さ -1 として符号化される。
#[test]
fn wire1_vector_and_null_cells_are_text_encoded() {
    let path = temp_db::unique_db_path("wire1-nulls");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 2.0]), Value::Null],
    )
    .expect("insert row");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,2.0]' LIMIT 1",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1].as_deref(), Some("[1,2]"));
    assert_eq!(row[2], None, "NULL lang cell must decode to None");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    drop(guard);
}

/// 3 テナント（alice/bob/carol）が wire 経由で同一 C1 を実行したとき、
/// `Private` 行は所有テナントを含めて誰にも見えず（`auth::verify` が導出する
/// `PolicyContext` は `Public` のみ許可。ポインタ: RLS-6）、`Public` 行は
/// テナント跨ぎで全員に見えること（`PolicyContext::is_visible` の許可可視性
/// 判定。ポインタ: RLS-7）を確認する。
///
/// 各テナントの `StartupMessage` の `user` パラメータから **サーバー側 `auth::verify`
/// が導出したテナント ID のみ**が `PolicyContext` へ渡ること（クライアント自己申告の
/// `database` パラメータ等はテナント決定に使わない。ポインタ: WIRE-2）を、
/// 3 ユーザーがそれぞれ自分のテナントの `Public` 行を見分けられることで
/// 間接的に確認する（テナント混線があれば行の内訳が一致しなくなる）。
#[test]
fn wire1_three_tenant_visibility_public_shared_private_hidden() {
    let path = temp_db::unique_db_path("wire1-rls");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");

    // 各テナントに Public 行 1 件・Private 行 1 件を投入する（Public 行の
    // ベクトルはテナントごとに異なる近傍点とし、混線があれば発覚するようにする）。
    let tenants: [(&str, u64, u64, [f32; 2]); 3] = [
        ("tenant-a", 1u64, 11u64, [1.0, 0.0]),
        ("tenant-b", 2u64, 12u64, [0.0, 1.0]),
        ("tenant-c", 3u64, 13u64, [-1.0, 0.0]),
    ];
    for (tenant, public_id, private_id, dir) in tenants {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            public_id,
            Visibility::Public,
            &[Value::Vector(dir.to_vec())],
        )
        .expect("insert public row");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            private_id,
            Visibility::Private,
            &[Value::Vector(dir.to_vec())],
        )
        .expect("insert private row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);

    // Public 行 3 件すべてが可視集合（全 id の合計 = 3 件、Private id は含まれない）。
    let expected_public_ids: std::collections::BTreeSet<&str> = ["1", "2", "3"].into();

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        let mut stream = authenticate_to_ready_for_query(addr, user, pw);
        send_simple_query(
            &mut stream,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        );
        let _columns = read_row_description(&mut stream);
        // LIMIT 10 だが可視な Public 行はちょうど 3 件（`allowed_visibilities` が
        // `Private` を含まないため、他テナントを含む全 `Private` 行は候補にすら
        // 入らない。`VectorArena::build_filtered` が構築時点で除外する）。
        let mut seen_ids = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let row = read_data_row(&mut stream);
            let id = row[0].clone().expect("id is not null");
            seen_ids.insert(id);
        }
        assert_eq!(
            seen_ids,
            expected_public_ids.iter().map(|s| s.to_string()).collect(),
            "tenant {user} must see exactly the 3 Public rows and no Private rows"
        );
        let tag = read_command_complete(&mut stream);
        assert_eq!(tag, "SELECT 3");
        read_ready_for_query(&mut stream);
    }

    drop(guard);
}
