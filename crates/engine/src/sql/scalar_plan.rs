//! `sql::exec` の SCALAR 事前フィルタ述語が `sql::scalar_index::ScalarIndex`
//! （Issue #473）を消費できる形状かどうかの**静的判定**（Issue #474。対応 ADR:
//! `docs/design/scalar-secondary-index.md`「採用案（候補 B）の確定仕様」節）。
//!
//! `sql::hnsw_cache::classify_ann_plan`（Issue #411）と同型の純粋関数
//! （[`classify_scalar_plan`]）として設計する: `sql::exec`（実行時の索引消費・
//! 縮退判定）と `sql::explain`（`scalar_plan:` 行の静的表示）が同じ入力形状
//! （[`ScalarShapeInput`]）から同じ分類を導出することで、両者の判定基準が
//! 実装として発散しない単一情報源にする。
//!
//! **索引対応述語の狭い定義**（ADR「索引対応述語の狭い定義」節）: `sql::exec`
//! の SCALAR 段は `bound.metadata_filters`（`TEXT` 列の等価・前方一致。
//! `declarative_filter::bind_all` が構築）と `bound.expr_filters`（`WHERE` の
//! 式述語。`sql::udf_call::BoundExpr`）を宣言順に短絡評価する。索引が扱えるのは
//! 前者すべてと、後者のうち **`id <op> <数値リテラル>`**（`op` は
//! `>`/`<`/`>=`/`<=`/`=`、左右いずれの位置でも可）という狭い形だけである。
//! `expr_filters` に 1 つでもこの形に一致しない要素（`Builtin`・`WasmCall`・
//! `VectorRef` を含む式、ネストした演算等）があれば、それは評価時にエラーに
//! なりうる残余述語であり、宣言順の短絡評価という既存の fail-closed 契約
//! （`sql::exec::execute_statement_with_cache` の `on_visible_row` 参照）を
//! 崩さないよう索引経路を一切使わない（[`ScalarPlan::PlainScan`] へ縮退）。
//! `WHERE visible()`（[`crate::sql::parser::WherePredicate::PredicateCall`]）は
//! `rls_predicate_present` フラグのみを立て `metadata_filters`/`expr_filters`
//! を増やさないため、この判定には現れない（残余述語にはならない）。

use crate::declarative_filter::MetadataFilter;
use crate::sql::udf_call::{BinOp, BoundExpr};

/// [`classify_scalar_plan`] の分類結果。閉じた語彙（`sql::explain` の
/// `scalar_plan:` 行の値と 1 対 1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarPlan {
    /// 索引を使わず全可視行を走査する（既定）。
    PlainScan,
    /// 索引対応述語がちょうど 1 件で、`TEXT` 列の等価条件。
    IndexEquality,
    /// 索引対応述語がちょうど 1 件で、`TEXT` 列の前方一致条件。
    IndexPrefix,
    /// 索引対応述語がちょうど 1 件で、`id` に対する単純比較。
    IndexIdRange,
    /// 索引対応述語が 2 件以上（交差が必要）。
    IndexConjunction,
}

/// [`classify_scalar_plan`] の入力。`sql::exec`・`sql::using_plan::
/// pre_check_bindable` の双方が同じ形へ組み立てる。
pub(crate) struct ScalarShapeInput<'a> {
    /// `ExecutionPlan::from_evaluation_order(..).scalar_prefilter`
    /// （SCALAR 段が DISTANCE 段より先に評価されるか）。`false`（`HINT ORDER`
    /// による DISTANCE 先行・SCALAR 事後フィルタ）では索引を使わない
    /// （`sql::exec` の事後フィルタ経路は索引化の対象外。モジュール
    /// ドキュメント参照）。
    pub(crate) scalar_prefilter: bool,
    pub(crate) metadata_filters: &'a [MetadataFilter],
    pub(crate) expr_filters: &'a [BoundExpr],
}

/// `id <op> <数値リテラル>` へ正規化した式述語（左右いずれの位置で束縛されて
/// いても `id` が左辺に来る形へ揃える。左右入替時は `op` を反転する: 例えば
/// `5 > id` は `id < 5` と同値であるため `Lt` へ変換する）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IdPredicate {
    pub(crate) op: BinOp,
    pub(crate) literal: f64,
}

