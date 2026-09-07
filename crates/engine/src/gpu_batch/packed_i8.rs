//! i8 パック常駐と `dot4I8Packed` シェーダによる opt-in GPU バッチ候補生成
//! バックエンド（Issue #542・親 #541・Phase 5 親 #460・ルート #455）。
//!
//! [`super::GpuBatchBackend`]（f16 2 要素/u32 パック常駐）の行データ移動量を
//! さらに半減させる（1 byte/要素）ため、次元別対称 SQ8 量子化で行を i8 へ
//! 落とし 4 要素/u32 でパックし、WGSL 組み込み `dot4I8Packed` で整数内積を
//! 計算する。親 Issue #541 の制約により**候補生成専用**（opt-in・最終スコアは
//! 必ず f32 再計算）とし、[`super::FallbackBatchEngine::build_with_gpu`] の
//! primary へは接続しない（CORE-12「経路を外部から上書きする機構を作らない」
//! と整合。構築は [`GpuI8BatchBackend::try_new`] からのみ）。
//!
//! # 依存 #521 の実装状況（着手前の確認事項）
//!
//! 2026-09-07 時点で #521（対称 SQ8 量子化・次元別 min/max）は OPEN・実装
//! 未着手（`crates/engine/src/` に `Sq8`/`sq8`/量子化のトップレベル共有
//! モジュールが存在しない）。そのため本モジュールは対称 SQ8 エンコーダを
//! 自前で持つ（[`encode_rows`]／[`Sq8DimParams`]）。#521 マージ後は
//! そちらの型へ統合する（`f16.rs` の前例に倣い、共有層の所有は #521 側に
//! 委ねる。詳細は `docs/design/gpu-batch-i8-packed.md` 参照）。
//!
//! # スコア契約（D8: 候補生成のみ・最終スコアは f32 再計算）
//!
//! GPU は i32 整数内積で候補を粗く選び、ホストが `super::ResidentMatrix`
//! （f16 常駐。既存 [`super::GpuBatchBackend`] と同じ真値）から f32 を復号
//! して `kernel::dot` で再計算した値だけを最終スコアとして返す。真値は常に
//! f16 復号 f32 dot であり、i8 量子化誤差は順位の粗い絞り込みにしか影響
//! しない。
//!
//! # panic を作らない設計
//!
//! `unwrap`/`expect`/添字アクセスは使わない（[`super`] モジュール冒頭コメント
//! と同じ契約）。新規 `unsafe` は追加しない（`isa.rs` 局在の既存契約を維持）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::batch_fallback::{BatchBackend, BatchBackendError, BatchExecError};
use crate::batch_search::{try_reserve_exact, BatchHit, BatchQuery, ResidentMatrix};
use crate::kernel::{dot, CandidateHit, TopKSelector};

use super::{
    block_on_with_device_poll, bytes_of_u32_slice, check_reachable_batch_work,
    create_topk_pipeline, finalize_gpu_hits, gather_reachable_rows, global_context,
    gpu_dispatch_lock, group_queries_by_ctx, validate_batch_queries, wait_and_read_buffer,
    GpuContext, GpuParams,
};

/// クランプ後の量子化値の絶対値上限（D3: `-128` は使わない。対称範囲で
/// `[-127, 127]` に収め、極値の掛け算がオーバーフロー計算を単純にする）。
const I8_CLAMP_ABS: f32 = 127.0;

/// [`GpuI8Options::oversample`] の既定値（D9）。実装既定値であり spec 由来の
/// 数値ではない。調整は #523／#543 の実測後の申し送り。
pub const DEFAULT_I8_OVERSAMPLE: usize = 4;
/// [`GpuI8Options::oversample`] の上限（D9・CORE-12: 候補選出量を有界にする
/// ための実装既定値）。
pub const MAX_I8_OVERSAMPLE: usize = 32;

/// `MAX_BATCH_DIM`（8,192）× 127 × 127 の理論上限（D6）。i32 に厳密に収まる
/// ことを単体テストで固定する。
#[cfg(test)]
const MAX_I8_DOT_MAGNITUDE: i64 = 8_192 * 127 * 127;

