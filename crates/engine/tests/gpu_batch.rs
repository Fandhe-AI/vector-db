//! `gpu_batch.rs::GpuBatchBackend` の結合テスト（TASK-128〜130・対象ビヘイビア:
//! CORE-6, 8, 16。ポインタ: Issue #178）。
//!
//! 実行環境に GPU が無い場合（CI の GitHub ホステッド runner 等）でも意味の
//! ある回帰にするため、環境条件で両分岐を検証する（TASK-128 設計方針 §3.5。
//! skip・ignore にはしない）:
//! - GPU 初期化に失敗する環境: `FallbackBatchEngine::build_with_gpu` が
//!   `FallbackEvent{reason: Init, target: "cpu-simd"}` を 1 件通知し、結果が
//!   CPU オラクル（`CpuScalarProvider`）と一致すること（CORE-8 の実バックエンド
//!   に対する回帰）
//! - GPU 初期化に成功する環境: GPU 結果が CPU オラクルと一致し（id 集合一致・
//!   スコア相対誤差 1e-3 以内）、複数テナント混在バッチで混入 0 件・奇数次元を
//!   含めて検証する

use std::sync::{Arc, Mutex};

use engine::batch_fallback::{BatchBackend, FallbackBatchEngine, FallbackEvent, FallbackObserver};
use engine::batch_search::BatchQuery;
use engine::gpu_batch::{GpuBatchBackend, GpuF32ContrastBackend};
use engine::kernel::{CandidateHit, CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::storage::Visibility;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant id")
}

/// `Visibility::Private` を許可する `PolicyContext`（既定の
/// `PolicyContext::new` は `Public` のみ許可するため、自テナントの
/// `Private` 行を見るにはこちらを使う）。
fn ctx_with_private(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private, Visibility::Public])
        .expect("valid tenant id")
}

#[derive(Clone, Default)]
struct RecordingObserver(Arc<Mutex<Vec<FallbackEvent>>>);

impl RecordingObserver {
    fn events(&self) -> Vec<FallbackEvent> {
        self.0.lock().expect("lock").clone()
    }
}

impl FallbackObserver for RecordingObserver {
    fn on_fallback(&self, event: FallbackEvent) {
        self.0.lock().expect("lock").push(event);
    }
}

/// 常駐行列の元データ 1 式（`GpuBatchBackend`・CPU オラクルの双方に同じ
/// フィクスチャを渡すための束ね）。
struct Fixture {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
}

/// 4 行・dim=2・tenant-a/tenant-b が各 2 行のフィクスチャ（`tests/batch_fallback.rs`
/// と同一形状。オラクル比較を単純にするため揃える）。`private` が `true` の
/// 場合は各行を自テナントのみ可視（`Visibility::Private`）にする（テナント
/// 混入 0 件の検証用。`Visibility::Public` はテナント境界を跨いで可視になる
/// 設計のため、その検証には使えない）。
fn fixture(private: bool) -> Fixture {
    let visibility = if private {
        Visibility::Private
    } else {
        Visibility::Public
    };
    Fixture {
        ids: vec![1, 2, 3, 4],
        tenant_ids: vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ],
        visibilities: vec![visibility; 4],
        dim: 2,
        #[rustfmt::skip]
        vectors: vec![
            1.0, 0.0,
            0.0, 1.0,
            2.0, 0.0,
            0.0, 2.0,
        ],
    }
}

/// CPU オラクル（`kernel.rs::CpuScalarProvider`）で 1 クエリを検索する。
/// `GpuBatchBackend`/`FallbackBatchEngine` の結果が「縮退後の CPU-SIMD 経路と
/// 構成的に一致する」契約（CORE-8）の基準として使う。`SearchInput` は
/// `core.rs` が可視性フィルタ済みの行だけを渡す契約（`kernel.rs` モジュール
/// ドキュメント参照）のため、本関数側で `PolicyContext::is_visible` により
/// 可視行だけへ絞り込んでから渡す。
fn cpu_oracle(fx: &Fixture, query: &[f32], k: usize, ctx: &PolicyContext) -> Vec<CandidateHit> {
    let provider = CpuScalarProvider;
    let mut visible_ids: Vec<u64> = Vec::new();
    let mut visible_vectors: Vec<f32> = Vec::new();
    for (((id, tenant), vis), vec) in fx
        .ids
        .iter()
        .zip(fx.tenant_ids.iter())
        .zip(fx.visibilities.iter())
        .zip(fx.vectors.chunks(fx.dim))
    {
        if ctx.is_visible(tenant, *vis) {
            visible_ids.push(*id);
            visible_vectors.extend_from_slice(vec);
        }
    }
    provider
        .search(SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: fx.dim as u32,
            query,
            k,
        })
        .expect("cpu oracle search should not fail on well-formed fixture")
}

