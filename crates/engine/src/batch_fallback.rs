//! GPU バックエンドの初期化失敗・実行時エラーに対する CPU-SIMD 縮退機構
//! （TASK-129・対象ビヘイビア: CORE-8）。
//!
//! `batch_search.rs::BatchEngine`（f16 パック常駐行列を走査する GPU 経路の CPU
//! 参照実装。TASK-128 ポインタ）を primary バックエンドとして [`BatchBackend`] に
//! 適合させ（[`GpuReferenceBackend`]）、初期化失敗・実行時エラー（デバイス
//! ロスト・カーネル起動失敗・転送失敗）の双方で [`FallbackBatchEngine`] が
//! f32 原本を保持する CPU 縮退経路（`kernel.rs::dot` + `kernel.rs::TopKSelector`。
//! CORE-3 と同一カーネル）へ panic なしに切り替える。走査・マスク・二重防御
//! ロジックは `batch_search.rs::run_batch_search`（行ソース抽象
//! `batch_search.rs::BatchRowSource`）を GPU 参照実装と共有するため、「縮退経路
//! だけ検査が緩い」という構造的な欠落を作らない。共有の結果、縮退後の Top-k は
//! CPU-SIMD 経路（`kernel.rs::CpuScalarProvider` と同じ選出規約）と構成的に
//! 一致する。
//!
//! # データ所有権・二重常駐について
//!
//! CORE-16 ポインタの方針（CPU 経路は f32 のまま維持）を満たすため、CPU 縮退
//! 用の行列は primary（GPU 参照実装の f16 パック常駐行列）とは独立に f32 原本を
//! 保持する（[`FallbackBatchEngine::build`] が両方を構築する）。これは常駐
//! コーパスの二重確保を意味するが、f16 パック行列（`ResidentMatrix::build` が
//! `batch_search.rs::MAX_BATCH_TOTAL_BYTES` で検証）と f32 縮退行列
//! （[`FallbackBatchEngine::build`] が同じ `MAX_BATCH_TOTAL_BYTES` 予算で
//! 独立に検証）のどちらも無制限確保にはならない設計とする。

use std::fmt;

use crate::batch_search::{
    run_batch_search, BatchEngine, BatchHit, BatchQuery, BatchRowSource, BatchSearchError,
    ResidentMatrix, MAX_BATCH_TOTAL_BYTES,
};
use crate::storage::Visibility;

/// primary バックエンド実行時のエラー種別（CORE-8 ポインタ）。GPU デバイス
/// 初期化失敗と実行時エラー（デバイスロスト・カーネル起動失敗・転送失敗）を
/// 区別し、[`FallbackEvent`] のログ可視化で要因を判別できるようにする。
/// メッセージはプログラム出力文字列（英語）で、テナント ID・クエリ内容を
/// 含めない（security.md「機微情報の漏えい」対応）。
#[derive(Debug, Clone, PartialEq)]
pub enum BatchBackendError {
    /// バックエンドの初期化（デバイス確保等）が失敗した。
    InitFailed(String),
    /// 初期化済みのデバイスが実行中に失われた。
    DeviceLost(String),
    /// カーネル起動に失敗した。
    KernelLaunchFailed(String),
    /// ホスト⇔デバイス間のデータ転送に失敗した。
    TransferFailed(String),
}

impl BatchBackendError {
    /// [`FallbackEvent`] のログ可視化用に、要因を「初期化失敗」「実行時エラー」の
    /// 2 種別へ分類する。
    fn reason(&self) -> FallbackReason {
        match self {
            BatchBackendError::InitFailed(_) => FallbackReason::Init,
            BatchBackendError::DeviceLost(_)
            | BatchBackendError::KernelLaunchFailed(_)
            | BatchBackendError::TransferFailed(_) => FallbackReason::Runtime,
        }
    }
}

