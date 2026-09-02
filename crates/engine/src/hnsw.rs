//! HNSW（Hierarchical Navigable Small World）グラフの構築（TASK-132・対象ビヘイビア:
//! CORE-9・CORE-10。ポインタ: `docs/design/ann-index-adoption.md`「実装ガイド（B 案）」節）。
//!
//! 本モジュールの範囲は**グラフ構築のみ**（Malkov & Yashunin 2016 の Algorithm 1〜4 相当）。
//! 探索 API（`ef_search` を使った top-k 探索）・並列構築・`search_engine.rs` への
//! `SearchEngineKind::Hnsw` 結線・世代整合キャッシュ・RLS 統合・永続化はいずれも
//! 別タスク（#405〜#409）の担当であり、本モジュールは触れない。ADR
//! （`docs/design/ann-index-adoption.md`）の「非契約的な実装詳細」区分に基づき、
//! spec 側の確定を待たずに着手している。
//!
//! # ベクトルの所有方針（#405・#408 への申し送り）
//!
//! [`HnswIndex`] はグラフ（隣接リスト・レベル・エントリポイント）のみを保持し、
//! ベクトル本体は複製しない。呼び出し元（`arena.rs::VectorArena` や #408 の
//! 世代整合キャッシュ）が row-major 連続バッファとして所有し続け、[`HnswIndex::build`]
//! へは `&[f32]` で借用のみ渡す契約とする。768 次元 × 100 万行では複製だけで
//! 3 GB 級になるため、この方針はメモリ効率上の要請である。探索 API（#405）も
//! 同じ `vectors: &[f32]` を受け取り、`len() * dim() == vectors.len()` を毎回
//! 検証する（fail-closed。呼び出し元がビルド後にベクトル集合を差し替えてしまう
//! 事故を検出する唯一の手段が長さ照合であるため）。
//!
//! # 距離カーネル
//!
//! `kernel::dot`（`isa.rs` の実行時検出 SIMD カーネルへの唯一の委譲経路）を使う。
//! スコアは内積で「大きいほど近い」（`kernel.rs::CandidateHit` と同じ規約。cosine
//! 距離は呼び出し元が正規化済みベクトルを渡すことで内積に一致させる既存契約を
//! 踏襲する）。ヒューリスティックの `d(e, q) < d(e, r)` は本モジュール内では
//! `dot(e, q) > dot(e, r)` と読み替える。
//!
//! # 順序規約
//!
//! 候補ヒープの同点タイブレークは `kernel.rs::MinHeapItem` と同じ「スコア
//! `total_cmp` 降順・同点は id（ノード番号）昇順」を踏襲する。`f32` は全順序を
//! 持たないため `total_cmp` を使い、非有限値は構築入力の段で拒否する
//! （[`HnswError::NonFiniteVector`]）ため探索段では常に有限値のみを比較する。
//! ソートは安定ソート（`sort_by`）のみを使う（`sort_unstable_by` 系は
//! `scripts/check_sort_determinism.sh` が禁止する）。
//!
//! # レベル割当の決定性・非暗号 PRNG
//!
//! `assign_level` は本モジュール専用の xorshift64*（`benches/harness/rng.rs::
//! DeterministicRng` と同アルゴリズム。`src/` からは bench harness を参照できない
//! ため小さく複製する）を `seed` で初期化し、同一 `seed`・同一入力なら
//! 完全に同一のグラフを構築する。**非暗号 PRNG であり、鍵・トークン等の
//! セキュリティ用途に転用してはならない**（OWASP A02）。
//!
//! # 上限・untrusted 入力の扱い（coding-rust.md・security.md）
//!
//! `dim`・`vectors.len()` の整合、[`MAX_HNSW_NODES`]・[`MAX_LEVEL`] による上限、
//! `HnswParams::validate` による `m`・`ef_construction`・`ef_search` の上限
//! （[`MAX_M`]・[`MAX_EF`]）を構築前に検証する。オフセット計算は `checked_*`、
//! ノード id 変換は `u32::try_from` を使い、スライス添字は `get()` のみで
//! `unwrap`／`expect`／`[]` を使わない。`unsafe` は使わない。環境変数・feature flag
//! による経路上書きは設けない（CORE-12 踏襲）。

use std::collections::BinaryHeap;
use std::fmt;

