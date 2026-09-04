//! HNSW 構築の並列化（Issue #406・親 #402・前提 #404/#405。TASK-132・対象
//! ビヘイビア CORE-9・CORE-10）。`super`（`hnsw.rs`）の子モジュールであり、
//! 呼び出し元は [`super::HnswIndex::build_with_threads`]／
//! [`super::HnswIndex::build_parallel`] のみ（本モジュールの関数・型は
//! すべて非公開）。
//!
//! pgvector `hnswbuild.c` のロック粒度設計（隣接リストは要素＝ノード単位
//! ロック、エントリポイント更新のみ排他）・qdrant の逐次プレフィックス方式
//! （孤立成分の発生防止）を参考にした（手法名・ライセンスのみの参照——
//! pgvector: PostgreSQL License／qdrant: Apache-2.0。コード転記はしない）。
//!
//! # ロック不変条件
//!
//! 同時に 2 つ以上のノードロック（[`BuildGraph::links`]）を保持しない。
//! [`BuildGraph::connect`] は `from` のみ、[`BuildGraph::shrink_links`] は
//! 対象ノードのみをロックし、呼び出しごとに取得・解放してから次のロックを
//! 取る（[`insert_node_locked`] 内で `connect(node_id, neighbor)` →
//! `connect(neighbor, node_id)` → `shrink_links(neighbor)` と逐次に呼ぶ）。
//! エントリポイントロック（[`BuildGraph::entry`]）とノードロックも入れ子に
//! しない。これによりデッドロックの可能性を構造的に排除する。
//!
//! # 決定性の範囲
//!
//! レベル割当（[`BuildGraph::levels`]）は並列フェーズ開始前に `seed` から
//! 逐次確定し、以後は読み取り専用（ロック不要）。挿入順序はワークスティール
//! （`AtomicUsize::fetch_add`）による非決定的な順序になるため、構築される
//! グラフの**形状**は `threads >= 2` かつ `n > SEQUENTIAL_PREFIX_NODES` の
//! 場合、同一 `seed` でも run-to-run で変わり得る。[`super::HnswIndex::
//! search`] の決定性契約「同一索引・同一クエリで再現」自体は、構築された
//! グラフがどの形状であっても不変（探索は構築後の固定グラフに対してのみ
//! 動く）。`docs/design/hnsw-parallel-build.md` 参照。
//!
//! # 失敗契約
//!
//! ワーカーの `Err`（`NonFiniteScore` 等）は最初の 1 件を保存し、`AtomicBool`
//! の停止フラグで全ワーカーを早期終了させる。全ハンドルを必ず `join` した
//! うえで判定する（`parallel_search.rs::join_all_or_panicked` と同じ理由。
//! 未 join のハンドルが残ったまま `thread::scope` を抜けると panic が
//! そのまま伝播し fail-closed のエラー変換に到達しない）。ワーカー panic・
//! ロック poison はいずれも [`super::HnswError::WorkerPanicked`] へ変換し、
//! 部分的に結線された索引を `Ok` で返さない。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{
    assign_level, compute_shrink, max_degree_for, node_vector, score_of,
    select_neighbors_heuristic_free, DeterministicRng, HnswError, HnswIndex, HnswParams, Node,
    ScoredNode, VisitedScratch,
};

/// 並列構築中の共有グラフ状態。ノード単位 `RwLock` で隣接リストを保護し、
/// エントリポイントのみ別ロックで排他する（モジュール冒頭「ロック
/// 不変条件」）。`levels` は並列フェーズ開始前に確定し以後不変（ロック不要）。
struct BuildGraph {
    dim: usize,
    params: HnswParams,
    vectors: Arc<[f32]>,
    levels: Vec<usize>,
    links: Vec<RwLock<Vec<Vec<u32>>>>,
    entry: RwLock<Option<u32>>,
}

impl BuildGraph {
    fn new(params: HnswParams, dim: usize, vectors: Arc<[f32]>, levels: Vec<usize>) -> Self {
        let links = levels
            .iter()
            .map(|&level| RwLock::new(vec![Vec::new(); level + 1]))
            .collect();
        Self {
            dim,
            params,
            vectors,
            levels,
            links,
            entry: RwLock::new(None),
        }
    }

