//! `benches/harness/hybrid_wire.rs`（Issue #465。`hybrid_wire_profile_bench.rs`
//! が engine 内 hybrid 経路／SQL 表層／wire 内訳を切り分けるための SQL 文字列
//! 組み立て・決定的本文生成を担う純関数群）の回帰テスト。
//!
//! `crates/wire-server/tests/knn_wire_profile_accept.rs` と同様、時間依存の
//! ベンチ本体は実行せず `#[path]` で取り込んだ純関数のみを `cargo test`
//! （`make ci` 対象）で検証する。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::hybrid_wire::{
    generate_body_text, generate_query_text, hybrid_statement, HybridWireProjection,
};
use harness::rng::DeterministicRng;
use harness::sql_c1::{vector_literal, SqlC1Error};

#[test]
fn hybrid_statement_id_projection_selects_id_only() {
    let literal = vector_literal(&[1.0, 0.0]).expect("finite vector");
    let sql = hybrid_statement(
        "docs",
        "embedding",
        "body",
        &literal,
        "vector search",
        10,
        HybridWireProjection::Id,
    )
    .expect("well-formed statement");
    assert!(sql.starts_with("SELECT id FROM docs"));
    assert!(sql.contains("hybrid_rrf(embedding, '[1,0]', body, 'vector search')"));
    assert!(sql.contains("LIMIT 10"));
}

#[test]
fn hybrid_statement_id_and_body_projection_selects_both_columns() {
    let literal = vector_literal(&[0.5, -0.5]).expect("finite vector");
    let sql = hybrid_statement(
        "docs",
        "embedding",
        "body",
        &literal,
        "q",
        5,
        HybridWireProjection::IdAndBody,
    )
    .expect("well-formed statement");
    assert!(sql.starts_with("SELECT id, body FROM docs"));
}

#[test]
fn hybrid_statement_rejects_invalid_table_identifier() {
    let literal = vector_literal(&[1.0]).expect("finite vector");
    let err = hybrid_statement(
        "docs; DROP TABLE docs",
        "embedding",
        "body",
        &literal,
        "q",
        1,
        HybridWireProjection::Id,
    )
    .unwrap_err();
    assert_eq!(err, SqlC1Error::InvalidIdentifier("table"));
}

#[test]
fn hybrid_statement_rejects_unsafe_query_text() {
    let literal = vector_literal(&[1.0]).expect("finite vector");
    let err = hybrid_statement(
        "docs",
        "embedding",
        "body",
        &literal,
        "a' OR '1'='1",
        1,
        HybridWireProjection::Id,
    )
    .unwrap_err();
    assert_eq!(err, SqlC1Error::InvalidIdentifier("query_text"));
}

#[test]
fn generate_body_text_is_deterministic_for_same_seed() {
    let mut rng_a = DeterministicRng::new(7);
    let mut rng_b = DeterministicRng::new(7);
    assert_eq!(
        generate_body_text(&mut rng_a),
        generate_body_text(&mut rng_b)
    );
}

#[test]
fn generate_query_text_is_always_sql_safe() {
    let mut rng = DeterministicRng::new(11);
    for _ in 0..50 {
        let text = generate_query_text(&mut rng);
        let literal = vector_literal(&[1.0]).expect("finite vector");
        hybrid_statement(
            "docs",
            "embedding",
            "body",
            &literal,
            &text,
            10,
            HybridWireProjection::Id,
        )
        .unwrap_or_else(|e| panic!("generated query text {text:?} was rejected: {e}"));
    }
}

#[test]
fn generate_body_and_query_text_are_nonempty() {
    let mut rng = DeterministicRng::new(3);
    assert!(!generate_body_text(&mut rng).is_empty());
    assert!(!generate_query_text(&mut rng).is_empty());
}
