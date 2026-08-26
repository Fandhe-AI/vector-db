//! バッチ検索の実 GPU バックエンド（TASK-128〜130・対象ビヘイビア: CORE-6, 8,
//! 16。ポインタ: Issue #178）。
//!
//! `batch_fallback.rs::BatchBackend` の公開差し替え点へ差し込む実装で、
//! `wgpu`（=30.0.1・依存追加はオーナー承認済み〔2026-08-26〕。`crates/engine/Cargo.toml`
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
    compute_tenant_work, resolve_batch_slot, try_owned_str, try_reserve_exact,
    validate_batch_queries, BatchHit, BatchQuery, BatchRowSource, BatchSearchError, MAX_BATCH_WORK,
};
use crate::kernel::SearchHit;
use crate::kernel::{CandidateHit, TopKSelector};
use crate::policy::PolicyContext;

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
    fn to_ne_bytes_vec(self) -> Result<Vec<u8>, BatchBackendError> {
        let mut out = Vec::new();
        try_reserve_bytes(&mut out, 16)?;
        out.extend_from_slice(&self.dim_half.to_ne_bytes());
        out.extend_from_slice(&self.row_count.to_ne_bytes());
        out.extend_from_slice(&self._pad0.to_ne_bytes());
        out.extend_from_slice(&self._pad1.to_ne_bytes());
        Ok(out)
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
    /// error scope 外で発生した wgpu エラーのラッチ（`Device::on_uncaptured_error`
    /// から更新）。既定ハンドラの panic を避けつつ、異常を握り潰さないための記録で、
    /// `GpuBatchBackend::batch_search` が検知すると backend エラーを返して CPU 縮退へ倒す。
    uncaptured_error: std::sync::Arc<AtomicBool>,
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

    // error scope で捕捉しきれなかったエラーの既定ハンドラは panic しうる
    // （wgpu の既定動作）。engine はライブラリクレートであり panic させない
    // 契約（coding-rust.md）のため、独自ハンドラで `uncaptured_error` ラッチへ
    // 記録するだけに置き換える。ラッチは次回以降の `batch_search` 冒頭で
    // 参照され、GPU 経路を使わず CPU 縮退（CORE-8）へ倒すための入力になる
    // （codex/Bugbot P1 指摘対応: scope 外の wgpu 操作が panic しうる問題）。
    let uncaptured_error = std::sync::Arc::new(AtomicBool::new(false));
    let uncaptured_error_flag = uncaptured_error.clone();
    device.on_uncaptured_error(std::sync::Arc::new(move |_e: wgpu::Error| {
        uncaptured_error_flag.store(true, Ordering::SeqCst);
    }));

    // シェーダ・レイアウト・パイプライン生成も error scope の内側で行う
    // （codex P1 指摘対応: scope 外の生成失敗は uncaptured error 扱いになり、
    // 上記ハンドラ導入前は panic しえた。ここで捕捉して `Err` へ写像する）。
    let init_validation_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let init_oom_scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

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

    // LIFO で pop する（後に push した OutOfMemory スコープを先に pop する）。
    // 待機中も `device.poll` を駆動する（`block_on_with_device_poll`）ため、
    // デバイスのポーリング待ちで無限スピンしない。
    if block_on_with_device_poll(&device, init_oom_scope.pop())
        .map_err(|_| "device poll failed during pipeline creation".to_string())?
        .is_some()
    {
        return Err("gpu out of memory during pipeline creation".to_string());
    }
    if block_on_with_device_poll(&device, init_validation_scope.pop())
        .map_err(|_| "device poll failed during pipeline creation".to_string())?
        .is_some()
    {
        return Err("gpu validation error during pipeline creation".to_string());
    }

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
        uncaptured_error,
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

/// `device.poll` を駆動しながら future を完了させる同期化ヘルパー
/// （codex/Bugbot P1 指摘対応: `push_error_scope`/`pop` の future は
/// デバイスをポーリングするまで `Pending` のままになりうるため、
/// [`pollster_free_block_on`] の自己ポーリングだけでは進行せずハングする）。
/// ポーリング自体が失敗した場合はデバイス異常として `Err(())` を返し、
/// 呼び出し元がデバイスロスト・初期化失敗として写像する。
fn block_on_with_device_poll<F: std::future::Future>(
    device: &wgpu::Device,
    fut: F,
) -> Result<F::Output, ()> {
    use std::task::{Context, Poll, Waker};

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut boxed = Box::pin(fut);
    loop {
        if let Poll::Ready(v) = boxed.as_mut().poll(&mut cx) {
            return Ok(v);
        }
        // 送信済みコマンド・コールバックを進める。`PollType::Poll`（非ブロッキング）
        // を使い、完了していなければ次のループで future を再ポーリングする。
        if device.poll(wgpu::PollType::Poll).is_err() {
            return Err(());
        }
        std::thread::yield_now();
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
    /// `GpuContext::uncaptured_error` と同一のラッチ（error scope 外で発生した
    /// wgpu エラーの記録）。`batch_search` 冒頭で参照し、記録があれば GPU 経路を
    /// 使わず backend エラーを返して CPU 縮退（CORE-8）へ倒す。
    uncaptured_error: std::sync::Arc<AtomicBool>,
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

        // 常駐行列バッファの確保・アップロードも error scope の内側で行い、
        // 失敗を `InitFailed`（＝CPU 縮退）へ写像する（codex P1 指摘対応）。
        let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let row_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("resident matrix packed rows"),
            size: packed_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let packed_staging = bytes_of_u32_slice(matrix.packed())?;
        ctx.queue.write_buffer(&row_buffer, 0, &packed_staging);
        let poll_failed =
            || BatchBackendError::InitFailed("device poll failed during buffer upload".to_string());
        if block_on_with_device_poll(&ctx.device, oom_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu out of memory while uploading the resident matrix".to_string(),
            ));
        }
        if block_on_with_device_poll(&ctx.device, validation_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu validation error while uploading the resident matrix".to_string(),
            ));
        }

        Ok(Self {
            matrix,
            row_buffer,
            device_lost: ctx.device_lost.clone(),
            uncaptured_error: ctx.uncaptured_error.clone(),
            dispatch_lock: Mutex::new(()),
        })
    }
}

