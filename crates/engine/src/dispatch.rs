//! 検索カーネルの実行経路選択ディスパッチ決定表（TASK-155・対象ビヘイビア: CORE-11, CORE-12）。
//!
//! `kernel.rs`（CORE-13 の provider 差し替え点）・`search_engine.rs`（CORE-9 の既定
//! provider 選択）・`batch_search.rs`（CORE-6, CORE-7 のバッチ実行エンジン）・
//! `batch_fallback.rs`（CORE-8 の GPU→CPU 縮退）は、それぞれ「入力 → 実行経路」の
//! 判断を個別に持ちうる構造だった。本モジュールはその判断を [`select_execution_path`]
//! という副作用なしの純関数へ 1 箇所に集約し、決定表として一意に定義する
//! （CORE-11）。呼び出し元（`core.rs::EngineCore::open` や `batch_search.rs` の
//! バッチ実行経路）は、実行前にここへ入力を渡して経路を確定してから、
//! 対応する provider・エンジンを構築・実行する想定である。
//!
//! # 実配線調査で判明した具体的な阻害要因（未接続のまま。TASK-155 レビュー起因）
//!
//! 単発クエリ経路（`core.rs::EngineCore::open`・`search_engine.rs`、CORE-9）への配線を
//! 試みたが、以下 2 点により安全に接続できないことを確認した:
//!
//! - `kernel.rs` が提供する provider は現状 `CpuScalarProvider`／
//!   `ParallelSearchProvider`（スレッド構成の違いのみで、ISA 別の SIMD 幅・GPU 実装を
//!   持たない）に限られ、本モジュールが返す `ExecutionPath::CpuSimd { width }`／
//!   `ExecutionPath::Gpu` を実際に分岐させる先が存在しない。
//! - `dim` の検証上限が不一致（[`DispatchInput::dim`] のドキュメント参照）。本モジュールは
//!   `batch_search::MAX_BATCH_DIM`（8_192）を使うが、単発クエリ経路は独立してより大きい
//!   `storage::MAX_EMBEDDING_DIM`（65_536）で検証しており、そのまま接続すると
//!   現在成功している 8_193〜65_536 次元のクエリを誤って拒否する。
//!
//! `batch_fallback.rs::FallbackBatchEngine::batch_search` への配線も試みたが、
//! こちらも安全に接続できないことを確認した（詳細は `batch_fallback.rs` モジュール
//! ドキュメントの「実配線調査で判明した阻害要因」参照）。
//!
//! いずれも SIMD 幅／GPU provider の実装・キュー層の追加・ISA 実行時検出
//! （TASK-156・CORE-14）を要する後続タスクの管轄とする。
//!
//! `batch_search.rs::should_aggregate_into_batch`（動的窓集約の判定）は本モジュールが
//! 呼び出す既存の純関数であり、二重に判定ロジックを持たない（同モジュールの
//! ドキュメンテーションコメントに明記の契約）。`batch_fallback.rs` が実装する
//! GPU 失敗時の実行時縮退（primary 失敗→CPU、CORE-8）はこの決定表の対象外である。
//! 本モジュールが担うのは「実行前の経路選択」であり、`batch_fallback` が担うのは
//! 「選択後の実行時 fail-safe」という責務分担を維持する。
//!
//! # CORE-12: 外部入力による経路上書き機構の不存在
//!
//! 本モジュールは経路選択を外部から上書きする引数・設定構造体・feature flag・
//! 環境変数読み取りを一切設けない。これは実装漏れの防止策ではなく、そもそも
//! そのような入力経路をコード上に作らないという設計方針そのものである
//! （未検証の ISA・バックエンド指定で `unsafe` カーネルを強制起動させる攻撃面を、
//! 機構の不存在によって構造的に排除する）。デバッグ用の経路可視化（`EXPLAIN` 等）が
//! 必要になった場合も、本モジュールへ書き込み経路を追加するのではなく、
//! wire/SQL 表層側で [`select_execution_path`] の戻り値を読み取り専用に
//! 表示する形の後続タスクとして検討する。
//!
//! ISA の実行時検出（TASK-156・CORE-14）は本タスクの範囲外であり、[`DetectedIsa`] は
//! 検出トークン導入前のデータ表現として、呼び出し元が指定する値をそのまま受け取る。

use crate::batch_search::{should_aggregate_into_batch, MAX_BATCH_DIM, MAX_BATCH_QUERIES};

