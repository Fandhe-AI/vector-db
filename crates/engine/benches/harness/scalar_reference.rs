//! 真のスカラー逐次和による Top-k 総当たり参照実装（Issue #177）。
//!
//! `simd_bench.rs`（旧 `parallel_bench.rs`。TASK-127）の CORE-4 判定は、TASK-156
//! （CORE-14・Issue #109・PR #202）で `kernel.rs::dot` が `isa::current().dot` へ
//! 委譲されて以降、比較対象の `CpuScalarProvider` も暗黙に SIMD カーネルを使うように
//! なった。そのため両者の比較は「SIMD+並列 vs SIMD+逐次」（スレッド分割の整合確認。
//! `tests/parallel_search.rs` 側で別途担保済み）にしかならず、SIMD 演算順序
//! （レーン分割・FMA）による丸め差を含む「SIMD 経路 vs 真のスカラー逐次和」という
//! CORE-4 本来の Recall 判定にならない。
//!
//! 本モジュールは `engine::isa::dot_scalar`（左から右への逐次和。SIMD 化されない
//! 参照実装）のみを使って総当たり Top-k を選出し、`simd_bench.rs` が
//! `ParallelSearchProvider`（SIMD+並列）との Recall@k 判定に使う「真のスカラー」
//! 参照を提供する。
//!
//! # 選出規約: `kernel.rs::TopKSelector` との整合
//!
//! `TopKSelector` は `pub(crate)` でありベンチ（`crates/engine` 外のコンパイル単位
//! である `cargo bench` バイナリ）からは参照できない。製品コードの公開面を
//! `simd_bench.rs` のためだけに広げない（TopKSelector を `pub` にしない）ため、
//! 同一の選出規約（非有限スコア除外・スコア降順・同点は id 昇順）を harness 側に
//! 独立実装する（`kernel.rs::TopKSelector::push`・`into_sorted_vec` のドキュメント
//! 参照。二重実装だが、比較対象〔SIMD 経路〕とは独立した参照実装であることが
//! 本来の目的であるため、あえて共有しない）。

use engine::isa::dot_scalar;

use super::stats::BenchError;

/// 総当たりでスコアを計算し、`(score 降順, id 昇順)` の Top-k を返す。
///
/// `ids.len() * dim == vectors.len()` かつ `query.len() == dim` を前提とする
/// （untrusted 入力ではなくベンチ内部の合成データだが、`coding-rust.md` の
/// 添字アクセス禁止・fail-closed 方針を harness にも適用し、不整合入力は
/// `Err` で拒否する）。`k == 0` または `ids` が空の場合は空列を返す。
pub fn top_k_ids_scalar(
    ids: &[u64],
    vectors: &[f32],
    dim: usize,
    query: &[f32],
    k: usize,
) -> Result<Vec<u64>, BenchError> {
    if query.len() != dim {
        return Err(BenchError::ProtocolViolation("query length must equal dim"));
    }
    let expected_len = ids
        .len()
        .checked_mul(dim)
        .ok_or(BenchError::ProtocolViolation("ids.len() * dim overflow"))?;
    if vectors.len() != expected_len {
        return Err(BenchError::ProtocolViolation(
            "vectors.len() must equal ids.len() * dim",
        ));
    }

    if k == 0 || ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut scored: Vec<(u64, f32)> = Vec::with_capacity(ids.len());
    for (row_index, &id) in ids.iter().enumerate() {
        let start = row_index.saturating_mul(dim);
        let end = start.saturating_add(dim);
        let row = vectors
            .get(start..end)
            .ok_or(BenchError::ProtocolViolation("row slice out of bounds"))?;
        let score = dot_scalar(row, query);
        if score.is_finite() {
            scored.push((id, score));
        }
    }

    // (score 降順, id 昇順)。`f32` は全順序を持たないが、非有限値は上で除外済みの
    // ため `total_cmp` の比較対象は常に有限値になる（`TopKSelector` と同じ前提）。
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(k);
    Ok(scored.into_iter().map(|(id, _)| id).collect())
}