/// WGSL: i8 パック常駐（4 要素/u32・[`pack_i8x4`] と同一のレーン順規約）に
/// 対する 1 クエリ分の整数内積を `dot4I8Packed` で計算する（Issue #542）。
///
/// [`super::DOT_SHADER_WGSL`]（f16 パック・複数クエリタイル）と異なり、本
/// シェーダは 1 dispatch = 1 クエリに単純化している（D11 の申し送り: i8 用
/// workgroup 内 Top-k・複数クエリタイル化は本 Issue の対象外）。bind group
/// layout（バインディング構成・型）は [`super::DOT_SHADER_WGSL`] と同一のため
/// `GpuContext::bind_group_layout` を共用できる（WGSL の要素型 `array<u32>`
/// vs `array<f32>`/`array<i32>` は wgpu のバインドグループレイアウト検証には
/// 現れない。`DOT_SHADER_F32_WGSL` のドキュメンテーションコメントと同じ理由）。
///
/// `params.row_stride`/`params.query_stride` は「1 行・1 クエリあたりの
/// `array<u32>` 要素数」= `dim.div_ceil(4)`（[`row_stride_for_dim`]）。
/// `params.query_count` は本シェーダでは常に 1 だが、`Params` の形は
/// [`super::GpuParams`] と揃えてある（bind group layout 共用のため）。
const DOT_SHADER_I8_WGSL: &str = r#"
struct Params {
    row_stride: u32,
    row_count: u32,
    query_count: u32,
    query_stride: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> packed_rows: array<u32>;
@group(0) @binding(2) var<storage, read> row_ids: array<u32>;
@group(0) @binding(3) var<storage, read> query: array<u32>;
@group(0) @binding(4) var<storage, read_write> scores: array<i32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.row_count) {
        return;
    }
    let row = row_ids[i];
    let row_base = row * params.row_stride;

    var acc: i32 = 0;
    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        acc = acc + dot4I8Packed(packed_rows[row_base + j], query[j]);
        j = j + 1u;
    }

    scores[i] = acc;
}
"#;

/// 1 行・1 クエリあたりの `array<u32>` 要素数（[`DOT_SHADER_I8_WGSL`] の
/// `row_stride`/`query_stride`。4 レーン/u32・末尾パディングは 0 詰め）。
fn row_stride_for_dim(dim: usize) -> usize {
    dim.div_ceil(4)
}

/// i8 パック常駐の次元別対称量子化パラメータ（D3。#521 マージ後に共有層へ
/// 統合予定。`docs/design/gpu-batch-i8-packed.md` 参照）。次元 `i` ごとに
/// `center_i = (min_i + max_i) / 2`・`alpha_i = (max_i - min_i) / 254` を
/// 持つ。`alpha_i == 0`（定数次元）は除算せず量子化値を常に 0 とする。
#[derive(Debug, Clone)]
pub struct Sq8DimParams {
    center: Vec<f32>,
    alpha: Vec<f32>,
}

impl Sq8DimParams {
    pub fn dim(&self) -> usize {
        self.center.len()
    }
}

/// [`encode_rows`]/[`quantize_query`] の失敗理由。テナント情報を含まない
/// （英語メッセージ・security.md 準拠）。
#[derive(Debug, Clone, PartialEq)]
pub enum I8EncodeError {
    /// 次元 0、または `vectors.len() != row_count * dim`。
    InvalidShape,
    /// 非有限（NaN/Inf）な入力値を検出した（fail-closed。量子化しない）。
    NonFinite,
    /// フォールブル確保の失敗（`try_reserve_exact` 相当）。
    AllocationFailed,
}

impl std::fmt::Display for I8EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            I8EncodeError::InvalidShape => write!(f, "i8 encode: invalid shape"),
            I8EncodeError::NonFinite => write!(f, "i8 encode: non-finite input"),
            I8EncodeError::AllocationFailed => write!(f, "i8 encode: allocation failed"),
        }
    }
}

/// `[i8; 4]` を 1 u32 へパックする（D4: レーン `j` を bit `8j..8j+7` へ、
/// 下位から詰める規約。`dot4I8Packed` の成分順・既存
/// `batch_search.rs::pack_f16x2` の「下位から」規約と一致）。
fn pack_i8x4(lanes: [i8; 4]) -> u32 {
    let mut out: u32 = 0;
    for (j, &lane) in lanes.iter().enumerate() {
        let byte = lane as u8 as u32;
        out |= byte << (8 * j);
    }
    out
}

/// [`pack_i8x4`] の逆写像（往復・単体テスト用）。
#[cfg(test)]
fn unpack_i8x4(packed: u32) -> [i8; 4] {
    let mut out = [0i8; 4];
    for (j, slot) in out.iter_mut().enumerate() {
        *slot = ((packed >> (8 * j)) & 0xFF) as u8 as i8;
    }
    out
}

