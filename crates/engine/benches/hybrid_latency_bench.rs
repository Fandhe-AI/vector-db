//! 境界同点グループ再取得ループ（Issue #320・`hybrid.rs::hybrid_search_boosted`）の
//! 単発クエリレイテンシへの寄与を計測するベンチ（Issue #324。ポインタ:
//! `docs/spec/04-behavior/core-engine.md` CORE-7・`docs/spec/04-behavior/
//! query-planning.md` PLAN-4, PLAN-6, PLAN-7。判定内容・数値基準は spec 側が SSOT
//! であり本ファイルには転記しない。`.claude/rules/spec-confidentiality.md`）。
//!
//! # 背景
//!
//! PR #320 は `hybrid_search_boosted` に、`pool_depth` 境界の同点グループが未確定
//! （`TieBoundary::Undetermined`）の場合に `fetch_k` を倍増して密 provider を
//! 再呼び出しするループを導入した（上限 `MAX_FETCH_K`・可視集合サイズで有界化）。
//! 既存の受け入れ基準ベンチのうち、CORE-7 ゲート（`batch_bench.rs::run_core7_gate`）は
//! `BatchEngine::batch_search`（f16 常駐・CPU-SIMD）経由で測定しており
//! `hybrid_search` を一切通らない（`docs/design/core7-dynamic-window-gate.md`）ため、
//! PR #320 の変更が CORE-7 の実測値に影響することは構造的にありえない。ティア別
//! レイテンシベンチ（`tier_latency_bench.rs`。PLAN-4/6/7）は `USING PLAN(...)` 経由で
//! `hybrid_search` を通るが常駐 Ollama 前提のため、この寄与だけを Ollama なしに
//! 分離して計測する入口が存在しなかった。本ベンチはその隙間を埋める
//! （`docs/design/hybrid-refetch-latency.md` 参照）。
//!
//! # 計測方式（in-build 比較）
//!
//! 2 コミット間 worktree A/B ではなく、単一ビルド内で「再取得がほぼ発生しない
//! 通常コーパス」と「再取得が可視集合サイズまで到達する量子化コーパス」を比較する
//! （`harness::hybrid_latency` モジュールドキュメント参照）。再取得ループの有無が
//! 唯一の変数であり、2 段の差分がそのままループの寄与を表す。加えて小規模・
//! 大規模の 2 スケールで測る（`tests/hybrid_recall.rs` の段構成に合わせる）。
//!
//! 測定対象は `hybrid::hybrid_search`（`RrfConfig::default()`）の単発呼び出しのみで、
//! SQL パース・テーブル走査を含めない（`sql/exec.rs` の C4 経路から再取得ループの
//! 寄与だけを分離する）。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make bench-hybrid`
//! からの手動実行専用）。誤って CI 経由で実行された場合の defense-in-depth として
//! `GITHUB_ACTIONS` 環境変数が設定されていれば起動直後に fail-closed で拒否する
//! （`harness::hybrid_latency::refuse_under_github_actions`）。
//!
//! `make bench-hybrid`（Makefile）から実行する。判定ロジック自体（時間非依存）は
//! `harness::hybrid_latency` にあり `tests/hybrid_latency_accept.rs` で `make ci`
//! 側から回帰検証する。本ベンチ自体は spec 由来の pass/fail 閾値を持たない
//! 情報提供専用（計画「出力規約」節）で、実測値は常に出力する。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::hybrid_latency::{
    aggregate_refetch_stats, generate_corpus, generate_query, refuse_under_github_actions,
    render_stage_line, summarize_refetch_stats, Corpus, Query, RefetchStats,
    RefetchTrackingProvider,
};
use harness::protocol::{run, MeasurementConfig};

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;

