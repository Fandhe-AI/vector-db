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
//! - CORE-7（動的窓の劣化上限・アクティブなゲート）: [`engine::batch_search::
//!   DynamicWindowAggregator`] の `push`/`drain` それ自体のオーバーヘッドのみを
//!   測定区間に含める（レビュー指摘対応: 以前は A/B どちらも同一の
//!   `BatchEngine::batch_search`〔10 万行 × dim 768 の全走査〕を測定区間に含めて
//!   いたため、全走査のミリ秒級コストの前で `push`/`drain` のナノ〜マイクロ秒級の
//!   差分が埋もれ、実質どんな劣化を注入しても pass してしまう判別力のないゲートに
//!   なっていた）。A（対照。集約器を経由しない）は `Vec<Vec<f32>>` へクエリを
//!   直接 `push`（`Vec::push`。検証なし）する経路、B（被検）は同じ本数のクエリを
//!   `DynamicWindowAggregator::push` に通してから `drain` する経路とし、両者とも
//!   1 反復あたり [`AGG_BATCH_SIZE`] 本のクエリを処理する（1 本ずつだとナノ秒級
//!   すぎて `Instant` の分解能・測定オーバーヘッドに埋もれるため、実運用の窓サイズを
//!   模した本数へ増幅してから測る）。差分は「集約器の検証・容量管理・所有権移動
//!   オーバーヘッド」のみになる。測定区間へ渡すクエリ本体（`Vec<f32>`）は
//!   `run_ab` 呼び出し前に反復回数分すべて事前生成し（[`build_query_pool`]）、
//!   各反復では事前生成済みの所有権を `Vec::pop` で取り出すだけにする（レビュー
//!   指摘対応: 以前は各反復の測定区間内で `query_base.clone()`〔dim 768 の
//!   確保・コピー、1 バッチあたり約 768 KiB〕を毎回行っており、この共通コストが
//!   両経路の測定時間を支配して `push`/`drain` の差分が p95 比率へ現れない
//!   ——あるいは無関係な確保コストの揺らぎで誤 fail しうる——状態になっていた）。
//!   B の p95 が A に対して劣化率上限（`BENCH_BATCH_MAX_DEGRADATION_PCT`）以内かを
//!   判定する。`BatchEngine::batch_search` 自体（f16 デコード・テナントマスク・
//!   visibility フィルタ・全走査）は本ゲートの測定対象外とする（CORE-3/CORE-5 側の
//!   ゲート〔`parallel_bench.rs`〕が別途担う関心事であり、本ゲートに混ぜると
//!   再び判別力を失う）。
//! - CORE-6（GPU 経路 vs CPU-SIMD の p95 短縮率）・CORE-16（f16 常駐 vs f32 常駐の
//!   p95 短縮率）: 実 GPU バックエンド未接続のため実測不能
//!   （`crates/engine/src/batch_search.rs` モジュール冒頭コメント参照。CPU 上の
//!   参照実装を GPU の代替として計測することはアサーション弱体化にあたるため行わない）。
//!   `BENCH_CORE6`/`BENCH_CORE16` フラグ（opt-in）が未設定（空文字含む）の既定では
//!   「対象外」を標準出力へ明示するのみで合否には数えない（`parallel_bench.rs` の
//!   CORE-5 opt-in の骨格を踏襲するが、opt-in 判定は非空値ならすべて要求とみなす
//!   よう本ファイル側で強化している。レビュー指摘対応: `"1"` 完全一致のみを
//!   有効とみなす方式だと `"true"`/`"yes"` 等の non-"1" な truthy 値がサイレントに
//!   「未設定」と同じ fail-open 側へ落ちるため）。フラグ指定時は「実 GPU 経路が
//!   未実装のため本フラグはまだ使用不可（GPU 実行への置き換え後に有効化される。
//!   `batch_search.rs` モジュール冒頭コメント参照）」ことを明示し、無条件で
//!   `pass=false` とする（レビュー指摘対応: 以前は「opt-in する」という運用上の
//!   誘導文言だけがあり、`check_improvement_at_least`〔判定ロジック本体〕を呼ぶ
//!   実測経路が存在しない不一致があった）。あわせて、判定ロジック自体の配線
//!   （env フラグ読み取り → 実測 → `check_improvement_at_least` 呼び出し →
//!   標準出力）が壊れていないかを CPU 経路同士の疎通測定で確認する
//!   （[`run_wiring_smoke`]）。これは GPU 実測の代替ではなく合否にも数えない
//!   ——同一 CPU 経路同士なので改善率は意味を持たない——が、配線そのものが
//!   `panic`/`Err` せず最後まで動くことだけを確認する。