/// GPU が初期化できない環境での CORE-8 回帰（実バックエンドに対する初期化
/// 失敗→縮退）。GPU が使える環境ではこのテスト自体は何も主張せず終了する
/// （成功パスは下の `*_when_gpu_available` 系テストが担う）。
#[test]
fn build_with_gpu_falls_back_to_cpu_when_gpu_unavailable() {
    let fx = fixture(false);
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build_with_gpu(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
        Box::new(observer.clone()),
    )
    .expect("build_with_gpu should not fail on well-formed fixture regardless of gpu availability");

    let events = observer.events();
    if events.is_empty() {
        eprintln!("gpu available in this environment; init-failure branch not exercised here");
        return;
    }

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason.to_string(), "init");
    assert_eq!(events[0].target, "cpu-simd");

    let query = [1.0f32, 1.0];
    let c = ctx("tenant-a");
    let batch_query = BatchQuery {
        vector: &query,
        k: 4,
        ctx: &c,
    };
    let hits = engine
        .batch_search(std::slice::from_ref(&batch_query))
        .expect("cpu fallback batch_search should succeed");
    let expected = cpu_oracle(&fx, &query, 4, &c);
    let mut actual_ids: Vec<u64> = hits[0].hits.iter().map(|h| h.id).collect();
    let mut expected_ids: Vec<u64> = expected.iter().map(|h| h.id).collect();
    actual_ids.sort_unstable();
    expected_ids.sort_unstable();
    assert_eq!(actual_ids, expected_ids);
}

/// GPU が利用可能な環境でのみ実走する: `GpuBatchBackend` 単体の結果が CPU
/// オラクルと一致し、複数テナント混在バッチで他テナントの id が混入しない
/// ことを検証する。
#[test]
fn gpu_backend_matches_cpu_oracle_when_gpu_available() {
    let fx = fixture(true);
    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let query_a = [1.0f32, 1.0];
    let query_b = [2.0f32, 0.5];
    let bq_a = BatchQuery {
        vector: &query_a,
        k: 4,
        ctx: &ctx_a,
    };
    let bq_b = BatchQuery {
        vector: &query_b,
        k: 4,
        ctx: &ctx_b,
    };
    let hits = backend
        .batch_search(&[bq_a, bq_b])
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), 2);

    // テナント混入 0 件: tenant-a のクエリ結果には id 1,2 のみ、tenant-b の
    // クエリ結果には id 3,4 のみが現れること。
    for hit in &hits[0].hits {
        assert!(
            hit.id == 1 || hit.id == 2,
            "unexpected id {} leaked into tenant-a result",
            hit.id
        );
    }
    for hit in &hits[1].hits {
        assert!(
            hit.id == 3 || hit.id == 4,
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
    }

    let expected_a = cpu_oracle(&fx, &query_a, 4, &ctx_a);
    let mut actual_a: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    let mut expected_a_sorted: Vec<(u64, f32)> =
        expected_a.iter().map(|h| (h.id, h.score)).collect();
    actual_a.sort_by_key(|(id, _)| *id);
    expected_a_sorted.sort_by_key(|(id, _)| *id);
    assert_eq!(actual_a.len(), expected_a_sorted.len());
    for ((aid, ascore), (eid, escore)) in actual_a.iter().zip(expected_a_sorted.iter()) {
        assert_eq!(aid, eid);
        assert!(
            (ascore - escore).abs() < 1e-3,
            "score mismatch for id {aid}: gpu={ascore} cpu={escore}"
        );
    }
}

