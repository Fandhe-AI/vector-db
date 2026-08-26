//! 対照エンジン（usearch）アダプタ（TASK-127 CORE-5。ポインタ:
//! `docs/spec/04-behavior/core-engine.md` CORE-5・`docs/spec/05-tasks.md` TASK-127・
//! Issue #176）。
//!
//! `contrast_bench.rs` からのみ呼ばれ、`ParallelSearchProvider`（被検・`kernel.rs` の
//! `SearchProvider` 実装）と同一データ・同一クエリで比較するための「既存インプロセンス
//! ベクトル DB」側の総当たり（exact）検索を提供する。`usearch::Index::exact_search` を
//! 使うのは、近似（ANN・HNSW）検索と厳密最近傍を比較すると被検側が構造的に不利になり
//! 比率がゲートとして意味を持たなくなるため（`Cargo.toml` の `contrast-bench` feature
//! コメント参照）。
//!
//! `contrast-bench` feature 限定で `harness/mod.rs` から登録される（`#[cfg(feature =
//! "contrast-bench")]`）。feature 無効時はモジュール自体が存在せず、`usearch` の
//! C++ FFI を含む依存は一切コンパイル対象に入らない。

use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use super::stats::BenchError;

/// 対照エンジン（usearch）の総当たり検索インデックス。
///
/// キー（`u64`）は呼び出し元（`contrast_bench.rs`）が渡す行 id をそのまま使う契約
/// （`build` 参照）。exact search では HNSW グラフ品質は測定対象外のため、
/// `connectivity`・`expansion_add` はビルド時間短縮のため最小値にする
/// （測定対象は構築後の検索のみ）。
pub struct ContrastIndex {
    index: Index,
}

impl ContrastIndex {
    /// `ids[i]` と `vectors[i * dim .. (i + 1) * dim]` の対応でインデックスを構築する。
    /// スコアは engine 側（`kernel.rs` の内積カーネル）と一致させるため `MetricKind::IP`
    /// （内積。usearch は距離として `1 - dot` を最小化するため、最小距離＝最大内積となり
    /// Top-k の順序が一致する）を使う。`ids.len() != vectors.len() / dim` は呼び出し元の
    /// 契約違反であり、ここでは検証しない（`contrast_bench.rs` がベンチ入力生成時点で
    /// 保証する合成データのみを渡す）。
    pub fn build(ids: &[u64], vectors: &[f32], dim: usize) -> Result<Self, BenchError> {
        let options = IndexOptions {
            dimensions: dim,
            metric: MetricKind::IP,
            quantization: ScalarKind::F32,
            connectivity: 3,
            expansion_add: 4,
            expansion_search: 0,
            multi: false,
        };
        let index = Index::new(&options).map_err(|err| {
            BenchError::ExternalEngine(format!("usearch Index::new failed: {err}"))
        })?;
        index
            .reserve(ids.len())
            .map_err(|err| BenchError::ExternalEngine(format!("usearch reserve failed: {err}")))?;
        for (row, &id) in ids.iter().enumerate() {
            let start = row * dim;
            let end = start + dim;
            let vector = vectors.get(start..end).ok_or_else(|| {
                BenchError::ExternalEngine(format!(
                    "usearch add: vectors slice out of bounds for row {row}"
                ))
            })?;
            index
                .add(id, vector)
                .map_err(|err| BenchError::ExternalEngine(format!("usearch add failed: {err}")))?;
        }
        Ok(Self { index })
    }

    /// `query` に対する Top-`k` の厳密最近傍 id 列を、内積降順（近い順）で返す。
    /// `usearch::Index::exact_search` の `Err` は panic させず `BenchError` へ写像する
    /// （coding-rust.md: ライブラリコードで `Result` を返す方針。本コードは bench 専用
    /// だが同一の防御規律を保つ）。
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<u64>, BenchError> {
        let matches = self.index.exact_search(query, k).map_err(|err| {
            BenchError::ExternalEngine(format!("usearch exact_search failed: {err}"))
        })?;
        Ok(matches.keys)
    }
}
