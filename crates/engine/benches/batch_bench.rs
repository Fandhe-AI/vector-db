//! バッチ高速化の受け入れ基準検証ベンチ（TASK-130。ポインタ: `docs/spec/05-tasks.md`
//! TASK-130・対象ビヘイビア CORE-6, CORE-7〔動的窓〕, CORE-16〔f16 常駐〕。
//! `docs/spec/04-behavior/core-engine.md` ポインタ参照）。
//!
//! `simd_bench.rs`（TASK-127）と同じ設計方針を踏襲する: `make ci` には含めず
//! `.github/workflows/bench.yml`（workflow_dispatch）から実行する時間依存ベンチであり、
//! 閾値は環境変数（Actions variables）から注入・未設定は fail-closed で非ゼロ終了する。
//! 標準出力には pass/fail と非数値状態のみを書き、実測値・注入された閾値は出力しない
//! （`.claude/rules/spec-confidentiality.md`・Issue #279）。実測値が必要な場合のみ
//! `BENCH_VERBOSE`（非空で有効）の opt-in で追加出力するが、`GITHUB_ACTIONS` 下では
//! public ログへの漏えいを防ぐため fail-closed で拒否する（PR #224 で CORE-5 側
//! 〔`contrast_bench.rs`〕に適用済みの「真偽値のみを既定出力にする」方針を本ファイルへ
//! 横展開したもの）。
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
//!   ゲート〔`simd_bench.rs`〕が別途担う関心事であり、本ゲートに混ぜると
//!   再び判別力を失う）。
//! - CORE-6（GPU 経路 vs CPU-SIMD の p95 短縮率）・CORE-16（f16 常駐 vs f32 常駐の
//!   p95 短縮率）: `BENCH_CORE6`/`BENCH_CORE16` フラグ（opt-in。未設定・空文字のみ
//!   「対象外」とし、非空値はすべて opt-in 要求とみなす）で有効化する実測ゲート
//!   （Issue #178 で実 GPU バックエンド〔`engine::gpu_batch`〕が接続されたため、
//!   従来の「未実装のため常に pass=false」を実測経路へ置き換えた）。
//!   - CORE-6: 対照 A = CPU-SIMD バッチ経路（`BatchEngine::batch_search`。f16 常駐
//!     行列を CPU の実行時 SIMD 検出カーネルで全走査）、被検 B = GPU バックエンド
//!     （`FallbackBatchEngine::build_with_gpu`）。閾値は
//!     `BENCH_CORE6_MIN_IMPROVEMENT_PCT` から注入し未設定は fail-closed。
//!     GPU が初期化できない環境（CI の GitHub ホステッド runner 等）で opt-in された
//!     場合は「判定不能」を `pass=false` として報告する（CPU 経路同士の比較値を
//!     GPU 実測の代替として計上しない＝アサーション弱体化を避ける）。計測中に
//!     CPU 縮退（CORE-8）が起きた場合も同様に `pass=false`（縮退後の値は GPU 経路の
//!     実測ではないため）。
//!   - CORE-16（Issue #234）: **GPU 常駐コピーの f16 パック vs f32 常駐**の p95
//!     短縮率。対照 A = f32 常駐対照経路（`engine::gpu_batch::
//!     GpuF32ContrastBackend`。GPU 側で元の f32 ベクトル列をそのまま常駐させ
//!     `unpack2x16float` を経由しない内積を計算する bench/テスト専用経路）、
//!     被検 B = 本番の f16 パック常駐 GPU 経路（`engine::gpu_batch::
//!     GpuBatchBackend`）。閾値は `BENCH_CORE16_MIN_IMPROVEMENT_PCT` から注入し
//!     未設定は fail-closed。どちらかの GPU 初期化が失敗した環境（CI の
//!     GitHub ホステッド runner 等）で opt-in された場合は「判定不能」を
//!     `pass=false` として報告する（CPU 経路同士の f16/f32 比較を代替として
//!     計上しない＝アサーション弱体化を避ける。CORE-6 と同方針）。
//!
//!
//!   いずれも既定では pass/fail・非数値状態のみを標準出力へ書き、実測値・注入した
//!   閾値は出力しない（`BENCH_VERBOSE` opt-in 時のみ実測値を追加出力する）。

