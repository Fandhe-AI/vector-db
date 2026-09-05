//! `benches/harness/hnsw_compare.rs`（自作 HNSW と usearch の構築時間・Recall・
//! 探索レイテンシ比較。Issue #402 系 ADR の実測補強）の回帰テスト。
//!
//! `hnsw_compare_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hnsw_build_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。usearch に依存しない部分（env 解析・速度比・Recall 集計・行分割・
//! 出力整形）は feature 無指定でも検証し、usearch 依存部分
//! （`usearch_adapter` モジュール）は `contrast-bench` feature 限定で検証する
//! （`tests/contrast_smoke.rs` と同一方針。`make test` は `--all-features` の
//! ため両方回る）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::hnsw_compare::{
    average_recall_at_k, parse_dim, parse_queries, parse_rows, parse_thread_ladder, partition_rows,
    ratio_self_over_usearch, refuse_under_github_actions, render_build_line, render_header_line,
    render_latency_line, render_ratio_line, render_recall_line, render_self_params_line,
    render_usearch_params_line, speedup, HnswCompareBenchError, ALLOWED_DIMS, DEFAULT_DIM,
    DEFAULT_QUERIES, DEFAULT_ROWS, MAX_QUERIES_GUARD, MAX_ROWS_GUARD,
};
use std::time::Duration;

// --- refuse_under_github_actions ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- parse_rows ---

#[test]
fn parse_rows_accepts_valid_value() {
    assert_eq!(parse_rows(Some("12345")), 12345);
}

#[test]
fn parse_rows_falls_back_to_default_on_missing() {
    assert_eq!(parse_rows(None), DEFAULT_ROWS);
}

#[test]
fn parse_rows_falls_back_to_default_on_invalid() {
    assert_eq!(parse_rows(Some("not-a-number")), DEFAULT_ROWS);
}

#[test]
fn parse_rows_falls_back_to_default_on_zero() {
    assert_eq!(parse_rows(Some("0")), DEFAULT_ROWS);
}

#[test]
fn parse_rows_falls_back_to_default_on_guard_overflow() {
    let overflow = (MAX_ROWS_GUARD + 1).to_string();
    assert_eq!(parse_rows(Some(&overflow)), DEFAULT_ROWS);
}

#[test]
fn parse_rows_accepts_guard_boundary() {
    let boundary = MAX_ROWS_GUARD.to_string();
    assert_eq!(parse_rows(Some(&boundary)), MAX_ROWS_GUARD);
}

// --- parse_dim ---

#[test]
fn parse_dim_accepts_allowed_values() {
    for &dim in &ALLOWED_DIMS {
        assert_eq!(parse_dim(Some(&dim.to_string())), dim);
    }
}

#[test]
fn parse_dim_falls_back_to_default_on_missing() {
    assert_eq!(parse_dim(None), DEFAULT_DIM);
}

#[test]
fn parse_dim_falls_back_to_default_on_disallowed_value() {
    assert_eq!(parse_dim(Some("96")), DEFAULT_DIM);
}

// --- parse_queries ---

#[test]
fn parse_queries_accepts_valid_value() {
    assert_eq!(parse_queries(Some("50")), 50);
}

#[test]
fn parse_queries_falls_back_to_default_on_missing() {
    assert_eq!(parse_queries(None), DEFAULT_QUERIES);
}

#[test]
fn parse_queries_falls_back_to_default_on_guard_overflow() {
    let overflow = (MAX_QUERIES_GUARD + 1).to_string();
    assert_eq!(parse_queries(Some(&overflow)), DEFAULT_QUERIES);
}

// --- parse_thread_ladder ---

#[test]
fn parse_thread_ladder_uses_explicit_list_when_valid() {
    let ladder = parse_thread_ladder(Some("4,1,2"), 16, 8);
    assert_eq!(ladder, vec![1, 2, 4]);
}

#[test]
fn parse_thread_ladder_dedups_and_sorts() {
    let ladder = parse_thread_ladder(Some("2,2,1,1"), 16, 8);
    assert_eq!(ladder, vec![1, 2]);
}

#[test]
fn parse_thread_ladder_drops_values_exceeding_max() {
    let ladder = parse_thread_ladder(Some("1,2,99"), 16, 8);
    assert_eq!(ladder, vec![1, 2]);
}

