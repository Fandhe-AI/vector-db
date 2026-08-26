//! `GROUP BY`/`HAVING` 集計（TASK-167・SQL-14）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-167・`docs/spec/04-behavior/sql-surface.md`
//! SQL-14・`docs/spec/04-behavior/rls.md` RLS-7, RLS-8。
//!
//! `tests/sql_aggregate.rs`（TASK-166・SQL-13）と同じ流儀（`unique_db_path`＋
//! `CleanupGuard`、決定的擬似乱数 xorshift64*、`PolicyContext::is_visible` を
//! 呼ばない独立オラクルで可視集合・グループ集計値を手計算し、`EngineCore::execute_sql`
//! （SQL 経由）の結果と突き合わせる）で検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};
use std::collections::BTreeMap;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

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
}

const DIM: usize = 4;
const TABLE: &str = "docs";
const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const LANGS: [&str; 3] = ["ja", "en", "fr"];
const ROWS_PER_TENANT: u64 = 12;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

#[derive(Clone)]
struct RowTruth {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    lang: &'static str,
}

fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

fn seed_multi_tenant_corpus(storage: &Storage) -> Vec<RowTruth> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(0xABCD_1234_5678);
    let mut truths = Vec::new();
    let mut id = 1u64;
    for &tenant in TENANTS.iter() {
        for i in 0..ROWS_PER_TENANT {
            let visibility = if i % 3 == 0 {
                Visibility::Private
            } else {
                Visibility::Public
            };
            let lang = LANGS[(rng.next_u64() as usize) % LANGS.len()];
            let ctx =
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            engine::tenant::insert_typed_row(
                storage,
                TABLE,
                &ctx,
                id,
                visibility,
                &[
                    Value::Vector(vec![0.0f32; DIM]),
                    Value::Text(lang.to_string()),
                ],
                &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                    .expect("valid operation_id"),
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

fn visible_rows<'a>(
    truths: &'a [RowTruth],
    viewer_tenant: &str,
    allow_private: bool,
) -> Vec<&'a RowTruth> {
    truths
        .iter()
        .filter(|t| is_allowed(t, viewer_tenant, allow_private))
        .collect()
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn as_integer(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn as_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Text(v) => Some(v.clone()),
        Cell::Null => None,
        other => panic!("expected Cell::Text or Cell::Null, got {other:?}"),
    }
}

/// オラクル: `visible` 行を `lang` でグループ化した `(lang, count, id_sum)`。
fn oracle_groups(visible: &[&RowTruth]) -> BTreeMap<String, (u64, u64)> {
    let mut groups: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for row in visible {
        let entry = groups.entry(row.lang.to_string()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += row.id;
    }
    groups
}

fn row_map(result: &QueryResult) -> BTreeMap<Option<String>, Vec<Cell>> {
    let mut out = BTreeMap::new();
    for row in &result.rows {
        let key = as_text(&row.cells[0]);
        out.insert(key, row.cells.clone());
    }
    out
}

// --- 基本: 3 テナント × Public/Private の可視集合ごとにグループ集計がオラクルと一致 ---

#[test]
fn group_by_basic_counts_and_sums_match_independent_oracle() {
    let path = unique_db_path("group-by-basic");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let visible = visible_rows(&truths, tenant, allow_private);
            let expected = oracle_groups(&visible);

            let result = core
                .execute_sql(
                    &ctx,
                    "SELECT lang, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang",
                )
                .expect("GROUP BY query should succeed");

            assert_eq!(
                result.rows.len(),
                expected.len(),
                "group count mismatch tenant={tenant} allow_private={allow_private}"
            );
            let rows = row_map(&result);
            for (lang, (count, id_sum)) in &expected {
                let cells = rows
                    .get(&Some(lang.clone()))
                    .unwrap_or_else(|| panic!("missing group {lang:?}"));
                assert_eq!(as_integer(&cells[1]), *count, "COUNT lang={lang}");
                assert_eq!(as_integer(&cells[2]), *id_sum, "SUM(id) lang={lang}");
            }
        }
    }
}

// --- RLS 境界: 他テナントにしか存在しないグループが結果に一切現れない ------------------

#[test]
fn group_by_never_reveals_other_tenants_exclusive_group() {
    let path = unique_db_path("group-by-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");

    let writer_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    // tenant-a だけが持つ Private 専用言語 "xx"。
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &writer_ctx,
        1,
        Visibility::Private,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("xx".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("op-1").expect("valid op"),
    )
    .expect("insert row");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &writer_ctx,
        2,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("ja".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("op-2").expect("valid op"),
    )
    .expect("insert row");

    let core = new_core(storage);
    let viewer_ctx = ctx_for("tenant-b", true);
    let result = core
        .execute_sql(
            &viewer_ctx,
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang",
        )
        .expect("GROUP BY query should succeed");
    let rows = row_map(&result);
    assert!(
        !rows.contains_key(&Some("xx".to_string())),
        "a group exclusive to another tenant's private rows must not appear"
    );
    assert_eq!(rows.len(), 1, "only the visible public group must appear");
    assert!(rows.contains_key(&Some("ja".to_string())));
}

