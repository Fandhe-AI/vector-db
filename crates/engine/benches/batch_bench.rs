//! バッチ高速化の受け入れ基準検証ベンチ（TASK-130。ポインタ: `docs/spec/05-tasks.md`
//! TASK-130・対象ビヘイビア CORE-6, CORE-7〔動的窓〕, CORE-16〔f16 常駐〕。
//! `docs/spec/04-behavior/core-engine.md` ポインタ参照）。
//!
//! `parallel_bench.rs`（TASK-127）と同じ設計方針を踏襲する: `make ci` には含めず
//! `.github/workflows/bench.yml`（workflow_dispatch）から実行する時間依存ベンチであり、
//! 閾値は環境変数（Actions variables）から注入・未設定は fail-closed で非ゼロ終了する。
//! 標準出力には実測値と pass/fail のみを書き、注入された閾値そのものは出力しない
//! （`.claude/rules/spec-confidentiality.md`）。
//!
//! - CORE-7（動的窓の劣化上限・アクティブなゲート）: 単発クエリを直接
//!   `ParallelSearchProvider` へ渡す経路（A・CORE-3 相当）と、同一クエリを
//!   [`engine::batch_search::DynamicWindowAggregator`] 経由でバッチ化して
//!   `BatchEngine::batch_search` へ渡す経路（B）を `harness::ab::run_ab` で
//!   interleaved 計測し、B の p95 が A に対して劣化率上限
//!   （`BENCH_BATCH_MAX_DEGRADATION_PCT`）以内かを判定する。
//! - CORE-6（GPU 経路 vs CPU-SIMD の p95 短縮率）・CORE-16（f16 常駐 vs f32 常駐の
//!   p95 短縮率）: 実 GPU バックエンド未接続のため実測不能
//!   （`crates/engine/src/batch_search.rs` モジュール冒頭コメント参照。CPU 上の
//!   参照実装を GPU の代替として計測することはアサーション弱体化にあたるため行わない）。
//!   `BENCH_CORE6`/`BENCH_CORE16` フラグ（opt-in）が未設定の既定では「対象外」を
//!   標準出力へ明示するのみで合否には数えない（`parallel_bench.rs` の CORE-5 opt-in
//!   方式と同型）。フラグ指定時のみ「未測定＝判定不能」を fail-closed として扱う。

// `harness` の取り込み方針は `parallel_bench.rs` と同一（本ファイルが実際に使う項目
// のみで、未到達の `pub` 項目は `dead_code` 警告になりうるためモジュール全体を許容する）。
#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{check_degradation_within_limit, p95_from_samples};
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;

