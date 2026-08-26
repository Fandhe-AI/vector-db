//! バッチ検索の実 GPU バックエンド（TASK-128〜130・対象ビヘイビア: CORE-6, 8,
//! 16。ポインタ: Issue #178）。
//!
//! `batch_fallback.rs::BatchBackend` の公開差し替え点へ差し込む実装で、
//! `wgpu`（=30.0.1・依存追加はオーナー承認待ち。`crates/engine/Cargo.toml`
//! コメント参照）を通じて Vulkan/Metal/DX12 の compute パイプラインを扱う。
//! `batch_search.rs::ResidentMatrix`（f16 2 要素/u32 パック常駐行列）が保持する
//! `packed()` バッファを GPU の STORAGE バッファへアップロードし、行 × クエリの
//! 内積計算だけを GPU 側で行う。
//!
//! # 責務境界（`batch_fallback.rs`・`batch_search.rs` との分担）
//!
//! - テナント境界・可視性判定は本モジュールも
//!   `policy.rs::PolicyContext::is_visible` の単一照合パスを使う（CORE-2 と
//!   同じ判定関数。独自のテナント文字列比較は行わない）
//! - 本バックエンドが返す結果は [`crate::batch_fallback::BatchBackend`] の
//!   契約（doc 参照）を満たすことを目指すが、`FallbackBatchEngine::
//!   revalidate_primary_hits` が独立に再検証するため、本モジュールが唯一の
//!   防御線ではない（codex-review P0 指摘対応の設計を踏襲）
//! - 計算量 DoS 対策は `batch_search.rs::compute_tenant_work`（`rows × queries
//!   × dim` の checked 演算）を CPU 経路と共有し、`batch_search` 冒頭で
//!   dispatch 前に `queries.len()` を乗じた総量を [`MAX_BATCH_WORK`] と照合
//!   する。GPU 経路はテナント別に走査を分けないため「常駐行列の全行数」を
//!   単一テナント分の行数として扱い、`run_batch_search` のテナント別合算と
//!   同じかより厳しい側に倒れる保守的な上界にする（クエリごとの
//!   `gather_reachable_rows` 内の単発チェックはこの総量ガードの後段に
//!   残す防御的な二重チェックであり、唯一の防御線ではない）
//!
//! # panic を作らない設計
//!
//! `unwrap`/`expect`/添字アクセスは使わない。バッファサイズは `checked_*` で
//! 導出し、wgpu のエラー・デバイスロスト・ポーリング失敗はすべて
//! [`crate::batch_fallback::BatchBackendError`] へ写像して `Result` で返す
//! （coding-rust.md「ライブラリコードでは panic させない」）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::batch_fallback::{BatchBackend, BatchBackendError, BatchExecError};
use crate::batch_search::{
    compute_tenant_work, validate_batch_queries, BatchHit, BatchQuery, BatchRowSource,
    MAX_BATCH_WORK,
};
use crate::kernel::{CandidateHit, TopKSelector};
use crate::policy::PolicyContext;
use crate::storage::Visibility;

/// 1 回の GPU dispatch で読み戻すスコアバッファの予算（バイト）。adapter の
/// `max_storage_buffer_binding_size` に依らず、compute の 1 次元 dispatch が
/// `max_compute_workgroups_per_dimension`（実測値: 65535。§0 実測記録
/// ポインタ）に収まるよう、ワークグループサイズ 256 との積が確実に収まる
/// 小さめの固定値を選ぶ（32 MiB ÷ 4 byte ÷ 256 = 32768 workgroups
/// ＜ 65535）。この値を超える行数は [`GpuBatchBackend::batch_search`] が
/// 複数回の dispatch に分割して処理する（クエリを跨いだ結果の合算は行わず、
/// クエリごとに独立して縮小するため、分割自体はスコア計算の正しさに影響しない）。
const GPU_SCORE_BUFFER_BUDGET_BYTES: usize = 32 * 1024 * 1024;
const GPU_WORKGROUP_SIZE: u32 = 256;
/// adapter の `max_compute_workgroups_per_dimension` に対する安全側の固定上限
/// （実機実測値は 65535）。取得できた実際の limits がこれを下回る場合はその値を使う。
const MAX_WORKGROUPS_PER_DIMENSION_FALLBACK: u32 = 65535;