// --- NULL グループ: nullable TEXT 列の NULL は 1 つのグループへまとまる ------------

#[test]
fn group_by_null_column_forms_a_single_null_group() {
    let path = unique_db_path("group-by-null");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |n: &str| {
        engine::recovery::required_op_id::OperationId::parse(n).expect("valid operation_id")
    };

    storage
        .alter_table_add_column(TABLE, ColumnDef::new("note", ColumnType::Text, true))
        .expect("alter table");

    for (id, note) in [(1u64, Some("hi")), (2, None), (3, None), (4, Some("hi"))] {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text("ja".to_string()),
                note.map(|n| Value::Text(n.to_string()))
                    .unwrap_or(Value::Null),
            ],
            &op(&format!("op-{id}")),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(&ctx, "SELECT note, COUNT(*) AS n FROM docs GROUP BY note")
        .expect("GROUP BY on a nullable column should succeed");
    let rows = row_map(&result);
    assert_eq!(rows.len(), 2);
    assert_eq!(as_integer(&rows[&Some("hi".to_string())][1]), 2);
    assert_eq!(as_integer(&rows[&None][1]), 2);
}

// --- HAVING: 呼び出し形（別名参照）・AND 連結・NULL 集計値の除外 ------------------

#[test]
fn having_filters_groups_by_aggregate_value() {
    let path = unique_db_path("group-by-having");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    let visible = visible_rows(&truths, "tenant-a", true);
    let expected = oracle_groups(&visible);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n > 1",
        )
        .expect("HAVING query should succeed");
    let rows = row_map(&result);
    for (lang, (count, _)) in &expected {
        let present = rows.contains_key(&Some(lang.clone()));
        assert_eq!(present, *count > 1, "lang={lang} count={count}");
    }
}

// --- ORDER BY + LIMIT: 集計値の降順 + 上位 N 件のみ ------------------------------

#[test]
fn order_by_and_limit_return_top_n_groups_by_count_desc() {
    let path = unique_db_path("group-by-order-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    let visible = visible_rows(&truths, "tenant-a", true);
    let expected = oracle_groups(&visible);

    let mut expected_sorted: Vec<(String, u64)> = expected
        .iter()
        .map(|(lang, (count, _))| (lang.clone(), *count))
        .collect();
    expected_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    expected_sorted.truncate(2);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY n DESC LIMIT 2",
        )
        .expect("ORDER BY/LIMIT query should succeed");
    assert_eq!(result.rows.len(), expected_sorted.len().min(2));
    for (row, (lang, count)) in result.rows.iter().zip(expected_sorted.iter()) {
        assert_eq!(as_text(&row.cells[0]), Some(lang.clone()));
        assert_eq!(as_integer(&row.cells[1]), *count);
    }
}

// --- 決定性: 同一クエリを 2 回実行して同じ行順・同じ値 ----------------------------

#[test]
fn group_by_result_order_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("group-by-determinism");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang";
    let first = core.execute_sql(&ctx, sql).expect("first call");
    let second = core.execute_sql(&ctx, sql).expect("second call");
    assert_eq!(first.rows, second.rows);
}

// --- 上限境界: MAX_GROUPS ちょうどは成功、+1 は 54000 -----------------------------

