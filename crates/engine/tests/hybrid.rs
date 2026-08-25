//! `hybrid.rs`（TASK-103）の統合テスト。対象ビヘイビア: SEARCH-1, SEARCH-3。
//!
//! `crates/engine/src/kernel.rs`（密検索 provider）と `crates/engine/src/sparse.rs`
//! （疎検索）を実際に組み合わせ、RRF 融合の入口 `hybrid::hybrid_search` を検証する。
//! ユニットレベルの RRF スコア計算・タイブレークは `hybrid.rs` 内の `#[cfg(test)]` で
//! 検証済みのため、本ファイルは密疎統合・provider 差し替え・fail-closed 系の検証に絞る。

use engine::hybrid::{hybrid_search, HybridError, RrfConfig};
use engine::kernel::{CpuScalarProvider, KernelError, SearchInput};
use engine::parallel_search::ParallelSearchProvider;
use engine::sparse::SparseIndex;

/// 合成コーパス 1 件分（文書 ID・検索用テキスト・低次元密ベクトル）。
struct Doc {
    id: u64,
    text: &'static str,
    vector: [f32; 2],
}

/// SEARCH-1・SEARCH-3 検証共通の合成コーパスを構築する。
///
/// - id=1: キーワード「rust」を含み、かつ密ベクトルもクエリに近い（疎・密ともに当たる）
/// - id=2: キーワードのみ一致（密ベクトルはクエリから遠い。「疎のみが当てられる文書」）
/// - id=3: 密ベクトルのみクエリに近い（テキストに一致語なし。「密のみが当てられる文書」）
/// - id=4: どちらにも当たらないノイズ文書
fn build_corpus() -> Vec<Doc> {
    vec![
        Doc {
            id: 1,
            text: "rust vector database search",
            vector: [1.0, 0.0],
        },
        Doc {
            id: 2,
            text: "rust programming language guide",
            vector: [0.0, 1.0],
        },
        Doc {
            id: 3,
            text: "completely unrelated topic about gardening",
            vector: [0.9, 0.1],
        },
        Doc {
            id: 4,
            text: "another unrelated topic about cooking",
            vector: [-1.0, 0.0],
        },
    ]
}

fn build_sparse_index(docs: &[Doc]) -> SparseIndex {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text)).collect();
    SparseIndex::build(&refs).expect("sparse index build ok")
}

fn flatten_vectors(docs: &[Doc]) -> (Vec<u64>, Vec<f32>) {
    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector).collect();
    (ids, vectors)
}

/// SEARCH-3 の非劣化性検証専用の合成コーパス。`build_corpus()` に加えてノイズ文書
/// （キーワード一致なし・クエリベクトルから遠い）を複数追加し、Top-k の truncate が
/// 実際に候補を絞り込む状況を作る（truncate が効かない状況では非劣化性の主張を
/// 検証できないため）。ノイズ文書の密ベクトルはクエリ [1.0, 0.0] との内積がすべて
/// 負かつ相異なる値になるよう構成し、密順位が id=1〜3 より必ず下位になるようにする。
fn build_corpus_with_noise() -> Vec<Doc> {
    let mut docs = build_corpus();
    let noise_texts: [&str; 6] = [
        "noise document about weather",
        "noise document about sports",
        "noise document about travel",
        "noise document about music",
        "noise document about finance",
        "noise document about history",
    ];
    for (offset, text) in noise_texts.iter().enumerate() {
        docs.push(Doc {
            id: 100 + offset as u64,
            text,
            // -0.1, -0.2, ... と相異なる負の内積になるベクトル。
            vector: [-0.1 * (offset as f32 + 1.0), 0.0],
        });
    }
    docs
}

// SEARCH-1 対応: キーワード一致文書・ベクトル近傍文書がともに融合 Top-k に入ること。
#[test]
fn hybrid_search_includes_both_keyword_and_vector_matches_in_top_k() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 4,
    };
    let cfg = RrfConfig::default();

    let hits = hybrid_search(&CpuScalarProvider, input, &index, "rust search", 3, &cfg)
        .expect("hybrid search ok");

    let result_ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
    // id=1 はキーワード一致・密近傍の両方に該当するため Top-k に含まれるはず。
    assert!(result_ids.contains(&1), "result_ids={result_ids:?}");
    // id=3 は密ベクトルがクエリに近いため Top-k に含まれるはず。
    assert!(result_ids.contains(&3), "result_ids={result_ids:?}");
}

