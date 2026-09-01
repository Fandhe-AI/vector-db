//! 束縛済み式（[`crate::sql::udf_call::BoundExpr`]）の行ループ実行を、毎行の
//! 再帰ツリーウォークから「束縛時に一度だけ平坦化したステップ列を、行ループでは
//! 明示スタックで線形実行する」形へ変える（Issue #353・式評価のステップ列
//! コンパイル化。WHERE 事前/事後フィルタ・投影の `Computed` 列・集計の
//! `ScalarExpr` が共通で使う）。
//!
//! 責務境界: 本モジュールは `BoundExpr` 木から [`ExprProgram`] を導出する
//! [`ExprProgram::compile`] と、行コンテキスト（`id`・`embedding`）に対して
//! それを実行する [`ExprProgram::eval`] のみを提供する。値レベルの評価規則
//! （0 除算・非有限値・`f32` キャスト後 Infinity の fail-closed 判定、
//! `try_reserve_exact` による確保失敗時の `54000` 写像）はすべて
//! `sql::udf_call`（[`crate::sql::udf_call::eval_binary`]・
//! [`crate::sql::udf_call::apply_builtin`]）を共有し、本モジュールでは複製しない。
//! `sql::udf_call::eval`（再帰版）はセマンティクスの参照実装として残し、
//! 本モジュールの [`ExprProgram::compile`]／[`ExprProgram::eval`] が同じ結果を
//! 返すことを差分テストで検証する。
//!
//! # 評価順序の保存
//!
//! `BoundExpr::Binary` は lhs→rhs、`Builtin`/`WasmCall` の引数は左から右へ、
//! いずれも無条件（短絡評価なし）に評価する（`sql::udf_call::eval` 参照。
//! `BoundExpr` に `And`/`Or` 相当の分岐評価ノードは存在しない——複合 `WHERE` は
//! 呼び出し元ループが `Vec<BoundExpr>` を順に AND 適用する形で短絡する）。
//! そのため [`compile_node`] の後行順（postorder）平坦化は、再帰 `eval` と
//! 同一の評価順・エラー発生順を厳密に保つ。
//!
//! # 定数畳み込み（defer-on-error）
//!
//! 行に依存しない部分式（数値リテラルのみからなるスカラー算術・比較）は
//! [`try_fold_scalar`] で束縛時に 1 回だけ評価し、結果を `ConstScalar`/
//! `ConstBool` ステップへ置換する。畳み込み中にエラーになる部分式（例:
//! 定数 0 除算）は畳み込まず、平坦化のみ行って実行時評価に委ねる
//! （defer-on-error）。可視行が 1 行も評価されなければエラーが発生しないという
//! 既存契約（`sql::exec` の SCALAR 段は可視行にのみ到達する）を、畳み込みの
//! 導入で変えないための判断。`WasmCall` はバックエンドが非決定的でありうる
//! ことと EXT-6 の行単位 deadline/中断契約を維持するため、畳み込み対象に含めない
//! （`try_fold_scalar` は `WasmCall`/`IdRef`/`VectorRef`/`Builtin` を素通りし
//! `None` を返す）。

use std::borrow::Cow;
use std::sync::Arc;

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::udf_call::{
    self, apply_builtin, finite_scalar, id_as_finite_scalar, BinOp, BoundExpr, BuiltinFn, ExprValue,
};
use crate::wasm_udf::WasmUdfBackend;