/// 奇数次元（`dim` が 2 で割り切れない）でも GPU 経路がパディングを正しく
/// 扱うこと（`ResidentMatrix::build` の f16 パックは奇数次元の最終ペアを
/// 0 埋めする。`gpu_batch.rs` のクエリ側パディングも同じ規約に揃える）。
#[test]
fn gpu_backend_handles_odd_dimension_when_gpu_available() {
    let ids = [1u64, 2];
    let tenant_ids = ["t".to_string(), "t".to_string()];
    let visibilities = [Visibility::Public, Visibility::Public];
    let dim = 3;
    let vectors = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0];
    let matrix = engine::batch_search::ResidentMatrix::build(
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
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let c = ctx("t");
    let query = [1.0f32, 1.0, 0.0];
    let bq = BatchQuery {
        vector: &query,
        k: 2,
        ctx: &c,
    };
    let hits = backend
        .batch_search(std::slice::from_ref(&bq))
        .expect("gpu batch_search should succeed once the device initialized");
    let mut scored: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    scored.sort_by_key(|(id, _)| *id);
    assert_eq!(scored.len(), 2);
    assert!((scored[0].1 - 1.0).abs() < 1e-3);
    assert!((scored[1].1 - 1.0).abs() < 1e-3);
}

// --- CORE-16（GPU 常駐コピーの f16 パック vs f32 常駐の A/B 対照経路。
// Issue #234・ポインタ: `docs/spec/04-behavior/core-engine.md` CORE-16）の
// 対照バックエンド `GpuF32ContrastBackend` に対する結合テスト。上の
// `GpuBatchBackend`（f16 パック常駐）用テストと同じ設計方針（GPU 有無の
// 両分岐を検証・skip/ignore にしない）を踏襲する。

/// GPU が初期化できない環境では `try_new` が panic せず `InitFailed` を返す
/// （CORE-8 と同様の「初期化失敗は `Err` で返す」契約の回帰）。GPU が使える
/// 環境ではこのテスト自体は何も主張せず終了する（成功パスは下の
/// `*_when_gpu_available` 系テストが担う）。
#[test]
fn f32_contrast_backend_try_new_fails_closed_when_gpu_unavailable() {
    let fx = fixture(false);
    match GpuF32ContrastBackend::try_new(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    ) {
        Ok(_) => {
            eprintln!("gpu available in this environment; init-failure branch not exercised here");
        }
        Err(e) => {
            // `InitFailed` 以外のバリアントへ写像されていないこと（`try_new` は
            // 初期化系の失敗のみを返す契約。`gpu_batch.rs::GpuBatchBackend::try_new`
            // と同じ契約）。
            assert!(
                matches!(e, engine::batch_fallback::BatchBackendError::InitFailed(_)),
                "unexpected error variant on init failure: {e:?}"
            );
        }
    }
}

/// GPU が利用可能な環境でのみ実走する: `GpuF32ContrastBackend` 単体の結果が
/// CPU オラクルと一致し、複数テナント混在バッチで他テナントの id が混入しない
/// ことを検証する（`GpuBatchBackend` 側の同名テストと対になる回帰）。f32
/// 常駐は f16 パックのような量子化誤差を持たないため、許容誤差を f16 経路
/// より厳しくする。
#[test]
fn f32_contrast_backend_matches_cpu_oracle_when_gpu_available() {
    let fx = fixture(true);
    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");

    let backend = match GpuF32ContrastBackend::try_new(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let query_a = [1.0f32, 1.0];
    let query_b = [2.0f32, 0.5];
    let bq_a = BatchQuery {
        vector: &query_a,
        k: 4,
        ctx: &ctx_a,
    };
    let bq_b = BatchQuery {
        vector: &query_b,
        k: 4,
        ctx: &ctx_b,
    };
    let hits = backend
        .batch_search(&[bq_a, bq_b])
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), 2);

    for hit in &hits[0].hits {
        assert!(
            hit.id == 1 || hit.id == 2,
            "unexpected id {} leaked into tenant-a result",
            hit.id
        );
    }
    for hit in &hits[1].hits {
        assert!(
            hit.id == 3 || hit.id == 4,
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
    }

    let expected_a = cpu_oracle(&fx, &query_a, 4, &ctx_a);
    let mut actual_a: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    let mut expected_a_sorted: Vec<(u64, f32)> =
        expected_a.iter().map(|h| (h.id, h.score)).collect();
    actual_a.sort_by_key(|(id, _)| *id);
    expected_a_sorted.sort_by_key(|(id, _)| *id);
    assert_eq!(actual_a.len(), expected_a_sorted.len());
    for ((aid, ascore), (eid, escore)) in actual_a.iter().zip(expected_a_sorted.iter()) {
        assert_eq!(aid, eid);
        // f32 常駐は f16 量子化を経ないため、GPU vs CPU オラクルの誤差は
        // 浮動小数点演算順序差のみに由来する。f16 経路の許容誤差（1e-3）より
        // 厳しい 1e-5 で一致を確認する。
        assert!(
            (ascore - escore).abs() < 1e-5,
            "score mismatch for id {aid}: gpu={ascore} cpu={escore}"
        );
    }
}

