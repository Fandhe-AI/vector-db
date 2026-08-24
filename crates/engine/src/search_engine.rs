//! 検索エンジンの選択・構築レイヤ（TASK-131・対象ビヘイビア: CORE-9）。
//!
//! `core.rs` の [`crate::core::EngineCore::open`] から呼ばれ、`Box<dyn SearchProvider>`
//! （`kernel.rs::SearchProvider`、CORE-13）を構築して返す。CORE-9 が指す「総当たり実装を
//! 差し替え可能なインターフェース越しに呼び出す」という差し替え点は、独立の trait 階層を
//! 新設せず CORE-13 の provider 注入機構（`SearchProvider` trait・TASK-124 実装済み）へ
//! 一本化する。将来の ANN 実装は本モジュールの [`SearchEngineKind`] に選択肢を追加し、
//! 新しい `SearchProvider` 実装を返す分岐を [`build`] に加えるだけで、`core.rs` 側の
//! コア API（`EngineCore`／`VectorCore`）を変更せずに追加できる。
//!
//! エンジンの選択はコード上の明示指定（[`SearchEngineKind`] の値）のみで決まる。
//! 環境変数・設定ファイルによる実行時の経路上書き機構は設けない（`kernel.rs` に明記の
//! 方針を踏襲。CORE-12 の先取り、ディスパッチ決定表自体は TASK-155 の範囲外）。

use crate::kernel::{CpuScalarProvider, SearchProvider};
use crate::parallel_search::ParallelSearchProvider;

/// 選択可能な検索エンジン（総当たり系のみ。将来の ANN 追加に備え非網羅とする）。
///
/// 各 variant はいずれも `kernel.rs::SearchProvider` の実装を返す（CORE-13 の
/// 単一 trait 階層への一本化。本モジュールは新規 trait を定義しない）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngineKind {
    /// 単一スレッド・スカラー演算の参照実装（[`crate::kernel::CpuScalarProvider`]）。
    /// 他 provider の正解値検証用途が主で、既定エンジンではない。
    CpuScalarBruteForce,
    /// マルチスレッド並列の総当たり Top-k（[`crate::parallel_search::ParallelSearchProvider`]、
    /// TASK-126）。既定エンジン（[`default_engine`] 参照）。
    ParallelBruteForce,
}

/// `kind` に対応する `SearchProvider` 実装を構築する。
///
/// 呼び出し元（`core.rs::EngineCore::open` 等）はここで返る `Box<dyn SearchProvider>` を
/// そのまま `EngineCore::with_provider` へ渡す想定（object-safe な trait のため
/// ジェネリクスなしで受け渡しできる）。
pub fn build(kind: SearchEngineKind) -> Box<dyn SearchProvider> {
    match kind {
        SearchEngineKind::CpuScalarBruteForce => Box::new(CpuScalarProvider),
        SearchEngineKind::ParallelBruteForce => Box::new(ParallelSearchProvider),
    }
}

/// 既定の検索エンジンを構築する（`EngineCore::open` から呼ばれる既定経路）。
///
/// 現時点の既定は [`SearchEngineKind::ParallelBruteForce`]（マルチスレッド並列総当たり）。
/// 挙動は `EngineCore::open` が従来 `ParallelSearchProvider` を直接生成していたときと同一で、
/// 性能・結果の回帰は発生しない。
pub fn default_engine() -> Box<dyn SearchProvider> {
    build(SearchEngineKind::ParallelBruteForce)
}

#[cfg(test)]
mod tests {
    use super::*;

    // object-safety の固定（CORE-13 の一本化要件）: build/default_engine の戻り値型が
    // `Box<dyn SearchProvider>` であることをコンパイル時代入で固定する。
    #[test]
    fn build_and_default_return_boxed_search_provider() {
        let _cpu: Box<dyn SearchProvider> = build(SearchEngineKind::CpuScalarBruteForce);
        let _parallel: Box<dyn SearchProvider> = build(SearchEngineKind::ParallelBruteForce);
        let _default: Box<dyn SearchProvider> = default_engine();
    }
}
