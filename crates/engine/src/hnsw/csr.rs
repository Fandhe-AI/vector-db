//! 凍結済み HNSW グラフの CSR（Compressed Sparse Row）表現（Issue #494・親
//! #402。ポインタ: `docs/design/hnsw-index.md` §14）。
//!
//! `super::HnswIndex` は構築完了後、可変長ビルダー表現
//! （[`super::GraphBuilder`]。ノードごとに `Vec<Vec<u32>>` を個別確保する）を
//! 1 回だけ本モジュールの [`CsrGraph`] へ平坦化する（[`super::HnswIndex::
//! freeze_from`] 参照）。構築中（並列構築・`repair_reachability` の
//! `connect`／`shrink_links` による可変長 in-place 更新）は本モジュールに
//! 一切関与しない——「構築中は可変長を維持し、修復完了後に 1 回だけ CSR へ
//! 平坦化する」2 相構成の後半のみを担う。
//!
//! # レイアウト
//!
//! ノード昇順 → レベル昇順（各ノード `level_of(node)+1` 個のレベル分）に
//! 連結する exact-length・パディングなしの CSR。各リスト内の要素順は
//! [`super::GraphBuilder`] が持っていた順序をそのまま保持し、並べ替え・
//! ソートは一切行わない（同点境界の探索集合を変えないため。決定性ガード
//! `scripts/check_sort_determinism.sh` の対象外だが同じ理由で本モジュールも
//! ソートしない）。
//!
//! - `levels: Vec<u8>`（長さ `n`）: 各ノードのレベル。
//! - `node_base: Vec<u32>`（長さ `n+1`）: `node_base[i]` はノード `i` の
//!   (node, level) エントリが `offsets`／`links` 上で始まる位置。
//! - `offsets: Vec<u32>`（長さ `node_base[n]+1`）: `offsets[node_base[node]+level]`
//!   ..`offsets[node_base[node]+level+1]` が `links` 上の該当区間。
//! - `links: Vec<u32>`: 上記区間の連結。
//!
//! 空索引（`n == 0`）は `node_base = [0]`・`offsets = [0]`・`links = []`。
use super::{Adjacency, HnswError, Node};

/// 凍結済み HNSW グラフの CSR 表現。フィールドはすべて非公開（`super::hnsw`
/// モジュール内のみで組み立て・参照する）。
#[derive(Debug, Clone, Default)]
pub(crate) struct CsrGraph {
    levels: Vec<u8>,
    node_base: Vec<u32>,
    offsets: Vec<u32>,
    links: Vec<u32>,
}

impl CsrGraph {
    /// [`super::GraphBuilder`] が保持していた `Node` 列（`build` 完了・
    /// `repair_reachability` 完了後のもの）から CSR を組み立てる。
    /// ノード数・レベル・リンク総数はいずれも `HnswIndex::build` の入力検証
    /// （[`super::MAX_HNSW_NODES`]・[`super::MAX_LEVEL`]）で上限が決まって
    /// いるため、ここでの `checked_*`／`u8::try_from` 失敗は通常到達しない
    /// 防御的経路だが、上限定数の将来変更に備えて fail-closed に
    /// [`HnswError::CapacityOverflow`] を返す（coding-rust.md）。
    pub(crate) fn from_nodes(nodes: &[Node]) -> Result<Self, HnswError> {
        let n = nodes.len();
        let mut levels: Vec<u8> = Vec::with_capacity(n);
        let mut node_base: Vec<u32> = Vec::with_capacity(n + 1);
        node_base.push(0);

        let mut total_entries: u32 = 0;
        for node in nodes {
            let level_u8 = u8::try_from(node.level).map_err(|_| HnswError::CapacityOverflow)?;
            levels.push(level_u8);
            let entry_count = (node.level as u32)
                .checked_add(1)
                .ok_or(HnswError::CapacityOverflow)?;
            total_entries = total_entries
                .checked_add(entry_count)
                .ok_or(HnswError::CapacityOverflow)?;
            node_base.push(total_entries);
        }

        let mut offsets: Vec<u32> = Vec::with_capacity(total_entries as usize + 1);
        offsets.push(0);
        let mut total_links: u32 = 0;
        let mut links: Vec<u32> = Vec::new();
        for node in nodes {
            for level_links in &node.links {
                let len_u32 =
                    u32::try_from(level_links.len()).map_err(|_| HnswError::CapacityOverflow)?;
                total_links = total_links
                    .checked_add(len_u32)
                    .ok_or(HnswError::CapacityOverflow)?;
                offsets.push(total_links);
                links.extend_from_slice(level_links);
            }
        }

        Ok(CsrGraph {
            levels,
            node_base,
            offsets,
            links,
        })
    }

