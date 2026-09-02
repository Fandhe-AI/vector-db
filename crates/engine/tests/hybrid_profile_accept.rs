//! `benches/harness/hybrid_profile.rs`（Issue #356。hybrid_rrf クエリの段別内訳
//! プロファイル切り分け。親 Issue #355・SEARCH-1・SEARCH-3 関連ポインタ）の
//! 回帰テスト。
//!
//! `hybrid_profile_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hybrid_latency_accept.rs` と同様、実測タイマー・env・実ストレージに
//! 依存しない時間非依存の契約のみを `#[path]` で取り込み `cargo test`
//! 〔`make ci` 対象〕で検証する）。
//!
//! `harness/hybrid_profile.rs` 自体に `#[cfg(test)] mod tests` を置かない理由は
//! `tests/hybrid_latency_accept.rs` 冒頭コメントと同一（`harness/*.rs` は
//! `#[path]` 経由で bench クレートと本テストクレートの双方に取り込まれ、bench
//! コンパイル時は `#[test]` 項目が丸ごと除去されるため `use super::*;` が
//! unused import になる）。
//!
//! Issue #387 PR #416 codex-review P2 指摘対応: `harness::hybrid_profile` は
//! `engine::hybrid::sparse_refetch_observed`（非既定 feature `bench-internals`
//! 限定）を無条件 import するモジュールへ変わったため、本テストファイル全体を
//! `cross-encoder` feature 限定の `tests/rerank_cross_encoder_recall.rs` と同じ
//! `#![cfg(feature = "...")]` パターンで同 feature の背後に置く。通常の
//! `cargo test -p engine`（feature 無指定）では本ファイルは空になり実行対象から
//! 外れる（`make lint`/`make test` は `--all-features` のため CI 経路は従来どおり）。

#![cfg(feature = "bench-internals")]

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::collections::BTreeSet;

use harness::hybrid_profile::{
    boundary_tie_decision, build_actually_succeeds, collect_body_strings, dense_refetch_schedule,
    fetch_cap, generate_corpus, generate_queries, initial_fetch_k, is_exhaustive, next_fetch_k,
    refetch_schedule_matches_observed_calls, refuse_under_github_actions,
    render_dense_refetch_line, render_sparse_refetch_line, render_sparse_refetch_summary_line,
    render_stage_line, replica_matches_real, sparse_refetch_schedule, sql_dense_statement,
    sql_hybrid_statement, summarize_sparse_refetch, tokenize_only, tokenize_term_doc_freq,
    tokenize_term_freq, ProfileError, ProfileSparseIndex, RefetchSchedule, TieDecision,
    MAX_CORPUS_DOCS_GUARD, MAX_FETCH_K_MIRROR, MAX_POOL_DEPTH_MIRROR,
};

use harness::hybrid_latency::RefetchTrackingProvider;

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::sparse::SparseIndex;

// --- generate_corpus: 決定性・形状 ---

#[test]
fn generate_corpus_is_deterministic_for_same_seed() {
    let a = generate_corpus(1, 64, 8).expect("corpus ok");
    let b = generate_corpus(1, 64, 8).expect("corpus ok");
    assert_eq!(a.ids, b.ids);
    assert_eq!(a.vectors, b.vectors);
    assert_eq!(a.bodies, b.bodies);
}

#[test]
fn generate_corpus_differs_across_seeds() {
    let a = generate_corpus(1, 64, 8).expect("corpus ok");
    let b = generate_corpus(2, 64, 8).expect("corpus ok");
    assert_ne!(
        a.vectors, b.vectors,
        "異なるシードから同一ベクトル系列が生成された"
    );
}

#[test]
fn generate_corpus_shapes_are_consistent() {
    let corpus = generate_corpus(7, 40, 8).expect("corpus ok");
    assert_eq!(corpus.ids.len(), 40);
    assert_eq!(corpus.bodies.len(), 40);
    assert_eq!(corpus.vectors.len(), 40 * 8);
    assert_eq!(corpus.dim, 8);
    assert!(corpus.bodies.iter().all(|b| !b.is_empty()));
}

