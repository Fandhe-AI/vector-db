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
//!
//! 同時実行クエリ間の合計ワーカースレッド数は [`GLOBAL_WORKER_BUDGET`]（プロセス全体で
//! 共有する `AtomicUsize`）で調停する（Issue #34 レビュー指摘対応。security.md
//! 「不安全な設計｜無制限リソース確保（DoS）」）。予算を確保できない分は
//! スレッドを追加せず単一スレッド相当まで縮退させるのみで、行の選出対象からの除外は
//! 一切発生しない（[`SimdSearchProvider::search`] 参照）。

use crate::kernel::{KernelError, SearchHit, SearchInput, SearchProvider, TopKSelector};
use std::sync::atomic::{AtomicUsize, Ordering};

/// クエリ 1 件あたりのスレッド数上限（CORE-3 の並列度の趣旨に対応）。
const MAX_THREADS_PER_QUERY: usize = 16;

/// プロセス全体で共有するワーカースレッド予算。`MAX_THREADS_PER_QUERY` はクエリ単独の
/// 上限に過ぎず、同時実行クエリの数だけスレッド総数が積み上がり得るため、
/// 追加で確保する（呼び出し元スレッド自身の分を除く）ワーカー数をこの上限まで
/// プロセス全体で調停する。
const MAX_TOTAL_EXTRA_WORKER_THREADS: usize = 64;

/// [`MAX_TOTAL_EXTRA_WORKER_THREADS`] を上限に、現在確保中の追加ワーカー数を
/// プロセス全体で共有するカウンタ（`Ordering::SeqCst` で十分に保守的に同期する。
/// 検索のホットパスではなく調停用のカウンタのため、性能より単純さを優先する）。
static GLOBAL_WORKER_BUDGET: AtomicUsize = AtomicUsize::new(0);

/// [`GLOBAL_WORKER_BUDGET`] から `desired` 件までの追加ワーカー枠を確保し、確保できた
/// 件数（0 件の場合もあり得る）を返す `RAII` ガード。ガードの `Drop` で必ず解放するため、
/// 途中で `?` によるアーリーリターンやワーカー panic が起きても予算がリークしない。
struct WorkerBudgetGuard(usize);

impl WorkerBudgetGuard {
    fn acquire(desired: usize) -> Self {
        let mut reserved = 0usize;
        let _ = GLOBAL_WORKER_BUDGET.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
            let available = MAX_TOTAL_EXTRA_WORKER_THREADS.saturating_sub(cur);
            reserved = desired.min(available);
            Some(cur.saturating_add(reserved))
        });
        Self(reserved)
    }

    fn granted(&self) -> usize {
        self.0
    }
}

