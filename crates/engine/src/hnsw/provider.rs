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
//! 同型の設計）。差分判定なしに索引だけを探索すると、索引構築後に追加された行・
//! 不可視化された行を検索結果へ混入・欠落させる（テナント境界・RLS 可視性契約を
//! 壊す）ため、[`SearchProvider`] の実装としての本 provider・[`SearchInput`] を
//! 直接受ける経路は安全側に倒して「索引済み集合は常に空」＝全件フォールバックの
//! ままにする。
//!
//! # #408 が接続した索引経路の seam
//!
//! 世代整合キャッシュ・索引探索・Top-k マージは Issue #408 で接続済みだが、
//! **本 provider（`SearchProvider::search`）の外側**として接続する（下記参照）。
//! `SearchProvider` trait 自体・本 provider の `search` 実装は無変更のまま：
//!
//! 1. 世代整合キャッシュは `sql::hnsw_cache::HnswIndexCache`（`(table, ctx)` ×
//!    テーブル単位世代キー）として本 provider の外側（`sql::exec::
//!    execute_statement_with_cache` から `core.rs::EngineCore::hnsw_state` 経由）に
//!    実装した。SQL 表層のフィルタなし `Ranking::Distance` クエリに限る適用条件。
//! 2. 索引側の探索は [`HnswIndex::search`](crate::hnsw::HnswIndex::search) を
//!    `HnswIndex::search(query, k, self.effective_ef(k), scratch)` の形で呼び、
//!    [`HnswSearchScratch`](crate::hnsw::HnswSearchScratch) は呼び出しスレッドごとに
//!    `thread_local!` で所有する（`hnsw.rs` モジュールドキュメント「ベクトルの
//!    所有方針」・`docs/design/hnsw-search.md` の申し送りどおり）。untrusted な
//!    `k` の上限保証は [`HnswSearchProvider::effective_ef`] ではなく
//!    `HnswIndex::search` 自身の `k > MAX_EF` fail-closed 検証が担う
//!    （[`HnswSearchProvider::effective_ef`] のドキュメンテーションコメント参照。
//!    codex-review P2 指摘・Issue #407 追記）。
//! 3. 索引側 hit と brute-force 側（未索引分）hit の Top-k マージは、
//!    `kernel.rs::TopKSelector` と同じ順序規約（スコア `total_cmp` 降順・同点 id
//!    昇順）を保った `sort_by`（安定ソート）で行う（`sort_unstable` 系は
//!    `scripts/check_sort_determinism.sh` が禁止する）。
//!
//! Rust API（`VectorCore::search`）・フィルタ付きクエリ・hybrid クエリは
//! `sql::hnsw_cache` を経由せず、本 provider の全件フォールバックのまま
//! （詳細・段階化の理由は `docs/design/hnsw-generation-cache.md` 参照）。

use crate::hnsw::{HnswParams, ValidatedHnswParams, MAX_EF};
use crate::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider};
use crate::parallel_search::ParallelSearchProvider;

/// [`crate::search_engine::SearchEngineKind::Hnsw`] が構築する provider。
///
/// [`Self::new`] は [`ValidatedHnswParams`] のみを受け取る。`ValidatedHnswParams` は
/// private フィールドで [`ValidatedHnswParams::new`]（[`HnswParams::validate`] を必ず
/// 経由する）以外の経路では構築できないため、不正値を保持した provider は型として
/// 存在しえない（codex-review P1 指摘・Issue #407・PR #433 追記。実行時エラー分類の
/// 流用・偽装ではなく型システムで到達不能にする設計）。`crate::hnsw` が公開モジュール
/// （`lib.rs::pub mod hnsw`）で本構造体へ直接到達できても、検証済み値なしには
/// 構築できない点は変わらない。
#[derive(Debug, Clone, Copy)]
pub struct HnswSearchProvider {
    params: ValidatedHnswParams,
    fallback: ParallelSearchProvider,
}

