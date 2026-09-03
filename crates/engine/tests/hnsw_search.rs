//! `engine::hnsw::HnswIndex::search`（TASK-132・対象ビヘイビア: CORE-9・CORE-10。
//! ポインタ: `docs/design/hnsw-search.md`）の結合テスト。
//!
//! - 受け入れ条件 (a): brute-force（`engine::kernel::CpuScalarProvider`。production
//!   が使う総当たりカーネル）対照の Recall@10 を ef=64・ef=256 で計測する。層 A
//!   （常時実行・縮小規模）は回帰保護、層 B（`#[ignore]`・10k×dim128・release 実行）
//!   が受け入れ条件の正本（`make hnsw-search-recall`）。
//! - 受け入れ条件 (b): 同一索引・同一クエリでの結果再現性（`Vec<CandidateHit>` の
//!   完全一致）。
//!
//! `tests/*.rs` の既存流儀（crate 外の公開 API のみで検証する。`tests/hnsw.rs` と
//! 同じ位置付け）に従う。

use std::collections::HashSet;

use engine::hnsw::{
    HnswError, HnswIndex, HnswParams, HnswSearchScratch, MAX_EF, SEQUENTIAL_PREFIX_NODES,
};
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

/// 決定的シードの xorshift64*（`crates/engine/tests/hnsw.rs::TestRng` と同
/// アルゴリズム。結合テストは crate 外の公開 API のみを使う流儀のため独立に
/// 複製する）。
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

    fn next_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32
    }

    fn next_unit(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
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

/// 埋め込みらしい緩いクラスタ構造を持つ、L2 正規化済みの決定的コーパスを生成する
/// （`hnsw.rs` 冒頭「cosine は正規化済みベクトルを渡して内積に一致させる」既定
/// 契約に合わせる）。`clusters` 個の決定的中心へジッタを加えた行を生成し、各行を
/// 単位ベクトルへ正規化する。
fn gen_clustered_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<f32> {
    let mut center_rng = TestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for i in 0..rows {
        let center = &centers[i % centers.len()];
        let mut v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
        normalize(&mut v);
        out.extend(v);
    }
    out
}

/// コーパス外のクエリベクトル（正規化済み）を生成する。埋め込みらしい評価に
/// するため、コーパスと同じクラスタ中心群からジッタを加えて生成する
/// （連続値のため厳密に同一のコーパス行を引く確率は無視できるほど小さい。
/// 完全な一様乱数クエリは HNSW にとって最難条件になるため採用しない。3.3 節
/// 参照）。`corpus_seed` はコーパス生成と同じ値を渡し、同じクラスタ中心を
/// 再現する。
fn gen_query(corpus_seed: u64, query_seed: u64, dim: usize, clusters: usize) -> Vec<f32> {
    let mut center_rng = TestRng::new(corpus_seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(query_seed);
    let center_idx = (rng.next_u64() as usize) % centers.len();
    let mut v: Vec<f32> = centers[center_idx]
        .iter()
        .map(|c| c + rng.next_unit() * 0.2)
        .collect();
    normalize(&mut v);
    v
}

fn gen_queries(
    corpus_seed: u64,
    query_seed: u64,
    dim: usize,
    clusters: usize,
    count: usize,
) -> Vec<Vec<f32>> {
    (0..count)
        .map(|i| {
            gen_query(
                corpus_seed,
                query_seed.wrapping_add(i as u64).wrapping_mul(0x9E37_79B1),
                dim,
                clusters,
            )
        })
        .collect()
}

/// 一様乱数のみの（クラスタ構造を持たない）決定的コーパス。HNSW にとって
/// 最難条件の一つであり、受け入れ判定の対象にはせず、層 B で informational
/// な参考値としてのみ計測する（3.3 節参照）。
fn gen_uniform_corpus(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
    let mut rng = TestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for _ in 0..rows {
        let mut v: Vec<f32> = (0..dim).map(|_| rng.next_unit()).collect();
        normalize(&mut v);
        out.extend(v);
    }
    out
}

fn gen_uniform_queries(seed: u64, dim: usize, count: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|i| {
            let mut rng = TestRng::new(seed.wrapping_add(i as u64).wrapping_mul(0x9E37_79B1));
            let mut v: Vec<f32> = (0..dim).map(|_| rng.next_unit()).collect();
            normalize(&mut v);
            v
        })
        .collect()
}

/// brute-force（`CpuScalarProvider`。production の総当たりカーネル）対照で
/// Recall@10 を計測する。id 集合の一致率をクエリ平均する。
fn recall_at_10(
    index: &HnswIndex,
    vectors: &[f32],
    dim: usize,
    rows: usize,
    ef: usize,
    queries: &[Vec<f32>],
) -> f64 {
    let ids: Vec<u64> = (0..rows as u64).collect();
    let provider = CpuScalarProvider;
    let mut scratch = HnswSearchScratch::default();
    let mut hits_total = 0usize;
    for query in queries {
        let brute = provider
            .search(SearchInput {
                ids: &ids,
                vectors,
                dim: dim as u32,
                query,
                k: 10,
            })
            .expect("brute-force search must succeed");
        let brute_ids: HashSet<u64> = brute.iter().map(|h| h.id).collect();

        let hnsw = index
            .search(query, 10, ef, &mut scratch)
            .expect("hnsw search must succeed");
        let hit = hnsw.iter().filter(|h| brute_ids.contains(&h.id)).count();
        hits_total += hit;
    }
    hits_total as f64 / (queries.len() as f64 * 10.0)
}

// --------------------------------------------------
// 層 A（常時 `#[test]`・小規模・debug 実行で数秒以内を目標）
// --------------------------------------------------

#[test]
fn recall_at_10_meets_threshold_on_small_fixture() {
    let dim = 32;
    let rows = 2_000;
    let vectors = gen_clustered_corpus(0xA5A5_1234_0000, dim, rows, 20);
    let params = HnswParams::default();
    let index = HnswIndex::build(params, dim as u32, &vectors, 0xF00D_1234).unwrap();
    let queries = gen_queries(0xA5A5_1234_0000, 0x51DE_0001, dim, 20, 100);

    let recall64 = recall_at_10(&index, &vectors, dim, rows, 64, &queries);
    assert!(
        recall64 >= 0.95,
        "Recall@10(ef=64) = {recall64} must be >= 0.95 (small fixture regression guard)"
    );

    let recall256 = recall_at_10(&index, &vectors, dim, rows, 256, &queries);
    assert!(
        recall256 >= 0.99,
        "Recall@10(ef=256) = {recall256} must be >= 0.99 (small fixture regression guard)"
    );
}

/// HNSW 構築の並列化（Issue #406）の受け入れ条件 (a): 並列構築版の Recall@10
/// が逐次構築版と同水準（`parallel >= sequential - 0.02`。実装計画 §6.3
/// 層 A）であることを、同一フィクスチャ・同一クエリ集合で確認する。
/// `n > SEQUENTIAL_PREFIX_NODES` を満たす規模でなければ並列フェーズが
/// 起動しない（`build_with_threads` が `build` へ縮退する）ため、行数を
/// 確保する。
#[test]
fn parallel_build_recall_at_10_matches_sequential_build_within_margin() {
    let dim = 32;
    let rows = SEQUENTIAL_PREFIX_NODES + 1_800;
    let vectors = gen_clustered_corpus(0xA5A5_1234_1111, dim, rows, 20);
    let params = HnswParams::default();
    let seed = 0xF00D_1234_5678;
    let queries = gen_queries(0xA5A5_1234_1111, 0x51DE_0004, dim, 20, 100);

    let sequential = HnswIndex::build(params, dim as u32, &vectors, seed).unwrap();
    let parallel = HnswIndex::build_with_threads(params, dim as u32, &vectors, seed, 4).unwrap();

    for ef in [64usize, 256] {
        let seq_recall = recall_at_10(&sequential, &vectors, dim, rows, ef, &queries);
        let par_recall = recall_at_10(&parallel, &vectors, dim, rows, ef, &queries);
        assert!(
            par_recall >= seq_recall - 0.02,
            "ef={ef} parallel Recall@10={par_recall} must be within 0.02 of \
             sequential Recall@10={seq_recall}"
        );
    }
}

// --------------------------------------------------
// 層 B（`#[ignore]`・受け入れ条件 (a) の正本。`make hnsw-search-recall` から
// release 実行する。debug では HnswIndex::build（10k×dim128）が数十秒〜約 110s
// かかるため常時 CI には含めない）
// --------------------------------------------------

#[test]
#[ignore]
fn recall_at_10_meets_threshold_on_large_fixture() {
    let dim = 128;
    let rows = 10_000;
    let vectors = gen_clustered_corpus(0xB6B6_5678_0000, dim, rows, 80);
    let params = HnswParams::default();
    let index = HnswIndex::build(params, dim as u32, &vectors, 0xC0DE_5678).unwrap();
    let queries = gen_queries(0xB6B6_5678_0000, 0x51DE_0002, dim, 80, 200);

    let recall64 = recall_at_10(&index, &vectors, dim, rows, 64, &queries);
    println!("hnsw_search_recall: ef=64 Recall@10={recall64:.4}");
    assert!(
        recall64 >= 0.95,
        "Recall@10(ef=64) = {recall64} must be >= 0.95 (Issue #405 accept condition (a))"
    );

    let recall256 = recall_at_10(&index, &vectors, dim, rows, 256, &queries);
    println!("hnsw_search_recall: ef=256 Recall@10={recall256:.4}");
    assert!(
        recall256 >= 0.99,
        "Recall@10(ef=256) = {recall256} must be >= 0.99 (Issue #405 accept condition (a))"
    );

    // 一様乱数のみのコーパス・クエリ（HNSW にとって最難条件）は informational
    // な参考値としてのみ出力する（3.3 節。アサーションなし。受け入れ判定は
    // 上記のクラスタ構造ありフィクスチャが正本）。
    let uniform_vectors = gen_uniform_corpus(0xD00D_9ABC_0000, dim, rows);
    let uniform_index = HnswIndex::build(
        HnswParams::default(),
        dim as u32,
        &uniform_vectors,
        0xE0E0_9ABC,
    )
    .unwrap();
    let uniform_queries = gen_uniform_queries(0x51DE_0003, dim, 200);
    let uniform_recall64 = recall_at_10(
        &uniform_index,
        &uniform_vectors,
        dim,
        rows,
        64,
        &uniform_queries,
    );
    println!("hnsw_search_recall: [informational] uniform-random corpus ef=64 Recall@10={uniform_recall64:.4}");
    let uniform_recall256 = recall_at_10(
        &uniform_index,
        &uniform_vectors,
        dim,
        rows,
        256,
        &uniform_queries,
    );
    println!(
        "hnsw_search_recall: [informational] uniform-random corpus ef=256 Recall@10={uniform_recall256:.4}"
    );
}

// --------------------------------------------------
// 決定性テスト（受け入れ条件 (b)）
// --------------------------------------------------

#[test]
fn search_is_deterministic_across_repeated_calls_and_fresh_scratch() {
    let dim = 16;
    let rows = 500;
    let vectors = gen_clustered_corpus(0x1357_9BDF, dim, rows, 10);
    let index = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 0x2468_ACE0).unwrap();
    let query = gen_query(0x1357_9BDF, 0x0BAD_F00D, dim, 10);

    let mut scratch = HnswSearchScratch::default();
    let first = index.search(&query, 10, 64, &mut scratch).unwrap();
    for _ in 0..2 {
        let again = index.search(&query, 10, 64, &mut scratch).unwrap();
        assert_eq!(
            first, again,
            "同一スクラッチでの反復呼び出しは完全一致するはず"
        );
    }

    let mut fresh_scratch = HnswSearchScratch::default();
    let with_fresh_scratch = index.search(&query, 10, 64, &mut fresh_scratch).unwrap();
    assert_eq!(
        first, with_fresh_scratch,
        "新規スクラッチでも結果は同一（スクラッチ状態に非依存）であるべき"
    );

    let rebuilt =
        HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 0x2468_ACE0).unwrap();
    let mut rebuilt_scratch = HnswSearchScratch::default();
    let with_rebuilt_index = rebuilt
        .search(&query, 10, 64, &mut rebuilt_scratch)
        .unwrap();
    assert_eq!(
        first, with_rebuilt_index,
        "同一 seed で再構築した索引でも結果は同一であるべき"
    );
}

