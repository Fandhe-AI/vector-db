//! GPU バッチ検索（`engine::gpu_batch`。TASK-128〜130・Issue #178 ポインタ）が
//! CPU-SIMD バッチ経路（`engine::batch_search::BatchEngine`）に対してどの規模・
//! バッチサイズから優位になるかを実測する、手動専用の規模スイープベンチ。
//!
//! `batch_bench.rs`（TASK-130・CORE-6/CORE-16）は固定規模（20,000 行・dim 256・
//! batch 8）での単一の A/B ゲート判定を担うのに対し、本ベンチは spec 由来の
//! 閾値を持たない情報提供専用の測定ツールであり、`BENCH_GPU_SCALING_ROWS`/
//! `BENCH_GPU_SCALING_DIMS`/`BENCH_GPU_SCALING_BATCH` の直積で構成した複数点を
//! 掃引し、各点の p50/p95・1 クエリあたりのレイテンシ・GPU f16 常駐経路の
//! 短縮率（`speedup_f16_p95`）を出力する（`hnsw_parallel_build_bench.rs`・
//! `Makefile: bench-hnsw-parallel-build` と同じ「手動専用・CI 非配線・情報提供」
//! の位置づけ）。
//!
//! 計測する 3 経路（いずれも `engine::batch_fallback::BatchBackend` 実装。
//! `FallbackBatchEngine` は経由しない——batch_bench.rs の CORE-6/CORE-16 と
//! 同方針で、CPU 縮退が混ざると経路比較にならないため）:
//! - A: CPU-SIMD（`engine::batch_search::BatchEngine`。f16 パック常駐行列を
//!   CPU の実行時 SIMD 検出カーネルで全走査）
//! - B: GPU f16 常駐（`engine::gpu_batch::GpuBatchBackend`。本番の dispatch 経路）
//! - C: GPU f32 常駐対照（`engine::gpu_batch::GpuF32ContrastBackend`。
//!   Issue #234・CORE-16 の対照経路を流用）
//!
//! 各構成の計測に先立ち、A を厳密対照として B・C の Top-k 結果が「A の id
//! 集合に含まれる」か「A の k 位スコア以上」のいずれかを満たすかを 1 回だけ
//! （計測区間外で）確認し、満たさない件数を `mismatch=` として出力する
//! （`harness::gpu_scaling::count_boundary_tolerant_mismatches`）。
//!
//! GPU アダプタ名・backend 種別の出力: `engine::gpu_batch` は adapter 情報を
//! 取得できる公開関数を持たないため（`wgpu::AdapterInfo` を返す getter が
//! `pub` で露出していない）、本ベンチはアダプタ名を出力しない（production
//! コード変更なしの制約下での取得経路なし）。
//!
//! GPU が全く初期化できない環境（adapter 未検出等）では起動直後の疎通確認で
//! 即座に非 0 終了する（fail-closed。CPU 経路だけの実測を GPU 実測の代替として
//! 出力しない）。個々の (rows, dim, batch) 点が測定量上限（`MAX_BATCH_WORK`・
//! `MAX_BATCH_ROWS`）を超える、またはメモリ確保に失敗する場合はその点だけを
//! `skip`/`not measurable` として飛ばし、他の点の計測は継続する。

#[allow(dead_code)]
mod harness;

use harness::accept::p95_from_samples;
use harness::env_report::EnvReport;
use harness::gpu_scaling::{
    count_boundary_tolerant_mismatches, format_skip_line, format_unavailable_line, parse_batches,
    parse_dims, parse_measured_iterations, parse_rows, parse_top_k, read_env_var, speedup_ratio,
    GpuScalingResult,
};
use harness::protocol::{run_fallible, MeasurementConfig, TrialFailure};
use harness::rng::DeterministicRng;

use engine::batch_fallback::BatchBackend;
use engine::batch_search::{
    BatchEngine, BatchQuery, ResidentMatrix, MAX_BATCH_ROWS, MAX_BATCH_TOTAL_BYTES, MAX_BATCH_WORK,
};
use engine::gpu_batch::{GpuBatchBackend, GpuF32ContrastBackend};
use engine::kernel::SearchHit;
use engine::policy::PolicyContext;
use engine::storage::Visibility;