use crate::kernel::dot;

/// 次数上限の安全上限（DoS 防止。`HnswParams::validate` が `m` をこの値以下に
/// 制限する）。
pub const MAX_M: usize = 128;

/// `ef_construction`／`ef_search` の上限（`core.rs::MAX_SEARCH_K` と同値。
/// untrusted な呼び出し元がここを起点に無制限の候補集合を要求できないようにする）。
pub const MAX_EF: usize = 10_000;

/// 構築可能なノード数の上限（`arena::MAX_ARENA_ROWS` と同値。ノード id を `u32` で
/// 表現できることの裏付けでもある）。
pub const MAX_HNSW_NODES: usize = 1_000_000;

/// 層数の絶対上限（`assign_level` の結果をこの値でクランプする。理論上は対数
/// オーダーで極小確率の外れ値しか生じないが、上限を設けないと未検証の外れ値が
/// `Vec` のネストを無制限に増やしうるため fail-closed に固定する）。
pub const MAX_LEVEL: usize = 32;

/// HNSW 構築パラメータ。
///
/// 既定値（`M=16`／`ef_construction=100`／`ef_search=64`）は ADR 起票 Issue #403
/// に記載の本リポ採用値（非規範的な実装既定値。spec 側の確定値ではない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    /// 挿入後に各ノードが層 1 以上で保持する隣接数の目安（層 0 は `2*m` まで許容する。
    /// Malkov & Yashunin 2016 の記法と同じ）。
    pub m: usize,
    /// 挿入時の貪欲探索の候補幅。
    pub ef_construction: usize,
    /// 探索時の候補幅（本モジュールでは構築後のパラメータ保持のみ。実際の探索は #405）。
    pub ef_search: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 100,
            ef_search: 64,
        }
    }
}

impl HnswParams {
    /// パラメータの範囲検証。`m < 2`（`m=1` だとレベル割当の `mL = 1/ln(m)` が
    /// 定義できない・次数上限が実用にならない）・`m > MAX_M`・`ef_construction == 0`・
    /// `ef_construction > MAX_EF`・`ef_search == 0`・`ef_search > MAX_EF` を拒否する。
    pub fn validate(&self) -> Result<(), HnswError> {
        if self.m < 2 {
            return Err(HnswError::InvalidParams {
                reason: "m must be >= 2",
            });
        }
        if self.m > MAX_M {
            return Err(HnswError::InvalidParams {
                reason: "m exceeds MAX_M",
            });
        }
        if self.ef_construction == 0 {
            return Err(HnswError::InvalidParams {
                reason: "ef_construction must be >= 1",
            });
        }
        if self.ef_construction > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef_construction exceeds MAX_EF",
            });
        }
        if self.ef_search == 0 {
            return Err(HnswError::InvalidParams {
                reason: "ef_search must be >= 1",
            });
        }
        if self.ef_search > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef_search exceeds MAX_EF",
            });
        }
        Ok(())
    }
}

/// [`HnswIndex::build`] の失敗要因。`Display`／`std::error::Error` を実装し
/// ライブラリコードとして panic せず `Result` で契約する（coding-rust.md）。
/// ベクトル値そのものは含めない（テナント情報・行データを含まない索引という
/// 本モジュールの契約を、エラー経路でも壊さないため）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HnswError {
    /// `HnswParams::validate` が拒否した。
    InvalidParams { reason: &'static str },
    /// `dim == 0`、または `vectors.len()` が `dim` の整数倍でない。
    DimMismatch { dim: u32, len: usize },
    /// ノード数が [`MAX_HNSW_NODES`] を超える。
    TooManyNodes { nodes: usize },
    /// 入力ベクトルに NaN／Inf が含まれる（構築後に順序を壊す経路を作らないため
    /// 構築段で拒否する）。
    NonFiniteVector { node: usize },
    /// オフセット・容量計算が `usize`／`u32` の範囲を超えた。
    CapacityOverflow,
}

