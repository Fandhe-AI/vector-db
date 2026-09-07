//! `tests/fixtures/recall_engine.rs`（Issue #412）自身の層 A 検証。
//!
//! 3 つの Recall 回帰ハーネスの層 B が ANN opt-in（[`RecallEngine::Hnsw`]）で
//! 測定する前に、この fixture 自体が満たすべき前提を固定する:
//!
//! 1. `RecallEngine::from_env` の受理／拒否契約（純関数 `parse` は
//!    `tests/fixtures/recall_engine.rs` 内の `#[cfg(test)]` で直接検証済み。
//!    ここでは環境変数を経由した公開エントリポイントとしての形だけ触れる）。
//! 2. **測定妥当性ガード**（Issue #412 設計判断 3）: 「SQL 表層 + 既定エンジン」
//!    の Top-k id 列が「in-memory `engine::hybrid::hybrid_search`」の id 列と
//!    完全一致すること。これが崩れると、ANN 有効時の Recall 差分がエンジンの
//!    違いではなく SQL 表層自体の違いに起因する可能性を排除できない。
//! 3. **非 vacuous ガード**（Issue #412 設計判断 4）:
//!    `MIN_INDEXED_ROWS`（1,024）以上のコーパスでは HNSW 索引が実際に構築され
//!    hybrid 密側再取得ループが索引経路を通ること、未満では索引が一切
//!    構築されないこと。

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::sparse::SparseIndex;

// `recall_engine` fixture（下記）が `super::temp_db` を参照するため、取り込み側
// である本ファイルでクレートルートに 1 回だけ宣言する（`rerank_recall.rs` と
// 同じ理由・同じ取り込み方式。`recall_engine.rs` 自身が `mod temp_db` を宣言
// すると同一物理ファイルの二重 `mod` になり `clippy::duplicate_mod` に抵触する
// ため、宣言はここへ一本化する）。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;

#[path = "fixtures/recall_engine.rs"]
mod recall_engine;
use recall_engine::{RecallEngine, SqlHybridFixture};

// ---------- 決定的擬似乱数（xorshift64*。`tests/hnsw_cache.rs::TestRng` の複製。
// `tests/` 直下は独立 test crate で共有モジュールを持たないためこのファイルへ
// 複製する） ----------

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

/// クラスタ構造を持つ、L2 正規化済みの決定的コーパス（`tests/hnsw_cache.rs::
/// gen_clustered_corpus` と同型の複製）。各文書のクラスタ語彙を `body` へ
/// 埋め込み、密・疎の両チャネルが同じクラスタへ一致するようにする
/// （`tests/hnsw_cache.rs::hybrid_queries_use_hnsw_dense_provider_and_match_
/// default_engine_recall` と同じ設計方針）。
fn gen_clustered_corpus(
    seed: u64,
    dim: usize,
    rows: usize,
    clusters: usize,
) -> Vec<(u64, Vec<f32>, String)> {
    let mut center_rng = TestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(seed);
    (0..rows)
        .map(|i| {
            let cluster = i % centers.len();
            let center = &centers[cluster];
            let mut v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
            normalize(&mut v);
            let body = format!("clusterword{cluster} filler text for recall engine fixture");
            (i as u64 + 1, v, body)
        })
        .collect()
}

#[test]
fn from_env_defaults_to_brute_force_when_unset() {
    // プロセス環境変数を直接操作するテストは他の並列テストと競合しうるため、
    // 実際の判定ロジックは純関数 `parse`（`recall_engine.rs` 内の
    // `#[cfg(test)]` で網羅検証済み）へ委譲し、ここでは「未設定時に
    // `from_env()` が panic しない」という公開エントリポイントの形だけ固定する
    // （並行実行下で env var を書き換える他のテストは無いことを前提に、この
    // ファイル自身では env var を変更しない）。
    if std::env::var_os("RECALL_ENGINE").is_none() {
        assert_eq!(RecallEngine::from_env(), RecallEngine::BruteForce);
    }
}

