//! `engine::rls::ImplicitRlsHook` の結合テスト（TASK-137・対象ビヘイビア: RLS-6, RLS-7。
//! ポインタ: `docs/spec/05-tasks.md` TASK-137・`docs/spec/04-behavior/rls.md` RLS-6, RLS-7）。
//!
//! `tests/rls_security.rs`・`tests/sql_surface.rs` と同じ流儀（`unique_db_path` +
//! `CleanupGuard`、決定的擬似乱数 xorshift64*、production の判定関数
//! （[`engine::policy::PolicyContext::is_visible`]）を一切呼ばない独立オラクル、
//! `Storage::open` → `EngineCore::from_storage`）で実 `Storage` 上に複数テナントの
//! コーパスを構築し、`EngineCore::execute_sql`（SQL 経由）と `VectorCore::search`
//! （trait 経由）の両経路で RLS-6・RLS-7 を機械検証する。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

// ---------- 決定的擬似乱数（xorshift64*。`tests/rls_security.rs` と同一実装） ----------

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32_signed(&mut self) -> f32 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        (unit * 2.0 - 1.0) as f32
    }
}

// ---------- テスト共通のセットアップ ----------

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vector-db-engine-rls-implicit-{label}-{}-{seq}.redb",
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

const DIM: usize = 4;
const TABLE: &str = "docs";
const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const LANGS: [&str; 2] = ["ja", "en"];
const ROWS_PER_TENANT: u64 = 12;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// シード時の行の真実（オラクル用。production の可視性判定は一切通さない）。
#[derive(Clone, Copy)]
struct RowTruth {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    lang: &'static str,
}

/// 独立オラクル: `PolicyContext::is_visible` を呼ばずに、テスト側で記録した
/// `RowTruth` から許可可否を判定する（production の判定関数自体のバグを
/// 見逃さないため。`tests/rls_security.rs` と同じ方針）。
fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

/// `body` に「その行だけが持つ一意なキーワード」を埋め込む（C4 の疎コーパス側からの
/// 混入検出に使う。行 id を含むトークンなので他の行の body とは一致しない）。
fn unique_keyword(id: u64) -> String {
    format!("uniquetoken{id}")
}