impl fmt::Display for HnswError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HnswError::InvalidParams { reason } => write!(f, "invalid HNSW params: {reason}"),
            HnswError::DimMismatch { dim, len } => {
                write!(
                    f,
                    "vector buffer length {len} is not a multiple of dim {dim}"
                )
            }
            HnswError::TooManyNodes { nodes } => {
                write!(
                    f,
                    "node count {nodes} exceeds MAX_HNSW_NODES ({MAX_HNSW_NODES})"
                )
            }
            HnswError::NonFiniteVector { node } => {
                write!(f, "vector for node {node} contains a non-finite value")
            }
            HnswError::CapacityOverflow => write!(f, "capacity computation overflowed"),
        }
    }
}

impl std::error::Error for HnswError {}

/// 決定的シードの xorshift64* PRNG（`benches/harness/rng.rs::DeterministicRng` と
/// 同アルゴリズム。`src/` から bench harness を参照できないため独立に複製する）。
///
/// 非暗号 PRNG。レベル割当専用でありセキュリティ用途に転用しない（OWASP A02）。
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `(0.0, 1.0]` の単位区間に収まる f64 疑似乱数値を返す（`0` を除外するのは
    /// `assign_level` が `-ln(u)` を取るため。`0` だと `ln(0) = -inf` になり
    /// レベルが発散する）。
    fn next_open01(&mut self) -> f64 {
        // 53bit の仮数部精度で一様分布を作る（f64 の仮数部ビット数に合わせる）。
        let bits = self.next_u64() >> 11; // 53bit
        let u = (bits as f64) / (1u64 << 53) as f64;
        // u ∈ [0, 1) を (0, 1] へシフトする（1.0 - u なら u=0 のとき 1.0 になる）。
        1.0 - u
    }
}

/// レベル割当（Malkov & Yashunin 2016 Algorithm 1 の `l = floor(-ln(unif(0,1)) * mL)`）。
/// `mL = 1 / ln(m)`。結果は [`MAX_LEVEL`] でクランプする（`HnswParams::validate` が
/// `m >= 2` を保証するため `ln(m)` は必ず正の有限値になる）。
fn assign_level(rng: &mut DeterministicRng, m: usize) -> usize {
    let m_l = 1.0 / (m as f64).ln();
    let u = rng.next_open01();
    let level = (-u.ln() * m_l).floor();
    if !level.is_finite() || level <= 0.0 {
        0
    } else {
        (level as usize).min(MAX_LEVEL)
    }
}

/// 候補ヒープの要素。スコア降順・同点は id 昇順を「強い」とする順序を `Ord` に
/// 埋め込む（`kernel.rs::MinHeapItem` と同じ規約。構築入力は事前に非有限値を
/// 拒否済みのため `total_cmp` の呼び出しは常に有限値同士になる）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScoredNode {
    node: u32,
    score: f32,
}

impl Eq for ScoredNode {}

impl PartialOrd for ScoredNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then(other.node.cmp(&self.node))
    }
}

/// 1 ノード分のグラフ状態。`links[l]` が層 `l` の隣接リスト（`l` は `0..=level`）。
/// ベクトル本体は持たない（モジュール冒頭「ベクトルの所有方針」参照）。
#[derive(Debug)]
struct Node {
    level: usize,
    links: Vec<Vec<u32>>,
}

/// 構築済み HNSW グラフ。ベクトル本体を保持しない（[`HnswIndex::build`] の
/// 呼び出し元が所有し続ける借用契約。モジュール冒頭コメント参照）。
#[derive(Debug)]
pub struct HnswIndex {
    params: HnswParams,
    dim: u32,
    nodes: Vec<Node>,
    entry_point: Option<u32>,
}

/// 層 `level` におけるノードの隣接リスト最大次数を返す（層 0 は `2*m`、
/// 層 1 以上は `m`。[`HnswIndex::max_degree`] の内部実装から共有する）。
fn max_degree_for(params: &HnswParams, level: usize) -> usize {
    if level == 0 {
        params.m.saturating_mul(2)
    } else {
        params.m
    }
}