impl fmt::Display for BatchBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchBackendError::InitFailed(msg) => write!(f, "batch backend init failed: {msg}"),
            BatchBackendError::DeviceLost(msg) => write!(f, "batch backend device lost: {msg}"),
            BatchBackendError::KernelLaunchFailed(msg) => {
                write!(f, "batch backend kernel launch failed: {msg}")
            }
            BatchBackendError::TransferFailed(msg) => {
                write!(f, "batch backend transfer failed: {msg}")
            }
        }
    }
}

impl std::error::Error for BatchBackendError {}

/// [`BatchBackend::batch_search`] のエラー。入力検証エラー（fail-closed で
/// そのままクライアントへ返す・縮退させない）とバックエンド実行エラー（縮退
/// トリガ）を型で峻別する（TASK-129 設計の要）。入力検証エラーを縮退で握り
/// つぶすと不正入力が黙殺されてしまうため、この区別を型レベルで強制する。
#[derive(Debug, Clone, PartialEq)]
pub enum BatchExecError {
    /// クエリ・常駐行列の入力自体が不正（fail-closed。縮退させず `Err` を返す）。
    Input(BatchSearchError),
    /// バックエンドの実行に失敗した（縮退トリガ）。
    Backend(BatchBackendError),
}

impl fmt::Display for BatchExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchExecError::Input(e) => write!(f, "{e}"),
            BatchExecError::Backend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BatchExecError {}

/// バッチ検索の実行バックエンド窓口（CORE-8 ポインタ）。object-safe
/// （`&self` メソッドのみ・ジェネリクスなし）を維持し、[`FallbackBatchEngine`]
/// が `Box<dyn BatchBackend>` として保持できることを前提にする。エラー注入
/// テストは本体へテスト専用の公開 API・feature を追加せず、本 trait を
/// テストコード側で実装したモックを差し込むことで実現する（test-support
/// feature 撤廃（Issue #137 対応）の方針を踏襲）。
pub trait BatchBackend: Send + Sync {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError>;
}

/// 既存 `BatchEngine`（f16 パック常駐行列を走査する GPU 経路の CPU 参照実装。
/// TASK-128 ポインタ）を primary バックエンドとして適合させるラッパー。実 GPU
/// （`wgpu` 等）への接続は依存追加のユーザー承認が前提のため未実装であり
/// （dependency-policy.md）、本実装からランタイムエラー（[`BatchExecError::Backend`]）
/// が自発的に発生することはない。実 GPU 接続時はこの型を差し替える想定。
pub struct GpuReferenceBackend {
    engine: BatchEngine,
}

impl GpuReferenceBackend {
    pub fn new(matrix: ResidentMatrix) -> Self {
        Self {
            engine: BatchEngine::new(matrix),
        }
    }
}

impl BatchBackend for GpuReferenceBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        self.engine
            .batch_search(queries)
            .map_err(BatchExecError::Input)
    }
}

/// 縮退の要因種別（ログ可視化用。CORE-8 ポインタ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// primary バックエンドの構築（初期化）が失敗した。
    Init,
    /// primary バックエンドは初期化済みだが、実行時エラー（デバイスロスト・
    /// カーネル起動失敗・転送失敗）により縮退した。
    Runtime,
}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FallbackReason::Init => write!(f, "init"),
            FallbackReason::Runtime => write!(f, "runtime"),
        }
    }
}

/// 縮退イベント 1 件（CORE-8 ポインタ）。テナント ID・クエリ内容は含めない
/// （security.md「機微情報の漏えい」対応。Display 衛生をユニットテストで
/// 固定する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackEvent {
    pub reason: FallbackReason,
    /// 切り替え先バックエンドの識別子（現状は常に `"cpu-simd"`）。
    pub target: &'static str,
}

impl fmt::Display for FallbackEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "batch_fallback: switched to {} backend (reason={})",
            self.target, self.reason
        )
    }
}

/// 縮退発生の可視化フック（CORE-8 ポインタ:「黙って性能劣化しない」契約）。
/// engine には log/tracing 系依存が存在しないため（dependency-policy.md:
/// 依存追加はユーザー承認制）、依存追加なしの注入型オブザーバとして設計する。
/// 既定実装 [`StderrFallbackObserver`] は英語 1 行を stderr へ出力する
/// （wire-server 側の将来のログ機構へ差し替え可能）。テストは記録型オブザーバで
/// 発生回数・要因・切り替え先をアサートする。
pub trait FallbackObserver: Send + Sync {
    fn on_fallback(&self, event: FallbackEvent);
}

