//! HNSW を `kernel.rs::SearchProvider` として結線するアダプタ（Issue #407・
//! `search_engine.rs::SearchEngineKind::Hnsw` の構築先。ADR
//! `docs/design/ann-index-adoption.md` B 案の Phase 3 分解タスク #404〜#413 のうち、
//! 「エンジン選択の opt-in 経路」を担う）。
//!
//! # 本タスク時点の契約: 全件 brute-force フォールバック
//!
//! [`HnswSearchProvider`] は本 Issue 時点では `hnsw.rs::HnswIndex` を構築・保持
//! **しない**。`SearchProvider::search` は常に
//! [`crate::parallel_search::ParallelSearchProvider`] へ委譲する。
//!
//! 理由: `kernel.rs::SearchInput` は「呼び出しごとに集合が変わりうる可視行の縮約
//! ビュー」（RLS フィルタ・テーブル更新のたびに変わる）であり、`HnswIndex` は
//! 構築時点のスナップショットしか探索できない。索引済み集合と `SearchInput` の
//! 差分を安全に判定するには世代整合キャッシュが要る（`sql::sparse_cache::
//! SparseIndexCache`〔Issue #357〕・`sql::arena_cache::SqlArenaCache`〔Issue #363〕と
//! 同型の設計）が、これは #408 の担当であり本 Issue のスコープ外。差分判定なしに
//! 索引だけを探索すると、索引構築後に追加された行・不可視化された行を検索結果へ
//! 混入・欠落させる（テナント境界・RLS 可視性契約を壊す）ため、安全側に倒して
//! 「索引済み集合は常に空」＝全件フォールバックとして実装する。
//!
//! # #408 が接続する索引経路の seam（本タスクでは実装しない）
//!
//! 1. 索引済み集合と `SearchInput` の差分を判定する世代整合キャッシュは、本
//!    provider の外側（`core.rs`／`sql` 側のテーブル世代整合機構）が持つ契約とする。
//! 2. 索引側の探索は [`HnswIndex::search`](crate::hnsw::HnswIndex::search) を
//!    `HnswIndex::search(query, k, self.effective_ef(k), scratch)` の形で呼び、
//!    [`HnswSearchScratch`](crate::hnsw::HnswSearchScratch) は呼び出しスレッドごとに
//!    呼び出し元が所有する（`hnsw.rs` モジュールドキュメント「ベクトルの所有方針」・
//!    `docs/design/hnsw-search.md` の申し送りを踏襲）。
//! 3. 索引側 hit と brute-force 側（未索引分）hit の Top-k マージは、
//!    `kernel.rs::TopKSelector` と同じ順序規約（スコア `total_cmp` 降順・同点 id
//!    昇順）を保った安定マージで行う（`sort_unstable` 系は
//!    `scripts/check_sort_determinism.sh` が禁止する）。
//!
//! これらは未実装のまま production へ置かない（未使用コードを持ち込まない）。

use crate::hnsw::{HnswParams, MAX_EF};
use crate::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider};
use crate::parallel_search::ParallelSearchProvider;

/// [`crate::search_engine::SearchEngineKind::Hnsw`] が構築する provider。
///
/// `HnswSearchProvider`（[`crate::hnsw::provider`]）は `crate::hnsw` が公開モジュール
/// （`lib.rs::pub mod hnsw`）のため `search_engine::build_validated` を経由せず本構造体を
/// 直接構築する外部呼び出しも到達しうる。そのため [`Self::new`] 自身が
/// [`HnswParams::validate`] を通し、不正値を保持した provider が構築される経路を
/// 構造的に無くす（codex-review P1 指摘・Issue #407 追記。`search_engine.rs` 側の
/// `build_validated` はこの検証と重複するが、呼び出し元ごとに異なるエラー型
/// （[`crate::search_engine::SearchEngineError`] と [`crate::hnsw::HnswError`]）を
/// そのまま返すため二重実装ではなく境界の異なる同一契約とする）。
#[derive(Debug, Clone, Copy)]
pub struct HnswSearchProvider {
    params: HnswParams,
    fallback: ParallelSearchProvider,
}

impl HnswSearchProvider {
    /// `params` を検証したうえで保持する provider を構築する。不正な `params`
    /// （[`HnswParams::validate`] が拒否する値）は [`crate::hnsw::HnswError`] として
    /// fail-closed に拒否し、`HnswSearchProvider` を構築しない。
    pub fn new(params: HnswParams) -> Result<Self, crate::hnsw::HnswError> {
        params.validate()?;
        Ok(HnswSearchProvider {
            params,
            fallback: ParallelSearchProvider,
        })
    }

    /// 保持している構築パラメータを返す（テスト・診断用）。
    pub fn params(&self) -> HnswParams {
        self.params
    }

    /// `k` 件の Top-k を得るために使う実効 `ef`（探索候補幅）。
    ///
    /// `HnswIndex::search` 自身も `ef.max(k)` へ引き上げる（`hnsw.rs` 参照）ため、
    /// 本メソッドの主眼は untrusted な `k`（wire 経由で到達しうる `SearchInput::k`）が
    /// [`MAX_EF`] を超えて索引側へ渡らないようにクランプすることにある（#408 が
    /// このメソッドを呼ぶ契約。モジュールドキュメント「seam」節 2. 参照）。
    pub fn effective_ef(&self, k: usize) -> usize {
        self.params.ef_search.max(k).min(MAX_EF)
    }
}

