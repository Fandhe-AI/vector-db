//! ベクトル化＋マルチスレッド並列の総当たり Top-k 検索 provider（TASK-126・対象
//! ビヘイビア: CORE-3, CORE-4, CORE-5・SEARCH-4）。
//!
//! `core.rs` の [`crate::core::EngineCore::open`] から `kernel.rs::SearchProvider` の
//! 既定実装として注入される（`core.rs` が可視行のみに縮約した
//! [`crate::kernel::SearchInput`] を受け取る構造は不変。可視性判定は本 provider の
//! 責務外）。エラー契約・入力検証は `kernel.rs::CpuScalarProvider` と同一
//! （[`crate::kernel::KernelError`] を共用し、`core.rs` 側の Top-k 契約検証とも整合）。
//!
//! 依存最小方針（`.claude/rules/dependency-policy.md`）に従い新規クレートは追加しない。
//! ベクトル化は `unsafe`・intrinsics を使わず `chunks_exact(8)` ＋複数アキュムレータの
//! 安全な形にとどめ、release ビルドでの自動ベクトル化に委ねる。並列化は
//! `std::thread::scope`（stable）による行範囲分割のみで、外部からのスレッド数・
//! カーネル選択の上書き機構は設けない（CORE-12 の方針）。

use crate::kernel::{KernelError, SearchHit, SearchInput, SearchProvider, TopKSelector};

/// クエリ 1 件あたりのスレッド数上限（CORE-3 の並列度の趣旨に対応）。
///
/// この上限は **クエリ単独** に対するものであり、同時実行中のクエリ間でスレッド数を
/// 共有・調停するグローバルなプールではない（`std::thread::scope` はクエリごとに
/// スレッドを生成して合流する）。複数クエリが同時に到達した場合、OS スレッド総数は
/// クエリ数 × 本上限まで増え得る。グローバルなスレッド予算・共有プールの導入は
/// スコープ外（TASK-126 の範囲外。後続タスクで検討）。
const MAX_THREADS_PER_QUERY: usize = 16;

/// スレッド分割の下限行数。担当行数がこれを下回るワーカーを作らないことで、
/// 小規模テーブルでの無用なスレッド生成コストを避ける（CORE-3 の「行数が小さい場合は
/// 単一スレッドへ縮退」という設計判断に対応）。
const MIN_ROWS_PER_THREAD: usize = 1024;

/// ベクトル化＋マルチスレッド並列の総当たり Top-k provider（TASK-126）。
///
/// 総当たり（exhaustive）である点は [`crate::kernel::CpuScalarProvider`] と同じで、
/// 近似検索ではない。したがって選出される Top-k 集合はスカラー参照実装と一致する
/// （浮動小数点の加算順序差により個々のスコア値が bit 単位で一致しない場合はあるが、
/// 集合・順序は一致する。`crates/engine/tests/simd_search.rs` で検証）。
#[derive(Debug, Default, Clone, Copy)]
pub struct SimdSearchProvider;

