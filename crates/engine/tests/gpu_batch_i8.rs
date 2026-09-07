//! `gpu_batch::packed_i8::GpuI8BatchBackend` の結合テスト（Issue #542・親
//! #541・Phase 5 親 #460）。
//!
//! `tests/gpu_batch.rs` は GPU 有無いずれの環境でも意味のある回帰になるよう
//! `eprintln!` による skip 分岐を許しているが、本ファイルはそれをそのまま
//! 踏襲すると「GPU はあるが i8 パイプライン生成が naga に拒否された」
//! ようなリグレッションを `InitFailed` → skip で隠してしまう
//! （advisor レビュー指摘）。そこで既存の f16 常駐バックエンド
//! （[`GpuBatchBackend`]）の `try_new` が成功する環境では「この環境に GPU は
//! ある」と確定できるため、その場合は `GpuI8BatchBackend::try_new` の失敗を
//! そのまま assert 失敗として扱う（vacuous pass を防ぐ）。f16 バックエンドも
//! 失敗する環境（CI の GitHub ホステッド runner 等）でのみ本当に skip する。

use engine::batch_search::{BatchQuery, ResidentMatrix};
#[cfg(feature = "bench-internals")]
use engine::gpu_batch::packed_i8::{dot_i8_packed_ref, encode_rows, quantize_query};
use engine::gpu_batch::packed_i8::{GpuI8BatchBackend, GpuI8Options};
use engine::gpu_batch::GpuBatchBackend;
use engine::policy::PolicyContext;
use engine::storage::Visibility;

fn ctx_with_private(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private, Visibility::Public])
        .expect("valid tenant id")
}

struct Fixture {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
}

/// 6 行・dim=5（4 の倍数でない奇数次元でパディング経路を確認）・
/// tenant-a/tenant-b 混在フィクスチャ。レーン値が非対称になるよう構成し、
/// パック時のレーン順取り違えを検出できるようにする。
fn fixture() -> Fixture {
    Fixture {
        ids: vec![1, 2, 3, 4, 5, 6],
        tenant_ids: vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ],
        visibilities: vec![Visibility::Private; 6],
        dim: 5,
        #[rustfmt::skip]
        vectors: vec![
            1.0, 0.0, 0.3, -0.2, 0.9,
            0.0, 1.0, -0.5, 0.1, 0.4,
            2.0, -1.0, 0.2, 0.7, -0.3,
            0.0, 2.0, 1.1, -0.9, 0.05,
            -1.0, -1.0, 0.6, 0.6, 0.6,
            3.0, 0.5, -0.8, 0.2, 1.5,
        ],
    }
}

fn build_matrix(fx: &Fixture) -> ResidentMatrix {
    ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture")
}

/// この開発環境に実 GPU があるかどうかを、既存の f16 常駐バックエンド
/// （[`GpuBatchBackend`]）の初期化可否で確定する（モジュール冒頭コメント
/// 参照）。i8 バックエンドの `try_new` 失敗を隠れた skip にしないためのゲート。
fn gpu_definitely_available(fx: &Fixture) -> bool {
    GpuBatchBackend::try_new(build_matrix(fx)).is_ok()
}

/// GPU が初期化できない環境では `try_new` が panic せず `InitFailed` を返す
/// （CORE-8 と同様の「初期化失敗は `Err` で返す」契約の回帰）。GPU が使える
/// 環境ではこのテスト自体は何も主張せず終了する。
#[test]
fn i8_backend_try_new_fails_closed_when_gpu_unavailable() {
    let fx = fixture();
    if gpu_definitely_available(&fx) {
        eprintln!("gpu available in this environment; init-failure branch not exercised here");
        return;
    }
    let err = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options::default())
        .err()
        .expect("try_new should fail closed when no gpu is available");
    assert!(matches!(
        err,
        engine::batch_fallback::BatchBackendError::InitFailed(_)
    ));
}

/// `GpuI8Options::oversample` の範囲外値は GPU デバイスへ触れる前に拒否
/// される（D9）。GPU 有無に関わらず実行できる。
#[test]
fn i8_backend_rejects_out_of_range_oversample() {
    let fx = fixture();
    for bad in [0usize, 33] {
        let err = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options { oversample: bad })
            .err()
            .expect("out-of-range oversample should be rejected");
        assert!(matches!(
            err,
            engine::batch_fallback::BatchBackendError::InitFailed(_)
        ));
    }
}

/// GPU が利用可能な環境でのみ実走する中核テスト: 生の i32 スコア
/// （`bench-internals` feature 限定フック）が CPU 参照実装
/// （[`dot_i8_packed_ref`]）と完全一致すること（「CPU i8 経路との整数一致」の
/// 実体）。奇数次元（dim=5、4 の倍数でない）を含む。
#[cfg(feature = "bench-internals")]
#[test]
fn i8_backend_raw_scores_match_cpu_reference_when_gpu_available() {
    let fx = fixture();
    if !gpu_definitely_available(&fx) {
        eprintln!("gpu unavailable in this environment, skipping");
        return;
    }
    let matrix = build_matrix(&fx);
    let backend = GpuI8BatchBackend::try_new(matrix, GpuI8Options::default()).expect(
        "i8 backend try_new must succeed once the f16 backend already confirmed gpu availability",
    );

    let (_row_scales, packed_rows) = encode_rows(fx.dim, fx.ids.len(), &fx.vectors)
        .expect("encode_rows should succeed for well-formed fixture");
    let row_stride = fx.dim.div_ceil(4);

    let ctx_a = ctx_with_private("tenant-a");
    let query = [0.5f32, -0.2, 1.0, 0.3, -0.7];
    let bq = BatchQuery {
        vector: &query,
        k: 3,
        ctx: &ctx_a,
    };

    let raw = backend
        .batch_search_raw_i32_for_tests(std::slice::from_ref(&bq))
        .expect("raw i32 score dispatch should succeed once gpu is available");
    assert_eq!(raw.len(), 1);
    let per_query = &raw[0];
    // tenant-a の可視行（id 1,2,3 → slot 0,1,2）のみが現れること。
    assert_eq!(per_query.len(), 3);

    let (_s_q, qq) = quantize_query(&query).expect("query quantize should succeed");
    for &(slot, gpu_score) in per_query {
        let row_start = (slot as usize) * row_stride;
        let row_end = row_start + row_stride;
        let row = packed_rows
            .get(row_start..row_end)
            .expect("slot should be within packed rows");
        let cpu_score = dot_i8_packed_ref(row, &qq);
        assert_eq!(
            gpu_score, cpu_score,
            "gpu i32 score for slot {slot} does not match cpu reference"
        );
    }
}