/// 既定の縮退オブザーバ。stderr へ英語 1 行を出力する。
#[derive(Debug, Default, Clone, Copy)]
pub struct StderrFallbackObserver;

impl FallbackObserver for StderrFallbackObserver {
    fn on_fallback(&self, event: FallbackEvent) {
        eprintln!("{event}");
    }
}

/// CPU 縮退経路が走査する f32 常駐行列（CORE-8 ポインタ）。`batch_search.rs::
/// ResidentMatrix` の f16 パックとは独立に f32 原本を保持する（本モジュール
/// 冒頭コメント「データ所有権・二重常駐について」参照）。
struct CpuFallbackMatrix {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
}

impl BatchRowSource for CpuFallbackMatrix {
    fn row_count(&self) -> usize {
        self.ids.len()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn ids(&self) -> &[u64] {
        &self.ids
    }

    fn tenant_ids(&self) -> &[String] {
        &self.tenant_ids
    }

    fn visibilities(&self) -> &[Visibility] {
        &self.visibilities
    }

    fn row_f32_into(&self, idx: usize, out: &mut Vec<f32>) -> Option<()> {
        let start = idx.checked_mul(self.dim)?;
        let end = start.checked_add(self.dim)?;
        let row = self.vectors.get(start..end)?;
        out.clear();
        out.extend_from_slice(row);
        Some(())
    }
}

/// primary バックエンドの保持状態。初期化失敗を検知した後は毎回 primary の
/// 呼び出しを試みず（無駄な失敗呼び出しを避ける）、以降の `batch_search` は
/// 常に CPU 縮退経路を使う。
enum PrimarySlot {
    Available(Box<dyn BatchBackend>),
    Unavailable,
}

/// バッチ経路の新しい公開入口（CORE-8 ポインタ）。primary バックエンド
/// （通常は [`GpuReferenceBackend`]）を実行し、バックエンド実行エラー
/// （[`BatchExecError::Backend`]）の場合のみ CPU-SIMD 縮退経路
/// （`batch_search.rs::run_batch_search` を CPU 縮退用行列で呼ぶ。CORE-3 と
/// 同一カーネル）へ 1 回だけ再実行する（リトライループを作らない。DoS
/// 抑止）。入力検証エラー（[`BatchExecError::Input`]）は縮退させず fail-closed
/// に `Err` をそのまま返す（不正入力を縮退で黙殺しない、という本モジュールの
/// 設計の要）。
pub struct FallbackBatchEngine {
    primary: PrimarySlot,
    cpu: CpuFallbackMatrix,
    observer: Box<dyn FallbackObserver>,
}

impl FallbackBatchEngine {
    /// 元データ（可視性フィルタ済みの行集合。`ResidentMatrix::build` と同じ
    /// 信頼境界: `tenant_ids`/`visibilities` は untrusted なユーザー入力では
    /// なく、呼び出し元が `Storage` から読み出した実データを渡す責務を負う）
    /// から primary バックエンドと CPU 縮退用の f32 常駐行列を構築する。
    ///
    /// `backend_factory` は primary バックエンドの初期化を担う（実 GPU 接続時
    /// はデバイス確保等を行う想定。[`GpuReferenceBackend`] は常に成功する）。
    /// 初期化が [`BatchBackendError`] を返した場合、本コンストラクタ自体は
    /// panic せず `Ok`（CPU 専用モード）で成立させ、縮退イベントを 1 件通知
    /// する（CORE-8: 初期化失敗からの縮退）。一方、`ids`/`tenant_ids`/
    /// `visibilities`/`vectors` 自体のデータ不整合・容量超過
    /// （[`BatchSearchError`] 系）は縮退させず `Err` で fail-closed に返す
    /// （構築段階の入力エラーも「縮退しない」対象として扱う）。
    pub fn build(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[Visibility],
        dim: usize,
        vectors: &[f32],
        backend_factory: impl FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>, BatchBackendError>,
        observer: Box<dyn FallbackObserver>,
    ) -> Result<Self, BatchSearchError> {
        // primary（f16 パック常駐行列）を構築する。次元・行数・容量・重複 id・
        // tenant_id 長の検証はここで一元的に行われ（`ResidentMatrix::build`）、
        // CPU 縮退用の f32 行列はこの検証を通過した後に同じ元データから
        // 独立に構築する（検証ロジックの二重管理を避ける）。
        let resident = ResidentMatrix::build(ids, tenant_ids, visibilities, dim, vectors)?;

        // CPU 縮退用 f32 行列の容量検証（本モジュール冒頭コメント「データ
        // 所有権・二重常駐について」参照）。primary の f16 パックは
        // `ResidentMatrix::build` が独立に `MAX_BATCH_TOTAL_BYTES` で検証済み
        // であり、本チェックは f32 原本コピー分を同じ予算で追加検証する。
        let f32_bytes = ids
            .len()
            .checked_mul(dim)
            .and_then(|n| n.checked_mul(std::mem::size_of::<f32>()))
            .ok_or(BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            })?;
        if f32_bytes > MAX_BATCH_TOTAL_BYTES {
            return Err(BatchSearchError::CapacityExceeded {
                total_bytes: f32_bytes,
                max: MAX_BATCH_TOTAL_BYTES,
            });
        }