/// 次元別対称量子化（D3）。`vectors` は `row_count * dim` 要素（行優先・f32）。
/// 戻り値は `(params, packed)` で、`packed` は `row_count * row_stride_for_dim(dim)`
/// 要素の行優先パック済み `u32` 列。
pub fn encode_rows(
    dim: usize,
    row_count: usize,
    vectors: &[f32],
) -> Result<(Sq8DimParams, Vec<u32>), I8EncodeError> {
    if dim == 0 {
        return Err(I8EncodeError::InvalidShape);
    }
    let expected_len = row_count
        .checked_mul(dim)
        .ok_or(I8EncodeError::InvalidShape)?;
    if vectors.len() != expected_len {
        return Err(I8EncodeError::InvalidShape);
    }
    for &v in vectors {
        if !v.is_finite() {
            return Err(I8EncodeError::NonFinite);
        }
    }

    let mut min_v: Vec<f32> = Vec::new();
    let mut max_v: Vec<f32> = Vec::new();
    min_v
        .try_reserve_exact(dim)
        .map_err(|_| I8EncodeError::AllocationFailed)?;
    max_v
        .try_reserve_exact(dim)
        .map_err(|_| I8EncodeError::AllocationFailed)?;
    min_v.resize(dim, f32::INFINITY);
    max_v.resize(dim, f32::NEG_INFINITY);

    for row in vectors.chunks(dim) {
        for (d, &v) in row.iter().enumerate() {
            if let (Some(mn), Some(mx)) = (min_v.get_mut(d), max_v.get_mut(d)) {
                if v < *mn {
                    *mn = v;
                }
                if v > *mx {
                    *mx = v;
                }
            }
        }
    }

    let mut center: Vec<f32> = Vec::new();
    let mut alpha: Vec<f32> = Vec::new();
    center
        .try_reserve_exact(dim)
        .map_err(|_| I8EncodeError::AllocationFailed)?;
    alpha
        .try_reserve_exact(dim)
        .map_err(|_| I8EncodeError::AllocationFailed)?;
    for d in 0..dim {
        let mn = min_v.get(d).copied().unwrap_or(0.0);
        let mx = max_v.get(d).copied().unwrap_or(0.0);
        center.push((mn + mx) / 2.0);
        alpha.push((mx - mn) / 254.0);
    }
    let params = Sq8DimParams { center, alpha };

    let row_stride = row_stride_for_dim(dim);
    let packed_len = row_count
        .checked_mul(row_stride)
        .ok_or(I8EncodeError::AllocationFailed)?;
    let mut packed: Vec<u32> = Vec::new();
    packed
        .try_reserve_exact(packed_len)
        .map_err(|_| I8EncodeError::AllocationFailed)?;

    for row in vectors.chunks(dim) {
        let mut d = 0usize;
        while d < row_stride.saturating_mul(4) {
            let mut lanes = [0i8; 4];
            for (j, lane) in lanes.iter_mut().enumerate() {
                let idx = d + j;
                let Some(&v) = row.get(idx) else {
                    continue;
                };
                let Some(&c) = params.center.get(idx) else {
                    continue;
                };
                let Some(&a) = params.alpha.get(idx) else {
                    continue;
                };
                *lane = quantize_scalar(v, c, a);
            }
            packed.push(pack_i8x4(lanes));
            d += 4;
        }
    }

    Ok((params, packed))
}

/// 単一のスカラー値を次元別中心・スケールで量子化し `[-127, 127]` へ
/// クランプする（D3。`f32::round` = half away from zero で固定。
/// `alpha == 0`（定数次元）は除算せず 0 を返す）。
fn quantize_scalar(v: f32, center: f32, alpha: f32) -> i8 {
    if alpha == 0.0 {
        return 0;
    }
    let raw = ((v - center) / alpha).round();
    let clamped = raw.clamp(-I8_CLAMP_ABS, I8_CLAMP_ABS);
    clamped as i8
}

/// クエリ側の量子化（D5）。次元別スケールを畳み込んだうえで単一のグローバル
/// スケール `s_q` へ再量子化する: `q'_i = q_i * alpha_i`、
/// `s_q = max_i(|q'_i|) / 127`、`qq_i = round(q'_i / s_q)` を `[-127,127]`
/// へクランプ（`s_q == 0` なら全 0）。
///
/// `dot(q, x) = Σ q_i·center_i + Σ q'_i·xq_i` のうち、第 1 項
/// `Σ q_i·center_i` は行 `x` に依存しない（`center` は全行共通の次元別
/// パラメータ）ため、バッチ内の候補順位付けには寄与しない定数である。
/// 本関数はこの定数項を計算せず（順位付けに不要）、整数内積
/// `Σ qq_i·xq_i` だけで候補を選べるようにする（GPU 側の計算を単純化する
/// 設計判断。最終スコアは常に D8 の f32 再計算が担うため、この定数項の省略が
/// 最終スコアの正しさに影響することはない）。
pub fn quantize_query(params: &Sq8DimParams, query: &[f32]) -> Result<Vec<u32>, I8EncodeError> {
    let dim = params.dim();
    if dim == 0 || query.len() != dim {
        return Err(I8EncodeError::InvalidShape);
    }
    for &v in query {
        if !v.is_finite() {
            return Err(I8EncodeError::NonFinite);
        }
    }

    let mut folded: Vec<f32> = Vec::new();
    folded
        .try_reserve_exact(dim)
        .map_err(|_| I8EncodeError::AllocationFailed)?;
    let mut max_abs = 0.0f32;
    for (d, &q) in query.iter().enumerate() {
        let a = params.alpha.get(d).copied().unwrap_or(0.0);
        let folded_v = q * a;
        if folded_v.abs() > max_abs {
            max_abs = folded_v.abs();
        }
        folded.push(folded_v);
    }

    let s_q = if max_abs == 0.0 {
        0.0
    } else {
        max_abs / I8_CLAMP_ABS
    };

    let row_stride = row_stride_for_dim(dim);
    let mut packed: Vec<u32> = Vec::new();
    packed
        .try_reserve_exact(row_stride)
        .map_err(|_| I8EncodeError::AllocationFailed)?;

    let mut d = 0usize;
    while d < row_stride.saturating_mul(4) {
        let mut lanes = [0i8; 4];
        for (j, lane) in lanes.iter_mut().enumerate() {
            let idx = d + j;
            let Some(&fv) = folded.get(idx) else {
                continue;
            };
            *lane = if s_q == 0.0 {
                0
            } else {
                (fv / s_q).round().clamp(-I8_CLAMP_ABS, I8_CLAMP_ABS) as i8
            };
        }
        packed.push(pack_i8x4(lanes));
        d += 4;
    }

    Ok(packed)
}

