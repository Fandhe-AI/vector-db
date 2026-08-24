//! 性能・Recall 受け入れ基準の回帰ベンチ（TASK-127。ポインタ: `docs/spec/05-tasks.md`
//! TASK-127・対象ビヘイビア CORE-3, CORE-4, CORE-5, SEARCH-4）。
//!
//! `parallel_smoke.rs`（TASK-126 の手動計測スモーク）と異なり、本ベンチは数値基準との
//! 突き合わせまで行い、基準未達なら非ゼロ終了する回帰ゲートとして機能する
//! （`harness/accept.rs` の判定ヘルパを利用）。`.github/workflows/bench.yml`
//! （週次 schedule + `workflow_dispatch`）から定期実行される想定で、`make ci` の対象には
//! しない（時間依存の測定値を CI アサーションへ混ぜない既存方針。`parallel_smoke.rs`
//! と同一）。
//!
//! - CORE-3・SEARCH-4: p95 レイテンシが上限以下であること（[`MAX_P95`]）
//! - CORE-4: `ParallelSearchProvider`（TASK-126・実測対象）と `CpuScalarProvider`
//!   （厳密最近傍の参照実装。`kernel.rs` 既存）の Top-k 一致率（Recall@k）が
//!   下限以上であること（[`MIN_RECALL`]）
//! - CORE-5: 対照エンジンとの中央値比較。対照エンジンクレートの導入がユーザー承認必須
//!   （`.claude/rules/dependency-policy.md`）のため本 PR では未接続（判定関数
//!   [`harness::accept::check_contrast_ratio_within_limit`] のみ用意。
//!   `Cargo.toml`・PR 本文の「対象外・承認事項」参照）
//!
//! 数値基準（[`MAX_P95`]・[`MIN_RECALL`]）・測定条件は spec（TASK-127）が SSOT。
//! 本ファイルには受け入れ判定に必要な定数のみを置き、根拠・議論は転記しない
//! （`.claude/rules/spec-confidentiality.md`）。

// `harness` は `benches/measurement.rs`・`benches/parallel_smoke.rs` と同様、独立した
// コンパイル単位（cargo bench バイナリ）から取り込まれる共有ソース。本ファイルが
// 実際に使う項目のみで、未到達の `pub` 項目は `dead_code` 警告になりうるため
// モジュール全体を対象に許容する（`parallel_smoke.rs` と同一方針。`harness/mod.rs`
// 自体は変更しない）。
#[allow(dead_code)]
mod harness;

use harness::accept::{
    check_p95_within_limit, check_recall_within_limit, p95_from_samples, recall_at_k,
};
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;

use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use std::time::Duration;

/// 測定条件（行数・次元・k）。TASK-127・`parallel_smoke.rs` と同一値を用いる
/// （測定条件そのものは spec の SSOT だが、既存ベンチ〔TASK-126〕がすでに同じ値を
/// 公開コードへ含んでいるため新規の漏えいではない）。
const ROW_COUNT: usize = 100_000;
const DIM: usize = 768;
const TOP_K: usize = 20;

/// Recall@k 判定に使うクエリ本数（複数クエリの平均で判定し、単一クエリの偶然による
/// ぶれを抑える。本数自体は本ベンチ独自の実装選択で spec 由来の値ではない）。
const RECALL_QUERY_COUNT: usize = 20;

/// CORE-3・SEARCH-4 の p95 上限（TASK-127 受け入れ基準）。
const MAX_P95: Duration = Duration::from_millis(100);

/// CORE-4 の Recall@20 下限（TASK-127 受け入れ基準）。
const MIN_RECALL: f64 = 0.99;

fn main() {
    let mut rng = DeterministicRng::new(1);
    let ids: Vec<u64> = (0..ROW_COUNT as u64).collect();
    let mut vectors = Vec::with_capacity(ROW_COUNT * DIM);
    for _ in 0..ROW_COUNT {
        vectors.extend(rng.next_vector(DIM));
    }

    let mut passed = true;

    // --- CORE-3 / SEARCH-4: p95 レイテンシ ---
    let provider = ParallelSearchProvider;
    let latency_query = rng.next_vector(DIM);
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    let measurement = run(&config, || {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &latency_query,
            k: TOP_K,
        };
        provider
            .search(input)
            .expect("search must succeed for well-formed synthetic input")
    })
    .expect("measurement must satisfy protocol minimums");

    let p95 = p95_from_samples(&measurement.samples).expect("non-empty samples must yield a p95");
    let p95_ok = check_p95_within_limit(p95, MAX_P95);
    passed &= p95_ok;
    println!(
        "p95_latency: rows={ROW_COUNT} dim={DIM} k={TOP_K} median={:?} p95={p95:?} limit={MAX_P95:?} pass={p95_ok}",
        measurement.summary.median,
    );

    // --- CORE-4: Recall@20（ParallelSearchProvider vs CpuScalarProvider 厳密最近傍） ---
    let reference = CpuScalarProvider;
    let mut recall_sum = 0.0f64;
    for _ in 0..RECALL_QUERY_COUNT {
        let query = rng.next_vector(DIM);

        let expected: Vec<u64> = reference
            .search(SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: DIM as u32,
                query: &query,
                k: TOP_K,
            })
            .expect("reference search must succeed for well-formed synthetic input")
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        let actual: Vec<u64> = provider
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

        recall_sum += recall_at_k(&expected, &actual).expect("non-empty reference top-k");
    }
    let recall = recall_sum / RECALL_QUERY_COUNT as f64;
    let recall_ok =
        check_recall_within_limit(recall, MIN_RECALL).expect("MIN_RECALL is within [0.0, 1.0]");
    passed &= recall_ok;
    println!(
        "recall_at_k: k={TOP_K} queries={RECALL_QUERY_COUNT} recall={recall:.6} limit={MIN_RECALL:.6} pass={recall_ok}"
    );

    // --- CORE-5: 対照エンジン比較（本 PR では未接続。判定関数のみ用意） ---
    println!(
        "contrast_ratio: not measured in this run (contrast engine dependency pending user approval; see harness::accept::check_contrast_ratio_within_limit)"
    );

    if !passed {
        eprintln!("simd_bench: acceptance criteria not met (TASK-127 CORE-3/CORE-4/SEARCH-4)");
        std::process::exit(1);
    }
}
