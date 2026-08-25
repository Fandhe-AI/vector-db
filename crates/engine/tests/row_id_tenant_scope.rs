//! 行 `id` の一意性スコープ（テナント内）と書き込み経路の存在情報秘匿の機械検証
//! （対象ビヘイビア: TABLE-12・RLS-9。越境書き込み遮断そのものは RECOVER-4 で
//! `tests/tenant_breach.rs` が担当する。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-12・`rls.md` RLS-9・`recovery.md` RECOVER-4）。
//!
//! 検証対象は `engine::tenant` の書き込みガード（`insert_row`/`update_row`/`delete_row`）と、
//! その下で `(tenant_id, id)` に名前空間化された行ストア（`engine::catalog` の
//! テーブルスコープ行 API）。他テナントが同じ行 `id` を保持しているか否かで、
//! 応答（成否・`wire_code`・エラー文言）も後続の読み取り結果も変化しないことを、
//! 本体の判定 API に依存しないテスト側オラクル（期待値のベタ書き）で確認する。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::{self, TenantWriteError};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-row-id-scope-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const TABLE: &str = "docs";
const DIM: u32 = 2;
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn row<'a>(tenant: &'a str, embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: tenant,
        visibility: Visibility::Public,
        embedding,
        metadata,
    }
}

fn open_seeded(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(label);
    let cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (storage, cleanup)
}

// (a) 対象ビヘイビア: TABLE-12 (b)。他テナントが保持する id と同じ id への
// 自テナント名義 INSERT は成功し、他テナントの行は一切変化しない
// （物理キーが `(tenant_id, id)` で名前空間化されているため上書きが起こらない）。
#[test]
fn table12_insert_with_an_id_held_by_another_tenant_succeeds_without_touching_that_row() {
    let (storage, _cleanup) = open_seeded("cross-tenant-id");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);

    tenant::insert_row(&storage, TABLE, &a, 1, &row(TENANT_A, &[1.0, 0.0], b"a1"))
        .expect("tenant-a insert id=1");
    tenant::insert_row(&storage, TABLE, &b, 1, &row(TENANT_B, &[0.0, 1.0], b"b1"))
        .expect("tenant-b must be able to use the same id in its own namespace");

    // 双方の行が独立に残っていること（テスト側で期待値をベタ書きし、本体の
    // 可視性判定 API には依存しない）。
    let a_row = storage
        .get_row_from_table(TABLE, TENANT_A, 1)
        .expect("tenant-a row must still exist");
    let b_row = storage
        .get_row_from_table(TABLE, TENANT_B, 1)
        .expect("tenant-b row must exist");
    assert_eq!(a_row.tenant_id, TENANT_A);
    assert_eq!(a_row.embedding, vec![1.0, 0.0]);
    assert_eq!(a_row.metadata, b"a1".to_vec());
    assert_eq!(b_row.tenant_id, TENANT_B);
    assert_eq!(b_row.embedding, vec![0.0, 1.0]);
    assert_eq!(b_row.metadata, b"b1".to_vec());
}

// (b) 対象ビヘイビア: TABLE-12 (a)。同一テナント内の重複 id は `23505` で拒否され、
// 既存行は上書きされない。
#[test]
fn table12_duplicate_id_within_the_same_tenant_is_rejected_with_23505() {
    let (storage, _cleanup) = open_seeded("same-tenant-dup");
    let a = ctx(TENANT_A);

    tenant::insert_row(
        &storage,
        TABLE,
        &a,
        7,
        &row(TENANT_A, &[1.0, 0.0], b"first"),
    )
    .expect("first insert ok");
    let err = tenant::insert_row(
        &storage,
        TABLE,
        &a,
        7,
        &row(TENANT_A, &[0.0, 1.0], b"second"),
    )
    .expect_err("duplicate id within the same tenant must be rejected");
    assert!(matches!(err, TenantWriteError::IdConflict));
    assert_eq!(err.wire_code(), "23505");

    let stored = storage
        .get_row_from_table(TABLE, TENANT_A, 7)
        .expect("row must still exist");
    assert_eq!(
        stored.metadata,
        b"first".to_vec(),
        "a rejected duplicate must not overwrite the existing row"
    );
}