/// `op` を左右入替時の同値な演算子へ反転する（`L op R` を `R op' L` に書き換える
/// ときの `op'`）。`Eq` は対称なため不変。
fn flip_comparison(op: BinOp) -> BinOp {
    match op {
        BinOp::Gt => BinOp::Lt,
        BinOp::Lt => BinOp::Gt,
        BinOp::Ge => BinOp::Le,
        BinOp::Le => BinOp::Ge,
        BinOp::Eq => BinOp::Eq,
        // 算術演算子はこの関数の呼び出し元（`id_predicate_from_expr`）が比較
        // 演算子のみに絞り込んだ後にしか渡さない。
        other => other,
    }
}

/// `expr` が「`id` と数値リテラルの単純比較」（狭義の索引対応述語）である場合に
/// 限り [`IdPredicate`] を返す。それ以外（算術式・`Builtin`・`WasmCall`・
/// `VectorRef` を含む式・ネストした比較等）はすべて `None`（索引非対応。呼び出し元
/// はこれを「残余述語あり」＝[`ScalarPlan::PlainScan`] の根拠として扱う）。
pub(crate) fn id_predicate_from_expr(expr: &BoundExpr) -> Option<IdPredicate> {
    let BoundExpr::Binary { op, lhs, rhs } = expr else {
        return None;
    };
    if !matches!(
        op,
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq
    ) {
        return None;
    }
    match (lhs.as_ref(), rhs.as_ref()) {
        (BoundExpr::IdRef, BoundExpr::Number(literal)) => Some(IdPredicate {
            op: *op,
            literal: *literal,
        }),
        (BoundExpr::Number(literal), BoundExpr::IdRef) => Some(IdPredicate {
            op: flip_comparison(*op),
            literal: *literal,
        }),
        _ => None,
    }
}

/// [`ScalarShapeInput`] から [`ScalarPlan`] を決定する（純粋関数・副作用なし）。
/// `sql::exec`（索引消費の可否判定）と `sql::explain`（`scalar_plan:` 行）が
/// この関数だけを単一情報源として使う（モジュールドキュメント参照）。
pub(crate) fn classify_scalar_plan(input: &ScalarShapeInput<'_>) -> ScalarPlan {
    if !input.scalar_prefilter {
        return ScalarPlan::PlainScan;
    }
    if input.metadata_filters.is_empty() && input.expr_filters.is_empty() {
        return ScalarPlan::PlainScan;
    }
    let mut id_predicate_count = 0usize;
    for expr in input.expr_filters {
        if id_predicate_from_expr(expr).is_none() {
            // 索引非対応の残余述語（評価時にエラーになりうる式を含む）が
            // 1 つでもあれば、索引経路を使わず宣言順の逐次評価
            // （既存 fail-closed 契約）へ全面的に委ねる。
            return ScalarPlan::PlainScan;
        }
        id_predicate_count += 1;
    }
    let total = input.metadata_filters.len() + id_predicate_count;
    if total >= 2 {
        return ScalarPlan::IndexConjunction;
    }
    // ここに到達する時点で `total == 1`（`total == 0` は上の空判定で除外済み）。
    if id_predicate_count == 1 {
        ScalarPlan::IndexIdRange
    } else {
        match input.metadata_filters[0].op() {
            crate::declarative_filter::FilterOp::Equals(_) => ScalarPlan::IndexEquality,
            crate::declarative_filter::FilterOp::StartsWith(_) => ScalarPlan::IndexPrefix,
        }
    }
}

/// `id_as_finite_scalar`（`sql::udf_call`）が許容する `id` の上限と同じ境界
/// （`2^53`。`f64` の仮数部が整数値を正確に表現できる上限）。`id_index` は
/// 全行がこの上限を満たす場合に限り `Some` になるため（`scalar_index.rs`
/// モジュールドキュメント参照）、リテラル側の判定にも同じ境界を使う。
const MAX_EXACT_ID: u64 = 1u64 << 53;

