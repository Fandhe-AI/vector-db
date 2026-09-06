//! `hnsw.rs::HnswIndex::search_layer` の隣接ノード探索ループへ挿入する
//! ソフトウェアパイプライン先読み（Issue #490）。hnswlib の
//! `searchBaseLayerST`（隣接 `j` を処理中に `j+1` の visited フラグ・
//! ベクトルを先読みしてキャッシュミスを隠蔽する構成）に倣うが、本モジュール
//! が発行するのは真の prefetch 命令ではなく「早期の demand load」である
//! （下記「stable での制約」節参照）。呼び出し元は `search_layer` のみ
//! （`pub(super)` 限定）。
//!
//! # P0 契約: 受理判定後にのみ触れる
//!
//! Issue #431 是正により `search_layer` は非受理（マスク外）ノードのベクトル・
//! visited スロットに一切触れない契約を持つ（`docs/design/hnsw-rls-cardinality-
//! switch.md` 参照）。本モジュールの先読みは `search_layer` 側で
//! `is_accepted` 判定を通過した後にのみ呼ばれる前提であり、モジュール自身は
//! 受理判定を行わない（呼び出し位置がこの契約を守る責務を持つ）。
//!
//! # stable での制約（Issue #490 計画時点で機械検証済み）
//!
//! `core::arch::{x86_64,aarch64}` の真の prefetch 命令
//! （`_mm_prefetch`／`_prefetch`）はいずれも `#[target_feature]` 付き関数の
//! 内側でのみ safe に呼べる（通常の fn から直接呼ぶと E0133）。本モジュールの
//! ように「新規 `unsafe` を追加しない」制約下では、`core::hint::black_box` に
//! よる早期 load で代替する（best-effort。ハードウェアの実プリフェッチャに
//! よる隠蔽は保証しない）。`core::hint::prefetch_read`（`hint_prefetch`
//! feature）が安定化するか、オーナー承認済みで `unsafe` を 1 箇所追加する
//! 判断がなされた場合、差し替え箇所は本モジュールの 2 関数に閉じている。
use std::hint::black_box;

use super::{node_vector, VisitedSet};

/// `node` のベクトル先頭 1 キャッシュラインぶんを早期に load する。
/// hnswlib／faiss も同様に先頭ラインのみを prefetch し、以降はハードウェアの
/// ストリームプリフェッチャに委ねる（連続レイアウトのため）。`node` が範囲外
/// （`node_vector` が `Err` を返す）の場合は何もしない（fail-closed。
/// coding-rust.md の untrusted 添字アクセス禁止に倣い `[]` は使わない）。
pub(super) fn touch_node_vector(vectors: &[f32], dim: usize, node: u32) {
    if let Ok(v) = node_vector(vectors, dim, node) {
        if let Some(first) = v.first() {
            black_box(*first);
        }
    }
}

/// visited 集合のスロットを早期に load する。`slot` は各 `VisitedSet` 実装が
/// 自身の内部表現（`VisitedScratch::epoch`・`VisitedBitmap::words`）から
/// `get()` で取り出した値（範囲外は `None`。呼び出し元が fail-closed に処理
/// 済みであることを前提とする）。
pub(super) fn touch_word<T: Copy>(slot: Option<&T>) {
    if let Some(v) = slot {
        black_box(*v);
    }
}

/// [`super::HnswIndex::search_layer`] の隣接ループへ先読みを差し込む戦略。
/// production 経路は [`PipelinePrefetch`]（ZST）のみを使い、`#[cfg(test)]` の
/// `NoPrefetch`／`RecordingPrefetch`（`hnsw.rs::tests` に定義）はビット同一性・
/// P0 契約の機械検証専用で production バイナリには到達しない。
pub(super) trait PrefetchPolicy {
    /// `node` を受理判定通過後に先読みする。`visited` は範囲外 `id` を
    /// `None` で返す `VisitedSet` 経由の値を想定し、本トレイトはその結果を
    /// そのまま `touch_word` へ渡すのみで受理判定は行わない。
    fn prefetch_neighbor<V: VisitedSet>(&self, node: u32, visited: &V, vectors: &[f32], dim: usize);
}

/// 唯一の production 実装（ZST）。`&P` は単相化されるためコストはゼロ。
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct PipelinePrefetch;

impl PrefetchPolicy for PipelinePrefetch {
    fn prefetch_neighbor<V: VisitedSet>(
        &self,
        node: u32,
        visited: &V,
        vectors: &[f32],
        dim: usize,
    ) {
        visited.prefetch_slot(node as usize);
        touch_node_vector(vectors, dim, node);
    }
}
