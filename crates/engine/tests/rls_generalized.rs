//! RLS 暗黙適用の一般化検証（TASK-138・対象ビヘイビア: RLS-8。ポインタ:
//! `docs/spec/05-tasks.md` TASK-138・`docs/spec/04-behavior/rls.md` RLS-8）。
//!
//! `tests/rls_implicit.rs`（TASK-137・C1〜C4 相当の限定経路）・`tests/rls_security.rs`
//! と同じ流儀（`unique_db_path` / `CleanupGuard`、決定的擬似乱数 xorshift64*、
//! production の可視性判定（[`engine::policy::PolicyContext::is_visible`]）を一切
//! 呼ばない独立オラクル）で、RLS-6/RLS-7 が MVP クエリカタログ以外の全読み取り経路
//! （複数の任意スキーマテーブル・任意形状 SELECT・UDF・`VectorCore::search`・
//! `get_row`・`tenant::visible_rows`）へも同じ契約で一般化されて働くことを検証する。
//!
//! `USING PLAN` 展開後クエリの検証は対象外（TASK-77 未実装。fail-closed に拒否される
//! ことのみ [`using_plan_is_rejected_fail_closed_until_task_77`] で固定し、展開後の
//! 検証は TASK-77/TASK-117 へ委ねる）。

use std::collections::HashSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（xorshift64*。`tests/rls_implicit.rs` と同一実装） ----------

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

// ---------- フィクスチャ: 3 テーブル（任意スキーマ・任意次元） × 4 テナント ----------

const TENANTS: [&str; 4] = ["tenant-a", "tenant-b", "tenant-c", "tenant-d"];
const ROWS_PER_TENANT: u64 = 6;

/// `docs`: `embedding VECTOR(4)`／`notes`: `vec VECTOR(3)`（VECTOR 列名も変える）／
/// `kb`: `embedding VECTOR(8)`（複数 TEXT 列）。行 id はテーブル間で同一番号を再利用し、
/// テーブル取り違えによる混入も検出対象にする（RLS-8: 「任意テーブル」軸）。
struct TableInfo {
    name: &'static str,
    dim: usize,
    vector_col: &'static str,
    text_cols: &'static [&'static str],
}

const TABLES: [TableInfo; 3] = [
    TableInfo {
        name: "docs",
        dim: 4,
        vector_col: "embedding",
        text_cols: &["lang", "body"],
    },
    TableInfo {
        name: "notes",
        dim: 3,
        vector_col: "vec",
        text_cols: &["body"],
    },
    TableInfo {
        name: "kb",
        dim: 8,
        vector_col: "embedding",
        text_cols: &["lang", "title", "body", "tag"],
    },
];

fn schema_for(t: &TableInfo) -> TableSchema {
    let mut cols = vec![ColumnDef::new(
        t.vector_col,
        ColumnType::Vector(t.dim as u32),
        false,
    )];
    for &c in t.text_cols {
        cols.push(ColumnDef::new(c, ColumnType::Text, false));
    }
    TableSchema::new(t.name, cols)
}

/// シード時の行の真実（オラクル用。production の可視性判定は一切通さない）。
#[derive(Clone)]
struct RowTruth {
    table: &'static str,
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    /// 不可視カナリア: ゼロベクトル（UDF `vec_div(v, vec_norm(v))` が評価されれば
    /// 0 除算で失敗する）。
    is_zero_canary: bool,
}

/// 独立オラクル: `PolicyContext::is_visible` を呼ばずに、テスト側で記録した
/// `RowTruth` から許可可否を判定する（production の判定関数自体のバグを見逃さない
/// ため。`tests/rls_implicit.rs`・`tests/rls_security.rs` と同じ方針）。
fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