/// 重複ヘビーコーパス（`tests/hnsw.rs::gen_duplicate_heavy_corpus` と同型。同点
/// スコアが多発する）でも決定性が保たれ、結果内の同点が id 昇順であることを
/// 検証する。
#[test]
fn search_is_deterministic_and_tie_ordered_on_duplicate_heavy_corpus() {
    let dim = 8;
    let rows = 400;
    let clusters = 5;
    // クラスタ中心のみを row として複製する（ジッタを加えないため厳密な同点
    // スコアが多発する）。
    let mut center_rng = TestRng::new(0x4242_4242);
    let centers: Vec<Vec<f32>> = (0..clusters)
        .map(|_| {
            let mut v: Vec<f32> = (0..dim).map(|_| center_rng.next_unit()).collect();
            normalize(&mut v);
            v
        })
        .collect();
    let mut vectors = Vec::with_capacity(rows * dim);
    for i in 0..rows {
        vectors.extend(centers[i % clusters].iter().copied());
    }

    let index = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 0x7777_1111).unwrap();
    let query = centers[0].clone();

    let mut scratch = HnswSearchScratch::default();
    let first = index.search(&query, 10, 64, &mut scratch).unwrap();
    let again = index.search(&query, 10, 64, &mut scratch).unwrap();
    assert_eq!(
        first, again,
        "重複ヘビーコーパスでも反復呼び出しは完全一致するはず"
    );

    for pair in first.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        assert!(
            a.score > b.score || (a.score == b.score && a.id < b.id),
            "結果はスコア降順・同点は id 昇順であるべき: a={a:?} b={b:?}"
        );
    }
}

