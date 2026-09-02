//! 転置索引化（Issue #386 Phase 1・#388〜#392）後の疎（BM25）側決定性契約の回帰
//! テスト（Issue #393）。`docs/design/rrf-tie-break-determinism.md` が固定した
//! 「スコア降順・同点は `doc_id` 昇順」の全順序契約が、`sparse.rs::Candidate`
//! に対する `select_nth_unstable_by`/`sort_unstable_by`（#391・#392。同ファイル
//! `// sort-determinism: allow` マーカー付き）へ置き換わった後も
//! `SparseIndex::search_within`/`score_within`/`SparseScored::top`・
//! `hybrid::hybrid_search`（疎側再取得ループ経由）で維持されることを、公開 API
//! だけを使って固定する。BM25 式そのものの独立再実装はしない
//! （`tests/hybrid.rs` の既存方針と同じく構造的性質のみ検証する）。
//!
//! 対応 spec ビヘイビア（ポインタのみ・本文非転記）: SEARCH-1, SEARCH-3。

use std::collections::BTreeSet;

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::{CpuScalarProvider, SearchInput};
use engine::parallel_search::ParallelSearchProvider;
use engine::sparse::SparseIndex;

/// 同一テキスト（同 tf・同文書長 → BM25 完全同点）の大きな同点グループを含む
/// 決定的コーパスを構築する。`tie_group_size` は 64（`TopKSelector`/バッファ境界の
/// 典型的な内部チャンクサイズの倍数）を跨ぐ大きさに設定し、同点グループが
/// 内部バッファ境界をまたいでも崩れないことを検証できるようにする。
///
/// 構成:
/// - id 1..=tie_group_size: 完全同点（同一本文 "anchor anchor anchor"、tf=3・len=3）。
///   BM25 の tf/文書長比が最大（1.0）になる構成のため、他群より必ず上位になる。
/// - id 1000..=1000+distinct_count: "anchor" の tf は 1 で固定しつつ文書長だけを
///   互いに異ならせた群（tf/文書長比が tie group（1.0）より必ず低くなるため、
///   スコアは tie group 未満で確定する。tf・文書長という異なる 2 軸で互いに
///   区別できる文書群を用意する狙い）
/// - id 2000: 一致語なし（不一致文書。ノイズ、スコア対象外）
fn build_corpus(tie_group_size: u64, distinct_count: u64) -> Vec<(u64, String)> {
    let mut docs = Vec::new();
    for id in 1..=tie_group_size {
        docs.push((id, "anchor anchor anchor".to_string()));
    }
    for i in 1..=distinct_count {
        let mut text = String::from("anchor");
        for _ in 0..i {
            text.push_str(" filler");
        }
        docs.push((1000 + i, text));
    }
    docs.push((2000, "unrelated noise text".to_string()));
    docs
}

fn build_index(docs: &[(u64, String)]) -> SparseIndex {
    let refs: Vec<(u64, &str)> = docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
    SparseIndex::build(&refs).expect("sparse index build ok")
}

fn all_visible(docs: &[(u64, String)]) -> BTreeSet<u64> {
    docs.iter().map(|(id, _)| *id).collect()
}

// --- 1: search_within と score_within().top(k) がスコアのビット列を含め
// 複数の k・複数回の反復にわたって完全一致すること。

#[test]
fn search_within_and_score_within_top_are_bit_identical_across_k_and_repetitions() {
    let tie_group_size = 300u64; // 64 の倍数境界（256）を跨ぐ大きさ。
    let distinct_count = 40u64;
    let docs = build_corpus(tie_group_size, distinct_count);
    let index = build_index(&docs);
    let visible = all_visible(&docs);

    let boundary_ks: [usize; 7] = [
        1,
        5,
        128,                                            // 同点グループの内側
        tie_group_size as usize,                        // 同点グループの末尾
        tie_group_size as usize + 1,                    // 同点グループ + 1
        (tie_group_size + distinct_count) as usize,     // M
        (tie_group_size + distinct_count) as usize + 1, // M + 1（可視ヒット総数を超える）
    ];

    for &k in &boundary_ks {
        let via_search_within = index
            .search_within("anchor", k, &visible)
            .expect("search_within ok");

        for rep in 0..20 {
            let mut scored = index
                .score_within("anchor", &visible)
                .expect("score_within ok");
            let via_top = scored.top(k);
            assert_eq!(
                via_search_within, via_top,
                "k={k} rep={rep}: search_within と score_within().top(k) が乖離した \
                 (search_within={via_search_within:?}, top={via_top:?})"
            );
        }
    }
}