// `harness` の取り込み方針は `parallel_bench.rs` と同一（本ファイルが実際に使う項目
// のみで、未到達の `pub` 項目は `dead_code` 警告になりうるためモジュール全体を許容する）。
#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{
    check_degradation_within_limit, check_improvement_at_least, p95_from_samples,
};
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;

use engine::batch_search::{BatchEngine, BatchQuery, DynamicWindowAggregator, ResidentMatrix};
use engine::policy::PolicyContext;
use engine::storage::Visibility;
use std::time::Instant;

/// CORE-7 ゲートで 1 反復あたり集約器へ通すクエリ本数。1 本単位では `push`/`drain`
/// が数百ナノ秒程度で終わり `Instant` の分解能・関数呼び出しオーバーヘッドへ
/// 埋もれるため、実運用の動的窓サイズ相当の本数へ増幅してから測る（値そのものは
/// spec の閾値ではなく本ベンチ固有の測定条件）。
const AGG_BATCH_SIZE: usize = 256;

/// CORE-7 ゲートで集約するクエリの次元数。
const AGG_QUERY_DIM: usize = 768;

/// CORE-6/CORE-16 の配線疎通測定（[`run_wiring_smoke`]）専用の合成データセット規模。
/// GPU 実測の代替ではなく「関数呼び出しが最後まで動くこと」の確認が目的のため、
/// `BatchEngine::batch_search` が現実的な時間で完走する程度の小規模で十分とし、
/// `AGG_BATCH_SIZE` 系のミリ秒級 CORE-7 計測と混同しないよう独立した定数にする。
const SMOKE_ROW_COUNT: usize = 2_000;
const SMOKE_DIM: usize = 64;
const SMOKE_TOP_K: usize = 5;

/// CORE-7 ゲートの計測に使うテナント ID（本ベンチ専用の合成データ。実データではない）。
const BENCH_TENANT: &str = "task-130-bench-tenant";

/// `BENCH_BATCH_MAX_DEGRADATION_PCT` 環境変数（パーセント・浮動小数点）を読み取り、
/// CORE-7 の劣化率上限として使う値を得る。未設定・非数値・負値は fail-closed で
/// 判定不能として扱う(数値そのものは spec が SSOT。本ファイルにはデフォルト値を
/// 持たない——`.claude/rules/spec-confidentiality.md`)。
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

/// CORE-6/CORE-16 opt-in フラグの「配線」だけを CPU 経路同士で疎通確認する
/// （[`check_improvement_at_least`] を実際に呼ぶ実測経路が存在しない、という
/// レビュー指摘への対応）。`baseline`/`candidate` はどちらも同一の
/// `BatchEngine::batch_search`（CPU 参照実装）を同一入力で 1 回ずつ呼んで得た
/// 実測時間であり、同一経路同士のため改善率に意味はない（GPU 実測の代替では
/// ない）。ここで確認したいのは「env 読み取り → 実測 → `check_improvement_at_least`
/// 呼び出し → 標準出力」という配線が `panic`/`Err` せず最後まで通ること自体であり、
/// 戻り値の pass/fail は疎通確認の対象外として全体の合否（`passed`）には反映しない。
/// 関数呼び出し自体が `Err` を返した場合（= 配線が壊れている）は疎通確認の失敗として
/// 呼び出し元へ伝える。
fn run_wiring_smoke(
    label: &str,
    batch_engine: &BatchEngine,
    ctx: &PolicyContext,
) -> Result<(), String> {
    let query = vec![0.1_f32; SMOKE_DIM];
    let batch_queries = [BatchQuery {
        vector: &query,
        k: SMOKE_TOP_K,
        ctx,
    }];

    let start_baseline = Instant::now();
    batch_engine
        .batch_search(&batch_queries)
        .map_err(|err| format!("{label} wiring smoke: baseline batch_search failed: {err}"))?;
    let baseline = start_baseline.elapsed();

    let start_candidate = Instant::now();
    batch_engine
        .batch_search(&batch_queries)
        .map_err(|err| format!("{label} wiring smoke: candidate batch_search failed: {err}"))?;
    let candidate = start_candidate.elapsed();

    // 疎通確認専用の暫定しきい値（spec の受け入れ基準値ではない。CPU 経路同士の
    // 比較で `check_improvement_at_least` の入力検証（有限・正値）を満たすためだけの
    // 値で、判定結果自体は合否に使わない）。
    const WIRING_SENTINEL_PCT: f64 = 0.001;
    match check_improvement_at_least(baseline, candidate, WIRING_SENTINEL_PCT) {
        Ok(result) => {
            println!(
                "{label}_wiring_smoke: baseline={baseline:?} candidate={candidate:?} \
                 result={result} (CPU-vs-CPU plumbing check only; not a GPU measurement; \
                 not counted toward pass/fail)"
            );
            Ok(())
        }
        Err(err) => Err(format!(
            "{label} wiring smoke: check_improvement_at_least failed: {err}"
        )),
    }
}