    fn node_count(&self) -> usize {
        self.levels.len()
    }

    fn read_links(&self, node: u32) -> Result<RwLockReadGuard<'_, Vec<Vec<u32>>>, HnswError> {
        self.links
            .get(node as usize)
            .ok_or(HnswError::CapacityOverflow)?
            .read()
            .map_err(|_| HnswError::WorkerPanicked)
    }

    fn write_links(&self, node: u32) -> Result<RwLockWriteGuard<'_, Vec<Vec<u32>>>, HnswError> {
        self.links
            .get(node as usize)
            .ok_or(HnswError::CapacityOverflow)?
            .write()
            .map_err(|_| HnswError::WorkerPanicked)
    }

    /// 層 `level` におけるノード `node` の隣接 id 集合をロック下で複製して
    /// 返す（読み取りロックの保持時間を最小化するため、スコア計算はロック
    /// 解放後に行う。モジュール冒頭のコメント参照）。
    fn neighbors_copy(&self, level: usize, node: u32) -> Result<Vec<u32>, HnswError> {
        let guard = self.read_links(node)?;
        Ok(guard.get(level).cloned().unwrap_or_default())
    }

    /// `from -> to` への単方向リンクを層 `level` へ追加する（`from` のみを
    /// ロックする。次数上限の適用は呼び出し元の [`Self::shrink_links`] が担う）。
    fn connect(&self, from: u32, to: u32, level: usize) -> Result<(), HnswError> {
        if from == to {
            return Ok(());
        }
        let mut guard = self.write_links(from)?;
        if let Some(links) = guard.get_mut(level) {
            if !links.contains(&to) {
                links.push(to);
            }
        }
        Ok(())
    }

    /// `node` の隣接リストが次数上限を超えていれば縮退する。書き込みロック
    /// 1 回の中で「現在のリンクを読む → `compute_shrink`（純粋関数。
    /// `hnsw.rs` が [`super::HnswIndex::shrink_links`] とも共有する）で
    /// 再選択を計算 → 書き戻す」を原子的に行うため、複数ワーカーが同じ
    /// ノードを同時に縮退しようとしても競合しない。
    fn shrink_links(&self, node: u32, level: usize, protect: u32) -> Result<(), HnswError> {
        let limit = max_degree_for(&self.params, level);
        let mut guard = self.write_links(node)?;
        let current: Vec<u32> = guard.get(level).cloned().unwrap_or_default();
        if let Some(shrunk) =
            compute_shrink(&current, node, self.dim, &self.vectors, limit, protect)?
        {
            if let Some(links) = guard.get_mut(level) {
                *links = shrunk;
            }
        }
        Ok(())
    }

    /// `level` が現在のエントリレベルより高い場合のみエントリポイントを
    /// 更新する（pgvector 由来の「エントリポイント更新のみ排他」設計。
    /// 書き込みロック内で現在値を再読込してから判定するため、複数ワーカーが
    /// 同時に高いレベルへ昇格しようとしても最終的に最大レベルのノードが残る）。
    fn try_promote_entry(&self, node_id: u32, level: usize) -> Result<(), HnswError> {
        let mut guard = self.entry.write().map_err(|_| HnswError::WorkerPanicked)?;
        let should_update = match *guard {
            Some(ep) => level > self.levels.get(ep as usize).copied().unwrap_or(0),
            None => true,
        };
        if should_update {
            *guard = Some(node_id);
        }
        Ok(())
    }

    fn entry_snapshot(&self) -> Result<Option<u32>, HnswError> {
        Ok(*self.entry.read().map_err(|_| HnswError::WorkerPanicked)?)
    }
}

