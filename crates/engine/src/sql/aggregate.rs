//! 集計 SELECT（`GROUP BY` なし・単一行結果、TASK-166・SQL-13）の実行本体。
//!
//! 責務境界: [`crate::sql::parser::bind_aggregate`] が返す
//! [`crate::sql::parser::BoundAggregate`] を受け取り、対象テーブルの行テーブル
//! （`user_rows/{table}`）を**ストリーミングで**（可視行の embedding・metadata を
//! 一度に確保せず、行ごとに判定・集約・破棄する O(1) メモリで）走査して単一行の
//! [`crate::sql::exec::QueryResult`] を組み立てる。`core.rs::EngineCore::execute_sql_in_session`
//! の `Statement::Aggregate` アームから呼ばれる（[`sql`](crate::sql) モジュール
//! ドキュメント参照）。
//!
//! [`crate::arena::VectorArena`]（既存の検索 SELECT 実行経路）は使わない: アリーナは
//! スキーマに `VECTOR` 列が必須（`validated_vector_dim_in_txn`）で、かつ可視行の
//! embedding を全件バッファへ確保するため、`VECTOR` 列を持たないテーブルの集計や
//! 大規模テーブルの `COUNT(*)` には過剰（メモリ）かつ非対応（対象ビヘイビア:
//! SQL-13 は `VECTOR` 列なしテーブルでの集計を要求する）。
//!
//! RLS 適用順序は [`crate::arena`] モジュールの走査ループと同一の規約に揃える
//! （`.claude/rules/security.md`「テナント境界（P0）」）: 行ヘッダから
//! `tenant_id`・`visibility` のみを取り出し（[`crate::storage::decode_row_tenant_and_visibility`]）、
//! [`crate::policy::PolicyContext::is_visible`] が `false` を返す行は embedding・
//! metadata を一切デコードせずスキップする。可視行のみ完全デコード
//! （[`crate::storage::decode_row`]）して SCALAR 段（`WHERE`）・集計へ進む。
//! `COUNT` 等の集計値から他テナント行の存在・件数を推測できないという契約
//! （RLS-7・RLS-8）は、この「不可視行は集約対象に一切現れない」という走査自体の
//! 構造で担保する。

use crate::catalog::{self, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::allowlist::{AggregateFunc, SqlSurfaceError};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::parser::{AggregateInput, BoundAggregate};
use crate::sql::udf_call::{self, ExprValue};
use crate::storage::{self, StorageError};
use redb::ReadableTable;

/// 型不整合・NULL 契約違反等の意味論的問題ではなく、`bind_aggregate` の型検査を
/// 通過したはずの `(AggregateFunc, AggregateInput)` の組み合わせが
/// [`Accumulator::new`] の網羅から漏れていた場合に返す（実装バグの検出用。
/// untrusted 入力起因ではないため `wire_code` は `XX000`）。
fn accumulator_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("aggregate accumulator/input type mismatch: {detail}"),
    }
}

/// 集計項目 1 つの実行時アキュムレータ（TASK-166・SQL-13）。すべて O(1) 状態
/// （`TextMin`/`TextMax` のみ、新しい極値を更新するたびに高々 1 本の `String` を
/// 保持し直す。`.claude/rules/security.md`「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。
enum Accumulator {
    /// `COUNT(*)`・`COUNT(id)`・`COUNT(<VECTOR 列>)`・`COUNT(<Scalar 式>)`。
    Count(u64),
    /// `SUM(id)`。`checked_add` で正確に演算し、超過は [`SqlSurfaceError::numeric_out_of_range`]。
    IdSum(Option<u64>),
    /// `AVG(id)`。合計は `u64` で正確に保持し、`finish` で `f64` 化して除算する。
    IdAvg {
        sum: Option<u64>,
        count: u64,
    },
    IdMin(Option<u64>),
    IdMax(Option<u64>),
    /// `SUM(<Scalar 式>)`。加算のたびに `is_finite()` を検査する。
    FloatSum(Option<f64>),
    FloatAvg {
        sum: Option<f64>,
        count: u64,
    },
    /// `MIN(<Scalar 式>)`。`f64` は全順序を持たないため `total_cmp` で比較する。
    FloatMin(Option<f64>),
    FloatMax(Option<f64>),
    /// `MIN(<TEXT 列>)`。バイト順比較（`str::lt`/`str::gt`）。
    TextMin(Option<String>),
    TextMax(Option<String>),
}

