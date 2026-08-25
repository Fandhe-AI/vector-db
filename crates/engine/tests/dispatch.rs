//! `dispatch.rs::select_execution_path` の決定表回帰テスト
//! （TASK-155・対象ビヘイビア: CORE-11, CORE-12）。
//!
//! unit テスト（`dispatch.rs` 内）が GPU capability を伴う分岐（`GpuCapability::proven`
//! が `pub(crate)` のため crate 内からしか呼べない）を含む全網羅走査を担うのに対し、
//! 本ファイルは「crate 外から到達できる公開 API だけで何が検証できるか」を担う結合
//! テストである。`DispatchInput` のフィールドは private・コンストラクタ経由でのみ
//! 構築できるため、本ファイルは `for_single_query`／`for_batch`（GPU capability には
//! 常に `None` を渡す）のみを使う。GPU capability を crate 外から構築できないこと
//! 自体が CORE-12（未検証の GPU capability による経路上書きを防ぐ）の結合テストでも
//! ある。決定性・CORE-12（外部上書き機構の不存在）のソース検査も担う。

use engine::batch_search::{should_aggregate_into_batch, MAX_BATCH_DIM, MAX_BATCH_QUERIES};
use engine::dispatch::{
    select_execution_path, DetectedIsa, DispatchError, DispatchInput, ExecutionPath, SimdWidth,
    SINGLE_QUERY_MAX_DIM,
};

fn isa_variants() -> [DetectedIsa; 4] {
    [
        DetectedIsa::Scalar,
        DetectedIsa::Neon,
        DetectedIsa::Avx2Fma,
        DetectedIsa::Avx512,
    ]
}

fn expected_simd_width(isa: DetectedIsa) -> SimdWidth {
    match isa {
        DetectedIsa::Scalar => SimdWidth::Scalar,
        DetectedIsa::Neon => SimdWidth::W128,
        DetectedIsa::Avx2Fma => SimdWidth::W256,
        DetectedIsa::Avx512 => SimdWidth::W512,
    }
}

/// GPU capability なし（`for_batch(None, ..)`）での決定表網羅走査: `isa` ×
/// `batch_size`（1・2・上限）の直積。`for_batch` 経由は件数によらず常にバッチ扱いに
/// なる（決定表ルール 1）が、GPU capability が `None` のため期待経路は常に
/// `CpuSimd`（決定表ルール 5）になる。
#[test]
fn decision_table_without_gpu_capability_always_selects_cpu_simd_for_batch() {
    let batch_sizes = [1usize, 2, MAX_BATCH_QUERIES];

    for isa in isa_variants() {
        for batch_size in batch_sizes {
            let input = DispatchInput::for_batch(None, isa, 8, batch_size).expect("valid input");
            let result = select_execution_path(input);
            assert_eq!(
                result,
                Ok(ExecutionPath::CpuSimd {
                    width: expected_simd_width(isa)
                }),
                "isa={isa:?} batch_size={batch_size}"
            );
        }
    }
}

/// GPU capability なし（`for_single_query`）での決定表網羅走査: `isa` ×
/// `pending_after_pop` の直積。GPU capability が常に `None`（`for_single_query` は
/// 引数に取らない）のため、`pending_after_pop` の値によらず期待経路は常に
/// `CpuSimd` になる。
#[test]
fn decision_table_without_gpu_capability_always_selects_cpu_simd_for_single_query() {
    for isa in isa_variants() {
        for pending_after_pop in [false, true] {
            let input =
                DispatchInput::for_single_query(isa, 8, pending_after_pop).expect("valid input");
            let result = select_execution_path(input);
            assert_eq!(
                result,
                Ok(ExecutionPath::CpuSimd {
                    width: expected_simd_width(isa)
                }),
                "isa={isa:?} pending={pending_after_pop}"
            );
        }
    }
}

/// `for_batch` の `batch_size` 検証（0・上限超過）。
#[test]
fn for_batch_rejects_invalid_batch_size() {
    for batch_size in [0usize, MAX_BATCH_QUERIES + 1] {
        assert_eq!(
            DispatchInput::for_batch(None, DetectedIsa::Scalar, 8, batch_size).unwrap_err(),
            DispatchError::InvalidBatchSize {
                batch_size,
                max: MAX_BATCH_QUERIES,
            }
        );
    }
}

/// fail-closed: `for_batch` は次元 0・`MAX_BATCH_DIM` 超過を拒否する。
#[test]
fn for_batch_dim_zero_and_over_limit_are_rejected() {
    for dim in [0usize, MAX_BATCH_DIM + 1] {
        assert_eq!(
            DispatchInput::for_batch(None, DetectedIsa::Scalar, dim, 1).unwrap_err(),
            DispatchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            }
        );
    }
}