/// 奇数次元でも f32 常駐対照経路が正しく扱うこと（f32 経路はパディング不要
/// だが、`ResidentMatrix::build` 自体は奇数次元でも f16 パック側で 0 埋めする
/// ため、対照経路がその影響を受けずに `dim` そのものをストライドとして扱う
/// ことを確認する）。
#[test]
fn f32_contrast_backend_handles_odd_dimension_when_gpu_available() {
    let ids = [1u64, 2];
    let tenant_ids = ["t".to_string(), "t".to_string()];
    let visibilities = [Visibility::Public, Visibility::Public];
    let dim = 3;
    let vectors = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0];

    let backend =
        match GpuF32ContrastBackend::try_new(&ids, &tenant_ids, &visibilities, dim, &vectors) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };

    let c = ctx("t");
    let query = [1.0f32, 1.0, 0.0];
    let bq = BatchQuery {
        vector: &query,
        k: 2,
        ctx: &c,
    };
    let hits = backend
        .batch_search(std::slice::from_ref(&bq))
        .expect("gpu batch_search should succeed once the device initialized");
    let mut scored: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    scored.sort_by_key(|(id, _)| *id);
    assert_eq!(scored.len(), 2);
    assert!((scored[0].1 - 1.0).abs() < 1e-5);
    assert!((scored[1].1 - 1.0).abs() < 1e-5);
}

// --- Issue #532: クエリタイル化（1 dispatch で複数クエリを処理）の結合テスト ---

/// テスト専用の決定的 xorshift32。`rand` 系クレートを追加せず
/// （依存最小方針）、GPU vs CPU オラクル比較に使う再現可能な浮動小数点値を
/// 生成する。
struct Xorshift32(u32);

impl Xorshift32 {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// `[-1.0, 1.0)` の f32 を返す。
    fn next_f32(&mut self) -> f32 {
        let bits = self.next_u32();
        ((bits as f64 / u32::MAX as f64) * 2.0 - 1.0) as f32
    }
}

/// 行・クエリともに `Xorshift32` で生成した固定次元のベクトル集合
/// （odd dim を含む。dim チャンク化は本 Issue の対象外だが、行数チャンク化
/// との組み合わせを兼ねて奇数次元でも検証する）。
struct MultiQueryFixture {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
    queries: Vec<Vec<f32>>,
}

fn multi_query_fixture(
    row_count: usize,
    dim: usize,
    query_count: usize,
    seed: u32,
) -> MultiQueryFixture {
    let mut rng = Xorshift32(seed | 1);
    let mut vectors = Vec::with_capacity(row_count * dim);
    for _ in 0..row_count * dim {
        vectors.push(rng.next_f32());
    }
    let mut queries = Vec::with_capacity(query_count);
    for _ in 0..query_count {
        let mut q = Vec::with_capacity(dim);
        for _ in 0..dim {
            q.push(rng.next_f32());
        }
        queries.push(q);
    }
    MultiQueryFixture {
        ids: (1..=row_count as u64).collect(),
        tenant_ids: vec!["tenant-a".to_string(); row_count],
        visibilities: vec![Visibility::Public; row_count],
        dim,
        vectors,
        queries,
    }
}