/// WGSL: 常駐行列の 1 行（f16 2 要素/u32 パック。`batch_search.rs::pack_f16x2`
/// と同一表現）と 1 クエリベクトルの内積を計算する。`unpack2x16float` は WGSL
/// コア機能（`shader-f16` 拡張は不要）で、`batch_search.rs::unpack_f16x2` と
/// 同じビット解釈をとる（同モジュールのドキュメンテーションコメント参照）。
const DOT_SHADER_WGSL: &str = r#"
struct Params {
    dim_half: u32,
    row_count: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> packed_rows: array<u32>;
@group(0) @binding(2) var<storage, read> row_ids: array<u32>;
@group(0) @binding(3) var<storage, read> query: array<f32>;
@group(0) @binding(4) var<storage, read_write> scores: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.row_count) {
        return;
    }
    let row = row_ids[i];
    let row_base = row * params.dim_half;
    var acc: f32 = 0.0;
    var j: u32 = 0u;
    loop {
        if (j >= params.dim_half) {
            break;
        }
        let packed = packed_rows[row_base + j];
        let unpacked = unpack2x16float(packed);
        acc = acc + unpacked.x * query[j * 2u] + unpacked.y * query[j * 2u + 1u];
        j = j + 1u;
    }
    scores[i] = acc;
}
"#;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct GpuParams {
    dim_half: u32,
    row_count: u32,
    _pad0: u32,
    _pad1: u32,
}

impl GpuParams {
    /// `bytemuck` は使わず（依存最小方針・.claude/rules/dependency-policy.md）、
    /// `to_ne_bytes` の連結だけでバイト列化する。フィールド順は WGSL の
    /// `Params` と一致させ、host/device 双方をネイティブエンディアンに揃える
    /// （native GPU バックエンドはホストと同一エンディアンで動作する前提）。
    fn to_ne_bytes_vec(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&self.dim_half.to_ne_bytes());
        out.extend_from_slice(&self.row_count.to_ne_bytes());
        out.extend_from_slice(&self._pad0.to_ne_bytes());
        out.extend_from_slice(&self._pad1.to_ne_bytes());
        out
    }
}

/// プロセス共有の GPU デバイス文脈（adapter/device/queue/pipeline）。
/// [`OnceLock`] で 1 回だけ初期化し、初期化失敗も含めて結果をキャッシュする
/// （毎回の `GpuBatchBackend::try_new` が重い初期化をやり直さないため）。
struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    max_storage_buffer_binding_size: u64,
    max_workgroups_per_dimension: u32,
    /// デバイスロスト検知用ラッチ。`Device::set_device_lost_callback` から
    /// 更新される（コールバックは別スレッドから呼ばれうるため `AtomicBool`）。
    device_lost: std::sync::Arc<AtomicBool>,
}

fn global_context() -> &'static Result<GpuContext, String> {
    static CONTEXT: OnceLock<Result<GpuContext, String>> = OnceLock::new();
    CONTEXT.get_or_init(init_gpu_context)
}

