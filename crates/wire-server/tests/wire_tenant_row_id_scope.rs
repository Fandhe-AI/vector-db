//! `INSERT` 経路における行 `id` の一意性スコープ（テナント内）契約の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証（TASK-95、対象
//! ビヘイビア: TABLE-12・RLS-9。ポインタ: `docs/spec/05-tasks.md` TASK-95・
//! `docs/spec/04-behavior/data-model.md` TABLE-12・
//! `docs/spec/04-behavior/rls.md` RLS-9）。
//!
//! 意味論の確定オラクルは `crates/engine/tests/row_id_tenant_scope.rs`
//! （`engine::tenant` 直呼び出しによる「他テナント保持 id と未存在 id とで
//! 応答が変化しない」「同一テナント内重複は `23505` で拒否され、応答文言に
//! テナント名・行 id が現れない」ことの確定）であり、本ファイルは同じ規則が
//! **wire フレーミング**（実 TCP・簡易クエリプロトコル）越しにも観測できる
//! ことの追加確認に専念する（`wire_insert_operation_id.rs`・
//! `wire_emergency_response.rs` と同じ「意味論は engine 側オラクルに委ね、
//! フレーミングでの再現に専念する」流儀）。production コードの変更は不要
//! （既存の `sql/exec.rs::TenantWriteError::IdConflict → SqlSurfaceError::
//! IdConflict`（`23505`）写像・固定文言をそのまま `ErrorResponse` へ載せる
//! wire 層の既存経路を確認するのみ）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read as _;
use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

use common::*;

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
    )
}

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-tenant-row-id-scope");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `tenant-b`（id=100）・`tenant-c`（id=200）へ、wire を経由せず
/// `EngineCore::insert_row` で直接 1 行ずつ seed する（`crates/wire-server/tests/
/// wire_emergency_response.rs` と同じ「wire を経由しない直接 API 呼び出し」の
/// 流儀。3 テナント構成にするのは `row_id_tenant_scope.rs` の (i) と同じ理由——
/// 「自テナント（alice/tenant-a）から見て他テナントが `id` を保持しているか
/// 否か」だけを変数にするため）。
fn seed_foreign_tenants(core: &EngineCore) {
    let b = PolicyContext::new(TENANT_B).expect("valid tenant");
    let c = PolicyContext::new(TENANT_C).expect("valid tenant");
    core.insert_row(
        &b,
        TABLE,
        100,
        &RowInput {
            tenant_id: TENANT_B,
            visibility: Visibility::Public,
            embedding: &[0.0, 1.0, 0.0],
            metadata: b"seed-b",
        },
        Some(&OperationId::parse("seed-op-tenant-b").expect("valid operation_id")),
    )
    .expect("seed tenant-b row id=100");
    core.insert_row(
        &c,
        TABLE,
        200,
        &RowInput {
            tenant_id: TENANT_C,
            visibility: Visibility::Public,
            embedding: &[0.0, 0.0, 1.0],
            metadata: b"seed-c",
        },
        Some(&OperationId::parse("seed-op-tenant-c").expect("valid operation_id")),
    )
    .expect("seed tenant-c row id=200");
}

fn spawn_with_alice(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", TENANT_A, "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID '{op_id}'"
    )
}

/// 次のメッセージ 1 件を、型バイト＋長さ＋body の生バイト列のまま読む
/// （パース後の文字列比較ではなく、`CommandComplete`/`ErrorResponse` の
/// 全フィールドがバイト単位で一致することを固定するための汎用ヘルパー。
/// `common::read_command_complete` はタグ文字列へパースする専用ヘルパーの
/// ため、本 Issue の「応答全体の同一性」を確認するにはこちらを使う）。
fn read_raw_message(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let mut out = Vec::with_capacity(1 + len);
    out.push(header[0]);
    out.extend_from_slice(&len_buf);
    out.extend_from_slice(&body);
    out
}