/// ステップ列 1 個分の命令（密な enum。`ExprProgram::eval` の行ループでは
/// `match` による線形ディスパッチのみを行い、再帰しない）。
#[derive(Debug, Clone)]
pub(crate) enum ExprStep {
    /// 定数畳み込み済みのスカラー値（数値リテラル、またはリテラルのみから
    /// なる算術部分式の畳み込み結果）。
    ConstScalar(f64),
    /// 定数畳み込み済みの真偽値（リテラルのみからなる比較部分式の畳み込み結果）。
    ConstBool(bool),
    /// 行 `id` を [`id_as_finite_scalar`] 経由でスカラー値として push する。
    PushId,
    /// テーブルの `VECTOR` 列（行の `embedding`）をそのまま借用して push する
    /// （`Cow::Borrowed`。`sql::udf_call::eval` の `BoundExpr::VectorRef` 分岐
    /// （Issue #352）と同じ契約——`vec_norm(embedding)` 等の読み取り専用式では
    /// 確保・複製が一切発生しない。PR #373 codex-review 指摘対応: 当初実装は
    /// 毎行 `Vec<f32>` を確保していたが、`ExprProgram::eval` の `embedding`
    /// 引数と行ループのスクラッチスタックへ同一ライフタイム `'a` を通すことで
    /// 借用へ戻した）。
    PushVector,
    /// 組み込み関数呼び出し。arity 分（[`udf_call::builtin_signature`]）を
    /// スタックから pop し、[`apply_builtin`] へ渡す。
    Builtin(BuiltinFn),
    /// 2 項演算。rhs・lhs の順にスタックから pop し
    /// [`crate::sql::udf_call::eval_binary`] へ渡す。
    Binary(BinOp),
    /// WASM UDF 呼び出し（ABI 固定: `(Vector, Scalar) -> Scalar`。TASK-149・
    /// EXT-5）。scalar 引数（後に push された方）から先に pop する。
    WasmCall { backend: Arc<dyn WasmUdfBackend> },
}

impl PartialEq for ExprStep {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ExprStep::ConstScalar(a), ExprStep::ConstScalar(b)) => a == b,
            (ExprStep::ConstBool(a), ExprStep::ConstBool(b)) => a == b,
            (ExprStep::PushId, ExprStep::PushId) => true,
            (ExprStep::PushVector, ExprStep::PushVector) => true,
            (ExprStep::Builtin(a), ExprStep::Builtin(b)) => a == b,
            (ExprStep::Binary(a), ExprStep::Binary(b)) => a == b,
            // `Arc<dyn WasmUdfBackend>` は `dyn` 型のため構造的な `PartialEq` を
            // 導出できない。同一性判定は `Arc::ptr_eq`（`BoundExpr::WasmCall` の
            // 既存方針〔`udf_call.rs`〕を踏襲）。
            (ExprStep::WasmCall { backend: a }, ExprStep::WasmCall { backend: b }) => {
                Arc::ptr_eq(a, b)
            }
            _ => false,
        }
    }
}

/// 束縛済み式 1 本をコンパイルした平坦なステップ列。束縛時（`sql::parser::bind_*`
/// 経由）に 1 回だけ構築し、行ループでは [`ExprProgram::eval`] を呼ぶだけにする
/// ことで、行ごとの再帰ツリーウォーク・enum マッチの分岐分散をなくす。
#[derive(Debug, Clone)]
pub(crate) struct ExprProgram {
    steps: Vec<ExprStep>,
    /// 実行時に値スタックが到達しうる最大深さ（コンパイル時に確定）。
    /// `ExprProgram::eval` は行ループの外で確保したスクラッチバッファを
    /// 使い回すため直接は使わないが、`steps.len()` と共に構造的な健全性の
    /// 検証（テスト）に用いる。
    pub(crate) max_stack: usize,
}

impl PartialEq for ExprProgram {
    fn eq(&self, other: &Self) -> bool {
        self.steps == other.steps && self.max_stack == other.max_stack
    }
}

/// 定数畳み込みの結果値。`ExprValue` は `Vector` variant を持つため、畳み込み
/// 対象をスカラー算術・比較のみへ構造的に限定するために専用の小さな型を使う
/// （`Vector` が生じうるかどうかを呼び出し側で判定する必要をなくす。P0
/// 「ライブラリコードで panic させない」対応・`.claude/rules/coding-rust.md`）。
enum FoldedConst {
    Scalar(f64),
    Bool(bool),
}