/// `ef=1` の貪欲降下（[`super::HnswIndex::greedy_descend`] のロック対応版。
/// 上位層のナビゲーション用。アルゴリズムは同一で、隣接アクセスのみ
/// [`BuildGraph::neighbors_copy`] を介する）。
fn greedy_descend_locked(
    graph: &BuildGraph,
    start: u32,
    query: &[f32],
    level: usize,
) -> Result<u32, HnswError> {
    let mut current = start;
    let mut current_best = ScoredNode {
        node: current,
        score: score_of(&graph.vectors, graph.dim, current, query)?,
    };
    loop {
        let mut improved = false;
        let neighbors = graph.neighbors_copy(level, current)?;
        for cand in neighbors {
            let cand_scored = ScoredNode {
                node: cand,
                score: score_of(&graph.vectors, graph.dim, cand, query)?,
            };
            if cand_scored > current_best {
                current = cand;
                current_best = cand_scored;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    Ok(current)
}

/// [`super::HnswIndex::search_layer`]（Algorithm 2）のロック対応版。アルゴリズム
/// （停止・受理判定を含む順序規約）は完全に同一で、隣接アクセスのみ
/// [`BuildGraph::neighbors_copy`] を介する（`hnsw.rs` モジュール冒頭
/// 「順序規約」節の契約を踏襲する）。
fn search_layer_locked(
    graph: &BuildGraph,
    entry_points: Vec<u32>,
    query: &[f32],
    ef: usize,
    level: usize,
    visited: &mut VisitedScratch,
) -> Result<Vec<ScoredNode>, HnswError> {
    use std::collections::BinaryHeap;

    visited.reset(graph.node_count());
    let mut candidates: BinaryHeap<ScoredNode> = BinaryHeap::new();
    let mut results: BinaryHeap<std::cmp::Reverse<ScoredNode>> = BinaryHeap::new();

    for ep in entry_points {
        match visited.mark_visited(ep as usize) {
            Some(true) => continue,
            Some(false) => {}
            None => continue,
        }
        let score = score_of(&graph.vectors, graph.dim, ep, query)?;
        let scored = ScoredNode { node: ep, score };
        candidates.push(scored);
        results.push(std::cmp::Reverse(scored));
    }

    while let Some(top_candidate) = candidates.pop() {
        if let Some(std::cmp::Reverse(worst)) = results.peek() {
            let strictly_farther =
                top_candidate.score.total_cmp(&worst.score) == std::cmp::Ordering::Less;
            if results.len() >= ef && strictly_farther {
                break;
            }
        }

        let neighbors = graph.neighbors_copy(level, top_candidate.node)?;
        for neighbor in neighbors {
            let already = match visited.mark_visited(neighbor as usize) {
                Some(seen) => seen,
                None => continue,
            };
            if already {
                continue;
            }
            let neighbor_score = score_of(&graph.vectors, graph.dim, neighbor, query)?;
            let scored = ScoredNode {
                node: neighbor,
                score: neighbor_score,
            };
            let worst_ok = match results.peek() {
                Some(std::cmp::Reverse(worst)) => {
                    results.len() < ef
                        || scored.score.total_cmp(&worst.score) != std::cmp::Ordering::Less
                }
                None => true,
            };
            if worst_ok {
                candidates.push(scored);
                results.push(std::cmp::Reverse(scored));
                if results.len() > ef {
                    results.pop();
                }
            }
        }
    }

    let mut out: Vec<ScoredNode> = results.into_iter().map(|r| r.0).collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    Ok(out)
}

/// 1 ノードをグラフへ挿入する（[`super::HnswIndex::insert_node`] のロック
/// 対応版。Algorithm 1 相当）。並列フェーズで呼ばれる前提として、エントリ
/// ポイントは常に `Some`（呼び出し元が先に逐次プレフィックスを挿入済み。
/// [`build_parallel_graph`] 参照）であることを要求し、`None` は
/// `WorkerPanicked` として fail-closed に扱う（この分岐へは通常到達しない
/// 防御的経路）。
fn insert_node_locked(
    graph: &BuildGraph,
    node_id: u32,
    visited: &mut VisitedScratch,
) -> Result<(), HnswError> {
    let level = graph
        .levels
        .get(node_id as usize)
        .copied()
        .ok_or(HnswError::CapacityOverflow)?;
    let dim = graph.dim;
    let query = node_vector(&graph.vectors, dim, node_id)?;

    let current_entry = graph.entry_snapshot()?.ok_or(HnswError::WorkerPanicked)?;
    let top_level = graph
        .levels
        .get(current_entry as usize)
        .copied()
        .unwrap_or(0);

    // (i) 上位層を ef=1 の貪欲降下でたどり、挿入ノードのレベル直上での
    // 最近傍 1 件をエントリポイントとして絞り込む。
    let mut nearest = current_entry;
    if top_level > level {
        for l in ((level + 1)..=top_level).rev() {
            nearest = greedy_descend_locked(graph, nearest, query, l)?;
        }
    }

    // (ii) 挿入ノードのレベル以下の各層で ef_construction 幅の探索 →
    // ヒューリスティック近傍選択 → 双方向リンク。
    let mut entry_candidates = vec![nearest];
    for l in (0..=level.min(top_level)).rev() {
        let candidates = search_layer_locked(
            graph,
            entry_candidates.clone(),
            query,
            graph.params.ef_construction,
            l,
            visited,
        )?;
        let selected =
            select_neighbors_heuristic_free(&candidates, graph.params.m, dim, &graph.vectors)?;

        for &neighbor in &selected {
            graph.connect(node_id, neighbor, l)?;
            graph.connect(neighbor, node_id, l)?;
            graph.shrink_links(neighbor, l, node_id)?;
        }

        // `node_id` 自身の層 `l` リストは `select_neighbors_heuristic_free` に
        // よって `<= params.m` 本に収まるはずだが（逐次経路 `insert_node` は
        // これを前提に自身の縮退を省略する）、並列経路ではこの前提が崩れる:
        // `node_id` が既に上位層で結線済みで他ノードから発見可能な間に、
        // 別のノード `other` の挿入処理が `node_id` を `other` 自身の近傍として
        // 選ぶと、その `insert_node_locked` 内 `for &neighbor in &selected`
        // ループが `neighbor = node_id` として `graph.connect(node_id, other,
        // l)`（`node_id` 自身のリストへ `other` を追加する逆方向リンク）と
        // `graph.shrink_links(node_id, l, other)` を呼ぶ。これが `node_id`
        // 自身の挿入処理がまだ進行中の間（自身のループでさらに
        // `graph.connect(node_id, own_neighbor, l)` を呼んでいる最中）に
        // 割り込むと、`other` 側の `shrink_links` は `other` を保護するのみで
        // `node_id` 自身が後から追加する分を考慮しないため、縮退なしでは
        // `node_id` の最終的なリストが次数上限を超え得る（Issue #406 実装時に
        // 不変条件テストで再現・発見した並列固有のレース。単一スレッド経路
        // には存在しない）。`protect=node_id` は `compute_shrink` の
        // `node != protect` 分岐を満たさない（`node == protect`）ため強制
        // 保護なしの純粋な上位 `limit` 件選択として働く。
        graph.shrink_links(node_id, l, node_id)?;

        entry_candidates = if candidates.is_empty() {
            vec![nearest]
        } else {
            candidates.iter().map(|c| c.node).collect()
        };
    }

    // (vi) 挿入ノードのレベルが現行最大層を超える可能性がある場合のみ
    // エントリポイント更新を試みる（実際に更新するかは `try_promote_entry`
    // が書き込みロック内で再読込のうえ判定する。モジュール冒頭参照）。
    if level > top_level {
        graph.try_promote_entry(node_id, level)?;
    }

    Ok(())
}

/// [`super::HnswIndex::build_with_threads`] から呼ばれる並列構築の本体。
/// `n > SEQUENTIAL_PREFIX_NODES`・`threads >= 2` であることを呼び出し元が
/// 保証する（`super::HnswIndex::build_with_threads` がそれ以外は
/// [`super::HnswIndex::build`] へ縮退させる）。
///
/// 1. レベル割当を `seed` から逐次確定する（要件 4: スレッド数に依らない
///    決定性）。
/// 2. 先頭 [`super::SEQUENTIAL_PREFIX_NODES`] 件を単一スレッドで挿入する
///    （qdrant 方式。孤立成分の発生を防ぐ）。
/// 3. 残りを `threads` 本のワーカーでワークスティール方式（`AtomicUsize::
///    fetch_add`）で並列挿入する。
/// 4. 凍結（`RwLock::into_inner`）して [`super::HnswIndex`] を組み立て、
///    既存の `repair_reachability`（単一スレッド後始末。`super::HnswIndex::
///    build` と共有）を実行する。
pub(crate) fn build_parallel_graph(
    params: HnswParams,
    dim: u32,
    vectors: &[f32],
    seed: u64,
    threads: usize,
    n: usize,
) -> Result<HnswIndex, HnswError> {
    let dim_usize = dim as usize;

    let mut rng = DeterministicRng::new(seed);
    let levels: Vec<usize> = (0..n).map(|_| assign_level(&mut rng, params.m)).collect();

    let owned_vectors: Arc<[f32]> = Arc::from(vectors);
    let graph = BuildGraph::new(params, dim_usize, owned_vectors.clone(), levels);

    let prefix_end = super::SEQUENTIAL_PREFIX_NODES.min(n);
    let mut seq_visited = VisitedScratch::default();
    for node_idx in 0..prefix_end {
        let node_id = node_idx as u32;
        if node_idx == 0 {
            // 最初のノードは近傍探索なしで直ちにエントリポイントになる
            // （`super::HnswIndex::insert_node` の `entry_point.is_none()`
            // 分岐と同じ挙動）。
            graph.try_promote_entry(node_id, graph.levels[0])?;
            continue;
        }
        insert_node_locked(&graph, node_id, &mut seq_visited)?;
    }

    let next = AtomicUsize::new(prefix_end);
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<HnswError>> = Mutex::new(None);

    // 全ハンドルを必ず join してから判定する（モジュール冒頭「失敗契約」・
    // `parallel_search.rs::join_all_or_panicked` と同じ理由）。
    let any_panicked = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let graph_ref = &graph;
            let next_ref = &next;
            let stop_ref = &stop;
            let first_error_ref = &first_error;
            handles.push(scope.spawn(move || {
                let mut visited = VisitedScratch::default();
                loop {
                    if stop_ref.load(Ordering::SeqCst) {
                        break;
                    }
                    let i = next_ref.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let node_id = i as u32;
                    if let Err(e) = insert_node_locked(graph_ref, node_id, &mut visited) {
                        let mut fe = first_error_ref
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        if fe.is_none() {
                            *fe = Some(e);
                        }
                        stop_ref.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }));
        }
        let mut any_panicked = false;
        for h in handles {
            if h.join().is_err() {
                any_panicked = true;
            }
        }
        any_panicked
    });

    if any_panicked {
        return Err(HnswError::WorkerPanicked);
    }
    if let Some(e) = first_error
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner())
    {
        return Err(e);
    }

    freeze(graph, params, dim, vectors, dim_usize, owned_vectors)
}

/// 並列フェーズ完了後の `BuildGraph` を [`super::HnswIndex`] へ凍結する。
/// 各ノードの `RwLock` を `into_inner` で消費し（`Vec<Node>` への再コピー
/// なし）、最後に既存の逐次後始末（`repair_reachability`。並列フェーズが
/// 生みうる上位層の到達不能ノードを閉じる。モジュール冒頭「決定性の範囲」
/// 節参照）を実行する。
fn freeze(
    graph: BuildGraph,
    params: HnswParams,
    dim: u32,
    original_vectors: &[f32],
    dim_usize: usize,
    owned_vectors: Arc<[f32]>,
) -> Result<HnswIndex, HnswError> {
    let entry_point = graph
        .entry
        .into_inner()
        .map_err(|_| HnswError::WorkerPanicked)?;
    let levels = graph.levels;
    let mut nodes: Vec<Node> = Vec::with_capacity(graph.links.len());
    for (idx, lock) in graph.links.into_iter().enumerate() {
        let links = lock.into_inner().map_err(|_| HnswError::WorkerPanicked)?;
        let level = levels.get(idx).copied().unwrap_or(0);
        nodes.push(Node { level, links });
    }

    let mut index = HnswIndex {
        params,
        dim,
        nodes,
        entry_point,
        vectors: owned_vectors,
    };
    index.repair_reachability(dim_usize, original_vectors)?;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hnsw::{HnswIndex as PubHnswIndex, MAX_BUILD_THREADS};

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

    /// `BuildGraph` のノードロックが poison した場合（ワーカーがロック保持中に
    /// panic した想定）、凍結が `WorkerPanicked` として fail-closed に拒否
    /// されることを確認する（モジュール冒頭「失敗契約」節）。
    #[test]
    fn freeze_reports_worker_panicked_when_a_node_lock_is_poisoned() {
        let params = HnswParams::default();
        let dim = 4usize;
        let vectors: Vec<f32> = vec![0.0; dim * 3];
        let owned: Arc<[f32]> = Arc::from(vectors.as_slice());
        let graph = BuildGraph::new(params, dim, owned.clone(), vec![0, 0, 0]);

        // 意図的にロック保持中に panic させ poison を作る（`catch_unwind` で
        // テストプロセス自体を落とさずに済ませる）。
        let lock = &graph.links[0];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.write().unwrap();
            panic!("intentional poison for WorkerPanicked test");
        }));
        assert!(result.is_err());
        assert!(lock.is_poisoned());

        let err = freeze(graph, params, dim as u32, &vectors, dim, owned).unwrap_err();
        assert_eq!(err, HnswError::WorkerPanicked);
    }

    #[test]
    fn build_with_threads_one_matches_sequential_build_exactly() {
        let dim = 6usize;
        let rows = 800usize;
        let vectors = gen_corpus(0x1234_5678, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 32,
        };
        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, 7).unwrap();
        let parallel =
            PubHnswIndex::build_with_threads(params, dim as u32, &vectors, 7, 1).unwrap();

        assert_eq!(sequential.entry_point(), parallel.entry_point());
        assert_eq!(sequential.max_level(), parallel.max_level());
        for node in 0..rows as u32 {
            assert_eq!(sequential.level_of(node), parallel.level_of(node));
            let seq_level = sequential.level_of(node).unwrap();
            for level in 0..=seq_level {
                assert_eq!(
                    sequential.neighbors(level, node),
                    parallel.neighbors(level, node),
                    "node={node} level={level}"
                );
            }
        }
    }

    #[test]
    fn build_with_threads_rejects_zero_and_over_max() {
        let params = HnswParams::default();
        let vectors = gen_corpus(1, 4, 10);
        assert!(matches!(
            PubHnswIndex::build_with_threads(params, 4, &vectors, 1, 0).unwrap_err(),
            HnswError::InvalidParams { .. }
        ));
        assert!(matches!(
            PubHnswIndex::build_with_threads(params, 4, &vectors, 1, MAX_BUILD_THREADS + 1)
                .unwrap_err(),
            HnswError::InvalidParams { .. }
        ));
    }

    #[test]
    fn build_with_threads_small_n_matches_sequential_build_regardless_of_threads() {
        // n <= SEQUENTIAL_PREFIX_NODES では並列フェーズが起動せず、常に
        // `build` と同一のグラフになる。
        let dim = 4usize;
        let rows = 100usize; // < SEQUENTIAL_PREFIX_NODES(256)
        let vectors = gen_corpus(0xAAAA, dim, rows);
        let params = HnswParams::default();
        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, 3).unwrap();
        let parallel =
            PubHnswIndex::build_with_threads(params, dim as u32, &vectors, 3, 4).unwrap();
        assert_eq!(sequential.entry_point(), parallel.entry_point());
        for node in 0..rows as u32 {
            assert_eq!(sequential.level_of(node), parallel.level_of(node));
        }
    }
}
