//! ACORN-1（Issue #501・親 #500）の可視比率別 Recall・レイテンシ前後比較
//! （Issue #502）専用の SQL 表層結合テスト。`tests/hnsw_cache.rs` の
//! `seed_acorn_fixture`（選択率 20%・単一固定点）を一般化し、可視比率
//! 1/N（N ∈ {2,4,5,10}）を横断して `TraversalRegime`（`sql::hnsw_cache`。
//! `PlainScan`／`OneHop`／`TwoHop`）の切替と各点での既定エンジン対照
//! Recall@10 を固定する。
//!
//! 層 A（常時実行・`make ci` 対象）は縮小フィクスチャ（1,200 行・dim16）で
//! レジーム切替・Recall・テナント非漏えい・非 vacuous 性を回帰として固定する。
//! 層 B（`#[ignore]`・`make hnsw-acorn-recall`）は 25,000 行・dim128 の
//! より現実的な規模で同じ判定を実測値付きで標準出力へ記録する
//! （`docs/design/benchmark-judgement-policy.md` は対象外——本テストは
//! Recall・レジーム分類の正しさを固定する回帰テストであり、レイテンシの
//! 前後比較〔`make bench-knn-visible-ratio SWEEP_CANDIDATES=acorn`〕とは
//! 別の関心事）。
//!
//! production コード〔`crates/engine/src/`〕は無変更・テスト専任。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hnsw::{HnswParams, Ratio};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（`tests/hnsw_cache.rs::TestRng`・
// `gen_clustered_corpus` の複製。結合テストは crate 外の公開 API のみを
// 使う流儀のため独立に複製する） ----------

struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_unit(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn gen_clustered_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<Vec<f32>> {
    let mut center_rng = TestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(seed);
    (0..rows)
        .map(|i| {
            let center = &centers[i % centers.len()];
            let mut v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
            normalize(&mut v);
            v
        })
        .collect()
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn query_ids(core: &EngineCore, ctx: &PolicyContext, query: &[f32], k: usize) -> Vec<u64> {
    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT {}",
        vec_literal(query),
        k
    );
    let result = core.execute_sql(ctx, &sql).expect("query should succeed");
    result.rows.iter().map(|r| r.id).collect()
}

fn schema(dim: u32) -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("bucket", ColumnType::Text, false),
        ],
    )
}

/// `denominator` 分の 1 の可視比率（`id % denominator == 0` を `bucket='b0'`
/// へ割り当てる）を持つクラスタ構造コーパスを投入する（`tests/hnsw_cache.rs::
/// seed_acorn_fixture` の一般化。同ファイルの `denominator=5` 固定版と
/// 完全に同じ生成方式——`i % denominator == 0` の行に `b0`、それ以外は
/// `bN`〔`i % denominator`〕を割り当てる。`denominator=5` のとき旧
/// `tag='x'`/`tag='y'` の 2 値割り当てと選択率が一致する）。
fn seed_bucketed_fixture(
    storage: &Storage,
    schema: &TableSchema,
    ctx: &PolicyContext,
    op_tag: &str,
    dim: usize,
    rows: usize,
    denominator: u32,
) -> Vec<Vec<f32>> {
    let vectors = gen_clustered_corpus(9, dim, rows, 6);
    let op_id = OperationId::parse(&format!("hnsw-acorn-recall-{op_tag}")).expect("valid op id");
    let metadata: Vec<Vec<u8>> = (0..rows)
        .map(|i| {
            let bucket = format!("b{}", i as u32 % denominator);
            encode_scalar_columns(schema, &[Value::Null, Value::Text(bucket)])
                .expect("encode bucket metadata")
        })
        .collect();
    let rows_input: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: metadata[i].as_slice(),
                },
            )
        })
        .collect();
    engine::tenant::insert_rows(storage, "docs", ctx, &rows_input, &op_id).expect("seed rows");
    vectors
}