impl HnswIndex {
    /// row-major 連続バッファ（`VectorArena::vectors()` と同レイアウト。
    /// `vectors[node * dim .. node * dim + dim]` が `node` 番目のベクトル）から
    /// 単一スレッドで構築する。
    ///
    /// 検証順序: パラメータ → 次元整合 → ノード数上限 → 非有限値。空入力
    /// （`vectors` が空）は空索引を返す（エラーにしない。呼び出し元が未挿入の
    /// テーブルへ構築を試みる自然なケースのため）。
    pub fn build(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        params.validate()?;

        if dim == 0 {
            return Err(HnswError::DimMismatch {
                dim,
                len: vectors.len(),
            });
        }
        let dim_usize = dim as usize;
        if !vectors.len().is_multiple_of(dim_usize) {
            return Err(HnswError::DimMismatch {
                dim,
                len: vectors.len(),
            });
        }
        let n = vectors.len() / dim_usize;
        if n > MAX_HNSW_NODES {
            return Err(HnswError::TooManyNodes { nodes: n });
        }
        // ノード id を u32 で表現できることを構築前に確定させる（MAX_HNSW_NODES は
        // u32::MAX よりずっと小さいため通常は失敗しないが、上限定数の将来変更に
        // 備えて明示的に検証する。coding-rust.md: `checked_*`／`try_into` の使用）。
        if u32::try_from(n).is_err() {
            return Err(HnswError::CapacityOverflow);
        }

        for (node_idx, chunk) in vectors.chunks_exact(dim_usize).enumerate() {
            if chunk.iter().any(|v| !v.is_finite()) {
                return Err(HnswError::NonFiniteVector { node: node_idx });
            }
        }

        let mut index = HnswIndex {
            params,
            dim,
            nodes: Vec::with_capacity(n),
            entry_point: None,
        };
        if n == 0 {
            return Ok(index);
        }

        let mut rng = DeterministicRng::new(seed);
        // 挿入順はノード番号昇順に固定する（呼び出し元の入力順＝挿入順。決定性の
        // 唯一の自由度は `seed` によるレベル割当だけにする）。
        for node_idx in 0..n {
            let level = assign_level(&mut rng, params.m);
            let node_id = node_idx as u32; // n <= MAX_HNSW_NODES であることを上で検証済み
            index.insert_node(node_id, level, dim_usize, vectors)?;
        }

        Ok(index)
    }