/// GPU 未接続の opt-in ゲート 1 本分の標準出力・合否寄与を処理する（CORE-6/CORE-16 共通）。
/// 既定（未設定）では「対象外」を明示するのみで合否に数えない（silent skip にしない）。
/// フラグ指定時は「実 GPU 経路が未実装のため本フラグはまだ使用不可」であることを明示し
/// `pass=false` とする（レビュー指摘対応: `check_improvement_at_least` を呼ぶ実測経路が
/// 存在しないまま「opt-in する」とだけ案内していた不一致を解消する）。あわせて配線疎通
/// （[`run_wiring_smoke`]）を実行し、疎通自体が壊れていれば `Err` として報告する。
fn report_gpu_unconnected_gate(
    label: &str,
    var: &str,
    batch_engine: &BatchEngine,
    ctx: &PolicyContext,
) -> Result<bool, String> {
    let requested = opt_in_requested_from_env(var);
    if requested {
        println!(
            "{label}: not usable yet (real GPU backend not implemented; this flag will be \
             enabled once the GPU execution path replaces the CPU reference implementation; \
             see crates/engine/src/batch_search.rs module doc, TASK-130 CORE-6/CORE-16 pointer) \
             requested=true pass=false"
        );
        run_wiring_smoke(label, batch_engine, ctx)?;
        Ok(false)
    } else {
        println!(
            "{label}: out of scope for this run (real GPU backend not implemented; not counted \
             toward pass/fail; set {var}=1 to see the not-usable-yet message once opted in; \
             see crates/engine/src/batch_search.rs module doc, TASK-130 CORE-6/CORE-16 pointer) \
             requested=false"
        );
        Ok(true)
    }
}