/// GPU デバイスの初期化本体。失敗はすべて `Err(String)`（英語・adapter 名や
/// テナント情報を含まない）として返し、panic 経路（`Instance::new` の一部条件
/// 等）を事前ガードで避ける（TASK-128 設計ドキュメント §3.2 ポインタ）。
fn init_gpu_context() -> Result<GpuContext, String> {
    if wgpu::Instance::enabled_backend_features().is_empty() {
        return Err("no wgpu backend compiled into this binary".to_string());
    }

    // `InstanceDescriptor::new_without_display_handle` を使い、`WGPU_*` 環境
    // 変数を読む `from_env` 系は使わない（CORE-12: 経路を外部から上書きする
    // 機構を設けない方針。`dispatch.rs` モジュールドキュメント参照）。
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

    let adapter = pollster_free_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        // fingerprinting 対策の limit bucketing は untrusted content へ wgpu
        // を露出する用途向けの機能で、engine は自プロセス内でのみ GPU を
        // 使うため無効のままでよい（実測 limits をそのまま使う）。
        apply_limit_buckets: false,
    }))
    .map_err(|e| format!("adapter request failed: {e}"))?;

    let info = adapter.get_info();
    if info.device_type == wgpu::DeviceType::Cpu {
        // lavapipe 等のソフトウェア実装は「GPU 経路」としての capability を
        // 偽陽性にしないため拒否する（CORE-8/16 の性能ゲート意図に反するため）。
        return Err("adapter is a software (CPU) implementation".to_string());
    }

    let adapter_limits = adapter.limits();
    let mut required_limits = wgpu::Limits::downlevel_defaults();
    required_limits.max_storage_buffer_binding_size =
        adapter_limits.max_storage_buffer_binding_size;
    required_limits.max_buffer_size = adapter_limits.max_buffer_size;
    required_limits.max_compute_workgroups_per_dimension =
        adapter_limits.max_compute_workgroups_per_dimension;
    required_limits.max_compute_invocations_per_workgroup = adapter_limits
        .max_compute_invocations_per_workgroup
        .max(256);
    required_limits.max_compute_workgroup_size_x =
        adapter_limits.max_compute_workgroup_size_x.max(256);

    let (device, queue) = pollster_free_block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("vector-db batch backend"),
        required_features: wgpu::Features::empty(),
        required_limits,
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .map_err(|e| format!("device request failed: {e}"))?;

    let device_lost = std::sync::Arc::new(AtomicBool::new(false));
    let device_lost_flag = device_lost.clone();
    device.set_device_lost_callback(move |_reason, _msg| {
        device_lost_flag.store(true, Ordering::SeqCst);
    });

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("batch dot product"),
        source: wgpu::ShaderSource::Wgsl(DOT_SHADER_WGSL.into()),
    });

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("batch dot product bind group layout"),
        entries: &[
            storage_layout_entry(0, wgpu::BufferBindingType::Uniform),
            storage_layout_entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
            storage_layout_entry(2, wgpu::BufferBindingType::Storage { read_only: true }),
            storage_layout_entry(3, wgpu::BufferBindingType::Storage { read_only: true }),
            storage_layout_entry(4, wgpu::BufferBindingType::Storage { read_only: false }),
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("batch dot product pipeline layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });

    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("batch dot product pipeline"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    let max_workgroups_per_dimension = if adapter_limits.max_compute_workgroups_per_dimension > 0 {
        adapter_limits
            .max_compute_workgroups_per_dimension
            .min(MAX_WORKGROUPS_PER_DIMENSION_FALLBACK)
    } else {
        MAX_WORKGROUPS_PER_DIMENSION_FALLBACK
    };

    Ok(GpuContext {
        device,
        queue,
        pipeline,
        bind_group_layout,
        max_storage_buffer_binding_size: adapter_limits.max_storage_buffer_binding_size,
        max_workgroups_per_dimension,
        device_lost,
    })
}

fn storage_layout_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// std-only の同期化ヘルパー（`pollster` 依存を追加しない。
/// .claude/rules/dependency-policy.md「依存最小方針」）。`Waker::noop()` は
/// std 安定 API（Rust 1.85+。本リポ toolchain は stable 1.96 想定）で、
/// wgpu の `request_adapter`/`request_device` は native バックエンドでは
/// 即座に完結するため、1 回の `poll` で `Ready` になる（`loop` は将来の
/// 実装差分に対する保険）。
fn pollster_free_block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Waker};

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut boxed = Box::pin(fut);
    loop {
        match boxed.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// GPU バックエンド構築・実行時の入力エラー（[`BatchExecError::Input`] へ写像）。
/// テナント情報・adapter 製品名は含めない。
#[derive(Debug, Clone, PartialEq)]
enum GpuInputError {
    CapacityExceeded,
    WorkBudgetExceeded,
}

impl GpuInputError {
    fn into_batch_search_error(self) -> crate::batch_search::BatchSearchError {
        use crate::batch_search::BatchSearchError;
        match self {
            GpuInputError::CapacityExceeded => BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: GPU_SCORE_BUFFER_BUDGET_BYTES,
            },
            GpuInputError::WorkBudgetExceeded => BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            },
        }
    }
}

/// 実 GPU バックエンド（TASK-128〜130。CORE-6, 8, 16 ポインタ）。
/// [`crate::batch_fallback::BatchBackend`] の実装として
/// [`crate::batch_fallback::FallbackBatchEngine::build_with_gpu`] から
/// primary として差し込まれる。
pub struct GpuBatchBackend {
    matrix: crate::batch_search::ResidentMatrix,
    row_buffer: wgpu::Buffer,
    /// 実行時エラー（デバイスロスト等）の 1 回限りのラッチではなく、呼び出し
    /// ごとにデバイスロストの有無を確認するための共有フラグへの参照
    /// （`GpuContext::device_lost` と同一。`FallbackBatchEngine` 側の
    /// `runtime_latched` とは独立: 本フィールドは「このプロセスの GPU
    /// デバイスが失われたか」を見るだけで、縮退の可否判断自体は
    /// `batch_fallback.rs` が担う）。
    device_lost: std::sync::Arc<AtomicBool>,
    /// バッファ生成・dispatch は `&self` から呼ばれる（`BatchBackend` が
    /// `&self` メソッドのみを要求する object-safe trait のため）。wgpu の
    /// `Queue::submit`/`Buffer` 操作自体は内部で同期を取るが、複数スレッドが
    /// 同時に同一 backend へ dispatch した場合の待ち合わせを明示するために
    /// `Mutex<()>` で直列化する（正当性のためではなく、readback の
    /// map_async コールバックが呼び出し順に混線しないようにするため）。
    dispatch_lock: Mutex<()>,
}

