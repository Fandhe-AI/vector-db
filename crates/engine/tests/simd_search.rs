//! `simd_search.rs::SimdSearchProvider` の結合テスト（TASK-126・対象ビヘイビア:
//! CORE-3, CORE-4, CORE-5・SEARCH-4）。
//!
//! 両 provider（`CpuScalarProvider`・`SimdSearchProvider`）とも総当たり実装のため、
//! 決定的シードの合成データに対して Top-k 選出集合・順序が完全一致することを検証する
//! （近似検索ではなく、参照実装との等価性そのものを主張する）。
//!
//! - `dim < 16` のケースはスコア値も bit 単位で一致することまで検証する
//!   （`simd_search.rs::dot_vectorized` のドキュメント参照）。
//! - `dim >= 16`（内積の加算順序が異なりうる）のケースは、境界（k 番目と k+1 番目）の
//!   参照スコアに十分なマージンを確保したうえで、選出される id 集合・順序の一致のみを
//!   主張する（浮動小数点の非結合性により生スコアが bit 単位で一致しない場合があるため）。
//!
//! `EngineCore` 経由の CORE-1/2/13 回帰は `tests/vector_core.rs` が既定構成
//! （`SimdSearchProvider`）でそのまま担保する（本ファイルは provider 単体の契約検証）。

use engine::kernel::{CpuScalarProvider, KernelError, SearchHit, SearchInput, SearchProvider};
use engine::simd_search::SimdSearchProvider;

/// テスト専用の決定的シード xorshift64*（`benches/harness/rng.rs` と同系だが、
/// 結合テストは bench クレートに依存しないため個別に持つ）。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32
    }

    fn next_vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_f32() * 2.0 - 1.0).collect()
    }
}

/// スカラー逐次和の内積（`kernel.rs::CpuScalarProvider` 内部実装と同じ加算順序）。
/// [`assert_top_k_matches`] が境界マージンチェック用の参照スコアを全行分計算するのに使う。
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// 決定的シードで `(ids, vectors, query)` を合成する。
fn synth_case(seed: u64, n: usize, dim: usize) -> (Vec<u64>, Vec<f32>, Vec<f32>) {
    let mut rng = Rng::new(seed);
    let ids: Vec<u64> = (0..n as u64).collect();
    let mut vectors = Vec::with_capacity(n * dim);
    for _ in 0..n {
        vectors.extend(rng.next_vector(dim));
    }
    let query = rng.next_vector(dim);
    (ids, vectors, query)
}

