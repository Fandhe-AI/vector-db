//! CORE-5（対照エンジン接続）の回帰ゲート入口（TASK-127。ポインタ:
//! `docs/spec/04-behavior/core-engine.md` CORE-5・`docs/spec/05-tasks.md` TASK-127・
//! Issue #176）。
//!
//! 被検（`ParallelSearchProvider`。TASK-126）と対照エンジン（usearch の総当たり
//! `exact_search`。`harness::contrast::ContrastIndex`）を同一データ・同一クエリで
//! `harness::ab::run_ab`（interleaved A/B）により比較し、p95 レイテンシの比率
//! （被検/対照）が上限以下であることを判定する（`harness::accept::p95_ratio` /
//! `check_contrast_ratio_within_limit`）。
//!
//! `parallel_bench.rs`（CORE-3/CORE-4）とは独立バイナリに分離してある
//! （`Cargo.toml` の `[[bench]] contrast_bench` コメント参照）。対照エンジン側は
//! C++ FFI（`usearch`）を経由するため、そちらの障害・ビルド失敗が CORE-3/CORE-4 の
//! ゲートへ波及しない failure domain 分離が目的。`required-features =
//! ["contrast-bench"]` により feature 無指定のビルド（`make check-cross` 等）では
//! 本ターゲット自体がスキップされる。
//!
//! 測定条件（`ROW_COUNT`・`DIM`・`TOP_K`・シード）は `parallel_bench.rs` と同一値を
//! 用いる（同一データでの比較が CORE-5 の前提のため）。
//!
//! 閾値は `BENCH_MAX_CONTRAST_RATIO` 環境変数（有限・正の浮動小数点）から注入する。
//! 未設定・不正値は fail-closed で非ゼロ終了する（`harness::accept::
//! parse_contrast_ratio_limit`。`parallel_bench.rs` の `max_p95_from_env` と同一方針）。
//! 閾値そのものは spec が SSOT のため標準出力へは出力しない
//! （`.claude/rules/spec-confidentiality.md`）。
//!
//! 健全性チェックとして同一クエリの Top-k 一致率（`recall_at_k`）を対照側と算出し
//! 標準出力へ併記する（配線ミス〔metric 取り違え・key 対応ずれ〕の検出用）。この値は
//! 合否判定には含めないが、`(0.0, 1.0]` の範囲外・空結果は fail-closed とする
//! （測定が成立していないことの検出。`harness::accept::recall_at_k`・`worst_recall` を
//! 再利用する）。

#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{check_contrast_ratio_within_limit, p95_ratio, parse_contrast_ratio_limit};
use harness::contrast::ContrastIndex;
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;

use engine::kernel::{SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;

/// 測定条件。`parallel_bench.rs` と同一値（CORE-5 は同一データでの比較が前提）。
const ROW_COUNT: usize = 100_000;
const DIM: usize = 768;
const TOP_K: usize = 20;

fn max_contrast_ratio_from_env() -> Result<f64, String> {
    let raw = std::env::var("BENCH_MAX_CONTRAST_RATIO").unwrap_or_default();
    parse_contrast_ratio_limit(&raw).map_err(|err| {
        format!("BENCH_MAX_CONTRAST_RATIO invalid (see .github/workflows/bench.yml vars): {err}")
    })
}

fn main() {
    let max_ratio = match max_contrast_ratio_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("contrast_bench: {msg}");
            std::process::exit(1);
        }
    };

    let mut rng = DeterministicRng::new(1);
    let ids: Vec<u64> = (0..ROW_COUNT as u64).collect();
    let mut vectors = Vec::with_capacity(ROW_COUNT * DIM);
    for _ in 0..ROW_COUNT {
        vectors.extend(rng.next_vector(DIM));
    }
    let query = rng.next_vector(DIM);

    let candidate = ParallelSearchProvider;
    let contrast = match ContrastIndex::build(&ids, &vectors, DIM) {
        Ok(index) => index,
        Err(err) => {
            eprintln!("contrast_bench: failed to build contrast index: {err}");
            std::process::exit(1);
        }
    };

    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    let measurement = match run_ab(
        &config,
        || {
            let input = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: DIM as u32,
                query: &query,
                k: TOP_K,
            };
            candidate
                .search(input)
                .expect("candidate search must succeed for well-formed synthetic input")
                .len()
        },
        || {
            contrast
                .search(&query, TOP_K)
                .expect("contrast search must succeed for well-formed synthetic input")
                .len()
        },
    ) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("contrast_bench: measurement failed: {err}");
            std::process::exit(1);
        }
    };

    let ratio = match p95_ratio(&measurement.a.samples, &measurement.b.samples) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("contrast_bench: p95_ratio failed: {err}");
            std::process::exit(1);
        }
    };
    let ratio_ok = match check_contrast_ratio_within_limit(ratio, max_ratio) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("contrast_bench: check_contrast_ratio_within_limit failed: {err}");
            std::process::exit(1);
        }
    };

    // 健全性チェック: 対照エンジンとの Top-k 一致率（配線ミス検出用。合否判定には含めない）。
    let candidate_topk: Vec<u64> = candidate
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &query,
            k: TOP_K,
        })
        .expect("candidate search must succeed for well-formed synthetic input")
        .into_iter()
        .map(|hit| hit.id)
        .collect();
    let contrast_topk = match contrast.search(&query, TOP_K) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("contrast_bench: sanity-check search failed: {err}");
            std::process::exit(1);
        }
    };
    let topk_overlap = match harness::accept::recall_at_k(&contrast_topk, &candidate_topk) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("contrast_bench: topk_overlap computation failed: {err}");
            std::process::exit(1);
        }
    };
    if !(topk_overlap > 0.0 && topk_overlap <= 1.0) {
        eprintln!(
            "contrast_bench: topk_overlap_min out of range ({topk_overlap}); wiring likely broken"
        );
        std::process::exit(1);
    }

    // `run_ab` が既に成功しているため `measurement.a/b.samples` は非空
    // （`stats::summarize` が空サンプルを `Err` で早期リターンする契約。
    // `parallel_bench.rs` の p95 算出と同一の `expect` 方針）。
    let candidate_p95 = harness::accept::p95_from_samples(&measurement.a.samples)
        .expect("non-empty samples must yield a p95");
    let contrast_p95 = harness::accept::p95_from_samples(&measurement.b.samples)
        .expect("non-empty samples must yield a p95");
    println!(
        "contrast_ratio(parallel_vs_usearch_exact): rows={ROW_COUNT} dim={DIM} k={TOP_K} candidate_p95={candidate_p95:?} contrast_p95={contrast_p95:?} p95_ratio={ratio:.6} median_ratio={:.6} topk_overlap_min={topk_overlap:.6} pass={ratio_ok}",
        measurement.median_ratio,
    );

    if !ratio_ok {
        eprintln!("contrast_bench: acceptance criteria not met (TASK-127 CORE-5)");
        std::process::exit(1);
    }
}