    /// 1 ノードをグラフへ挿入する（Algorithm 1 相当）。探索段（不変参照のみ）と
    /// 結線段（可変）を関数分離しておくのは、#406（並列構築）が要素単位ロックへ
    /// 差し替える際にこの境界をそのまま流用できるようにするため。
    fn insert_node(
        &mut self,
        node_id: u32,
        level: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<(), HnswError> {
        self.nodes.push(Node {
            level,
            links: vec![Vec::new(); level + 1],
        });

        let query = node_vector(vectors, dim, node_id)?;

        let (current_entry, top_level) = match self.entry_point {
            Some(ep) => {
                let ep_level = self.level_of(ep).unwrap_or(0);
                (ep, ep_level)
            }
            None => {
                self.entry_point = Some(node_id);
                return Ok(());
            }
        };

        // (i) 上位層（挿入ノードのレベルより上）を ef=1 の貪欲降下でたどり、
        // 挿入ノードのレベル直上での最近傍 1 件をエントリポイントとして絞り込む。
        let mut nearest = current_entry;
        if top_level > level {
            for l in ((level + 1)..=top_level).rev() {
                nearest = self.greedy_descend(nearest, query, l, dim, vectors)?;
            }
        }

        // (ii) 挿入ノードのレベル以下の各層で ef_construction 幅の探索 →
        // ヒューリスティック近傍選択 → 双方向リンク。
        let mut entry_candidates = vec![nearest];
        for l in (0..=level.min(top_level)).rev() {
            let candidates = self.search_layer(
                entry_candidates.clone(),
                query,
                self.params.ef_construction,
                l,
                dim,
                vectors,
            )?;
            // 層 0 は次数上限が最大 2*m まで許容される（`shrink_links` が参照する
            // `max_degree_for` 側で扱う）が、新規ノード自身の選択本数は Algorithm 1
            // の記法どおり層を問わず常に `m` 本にする。
            let selected =
                self.select_neighbors_heuristic(&candidates, self.params.m, dim, vectors)?;

            for &neighbor in &selected {
                self.connect(node_id, neighbor, l);
                self.connect(neighbor, node_id, l);
                self.shrink_links(neighbor, l, dim, vectors)?;
            }

            entry_candidates = if candidates.is_empty() {
                vec![nearest]
            } else {
                candidates.iter().map(|c| c.node).collect()
            };
        }

        // (vi) 挿入ノードのレベルが現行最大層を超えるならエントリポイントを更新する。
        if level > top_level {
            self.entry_point = Some(node_id);
        }

        Ok(())
    }

    /// `ef=1` の貪欲降下（Algorithm 2 の `ef=1` 特殊形。上位層のナビゲーション用）。
    fn greedy_descend(
        &self,
        start: u32,
        query: &[f32],
        level: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<u32, HnswError> {
        let mut current = start;
        let mut current_score = self.score(current, query, dim, vectors)?;
        loop {
            let mut improved = false;
            if let Some(neighbors) = self.neighbors(level, current) {
                for &cand in neighbors {
                    let cand_score = self.score(cand, query, dim, vectors)?;
                    if cand_score > current_score {
                        current = cand;
                        current_score = cand_score;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
        Ok(current)
    }

    /// `search_layer`（Algorithm 2）。層 `level` 上で `entry_points` から出発し、
    /// 幅 `ef` の貪欲拡張探索を行い、`dot` 降順（同点は id 昇順）に並んだ最大 `ef`
    /// 件の候補を返す。`pub(crate)` にして #405（探索 API）が `ef_search` で再利用
    /// できるようにする。
    ///
    /// visited は呼び出しごとに新規の `Vec<bool>` を確保する（構築段は挿入ノード
    /// あたり高々 `level+1` 回しか呼ばれず、ホットパスではないため。#405 が
    /// クエリ多発経路で再利用する場合はエポック方式へ差し替える余地を残す）。
    pub(crate) fn search_layer(
        &self,
        entry_points: Vec<u32>,
        query: &[f32],
        ef: usize,
        level: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<Vec<ScoredNode>, HnswError> {
        let mut visited = vec![false; self.nodes.len()];
        let mut candidates: BinaryHeap<ScoredNode> = BinaryHeap::new();
        // 結果集合は最小ヒープとして扱いたいので `Reverse` で包む。
        let mut results: BinaryHeap<std::cmp::Reverse<ScoredNode>> = BinaryHeap::new();

        for ep in entry_points {
            if let Some(slot) = visited.get_mut(ep as usize) {
                if *slot {
                    continue;
                }
                *slot = true;
            } else {
                continue;
            }
            let score = self.score(ep, query, dim, vectors)?;
            let scored = ScoredNode { node: ep, score };
            candidates.push(scored);
            results.push(std::cmp::Reverse(scored));
        }

        while let Some(top_candidate) = candidates.pop() {
            // 候補集合の最良要素が、結果集合中の最悪要素より劣るなら打ち切る
            // （Algorithm 2 の停止条件）。
            if let Some(std::cmp::Reverse(worst)) = results.peek() {
                if results.len() >= ef && top_candidate.score < worst.score {
                    break;
                }
            }

            if let Some(neighbors) = self.neighbors(level, top_candidate.node) {
                for &neighbor in neighbors {
                    let already = match visited.get_mut(neighbor as usize) {
                        Some(slot) => {
                            let seen = *slot;
                            *slot = true;
                            seen
                        }
                        None => continue,
                    };
                    if already {
                        continue;
                    }
                    let neighbor_score = self.score(neighbor, query, dim, vectors)?;
                    let worst_ok = match results.peek() {
                        Some(std::cmp::Reverse(worst)) => {
                            results.len() < ef || neighbor_score > worst.score
                        }
                        None => true,
                    };
                    if worst_ok {
                        let scored = ScoredNode {
                            node: neighbor,
                            score: neighbor_score,
                        };
                        candidates.push(scored);
                        results.push(std::cmp::Reverse(scored));
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }

        let mut out: Vec<ScoredNode> = results.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        Ok(out)
    }

    /// 近傍選択ヒューリスティック（Algorithm 4）。既定は `extend_candidates=false`・
    /// `keep_pruned_connections=true`（余った枠を枝刈り済み候補で埋め、次数を
    /// 確保する）。この既定の採用理由・#405 での見直し余地は
    /// `docs/design/hnsw-graph-construction.md` に記録する。`extend_candidates=true`
    /// 形（候補の隣接をさらに候補へ加える拡張）は実装しない（到達しない分岐は
    /// 検証されないままコードに残り将来のバグ源になるため。既定を有効化する
    /// 場合に別途実装する）。
    fn select_neighbors_heuristic(
        &self,
        candidates: &[ScoredNode],
        m: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<Vec<u32>, HnswError> {
        const KEEP_PRUNED_CONNECTIONS: bool = true;

        // 候補は search_layer が既にスコア降順で返すため、優先度付きキューへ
        // 詰め直す代わりにそのまま消費できるが、Algorithm 4 の記法に合わせて
        // 「未処理候補」を降順に保った Vec として扱う。
        let working: Vec<ScoredNode> = candidates.to_vec();

        let mut selected: Vec<ScoredNode> = Vec::new();
        let mut discarded: Vec<ScoredNode> = Vec::new();

        for cand in working {
            if selected.len() >= m {
                break;
            }
            let cand_vec = node_vector(vectors, dim, cand.node)?;
            // 「候補が既選択集合のどの要素よりも近い場合のみ採用する」枝刈り規則
            // （Algorithm 4）。dot は大きいほど近いため `>` が「より近い」の向き。
            let mut keep = true;
            for &sel in &selected {
                let sel_vec = node_vector(vectors, dim, sel.node)?;
                let d_to_selected = dot(cand_vec, sel_vec);
                if d_to_selected > cand.score {
                    keep = false;
                    break;
                }
            }
            if keep {
                selected.push(cand);
            } else {
                discarded.push(cand);
            }
        }

        if KEEP_PRUNED_CONNECTIONS {
            let mut i = 0;
            while selected.len() < m {
                let Some(extra) = discarded.get(i) else {
                    break;
                };
                selected.push(*extra);
                i += 1;
            }
        }

        selected.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        Ok(selected.into_iter().map(|s| s.node).collect())
    }

    /// `from -> to` への単方向リンクを層 `level` へ追加する（重複・自己ループは
    /// 追加しない）。次数上限の適用は呼び出し元の [`Self::shrink_links`] が担う。
    fn connect(&mut self, from: u32, to: u32, level: usize) {
        if from == to {
            return;
        }
        let Some(node) = self.nodes.get_mut(from as usize) else {
            return;
        };
        let Some(links) = node.links.get_mut(level) else {
            return;
        };
        if !links.contains(&to) {
            links.push(to);
        }
    }

    /// `node` の層 `level` における隣接数が次数上限を超えていれば、その隣接集合
    /// 全体へヒューリスティック近傍選択を再適用して上限内へ縮退させる
    /// （Algorithm 1 の「次数上限超過時の再選択」段）。
    fn shrink_links(
        &mut self,
        node: u32,
        level: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<(), HnswError> {
        let limit = max_degree_for(&self.params, level);
        let current_len = self
            .nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .map(|l| l.len())
            .unwrap_or(0);
        if current_len <= limit {
            return Ok(());
        }

        let node_vec = node_vector(vectors, dim, node)?;
        let neighbor_ids: Vec<u32> = self
            .nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .cloned()
            .unwrap_or_default();

        let mut scored: Vec<ScoredNode> = Vec::with_capacity(neighbor_ids.len());
        for id in neighbor_ids {
            let v = node_vector(vectors, dim, id)?;
            scored.push(ScoredNode {
                node: id,
                score: dot(node_vec, v),
            });
        }
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));

        let shrunk = self.select_neighbors_heuristic(&scored, limit, dim, vectors)?;
        if let Some(n) = self.nodes.get_mut(node as usize) {
            if let Some(links) = n.links.get_mut(level) {
                *links = shrunk;
            }
        }
        Ok(())
    }

    /// `dot(node, query)`。ノード id が範囲外／`vectors` が短すぎる場合は
    /// `NonFiniteVector` 相当ではなく、境界検証は [`node_vector`] が担う
    /// （呼び出し元は構築時に検証済みの id しか渡さない内部専用パス）。
    fn score(
        &self,
        node: u32,
        query: &[f32],
        dim: usize,
        vectors: &[f32],
    ) -> Result<f32, HnswError> {
        let v = node_vector(vectors, dim, node)?;
        Ok(dot(v, query))
    }

    /// 構築済みパラメータを返す。
    pub fn params(&self) -> &HnswParams {
        &self.params
    }

    /// ベクトルの次元数（`build` に渡した `dim`）。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 構築済みノード数。
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// ノード数が 0 か。
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// グラフ全体の最大層（エントリポイントのレベル）。空索引では `None`。
    pub fn max_level(&self) -> Option<usize> {
        self.entry_point.and_then(|ep| self.level_of(ep))
    }

    /// エントリポイントのノード id。空索引では `None`。
    pub fn entry_point(&self) -> Option<u32> {
        self.entry_point
    }

    /// ノード `node` が割り当てられたレベル。存在しないノードは `None`。
    pub fn level_of(&self, node: u32) -> Option<usize> {
        self.nodes.get(node as usize).map(|n| n.level)
    }

    /// 層 `level` におけるノード `node` の隣接リスト。存在しない層・ノードは
    /// `None`（ノードのレベルが `level` 未満の場合を含む）。
    pub fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]> {
        self.nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .map(|l| l.as_slice())
    }

    /// 層 `level` における最大次数（層 0 は `2*m`、層 1 以上は `m`）。テスト・
    /// `EXPLAIN`（#411 の担当範囲）が参照する想定の公開ヘルパ。
    pub fn max_degree(&self, level: usize) -> usize {
        max_degree_for(&self.params, level)
    }
}

/// row-major バッファから `node` 番目のベクトルスライスを取り出す。範囲外
/// アクセスは `[]` を使わず `get()` で検出し、`CapacityOverflow` として拒否する
/// （coding-rust.md: untrusted 添字アクセス禁止。本関数はモジュール内部専用だが
/// `node_id` は `u32::try_from` 済みの構築時検証を経ているため、通常この分岐へは
/// 到達しない防御的経路である）。
fn node_vector(vectors: &[f32], dim: usize, node: u32) -> Result<&[f32], HnswError> {
    let start = (node as usize)
        .checked_mul(dim)
        .ok_or(HnswError::CapacityOverflow)?;
    let end = start.checked_add(dim).ok_or(HnswError::CapacityOverflow)?;
    vectors.get(start..end).ok_or(HnswError::CapacityOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_corpus(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
        let mut rng = DeterministicRng::new(seed);
        let mut out = Vec::with_capacity(rows * dim);
        for _ in 0..rows {
            for _ in 0..dim {
                let bits = rng.next_u64() >> 40;
                let f = (bits as f32) / (1u32 << 24) as f32;
                out.push(f * 2.0 - 1.0);
            }
        }
        out
    }

    #[test]
    fn default_params_match_documented_values() {
        let p = HnswParams::default();
        assert_eq!(p.m, 16);
        assert_eq!(p.ef_construction, 100);
        assert_eq!(p.ef_search, 64);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_rejects_out_of_range_params() {
        assert!(HnswParams {
            m: 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            m: MAX_M + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_construction: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_construction: MAX_EF + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_search: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_search: MAX_EF + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn assign_level_is_deterministic_for_same_seed() {
        let m = 16;
        let mut a = DeterministicRng::new(42);
        let mut b = DeterministicRng::new(42);
        let levels_a: Vec<usize> = (0..1000).map(|_| assign_level(&mut a, m)).collect();
        let levels_b: Vec<usize> = (0..1000).map(|_| assign_level(&mut b, m)).collect();
        assert_eq!(levels_a, levels_b);
    }

    #[test]
    fn assign_level_distribution_is_roughly_geometric() {
        // P(level >= 1) はおおむね 1/m。厳密な統計検定ではなく、明らかな実装崩れ
        // （例: 常に 0 を返す・毎回 MAX_LEVEL に張り付く）を検知する緩い帯で確認する。
        let m = 16usize;
        let mut rng = DeterministicRng::new(7);
        let samples = 20_000;
        let at_least_one = (0..samples)
            .filter(|_| assign_level(&mut rng, m) >= 1)
            .count();
        let ratio = at_least_one as f64 / samples as f64;
        let expected = 1.0 / m as f64;
        assert!(
            ratio > expected * 0.5 && ratio < expected * 2.0,
            "ratio={ratio} expected~={expected}"
        );
    }

    #[test]
    fn select_neighbors_heuristic_prunes_redundant_close_candidates() {
        // 2 次元の単位ベクトルで手動検証可能な配置を作る（dot を cosine 類似度として
        // 扱う既定契約に合わせ、正規化済みベクトルで構成する）:
        // クエリ = 0°、候補 A = 10°（クエリに極めて近い）、候補 B = 12°（A に
        // さらに近く、A に対して冗長）、候補 C = -80°（クエリからは離れているが
        // A からも離れており、A を経由してクエリより近づける代替ルートにならない）。
        // m=2 で選ぶと、B は「A に対する方がクエリに対するより近い」ため枝刈りされ、
        // A・C が採用される（B は A の近傍探索で別途到達可能なため、A・B を両方
        // 直結するのは次数の無駄という Algorithm 4 の意図どおりの挙動）。
        let dim = 2usize;
        let a = [0.9848f32, 0.1736f32]; // 10°
        let b = [0.9781f32, 0.2079f32]; // 12°（A に極めて近い）
        let c = [0.1736f32, -0.9848f32]; // -80°（A からも離れている）
        let vectors: Vec<f32> = [a, b, c].concat();
        let params = HnswParams {
            m: 2,
            ..Default::default()
        };
        let index = HnswIndex {
            params,
            dim: dim as u32,
            nodes: vec![
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            entry_point: Some(0),
        };
        let query = &[1.0f32, 0.0f32];
        let candidates = vec![
            ScoredNode {
                node: 0,
                score: dot(&a, query),
            },
            ScoredNode {
                node: 1,
                score: dot(&b, query),
            },
            ScoredNode {
                node: 2,
                score: dot(&c, query),
            },
        ];
        let selected = index
            .select_neighbors_heuristic(&candidates, 2, dim, &vectors)
            .unwrap();
        assert!(selected.contains(&0));
        // B(1) は A(0) に対してクエリより近いため枝刈りされ、C(2) が代わりに採用される。
        assert!(selected.contains(&2));
        assert!(!selected.contains(&1));
    }

    #[test]
    fn build_rejects_dim_mismatch() {
        let err = HnswIndex::build(HnswParams::default(), 4, &[1.0, 2.0, 3.0], 1).unwrap_err();
        assert!(matches!(err, HnswError::DimMismatch { .. }));
    }

    #[test]
    fn build_rejects_zero_dim() {
        let err = HnswIndex::build(HnswParams::default(), 0, &[1.0, 2.0], 1).unwrap_err();
        assert!(matches!(err, HnswError::DimMismatch { .. }));
    }

    #[test]
    fn build_rejects_non_finite_vector() {
        let vectors = vec![1.0, f32::NAN, 0.0, 1.0];
        let err = HnswIndex::build(HnswParams::default(), 2, &vectors, 1).unwrap_err();
        assert!(matches!(err, HnswError::NonFiniteVector { .. }));
    }

    #[test]
    fn build_empty_input_yields_empty_index() {
        let index = HnswIndex::build(HnswParams::default(), 4, &[], 1).unwrap();
        assert_eq!(index.len(), 0);
        assert!(index.is_empty());
        assert_eq!(index.entry_point(), None);
        assert_eq!(index.max_level(), None);
    }

    #[test]
    fn search_layer_finds_true_nearest_neighbor_in_small_corpus() {
        let dim = 8;
        let rows = 300;
        let vectors = gen_corpus(0xABCD_1234, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 99).unwrap();

        // 総当たりで真の最近傍を求め、ef_construction 幅の search_layer 結果に
        // 含まれる率がおおむね高いことを検証する（結合テストの受け入れ条件は
        // tests/hnsw.rs 側で厳密に検証するため、ここでは内部関数の健全性のみ確認）。
        let mut hits = 0;
        let queries = 20;
        for q in 0..queries {
            let query = gen_corpus(0x1111_0000 + q as u64, dim, 1);
            let mut brute: Vec<ScoredNode> = (0..rows as u32)
                .map(|n| ScoredNode {
                    node: n,
                    score: dot(node_vector(&vectors, dim, n).unwrap(), &query),
                })
                .collect();
            brute.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
            let true_nearest = brute[0].node;

            let ep = index.entry_point().unwrap();
            let found = index
                .search_layer(vec![ep], &query, params.ef_construction, 0, dim, &vectors)
                .unwrap();
            if found.iter().any(|c| c.node == true_nearest) {
                hits += 1;
            }
        }
        assert!(hits as f64 / queries as f64 >= 0.9, "hits={hits}/{queries}");
    }
}