/// 実行時に検出された ISA（TASK-156 の検出トークン導入前のデータ表現）。
///
/// `#[non_exhaustive]` にはしない。variant 追加時に [`select_execution_path`] の
/// 網羅 match がコンパイルエラーになり、決定表の更新漏れを構造的に防ぐため
/// （CORE-11: 決定表を 1 箇所に保つという設計意図と表裏一体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedIsa {
    /// SIMD 拡張なし（スカラー演算のみ）。
    Scalar,
    /// Arm Neon（128 bit）。
    Neon,
    /// x86_64 AVX2 + FMA（256 bit）。
    Avx2Fma,
    /// x86_64 AVX-512（512 bit）。
    Avx512,
}

/// クエリベクトルの要素型。現状は `F32` のみを扱う。
///
/// `batch_search.rs::ResidentMatrix` が内部で保持する f16 パック表現（CORE-16
/// ポインタ）はバッチエンジン内部の常駐形式であり、呼び出し元が指定する
/// クエリ入力の型とは独立のため、決定表の入力には含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryDtype {
    F32,
}

/// CPU-SIMD 経路の実行幅。[`DetectedIsa`] からの純写像（[`select_execution_path`] 内で
/// 決まり、これ自体が別の判断を持つことはない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimdWidth {
    Scalar,
    W128,
    W256,
    W512,
}

/// 決定表が確定させる実行経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPath {
    /// CPU-SIMD 経路。`width` は [`DetectedIsa`] に対応する実行幅。
    CpuSimd { width: SimdWidth },
    /// GPU 経路（`batch_search.rs::BatchEngine` の f16 パック常駐行列参照実装、
    /// または `batch_fallback.rs::BatchBackend` を実装する将来の実 GPU バックエンド）。
    Gpu,
}

/// [`select_execution_path`] への入力。すべて値渡しで、参照透過性（同一入力→同一出力）を
/// 保つ（グローバル状態・環境変数・ファイル・時刻を一切参照しない。CORE-12）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchInput {
    /// GPU バックエンドが利用可能かどうか（HW capability）。実 GPU 未接続の現時点では
    /// 呼び出し元がバックエンド構築可否から与える（`batch_fallback.rs::BatchBackend`
    /// の構築結果に対応）。
    pub gpu_available: bool,
    /// 実行時に検出された ISA（TASK-156 の検出トークン導入前は呼び出し元指定）。
    pub isa: DetectedIsa,
    /// クエリベクトルの次元。0、または `batch_search::MAX_BATCH_DIM` 超過は不正入力として
    /// `Err` を返す（fail-closed）。単発クエリ（`batch_size == 1`）でも同じ上限で
    /// 検証する。`core.rs::EngineCore::search` の単発クエリ経路は別途
    /// `storage::MAX_EMBEDDING_DIM`（より大きい値）で次元を検証しており、本モジュールは
    /// まだその経路へ接続していない（後続タスクの管轄）。接続時は「決定表の入力は常に
    /// バッチ側の上限で検証する」という本方針を踏襲するか、単発クエリ用の別上限を
    /// 設けるかを改めて検討する。
    pub dim: usize,
    /// バッチ内のクエリ件数。0、または `batch_search::MAX_BATCH_QUERIES` 超過は不正入力
    /// として `Err` を返す（fail-closed）。単発クエリは 1 を渡す。
    pub batch_size: usize,
    /// クエリベクトルの要素型。
    pub dtype: QueryDtype,
    /// 動的窓判定用の入力。キューから 1 件取り出した直後に後続が存在するかどうか
    /// （`batch_search.rs::should_aggregate_into_batch` へそのまま渡す）。
    pub pending_after_pop: bool,
}

