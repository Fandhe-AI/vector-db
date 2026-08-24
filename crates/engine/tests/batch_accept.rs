//! TASK-130（バッチ高速化の受け入れ基準検証）の時間非依存な回帰テスト
//! （ポインタ: `docs/spec/05-tasks.md` TASK-130・対象ビヘイビア CORE-6, CORE-7,
//! CORE-16。`docs/spec/04-behavior/core-engine.md` ポインタ参照）。
//!
//! `batch_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/bench_accept.rs` と同様、実測タイマーに依存しない判定ロジックのみを
//! `#[path]` で取り込み `cargo test`（`make ci` 対象）で検証する）。
//!
//! 本ファイルの構成:
//! - `harness::accept` の新規判定ヘルパ（`check_degradation_within_limit`・
//!   `check_improvement_at_least`）の境界値・退化入力の単体検証
//! - CORE-16: f16 パック常駐経路（[`BatchEngine`]）と f32 厳密経路
//!   （[`CpuScalarProvider`]）の Top-k が Recall@k として非劣化であることの検証
//! - CORE-7: [`DynamicWindowAggregator`] を経由して集約した異テナント混在バッチが、
//!   `crates/engine/src/batch_search.rs` 内の直接呼び出しテストと同じテナント境界を
//!   保つことの検証（動的窓経由という不足していたカバレッジを追加する）

// 本テストは `accept`（受け入れ判定ヘルパ）・`rng`（決定的入力生成）を経由し、
// `protocol`/`ab`（実測タイマーに依存する部分）は経由しない。未到達の `pub` 項目が
// dead_code として警告されうるため許容する（`tests/bench_accept.rs` と同一の理由・
// 対処。`harness/mod.rs` 自体は変更しない）。
#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::accept::{
    check_degradation_within_limit, check_improvement_at_least, recall_at_k, worst_recall,
};
use harness::rng::DeterministicRng;
use harness::stats::BenchError;
use std::time::Duration;

use engine::batch_search::{BatchEngine, BatchQuery, DynamicWindowAggregator, ResidentMatrix};
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::storage::Visibility;

// ---------------------------------------------------------------------
// check_degradation_within_limit（CORE-7）
// ---------------------------------------------------------------------

#[test]
fn check_degradation_within_limit_accepts_no_degradation() {
    // candidate が baseline と同じ、または速い（劣化なし）場合は常に通過する。
    assert!(check_degradation_within_limit(
        Duration::from_millis(100),
        Duration::from_millis(100),
        0.0
    )
    .unwrap());
    assert!(check_degradation_within_limit(
        Duration::from_millis(100),
        Duration::from_millis(50),
        0.0
    )
    .unwrap());
}

#[test]
fn check_degradation_within_limit_accepts_degradation_at_or_below_pct() {
    // 110ms は 100ms の baseline に対してちょうど 10% の劣化。
    assert!(check_degradation_within_limit(
        Duration::from_millis(100),
        Duration::from_millis(110),
        10.0
    )
    .unwrap());
}

#[test]
fn check_degradation_within_limit_rejects_degradation_above_pct() {
    assert!(!check_degradation_within_limit(
        Duration::from_millis(100),
        Duration::from_millis(111),
        10.0
    )
    .unwrap());
}

#[test]
fn check_degradation_within_limit_rejects_zero_baseline() {
    let err =
        check_degradation_within_limit(Duration::ZERO, Duration::from_millis(1), 10.0).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));
}