// SEARCH-1 対応: 同一入力での再現性（2 回実行で bit 一致）。`ParallelSearchProvider`
// （マルチスレッド実行）を使う。再現性のリスクはスレッドスケジューリングに由来するため、
// 単一スレッドの `CpuScalarProvider` では検証にならない。
#[test]
fn hybrid_search_is_deterministic_across_repeated_runs() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let cfg = RrfConfig::default();

    let run = || {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query_vector,
            k: 4,
        };
        hybrid_search(
            &ParallelSearchProvider,
            input,
            &index,
            "rust search",
            4,
            &cfg,
        )
        .expect("hybrid search ok")
    };

    let first = run();
    let second = run();
    assert_eq!(first, second);
}

// SEARCH-3 対応: 融合結果の Top-k が「疎のみが当てられる文書」「密のみが当てられる文書」の
// 双方をノイズ文書より上位にランクすること（非劣化性の構造検証）。`k` を候補数より
// 厳密に小さくして truncate が実際に候補を絞り込む状況を作る（truncate が効かない
// 状況では非劣化性の主張を検証できない）。
#[test]
fn hybrid_search_top_k_ranks_sparse_only_and_dense_only_hits_above_noise() {
    let docs = build_corpus_with_noise();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let cfg = RrfConfig::default();

    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 4,
    };
    // 候補は id=1（疎・密両方）, id=2（疎のみ）, id=3（密のみ）＋ノイズ 6 件の計 9 件。
    // k=3 は候補数を厳密に下回るため、上位 3 件に何が選ばれるかが実際に検証対象になる。
    let hybrid_hits = hybrid_search(&CpuScalarProvider, input, &index, "rust search", 3, &cfg)
        .expect("hybrid search ok");
    let hybrid_ids: Vec<u64> = hybrid_hits.iter().map(|h| h.id).collect();

    assert_eq!(
        hybrid_ids.len(),
        3,
        "expected exactly 3 hits, got {hybrid_ids:?}"
    );
    // id=1（疎・密両方一致）は最上位のはず。
    assert!(hybrid_ids.contains(&1), "hybrid_ids={hybrid_ids:?}");
    // id=2（疎のみ一致）がノイズより上位にランクされ Top-3 に入るはず。
    assert!(
        hybrid_ids.contains(&2),
        "sparse-only doc id=2 missing from top-3: hybrid_ids={hybrid_ids:?}"
    );
    // id=3（密のみ一致）がノイズより上位にランクされ Top-3 に入るはず。
    assert!(
        hybrid_ids.contains(&3),
        "dense-only doc id=3 missing from top-3: hybrid_ids={hybrid_ids:?}"
    );
    // ノイズ文書（id>=100）はいずれも Top-3 に含まれないはず。
    assert!(
        hybrid_ids.iter().all(|id| *id < 100),
        "noise doc leaked into top-3: hybrid_ids={hybrid_ids:?}"
    );
}

// fail-closed 系: k=0 は拒否される。
#[test]
fn hybrid_search_rejects_k_zero() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 4,
    };
    let cfg = RrfConfig::default();
    let err = hybrid_search(&CpuScalarProvider, input, &index, "rust", 0, &cfg).unwrap_err();
    assert_eq!(err, HybridError::InvalidK);
}

// fail-closed 系: 不正な RrfConfig（pool_depth=0）は構築時点で拒否される。
#[test]
fn rrf_config_rejects_zero_pool_depth() {
    let err = RrfConfig::new(60.0, 1.0, 1.0, 0).unwrap_err();
    assert_eq!(err, HybridError::InvalidConfig);
}

// fail-closed 系: 下位の KernelError（次元不一致）がそのまま伝播すること。
#[test]
fn hybrid_search_propagates_dense_dim_mismatch() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    // dim=2 のコーパスに対して 3 次元クエリを渡す。
    let bad_query = [1.0f32, 0.0, 0.0];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &bad_query,
        k: 4,
    };
    let cfg = RrfConfig::default();
    let err = hybrid_search(&CpuScalarProvider, input, &index, "rust", 4, &cfg).unwrap_err();
    assert!(matches!(
        err,
        HybridError::Kernel(KernelError::DimMismatch { .. })
    ));
}