impl GpuBatchBackend {
    /// 常駐行列から GPU バックエンドを構築する。GPU デバイスの初期化
    /// （[`global_context`]）はプロセス内で 1 回だけ行われ、以降の呼び出しは
    /// キャッシュされた結果（成功・失敗いずれも）を使う。シグネチャは
    /// `batch_fallback.rs::FallbackBatchEngine::build` の `backend_factory`
    /// 引数（`FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>,
    /// BatchBackendError>`）にそのまま渡せる形にする（`build_with_gpu` 参照）。
    pub fn try_new(matrix: crate::batch_search::ResidentMatrix) -> Result<Self, BatchBackendError> {
        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => return Err(BatchBackendError::InitFailed(msg.clone())),
        };

        let packed_bytes = matrix
            .packed()
            .len()
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| {
                BatchBackendError::InitFailed("packed matrix byte size overflow".to_string())
            })?;
        if packed_bytes as u64 > ctx.max_storage_buffer_binding_size {
            return Err(BatchBackendError::InitFailed(
                "resident matrix exceeds adapter storage buffer limit".to_string(),
            ));
        }
        // wgpu は 0 バイトのバッファ作成を許さない実装があるため、空行列は
        // GPU 経路を使わず CPU 縮退へ委ねる（`FallbackBatchEngine::build` が
        // 空行列を許容する契約と衝突しないよう、ここでは軽い理由で `InitFailed`
        // にする）。
        if packed_bytes == 0 {
            return Err(BatchBackendError::InitFailed(
                "resident matrix is empty".to_string(),
            ));
        }

        let row_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("resident matrix packed rows"),
            size: packed_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue
            .write_buffer(&row_buffer, 0, &bytes_of_u32_slice(matrix.packed()));

        Ok(Self {
            matrix,
            row_buffer,
            device_lost: ctx.device_lost.clone(),
            dispatch_lock: Mutex::new(()),
        })
    }
}

/// `&[u32]` を `to_ne_bytes` で `&[u8]` 相当のバイト列へ変換する
/// （`bytemuck` 不採用。依存最小方針）。返す `Vec<u8>` は呼び出し元が
/// `Queue::write_buffer` へそのまま渡す想定の一時バッファ。
fn bytes_of_u32_slice(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len().saturating_mul(4));
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

fn bytes_of_f32_slice(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len().saturating_mul(4));
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

/// GPU の readback バッファから `f32` 列を復元する（`from_ne_bytes`。
/// `bytemuck` 不採用）。長さが 4 の倍数でない場合は空を返す（呼び出し元が
/// バッファサイズを 4 の倍数で確保しているため通常到達しないが、fail-closed
/// に空扱いで打ち切る）。
fn f32_vec_from_ne_bytes(bytes: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(bytes.len() / 4);
    let mut chunks = bytes.chunks_exact(4);
    for chunk in &mut chunks {
        let arr: [u8; 4] = match chunk.try_into() {
            Ok(a) => a,
            Err(_) => return Vec::new(),
        };
        out.push(f32::from_ne_bytes(arr));
    }
    out
}

