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

use harness::hybrid_latency::{
    aggregate_refetch_stats, generate_corpus, generate_query, refuse_under_github_actions,
    render_stage_line, summarize_refetch_stats, HybridLatencyError, RefetchStats,
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
    let line = render_stage_line("small_max_refetch", 1200, 1800, summary);
    assert!(line.contains("stage=small_max_refetch"));
    assert!(line.contains("p95_us=1800"));
    assert!(line.contains("median_us=1200"));
    assert!(line.contains("provider_calls_max=3"));
    assert!(line.contains("max_k_across_queries=150"));
    assert!(line.contains("reached_visible_set=0/1"));
}