// (c) 対象ビヘイビア: RLS-9。他テナントに同一 id の行が「ある場合」と「ない場合」とで、
// 自テナント名義 INSERT の応答（成否・`wire_code`・エラー文言）が完全に一致すること。
// レイテンシ差は本テストでは判定しない（閾値・試行回数は spec 側の宿題。実装上は
// 物理キーの名前空間化により他テナント行を参照する分岐が存在しないため、経路自体が同一）。
#[test]
fn rls9_insert_response_is_identical_whether_or_not_another_tenant_holds_the_id() {
    // ケース 1: 他テナントが id=42 を保持している状態で tenant-a が id=42 を INSERT。
    let (storage_with, _cleanup_with) = open_seeded("resp-with-foreign");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    tenant::insert_row(
        &storage_with,
        TABLE,
        &b,
        42,
        &row(TENANT_B, &[0.0, 1.0], b"foreign"),
    )
    .expect("seed tenant-b row id=42");
    let with_foreign = tenant::insert_row(
        &storage_with,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"mine"),
    );

    // ケース 2: どのテナントも id=42 を保持していない状態で同じ INSERT。
    let (storage_without, _cleanup_without) = open_seeded("resp-without-foreign");
    let without_foreign = tenant::insert_row(
        &storage_without,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"mine"),
    );

    assert!(
        with_foreign.is_ok() && without_foreign.is_ok(),
        "both cases must take the same successful path (RLS-9)"
    );
    assert_eq!(
        format!("{with_foreign:?}"),
        format!("{without_foreign:?}"),
        "responses must be indistinguishable between the two cases"
    );

    // 逆方向（同一テナント内重複）でも、他テナント行の有無で応答が変わらないこと。
    let dup_with_foreign = tenant::insert_row(
        &storage_with,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"dup"),
    )
    .expect_err("duplicate within tenant-a must be rejected");
    let dup_without_foreign = tenant::insert_row(
        &storage_without,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"dup"),
    )
    .expect_err("duplicate within tenant-a must be rejected");
    assert_eq!(
        dup_with_foreign.wire_code(),
        dup_without_foreign.wire_code()
    );
    assert_eq!(
        format!("{dup_with_foreign}"),
        format!("{dup_without_foreign}")
    );
    assert_eq!(
        format!("{dup_with_foreign:?}"),
        format!("{dup_without_foreign:?}")
    );
    // 応答文言に他テナント名・id が現れないこと（存在情報秘匿の回帰検証）。
    let text = format!("{dup_with_foreign} {dup_with_foreign:?}");
    assert!(!text.contains(TENANT_B), "response leaked a tenant: {text}");
    assert!(!text.contains("42"), "response leaked the row id: {text}");
}

// (d) 対象ビヘイビア: RECOVER-4。越境 UPDATE/DELETE は、対象 id が他テナントに
// 存在する場合も存在しない場合も一律 `NotFound` に統一される。
#[test]
fn recover4_cross_tenant_update_and_delete_are_uniformly_not_found() {
    let (storage, _cleanup) = open_seeded("cross-update-delete");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    tenant::insert_row(&storage, TABLE, &b, 5, &row(TENANT_B, &[0.0, 1.0], b"b5"))
        .expect("seed tenant-b row id=5");

    let update_existing =
        tenant::update_row(&storage, TABLE, &a, 5, &row(TENANT_A, &[1.0, 0.0], b"x"))
            .expect_err("cross-tenant update must fail");
    let update_missing =
        tenant::update_row(&storage, TABLE, &a, 6, &row(TENANT_A, &[1.0, 0.0], b"x"))
            .expect_err("update of a nonexistent id must fail");
    let delete_existing =
        tenant::delete_row(&storage, TABLE, &a, 5).expect_err("cross-tenant delete must fail");
    let delete_missing = tenant::delete_row(&storage, TABLE, &a, 6)
        .expect_err("delete of a nonexistent id must fail");

    for e in [
        &update_existing,
        &update_missing,
        &delete_existing,
        &delete_missing,
    ] {
        assert!(matches!(e, TenantWriteError::NotFound));
        assert_eq!(e.wire_code(), "P0002");
    }
    assert_eq!(
        format!("{update_existing} {update_existing:?}"),
        format!("{update_missing} {update_missing:?}"),
        "the presence of another tenant's row must not change the response"
    );

    // 他テナントの行が試行後も不変であること。
    let victim = storage
        .get_row_from_table(TABLE, TENANT_B, 5)
        .expect("tenant-b row must still exist");
    assert_eq!(victim.metadata, b"b5".to_vec());
}

// (e) 対象ビヘイビア: TABLE-12 の読み取り経路への波及。異なるテナントの `Public` 行が
// 同一 id を持つ構成でも検索が fail-closed に落ちず（`core::provider_result_is_valid` の
// 多重集合判定）、両方の可視行が結果に現れること。集合ベースの重複拒否へ戻すと、
// 他テナントが同じ id の `Public` 行を作るだけで検索が失敗する（テナント間の可用性干渉に
// なる）ため、その退行を防ぐ回帰テストでもある。
#[test]
fn table12_search_tolerates_the_same_id_held_by_two_tenants_as_public_rows() {
    let (storage, _cleanup) = open_seeded("search-dup-id");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    tenant::insert_row(&storage, TABLE, &a, 1, &row(TENANT_A, &[1.0, 0.0], b"a1"))
        .expect("tenant-a public row id=1");
    tenant::insert_row(&storage, TABLE, &b, 1, &row(TENANT_B, &[1.0, 0.0], b"b1"))
        .expect("tenant-b public row id=1");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let hits = core
        .search(&a, TABLE, &[1.0, 0.0], 10)
        .expect("search must not fail merely because two tenants share a row id");
    assert_eq!(
        hits.len(),
        2,
        "both visible rows (own row and the other tenant's Public row) must be returned"
    );
    assert!(hits.iter().all(|h| h.id == 1));
}
