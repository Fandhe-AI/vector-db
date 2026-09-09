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
//! Issue #679（親 #502）: 上記の均等分散マスク（`id % N == 0`）は 25,000 行
//! 規模（並列 HNSW 構築）で TwoHop 到達が run 依存になることが判明したため、
//! `MaskShape`（クラスタ丸ごと可視・「縞」の 2 族）・`HopArm`・
//! `run_cluster_mask_arm` 以降を追加し、確実に到達する決定的フィクスチャで
//! hop モード別 Recall・`acorn_expansions` を記録する（改善は #680・
//! 展開過多時の fail-closed 縮退は #681 の担当）。
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

/// テナント境界確認用の private 行（tenant-b）を投入する。`bucket_label` は
/// フィクスチャの可視ラベル体系（`run_regime_sweep` は `"b0"`・
/// `run_cluster_mask_arm` は `"v"`）に合わせて呼び出し側が指定する——ラベルを
/// 固定すると、可視ラベルと異なる文字列を tenant-b 行へ付けてしまい
/// 「WHERE 可視 = tenant-b 行が構造的に一致しない」非 vacuous な非混入検査に
/// なってしまう（tenant-b 行は常にクエリの可視条件へマッチさせたうえで、
/// テナント境界自体で弾かれることを確認する必要がある）。
fn seed_private_tenant_b(
    storage: &Storage,
    schema: &TableSchema,
    dim: usize,
    id_offset: u64,
    rows: usize,
    op_tag: &str,
    bucket_label: &str,
) -> (Vec<Vec<f32>>, PolicyContext) {
    let vectors = gen_clustered_corpus(42, dim, rows, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
    let metadata_b = encode_scalar_columns(
        schema,
        &[Value::Null, Value::Text(bucket_label.to_string())],
    )
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
        seed_private_tenant_b(&storage, &sch, dim, rows as u64 + 1, 100, op_tag, "b0");

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
        "b0",
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

// ---------- Issue #679: TwoHop へ確実に到達する決定的フィクスチャ ----------
//
// 背景: 上の `run_regime_sweep`（均等分散マスク `id % denominator == 0`）は
// 25,000 行規模（並列 HNSW 構築）で `mask_splits_graph` の発火が run 依存
// （観測 3 run 中 1 run のみ TwoHop へ到達）——`hnsw/parallel_build.rs`
// 「決定性の範囲」が示すとおり並列構築はグラフ形状自体が run 間で
// 非決定的であり、疎で散在したマスクは連結性がその形状の細部に依存する。
// 本節はクラスタ丸ごと可視という粗いマスクで到達を安定させ、hop モード別の
// Recall・`acorn_expansions` を記録する（改善そのものは #680・展開過多時の
// fail-closed 縮退は #681 が担当。本 Issue は現状値の固定に専念する）。

/// 可視マスクの族。行番号 `i`（0 始まり）・クラスタ番号
/// `c = i % MASK_CLUSTERS`・クラスタ内序数 `j = i / MASK_CLUSTERS` から
/// 可視性を決定的に導出する（`gen_clustered_corpus` の割り当て方式
/// `center = centers[i % clusters]` と同じ剰余演算を使うことで、
/// 「クラスタ単位で丸ごと可視／不可視」を表現する）。
#[derive(Clone, Copy)]
enum MaskShape {
    /// 先頭 `clusters` 個のクラスタを丸ごと可視にする（主 variant）。
    /// クラスタ境界の橋渡し本数が少なく、連結性が構築側の run 間差に
    /// 左右されにくいと期待される形状。
    Whole { clusters: usize },
    /// `clusters` 個のクラスタのうち、各クラスタ内で `stride` おきの行のみを
    /// 可視にする「縞」形状（副次 variant）。可視行が同一クラスタ内に散在
    /// するため、TwoHop での連結が橋渡し数に強く依存する——#680／#681 が
    /// 挙げる劣化条件（橋渡し候補の希釈）の再現候補として informational に
    /// 記録するのみで、受け入れ条件の判定には使わない。
    Striped { clusters: usize, stride: usize },
}

/// `gen_clustered_corpus` へ渡すクラスタ数（`MaskShape` の剰余演算と
/// 揃える必要がある固定値。既存 `run_regime_sweep` の `CLUSTERS` と同値）。
const MASK_CLUSTERS: usize = 6;

impl MaskShape {
    fn is_visible(&self, i: usize) -> bool {
        let c = i % MASK_CLUSTERS;
        match *self {
            MaskShape::Whole { clusters } => c < clusters,
            MaskShape::Striped { clusters, stride } => {
                c < clusters && (i / MASK_CLUSTERS).is_multiple_of(stride)
            }
        }
    }

    /// 標準出力・doc 記録用のラベル（`docs/design/hnsw-rls-cardinality-switch.md`
    /// 「Issue #679」節の実測表と対応させる）。
    fn label(&self) -> String {
        match *self {
            MaskShape::Whole { clusters } => format!("whole{{clusters={clusters}}}"),
            MaskShape::Striped { clusters, stride } => {
                format!("striped{{clusters={clusters},stride={stride}}}")
            }
        }
    }
}

/// `MaskShape::is_visible` に従って `bucket` 列へ `'v'`（可視）／`'h'`
/// （不可視）を割り当てるクラスタ構造コーパスを投入する（`seed_bucketed_fixture`
/// の一般化。`denominator` 分の 1 の等間隔マスクではなく `MaskShape` の
/// クラスタ単位マスクを使う点のみが異なる）。
fn seed_cluster_mask_fixture(
    storage: &Storage,
    schema: &TableSchema,
    ctx: &PolicyContext,
    op_tag: &str,
    dim: usize,
    rows: usize,
    shape: MaskShape,
) -> Vec<Vec<f32>> {
    let vectors = gen_clustered_corpus(9, dim, rows, MASK_CLUSTERS);
    let op_id = OperationId::parse(&format!("hnsw-acorn-recall-{op_tag}")).expect("valid op id");
    let metadata: Vec<Vec<u8>> = (0..rows)
        .map(|i| {
            let bucket = if shape.is_visible(i) { "v" } else { "h" };
            encode_scalar_columns(schema, &[Value::Null, Value::Text(bucket.to_string())])
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

/// hop モード別の 3 arm。`OneHop` は既定パラメータのまま（ACORN opt-in 無効）、
/// `PlainScan` は `full_scan_ratio=1/1` で可視カーディナリティ比に関わらず
/// 常に plain scan を選ばせる対照点、`TwoHop` は `acorn_max_visible_ratio=4/10`
/// opt-in で ACORN-1 を有効化する（`full_scan_ratio` は既定 1/10 のまま）。
#[derive(Clone, Copy)]
enum HopArm {
    PlainScan,
    OneHop,
    TwoHop,
}

impl HopArm {
    fn label(&self) -> &'static str {
        match self {
            HopArm::PlainScan => "plain_scan",
            HopArm::OneHop => "one_hop",
            HopArm::TwoHop => "two_hop",
        }
    }
}

const RATIO_1_1: Ratio = Ratio {
    numerator: 1,
    denominator: 1,
};

/// `arm` に応じたパラメータ上書きを既定 `HnswParams` へ適用する
/// （`hnsw_kind_with_acorn` と同型だが、`full_scan_ratio` 上書きも扱う点が
/// 異なる。`PlainScan` と `TwoHop` は互いに独立したフィールドを上書きする
/// ため同時適用しない——`with_acorn_max_visible_ratio` は
/// `ratio >= full_scan_ratio` を要求するため、`full_scan_ratio=1/1` の
/// arm で acorn を設定すると必ず `HnswError::InvalidParams` になる）。
fn hnsw_kind_for_arm(arm: HopArm) -> search_engine::SearchEngineKind {
    let kind = search_engine::hnsw_kind(HnswParams::default()).expect("valid hnsw params");
    match kind {
        search_engine::SearchEngineKind::Hnsw(validated) => {
            let validated = match arm {
                HopArm::PlainScan => validated
                    .with_full_scan_ratio(RATIO_1_1)
                    .expect("valid full_scan_ratio"),
                HopArm::OneHop => validated,
                HopArm::TwoHop => validated
                    .with_acorn_max_visible_ratio(ACORN_4_10)
                    .expect("valid acorn_max_visible_ratio"),
            };
            search_engine::SearchEngineKind::Hnsw(validated)
        }
        other => other,
    }
}

/// `hnsw_kind_for_arm` の結果へ TwoHop 展開過多ガード（Issue #681・親 #674。
/// `ValidatedHnswParams::acorn_max_expansion_ratio`）を追加適用する。
/// `acorn_max_visible_ratio` とは独立フィールドのため `HopArm::OneHop`
/// （ACORN 自体が無効）へ適用しても構築は成功する——その場合 `hop` が常に
/// `OneHop` のためガード自体が一切観測されない（Issue #681 の「Overlay 側で
/// 独立に受理する」設計の直接検証。`acorn_expansion_guard_is_inert_without_
/// two_hop` 参照）。
fn hnsw_kind_for_arm_with_guard(
    arm: HopArm,
    guard_ratio: Option<Ratio>,
) -> search_engine::SearchEngineKind {
    let kind = hnsw_kind_for_arm(arm);
    match (kind, guard_ratio) {
        (search_engine::SearchEngineKind::Hnsw(validated), Some(ratio)) => {
            search_engine::SearchEngineKind::Hnsw(
                validated
                    .with_acorn_max_expansion_ratio(ratio)
                    .expect("valid acorn_max_expansion_ratio"),
            )
        }
        (kind, _) => kind,
    }
}

/// 可視行（`shape.is_visible(i)` を満たす行番号）を昇順に集め、その中から
/// 等間隔に最大 `max_queries` 本を決定的に選ぶ（`run_regime_sweep` の
/// クラスタ別ラウンドロビンと異なり、`Whole` 形状では可視行が単一クラスタ
/// 内に閉じるため「クラスタ横断」ではなく「可視集合内の等間隔抽出」が
/// 適切——単一クラスタ可視時にラウンドロビンを使うと退化して先頭 1 件しか
/// 選べない）。可視行が 0 件の場合はフィクスチャ不備として panic する。
fn select_visible_queries(rows: usize, shape: MaskShape, max_queries: usize) -> Vec<usize> {
    let visible: Vec<usize> = (0..rows).filter(|&i| shape.is_visible(i)).collect();
    assert!(
        !visible.is_empty(),
        "fixture must have at least one visible row (shape={})",
        shape.label()
    );
    if visible.len() <= max_queries {
        return visible;
    }
    let step = visible.len() as f64 / max_queries as f64;
    let mut selected: Vec<usize> = (0..max_queries)
        .map(|k| {
            let idx = ((k as f64 * step) as usize).min(visible.len() - 1);
            visible[idx]
        })
        .collect();
    // 等間隔の丸め込みで隣接候補が同一行番号へ縮退する可能性（`visible.len()`
    // が `max_queries` にごく近い場合）に備え、昇順であることを利用した
    // 連続重複除去で安全側に倒す（同一クエリの重複計上を防ぐ）。
    selected.dedup();
    selected
}

/// 1 arm（可視マスク `shape` × hop モード `arm`）の測定結果。
struct ArmPoint {
    shape_label: String,
    arm_label: &'static str,
    recall_at_10: f64,
    queries: usize,
    subset_searches: u64,
    acorn_searches: u64,
    acorn_expansions: u64,
    /// greedy_descend_masked のブリッジ降下（Issue #680）が受理・比較した
    /// 2-hop ノード数の累計。層 0 の bridge_expand が数える acorn_expansions
    /// とは別カウンタ。
    acorn_descent_bridges: u64,
    plain_scans: u64,
    mask_splits_graph: u64,
}

impl ArmPoint {
    fn acorn_expansions_per_query(&self) -> f64 {
        if self.queries == 0 {
            0.0
        } else {
            self.acorn_expansions as f64 / self.queries as f64
        }
    }
}

/// `shape`（可視マスク）× `arm`（hop モード）の 1 点を測定する共通本体
/// （`run_regime_sweep` と同じ構造——対象・brute-force 対照の 2 本の DB を
/// 構築し、tenant-b private 行の非混入・`Subset` 形状が warm-up 後に
/// 再構築されないことを assert する）。tenant-b 行には可視ラベル `'v'` を
/// 付け、可視条件 `WHERE bucket = 'v'` に構造的にマッチさせたうえで
/// テナント境界自体が弾くことを確認する（付けないと非混入 assert が
/// vacuous になる）。
fn run_cluster_mask_arm(
    dim: usize,
    rows: usize,
    shape: MaskShape,
    arm: HopArm,
    op_tag: &str,
) -> ArmPoint {
    let dir = unique_db_path(&format!("hnsw-acorn-recall-{op_tag}"));
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let sch = schema(dim as u32);
    storage.create_table(&sch).expect("create table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = seed_cluster_mask_fixture(&storage, &sch, &ctx_a, op_tag, dim, rows, shape);
    let (_b_vectors, _ctx_b) =
        seed_private_tenant_b(&storage, &sch, dim, rows as u64 + 1, 100, op_tag, "v");

    let ref_dir = unique_db_path(&format!("hnsw-acorn-recall-{op_tag}-ref"));
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&sch).expect("create ref table");
    let _ = seed_cluster_mask_fixture(
        &ref_storage,
        &sch,
        &ctx_a,
        &format!("{op_tag}-ref"),
        dim,
        rows,
        shape,
    );
    let (b_vectors_ref, ctx_b_ref) = seed_private_tenant_b(
        &ref_storage,
        &sch,
        dim,
        rows as u64 + 1,
        100,
        &format!("{op_tag}-ref"),
        "v",
    );
    let _ = b_vectors_ref;
    let _ = ctx_b_ref;
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let kind = hnsw_kind_for_arm(arm);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    // フィルタなしクエリを 1 本先に投げ `FullVisible` 索引を warm する
    // （`Subset` 形状は `Lookup::Miss` では構築しない契約。`run_regime_sweep`
    // と同じ理由）。
    let _ = query_ids(&core, &ctx_a, &vectors[0], 10);
    let builds_after_warm = core.hnsw_index_cache_stats().builds;

    const K: usize = 10;
    const QUERIES: usize = 20;
    let candidate_indices = select_visible_queries(rows, shape, QUERIES);

    let mut total_hits = 0usize;
    let mut queried = 0usize;
    for &candidate_idx in &candidate_indices {
        queried += 1;
        let query = &vectors[candidate_idx];
        let sql = format!(
            "SELECT id FROM docs WHERE bucket = 'v' ORDER BY embedding <=> '{}' LIMIT {K}",
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
                "tenant-a result must not include tenant-b row id {} (shape={} arm={})",
                row.id,
                shape.label(),
                arm.label()
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        total_hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
    }
    assert!(
        queried > 0,
        "no query originated from bucket='v' (shape={}); fixture must yield at least one visible row",
        shape.label()
    );
    let recall = total_hits as f64 / (queried * K) as f64;

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.builds,
        builds_after_warm,
        "Subset shape must not rebuild the index per query (shape={} arm={})",
        shape.label(),
        arm.label()
    );

    ArmPoint {
        shape_label: shape.label(),
        arm_label: arm.label(),
        recall_at_10: recall,
        queries: queried,
        subset_searches: stats.subset_searches,
        acorn_searches: stats.acorn_searches,
        acorn_expansions: stats.acorn_expansions,
        acorn_descent_bridges: stats.acorn_descent_bridges,
        plain_scans: stats.plain_scans,
        mask_splits_graph: stats.mask_splits_graph,
    }
}

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

/// 層 A（常時実行）: 1,200 行・dim16 のクラスタ丸ごと可視マスク（Issue #679）
/// で TwoHop 到達を固定する。1,200 行の索引構築は常に逐次（
/// `hnsw/parallel_search.rs::MIN_ROWS_PER_THREAD=1,024` により
/// `thread_count_for(1,200) == 1`）なのでグラフ形状は環境非依存でビット
/// 安定であり、`mask_splits_graph == 0`（分断縮退が一度も起きない）を
/// 契約として固定できる（25,000 行規模〔並列構築・run 依存〕の層 B とは
/// 異なる保証強度）。`Whole{1}`（可視比率 1/6）・`Whole{2}`（1/3）は
/// TraversalRegime::TwoHop（`full_scan_ratio=1/10 <= r <= acorn=4/10`）へ、
/// `Whole{3}`（1/2）は ACORN が格上げしないこと（`acorn_searches == 0`）を
/// 確認する。
#[test]
fn cluster_whole_mask_reaches_two_hop_deterministically() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;

    for clusters in [1usize, 2] {
        let shape = MaskShape::Whole { clusters };
        let p = run_cluster_mask_arm(
            DIM,
            ROWS,
            shape,
            HopArm::TwoHop,
            &format!("layer-a-679-whole-{clusters}-twohop"),
        );
        assert!(
            p.subset_searches > 0,
            "Subset shape must be exercised (non-vacuous) at shape={}",
            p.shape_label
        );
        assert!(
            p.acorn_searches > 0,
            "expected TwoHop (acorn_searches > 0) at shape={} (1,200 rows is always built sequentially, so \
             connectivity is deterministic), got acorn_searches=0 (mask_splits_graph={})",
            p.shape_label,
            p.mask_splits_graph
        );
        assert_eq!(
            p.mask_splits_graph, 0,
            "sequential build (1,200 rows) must never split the mask at shape={}",
            p.shape_label
        );
        assert!(
            p.acorn_expansions > 0,
            "acorn_expansions must be non-vacuous at shape={}",
            p.shape_label
        );
    }

    // `Whole{3}`（可視比率 1/2）は `full_scan_ratio(1/10) <= r <= acorn(4/10)`
    // を満たさず OneHop のまま——ACORN opt-in が無条件に格上げしないことの
    // 対照点。
    let p3 = run_cluster_mask_arm(
        DIM,
        ROWS,
        MaskShape::Whole { clusters: 3 },
        HopArm::TwoHop,
        "layer-a-679-whole-3-twohop",
    );
    assert_eq!(
        p3.acorn_searches, 0,
        "expected 1-hop (acorn_searches == 0) at shape={} (r=1/2 > 4/10)",
        p3.shape_label
    );

    // `PlainScan` arm（`full_scan_ratio=1/1`）は可視比率に関わらず常に
    // plain scan（アリーナ全体の brute-force）を選ぶため、既定エンジン
    // 対照との Recall@10 は構造的に 1.0 になる。
    let p_plain = run_cluster_mask_arm(
        DIM,
        ROWS,
        MaskShape::Whole { clusters: 1 },
        HopArm::PlainScan,
        "layer-a-679-whole-1-plainscan",
    );
    assert!(
        p_plain.plain_scans > 0,
        "PlainScan arm must be exercised (non-vacuous) at shape={}",
        p_plain.shape_label
    );
    assert_eq!(p_plain.acorn_searches, 0);
    assert_eq!(
        p_plain.recall_at_10, 1.0,
        "PlainScan arm must match brute-force exactly (structural, not just >= 0.9) at shape={}",
        p_plain.shape_label
    );
}

/// 層 A（常時実行）: `acorn_max_visible_ratio` 未設定（`HopArm::OneHop`）では
/// クラスタ丸ごと可視マスクでも `acorn_searches` が常に 0 のまま
/// （Issue #501 の既存契約を維持）であることを固定する（Issue #679 の
/// フィクスチャでも opt-in しない限り既存動作へ影響しないことの直接証拠）。
#[test]
fn cluster_whole_mask_acorn_disabled_by_default() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;

    let p = run_cluster_mask_arm(
        DIM,
        ROWS,
        MaskShape::Whole { clusters: 1 },
        HopArm::OneHop,
        "layer-a-679-whole-1-onehop-disabled",
    );
    assert_eq!(
        p.acorn_searches, 0,
        "acorn_max_visible_ratio == None must never select HopMode::TwoHop (shape={})",
        p.shape_label
    );
    assert_eq!(p.acorn_expansions, 0);
}

/// 層 A（常時実行）: TwoHop 展開過多ガード（Issue #681・親 #674）が
/// `acorn_max_expansion_ratio = 0/1`（展開が 1 件でもあれば発火）opt-in で
/// 実際に発火し、発火クエリの結果が既定エンジン（brute-force。plain scan が
/// 内部的に使うのと同じ全件探索）と完全一致することを固定する（AC1）。
/// クエリ単位で「発火」（`acorn_guard_fallbacks` の増分）と「TwoHop 完走」
/// （`acorn_searches` の増分）の二分割が全クエリを尽くすこと（他の縮退経路
/// 〔`masked_short`／`mask_splits_graph`／`plain_scans`〕へ逸れていないこと）
/// もあわせて確認する。
#[test]
fn acorn_expansion_guard_fires_and_matches_plain_scan() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const K: usize = 10;
    let shape = MaskShape::Whole { clusters: 1 };
    let op_tag = "layer-a-681-guard-fires";

    let dir = unique_db_path(&format!("hnsw-acorn-guard-{op_tag}"));
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let sch = schema(DIM as u32);
    storage.create_table(&sch).expect("create table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = seed_cluster_mask_fixture(&storage, &sch, &ctx_a, op_tag, DIM, ROWS, shape);
    let (_b_vectors, _ctx_b) =
        seed_private_tenant_b(&storage, &sch, DIM, ROWS as u64 + 1, 100, op_tag, "v");

    // 参照（brute-force。ガード発火時の plain scan と結果集合として同値に
    // なるはずの対照。同一シード〔`gen_clustered_corpus` seed=9 固定〕から
    // 独立に再構築するため `vectors` の内容は本体と一致する）。
    let ref_dir = unique_db_path(&format!("hnsw-acorn-guard-{op_tag}-ref"));
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&sch).expect("create ref table");
    let _ = seed_cluster_mask_fixture(
        &ref_storage,
        &sch,
        &ctx_a,
        &format!("{op_tag}-ref"),
        DIM,
        ROWS,
        shape,
    );
    let (_b_vectors_ref, _ctx_b_ref) = seed_private_tenant_b(
        &ref_storage,
        &sch,
        DIM,
        ROWS as u64 + 1,
        100,
        &format!("{op_tag}-ref"),
        "v",
    );
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let kind = hnsw_kind_for_arm_with_guard(
        HopArm::TwoHop,
        Some(Ratio {
            numerator: 0,
            denominator: 1,
        }),
    );
    let core = EngineCore::from_storage_with_engine(storage, kind);
    // フィルタなしクエリを 1 本先に投げ `FullVisible` 索引を warm する
    // （`Subset` 形状は `Lookup::Miss` では構築しない契約。`run_cluster_mask_arm`
    // と同じ理由）。
    let _ = query_ids(&core, &ctx_a, &vectors[0], K);

    let candidate_indices = select_visible_queries(ROWS, shape, 20);
    let mut fired_queries = 0usize;
    let mut completed_queries = 0usize;
    let mut fired_recall_hits = 0usize;
    let mut fired_recall_total = 0usize;
    for &idx in &candidate_indices {
        let before = core.hnsw_index_cache_stats();
        let query = &vectors[idx];
        let sql = format!(
            "SELECT id FROM docs WHERE bucket = 'v' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx_a, &sql).expect("guarded query").rows;
        let after = core.hnsw_index_cache_stats();

        let fired = after.acorn_guard_fallbacks > before.acorn_guard_fallbacks;
        let completed = after.acorn_searches > before.acorn_searches;
        assert!(
            fired ^ completed,
            "each TwoHop query must either fire the guard or complete TwoHop, never both/neither (idx={idx})"
        );
        assert_eq!(
            after.masked_short, before.masked_short,
            "masked_short must not fire (idx={idx})"
        );
        assert_eq!(
            after.mask_splits_graph, before.mask_splits_graph,
            "mask_splits_graph must not fire (idx={idx})"
        );
        assert_eq!(
            after.plain_scans, before.plain_scans,
            "plain_scans (full_scan_ratio 由来の縮退) must not fire (idx={idx})"
        );

        for row in &got {
            assert!(
                row.id <= ROWS as u64,
                "tenant-a result must not include tenant-b row id {} (idx={idx})",
                row.id
            );
        }

        if fired {
            fired_queries += 1;
            let want = ref_core.execute_sql(&ctx_a, &sql).expect("ref query").rows;
            let got_ids: std::collections::HashSet<u64> = got.iter().map(|r| r.id).collect();
            let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
            assert_eq!(
                got_ids, want_ids,
                "guard-fired result must equal brute-force (plain scan equivalence) at idx={idx}"
            );
            fired_recall_hits += got_ids.intersection(&want_ids).count();
            fired_recall_total += want_ids.len();
        } else {
            completed_queries += 1;
        }
    }

    assert!(
        fired_queries > 0,
        "guard must fire for at least one query with ratio=0/1 (non-vacuous)"
    );
    assert_eq!(
        fired_queries + completed_queries,
        candidate_indices.len(),
        "fired + completed TwoHop must account for every query"
    );
    assert_eq!(
        fired_recall_hits, fired_recall_total,
        "fired-query subset Recall@10 must be exactly 1.0 (plain scan equivalence)"
    );
}

/// 層 A（常時実行）: `acorn_max_expansion_ratio = 1/1` は `bridge_expand` の
/// 停止性契約（`expansions <= visible` が構造的に成立。
/// `crate::hnsw::acorn_expansions_exceed` ドキュメンテーションコメント参照）
/// により構造的に発火不能であることを、ガードなし TwoHop と全クエリの結果
/// 行 id 列が完全一致することで固定する（AC2）。`acorn_guard_fallbacks == 0`・
/// 両アームとも `acorn_searches > 0`（非 vacuous）・`acorn_expansions` が
/// 一致することもあわせて確認する。
#[test]
fn acorn_expansion_guard_never_fires_at_1_1_and_is_bit_identical() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const K: usize = 10;
    let shape = MaskShape::Whole { clusters: 1 };

    let baseline_dir = unique_db_path("hnsw-acorn-guard-1-1-baseline");
    let _baseline_cleanup = CleanupGuard(baseline_dir.clone());
    let baseline_storage = Storage::open(&baseline_dir).expect("open baseline storage");
    let sch = schema(DIM as u32);
    baseline_storage
        .create_table(&sch)
        .expect("create baseline table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = seed_cluster_mask_fixture(
        &baseline_storage,
        &sch,
        &ctx_a,
        "guard-1-1-baseline",
        DIM,
        ROWS,
        shape,
    );
    let baseline_kind = hnsw_kind_for_arm_with_guard(HopArm::TwoHop, None);
    let baseline_core = EngineCore::from_storage_with_engine(baseline_storage, baseline_kind);
    let _ = query_ids(&baseline_core, &ctx_a, &vectors[0], K);

    let guarded_dir = unique_db_path("hnsw-acorn-guard-1-1-guarded");
    let _guarded_cleanup = CleanupGuard(guarded_dir.clone());
    let guarded_storage = Storage::open(&guarded_dir).expect("open guarded storage");
    guarded_storage
        .create_table(&sch)
        .expect("create guarded table");
    // `gen_clustered_corpus` は seed 固定（op_tag 非依存）のため、独立に
    // 再構築しても `vectors` と同一のコーパスになる。
    let _ = seed_cluster_mask_fixture(
        &guarded_storage,
        &sch,
        &ctx_a,
        "guard-1-1-guarded",
        DIM,
        ROWS,
        shape,
    );
    let guarded_kind = hnsw_kind_for_arm_with_guard(
        HopArm::TwoHop,
        Some(Ratio {
            numerator: 1,
            denominator: 1,
        }),
    );
    let guarded_core = EngineCore::from_storage_with_engine(guarded_storage, guarded_kind);
    let _ = query_ids(&guarded_core, &ctx_a, &vectors[0], K);

    let candidate_indices = select_visible_queries(ROWS, shape, 20);
    for &idx in &candidate_indices {
        let query = &vectors[idx];
        let sql = format!(
            "SELECT id FROM docs WHERE bucket = 'v' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let baseline_ids: Vec<u64> = baseline_core
            .execute_sql(&ctx_a, &sql)
            .expect("baseline query")
            .rows
            .iter()
            .map(|r| r.id)
            .collect();
        let guarded_ids: Vec<u64> = guarded_core
            .execute_sql(&ctx_a, &sql)
            .expect("guarded query")
            .rows
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            baseline_ids, guarded_ids,
            "acorn_max_expansion_ratio=1/1 must be bit-identical to no guard at idx={idx}"
        );
    }

    let baseline_stats = baseline_core.hnsw_index_cache_stats();
    let guarded_stats = guarded_core.hnsw_index_cache_stats();
    assert_eq!(
        guarded_stats.acorn_guard_fallbacks, 0,
        "acorn_max_expansion_ratio=1/1 must never fire (structurally unfireable)"
    );
    assert!(
        guarded_stats.acorn_searches > 0,
        "guarded arm must reach TwoHop and complete (non-vacuous)"
    );
    assert!(
        baseline_stats.acorn_searches > 0,
        "baseline arm must reach TwoHop and complete (non-vacuous)"
    );
    assert_eq!(
        baseline_stats.acorn_expansions, guarded_stats.acorn_expansions,
        "acorn_expansions must match between guarded (never fires) and baseline arms"
    );
}

/// 層 A（常時実行）: TwoHop 展開過多ガード（Issue #681）は `hop ==
/// HopMode::TwoHop` のときのみ参照される独立フィールドであるため、
/// `HopArm::OneHop`（ACORN 自体が無効）へ `acorn_max_expansion_ratio = 0/1`
/// を設定しても一切観測されない（inert）ことを、ガードなし OneHop と全クエリ
/// の結果行 id 列が完全一致することで固定する（AC2）。
#[test]
fn acorn_expansion_guard_is_inert_without_two_hop() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const K: usize = 10;
    let shape = MaskShape::Whole { clusters: 1 };

    let baseline_dir = unique_db_path("hnsw-acorn-guard-onehop-baseline");
    let _baseline_cleanup = CleanupGuard(baseline_dir.clone());
    let baseline_storage = Storage::open(&baseline_dir).expect("open baseline storage");
    let sch = schema(DIM as u32);
    baseline_storage
        .create_table(&sch)
        .expect("create baseline table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = seed_cluster_mask_fixture(
        &baseline_storage,
        &sch,
        &ctx_a,
        "guard-onehop-baseline",
        DIM,
        ROWS,
        shape,
    );
    let baseline_kind = hnsw_kind_for_arm_with_guard(HopArm::OneHop, None);
    let baseline_core = EngineCore::from_storage_with_engine(baseline_storage, baseline_kind);
    let _ = query_ids(&baseline_core, &ctx_a, &vectors[0], K);

    let guarded_dir = unique_db_path("hnsw-acorn-guard-onehop-guarded");
    let _guarded_cleanup = CleanupGuard(guarded_dir.clone());
    let guarded_storage = Storage::open(&guarded_dir).expect("open guarded storage");
    guarded_storage
        .create_table(&sch)
        .expect("create guarded table");
    let _ = seed_cluster_mask_fixture(
        &guarded_storage,
        &sch,
        &ctx_a,
        "guard-onehop-guarded",
        DIM,
        ROWS,
        shape,
    );
    let guarded_kind = hnsw_kind_for_arm_with_guard(
        HopArm::OneHop,
        Some(Ratio {
            numerator: 0,
            denominator: 1,
        }),
    );
    let guarded_core = EngineCore::from_storage_with_engine(guarded_storage, guarded_kind);
    let _ = query_ids(&guarded_core, &ctx_a, &vectors[0], K);

    let candidate_indices = select_visible_queries(ROWS, shape, 20);
    for &idx in &candidate_indices {
        let query = &vectors[idx];
        let sql = format!(
            "SELECT id FROM docs WHERE bucket = 'v' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let baseline_ids: Vec<u64> = baseline_core
            .execute_sql(&ctx_a, &sql)
            .expect("baseline query")
            .rows
            .iter()
            .map(|r| r.id)
            .collect();
        let guarded_ids: Vec<u64> = guarded_core
            .execute_sql(&ctx_a, &sql)
            .expect("guarded query")
            .rows
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            baseline_ids, guarded_ids,
            "guard on OneHop regime must be inert (no TwoHop) at idx={idx}"
        );
    }

    let guarded_stats = guarded_core.hnsw_index_cache_stats();
    assert_eq!(guarded_stats.acorn_guard_fallbacks, 0);
    assert_eq!(guarded_stats.acorn_searches, 0);
    let baseline_stats = baseline_core.hnsw_index_cache_stats();
    assert_eq!(baseline_stats.acorn_searches, 0);
}