#[test]
fn check_degradation_within_limit_rejects_non_finite_or_negative_max_pct() {
    let err = check_degradation_within_limit(
        Duration::from_millis(100),
        Duration::from_millis(100),
        f64::NAN,
    )
    .unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err = check_degradation_within_limit(
        Duration::from_millis(100),
        Duration::from_millis(100),
        -1.0,
    )
    .unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

// ---------------------------------------------------------------------
// check_improvement_at_least（CORE-6/CORE-16。本 PR では batch_bench.rs から
// 未接続だが判定ロジック自体は検証しておく）
// ---------------------------------------------------------------------

#[test]
fn check_improvement_at_least_accepts_improvement_at_or_above_pct() {
    // 90ms は 100ms の baseline に対してちょうど 10% の短縮。
    assert!(check_improvement_at_least(
        Duration::from_millis(100),
        Duration::from_millis(90),
        10.0
    )
    .unwrap());
}

#[test]
fn check_improvement_at_least_rejects_improvement_below_pct() {
    assert!(!check_improvement_at_least(
        Duration::from_millis(100),
        Duration::from_millis(91),
        10.0
    )
    .unwrap());
}

#[test]
fn check_improvement_at_least_rejects_when_candidate_is_slower() {
    assert!(!check_improvement_at_least(
        Duration::from_millis(100),
        Duration::from_millis(101),
        10.0
    )
    .unwrap());
}

#[test]
fn check_improvement_at_least_rejects_zero_baseline() {
    let err =
        check_improvement_at_least(Duration::ZERO, Duration::from_millis(1), 10.0).unwrap_err();
    assert!(matches!(err, BenchError::DegenerateRatio(_)));
}

#[test]
fn check_improvement_at_least_rejects_non_finite_or_non_positive_min_pct() {
    let err = check_improvement_at_least(
        Duration::from_millis(100),
        Duration::from_millis(50),
        f64::INFINITY,
    )
    .unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err =
        check_improvement_at_least(Duration::from_millis(100), Duration::from_millis(50), 0.0)
            .unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));

    let err =
        check_improvement_at_least(Duration::from_millis(100), Duration::from_millis(50), -1.0)
            .unwrap_err();
    assert!(matches!(err, BenchError::ProtocolViolation(_)));
}

// ---------------------------------------------------------------------
// CORE-16: f16 パック常駐経路（BatchEngine）vs f32 厳密経路（CpuScalarProvider）の
// Recall@k 非劣化。決定的シードで生成した単一テナントのコーパスに対し、両経路の
// Top-k を比較する（`parallel_bench.rs` の CORE-4 検証と同型のアプローチ）。
// ---------------------------------------------------------------------

const RECALL_ROW_COUNT: usize = 500;
const RECALL_DIM: usize = 32;
const RECALL_TOP_K: usize = 10;
const RECALL_QUERY_COUNT: usize = 20;
const RECALL_TENANT: &str = "task-130-recall-tenant";

/// 本テスト自身が定める regression baseline（spec の受け入れ基準そのものではなく、
/// f16 往復デコードによる意味のある品質劣化を検出するための実装内部の閾値）。
const MIN_RECALL_F16_VS_F32: f64 = 0.9;

#[test]
fn batch_engine_f16_packed_recall_does_not_degrade_vs_f32_exact() {
    let mut rng = DeterministicRng::new(7);
    let ids: Vec<u64> = (0..RECALL_ROW_COUNT as u64).collect();
    let tenant_ids: Vec<String> =
        std::iter::repeat_n(RECALL_TENANT.to_string(), RECALL_ROW_COUNT).collect();
    let visibilities: Vec<Visibility> =
        std::iter::repeat_n(Visibility::Public, RECALL_ROW_COUNT).collect();
    let mut vectors = Vec::with_capacity(RECALL_ROW_COUNT * RECALL_DIM);
    for _ in 0..RECALL_ROW_COUNT {
        vectors.extend(rng.next_vector(RECALL_DIM));
    }

    let matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, RECALL_DIM, &vectors)
        .expect("resident matrix must build for well-formed synthetic input");
    let batch_engine = BatchEngine::new(matrix);
    let reference = CpuScalarProvider;
    let ctx = PolicyContext::new(RECALL_TENANT).expect("valid tenant");

    let mut recalls = Vec::with_capacity(RECALL_QUERY_COUNT);
    for _ in 0..RECALL_QUERY_COUNT {
        let query = rng.next_vector(RECALL_DIM);

        let expected: Vec<u64> = reference
            .search(SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: RECALL_DIM as u32,
                query: &query,
                k: RECALL_TOP_K,
            })
            .expect("reference search must succeed for well-formed synthetic input")
            .into_iter()
            .map(|hit| hit.id)
            .collect();

        let batch_queries = vec![BatchQuery {
            vector: &query,
            k: RECALL_TOP_K,
            ctx: &ctx,
        }];
        let actual: Vec<u64> = batch_engine
            .batch_search(&batch_queries)
            .expect("batch search must succeed for well-formed synthetic input")
            .into_iter()
            .next()
            .expect("exactly one query submitted")
            .hits
            .into_iter()
            .map(|hit| hit.id)
            .collect();

        recalls.push(recall_at_k(&expected, &actual).expect("non-empty reference top-k"));
    }

    let recall_min =
        worst_recall(&recalls).expect("RECALL_QUERY_COUNT queries yield a non-empty recall list");
    assert!(
        recall_min >= MIN_RECALL_F16_VS_F32,
        "f16 packed resident matrix must not degrade Recall@{RECALL_TOP_K} below {MIN_RECALL_F16_VS_F32}: got {recall_min}"
    );
}