/// CORE-7 A/B 計測の測定区間からクエリ確保・コピーを追い出すための事前生成
/// プール。`total_iterations` バッチ分（各バッチ [`AGG_BATCH_SIZE`] 本）を
/// 呼び出し時（= `run_ab` 呼び出し前・測定区間外）にまとめて `clone` し、
/// 測定区間側は `Vec::pop` で取り出すだけにする（モジュール冒頭コメント参照。
/// レビュー指摘対応: 測定区間内の `clone` が両経路のコストを支配していた
/// 問題の是正）。
fn build_query_pool(rng: &mut DeterministicRng, total_iterations: usize) -> Vec<Vec<Vec<f32>>> {
    let base = rng.next_vector(AGG_QUERY_DIM);
    let mut pool: Vec<Vec<Vec<f32>>> = Vec::with_capacity(total_iterations);
    for _ in 0..total_iterations {
        let mut batch: Vec<Vec<f32>> = Vec::with_capacity(AGG_BATCH_SIZE);
        for _ in 0..AGG_BATCH_SIZE {
            batch.push(base.clone());
        }
        pool.push(batch);
    }
    pool
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
    let mut passed = true;

    // --- CORE-7: 動的窓集約それ自体のオーバーヘッドを、`BatchEngine::batch_search`
    // を測定区間から除外したうえで interleaved A/B で計測する（モジュール冒頭
    // コメント参照）。A（対照）は検証なしの `Vec::push`、B（被検）は
    // `DynamicWindowAggregator::push`/`drain` を通す点だけが差分になるよう揃える。
    // クエリ本体（`Vec<f32>`）は測定開始前にすべて事前生成し（[`build_query_pool`]）、
    // 各反復の測定区間内では `Vec::pop`（O(1)・確保もコピーもしない所有権移動）で
    // 取り出すだけにする（レビュー指摘対応: 以前は各反復の測定区間内で
    // `query_base.clone()` を `AGG_BATCH_SIZE` 回行っており、この確保・コピーの
    // 共通コストが両経路の測定時間を支配して `push`/`drain` の差分が p95 比率へ
    // 現れない状態になっていた）。---
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    // `run_ab` は warmup・計測の両フェーズで各経路をちょうど 1 回ずつ呼ぶ
    // （`harness::ab::run_ab` の契約）ため、必要な「バッチ」総数は
    // warmup_iterations + measured_iterations と一致する。
    let total_iterations = (config.warmup_iterations() as usize)
        .checked_add(config.measured_iterations() as usize)
        .expect("MeasurementConfig::new bounds iteration counts within usize");
    let mut pool_a = build_query_pool(&mut rng, total_iterations);
    let mut pool_b = build_query_pool(&mut rng, total_iterations);

    let workload_a = move || {
        let batch = pool_a
            .pop()
            .expect("query pool sized to warmup + measured iteration count");
        let mut queries: Vec<Vec<f32>> = Vec::with_capacity(AGG_BATCH_SIZE);
        for query in batch {
            queries.push(query);
        }
        queries
    };

    let workload_b = move || {
        let batch = pool_b
            .pop()
            .expect("query pool sized to warmup + measured iteration count");
        let mut window = DynamicWindowAggregator::new();
        for query in batch {
            window
                .push(query)
                .expect("well-formed synthetic query must satisfy window limits");
        }
        window.drain()
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
        "dynamic_window_degradation: batch_size={AGG_BATCH_SIZE} dim={AGG_QUERY_DIM} direct_median={:?} direct_p95={p95_a:?} windowed_median={:?} windowed_p95={p95_b:?} pass={degradation_ok}",
        ab.a.summary.median, ab.b.summary.median,
    );

    // --- CORE-6 / CORE-16: 実 GPU 未接続の opt-in fail-closed ゲート＋配線疎通確認 ---
    // 配線疎通用の合成データ（実データではない・GPU 実測の代替ではない）。
    let smoke_ctx = PolicyContext::new(BENCH_TENANT).expect("valid tenant");
    let smoke_ids: Vec<u64> = (0..SMOKE_ROW_COUNT as u64).collect();
    let smoke_tenant_ids: Vec<String> =
        std::iter::repeat_n(BENCH_TENANT.to_string(), SMOKE_ROW_COUNT).collect();
    let smoke_visibilities: Vec<Visibility> =
        std::iter::repeat_n(Visibility::Public, SMOKE_ROW_COUNT).collect();
    let mut smoke_vectors = Vec::with_capacity(SMOKE_ROW_COUNT * SMOKE_DIM);
    for _ in 0..SMOKE_ROW_COUNT {
        smoke_vectors.extend(rng.next_vector(SMOKE_DIM));
    }
    let smoke_matrix = ResidentMatrix::build(
        &smoke_ids,
        &smoke_tenant_ids,
        &smoke_visibilities,
        SMOKE_DIM,
        &smoke_vectors,
    )
    .expect("resident matrix must build for well-formed synthetic smoke input");
    let smoke_batch_engine = BatchEngine::new(smoke_matrix);

    let core6_ok = match report_gpu_unconnected_gate(
        "gpu_vs_cpu_simd_p95",
        "BENCH_CORE6",
        &smoke_batch_engine,
        &smoke_ctx,
    ) {
        Ok(ok) => ok,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };
    let core16_ok = match report_gpu_unconnected_gate(
        "f16_resident_vs_f32_resident_p95",
        "BENCH_CORE16",
        &smoke_batch_engine,
        &smoke_ctx,
    ) {
        Ok(ok) => ok,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };
    passed &= core6_ok;
    passed &= core16_ok;

    if !passed {
        eprintln!("batch_bench: acceptance criteria not met (TASK-130 CORE-6/CORE-7/CORE-16)");
        std::process::exit(1);
    }
}
