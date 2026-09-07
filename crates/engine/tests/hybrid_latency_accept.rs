//! `benches/harness/hybrid_latency.rs`（Issue #324。境界同点グループ再取得ループ
//! 〔Issue #320〕のレイテンシ影響計測ハーネス。CORE-7・PLAN-4/6/7 の関連ポインタ）の
//! 回帰テスト。
//!
//! `hybrid_latency_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/tier_latency_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。
//!
//! `harness/hybrid_latency.rs` 自体に `#[cfg(test)] mod tests` を置かない理由は
//! `tests/tier_latency_accept.rs` 冒頭コメントと同一（`harness/*.rs` は `#[path]`
//! 経由で bench クレートと本テストクレートの双方に取り込まれ、bench コンパイル時は
//! `#[test]` 項目が丸ごと除去されるため `use super::*;` が unused import になる）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::bench_engine::BenchEngine;
use harness::hybrid_latency::{
    aggregate_refetch_stats, check_ann_non_vacuous, extract_counter, generate_corpus,
    generate_query, parse_bounded_usize, parse_corpus_selection, parse_scale_selection,
    refuse_under_github_actions, render_ann_stage_line, render_stage_line, summarize_refetch_stats,
    AnnRoundStats, HybridLatencyError, LatencyCorpusKind, LatencyScale, RefetchStats,
};

// --- generate_corpus: 決定性・上限検証 ---

#[test]
fn generate_corpus_is_deterministic_for_same_seed() {
    let a = generate_corpus(1, 32, 16, 8, None).expect("corpus ok");
    let b = generate_corpus(1, 32, 16, 8, None).expect("corpus ok");
    assert_eq!(a.ids, b.ids);
    assert_eq!(a.vectors, b.vectors);
    assert_eq!(a.texts, b.texts);
}

#[test]
fn generate_corpus_differs_across_seeds() {
    let a = generate_corpus(1, 32, 16, 8, None).expect("corpus ok");
    let b = generate_corpus(2, 32, 16, 8, None).expect("corpus ok");
    assert_ne!(
        a.vectors, b.vectors,
        "異なるシードから同一ベクトル系列が生成された"
    );
}

#[test]
fn generate_corpus_rejects_num_docs_beyond_guard() {
    let err = generate_corpus(
        1,
        harness::hybrid_latency::MAX_CORPUS_DOCS_GUARD + 1,
        16,
        8,
        None,
    )
    .unwrap_err();
    assert_eq!(err, HybridLatencyError::CorpusTooLarge);
}

#[test]
fn generate_corpus_rejects_quantize_levels_below_two() {
    let err = generate_corpus(1, 32, 16, 8, Some(1)).unwrap_err();
    assert_eq!(err, HybridLatencyError::InvalidQuantizeLevels);

    let err0 = generate_corpus(1, 32, 16, 8, Some(0)).unwrap_err();
    assert_eq!(err0, HybridLatencyError::InvalidQuantizeLevels);
}

#[test]
fn generate_corpus_shapes_are_consistent() {
    let corpus = generate_corpus(7, 40, 20, 8, None).expect("corpus ok");
    assert_eq!(corpus.ids.len(), 40);
    assert_eq!(corpus.texts.len(), 40);
    assert_eq!(corpus.vectors.len(), 40 * 8);
    assert_eq!(corpus.dim, 8);
    assert!(corpus.texts.iter().all(|t| !t.is_empty()));
}

// --- quantize モード: 同点誘発の効果を検証 ---

#[test]
fn quantized_corpus_produces_far_fewer_distinct_vectors_than_continuous() {
    // 量子化コーパスは離散値のみで構成されるため、文書数を増やせば同一ベクトルの
    // 重複（＝密チャネルの内積同点の温床）が連続値コーパスより大幅に多く発生する
    // はずである（本モジュールドキュメント「量子化ベクトルモード」の効果そのものを
    // 固定する回帰）。
    use std::collections::BTreeSet;

    let dim = 4;
    let num_docs = 500;
    let continuous = generate_corpus(11, num_docs, 16, dim, None).expect("corpus ok");
    let quantized = generate_corpus(11, num_docs, 16, dim, Some(3)).expect("corpus ok");

    let distinct_count = |vectors: &[f32], dim: usize| -> usize {
        let mut set: BTreeSet<Vec<u32>> = BTreeSet::new();
        for chunk in vectors.chunks_exact(dim) {
            set.insert(chunk.iter().map(|f| f.to_bits()).collect());
        }
        set.len()
    };

    let continuous_distinct = distinct_count(&continuous.vectors, dim);
    let quantized_distinct = distinct_count(&quantized.vectors, dim);

    assert_eq!(
        continuous_distinct, num_docs,
        "連続値コーパスは（このシード・次元では）全ベクトルが相異なるはずである"
    );
    assert!(
        quantized_distinct < continuous_distinct,
        "量子化コーパスの相異なるベクトル数（{quantized_distinct}）が連続値コーパス \
         （{continuous_distinct}）を下回らなかった（同点誘発効果が確認できない）"
    );
}