// `harness` の取り込み方針は `simd_bench.rs` と同一（本ファイルが実際に使う項目
// のみで、未到達の `pub` 項目は `dead_code` 警告になりうるためモジュール全体を許容する）。
#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{
    check_degradation_within_limit, check_improvement_at_least, p95_from_samples,
};
use harness::protocol::MeasurementConfig;
use harness::rng::DeterministicRng;

use engine::batch_fallback::BatchBackend;
use engine::batch_search::{BatchEngine, BatchQuery, DynamicWindowAggregator, ResidentMatrix};
use engine::policy::PolicyContext;
use engine::storage::Visibility;

/// CORE-7 ゲートで 1 反復あたり集約器へ通すクエリ本数。1 本単位では `push`/`drain`
/// が数百ナノ秒程度で終わり `Instant` の分解能・関数呼び出しオーバーヘッドへ
/// 埋もれるため、実運用の動的窓サイズ相当の本数へ増幅してから測る（値そのものは
/// spec の閾値ではなく本ベンチ固有の測定条件）。
const AGG_BATCH_SIZE: usize = 256;

/// CORE-7 ゲートで集約するクエリの次元数。
const AGG_QUERY_DIM: usize = 768;

/// CORE-6/CORE-16 ゲートの実測に使う合成データセット規模（本ベンチ固有の測定条件で
/// あり spec の閾値ではない）。GPU 転送・dispatch の固定コストを償却できる程度の
/// 行数・次元にしつつ、GPU 非搭載環境でも CORE-16 側（CPU 上の f16/f32 比較）が
/// 現実的な時間で完走する規模に留める。
const GPU_GATE_ROW_COUNT: usize = 20_000;
const GPU_GATE_DIM: usize = 256;
const GPU_GATE_TOP_K: usize = 10;
/// 1 反復あたりのバッチ本数（GPU 経路はクエリ単位に dispatch するため、1 本だと
/// 転送・同期の固定コストが支配的になり経路差が現れない）。
const GPU_GATE_BATCH_SIZE: usize = 8;

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

/// `BENCH_VERBOSE` 環境変数（非空で有効。[`opt_in_requested_from_env`] と同じ
/// 「値の有無のみで判定し内容は解釈しない」規約）を読み取り、実測値（p95・median）を
/// 標準出力へ追加するかを返す。実測値は public リポの Actions ログへ出すと注入済みの
/// 非公開閾値を逆算されうるため（Issue #279）、`GITHUB_ACTIONS` が設定された実行環境
/// （GitHub Actions ランナーは常にこの変数を設定する）では opt-in 自体を拒否する
/// （fail-closed。`.github/workflows/bench.yml` は `BENCH_VERBOSE` を注入しない運用と
/// 二重化することで、誤注入時にも public ログへ実測値が漏れないようにする）。
fn verbose_requested_from_env() -> Result<bool, String> {
    let requested = opt_in_requested_from_env("BENCH_VERBOSE");
    if requested && std::env::var("GITHUB_ACTIONS").is_ok() {
        return Err(
            "BENCH_VERBOSE is refused under GitHub Actions (public log; Issue #279)".to_string(),
        );
    }
    Ok(requested)
}

/// `BENCH_CORE6_MIN_IMPROVEMENT_PCT` を読み取り、
/// p95 短縮率の下限として使う値を得る。opt-in されているのに閾値が未設定・非数値・
/// 非正値の場合は「判定不能」として fail-closed に `Err` を返す
/// （[`max_degradation_pct_from_env`] と同一方針。数値そのものは spec が SSOT であり
/// 本ファイルにデフォルト値を持たない——`.claude/rules/spec-confidentiality.md`）。
fn min_improvement_pct_from_env(var: &str) -> Result<f64, String> {
    let raw = std::env::var(var)
        .map_err(|_| format!("{var} is not set (see .github/workflows/bench.yml vars)"))?;
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{var} must be a floating-point number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("{var} must be a finite, positive value"));
    }
    Ok(value)
}

/// CORE-6/CORE-16 ゲート用の合成データセット（本ベンチ専用。実データではない）。
/// 単一テナント・全 `Public` の素直な配置にし、テナントマスクの分岐差が経路間の
/// 比較へ混ざらないようにする。
struct GateDataset {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    vectors: Vec<f32>,
    queries: Vec<Vec<f32>>,
}