impl HnswSearchProvider {
    /// 検証済みパラメータを保持する provider を構築する（infallible。検証は
    /// [`ValidatedHnswParams::new`] の時点で完了済み）。
    pub fn new(params: ValidatedHnswParams) -> Self {
        HnswSearchProvider {
            params,
            fallback: ParallelSearchProvider,
        }
    }

    /// 保持している構築パラメータを返す（テスト・診断用）。
    pub fn params(&self) -> HnswParams {
        self.params.get()
    }

    /// `k` 件の Top-k を得るために `HnswIndex::search` の `ef` 引数へ渡す値を返す
    /// （`self.params.ef_search.max(k).min(MAX_EF)`）。
    ///
    /// **戻り値は `k` に依存する**（例: 構築時 `ef_search=32` で `k=64` を渡すと
    /// `64` を返す。codex-review P2 指摘・Issue #407 追記で「戻り値は `k` に
    /// 一切影響されない」という誤記載を訂正した）。ただし本メソッドが行うのは
    /// 「`k` との `max` を取ったうえで [`MAX_EF`] へクランプする」ことだけで、
    /// **引数 `k` 自体を書き換えて `HnswIndex::search` へ渡すわけではない**
    /// （`k` はそのまま呼び出し元から `HnswIndex::search` の `k` 引数へ渡る）。
    /// `HnswIndex::search` 自身も内部で同じ `ef.max(k)` を計算するため
    /// （`hnsw.rs::HnswIndex::search` の実装）、本メソッドの戻り値をそのまま
    /// `ef` として渡しても渡さなくても実効 `ef` は同じ値に揃う。本メソッドの
    /// 主な役割は構築時パラメータ `ef_search`（untrusted 入力ではない
    /// `HnswParams::validate` 済みの構成値）を [`MAX_EF`] 内へ収めることにある。
    ///
    /// untrusted な `k`（wire 経由で到達しうる `SearchInput::k`）に対する実際の
    /// 上限保証は本メソッドではなく、`HnswIndex::search` 自身の検証が担う
    /// （`ef.max(k)` を計算する**前**に `k > MAX_EF` を fail-closed で
    /// `Err(HnswError::InvalidParams)` として拒否する。#408 がこのメソッドを
    /// 呼ぶ契約。モジュールドキュメント「seam」節 2. 参照）。
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
        let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
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
        let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
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
        let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
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

        let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
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
        let small_ef = HnswSearchProvider::new(
            ValidatedHnswParams::new(HnswParams {
                m: 16,
                ef_construction: 100,
                ef_search: 10,
            })
            .unwrap(),
        );
        // k > ef_search のときは k まで引き上げる。
        assert_eq!(small_ef.effective_ef(50), 50);
        // k <= ef_search のときは ef_search をそのまま使う。
        assert_eq!(small_ef.effective_ef(1), 10);

        let large_ef = HnswSearchProvider::new(
            ValidatedHnswParams::new(HnswParams {
                m: 16,
                ef_construction: 100,
                ef_search: MAX_EF,
            })
            .unwrap(),
        );
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
        let provider = HnswSearchProvider::new(ValidatedHnswParams::new(params).unwrap());
        assert_eq!(provider.params(), params);
    }

    // 不正な `HnswParams` は `ValidatedHnswParams::new` の時点で拒否され、
    // `HnswSearchProvider::new` へは到達しない（型で保証。codex-review P1 指摘・
    // Issue #407・PR #433 追記）。`HnswSearchProvider::new` 自身は
    // `ValidatedHnswParams` 以外を受け取れないためコンパイルもできない。
    #[test]
    fn validated_hnsw_params_new_rejects_invalid_params() {
        let invalid = HnswParams {
            m: 1, // HnswParams::validate は m < 2 を拒否する
            ..HnswParams::default()
        };
        assert!(ValidatedHnswParams::new(invalid).is_err());
    }
}