/// 本ベンチ専用の合成データセットが使うテナント ID（実データではない。
/// `batch_bench.rs::BENCH_TENANT` と同じ位置づけ）。
const BENCH_TENANT: &str = "gpu-scaling-bench-tenant";

/// `BENCH_GPU_SCALING_ROWS` 既定値。GPU 転送・dispatch の固定コストを償却できる
/// 下限（20,000）から、f16 常駐で 1GiB 予算（`MAX_BATCH_TOTAL_BYTES`）に迫る
/// 規模（500,000 × dim 256 で約 256 MiB）までを掃引する。
const DEFAULT_ROWS: [usize; 3] = [20_000, 100_000, 500_000];
/// `BENCH_GPU_SCALING_DIMS` 既定値。
const DEFAULT_DIMS: [usize; 2] = [128, 256];
/// `BENCH_GPU_SCALING_BATCH` 既定値。GPU 経路はクエリ単位に dispatch するため、
/// 1 本（固定コスト支配）から 256 本（転送・同期コストを償却できる規模）まで。
const DEFAULT_BATCH: [usize; 4] = [1, 8, 64, 256];
/// `BENCH_GPU_SCALING_TOPK` 既定値。
const DEFAULT_TOP_K: usize = 10;
/// `BENCH_GPU_SCALING_ITERS`（計測反復回数）既定値。warmup は
/// `harness::protocol::MeasurementConfig` の下限（20 回）固定で上書き対象外。
const DEFAULT_MEASURED_ITERATIONS: u32 = 20;
const WARMUP_ITERATIONS: u32 = 20;

/// 1 つの (rows, dim, batch) 構成分の合成データセット。`batch_bench.rs::
/// GateDataset` と同型だが、行数・次元・バッチ本数がすべて可変である点が異なる。
struct ScalingDataset {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    vectors: Vec<f32>,
    queries: Vec<Vec<f32>>,
}

/// `rows`/`dim`/`batch` から合成データセットを構築する。アロケーションは
/// すべて `try_reserve_exact` を経由し（coding-rust.md「長さフィールドは上限
/// 検証してからアロケーションに使う」）、確保失敗は panic ではなく `Err` として
/// 呼び出し元（`main` のスイープループ）へ返し、この構成点だけを飛ばせるようにする。
fn build_dataset(
    rng: &mut DeterministicRng,
    rows: usize,
    dim: usize,
    batch: usize,
) -> Result<ScalingDataset, String> {
    let elements = rows
        .checked_mul(dim)
        .ok_or_else(|| "rows * dim overflows usize".to_string())?;

    let mut vectors: Vec<f32> = Vec::new();
    vectors
        .try_reserve_exact(elements)
        .map_err(|e| format!("vectors allocation failed: {e}"))?;
    for _ in 0..rows {
        vectors.extend(rng.next_vector(dim));
    }

    let mut ids: Vec<u64> = Vec::new();
    ids.try_reserve_exact(rows)
        .map_err(|e| format!("ids allocation failed: {e}"))?;
    ids.extend(0..rows as u64);

    let mut tenant_ids: Vec<String> = Vec::new();
    tenant_ids
        .try_reserve_exact(rows)
        .map_err(|e| format!("tenant_ids allocation failed: {e}"))?;
    tenant_ids.extend(std::iter::repeat_n(BENCH_TENANT.to_string(), rows));

    let mut visibilities: Vec<Visibility> = Vec::new();
    visibilities
        .try_reserve_exact(rows)
        .map_err(|e| format!("visibilities allocation failed: {e}"))?;
    visibilities.extend(std::iter::repeat_n(Visibility::Public, rows));

    let mut queries: Vec<Vec<f32>> = Vec::new();
    queries
        .try_reserve_exact(batch)
        .map_err(|e| format!("queries allocation failed: {e}"))?;
    for _ in 0..batch {
        queries.push(rng.next_vector(dim));
    }

    Ok(ScalingDataset {
        ids,
        tenant_ids,
        visibilities,
        vectors,
        queries,
    })
}