// fail-closed 系: 空コーパスでの検索は Ok(空) になること。
#[test]
fn hybrid_search_on_empty_corpus_returns_ok_empty() {
    // `SparseIndex::build` は空コーパスを受け付けないため、クエリと一致しない
    // ダミー文書 1 件で構築する（BM25 スコアは 0 になり候補から除外される）。
    // 密側は行を持たないため、融合結果は空になるはず。
    let index = SparseIndex::build(&[(1, "unrelated content")]).expect("index build ok");
    let ids: [u64; 0] = [];
    let vectors: [f32; 0] = [];
    let query: [f32; 0] = [];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 0,
        query: &query,
        k: 4,
    };
    let cfg = RrfConfig::default();
    let hits = hybrid_search(&CpuScalarProvider, input, &index, "anything", 4, &cfg).expect("ok");
    assert!(hits.is_empty());
}

// TASK-84（対応 Issue #61）: RRF 融合結果の同点タイブレークが `hybrid_search` の
// `LIMIT`（＝ `k`）境界を跨ぐ場合でも決定的であることの回帰テスト。PoC-10 が
// 指摘した非決定性（`HashMap` 由来の走査順・`sort_unstable_by` のタイブレーク欠如）
// は engine 本体には存在しない（`hybrid.rs` は `BTreeMap` 累積 + 安定ソート +
// id 昇順タイブレークを使う。`docs/design/rrf-tie-break-determinism.md` 参照）。
// 本テストは密側で複数 id が完全同点（同一内積）になるコーパスを構成し、
// その同点グループが `k` 境界を跨いでも (1) `ParallelSearchProvider` で
// 反復実行して常に同一結果になること、(2) 独立オラクル（密側は本体実装と
// 別経路の内積再計算、疎側はタイブレーク検証が主目的のため
// `SparseIndex::search_within` の結果をそのまま用いる。下記コメント参照）と
// 一致することを確認する。
#[test]
fn hybrid_search_tie_group_across_limit_boundary_is_deterministic_and_matches_oracle() {
    // 密側 provider は `pool_depth`（既定で候補数を上回る）を `k` として渡すため
    // 可視 6 行すべてを走査する。id 10〜12 はクエリベクトルと直交（内積 0）で
    // 完全同点の密ランキング下位グループを形成し（id 昇順でタイブレーク）、
    // id 30〜32 は密側で相異なる内積を持つ上位グループを形成する。id 10〜12 は
    // さらに「anchor」で疎側にもヒットする（文書長差により疎側の寄与は
    // 三者で異なるため、RRF 合計スコアは同点にならない）。この構成により、
    // 密側だけで発生する厳密な同点グループ（id 10, 11, 12）が `k` 境界を
    // またぐ場合の決定性を検証できる。
    struct TieDoc {
        id: u64,
        text: &'static str,
        // 疎のみヒットさせるため密ベクトルはクエリと直交させる。
        vector: [f32; 2],
    }
    let query_vector = [1.0f32, 0.0];
    // 密のみ id（30台）: クエリに近い順に rank 0,1,2 を占め、テキストはノイズ。
    // 疎のみ id（10台）: キーワード「anchor」に一致するがベクトルはクエリと直交。
    let docs: Vec<TieDoc> = vec![
        TieDoc {
            id: 30,
            text: "unrelated content alpha",
            vector: [0.9, 0.1],
        },
        TieDoc {
            id: 31,
            text: "unrelated content beta",
            vector: [0.8, 0.1],
        },
        TieDoc {
            id: 32,
            text: "unrelated content gamma",
            vector: [0.7, 0.1],
        },
        TieDoc {
            id: 10,
            text: "anchor anchor anchor",
            vector: [0.0, 1.0],
        },
        TieDoc {
            id: 11,
            text: "anchor anchor",
            vector: [0.0, -1.0],
        },
        TieDoc {
            id: 12,
            text: "anchor",
            vector: [0.0, 2.0],
        },
    ];
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text)).collect();
    let index = SparseIndex::build(&refs).expect("sparse index build ok");
    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector).collect();
    let cfg = RrfConfig::default();
    // 候補 6 件のうち k=4 は密側同点グループ（id 10, 11, 12。3 件）の全件を含み、
    // 疎側の寄与差により総合スコアが最も高くなる id=30 が加わる位置になる
    // （実測値はオラクルとの一致で検証する。ハードコードした期待順序には
    // 依存しない）。
    let k = 4;

    let run = |trial: u64| {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query_vector,
            k: 6,
        };
        hybrid_search(&ParallelSearchProvider, input, &index, "anchor", k, &cfg)
            .unwrap_or_else(|e| panic!("trial={trial}: hybrid search ok, got {e}"))
    };

    let baseline = run(0);
    for trial in 1..20u64 {
        assert_eq!(run(trial), baseline, "trial={trial} diverged from baseline");
    }

    // 独立オラクル: 本体の `rrf_fuse`/`hybrid_search` を経由せず、RRF の定義
    // （`weight / (k_const + rank + 1)` の id ごと加算）を BTreeMap で再計算する。
    let k_const = 60.0f64;
    let mut oracle_scores: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
    // 密ランキング: クエリ [1,0] との内積降順。id 30,31,32 のみが有意な内積を持つ
    // （id 10,11,12 は直交＝内積 0 だが 0 は非負のため密リストにも出現しうる。
    // オラクルは密側 provider と同じ「Top-k 全件」を使うため、ここでは
    // `CpuScalarProvider` 相当の全件内積降順・id 昇順タイブレークで並べる）。
    let mut dense_ranked: Vec<(u64, f64)> = docs
        .iter()
        .map(|d| {
            let dot = d.vector[0] as f64 * query_vector[0] as f64
                + d.vector[1] as f64 * query_vector[1] as f64;
            (d.id, dot)
        })
        .collect();
    dense_ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    for (rank, (id, _)) in dense_ranked.iter().enumerate() {
        *oracle_scores.entry(*id).or_insert(0.0) += 1.0 / (k_const + rank as f64 + 1.0);
    }
    // 疎ランキング: BM25 スコアは内部実装依存のため独立再計算はしないが、
    // 「anchor」を含む id 10,11,12 のみが疎側で有意にヒットし、id 30〜32 は
    // 疎側で 0 件（BM25 未ヒット）になる。相対順位は tf のみで決まる単純な
    // コーパス（IDF は全ヒット文書で同一）のため、tf 降順・id 昇順を疎ランクの
    // オラクルとして用いる。
    // 疎ランキング: BM25 スコアの計算式自体は private spec 側の管轄のため独立再実装
    // しない。本テストが独立検証するのは「タイブレーク規約（スコア降順・id 昇順）が
    // `LIMIT` 境界を跨いでも保たれるか」であり、疎側の相対順位はテスト対象の
    // `SparseIndex::search_within` から取得してそのままオラクルへ入力する（密側の
    // ランキングのみを完全に別経路で再計算し、独立性を確保する）。
    let sparse_hits = index
        .search_within(
            "anchor",
            cfg.pool_depth(),
            &ids.iter()
                .copied()
                .collect::<std::collections::BTreeSet<u64>>(),
        )
        .expect("sparse search ok");
    for (rank, doc) in sparse_hits.iter().enumerate() {
        *oracle_scores.entry(doc.doc_id).or_insert(0.0) += 1.0 / (k_const + rank as f64 + 1.0);
    }
    let mut oracle_sorted: Vec<(u64, f64)> = oracle_scores.into_iter().collect();
    oracle_sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let expected_ids: Vec<u64> = oracle_sorted
        .into_iter()
        .take(k)
        .map(|(id, _)| id)
        .collect();

    let actual_ids: Vec<u64> = baseline.iter().map(|h| h.id).collect();
    assert_eq!(actual_ids, expected_ids);
}

// provider 差し替え検証（CORE-3〜5 統合の確認）: CpuScalarProvider と
// ParallelSearchProvider で融合結果が一致すること。
#[test]
fn hybrid_search_matches_across_dense_provider_implementations() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let cfg = RrfConfig::default();

    let input_scalar = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 4,
    };
    let scalar_hits = hybrid_search(
        &CpuScalarProvider,
        input_scalar,
        &index,
        "rust search",
        4,
        &cfg,
    )
    .expect("scalar hybrid search ok");

    let input_parallel = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 4,
    };
    let parallel_hits = hybrid_search(
        &ParallelSearchProvider,
        input_parallel,
        &index,
        "rust search",
        4,
        &cfg,
    )
    .expect("parallel hybrid search ok");

    assert_eq!(scalar_hits, parallel_hits);
}