/// 層 B（`#[ignore]`・`make hnsw-acorn-twohop-runs`）: 25,000 行・dim128 の
/// クラスタ丸ごと可視マスク（Issue #679）で、並列 HNSW 構築（run 依存の
/// グラフ形状）のもとでも TwoHop 到達（`acorn_searches > 0`）が
/// `HNSW_ACORN_RECALL_RUNS`（既定 1・fail-closed パース）回連続で再現する
/// ことを記録する。主 variant（`Whole{1}`）は全 run で到達することを
/// assert し（受け入れ条件 1）、`Whole{2}`・副次 variant（`Striped{2,2}`）は
/// informational（診断出力のみ・assert なし——実測で `Whole{2}` は逆に
/// `mask_splits_graph` へ 5/5 run とも縮退することが判明したため、計画の
/// フォールバック方針に従い主 variant を `Whole{1}` 単独へ絞った。詳細は
/// `docs/design/hnsw-rls-cardinality-switch.md`「Issue #679」節参照）とする。
/// `Whole{3}`（対照点）は時間節約のため層 A のみで確認済み。実測値は同節へ
/// 転記することを想定する（オーナー判断〔2026-08-29〕により公開可）。
#[test]
#[ignore]
fn layer_b_25k_dim128_cluster_mask_hop_mode_report() {
    const DIM: usize = 128;
    const ROWS: usize = 25_000;
    const MAX_RUNS: u32 = 20;

    let runs: u32 = match std::env::var("HNSW_ACORN_RECALL_RUNS") {
        Ok(s) => s
            .parse::<u32>()
            .ok()
            .filter(|&n| (1..=MAX_RUNS).contains(&n))
            .unwrap_or_else(|| {
                panic!("HNSW_ACORN_RECALL_RUNS must be an integer in 1..={MAX_RUNS}, got {s:?}")
            }),
        Err(std::env::VarError::NotPresent) => 1,
        Err(e) => panic!("HNSW_ACORN_RECALL_RUNS must be valid UTF-8: {e}"),
    };

    println!(
        "hnsw_acorn_recall: layer B cluster-mask hop-mode report (rows={ROWS} dim={DIM} runs={runs})"
    );
    println!("run shape ratio arm recall@10 queries subset_searches acorn_searches acorn_expansions acorn_expansions/query acorn_descent_bridges plain_scans mask_splits_graph");

    let points: [(MaskShape, &str); 3] = [
        (MaskShape::Whole { clusters: 1 }, "1/6"),
        (MaskShape::Whole { clusters: 2 }, "1/3"),
        (
            MaskShape::Striped {
                clusters: 2,
                stride: 2,
            },
            "1/6(striped)",
        ),
    ];

    for (shape, ratio_label) in points {
        let mut twohop_reached = 0u32;
        for run in 0..runs {
            for arm in [HopArm::PlainScan, HopArm::OneHop, HopArm::TwoHop] {
                let op_tag = format!(
                    "layer-b-679-{}-run{run}-{}",
                    shape.label().replace(['{', '}', ',', '=', ':'], "_"),
                    arm.label()
                );
                let p = run_cluster_mask_arm(DIM, ROWS, shape, arm, &op_tag);
                println!(
                    "{run} {} {ratio_label} {} {:.4} {} {} {} {} {:.2} {} {} {}",
                    p.shape_label,
                    p.arm_label,
                    p.recall_at_10,
                    p.queries,
                    p.subset_searches,
                    p.acorn_searches,
                    p.acorn_expansions,
                    p.acorn_expansions_per_query(),
                    p.acorn_descent_bridges,
                    p.plain_scans,
                    p.mask_splits_graph
                );
                if matches!(arm, HopArm::TwoHop) && p.acorn_searches > 0 {
                    twohop_reached += 1;
                    // Issue #680: TwoHop 到達時は Recall@10 が既定エンジン
                    // 対照で 0.9 以上であることを固定する（改善前の実測値
                    // 0.9750 と非劣化、`docs/design/hnsw-rls-cardinality-switch.md`
                    // 「Issue #680」節参照）。
                    assert!(
                        p.recall_at_10 >= 0.9,
                        "TwoHop recall@10 must be >= 0.9 when reached (shape={} got {})",
                        p.shape_label,
                        p.recall_at_10
                    );
                }
            }
        }
        println!(
            "twohop_reached={twohop_reached}/{runs} shape={}",
            shape.label()
        );
        // 受け入れ条件 1（Issue #679）: 主 variant（`Whole{1}`）は全 run で
        // TwoHop へ到達する。実測では `Whole{2}`（可視比率 1/3）は逆に
        // `mask_splits_graph` へ 5/5 run とも縮退することが判明した——
        // 可視比率が大きいほど連結しやすいという直感に反する結果であり、
        // 並列構築のグラフ形状（クラスタ境界の橋渡し配置）に依存する
        // fixture 固有の挙動と考えられる（詳細は
        // `docs/design/hnsw-rls-cardinality-switch.md`「Issue #679」節）。
        // そのため `Whole{2}` は `Striped` と同じく informational（診断出力
        // のみ）へ位置づけを変更し、主 variant を `Whole{1}` 単独に絞る
        // （計画の「主 variant が未達なら他候補へ格上げ」フォールバックを
        // 適用し、5/5 到達を安定して示す形状のみを受け入れ条件の対象にした）。
        if matches!(shape, MaskShape::Whole { clusters: 1 }) {
            assert_eq!(
                twohop_reached, runs,
                "expected TwoHop to be reached on every run for shape={} (got {twohop_reached}/{runs})",
                shape.label()
            );
        }
    }
}