/// `search_engine::hnsw_kind`（既定 `HnswParams`）へ ACORN-1 opt-in
/// （`ratio = None` は既存契約〔1-hop のみ〕のまま）を適用するテスト専用
/// ヘルパ（`tests/hnsw_cache.rs::hnsw_kind_with_acorn` と同型）。
fn hnsw_kind_with_acorn(ratio: Option<Ratio>) -> search_engine::SearchEngineKind {
    let kind = search_engine::hnsw_kind(HnswParams::default()).expect("valid hnsw params");
    match (kind, ratio) {
        (search_engine::SearchEngineKind::Hnsw(validated), Some(r)) => {
            search_engine::SearchEngineKind::Hnsw(
                validated
                    .with_acorn_max_visible_ratio(r)
                    .expect("valid acorn_max_visible_ratio"),
            )
        }
        (other, _) => other,
    }
}

/// テナント境界確認用の private 行（tenant-b）を投入する。
fn seed_private_tenant_b(
    storage: &Storage,
    schema: &TableSchema,
    dim: usize,
    id_offset: u64,
    rows: usize,
    op_tag: &str,
) -> (Vec<Vec<f32>>, PolicyContext) {
    let vectors = gen_clustered_corpus(42, dim, rows, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
    let metadata_b = encode_scalar_columns(schema, &[Value::Null, Value::Text("b0".to_string())])
        .expect("encode tenant-b metadata");
    let rows_b: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                id_offset + i as u64,
                RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: metadata_b.as_slice(),
                },
            )
        })
        .collect();
    let op_b = OperationId::parse(&format!("hnsw-acorn-recall-{op_tag}-b")).expect("valid op id");
    engine::tenant::insert_rows(storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");
    (vectors, ctx_b)
}

/// 1 点（可視比率 1/`denominator`）の測定結果。
struct RegimePoint {
    denominator: u32,
    recall_at_10: f64,
    subset_searches: u64,
    acorn_searches: u64,
    acorn_expansions: u64,
    plain_scans: u64,
    mask_splits_graph: u64,
}