impl Accumulator {
    /// `bind_aggregate`（[`crate::sql::parser::resolve_aggregate_input`]）が型検査
    /// 済みの `(func, input)` から初期状態を作る。ここで到達しない組み合わせが
    /// あれば `bind_aggregate` 側のバグであり、[`accumulator_bug`] で fail-closed に
    /// 拒否する（黙って `Count(0)` 等へ縮退しない）。
    fn new(func: AggregateFunc, input: &AggregateInput) -> Result<Self, SqlSurfaceError> {
        use AggregateFunc::{Avg, Count, Max, Min, Sum};
        Ok(match (func, input) {
            (Count, _) => Accumulator::Count(0),
            (Sum, AggregateInput::IdU64) => Accumulator::IdSum(None),
            (Avg, AggregateInput::IdU64) => Accumulator::IdAvg {
                sum: None,
                count: 0,
            },
            (Min, AggregateInput::IdU64) => Accumulator::IdMin(None),
            (Max, AggregateInput::IdU64) => Accumulator::IdMax(None),
            (Sum, AggregateInput::ScalarExpr(_)) => Accumulator::FloatSum(None),
            (Avg, AggregateInput::ScalarExpr(_)) => Accumulator::FloatAvg {
                sum: None,
                count: 0,
            },
            (Min, AggregateInput::ScalarExpr(_)) => Accumulator::FloatMin(None),
            (Max, AggregateInput::ScalarExpr(_)) => Accumulator::FloatMax(None),
            (Min, AggregateInput::TextColumn(_)) => Accumulator::TextMin(None),
            (Max, AggregateInput::TextColumn(_)) => Accumulator::TextMax(None),
            _ => return Err(accumulator_bug("unsupported (func, input) combination")),
        })
    }

    /// 可視行 1 件を観測して状態を更新する。`scanned` は同じ行の
    /// `row_codec::scan_scalar_columns` 結果（借用のまま）。
    fn observe(
        &mut self,
        input: &AggregateInput,
        id: u64,
        embedding: &[f32],
        scanned: &[Option<&str>],
    ) -> Result<(), SqlSurfaceError> {
        match input {
            AggregateInput::AllVisible => self.observe_present(),
            AggregateInput::IdU64 => self.observe_id(id),
            AggregateInput::TextColumn(index) => {
                let value = scanned.get(*index).copied().flatten();
                self.observe_text(value)
            }
            AggregateInput::ScalarExpr(expr) => match udf_call::eval(expr, id, embedding)? {
                ExprValue::Scalar(v) => self.observe_float(v),
                // `resolve_aggregate_input` が `ExprType::Scalar` のみを
                // `ScalarExpr` として束縛するため到達しない（束縛段の型検査と
                // 評価結果の型が食い違う実装バグの検出用）。
                _ => Err(accumulator_bug(
                    "scalar-typed BoundExpr evaluated to a non-scalar value",
                )),
            },
        }
    }

