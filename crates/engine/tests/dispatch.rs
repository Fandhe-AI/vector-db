//! `dispatch.rs::select_execution_path` の決定表回帰テスト
//! （TASK-155・対象ビヘイビア: CORE-11, CORE-12）。
//!
//! unit テスト（`dispatch.rs` 内）が代表ケースを検証するのに対し、本ファイルは
//! 決定表全分岐の網羅走査・決定性・CORE-12（外部上書き機構の不存在）の
//! ソース検査を担う結合テストである。

use engine::batch_search::{should_aggregate_into_batch, MAX_BATCH_DIM, MAX_BATCH_QUERIES};
use engine::dispatch::{
    select_execution_path, DetectedIsa, DispatchError, DispatchInput, ExecutionPath, QueryDtype,
    SimdWidth,
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

/// 決定表全分岐網羅: `gpu_available` × `isa` × `batch_size`（1・2・上限・上限+1・0）×
/// `pending_after_pop` の直積を走査し、期待経路と一致することを検証する。`dtype` は
/// 現状 `QueryDtype::F32` の 1 variant のみのため走査対象に含めず固定する
/// （variant が増えた場合はここへ追加する）。
#[test]
fn decision_table_covers_full_input_product() {
    let batch_sizes = [0usize, 1, 2, MAX_BATCH_QUERIES, MAX_BATCH_QUERIES + 1];

    for gpu_available in [false, true] {
        for isa in isa_variants() {
            for batch_size in batch_sizes {
                for pending_after_pop in [false, true] {
                    let input = DispatchInput {
                        gpu_available,
                        isa,
                        dim: 8,
                        batch_size,
                        dtype: QueryDtype::F32,
                        pending_after_pop,
                    };
                    let result = select_execution_path(input);

                    if batch_size == 0 || batch_size > MAX_BATCH_QUERIES {
                        assert_eq!(
                            result,
                            Err(DispatchError::InvalidBatchSize {
                                batch_size,
                                max: MAX_BATCH_QUERIES,
                            }),
                            "input={input:?}"
                        );
                        continue;
                    }

                    let treated_as_batch =
                        batch_size >= 2 || should_aggregate_into_batch(pending_after_pop);
                    let expected = if treated_as_batch && gpu_available {
                        ExecutionPath::Gpu
                    } else {
                        ExecutionPath::CpuSimd {
                            width: expected_simd_width(isa),
                        }
                    };
                    assert_eq!(result, Ok(expected), "input={input:?}");
                }
            }
        }
    }
}

/// fail-closed: 次元 0・上限超過が `Err` になることを、上記網羅走査とは独立に
/// 単独でも確認する（batch_size 側の検証と混同しないため次元固定・0/正常/超過のみ走査）。
#[test]
fn dim_zero_and_over_limit_are_rejected_independently_of_batch_size() {
    for dim in [0usize, MAX_BATCH_DIM + 1] {
        let input = DispatchInput {
            gpu_available: true,
            isa: DetectedIsa::Scalar,
            dim,
            batch_size: 1,
            dtype: QueryDtype::F32,
            pending_after_pop: false,
        };
        assert_eq!(
            select_execution_path(input),
            Err(DispatchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            })
        );
    }
}

/// 上限は inclusive（`dim == MAX_BATCH_DIM` は受理）であることを、上記の
/// `MAX_BATCH_DIM + 1` が拒否されるケースと対にして固定する
/// （`>` と `>=` の取り違え回帰を検出する境界値テスト）。
#[test]
fn dim_at_limit_is_accepted() {
    let input = DispatchInput {
        gpu_available: false,
        isa: DetectedIsa::Scalar,
        dim: MAX_BATCH_DIM,
        batch_size: 1,
        dtype: QueryDtype::F32,
        pending_after_pop: false,
    };
    assert!(select_execution_path(input).is_ok());
}

/// 決定性: 同一入力を 2 回呼び出しても結果が一致すること（純関数性の回帰）。
#[test]
fn same_input_is_deterministic_across_repeated_calls() {
    let inputs = [
        DispatchInput {
            gpu_available: true,
            isa: DetectedIsa::Avx512,
            dim: 128,
            batch_size: 16,
            dtype: QueryDtype::F32,
            pending_after_pop: false,
        },
        DispatchInput {
            gpu_available: false,
            isa: DetectedIsa::Neon,
            dim: 3,
            batch_size: 1,
            dtype: QueryDtype::F32,
            pending_after_pop: true,
        },
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
/// `batch_search::should_aggregate_into_batch` の結果と一致すること
/// （二重管理を避けるという `dispatch.rs`／`batch_search.rs` 双方のコメント契約の
/// 回帰確認）。
#[test]
fn dynamic_window_row_matches_should_aggregate_into_batch_for_all_gpu_availability() {
    for gpu_available in [false, true] {
        for pending_after_pop in [false, true] {
            let input = DispatchInput {
                gpu_available,
                isa: DetectedIsa::Scalar,
                dim: 8,
                batch_size: 1,
                dtype: QueryDtype::F32,
                pending_after_pop,
            };
            let treated_as_batch = should_aggregate_into_batch(pending_after_pop);
            let expected = if treated_as_batch && gpu_available {
                ExecutionPath::Gpu
            } else {
                ExecutionPath::CpuSimd {
                    width: SimdWidth::Scalar,
                }
            };
            assert_eq!(select_execution_path(input), Ok(expected));
        }
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
