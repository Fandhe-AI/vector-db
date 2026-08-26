//! `benches/harness/contrast.rs`（usearch アダプタ。TASK-127 CORE-5・Issue #176）の
//! 小規模スモークテスト。
//!
//! `contrast-bench` feature 限定（`#![cfg(...)]`）。CI（`--all-features`）では実行され、
//! feature 無効のローカル `cargo test` では本ファイル自体が空になる
//! （`Cargo.toml` の `contrast-bench` feature コメント参照）。
//!
//! 小規模データで `ContrastIndex`（usearch の総当たり `exact_search`）の Top-k が
//! `CpuScalarProvider`（厳密最近傍の参照実装。`kernel.rs`）と一致すること
//! （`recall_at_k == 1.0`）を検証し、`contrast_bench.rs` の測定条件（大規模データ）へ
//! 進む前に配線ミス（metric 取り違え・key 対応ずれ）を検出する。

#![cfg(feature = "contrast-bench")]

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::accept::recall_at_k;
use harness::contrast::ContrastIndex;
use harness::rng::DeterministicRng;

use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

const ROW_COUNT: usize = 500;
const DIM: usize = 16;
const TOP_K: usize = 5;

#[test]
fn contrast_index_topk_matches_cpu_scalar_provider() {
    let mut rng = DeterministicRng::new(7);
    let ids: Vec<u64> = (0..ROW_COUNT as u64).collect();
    let mut vectors = Vec::with_capacity(ROW_COUNT * DIM);
    for _ in 0..ROW_COUNT {
        vectors.extend(rng.next_vector(DIM));
    }
    let query = rng.next_vector(DIM);

    let contrast = ContrastIndex::build(&ids, &vectors, DIM).expect("contrast index builds");
    let contrast_topk = contrast
        .search(&query, TOP_K)
        .expect("contrast search succeeds");

    let reference = CpuScalarProvider;
    let expected: Vec<u64> = reference
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &query,
            k: TOP_K,
        })
        .expect("reference search succeeds")
        .into_iter()
        .map(|hit| hit.id)
        .collect();

    let recall = recall_at_k(&expected, &contrast_topk).expect("non-empty expected top-k");
    assert!(
        (recall - 1.0).abs() < f64::EPSILON,
        "contrast index top-k must exactly match the exhaustive reference (recall={recall})"
    );
}