/// CPU 参照実装（D7）: [`encode_rows`]/[`quantize_query`] が生成したパック
/// 済み行・クエリの整数内積を計算する。`get()` のみを使い添字アクセスは
/// しない。レーン展開・i32 累積は GPU シェーダ（[`DOT_SHADER_I8_WGSL`]）と
/// 同一の演算順（次元昇順の逐次加算）を踏む。#522（VNNI 実装）はこの参照
/// 実装とビット同一であることを契約とする（Issue #542 実装計画ポインタ）。
pub fn dot_i8_packed_ref(row: &[u32], query: &[u32]) -> i32 {
    let mut acc: i64 = 0;
    let len = row.len().min(query.len());
    for j in 0..len {
        let (Some(&r), Some(&q)) = (row.get(j), query.get(j)) else {
            continue;
        };
        let r_lanes = unpack_i8x4_i32(r);
        let q_lanes = unpack_i8x4_i32(q);
        for lane in 0..4 {
            acc += i64::from(r_lanes[lane]) * i64::from(q_lanes[lane]);
        }
    }
    // D6: `MAX_BATCH_DIM(8,192) × 127 × 127 < 2^31` を実装契約として単体テストで
    // 固定しているため、i32 への切り詰めは正常系では常に無損失。防御的に
    // `clamp` で二重に範囲内へ収める（fail-closed。GPU 側 WGSL `i32` 演算も
    // 同じ理論上限の範囲内で動く）。
    acc.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn unpack_i8x4_i32(packed: u32) -> [i32; 4] {
    let mut out = [0i32; 4];
    for (j, slot) in out.iter_mut().enumerate() {
        let byte = ((packed >> (8 * j)) & 0xFF) as u8;
        *slot = i32::from(byte as i8);
    }
    out
}

/// [`GpuI8BatchBackend::try_new`] のオプション（D9・CORE-12: 環境変数では
/// 読まず、コンストラクタ引数のみで受ける）。
#[derive(Debug, Clone, Copy)]
pub struct GpuI8Options {
    /// 候補生成の広さ: `k' = min(reachable_rows, k * oversample)` 件を
    /// i32 スコア降順で GPU 側から選び、f32 再スコア後に上位 `k` 件へ
    /// 切り詰める。`0` または [`MAX_I8_OVERSAMPLE`] 超は
    /// [`BatchBackendError::InitFailed`]。
    pub oversample: usize,
}

impl Default for GpuI8Options {
    fn default() -> Self {
        Self {
            oversample: DEFAULT_I8_OVERSAMPLE,
        }
    }
}

/// 専用整数内積命令（`OpSDot`/`dot4add_i8packed`/`packed_char4`）と polyfill
/// のどちらが選ばれたかの判別結果（D12）。
///
/// wgpu 30.0.1 の公開 API では判別できない（`wgpu-types` に該当 feature 定数が
/// 無く、判別に使える唯一の経路 `Adapter::as_hal` は `unsafe` かつ本リポの
/// `unsafe` 原則禁止に反する。`docs/design/gpu-batch-i8-packed.md`「D12」節
/// 参照）ため、本実装では常に [`Dot4I8Impl::Undetermined`] を返す。将来 wgpu が
/// 判別可能な feature を公開した場合の実装は申し送りとする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot4I8Impl {
    Native,
    Polyfill,
    Undetermined,
}

/// [`GpuI8BatchBackend::meta`] が返す観測専用メタ情報（D12）。テナント・
/// 行数等は含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuI8Meta {
    pub backend: wgpu::Backend,
    pub dot4_impl: Dot4I8Impl,
}

/// [`GpuI8BatchBackend::stats`] が返す観測専用カウンタ（D13）。
#[derive(Debug, Default)]
struct GpuI8Stats {
    dispatches: AtomicU64,
    readback_bytes: AtomicU64,
    rescored_candidates: AtomicU64,
}