impl SearchProvider for HnswSearchProvider {
    /// モジュールドキュメント「本タスク時点の契約」節のとおり、常に
    /// [`ParallelSearchProvider`] へ委譲する（全件 brute-force フォールバック）。
    /// エラー型・順序規約（スコア降順・同点 id 昇順）・入力検証（次元不一致・
    /// 非有限クエリ・`k == 0`）は委譲先とビット単位で同一になる。
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.fallback.search(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift64star(state: &mut u64) -> u64 {
        // hnsw.rs::tests・hnsw_search テストと同型の決定的 xorshift64* 生成器
        // （手法名のみ参照。コード転記元は本モジュールが唯一の実装）。
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn deterministic_corpus(n: usize, dim: usize, seed: u64) -> Vec<f32> {
        let mut state = seed.max(1);
        let mut out = Vec::with_capacity(n * dim);
        for _ in 0..n * dim {
            let bits = xorshift64star(&mut state);
            // [-1.0, 1.0) の範囲に収める簡易変換。
            let v = ((bits >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0;
            out.push(v);
        }
        out
    }

    fn ids_for(n: usize) -> Vec<u64> {
        (0..n as u64).collect()
    }

    #[test]
    fn dim_mismatch_matches_fallback() {
        let provider = HnswSearchProvider::new(HnswParams::default()).unwrap();
        let vectors = deterministic_corpus(4, 3, 7);
        let ids = ids_for(4);
        let query = vec![0.0_f32, 1.0]; // dim=3 のはずが 2 要素
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 3,
            query: &query,
            k: 2,
        };
        let err = provider.search(input).expect_err("dim mismatch must error");
        assert_eq!(
            err,
            KernelError::DimMismatch {
                expected: 3,
                found: 2
            }
        );
    }

    #[test]
    fn non_finite_query_matches_fallback() {
        let provider = HnswSearchProvider::new(HnswParams::default()).unwrap();
        let vectors = deterministic_corpus(4, 3, 11);
        let ids = ids_for(4);
        let query = vec![f32::NAN, 0.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 3,
            query: &query,
            k: 2,
        };
        let err = provider
            .search(input)
            .expect_err("non-finite query must error");
        assert_eq!(err, KernelError::NonFiniteQuery);
    }

    #[test]
    fn k_zero_and_empty_input_return_empty() {
        let provider = HnswSearchProvider::new(HnswParams::default()).unwrap();
        let vectors = deterministic_corpus(4, 3, 13);
        let ids = ids_for(4);
        let query = vec![0.0_f32, 0.0, 0.0];

        let zero_k = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 3,
            query: &query,
            k: 0,
        };
        assert_eq!(provider.search(zero_k).unwrap(), Vec::new());

        let empty_ids: Vec<u64> = Vec::new();
        let empty_vectors: Vec<f32> = Vec::new();
        let empty_input = SearchInput {
            ids: &empty_ids,
            vectors: &empty_vectors,
            dim: 3,
            query: &query,
            k: 2,
        };
        assert_eq!(provider.search(empty_input).unwrap(), Vec::new());
    }

    #[test]
    fn matches_cpu_scalar_reference_bit_for_bit() {
        // フォールバック委譲のため CpuScalarProvider（TopKSelector 経由の参照実装）と
        // 完全一致することを決定的コーパスで確認する（同点タイブレーク含む）。
        use crate::kernel::CpuScalarProvider;

        let dim = 5usize;
        let n = 200usize;
        let vectors = deterministic_corpus(n, dim, 99);
        let ids = ids_for(n);
        let query = deterministic_corpus(1, dim, 4242);

        let hnsw_provider = HnswSearchProvider::new(HnswParams::default()).unwrap();
        let reference = CpuScalarProvider;

        for k in [1usize, 5, 20, 200] {
            let input_a = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k,
            };
            let input_b = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k,
            };
            let got = hnsw_provider.search(input_a).unwrap();
            let want = reference.search(input_b).unwrap();
            assert_eq!(got, want, "k={k}");
        }
    }

    #[test]
    fn effective_ef_clamps_to_max_ef_and_respects_k() {
        let small_ef = HnswSearchProvider::new(HnswParams {
            m: 16,
            ef_construction: 100,
            ef_search: 10,
        })
        .unwrap();
        // k > ef_search のときは k まで引き上げる。
        assert_eq!(small_ef.effective_ef(50), 50);
        // k <= ef_search のときは ef_search をそのまま使う。
        assert_eq!(small_ef.effective_ef(1), 10);

        let large_ef = HnswSearchProvider::new(HnswParams {
            m: 16,
            ef_construction: 100,
            ef_search: MAX_EF,
        })
        .unwrap();
        // untrusted な k が MAX_EF を超えても MAX_EF でクランプする。
        assert_eq!(large_ef.effective_ef(MAX_EF + 10_000), MAX_EF);
    }

    #[test]
    fn params_accessor_returns_constructed_value() {
        let params = HnswParams {
            m: 8,
            ef_construction: 50,
            ef_search: 32,
        };
        let provider = HnswSearchProvider::new(params).unwrap();
        assert_eq!(provider.params(), params);
    }

    // `new` 自身が検証すること（codex-review P1 指摘・Issue #407 追記）: 直接
    // `HnswSearchProvider::new(invalid_params)` を呼んでも不正値を保持した provider が
    // 構築されない。
    #[test]
    fn new_rejects_invalid_params() {
        let invalid = HnswParams {
            m: 1, // HnswParams::validate は m < 2 を拒否する
            ..HnswParams::default()
        };
        assert!(HnswSearchProvider::new(invalid).is_err());
    }
}
