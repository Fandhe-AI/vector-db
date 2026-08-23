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
    /// クエリベクトルの要素に非有限値（NaN・Inf）が含まれる。呼び出し元（wire 経路）からの
    /// untrusted 入力のため、`total_cmp` の順序に頼らず明示的に拒否する
    /// （coding-rust.md「untrusted 入力の扱い」対応。fail-closed）。
    NonFiniteQuery,
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::DimMismatch { expected, found } => write!(
                f,
                "kernel query dim mismatch: expected={expected} found={found}"
            ),
            KernelError::NonFiniteQuery => write!(f, "kernel query contains non-finite value"),
        }
    }
}

impl std::error::Error for KernelError {}

/// 実行バックエンドの入力ビュー。`core.rs` がアリーナのカラムナ表現から、呼び出し元
/// `PolicyContext` の下で可視な行だけを抽出した縮約ビューとして組み立てる（codex P0
/// 指摘・Issue #137 対応）。provider 側は所有権を持たず、呼び出し中だけ borrow する
/// （object-safe を保つためジェネリクスを持たない参照渡しに統一する）。
///
/// 可視性判定はここへ渡す前に `core.rs` 側で完結しており、本構造体には不可視行の
/// id・ベクトルは一切含まれない。以前のバージョンはアリーナ全行を渡し、provider が
/// 呼び出す「規約」の `is_visible` クロージャでマスクする設計だったが、provider が
/// クロージャを無視すれば不可視行のベクトル・id を読み取れてしまう構造的な問題が
/// あった（他テナントのデータをそもそも provider のアドレス空間へ渡さない、という
/// より強い境界に変更した）。
pub struct SearchInput<'a> {
    pub ids: &'a [u64],
    /// `ids.len() * dim` 要素のフラット化済みベクトル（行 i の embedding は
    /// `vectors[i * dim .. (i + 1) * dim]`）。可視行のみを含む（上記構造体ドキュメント
    /// 参照）。
    pub vectors: &'a [f32],
    pub dim: u32,
    pub query: &'a [f32],
    pub k: usize,
}

/// コアが依存する検索バックエンドの窓口（CORE-13）。object-safe（ジェネリクスなし・
/// `&self` メソッドのみ）を維持し、`Box<dyn SearchProvider>` として `core.rs` に
/// 保持されることを前提とする。
pub trait SearchProvider: Send + Sync {
    /// `input` に含まれる行（呼び出し元があらかじめ可視行だけへ絞り込み済み）から
    /// 総当たり Top-k 検索を行う。
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
        // クエリは wire 経路からの untrusted 入力であり得るため、次元検証の直後に
        // 明示的に拒否する（`total_cmp` は NaN を最大値扱いにするため、ここで
        // 弾かないと不正なクエリ 1 件が Top-k を恒久的に占有し得る）。
        if input.query.iter().any(|v| !v.is_finite()) {
            return Err(KernelError::NonFiniteQuery);
        }
        if input.k == 0 || input.ids.is_empty() {
            return Ok(Vec::new());
        }

        // スコア最小のヒープを保持し、サイズ k を超えたら最小要素を捨てる
        // （Top-k 選出。事前に全件確保しない）。`f32` は全順序を持たないため
        // 比較には `total_cmp` を使う（[`MinHeapItem`] 参照）。ただし NaN/Inf 混入行は
        // 下記ループで `score.is_finite()` を確認して除外するため、ヒープに乗る
        // スコアは常に有限値になる（`total_cmp` の全順序性は同点判定・整列の安定性の
        // ためだけに使い、NaN の順序上の扱いには依存しない）。
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;