/// GPU_QUERY_TILE_MAX（本モジュール doc 記載の実装既定値: 16）の非倍数本の
/// クエリを 1 バッチで投げ、複数タイル・複数行チャンクにまたがっても各クエリの
/// Top-k が CPU オラクルと id 集合一致・スコア 1e-3 以内で一致することを確認する
/// （Issue #532・R1〜R4: 1 dispatch で複数クエリを処理する構造への変更が
/// 結果の正しさに影響しないことの回帰）。
#[test]
fn gpu_backend_multi_tile_matches_cpu_oracle_when_gpu_available() {
    // GPU_QUERY_TILE_MAX=16 の非倍数（タイル境界をまたぐ）本数。
    let fx = multi_query_fixture(300, 131, 21, 0x532_2026);
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let c = ctx("tenant-a");
    let batch_queries: Vec<BatchQuery<'_>> = fx
        .queries
        .iter()
        .map(|v| BatchQuery {
            vector: v,
            k: 5,
            ctx: &c,
        })
        .collect();
    let hits = backend
        .batch_search(&batch_queries)
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), fx.queries.len());

    let simple_fx = Fixture {
        ids: fx.ids.clone(),
        tenant_ids: fx.tenant_ids.clone(),
        visibilities: fx.visibilities.clone(),
        dim: fx.dim,
        vectors: fx.vectors.clone(),
    };
    for (qi, query) in fx.queries.iter().enumerate() {
        let expected = cpu_oracle(&simple_fx, query, 5, &c);
        let mut actual: Vec<(u64, f32)> = hits[qi].hits.iter().map(|h| (h.id, h.score)).collect();
        let mut expected_sorted: Vec<(u64, f32)> =
            expected.iter().map(|h| (h.id, h.score)).collect();
        actual.sort_by_key(|(id, _)| *id);
        expected_sorted.sort_by_key(|(id, _)| *id);
        assert_eq!(
            actual.len(),
            expected_sorted.len(),
            "hit count mismatch for query {qi}"
        );
        for ((aid, ascore), (eid, escore)) in actual.iter().zip(expected_sorted.iter()) {
            assert_eq!(aid, eid, "id mismatch for query {qi}");
            // dim=131 の非正規化ランダムベクトルはスコアの絶対値が
            // 既存テスト（dim=2 の単位ベクトル）より大きく（数〜十数程度）、
            // f16 量子化の絶対誤差もスコアの大きさに比例して増える。他
            // テストの絶対許容誤差（1e-3）ではなく、スコアの大きさに応じた
            // 相対許容誤差を使う。
            let tolerance = 5e-3 * (1.0 + escore.abs());
            assert!(
                (ascore - escore).abs() < tolerance,
                "score mismatch for query {qi} id {aid}: gpu={ascore} cpu={escore}"
            );
        }
    }
}

/// 複数テナントのクエリを交互に並べた 1 バッチで、`group_queries_by_ctx`
/// によるグループ化後もタイル境界を越えてテナントが混入しないことを確認する
/// （Issue #532・R1。private の `group_queries_by_ctx` は直接呼べないため、
/// `GpuBatchBackend::batch_search` 経由の結果でテナント境界を検証する）。
#[test]
fn gpu_backend_mixed_tenant_batch_has_no_cross_tenant_leak_when_gpu_available() {
    let fx = fixture(true);
    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let query_a = [1.0f32, 1.0];
    let query_b = [2.0f32, 0.5];
    // tenant-a, tenant-b, tenant-a を交互に並べ、`group_queries_by_ctx` が
    // 入力順を保ったまま tenant-a を index [0, 2] へ、tenant-b を index [1]
    // へ束ねることを間接的に確認する（結果の入力順復元が壊れていれば
    // どこかの id セットが入れ替わって検出される）。
    let batch_queries = vec![
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
        BatchQuery {
            vector: &query_a,
            k: 4,
            ctx: &ctx_a,
        },
    ];
    let hits = backend
        .batch_search(&batch_queries)
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), 3);

    for (qi, expected_ids) in [(0usize, [1u64, 2]), (2, [1, 2])] {
        for hit in &hits[qi].hits {
            assert!(
                expected_ids.contains(&hit.id),
                "unexpected id {} leaked into tenant-a result at query {qi}",
                hit.id
            );
        }
    }
    for hit in &hits[1].hits {
        assert!(
            hit.id == 3 || hit.id == 4,
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
    }
    // index 0 と index 2 は同一クエリ・同一 ctx なので結果も一致するはず
    // （入力順復元・タイル分割の実装バグがあれば崩れる）。
    let mut hits0: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    let mut hits2: Vec<(u64, f32)> = hits[2].hits.iter().map(|h| (h.id, h.score)).collect();
    hits0.sort_by_key(|(id, _)| *id);
    hits2.sort_by_key(|(id, _)| *id);
    assert_eq!(hits0, hits2);
}