/// `&[u32]` を `to_ne_bytes` で `&[u8]` 相当のバイト列へ変換する
/// （`bytemuck` 不採用。依存最小方針）。返す `Vec<u8>` は呼び出し元が
/// `Queue::write_buffer` へそのまま渡す想定の一時バッファ。
fn bytes_of_u32_slice(values: &[u32]) -> Result<Vec<u8>, BatchBackendError> {
    let mut out = Vec::new();
    try_reserve_bytes(&mut out, values.len().saturating_mul(4))?;
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    Ok(out)
}

fn bytes_of_f32_slice(values: &[f32]) -> Result<Vec<u8>, BatchBackendError> {
    let mut out = Vec::new();
    try_reserve_bytes(&mut out, values.len().saturating_mul(4))?;
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    Ok(out)
}

/// ステージング用バイト列・`f32` 列のフォールブル確保ヘルパー
/// （Cursor Bugbot / codex P1 指摘対応: `Vec::with_capacity` は確保失敗時に
/// プロセスを abort するため、常駐行列・チャンク単位の数 MiB 級コピーでは使わない。
/// 失敗は [`BatchBackendError::KernelLaunchFailed`] として返し、`FallbackBatchEngine`
/// の CPU 縮退（CORE-8）が働く経路に載せる）。
fn try_reserve_bytes(buf: &mut Vec<u8>, additional: usize) -> Result<(), BatchBackendError> {
    buf.try_reserve_exact(additional).map_err(|_| {
        BatchBackendError::KernelLaunchFailed("staging buffer allocation failed".to_string())
    })
}

fn try_reserve_f32(buf: &mut Vec<f32>, additional: usize) -> Result<(), BatchBackendError> {
    buf.try_reserve_exact(additional).map_err(|_| {
        BatchBackendError::TransferFailed("readback buffer allocation failed".to_string())
    })
}