#[test]
fn generate_corpus_texts_are_identical_across_quantize_modes() {
    // A/B 比較（`hybrid_latency_bench.rs`）が密チャネルの再取得ループ以外の変数を
    // 持たないための前提: 同一 `(seed, num_docs, vocab_size)` なら `quantize_levels`
    // の値（`None`/`Some(n)`）に関わらず `texts`（疎チャネル）は完全に一致しなければ
    // ならない（codex-review P1 指摘・PR #325。`quantize_levels` はベクトル生成のみに
    // 影響し、疎チャネルへ波及してはいけない契約を固定する）。
    let continuous = generate_corpus(11, 200, 16, 4, None).expect("corpus ok");
    let quantized_a = generate_corpus(11, 200, 16, 4, Some(3)).expect("corpus ok");
    let quantized_b = generate_corpus(11, 200, 16, 4, Some(7)).expect("corpus ok");

    assert_eq!(
        continuous.texts, quantized_a.texts,
        "quantize_levels=None と Some(3) で texts が食い違った"
    );
    assert_eq!(
        continuous.texts, quantized_b.texts,
        "quantize_levels=None と Some(7) で texts が食い違った（quantize_levels の \
         値自体が texts に波及している）"
    );
}

// --- generate_query ---

#[test]
fn generate_query_is_deterministic_for_same_seed() {
    let a = generate_query(3, 8, 16);
    let b = generate_query(3, 8, 16);
    assert_eq!(a.vector, b.vector);
    assert_eq!(a.text, b.text);
}

#[test]
fn generate_query_independent_of_corpus_generation_order() {
    // クエリ生成はコーパス生成と別系列（固定オフセット加算）を使う契約
    // （`generate_query` ドキュメント参照）。コーパスを生成してもクエリの結果が
    // 変わらないことを確認する。
    let _ = generate_corpus(5, 100, 16, 8, None).expect("corpus ok");
    let a = generate_query(5, 8, 16);
    let b = generate_query(5, 8, 16);
    assert_eq!(a.vector, b.vector);
}

// --- RefetchStats 集計 ---

#[test]
fn aggregate_refetch_stats_flags_visible_set_reached() {
    let reached = aggregate_refetch_stats(5, 200, 200);
    assert!(reached.reached_visible_set);
    assert_eq!(reached.calls, 5);
    assert_eq!(reached.max_k_seen, 200);

    let not_reached = aggregate_refetch_stats(2, 100, 200);
    assert!(!not_reached.reached_visible_set);
}

#[test]
fn aggregate_refetch_stats_reached_when_max_k_exceeds_visible_set() {
    // dense_cap = MAX_FETCH_K.min(visible_ids.len()) の契約上 max_k は可視集合を
    // 超えないはずだが、集計関数自体は「以上」で判定する契約（`>=`）であることを
    // 固定する（`hybrid.rs` 側の契約が壊れた場合にもこの集計関数は誤って
    // false を返さない）。
    let stats = aggregate_refetch_stats(3, 250, 200);
    assert!(stats.reached_visible_set);
}

#[test]
fn summarize_refetch_stats_aggregates_across_queries() {
    let stats = vec![
        RefetchStats {
            calls: 1,
            max_k_seen: 40,
            reached_visible_set: false,
        },
        RefetchStats {
            calls: 4,
            max_k_seen: 200,
            reached_visible_set: true,
        },
        RefetchStats {
            calls: 2,
            max_k_seen: 80,
            reached_visible_set: false,
        },
    ];
    let summary = summarize_refetch_stats(&stats);
    assert_eq!(summary.queries, 3);
    assert_eq!(summary.calls_max, 4);
    assert_eq!(summary.max_k_across_queries, 200);
    assert_eq!(summary.reached_visible_set_count, 1);
}

#[test]
fn summarize_refetch_stats_empty_input_is_all_zero() {
    let summary = summarize_refetch_stats(&[]);
    assert_eq!(summary.queries, 0);
    assert_eq!(summary.calls_max, 0);
    assert_eq!(summary.max_k_across_queries, 0);
    assert_eq!(summary.reached_visible_set_count, 0);
}

// --- refuse_under_github_actions（fail-closed。Issue #324 計画「fail-closed」節） ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, HybridLatencyError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- render_stage_line: 実測値を必ず含む（本ベンチは非公開閾値を持たないため） ---

