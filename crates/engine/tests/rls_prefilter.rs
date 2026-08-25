//! `engine::rls::PrefilterIndex` の結合テスト（TASK-133・対象ビヘイビア: RLS-1〜4）。
//!
//! `crates/engine/tests/vector_core.rs`（TASK-124）のシード手法（`Storage::open` で
//! 直接テーブル作成・行投入してから production API へ渡す）と、
//! `crates/engine/tests/hybrid_recall.rs`（TASK-104）の決定的合成コーパス生成
//! （自前 xorshift64*・外部クレート不使用）を踏襲する。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::kernel::{CpuScalarProvider, KernelError, SearchHit, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::rls::PrefilterIndex;
use engine::storage::{RowInput, Storage, Visibility};

// ---------- 決定的擬似乱数（xorshift64*。`tests/hybrid_recall.rs` と同一実装。外部クレート不使用） ----------

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

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// `[-1.0, 1.0)` の決定的な擬似乱数 `f32`（埋め込み成分の生成に使う）。
    fn next_f32_signed(&mut self) -> f32 {
        (self.next_f64() * 2.0 - 1.0) as f32
    }
}

// ---------- テスト共通のセットアップ ----------

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// テナント単位へ分割してシード投入する共通ヘルパ（複数テストへの複製を避けるため
// `src/test_util/seed_rows.rs` へ一本化した。`temp_db.rs` と同じ取り込み方式）。
#[path = "../src/test_util/seed_rows.rs"]
mod seed_rows;
use seed_rows::seed_rows_grouped_by_tenant;

const DIM: u32 = 16;
const TARGET_TENANT: &str = "tenant-target";
const OTHER_TENANT: &str = "tenant-other";