/// 境界（k 番目・k+1 番目）の参照スコア差が十分なマージンを持つことを確認したうえで
/// 両 provider の Top-k を比較する（浮動小数点誤差で境界の勝敗が入れ替わらないことの
/// 事前条件チェック）。マージン不足の場合はテスト構成自体の誤りとして panic させる
/// （fail-closed: 曖昧な入力でテストの green を偽装しない）。
fn assert_top_k_matches(ids: &[u64], vectors: &[f32], dim: u32, query: &[f32], k: usize) {
    let scalar_input = SearchInput {
        ids,
        vectors,
        dim,
        query,
        k,
    };
    let simd_input = SearchInput {
        ids,
        vectors,
        dim,
        query,
        k,
    };
    let scalar = CpuScalarProvider.search(scalar_input).expect("scalar ok");
    let simd = SimdSearchProvider.search(simd_input).expect("simd ok");

    let dim_usize = dim as usize;
    if dim_usize < 16 {
        assert_eq!(scalar, simd, "dim<16 must match bit-for-bit (ids/scores)");
        return;
    }

    // 境界マージンチェック: 全行の参照スコア（スカラー逐次和 `dot`）を降順に並べ、
    // k 番目と k+1 番目の差が丸め誤差より十分大きいことを確認してから id 集合・
    // 順序を比較する。マージンが小さすぎると、加算順序差に起因する丸め誤差だけで
    // 選出集合の境界が入れ替わりうるため（テスト構成自体の誤り。fail-closed: 曖昧な
    // 入力でテストの green を偽装しない）。
    let row_count = ids.len();
    if row_count > k {
        let mut all_scores: Vec<f32> = (0..row_count)
            .map(|i| {
                let start = i * dim_usize;
                let end = start + dim_usize;
                dot(&vectors[start..end], query)
            })
            .collect();
        all_scores.sort_by(|a, b| b.total_cmp(a));
        let boundary = all_scores[k - 1];
        let next = all_scores[k];
        let margin = boundary - next;
        // 加算順序差に起因する誤差は `chunks_exact(8)` 化で高々 O(dim * f32::EPSILON *
        // スコア規模) 程度（各要素の丸め誤差が線形に積み上がる上界）。安全係数 16 倍を
        // 掛けて、この見積りより明確に大きい境界差だけを「安全」とみなす
        // （1e-3 のような固定相対許容度は dim・スコア規模に依存せず恣意的なため使わない）。
        let boundary_tolerance =
            f32::EPSILON * (dim_usize as f32) * boundary.abs().max(next.abs()).max(1.0) * 16.0;
        assert!(
            margin > boundary_tolerance,
            "test case has insufficient score margin at k-th boundary \
             (margin={margin}, boundary_tolerance={boundary_tolerance}); pick a different seed/k"
        );
    }

    let ids_match: Vec<u64> = simd.iter().map(|h| h.id).collect();
    let expected_ids: Vec<u64> = scalar.iter().map(|h| h.id).collect();
    assert_eq!(ids_match, expected_ids, "top-k id order must match");
    for (s, m) in scalar.iter().zip(simd.iter()) {
        assert_eq!(s.id, m.id);
        assert!(
            (s.score - m.score).abs() <= s.score.abs().max(m.score.abs()) * 1e-3 + 1e-4,
            "score mismatch beyond fp accumulation-order tolerance: scalar={:?} simd={:?}",
            s,
            m
        );
    }
}

#[test]
fn matches_scalar_reference_for_various_n_dim_k() {
    // n・dim（8 の倍数／非倍数を両方含む）・k（k > n・通常ケース）の組み合わせ。
    let cases: &[(u64, usize, usize, usize)] = &[
        (1, 5, 3, 2),
        (2, 5, 3, 10), // k > n
        (3, 50, 8, 5), // dim = 8 の丁度境界
        (4, 50, 9, 5), // dim = 8 の 1 つ上（remainder 1）
        (5, 200, 15, 7),
        (6, 200, 16, 7), // dim = 16 の丁度境界（複数チャンクへ入る最小）
        (7, 500, 33, 12),
        (8, 500, 64, 12),
    ];
    for &(seed, n, dim, k) in cases {
        let (ids, vectors, query) = synth_case(seed, n, dim);
        assert_top_k_matches(&ids, &vectors, dim as u32, &query, k);
    }
}

#[test]
fn matches_scalar_reference_with_duplicate_vectors_tie() {
    // 同一ベクトルの複製による同点を、選出段のタイブレーク（id 昇順）が両 provider で
    // 一致することの確認に使う。dim < 16（`chunks_exact(8)` のフルチャンクが 1 個以下）を
    // 選ぶ: `dot_vectorized`
    // のドキュメントどおりこの範囲でのみスカラー逐次和と bit 単位で一致する
    // （複数チャンクにまたがる dim では、値が同一でも加算順序自体が異なるため
    // 一致しない）。
    let dim = 8usize;
    let mut rng = Rng::new(42);
    let base = rng.next_vector(dim);
    let n = 6;
    let ids: Vec<u64> = (0..n as u64).collect();
    let mut vectors = Vec::with_capacity(n * dim);
    for _ in 0..n {
        vectors.extend(base.clone());
    }
    let query = rng.next_vector(dim);
    let k = 3;

    let scalar_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k,
    };
    let simd_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k,
    };
    let scalar = CpuScalarProvider.search(scalar_input).expect("scalar ok");
    let simd = SimdSearchProvider.search(simd_input).expect("simd ok");
    assert_eq!(scalar, simd);
    // 同点タイブレークで id 昇順の最小 k 件（0,1,2）が選ばれるはず。
    let selected_ids: Vec<u64> = simd.iter().map(|h| h.id).collect();
    assert_eq!(selected_ids, vec![0, 1, 2]);
}

