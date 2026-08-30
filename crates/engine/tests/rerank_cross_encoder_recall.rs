//! Issue #333（SEARCH-7 方式変更）: `OnnxCrossEncoderBackend`（`cross-encoder`
//! feature 限定・実 ONNX 推論）による自然言語 fixture（`tests/fixtures/nl_qa.rs`）の
//! opt-in 実測ハーネス。`bench-tier`（TASK-116）と同じ位置づけ——常駐リソース
//! （実 ONNX モデルファイル・`tokenizer.json`・`ORT_DYLIB_PATH` で指す onnxruntime
//! 共有ライブラリ）を前提とするため CI には配線せず、`make rerank-cross-encoder-eval`
//! から運用者が明示実行する（`#[ignore]`。`GITHUB_ACTIONS` 下は拒否）。
//!
//! `docs/design/rerank-recall-regression.md`「Issue #333」節に実測手順・実測値を
//! 記録する。production コード（`crates/engine/src/`）は変更しない。

#![cfg(feature = "cross-encoder")]

use std::env;
use std::path::PathBuf;

use engine::rerank::{
    cross_encoder_onnx::OnnxCrossEncoderBackend, CrossEncoderConfig, CrossEncoderReranker,
};

#[path = "fixtures/nl_qa.rs"]
mod nl_qa;

const SEED: u64 = 0x1234_5678;
const NUM_DOCS: usize = 200;
const NUM_QUERIES: usize = 20;

/// 実測ハーネス本体。`#[ignore]` のため通常の `cargo test`（`make ci`）では
/// 走らない。`CROSS_ENCODER_MODEL_PATH`・`CROSS_ENCODER_TOKENIZER_PATH`
/// （実行時に `ORT_DYLIB_PATH` も必要。`OnnxCrossEncoderBackend::from_files` 冒頭
/// ドキュメント参照）が未設定の場合は明確なメッセージで fail する（明示実行時は
/// fail-closed。実測なしで「実測済み」を装わない。既存層 B の strict モードと
/// 同じ思想）。
#[test]
#[ignore]
fn cross_encoder_nl_qa_recall_measurement() {
    // `.github/workflows/*` へは配線しない手動・ローカル専用ハーネスであることを
    // 実行時にも強制する（Issue #303 の既存方針: 実測値の標準出力は GitHub Actions
    // 下では常に拒否する）。
    assert!(
        env::var_os("GITHUB_ACTIONS").is_none(),
        "cross_encoder_nl_qa_recall_measurement は GitHub Actions 上では実行しない（手動・ローカル専用の実測ハーネス）"
    );

    let model_path = env::var("CROSS_ENCODER_MODEL_PATH").unwrap_or_else(|_| {
        panic!(
            "CROSS_ENCODER_MODEL_PATH が未設定です。make rerank-cross-encoder-eval のヘルプ、または \
             docs/design/rerank-recall-regression.md「Issue #333」節の実測手順を参照してください。"
        )
    });
    let tokenizer_path = env::var("CROSS_ENCODER_TOKENIZER_PATH").unwrap_or_else(|_| {
        panic!(
            "CROSS_ENCODER_TOKENIZER_PATH が未設定です。make rerank-cross-encoder-eval のヘルプ、または \
             docs/design/rerank-recall-regression.md「Issue #333」節の実測手順を参照してください。"
        )
    });

    let max_seq_len: usize = 256;
    let backend = OnnxCrossEncoderBackend::from_files(
        &PathBuf::from(model_path),
        &PathBuf::from(tokenizer_path),
        max_seq_len,
    )
    .expect("OnnxCrossEncoderBackend::from_files failed; see ORT_DYLIB_PATH / model path");
    let cfg = CrossEncoderConfig::new(32, 200, max_seq_len).expect("valid CrossEncoderConfig");
    let reranker = CrossEncoderReranker::new(backend, cfg).expect("valid CrossEncoderReranker");

    let (docs, qa) = nl_qa::generate_nl_corpus(SEED, NUM_DOCS, NUM_QUERIES);

    // 決定性確認: 同一入力・同一モデルで 2 回実測し、一致することを確認する
    // （実装計画の検証方法 4「2 回実行して一致を確認」）。
    let first = nl_qa::measure_recall_with_reranker(&docs, &qa, &reranker);
    let second = nl_qa::measure_recall_with_reranker(&docs, &qa, &reranker);
    assert_eq!(
        first.after_hits20, second.after_hits20,
        "同一モデル・同一入力での 2 回実測が一致しない（決定性の契約違反）"
    );

    println!("cross-encoder nl_qa recall measurement (Issue #333 / SEARCH-7):");
    println!("  total_correct        = {}", first.total_correct);
    println!("  baseline_hits20      = {}", first.baseline_hits20);
    println!("  after_hits20         = {}", first.after_hits20);
    println!("  pool_hits100         = {}", first.pool_hits100);
    println!("  pool_hits200         = {}", first.pool_hits200);
    println!("  pool_ceiling_hits20  = {}", first.pool_ceiling_hits20);
    println!(
        "  ceil20/100/200       = {}/{}/{}",
        first.ceil20, first.ceil100, first.ceil200
    );
    match first.improvement_ratio() {
        Some(ratio) => println!("  improvement_ratio    = {ratio:.4}"),
        None => println!("  improvement_ratio    = None (headroom < 1% of ceil20)"),
    }

    // 契約のみを検証する（実モデル出力の固定値焼き込みは onnxruntime バージョン依存に
    // なるため行わない。実測値は docs/design/rerank-recall-regression.md へ記録する）。
    assert!(first.after_hits20 <= first.ceil20);
    assert!(first.baseline_hits20 <= first.ceil20);
    assert!(first.pool_ceiling_hits20 <= first.ceil20);
}