impl GpuI8Stats {
    fn snapshot(&self) -> GpuI8BatchStatsSnapshot {
        GpuI8BatchStatsSnapshot {
            dispatches: self.dispatches.load(Ordering::Relaxed),
            readback_bytes: self.readback_bytes.load(Ordering::Relaxed),
            rescored_candidates: self.rescored_candidates.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuI8BatchStatsSnapshot {
    pub dispatches: u64,
    pub readback_bytes: u64,
    pub rescored_candidates: u64,
}

/// i8 パック常駐の compute pipeline をプロセス内で 1 回だけ遅延初期化する
/// （[`super::f32_contrast_pipeline`] と同方針。生成失敗もキャッシュし
/// 再試行しない）。bind group layout は本番経路と共用する
/// （[`DOT_SHADER_I8_WGSL`] ドキュメンテーションコメント参照）。
fn i8_pipeline() -> Result<&'static wgpu::ComputePipeline, String> {
    static PIPELINE: OnceLock<Result<wgpu::ComputePipeline, String>> = OnceLock::new();
    let ctx = global_context().as_ref().map_err(String::clone)?;
    let result = PIPELINE.get_or_init(|| {
        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        create_topk_pipeline(
            &ctx.device,
            &ctx.bind_group_layout,
            DOT_SHADER_I8_WGSL,
            "i8",
        )
    });
    result.as_ref().map_err(String::clone)
}

/// i8 パック常駐・`dot4I8Packed` による opt-in GPU 候補生成バックエンド
/// （Issue #542）。[`BatchBackend`] を実装するが、`FallbackBatchEngine` の
/// primary へは接続しない（モジュール冒頭コメント参照）。
pub struct GpuI8BatchBackend {
    matrix: ResidentMatrix,
    params: Sq8DimParams,
    row_buffer: wgpu::Buffer,
    row_stride: usize,
    options: GpuI8Options,
    device_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    uncaptured_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stats: std::sync::Arc<GpuI8Stats>,
    meta: GpuI8Meta,
}

impl GpuI8BatchBackend {
    /// 常駐行列から i8 パック常駐バックエンドを構築する。量子化パラメータ
    /// （次元別 min/max）は `matrix` 自身が保持する全行を
    /// [`ResidentMatrix::row_f32_into`]（`pub(crate)`。Issue #542 で公開）
    /// 経由で f16 往復デコードした値から導出する——別引数で元の f32
    /// ベクトル列を受け取る設計にすると、呼び出し元が渡す `vectors` の行順が
    /// `matrix` のスロット順と食い違った場合にスロット不整合を起こしうる
    /// ため、`matrix` を単一の真値にする（D8 の f32 再スコアが参照する真値
    /// とも一致させる。「読んでから決める」設計判断）。
    pub fn try_new(
        matrix: ResidentMatrix,
        options: GpuI8Options,
    ) -> Result<Self, BatchBackendError> {
        if options.oversample == 0 || options.oversample > MAX_I8_OVERSAMPLE {
            return Err(BatchBackendError::InitFailed(
                "i8 oversample out of range".to_string(),
            ));
        }

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => return Err(BatchBackendError::InitFailed(msg.clone())),
        };
        i8_pipeline().map_err(BatchBackendError::InitFailed)?;

        let dim = matrix.dim();
        let row_count = matrix.row_count();
        if dim == 0 || row_count == 0 {
            return Err(BatchBackendError::InitFailed(
                "resident matrix is empty".to_string(),
            ));
        }

        // 量子化入力: 全行を f16 往復デコードして f32 行列を作る（一時
        // バッファ。D8 の真値〔f16 常駐〕とも一致させるための構築時コスト）。
        let mut decoded: Vec<f32> = Vec::new();
        decoded
            .try_reserve_exact(row_count.saturating_mul(dim))
            .map_err(|_| {
                BatchBackendError::InitFailed("i8 decode buffer allocation failed".to_string())
            })?;
        let mut row_buf: Vec<f32> = Vec::new();
        for idx in 0..row_count {
            if matrix.row_f32_into(idx, &mut row_buf).is_none() {
                return Err(BatchBackendError::InitFailed(
                    "resident matrix row decode failed".to_string(),
                ));
            }
            decoded.extend_from_slice(&row_buf);
        }

        let (params, packed) = encode_rows(dim, row_count, &decoded)
            .map_err(|e| BatchBackendError::InitFailed(format!("i8 encode failed: {e}")))?;
        let row_stride = row_stride_for_dim(dim);

        let packed_bytes = packed
            .len()
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| {
                BatchBackendError::InitFailed("i8 packed matrix byte size overflow".to_string())
            })?;
        if packed_bytes as u64 > ctx.max_storage_buffer_binding_size {
            return Err(BatchBackendError::InitFailed(
                "i8 resident matrix exceeds adapter storage buffer limit".to_string(),
            ));
        }
        if packed_bytes == 0 {
            return Err(BatchBackendError::InitFailed(
                "i8 resident matrix is empty".to_string(),
            ));
        }

        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let packed_staging = bytes_of_u32_slice(&packed)?;
        let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let row_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("i8 resident matrix packed rows"),
            size: packed_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(&row_buffer, 0, &packed_staging);
        let poll_failed = || {
            BatchBackendError::InitFailed(
                "device poll failed or timed out during i8 buffer upload".to_string(),
            )
        };
        if block_on_with_device_poll(&ctx.device, oom_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu out of memory while uploading the i8 resident matrix".to_string(),
            ));
        }
        if block_on_with_device_poll(&ctx.device, validation_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu validation error while uploading the i8 resident matrix".to_string(),
            ));
        }

        Ok(Self {
            matrix,
            params,
            row_buffer,
            row_stride,
            options,
            device_lost: ctx.device_lost.clone(),
            uncaptured_error: ctx.uncaptured_error.clone(),
            stats: std::sync::Arc::new(GpuI8Stats::default()),
            meta: GpuI8Meta {
                backend: ctx.backend,
                dot4_impl: Dot4I8Impl::Undetermined,
            },
        })
    }

    pub fn stats(&self) -> GpuI8BatchStatsSnapshot {
        self.stats.snapshot()
    }

    pub fn meta(&self) -> GpuI8Meta {
        self.meta
    }

    /// **テスト・ベンチ専用**（`bench-internals` feature 限定）。再スコア前の
    /// 生 i32 スコア（`(slot, score)`。GPU から readback した値そのまま）を
    /// クエリごとに返す。可視性判定・スコア契約には関与しない（既存
    /// `batch_search_with_row_budget_for_tests` と同じ露出方針）。CPU 参照
    /// 実装（[`dot_i8_packed_ref`]）との整数一致検証に使う。
    #[cfg(feature = "bench-internals")]
    pub fn batch_search_raw_i32_for_tests(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<Vec<(u32, i32)>>, BatchExecError> {
        self.raw_i32_scores(queries)
    }

    fn raw_i32_scores(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<Vec<(u32, i32)>>, BatchExecError> {
        self.guard_runtime()?;
        validate_batch_queries(self.matrix.dim(), queries).map_err(BatchExecError::Input)?;
        check_reachable_batch_work(&self.matrix, queries).map_err(BatchExecError::Input)?;

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => {
                return Err(BatchExecError::Backend(BatchBackendError::InitFailed(
                    msg.clone(),
                )))
            }
        };
        let pipeline = i8_pipeline()
            .map_err(|msg| BatchExecError::Backend(BatchBackendError::InitFailed(msg)))?;

        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let groups = group_queries_by_ctx(queries);
        let mut out: Vec<Option<Vec<(u32, i32)>>> = Vec::new();
        try_reserve_exact(&mut out, queries.len(), "gpu i8 raw scores")
            .map_err(BatchExecError::Input)?;
        out.resize_with(queries.len(), || None);

        for group in &groups {
            let Some(&first_idx) = group.first() else {
                continue;
            };
            let group_ctx = queries.get(first_idx).map(|q| q.ctx).ok_or_else(|| {
                BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                    "i8 query group index out of range".to_string(),
                ))
            })?;
            let reachable = gather_reachable_rows(&self.matrix, group_ctx)
                .map_err(|e| BatchExecError::Input(e.into_batch_search_error()))?;

            for &qi in group {
                let vector = queries.get(qi).map(|q| q.vector).ok_or_else(|| {
                    BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                        "i8 query index out of range".to_string(),
                    ))
                })?;
                let qq = quantize_query(&self.params, vector).map_err(|e| {
                    BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(format!(
                        "i8 query quantize failed: {e}"
                    )))
                })?;

                let mut per_query: Vec<(u32, i32)> = Vec::new();
                let chunk_cap = i8_chunk_rows(ctx.max_workgroups_per_dimension);
                for chunk in reachable.chunks(chunk_cap.max(1)) {
                    let scores = dispatch_i8_dot_products(
                        ctx,
                        pipeline,
                        &self.row_buffer,
                        self.row_stride,
                        chunk,
                        &qq,
                    )?;
                    if scores.len() != chunk.len() {
                        return Err(BatchExecError::Backend(BatchBackendError::TransferFailed(
                            "i8 readback length mismatch".to_string(),
                        )));
                    }
                    self.stats.dispatches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .readback_bytes
                        .fetch_add((scores.len() as u64).saturating_mul(4), Ordering::Relaxed);
                    for (&slot, &score) in chunk.iter().zip(scores.iter()) {
                        per_query.push((slot, score));
                    }
                }
                if let Some(slot) = out.get_mut(qi) {
                    *slot = Some(per_query);
                }
            }
        }

        let mut results: Vec<Vec<(u32, i32)>> = Vec::new();
        try_reserve_exact(&mut results, out.len(), "gpu i8 raw scores (final)")
            .map_err(BatchExecError::Input)?;
        for slot in out {
            match slot {
                Some(v) => results.push(v),
                None => {
                    return Err(BatchExecError::Backend(
                        BatchBackendError::KernelLaunchFailed(
                            "i8 query result missing after dispatch".to_string(),
                        ),
                    ))
                }
            }
        }
        Ok(results)
    }

    fn guard_runtime(&self) -> Result<(), BatchExecError> {
        if self.device_lost.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(BatchExecError::Backend(BatchBackendError::DeviceLost(
                "gpu device lost".to_string(),
            )));
        }
        if self
            .uncaptured_error
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(BatchExecError::Backend(
                BatchBackendError::KernelLaunchFailed(
                    "gpu reported an uncaptured error".to_string(),
                ),
            ));
        }
        Ok(())
    }
}