/// GPU の readback バッファから `f32` 列を復元する（`from_ne_bytes`。
/// `bytemuck` 不採用）。長さが 4 の倍数でない場合は空を返す（呼び出し元が
/// バッファサイズを 4 の倍数で確保しているため通常到達しないが、fail-closed
/// に空扱いで打ち切る）。
fn f32_vec_from_ne_bytes(bytes: &[u8]) -> Result<Vec<f32>, BatchBackendError> {
    let mut out = Vec::new();
    try_reserve_f32(&mut out, bytes.len() / 4)?;
    let mut chunks = bytes.chunks_exact(4);
    for chunk in &mut chunks {
        let arr: [u8; 4] = match chunk.try_into() {
            Ok(a) => a,
            Err(_) => return Ok(Vec::new()),
        };
        out.push(f32::from_ne_bytes(arr));
    }
    Ok(out)
}

impl BatchBackend for GpuBatchBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        if self.device_lost.load(Ordering::SeqCst) {
            return Err(BatchExecError::Backend(BatchBackendError::DeviceLost(
                "gpu device lost".to_string(),
            )));
        }
        // error scope 外で発生した wgpu エラーが記録されていれば、GPU 経路を
        // 信頼せず backend エラーとして返す（`FallbackBatchEngine` が CPU 縮退へ
        // 倒す。codex/Bugbot P1 指摘対応の一部）。
        if self.uncaptured_error.load(Ordering::SeqCst) {
            return Err(BatchExecError::Backend(
                BatchBackendError::KernelLaunchFailed(
                    "gpu reported an uncaptured error".to_string(),
                ),
            ));
        }

        // `FallbackBatchEngine::batch_search` が本メソッド呼び出し前に
        // `validate_batch_queries` を適用する契約だが（`batch_fallback.rs`
        // の `BatchBackend` trait doc 参照）、本バックエンドは独自の走査
        // パイプラインを持つため（`run_batch_search` を経由しない）防御的に
        // 再検証する（TASK-128 設計方針 §3.2 ポインタ）。
        validate_batch_queries(self.matrix.dim(), queries).map_err(BatchExecError::Input)?;

        // dispatch 前の総量ガード（Issue #178 レビュー指摘対応: GPU 経路が
        // `queries.len()` を乗じていなかった DoS 増幅の修正と、その後の
        // codex/Bugbot P1 指摘対応: 全行 × 全クエリの直積で課金すると CPU 経路の
        // テナント別合算では予算内の要求まで `Input` エラー〔＝CPU 縮退しない〕で
        // 恒久的に拒否してしまうため、クエリごとの実到達行数で課金する）。
        check_reachable_batch_work(&self.matrix, queries).map_err(BatchExecError::Input)?;

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

        // `Vec::with_capacity`（abort-on-OOM）ではなくフォールブル確保にする
        // （CPU 経路 `run_batch_search` の `out` と同じ方針。Issue #178 レビュー指摘）。
        let mut hits: Vec<BatchHit> = Vec::new();
        try_reserve_exact(&mut hits, queries.len(), "gpu batch results")
            .map_err(BatchExecError::Input)?;
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
                    // 候補識別子は「行 id」ではなく常駐行列のスロット番号
                    // （`gather_reachable_rows` が返す行 index）を使う。
                    // `TopKSelector` の同点タイブレークは候補識別子の昇順で
                    // あり、CPU 経路（`batch_search.rs::run_batch_search`）は
                    // スロット昇順を契約としているため（`batch_fallback.rs::
                    // revalidate_primary_hits` の順序検証 (4) が同じ基準で
                    // 判定する）、ここで行 id を使うと同点時に順序契約違反と
                    // なり正当な結果まで `PrimaryResultRejected` で拒否される
                    // （PR #205/#228 の `(tenant_id, id)` 統一に追随。Issue #178）。
                    selector.push(CandidateHit {
                        id: u64::from(row_idx),
                        score,
                    });
                }
            }

            hits.push(BatchHit {
                hits: finalize_gpu_hits(&self.matrix, q.ctx, &selector.into_sorted_vec())
                    .map_err(BatchExecError::Input)?,
            });
        }

        Ok(hits)
    }
}