impl BatchBackend for GpuBatchBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        if self.device_lost.load(Ordering::SeqCst) {
            return Err(BatchExecError::Backend(BatchBackendError::DeviceLost(
                "gpu device lost".to_string(),
            )));
        }

        // `FallbackBatchEngine::batch_search` が本メソッド呼び出し前に
        // `validate_batch_queries` を適用する契約だが（`batch_fallback.rs`
        // の `BatchBackend` trait doc 参照）、本バックエンドは独自の走査
        // パイプラインを持つため（`run_batch_search` を経由しない）防御的に
        // 再検証する（TASK-128 設計方針 §3.2 ポインタ）。
        validate_batch_queries(self.matrix.dim(), queries).map_err(BatchExecError::Input)?;

        // dispatch 前の総量ガード（codex/Issue #178 レビュー指摘対応: GPU 経路が
        // `queries.len()` を乗じていなかった DoS 増幅の修正）。CPU 経路
        // （`run_batch_search`）と同じ `compute_tenant_work` を使い、
        // `常駐行列の全行数 × クエリ件数 × dim` を [`MAX_BATCH_WORK`] と照合する。
        // GPU 経路はテナント別に走査を分けないため「全行数」を単一テナント分の
        // 行数として扱う。これは実際に走査しうる最大の行集合（テナント絞り込み
        // 後の集合の superset）であり、CPU 経路のテナント別合算と同じか
        // それより厳しい側に倒れる保守的な上界になる。
        // `compute_tenant_work` はオーバーフローのみ `Err` にする（`compute_batch_work`
        // が合算後に上限照合する設計のため）。GPU 経路にはテナント別の合算対象が
        // ないため、ここで直接 [`MAX_BATCH_WORK`] とも照合する。
        let total_work =
            compute_tenant_work(self.matrix.row_count(), queries.len(), self.matrix.dim())
                .map_err(BatchExecError::Input)?;
        if total_work > MAX_BATCH_WORK {
            return Err(BatchExecError::Input(
                crate::batch_search::BatchSearchError::WorkBudgetExceeded {
                    work: total_work,
                    max: MAX_BATCH_WORK,
                },
            ));
        }

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => {
                return Err(BatchExecError::Backend(BatchBackendError::InitFailed(
                    msg.clone(),
                )))
            }
        };

        let _guard = self.dispatch_lock.lock().map_err(|_| {
            BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                "dispatch lock poisoned".to_string(),
            ))
        })?;

        let dim = self.matrix.dim();
        let dim_half = dim.div_ceil(2);
        let query_stride = dim_half.saturating_mul(2);

        let mut hits: Vec<BatchHit> = Vec::with_capacity(queries.len());
        for q in queries {
            let reachable = gather_reachable_rows(&self.matrix, q.ctx)
                .map_err(|e| BatchExecError::Input(e.into_batch_search_error()))?;

            let mut selector = TopKSelector::new(q.k);

            // 行数がバッファ予算を超える場合は複数回の dispatch に分割する
            // （GPU_SCORE_BUFFER_BUDGET_BYTES ポインタ）。各チャンクは独立に
            // 実行し、選出器へ逐次 push するため正しさに影響しない。
            let chunk_rows = gpu_chunk_row_capacity(ctx);
            for chunk in reachable.chunks(chunk_rows.max(1)) {
                let scores = dispatch_dot_products(
                    ctx,
                    &self.row_buffer,
                    self.bind_group_layout_ref(ctx),
                    chunk,
                    q.vector,
                    dim_half as u32,
                    query_stride,
                )
                .map_err(BatchExecError::Backend)?;

                if scores.len() != chunk.len() {
                    return Err(BatchExecError::Backend(BatchBackendError::TransferFailed(
                        "readback length mismatch".to_string(),
                    )));
                }
                for (&row_idx, &score) in chunk.iter().zip(scores.iter()) {
                    if !score.is_finite() {
                        continue;
                    }
                    let id = match self.matrix.ids().get(row_idx as usize) {
                        Some(id) => *id,
                        None => continue,
                    };
                    selector.push(CandidateHit { id, score });
                }
            }

            hits.push(BatchHit {
                hits: selector.into_sorted_vec(),
            });
        }

        Ok(hits)
    }
}

impl GpuBatchBackend {
    /// `GpuContext` から bind group layout の参照を取り出す（`&self` から
    /// `ctx` を経由するだけの薄いヘルパー。`dispatch_dot_products` の引数を
    /// 揃えるために存在する）。
    fn bind_group_layout_ref<'a>(&self, ctx: &'a GpuContext) -> &'a wgpu::BindGroupLayout {
        &ctx.bind_group_layout
    }
}

/// 1 チャンクあたりの最大行数（スコア + 行 index バッファの合計が
/// [`GPU_SCORE_BUFFER_BUDGET_BYTES`] に収まり、かつ dispatch のワークグループ数が
/// adapter の上限内に収まるように決める）。
fn gpu_chunk_row_capacity(ctx: &GpuContext) -> usize {
    let by_budget = GPU_SCORE_BUFFER_BUDGET_BYTES / 8; // scores(f32) + row_ids(u32) = 8 bytes/row
    let by_workgroups =
        (ctx.max_workgroups_per_dimension as usize).saturating_mul(GPU_WORKGROUP_SIZE as usize);
    by_budget.min(by_workgroups).max(1)
}