#[test]
fn generate_corpus_bodies_are_independent_of_dim() {
    // 疎チャネル（bodies）は密チャネル用 RNG と別系列のため、dim を変えても
    // bodies の内容は変わらない契約（モジュールドキュメント参照）。
    let a = generate_corpus(3, 20, 8).expect("corpus ok");
    let b = generate_corpus(3, 20, 32).expect("corpus ok");
    assert_eq!(a.bodies, b.bodies);
}

#[test]
fn generate_corpus_rejects_num_docs_beyond_guard() {
    let err = generate_corpus(1, MAX_CORPUS_DOCS_GUARD + 1, 8).unwrap_err();
    assert_eq!(err, ProfileError::CorpusTooLarge);
}

#[test]
fn generate_corpus_sparse_docs_round_trips_ids_and_bodies() {
    let corpus = generate_corpus(9, 10, 4).expect("corpus ok");
    let docs = corpus.sparse_docs();
    assert_eq!(docs.len(), 10);
    for (i, (doc_id, text)) in docs.iter().enumerate() {
        assert_eq!(*doc_id, corpus.ids[i]);
        assert_eq!(*text, corpus.bodies[i].as_str());
    }
}

// --- generate_queries ---

#[test]
fn generate_queries_is_deterministic_for_same_seed() {
    let a = generate_queries(3, 5, 8);
    let b = generate_queries(3, 5, 8);
    assert_eq!(a.len(), 5);
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.vector, y.vector);
        assert_eq!(x.text, y.text);
    }
}

#[test]
fn generate_queries_independent_of_corpus_generation() {
    let _ = generate_corpus(5, 100, 8).expect("corpus ok");
    let a = generate_queries(5, 3, 8);
    let b = generate_queries(5, 3, 8);
    assert_eq!(a[0].vector, b[0].vector);
}

// --- build_actually_succeeds: 複製ロジックの構造的整合性チェック ---

#[test]
fn build_actually_succeeds_for_generated_corpus() {
    let corpus = generate_corpus(11, 50, 8).expect("corpus ok");
    assert!(build_actually_succeeds(&corpus));
}

// --- tokenize/term_freq/doc_freq 複製の累積性・決定性 ---

#[test]
fn tokenize_stage_functions_are_deterministic() {
    let corpus = generate_corpus(13, 30, 8).expect("corpus ok");
    assert_eq!(tokenize_only(&corpus.bodies), tokenize_only(&corpus.bodies));
    assert_eq!(
        tokenize_term_freq(&corpus.bodies),
        tokenize_term_freq(&corpus.bodies)
    );
    assert_eq!(
        tokenize_term_doc_freq(&corpus.bodies),
        tokenize_term_doc_freq(&corpus.bodies)
    );
}

#[test]
fn tokenize_term_doc_freq_vocab_size_is_bounded_by_total_tokens() {
    // コーパス全体の語彙数（doc_freq.len()）は総トークン数を超えない
    // （各語彙エントリは少なくとも 1 トークンから生じる）。
    let corpus = generate_corpus(17, 40, 8).expect("corpus ok");
    let total_tokens = tokenize_only(&corpus.bodies);
    let vocab_size = tokenize_term_doc_freq(&corpus.bodies);
    assert!(
        vocab_size <= total_tokens,
        "vocab_size ({vocab_size}) exceeded total_tokens ({total_tokens})"
    );
}

#[test]
fn tokenize_term_freq_unique_terms_sum_is_bounded_by_total_tokens() {
    let corpus = generate_corpus(19, 40, 8).expect("corpus ok");
    let total_tokens = tokenize_only(&corpus.bodies);
    let unique_terms_sum = tokenize_term_freq(&corpus.bodies);
    assert!(
        unique_terms_sum <= total_tokens,
        "unique_terms_sum ({unique_terms_sum}) exceeded total_tokens ({total_tokens})"
    );
}

#[test]
fn tokenize_stage_functions_yield_nonzero_for_nonempty_corpus() {
    let corpus = generate_corpus(23, 5, 8).expect("corpus ok");
    assert!(tokenize_only(&corpus.bodies) > 0);
    assert!(tokenize_term_freq(&corpus.bodies) > 0);
    assert!(tokenize_term_doc_freq(&corpus.bodies) > 0);
}

