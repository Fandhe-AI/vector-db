//! `engine::hnsw` の結合テスト（TASK-132・対象ビヘイビア: CORE-9・CORE-10。
//! ポインタ: `docs/design/ann-index-adoption.md`「実装ガイド（B 案）」節）。
//!
//! 受け入れ条件 (a)（小規模決定的フィクスチャでの層構造・次数上限・連結性の
//! 単体テスト）を、`tests/*.rs` の既存流儀（crate 外の公開 API のみで検証する。
//! `tests/isa.rs`・`tests/dispatch.rs` と同じ位置付け）に従って検証する。
//! 受け入れ条件 (b)（構築計算量が N log N にほぼ従うことの簡易ベンチ確認）は
//! `benches/hnsw_build_bench.rs`・`tests/hnsw_build_accept.rs` が担う。

use engine::hnsw::{HnswError, HnswIndex, HnswParams, MAX_EF, MAX_HNSW_NODES, MAX_LEVEL, MAX_M};

/// 決定的シードの xorshift64*（`benches/harness/rng.rs::DeterministicRng` と同
/// アルゴリズム。結合テストは crate 外の公開 API のみを使う流儀のため、bench
/// harness には依存せず本ファイル内に独立して複製する）。
struct TestRng {
    state: u64,
}

impl TestRng {
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

    fn next_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32
    }

    fn next_vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_f32() * 2.0 - 1.0).collect()
    }
}

fn gen_corpus(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
    let mut rng = TestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for _ in 0..rows {
        out.extend(rng.next_vector(dim));
    }
    out
}

const DIM: usize = 16;
const ROWS: usize = 800;
const SEED: u64 = 0xC0FF_EE12_3400;

fn build_fixture(params: HnswParams, seed: u64) -> (Vec<f32>, HnswIndex) {
    let vectors = gen_corpus(SEED, DIM, ROWS);
    let index = HnswIndex::build(params, DIM as u32, &vectors, seed).expect("build should succeed");
    (vectors, index)
}

#[test]
fn default_params_validate() {
    assert!(HnswParams::default().validate().is_ok());
}

#[test]
fn param_validation_rejects_out_of_range_values() {
    let cases = [
        HnswParams {
            m: 0,
            ..Default::default()
        },
        HnswParams {
            m: 1,
            ..Default::default()
        },
        HnswParams {
            m: MAX_M + 1,
            ..Default::default()
        },
        HnswParams {
            ef_construction: 0,
            ..Default::default()
        },
        HnswParams {
            ef_construction: MAX_EF + 1,
            ..Default::default()
        },
        HnswParams {
            ef_search: 0,
            ..Default::default()
        },
        HnswParams {
            ef_search: MAX_EF + 1,
            ..Default::default()
        },
    ];
    for params in cases {
        assert!(matches!(
            params.validate(),
            Err(HnswError::InvalidParams { .. })
        ));
        // validate で拒否されるパラメータは build でも同じ理由で拒否される
        // （build は validate を先頭で呼ぶ契約）。
        let vectors = gen_corpus(1, 4, 4);
        assert!(matches!(
            HnswIndex::build(params, 4, &vectors, 1),
            Err(HnswError::InvalidParams { .. })
        ));
    }
}

#[test]
fn build_rejects_dim_mismatch_and_zero_dim() {
    let vectors = vec![1.0f32, 2.0, 3.0];
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 4, &vectors, 1),
        Err(HnswError::DimMismatch { .. })
    ));
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 0, &vectors, 1),
        Err(HnswError::DimMismatch { .. })
    ));
}

#[test]
fn build_rejects_non_finite_vector() {
    let mut vectors = gen_corpus(2, 4, 4);
    vectors[5] = f32::NAN;
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 4, &vectors, 1),
        Err(HnswError::NonFiniteVector { .. })
    ));

    let mut vectors2 = gen_corpus(3, 4, 4);
    vectors2[9] = f32::INFINITY;
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 4, &vectors2, 1),
        Err(HnswError::NonFiniteVector { .. })
    ));
}