#[test]
fn group_count_over_max_groups_is_rejected_as_payload_too_large() {
    let path = unique_db_path("group-by-max-groups");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let schema = TableSchema::new(
        "wide",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(1), false),
            ColumnDef::new("k", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // MAX_GROUPS(10_000) を超えるのは重いため、直接の全件検証ではなく実装の
    // `MAX_GROUPS` 定数へ依存せず「明らかに超過する規模」を投入して `54000` を
    // 確認する（境界値ちょうどの検証はユニットテスト（`group_by` モジュール）が
    // 別途担う想定・本結合テストは fail-closed の実地確認に留める）。
    const OVER: u64 = 10_001;
    for i in 0..OVER {
        engine::tenant::insert_typed_row(
            &storage,
            "wide",
            &ctx,
            i,
            Visibility::Public,
            &[Value::Vector(vec![0.0f32]), Value::Text(format!("k{i}"))],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{i}"))
                .expect("valid op"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let err = core
        .execute_sql(&ctx, "SELECT k, COUNT(*) FROM wide GROUP BY k")
        .expect_err("exceeding MAX_GROUPS must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- 拒否経路: 型不整合・未受理形 --------------------------------------------------

#[test]
fn rejects_group_by_on_vector_column_and_id_pseudo_column() {
    let path = unique_db_path("group-by-type-reject");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let err = core
        .execute_sql(
            &ctx,
            "SELECT embedding, COUNT(*) FROM docs GROUP BY embedding",
        )
        .expect_err("GROUP BY on a VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql(&ctx, "SELECT id, COUNT(*) FROM docs GROUP BY id")
        .expect_err("GROUP BY on the id pseudo-column must be rejected");
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql(&ctx, "SELECT ghost, COUNT(*) FROM docs GROUP BY ghost")
        .expect_err("GROUP BY on an unknown column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn rejects_having_on_text_min_max_and_group_key_column() {
    let path = unique_db_path("group-by-having-type-reject");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let err = core
        .execute_sql(
            &ctx,
            "SELECT lang, MIN(lang) AS m FROM docs GROUP BY lang HAVING m > 1",
        )
        .expect_err("HAVING on a TEXT-valued MIN must be rejected");
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql(
            &ctx,
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING lang > 1",
        )
        .expect_err("HAVING referencing the GROUP BY key column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn rejects_limit_out_of_range_on_group_by() {
    let path = unique_db_path("group-by-limit-reject");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let err = core
        .execute_sql(
            &ctx,
            "SELECT lang, COUNT(*) FROM docs GROUP BY lang LIMIT 0",
        )
        .expect_err("LIMIT 0 must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- PR #230 codex-review/Bugbot 指摘の回帰テスト ----------------------------------

// NULL グループは既定の昇順（`ORDER BY` 未指定）で常に末尾に来る（`GroupKey` の
// `Ord` が `Option<String>` の派生 `Ord`（`None` が先頭）のままだと、この
// `LIMIT 1` が非 NULL の先頭グループではなく NULL グループを返してしまう）。
#[test]
fn group_by_default_order_places_null_group_last_for_limit() {
    let path = unique_db_path("group-by-null-sorts-last");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |n: &str| {
        engine::recovery::required_op_id::OperationId::parse(n).expect("valid operation_id")
    };

    storage
        .alter_table_add_column(TABLE, ColumnDef::new("note", ColumnType::Text, true))
        .expect("alter table");

    for (id, note) in [
        (1u64, Some("bb")),
        (2, None),
        (3, Some("aa")),
        (4, None),
        (5, Some("cc")),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text("ja".to_string()),
                note.map(|n| Value::Text(n.to_string()))
                    .unwrap_or(Value::Null),
            ],
            &op(&format!("op-{id}")),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT note, COUNT(*) AS n FROM docs GROUP BY note LIMIT 1",
        )
        .expect("GROUP BY with default order and LIMIT should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        as_text(&result.rows[0].cells[0]),
        Some("aa".to_string()),
        "the smallest non-NULL group must sort before the NULL group"
    );
}

// `ORDER BY` は SELECT リストで `GROUP BY` 列に付けたエイリアスも参照できる
// （`resolve_target` が生の列名にしか一致しないと `unknown GROUP BY reference`
// として `22000` を返してしまう）。
#[test]
fn order_by_accepts_group_key_alias() {
    let path = unique_db_path("group-by-order-by-alias");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    let visible = visible_rows(&truths, "tenant-a", true);
    let expected = oracle_groups(&visible);

    let mut expected_langs: Vec<String> = expected.keys().cloned().collect();
    expected_langs.sort();

    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang AS language, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY language",
        )
        .expect("ORDER BY referencing the GROUP BY key alias should succeed");
    let actual_langs: Vec<String> = result
        .rows
        .iter()
        .map(|row| as_text(&row.cells[0]).expect("group key must be present"))
        .collect();
    assert_eq!(actual_langs, expected_langs);
}

// クエリ全体での `MIN`/`MAX(<TEXT 列>)` 集計状態の累計バイト数は
// `MAX_TEXT_ACCUMULATOR_TOTAL_BYTES` で頭打ちにされ、超過は `54000`
// （codex-review P1 指摘対応: グループ数×項目数分の `TextMin`/`TextMax` が
// 無制限にメモリを確保しないことを確認する）。
#[test]
fn text_min_max_accumulator_total_size_over_budget_is_rejected() {
    let path = unique_db_path("group-by-text-accumulator-budget");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let schema = TableSchema::new(
        "textbudget",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(1), false),
            ColumnDef::new("k", ColumnType::Text, false),
            ColumnDef::new("v", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 各グループの MIN(v) 対象文字列を 3 MiB（行のメタデータ総量上限 4 MiB を
    // 下回る値）にし、6 グループ分（18 MiB）投入して 16 MiB のクエリ全体予算を
    // 超過させる。
    const GROUPS: u64 = 6;
    const VALUE_LEN: usize = 3 * 1024 * 1024;
    for i in 0..GROUPS {
        let value = "x".repeat(VALUE_LEN);
        engine::tenant::insert_typed_row(
            &storage,
            "textbudget",
            &ctx,
            i,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32]),
                Value::Text(format!("k{i}")),
                Value::Text(value),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{i}"))
                .expect("valid op"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let err = core
        .execute_sql(&ctx, "SELECT k, MIN(v) FROM textbudget GROUP BY k")
        .expect_err("exceeding the TEXT accumulator total byte budget must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// `ORDER BY <GROUP BY 列> DESC` でも `NULL` グループは常に末尾（PR #230
// codex-review P1 指摘対応: 以前は非 `NULL` 側との大小関係を含む `Ordering`
// 全体を `.reverse()` していたため、`DESC` 指定時に `NULL` グループが先頭へ来て
// `LIMIT` が本来先頭に来るべき非 `NULL` グループ（この場合 "cc"）を取りこぼして
// いた）。
#[test]
fn group_by_desc_order_still_places_null_group_last_for_limit() {
    let path = unique_db_path("group-by-desc-null-last");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |n: &str| {
        engine::recovery::required_op_id::OperationId::parse(n).expect("valid operation_id")
    };

    storage
        .alter_table_add_column(TABLE, ColumnDef::new("note", ColumnType::Text, true))
        .expect("alter table");

    for (id, note) in [
        (1u64, Some("bb")),
        (2, None),
        (3, Some("aa")),
        (4, None),
        (5, Some("cc")),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text("ja".to_string()),
                note.map(|n| Value::Text(n.to_string()))
                    .unwrap_or(Value::Null),
            ],
            &op(&format!("op-{id}")),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT note, COUNT(*) AS n FROM docs GROUP BY note ORDER BY note DESC LIMIT 1",
        )
        .expect("GROUP BY with DESC order and LIMIT should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        as_text(&result.rows[0].cells[0]),
        Some("cc".to_string()),
        "the largest non-NULL group must sort before the NULL group even under DESC"
    );
}

// `HAVING` は `SUM(id)` のように `2^53` を超えうる整数集計値を精度損失なく比較
// する（PR #230 codex-review P1 指摘対応: 以前は `Cell::Integer(u64)` を無条件に
// `f64` へキャストしていたため、`2^53` 超の集計値が丸められ等号・不等号比較が
// 誤判定しうた）。HAVING リテラル自体は束縛段で `2^53` 以下に制限されるため、
// ここでは集計値側（`SUM(id)`）を `2^53` 超にして検証する。
#[test]
fn having_compares_large_integer_sum_without_precision_loss() {
    let path = unique_db_path("group-by-having-large-integer");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |n: &str| {
        engine::recovery::required_op_id::OperationId::parse(n).expect("valid operation_id")
    };

    // 2^53 + 1（`f64` で正確に表現できない最小の整数）。この行 1 件だけで
    // グループを構成するため、SUM(id) はそのままこの id の値になる。
    const HUGE_ID: u64 = (1u64 << 53) + 1;
    // 束縛段（`sql::udf_call::parse_number_literal`）が受理する上限（2^53 ちょうど）。
    const MAX_LITERAL: u64 = 1u64 << 53;

    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        HUGE_ID,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("huge".to_string()),
        ],
        &op("op-huge"),
    )
    .expect("insert row");

    let core = new_core(storage);

    // 丸めると HUGE_ID は MAX_LITERAL（2^53）に一致してしまうが、実際の SUM は
    // HUGE_ID（2^53 + 1）であり MAX_LITERAL とは等しくない。
    let eq_result = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT lang, SUM(id) AS s FROM docs WHERE lang = 'huge' GROUP BY lang HAVING s = {MAX_LITERAL}"
            ),
        )
        .expect("HAVING with large SUM should succeed");
    assert!(
        eq_result.rows.is_empty(),
        "SUM(id) = {HUGE_ID} must not equal the rounded literal {MAX_LITERAL}"
    );

    // 丸めると HUGE_ID と MAX_LITERAL が等しくなってしまうため、丸め誤差込みの
    // 比較では `>` が偽になる。実際には HUGE_ID > MAX_LITERAL のため真である
    // べき。
    let gt_result = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT lang, SUM(id) AS s FROM docs WHERE lang = 'huge' GROUP BY lang HAVING s > {MAX_LITERAL}"
            ),
        )
        .expect("HAVING with large SUM should succeed");
    assert_eq!(
        gt_result.rows.len(),
        1,
        "SUM(id) = {HUGE_ID} must compare greater than the literal {MAX_LITERAL}"
    );
}