/// 1 dispatch に含める行チャンクの行数上限（D11: i8 用の全量 readback のみ。
/// ワークグループ数が adapter 上限を超えないようにするだけの単純な上限）。
fn i8_chunk_rows(max_workgroups_per_dimension: u32) -> usize {
    (max_workgroups_per_dimension as usize).saturating_mul(256)
}

impl BatchBackend for GpuI8BatchBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        let raw = self.raw_i32_scores(queries)?;

        let mut hits: Vec<BatchHit> = Vec::new();
        try_reserve_exact(&mut hits, queries.len(), "gpu i8 batch hits")
            .map_err(BatchExecError::Input)?;

        let mut row_buf: Vec<f32> = Vec::new();
        for (qi, per_query) in raw.iter().enumerate() {
            let query = queries.get(qi).ok_or_else(|| {
                BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                    "i8 query index out of range".to_string(),
                ))
            })?;

            // D8: k' = min(reachable_rows, k * oversample) 件を i32 降順
            // （同点は slot 昇順）で選び、その候補だけを f32 再スコアする。
            let k_prime = query
                .k
                .saturating_mul(self.options.oversample)
                .min(per_query.len());
            let mut candidates: Vec<(u32, i32)> = per_query.clone();
            candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            candidates.truncate(k_prime);

            self.stats
                .rescored_candidates
                .fetch_add(candidates.len() as u64, Ordering::Relaxed);

            let mut selector = TopKSelector::new(query.k);
            for (slot, _int_score) in &candidates {
                if self
                    .matrix
                    .row_f32_into(*slot as usize, &mut row_buf)
                    .is_none()
                {
                    continue;
                }
                let score = dot(query.vector, &row_buf);
                if !score.is_finite() {
                    continue;
                }
                selector.push(CandidateHit {
                    id: u64::from(*slot),
                    score,
                });
            }

            let resolved = finalize_gpu_hits(&self.matrix, query.ctx, &selector.into_sorted_vec())
                .map_err(BatchExecError::Input)?;
            hits.push(BatchHit { hits: resolved });
        }

        Ok(hits)
    }
}