#[test]
fn build_empty_input_yields_empty_index() {
    let index = HnswIndex::build(HnswParams::default(), 8, &[], 1).unwrap();
    assert_eq!(index.len(), 0);
    assert!(index.is_empty());
    assert_eq!(index.entry_point(), None);
    assert_eq!(index.max_level(), None);
}

#[test]
fn same_seed_produces_identical_graph() {
    let params = HnswParams {
        m: 8,
        ef_construction: 40,
        ef_search: 20,
    };
    let (_, index_a) = build_fixture(params, 42);
    let (_, index_b) = build_fixture(params, 42);

    assert_eq!(index_a.len(), index_b.len());
    assert_eq!(index_a.entry_point(), index_b.entry_point());
    assert_eq!(index_a.max_level(), index_b.max_level());
    for node in 0..index_a.len() as u32 {
        assert_eq!(index_a.level_of(node), index_b.level_of(node));
        let level = index_a.level_of(node).unwrap();
        for l in 0..=level {
            assert_eq!(index_a.neighbors(l, node), index_b.neighbors(l, node));
        }
    }
}

#[test]
fn different_seed_yields_a_different_graph() {
    let params = HnswParams {
        m: 8,
        ef_construction: 40,
        ef_search: 20,
    };
    let (_, index_a) = build_fixture(params, 1);
    let (_, index_b) = build_fixture(params, 2);

    // 少なくとも 1 ノードのレベルまたは隣接集合が異なることを確認する
    // （完全一致は事実上ありえないが、決定論的な検証として明示比較する）。
    let mut any_diff = index_a.max_level() != index_b.max_level();
    if !any_diff {
        for node in 0..index_a.len() as u32 {
            if index_a.level_of(node) != index_b.level_of(node) {
                any_diff = true;
                break;
            }
            let level = index_a.level_of(node).unwrap();
            for l in 0..=level {
                if index_a.neighbors(l, node) != index_b.neighbors(l, node) {
                    any_diff = true;
                    break;
                }
            }
            if any_diff {
                break;
            }
        }
    }
    assert!(
        any_diff,
        "different seeds should not yield an identical graph"
    );
}

#[test]
fn layer_structure_invariants_hold() {
    let params = HnswParams {
        m: 8,
        ef_construction: 40,
        ef_search: 20,
    };
    let (_, index) = build_fixture(params, 7);

    let max_level = index.max_level().unwrap();
    assert!(max_level <= MAX_LEVEL);
    let entry = index.entry_point().unwrap();
    assert_eq!(index.level_of(entry), Some(max_level));

    let mut nodes_at_level_1_or_above = 0usize;
    for node in 0..index.len() as u32 {
        let level = index.level_of(node).unwrap();
        // 全ノードが層 0 に存在する。
        assert!(index.neighbors(0, node).is_some());
        // レベル l のノードは 0..=l 全層に隣接リスト（空でもよい）を持つ。
        for l in 0..=level {
            assert!(
                index.neighbors(l, node).is_some(),
                "node {node} missing layer {l} (level={level})"
            );
        }
        // レベルより上の層には属さない。
        assert!(index.neighbors(level + 1, node).is_none());
        if level >= 1 {
            nodes_at_level_1_or_above += 1;
        }
    }

    // レベル >= 1 のノード数が N/m のおおよその帯に収まること（緩い統計チェック。
    // 理論期待値は N * (1 - 1/m) 弱ではなく N/m に近い上位層在籍率）。
    let ratio = nodes_at_level_1_or_above as f64 / ROWS as f64;
    let expected = 1.0 / params.m as f64;
    assert!(
        ratio > expected * 0.3 && ratio < expected * 3.0,
        "ratio={ratio} expected~={expected}"
    );
}