/// 定数畳み込みの対象を、行に依存しないスカラー算術・比較のみに限定した
/// 純粋関数（副作用なし。`ExprProgram::compile` から呼ばれる）。`IdRef`・
/// `VectorRef`・`Builtin`・`WasmCall` は行依存または非決定的なため常に
/// `None`（畳み込まない）を返す。畳み込み中の評価が `Err`（0 除算・非有限値）
/// になる場合も `None` を返し、defer-on-error を保つ（§モジュールドキュメント
/// 参照）。
fn try_fold_scalar(expr: &BoundExpr) -> Option<FoldedConst> {
    match expr {
        BoundExpr::Number(v) => Some(FoldedConst::Scalar(*v)),
        BoundExpr::Binary { op, lhs, rhs } => {
            let l = match try_fold_scalar(lhs)? {
                FoldedConst::Scalar(v) => ExprValue::Scalar(v),
                FoldedConst::Bool(b) => ExprValue::Bool(b),
            };
            let r = match try_fold_scalar(rhs)? {
                FoldedConst::Scalar(v) => ExprValue::Scalar(v),
                FoldedConst::Bool(b) => ExprValue::Bool(b),
            };
            match udf_call::eval_binary(*op, l, r).ok()? {
                ExprValue::Scalar(v) => Some(FoldedConst::Scalar(v)),
                ExprValue::Bool(b) => Some(FoldedConst::Bool(b)),
                // `l`/`r` は `try_fold_scalar` の再帰でスカラー・真偽値に限定
                // 済みのため、四則演算・比較の結果は理論上 `Vector` になり
                // 得ない。`unreachable!` ではなく畳み込み対象外（`None`）として
                // fail-safe に扱う（`compile_node` は通常のステップ平坦化へ
                // フォールバックする）。
                ExprValue::Vector(_) => None,
            }
        }
        BoundExpr::IdRef
        | BoundExpr::VectorRef
        | BoundExpr::Builtin { .. }
        | BoundExpr::WasmCall { .. } => None,
    }
}