/// 対象ビヘイビア: RLS-9。他テナント（tenant-b）が保持する `id` への
/// `INSERT` と、どのテナントも保持しない `id` への `INSERT` とで、wire 応答
/// （`CommandComplete` の生バイト列＝成否・タグ文言のすべて）が完全に一致
/// すること。物理キーが `(tenant_id, id)` で名前空間化されているため、他
/// テナントの行有無を参照する分岐が構造的に存在しないことをフレーミング
/// 越しに固定する。
#[test]
fn rls9_wire_insert_response_bytes_are_identical_for_foreign_held_id_and_absent_id() {
    let (core, _guard) = new_core_with_docs_table();
    seed_foreign_tenants(&core);
    let mut stream = spawn_with_alice(core);

    // (b) 他テナント（tenant-b）が保持する id=100 と同じ id への自テナント
    // 名義 INSERT。
    send_simple_query(&mut stream, &insert_sql(100, "wire-op-foreign-held"));
    let bytes_foreign_held = read_raw_message(&mut stream);
    assert_eq!(
        bytes_foreign_held.first(),
        Some(&b'C'),
        "expected CommandComplete for id held by another tenant, got: {bytes_foreign_held:?}"
    );
    read_ready_for_query(&mut stream);

    // (c) どのテナントも保持しない id=777 への INSERT。
    send_simple_query(&mut stream, &insert_sql(777, "wire-op-absent"));
    let bytes_absent = read_raw_message(&mut stream);
    assert_eq!(
        bytes_absent.first(),
        Some(&b'C'),
        "expected CommandComplete for an absent id, got: {bytes_absent:?}"
    );
    read_ready_for_query(&mut stream);

    assert_eq!(
        bytes_foreign_held, bytes_absent,
        "wire response bytes must be indistinguishable regardless of whether another tenant holds the id"
    );
}

/// 対象ビヘイビア: TABLE-12。他テナント（tenant-b・tenant-c）を事前に seed
/// した状態でも、同一テナント（tenant-a）内での重複 `id` への `INSERT` は
/// `23505` で拒否され、応答本文に他テナント名・行 `id`（重複対象自身の
/// `id`＝42 を含む）を含む識別子が漏えいしないこと。存在情報秘匿の回帰検証
/// （`row_id_tenant_scope.rs::rls9_insert_response_is_identical_...` と同型
/// のアサーション）。重複対象の id は `100`/`200`（他テナント seed 行の id）
/// と数字列として衝突しない `42` を使う（`row_id_tenant_scope.rs` の
/// `dup_with_foreign`/`dup_without_foreign` テストと同じ値。codex-review
/// 指摘: 従来は `id=1` を使っており、他テナント id（100/200）の非漏えいしか
/// 検査できず「重複対象自身の id が現れない」という本テストの目的を満たして
/// いなかった）。
#[test]
fn table12_wire_insert_duplicate_within_own_tenant_is_rejected_with_23505_without_leaking_row_identifiers(
) {
    let (core, _guard) = new_core_with_docs_table();
    seed_foreign_tenants(&core);
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, &insert_sql(42, "wire-op-dup-first"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // 同一 id・別 operation_id（台帳照合と行キー衝突を混同しないための注意点は
    // `wire_insert_operation_id.rs`・`row_id_tenant_scope.rs` と同じ）。
    send_simple_query(&mut stream, &insert_sql(42, "wire-op-dup-second"));
    let bytes = read_raw_message(&mut stream);
    assert_eq!(
        bytes.first(),
        Some(&b'E'),
        "expected ErrorResponse for a duplicate id within the same tenant, got: {bytes:?}"
    );
    let body_str = String::from_utf8_lossy(&bytes);
    assert!(
        body_str.contains("23505"),
        "expected SQLSTATE 23505, got: {body_str:?}"
    );
    assert!(
        !body_str.contains(TENANT_A)
            && !body_str.contains(TENANT_B)
            && !body_str.contains(TENANT_C),
        "error response must not leak a tenant identifier: {body_str:?}"
    );
    assert!(
        !body_str.contains("100") && !body_str.contains("200"),
        "error response must not leak another tenant's row id: {body_str:?}"
    );
    assert!(
        !body_str.contains("42"),
        "error response must not leak the duplicate row's own id: {body_str:?}"
    );
    read_ready_for_query(&mut stream);

    // 接続が維持されていることを確認する（後続の正規クエリが通ること）。
    send_simple_query(&mut stream, &insert_sql(2, "wire-op-after-dup"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}