/// 測定妥当性ガード: 「SQL 表層 + 既定エンジン」の hybrid クエリ結果（id 順序）が
/// 「in-memory `hybrid_search`」の結果と完全一致することを固定する。ANN
/// （[`RecallEngine::Hnsw`]）と既定エンジンの差分測定が、SQL 表層自体の違いでは
/// なく検索エンジンの違いにのみ起因することの前提を担保する。
#[test]
fn sql_default_engine_hybrid_top_matches_in_memory_hybrid_search() {
    const DIM: usize = 8;
    const ROWS: usize = 40;
    const CLUSTERS: usize = 4;
    const K: usize = 10;

    let rows = gen_clustered_corpus(1, DIM, ROWS, CLUSTERS);
    let fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::BruteForce);
    // 未索引規模（`MIN_INDEXED_ROWS` 未満）でも `BruteForce` エンジン自体は
    // `HnswIndexCache` を一切経由しないため、常に `builds == 0`。
    fixture.assert_ann_non_vacuous(false);

    // in-memory 側（`ParallelSearchProvider` ＋ `SparseIndex::build` ＋
    // `hybrid_search`。`RrfConfig::default()` = SQL 表層の `Ranking::Hybrid`
    // 分岐と同一構成〔`k_const 60・等重み・pool_depth 200`〕）。
    let ids: Vec<u64> = rows.iter().map(|(id, _, _)| *id).collect();
    let vectors: Vec<f32> = rows
        .iter()
        .flat_map(|(_, v, _)| v.iter().copied())
        .collect();
    let refs: Vec<(u64, &str)> = rows.iter().map(|(id, _, t)| (*id, t.as_str())).collect();
    let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");
    let provider = ParallelSearchProvider;
    let cfg = RrfConfig::default();

    for (cluster, (_, query_vec, _)) in rows.iter().enumerate().take(CLUSTERS) {
        let query_vec = query_vec.clone();
        let query_text = format!("clusterword{cluster}");

        let got = fixture.hybrid_top(&query_vec, &query_text, K);
        let got_ids: Vec<u64> = got.iter().map(|(id, _)| *id).collect();

        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &query_vec,
            k: K,
        };
        let want = hybrid_search(&provider, input, &sparse_index, &query_text, K, &cfg)
            .expect("hybrid_search ok");
        let want_ids: Vec<u64> = want.iter().map(|h| h.id).collect();

        assert_eq!(
            got_ids, want_ids,
            "SQL surface + default engine must match in-memory hybrid_search exactly (cluster={cluster})"
        );
    }
}

/// 非 vacuous ガード（正）: `MIN_INDEXED_ROWS`（1,024）以上のコーパスでは
/// `RecallEngine::Hnsw` が実際に索引を構築し、hybrid 密側再取得ループが索引
/// 経路を通ること。行数は `build_parallel` の並列化閾値（`MIN_ROWS_PER_THREAD *
/// 2` = 2,048）未満に収め、構築グラフを決定的に保つ（`tests/hnsw_cache.rs`
/// の fixture 方針と同じ）。
#[test]
fn hnsw_engine_builds_index_at_or_above_min_indexed_rows() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const CLUSTERS: usize = 6;

    let rows = gen_clustered_corpus(2, DIM, ROWS, CLUSTERS);
    let fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::Hnsw);

    let query_vec = rows[0].1.clone();
    let got = fixture.hybrid_top(&query_vec, "clusterword0", 10);
    assert!(!got.is_empty(), "expected non-empty hybrid result");
    fixture.assert_ann_non_vacuous(true);
}

/// 非 vacuous ガード（負）: `MIN_INDEXED_ROWS` 未満のコーパスは
/// `RecallEngine::Hnsw` を指定しても索引を構築しない（構造的に brute-force。
/// `hybrid_recall.rs` 小規模段〔400 docs〕と同じ規模）。この段を ANN 通過と
/// 誤って数えないことを、層 B 側もこの契約を前提に判定する。
#[test]
fn hnsw_engine_does_not_build_index_below_min_indexed_rows() {
    const DIM: usize = 16;
    const ROWS: usize = 400;
    const CLUSTERS: usize = 6;

    let rows = gen_clustered_corpus(3, DIM, ROWS, CLUSTERS);
    let fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::Hnsw);

    let query_vec = rows[0].1.clone();
    let got = fixture.hybrid_top(&query_vec, "clusterword0", 10);
    assert!(!got.is_empty(), "expected non-empty hybrid result");
    fixture.assert_ann_non_vacuous(false);
}

/// 非 vacuous ガード（正・F16 常駐。Issue #515）:
/// `RecallEngine::HnswF16` でも `MIN_INDEXED_ROWS` 以上のコーパスでは実際に
/// 索引が構築され、`resident=f16`（`search_engine_kind()` の Display）で
/// 自動縮退（D6）が発生していないことを固定する。
#[test]
fn hnsw_f16_engine_builds_index_at_or_above_min_indexed_rows() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const CLUSTERS: usize = 6;

    let rows = gen_clustered_corpus(4, DIM, ROWS, CLUSTERS);
    let fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::HnswF16);

    let query_vec = rows[0].1.clone();
    let got = fixture.hybrid_top(&query_vec, "clusterword0", 10);
    assert!(!got.is_empty(), "expected non-empty hybrid result");
    fixture.assert_ann_non_vacuous(true);
}

/// 非 vacuous ガード（負・F16 常駐）: `MIN_INDEXED_ROWS` 未満のコーパスは
/// `RecallEngine::HnswF16` を指定しても索引を構築しない。
#[test]
fn hnsw_f16_engine_does_not_build_index_below_min_indexed_rows() {
    const DIM: usize = 16;
    const ROWS: usize = 400;
    const CLUSTERS: usize = 6;

    let rows = gen_clustered_corpus(5, DIM, ROWS, CLUSTERS);
    let fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::HnswF16);

    let query_vec = rows[0].1.clone();
    let got = fixture.hybrid_top(&query_vec, "clusterword0", 10);
    assert!(!got.is_empty(), "expected non-empty hybrid result");
    fixture.assert_ann_non_vacuous(false);
}