/// `id` 比較述語 [`IdPredicate`] を `ScalarIndex::candidates_id_range` が受け取る
/// `(Bound<u64>, Bound<u64>)` へ変換する。`sql::udf_call::eval_binary` の
/// `(id as f64) op literal` 評価とビット同値になるよう導出する
/// （本モジュールの [`tests::id_bounds_matches_eval_binary_property`] で固定）。
///
/// `None` は「判定不能（呼び出し元は索引を使わず全走査へ縮退する）」であり
/// 「一致 0 件」ではない。文法上 `literal` は非負かつ有限のみ生成されうる
/// （単項マイナスを持たない構文・`sql::udf_call::parse_number_literal` の
/// `is_finite` 検査。`sql::lexer::lex_number` 参照）が、将来の構文拡張に備え
/// 非有限値は防御的に `None` へ倒す。負のリテラルも構文上は到達しないが、
/// 同じ防御のため正しい境界（`Gt`/`Ge` は全件、`Lt`/`Le`/`Eq` は空集合）を返す。
pub(crate) fn id_bounds(
    pred: &IdPredicate,
) -> Option<(std::ops::Bound<u64>, std::ops::Bound<u64>)> {
    use std::ops::Bound;

    if !pred.literal.is_finite() {
        return None;
    }
    let l = pred.literal;
    let max = MAX_EXACT_ID as f64;
    // `lower > upper` になるよう構成した正準の「一致 0 件」（`candidates_id_range`
    // の `in_lower && in_upper` 判定が常に偽になる）。
    const EMPTY: (Bound<u64>, Bound<u64>) = (Bound::Included(1), Bound::Included(0));

    Some(match pred.op {
        BinOp::Gt => {
            if l < 0.0 {
                (Bound::Unbounded, Bound::Unbounded)
            } else if l >= max {
                EMPTY
            } else {
                let lower = (l.floor() as u64).saturating_add(1);
                (Bound::Included(lower), Bound::Unbounded)
            }
        }
        BinOp::Ge => {
            if l < 0.0 {
                (Bound::Unbounded, Bound::Unbounded)
            } else if l > max {
                EMPTY
            } else {
                (Bound::Included(l.ceil() as u64), Bound::Unbounded)
            }
        }
        BinOp::Lt => {
            if l <= 0.0 {
                EMPTY
            } else if l > max {
                (Bound::Unbounded, Bound::Unbounded)
            } else {
                let upper = if l.fract() == 0.0 {
                    (l as u64).saturating_sub(1)
                } else {
                    l.floor() as u64
                };
                (Bound::Unbounded, Bound::Included(upper))
            }
        }
        BinOp::Le => {
            if l < 0.0 {
                EMPTY
            } else if l >= max {
                (Bound::Unbounded, Bound::Unbounded)
            } else {
                (Bound::Unbounded, Bound::Included(l.floor() as u64))
            }
        }
        BinOp::Eq => {
            if !(0.0..=max).contains(&l) || l.fract() != 0.0 {
                EMPTY
            } else {
                let v = l as u64;
                (Bound::Included(v), Bound::Included(v))
            }
        }
        // `id_predicate_from_expr` が比較演算子のみへ絞り込み済みのため到達
        // しない。
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declarative_filter::DeclarativeFilter;

    fn id_gt(literal: f64) -> BoundExpr {
        BoundExpr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(BoundExpr::IdRef),
            rhs: Box::new(BoundExpr::Number(literal)),
        }
    }

    fn test_schema() -> crate::catalog::TableSchema {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("c0", ColumnType::Text, true),
                ColumnDef::new("c1", ColumnType::Text, true),
            ],
        )
    }

    fn eq_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::equals(
                schema.columns[column_index].name.clone(),
                "v".to_string(),
            )],
            &schema,
        )
        .expect("bind equals filter");
        bound.into_iter().next().expect("one filter")
    }

    #[test]
    fn plain_scan_when_scalar_postfilter() {
        let filters = vec![eq_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: false,
            metadata_filters: &filters,
            expr_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_when_no_predicates() {
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &[],
            expr_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn index_equality_single_metadata_filter() {
        let filters = vec![eq_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexEquality);
    }

    #[test]
    fn index_id_range_single_expr_filter() {
        let exprs = vec![id_gt(5.0)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &[],
            expr_filters: &exprs,
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexIdRange);
    }

    #[test]
    fn index_conjunction_two_predicates() {
        let filters = vec![eq_filter(1)];
        let exprs = vec![id_gt(5.0)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &exprs,
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexConjunction);
    }

    #[test]
    fn plain_scan_on_residual_vector_ref_expr() {
        // `VectorRef` を参照する残余述語（`embedding` を直接比較する形。
        // 索引が扱えるのは `id` の単純比較のみであることを固定する）。
        let exprs = vec![BoundExpr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(BoundExpr::VectorRef),
            rhs: Box::new(BoundExpr::Number(1.0)),
        }];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &[],
            expr_filters: &exprs,
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn left_right_swap_flips_operator() {
        // `5 > id` は `id < 5` と同値。
        let expr = BoundExpr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(BoundExpr::Number(5.0)),
            rhs: Box::new(BoundExpr::IdRef),
        };
        let pred = id_predicate_from_expr(&expr).expect("id predicate");
        assert_eq!(pred.op, BinOp::Lt);
        assert_eq!(pred.literal, 5.0);
    }

    #[test]
    fn id_bounds_matches_eval_binary_property() {
        use crate::sql::udf_call::{eval_binary, ExprValue};
        let ids: Vec<u64> = (0..=20).collect();
        let literals = [0.0, 0.5, 1.0, 5.0, 5.5, 10.0, 19.5, 20.0, 20.5];
        for &literal in &literals {
            for op in [BinOp::Gt, BinOp::Lt, BinOp::Ge, BinOp::Le, BinOp::Eq] {
                let pred = IdPredicate { op, literal };
                let bounds = id_bounds(&pred).expect("finite non-negative literal");
                for &id in &ids {
                    let expected = matches!(
                        eval_binary(op, ExprValue::Scalar(id as f64), ExprValue::Scalar(literal),)
                            .expect("comparison never errors for finite operands"),
                        ExprValue::Bool(true)
                    );
                    let in_range = bound_contains(bounds, id);
                    assert_eq!(in_range, expected, "op={op:?} literal={literal} id={id}");
                }
            }
        }
    }

    /// [`tests::id_bounds_matches_eval_binary_property`] は `id ∈ 0..=20` ・
    /// 小さな literal しか網羅しないため、`id_index` が扱う実際の上限
    /// （[`MAX_EXACT_ID`]＝`2^53`）付近の境界を別途固定する。`2^53 + 1.0` は
    /// f64 の刻み幅が 1 を超えるため `2^53` へ丸められてしまう（`f64::from_str`
    /// の丸めハザード）ので、あえて `+2.0` を使い実際に異なる値になる literal を
    /// 網羅する。`id` 側は `MAX_EXACT_ID` を上限とする（`id > 2^53` の行が
    /// 1 件でもあれば `id_index` 自体が `None` になり `id_bounds` は呼ばれない
    /// 契約——`sql::udf_call::id_as_finite_scalar` 参照——ため、その領域は
    /// `eval_binary` 側の `(id as f64)` 丸めで exact 比較契約が崩れても
    /// 本関数の正しさに影響しない）。
    #[test]
    fn id_bounds_matches_eval_binary_property_near_2_53_boundary() {
        use crate::sql::udf_call::{eval_binary, ExprValue};
        let ids: Vec<u64> = vec![MAX_EXACT_ID - 2, MAX_EXACT_ID - 1, MAX_EXACT_ID];
        let literals = [
            (MAX_EXACT_ID - 1) as f64,
            MAX_EXACT_ID as f64,
            (MAX_EXACT_ID as f64) + 2.0,
        ];
        for &literal in &literals {
            for op in [BinOp::Gt, BinOp::Lt, BinOp::Ge, BinOp::Le, BinOp::Eq] {
                let pred = IdPredicate { op, literal };
                let bounds = id_bounds(&pred).expect("finite non-negative literal");
                for &id in &ids {
                    let expected = matches!(
                        eval_binary(op, ExprValue::Scalar(id as f64), ExprValue::Scalar(literal),)
                            .expect("comparison never errors for finite operands"),
                        ExprValue::Bool(true)
                    );
                    let in_range = bound_contains(bounds, id);
                    assert_eq!(in_range, expected, "op={op:?} literal={literal} id={id}");
                }
            }
        }
    }

    fn bound_contains(bounds: (std::ops::Bound<u64>, std::ops::Bound<u64>), id: u64) -> bool {
        use std::ops::Bound;
        let lower_ok = match bounds.0 {
            Bound::Included(l) => id >= l,
            Bound::Excluded(l) => id > l,
            Bound::Unbounded => true,
        };
        let upper_ok = match bounds.1 {
            Bound::Included(u) => id <= u,
            Bound::Excluded(u) => id < u,
            Bound::Unbounded => true,
        };
        lower_ok && upper_ok
    }
}