fn build_gate_dataset(rng: &mut DeterministicRng) -> GateDataset {
    let ids: Vec<u64> = (0..GPU_GATE_ROW_COUNT as u64).collect();
    let tenant_ids: Vec<String> =
        std::iter::repeat_n(BENCH_TENANT.to_string(), GPU_GATE_ROW_COUNT).collect();
    let visibilities: Vec<Visibility> =
        std::iter::repeat_n(Visibility::Public, GPU_GATE_ROW_COUNT).collect();
    let mut vectors = Vec::with_capacity(GPU_GATE_ROW_COUNT * GPU_GATE_DIM);
    for _ in 0..GPU_GATE_ROW_COUNT {
        vectors.extend(rng.next_vector(GPU_GATE_DIM));
    }
    let queries: Vec<Vec<f32>> = (0..GPU_GATE_BATCH_SIZE)
        .map(|_| rng.next_vector(GPU_GATE_DIM))
        .collect();
    GateDataset {
        ids,
        tenant_ids,
        visibilities,
        vectors,
        queries,
    }
}

/// CORE-8 の縮退イベント件数だけを数える observer（GPU ゲートの測定妥当性判定用）。
/// 構築時・計測中に 1 件でも縮退が起きていれば、その測定値は GPU 経路のものでは
/// ないため「判定不能（`pass=false`）」に倒す。
struct CountingObserver(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl engine::batch_fallback::FallbackObserver for CountingObserver {
    fn on_fallback(&self, event: engine::batch_fallback::FallbackEvent) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        eprintln!("batch_bench: fallback observed during gpu gate: {event}");
    }
}

/// `run_ab` の 1 反復分として与えるバッチクエリ列を組み立てる（測定区間外での
/// 参照の組み立てはできないため、各ワークロード内で `BatchQuery` を作る。
/// クエリベクトル本体は事前生成済みで確保・コピーは発生しない）。
fn gate_batch_queries<'a>(queries: &'a [Vec<f32>], ctx: &'a PolicyContext) -> Vec<BatchQuery<'a>> {
    queries
        .iter()
        .map(|q| BatchQuery {
            vector: q.as_slice(),
            k: GPU_GATE_TOP_K,
            ctx,
        })
        .collect()
}

/// CORE-6 ゲート（GPU 経路 vs CPU-SIMD 経路の p95 短縮率）。
/// opt-in されていなければ「対象外」を出力して合否に数えない（silent skip にしない）。
/// opt-in 時に GPU が初期化できない・計測中に CPU 縮退した場合は `pass=false`
/// （fail-closed。CPU 同士の比較値を GPU 実測の代替として計上しない）。
fn run_core6_gate(
    dataset: &GateDataset,
    ctx: &PolicyContext,
    verbose: bool,
) -> Result<bool, String> {
    const LABEL: &str = "gpu_vs_cpu_simd_p95";
    if !opt_in_requested_from_env("BENCH_CORE6") {
        println!(
            "{LABEL}: out of scope for this run (not counted toward pass/fail; \
             set BENCH_CORE6=1 with BENCH_CORE6_MIN_IMPROVEMENT_PCT to enable) requested=false"
        );
        return Ok(true);
    }
    let min_improvement_pct = min_improvement_pct_from_env("BENCH_CORE6_MIN_IMPROVEMENT_PCT")?;

    let fallback_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gpu_engine = engine::batch_fallback::FallbackBatchEngine::build_with_gpu(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
        Box::new(CountingObserver(std::sync::Arc::clone(&fallback_count))),
    )
    .map_err(|err| format!("{LABEL}: gpu-backed engine build failed: {err}"))?;
    if fallback_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        println!(
            "{LABEL}: not measurable in this environment (gpu backend unavailable; \
             cpu fallback engaged) requested=true pass=false"
        );
        return Ok(false);
    }

    let cpu_matrix = ResidentMatrix::build(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
    )
    .map_err(|err| format!("{LABEL}: resident matrix build failed: {err}"))?;
    let cpu_engine = BatchEngine::new(cpu_matrix);

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let workload_a = || {
        let queries = gate_batch_queries(&dataset.queries, ctx);
        cpu_engine.batch_search(&queries).map(|hits| hits.len())
    };
    let workload_b = || {
        let queries = gate_batch_queries(&dataset.queries, ctx);
        gpu_engine.batch_search(&queries).map(|hits| hits.len())
    };
    let ab = run_ab(&config, workload_a, workload_b)
        .map_err(|err| format!("{LABEL}: A/B measurement failed: {err}"))?;
    if fallback_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        println!(
            "{LABEL}: not measurable in this environment (cpu fallback engaged during \
             measurement) requested=true pass=false"
        );
        return Ok(false);
    }

    let p95_a = p95_from_samples(&ab.a.samples)
        .map_err(|err| format!("{LABEL}: p95 of A samples unavailable: {err}"))?;
    let p95_b = p95_from_samples(&ab.b.samples)
        .map_err(|err| format!("{LABEL}: p95 of B samples unavailable: {err}"))?;
    let pass = check_improvement_at_least(p95_a, p95_b, min_improvement_pct)
        .map_err(|err| format!("{LABEL}: improvement check failed: {err}"))?;
    println!(
        "{LABEL}: rows={GPU_GATE_ROW_COUNT} dim={GPU_GATE_DIM} batch={GPU_GATE_BATCH_SIZE} \
         requested=true pass={pass}"
    );
    if verbose {
        println!("verbose({LABEL}): cpu_simd_p95={p95_a:?} gpu_p95={p95_b:?}");
    }
    Ok(pass)
}

