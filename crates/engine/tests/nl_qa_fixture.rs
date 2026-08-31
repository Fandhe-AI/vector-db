//! Issue #333（SEARCH-7 方式変更）: `tests/fixtures/nl_qa.rs`（自然言語 QA 決定的
//! fixture）を feature 非依存で常時 `cargo test` に含める層 A テスト
//! （`rerank_recall.rs` の層 A/層 B 分離と同じ方針。ここには spec 由来の閾値は
//! 持ち込まず、決定性の固定と `LexicalOverlapReranker` 実測値の回帰トラッキング
//! のみを行う）。
//!
//! `cross-encoder` feature（`ort`/`tokenizers`）は本ファイルには含めない
//! （`docs/design/rerank-recall-regression.md`「Issue #333」節参照）ため、
//! クロスエンコーダの実測はここに含まれない。以下の固定値は
//! **`LexicalOverlapReranker`（暫定・字句一致方式）のみ**の実測であり、
//! クロスエンコーダの効果を主張するものではない。
//!
//! Issue #337 で `nl_qa.rs` の文書テキスト生成方式を再設計（正解概念の非流暢な
//! 追記フレーズを全廃し、全概念を流暢な自然文へ埋め込む方式へ変更）したため、
//! 以下の固定値は再設計後の実測値に更新済み（`docs/design/
//! rerank-recall-regression.md`「Issue #337」節参照）。

#[path = "fixtures/nl_qa.rs"]
mod nl_qa;

use engine::rerank::LexicalOverlapReranker;

const SEED: u64 = 0x1234_5678;
const NUM_DOCS: usize = 200;
const NUM_QUERIES: usize = 20;

#[test]
fn nl_qa_fixture_generation_is_deterministic() {
    let (docs_a, qa_a) = nl_qa::generate_nl_corpus(SEED, NUM_DOCS, NUM_QUERIES);
    let (docs_b, qa_b) = nl_qa::generate_nl_corpus(SEED, NUM_DOCS, NUM_QUERIES);
    assert_eq!(docs_a.len(), docs_b.len());
    for (a, b) in docs_a.iter().zip(docs_b.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.text, b.text);
        assert_eq!(a.keywords, b.keywords);
        assert_eq!(a.vector, b.vector);
    }
    assert_eq!(qa_a.len(), qa_b.len());
    for (a, b) in qa_a.iter().zip(qa_b.iter()) {
        assert_eq!(a.query_text, b.query_text);
        assert_eq!(a.query_vector, b.query_vector);
        assert_eq!(a.correct, b.correct);
    }
}

#[test]
fn nl_qa_fixture_corpus_within_limits() {
    let (docs, qa) = nl_qa::generate_nl_corpus(SEED, NUM_DOCS, NUM_QUERIES);
    nl_qa::assert_corpus_within_limits(&docs);
    assert!(!qa.is_empty(), "QA セットが空になってはならない");
}

/// `LexicalOverlapReranker`（暫定方式）実測値の回帰トラッキング（層 A・固定値
/// アサーション。`rerank_recall.rs::rerank_recall_large_scale_regression` と同じ
/// 「実測値をテストコードへ焼き込み、変化を検知したら意図的な変更かを確認する」
/// 方針）。この自然言語 fixture は文書生成に variant 0、クエリ生成に別 variant を
/// 使う設計（`nl_qa.rs` モジュールドキュメント参照）により字句一致では拾いにくい
/// クエリ・文書ペアを意図的に作っており、実測は字句一致リランキングが
/// after_hits20 < baseline_hits20（43 < 46）と *悪化* することを示す
/// （`pool_ceiling_hits20`＝79 の改善余地に対して字句信号が逆効果になりうる
/// 一例。Issue #337 の fixture 再設計〔正解概念を流暢な自然文へ埋め込む方式へ
/// 変更。文書テキストの語数増加により `ceil20` は 79 のまま・baseline_hits20 は
/// 43→46 へ変動——正解集合そのものを決める kw_set/QA 抽選用 rng ストリームは
/// 文テンプレート選択用ストリームと分離し不変に保っている（`nl_qa.rs::
/// generate_nl_corpus` 参照）〕後もこの傾向自体は変わらない。字句一致リランカーの
/// 弱点そのものが Issue #333 が方式変更〔クロスエンコーダ〕を検討する動機であり、
/// `docs/design/rerank-recall-regression.md`「Issue #337」節にも記録する）。
#[test]
fn nl_qa_fixture_lexical_reranker_recall_regression() {
    let (docs, qa) = nl_qa::generate_nl_corpus(SEED, NUM_DOCS, NUM_QUERIES);
    let reranker = LexicalOverlapReranker::default();
    let result = nl_qa::measure_recall_with_reranker(&docs, &qa, &reranker);

    assert_eq!(result.baseline_hits20, 46);
    assert_eq!(result.after_hits20, 43);
    assert_eq!(result.pool_ceiling_hits20, 79);
    assert_eq!(result.ceil20, 79);
}