/// 混在テナントバッチで他テナントの id が混入しないこと（GPU 利用可能な
/// 環境でのみ実走）。最終結果は D8 の f32 再スコア契約により CPU オラクル
/// （既存 `GpuBatchBackend` の f16 常駐真値）と同じ可視性判定を通る。
#[test]
fn i8_backend_mixed_tenant_batch_has_no_cross_tenant_leak_when_gpu_available() {
    let fx = fixture();
    if !gpu_definitely_available(&fx) {
        eprintln!("gpu unavailable in this environment, skipping");
        return;
    }
    let matrix = build_matrix(&fx);
    let backend = GpuI8BatchBackend::try_new(matrix, GpuI8Options::default()).expect(
        "i8 backend try_new must succeed once the f16 backend already confirmed gpu availability",
    );

    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");
    let query_a = [1.0f32, 0.0, 0.0, 0.0, 0.0];
    let query_b = [0.0f32, 1.0, 0.0, 0.0, 0.0];
    let bq_a = BatchQuery {
        vector: &query_a,
        k: 3,
        ctx: &ctx_a,
    };
    let bq_b = BatchQuery {
        vector: &query_b,
        k: 3,
        ctx: &ctx_b,
    };

    let hits = engine::batch_fallback::BatchBackend::batch_search(&backend, &[bq_a, bq_b])
        .expect("i8 batch_search should succeed once gpu is available");
    assert_eq!(hits.len(), 2);
    for hit in &hits[0].hits {
        assert!(
            (1..=3).contains(&hit.id),
            "unexpected id {} leaked into tenant-a result",
            hit.id
        );
        assert_eq!(hit.tenant_id, "tenant-a");
    }
    for hit in &hits[1].hits {
        assert!(
            (4..=6).contains(&hit.id),
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
        assert_eq!(hit.tenant_id, "tenant-b");
    }

    // stats() が非 vacuous であること（実際に dispatch・再スコアが発生した）。
    let stats = backend.stats();
    assert!(stats.dispatches >= 1);
    assert!(stats.rescored_candidates >= 1);

    // meta(): backend は有効な wgpu::Backend、dot4_impl は D12 の契約どおり
    // 常に Undetermined（wgpu 30.0.1 の公開 API では判別できないため）。
    let meta = backend.meta();
    assert_eq!(
        meta.dot4_impl,
        engine::gpu_batch::packed_i8::Dot4I8Impl::Undetermined
    );
}

/// 同一入力を 2 回実行した場合の決定性（id・スコア列が一致すること）。
#[test]
fn i8_backend_batch_search_is_deterministic_when_gpu_available() {
    let fx = fixture();
    if !gpu_definitely_available(&fx) {
        eprintln!("gpu unavailable in this environment, skipping");
        return;
    }
    let backend1 = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options::default())
        .expect("i8 backend try_new must succeed once gpu availability is confirmed");
    let backend2 = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options::default())
        .expect("i8 backend try_new must succeed once gpu availability is confirmed");

    let ctx_a = ctx_with_private("tenant-a");
    let query = [0.5f32, -0.2, 1.0, 0.3, -0.7];
    let bq = BatchQuery {
        vector: &query,
        k: 3,
        ctx: &ctx_a,
    };

    let hits1 =
        engine::batch_fallback::BatchBackend::batch_search(&backend1, std::slice::from_ref(&bq))
            .expect("batch_search should succeed");
    let hits2 =
        engine::batch_fallback::BatchBackend::batch_search(&backend2, std::slice::from_ref(&bq))
            .expect("batch_search should succeed");

    let extract = |hits: &engine::batch_search::BatchHit| -> Vec<(u64, u32)> {
        hits.hits
            .iter()
            .map(|h| (h.id, h.score.to_bits()))
            .collect()
    };
    assert_eq!(extract(&hits1[0]), extract(&hits2[0]));
}

/// 既存 GPU テスト（f16 常駐・CORE-16 対照経路）が本 Issue の変更後も green
/// であることの回帰アンカー（受け入れ条件「既存 GPU テストが green」）。
/// 実行自体は `tests/gpu_batch.rs`・`tests/batch_fallback.rs`・
/// `tests/gpu_scaling_accept.rs` が担うため、本テストはそれらのテストが
/// 同一バイナリに存在することの索引としてのみ機能する（実 CI コマンドは
/// `cargo test -p engine --all-features --test gpu_batch --test
/// batch_fallback --test gpu_scaling_accept`。README・実装計画ポインタ）。
#[test]
fn existing_gpu_tests_are_run_via_separate_test_binaries_note() {
    // 本テストは常に成功する（ドキュメント目的のプレースホルダ）。
}