#[test]
fn matches_scalar_reference_with_non_finite_rows_mixed_in() {
    let ids = [1u64, 2, 3, 4, 5];
    let dim = 4usize;
    // id=3 の行に NaN、id=5 の行に Inf を混入させる。
    #[rustfmt::skip]
    let vectors = [
        1.0, 0.0, 0.0, 0.0,
        2.0, 0.0, 0.0, 0.0,
        f32::NAN, 0.0, 0.0, 0.0,
        3.0, 0.0, 0.0, 0.0,
        f32::INFINITY, 0.0, 0.0, 0.0,
    ];
    let query = [1.0, 0.0, 0.0, 0.0];
    let scalar_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k: 3,
    };
    let simd_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k: 3,
    };
    let scalar = CpuScalarProvider.search(scalar_input).expect("scalar ok");
    let simd = SimdSearchProvider.search(simd_input).expect("simd ok");
    assert_eq!(scalar, simd);
    assert_eq!(
        simd,
        vec![
            SearchHit { id: 4, score: 3.0 },
            SearchHit { id: 2, score: 2.0 },
            SearchHit { id: 1, score: 1.0 },
        ]
    );
}

#[test]
fn multi_thread_path_matches_scalar_reference_at_scale() {
    // `simd_search.rs::MIN_ROWS_PER_THREAD` を超える規模でマルチスレッド経路を
    // 実際に使わせたうえで、スカラー参照実装と一致することを確認する（CORE-3・SEARCH-4）。
    let dim = 32usize;
    let n = 5000;
    let (ids, vectors, query) = synth_case(99, n, dim);
    assert_top_k_matches(&ids, &vectors, dim as u32, &query, 20);
}

#[test]
fn error_contract_matches_scalar_reference() {
    let ids = [1u64];
    let vectors = [1.0f32, 0.0];

    // 次元不一致
    let bad_query = [1.0f32, 0.0, 0.0];
    let scalar_err = CpuScalarProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &bad_query,
            k: 1,
        })
        .unwrap_err();
    let simd_err = SimdSearchProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &bad_query,
            k: 1,
        })
        .unwrap_err();
    assert_eq!(scalar_err, simd_err);
    assert_eq!(
        simd_err,
        KernelError::DimMismatch {
            expected: 2,
            found: 3
        }
    );

    // 非有限クエリ
    let nan_query = [f32::NAN, 0.0];
    let scalar_err = CpuScalarProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &nan_query,
            k: 1,
        })
        .unwrap_err();
    let simd_err = SimdSearchProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &nan_query,
            k: 1,
        })
        .unwrap_err();
    assert_eq!(scalar_err, simd_err);
    assert_eq!(simd_err, KernelError::NonFiniteQuery);

    // 空入力・k=0
    let empty_ids: [u64; 0] = [];
    let empty_vectors: [f32; 0] = [];
    let query = [1.0f32, 0.0];
    let scalar_hits = CpuScalarProvider
        .search(SearchInput {
            ids: &empty_ids,
            vectors: &empty_vectors,
            dim: 2,
            query: &query,
            k: 5,
        })
        .expect("scalar ok");
    let simd_hits = SimdSearchProvider
        .search(SearchInput {
            ids: &empty_ids,
            vectors: &empty_vectors,
            dim: 2,
            query: &query,
            k: 5,
        })
        .expect("simd ok");
    assert!(scalar_hits.is_empty());
    assert!(simd_hits.is_empty());

    let simd_k0 = SimdSearchProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 0,
        })
        .expect("simd ok");
    assert!(simd_k0.is_empty());
}