/// CORE-16 ゲート（GPU 常駐コピーの f16 パック vs f32 常駐の p95 短縮率。
/// Issue #234）。
///
/// 本 ID の A/B は **GPU バッチ経路上**の常駐形式比較であり（ポインタ:
/// `docs/spec/04-behavior/core-engine.md` CORE-16。CPU-SIMD 経路への f16 適用は
/// 本 ID の対象外）、対照 A = `engine::gpu_batch::GpuF32ContrastBackend`
/// （f32 常駐。bench/テスト専用の対照経路）、被検 B = 本番の f16 パック常駐
/// `engine::gpu_batch::GpuBatchBackend` を**どちらも GPU 経路で直接構築**して
/// 比較する（`FallbackBatchEngine` は経由しない。CPU 縮退が発生すると GPU 経路
/// 同士の比較にならないため）。opt-in されていなければ「対象外」を出力して
/// 合否に数えない（silent skip にしない）。opt-in 時にどちらかの GPU 初期化が
/// 失敗した場合は `pass=false`（fail-closed。CPU 比較を GPU 実測の代替として
/// 計上しない＝アサーション弱体化を避ける。CORE-6 と同方針）。
fn run_core16_gate(
    dataset: &GateDataset,
    ctx: &PolicyContext,
    verbose: bool,
) -> Result<bool, String> {
    const LABEL: &str = "f16_resident_vs_f32_resident_p95";
    if !opt_in_requested_from_env("BENCH_CORE16") {
        println!(
            "{LABEL}: out of scope for this run (not counted toward pass/fail; \
             set BENCH_CORE16=1 with BENCH_CORE16_MIN_IMPROVEMENT_PCT to enable) requested=false"
        );
        return Ok(true);
    }
    let min_improvement_pct = min_improvement_pct_from_env("BENCH_CORE16_MIN_IMPROVEMENT_PCT")?;

    let f32_backend = match engine::gpu_batch::GpuF32ContrastBackend::try_new(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
    ) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "{LABEL}: not measurable in this environment (f32-resident gpu contrast \
                 backend unavailable: {e}) requested=true pass=false"
            );
            return Ok(false);
        }
    };
    let f16_matrix = ResidentMatrix::build(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        GPU_GATE_DIM,
        &dataset.vectors,
    )
    .map_err(|err| format!("{LABEL}: resident matrix build failed: {err}"))?;
    let f16_backend = match engine::gpu_batch::GpuBatchBackend::try_new(f16_matrix) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "{LABEL}: not measurable in this environment (f16-resident gpu backend \
                 unavailable: {e}) requested=true pass=false"
            );
            return Ok(false);
        }
    };

    // レビュー指摘対応（PR #245）: `gate_batch_queries` の `Vec<BatchQuery>` 確保・
    // 構築は測定区間の外（`run_ab` 呼び出し前）で 1 回だけ行い、両ワークロードから
    // 同じ参照を再利用する。`BatchQuery` はクエリ本体・ctx への借用のみを保持し
    // `batch_search` も `&[BatchQuery]` を読み取るだけで消費しないため、反復間で
    // 使い回しても計測対象（f16/f32 常駐形式の差）を歪めない。以前は各反復の
    // 測定区間内で構築しており、その共通コストが f16/f32 間の短縮率を薄めていた
    // （CORE-7 と同じ「入力生成・確保は測定区間外」契約に反していた）。
    let queries = gate_batch_queries(&dataset.queries, ctx);

    // レビュー指摘対応（PR #245・cursor[bot]）: `batch_search` の `Result` を
    // 検査せず捨てていたため、`try_new` 成功後にランタイムで GPU 失敗が起きても
    // エラー経路（通常大幅に軽量）がそのまま計測サンプルへ計上され、
    // `check_improvement_at_least` が誤って `pass=true` を返しうる状態だった
    // （CORE-6 の縮退カウントに相当する fail-closed チェックが本ゲートに無かった）。
    // 両ワークロードのエラー発生を計測中も観測し、1 件でもあれば「判定不能」として
    // `pass=false` に倒す。
    let error_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let error_count_a = std::sync::Arc::clone(&error_count);
    let error_count_b = std::sync::Arc::clone(&error_count);

    let config = MeasurementConfig::new(20, 20, 1).expect("protocol minimums satisfied");
    let workload_a = || match f32_backend.batch_search(&queries) {
        Ok(hits) => hits.len(),
        Err(err) => {
            error_count_a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "batch_bench: {LABEL}: f32-resident batch_search returned an error \
                 during measurement: {err}"
            );
            0
        }
    };
    let workload_b = || match f16_backend.batch_search(&queries) {
        Ok(hits) => hits.len(),
        Err(err) => {
            error_count_b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "batch_bench: {LABEL}: f16-resident batch_search returned an error \
                 during measurement: {err}"
            );
            0
        }
    };
    let ab = run_ab(&config, workload_a, workload_b)
        .map_err(|err| format!("{LABEL}: A/B measurement failed: {err}"))?;
    if error_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        println!(
            "{LABEL}: not measurable in this environment (batch_search returned an \
             error during measurement; see stderr) requested=true pass=false"
        );
        return Ok(false);
    }

    let p95_a = p95_from_samples(&ab.a.samples)
        .map_err(|err| format!("{LABEL}: p95 of A samples unavailable: {err}"))?;
    let p95_b = p95_from_samples(&ab.b.samples)
        .map_err(|err| format!("{LABEL}: p95 of B samples unavailable: {err}"))?;
    let pass = check_improvement_at_least(p95_a, p95_b, min_improvement_pct)
        .map_err(|err| format!("{LABEL}: improvement check failed: {err}"))?;
    println!(
        "{LABEL}: rows={GPU_GATE_ROW_COUNT} dim={GPU_GATE_DIM} batch={GPU_GATE_BATCH_SIZE} \
         requested=true pass={pass}"
    );
    if verbose {
        println!("verbose({LABEL}): f32_resident_p95={p95_a:?} f16_resident_p95={p95_b:?}");
    }
    Ok(pass)
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
    // 実測値の既定非出力（Issue #279）の opt-in ゲート。CI（`GITHUB_ACTIONS`）下では
    // `verbose_requested_from_env` 自体が fail-closed で拒否するため、ここで検証不能な
    // 経路を通す前に必ず判定する。
    let verbose = match verbose_requested_from_env() {
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
    // limit（BENCH_BATCH_MAX_DEGRADATION_PCT）・実測値（p95・median）は意図的に
    // ログへ出力しない（閾値は spec が SSOT・実測値は public リポの Actions ログから
    // 閾値を逆算されうるため出さない。モジュール冒頭コメント・Issue #279 参照）。
    // 実測値が必要な場合のみ `BENCH_VERBOSE` opt-in で追加出力する。
    println!(
        "dynamic_window_degradation: batch_size={AGG_BATCH_SIZE} dim={AGG_QUERY_DIM} pass={degradation_ok}"
    );
    if verbose {
        println!(
            "verbose(dynamic_window_degradation): direct_median={:?} direct_p95={p95_a:?} windowed_median={:?} windowed_p95={p95_b:?}",
            ab.a.summary.median, ab.b.summary.median,
        );
    }

    // --- CORE-6 / CORE-16: opt-in の実測ゲート（Issue #178 で実 GPU バックエンドへ
    // 接続済み。未 opt-in なら「対象外」を出力するだけで合否に数えない）---
    let gate_ctx = PolicyContext::new(BENCH_TENANT).expect("valid tenant");
    let gate_dataset = build_gate_dataset(&mut rng);

    let core6_ok = match run_core6_gate(&gate_dataset, &gate_ctx, verbose) {
        Ok(ok) => ok,
        Err(msg) => {
            eprintln!("batch_bench: {msg}");
            std::process::exit(1);
        }
    };
    let core16_ok = match run_core16_gate(&gate_dataset, &gate_ctx, verbose) {
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