fn unique_keyword(table: &str, id: u64) -> String {
    format!("uniquetoken_{table}_{id}")
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// 全テーブルへ 4 テナント × Public/Private の決定的コーパスを構築する。各テナントの
/// Private 行のうち先頭 1 件をゼロベクトルのカナリアにする。
fn seed_corpus(storage: &Storage) -> Vec<RowTruth> {
    let mut rng = Xorshift64::new(0x0C0F_FEE0_1137);
    let mut truths = Vec::new();
    for t in TABLES.iter() {
        storage.create_table(&schema_for(t)).expect("create table");
        let mut id = 1u64;
        for &tenant in TENANTS.iter() {
            for i in 0..ROWS_PER_TENANT {
                let visibility = if i % 2 == 0 {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                // 各テナントの Private 行のうち最初の 1 件をゼロベクトルカナリアにする。
                let is_zero_canary = i == 1;
                let emb: Vec<f32> = if is_zero_canary {
                    vec![0.0f32; t.dim]
                } else {
                    (0..t.dim).map(|_| rng.next_f32_signed()).collect()
                };
                let ctx = PolicyContext::with_visibilities(
                    tenant,
                    [Visibility::Public, Visibility::Private],
                )
                .expect("valid tenant");
                let mut values = vec![Value::Vector(emb)];
                for (idx, &col) in t.text_cols.iter().enumerate() {
                    let cell = if col == "lang" {
                        if i % 2 == 0 {
                            "ja".to_string()
                        } else {
                            "en".to_string()
                        }
                    } else {
                        format!("{col}-{idx} {}", unique_keyword(t.name, id))
                    };
                    values.push(Value::Text(cell));
                }
                engine::tenant::insert_typed_row(storage, t.name, &ctx, id, visibility, &values)
                    .expect("insert row");
                truths.push(RowTruth {
                    table: t.name,
                    id,
                    tenant,
                    visibility,
                    is_zero_canary,
                });
                id += 1;
            }
        }
    }
    truths
}

fn allowed_set(
    truths: &[RowTruth],
    table: &str,
    viewer_tenant: &str,
    allow_private: bool,
) -> HashSet<u64> {
    truths
        .iter()
        .filter(|t| t.table == table && is_allowed(t, viewer_tenant, allow_private))
        .map(|t| t.id)
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

fn query_vec(dim: usize) -> String {
    let v: Vec<f32> = (0..dim).map(|i| 1.0 + i as f32 * 0.1).collect();
    format!(
        "[{}]",
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// 投影セルから TEXT を抽出し、行の一意キーワードが含まれるかを混入検出に使う
/// （id 以外の経路での取り違え検出。`table` 内の全 id をまたぐキーワード衝突は
/// [`unique_keyword`] の命名で防いでいる）。
fn text_cells(row: &engine::sql::exec::ResultRow) -> Vec<&str> {
    row.cells
        .iter()
        .filter_map(|c| match c {
            Cell::Text(s) => Some(s.as_str()),
            _ => None,
        })
        .collect()
}

// ---------- R1: `execute_sql`（任意形状 SELECT・全テーブル・全テナント） ----------

/// SQL 文字列は固定語彙（テーブル定義・テストの定数）からのみ組み立てる
/// （未検証入力の連結を新設しない。security.md インジェクション観点）。
fn shape_queries(t: &TableInfo, with_visible_predicate: bool) -> Vec<String> {
    let q = query_vec(t.dim);
    let has_lang = t.text_cols.contains(&"lang");
    let has_body = t.text_cols.contains(&"body");

    let mut wheres: Vec<Option<String>> = vec![None];
    if has_lang {
        wheres.push(Some("lang = 'ja'".to_string()));
    }
    wheres.push(Some(format!("vec_norm({}) >= 0.0", t.vector_col)));
    if has_lang {
        wheres.push(Some(format!(
            "lang = 'ja' AND vec_norm({}) >= 0.0",
            t.vector_col
        )));
    }

    let mut order_bys: Vec<String> = vec![format!("{} <=> '{q}'", t.vector_col)];
    if has_body {
        order_bys.push(format!("hybrid_rrf({}, '{q}', body, 'ja')", t.vector_col));
    }

    let projections = ["*", "id"];
    let hints = [None, Some("HINT ORDER(DISTANCE, SCALAR, RLS)")];
    let limits = [1usize, 5, 20];

    let mut out = Vec::new();
    for proj in projections {
        for w in &wheres {
            for order_by in &order_bys {
                for hint in hints {
                    for limit in limits {
                        let mut sql = format!("SELECT {proj} FROM {}", t.name);
                        let mut clauses: Vec<String> = Vec::new();
                        if let Some(w) = w {
                            clauses.push(w.clone());
                        }
                        if with_visible_predicate {
                            clauses.push("visible()".to_string());
                        }
                        if !clauses.is_empty() {
                            sql.push_str(" WHERE ");
                            sql.push_str(&clauses.join(" AND "));
                        }
                        sql.push_str(&format!(" ORDER BY {order_by}"));
                        sql.push_str(&format!(" LIMIT {limit}"));
                        if let Some(h) = hint {
                            sql.push(' ');
                            sql.push_str(h);
                        }
                        out.push(sql);
                    }
                }
            }
        }
    }
    out
}

#[test]
fn shape_matrix_is_nonempty_and_covers_each_axis() {
    for t in TABLES.iter() {
        let shapes = shape_queries(t, false);
        assert!(!shapes.is_empty(), "table={}", t.name);
        assert!(
            shapes.iter().any(|s| s.contains("WHERE")),
            "table={} missing a WHERE-bearing shape",
            t.name
        );
        assert!(
            shapes.iter().any(|s| s.contains("HINT ORDER")),
            "table={} missing a HINT ORDER shape",
            t.name
        );
        if t.text_cols.contains(&"body") {
            assert!(
                shapes.iter().any(|s| s.contains("hybrid_rrf")),
                "table={} missing a hybrid_rrf shape",
                t.name
            );
        }
    }
}

#[test]
fn arbitrary_select_shapes_never_leak_across_tables_and_tenants() {
    let path = unique_db_path("rls8-shapes");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);
    let core = new_core(storage);

    for t in TABLES.iter() {
        for &tenant in TENANTS.iter() {
            for allow_private in [false, true] {
                let ctx = ctx_for(tenant, allow_private);
                let allowed = allowed_set(&truths, t.name, tenant, allow_private);
                for sql in shape_queries(t, false) {
                    let result = core
                        .execute_sql(&ctx, &sql)
                        .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}"));
                    assert!(
                        result.rows.len() <= allowed.len(),
                        "row count exceeds allowed set: table={} tenant={tenant} allow_private={allow_private} sql={sql:?}",
                        t.name
                    );
                    for row in &result.rows {
                        assert!(
                            allowed.contains(&row.id),
                            "disallowed row leaked: table={} tenant={tenant} allow_private={allow_private} sql={sql:?} id={}",
                            t.name, row.id
                        );
                        // TEXT 投影経路: 返却セルの一意キーワードが自テーブルのものである
                        // ことを確認する（id 以外の経路でのテーブル取り違え検出）。
                        for cell in text_cells(row) {
                            assert!(
                                !cell.contains("uniquetoken_")
                                    || cell.contains(&format!("uniquetoken_{}_", t.name)),
                                "cross-table keyword leaked into projection: table={} sql={sql:?} cell={cell:?}",
                                t.name
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn visible_predicate_presence_does_not_change_results() {
    let path = unique_db_path("rls8-predicate-equiv");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_corpus(&storage);
    let core = new_core(storage);

    for t in TABLES.iter() {
        for &tenant in TENANTS.iter() {
            for allow_private in [false, true] {
                let ctx = ctx_for(tenant, allow_private);
                let no_pred = shape_queries(t, false);
                let with_pred = shape_queries(t, true);
                assert_eq!(no_pred.len(), with_pred.len());
                for (sql_no, sql_yes) in no_pred.iter().zip(with_pred.iter()) {
                    let r1 = core
                        .execute_sql(&ctx, sql_no)
                        .unwrap_or_else(|e| panic!("sql={sql_no:?} err={e:?}"));
                    let r2 = core
                        .execute_sql(&ctx, sql_yes)
                        .unwrap_or_else(|e| panic!("sql={sql_yes:?} err={e:?}"));
                    assert_eq!(
                        result_ids(&r1),
                        result_ids(&r2),
                        "table={} tenant={tenant} allow_private={allow_private} sql_no={sql_no:?} sql_yes={sql_yes:?}",
                        t.name
                    );
                }
            }
        }
    }
}

// ---------- R2: UDF（宣言的 UDF・セッション経由の読み取り） ----------

#[test]
fn udf_reads_only_reach_visible_rows_across_tables_and_sessions() {
    let path = unique_db_path("rls8-udf");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);
    let core = new_core(storage);

    for t in TABLES.iter() {
        for &tenant in TENANTS.iter() {
            let ctx = ctx_for(tenant, true);
            let mut session = SessionState::default();
            core.execute_sql_in_session(
                &ctx,
                &mut session,
                "CREATE FUNCTION unit_sum(v) AS vec_sum(vec_div(v, vec_norm(v)))",
            )
            .expect("CREATE FUNCTION should succeed");

            let q = query_vec(t.dim);
            let allowed = allowed_set(&truths, t.name, tenant, true);

            // 結果列位置: 他テナントの不可視ゼロベクトル（カナリア）があっても
            // 0 除算に落ちず成功する（不可視行では UDF が一切評価されない）。自テナントの
            // 可視カナリア（ゼロベクトル）は意図的に 0 除算を起こすため、
            // `vec_norm(v) > 0.0` の式述語（許可リスト SQL-9 の式述語形。`=` 以外の
            // 比較演算子は列の直接比較には使えないため関数式で表す）で除外する
            // （可視カナリア自身の 0 除算検証は本関数末尾で別途行う）。
            let sql_result_col = format!(
                "SELECT id, unit_sum({}) AS s FROM {} WHERE vec_norm({}) > 0.0 ORDER BY {} <=> '{q}' LIMIT 20",
                t.vector_col, t.name, t.vector_col, t.vector_col
            );
            let outcome = core
                .execute_sql_in_session(&ctx, &mut session, &sql_result_col)
                .unwrap_or_else(|e| {
                    panic!(
                        "table={} tenant={tenant} sql={sql_result_col:?} err={e:?}",
                        t.name
                    )
                });
            let result = expect_query(outcome);
            for row in &result.rows {
                assert!(
                    allowed.contains(&row.id),
                    "disallowed row leaked via UDF result column: table={} tenant={tenant} id={}",
                    t.name,
                    row.id
                );
            }

            // WHERE 位置でも同じ契約（同じ理由で自テナントの可視カナリアを除外する）。
            let sql_where = format!(
                "SELECT id FROM {} WHERE vec_norm({}) > 0.0 AND unit_sum({}) < 1000.0 ORDER BY {} <=> '{q}' LIMIT 20",
                t.name, t.vector_col, t.vector_col, t.vector_col
            );
            let outcome = core
                .execute_sql_in_session(&ctx, &mut session, &sql_where)
                .unwrap_or_else(|e| {
                    panic!(
                        "table={} tenant={tenant} sql={sql_where:?} err={e:?}",
                        t.name
                    )
                });
            let result = expect_query(outcome);
            for row in &result.rows {
                assert!(
                    allowed.contains(&row.id),
                    "disallowed row leaked via UDF WHERE: table={} tenant={tenant} id={}",
                    t.name,
                    row.id
                );
            }

            // 可視側のカナリア（自テナントの Private ゼロベクトル行）に対しては
            // 0 除算で 22000 になることを固定し、「不可視行で評価されていない」ことを
            // 両側から裏付ける（見えているものは評価される・見えていないものは
            // 評価されない、の両方を検証しないと「常に評価されない」実装でも
            // 通ってしまう）。
            let canary = truths
                .iter()
                .find(|r| r.table == t.name && r.tenant == tenant && r.is_zero_canary)
                .expect("fixture has a per-tenant zero canary");
            let sql_canary_only = format!(
                "SELECT id, unit_sum({}) AS s FROM {} WHERE id = {} ORDER BY {} <=> '{q}' LIMIT 1",
                t.vector_col, t.name, canary.id, t.vector_col
            );
            let err = core
                .execute_sql_in_session(&ctx, &mut session, &sql_canary_only)
                .expect_err("visible zero-vector row must trigger division by zero");
            assert_eq!(
                err.wire_code(),
                "22000",
                "table={} tenant={tenant} id={}",
                t.name,
                canary.id
            );
        }
    }
}

#[test]
fn session_mode_switch_keeps_implicit_rls() {
    let path = unique_db_path("rls8-mode-switch");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);
    let core = new_core(storage);

    let t = &TABLES[0];
    let tenant = "tenant-a";
    let ctx = ctx_for(tenant, false);
    let allowed = allowed_set(&truths, t.name, tenant, false);
    let q = query_vec(t.dim);

    for mode in ["'precision'", "'recall'"] {
        let mut session = SessionState::default();
        core.execute_sql_in_session(&ctx, &mut session, &format!("SET search_mode = {mode}"))
            .unwrap_or_else(|e| panic!("SET search_mode {mode} err={e:?}"));
        let sql = format!(
            "SELECT id FROM {} ORDER BY {} <=> '{q}' LIMIT 20",
            t.name, t.vector_col
        );
        let outcome = core
            .execute_sql_in_session(&ctx, &mut session, &sql)
            .unwrap_or_else(|e| panic!("mode={mode} sql={sql:?} err={e:?}"));
        let result = expect_query(outcome);
        for row in &result.rows {
            assert!(
                allowed.contains(&row.id),
                "disallowed row leaked after SET search_mode {mode}: id={}",
                row.id
            );
        }
    }
}

// ---------- R3/R5: `VectorCore::search`（trait 経由）・`tenant::verify_hits` ----------

#[test]
fn trait_search_across_tables_matches_oracle_and_verify_hits() {
    let path = unique_db_path("rls8-trait-search");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);
    let core: Box<dyn VectorCore> = Box::new(new_core(storage));

    // フェーズ 1: `VectorCore::search`（trait 経由）を全テーブル・全テナント・
    // 全可視性設定で叩き、独立オラクル（`allowed_set`）で混入 0 件を確認しつつ
    // 結果を集める。
    let mut collected: Vec<(&'static str, PolicyContext, Vec<engine::kernel::SearchHit>)> =
        Vec::new();
    for t in TABLES.iter() {
        let query: Vec<f32> = (0..t.dim).map(|i| 1.0 + i as f32 * 0.1).collect();
        for &tenant in TENANTS.iter() {
            for allow_private in [false, true] {
                let ctx = ctx_for(tenant, allow_private);
                let allowed = allowed_set(&truths, t.name, tenant, allow_private);
                for k in [1usize, 5, 20] {
                    let hits = core
                        .search(&ctx, t.name, &query, k)
                        .unwrap_or_else(|e| panic!("table={} tenant={tenant} err={e:?}", t.name));
                    for hit in &hits {
                        assert!(
                            allowed.contains(&hit.id),
                            "disallowed row leaked via VectorCore::search: table={} tenant={tenant} allow_private={allow_private} k={k} id={}",
                            t.name, hit.id
                        );
                    }
                    collected.push((t.name, ctx.clone(), hits));
                }
            }
        }
    }

    // フェーズ 2: `core`（＝内部 `Storage`）を解放してから、`tenant::verify_hits`
    // （`(tenant_id, id)` 完全キー照合の独立経路）で同じ結果を再検証する。
    // `redb::Database` は同一ファイルへの複数ハンドル同時オープンを許さないため、
    // `core` を drop してハンドルを 1 つに保つ（フェーズを分離する理由）。
    drop(core);
    let verify_storage = Storage::open(&path).expect("reopen storage for verify_hits");
    for (table, ctx, hits) in &collected {
        engine::tenant::verify_hits(&verify_storage, table, ctx, hits).unwrap_or_else(|e| {
            panic!("verify_hits failed independent-key check: table={table} err={e:?}")
        });
    }
}

// ---------- R4: `get_row`（点取得） ----------

#[test]
fn get_row_is_not_found_for_every_disallowed_row_and_ok_for_every_allowed_row() {
    let path = unique_db_path("rls8-get-row");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);
    let core: Box<dyn VectorCore> = Box::new(new_core(storage));

    for t in TABLES.iter() {
        for &viewer in TENANTS.iter() {
            let ctx = ctx_for(viewer, true);
            for truth in truths.iter().filter(|r| r.table == t.name) {
                let got = core.get_row(&ctx, t.name, truth.tenant, truth.id);
                if is_allowed(truth, viewer, true) {
                    let row = got.unwrap_or_else(|e| {
                        panic!(
                            "expected Ok for allowed row: table={} viewer={viewer} owner={} id={} err={e:?}",
                            t.name, truth.tenant, truth.id
                        )
                    });
                    assert_eq!(row.tenant_id, truth.tenant);
                    assert_eq!(row.visibility, truth.visibility);
                } else {
                    assert!(
                        matches!(got, Err(CoreError::NotFound)),
                        "expected NotFound for disallowed row: table={} viewer={viewer} owner={} id={}",
                        t.name, truth.tenant, truth.id
                    );
                }
            }
        }
    }
}

// ---------- R5: `tenant::visible_rows` ----------

#[test]
fn visible_rows_equals_oracle_set_exactly() {
    let path = unique_db_path("rls8-visible-rows");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);

    for t in TABLES.iter() {
        for &tenant in TENANTS.iter() {
            for allow_private in [false, true] {
                let ctx = ctx_for(tenant, allow_private);
                let expected = allowed_set(&truths, t.name, tenant, allow_private);
                let rows = engine::tenant::visible_rows(&storage, t.name, &ctx)
                    .unwrap_or_else(|e| panic!("table={} tenant={tenant} err={e:?}", t.name));
                let got: HashSet<u64> = rows.iter().map(|r| r.id).collect();
                assert_eq!(
                    got, expected,
                    "table={} tenant={tenant} allow_private={allow_private}",
                    t.name
                );
                // 返却行は「他テナントの Public 行」または「自テナントの行（allow_private
                // のときのみ Private も含む）」のいずれかに限る（他テナントの Private 行が
                // 紛れ込んでいないことを行の帰属レベルでも確認する）。
                for row in &rows {
                    let self_owned = row.tenant_id == tenant;
                    // 他テナント行は Public のみ許容し、自テナント行も allow_private が
                    // false のときは Public のみ許容する（自テナントかつ allow_private の
                    // ときだけ Private を許容する）。
                    let private_allowed_here = self_owned && allow_private;
                    assert!(
                        private_allowed_here || row.visibility == Visibility::Public,
                        "visible_rows returned a row outside the allowed visibility: table={} viewer={tenant} owner={} id={} visibility={:?}",
                        t.name, row.tenant_id, row.id, row.visibility
                    );
                }
            }
        }
    }
}

// ---------- fail-closed: `USING PLAN` 展開後クエリは TASK-77 まで未実装 ----------

#[test]
fn using_plan_is_rejected_fail_closed_until_task_77() {
    let path = unique_db_path("rls8-using-plan");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    // `USING PLAN` の展開後クエリへの RLS 暗黙適用一般化検証は TASK-77（プラン文字列
    // 実行器）実装後に TASK-117 の管轄で行う。現状は許可リストが fail-closed に
    // 拒否することのみを固定し、「未実装経路が暗黙適用なしに開いている」ことがないと
    // 確認する。
    for t in TABLES.iter() {
        let q = query_vec(t.dim);
        let sql = format!(
            "SELECT * FROM {} ORDER BY {} <=> '{q}' LIMIT 5 USING PLAN 'x'",
            t.name, t.vector_col
        );
        let err = core
            .execute_sql(&ctx, &sql)
            .expect_err("USING PLAN must be rejected");
        assert_eq!(err.wire_code(), "42601", "table={} sql={sql:?}", t.name);
    }
}

// ---------- 負の対照: 検査ヘルパ自体が違反を見逃さないことを固定する ----------

#[test]
fn checker_negative_control_detects_fabricated_violation() {
    let path = unique_db_path("rls8-negative-control");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_corpus(&storage);

    let t = &TABLES[0];
    let viewer = "tenant-a";
    let allowed = allowed_set(&truths, t.name, viewer, false);

    // 実在する他テナント Private 行（本来は不許可）の id を、他テストの混入検出
    // アサーションへそのまま与える。検査ヘルパ（`allowed.contains`）自体が空振り
    // せず、この違反を検出できることを固定する（`tests/rls_security.rs` と同方針:
    // 常に成功する検査は無意味であるため、意図的な違反注入で検査器を裏付ける）。
    let disallowed_id = truths
        .iter()
        .find(|r| r.table == t.name && !is_allowed(r, viewer, false))
        .expect("fixture has a disallowed row")
        .id;

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            allowed.contains(&disallowed_id),
            "disallowed row leaked: table={} tenant={viewer} id={disallowed_id}",
            t.name
        );
    }))
    .is_err();

    assert!(
        caught,
        "the leak-detection assertion failed to catch a fabricated violation (checker is broken)"
    );
}
