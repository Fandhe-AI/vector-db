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
//! - CORE-7（動的窓の劣化上限・アクティブなゲート）: 同一クエリを同一
//!   `BatchEngine::batch_search`（同一 `ResidentMatrix`・f16 デコード・
//!   テナントマスク・visibility フィルタを共有）へ渡す 2 経路を
//!   `harness::ab::run_ab` で interleaved 計測する。A（CORE-3 相当）は
//!   [`engine::batch_search::DynamicWindowAggregator`] を経由せず 1 件の
//!   `BatchQuery` を直接組み立てて渡し、B は同じクエリを
//!   `DynamicWindowAggregator::push`/`drain` に通してから渡す。両者の差分が
//!   「動的窓集約それ自体のオーバーヘッド」のみになるよう、エンジン・行列・
//!   マスク経路・k をすべて揃える（レビュー指摘対応: 以前は A 側が
//!   `ParallelSearchProvider` への直接呼び出しだったため、f16 デコード・
//!   テナントマスク・並列/非並列の違いまで丸ごと乗った差分になっており、
//!   窓集約の真の劣化を検出できないゲートになっていた）。B の p95 が A に
//!   対して劣化率上限（`BENCH_BATCH_MAX_DEGRADATION_PCT`）以内かを判定する。
//! - CORE-6（GPU 経路 vs CPU-SIMD の p95 短縮率）・CORE-16（f16 常駐 vs f32 常駐の
//!   p95 短縮率）: 実 GPU バックエンド未接続のため実測不能
//!   （`crates/engine/src/batch_search.rs` モジュール冒頭コメント参照。CPU 上の
//!   参照実装を GPU の代替として計測することはアサーション弱体化にあたるため行わない）。
//!   `BENCH_CORE6`/`BENCH_CORE16` フラグ（opt-in）が未設定（空文字含む）の既定では
//!   「対象外」を標準出力へ明示するのみで合否には数えない（`parallel_bench.rs` の
//!   CORE-5 opt-in の骨格を踏襲するが、opt-in 判定は非空値ならすべて要求とみなす
//!   よう本ファイル側で強化している。レビュー指摘対応: `"1"` 完全一致のみを
//!   有効とみなす方式だと `"true"`/`"yes"` 等の non-"1" な truthy 値がサイレントに
//!   「未設定」と同じ fail-open 側へ落ちるため）。フラグ指定時のみ
//!   「未測定＝判定不能」を fail-closed として扱う。

// `harness` の取り込み方針は `parallel_bench.rs` と同一（本ファイルが実際に使う項目
// のみで、未到達の `pub` 項目は `dead_code` 警告になりうるためモジュール全体を許容する）。
#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{check_degradation_within_limit, p95_from_samples};
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;

use engine::batch_search::{BatchEngine, BatchQuery, DynamicWindowAggregator, ResidentMatrix};
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
/// opt-in で有効化するかを返す。値が未設定・空文字のときのみ「対象外」（`false`）とし、
/// それ以外の非空値はすべて opt-in 要求とみなす（レビュー指摘対応: 以前は `"1"` 完全
/// 一致のみを有効とみなしており、`"true"`/`"yes"` 等の non-"1" な truthy 値を設定しても
/// サイレントに「未設定」と同じ fail-open 側の「対象外」経路に落ちていた。opt-in
/// ゲートの趣旨は「明示的な値が設定されていれば fail-closed 側で判定する」ことなので、
/// 値の有無で判定し内容の解釈は行わない）。
fn opt_in_requested_from_env(var: &str) -> bool {
    std::env::var(var)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
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

    // --- CORE-7: 動的窓集約それ自体のオーバーヘッドを、同一エンジン・同一
    // 常駐行列・同一マスク経路（`BatchEngine::batch_search`）上で
    // `DynamicWindowAggregator` を経由するか否かの差分だけに揃えて
    // interleaved A/B で計測する。両ワークロードとも `query_a`/`query_b`
    // （内容は同一）を毎回 `clone()` してから経路へ渡す点まで揃え、
    // 「アグリゲータ経由か否か」以外の非対称性（クローン有無など）を
    // 測定区間へ持ち込まない（レビュー指摘対応）。---
    let ctx = PolicyContext::new(BENCH_TENANT).expect("valid tenant");
    let matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, DIM, &vectors)
        .expect("resident matrix must build for well-formed synthetic input");
    let batch_engine = BatchEngine::new(matrix);

    let query_a = rng.next_vector(DIM);
    let query_b = query_a.clone();
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");

    // 両ワークロードは `run_ab::<T>` の単一型制約を満たすため、戻り値を
    // `Vec<u64>`（選出 id 列）へ揃える（`black_box` に渡す実体があれば十分で、
    // 経路間で結果の型を揃えること自体に測定上の意味はない）。
    let workload_a = || {
        let query = query_a.clone();
        let batch_queries = [BatchQuery {
            vector: &query,
            k: TOP_K,
            ctx: &ctx,
        }];
        batch_engine
            .batch_search(&batch_queries)
            .expect("batch search must succeed for well-formed synthetic input")
            .into_iter()
            .flat_map(|hit| hit.hits.into_iter().map(|h| h.id))
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