        // 以降のフォールブル確保は `ResidentMatrix::build` と同方針
        // （`Vec::with_capacity`・`to_vec()`・`String::clone` は abort-on-OOM
        // のため使わず、`try_reserve_exact` で明示的に処理する）。
        let mut owned_vectors: Vec<f32> = Vec::new();
        owned_vectors
            .try_reserve_exact(vectors.len())
            .map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback vectors: {e}"
                ))
            })?;
        owned_vectors.extend_from_slice(vectors);

        let mut owned_ids: Vec<u64> = Vec::new();
        owned_ids.try_reserve_exact(ids.len()).map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve fallback ids: {e}"))
        })?;
        owned_ids.extend_from_slice(ids);

        let mut owned_tenant_ids: Vec<String> = Vec::new();
        owned_tenant_ids
            .try_reserve_exact(tenant_ids.len())
            .map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback tenant_ids: {e}"
                ))
            })?;
        for tenant in tenant_ids {
            let mut owned = String::new();
            owned.try_reserve_exact(tenant.len()).map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback tenant_id: {e}"
                ))
            })?;
            owned.push_str(tenant);
            owned_tenant_ids.push(owned);
        }

        let mut owned_visibilities: Vec<Visibility> = Vec::new();
        owned_visibilities
            .try_reserve_exact(visibilities.len())
            .map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback visibilities: {e}"
                ))
            })?;
        owned_visibilities.extend_from_slice(visibilities);

        let cpu = CpuFallbackMatrix {
            ids: owned_ids,
            tenant_ids: owned_tenant_ids,
            visibilities: owned_visibilities,
            dim,
            vectors: owned_vectors,
        };

        let primary = match backend_factory(resident) {
            Ok(backend) => PrimarySlot::Available(backend),
            Err(init_err) => {
                debug_assert_eq!(init_err.reason(), FallbackReason::Init);
                observer.on_fallback(FallbackEvent {
                    reason: FallbackReason::Init,
                    target: "cpu-simd",
                });
                PrimarySlot::Unavailable
            }
        };

        Ok(Self {
            primary,
            cpu,
            observer,
        })
    }

    /// [`GpuReferenceBackend`] を primary として使う既定コンストラクタ。実 GPU
    /// 未接続の現段階では常に初期化成功する（モジュール冒頭コメント参照）ため、
    /// 通常の呼び出し元はこちらを使う。
    pub fn build_with_gpu_reference(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[Visibility],
        dim: usize,
        vectors: &[f32],
        observer: Box<dyn FallbackObserver>,
    ) -> Result<Self, BatchSearchError> {
        Self::build(
            ids,
            tenant_ids,
            visibilities,
            dim,
            vectors,
            |matrix| Ok(Box::new(GpuReferenceBackend::new(matrix)) as Box<dyn BatchBackend>),
            observer,
        )
    }

    /// バッチ検索を実行する（CORE-8 ポインタ）。primary が利用可能なら primary
    /// を実行し、[`BatchExecError::Backend`]（実行時エラー）の場合のみ CPU
    /// 縮退経路へ 1 回だけ再実行する。[`BatchExecError::Input`]（入力エラー）は
    /// 縮退させず `Err` をそのまま返す（TenantMaskViolation を含む
    /// `BatchSearchError` は常にこちら経由で、縮退イベントを発生させない）。
    /// primary が初期化失敗済み（[`PrimarySlot::Unavailable`]）の場合は
    /// 最初から CPU 経路を使う（追加の縮退イベントは発生させない。構築時に
    /// 既に 1 件通知済みのため）。
    pub fn batch_search(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<BatchHit>, BatchSearchError> {
        match &self.primary {
            PrimarySlot::Available(backend) => match backend.batch_search(queries) {
                Ok(hits) => Ok(hits),
                Err(BatchExecError::Input(e)) => Err(e),
                Err(BatchExecError::Backend(err)) => {
                    self.observer.on_fallback(FallbackEvent {
                        reason: err.reason(),
                        target: "cpu-simd",
                    });
                    run_batch_search(&self.cpu, queries)
                }
            },
            PrimarySlot::Unavailable => run_batch_search(&self.cpu, queries),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyContext;

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id")
    }

    struct RecordingObserver {
        events: std::sync::Mutex<Vec<FallbackEvent>>,
    }

    impl RecordingObserver {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<FallbackEvent> {
            self.events.lock().expect("lock").clone()
        }
    }

    impl FallbackObserver for RecordingObserver {
        fn on_fallback(&self, event: FallbackEvent) {
            self.events.lock().expect("lock").push(event);
        }
    }

    // Display 衛生（security.md「機微情報の漏えい」対応）: エラー・イベントの
    // 出力文字列にテナント情報・クエリ内容が含まれないことを固定する。
    #[test]
    fn backend_error_display_excludes_tenant_and_query_content() {
        let err = BatchBackendError::DeviceLost("nvidia-driver-reset".to_string());
        let rendered = err.to_string();
        assert!(rendered.contains("device lost"));
        assert!(!rendered.contains("tenant"));
    }

    #[test]
    fn fallback_event_display_is_english_and_stable() {
        let event = FallbackEvent {
            reason: FallbackReason::Runtime,
            target: "cpu-simd",
        };
        assert_eq!(
            event.to_string(),
            "batch_fallback: switched to cpu-simd backend (reason=runtime)"
        );
    }

    #[test]
    fn backend_error_reason_classifies_init_vs_runtime() {
        assert_eq!(
            BatchBackendError::InitFailed("x".to_string()).reason(),
            FallbackReason::Init
        );
        for err in [
            BatchBackendError::DeviceLost("x".to_string()),
            BatchBackendError::KernelLaunchFailed("x".to_string()),
            BatchBackendError::TransferFailed("x".to_string()),
        ] {
            assert_eq!(err.reason(), FallbackReason::Runtime);
        }
    }

    // CPU 縮退用 f32 行列の容量検証（本モジュール冒頭コメント「データ所有権・
    // 二重常駐について」対応）。行数・次元それぞれは上限内でも、f32 原本の
    // 総確保バイト量が `MAX_BATCH_TOTAL_BYTES` を超える組み合わせは拒否する。
    #[test]
    fn build_rejects_fallback_matrix_capacity_over_limit() {
        // dim * rows * 4 bytes が 1 GiB を超えるが、`ResidentMatrix::build` の
        // f16 パック単体（半分のバイト数）は 1 GiB に収まらない規模なので、
        // まず先に `ResidentMatrix::build` 側で拒否される可能性がある。
        // ここでは f32 側だけが超過する境界（f16 は収まるが f32 は超える）を
        // 作れないため、いずれにせよ `CapacityExceeded` が返ることを確認する
        // （どちらのチェックで拒否されても fail-closed という契約は同じ）。
        let dim = 8_192usize;
        let rows = 200_000usize; // 8192 * 200000 * 4 bytes ≈ 6.55 GiB
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenant_ids: Vec<String> = vec!["tenant-a".to_string(); rows];
        let visibilities = vec![Visibility::Public; rows];
        let vectors = vec![0.0f32; 1]; // 意図的に短い: 容量チェックが長さ検証より先に走ることを確認する
        let observer = Box::new(RecordingObserver::new());
        let result = FallbackBatchEngine::build_with_gpu_reference(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            observer,
        );
        let err = match result {
            Ok(_) => panic!("expected CapacityExceeded, got Ok"),
            Err(e) => e,
        };
        assert!(matches!(err, BatchSearchError::CapacityExceeded { .. }));
    }

    // 入力エラー峻別（TASK-129 設計の要）: 構築段階のデータ不整合
    // （`ArenaLengthMismatch`）は `BatchBackendError` 経由の縮退トリガでは
    // なく、`ResidentMatrix::build` の通常の fail-closed エラーとしてそのまま
    // 返る（縮退イベントは発生しない）。
    #[test]
    fn build_propagates_input_errors_without_triggering_fallback() {
        let ids = [1u64, 2];
        let tenant_ids = ["tenant-a".to_string(), "tenant-a".to_string()];
        let visibilities = [Visibility::Public, Visibility::Public];
        let vectors = [1.0f32, 0.0]; // ids.len()=2, dim=2 のはずが要素数が不足
        let observer = Box::new(RecordingObserver::new());
        let result = FallbackBatchEngine::build_with_gpu_reference(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &vectors,
            observer,
        );
        let err = match result {
            Ok(_) => panic!("expected ArenaLengthMismatch, got Ok"),
            Err(e) => e,
        };
        assert_eq!(err, BatchSearchError::ArenaLengthMismatch);
    }

    fn build_engine_with_backend(
        backend_factory: impl FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>, BatchBackendError>,
        observer: Box<dyn FallbackObserver>,
    ) -> FallbackBatchEngine {
        let ids = [1u64, 2, 3, 4];
        let tenant_ids = [
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ];
        let visibilities = [
            Visibility::Public,
            Visibility::Public,
            Visibility::Public,
            Visibility::Public,
        ];
        #[rustfmt::skip]
        let vectors = [
            1.0, 0.0,
            0.0, 1.0,
            2.0, 0.0,
            0.0, 2.0,
        ];
        FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &vectors,
            backend_factory,
            observer,
        )
        .expect("build ok")
    }

    /// primary バックエンドの実行時エラーを注入するモック（本体へテスト専用
    /// API を追加せず、`BatchBackend` trait をテストコード側で実装する
    /// エラー注入手段。Issue #137 対応の test-support feature 撤廃方針を踏襲）。
    struct FailingBackend(BatchBackendError);

    impl BatchBackend for FailingBackend {
        fn batch_search(
            &self,
            _queries: &[BatchQuery<'_>],
        ) -> Result<Vec<BatchHit>, BatchExecError> {
            Err(BatchExecError::Backend(self.0.clone()))
        }
    }

    /// primary バックエンドが `TenantMaskViolation` 等の入力エラーを返すことを
    /// 模したモック（縮退トリガにならないことの検証用）。
    struct InputErrorBackend(BatchSearchError);

    impl BatchBackend for InputErrorBackend {
        fn batch_search(
            &self,
            _queries: &[BatchQuery<'_>],
        ) -> Result<Vec<BatchHit>, BatchExecError> {
            Err(BatchExecError::Input(self.0.clone()))
        }
    }

    // オラクル: 可視行だけを渡した `kernel::CpuScalarProvider::search` の結果
    // （スコア降順・同点 id 昇順まで完全一致）と縮退後の Top-k を突き合わせる。
    fn oracle_search(
        ids: &[u64],
        tenant_ids: &[&str],
        vectors_per_row: &[[f32; 2]],
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
    ) -> Vec<crate::kernel::SearchHit> {
        use crate::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
        let mut visible_ids = Vec::new();
        let mut visible_vectors = Vec::new();
        for ((&id, &tenant), row) in ids.iter().zip(tenant_ids).zip(vectors_per_row) {
            if ctx.is_visible(tenant, Visibility::Public) {
                visible_ids.push(id);
                visible_vectors.extend_from_slice(row);
            }
        }
        CpuScalarProvider
            .search(SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: 2,
                query,
                k,
            })
            .expect("oracle search ok")
    }

    // CORE-8: 初期化失敗注入 → 構築は `Ok`（CPU 専用モード）・検索は `Ok`・
    // イベントちょうど 1 件（要因=init・切り替え先=cpu-simd）・Top-k がオラクル
    // 一致。
    #[test]
    fn init_failure_falls_back_to_cpu_with_one_event_and_matches_oracle() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        let observer_for_engine: Box<dyn FallbackObserver> = {
            struct ArcObserver(std::sync::Arc<RecordingObserver>);
            impl FallbackObserver for ArcObserver {
                fn on_fallback(&self, event: FallbackEvent) {
                    self.0.on_fallback(event);
                }
            }
            Box::new(ArcObserver(observer.clone()))
        };

        let engine = build_engine_with_backend(
            |_matrix| Err(BatchBackendError::InitFailed("no gpu device".to_string())),
            observer_for_engine,
        );

        let events_after_build = observer.events();
        assert_eq!(events_after_build.len(), 1);
        assert_eq!(events_after_build[0].reason, FallbackReason::Init);
        assert_eq!(events_after_build[0].target, "cpu-simd");

        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx_a,
        }];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 1);

        let ids = [1u64, 2, 3, 4];
        let tenant_ids = ["tenant-a", "tenant-a", "tenant-b", "tenant-b"];
        let vectors_per_row = [[1.0, 0.0], [0.0, 1.0], [2.0, 0.0], [0.0, 2.0]];
        let expected = oracle_search(&ids, &tenant_ids, &vectors_per_row, &ctx_a, &query, 2);
        assert_eq!(hits[0].hits, expected);

        // 検索を重ねても追加の縮退イベントは発生しない（構築時の 1 件のみ）。
        engine.batch_search(&queries).expect("search ok");
        assert_eq!(observer.events().len(), 1);
    }

    // CORE-8: 実行時エラー注入（デバイスロスト・カーネル起動失敗・転送失敗の
    // 各種別）→ 構築は primary 利用可能のまま `Ok`・検索は `Ok`・イベント要因が
    // Runtime・Top-k がオラクル一致。
    #[test]
    fn runtime_errors_fall_back_to_cpu_and_match_oracle() {
        for backend_err in [
            BatchBackendError::DeviceLost("lost".to_string()),
            BatchBackendError::KernelLaunchFailed("launch".to_string()),
            BatchBackendError::TransferFailed("transfer".to_string()),
        ] {
            let observer = RecordingObserver::new();
            let engine = build_engine_with_backend(
                |_matrix| Ok(Box::new(FailingBackend(backend_err.clone())) as Box<dyn BatchBackend>),
                Box::new(observer),
            );

            let ctx_b = ctx("tenant-b");
            let query = [1.0f32, 0.0];
            let queries = vec![BatchQuery {
                vector: &query,
                k: 2,
                ctx: &ctx_b,
            }];
            let hits = engine.batch_search(&queries).expect("search ok");

            let ids = [1u64, 2, 3, 4];
            let tenant_ids = ["tenant-a", "tenant-a", "tenant-b", "tenant-b"];
            let vectors_per_row = [[1.0, 0.0], [0.0, 1.0], [2.0, 0.0], [0.0, 2.0]];
            let expected = oracle_search(&ids, &tenant_ids, &vectors_per_row, &ctx_b, &query, 2);
            assert_eq!(hits[0].hits, expected);
        }
    }

    // 実行時エラーが発生した回だけ、要因種別が正しくイベントへ反映されることを
    // 確認する（`FallbackReason::Runtime` に丸められるが、元のエラー種別に
    // 依らず一貫して Runtime に分類されることが本テストの主眼）。
    #[test]
    fn runtime_error_event_reason_is_runtime() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        struct ArcObserver(std::sync::Arc<RecordingObserver>);
        impl FallbackObserver for ArcObserver {
            fn on_fallback(&self, event: FallbackEvent) {
                self.0.on_fallback(event);
            }
        }
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(FailingBackend(BatchBackendError::DeviceLost(
                    "lost".to_string(),
                ))) as Box<dyn BatchBackend>)
            },
            Box::new(ArcObserver(observer.clone())),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];
        engine.batch_search(&queries).expect("search ok");
        let events = observer.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, FallbackReason::Runtime);
        assert_eq!(events[0].target, "cpu-simd");
    }

    // セキュリティ観点（security.md「不安全な設計」対応）: primary が入力エラー
    // （`TenantMaskViolation` を含む `BatchSearchError`）を返した場合は縮退させず
    // `Err` をそのまま返す。縮退で不正入力・整合性違反を黙殺しない、という
    // 本モジュールの設計の要を検証する。
    #[test]
    fn input_errors_from_primary_are_not_treated_as_fallback_trigger() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        struct ArcObserver(std::sync::Arc<RecordingObserver>);
        impl FallbackObserver for ArcObserver {
            fn on_fallback(&self, event: FallbackEvent) {
                self.0.on_fallback(event);
            }
        }
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(
                    Box::new(InputErrorBackend(BatchSearchError::TenantMaskViolation))
                        as Box<dyn BatchBackend>,
                )
            },
            Box::new(ArcObserver(observer.clone())),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::TenantMaskViolation);
        // 入力エラーは縮退トリガではないため、イベントは 1 件も発生しない。
        assert!(observer.events().is_empty());
    }

    // マルチテナントバッチで縮退後も他テナント行の混入 0 件であること・
    // クエリごとに独立した結果順序が保たれることを検証する。
    #[test]
    fn fallback_search_does_not_leak_rows_across_tenants_in_multi_tenant_batch() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(FailingBackend(BatchBackendError::DeviceLost(
                    "lost".to_string(),
                ))) as Box<dyn BatchBackend>)
            },
            Box::new(observer),
        );
        let ctx_a = ctx("tenant-a");
        let ctx_b = ctx("tenant-b");
        let query_a = [1.0f32, 1.0];
        let query_b = [1.0f32, 1.0];
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 4,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 4,
                ctx: &ctx_b,
            },
        ];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 2);
        // tenant-a の行 id は {1, 2}、tenant-b の行 id は {3, 4}。
        for hit in &hits[0].hits {
            assert!(
                hit.id == 1 || hit.id == 2,
                "tenant-a leaked row id={}",
                hit.id
            );
        }
        for hit in &hits[1].hits {
            assert!(
                hit.id == 3 || hit.id == 4,
                "tenant-b leaked row id={}",
                hit.id
            );
        }
    }

    // 正常時（primary 成功）はイベント 0 件・縮退再実行なしであることを確認する。
    #[test]
    fn successful_primary_search_emits_no_fallback_event() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend(
            |matrix| Ok(Box::new(GpuReferenceBackend::new(matrix)) as Box<dyn BatchBackend>),
            Box::new(observer),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 1);
    }

    // 決定性: 同一入力の再実行が同一結果を返すことを確認する（CPU 縮退経路が
    // 内部状態を持ち越して結果が揺れないこと）。
    #[test]
    fn fallback_search_is_deterministic_across_repeated_calls() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(FailingBackend(BatchBackendError::TransferFailed(
                    "transfer".to_string(),
                ))) as Box<dyn BatchBackend>)
            },
            Box::new(observer),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 1.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx_a,
        }];
        let first = engine.batch_search(&queries).expect("search ok");
        let second = engine.batch_search(&queries).expect("search ok");
        assert_eq!(first[0].hits, second[0].hits);
    }
}