impl Drop for WorkerBudgetGuard {
    fn drop(&mut self) {
        GLOBAL_WORKER_BUDGET.fetch_sub(self.0, Ordering::SeqCst);
    }
}

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
        let desired_threads = thread_count_for(row_count);

        // `desired_threads <= 1` の場合は追加ワーカーを 1 つも生成しないため、
        // グローバル予算（`GLOBAL_WORKER_BUDGET`）を消費せず単一スレッド経路をそのまま使う。
        // `desired_threads > 1` の場合のみ、これから生成する追加ワーカー数分の予算確保を
        // 試みる。確保できた数（`effective_threads`）が同時実行クエリの多さにより
        // `desired_threads` を下回ることがあるが、その場合はパーティション数を単に
        // 減らすだけで、行を選出対象から除外することは一切ない（DoS 対策の縮退は
        // 「並列度を落とす」形でのみ行い、fail-open にはしない）。
        let _budget_guard;
        let effective_threads = if desired_threads <= 1 {
            _budget_guard = None;
            1usize
        } else {
            let guard = WorkerBudgetGuard::acquire(desired_threads);
            let granted = guard.granted().max(1);
            _budget_guard = Some(guard);
            granted
        };

        let partials: Vec<TopKSelector> =
            if effective_threads <= 1 {
                vec![search_range(
                    input.ids,
                    input.vectors,
                    0,
                    dim,
                    input.query,
                    input.k,
                )]
            } else {
                // 行範囲を均等分割し、各スレッドが担当範囲だけで部分 Top-k を選出する。
                // `TopKSelector` は事前確保をせず push 時に自然成長するため、中間バッファは
                // 「実際に保持する要素数が高々 k」という意味で有界（無制限 `with_capacity`
                // 禁止。coding-rust.md・`kernel.rs::TopKSelector::new` 参照）。
                //
                // `ids`・`vectors` はどちらも切り出さずスレッドへ丸ごと渡し、各ワーカーは
                // 担当範囲の絶対行インデックス（`row_start` を起点とする）で `vectors` を
                // 参照する（`search_range` 参照）。以前は `vectors.get(vec_start..vec_end)`
                // が `None` の場合に担当範囲全体（数千行規模になり得る）を空スライスへ縮退させ、
                // `CpuScalarProvider`（壊れた行 1 件だけを skip）と選出結果が食い違う
                // バグがあった（Issue #34 レビュー指摘対応）。行単位の `get()` 失敗のみを
                // skip する現在の形は、スレッド分割の有無・分割数に依らず
                // `CpuScalarProvider` と同じ候補集合を走査することを保証する。
                let rows_per_thread = row_count.div_ceil(effective_threads);
                std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(effective_threads);
                    let mut row_start = 0usize;
                    while row_start < row_count {
                        let row_end = row_start.saturating_add(rows_per_thread).min(row_count);
                        // `ids` の担当範囲は `row_start..row_end ⊆ 0..row_count == ids.len()` と
                        // なるよう構築しているため常に `Some` になるが、添字アクセスによる
                        // panic を避けるため `get()` で防御的に取り出す。
                        let ids_slice = input.ids.get(row_start..row_end).unwrap_or(&[]);
                        let vectors = input.vectors;
                        let query = input.query;
                        let k = input.k;
                        handles.push(scope.spawn(move || {
                            search_range(ids_slice, vectors, row_start, dim, query, k)
                        }));
                        row_start = row_end;
                    }
                    handles
                        .into_iter()
                        .map(|h| h.join())
                        .collect::<Result<Vec<TopKSelector>, _>>()
                })
                // ワーカーが panic した場合、その分の部分結果を欠いたまま `Ok` を返すと
                // 該当パーティションの行が黙って選出対象から消える（実質 fail-open。
                // Issue #34 レビュー指摘対応）。検索全体を失敗として呼び出し元へ伝播させる
                // （fail-closed。`KernelError::WorkerPanicked`）。
                .map_err(|_| KernelError::WorkerPanicked)?
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