/// 小規模段。`tests/hybrid_recall.rs` の小規模フィクスチャ（400 件）は
/// `RrfConfig::default().pool_depth() * 2`（初回 `fetch_k` = 400）と偶然一致し、
/// 通常コーパスでも初回呼び出しで可視集合全体を取り切ってしまい再取得ループの
/// 有無を比較できない。本ベンチは初回 `fetch_k` を上回る規模にして
/// 「再取得の余地がある」条件を保つ。
const SMALL_NUM_DOCS: usize = 1_000;
/// 大規模段（`tests/hybrid_recall.rs` の大規模フィクスチャと同一件数。可視集合到達の
/// 判定条件をそのまま流用できるようにする）。
const LARGE_NUM_DOCS: usize = 20_000;
const VOCAB_SIZE: usize = 256;
const DIM: usize = 32;
const TOP_K: usize = 20;
/// プロトタイプクラスタコーパスのクラスタ数（少ないほど 1 クラスタあたりの文書数が
/// 増え、同点グループが大きくなる。密チャネルの同点誘発の強度パラメータであり、
/// spec 由来の数値ではないためここに定数として持つ）。
const QUANTIZE_LEVELS: usize = 5;
const NUM_QUERIES: usize = 20;
const SEED: u64 = 0x4832_4832_4832_4832;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("hybrid_latency_bench: {msg}");
    std::process::exit(1);
}

/// 1 段（コーパス規模 × 量子化有無）の計測。`corpus` に対する `queries` 件の
/// クエリを round-robin し、`RefetchTrackingProvider` で再取得統計を集計しつつ
/// `harness::protocol::run` で p95/median を測る。
fn measure_stage(stage_name: &str, corpus: &Corpus, queries: &[Query], cfg: &RrfConfig) {
    let sparse_index = corpus
        .build_sparse_index()
        .unwrap_or_else(|e| fail_closed(format!("sparse index build failed: {e}")));
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let visible_set_size = corpus.ids.len();

    let config = MeasurementConfig::new(20, 30, SEED)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));

    // クエリ単位の再取得統計は計測フェーズの外側（`run` の外）で 1 回集計する
    // （`run` は同じ workload を warmup + 計測回数だけ繰り返すため、`run` の内側で
    // 統計をリセット・蓄積すると warmup 分と計測分が混在し集計が破綻する）。
    let mut refetch_stats: Vec<RefetchStats> = Vec::with_capacity(queries.len());
    let mut query_idx = 0usize;

    let measurement = run(&config, || {
        let query = &queries[query_idx % queries.len()];
        query_idx += 1;
        provider.reset();
        let input = SearchInput {
            ids: &corpus.ids,
            vectors: &corpus.vectors,
            dim: corpus.dim,
            query: &query.vector,
            k: TOP_K,
        };
        let hits = hybrid_search(&provider, input, &sparse_index, &query.text, TOP_K, cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search failed: {e}")));
        refetch_stats.push(aggregate_refetch_stats(
            provider.calls(),
            provider.max_k_seen(),
            visible_set_size,
        ));
        hits
    })
    .unwrap_or_else(|e| fail_closed(format!("measurement protocol violation: {e}")));

    let summary = summarize_refetch_stats(&refetch_stats);
    let p95 = harness::accept::p95_from_samples(&measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            stage_name,
            measurement.summary.median.as_micros(),
            p95.as_micros(),
            summary,
        )
    );
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "hybrid_latency_bench: measures hybrid_search_boosted's boundary tie-group refetch \
         loop (Issue #320) latency contribution via an in-build comparison (no-refetch vs \
         max-refetch corpora), not a pass/fail gate (Issue #324; see docs/design/\
         hybrid-refetch-latency.md)"
    );

    let cfg = RrfConfig::default();

    for (label, num_docs) in [("small", SMALL_NUM_DOCS), ("large", LARGE_NUM_DOCS)] {
        let no_refetch_corpus = generate_corpus(SEED, num_docs, VOCAB_SIZE, DIM, None)
            .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));
        let max_refetch_corpus =
            generate_corpus(SEED, num_docs, VOCAB_SIZE, DIM, Some(QUANTIZE_LEVELS))
                .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));

        // クエリ生成はプロトタイプクラスタモードに依存しない（`generate_query`
        // ドキュメント参照: 同点誘発はコーパス側のベクトル重複のみで成立する）ため
        // 2 段で共有する。
        let queries: Vec<Query> = (0..NUM_QUERIES)
            .map(|i| generate_query(SEED.wrapping_add(i as u64), DIM, VOCAB_SIZE))
            .collect();

        measure_stage(
            &format!("{label}_no_refetch"),
            &no_refetch_corpus,
            &queries,
            &cfg,
        );
        measure_stage(
            &format!("{label}_max_refetch"),
            &max_refetch_corpus,
            &queries,
            &cfg,
        );
    }
}