fn schema_for(table_name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        table_name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

/// 決定的な合成コーパスを構築する。行 `id`（`1..=num_rows`）ごとに `rng.next_f64() < visible_rate`
/// で対象テナント（[`TARGET_TENANT`]・`Public`）か他テナント（[`OTHER_TENANT`]・`Private`）かを
/// 振り分け、埋め込みは `[-1.0, 1.0)` の一様乱数（`DIM` 次元）で生成する（構造上、対象
/// テナントの可視行だけが `PrefilterIndex` へ入る。RLS-1 の混入検証に使うため、対象テナント行
/// の id 集合を独立に返す）。他テナント行は `Private` にしてテナント分離そのものを検証する
/// （ポインタ: TASK-89 / TABLE-9。本ファイルの `ctx` は既定の `Public` のみ許可のため、
/// `Private` の他テナント行は引き続き不可視）。
///
/// `seed` はコーパスごとに変え、可視率間でコーパス自体が偏らないようにする。
fn seed_corpus(
    storage: &Storage,
    table: &str,
    num_rows: u64,
    visible_rate: f64,
    seed: u64,
) -> std::collections::BTreeSet<u64> {
    storage
        .create_table(&schema_for(table, DIM))
        .expect("create table");
    let mut rng = Xorshift64::new(seed);
    let mut target_ids = std::collections::BTreeSet::new();
    // 単一トランザクション（`insert_rows_into_table`。TASK-146）でまとめて挿入する。
    // 行ごとの `insert_row_into_table` は呼び出しごとに commit（fsync 含む）するため、
    // 数千〜数万行規模のコーパス生成では所要時間が支配的になり、本ファイルの計測系
    // テスト（RLS-2）のノイズ源にもなる。
    let mut tenants: Vec<&str> = Vec::with_capacity(num_rows as usize);
    let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(num_rows as usize);
    for id in 1..=num_rows {
        let is_target = rng.next_f64() < visible_rate;
        tenants.push(if is_target {
            TARGET_TENANT
        } else {
            OTHER_TENANT
        });
        embeddings.push((0..DIM).map(|_| rng.next_f32_signed()).collect());
        if is_target {
            target_ids.insert(id);
        }
    }
    let rows: Vec<(u64, RowInput<'_>)> = (1..=num_rows)
        .map(|id| {
            let idx = (id - 1) as usize;
            let visibility = if tenants[idx] == TARGET_TENANT {
                Visibility::Public
            } else {
                Visibility::Private
            };
            (
                id,
                RowInput {
                    tenant_id: tenants[idx],
                    visibility,
                    embedding: &embeddings[idx],
                    metadata: &[],
                },
            )
        })
        .collect();
    seed_rows_grouped_by_tenant(storage, table, &rows);
    target_ids
}

fn random_query(seed: u64) -> Vec<f32> {
    let mut rng = Xorshift64::new(seed);
    (0..DIM).map(|_| rng.next_f32_signed()).collect()
}

// 対象ビヘイビア: RLS-1。複数の可視率・複数クエリで検索し、全ヒットが許可集合
// （[`TARGET_TENANT`]）に属することを機械検証する（テナント境界 P0）。
#[test]
fn rls1_no_cross_tenant_leakage_across_visibility_rates() {
    const NUM_ROWS: u64 = 4_000;
    let ctx = PolicyContext::new(TARGET_TENANT).expect("valid tenant");

    for (rate_idx, &visible_rate) in [0.9, 0.5, 0.3, 0.1].iter().enumerate() {
        let path = unique_db_path(&format!("rls1-{rate_idx}"));
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let target_ids = seed_corpus(
            &storage,
            "docs",
            NUM_ROWS,
            visible_rate,
            1000 + rate_idx as u64,
        );

        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build prefilter index");
        assert_eq!(
            index.len(&ctx).expect("len ok"),
            target_ids.len(),
            "prefilter index must hold exactly the target-tenant rows (rate={visible_rate})"
        );

        for query_idx in 0..5u64 {
            let query = random_query(9000 + rate_idx as u64 * 10 + query_idx);
            let hits = index
                .search(&ctx, &CpuScalarProvider, &query, 20)
                .expect("search ok");
            for hit in &hits {
                assert!(
                    target_ids.contains(&hit.id),
                    "cross-tenant leak detected: id={} rate={visible_rate} query={query_idx}",
                    hit.id
                );
            }
        }
    }
}

// 対象ビヘイビア: RLS-2。可視率ごとに `PrefilterIndex` を事前構築し（構築時間は計測対象外）、
// 検索専用の所要時間を p95 で相対比較する（CI ノイズ吸収のマージン付き）。
#[test]
fn rls2_lower_visibility_rate_does_not_regress_search_only_latency() {
    const NUM_ROWS: u64 = 20_000;
    const WARMUP_TRIALS: usize = 3;
    const TIMED_TRIALS: usize = 20;
    // CI ノイズを吸収するマージン（低可視率側の p95 がこの倍率を超えて高可視率側を
    // 上回った場合のみ失敗とする。可視行数が約 9 倍違う設計のため、実際のスキャン量差は
    // このマージンよりずっと大きい）。
    const NOISE_MARGIN: f64 = 1.5;

    fn measure_p95_search_only(visible_rate: f64, seed: u64) -> Duration {
        let path = unique_db_path(&format!("rls2-{seed}"));
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        seed_corpus(&storage, "docs", NUM_ROWS, visible_rate, seed);
        let ctx = PolicyContext::new(TARGET_TENANT).expect("valid tenant");
        // 構築（アリーナ確保）は計測対象外。ここで完了させてから計測ループへ入る。
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build prefilter index");

        let query = random_query(seed * 100);
        for _ in 0..WARMUP_TRIALS {
            let _ = index
                .search(&ctx, &CpuScalarProvider, &query, 20)
                .expect("warmup search");
        }

        let mut durations = Vec::with_capacity(TIMED_TRIALS);
        for _ in 0..TIMED_TRIALS {
            let start = Instant::now();
            let _ = index
                .search(&ctx, &CpuScalarProvider, &query, 20)
                .expect("timed search");
            durations.push(start.elapsed());
        }
        durations.sort();
        // p95: ソート済み配列の 95 パーセンタイル位置（`TIMED_TRIALS=20` なら添字 18、
        // すなわち 20 件中 19 番目 = 2 番目に大きい値。試行数が小さいため近似だが、
        // `tests/incremental_write_perf.rs` と同じく中央値ではなく外れ値側を見ることで
        // 悪化を見逃さない側に倒す）。
        let idx = ((durations.len() as f64) * 0.95).ceil() as usize;
        let idx = idx.saturating_sub(1).min(durations.len() - 1);
        durations[idx]
    }

    let p95_high_visibility = measure_p95_search_only(0.9, 2000);
    let p95_low_visibility = measure_p95_search_only(0.1, 2001);

    assert!(
        p95_low_visibility.as_secs_f64() <= p95_high_visibility.as_secs_f64() * NOISE_MARGIN,
        "low-visibility search-only p95 ({p95_low_visibility:?}) regressed past \
         high-visibility p95 ({p95_high_visibility:?}) beyond the noise margin"
    );
}

/// [`rls3_search_calls_provider_exactly_once_with_requested_k`] 用の計装 provider。
/// 呼び出し回数と要求 `k` を記録してから [`CpuScalarProvider`] へ委譲する。
struct CountingProvider {
    calls: AtomicUsize,
    requested_ks: Mutex<Vec<usize>>,
}

impl CountingProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            requested_ks: Mutex::new(Vec::new()),
        }
    }
}