/// `BoundExpr` を後行順（postorder）で `steps` へ平坦化する（[`ExprProgram::compile`]
/// の内部再帰）。`current_depth`／`max_stack` は実行時の値スタックの深さを
/// コンパイル時にシミュレートし、`ExprProgram::max_stack` を確定させる。
fn compile_node(
    expr: &BoundExpr,
    steps: &mut Vec<ExprStep>,
    current_depth: &mut usize,
    max_stack: &mut usize,
) {
    if let Some(folded) = try_fold_scalar(expr) {
        match folded {
            FoldedConst::Scalar(v) => steps.push(ExprStep::ConstScalar(v)),
            FoldedConst::Bool(b) => steps.push(ExprStep::ConstBool(b)),
        }
        *current_depth += 1;
        *max_stack = (*max_stack).max(*current_depth);
        return;
    }
    match expr {
        // `try_fold_scalar` が `Number` を必ず畳み込むため実行時には到達しない
        // 分岐だが、`BoundExpr` の全 variant を網羅する `match` として残す。
        BoundExpr::Number(v) => {
            steps.push(ExprStep::ConstScalar(*v));
            *current_depth += 1;
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::IdRef => {
            steps.push(ExprStep::PushId);
            *current_depth += 1;
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::VectorRef => {
            steps.push(ExprStep::PushVector);
            *current_depth += 1;
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::Builtin { f, args } => {
            for a in args {
                compile_node(a, steps, current_depth, max_stack);
            }
            steps.push(ExprStep::Builtin(*f));
            *current_depth = current_depth.saturating_sub(args.len()).saturating_add(1);
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::Binary { op, lhs, rhs } => {
            compile_node(lhs, steps, current_depth, max_stack);
            compile_node(rhs, steps, current_depth, max_stack);
            steps.push(ExprStep::Binary(*op));
            *current_depth = current_depth.saturating_sub(2).saturating_add(1);
            *max_stack = (*max_stack).max(*current_depth);
        }
        BoundExpr::WasmCall { backend, args, .. } => {
            // ABI 固定（`bind_call` が保証）: args[0] = Vector, args[1] = Scalar。
            // 再帰 `eval` は `eval_vector_arg(args, 0, ...)` の後に
            // `eval_scalar_arg(args, 1, ...)` を呼ぶ（左→右）ため、同じ順で
            // push する。
            if let Some(vector_arg) = args.first() {
                compile_node(vector_arg, steps, current_depth, max_stack);
            }
            if let Some(scalar_arg) = args.get(1) {
                compile_node(scalar_arg, steps, current_depth, max_stack);
            }
            steps.push(ExprStep::WasmCall {
                backend: Arc::clone(backend),
            });
            // 束縛段（`bind_call`）が args.len() == 2 を保証する。ここで args が
            // 2 要素に満たない場合でも push 済みステップ数分だけ減算する
            // （負方向へ折り込まず `saturating_sub` で 0 未満にしない）。
            *current_depth = current_depth
                .saturating_sub(args.len().min(2))
                .saturating_add(1);
            *max_stack = (*max_stack).max(*current_depth);
        }
    }
}

impl ExprProgram {
    /// `BoundExpr` 木を平坦なステップ列へコンパイルする（束縛時に 1 回だけ
    /// 呼ぶ想定。`sql::parser::bind_where_predicates` 等から呼ばれる）。
    pub(crate) fn compile(expr: &BoundExpr) -> ExprProgram {
        let mut steps = Vec::new();
        let mut current_depth = 0usize;
        let mut max_stack = 0usize;
        compile_node(expr, &mut steps, &mut current_depth, &mut max_stack);
        ExprProgram { steps, max_stack }
    }

    /// ステップ列を明示スタック（`scratch`）で線形実行する（再帰しない）。
    /// `scratch` は呼び出し元の行ループの外で確保し、行ごとに使い回す想定
    /// （呼び出し前に空である必要はない。本関数の先頭で `clear` する）。
    ///
    /// エラー契約は再帰 `eval`（[`crate::sql::udf_call::eval`]）と同一:
    /// スタック underflow・型不一致は `SqlSurfaceError::Internal`（固定文言。
    /// 束縛段の不変条件が崩れた場合の保険であり、束縛済み式に対しては
    /// 通常発生しない）、0 除算・非有限値・確保失敗はそれぞれ既存の
    /// `22000`／`54000` 写像を共有する（[`apply_builtin`]・
    /// [`crate::sql::udf_call::eval_binary`] 経由）。
    pub(crate) fn eval<'a>(
        &self,
        id: u64,
        embedding: &'a [f32],
        scratch: &mut Vec<ExprValue<'a>>,
    ) -> Result<ExprValue<'a>, SqlSurfaceError> {
        scratch.clear();
        for step in &self.steps {
            match step {
                ExprStep::ConstScalar(v) => scratch.push(ExprValue::Scalar(*v)),
                ExprStep::ConstBool(b) => scratch.push(ExprValue::Bool(*b)),
                ExprStep::PushId => {
                    scratch.push(ExprValue::Scalar(id_as_finite_scalar(id)?));
                }
                ExprStep::PushVector => {
                    // Issue #352 の契約を踏襲: 行の embedding をそのまま借用する
                    // （確保・複製なし）。`eval` のシグネチャで `embedding: &'a
                    // [f32]` と `scratch: &mut Vec<ExprValue<'a>>` を同一
                    // ライフタイムで結び、借用したベクトルをスタック経由で返せる
                    // ようにしている。
                    scratch.push(ExprValue::Vector(Cow::Borrowed(embedding)));
                }
                ExprStep::Builtin(f) => {
                    let arity = udf_call::builtin_signature(*f).0.len();
                    if scratch.len() < arity {
                        return Err(stack_underflow());
                    }
                    let split_at = scratch.len() - arity;
                    let args = scratch.split_off(split_at);
                    let result = apply_builtin(*f, args)?;
                    scratch.push(result);
                }
                ExprStep::Binary(op) => {
                    let r = scratch.pop().ok_or_else(stack_underflow)?;
                    let l = scratch.pop().ok_or_else(stack_underflow)?;
                    let result = udf_call::eval_binary(*op, l, r)?;
                    scratch.push(result);
                }
                ExprStep::WasmCall { backend } => {
                    let scalar_val = scratch.pop().ok_or_else(stack_underflow)?;
                    let vector_val = scratch.pop().ok_or_else(stack_underflow)?;
                    let v = match vector_val {
                        ExprValue::Vector(v) => v,
                        _ => return Err(type_mismatch()),
                    };
                    let s = match scalar_val {
                        ExprValue::Scalar(s) => s,
                        _ => return Err(type_mismatch()),
                    };
                    // バックエンドの失敗（deadline 超過・トラップ・メモリ確保
                    // 失敗・`Mutex` poison 等）は種別を問わずすべて `22000` へ
                    // 写像する（行値・テナント情報を含まない固定文言。
                    // `sql::udf_call::eval` の `WasmCall` 分岐と同じ契約。
                    // EXT-6 の拒否・強制中断はここで行単位のエラーへ収束し、
                    // プロセスは生存する）。
                    let result = backend
                        .call_vector_scalar(&v, s)
                        .map_err(|e| SqlSurfaceError::invalid_input(e.to_string()))?;
                    scratch.push(finite_scalar(result, "wasm udf")?);
                }
            }
        }
        scratch.pop().ok_or_else(stack_underflow)
    }
}

fn stack_underflow() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "expression program stack underflow at evaluation time".to_string(),
    }
}