#[test]
fn render_stage_line_includes_measured_values() {
    let summary = summarize_refetch_stats(&[RefetchStats {
        calls: 3,
        max_k_seen: 150,
        reached_visible_set: false,
    }]);
    let line = render_stage_line("small_tie_refetch", 1200, 1800, summary);
    assert!(line.contains("stage=small_tie_refetch"));
    assert!(line.contains("p95_us=1800"));
    assert!(line.contains("median_us=1200"));
    assert!(line.contains("provider_calls_max=3"));
    assert!(line.contains("max_k_across_queries=150"));
    assert!(line.contains("reached_visible_set=0/1"));
}

// --- SQL 表層（hnsw opt-in）計測モードの契約テスト（Issue #506） ---

// extract_counter: Debug 文字列越しの薄いパーサ。

#[test]
fn extract_counter_reads_matching_field() {
    let debug = "HnswIndexCacheStats { hits: 3, hybrid_resumed_rounds: 42, entries: 1 }";
    assert_eq!(extract_counter(debug, "hybrid_resumed_rounds"), Some(42));
}

#[test]
fn extract_counter_reads_zero() {
    let debug = "S { hybrid_resumed_rounds: 0 }";
    assert_eq!(extract_counter(debug, "hybrid_resumed_rounds"), Some(0));
}

#[test]
fn extract_counter_returns_none_when_field_absent() {
    // `838c53e`（Issue #505 未マージのツリー）の Debug 出力を模す:
    // `hybrid_resumed_rounds` フィールド自体が存在しない。
    let debug = "HnswIndexCacheStats { hits: 3, entries: 1 }";
    assert_eq!(extract_counter(debug, "hybrid_resumed_rounds"), None);
}

#[test]
fn extract_counter_returns_none_on_malformed_value() {
    let debug = "S { hybrid_resumed_rounds: abc }";
    assert_eq!(extract_counter(debug, "hybrid_resumed_rounds"), None);
}

#[test]
fn extract_counter_does_not_confuse_prefix_field_names() {
    // `hybrid_dense_searches` は `hybrid_resumed_rounds` の探索対象ではない
    // ため誤って拾わないことを固定する。
    let debug = "S { hybrid_dense_searches: 7 }";
    assert_eq!(extract_counter(debug, "hybrid_resumed_rounds"), None);
}

// --- parse_scale_selection / parse_corpus_selection ---

#[test]
fn parse_scale_selection_defaults_to_both() {
    assert_eq!(
        parse_scale_selection(None).expect("ok"),
        vec![LatencyScale::Small, LatencyScale::Large]
    );
    assert_eq!(
        parse_scale_selection(Some("")).expect("ok"),
        vec![LatencyScale::Small, LatencyScale::Large]
    );
    assert_eq!(
        parse_scale_selection(Some("all")).expect("ok"),
        vec![LatencyScale::Small, LatencyScale::Large]
    );
}

#[test]
fn parse_scale_selection_accepts_single_values() {
    assert_eq!(
        parse_scale_selection(Some("small")).expect("ok"),
        vec![LatencyScale::Small]
    );
    assert_eq!(
        parse_scale_selection(Some("large")).expect("ok"),
        vec![LatencyScale::Large]
    );
}

#[test]
fn parse_scale_selection_rejects_unknown_values() {
    assert!(parse_scale_selection(Some("huge")).is_err());
}

#[test]
fn parse_corpus_selection_defaults_to_both() {
    assert_eq!(
        parse_corpus_selection(None).expect("ok"),
        vec![LatencyCorpusKind::NoRefetch, LatencyCorpusKind::TieRefetch]
    );
}

#[test]
fn parse_corpus_selection_accepts_single_values() {
    assert_eq!(
        parse_corpus_selection(Some("no_refetch")).expect("ok"),
        vec![LatencyCorpusKind::NoRefetch]
    );
    assert_eq!(
        parse_corpus_selection(Some("tie_refetch")).expect("ok"),
        vec![LatencyCorpusKind::TieRefetch]
    );
}

#[test]
fn parse_corpus_selection_rejects_unknown_values() {
    assert!(parse_corpus_selection(Some("weird")).is_err());
}

// --- parse_bounded_usize ---

#[test]
fn parse_bounded_usize_uses_default_when_unset() {
    assert_eq!(
        parse_bounded_usize(None, 32, 1, 4096, "DIM").expect("ok"),
        32
    );
    assert_eq!(
        parse_bounded_usize(Some(""), 32, 1, 4096, "DIM").expect("ok"),
        32
    );
}

#[test]
fn parse_bounded_usize_accepts_value_in_range() {
    assert_eq!(
        parse_bounded_usize(Some("16"), 32, 1, 4096, "DIM").expect("ok"),
        16
    );
}