// --- 2: 同点グループを k で切ると id 昇順の先頭 k 件のみが残り、
// top(k1) が top(k2)（k1 <= k2）の前方一致であること。

#[test]
fn tie_group_cut_by_k_yields_smallest_doc_ids_and_prefix_consistency() {
    let tie_group_size = 300u64;
    let distinct_count = 10u64;
    let docs = build_corpus(tie_group_size, distinct_count);
    let index = build_index(&docs);
    let visible = all_visible(&docs);
    // ノイズ文書（id=2000。スコア 0）を除いた、スコア > 0 のヒット総数（M）。
    let total_hits = docs.len() - 1;

    // 同点グループ内で k を切る（グループの id は 1..=tie_group_size で密に採番）。
    for k in [1usize, 2, 50, 150, tie_group_size as usize] {
        let hits = index
            .search_within("anchor", k, &visible)
            .expect("search_within ok");
        assert_eq!(hits.len(), k, "k={k}: 件数が一致しない");
        let ids: Vec<u64> = hits.iter().map(|h| h.doc_id).collect();
        let expected: Vec<u64> = (1..=k as u64).collect();
        assert_eq!(
            ids, expected,
            "k={k}: 同点グループ内の切り詰めは id 昇順の先頭 k 件のみを残すはず"
        );
    }

    // 前方一致契約: k1 <= k2 なら top(k1) は top(k2) の前方一致な部分列。
    let ks: [usize; 5] = [1, 10, 128, 256, 305];
    let mut scored = index
        .score_within("anchor", &visible)
        .expect("score_within ok");
    let mut prev: Vec<engine::sparse::ScoredDoc> = Vec::new();
    for &k in &ks {
        let cur = scored.top(k);
        assert_eq!(cur.len(), k.min(total_hits));
        assert!(
            cur.starts_with(&prev),
            "k={k}: 前方一致契約が崩れた (prev={prev:?}, cur={cur:?})"
        );
        prev = cur;
    }

    // 冪等性: 同じ k を複数回呼んでも同じ列を返す。
    let a = scored.top(128);
    let b = scored.top(128);
    assert_eq!(a, b, "top(k) の反復呼び出しは冪等であるはず");
}

// --- 3: 投入順（doc_idx/TermId 割り当て）に依存せず、search_within の
// (doc_id, score) 列がビット一致すること。可視集合を全体／部分で与えた場合も
// 同点内 id 昇順が保たれること。