// --- collect_body_strings ---

#[test]
fn collect_body_strings_preserves_ids_and_bodies() {
    let corpus = generate_corpus(29, 12, 4).expect("corpus ok");
    let collected = collect_body_strings(&corpus.ids, &corpus.bodies);
    assert_eq!(collected.len(), 12);
    for (i, (id, body)) in collected.iter().enumerate() {
        assert_eq!(*id, corpus.ids[i]);
        assert_eq!(body, &corpus.bodies[i]);
    }
}

// --- SQL 文字列組み立て: 識別子検証・リテラル埋め込み ---

#[test]
fn sql_hybrid_statement_embeds_vector_and_text() {
    let sql = sql_hybrid_statement(
        "docs",
        "embedding",
        "body",
        &[1.0, 0.0],
        "vector search",
        10,
    )
    .expect("well-formed statement");
    assert!(sql.contains("hybrid_rrf(embedding, '[1,0]', body, 'vector search')"));
    assert!(sql.contains("LIMIT 10"));
}

#[test]
fn sql_dense_statement_embeds_vector_literal() {
    let sql =
        sql_dense_statement("docs", "embedding", &[0.5, -0.5], 5).expect("well-formed statement");
    assert!(sql.contains("embedding <=> '[0.5,-0.5]'"));
    assert!(sql.contains("LIMIT 5"));
}

#[test]
fn sql_hybrid_statement_rejects_invalid_table_identifier() {
    let err = sql_hybrid_statement("docs; DROP TABLE docs", "embedding", "body", &[1.0], "q", 1)
        .unwrap_err();
    assert_eq!(err, ProfileError::InvalidIdentifier("table"));
}

#[test]
fn sql_hybrid_statement_rejects_unsafe_query_text() {
    let err =
        sql_hybrid_statement("docs", "embedding", "body", &[1.0], "a' OR '1'='1", 1).unwrap_err();
    assert_eq!(err, ProfileError::UnsafeQueryText);
}

#[test]
fn generate_queries_text_is_always_sql_safe() {
    // generate_queries が組み立てるテキストは常に sql_hybrid_statement の検証を
    // 通過する契約（`generate_queries` ドキュメント参照）。
    let queries = generate_queries(31, 20, 8);
    for q in &queries {
        sql_hybrid_statement("docs", "embedding", "body", &q.vector, &q.text, 10)
            .unwrap_or_else(|e| panic!("generated query text {:?} was rejected: {e}", q.text));
    }
}

// --- fail-closed（GITHUB_ACTIONS） ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, ProfileError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- render_stage_line: 実測値を必ず含む ---

#[test]
fn render_stage_line_includes_measured_values() {
    let line = render_stage_line("sql_hybrid", 1200, 1800, 25000);
    assert!(line.contains("stage=sql_hybrid"));
    assert!(line.contains("p95_us=1800"));
    assert!(line.contains("median_us=1200"));
    assert!(line.contains("check_value=25000"));
}

// --- Issue #387: ProfileSparseIndex 複製の忠実性 ---------------------------------

fn visible_set(corpus: &harness::hybrid_profile::ProfileCorpus) -> BTreeSet<u64> {
    corpus.ids.iter().copied().collect()
}

#[test]
fn profile_sparse_index_replica_matches_real_search_within() {
    let corpus = generate_corpus(41, 64, 8).expect("corpus ok");
    let real = SparseIndex::build(&corpus.sparse_docs()).expect("real index ok");
    let replica = ProfileSparseIndex::build(&corpus.sparse_docs()).expect("replica index ok");
    let full_visible = visible_set(&corpus);
    let even_visible: BTreeSet<u64> = full_visible
        .iter()
        .copied()
        .filter(|id| id % 2 == 0)
        .collect();
    let empty_visible: BTreeSet<u64> = BTreeSet::new();

    let queries = generate_queries(41, 4, 8);
    for q in &queries {
        for k in [1usize, 5, 64, 400] {
            for visible in [&full_visible, &even_visible, &empty_visible] {
                replica_matches_real(&real, &replica, &q.text, k, visible)
                    .unwrap_or_else(|e| panic!("replica mismatch: {e}"));
            }
        }
    }
}