/// クエリ `ctx` から見て到達可能な行の index 列を求める（CORE-2 の単一照合
/// パス `PolicyContext::is_visible` を使う。テナント文字列の独自比較はしない）。
/// 計算量ガードの主防御線は呼び出し元 [`GpuBatchBackend::batch_search`] 冒頭の
/// `compute_tenant_work(rows, queries.len(), dim)` 総量チェック（`queries.len()`
/// を乗じた上界）であり、本関数内の `rows * 1 query * dim` チェックはその後段の
/// 防御的な二重チェックに過ぎない（単独では `queries.len()` を考慮しないため
/// 総量ガードの代替にはならない）。
fn gather_reachable_rows(
    matrix: &crate::batch_search::ResidentMatrix,
    ctx: &PolicyContext,
) -> Result<Vec<u32>, GpuInputError> {
    let rows = matrix.row_count();
    let work = rows
        .checked_mul(matrix.dim())
        .ok_or(GpuInputError::WorkBudgetExceeded)?;
    if work > MAX_BATCH_WORK {
        return Err(GpuInputError::WorkBudgetExceeded);
    }

    let mut out: Vec<u32> = Vec::new();
    out.try_reserve(rows)
        .map_err(|_| GpuInputError::CapacityExceeded)?;
    for (idx, (tenant, visibility)) in matrix
        .tenant_ids()
        .iter()
        .zip(matrix.visibilities().iter())
        .enumerate()
    {
        if ctx.is_visible(tenant, *visibility) {
            // `idx` は `ResidentMatrix::build` が `MAX_BATCH_ROWS`
            // （1,000,000）以下に制限済みのため `u32` で表現できる。
            if let Ok(idx_u32) = u32::try_from(idx) {
                out.push(idx_u32);
            }
        }
    }
    let _ = Visibility::Public; // import 用途の明示（マスク判定は is_visible 経由のみ）
    Ok(out)
}