/// `ids`（担当範囲だけに絞り込み済み）と `vectors`（絞り込まず全行分。`row_offset` を
/// 起点とする絶対行インデックスで参照する）に対して総当たり Top-k を選出する。
/// 単一スレッド経路・並列ワーカーの両方から呼ばれる共通処理。
///
/// `vectors` を担当範囲で事前に切り出さず絶対インデックスで参照するのは、`get()` に
/// よる境界チェックを行単位で行うためで、`kernel.rs::CpuScalarProvider::search`
/// （行単位で `vectors.get(start..end)` を試し、失敗した 1 行だけを skip する）と
/// 挙動を完全に一致させる（Issue #34 レビュー指摘対応。以前は担当範囲全体を
/// 事前に切り出しており、範囲内の 1 行でも `vectors` が不足しているとパーティション
/// 全体が消えて `CpuScalarProvider` と選出結果が食い違うバグがあった）。
fn search_range(
    ids: &[u64],
    vectors: &[f32],
    row_offset: usize,
    dim: usize,
    query: &[f32],
    k: usize,
) -> TopKSelector {
    let mut selector = TopKSelector::new(k);
    for (idx, &id) in ids.iter().enumerate() {
        let row = row_offset.saturating_add(idx);
        let start = row.saturating_mul(dim);
        let end = start.saturating_add(dim);
        let Some(vector) = vectors.get(start..end) else {
            // `kernel.rs::CpuScalarProvider::search` と同じ理由（アリーナ側の不変条件
            // 破れに対して、破損した当該行だけを候補から除外する。呼び出し全体は
            // `Ok` のまま。厳密な fail-closed ではない点は `kernel.rs` 側のコメント参照）。
            continue;
        };
        let score = dot_vectorized(vector, query);
        if !score.is_finite() {
            // 格納ベクトルの NaN/Inf 混入に対する除外
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

    #[test]
    fn search_range_with_nonzero_offset_skips_only_the_row_straddling_the_vectors_boundary() {
        // Issue #34 レビュー指摘の回帰テスト: 非 0 の `row_offset`（実際の並列パスで
        // 各ワーカーが受け取る値）を使い、担当範囲の途中で `vectors` が尽きる状況で
        // パーティション全体ではなく境界の 1 行だけが除外されることを、
        // `thread_count_for` の実際のスレッド数（CI 環境のコア数依存）に依存せず
        // 直接検証する。`row_offset = 0` だと `row = row_offset + idx` が旧実装の
        // `row = idx` と区別できないため、必ず非 0 の offset で境界をまたぐケースにする。
        let dim = 2usize;
        // 6 行分（0..6）の embedding のうち、末尾の行 5 だけ 1 要素分足りない
        // （`vectors.len() == 11` は `6 * dim == 12` に対して 1 要素不足）。
        let vectors = [
            1.0f32, 0.0, // row 0
            2.0, 0.0, // row 1
            3.0, 0.0, // row 2
            4.0, 0.0, // row 3
            5.0, 0.0, // row 4
            6.0, // row 5（1 要素欠落・境界外）
        ];
        let query = [1.0f32, 0.0];
        // 担当範囲は絶対行 4..6（id=100 → row 4, id=101 → row 5）。
        let ids = [100u64, 101];
        let row_offset = 4usize;

        let selector = search_range(&ids, &vectors, row_offset, dim, &query, 10);
        let hits = selector.into_sorted_vec();

        // row 4（id=100）は `vectors[8..10]` が範囲内 → score 5.0 で選出される。
        // row 5（id=101）は `vectors[10..12]` が範囲外 → 当該行だけ除外される
        // （パーティション全体が消える旧実装のバグなら空集合になり、このアサーションが
        // 落ちる）。
        assert_eq!(
            hits,
            vec![SearchHit {
                id: 100,
                score: 5.0
            }]
        );
    }

    #[test]
    fn multi_thread_path_matches_scalar_reference_when_vectors_are_truncated() {
        // レビューで実機再現された条件（dim=32, n=5000, 末尾 1 行分の vectors を
        // truncate）を縮小再現し、`SimdSearchProvider`（マルチスレッド経路を強制する
        // 規模）と `CpuScalarProvider` の選出集合が一致することを確認する。
        use crate::kernel::CpuScalarProvider;

        let dim = 16usize;
        let row_count = MIN_ROWS_PER_THREAD * 2 + 3;
        let mut ids = Vec::with_capacity(row_count);
        let mut vectors = Vec::with_capacity(row_count * dim);
        for i in 0..row_count {
            ids.push(i as u64);
            for d in 0..dim {
                vectors.push(((i * dim + d) % 89) as f32 * 0.01 - 0.4);
            }
        }
        // 末尾行 1 行分の vectors を truncate し、`ids.len() * dim != vectors.len()` の
        // 不変条件破れを再現する。
        vectors.truncate(vectors.len() - dim / 2);
        let query: Vec<f32> = (0..dim).map(|d| (d as f32) * 0.05 - 0.3).collect();

        let make_input = || SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &query,
            k: 20,
        };

        let simd_hits = SimdSearchProvider.search(make_input()).expect("simd ok");
        let scalar_hits = CpuScalarProvider.search(make_input()).expect("scalar ok");

        let simd_ids: Vec<u64> = simd_hits.iter().map(|h| h.id).collect();
        let scalar_ids: Vec<u64> = scalar_hits.iter().map(|h| h.id).collect();
        assert_eq!(
            simd_ids, scalar_ids,
            "SimdSearchProvider と CpuScalarProvider の選出 id 集合が一致すること"
        );
    }

    #[test]
    fn scoped_worker_panic_is_mapped_to_worker_panicked_error_without_repanicking() {
        // Issue #34 レビュー指摘の回帰テスト: `search()` のワーカー panic 処理が
        // 使う「各ハンドルを必ず `join()` してから `collect::<Result<_, _>>()` する」
        // パターンが実際に `KernelError::WorkerPanicked` へマップされ、
        // `std::thread::scope` 自体が（`join()` されなかった場合のように）
        // 再 panic しないことを検証する（`std::thread::scope` は panic した
        // スレッドが 1 つでも `join()` されずに残っていた場合のみ、スコープ終了時に
        // 自ら panic する。全ハンドルを `join()` 済みならその再 panic は起きない）。
        //
        // パニックメッセージが標準の panic hook 経由でテスト出力に出るのを避けるため、
        // 一時的に無音の hook へ差し替える（`search()` 本体のワーカー panic 経路と
        // 同一の join → collect → map_err の並びを、`SearchInput` を組み立てずに
        // 直接検証する）。
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result: Result<Vec<i32>, KernelError> = std::thread::scope(|scope| {
            let ok_handle = scope.spawn(|| 1i32);
            let panicking_handle = scope.spawn(|| -> i32 { panic!("induced for test") });
            vec![ok_handle, panicking_handle]
                .into_iter()
                .map(|h| h.join())
                .collect::<Result<Vec<i32>, _>>()
        })
        .map_err(|_| KernelError::WorkerPanicked);
        std::panic::set_hook(previous_hook);

        assert_eq!(result, Err(KernelError::WorkerPanicked));
    }
}
