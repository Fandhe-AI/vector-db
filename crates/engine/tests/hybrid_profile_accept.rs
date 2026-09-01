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

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::hybrid_profile::{
    build_actually_succeeds, collect_body_strings, generate_corpus, generate_queries,
    refuse_under_github_actions, render_stage_line, sql_dense_statement, sql_hybrid_statement,
    tokenize_only, tokenize_term_doc_freq, tokenize_term_freq, ProfileError, MAX_CORPUS_DOCS_GUARD,
};

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