/// 1 クエリ × `row_indices` 分の i8 整数内積を GPU で計算し、readback した
/// `i32` 列を返す（[`super::dispatch_dot_products`] の i8 版。手順・防御は
/// 同一だが、readback の解釈が `u32 as i32`〔ビット再解釈。二の補数のため
/// `as` キャストで正しい値になる〕である点だけが異なる）。
fn dispatch_i8_dot_products(
    ctx: &GpuContext,
    pipeline: &wgpu::ComputePipeline,
    row_buffer: &wgpu::Buffer,
    row_stride: usize,
    row_indices: &[u32],
    query_packed: &[u32],
) -> Result<Vec<i32>, BatchExecError> {
    if row_indices.is_empty() {
        return Ok(Vec::new());
    }
    if query_packed.len() != row_stride {
        return Err(BatchExecError::Backend(BatchBackendError::TransferFailed(
            "i8 query buffer length does not match row_stride".to_string(),
        )));
    }

    let row_count = row_indices.len() as u32;
    let params = GpuParams {
        row_stride: row_stride as u32,
        row_count,
        query_count: 1,
        query_stride: row_stride as u32,
    };
    let params_bytes = params.to_ne_bytes_vec().map_err(BatchExecError::Backend)?;
    let row_ids_bytes = bytes_of_u32_slice(row_indices).map_err(BatchExecError::Backend)?;
    let query_bytes = bytes_of_u32_slice(query_packed).map_err(BatchExecError::Backend)?;

    let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let params_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product params"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&params_buffer, 0, &params_bytes);

    let row_ids_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product row ids"),
        size: row_ids_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&row_ids_buffer, 0, &row_ids_bytes);

    let query_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product query"),
        size: query_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&query_buffer, 0, &query_bytes);

    let scores_bytes = (row_indices.len() as u64).saturating_mul(4);
    let scores_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product scores"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product readback"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("i8 batch dot product bind group"),
        layout: &ctx.bind_group_layout,
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
            label: Some("i8 batch dot product encoder"),
        });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("i8 batch dot product pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        let workgroups = (row_count as u64).div_ceil(256);
        let workgroups_x = u32::try_from(workgroups).unwrap_or(u32::MAX);
        pass.dispatch_workgroups(workgroups_x, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&scores_buffer, 0, &readback_buffer, 0, scores_bytes);
    ctx.queue.submit(std::iter::once(encoder.finish()));

    let poll_failed =
        || BatchBackendError::DeviceLost("device poll failed or timed out".to_string());
    let oom_err = block_on_with_device_poll(&ctx.device, oom_scope.pop())
        .map_err(|_| poll_failed())
        .map_err(BatchExecError::Backend)?;
    let validation_err = block_on_with_device_poll(&ctx.device, validation_scope.pop())
        .map_err(|_| poll_failed())
        .map_err(BatchExecError::Backend)?;
    if let Some(e) = oom_err {
        return Err(BatchExecError::Backend(
            BatchBackendError::KernelLaunchFailed(format!("gpu out of memory: {e}")),
        ));
    }
    if let Some(e) = validation_err {
        return Err(BatchExecError::Backend(
            BatchBackendError::KernelLaunchFailed(format!("gpu validation error: {e}")),
        ));
    }

    let bytes = wait_and_read_buffer(ctx, &readback_buffer).map_err(BatchExecError::Backend)?;
    let (quads, remainder) = bytes.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(BatchExecError::Backend(BatchBackendError::TransferFailed(
            "i8 readback byte length not a multiple of 4".to_string(),
        )));
    }
    let mut out: Vec<i32> = Vec::new();
    out.try_reserve_exact(quads.len())
        .map_err(|_| BatchBackendError::TransferFailed("i8 readback allocation failed".to_string()))
        .map_err(BatchExecError::Backend)?;
    for quad in quads {
        out.push(u32::from_ne_bytes(*quad) as i32);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_i8x4_round_trips_and_preserves_lane_order() {
        let lanes = [1i8, -2, 3, -4];
        let packed = pack_i8x4(lanes);
        // レーン 0 が最下位バイトに来ることを固定する（レーン順の取り違えを
        // 検出できるよう非対称な値を使う）。
        assert_eq!(packed & 0xFF, 1u32);
        assert_eq!(unpack_i8x4(packed), lanes);
    }

    #[test]
    fn encode_rows_clamps_to_signed_range_and_handles_constant_dim() {
        // dim=2: 次元 0 は値が変化する・次元 1 は定数（alpha == 0 になる）。
        let vectors = [0.0f32, 5.0, 10.0, 5.0, -10.0, 5.0];
        let (params, packed) = encode_rows(2, 3, &vectors).expect("encode should succeed");
        assert_eq!(params.alpha.get(1).copied(), Some(0.0));
        assert_eq!(packed.len(), 3); // row_stride_for_dim(2) == 1, 3 rows
        for &p in &packed {
            let lanes = unpack_i8x4(p);
            for &lane in &lanes[..2] {
                assert!((-127..=127).contains(&(lane as i32)));
            }
        }
    }

    #[test]
    fn encode_rows_rejects_non_finite_input() {
        let vectors = [0.0f32, f32::NAN];
        assert_eq!(
            encode_rows(2, 1, &vectors).err(),
            Some(I8EncodeError::NonFinite)
        );
    }

    #[test]
    fn encode_rows_rejects_shape_mismatch() {
        let vectors = [0.0f32, 1.0, 2.0];
        assert_eq!(
            encode_rows(2, 1, &vectors).err(),
            Some(I8EncodeError::InvalidShape)
        );
    }

    #[test]
    fn quantize_query_all_zero_scale_yields_zero_packed() {
        let params = Sq8DimParams {
            center: vec![0.0, 0.0],
            alpha: vec![0.0, 0.0],
        };
        let packed = quantize_query(&params, &[1.0, 2.0]).expect("quantize should succeed");
        for p in packed {
            assert_eq!(p, 0);
        }
    }

    #[test]
    fn dot_i8_packed_ref_extreme_values_stay_within_i32_bound() {
        // D6: dim=8192・全レーン ±127 の極値で i32 の理論上限内に厳密に収まる。
        let dim = 8_192usize;
        let row_stride = row_stride_for_dim(dim);
        let row: Vec<u32> = (0..row_stride)
            .map(|_| pack_i8x4([127, -127, 127, -127]))
            .collect();
        let query: Vec<u32> = (0..row_stride)
            .map(|_| pack_i8x4([127, -127, 127, -127]))
            .collect();
        let score = dot_i8_packed_ref(&row, &query);
        // 全レーンが `127*127` の正値（符号が一致するペアの積は常に正）に
        // なるよう組んだ入力のため、理論上限に一致する。
        assert_eq!(i64::from(score), MAX_I8_DOT_MAGNITUDE);
    }

    #[test]
    fn dot_i8_packed_ref_matches_naive_lane_expansion() {
        let row = vec![pack_i8x4([1, 2, 3, 4]), pack_i8x4([-1, -2, -3, -4])];
        let query = vec![pack_i8x4([4, 3, 2, 1]), pack_i8x4([1, 1, 1, 1])];
        // 4+6+6+4=20、-1-2-3-4=-10 で expected=10。
        let expected: i32 = 10;
        assert_eq!(dot_i8_packed_ref(&row, &query), expected);
    }

    #[test]
    fn dot_shader_i8_wgsl_uses_dot4_i8_packed_builtin() {
        assert!(DOT_SHADER_I8_WGSL.contains("dot4I8Packed"));
    }

    #[test]
    fn gpu_i8_options_out_of_range_oversample_is_rejected_without_gpu() {
        // GPU 非搭載環境でも純粋に検証できる範囲（`try_new` 冒頭のオプション
        // 検証は GPU デバイスへ触れる前に走る）だけを固定する。
        const _: () = assert!(DEFAULT_I8_OVERSAMPLE >= 1);
        const _: () = assert!(DEFAULT_I8_OVERSAMPLE <= MAX_I8_OVERSAMPLE);
    }
}