    /// NULL・非存在の概念を持たない入力（`*`・`id`・`VECTOR` 列・`Scalar` 式）を
    /// 数えるだけの経路。`COUNT` 以外がこの経路に来ることはない
    /// （[`Accumulator::new`] の型検査）。
    fn observe_present(&mut self) -> Result<(), SqlSurfaceError> {
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_present on a non-Count accumulator",
            )),
        }
    }

    fn observe_id(&mut self, id: u64) -> Result<(), SqlSurfaceError> {
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::IdSum(sum) => {
                let next = match sum {
                    None => id,
                    Some(cur) => cur.checked_add(id).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range("SUM(id) exceeds u64 range")
                    })?,
                };
                *sum = Some(next);
                Ok(())
            }
            Accumulator::IdAvg { sum, count } => {
                let next = match sum {
                    None => id,
                    Some(cur) => cur.checked_add(id).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range("AVG(id) exceeds u64 range")
                    })?,
                };
                *sum = Some(next);
                *count = count.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("AVG(id) row count exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::IdMin(m) => {
                *m = Some(match m {
                    None => id,
                    Some(cur) => id.min(*cur),
                });
                Ok(())
            }
            Accumulator::IdMax(m) => {
                *m = Some(match m {
                    None => id,
                    Some(cur) => id.max(*cur),
                });
                Ok(())
            }
            _ => Err(accumulator_bug("observe_id on an incompatible accumulator")),
        }
    }

    /// `value` が `None`（`TEXT` 列 NULL）の行は `COUNT`・`MIN`・`MAX` いずれも
    /// 無視する（PostgreSQL 互換の NULL 契約。ポインタ: TASK-166・SQL-13）。
    fn observe_text(&mut self, value: Option<&str>) -> Result<(), SqlSurfaceError> {
        let Some(value) = value else {
            return Ok(());
        };
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::TextMin(m) => {
                let should_update = match m {
                    None => true,
                    Some(cur) => value < cur.as_str(),
                };
                if should_update {
                    *m = Some(try_clone_str(value)?);
                }
                Ok(())
            }
            Accumulator::TextMax(m) => {
                let should_update = match m {
                    None => true,
                    Some(cur) => value > cur.as_str(),
                };
                if should_update {
                    *m = Some(try_clone_str(value)?);
                }
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_text on an incompatible accumulator",
            )),
        }
    }

    /// `sql::udf_call::eval` は各行単独の評価結果が有限であることを保証するが
    /// （`finite_scalar`／`apply_scalar_op` 参照）、複数行にわたる**累積**は
    /// 個々が有限でもオーバーフローで非有限化しうる。ここで加算・平均の分子ごとに
    /// `is_finite()` を検査し、超過は [`SqlSurfaceError::numeric_out_of_range`]
    /// （`22003`）で fail-closed に拒否する（黙って `inf`/`NaN` を返さない）。
    fn observe_float(&mut self, v: f64) -> Result<(), SqlSurfaceError> {
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::FloatSum(s) => {
                let next = s.unwrap_or(0.0) + v;
                if !next.is_finite() {
                    return Err(SqlSurfaceError::numeric_out_of_range(
                        "SUM overflowed to a non-finite value",
                    ));
                }
                *s = Some(next);
                Ok(())
            }
            Accumulator::FloatAvg { sum, count } => {
                let next = sum.unwrap_or(0.0) + v;
                if !next.is_finite() {
                    return Err(SqlSurfaceError::numeric_out_of_range(
                        "AVG overflowed to a non-finite value",
                    ));
                }
                *sum = Some(next);
                *count = count.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("AVG row count exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::FloatMin(m) => {
                *m = Some(match m {
                    None => v,
                    Some(cur) if v.total_cmp(cur) == std::cmp::Ordering::Less => v,
                    Some(cur) => *cur,
                });
                Ok(())
            }
            Accumulator::FloatMax(m) => {
                *m = Some(match m {
                    None => v,
                    Some(cur) if v.total_cmp(cur) == std::cmp::Ordering::Greater => v,
                    Some(cur) => *cur,
                });
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_float on an incompatible accumulator",
            )),
        }
    }

    /// 空集合契約: `COUNT` は `0`、それ以外は `NULL`（PostgreSQL 互換。TASK-166・
    /// SQL-13）。`AVG` は合計を保持したまま `finish` の時点で除算する。
    fn finish(self) -> Cell {
        match self {
            Accumulator::Count(n) => Cell::Integer(n),
            Accumulator::IdSum(s) => s.map(Cell::Integer).unwrap_or(Cell::Null),
            Accumulator::IdAvg { sum, count } => match sum {
                None => Cell::Null,
                Some(s) => Cell::Float(s as f64 / count as f64),
            },
            Accumulator::IdMin(m) => m.map(Cell::Integer).unwrap_or(Cell::Null),
            Accumulator::IdMax(m) => m.map(Cell::Integer).unwrap_or(Cell::Null),
            Accumulator::FloatSum(s) => s.map(Cell::Float).unwrap_or(Cell::Null),
            Accumulator::FloatAvg { sum, count } => match sum {
                None => Cell::Null,
                Some(s) => Cell::Float(s / count as f64),
            },
            Accumulator::FloatMin(m) => m.map(Cell::Float).unwrap_or(Cell::Null),
            Accumulator::FloatMax(m) => m.map(Cell::Float).unwrap_or(Cell::Null),
            Accumulator::TextMin(m) => m.map(Cell::Text).unwrap_or(Cell::Null),
            Accumulator::TextMax(m) => m.map(Cell::Text).unwrap_or(Cell::Null),
        }
    }
}