#[test]
fn parse_bounded_usize_rejects_out_of_range() {
    assert!(parse_bounded_usize(Some("0"), 32, 1, 4096, "DIM").is_err());
    assert!(parse_bounded_usize(Some("5000"), 32, 1, 4096, "DIM").is_err());
}

#[test]
fn parse_bounded_usize_rejects_non_numeric() {
    assert!(parse_bounded_usize(Some("abc"), 32, 1, 4096, "DIM").is_err());
}

// --- render_ann_stage_line: 実測値・engine トークンを必ず含む ---

#[test]
fn render_ann_stage_line_includes_measured_values_and_resumed_rounds() {
    let stats = AnnRoundStats {
        builds: 1,
        build_failures: 0,
        hybrid_dense_searches: 240,
        hybrid_rounds_max: 4,
        masked_short: 0,
        fallbacks: 0,
        ef_cap_fallbacks: 0,
        f16_residency_fallbacks: 0,
        hybrid_resumed_rounds: Some(180),
    };
    let line = render_ann_stage_line(
        "sql_large_tie_refetch",
        BenchEngine::Hnsw,
        1360,
        1382,
        stats,
    );
    assert!(line.contains("stage=sql_large_tie_refetch"));
    assert!(line.contains("engine=hnsw"));
    assert!(line.contains("p95_us=1382"));
    assert!(line.contains("median_us=1360"));
    assert!(line.contains("hybrid_dense_searches=240"));
    assert!(line.contains("hybrid_rounds_max=4"));
    assert!(line.contains("masked_short=0"));
    assert!(line.contains("hybrid_resumed_rounds=180"));
}

#[test]
fn render_ann_stage_line_shows_na_when_resumed_rounds_unavailable() {
    let stats = AnnRoundStats {
        builds: 1,
        ..Default::default()
    };
    let line = render_ann_stage_line("stage", BenchEngine::Hnsw, 0, 0, stats);
    assert!(line.contains("hybrid_resumed_rounds=n/a"));
}

// --- check_ann_non_vacuous ---

#[test]
fn check_ann_non_vacuous_rejects_zero_builds() {
    let stats = AnnRoundStats::default();
    let err = check_ann_non_vacuous(stats, false).unwrap_err();
    assert!(matches!(err, HybridLatencyError::VacuousAnnMeasurement(_)));
}

#[test]
fn check_ann_non_vacuous_rejects_build_failures() {
    let stats = AnnRoundStats {
        builds: 1,
        build_failures: 1,
        hybrid_dense_searches: 1,
        ..Default::default()
    };
    assert!(check_ann_non_vacuous(stats, false).is_err());
}

#[test]
fn check_ann_non_vacuous_rejects_zero_dense_searches() {
    let stats = AnnRoundStats {
        builds: 1,
        ..Default::default()
    };
    assert!(check_ann_non_vacuous(stats, false).is_err());
}

#[test]
fn check_ann_non_vacuous_passes_without_resumed_requirement() {
    let stats = AnnRoundStats {
        builds: 1,
        hybrid_dense_searches: 1,
        hybrid_resumed_rounds: Some(0),
        ..Default::default()
    };
    assert!(check_ann_non_vacuous(stats, false).is_ok());
}

#[test]
fn check_ann_non_vacuous_rejects_zero_resumed_rounds_when_expected() {
    let stats = AnnRoundStats {
        builds: 1,
        hybrid_dense_searches: 1,
        hybrid_resumed_rounds: Some(0),
        ..Default::default()
    };
    assert!(check_ann_non_vacuous(stats, true).is_err());
}

#[test]
fn check_ann_non_vacuous_rejects_missing_resumed_rounds_field_when_expected() {
    // before ツリー（`838c53e`）を模す: フィールド自体が存在しないため
    // `extract_counter` は常に `None` を返す。`EXPECT_RESUMED=1` を before
    // バイナリへ渡すのは呼び出し側の誤りであり fail-closed で拒否する。
    let stats = AnnRoundStats {
        builds: 1,
        hybrid_dense_searches: 1,
        hybrid_resumed_rounds: None,
        ..Default::default()
    };
    assert!(check_ann_non_vacuous(stats, true).is_err());
}

#[test]
fn check_ann_non_vacuous_passes_when_resumed_rounds_positive_and_expected() {
    let stats = AnnRoundStats {
        builds: 1,
        hybrid_dense_searches: 240,
        hybrid_rounds_max: 4,
        hybrid_resumed_rounds: Some(180),
        ..Default::default()
    };
    assert!(check_ann_non_vacuous(stats, true).is_ok());
}