// ---------------------------------------------------------------------
// CORE-7: DynamicWindowAggregator 経由で集約した異テナント混在バッチのテナント分離。
// `crates/engine/src/batch_search.rs` の既存テストは `BatchQuery` を直接組み立てて
// `batch_search` を呼ぶが、動的窓（`DynamicWindowAggregator::push`/`drain`）を経由した
// 集約経路そのものを通す回帰テストが不足していたため追加する（実装と経路独立の
// 検査器で確認する既存パターンを踏襲）。
// ---------------------------------------------------------------------

#[test]
fn dynamic_window_aggregated_batch_excludes_other_tenant_rows() {
    // tenant-a: id=1,2 / tenant-b: id=3,4。dim=2。全行 Public
    // （`batch_search.rs::build_two_tenant_matrix` と同一構成）。
    let ids = vec![1u64, 2, 3, 4];
    let tenant_ids = vec![
        "tenant-a".to_string(),
        "tenant-a".to_string(),
        "tenant-b".to_string(),
        "tenant-b".to_string(),
    ];
    let visibilities = vec![Visibility::Public; 4];
    let vectors = vec![1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0];
    let matrix =
        ResidentMatrix::build(&ids, &tenant_ids, &visibilities, 2, &vectors).expect("valid matrix");
    let engine = BatchEngine::new(matrix);

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
    let query_a = vec![1.0f32, 0.0];
    let query_b = vec![0.0f32, 1.0];

    // 動的窓経由で異テナントの 2 クエリを 1 バッチへ集約する（`should_aggregate_into_batch`
    // が true を返すキュー取り出し文脈を模す）。
    let mut window = DynamicWindowAggregator::new();
    window.push(query_a).expect("push must succeed");
    window.push(query_b).expect("push must succeed");
    assert_eq!(window.len(), 2);
    let drained = window.drain();
    assert!(window.is_empty(), "window must reset after drain");

    let batch_queries = vec![
        BatchQuery {
            vector: &drained[0],
            k: 10,
            ctx: &ctx_a,
        },
        BatchQuery {
            vector: &drained[1],
            k: 10,
            ctx: &ctx_b,
        },
    ];
    let results = engine
        .batch_search(&batch_queries)
        .expect("batch search must succeed");
    assert_eq!(results.len(), 2);

    // 独立検査器: 返った id が期待テナントの id 集合に含まれるかを、engine 内部の
    // マスク実装を経由せず直接確認する（`batch_search.rs` の同種テストと同一方針）。
    let tenant_a_ids: std::collections::HashSet<u64> = [1, 2].into_iter().collect();
    let tenant_b_ids: std::collections::HashSet<u64> = [3, 4].into_iter().collect();
    for hit in &results[0].hits {
        assert!(
            tenant_a_ids.contains(&hit.id) && !tenant_b_ids.contains(&hit.id),
            "tenant-a ctx leaked a tenant-b row via dynamic window aggregation: id={}",
            hit.id
        );
    }
    for hit in &results[1].hits {
        assert!(
            tenant_b_ids.contains(&hit.id) && !tenant_a_ids.contains(&hit.id),
            "tenant-b ctx leaked a tenant-a row via dynamic window aggregation: id={}",
            hit.id
        );
    }
    assert!(!results[0].hits.is_empty());
    assert!(!results[1].hits.is_empty());
}