impl SearchProvider for CountingProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requested_ks
            .lock()
            .expect("lock requested_ks")
            .push(input.k);
        CpuScalarProvider.search(input)
    }
}

// 対象ビヘイビア: RLS-3。1 検索につき provider 呼び出しが 1 回・要求件数がちょうど
// 呼び出し元の k であることを検証する。
#[test]
fn rls3_search_calls_provider_exactly_once_with_requested_k() {
    let path = unique_db_path("rls3");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // 可視行 5 件のみの小さいコーパス（k > 可視件数のケースを作るため）。
    let target_ids = seed_corpus(&storage, "docs", 5, 1.0, 3000);
    assert_eq!(target_ids.len(), 5);

    let ctx = PolicyContext::new(TARGET_TENANT).expect("valid tenant");
    let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build prefilter index");
    let query = random_query(3001);

    // ケース 1: k(10) > 可視件数(5) → min(k, 5) = 5 件、呼び出し 1 回・要求 k=10。
    let provider = CountingProvider::new();
    let hits = index
        .search(&ctx, &provider, &query, 10)
        .expect("search ok");
    assert_eq!(hits.len(), 5);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider
            .requested_ks
            .lock()
            .expect("lock requested_ks")
            .as_slice(),
        &[10]
    );

    // ケース 2: k(3) < 可視件数(5) → min(k, 5) = 3 件、呼び出し 1 回・要求 k=3。
    let provider = CountingProvider::new();
    let hits = index.search(&ctx, &provider, &query, 3).expect("search ok");
    assert_eq!(hits.len(), 3);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider
            .requested_ks
            .lock()
            .expect("lock requested_ks")
            .as_slice(),
        &[3]
    );
}

/// 内積スコア。[`engine::kernel::CpuScalarProvider`] が使うカーネルと同一の
/// `engine::isa::current().dot` へ委譲する（TASK-156・CORE-14 対応: SIMD 化で
/// 加算順序が変わり得るため、本テストのように production の Top-K と float 完全一致
/// で比較する箇所は、算術カーネル自体は本番経路と共有しつつ、順位ロジック
/// （全行スキャン→許可集合フィルタ→ソート、という手順そのもの）は本テストが独立に
/// 組み立てることで、`PrefilterIndex` の実装を経由しない検証という趣旨を保つ）。
fn dot(a: &[f32], b: &[f32]) -> f32 {
    engine::isa::current().dot(a, b)
}

// 対象ビヘイビア: RLS-4。テスト側で独立に「全行スキャン→許可集合でフィルタ→Top-K」を
// 算出し、`PrefilterIndex` の Top-K と一致することを検証する。タイブレーク規則
// （スコア降順・同点 id 昇順）は `PrefilterIndex::search` が委譲する `CpuScalarProvider`
// と同一の規則を本テスト側でも用いる。
#[test]
fn rls4_top_k_matches_independently_computed_full_scan_ranking() {
    const NUM_ROWS: u64 = 3_000;
    const K: usize = 20;

    let path = unique_db_path("rls4");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let target_ids = seed_corpus(&storage, "docs", NUM_ROWS, 0.4, 4000);

    let ctx = PolicyContext::new(TARGET_TENANT).expect("valid tenant");
    let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build prefilter index");

    for query_idx in 0..5u64 {
        let query = random_query(8000 + query_idx);

        // 「全行スキャン→許可集合でフィルタ→Top-K」を独立に算出する（`PrefilterIndex` を
        // 経由しない。`Storage::get_row_from_table` で対象テナント行を直接読み直し、
        // `PrefilterIndex::search` と同じスコア降順・同点 id 昇順で並べる）。
        let mut scored: Vec<(u64, f32)> = target_ids
            .iter()
            .map(|&id| {
                let row = storage
                    .get_row_from_table("docs", TARGET_TENANT, id)
                    .expect("row must exist");
                (id, dot(&row.embedding, &query))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(K);

        let hits = index
            .search(&ctx, &CpuScalarProvider, &query, K)
            .expect("search ok");

        let actual: Vec<(u64, f32)> = hits.into_iter().map(|h| (h.id, h.score)).collect();
        assert_eq!(
            actual, scored,
            "PrefilterIndex top-{K} must match the independently computed full-scan \
             ranking (query={query_idx})"
        );
    }
}