#[test]
fn parse_thread_ladder_falls_back_to_default_ladder_when_all_invalid() {
    let ladder = parse_thread_ladder(Some("0,99,abc"), 16, 8);
    assert_eq!(ladder, vec![1, 2, 4, 8]);
}

#[test]
fn parse_thread_ladder_falls_back_to_default_ladder_when_missing() {
    let ladder = parse_thread_ladder(None, 16, 8);
    assert_eq!(ladder, vec![1, 2, 4, 8]);
}

#[test]
fn parse_thread_ladder_clamps_default_ladder_to_max_threads() {
    let ladder = parse_thread_ladder(None, 3, 16);
    assert_eq!(ladder.last(), Some(&3));
    assert!(ladder.iter().all(|&t| t <= 3));
}

#[test]
fn parse_thread_ladder_single_core_yields_singleton() {
    let ladder = parse_thread_ladder(None, 16, 1);
    assert_eq!(ladder, vec![1]);
}

// --- speedup ---

#[test]
fn speedup_without_baseline_is_one() {
    let sp = speedup(None, Duration::from_millis(10)).unwrap();
    assert!((sp - 1.0).abs() < f64::EPSILON);
}

#[test]
fn speedup_with_baseline_reflects_ratio() {
    let sp = speedup(Some(Duration::from_millis(100)), Duration::from_millis(25)).unwrap();
    assert!((sp - 4.0).abs() < 1e-9);
}

#[test]
fn speedup_rejects_zero_current() {
    let err = speedup(Some(Duration::from_millis(100)), Duration::ZERO).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::InsufficientSamples);
}

// --- ratio_self_over_usearch ---

#[test]
fn ratio_self_over_usearch_computes_expected_value() {
    let ratio =
        ratio_self_over_usearch(Duration::from_millis(50), Duration::from_millis(25)).unwrap();
    assert!((ratio - 2.0).abs() < 1e-9);
}

#[test]
fn ratio_self_over_usearch_rejects_zero_usearch_median() {
    let err = ratio_self_over_usearch(Duration::from_millis(50), Duration::ZERO).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::InsufficientSamples);
}

// --- average_recall_at_k ---

#[test]
fn average_recall_at_k_matches_manual_average() {
    let per_query = vec![
        (vec![1, 2, 3], vec![1, 2, 3]),  // 1.0
        (vec![1, 2, 3], vec![1, 2, 99]), // 2/3
    ];
    let avg = average_recall_at_k(&per_query).unwrap();
    assert!((avg - (1.0 + 2.0 / 3.0) / 2.0).abs() < 1e-9);
}

#[test]
fn average_recall_at_k_rejects_empty_input() {
    let err = average_recall_at_k(&[]).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::InsufficientSamples);
}

// --- partition_rows ---

#[test]
fn partition_rows_covers_all_rows_without_gaps_or_overlap() {
    let ranges = partition_rows(10, 3);
    assert_eq!(ranges, vec![(0, 4), (4, 7), (7, 10)]);
}

#[test]
fn partition_rows_single_thread_covers_everything() {
    let ranges = partition_rows(7, 1);
    assert_eq!(ranges, vec![(0, 7)]);
}

#[test]
fn partition_rows_zero_threads_yields_empty() {
    let ranges = partition_rows(10, 0);
    assert!(ranges.is_empty());
}

#[test]
fn partition_rows_more_threads_than_rows_yields_empty_tail_ranges() {
    let ranges = partition_rows(2, 5);
    assert_eq!(ranges.len(), 5);
    let total: usize = ranges.iter().map(|&(s, e)| e - s).sum();
    assert_eq!(total, 2);
    // 空範囲（`s == e`）が含まれてよいが、範囲同士は連続していること。
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].1, pair[1].0);
    }
}

// --- render_* (出力整形のスモーク。フォーマット崩れの回帰検出) ---

