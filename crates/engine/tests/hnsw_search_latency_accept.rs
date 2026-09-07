//! `benches/harness/hnsw_search_latency.rs`（Issue #491。受理判定後 prefetch
//! 〔Issue #490・PR #574〕の前後比較実測が使う入力生成・出力整形ヘルパ）の
//! 回帰テスト。
//!
//! `hnsw_search_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hnsw_compare_accept.rs`・`tests/dot_kernel_accept.rs` と同様、
//! 実測タイマー・env に依存しない時間非依存の契約のみを `#[path]` で取り込み
//! `cargo test`〔`make ci` 対象〕で検証する）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::hnsw_search_latency::{
    generate_corpus, generate_mask, generate_query, parse_dim, parse_ef, parse_k, parse_mask,
    parse_queries, parse_rows, reference_band, refuse_under_github_actions, render_header_line,
    render_masked_short_line, render_reference_line, render_target_line, HnswSearchLatencyError,
    MaskSpec, DEFAULT_DIM, DEFAULT_EF, DEFAULT_QUERIES, DEFAULT_ROWS,
};

// --- refuse_under_github_actions ---

#[test]
fn refuse_under_github_actions_rejects_true() {
    assert!(refuse_under_github_actions(true).is_err());
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- parse_rows ---

#[test]
fn parse_rows_falls_back_on_missing_or_invalid() {
    assert_eq!(parse_rows(None), DEFAULT_ROWS);
    assert_eq!(parse_rows(Some("")), DEFAULT_ROWS);
    assert_eq!(parse_rows(Some("abc")), DEFAULT_ROWS);
    assert_eq!(parse_rows(Some("0")), DEFAULT_ROWS);
    assert_eq!(parse_rows(Some("200001")), DEFAULT_ROWS);
    assert_eq!(parse_rows(Some("100000")), 100_000);
}

// --- parse_dim ---

#[test]
fn parse_dim_falls_back_on_missing_or_invalid() {
    assert_eq!(parse_dim(None), DEFAULT_DIM);
    assert_eq!(parse_dim(Some("abc")), DEFAULT_DIM);
    assert_eq!(parse_dim(Some("0")), DEFAULT_DIM);
    assert_eq!(parse_dim(Some("4097")), DEFAULT_DIM);
    assert_eq!(parse_dim(Some("768")), 768);
}

// --- parse_queries ---

#[test]
fn parse_queries_falls_back_on_missing_or_invalid() {
    assert_eq!(parse_queries(None), DEFAULT_QUERIES);
    assert_eq!(parse_queries(Some("0")), DEFAULT_QUERIES);
    assert_eq!(parse_queries(Some("2001")), DEFAULT_QUERIES);
    assert_eq!(parse_queries(Some("500")), 500);
}

// --- parse_ef ---

#[test]
fn parse_ef_falls_back_on_missing_or_invalid() {
    assert_eq!(parse_ef(None), DEFAULT_EF);
    assert_eq!(parse_ef(Some("0")), DEFAULT_EF);
    assert_eq!(parse_ef(Some("256")), 256);
}

// --- parse_k ---

#[test]
fn parse_k_clamps_to_ef_and_falls_back() {
    assert_eq!(parse_k(None, 64), 10);
    assert_eq!(parse_k(Some("0"), 64), 10);
    assert_eq!(parse_k(Some("100"), 64), 10);
    assert_eq!(parse_k(Some("5"), 64), 5);
    // ef 自体が既定 k (10) より小さい場合、フォールバックも ef に収まる。
    assert_eq!(parse_k(None, 3), 3);
}

// --- parse_mask ---

#[test]
fn parse_mask_resolves_none_and_percent() {
    assert_eq!(parse_mask(None), MaskSpec::None);
    assert_eq!(parse_mask(Some("")), MaskSpec::None);
    assert_eq!(parse_mask(Some("none")), MaskSpec::None);
    assert_eq!(parse_mask(Some("0")), MaskSpec::None);
    assert_eq!(parse_mask(Some("100")), MaskSpec::None);
    assert_eq!(parse_mask(Some("abc")), MaskSpec::None);
    assert_eq!(parse_mask(Some("50")), MaskSpec::VisiblePercent(50));
    assert_eq!(parse_mask(Some("1")), MaskSpec::VisiblePercent(1));
    assert_eq!(parse_mask(Some("99")), MaskSpec::VisiblePercent(99));
}

#[test]
fn mask_spec_token_renders_expected_strings() {
    assert_eq!(MaskSpec::None.token(), "none");
    assert_eq!(MaskSpec::VisiblePercent(50).token(), "50%");
}

// --- generate_corpus ---

#[test]
fn generate_corpus_rejects_oversized_element_count() {
    // rows * dim が MAX_CORPUS_ELEMENTS_GUARD を超える組み合わせ。
    let err = generate_corpus(0, 4_096, 200_000).unwrap_err();
    assert_eq!(err, HnswSearchLatencyError::CorpusTooLarge);
}

#[test]
fn generate_corpus_accepts_max_scenario_scale() {
    // 本 Issue の最大規模点（100,000 行 × dim 768）は受理される。
    let corpus = generate_corpus(0, 768, 100_000).expect("must accept 100k x 768");
    assert_eq!(corpus.len(), 100_000 * 768);
}

#[test]
fn generate_corpus_is_deterministic_for_same_seed() {
    let a = generate_corpus(42, 16, 100).unwrap();
    let b = generate_corpus(42, 16, 100).unwrap();
    assert_eq!(a, b);
}

#[test]
fn generate_query_differs_from_corpus_series() {
    let corpus = generate_corpus(7, 8, 1).unwrap();
    let query = generate_query(7, 8);
    assert_ne!(corpus, query);
}

// --- generate_mask ---

#[test]
fn generate_mask_is_deterministic_and_length_matches() {
    let m1 = generate_mask(123, 1_000, 50);
    let m2 = generate_mask(123, 1_000, 50);
    assert_eq!(m1.len(), 1_000);
    for node in 0..1_000u32 {
        assert_eq!(m1.get(node), m2.get(node));
    }
}

#[test]
fn generate_mask_visible_ratio_is_approximately_requested_percent() {
    let len = 10_000usize;
    let percent = 30u8;
    let mask = generate_mask(999, len, percent);
    let ratio = mask.count_ones() as f64 / len as f64;
    // ベルヌーイ試行の標本誤差を許容する緩いバンド（±5%）。
    assert!(
        (ratio - 0.30).abs() < 0.05,
        "visible ratio {ratio} far from requested 0.30"
    );
}

// --- reference_band ---
//
// Issue #491 codex-review 指摘: 参照ノイズ帯は単一プロセス内の分布からではなく
// 複数プロセス launch の代表値列（例: 交互起動した各プロセスの min_us）から
// 算出する契約に変更した（`reference_band` のシグネチャも `&[f64]` へ変更）。

#[test]
fn reference_band_computes_expected_percentage() {
    // benchmark-judgement-policy.md の例: 1000〜1050 µs -> 5.0%
    // （5 プロセス launch から得た代表値列を模す）。
    let values = [1_000.0, 1_020.0, 1_050.0, 1_010.0, 1_030.0];
    let band = reference_band(&values).unwrap();
    assert!((band - 5.0).abs() < 1e-9);
}

#[test]
fn reference_band_rejects_empty_slice() {
    let err = reference_band(&[]).unwrap_err();
    assert_eq!(err, HnswSearchLatencyError::EmptyOrNonPositiveMin);
}

#[test]
fn reference_band_rejects_non_positive_min() {
    let err = reference_band(&[0.0, 10.0]).unwrap_err();
    assert_eq!(err, HnswSearchLatencyError::EmptyOrNonPositiveMin);
}

// --- render_* ---

#[test]
fn render_lines_contain_expected_fields() {
    let header = render_header_line(
        10_000,
        128,
        MaskSpec::VisiblePercent(50),
        64,
        10,
        200,
        false,
        "abc123",
        "build_env",
        123.456,
    );
    assert!(header.contains("rows=10000"));
    assert!(header.contains("dim=128"));
    assert!(header.contains("mask=50%"));
    assert!(header.contains("dedicated=false"));
    assert!(header.contains("commit=abc123"));
    assert!(header.contains("commit_source=build_env"));

    let target = render_target_line(1.0, 2.0, 3.0, 400);
    assert!(target.contains("target=hnsw_search"));
    assert!(target.contains("samples=400"));

    let reference = render_reference_line(1.0, 1.02, 1.05, 400);
    assert!(reference.contains("reference=brute_force"));
    assert!(reference.contains("samples=400"));

    let masked = render_masked_short_line(3);
    assert_eq!(masked, "hnsw_search_bench: masked_short_queries=3");
}