/// `degree_limits_are_respected_and_links_are_well_formed`／
/// `degree_limits_are_respected_on_duplicate_heavy_corpus` 共通の不変条件
/// 検証（次数上限・自己ループなし・重複なし・隣接先の層整合）。
fn assert_degree_and_wellformed_invariants(index: &HnswIndex) {
    for node in 0..index.len() as u32 {
        let level = index.level_of(node).unwrap();
        for l in 0..=level {
            let neighbors = index.neighbors(l, node).unwrap();
            let limit = index.max_degree(l);
            assert!(
                neighbors.len() <= limit,
                "node {node} layer {l} exceeds degree limit: {} > {limit}",
                neighbors.len()
            );
            // 自己ループなし。
            assert!(!neighbors.contains(&node));
            // 重複なし。
            let mut sorted = neighbors.to_vec();
            sorted.sort_unstable(); // 集合の一意性検証のみに使う（順序規約とは無関係。id は全順序を持つ整数）
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                neighbors.len(),
                "node {node} layer {l} has duplicate neighbors"
            );
            // 隣接先が範囲内かつその層に属する。
            for &n in neighbors {
                assert!((n as usize) < index.len());
                let n_level = index.level_of(n).unwrap();
                assert!(
                    n_level >= l,
                    "neighbor {n} of node {node} does not reach layer {l}"
                );
            }
        }
    }
}

#[test]
fn degree_limits_are_respected_and_links_are_well_formed() {
    let params = HnswParams {
        m: 6,
        ef_construction: 32,
        ef_search: 16,
    };
    let (_, index) = build_fixture(params, 11);

    assert_degree_and_wellformed_invariants(&index);
    assert_eq!(index.max_degree(0), params.m * 2);
    assert_eq!(index.max_degree(1), params.m);
}

/// 到達性修復フェーズ 2（`hnsw.rs::repair_reachability`）は重複ヘビー
/// コーパスでは残存ノードの大半（数百件規模）を処理することが判明している
/// （codex-review #423 検証時の instrumentation で確認）。`build_fixture`
/// （ランダムコーパス）はフェーズ 2 をほとんど経由しないため、次数上限の
/// 単体テストをこの重複ヘビーコーパスにも適用し、フェーズ 2 の結線
/// （id 昇順の片方向チェーン＋ entry の犠牲リンク再結線。codex-review #423
/// P1・Bugbot 指摘の是正）が次数上限を破らないことを直接検証する。
#[test]
fn degree_limits_are_respected_on_duplicate_heavy_corpus() {
    let params = HnswParams {
        m: 6,
        ef_construction: 32,
        ef_search: 16,
    };
    for seed in 0..10u64 {
        let vectors = gen_duplicate_heavy_corpus(seed, 12, 400, 5);
        let index = HnswIndex::build(params, 12, &vectors, seed).expect("build should succeed");
        assert_degree_and_wellformed_invariants(&index);
    }
}

/// 層 `level` 上でエントリポイントから幅優先探索し、到達可能なノード集合を返す。
fn bfs_reachable(index: &HnswIndex, level: usize, start: u32) -> std::collections::HashSet<u32> {
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(node) = queue.pop_front() {
        if let Some(neighbors) = index.neighbors(level, node) {
            for &n in neighbors {
                if visited.insert(n) {
                    queue.push_back(n);
                }
            }
        }
    }
    visited
}

#[test]
fn each_layer_is_connected_from_the_entry_point() {
    let params = HnswParams {
        m: 8,
        ef_construction: 48,
        ef_search: 24,
    };
    let (_, index) = build_fixture(params, 55);
    let entry = index.entry_point().unwrap();
    let max_level = index.max_level().unwrap();
    // 上位層（level >= 1）が実際に構築・検証されることを保証する（上位層が
    // 常に 0 のまま偶然 green になる回帰を防ぐ）。
    assert!(
        max_level >= 1,
        "fixture did not exercise any upper layer (max_level={max_level})"
    );

    for level in 0..=max_level {
        // その層に属するノード集合を集める。
        let members: std::collections::HashSet<u32> = (0..index.len() as u32)
            .filter(|&n| index.level_of(n).map(|l| l >= level).unwrap_or(false))
            .collect();
        assert!(
            members.contains(&entry),
            "entry point missing from layer {level}"
        );
        let reachable = bfs_reachable(&index, level, entry);
        let missing: Vec<u32> = members.difference(&reachable).copied().collect();
        assert!(
            missing.is_empty(),
            "layer {level} not fully connected from entry point; unreachable nodes: {missing:?}"
        );
    }
}