        // 同点タイブレーク（Low 指摘対応）: スコアが同じ場合は id が小さい方を「強い」候補
        // として扱う（返却直前の `sort_by` が id 昇順で安定させるのと選出段の基準を揃える。
        // ヒープ挿入順・入力順に依存しない決定的な選出にする）。
        struct MinHeapItem(SearchHit);
        impl PartialEq for MinHeapItem {
            fn eq(&self, other: &Self) -> bool {
                self.cmp(other) == std::cmp::Ordering::Equal
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
                self.0
                    .score
                    .total_cmp(&other.0.score)
                    .then(other.0.id.cmp(&self.0.id))
            }
        }

        let mut heap: BinaryHeap<Reverse<MinHeapItem>> = BinaryHeap::new();
        for (idx, &id) in input.ids.iter().enumerate() {
            let start = idx.saturating_mul(dim);
            let end = start.saturating_add(dim);
            let Some(vector) = input.vectors.get(start..end) else {
                // アリーナ側の不変条件（`vectors.len() == ids.len() * dim`）が破れている。
                // untrusted 入力由来ではないが、添字アクセスで panic させず黙って
                // スキップする（fail-closed: 壊れた行を結果に混入させない）。
                continue;
            };
            let score = dot(vector, input.query);
            if !score.is_finite() {
                // 格納ベクトルの NaN/Inf 混入、またはオーバーフローによる内積の非有限化
                // （Medium 指摘対応）。`total_cmp` は +NaN を最大値扱いにするため、
                // 有限値チェックなしでは 1 行の非有限スコアが Top-k を恒久的に占有し
                // 正当な上位ヒットを押し出しかねない。fail-closed に当該行を除外する。
                continue;
            }
            let hit = SearchHit { id, score };
            let candidate = MinHeapItem(hit);
            if heap.len() < input.k {
                heap.push(Reverse(candidate));
            } else if let Some(Reverse(top)) = heap.peek() {
                if candidate > *top {
                    heap.pop();
                    heap.push(Reverse(candidate));
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

    // 可視性マスクの適用（不可視行が Top-k に混ざらないこと）は、以前は本モジュールの
    // `SearchInput::is_visible` クロージャで検証していたが、codex P0 指摘（Issue #137）
    // 対応で `SearchInput` はコア側（`core.rs`）が可視行だけへ絞り込んだ縮約ビューのみを
    // 受け取る設計へ変更した。そのため可視性マスクの検証は本モジュールの責務外になり、
    // `crates/engine/tests/vector_core.rs::search_excludes_private_rows_of_the_same_tenant_when_ctx_disallows_private`
    // （CORE-2）へ移動している。本モジュールはあくまで「渡された入力の中での Top-k 選出」
    // のみを検証する。

    #[test]
    fn top_k_returns_highest_dot_product_scores() {
        let ids = [1u64, 2, 3, 4];
        // dim=2 の 4 行。クエリ [1.0, 0.0] との内積は id 昇順に 1.0, 2.0, 0.0, 3.0。
        let vectors = [1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
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
            let err = CpuScalarProvider.search(input).unwrap_err();
            assert_eq!(err, KernelError::NonFiniteQuery, "query={query:?}");
        }
    }

    #[test]
    fn non_finite_stored_score_is_excluded_and_legitimate_hits_keep_rank() {
        let ids = [1u64, 2, 3];
        // id=2 の行は NaN を含み、内積が NaN になる（Top-k を占有してはならない）。
        let vectors = [1.0, 0.0, f32::NAN, 0.0, 2.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(
            hits,
            vec![
                SearchHit { id: 3, score: 2.0 },
                SearchHit { id: 1, score: 1.0 }
            ]
        );
    }

    #[test]
    fn tied_scores_prefer_smaller_id_at_selection_boundary() {
        let ids = [3u64, 2, 1];
        // 3 行とも同スコア。k=1 のため、選出段のタイブレークで最終結果が決まる
        // （挿入順は降順 id だが、結果は最小 id を選ぶ）。
        let vectors = [1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 1,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(hits, vec![SearchHit { id: 1, score: 1.0 }]);
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
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 0,
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
