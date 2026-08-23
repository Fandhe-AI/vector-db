//! 検索カーネルの実行バックエンド provider 層（TASK-124・対象ビヘイビア: CORE-13）。
//!
//! `core.rs` の [`crate::core::EngineCore`] は具象バックエンド型（CPU-SIMD・GPU・将来
//! ANN）へ直接依存せず、本モジュールが定義する object-safe な [`SearchProvider`] trait
//! 経由で実行バックエンドを注入される。既定コンストラクタは本モジュールの
//! [`CpuScalarProvider`]（総当たりスカラー参照実装）を登録し、CPU-only 構成のみで
//! 全機能が成立することを保証する。SIMD 化・並列化は後続タスク（TASK-126）の範囲。
//!
//! 経路選択の外部上書き機構（環境変数・設定フラグ等）は設けない（CORE-12 の方針先取り。
//! ディスパッチ決定表は TASK-155 の範囲）。

use std::fmt;

/// 検索結果 1 件（行 ID とスコア）。
///
/// スコアは内積（dot product）で定義する（`arena.rs` の既存テストが検証基準に
/// 使っている尺度と揃える）。値が大きいほど上位。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchHit {
    pub id: u64,
    pub score: f32,
}

/// [`SearchProvider::search`] が返すエラー。
#[derive(Debug, Clone, PartialEq)]
pub enum KernelError {
    /// クエリベクトルの次元がアリーナの次元と一致しない。
    DimMismatch { expected: u32, found: usize },
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::DimMismatch { expected, found } => write!(
                f,
                "kernel query dim mismatch: expected={expected} found={found}"
            ),
        }
    }
}

impl std::error::Error for KernelError {}

/// 実行バックエンドの入力ビュー。`core.rs` がアリーナのカラムナ表現から組み立てる。
/// provider 側は所有権を持たず、呼び出し中だけ borrow する（object-safe を保つため
/// ジェネリクスを持たない参照渡しに統一する）。
pub struct SearchInput<'a> {
    pub ids: &'a [u64],
    /// `ids.len() * dim` 要素のフラット化済みベクトル（行 i の embedding は
    /// `vectors[i * dim .. (i + 1) * dim]`）。`arena.rs::VectorArena::vectors` と同じ表現。
    pub vectors: &'a [f32],
    pub dim: u32,
    pub query: &'a [f32],
    pub k: usize,
    /// 行インデックス（`ids`/`vectors` 上の位置）が結果候補として可視かどうかの判定。
    /// `core.rs` が [`crate::policy::PolicyContext::is_visible`] を経由して構築する
    /// クロージャで、provider 自身はテナント境界判定を行わない（CORE-2 の単一照合パス
    /// を `core.rs` に閉じ込め、provider 実装の増加が判定ロジックの分岐点を増やさない
    /// ようにするための設計）。
    pub is_visible: &'a dyn Fn(usize) -> bool,
}

/// コアが依存する検索バックエンドの窓口（CORE-13）。object-safe（ジェネリクスなし・
/// `&self` メソッドのみ）を維持し、`Box<dyn SearchProvider>` として `core.rs` に
/// 保持されることを前提とする。
pub trait SearchProvider: Send + Sync {
    /// 総当たり Top-k 検索を行う。可視でない行（`input.is_visible` が `false` を返す行）は
    /// 結果候補から除外する（マスク適用は Top-k 選出段で行い、除外行のスコアが
    /// 上位 k 件の選出に影響しないようにする）。
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError>;
}

/// 既定の CPU-only 参照実装。内積スコアでの総当たり Top-k（`O(n log k)`、`BinaryHeap`
/// による部分ソート）。SIMD 化・並列化はしない（TASK-126 の範囲）。
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuScalarProvider;

impl SearchProvider for CpuScalarProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
        let dim = input.dim as usize;
        if input.query.len() != dim {
            return Err(KernelError::DimMismatch {
                expected: input.dim,
                found: input.query.len(),
            });
        }
        if input.k == 0 || input.ids.is_empty() {
            return Ok(Vec::new());
        }

        // スコア最小のヒープを保持し、サイズ k を超えたら最小要素を捨てる
        // （Top-k 選出。事前に全件確保しない）。`f32` は全順序を持たないため
        // `total_cmp` で比較し、NaN 混入時も panic せず一貫した順序を得る。
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;

        struct MinHeapItem(SearchHit);
        impl PartialEq for MinHeapItem {
            fn eq(&self, other: &Self) -> bool {
                self.0.score.total_cmp(&other.0.score) == std::cmp::Ordering::Equal
            }
        }
        impl Eq for MinHeapItem {}
        impl PartialOrd for MinHeapItem {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for MinHeapItem {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.0.score.total_cmp(&other.0.score)
            }
        }

        let mut heap: BinaryHeap<Reverse<MinHeapItem>> = BinaryHeap::new();
        for (idx, &id) in input.ids.iter().enumerate() {
            if !(input.is_visible)(idx) {
                continue;
            }
            let start = idx.saturating_mul(dim);
            let end = start.saturating_add(dim);
            let Some(vector) = input.vectors.get(start..end) else {
                // アリーナ側の不変条件（`vectors.len() == ids.len() * dim`）が破れている。
                // untrusted 入力由来ではないが、添字アクセスで panic させず黙って
                // スキップする（fail-closed: 壊れた行を結果に混入させない）。
                continue;
            };
            let score = dot(vector, input.query);
            let hit = SearchHit { id, score };
            if heap.len() < input.k {
                heap.push(Reverse(MinHeapItem(hit)));
            } else if let Some(Reverse(top)) = heap.peek() {
                if hit.score > top.0.score {
                    heap.pop();
                    heap.push(Reverse(MinHeapItem(hit)));
                }
            }
        }

        let mut out: Vec<SearchHit> = heap.into_iter().map(|Reverse(item)| item.0).collect();
        // スコア降順（内積が大きいほど上位）に整列して返す。同点は id 昇順で安定させる。
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        Ok(out)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_visible(_idx: usize) -> bool {
        true
    }

    #[test]
    fn top_k_returns_highest_dot_product_scores() {
        let ids = [1u64, 2, 3, 4];
        // dim=2 の 4 行。クエリ [1.0, 0.0] との内積は id 昇順に 1.0, 2.0, 0.0, 3.0。
        let vectors = [1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0, 0.0];
        let query = [1.0, 0.0];
        let visible = always_visible;
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
            is_visible: &visible,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(
            hits,
            vec![
                SearchHit { id: 4, score: 3.0 },
                SearchHit { id: 2, score: 2.0 }
            ]
        );
    }

    #[test]
    fn masked_rows_are_excluded_from_top_k() {
        let ids = [1u64, 2, 3];
        let vectors = [1.0, 0.0, 5.0, 0.0, 2.0, 0.0];
        let query = [1.0, 0.0];
        // 最高スコア行（idx=1, id=2, score=5.0）を不可視にする。
        let visible = |idx: usize| idx != 1;
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
            is_visible: &visible,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.id != 2));
    }

    #[test]
    fn dim_mismatch_query_is_rejected() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        let query = [1.0, 0.0, 0.0];
        let visible = always_visible;
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 1,
            is_visible: &visible,
        };
        let err = CpuScalarProvider.search(input).unwrap_err();
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
        let visible = always_visible;
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 0,
            is_visible: &visible,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert!(hits.is_empty());
    }

    // object-safety の固定（CORE-13）: `Box<dyn SearchProvider>` として保持できること。
    #[test]
    fn provider_is_object_safe() {
        let _boxed: Box<dyn SearchProvider> = Box::new(CpuScalarProvider);
    }
}