use engine::batch_search::{BatchEngine, BatchQuery, DynamicWindowAggregator, ResidentMatrix};
use engine::kernel::{SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::storage::Visibility;

/// 測定条件（行数・次元・k）。`parallel_bench.rs` と同一値を用いる（測定条件そのものは
/// spec の SSOT だが、既存ベンチ（TASK-127）がすでに同じ値を公開コードへ含んでいる
/// ため新規の漏えいではない）。
const ROW_COUNT: usize = 100_000;
const DIM: usize = 768;
const TOP_K: usize = 20;

/// CORE-7 ゲートの計測に使うテナント ID（本ベンチ専用の合成データ。実データではない）。
const BENCH_TENANT: &str = "task-130-bench-tenant";

/// `BENCH_BATCH_MAX_DEGRADATION_PCT` 環境変数（パーセント・浮動小数点）を読み取り、
/// CORE-7 の劣化率上限として使う値を得る。未設定・非数値・負値は fail-closed で
/// 判定不能として扱う（数値そのものは spec が SSOT。本ファイルにはデフォルト値を
/// 持たない——`.claude/rules/spec-confidentiality.md`）。
fn max_degradation_pct_from_env() -> Result<f64, String> {
    let raw = std::env::var("BENCH_BATCH_MAX_DEGRADATION_PCT").map_err(|_| {
        "BENCH_BATCH_MAX_DEGRADATION_PCT is not set (see .github/workflows/bench.yml vars)"
            .to_string()
    })?;
    let value: f64 = raw.trim().parse().map_err(|_| {
        "BENCH_BATCH_MAX_DEGRADATION_PCT must be a floating-point number".to_string()
    })?;
    if !value.is_finite() || value < 0.0 {
        return Err(
            "BENCH_BATCH_MAX_DEGRADATION_PCT must be a finite, non-negative value".to_string(),
        );
    }
    Ok(value)
}

/// `BENCH_CORE6`/`BENCH_CORE16` 環境変数を読み取り、実 GPU 未接続の判定不能ゲートを
/// opt-in で有効化するかを返す（`"1"` のときのみ有効）。`parallel_bench.rs` の
/// `core5_requested_from_env` と同一方針。
fn opt_in_requested_from_env(var: &str) -> bool {
    std::env::var(var).map(|v| v.trim() == "1").unwrap_or(false)
}

/// GPU 未接続の opt-in ゲート 1 本分の標準出力・合否寄与を処理する（CORE-6/CORE-16 共通）。
/// 既定（未設定）では「対象外」を明示するのみで合否に数えない（silent skip にしない）。
/// フラグ指定時は「未測定＝判定不能」を fail-closed として合否へ反映する。
fn report_gpu_unconnected_gate(label: &str, var: &str) -> bool {
    let requested = opt_in_requested_from_env(var);
    if requested {
        println!(
            "{label}: not measured in this run (real GPU backend not connected; see crates/engine/src/batch_search.rs module doc, TASK-130 CORE-6/CORE-16 pointer) requested=true pass=false"
        );
        false
    } else {
        println!(
            "{label}: out of scope for this run (real GPU backend not connected; not counted toward pass/fail; set {var}=1 to opt in once connected; see crates/engine/src/batch_search.rs module doc, TASK-130 CORE-6/CORE-16 pointer) requested=false"
        );
        true
    }
}

fn main() {
    let max_degradation_pct = match max_degradation_pct_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };

    let mut rng = DeterministicRng::new(1);
    let ids: Vec<u64> = (0..ROW_COUNT as u64).collect();
    let tenant_ids: Vec<String> =
        std::iter::repeat_n(BENCH_TENANT.to_string(), ROW_COUNT).collect();
    let visibilities: Vec<Visibility> =
        std::iter::repeat_n(Visibility::Public, ROW_COUNT).collect();
    let mut vectors = Vec::with_capacity(ROW_COUNT * DIM);
    for _ in 0..ROW_COUNT {
        vectors.extend(rng.next_vector(DIM));
    }

    let mut passed = true;

    // --- CORE-7: 動的窓経由のバッチ 1 件処理が、単発クエリの直接経路（CORE-3
    // 相当・ParallelSearchProvider）に対してどれだけ劣化するかを interleaved
    // A/B で計測する。---
    let ctx = PolicyContext::new(BENCH_TENANT).expect("valid tenant");
    let matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, DIM, &vectors)
        .expect("resident matrix must build for well-formed synthetic input");
    let batch_engine = BatchEngine::new(matrix);
    let direct_provider = ParallelSearchProvider;

    let query_a = rng.next_vector(DIM);
    let query_b = query_a.clone();
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");

    // 両ワークロードは `run_ab::<T>` の単一型制約を満たすため、戻り値を
    // `Vec<u64>`（選出 id 列）へ揃える（`black_box` に渡す実体があれば十分で、
    // 経路間で結果の型を揃えること自体に測定上の意味はない）。
    let workload_a = || {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: DIM as u32,
            query: &query_a,
            k: TOP_K,
        };
        direct_provider
            .search(input)
            .expect("direct single-query search must succeed for well-formed synthetic input")
            .into_iter()
            .map(|hit| hit.id)
            .collect::<Vec<u64>>()
    };

    let mut window = DynamicWindowAggregator::new();
    let workload_b = || {
        window
            .push(query_b.clone())
            .expect("single query must satisfy dynamic window limits");
        let drained = window.drain();
        let batch_queries: Vec<BatchQuery<'_>> = drained
            .iter()
            .map(|q| BatchQuery {
                vector: q,
                k: TOP_K,
                ctx: &ctx,
            })
            .collect();
        batch_engine
            .batch_search(&batch_queries)
            .expect("batch search must succeed for well-formed synthetic input")
            .into_iter()
            .flat_map(|hit| hit.hits.into_iter().map(|h| h.id))
            .collect::<Vec<u64>>()
    };

    let ab = run_ab(&config, workload_a, workload_b)
        .expect("A/B measurement must satisfy protocol minimums");
    let p95_a = p95_from_samples(&ab.a.samples).expect("non-empty A samples must yield a p95");
    let p95_b = p95_from_samples(&ab.b.samples).expect("non-empty B samples must yield a p95");
    let degradation_ok = check_degradation_within_limit(p95_a, p95_b, max_degradation_pct)
        .expect("max_degradation_pct validated by max_degradation_pct_from_env");
    passed &= degradation_ok;
    // limit（BENCH_BATCH_MAX_DEGRADATION_PCT の実測値）は意図的にログへ出力しない
    // （閾値は spec が SSOT であり public リポの Actions ログへ能動的に書き出さない。
    // モジュール冒頭コメント参照）。
    println!(
        "dynamic_window_degradation: rows={ROW_COUNT} dim={DIM} k={TOP_K} direct_median={:?} direct_p95={p95_a:?} windowed_median={:?} windowed_p95={p95_b:?} pass={degradation_ok}",
        ab.a.summary.median, ab.b.summary.median,
    );

    // --- CORE-6 / CORE-16: 実 GPU 未接続のため opt-in fail-closed ゲート ---
    let core6_ok = report_gpu_unconnected_gate("gpu_vs_cpu_simd_p95", "BENCH_CORE6");
    let core16_ok = report_gpu_unconnected_gate("f16_resident_vs_f32_resident_p95", "BENCH_CORE16");
    passed &= core6_ok;
    passed &= core16_ok;

    if !passed {
        eprintln!("batch_bench: acceptance criteria not met (TASK-130 CORE-6/CORE-7/CORE-16)");
        std::process::exit(1);
    }
}