    /// 索引本体（4 配列）の概算ヒープバイト量（[`super::HnswIndex::
    /// approx_heap_bytes`] が `self.vectors` 分と合算する）。
    pub(crate) fn approx_heap_bytes(&self) -> usize {
        let levels_bytes = self
            .levels
            .capacity()
            .saturating_mul(std::mem::size_of::<u8>());
        let node_base_bytes = self
            .node_base
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let offsets_bytes = self
            .offsets
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let links_bytes = self
            .links
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        levels_bytes
            .saturating_add(node_base_bytes)
            .saturating_add(offsets_bytes)
            .saturating_add(links_bytes)
    }
}

impl Adjacency for CsrGraph {
    /// ノード `node` が割り当てられたレベル。範囲外は `None`
    /// （`Vec::get` のみを使い `unwrap`／`[]` を使わない。coding-rust.md）。
    fn level_of(&self, node: u32) -> Option<usize> {
        self.levels.get(node as usize).map(|&l| l as usize)
    }

    /// 層 `level` におけるノード `node` の隣接リスト。旧表現
    /// （`Vec<Vec<u32>>`。`nodes.get(node).and_then(|n| n.links.get(level))`）と
    /// 同じく、`level > level_of(node)` または `node` が範囲外なら `None` を
    /// 返す——CSR の exact-length オフセットだけでは「範囲内レベルだが
    /// リンク 0 件」の `Some(&[])` と区別できないため、レベル上限を明示的に
    /// 検査してから区間を引く。
    fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]> {
        let node_level = self.level_of(node)?;
        if level > node_level {
            return None;
        }
        let base = *self.node_base.get(node as usize)?;
        let entry_idx = (base as usize).checked_add(level)?;
        let start = *self.offsets.get(entry_idx)? as usize;
        let end = *self.offsets.get(entry_idx.checked_add(1)?)? as usize;
        self.links.get(start..end)
    }

    fn node_count(&self) -> usize {
        self.levels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(level: usize, links: Vec<Vec<u32>>) -> Node {
        Node { level, links }
    }

    #[test]
    fn from_nodes_round_trips_neighbors_level_of_and_node_count() {
        let nodes = vec![
            node(1, vec![vec![1, 2], vec![2]]),
            node(0, vec![vec![0]]),
            node(2, vec![vec![], vec![0, 1], vec![]]),
        ];
        let csr = CsrGraph::from_nodes(&nodes).unwrap();

        assert_eq!(csr.node_count(), 3);
        assert_eq!(csr.level_of(0), Some(1));
        assert_eq!(csr.level_of(1), Some(0));
        assert_eq!(csr.level_of(2), Some(2));

        assert_eq!(csr.neighbors(0, 0), Some(&[1u32, 2][..]));
        assert_eq!(csr.neighbors(1, 0), Some(&[2u32][..]));
        assert_eq!(csr.neighbors(0, 1), Some(&[0u32][..]));
        assert_eq!(csr.neighbors(0, 2), Some(&[][..]));
        assert_eq!(csr.neighbors(1, 2), Some(&[0u32, 1][..]));
        assert_eq!(csr.neighbors(2, 2), Some(&[][..]));
    }

    #[test]
    fn neighbors_returns_none_when_level_exceeds_node_level() {
        let nodes = vec![node(0, vec![vec![1]])];
        let csr = CsrGraph::from_nodes(&nodes).unwrap();
        assert_eq!(csr.neighbors(1, 0), None);
    }

    #[test]
    fn neighbors_and_level_of_return_none_for_out_of_range_node() {
        let nodes = vec![node(0, vec![vec![]])];
        let csr = CsrGraph::from_nodes(&nodes).unwrap();
        assert_eq!(csr.level_of(1), None);
        assert_eq!(csr.neighbors(0, 1), None);
    }

    #[test]
    fn empty_index_has_zero_node_count_and_no_neighbors() {
        let csr = CsrGraph::from_nodes(&[]).unwrap();
        assert_eq!(csr.node_count(), 0);
        assert_eq!(csr.level_of(0), None);
        assert_eq!(csr.neighbors(0, 0), None);
        // `node_base`／`offsets` は空索引でも 1 要素（`[0]`）を持つ（レイアウト
        // 契約）ため `approx_heap_bytes()` は 0 とは限らない（アロケータが
        // 割り当てる `capacity()` の実装依存の余剰を含み得る）。ここでは
        // 「呼び出しが panic しない」ことのみを固定する。
        let _ = csr.approx_heap_bytes();
    }
}
