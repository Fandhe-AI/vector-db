//! CORE-5（対照エンジン接続）の回帰ゲート入口（TASK-127。ポインタ:
//! `docs/spec/04-behavior/core-engine.md` CORE-5・`docs/spec/05-tasks.md` TASK-127・
//! Issue #176）。
//!
//! 被検（`ParallelSearchProvider`。TASK-126）と対照エンジン（usearch の総当たり
//! `exact_search`。`harness::contrast::ContrastIndex`）を同一データ・同一クエリで
//! `harness::ab::run_ab`（interleaved A/B）により比較し、`harness::accept::p95_ratio` /
//! `check_contrast_ratio_within_limit` の結果が上限以下であることを判定する
//! （CORE-5 の判定統計量の詳細は `docs/spec/04-behavior/core-engine.md` CORE-5 が
//! SSOT のため本コメントでは転記しない）。
//!
//! `simd_bench.rs`（CORE-3/CORE-4）とは独立バイナリに分離してある
//! （`Cargo.toml` の `[[bench]] contrast_bench` コメント参照）。対照エンジン側は
//! C++ FFI（`usearch`）を経由するため、そちらの障害・ビルド失敗が CORE-3/CORE-4 の
//! ゲートへ波及しない failure domain 分離が目的。`required-features =
//! ["contrast-bench"]` により feature 無指定のビルド（`make check-cross` 等）では
//! 本ターゲット自体がスキップされる。
//!
//! 測定条件（`ROW_COUNT`・`DIM`・`TOP_K`・シード）は `simd_bench.rs` と同一値を
//! 用いる（同一データでの比較が CORE-5 の前提のため）。
//!
//! 閾値は `BENCH_MAX_CONTRAST_RATIO` 環境変数（有限・正の浮動小数点）から注入する。
//! 未設定・不正値は fail-closed で非ゼロ終了する（`harness::accept::
//! parse_contrast_ratio_limit`。`simd_bench.rs` の `max_p95_from_env` と同一方針）。
//! 閾値そのものは spec が SSOT のため標準出力へは出力しない
//! （`.claude/rules/spec-confidentiality.md`）。
//!
//! 健全性チェックとして同一クエリの Top-k 一致率（`recall_at_k`）を対照側と算出し
//! 標準出力へ併記し、[`MIN_TOPK_OVERLAP`] 未満なら CORE-5 の合否判定自体を fail-closed で
//! 拒否する（codex-review 指摘・PR #224 対応。被検・対照エンジンはいずれも同一データ・
//! 同一メトリック（内積）に対する厳密最近傍探索であり（`ContrastIndex::search`・
//! `ParallelSearchProvider` のドキュメンテーションコメント参照）、連続分布の合成ベクトル
//! では Top-k 境界での完全な同点はほぼ発生しないため、ほぼ全件一致するのが正しい配線の
//! 契約である。metric 取り違え・key 対応ずれのように一致率が 0 より大きい値のまま歪む
//! ケースも本チェックで検出できるようにし、単なる配線断線（一致率 0）検出に留めない。
//! 浮動小数点の加算順序差（被検側 `kernel.rs::dot` と対照側 usearch C++ 実装）による
//! 境界付近のごく少数の順序入れ替わりのみは許容するため、しきい値は 1.0 未満に設定する。

#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{check_contrast_ratio_within_limit, p95_ratio, parse_contrast_ratio_limit};
use harness::contrast::ContrastIndex;
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;

use engine::kernel::{SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;

/// 測定条件。`simd_bench.rs` と同一値（CORE-5 は同一データでの比較が前提）。
const ROW_COUNT: usize = 100_000;
const DIM: usize = 768;
const TOP_K: usize = 20;

/// Top-k 一致率（[`harness::accept::recall_at_k`]）の合否判定用下限。被検・対照とも
/// 同一データ・同一メトリックの厳密最近傍探索であるため、正しい配線では 1.0 に極めて
/// 近い値になる契約（モジュールドキュメント参照）。1.0 ちょうどを要求しないのは、被検・
/// 対照エンジン間の浮動小数点加算順序差による境界付近のごく少数の順序入れ替わりを
/// 誤検知しないため。
const MIN_TOPK_OVERLAP: f64 = 0.95;

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

    // 健全性チェック: 対照エンジンとの Top-k 一致率。[`MIN_TOPK_OVERLAP`] 未満は
    // 単なる配線断線（一致率 0）だけでなく metric 取り違え・key 対応ずれのような
    // 歪みも検出対象に含め、CORE-5 の最終合否（`overall_ok`）へ反映する
    // （codex-review 指摘・PR #224 対応）。
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
    // `(0.0, 1.0]` の範囲チェック（上）は測定不成立の検出、`MIN_TOPK_OVERLAP` との比較（下）は
    // 厳密最近傍同士の一致率そのものの合否判定であり、役割が異なるため両方を維持する。
    let topk_overlap_ok = topk_overlap >= MIN_TOPK_OVERLAP;

    // `run_ab` が既に成功しているため `measurement.a/b.samples` は非空
    // （`stats::summarize` が空サンプルを `Err` で早期リターンする契約。
    // `simd_bench.rs` の p95 算出と同一の `expect` 方針）。
    let candidate_p95 = harness::accept::p95_from_samples(&measurement.a.samples)
        .expect("non-empty samples must yield a p95");
    let contrast_p95 = harness::accept::p95_from_samples(&measurement.b.samples)
        .expect("non-empty samples must yield a p95");
    // CORE-5 の最終合否は p95 比率（`ratio_ok`）と Top-k 一致率（`topk_overlap_ok`）の
    // 両方を満たすことを要求する（codex-review 指摘・PR #224 対応。片方のみでは metric
    // 取り違え・key 対応ずれのように処理が速いだけの無効な計測を合格させてしまう）。
    let overall_ok = ratio_ok && topk_overlap_ok;
    println!(
        "contrast_ratio(parallel_vs_usearch_exact): rows={ROW_COUNT} dim={DIM} k={TOP_K} candidate_p95={candidate_p95:?} contrast_p95={contrast_p95:?} p95_ratio={ratio:.6} median_ratio={:.6} topk_overlap_min={topk_overlap:.6} ratio_ok={ratio_ok} topk_overlap_ok={topk_overlap_ok} pass={overall_ok}",
        measurement.median_ratio,
    );

    if !overall_ok {
        eprintln!("contrast_bench: acceptance criteria not met (TASK-127 CORE-5)");
        std::process::exit(1);
    }
}