fn type_mismatch() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "function argument type mismatch at evaluation time".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::udf_call::MAX_EXPR_NODES;

    fn num(v: f64) -> BoundExpr {
        BoundExpr::Number(v)
    }

    fn bin(op: BinOp, lhs: BoundExpr, rhs: BoundExpr) -> BoundExpr {
        BoundExpr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    /// 差分テスト用の共通比較（コンパイル実行結果と再帰 `eval` の結果が
    /// 一致することを検証する）。
    fn assert_matches_recursive_eval(expr: &BoundExpr, id: u64, embedding: &[f32]) {
        let program = ExprProgram::compile(expr);
        let mut scratch = Vec::new();
        let compiled = program.eval(id, embedding, &mut scratch);
        let recursive = udf_call::eval(expr, id, embedding);
        match (compiled, recursive) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "compiled/recursive eval diverged"),
            (Err(_), Err(_)) => {} // 両者ともエラー（詳細メッセージの一致までは要求しない）
            (a, b) => panic!("compiled={a:?} recursive={b:?} diverged in Ok/Err"),
        }
    }

    #[test]
    fn number_literal_folds_to_const_scalar() {
        let expr = num(3.5);
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::ConstScalar(3.5)]);
        assert_eq!(program.max_stack, 1);
    }

    #[test]
    fn constant_arithmetic_folds_to_single_step() {
        let expr = bin(BinOp::Mul, num(2.0), num(3.0));
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::ConstScalar(6.0)]);
        assert_eq!(program.max_stack, 1);
    }

    #[test]
    fn constant_comparison_folds_to_const_bool() {
        let expr = bin(BinOp::Lt, num(1.0), num(2.0));
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::ConstBool(true)]);
    }

    #[test]
    fn constant_division_by_zero_is_not_folded_defer_on_error() {
        // defer-on-error: 定数 0 除算はコンパイル時に畳み込まず、平坦化のみ
        // 行う（§モジュールドキュメント参照）。畳み込まれていれば
        // `ConstScalar`/`ConstBool` 1 ステップになるはずが、ここでは
        // `Binary` ステップが残ることを確認する。
        let expr = bin(BinOp::Div, num(1.0), num(0.0));
        let program = ExprProgram::compile(&expr);
        assert!(matches!(
            program.steps.last(),
            Some(ExprStep::Binary(BinOp::Div))
        ));
        // 実行時（行が評価された時点）でのみ 22000 相当のエラーになる。
        let mut scratch = Vec::new();
        assert!(program.eval(1, &[], &mut scratch).is_err());
    }

    #[test]
    fn id_ref_is_not_folded_and_reflects_row_id() {
        let expr = BoundExpr::IdRef;
        assert_matches_recursive_eval(&expr, 42, &[]);
        let program = ExprProgram::compile(&expr);
        assert_eq!(program.steps, vec![ExprStep::PushId]);
    }

    #[test]
    fn id_beyond_exact_f64_range_is_rejected_fail_closed() {
        let expr = BoundExpr::IdRef;
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert!(program.eval(1u64 << 60, &[], &mut scratch).is_err());
    }

    #[test]
    fn vector_ref_matches_recursive_eval() {
        let expr = BoundExpr::VectorRef;
        assert_matches_recursive_eval(&expr, 1, &[1.0, 2.0, 3.0]);
    }

    /// PR #373 codex-review 指摘対応: `ExprStep::PushVector` が行の embedding を
    /// 確保・複製せず借用のまま返すことを検証する（Issue #352 の
    /// `sql::udf_call::eval` の `Cow::Borrowed` 契約と同じ挙動を、ステップ列
    /// コンパイル経由の評価でも維持することの回帰防止）。ポインタ一致
    /// （`as_ptr`）まで確認し、`Cow::Owned` へコピーされていないことを保証する。
    #[test]
    fn vector_ref_borrows_embedding_without_copy() {
        let expr = BoundExpr::VectorRef;
        let program = ExprProgram::compile(&expr);
        let embedding = [1.0f32, 2.0, 3.0];
        let mut scratch = Vec::new();
        let value = program
            .eval(1, &embedding, &mut scratch)
            .expect("VectorRef eval should succeed");
        match value {
            ExprValue::Vector(Cow::Borrowed(borrowed)) => {
                assert_eq!(borrowed.as_ptr(), embedding.as_ptr());
            }
            other => panic!("expected Cow::Borrowed vector, got {other:?}"),
        }
    }

    #[test]
    fn mixed_row_dependent_and_constant_subexpr_matches_recursive_eval() {
        // `id * (2.0 * 3.0)`: 右部分木のみ畳み込まれ、左部分木（行依存）は
        // 実行時にのみ確定する。
        let expr = bin(
            BinOp::Mul,
            BoundExpr::IdRef,
            bin(BinOp::Mul, num(2.0), num(3.0)),
        );
        assert_matches_recursive_eval(&expr, 4, &[]);
        let program = ExprProgram::compile(&expr);
        assert_eq!(
            program.steps,
            vec![
                ExprStep::PushId,
                ExprStep::ConstScalar(6.0),
                ExprStep::Binary(BinOp::Mul)
            ]
        );
    }

    #[test]
    fn builtin_vec_norm_matches_recursive_eval() {
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::VecNorm,
            args: vec![BoundExpr::VectorRef],
        };
        assert_matches_recursive_eval(&expr, 1, &[3.0, 4.0]);
    }

    #[test]
    fn builtin_vec_div_by_zero_is_fail_closed_and_matches_recursive_eval() {
        let expr = BoundExpr::Builtin {
            f: BuiltinFn::VecDiv,
            args: vec![BoundExpr::VectorRef, num(0.0)],
        };
        assert_matches_recursive_eval(&expr, 1, &[1.0, 2.0]);
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        assert!(program.eval(1, &[1.0, 2.0], &mut scratch).is_err());
    }

    #[test]
    fn comparison_expression_matches_recursive_eval() {
        let expr = bin(BinOp::Gt, BoundExpr::IdRef, num(10.0));
        assert_matches_recursive_eval(&expr, 5, &[]);
        assert_matches_recursive_eval(&expr, 20, &[]);
    }

    /// Issue #353 の受け入れ条件 3（前後比較の記録）用の手動専用ベンチマーク。
    /// 本リポには計画が名指した `feature_bench` 相当の汎用ベンチ例が存在せず
    /// （`crates/engine/examples/` を参照）、既存の SQL 表層ベンチ
    /// （`benches/sql_c1_bench.rs`）は spec 由来の非公開閾値環境変数が前提のため
    /// 本セッションからは実行できない。そのため、行ループの分岐機構そのもの
    /// （再帰ツリーウォーク vs 平坦ステップ列の線形実行）を同一プロセス内で
    /// 直接比較する。両実装は本 PR で並存する（`udf_call::eval` は参照実装として
    /// 残置）ため、この比較は「Issue #353 が変えた部分」の前後差を厳密に表す。
    /// `cargo test -p engine --lib sql::expr_program -- --ignored --nocapture`
    /// で手動実行し、実測値を `docs/design/expr-step-compilation.md` へ転記する
    /// （CI では実行しない。デフォルトでは無視される診断用ベンチのため）。
    #[test]
    #[ignore]
    fn bench_recursive_eval_vs_compiled_program() {
        use std::time::Instant;

        // WHERE 述語で典型的な複合式（`vec_norm(embedding) > 2.0 AND vec_sum(...) ...`
        // 相当の 1 述語分に近い深さ）を模した式木。定数畳み込みが効かない
        // 行依存の組み込み関数呼び出し中心（実クエリの主要コストである行ごとの
        // 評価そのものを測る）。
        let embedding: Vec<f32> = (0..16).map(|i| i as f32 * 0.5 + 1.0).collect();
        let expr = bin(
            BinOp::Gt,
            BoundExpr::Builtin {
                f: BuiltinFn::VecNorm,
                args: vec![BoundExpr::VectorRef],
            },
            num(2.0),
        );
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();

        const ITERS: u64 = 200_000;

        // ウォームアップ（ページフォールト・分岐予測のコールドスタートを両者から除く）。
        for id in 0..1000u64 {
            let _ = udf_call::eval(&expr, id, &embedding);
            let _ = program.eval(id, &embedding, &mut scratch);
        }

        let start = Instant::now();
        for id in 0..ITERS {
            std::hint::black_box(udf_call::eval(&expr, id, &embedding).unwrap());
        }
        let recursive_elapsed = start.elapsed();

        let start = Instant::now();
        for id in 0..ITERS {
            std::hint::black_box(program.eval(id, &embedding, &mut scratch).unwrap());
        }
        let compiled_elapsed = start.elapsed();

        eprintln!(
            "bench_recursive_eval_vs_compiled_program: recursive={recursive_elapsed:?} \
             compiled={compiled_elapsed:?} iters={ITERS}"
        );
    }

    #[test]
    fn step_count_never_exceeds_bound_expr_node_budget() {
        // `steps.len()` はコンパイル時に 1 ノード → 高々 1 ステップの対応で
        // 平坦化するため、束縛段の `MAX_EXPR_NODES`（node_budget）を超えない
        // （境界の検証。DoS 耐性の裏付け）。ここでは深いが定数畳み込みされない
        // （id 参照を交互に挟む）連鎖式で検証する。
        let mut expr = BoundExpr::IdRef;
        let depth = 64; // MAX_EXPR_DEPTH 内に収まる程度の深さ
        for _ in 0..depth {
            expr = bin(BinOp::Add, expr, num(1.0));
        }
        let program = ExprProgram::compile(&expr);
        assert!(program.steps.len() <= MAX_EXPR_NODES);
        assert!(program.max_stack <= program.steps.len());
    }
}