/// fail-closed: `for_single_query` は次元 0・`MAX_EMBEDDING_DIM` 超過を拒否する
/// （単発クエリ経路は `MAX_BATCH_DIM` より広い `MAX_EMBEDDING_DIM` を上限に使う。
/// `dispatch.rs` モジュールドキュメント「実配線」の項参照）。
#[test]
fn for_single_query_dim_zero_and_over_limit_are_rejected() {
    let max = SINGLE_QUERY_MAX_DIM;
    for dim in [0usize, max + 1] {
        assert_eq!(
            DispatchInput::for_single_query(DetectedIsa::Scalar, dim, false).unwrap_err(),
            DispatchError::InvalidDim { dim, max }
        );
    }
}

/// 上限は inclusive（`dim == MAX_BATCH_DIM` は受理）であることを、上記の
/// `MAX_BATCH_DIM + 1` が拒否されるケースと対にして固定する
/// （`>` と `>=` の取り違え回帰を検出する境界値テスト）。
#[test]
fn dim_at_batch_limit_is_accepted() {
    let input =
        DispatchInput::for_batch(None, DetectedIsa::Scalar, MAX_BATCH_DIM, 1).expect("valid dim");
    assert!(select_execution_path(input).is_ok());
}

/// 決定性: 同一入力を 2 回呼び出しても結果が一致すること（純関数性の回帰）。
#[test]
fn same_input_is_deterministic_across_repeated_calls() {
    let inputs = [
        DispatchInput::for_batch(None, DetectedIsa::Avx512, 128, 16).expect("valid input"),
        DispatchInput::for_single_query(DetectedIsa::Neon, 3, true).expect("valid input"),
        DispatchInput::for_single_query(DetectedIsa::Avx2Fma, 3, false).expect("valid input"),
    ];
    for input in inputs {
        let results: Vec<_> = (0..5).map(|_| select_execution_path(input)).collect();
        assert!(
            results.windows(2).all(|w| w[0] == w[1]),
            "results diverged for input={input:?}: {results:?}"
        );
    }
}

/// 既存判定との整合: 動的窓行（単発クエリ + `pending_after_pop`）が
/// `batch_search::should_aggregate_into_batch` の結果と一致すること（GPU capability
/// なしのため、`should_aggregate_into_batch` が `true` でも GPU へは昇格せず常に
/// `CpuSimd` になる。GPU 昇格の回帰は crate 内 unit テストが担う）。
#[test]
fn dynamic_window_row_without_gpu_capability_stays_on_cpu_simd() {
    for pending_after_pop in [false, true] {
        let input = DispatchInput::for_single_query(DetectedIsa::Scalar, 8, pending_after_pop)
            .expect("valid input");
        // `should_aggregate_into_batch` 自体の結果は本テストの主張に影響しない
        // （GPU capability が常に `None` のため、いずれにせよ `CpuSimd` になる）が、
        // `dispatch.rs`／`batch_search.rs` 双方のコメント契約（二重管理を避ける）に
        // 存在を明記する意図で呼んでおく。
        let _treated_as_batch = should_aggregate_into_batch(pending_after_pop);
        assert_eq!(
            select_execution_path(input),
            Ok(ExecutionPath::CpuSimd {
                width: SimdWidth::Scalar
            })
        );
    }
}

/// CORE-12: 経路選択への外部入力上書き機構（環境変数・設定ファイル読み取り等）が
/// ソース上に存在しないことを確認する。実装漏れではなく設計方針そのものであるため、
/// 通常のユニットテストでは検出できない「不在」を直接検査する（spec の
/// 「設定読み取り経路の不存在確認テスト」に対応するポインタ表記。spec 本文は
/// 転記しない）。
#[test]
fn dispatch_source_has_no_external_override_entry_points() {
    let source = include_str!("../src/dispatch.rs");

    // 検査対象トークン一覧（理由付き）:
    // - `std::env` / `env::var` / `env::var_os`: 環境変数読み取り経路
    // - `std::fs` / `read_to_string` / `File::open`: 設定ファイル読み取り経路
    // - `option_env!`: コンパイル時環境変数埋め込み（実行時上書きの類似経路）
    let forbidden_tokens = [
        "std::env",
        "env::var",
        "env::var_os",
        "std::fs",
        "read_to_string",
        "File::open",
        "option_env!",
    ];

    for token in forbidden_tokens {
        assert!(
            !source.contains(token),
            "dispatch.rs must not contain external override entry point token: {token}"
        );
    }
}