/// `dataset.queries` から測定区間外で `BatchQuery` 列を組み立てる
/// （`batch_bench.rs::gate_batch_queries` と同方針）。
fn batch_queries<'a>(
    queries: &'a [Vec<f32>],
    ctx: &'a PolicyContext,
    k: usize,
) -> Vec<BatchQuery<'a>> {
    queries
        .iter()
        .map(|q| BatchQuery {
            vector: q.as_slice(),
            k,
            ctx,
        })
        .collect()
}

/// `BatchHit::hits`（`SearchHit` 列）を `(id, score)` の組へ変換する
/// （`harness::gpu_scaling::count_boundary_tolerant_mismatches` の入力形）。
fn hit_pairs(hits: &[SearchHit]) -> Vec<(u64, f32)> {
    hits.iter().map(|h| (h.id, h.score)).collect()
}

/// 環境変数から本ベンチの計測条件一式を読み取る。
struct ScalingConfig {
    rows: Vec<usize>,
    dims: Vec<usize>,
    batches: Vec<usize>,
    top_k: usize,
    measured_iterations: u32,
}

fn load_config() -> Result<ScalingConfig, String> {
    let rows_raw = read_env_var("BENCH_GPU_SCALING_ROWS").map_err(|e| e.to_string())?;
    let dims_raw = read_env_var("BENCH_GPU_SCALING_DIMS").map_err(|e| e.to_string())?;
    let batch_raw = read_env_var("BENCH_GPU_SCALING_BATCH").map_err(|e| e.to_string())?;
    let topk_raw = read_env_var("BENCH_GPU_SCALING_TOPK").map_err(|e| e.to_string())?;
    let iters_raw = read_env_var("BENCH_GPU_SCALING_ITERS").map_err(|e| e.to_string())?;

    let rows = parse_rows(rows_raw.as_deref(), &DEFAULT_ROWS).map_err(|e| e.to_string())?;
    let dims = parse_dims(dims_raw.as_deref(), &DEFAULT_DIMS).map_err(|e| e.to_string())?;
    let batches = parse_batches(batch_raw.as_deref(), &DEFAULT_BATCH).map_err(|e| e.to_string())?;
    let top_k = parse_top_k(topk_raw.as_deref(), DEFAULT_TOP_K).map_err(|e| e.to_string())?;
    let measured_iterations =
        parse_measured_iterations(iters_raw.as_deref(), DEFAULT_MEASURED_ITERATIONS)
            .map_err(|e| e.to_string())?;

    Ok(ScalingConfig {
        rows,
        dims,
        batches,
        top_k,
        measured_iterations,
    })
}

/// 起動直後の GPU 疎通確認。1 行だけの最小データセットで
/// [`GpuBatchBackend::try_new`] を試み、失敗した場合はプロセス全体として GPU が
/// 使えない環境だとみなす（fail-closed。以降のスイープを一切実行せず終了する）。
fn probe_gpu_available(rng: &mut DeterministicRng, dim: usize) -> Result<(), String> {
    let ids = vec![0u64];
    let tenant_ids = vec![BENCH_TENANT.to_string()];
    let visibilities = vec![Visibility::Public];
    let vectors = rng.next_vector(dim);
    let matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, &vectors)
        .map_err(|e| format!("resident matrix build failed during gpu probe: {e}"))?;
    GpuBatchBackend::try_new(matrix)
        .map(|_backend| ())
        .map_err(|e| e.to_string())
}