// --------------------------------------------------
// 不変条件・境界テスト
// --------------------------------------------------

fn small_index() -> (Vec<f32>, HnswIndex) {
    let dim = 8;
    let rows = 200;
    let vectors = gen_clustered_corpus(0x9999_0000, dim, rows, 8);
    let index = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 0x1234_5678).unwrap();
    (vectors, index)
}

#[test]
fn search_result_length_and_id_bounds_and_score_order_hold() {
    let (_, index) = small_index();
    let dim = 8;
    let rows = 200;
    let query = gen_query(0x9999_0000, 0xAAAA_1111, dim, 8);
    let mut scratch = HnswSearchScratch::default();
    let results = index.search(&query, 10, 64, &mut scratch).unwrap();

    assert!(results.len() <= 10);
    let mut seen = HashSet::new();
    for hit in &results {
        assert!((hit.id as usize) < rows, "id must be < index len");
        assert!(
            seen.insert(hit.id),
            "results must not contain duplicate ids"
        );
    }
    for pair in results.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "results must be sorted by score descending"
        );
    }
}

#[test]
fn search_with_k_greater_than_ef_uses_effective_ef_and_returns_k_hits() {
    let (_, index) = small_index();
    let dim = 8;
    let query = gen_query(0x9999_0000, 0xBBBB_2222, dim, 8);
    let mut scratch = HnswSearchScratch::default();
    // k=20 > ef=1: 実効 ef は `ef.max(k)` へ引き上げられるため k 件返るはず。
    let results = index.search(&query, 20, 1, &mut scratch).unwrap();
    assert_eq!(results.len(), 20);
}