/// `each_layer_is_connected_from_the_entry_point` と同じ判定（各層でエント
/// リポイントからの BFS が全メンバへ到達する）を任意の索引へ適用する共通化
/// ヘルパ。ランダム構成・重複ヘビーコーパスの連結性テスト（下記）から再利用
/// する。
fn assert_fully_connected_from_entry(index: &HnswIndex) {
    let Some(entry) = index.entry_point() else {
        return;
    };
    let Some(max_level) = index.max_level() else {
        return;
    };
    for level in 0..=max_level {
        let members: std::collections::HashSet<u32> = (0..index.len() as u32)
            .filter(|&n| index.level_of(n).map(|l| l >= level).unwrap_or(false))
            .collect();
        let reachable = bfs_reachable(index, level, entry);
        let missing: Vec<u32> = members.difference(&reachable).copied().collect();
        assert!(
            missing.is_empty(),
            "level {level} not fully connected from entry point; unreachable nodes: {missing:?}"
        );
    }
}

/// `clusters` 個のクラスタ中心を生成し、各行をいずれかの中心の**完全な複製**
/// にする（ジッタなし）。同一クラスタに属する行同士は任意のクエリに対し
/// 常に厳密同点スコアになるため、逆方向リンクの枝刈り・`search_layer` の
/// 停止／受理判定の双方を同時に強くストレスする決定的コーパス
/// （`docs/design/hnsw-graph-construction.md`「逆方向リンクの到達性保証」
/// 「`search_layer` の停止・受理判定: 順序規約の使い分け」各節参照）。
fn gen_duplicate_heavy_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<f32> {
    let mut rng = TestRng::new(seed);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1)).map(|_| rng.next_vector(dim)).collect();
    let mut out = Vec::with_capacity(rows * dim);
    for i in 0..rows {
        let center = &centers[i % centers.len()];
        out.extend_from_slice(center);
    }
    out
}

/// 逆方向リンク枝刈りによる到達不能・`search_layer` の同点早期打ち切りは
/// いずれも固定 1 fixture（`build_fixture`／`SEED`）では顕在化しなかった
/// 入力依存のバグだったため、複数 seed × 複数 `(rows, dim, m)` のランダム
/// 構成、および完全同点スコアを誘発する重複ヘビーコーパスの双方で最終
/// グラフの全層連結性を検証する（受け入れ条件 (a) の拡張）。
#[test]
fn randomized_configs_and_duplicate_heavy_corpus_stay_fully_connected() {
    struct Config {
        dim: usize,
        rows: usize,
        m: usize,
    }
    let configs = [
        Config {
            dim: 8,
            rows: 300,
            m: 4,
        },
        Config {
            dim: 16,
            rows: 500,
            m: 8,
        },
        Config {
            dim: 32,
            rows: 200,
            m: 16,
        },
    ];
    for (ci, cfg) in configs.iter().enumerate() {
        for seed_idx in 0..10u64 {
            let seed = (0xA5A5_0000_0000_0000u64) ^ ((ci as u64) << 32) ^ seed_idx;
            let vectors = gen_corpus(seed ^ 0x1111_1111, cfg.dim, cfg.rows);
            let params = HnswParams {
                m: cfg.m,
                ef_construction: 48,
                ef_search: 24,
            };
            let index = HnswIndex::build(params, cfg.dim as u32, &vectors, seed)
                .expect("build should succeed");
            assert_fully_connected_from_entry(&index);
        }
    }

    for seed in 0..10u64 {
        let vectors = gen_duplicate_heavy_corpus(seed, 12, 400, 5);
        let params = HnswParams {
            m: 6,
            ef_construction: 32,
            ef_search: 16,
        };
        let index = HnswIndex::build(params, 12, &vectors, seed).expect("build should succeed");
        assert_fully_connected_from_entry(&index);
    }
}

#[test]
fn build_rejects_node_count_beyond_max_hnsw_nodes() {
    // dim=1 なら MAX_HNSW_NODES+1 行分でも約 4 MB で確保可能（テストとして許容範囲）。
    // 上限超過は build の入力検証の先頭付近（次元整合の直後）で弾かれ、実際の
    // グラフ構築（O(N log N) 級の処理）へは進まない契約であることを確認する。
    let vectors = vec![0.0f32; MAX_HNSW_NODES + 1];
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 1, &vectors, 1),
        Err(HnswError::TooManyNodes { .. })
    ));
}