fn main() {
    let config = match load_config() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("gpu_scaling_bench: {msg}");
            std::process::exit(1);
        }
    };

    let detected_isa = format!("{:?}", engine::isa::current().isa());
    println!("{}", EnvReport::capture(detected_isa));

    let mut rng = DeterministicRng::new(1);

    // 起動直後の GPU 疎通確認（プロセス全体として GPU が使えない環境は即座に
    // fail-closed で終了する。CPU 経路だけの実測を GPU 実測の代替として
    // 出力しない）。プローブに使う次元はスイープ対象の最小次元を使う。
    let probe_dim = config.dims.iter().copied().min().unwrap_or(DEFAULT_DIMS[0]);
    if let Err(msg) = probe_gpu_available(&mut rng, probe_dim) {
        println!("{}", format_unavailable_line(None, &msg));
        std::process::exit(1);
    }

    let ctx = match PolicyContext::new(BENCH_TENANT) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gpu_scaling_bench: failed to construct policy context: {e}");
            std::process::exit(1);
        }
    };

    let mut any_measured = false;
    let mut seed: u64 = 1;

    for &rows in &config.rows {
        for &dim in &config.dims {
            for &batch in &config.batches {
                seed = seed.saturating_add(1);
                let k = config.top_k;

                if rows > MAX_BATCH_ROWS {
                    println!(
                        "{}",
                        format_skip_line(
                            rows,
                            dim,
                            batch,
                            k,
                            "rows exceeds engine::batch_search::MAX_BATCH_ROWS"
                        )
                    );
                    continue;
                }
                let total_work = rows.checked_mul(batch).and_then(|v| v.checked_mul(dim));
                match total_work {
                    Some(work) if work <= MAX_BATCH_WORK => {}
                    _ => {
                        println!(
                            "{}",
                            format_skip_line(
                                rows,
                                dim,
                                batch,
                                k,
                                "rows * batch * dim exceeds MAX_BATCH_WORK"
                            )
                        );
                        continue;
                    }
                }

                // データ生成前にバイト量を検査する。`ResidentMatrix::build` の
                // 1 GiB 上限（`MAX_BATCH_TOTAL_BYTES`）で拒否される組み合わせは f32 の
                // コーパスを確保する前に除外し、実ページ確保時のメモリ不足でプロセスが
                // 停止する前にスキップ行を出力する。
                let dataset_bytes = rows
                    .checked_mul(dim)
                    .and_then(|v| v.checked_mul(std::mem::size_of::<f32>()));
                match dataset_bytes {
                    Some(bytes) if bytes <= MAX_BATCH_TOTAL_BYTES => {}
                    _ => {
                        println!(
                            "{}",
                            format_skip_line(
                                rows,
                                dim,
                                batch,
                                k,
                                "rows * dim * 4 bytes exceeds MAX_BATCH_TOTAL_BYTES"
                            )
                        );
                        continue;
                    }
                }

                let dataset = match build_dataset(&mut rng, rows, dim, batch) {
                    Ok(d) => d,
                    Err(msg) => {
                        println!("{}", format_skip_line(rows, dim, batch, k, &msg));
                        continue;
                    }
                };

                let cpu_matrix = match ResidentMatrix::build(
                    &dataset.ids,
                    &dataset.tenant_ids,
                    &dataset.visibilities,
                    dim,
                    &dataset.vectors,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("cpu resident matrix build failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let cpu_engine = BatchEngine::new(cpu_matrix);

                let f16_matrix = match ResidentMatrix::build(
                    &dataset.ids,
                    &dataset.tenant_ids,
                    &dataset.visibilities,
                    dim,
                    &dataset.vectors,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f16 resident matrix build failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let gpu_f16 = match GpuBatchBackend::try_new(f16_matrix) {
                    Ok(b) => b,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(Some((rows, dim, batch, k)), &e.to_string())
                        );
                        continue;
                    }
                };

                let gpu_f32 = match GpuF32ContrastBackend::try_new(
                    &dataset.ids,
                    &dataset.tenant_ids,
                    &dataset.visibilities,
                    dim,
                    &dataset.vectors,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(Some((rows, dim, batch, k)), &e.to_string())
                        );
                        continue;
                    }
                };

                let queries = batch_queries(&dataset.queries, &ctx, k);

                // 正しさの検証（計測区間外・1 回のみ）: A を厳密対照として B・C
                // の Top-k が同点許容つきで一致することを確認する。
                let cpu_hits = match cpu_engine.batch_search(&queries) {
                    Ok(h) => h,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("cpu batch_search failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f16_hits = match gpu_f16.batch_search(&queries) {
                    Ok(h) => h,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f16 batch_search failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f32_hits = match gpu_f32.batch_search(&queries) {
                    Ok(h) => h,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f32 batch_search failed: {e}")
                            )
                        );
                        continue;
                    }
                };

                if cpu_hits.len() != f16_hits.len() || cpu_hits.len() != f32_hits.len() {
                    println!(
                        "{}",
                        format_unavailable_line(
                            Some((rows, dim, batch, k)),
                            "query count mismatch across backends"
                        )
                    );
                    continue;
                }

                let mut mismatch = 0usize;
                for i in 0..cpu_hits.len() {
                    let baseline = hit_pairs(&cpu_hits[i].hits);
                    mismatch += count_boundary_tolerant_mismatches(
                        &baseline,
                        &hit_pairs(&f16_hits[i].hits),
                    );
                    mismatch += count_boundary_tolerant_mismatches(
                        &baseline,
                        &hit_pairs(&f32_hits[i].hits),
                    );
                }

                let measure_config = match MeasurementConfig::new(
                    WARMUP_ITERATIONS,
                    config.measured_iterations,
                    seed,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("gpu_scaling_bench: measurement config invalid: {e}");
                        std::process::exit(1);
                    }
                };

                // 各試行の `batch_search` が `Err` を返した場合（事前確認後の GPU 転送
                // 失敗等）は失敗処理の所要時間を正常サンプルに混ぜず、`Fatal` として
                // この規模点の計測失敗へ伝播する（除外・埋め合わせはしない）。
                fn fatal<E>(_: &E) -> TrialFailure {
                    TrialFailure::Fatal
                }
                let cpu_measurement = run_fallible(
                    &measure_config,
                    0,
                    || cpu_engine.batch_search(&queries),
                    fatal,
                )
                .map(|m| m.measurement);
                let f16_measurement =
                    run_fallible(&measure_config, 0, || gpu_f16.batch_search(&queries), fatal)
                        .map(|m| m.measurement);
                let f32_measurement =
                    run_fallible(&measure_config, 0, || gpu_f32.batch_search(&queries), fatal)
                        .map(|m| m.measurement);

                let (cpu_measurement, f16_measurement, f32_measurement) =
                    match (cpu_measurement, f16_measurement, f32_measurement) {
                        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
                        (a, b, c) => {
                            let reason = [a.err(), b.err(), c.err()]
                                .into_iter()
                                .flatten()
                                .map(|e| e.to_string())
                                .collect::<Vec<_>>()
                                .join("; ");
                            println!(
                                "{}",
                                format_unavailable_line(
                                    Some((rows, dim, batch, k)),
                                    &format!("measurement failed: {reason}")
                                )
                            );
                            continue;
                        }
                    };

                let cpu_p95 = match p95_from_samples(&cpu_measurement.samples) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("cpu p95 unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f16_p95 = match p95_from_samples(&f16_measurement.samples) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f16 p95 unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f32_p95 = match p95_from_samples(&f32_measurement.samples) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f32 p95 unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };

                let speedup_f16_p95 = match speedup_ratio(cpu_p95, f16_p95) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("speedup ratio unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };

                let batch_u32 = u32::try_from(batch).unwrap_or(1).max(1);
                let result = GpuScalingResult {
                    rows,
                    dim,
                    batch,
                    k,
                    cpu_simd_p50: cpu_measurement.summary.median,
                    cpu_simd_p95: cpu_p95,
                    gpu_f16_p50: f16_measurement.summary.median,
                    gpu_f16_p95: f16_p95,
                    gpu_f32_p50: f32_measurement.summary.median,
                    gpu_f32_p95: f32_p95,
                    per_query_cpu_p50: cpu_measurement.summary.median / batch_u32,
                    per_query_gpu_f16_p50: f16_measurement.summary.median / batch_u32,
                    speedup_f16_p95,
                    mismatch,
                };
                println!("{result}");
                any_measured = true;
            }
        }
    }

    if !any_measured {
        eprintln!("gpu_scaling_bench: no (rows, dim, batch) combination could be measured");
        std::process::exit(1);
    }
}