#[test]
fn results_are_independent_of_build_order_and_visible_set_representation() {
    let tie_group_size = 300u64;
    let distinct_count = 20u64;
    let mut docs = build_corpus(tie_group_size, distinct_count);

    // 整列順（ベースライン）。
    let baseline_index = build_index(&docs);
    let visible = all_visible(&docs);
    let baseline = baseline_index
        .search_within("anchor", 200, &visible)
        .expect("search_within ok");

    // 決定的シャッフル（xorshift）で投入順を変える。
    let mut state = 0x1234_5678_9abc_def1u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let n = docs.len();
    for i in (1..n).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        docs.swap(i, j);
    }
    let shuffled_index = build_index(&docs);
    let shuffled = shuffled_index
        .search_within("anchor", 200, &visible)
        .expect("search_within ok");
    assert_eq!(
        baseline, shuffled,
        "投入順をシャッフルしても search_within の (doc_id, score) 列は不変であるはず"
    );

    // 逆順投入。
    docs.reverse();
    let reversed_index = build_index(&docs);
    let reversed = reversed_index
        .search_within("anchor", 200, &visible)
        .expect("search_within ok");
    assert_eq!(
        baseline, reversed,
        "投入順を逆順にしても search_within の (doc_id, score) 列は不変であるはず"
    );

    // 可視集合を部分（同点グループの中央域 50..100 を不可視化）にした場合も、
    // 残った id（1..=49・101..=tie_group_size）が id 昇順で返ること（不可視域を
    // 挟んで前後の連続域が「飛び越えて」つながる構成で、同点内 id 昇順が
    // ギャップの有無に関わらず保たれることを検証する）。
    let partial_visible: BTreeSet<u64> = all_visible(&docs)
        .into_iter()
        .filter(|id| !(*id >= 50 && *id < 100 && *id <= tie_group_size))
        .collect();
    let partial_hits = baseline_index
        .search_within("anchor", 100, &partial_visible)
        .expect("search_within ok");
    let partial_ids: Vec<u64> = partial_hits.iter().map(|h| h.doc_id).collect();
    // 期待値: 1..=49（49 件）に続けて 100..=150（51 件）で計 100 件
    // （不可視化した 50..=99 を飛び越えて id 昇順のまま連結される）。
    let expected: Vec<u64> = (1u64..=49).chain(100u64..=150).collect();
    assert_eq!(
        partial_ids, expected,
        "部分可視集合でも同点グループ内は不可視域を飛び越えて id 昇順であるはず"
    );
    assert!(
        !partial_ids.iter().any(|id| (50..100).contains(id)),
        "不可視化した id は結果に一切現れないはず"
    );
}

// --- 4: hybrid_search の疎側境界同点グループが、pool_depth を小さくして
// 再取得ループが複数ラウンド発火する構成でも決定的であること
// （TieRank::GroupEnd によりグループが分断されないこと）。

#[test]
fn hybrid_sparse_tie_group_across_pool_boundary_is_deterministic_over_multiple_refetch_rounds() {
    // 疎側のみが厚く同点になるコーパス（密ベクトルは全件同一で密側も安定させる）。
    let tie_group_size = 300u64; // pool_depth=8 から 16→32→64→128→256→512 と
                                 // 複数ラウンドの再取得が必要になる大きさ。
    let docs: Vec<(u64, String, [f32; 2])> = (1..=tie_group_size)
        .map(|id| (id, "anchor anchor anchor".to_string(), [1.0, 0.0]))
        .collect();

    let refs: Vec<(u64, &str)> = docs.iter().map(|(id, t, _)| (*id, t.as_str())).collect();
    let index = SparseIndex::build(&refs).expect("sparse index build ok");
    let ids: Vec<u64> = docs.iter().map(|(id, _, _)| *id).collect();
    let vectors: Vec<f32> = docs.iter().flat_map(|(_, _, v)| *v).collect();

    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid rrf config (small pool_depth)");

    for rep in 0..20 {
        let input_scalar = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 8,
        };
        let scalar_hits =
            hybrid_search(&CpuScalarProvider, input_scalar, &index, "anchor", 8, &cfg)
                .expect("hybrid_search (scalar) ok");

        let input_parallel = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 8,
        };
        let parallel_hits = hybrid_search(
            &ParallelSearchProvider,
            input_parallel,
            &index,
            "anchor",
            8,
            &cfg,
        )
        .expect("hybrid_search (parallel) ok");

        assert_eq!(
            scalar_hits, parallel_hits,
            "rep={rep}: CpuScalarProvider と ParallelSearchProvider で疎側同点境界の \
             結果が乖離した"
        );

        // TieRank::GroupEnd（既定）の下では、全件が同一融合スコアの同点グループに
        // 属するこの構成では、返る id 集合が id 昇順の先頭 k 件（1..=8）で
        // 安定するはず（境界同点グループが分断されない）。
        let result_ids: Vec<u64> = scalar_hits.iter().map(|h| h.id).collect();
        assert_eq!(
            result_ids,
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            "rep={rep}: 疎側全件同点構成での hybrid_search 結果が id 昇順の \
             先頭 k 件から外れた"
        );
    }
}