/// [`select_execution_path`] が返すエラー。fail-closed（曖昧な入力は拒否側に倒す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    /// `dim` が 0、または `batch_search::MAX_BATCH_DIM` を超過した。
    InvalidDim { dim: usize, max: usize },
    /// `batch_size` が 0、または `batch_search::MAX_BATCH_QUERIES` を超過した。
    InvalidBatchSize { batch_size: usize, max: usize },
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::InvalidDim { dim, max } => {
                write!(f, "dispatch input dim invalid: dim={dim} max={max}")
            }
            DispatchError::InvalidBatchSize { batch_size, max } => {
                write!(
                    f,
                    "dispatch input batch_size invalid: batch_size={batch_size} max={max}"
                )
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// ISA から CPU-SIMD 実行幅への純写像。[`select_execution_path`] からのみ呼ばれる。
fn simd_width_for(isa: DetectedIsa) -> SimdWidth {
    match isa {
        DetectedIsa::Scalar => SimdWidth::Scalar,
        DetectedIsa::Neon => SimdWidth::W128,
        DetectedIsa::Avx2Fma => SimdWidth::W256,
        DetectedIsa::Avx512 => SimdWidth::W512,
    }
}

/// 実行経路選択の決定表本体（CORE-11）。副作用なしの純関数（同一 `input` に対し
/// 常に同一の `Result` を返す）。
///
/// 決定表の行（既存モジュールの判定を吸収する。実装は変更せず、経路選択だけを
/// ここへ集約する）:
///
/// 1. `dim`・`batch_size` の 0・上限超過 → `Err`（fail-closed。他の行より先に検証する）
/// 2. `batch_size == 1` かつ動的窓判定（[`should_aggregate_into_batch`]）が `false`
///    → 単発クエリとして GPU を使わず CPU-SIMD（CORE-7 の単発クエリ行）
/// 3. `batch_size == 1` だが動的窓判定が `true`（後続クエリが控えている）
///    → バッチ扱いへ昇格する（CORE-7 の動的窓例外行）
/// 4. 上記以外（`batch_size >= 2`、またはバッチ昇格後）で `gpu_available == true`
///    → GPU を優先する（CORE-6 の対応行）
/// 5. `gpu_available == false` → 常に CPU-SIMD（CORE-8 の縮退対応行。
///    `batch_fallback.rs` の実行時縮退とは独立に、事前の経路選択としても
///    GPU 不能なら最初から CPU-SIMD を選ぶ）
///
/// `dtype` は現状 `F32` の 1 variant のみのため経路分岐には寄与しないが、
/// 網羅 match の対象に含め、将来 variant が増えた際に分岐漏れをコンパイルエラーで
/// 検出できるようにする。
pub fn select_execution_path(input: DispatchInput) -> Result<ExecutionPath, DispatchError> {
    if input.dim == 0 || input.dim > MAX_BATCH_DIM {
        return Err(DispatchError::InvalidDim {
            dim: input.dim,
            max: MAX_BATCH_DIM,
        });
    }
    if input.batch_size == 0 || input.batch_size > MAX_BATCH_QUERIES {
        return Err(DispatchError::InvalidBatchSize {
            batch_size: input.batch_size,
            max: MAX_BATCH_QUERIES,
        });
    }

    // dtype は現時点で分岐に寄与しないが、網羅 match で束縛して将来の variant 追加を
    // コンパイルエラーで検出可能にしておく（決定表更新漏れの構造的防止。CORE-11）。
    match input.dtype {
        QueryDtype::F32 => {}
    }

    // バッチ扱いにするかどうか（単発クエリ + 動的窓判定の吸収。CORE-7）。
    let treated_as_batch =
        input.batch_size >= 2 || should_aggregate_into_batch(input.pending_after_pop);

    if treated_as_batch && input.gpu_available {
        return Ok(ExecutionPath::Gpu);
    }

    // GPU 不能（CORE-8 縮退対応行）、または単発クエリで動的窓に入らない場合は
    // CPU-SIMD を選ぶ。ISA からの実行幅写像は分岐を持たない純写像のため、
    // ここでは呼び出すだけで新たな判断は追加しない。
    Ok(ExecutionPath::CpuSimd {
        width: simd_width_for(input.isa),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> DispatchInput {
        DispatchInput {
            gpu_available: false,
            isa: DetectedIsa::Scalar,
            dim: 8,
            batch_size: 1,
            dtype: QueryDtype::F32,
            pending_after_pop: false,
        }
    }

    #[test]
    fn single_query_without_pending_uses_cpu_simd_even_if_gpu_available() {
        let input = DispatchInput {
            gpu_available: true,
            batch_size: 1,
            pending_after_pop: false,
            ..base_input()
        };
        assert_eq!(
            select_execution_path(input).expect("valid input"),
            ExecutionPath::CpuSimd {
                width: SimdWidth::Scalar
            }
        );
    }

    #[test]
    fn single_query_with_pending_promotes_to_batch_and_uses_gpu_when_available() {
        let input = DispatchInput {
            gpu_available: true,
            batch_size: 1,
            pending_after_pop: true,
            ..base_input()
        };
        assert_eq!(
            select_execution_path(input).expect("valid input"),
            ExecutionPath::Gpu
        );
    }

    #[test]
    fn batch_prefers_gpu_when_available() {
        let input = DispatchInput {
            gpu_available: true,
            batch_size: 2,
            ..base_input()
        };
        assert_eq!(
            select_execution_path(input).expect("valid input"),
            ExecutionPath::Gpu
        );
    }

    #[test]
    fn gpu_unavailable_always_falls_back_to_cpu_simd() {
        for batch_size in [1usize, 2, MAX_BATCH_QUERIES] {
            for pending in [false, true] {
                let input = DispatchInput {
                    gpu_available: false,
                    isa: DetectedIsa::Avx2Fma,
                    batch_size,
                    pending_after_pop: pending,
                    ..base_input()
                };
                assert_eq!(
                    select_execution_path(input).expect("valid input"),
                    ExecutionPath::CpuSimd {
                        width: SimdWidth::W256
                    },
                    "batch_size={batch_size} pending={pending}"
                );
            }
        }
    }

    #[test]
    fn isa_maps_to_expected_simd_width() {
        let cases = [
            (DetectedIsa::Scalar, SimdWidth::Scalar),
            (DetectedIsa::Neon, SimdWidth::W128),
            (DetectedIsa::Avx2Fma, SimdWidth::W256),
            (DetectedIsa::Avx512, SimdWidth::W512),
        ];
        for (isa, expected_width) in cases {
            let input = DispatchInput {
                isa,
                ..base_input()
            };
            assert_eq!(
                select_execution_path(input).expect("valid input"),
                ExecutionPath::CpuSimd {
                    width: expected_width
                },
                "isa={isa:?}"
            );
        }
    }

    #[test]
    fn zero_dim_is_rejected() {
        let input = DispatchInput {
            dim: 0,
            ..base_input()
        };
        assert_eq!(
            select_execution_path(input).unwrap_err(),
            DispatchError::InvalidDim {
                dim: 0,
                max: MAX_BATCH_DIM
            }
        );
    }

    #[test]
    fn dim_over_limit_is_rejected() {
        let input = DispatchInput {
            dim: MAX_BATCH_DIM + 1,
            ..base_input()
        };
        assert_eq!(
            select_execution_path(input).unwrap_err(),
            DispatchError::InvalidDim {
                dim: MAX_BATCH_DIM + 1,
                max: MAX_BATCH_DIM
            }
        );
    }

    #[test]
    fn zero_batch_size_is_rejected() {
        let input = DispatchInput {
            batch_size: 0,
            ..base_input()
        };
        assert_eq!(
            select_execution_path(input).unwrap_err(),
            DispatchError::InvalidBatchSize {
                batch_size: 0,
                max: MAX_BATCH_QUERIES
            }
        );
    }

    #[test]
    fn batch_size_over_limit_is_rejected() {
        let input = DispatchInput {
            batch_size: MAX_BATCH_QUERIES + 1,
            ..base_input()
        };
        assert_eq!(
            select_execution_path(input).unwrap_err(),
            DispatchError::InvalidBatchSize {
                batch_size: MAX_BATCH_QUERIES + 1,
                max: MAX_BATCH_QUERIES
            }
        );
    }

    #[test]
    fn determinism_same_input_yields_same_output_across_repeated_calls() {
        let cases = [
            base_input(),
            DispatchInput {
                gpu_available: true,
                batch_size: 4,
                isa: DetectedIsa::Avx512,
                ..base_input()
            },
            DispatchInput {
                gpu_available: true,
                batch_size: 1,
                pending_after_pop: true,
                ..base_input()
            },
        ];
        for input in cases {
            let first = select_execution_path(input);
            let second = select_execution_path(input);
            assert_eq!(first, second, "input={input:?}");
        }
    }

    // 動的窓行が batch_search::should_aggregate_into_batch と一致すること
    // （二重管理を避けるための契約の回帰確認）。
    #[test]
    fn dynamic_window_row_matches_should_aggregate_into_batch() {
        for pending in [false, true] {
            let input = DispatchInput {
                gpu_available: true,
                batch_size: 1,
                pending_after_pop: pending,
                ..base_input()
            };
            let expected = if should_aggregate_into_batch(pending) {
                ExecutionPath::Gpu
            } else {
                ExecutionPath::CpuSimd {
                    width: SimdWidth::Scalar,
                }
            };
            assert_eq!(select_execution_path(input).expect("valid input"), expected);
        }
    }
}