#[test]
fn search_with_k_zero_returns_empty() {
    let (_, index) = small_index();
    let dim = 8;
    let query = gen_query(0x9999_0000, 0xCCCC_3333, dim, 8);
    let mut scratch = HnswSearchScratch::default();
    let results = index.search(&query, 0, 64, &mut scratch).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_on_empty_index_returns_empty() {
    let dim = 8;
    let index = HnswIndex::build(HnswParams::default(), dim as u32, &[], 1).unwrap();
    let query = gen_query(0xDDDD_4444, 0xDDDD_4444, dim, 1);
    let mut scratch = HnswSearchScratch::default();
    let results = index.search(&query, 10, 64, &mut scratch).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_rejects_query_dim_mismatch() {
    let (_, index) = small_index();
    let mut scratch = HnswSearchScratch::default();
    let wrong_dim_query = vec![0.0f32; 4]; // index dim は 8
    let err = index
        .search(&wrong_dim_query, 10, 64, &mut scratch)
        .unwrap_err();
    assert_eq!(
        err,
        HnswError::QueryDimMismatch {
            expected: 8,
            found: 4
        }
    );
}

#[test]
fn search_rejects_non_finite_query() {
    let (_, index) = small_index();
    let mut scratch = HnswSearchScratch::default();
    let mut query = gen_query(0x9999_0000, 0xEEEE_5555, 8, 8);
    query[0] = f32::NAN;
    let err = index.search(&query, 10, 64, &mut scratch).unwrap_err();
    assert_eq!(err, HnswError::NonFiniteQuery);
}

#[test]
fn search_rejects_ef_zero() {
    let (_, index) = small_index();
    let query = gen_query(0x9999_0000, 0x1111_7777, 8, 8);
    let mut scratch = HnswSearchScratch::default();
    let err = index.search(&query, 10, 0, &mut scratch).unwrap_err();
    assert!(matches!(err, HnswError::InvalidParams { .. }));
}

#[test]
fn search_rejects_ef_beyond_max_ef() {
    let (_, index) = small_index();
    let query = gen_query(0x9999_0000, 0x2222_8888, 8, 8);
    let mut scratch = HnswSearchScratch::default();
    let err = index
        .search(&query, 10, MAX_EF + 1, &mut scratch)
        .unwrap_err();
    assert!(matches!(err, HnswError::InvalidParams { .. }));
}

#[test]
fn search_rejects_k_beyond_max_ef() {
    let (_, index) = small_index();
    let query = gen_query(0x9999_0000, 0x3333_9999, 8, 8);
    let mut scratch = HnswSearchScratch::default();
    let err = index
        .search(&query, MAX_EF + 1, 64, &mut scratch)
        .unwrap_err();
    assert!(matches!(err, HnswError::InvalidParams { .. }));
}

/// 並列構築版の索引に対する探索も、逐次構築版と同じ決定性契約（同一索引・
/// 同一クエリ・任意のスクラッチ状態で結果が完全一致する）を満たすことを
/// 確認する（並列構築が変えるのはグラフの**構築時の形状**のみで、構築後の
/// 探索の決定性は構築方式に依らず不変。実装計画 §6.3）。
#[test]
fn search_on_parallel_built_index_is_deterministic_across_repeated_calls() {
    let dim = 16;
    let rows = SEQUENTIAL_PREFIX_NODES + 400;
    let vectors = gen_clustered_corpus(0x1357_9BDF_2222, dim, rows, 10);
    let index =
        HnswIndex::build_with_threads(HnswParams::default(), dim as u32, &vectors, 0x2468_ACE1, 4)
            .unwrap();
    let query = gen_query(0x1357_9BDF_2222, 0x0BAD_F00E, dim, 10);

    let mut scratch = HnswSearchScratch::default();
    let first = index.search(&query, 10, 64, &mut scratch).unwrap();
    for _ in 0..2 {
        let again = index.search(&query, 10, 64, &mut scratch).unwrap();
        assert_eq!(first, again);
    }
    let mut fresh_scratch = HnswSearchScratch::default();
    let with_fresh_scratch = index.search(&query, 10, 64, &mut fresh_scratch).unwrap();
    assert_eq!(first, with_fresh_scratch);
}