/// 3 テナント × Public/Private × lang{ja,en} の決定的コーパスを構築する。
fn seed_multi_tenant_corpus(storage: &Storage) -> Vec<RowTruth> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(0x0C0F_FEE0_1137);
    let mut truths = Vec::new();
    let mut id = 1u64;
    for &tenant in TENANTS.iter() {
        for i in 0..ROWS_PER_TENANT {
            let visibility = if i % 2 == 0 {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let lang = LANGS[(i as usize) % LANGS.len()];
            let mut emb = vec![0.0f32; DIM];
            for slot in emb.iter_mut() {
                *slot = rng.next_f32_signed();
            }
            let body = format!("{lang} document about vectors {}", unique_keyword(id));
            storage
                .insert_typed_row(
                    TABLE,
                    id,
                    tenant,
                    visibility,
                    &[
                        Value::Vector(emb),
                        Value::Text(lang.to_string()),
                        Value::Text(body),
                    ],
                )
                .expect("insert row");
            truths.push(RowTruth {
                id,
                tenant,
                visibility,
                lang,
            });
            id += 1;
        }
    }
    truths
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn allowed_set(truths: &[RowTruth], viewer_tenant: &str, allow_private: bool) -> HashSet<u64> {
    truths
        .iter()
        .filter(|t| is_allowed(t, viewer_tenant, allow_private))
        .map(|t| t.id)
        .collect()
}

fn query_vec() -> String {
    let v = [1.0f32; DIM];
    format!(
        "[{}]",
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

// --- RLS-7（TASK-137） -----------------------------------------------------------

#[test]
fn rls7_c1_without_predicate_matches_with_predicate_and_leaks_nothing() {
    let path = unique_db_path("rls7-c1");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = if allow_private {
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant")
            } else {
                PolicyContext::new(tenant).expect("valid tenant")
            };
            let allowed = allowed_set(&truths, tenant, allow_private);

            for k in [1usize, 5, 20] {
                let sql_no_pred =
                    format!("SELECT * FROM docs ORDER BY embedding <=> '{q}' LIMIT {k}");
                let sql_pred = format!(
                    "SELECT * FROM docs WHERE visible() ORDER BY embedding <=> '{q}' LIMIT {k}"
                );
                let r1 = core
                    .execute_sql(&ctx, &sql_no_pred)
                    .expect("C1 without predicate should succeed");
                let r2 = core
                    .execute_sql(&ctx, &sql_pred)
                    .expect("C1 with predicate should succeed");
                assert_eq!(
                    result_ids(&r1),
                    result_ids(&r2),
                    "tenant={tenant} allow_private={allow_private} k={k}: predicate presence must not change the result"
                );
                for id in result_ids(&r1) {
                    assert!(
                        allowed.contains(&id),
                        "disallowed row leaked: tenant={tenant} allow_private={allow_private} k={k} id={id}"
                    );
                }
            }
        }
    }
}

#[test]
fn rls7_c2_scalar_filter_without_predicate_leaks_nothing() {
    let path = unique_db_path("rls7-c2");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let allowed = allowed_set(&truths, "tenant-a", true);
    let allowed_ja: HashSet<u64> = truths
        .iter()
        .filter(|t| t.lang == "ja" && is_allowed(t, "tenant-a", true))
        .map(|t| t.id)
        .collect();

    let sql_no_pred =
        format!("SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{q}' LIMIT 20");
    let sql_pred = format!(
        "SELECT * FROM docs WHERE lang = 'ja' AND visible() ORDER BY embedding <=> '{q}' LIMIT 20"
    );

    let r1 = core
        .execute_sql(&ctx, &sql_no_pred)
        .expect("C2 without predicate should succeed");
    let r2 = core
        .execute_sql(&ctx, &sql_pred)
        .expect("C2 with predicate should succeed");
    assert_eq!(result_ids(&r1), result_ids(&r2));

    let got: HashSet<u64> = result_ids(&r1).into_iter().collect();
    // under-fetch なし: 許可集合 ∩ lang=ja の件数と k=20 の小さい方まで返る。
    assert_eq!(got.len(), allowed_ja.len().min(20));
    for id in &got {
        assert!(allowed.contains(id), "disallowed row leaked: id={id}");
        assert!(allowed_ja.contains(id), "lang filter bypassed: id={id}");
    }
}

#[test]
fn rls7_c4_hybrid_without_predicate_leaks_nothing_including_sparse_side() {
    let path = unique_db_path("rls7-c4");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    // tenant-a・Public のみ許可。tenant-b の Private 行（不可視）のキーワードで検索する。
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let allowed = allowed_set(&truths, "tenant-a", false);

    let invisible_row = truths
        .iter()
        .find(|t| t.tenant == "tenant-b" && matches!(t.visibility, Visibility::Private))
        .expect("fixture has a tenant-b private row");
    let invisible_keyword = unique_keyword(invisible_row.id);

    let sql_invisible = format!(
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{q}', body, '{invisible_keyword}') LIMIT 20"
    );
    let result = core
        .execute_sql(&ctx, &sql_invisible)
        .expect("C4 hybrid execution should succeed");
    assert!(
        !result_ids(&result).contains(&invisible_row.id),
        "sparse side leaked an invisible row via its unique keyword"
    );
    for id in result_ids(&result) {
        assert!(allowed.contains(&id), "disallowed row leaked: id={id}");
    }

    // 陽性対照: 可視行のキーワードなら検索できる（フィルタが効きすぎて全滅していないことの確認）。
    let visible_row = truths
        .iter()
        .find(|t| is_allowed(t, "tenant-a", false))
        .expect("fixture has an allowed row");
    let visible_keyword = unique_keyword(visible_row.id);
    let sql_visible = format!(
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{q}', body, '{visible_keyword}') LIMIT 20"
    );
    let result_visible = core
        .execute_sql(&ctx, &sql_visible)
        .expect("C4 hybrid execution should succeed");
    assert!(
        result_ids(&result_visible).contains(&visible_row.id),
        "visible row with its own unique keyword should be retrievable"
    );
}

#[test]
fn rls7_predicate_alteration_cannot_widen_visibility() {
    let path = unique_db_path("rls7-alteration");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let altered_forms = [
        format!("SELECT * FROM docs WHERE visible('tenant-b') ORDER BY embedding <=> '{q}' LIMIT 10"),
        format!("SELECT * FROM docs WHERE NOT visible() ORDER BY embedding <=> '{q}' LIMIT 10"),
        format!(
            "SELECT * FROM docs WHERE visible() OR lang = 'ja' ORDER BY embedding <=> '{q}' LIMIT 10"
        ),
    ];
    for sql in altered_forms {
        let err = core
            .execute_sql(&ctx, &sql)
            .expect_err("altered/self-declared predicate form must be rejected");
        // 実装時の実際の分類（`42601`/`22000` 等）に関わらず、`Err` かつ行を返さない
        // ことのみを必須条件とする（改変によって可視性を広げられないことの検証）。
        assert!(!err.wire_code().is_empty(), "sql={sql:?}");
    }
}

// --- RLS-6（TASK-137） -----------------------------------------------------------

#[test]
fn rls6_tenant_is_derived_only_from_policy_context() {
    let path = unique_db_path("rls6-derivation");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    // SQL テキスト中に他テナントの文字列リテラルを含めても、実際のテナント判定は
    // `ctx` からのみ行われる（`lang` 列に 'tenant-b' を書いても該当行が無いだけで、
    // テナント境界そのものには影響しない）。
    let sql =
        format!("SELECT * FROM docs WHERE lang = 'tenant-b' ORDER BY embedding <=> '{q}' LIMIT 10");

    for &tenant in TENANTS.iter() {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let allowed = allowed_set(&truths, tenant, false);
        let result = core
            .execute_sql(&ctx, &sql)
            .expect("scalar filter with no matching rows should still succeed");
        assert!(result_ids(&result).is_empty());
        for id in result_ids(&result) {
            assert!(allowed.contains(&id));
        }
    }

    // 同一 SQL・異なる ctx で結果がそれぞれのオラクル許可集合の部分集合であることを、
    // マッチする条件でも確認する。
    let sql_topk = format!("SELECT * FROM docs ORDER BY embedding <=> '{q}' LIMIT 10");
    for &tenant in TENANTS.iter() {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let allowed = allowed_set(&truths, tenant, false);
        let result = core
            .execute_sql(&ctx, &sql_topk)
            .expect("C1 should succeed");
        for id in result_ids(&result) {
            assert!(
                allowed.contains(&id),
                "tenant={tenant} id={id} not in oracle allowed set"
            );
        }
    }
}

#[test]
fn rls6_sql_cannot_reference_tenant_column_or_session_settings() {
    let path = unique_db_path("rls6-no-tenant-column");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let sql_unknown_column = format!(
        "SELECT * FROM docs WHERE tenant_id = 'tenant-b' ORDER BY embedding <=> '{q}' LIMIT 10"
    );
    let err = core
        .execute_sql(&ctx, &sql_unknown_column)
        .expect_err("tenant_id is not a queryable column");
    assert_eq!(err.wire_code(), "22000");

    for sql in [
        "SET search_path = tenant_b",
        "SELECT set_config('vector_db.tenant', 'tenant-b', false)",
        format!("SELECT * FROM docs ORDER BY embedding <=> '{q}' LIMIT 10; SELECT 1").as_str(),
    ] {
        let err = core
            .execute_sql(&ctx, sql)
            .expect_err("session-setting / multi-statement forms must be rejected");
        assert_eq!(err.wire_code(), "42601", "sql={sql:?}");
    }
}

// --- RLS-6/7（TASK-137）: `VectorCore::search`（trait 経由）でも同じ契約 ----------

#[test]
fn rls_hook_is_the_only_path_for_vector_core_search() {
    let path = unique_db_path("rls-hook-trait-path");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core: Box<dyn VectorCore> = Box::new(new_core(storage));
    let query = vec![1.0f32; DIM];

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = if allow_private {
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant")
            } else {
                PolicyContext::new(tenant).expect("valid tenant")
            };
            let allowed = allowed_set(&truths, tenant, allow_private);
            let hits = core
                .search(&ctx, TABLE, &query, 20)
                .expect("VectorCore::search should succeed");
            for hit in hits {
                assert!(
                    allowed.contains(&hit.id),
                    "disallowed row leaked via VectorCore::search: tenant={tenant} allow_private={allow_private} id={}",
                    hit.id
                );
            }
        }
    }
}