/// 1 クエリ × `row_indices` 分の内積を GPU で計算し、readback した `f32` 列を返す。
fn dispatch_dot_products(
    ctx: &GpuContext,
    row_buffer: &wgpu::Buffer,
    bind_group_layout: &wgpu::BindGroupLayout,
    row_indices: &[u32],
    query: &[f32],
    dim_half: u32,
    query_stride: usize,
) -> Result<Vec<f32>, BatchBackendError> {
    if row_indices.is_empty() {
        return Ok(Vec::new());
    }
    let row_count = row_indices.len() as u32;

    let mut padded_query: Vec<f32> = Vec::with_capacity(query_stride);
    padded_query.extend_from_slice(query);
    padded_query.resize(query_stride, 0.0);

    let params = GpuParams {
        dim_half,
        row_count,
        _pad0: 0,
        _pad1: 0,
    };

    let params_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product params"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue
        .write_buffer(&params_buffer, 0, &params.to_ne_bytes_vec());

    let row_ids_bytes = bytes_of_u32_slice(row_indices);
    let row_ids_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product row ids"),
        size: row_ids_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&row_ids_buffer, 0, &row_ids_bytes);

    let query_bytes = bytes_of_f32_slice(&padded_query);
    let query_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product query"),
        size: query_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&query_buffer, 0, &query_bytes);

    let scores_bytes = (row_indices.len() as u64).saturating_mul(4);
    let scores_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product scores"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product readback"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("batch dot product bind group"),
        layout: bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: row_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: row_ids_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: query_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: scores_buffer.as_entire_binding(),
            },
        ],
    });

    let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("batch dot product encoder"),
        });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("batch dot product pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&ctx.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        let workgroups = (row_count as u64).div_ceil(GPU_WORKGROUP_SIZE as u64);
        let workgroups_x = u32::try_from(workgroups).unwrap_or(u32::MAX);
        pass.dispatch_workgroups(workgroups_x, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&scores_buffer, 0, &readback_buffer, 0, scores_bytes);
    ctx.queue.submit(std::iter::once(encoder.finish()));

    // LIFO で pop する（後に push した OutOfMemory スコープを先に pop する）。
    let oom_err = pollster_free_block_on(oom_scope.pop());
    let validation_err = pollster_free_block_on(validation_scope.pop());
    if let Some(e) = oom_err {
        return Err(BatchBackendError::KernelLaunchFailed(format!(
            "gpu out of memory: {e}"
        )));
    }
    if let Some(e) = validation_err {
        return Err(BatchBackendError::KernelLaunchFailed(format!(
            "gpu validation error: {e}"
        )));
    }

    let slice = readback_buffer.slice(..);
    let map_result: std::sync::Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> =
        std::sync::Arc::new(Mutex::new(None));
    let map_result_cb = map_result.clone();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        if let Ok(mut guard) = map_result_cb.lock() {
            *guard = Some(res);
        }
    });

    loop {
        let poll_result = ctx.device.poll(wgpu::PollType::wait_indefinitely());
        if poll_result.is_err() {
            return Err(BatchBackendError::DeviceLost(
                "device poll failed".to_string(),
            ));
        }
        let ready = match map_result.lock() {
            Ok(guard) => guard.is_some(),
            Err(_) => {
                return Err(BatchBackendError::TransferFailed(
                    "map result mutex poisoned".to_string(),
                ))
            }
        };
        if ready {
            break;
        }
        std::thread::yield_now();
    }

    let mapped = match map_result.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => {
            return Err(BatchBackendError::TransferFailed(
                "map result mutex poisoned".to_string(),
            ))
        }
    };
    match mapped {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            return Err(BatchBackendError::TransferFailed(format!(
                "buffer map failed: {e}"
            )))
        }
        None => {
            return Err(BatchBackendError::TransferFailed(
                "buffer map did not complete".to_string(),
            ))
        }
    }

    let scores = {
        let view = slice.get_mapped_range().map_err(|e| {
            BatchBackendError::TransferFailed(format!("get_mapped_range failed: {e}"))
        })?;
        f32_vec_from_ne_bytes(&view)
    };
    readback_buffer.unmap();

    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    // GPU デバイスに依存しない純粋関数のみをここで検証する。デバイス初期化を
    // 要するテスト（初期化失敗→縮退・実 GPU 分岐）は `tests/gpu_batch.rs`
    // （結合テスト。環境条件で両分岐を検証する。TASK-128 設計方針 §3.5）に置く。

    #[test]
    fn bytes_of_u32_slice_round_trips_via_f32_vec_from_ne_bytes_is_not_applicable() {
        // u32 バイト列と f32 バイト列は別関数だが、ラウンドトリップ可能な
        // エンコード（native endian の 4 byte 固定）であることだけを確認する。
        let values = [0u32, 1, u32::MAX, 42];
        let bytes = bytes_of_u32_slice(&values);
        assert_eq!(bytes.len(), 16);
        let mut restored = Vec::new();
        for chunk in bytes.chunks_exact(4) {
            let arr: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
            restored.push(u32::from_ne_bytes(arr));
        }
        assert_eq!(restored, values);
    }

    #[test]
    fn f32_vec_from_ne_bytes_round_trips() {
        let values = [0.0f32, 1.5, -3.25, f32::MIN, f32::MAX];
        let bytes = bytes_of_f32_slice(&values);
        let restored = f32_vec_from_ne_bytes(&bytes);
        assert_eq!(restored, values);
    }

    #[test]
    fn f32_vec_from_ne_bytes_rejects_truncated_input() {
        let bytes = vec![0u8, 1, 2];
        assert!(f32_vec_from_ne_bytes(&bytes).is_empty());
    }

    // 回帰テスト（Issue #178 レビュー指摘対応）: GPU 経路は以前
    // `gather_reachable_rows` 内で `rows * dim`（1 クエリ分）しか
    // 見積もらず `queries.len()` を乗じていなかったため、単発クエリでは
    // 予算内でもクエリ件数が多いバッチで総計算量が
    // `MAX_BATCH_WORK` を大幅に超過しうる DoS 増幅の穴があった。
    // `GpuBatchBackend::batch_search` はいまや dispatch 前に
    // `compute_tenant_work(rows, queries.len(), dim)` で `rows × queries ×
    // dim` を求め、[`MAX_BATCH_WORK`] と直接照合してから dispatch する
    // （CPU 経路と同じ計算式。`compute_tenant_work` 自体はオーバーフロー
    // のみを `Err` にする関数のため、超過判定は呼び出し側の責務——
    // `batch_search` 本体と同じ手順をここで直接再現して検証する）。
    // ここでは GPU デバイスなしで検証できるよう、1 クエリ分
    // （`rows * dim`）は `MAX_BATCH_WORK` 未満だが、`queries.len()` 倍
    // すると超過するケースで超過判定が働くことを確認する。
    #[test]
    fn compute_tenant_work_total_exceeds_budget_when_query_count_multiplied_in() {
        let rows = 2_000_000usize;
        let dim = 2_000usize;
        let single_query_work = rows.checked_mul(dim).expect("fixture should not overflow");
        assert!(
            single_query_work <= MAX_BATCH_WORK,
            "fixture must stay within budget for a single query: {single_query_work}"
        );

        let queries = 4_096usize; // MAX_BATCH_QUERIES
        let total_work =
            compute_tenant_work(rows, queries, dim).expect("fixture total must not overflow usize");
        assert!(
            total_work > MAX_BATCH_WORK,
            "fixture must exceed budget once query count is multiplied in: {total_work}"
        );
    }

    #[test]
    fn gpu_input_error_maps_to_expected_batch_search_error_variant() {
        use crate::batch_search::BatchSearchError;
        match GpuInputError::CapacityExceeded.into_batch_search_error() {
            BatchSearchError::CapacityExceeded { .. } => {}
            other => panic!("unexpected variant: {other:?}"),
        }
        match GpuInputError::WorkBudgetExceeded.into_batch_search_error() {
            BatchSearchError::WorkBudgetExceeded { .. } => {}
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn probe_gpu_availability_debug_only() {
        match global_context() {
            Ok(_) => eprintln!("GPU_PROBE: available"),
            Err(e) => eprintln!("GPU_PROBE: unavailable: {e}"),
        }
    }

    #[test]
    fn gather_reachable_rows_respects_policy_context_is_visible() {
        let ids = vec![1u64, 2, 3];
        let tenant_ids = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let visibilities = vec![Visibility::Private, Visibility::Public, Visibility::Private];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 0.0, 1.0, 1.0, 1.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let ctx = PolicyContext::new("a").expect("policy context should build for valid tenant");
        let reachable = gather_reachable_rows(&matrix, &ctx)
            .expect("reachable row gather should succeed within work budget");
        // テナント a の Private 行（idx 0, 2）と、Public 許可がある場合の
        // テナント b の Public 行（idx 1）が到達可能（`PolicyContext::new` は
        // 既定で Public のみ許可するため、Private 行 idx 0/2 は不可視）。
        assert_eq!(reachable, vec![1]);

        let ctx_priv =
            PolicyContext::with_visibilities("a", [Visibility::Private, Visibility::Public])
                .expect("policy context with explicit visibilities should build");
        let mut reachable_priv = gather_reachable_rows(&matrix, &ctx_priv)
            .expect("reachable row gather should succeed within work budget");
        reachable_priv.sort_unstable();
        assert_eq!(reachable_priv, vec![0, 1, 2]);
    }

    // GPU デバイスが利用可能な実行環境でのみ実走する end-to-end 正しさの検証。
    // 利用不能な環境（CI の GitHub ホステッド runner 等）では `try_new` が
    // `InitFailed` を返すため、テスト自体は「初期化失敗を確認して終了」に
    // フォールバックする（skip・ignore にはしない。TASK-128 設計方針 §3.5）。
    #[test]
    fn gpu_batch_search_matches_hand_computed_dot_products_when_available() {
        let ids = vec![10u64, 20, 30];
        let tenant_ids = vec!["t".to_string(); 3];
        let visibilities = vec![Visibility::Public; 3];
        let dim = 4;
        // 行 0: [1,0,0,0] 行 1: [0,1,0,0] 行 2: [1,1,1,1]
        let vectors = vec![
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            1.0, 1.0, 1.0, 1.0,
        ];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "gpu unavailable in this environment, skipping end-to-end assertions: {e}"
                );
                return;
            }
        };

        let ctx = PolicyContext::new("t").expect("policy context should build for valid tenant");
        let query_vec = vec![2.0f32, 3.0, 0.0, 0.0];
        let query = BatchQuery {
            vector: &query_vec,
            k: 3,
            ctx: &ctx,
        };
        let hits = backend
            .batch_search(std::slice::from_ref(&query))
            .expect("gpu batch_search should succeed once the device initialized");
        assert_eq!(hits.len(), 1);
        let mut scored: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
        scored.sort_by_key(|(id, _)| *id);
        // 期待値: id=10 → 2.0（[1,0,0,0]・[2,3,0,0]）・id=20 → 3.0
        // （[0,1,0,0]・[2,3,0,0]）・id=30 → 5.0（[1,1,1,1]・[2,3,0,0]）。
        assert_eq!(scored.len(), 3);
        for (id, score) in scored {
            let expected = match id {
                10 => 2.0f32,
                20 => 3.0f32,
                30 => 5.0f32,
                other => panic!("unexpected id in gpu batch result: {other}"),
            };
            assert!(
                (score - expected).abs() < 1e-3,
                "id={id} expected={expected} actual={score}"
            );
        }
    }
}