/// F16 常駐（候補生成が f16→f32 昇格 dot になり探索順序が変わり得る）でも
/// F32 常駐と同一コーパス・同一クエリで Top-20 の重なりが高い（Recall
/// tolerance 0.9 以上）ことを固定する（候補順序差の許容。完全一致は要求しない。
/// 最終スコアは常に `kernel::dot` の f32 再計算——#408 契約——のため id 集合の
/// 差は探索経路のみに起因する）。
#[test]
fn hnsw_f16_engine_top_ids_match_hnsw_within_recall_tolerance() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const CLUSTERS: usize = 6;
    const K: usize = 20;

    let rows = gen_clustered_corpus(6, DIM, ROWS, CLUSTERS);
    let f32_fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::Hnsw);
    let f16_fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::HnswF16);

    let query_vec = rows[0].1.clone();
    let f32_top = f32_fixture.hybrid_top(&query_vec, "clusterword0", K);
    let f16_top = f16_fixture.hybrid_top(&query_vec, "clusterword0", K);
    assert!(!f32_top.is_empty() && !f16_top.is_empty());

    let f32_ids: std::collections::HashSet<u64> = f32_top.iter().map(|(id, _)| *id).collect();
    let overlap = f16_top
        .iter()
        .filter(|(id, _)| f32_ids.contains(id))
        .count();
    let recall = overlap as f64 / f32_ids.len() as f64;
    assert!(
        recall >= 0.9,
        "expected hnsw_f16 top-{K} to overlap hnsw (f32) top-{K} by at least 0.9, got \
         {recall} (overlap={overlap}, f32_ids={})",
        f32_ids.len()
    );
}

/// 非 vacuous ガード（正・I8 常駐。Issue #523）:
/// `RecallEngine::HnswI8` でも `MIN_INDEXED_ROWS` 以上のコーパスでは実際に
/// 索引が構築され、`resident=i8`（`search_engine_kind()` の Display）で
/// 自動縮退（D6）が発生していないことを固定する（f16 版と同型）。
#[test]
fn hnsw_i8_engine_builds_index_at_or_above_min_indexed_rows() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const CLUSTERS: usize = 6;

    let rows = gen_clustered_corpus(4, DIM, ROWS, CLUSTERS);
    let fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::HnswI8);

    let query_vec = rows[0].1.clone();
    let got = fixture.hybrid_top(&query_vec, "clusterword0", 10);
    assert!(!got.is_empty(), "expected non-empty hybrid result");
    fixture.assert_ann_non_vacuous(true);
}

/// 非 vacuous ガード（負・I8 常駐）: `MIN_INDEXED_ROWS` 未満のコーパスは
/// `RecallEngine::HnswI8` を指定しても索引を構築しない。
#[test]
fn hnsw_i8_engine_does_not_build_index_below_min_indexed_rows() {
    const DIM: usize = 16;
    const ROWS: usize = 400;
    const CLUSTERS: usize = 6;

    let rows = gen_clustered_corpus(5, DIM, ROWS, CLUSTERS);
    let fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::HnswI8);

    let query_vec = rows[0].1.clone();
    let got = fixture.hybrid_top(&query_vec, "clusterword0", 10);
    assert!(!got.is_empty(), "expected non-empty hybrid result");
    fixture.assert_ann_non_vacuous(false);
}

/// I8 常駐（候補生成が対称 SQ8 量子化の整数 dot になり探索順序が変わり得る）
/// でも F32 常駐と同一コーパス・同一クエリで Top-20 の重なりが一定水準
/// 以上であることを固定する。I8 は 8-bit 量子化ぶん F16（ほぼ無損失な半精度）
/// より探索順序への影響が大きいため、`hnsw_cache.rs::min_recall_for` と同じ
/// 判断（Issue #523）で許容下限を 0.8 とする（本リポ独自の実装既定値。
/// 最終スコアは常に `kernel::dot` の f32 再計算のため id 集合の差は探索経路
/// のみに起因する）。
#[test]
fn hnsw_i8_engine_top_ids_match_hnsw_within_recall_tolerance() {
    const DIM: usize = 16;
    const ROWS: usize = 1_200;
    const CLUSTERS: usize = 6;
    const K: usize = 20;

    let rows = gen_clustered_corpus(6, DIM, ROWS, CLUSTERS);
    let f32_fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::Hnsw);
    let i8_fixture = SqlHybridFixture::new(DIM as u32, &rows, RecallEngine::HnswI8);

    let query_vec = rows[0].1.clone();
    let f32_top = f32_fixture.hybrid_top(&query_vec, "clusterword0", K);
    let i8_top = i8_fixture.hybrid_top(&query_vec, "clusterword0", K);
    assert!(!f32_top.is_empty() && !i8_top.is_empty());

    let f32_ids: std::collections::HashSet<u64> = f32_top.iter().map(|(id, _)| *id).collect();
    let overlap = i8_top.iter().filter(|(id, _)| f32_ids.contains(id)).count();
    let recall = overlap as f64 / f32_ids.len() as f64;
    assert!(
        recall >= 0.8,
        "expected hnsw_i8 top-{K} to overlap hnsw (f32) top-{K} by at least 0.8, got \
         {recall} (overlap={overlap}, f32_ids={})",
        f32_ids.len()
    );
}