#[test]
fn render_lines_contain_expected_tokens() {
    let build = render_build_line("self", 4, Duration::from_millis(12), 2.5);
    assert!(build.contains("engine=self"));
    assert!(build.contains("threads=4"));

    let ratio = render_ratio_line(4, 1.25);
    assert!(ratio.contains("self_over_usearch=1.250x"));

    let recall = render_recall_line("usearch", 8, 0.9876);
    assert!(recall.contains("recall@10=0.9876"));

    let latency = render_latency_line("self", Duration::from_micros(42));
    assert!(latency.contains("median_us=42"));

    let header = render_header_line(1000, 64, 50, &[1, 2, 4]);
    assert!(header.contains("rows=1000"));
    assert!(header.contains("dim=64"));
    assert!(header.contains("queries=50"));

    let self_params = render_self_params_line(16, 100, 64);
    assert!(self_params.contains("engine=self"));
    assert!(self_params.contains("m=16"));

    let usearch_params = render_usearch_params_line(16, 100, 64);
    assert!(usearch_params.contains("engine=usearch"));
    assert!(usearch_params.contains("connectivity=16"));
}

// --------------------------------------------------
// usearch 依存部分（`contrast-bench` feature 限定）
// --------------------------------------------------

#[cfg(feature = "contrast-bench")]
mod usearch_adapter_tests {
    use super::harness::hnsw_compare::usearch_adapter::{
        build_usearch_index_parallel, usearch_index_options, usearch_search_topk,
    };
    use super::harness::rng::DeterministicRng;
    use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

    const ROWS: usize = 300;
    const DIM: usize = 16;
    const TOP_K: usize = 5;

    #[test]
    fn usearch_index_options_reflects_engine_defaults() {
        let options = usearch_index_options(DIM);
        assert_eq!(options.dimensions, DIM);
        assert_eq!(options.connectivity, 16);
        assert_eq!(options.expansion_add, 100);
        assert_eq!(options.expansion_search, 64);
        assert!(!options.multi);
    }

    #[test]
    fn build_usearch_index_parallel_with_multiple_threads_matches_single_thread_recall() {
        let mut rng = DeterministicRng::new(11);
        let mut vectors = Vec::with_capacity(ROWS * DIM);
        for _ in 0..ROWS {
            vectors.extend(rng.next_vector(DIM));
        }
        let query = rng.next_vector(DIM);

        let single = build_usearch_index_parallel(ROWS, DIM, &vectors, 1)
            .expect("single-threaded usearch build succeeds");
        let parallel = build_usearch_index_parallel(ROWS, DIM, &vectors, 4)
            .expect("parallel usearch build succeeds");

        assert_eq!(single.size(), ROWS);
        assert_eq!(parallel.size(), ROWS);

        let ids: Vec<u64> = (0..ROWS as u64).collect();
        let reference = CpuScalarProvider
            .search(SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: DIM as u32,
                query: &query,
                k: TOP_K,
            })
            .expect("reference search succeeds");
        let expected: Vec<u64> = reference.into_iter().map(|hit| hit.id).collect();

        let single_topk =
            usearch_search_topk(&single, &query, TOP_K).expect("single-threaded search succeeds");
        let parallel_topk =
            usearch_search_topk(&parallel, &query, TOP_K).expect("parallel search succeeds");

        // usearch は近似探索のため厳密一致は要求しないが、この小規模フィクスチャ
        // （300 点・16 次元）では上位 5 件が brute-force 対照とほぼ一致する
        // ことを配線確認として使う（`contrast_smoke.rs` と異なり `search`
        // は近似探索であるため `recall == 1.0` は要求しない）。
        let single_hits = single_topk
            .iter()
            .filter(|id| expected.contains(id))
            .count();
        let parallel_hits = parallel_topk
            .iter()
            .filter(|id| expected.contains(id))
            .count();
        assert!(
            single_hits >= TOP_K - 1,
            "single-threaded usearch top-{TOP_K} should closely match brute-force reference"
        );
        assert!(
            parallel_hits >= TOP_K - 1,
            "parallel usearch top-{TOP_K} should closely match brute-force reference"
        );
    }

    #[test]
    fn build_usearch_index_parallel_rejects_thread_count_wider_than_rows_gracefully() {
        let mut rng = DeterministicRng::new(3);
        let mut vectors = Vec::with_capacity(4 * DIM);
        for _ in 0..4 {
            vectors.extend(rng.next_vector(DIM));
        }
        // rows(4) < threads(8) でも一部ワーカーの担当範囲が空になるだけで
        // 失敗しないことを確認する（`partition_rows` が空範囲を許すため）。
        let index = build_usearch_index_parallel(4, DIM, &vectors, 8)
            .expect("build with more threads than rows should still succeed");
        assert_eq!(index.size(), 4);
    }
}
