//! 検索カーネルの実行バックエンド provider 層（TASK-124・対象ビヘイビア: CORE-13）。
//!
//! `core.rs` の [`crate::core::EngineCore`] は具象バックエンド型（CPU 並列・GPU・将来
//! ANN）へ直接依存せず、本モジュールが定義する object-safe な [`SearchProvider`] trait
//! 経由で実行バックエンドを注入される。本モジュールはスカラー参照実装
//! [`CpuScalarProvider`] と、Top-k 選出の共通ヘルパ [`TopKSelector`] を提供する。
//! `TopKSelector` は `crates/engine/src/parallel_search.rs::ParallelSearchProvider`（TASK-126）
//! とも共用し、選出規約（スコア降順・同点 id 昇順・非有限値除外）の二重管理を防ぐ。
//! 既定コンストラクタが実際にどちらの provider を注入するかは `search_engine.rs`
//! （TASK-131・CORE-9 の差し替え点確定化レイヤ）経由で `core.rs::EngineCore::open` を参照。
//!
//! 経路選択の外部上書き機構（環境変数・設定フラグ等）は設けない（CORE-12）。
//! 実行経路（CPU-SIMD／GPU）自体の決定表は `dispatch.rs::select_execution_path`
//! （TASK-155・CORE-11, 12）に集約されており、本モジュールはあくまで provider
//! trait の窓口を提供する。

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
    /// `parallel_search.rs::ParallelSearchProvider` の並列ワーカースレッドが panic した。
    /// 部分結果を欠いたまま `Ok` を返すと該当パーティションの行が黙って選出対象から
    /// 消える（実質 fail-open）ため、検索全体を失敗として呼び出し元へ伝播させる
    /// （Issue #34 レビュー指摘対応）。
    WorkerPanicked,
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::DimMismatch { expected, found } => write!(
                f,
                "kernel query dim mismatch: expected={expected} found={found}"
            ),
            KernelError::NonFiniteQuery => write!(f, "kernel query contains non-finite value"),
            KernelError::WorkerPanicked => {
                write!(f, "kernel search worker thread panicked")
            }
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
    /// `vectors` の行と 1 対 1 に対応する識別子。**呼び出し元が定義する識別子**であり、
    /// 行 `id` とは限らない（provider は値の意味を解釈せず、そのまま
    /// [`SearchHit::id`] として返す）。行 `id` の一意性スコープはテナント内に閉じている
    /// （対象ビヘイビア: TABLE-12）ため、同一 `id` の可視行が複数含まれうる文脈では
    /// 呼び出し元が一意な識別子を渡す責務を負う: SQL 表層（`sql::exec`）は候補アリーナの
    /// スロット番号を渡し（投影・RRF 融合・疎コーパスの結合キーを一意にするため）、
    /// `core::EngineCore::search` は行 `id` をそのまま渡す（結果を id で返す契約のため。
    /// 重複しうることは `VectorCore::search` のドキュメント参照）。
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
/// による部分ソート）を単一スレッドで行う。内積カーネル自体は `isa.rs`
/// （TASK-156・CORE-14）の実行時検出結果に従う（`dot` 参照。対応 CPU では SIMD、
/// 非対応環境ではスカラー逐次和）。スレッド並列化された
/// [`crate::parallel_search::ParallelSearchProvider`]（TASK-126）の正解値検証用の参照実装も兼ねる。
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

        let mut selector = TopKSelector::new(input.k);
        for (idx, &id) in input.ids.iter().enumerate() {
            let start = idx.saturating_mul(dim);
            let end = start.saturating_add(dim);
            let Some(vector) = input.vectors.get(start..end) else {
                // アリーナ側の不変条件（`vectors.len() == ids.len() * dim`）が破れている。
                // untrusted 入力由来ではないが、添字アクセスで panic させず該当行だけを
                // 候補から除外する（呼び出し全体は `Ok` のまま。破損行 1 件を混入させない
                // という意味では安全側だが、検索全体を拒否するわけではないため厳密な
                // fail-closed ではない点に注意。共有参照実装として
                // `parallel_search.rs::search_range` と同一の挙動を維持する）。
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
            selector.push(SearchHit { id, score });
        }
        Ok(selector.into_sorted_vec())
    }
}

/// 内積（dot product）。実体は `isa.rs::current().dot`（TASK-156・CORE-14）へ
/// 委譲し、実行時検出された ISA（AVX2+FMA・AVX-512・NEON。非対応環境では
/// `isa::dot_scalar` と同じ左から右への逐次和）を使う。
///
/// `parallel_search.rs::search_range`・`batch_search.rs`・`rls.rs` からも同一関数
/// として呼ばれる（Issue #34 レビュー指摘対応: 加算順序を分岐させると
/// `ParallelSearchProvider` 等と本 provider の Top-k 集合・順序が丸め誤差で
/// 食い違い得るため、`pub(crate)` にして共有し、単一のカーネル・単一の加算順序で
/// あることを構造的に保証する。ISA 検出はプロセス内で単調なため、実行中に加算順序が
/// 変わることはない）。
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    crate::isa::current().dot(a, b)
}