/// `acorn_ratio`（`None` なら opt-in 無効・既存 1-hop のまま）で可視比率
/// 1/`denominator`（`WHERE bucket='b0'`）を 1 点測定する共通本体。
/// denominator ごとに独立した DB（対象・brute-force 対照の 2 本）を構築
/// する——`bucket` 列の割り当て自体が denominator に依存するため、複数
/// denominator を 1 DB で共有できない。tenant-b private 行の非混入・
/// `builds` が warm-up 1 回のみであること（クエリごとの再構築が起きて
/// いないこと）もあわせて検証する。
fn run_regime_sweep(
    dim: usize,
    rows: usize,
    denominator: u32,
    acorn_ratio: Option<Ratio>,
    op_tag: &str,
) -> RegimePoint {
    let dir = unique_db_path(&format!("hnsw-acorn-recall-{op_tag}"));
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let sch = schema(dim as u32);
    storage.create_table(&sch).expect("create table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = seed_bucketed_fixture(&storage, &sch, &ctx_a, op_tag, dim, rows, denominator);
    let (_b_vectors, _ctx_b) =
        seed_private_tenant_b(&storage, &sch, dim, rows as u64 + 1, 100, op_tag);

    let ref_dir = unique_db_path(&format!("hnsw-acorn-recall-{op_tag}-ref"));
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&sch).expect("create ref table");
    let _ = seed_bucketed_fixture(
        &ref_storage,
        &sch,
        &ctx_a,
        &format!("{op_tag}-ref"),
        dim,
        rows,
        denominator,
    );
    let (b_vectors_ref, ctx_b_ref) = seed_private_tenant_b(
        &ref_storage,
        &sch,
        dim,
        rows as u64 + 1,
        100,
        &format!("{op_tag}-ref"),
    );
    let _ = b_vectors_ref;
    let _ = ctx_b_ref;
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let kind = hnsw_kind_with_acorn(acorn_ratio);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    // フィルタなしクエリを 1 本先に投げ `FullVisible` 索引を warm する
    // （`Subset` 形状は `Lookup::Miss` では構築しない契約）。
    let _ = query_ids(&core, &ctx_a, &vectors[0], 10);
    let builds_after_warm = core.hnsw_index_cache_stats().builds;

    const K: usize = 10;
    const QUERIES: usize = 20;
    // `gen_clustered_corpus` が割り当てるクラスタ数（`seed_bucketed_fixture`
    // の呼び出しに合わせた固定値）。可視集合内の行を「等間隔の行番号」で
    // 選ぶと `rows / QUERIES` がクラスタ数の倍数になりやすく、全クエリが
    // 単一クラスタへ偏る（codex-review 指摘）。可視な行をクラスタ別に
    // 集計しラウンドロビンで選ぶことで、可視クラスタを横断した決定的な
    // クエリ選択にする。
    const CLUSTERS: usize = 6;
    let mut by_cluster: Vec<Vec<usize>> = vec![Vec::new(); CLUSTERS];
    for idx in 0..rows {
        if (idx as u32).is_multiple_of(denominator) {
            by_cluster[idx % CLUSTERS].push(idx);
        }
    }
    let mut candidate_indices: Vec<usize> = Vec::with_capacity(QUERIES);
    let mut cursor = [0usize; CLUSTERS];
    'select: loop {
        let mut progressed = false;
        for (c, bucket) in cursor.iter_mut().zip(by_cluster.iter()) {
            if candidate_indices.len() >= QUERIES {
                break 'select;
            }
            if let Some(&idx) = bucket.get(*c) {
                candidate_indices.push(idx);
                *c += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    let mut total_hits = 0usize;
    let mut queried = 0usize;
    for &candidate_idx in &candidate_indices {
        queried += 1;
        let query = &vectors[candidate_idx];
        let sql = format!(
            "SELECT id FROM docs WHERE bucket = 'b0' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx_a, &sql).expect("filtered query").rows;
        let want = ref_core
            .execute_sql(&ctx_a, &sql)
            .expect("filtered query (ref)")
            .rows;
        for row in &got {
            assert!(
                row.id <= rows as u64,
                "tenant-a result must not include tenant-b row id {} (denominator={denominator})",
                row.id
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        total_hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
    }
    assert!(
        queried > 0,
        "no query originated from bucket='b0' (denominator={denominator}); fixture must yield at least one b0-origin query"
    );
    let recall = total_hits as f64 / (queried * K) as f64;

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.builds, builds_after_warm,
        "Subset shape must not rebuild the index per query (denominator={denominator})"
    );

    RegimePoint {
        denominator,
        recall_at_10: recall,
        subset_searches: stats.subset_searches,
        acorn_searches: stats.acorn_searches,
        acorn_expansions: stats.acorn_expansions,
        plain_scans: stats.plain_scans,
        mask_splits_graph: stats.mask_splits_graph,
    }
}

const ACORN_4_10: Ratio = Ratio {
    numerator: 4,
    denominator: 10,
};

/// 層 A（常時実行）: 縮小フィクスチャ（1,200 行・dim16）で可視比率
/// 1/2・1/4・1/5・1/10 を横断し、`acorn_max_visible_ratio = 4/10` opt-in の
/// もとで、full_scan_ratio（既定 1/10）以上かつ 4/10 以下の点
/// （denominator ∈ {4,5,10}）が TwoHop（`acorn_searches > 0`）へ、4/10 を
/// 上回る点（denominator=2・r=1/2）は 1-hop のまま（`acorn_searches == 0`）
/// であることを固定する。各点で既定エンジン対照 Recall@10 >= 0.9・
/// tenant-b 非混入（`run_regime_sweep` 内で assert 済み）を確認する。
#[test]
fn acorn_4_10_regime_sweep_matches_expected_hop_mode_and_recall() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;

    for denominator in [2u32, 4, 5, 10] {
        let p = run_regime_sweep(
            DIM,
            ROWS,
            denominator,
            Some(ACORN_4_10),
            &format!("layer-a-4-10-{denominator}"),
        );
        assert!(
            p.subset_searches > 0,
            "Subset shape must be exercised (non-vacuous) at denominator={denominator}"
        );
        if (4..=10).contains(&denominator) {
            // r = 1/denominator ∈ [1/10, 4/10] → TwoHop。
            assert!(
                p.acorn_searches > 0,
                "expected TwoHop (acorn_searches > 0) at denominator={denominator} (r=1/{denominator} <= 4/10), got acorn_searches=0 (mask_splits_graph={})",
                p.mask_splits_graph
            );
            assert!(
                p.acorn_expansions > 0,
                "acorn_expansions must be non-vacuous at denominator={denominator}"
            );
        } else {
            // denominator=2: r=1/2 > 4/10 → 既存の 1-hop 判定のまま
            // （このフィクスチャでは `mask_splits_graph` へ縮退するか、
            // 縮退せず `subset_searches` のみ増える可能性がある——ACORN が
            // 格上げしないことだけを固定し、縮退の有無は診断情報として
            // 記録する）。
            assert_eq!(
                p.acorn_searches, 0,
                "expected 1-hop (acorn_searches == 0) at denominator={denominator} (r=1/{denominator} > 4/10)"
            );
        }
        assert!(
            p.recall_at_10 >= 0.9,
            "recall@10 must be >= 0.9 at denominator={denominator}, got {} (subset_searches={} acorn_searches={} plain_scans={} mask_splits_graph={})",
            p.recall_at_10,
            p.subset_searches,
            p.acorn_searches,
            p.plain_scans,
            p.mask_splits_graph
        );
    }
}

/// 層 A（常時実行）: `acorn_max_visible_ratio` 未設定（既定 `None`）では、
/// 同じフィクスチャ・同じ可視比率で `acorn_searches`／`acorn_expansions` が
/// 常に 0 のまま（Issue #501 の既存契約を維持）であることを固定する
/// （opt-in しない限り本 Issue が既存動作へ影響しないことの直接証拠）。
#[test]
fn acorn_disabled_by_default_keeps_regime_sweep_unaffected() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;

    for denominator in [2u32, 4, 5, 10] {
        let p = run_regime_sweep(
            DIM,
            ROWS,
            denominator,
            None,
            &format!("layer-a-disabled-{denominator}"),
        );
        assert_eq!(
            p.acorn_searches, 0,
            "acorn_max_visible_ratio == None must never select HopMode::TwoHop (denominator={denominator})"
        );
        assert_eq!(p.acorn_expansions, 0);
    }
}

/// 層 B（`#[ignore]`・`make hnsw-acorn-recall`）: 25,000 行・dim128 の
/// より現実的な規模で可視比率 1/2・1/4・1/5・1/10 を横断し、`acorn_max_
/// visible_ratio = 4/10` opt-in のもとでの Recall@10・レジーム分類を表として
/// 標準出力へ記録する（実測値はオーナー判断〔2026-08-29〕により公開可・
/// `docs/design/hnsw-rls-cardinality-switch.md`「Issue #502」節へ転記する
/// ことを想定）。
#[test]
#[ignore]
fn layer_b_25k_dim128_acorn_regime_sweep_report() {
    const DIM: usize = 128;
    const ROWS: usize = 25_000;

    println!(
        "hnsw_acorn_recall: layer B report (rows={ROWS} dim={DIM} acorn_max_visible_ratio=4/10)"
    );
    println!("denominator recall@10 subset_searches acorn_searches acorn_expansions plain_scans mask_splits_graph");
    for denominator in [2u32, 4, 5, 10] {
        let p = run_regime_sweep(
            DIM,
            ROWS,
            denominator,
            Some(ACORN_4_10),
            &format!("layer-b-{denominator}"),
        );
        println!(
            "1/{} {:.4} {} {} {} {} {}",
            p.denominator,
            p.recall_at_10,
            p.subset_searches,
            p.acorn_searches,
            p.acorn_expansions,
            p.plain_scans,
            p.mask_splits_graph
        );
    }
}