#[test]
fn profile_sparse_index_rejects_duplicate_doc_id() {
    let docs: Vec<(u64, &str)> = vec![(1, "vector search"), (1, "dense sparse")];
    let err = ProfileSparseIndex::build(&docs).unwrap_err();
    assert_eq!(err, ProfileError::DuplicateDocId);
}

#[test]
fn subset_only_and_subset_df_are_consistent_with_replica() {
    let corpus = generate_corpus(43, 30, 8).expect("corpus ok");
    let replica = ProfileSparseIndex::build(&corpus.sparse_docs()).expect("replica index ok");
    let visible = visible_set(&corpus);
    let queries = generate_queries(43, 3, 8);
    for q in &queries {
        let subset_len = replica.subset_only(&q.text, &visible);
        assert!(subset_len <= visible.len());
        let unique_term_count: BTreeSet<String> =
            engine::sparse::tokenize(&q.text).into_iter().collect();
        let unique_term_count = unique_term_count.len();
        let total_df = replica.subset_df(&q.text, &visible);
        assert!(total_df <= subset_len * unique_term_count.max(1));
    }
}

// --- boundary_tie_decision: hybrid.rs の複製の判定分岐 ---------------------------

#[test]
fn boundary_tie_decision_no_boundary_when_scores_shorter_than_pool_depth() {
    let scores = [3.0, 2.0];
    assert_eq!(
        boundary_tie_decision(&scores, 5, false, true),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_no_tie_at_boundary_is_resolved() {
    let scores = [5.0, 4.0, 3.0, 2.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_tie_group_ends_within_observed_range_is_resolved() {
    let scores = [5.0, 4.0, 4.0, 3.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_tie_to_tail_non_exhaustive_is_undetermined() {
    let scores = [5.0, 4.0, 4.0, 4.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Undetermined
    );
}

#[test]
fn boundary_tie_decision_tie_to_tail_exhaustive_is_resolved() {
    let scores = [5.0, 4.0, 4.0, 4.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, true, false),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_len_equals_pool_depth_non_exhaustive_is_undetermined() {
    let scores = [5.0, 4.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Undetermined
    );
}

#[test]
fn boundary_tie_decision_zero_boundary_with_no_signal_is_resolved() {
    // 疎チャネル（zero_is_no_signal=true）で境界スコアが非正なら、末尾まで
    // 同点が続いていても即座に Resolved（`hybrid.rs::resolve_boundary_tie_group`
    // の無シグナル群早期確定と同じ挙動）。
    let scores = [5.0, 0.0, 0.0, 0.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, true),
        TieDecision::Resolved
    );
}

// --- fetch_k スケジュールヘルパ ---------------------------------------------------

#[test]
fn fetch_k_schedule_helpers_behave_as_expected() {
    assert_eq!(initial_fetch_k(200, 25_000), 400);
    assert_eq!(initial_fetch_k(200, 100), 100);
    assert_eq!(next_fetch_k(400, 25_000), Some(800));
    assert_eq!(next_fetch_k(25_000, 25_000), None);
    assert_eq!(fetch_cap(25_000), 25_000.min(MAX_FETCH_K_MIRROR));
    assert!(is_exhaustive(100, 50, 1_000));
    assert!(is_exhaustive(1_000, 100, 1_000));
    assert!(!is_exhaustive(100, 100, 1_000));
}

// --- sparse_refetch_schedule / dense_refetch_schedule ---------------------------

#[test]
fn sparse_refetch_schedule_stops_at_first_resolved_for_non_tied_corpus() {
    // 文書ごとに固有の語彙を割り当て、同点が実質発生しないコーパスでは
    // 初回 fetch_k で終端確定できる（再取得ラウンド数 1）はず。
    let docs: Vec<(u64, String)> = (0..64u64)
        .map(|i| (i, format!("uniqueterm{i} filler filler filler")))
        .collect();
    let doc_refs: Vec<(u64, &str)> = docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
    let index = SparseIndex::build(&doc_refs).expect("index ok");
    let visible: BTreeSet<u64> = (0..64u64).collect();
    let schedule = sparse_refetch_schedule(&index, "uniqueterm0", &visible, 8)
        .expect("schedule reproduction ok");
    assert_eq!(schedule.fetch_ks.len(), 1);
}

#[test]
fn sparse_refetch_schedule_reaches_cap_on_all_tied_corpus() {
    // 全文書が同一語彙・同一文書長を持つ同点誘発コーパス。可視集合 64 件・
    // pool_depth 8 なら 16→32→64 と倍増して cap（可視集合サイズ）まで到達する。
    let docs: Vec<(u64, String)> = (0..64u64)
        .map(|_| (0u64, "vector search dense sparse".to_string()))
        .enumerate()
        .map(|(i, (_, t))| (i as u64, t))
        .collect();
    let doc_refs: Vec<(u64, &str)> = docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
    let index = SparseIndex::build(&doc_refs).expect("index ok");
    let visible: BTreeSet<u64> = (0..64u64).collect();
    let schedule = sparse_refetch_schedule(&index, "vector search", &visible, 8)
        .expect("schedule reproduction ok");
    assert!(schedule.reached_cap);
    assert_eq!(*schedule.fetch_ks.last().expect("nonempty schedule"), 64);
}

#[test]
fn dense_refetch_schedule_matches_real_hybrid_search_calls_normal_corpus() {
    let corpus = harness::hybrid_latency::generate_corpus(53, 300, 20, 8, None).expect("corpus ok");
    let sparse_index = corpus.build_sparse_index().expect("sparse index ok");
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let query = harness::hybrid_latency::generate_query(53, 8, 20);
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid cfg");

    let predicted = dense_refetch_schedule(
        &provider,
        &corpus.ids,
        &corpus.vectors,
        corpus.dim,
        &query.vector,
        8,
    )
    .expect("dense schedule reproduction ok");

    provider.reset();
    let input = SearchInput {
        ids: &corpus.ids,
        vectors: &corpus.vectors,
        dim: corpus.dim,
        query: &query.vector,
        k: 8,
    };
    hybrid_search(&provider, input, &sparse_index, &query.text, 8, &cfg).expect("hybrid_search ok");

    refetch_schedule_matches_observed_calls(0, &predicted, provider.calls())
        .unwrap_or_else(|e| panic!("refetch schedule mismatch: {e}"));
}

#[test]
fn dense_refetch_schedule_matches_real_hybrid_search_calls_tied_corpus() {
    let corpus =
        harness::hybrid_latency::generate_corpus(59, 300, 20, 8, Some(4)).expect("corpus ok");
    let sparse_index = corpus.build_sparse_index().expect("sparse index ok");
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let query = harness::hybrid_latency::generate_query(59, 8, 20);
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid cfg");

    let predicted = dense_refetch_schedule(
        &provider,
        &corpus.ids,
        &corpus.vectors,
        corpus.dim,
        &query.vector,
        8,
    )
    .expect("dense schedule reproduction ok");

    provider.reset();
    let input = SearchInput {
        ids: &corpus.ids,
        vectors: &corpus.vectors,
        dim: corpus.dim,
        query: &query.vector,
        k: 8,
    };
    hybrid_search(&provider, input, &sparse_index, &query.text, 8, &cfg).expect("hybrid_search ok");

    refetch_schedule_matches_observed_calls(0, &predicted, provider.calls())
        .unwrap_or_else(|e| panic!("refetch schedule mismatch: {e}"));
}

#[test]
fn dense_refetch_schedule_matches_real_hybrid_search_calls_small_visible_set() {
    // 可視集合が初期 fetch_k（pool_depth*2）より小さいケース: cap がすぐに
    // 可視集合サイズへ張り付き、1 回で終端確定するはず。
    let corpus = harness::hybrid_latency::generate_corpus(61, 20, 20, 8, None).expect("corpus ok");
    let sparse_index = corpus.build_sparse_index().expect("sparse index ok");
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let query = harness::hybrid_latency::generate_query(61, 8, 20);
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid cfg");

    let predicted = dense_refetch_schedule(
        &provider,
        &corpus.ids,
        &corpus.vectors,
        corpus.dim,
        &query.vector,
        8,
    )
    .expect("dense schedule reproduction ok");
    assert_eq!(predicted.fetch_ks.len(), 1);

    provider.reset();
    let input = SearchInput {
        ids: &corpus.ids,
        vectors: &corpus.vectors,
        dim: corpus.dim,
        query: &query.vector,
        k: 8,
    };
    hybrid_search(&provider, input, &sparse_index, &query.text, 8, &cfg).expect("hybrid_search ok");

    refetch_schedule_matches_observed_calls(0, &predicted, provider.calls())
        .unwrap_or_else(|e| panic!("refetch schedule mismatch: {e}"));
}

#[test]
fn refetch_schedule_matches_observed_calls_detects_mismatch() {
    let predicted = RefetchSchedule {
        fetch_ks: vec![16, 32],
        final_hits: 5,
        reached_cap: false,
    };
    let err = refetch_schedule_matches_observed_calls(3, &predicted, 1).unwrap_err();
    assert_eq!(
        err,
        ProfileError::RefetchMismatch {
            query: 3,
            predicted: 2,
            observed: 1,
        }
    );
}

// --- 鏡像定数のドリフト検知 ---

#[test]
fn max_pool_depth_mirror_matches_rrf_config_bounds() {
    assert!(RrfConfig::new(60.0, 1.0, 1.0, MAX_POOL_DEPTH_MIRROR).is_ok());
    assert!(RrfConfig::new(60.0, 1.0, 1.0, MAX_POOL_DEPTH_MIRROR + 1).is_err());
}

// --- 描画関数 ---

#[test]
fn render_sparse_refetch_line_includes_schedule() {
    let schedule = RefetchSchedule {
        fetch_ks: vec![400, 800, 1600],
        final_hits: 200,
        reached_cap: false,
    };
    let line = render_sparse_refetch_line(2, &schedule);
    assert!(line.contains("query=2"));
    assert!(line.contains("calls=3"));
    assert!(line.contains("fetch_ks=400,800,1600"));
    assert!(line.contains("final_hits=200"));
    assert!(line.contains("reached_cap=false"));
}

#[test]
fn render_sparse_refetch_summary_line_includes_measured_values() {
    let schedules = vec![
        RefetchSchedule {
            fetch_ks: vec![400],
            final_hits: 10,
            reached_cap: false,
        },
        RefetchSchedule {
            fetch_ks: vec![400, 800],
            final_hits: 20,
            reached_cap: true,
        },
    ];
    let summary = summarize_sparse_refetch(&schedules);
    assert_eq!(summary.queries, 2);
    assert_eq!(summary.calls_max, 2);
    assert_eq!(summary.calls_total, 3);
    assert_eq!(summary.reached_cap_count, 1);
    assert_eq!(summary.max_fetch_k, 800);
    let line = render_sparse_refetch_summary_line(&summary, 1234);
    assert!(line.contains("queries=2"));
    assert!(line.contains("calls_max=2"));
    assert!(line.contains("calls_total=3"));
    assert!(line.contains("reached_cap_count=1"));
    assert!(line.contains("max_fetch_k=800"));
    assert!(line.contains("estimated_cumulative_mixed_median_us=1234"));
}

#[test]
fn render_dense_refetch_line_includes_measured_values() {
    let summary = harness::hybrid_latency::aggregate_refetch_stats(3, 800, 1000);
    let batch = harness::hybrid_latency::summarize_refetch_stats(&[summary]);
    let line = render_dense_refetch_line("hybrid_search_cached_index", 500, 900, &batch);
    assert!(line.contains("stage=hybrid_search_cached_index"));
    assert!(line.contains("p95_us=900"));
    assert!(line.contains("median_us=500"));
    assert!(line.contains("provider_calls_max=3"));
}

// --- ProfileError の Display ---

#[test]
fn profile_error_display_is_nonempty_for_new_variants() {
    let variants = [
        ProfileError::DuplicateDocId,
        ProfileError::ReplicaMismatch {
            position: 1,
            detail: "x".to_string(),
        },
        ProfileError::RefetchMismatch {
            query: 0,
            predicted: 1,
            observed: 2,
        },
        ProfileError::ContractViolation("boom".to_string()),
    ];
    for v in variants {
        assert!(!v.to_string().is_empty());
    }
}