impl SearchProvider for SimdSearchProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
        let dim = input.dim as usize;
        if input.query.len() != dim {
            return Err(KernelError::DimMismatch {
                expected: input.dim,
                found: input.query.len(),
            });
        }
        // untrusted なクエリ入力の拒否契約は `CpuScalarProvider` と同一
        // （`kernel.rs::CpuScalarProvider::search` のコメント参照）。
        if input.query.iter().any(|v| !v.is_finite()) {
            return Err(KernelError::NonFiniteQuery);
        }
        if input.k == 0 || input.ids.is_empty() {
            return Ok(Vec::new());
        }

        let row_count = input.ids.len();
        let thread_count = thread_count_for(row_count);

        let partials: Vec<TopKSelector> = if thread_count <= 1 {
            vec![search_range(
                input.ids,
                input.vectors,
                dim,
                input.query,
                input.k,
            )]
        } else {
            // 行範囲を均等分割し、各スレッドが担当範囲だけで部分 Top-k を選出する。
            // `TopKSelector` は事前確保をせず push 時に自然成長するため、中間バッファは
            // 「実際に保持する要素数が高々 k」という意味で有界（無制限 `with_capacity`
            // 禁止。coding-rust.md・`kernel.rs::TopKSelector::new` 参照）。
            let rows_per_thread = row_count.div_ceil(thread_count);
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(thread_count);
                let mut row_start = 0usize;
                while row_start < row_count {
                    let row_end = row_start.saturating_add(rows_per_thread).min(row_count);
                    // `get()` による境界安全アクセス。通常はここで `None` にはならない
                    // （`row_start..row_end` は `row_count` 以内に収まるよう構築している）が、
                    // 添字アクセスによる panic を避けるため防御的に空スライスへ縮退させる
                    // （fail-closed: 万一の不整合時も該当範囲を黙ってスキップする）。
                    let ids_slice = input.ids.get(row_start..row_end).unwrap_or(&[]);
                    let vec_start = row_start.saturating_mul(dim);
                    let vec_end = row_end.saturating_mul(dim);
                    let vectors_slice = input.vectors.get(vec_start..vec_end).unwrap_or(&[]);
                    let query = input.query;
                    let k = input.k;
                    handles.push(
                        scope.spawn(move || search_range(ids_slice, vectors_slice, dim, query, k)),
                    );
                    row_start = row_end;
                }
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| TopKSelector::new(0)))
                    .collect()
            })
        };

        // 部分結果を最終セレクタへ再投入してマージする。`TopKSelector` は
        // `kernel.rs::CpuScalarProvider` と共通の選出規約（スコア降順・同点 id
        // 昇順・非有限値除外）を使うため、分割数・スレッド数に依らず選出集合・
        // 順序が決定的になる（CORE-3・SEARCH-4）。
        let mut merged = TopKSelector::new(input.k);
        for partial in partials {
            for hit in partial.into_sorted_vec() {
                merged.push(hit);
            }
        }
        Ok(merged.into_sorted_vec())
    }
}

/// 利用可能な並列度を [`MAX_THREADS_PER_QUERY`] でクランプし、担当行数が
/// [`MIN_ROWS_PER_THREAD`] を割り込まない範囲に収める。
fn thread_count_for(row_count: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_THREADS_PER_QUERY);
    let by_rows = row_count / MIN_ROWS_PER_THREAD;
    available.min(by_rows).max(1)
}

/// 行範囲 `[0, ids.len())`（呼び出し元が担当範囲に絞り込み済み）に対して総当たり
/// Top-k を選出する。単一スレッド経路・並列ワーカーの両方から呼ばれる共通処理。
fn search_range(ids: &[u64], vectors: &[f32], dim: usize, query: &[f32], k: usize) -> TopKSelector {
    let mut selector = TopKSelector::new(k);
    for (idx, &id) in ids.iter().enumerate() {
        let start = idx.saturating_mul(dim);
        let end = start.saturating_add(dim);
        let Some(vector) = vectors.get(start..end) else {
            // `kernel.rs::CpuScalarProvider::search` と同じ理由（アリーナ側の不変条件
            // 破れに対する fail-closed なスキップ）。
            continue;
        };
        let score = dot_vectorized(vector, query);
        if !score.is_finite() {
            // 格納ベクトルの NaN/Inf 混入に対する fail-closed な除外
            // （`kernel.rs::CpuScalarProvider::search` と同じ理由）。
            continue;
        }
        selector.push(SearchHit { id, score });
    }
    selector
}