/// ヒープ内の同点タイブレーク規約（Low 指摘対応）: スコアが同じ場合は id が小さい方を
/// 「強い」候補として扱う（[`TopKSelector::into_sorted_vec`] の `sort_by` が返却直前に
/// id 昇順で安定させるのと選出段の基準を揃え、ヒープ挿入順・入力順に依存しない決定的な
/// 選出にする）。`f32` は全順序を持たないため `total_cmp` を使う。ただし非有限スコアは
/// [`TopKSelector::push`] が事前に弾くため、ここで比較する値は常に有限値になる。
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

/// Top-k 選出の共通ヘルパ（対象ビヘイビア: CORE-4）。スコア最小のヒープを保持し、
/// サイズ `k` を超えたら最小要素を捨てる方式（事前に全件確保しない・`O(n log k)`）。
///
/// [`CpuScalarProvider`]（本ファイル）と `parallel_search.rs::ParallelSearchProvider`
/// （TASK-126）の両方から使われる。後者はスレッドごとに本セレクタで部分 Top-k を
/// 選出したうえで、部分結果を同じセレクタへ再度 push してマージする
/// （分割数・スレッド数に依らず選出規約が一意に決まる設計。CORE-3・SEARCH-4）。
pub(crate) struct TopKSelector {
    k: usize,
    heap: std::collections::BinaryHeap<std::cmp::Reverse<MinHeapItem>>,
}

impl TopKSelector {
    /// 容量 `k` の選出器を作る。`k == 0` の場合は何を push しても常に空集合を返す。
    pub(crate) fn new(k: usize) -> Self {
        Self {
            k,
            // `k` は `SearchProvider` trait 経由で外部（wire-server 等の呼び出し元）から
            // 到達しうる値で、`core.rs` は `MAX_SEARCH_K` でクランプするが本 provider 自体は
            // 検証しない。`with_capacity(k)` で事前確保すると未検証の巨大な `k` がそのまま
            // アロケーションサイズになってしまう（coding-rust.md「無制限確保禁止」）ため、
            // 事前確保はせず push 時に自然成長させる（旧 `CpuScalarProvider` 実装と同じ挙動。
            // 実際に保持する要素数は `push` のロジックにより高々 `k` に抑えられる）。
            heap: std::collections::BinaryHeap::new(),
        }
    }

    /// 内部ヒープへ `additional` 件分の容量をフォールブルに予約する。[`Self::new`]
    /// があえて事前確保しない理由（未検証の `k` を確保サイズへ直接使わない）とは
    /// 矛盾しない: 本メソッドは呼び出し元が別途総量を上限検証済みであることを
    /// 前提にした任意 API である（`batch_search.rs::BatchEngine::batch_search` が
    /// バッチ全体の `sum(k)` を検証してから各選出器へ呼ぶ想定）。呼び出し元が
    /// 本メソッドを使わず素朴に `push` するだけの経路（`CpuScalarProvider`・
    /// `ParallelSearchProvider`）は今までどおり amortized 成長のままでよい
    /// （codex P1 指摘対応: `BinaryHeap::push` の内部確保は abort-on-OOM のため、
    /// 総量が上限検証済みの呼び出し元には `Result` 契約の確保手段を用意する。
    /// security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    pub(crate) fn try_reserve(
        &mut self,
        additional: usize,
    ) -> Result<(), std::collections::TryReserveError> {
        self.heap.try_reserve(additional)
    }

    /// 候補 1 件を選出器へ投入する。非有限スコア（NaN/Inf）は fail-closed に無視する
    /// （呼び出し元が事前に除外している場合でも、二重の安全網として機能する）。
    pub(crate) fn push(&mut self, hit: SearchHit) {
        if self.k == 0 || !hit.score.is_finite() {
            return;
        }
        let candidate = MinHeapItem(hit);
        if self.heap.len() < self.k {
            self.heap.push(std::cmp::Reverse(candidate));
        } else if let Some(std::cmp::Reverse(top)) = self.heap.peek() {
            if candidate > *top {
                self.heap.pop();
                self.heap.push(std::cmp::Reverse(candidate));
            }
        }
    }

    /// 選出結果をスコア降順（同点は id 昇順）で確定して返す。
    pub(crate) fn into_sorted_vec(self) -> Vec<SearchHit> {
        let mut out: Vec<SearchHit> = self
            .heap
            .into_iter()
            .map(|std::cmp::Reverse(item)| item.0)
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        out
    }
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
