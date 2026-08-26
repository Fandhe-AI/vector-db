//! `benches/harness/scalar_reference.rs`（真のスカラー逐次和による Top-k 総当たり
//! 参照。Issue #177）の回帰テスト。
//!
//! `simd_bench.rs` 自体は時間依存のためこのテストからは実行しない
//! （`tests/bench_accept.rs` と同様、実測タイマーに依存しない判定ロジックのみを
//! `#[path]` で取り込み `cargo test`（`make ci` 対象）で検証する）。

// `harness` は独立したコンパイル単位（cargo bench バイナリ）から取り込まれる共有
// ソース。本テストが実際に使う項目のみで、未到達の `pub` 項目は `dead_code`
// 警告になりうるためモジュール全体を対象に許容する（`tests/bench_accept.rs` と
// 同一方針。`harness/mod.rs` 自体は変更しない）。
#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::rng::DeterministicRng;
use harness::scalar_reference::top_k_ids_scalar;
use harness::stats::BenchError;

use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

const DIM: usize = 4;

#[test]
fn matches_cpu_scalar_provider_on_integer_vectors_without_rounding_error() {
    // 整数値ベクトルは浮動小数点丸め誤差を生まないため、`CpuScalarProvider`
    // （TASK-156 以降は SIMD カーネル経由）と本参照実装（常にスカラー逐次和）の
    // id 列が完全一致するはずである（丸め差が生じない条件での整合確認）。
    let ids: Vec<u64> = (0..8).collect();
    let vectors: Vec<f32> = (0..8)
        .flat_map(|row| (0..DIM).map(move |col| (row * DIM + col) as f32))
        .collect();
    let query: Vec<f32> = vec![1.0, 0.0, 2.0, 0.0];

    let expected: Vec<u64> = CpuScalarProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &query,
            k: 3,
        })
        .expect("search must succeed for well-formed input")
        .into_iter()
        .map(|hit| hit.id)
        .collect();

    let actual = top_k_ids_scalar(&ids, &vectors, DIM, &query, 3).expect("well-formed input");
    assert_eq!(expected, actual);
}

#[test]
fn matches_cpu_scalar_provider_on_deterministic_random_input() {
    // 決定的乱数の小規模ケースでも id 集合が一致すること（Recall@k = 1.0 相当）。
    // 同点が起きにくいよう次元・行数を小さくとる。
    let mut rng = DeterministicRng::new(7);
    let ids: Vec<u64> = (0..64).collect();
    let mut vectors = Vec::with_capacity(64 * DIM);
    for _ in 0..64 {
        vectors.extend(rng.next_vector(DIM));
    }
    let query = rng.next_vector(DIM);

    let expected: Vec<u64> = CpuScalarProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &query,
            k: 5,
        })
        .expect("search must succeed for well-formed input")
        .into_iter()
        .map(|hit| hit.id)
        .collect();

    let actual = top_k_ids_scalar(&ids, &vectors, DIM, &query, 5).expect("well-formed input");
    assert_eq!(expected, actual);
}

#[test]
fn ties_break_by_ascending_id() {
    // 重複ベクトル（同点スコア）は id 昇順で選ばれる（`TopKSelector` と同じ選出規約）。
    let ids: Vec<u64> = vec![30, 10, 20];
    let vectors: Vec<f32> = vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
    let query: Vec<f32> = vec![1.0, 0.0];

    let actual = top_k_ids_scalar(&ids, &vectors, 2, &query, 3).expect("well-formed input");
    assert_eq!(actual, vec![10, 20, 30]);
}

#[test]
fn excludes_rows_with_non_finite_score() {
    // NaN を含む行はスコアが非有限になり除外される（`TopKSelector::push` と同一規約）。
    let ids: Vec<u64> = vec![1, 2];
    let vectors: Vec<f32> = vec![f32::NAN, 0.0, 1.0, 0.0];
    let query: Vec<f32> = vec![1.0, 0.0];

    let actual = top_k_ids_scalar(&ids, &vectors, 2, &query, 2).expect("well-formed input");
    assert_eq!(actual, vec![2]);
}

#[test]
fn boundary_k_zero_k_greater_than_n_and_empty_input() {
    let ids: Vec<u64> = vec![1, 2];
    let vectors: Vec<f32> = vec![1.0, 0.0, 0.0, 1.0];
    let query: Vec<f32> = vec![1.0, 0.0];

    assert!(top_k_ids_scalar(&ids, &vectors, 2, &query, 0)
        .expect("well-formed input")
        .is_empty());

    let actual = top_k_ids_scalar(&ids, &vectors, 2, &query, 10).expect("well-formed input");
    assert_eq!(actual, vec![1, 2]);

    let empty_ids: Vec<u64> = Vec::new();
    let empty_vectors: Vec<f32> = Vec::new();
    assert!(top_k_ids_scalar(&empty_ids, &empty_vectors, 2, &query, 3)
        .expect("well-formed input")
        .is_empty());
}

#[test]
fn rejects_query_length_mismatch() {
    let ids: Vec<u64> = vec![1];
    let vectors: Vec<f32> = vec![1.0, 0.0];
    let query: Vec<f32> = vec![1.0, 0.0, 0.0]; // dim=2 のはずが 3 要素

    let err = top_k_ids_scalar(&ids, &vectors, 2, &query, 1).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

#[test]
fn rejects_vectors_length_mismatch() {
    let ids: Vec<u64> = vec![1, 2];
    let vectors: Vec<f32> = vec![1.0, 0.0]; // ids.len() * dim = 4 のはずが 2 要素
    let query: Vec<f32> = vec![1.0, 0.0];

    let err = top_k_ids_scalar(&ids, &vectors, 2, &query, 1).unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}