/// 内積（dot product）を 8 要素幅のアキュムレータ配列でベクトル化して計算する。
/// `unsafe`・intrinsics は使わず、release ビルド（opt-level 3）での自動ベクトル化
/// （NEON/AVX2 相当）に委ねる安全な形にとどめる（依存最小方針・coding-rust.md
/// 「`unsafe` は原則禁止」準拠）。
///
/// 添字アクセスは使わず `chunks_exact` ＋ イテレータ合成のみで書く（coding-rust.md の
/// untrusted 入力に対する添字アクセス禁止方針を、格納済みベクトル経路にも一貫して
/// 適用する）。
///
/// `dim < 16`（`chunks_exact(8)` が生成するフルチャンクが 1 個以下）の場合、本関数は
/// `kernel.rs::dot`（スカラー実装の左から右への逐次加算）と bit 単位で同一の結果を返す
/// （アキュムレータ各要素がちょうど 1 個の積しか保持せず、最終還元の加算順序が
/// スカラー実装の逐次和と一致するため）。`dim >= 16` では複数チャンクにまたがる
/// 加算順序がスカラー実装と異なるため、浮動小数点演算の非結合性により値が
/// bit 単位では一致しない場合がある（総当たりであるため選出される集合・順序は
/// 一致する。`crates/engine/tests/simd_search.rs` 参照）。
fn dot_vectorized(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let mut chunks_a = a.chunks_exact(8);
    let mut chunks_b = b.chunks_exact(8);
    for (chunk_a, chunk_b) in chunks_a.by_ref().zip(chunks_b.by_ref()) {
        for (acc_lane, (&x, &y)) in acc.iter_mut().zip(chunk_a.iter().zip(chunk_b.iter())) {
            *acc_lane += x * y;
        }
    }
    let mut sum = 0f32;
    for &lane in acc.iter() {
        sum += lane;
    }
    for (&x, &y) in chunks_a.remainder().iter().zip(chunks_b.remainder().iter()) {
        sum += x * y;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_vectorized_matches_sequential_sum_for_small_dims() {
        // dim < 16（`chunks_exact(8)` のフルチャンクが 1 個以下）は本関数のドキュメント
        // どおり bit 単位でスカラー逐次和と一致するはず。
        for dim in [0usize, 1, 7, 8, 9, 15] {
            let a: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.5 + 1.0).collect();
            let b: Vec<f32> = (0..dim).map(|i| (i as f32) * -0.25 + 2.0).collect();
            let sequential: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
            assert_eq!(
                dot_vectorized(&a, &b),
                sequential,
                "dim={dim} must match bit-for-bit"
            );
        }
    }

    #[test]
    fn top_k_returns_highest_dot_product_scores() {
        let ids = [1u64, 2, 3, 4];
        let vectors = [1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
        };
        let hits = SimdSearchProvider.search(input).expect("search ok");
        assert_eq!(
            hits,
            vec![
                SearchHit { id: 4, score: 3.0 },
                SearchHit { id: 2, score: 2.0 }
            ]
        );
    }

    #[test]
    fn non_finite_query_is_rejected() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        for query in [
            [f32::NAN, 0.0],
            [f32::INFINITY, 0.0],
            [0.0, f32::NEG_INFINITY],
        ] {
            let input = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 1,
            };
            let err = SimdSearchProvider.search(input).unwrap_err();
            assert_eq!(err, KernelError::NonFiniteQuery, "query={query:?}");
        }
    }

    #[test]
    fn dim_mismatch_query_is_rejected() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        let query = [1.0, 0.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 1,
        };
        let err = SimdSearchProvider.search(input).unwrap_err();
        assert_eq!(
            err,
            KernelError::DimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn k_zero_returns_empty() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 0,
        };
        let hits = SimdSearchProvider.search(input).expect("search ok");
        assert!(hits.is_empty());
    }

    #[test]
    fn multi_thread_path_is_deterministic_across_repeated_runs() {
        // MIN_ROWS_PER_THREAD を超える規模でマルチスレッド経路を実際に使わせ、
        // 同一入力を 2 回実行して結果が完全一致することを確認する（CORE-3・SEARCH-4）。
        let dim = 16usize;
        let row_count = MIN_ROWS_PER_THREAD * 4 + 7;
        let mut ids = Vec::with_capacity(row_count);
        let mut vectors = Vec::with_capacity(row_count * dim);
        for i in 0..row_count {
            ids.push(i as u64);
            for d in 0..dim {
                vectors.push(((i * dim + d) % 97) as f32 * 0.01 - 0.5);
            }
        }
        let query: Vec<f32> = (0..dim).map(|d| (d as f32) * 0.1 - 0.5).collect();

        let run = || {
            let input = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k: 20,
            };
            SimdSearchProvider.search(input).expect("search ok")
        };
        let first = run();
        let second = run();
        assert_eq!(first, second);
        assert_eq!(first.len(), 20);
    }

    // object-safety の固定（CORE-13）: `Box<dyn SearchProvider>` として保持できること。
    #[test]
    fn provider_is_object_safe() {
        let _boxed: Box<dyn SearchProvider> = Box::new(SimdSearchProvider);
    }
}