/// `TextMin`/`TextMax` が新しい極値を保持し直す際にのみ呼ぶ（1 項目あたり高々
/// 1 本。`.claude/rules/security.md`「不安全な設計」対応で `try_reserve_exact` を
/// 使い、確保失敗を abort ではなく `54000` として返す）。
fn try_clone_str(value: &str) -> Result<String, SqlSurfaceError> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("aggregate text value exceeds available memory")
    })?;
    owned.push_str(value);
    Ok(owned)
}

fn storage_internal(e: impl Into<StorageError>) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("aggregate row scan failed: {}", e.into()),
    }
}

/// [`crate::sql::parser::BoundAggregate`] を実行し、単一行の [`QueryResult`] を返す
/// （TASK-166・SQL-13）。`read_txn` は呼び出し元（`core.rs::EngineCore::execute_sql_in_session`）
/// が `schema` 取得と同一のトランザクションから渡す（既存の検索 SELECT 実行経路
/// `sql::exec::execute_statement` と同じ「単一スナップショット」契約。Issue #56
/// レビュー指摘対応の踏襲）。
pub(crate) fn execute_aggregate(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundAggregate,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut accumulators = Vec::with_capacity(bound.items.len());
    for item in &bound.items {
        accumulators.push(Accumulator::new(item.func, &item.input)?);
    }

    // `VECTOR` 列を宣言するスキーマのみ次元検証を行う（`VECTOR` 列を持たない
    // テーブルは `row_codec`/`storage` 側で常に埋め込み次元 0 として符号化される
    // ため検証対象がない）。`arena.rs::validated_vector_dim_in_txn` と異なり
    // 「`VECTOR` 列が存在しないと失敗する」制約を持たない実装にする（SQL-13 は
    // `VECTOR` 列なしテーブルでの集計を要求するため、本関数は `arena` を経由しない
    // 独自の走査を持つ）。
    let expected_dim = schema.vector_dim();

    let row_table_name = catalog::user_rows_table_name(&bound.table);
    let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => Some(t),
        // 対象テーブルの行が 1 件も書き込まれていない（行テーブル自体が未作成）は
        // 空集合として扱う（既存の検索 SELECT 実行経路
        // `arena::build_filtered_with_rows_in_txn` と同じ契約）。
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => {
            return Err(SqlSurfaceError::Internal {
                detail: format!(
                    "aggregate row scan failed: {}",
                    catalog::map_row_table_error(e)
                ),
            })
        }
    };

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (_key_tenant, id) = k.value();
            let buf = v.value();

            // RLS 段（無条件・デコード前）: `arena.rs` の走査ループと同一の順序
            // （ヘッダのみ読み `predicate` 相当を通過しない行は embedding・
            // metadata を一切デコードしない）。不可視行の破損状態が対象テナントの
            // クエリ可用性へ干渉しない設計を踏襲する（codex P0 対応・Issue #137、
            // security.md P0「テナント境界」）。
            let (tenant_id, visibility) =
                storage::decode_row_tenant_and_visibility(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // ここに到達するのは可視行のみ。以降は完全デコードして SCALAR 段・
            // 集計へ進む。
            let row = storage::decode_row(id, buf).map_err(storage_internal)?;
            if let Some(dim) = expected_dim {
                let found = u32::try_from(row.embedding.len()).unwrap_or(u32::MAX);
                if found != dim {
                    return Err(SqlSurfaceError::Internal {
                        detail: "aggregate row scan failed: embedding dimension mismatch"
                            .to_string(),
                    });
                }
            }

            let scanned = row_codec::scan_scalar_columns(schema, &row.metadata)?;

            // SCALAR 段（WHERE）: 既存の検索 SELECT 実行経路（`sql::exec`）と同じ
            // 意味論（等価・前方一致条件 → 式述語の順）で適用する。
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            for expr in &bound.expr_filters {
                match udf_call::eval(expr, id, &row.embedding)? {
                    ExprValue::Bool(true) => {}
                    ExprValue::Bool(false) => continue 'rows,
                    // 束縛段（`sql::parser::bind_where_predicates`）が `WHERE` 式
                    // 述語の型を `Bool` に限定済みのため到達しない。
                    _ => {
                        return Err(SqlSurfaceError::invalid_input(
                            "WHERE expression did not evaluate to a boolean",
                        ))
                    }
                }
            }

            // defense-in-depth（RlsSafetyNet と同趣旨）: 上のヘッダ由来判定と
            // 独立した検査ではなく、デコード済みの行データに対して同じ
            // `PolicyContext::is_visible` を再適用するだけの重ね掛け。デコード前
            // 判定が唯一の防御線にならないようにする（security.md P0）。
            if !ctx.is_visible(&row.tenant_id, row.visibility) {
                continue;
            }

            // AGG 段。
            for (accumulator, item) in accumulators.iter_mut().zip(&bound.items) {
                accumulator.observe(&item.input, id, &row.embedding, &scanned)?;
            }
        }
    }

    let mut columns = Vec::with_capacity(bound.items.len());
    let mut cells = Vec::with_capacity(bound.items.len());
    for (accumulator, item) in accumulators.into_iter().zip(&bound.items) {
        columns.push(ColumnMeta::Computed {
            name: item.name.clone(),
        });
        cells.push(accumulator.finish());
    }

    Ok(QueryResult {
        columns,
        rows: vec![ResultRow {
            id: 0,
            score: 0.0,
            cells,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::allowlist::AggregateFunc;

    fn count_acc() -> Accumulator {
        Accumulator::new(AggregateFunc::Count, &AggregateInput::AllVisible).unwrap()
    }

    #[test]
    fn count_empty_set_is_zero() {
        let acc = count_acc();
        assert_eq!(acc.finish(), Cell::Integer(0));
    }

    #[test]
    fn sum_id_empty_set_is_null() {
        let acc = Accumulator::new(AggregateFunc::Sum, &AggregateInput::IdU64).unwrap();
        assert_eq!(acc.finish(), Cell::Null);
    }

    #[test]
    fn sum_id_overflow_is_rejected_as_numeric_out_of_range() {
        let mut acc = Accumulator::new(AggregateFunc::Sum, &AggregateInput::IdU64).unwrap();
        acc.observe_id(u64::MAX).unwrap();
        let err = acc.observe_id(1).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn avg_id_divides_sum_by_count() {
        let mut acc = Accumulator::new(AggregateFunc::Avg, &AggregateInput::IdU64).unwrap();
        acc.observe_id(2).unwrap();
        acc.observe_id(4).unwrap();
        assert_eq!(acc.finish(), Cell::Float(3.0));
    }

    #[test]
    fn float_sum_overflow_is_rejected_as_numeric_out_of_range() {
        let mut acc = Accumulator::FloatSum(None);
        acc.observe_float(f64::MAX).unwrap();
        let err = acc.observe_float(f64::MAX).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn text_min_max_skip_null_and_compare_by_byte_order() {
        let mut min = Accumulator::TextMin(None);
        let mut max = Accumulator::TextMax(None);
        for v in [Some("banana"), None, Some("apple"), Some("cherry")] {
            min.observe_text(v).unwrap();
            max.observe_text(v).unwrap();
        }
        assert_eq!(min.finish(), Cell::Text("apple".to_string()));
        assert_eq!(max.finish(), Cell::Text("cherry".to_string()));
    }

    #[test]
    fn text_min_max_empty_set_is_null() {
        let min = Accumulator::TextMin(None);
        assert_eq!(min.finish(), Cell::Null);
    }

    #[test]
    fn float_min_max_use_total_cmp() {
        let mut min = Accumulator::FloatMin(None);
        let mut max = Accumulator::FloatMax(None);
        for v in [3.0, -1.5, 42.0] {
            min.observe_float(v).unwrap();
            max.observe_float(v).unwrap();
        }
        assert_eq!(min.finish(), Cell::Float(-1.5));
        assert_eq!(max.finish(), Cell::Float(42.0));
    }
}