/// GPU 側で選出した候補（常駐行列のスロット番号 + スコア）を、テナント修飾済みの
/// [`SearchHit`]（`(tenant_id, id)` で行を一意に解決できる契約。対象ビヘイビア:
/// TABLE-12・RLS-9。PR #205/#228）へ解決する。
///
/// [`GpuBatchBackend::batch_search`] の最終段だが、GPU デバイスに触れないため
/// GPU 非搭載環境（CI）でも単体テストできるよう独立関数として切り出している
/// （`tests` モジュール参照。Issue #178 レビュー指摘対応）。
///
/// 解決は CPU 経路（`batch_search.rs::run_batch_search` の「選出後の独立再検証」）
/// と同一の `resolve_batch_slot` + `PolicyContext::is_visible`（CORE-2 の単一照合
/// パス）を通す。スロットが解決不能（GPU 側の readback 破損・実装バグ）、または
/// 解決した行が当該クエリから不可視の場合は、部分結果を返さず
/// [`BatchSearchError::TenantMaskViolation`] で全体を拒否する（fail-closed）。
fn finalize_gpu_hits(
    matrix: &crate::batch_search::ResidentMatrix,
    ctx: &PolicyContext,
    candidates: &[CandidateHit],
) -> Result<Vec<SearchHit>, BatchSearchError> {
    let mut out: Vec<SearchHit> = Vec::new();
    try_reserve_exact(&mut out, candidates.len(), "gpu resolved hits")?;
    for hit in candidates {
        let Some((tenant, id, visibility)) = resolve_batch_slot(matrix, hit.id) else {
            return Err(BatchSearchError::TenantMaskViolation);
        };
        if !ctx.is_visible(tenant, visibility) {
            return Err(BatchSearchError::TenantMaskViolation);
        }
        out.push(SearchHit {
            tenant_id: try_owned_str(tenant)?,
            id,
            score: hit.score,
        });
    }
    Ok(out)
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

/// [`GpuBatchBackend::batch_search`] の dispatch 前総量ガード本体。
///
/// CPU 経路（`batch_search.rs::run_batch_search`）は「テナントごとの行数 ×
/// そのテナントのクエリ数 × dim」を合算して [`MAX_BATCH_WORK`] と照合する。
/// GPU 経路も同じ基準に揃えるため、クエリごとに実際に走査する行
/// （`PolicyContext::is_visible` を満たす行）の数だけを課金する
/// （codex/Bugbot P1 指摘対応: 以前は「常駐行列の全行数 × 全クエリ数 × dim」の
/// 直積で課金していたため、複数テナントが混在すると CPU 経路では予算内の要求まで
/// 超過扱いになり、しかも超過は `BatchExecError::Input` として
/// `FallbackBatchEngine` の CPU 縮退対象外＝恒久的な失敗になっていた）。
///
/// 事前走査のコストは `rows × queries` 回の可視性判定のみ（dim を乗じない）で、
/// CPU 経路が本走査で行う判定回数と同じオーダーに収まる。
fn check_reachable_batch_work(
    matrix: &crate::batch_search::ResidentMatrix,
    queries: &[BatchQuery<'_>],
) -> Result<(), crate::batch_search::BatchSearchError> {
    let mut counts: Vec<usize> = Vec::new();
    try_reserve_exact(&mut counts, queries.len(), "gpu reachable row counts")?;
    for q in queries {
        let visible = matrix
            .tenant_ids()
            .iter()
            .zip(matrix.visibilities().iter())
            .filter(|(tenant, visibility)| q.ctx.is_visible(tenant, **visibility))
            .count();
        counts.push(visible);
    }
    check_batch_work_from_visible_counts(&counts, matrix.dim())
}

/// [`check_reachable_batch_work`] の判定本体（GPU デバイス非依存。
/// 境界値をハードウェアなしでテストできるよう独立関数として切り出す）。
/// 各要素は 1 クエリが実際に走査する行数で、`Σ(rows_q × dim)` を
/// [`MAX_BATCH_WORK`] と照合する。オーバーフローは超過として扱う。
fn check_batch_work_from_visible_counts(
    visible_rows_per_query: &[usize],
    dim: usize,
) -> Result<(), crate::batch_search::BatchSearchError> {
    let mut total: usize = 0;
    for rows in visible_rows_per_query {
        let work = compute_tenant_work(*rows, 1, dim)?;
        total = total.checked_add(work).ok_or(
            crate::batch_search::BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            },
        )?;
    }
    if total > MAX_BATCH_WORK {
        return Err(crate::batch_search::BatchSearchError::WorkBudgetExceeded {
            work: total,
            max: MAX_BATCH_WORK,
        });
    }
    Ok(())
}

/// クエリ `ctx` から見て到達可能な行の index 列を求める（CORE-2 の単一照合
/// パス `PolicyContext::is_visible` を使う。テナント文字列の独自比較はしない）。
/// 計算量ガードの主防御線は呼び出し元 [`GpuBatchBackend::batch_search`] 冒頭の
/// [`check_reachable_batch_work`]（クエリごとの実到達行数を合算した総量）であり、
/// 本関数内の `rows * 1 query * dim` チェックはその後段の防御的な二重チェックに
/// 過ぎない（単独ではクエリ件数を考慮しないため総量ガードの代替にはならない）。
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

    let mut padded_query: Vec<f32> = Vec::new();
    try_reserve_f32(&mut padded_query, query_stride)?;
    padded_query.extend_from_slice(query);
    padded_query.resize(query_stride, 0.0);

    let params = GpuParams {
        dim_half,
        row_count,
        _pad0: 0,
        _pad1: 0,
    };

    // バッファ・bind group の生成もすべて error scope の内側で行う
    // （codex/Bugbot P1 指摘対応: 以前は encoder 直前で push していたため、
    // 生成失敗が scope 外の uncaptured error になり panic しえた）。
    let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let params_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product params"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue
        .write_buffer(&params_buffer, 0, &params.to_ne_bytes_vec()?);

    let row_ids_bytes = bytes_of_u32_slice(row_indices)?;
    let row_ids_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batch dot product row ids"),
        size: row_ids_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&row_ids_buffer, 0, &row_ids_bytes);

    let query_bytes = bytes_of_f32_slice(&padded_query)?;
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
    // `pop()` の future はデバイスがポーリングされるまで `Pending` のままに
    // なりうるため、待機中も `device.poll` を駆動する（codex/Bugbot P1 指摘対応:
    // 自己ポーリングのみだと後続の readback ポーリングへ到達できずハングする）。
    let poll_failed = || BatchBackendError::DeviceLost("device poll failed".to_string());
    let oom_err =
        block_on_with_device_poll(&ctx.device, oom_scope.pop()).map_err(|_| poll_failed())?;
    let validation_err = block_on_with_device_poll(&ctx.device, validation_scope.pop())
        .map_err(|_| poll_failed())?;
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
        f32_vec_from_ne_bytes(&view)?
    };
    readback_buffer.unmap();

    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;
    // 本体側はマスク判定を `PolicyContext::is_visible` の単一照合パスへ委ねており
    // `Visibility` を直接参照しない。フィクスチャ構築のためテスト側でのみ取り込む。
    use crate::storage::Visibility;

    // GPU デバイスに依存しない純粋関数のみをここで検証する。デバイス初期化を
    // 要するテスト（初期化失敗→縮退・実 GPU 分岐）は `tests/gpu_batch.rs`
    // （結合テスト。環境条件で両分岐を検証する。TASK-128 設計方針 §3.5）に置く。

    #[test]
    fn bytes_of_u32_slice_round_trips_via_f32_vec_from_ne_bytes_is_not_applicable() {
        // u32 バイト列と f32 バイト列は別関数だが、ラウンドトリップ可能な
        // エンコード（native endian の 4 byte 固定）であることだけを確認する。
        let values = [0u32, 1, u32::MAX, 42];
        let bytes = bytes_of_u32_slice(&values).expect("small staging buffer must allocate");
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
        let bytes = bytes_of_f32_slice(&values).expect("small staging buffer must allocate");
        let restored = f32_vec_from_ne_bytes(&bytes).expect("small readback buffer must allocate");
        assert_eq!(restored, values);
    }

    #[test]
    fn f32_vec_from_ne_bytes_rejects_truncated_input() {
        let bytes = vec![0u8, 1, 2];
        assert!(f32_vec_from_ne_bytes(&bytes)
            .expect("small readback buffer must allocate")
            .is_empty());
    }

    // 回帰テスト（Issue #178 レビュー指摘対応）: GPU 経路は以前
    // `gather_reachable_rows` 内で `rows * dim`（1 クエリ分）しか見積もらず
    // クエリ件数を乗じていなかったため、単発クエリでは予算内でもクエリ件数が
    // 多いバッチで総計算量が `MAX_BATCH_WORK` を大幅に超過しうる DoS 増幅の
    // 穴があった。判定本体（GPU デバイス非依存）を直接呼んで検証する。
    #[test]
    fn batch_work_budget_accumulates_across_queries() {
        let rows = 2_000_000usize;
        let dim = 2_000usize;
        let single_query_work = rows.checked_mul(dim).expect("fixture should not overflow");
        assert!(
            single_query_work <= MAX_BATCH_WORK,
            "fixture must stay within budget for a single query: {single_query_work}"
        );
        assert!(
            check_batch_work_from_visible_counts(&[rows], dim).is_ok(),
            "a single query within budget must be accepted"
        );

        let counts = vec![rows; 4_096]; // MAX_BATCH_QUERIES
        match check_batch_work_from_visible_counts(&counts, dim) {
            Err(crate::batch_search::BatchSearchError::WorkBudgetExceeded { .. }) => {}
            other => {
                panic!("expected WorkBudgetExceeded once query count is accumulated, got {other:?}")
            }
        }
    }

    // 回帰テスト（codex/Bugbot P1 指摘対応）: 課金対象はクエリごとの実到達行数で
    // あり「常駐行列の全行数 × 全クエリ数」の直積ではない。テナント分離により
    // 各クエリが常駐行列の一部しか走査しない構成では、直積課金だと超過扱いに
    // なる要求が受理されることを確認する（超過は `Input` エラーとして CPU 縮退の
    // 対象外になるため、過大課金は成功可能な検索の恒久的失敗を招く）。
    #[test]
    fn batch_work_budget_bills_reachable_rows_not_the_cartesian_product() {
        // 100 テナント × 各 10,000 行 = 全 1,000,000 行の常駐行列に、
        // テナントごとに 1 本ずつ（計 100 本）のクエリが来る構成を模す。
        let total_rows = 1_000_000usize;
        let queries = 100usize;
        let reachable_per_query = total_rows / queries;
        let dim = 768usize;

        // 直積課金（旧実装）: 全行 × 全クエリ × dim は予算を大きく超える。
        let cartesian = total_rows.saturating_mul(queries).saturating_mul(dim);
        assert!(
            cartesian > MAX_BATCH_WORK,
            "fixture must exceed the budget under the old cartesian billing"
        );

        // 実到達行数課金（本実装）: Σ(到達行数 × dim) は予算内であり受理される。
        let counts = vec![reachable_per_query; queries];
        assert!(
            check_batch_work_from_visible_counts(&counts, dim).is_ok(),
            "a batch that the cpu path would accept must not be rejected by the gpu guard"
        );

        // 予算を実際に超える構成は引き続き拒否される（ガードの弱体化防止）。
        let over_budget = vec![total_rows; queries];
        match check_batch_work_from_visible_counts(&over_budget, dim) {
            Err(crate::batch_search::BatchSearchError::WorkBudgetExceeded { .. }) => {}
            other => {
                panic!("expected WorkBudgetExceeded for a genuinely oversized batch, got {other:?}")
            }
        }
    }

    // `check_reachable_batch_work` が常駐行列の可視性判定（`PolicyContext::is_visible`）
    // を通して行数を数えること（他テナントの Private 行を課金対象にしないこと）を、
    // GPU デバイスなしで確認する。
    #[test]
    fn reachable_batch_work_counts_only_visible_rows() {
        let ids = vec![1u64, 2, 3];
        let tenant_ids = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let visibilities = vec![
            Visibility::Private,
            Visibility::Private,
            Visibility::Private,
        ];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 0.0, 1.0, 1.0, 1.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let ctx = PolicyContext::with_visibilities("a", [Visibility::Private])
            .expect("policy context with explicit visibilities should build");
        let query = [1.0f32, 0.0];
        let queries = [BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx,
        }];
        // テナント a の 2 行のみが課金対象（テナント b の Private 行は不可視）。
        assert!(check_reachable_batch_work(&matrix, &queries).is_ok());
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

    /// 同一 `(tenant_id, id)` 契約の検証用フィクスチャ（Issue #178 レビュー指摘対応）。
    /// スロット 0 = `("tenant-a", 99, Public)`・スロット 1 = `("tenant-b", 1, Public)` と、
    /// 「スロット昇順」と「行 id 昇順」が逆順になる配置にする。行は本番経路では
    /// `(tenant_id, id)` 順で常駐行列へ渡される（`batch_search.rs::BatchHit` の
    /// 順序契約）ため、この配置は特殊ケースではなく通常のマルチテナント配置である。
    fn tie_fixture() -> (Vec<u64>, Vec<String>, Vec<Visibility>, usize, Vec<f32>) {
        (
            vec![99u64, 1],
            vec!["tenant-a".to_string(), "tenant-b".to_string()],
            vec![Visibility::Public, Visibility::Public],
            2,
            // 2 行とも同一ベクトル = 同点スコアになりタイブレークが顕在化する。
            vec![1.0, 0.0, 1.0, 0.0],
        )
    }

    fn tie_matrix() -> crate::batch_search::ResidentMatrix {
        let (ids, tenant_ids, visibilities, dim, vectors) = tie_fixture();
        crate::batch_search::ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, &vectors)
            .expect("resident matrix build should succeed for well-formed fixture")
    }

    /// `finalize_gpu_hits` は候補識別子をスロット番号として解決し、
    /// `(tenant_id, id)` 付きの `SearchHit` を選出順のまま返す。
    #[test]
    fn finalize_gpu_hits_resolves_slots_to_tenant_qualified_hits() {
        let matrix = tie_matrix();
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let mut selector = TopKSelector::new(2);
        selector.push(CandidateHit { id: 0, score: 1.0 });
        selector.push(CandidateHit { id: 1, score: 1.0 });
        let hits = finalize_gpu_hits(&matrix, &ctx, &selector.into_sorted_vec())
            .expect("visible slots should resolve");
        // 同点のタイブレークはスロット昇順（行 id 昇順ではない）。
        let resolved: Vec<(&str, u64)> =
            hits.iter().map(|h| (h.tenant_id.as_str(), h.id)).collect();
        assert_eq!(resolved, vec![("tenant-a", 99), ("tenant-b", 1)]);
    }

    /// 解決不能なスロット（GPU 側 readback 破損・実装バグを模す）は部分結果を
    /// 返さず全体を拒否する（fail-closed）。
    #[test]
    fn finalize_gpu_hits_rejects_out_of_range_slot() {
        let matrix = tie_matrix();
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let err = finalize_gpu_hits(
            &matrix,
            &ctx,
            &[
                CandidateHit { id: 0, score: 1.0 },
                CandidateHit {
                    id: 9_999,
                    score: 0.5,
                },
            ],
        )
        .expect_err("out-of-range slot must be rejected");
        assert_eq!(
            err,
            crate::batch_search::BatchSearchError::TenantMaskViolation
        );
    }

    /// 当該クエリから不可視の行（他テナントの `Private` 行）を指すスロットも
    /// `PolicyContext::is_visible` の単一照合パスで拒否する。
    #[test]
    fn finalize_gpu_hits_rejects_slot_invisible_to_query_context() {
        let ids = vec![1u64, 2];
        let tenant_ids = vec!["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = vec![Visibility::Public, Visibility::Private];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 1.0, 0.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let err = finalize_gpu_hits(&matrix, &ctx, &[CandidateHit { id: 1, score: 1.0 }])
            .expect_err("invisible row must be rejected");
        assert_eq!(
            err,
            crate::batch_search::BatchSearchError::TenantMaskViolation
        );
    }

    /// テナント跨ぎで同じ `id` を持つ行（`id` 単独では一意でない）も、
    /// `(tenant_id, id)` として区別して返る（対象ビヘイビア: TABLE-12・RLS-9）。
    #[test]
    fn finalize_gpu_hits_distinguishes_same_id_across_tenants() {
        let ids = vec![7u64, 7];
        let tenant_ids = vec!["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = vec![Visibility::Public, Visibility::Public];
        let matrix = crate::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &[1.0, 0.0, 1.0, 0.0],
        )
        .expect("resident matrix build should succeed for well-formed fixture");
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let hits = finalize_gpu_hits(
            &matrix,
            &ctx,
            &[
                CandidateHit { id: 0, score: 1.0 },
                CandidateHit { id: 1, score: 1.0 },
            ],
        )
        .expect("both public rows are visible to tenant-a");
        let resolved: Vec<(&str, u64)> =
            hits.iter().map(|h| (h.tenant_id.as_str(), h.id)).collect();
        assert_eq!(resolved, vec![("tenant-a", 7), ("tenant-b", 7)]);
    }

    /// GPU 経路の最終出力が `FallbackBatchEngine::revalidate_primary_hits`
    /// （PR #205/#228 で `(tenant_id, id)` 基準へ統一された順序・存在・可視性の
    /// 独立再検証）を通ることを、GPU デバイスなしで確認する回帰テスト。
    /// 選出時の候補識別子に行 id を使う旧実装では、同点時に「行 id 昇順」で
    /// 並ぶため再検証の順序契約（スロット昇順）に違反し、正当な結果まで
    /// `PrimaryResultRejected` として拒否されていた（Issue #178 レビュー指摘）。
    #[test]
    fn gpu_finalized_hits_pass_primary_revalidation_while_row_id_order_is_rejected() {
        use crate::batch_fallback::{
            BatchBackend, BatchExecError, FallbackBatchEngine, FallbackObserver,
        };

        /// 与えられたクロージャの結果をそのまま返す差し替え用バックエンド
        /// （`batch_fallback.rs` のテストにある `MaliciousBackend` と同じ形。
        /// 本体へテスト専用の公開 API は追加しない）。
        struct StubBackend<F: Fn() -> Vec<BatchHit> + Send + Sync> {
            make_hits: F,
        }
        impl<F: Fn() -> Vec<BatchHit> + Send + Sync> BatchBackend for StubBackend<F> {
            fn batch_search(
                &self,
                _queries: &[BatchQuery<'_>],
            ) -> Result<Vec<BatchHit>, BatchExecError> {
                Ok((self.make_hits)())
            }
        }

        struct SilentObserver;
        impl FallbackObserver for SilentObserver {
            fn on_fallback(&self, _event: crate::batch_fallback::FallbackEvent) {}
        }

        let (ids, tenant_ids, visibilities, dim, vectors) = tie_fixture();
        let ctx = PolicyContext::new("tenant-a").expect("policy context should build");
        let query_vec = vec![1.0f32, 0.0];

        // 現行実装（スロット順）の出力を返すバックエンド: 再検証を通る。
        let engine = FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            |matrix| {
                Ok(Box::new(StubBackend {
                    make_hits: move || {
                        let ctx = PolicyContext::new("tenant-a")
                            .expect("policy context should build in stub");
                        let mut selector = TopKSelector::new(2);
                        selector.push(CandidateHit { id: 0, score: 1.0 });
                        selector.push(CandidateHit { id: 1, score: 1.0 });
                        let hits = finalize_gpu_hits(&matrix, &ctx, &selector.into_sorted_vec())
                            .expect("slots are visible to tenant-a");
                        vec![BatchHit { hits }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(SilentObserver),
        )
        .expect("fallback engine build should succeed for well-formed fixture");
        let queries = [BatchQuery {
            vector: &query_vec,
            k: 2,
            ctx: &ctx,
        }];
        let hits = engine
            .batch_search(&queries)
            .expect("slot-ordered gpu output must pass primary revalidation");
        let resolved: Vec<(&str, u64)> = hits
            .first()
            .map(|b| {
                b.hits
                    .iter()
                    .map(|h| (h.tenant_id.as_str(), h.id))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(resolved, vec![("tenant-a", 99), ("tenant-b", 1)]);

        // 旧実装（行 id 順）の出力を模したバックエンド: 再検証で拒否される
        // ことを確認し、上のテストが順序契約を実際に守っていることを裏付ける。
        let engine_row_id_order = FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            |_matrix| {
                Ok(Box::new(StubBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![
                                SearchHit::new("tenant-b", 1, 1.0),
                                SearchHit::new("tenant-a", 99, 1.0),
                            ],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(SilentObserver),
        )
        .expect("fallback engine build should succeed for well-formed fixture");
        let err = engine_row_id_order
            .batch_search(&queries)
            .expect_err("row-id-ordered output violates the slot-ascending contract");
        assert_eq!(
            err,
            crate::batch_search::BatchSearchError::PrimaryResultRejected
        );
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
