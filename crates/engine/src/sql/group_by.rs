//! `GROUP BY <TEXT 列>` 集計（複数行結果、TASK-167・SQL-14）の実行本体。
//!
//! 責務境界: [`crate::sql::aggregate::execute_aggregate`] が `BoundAggregate::group_by`
//! を検出した場合にのみ呼ばれる（`GROUP BY` なしの単一行集計は `aggregate.rs` が
//! 引き続き担う）。行の走査・RLS 適用順序（ヘッダのみで可視性判定 → 可視行のみ
//! 完全デコード → `WHERE` → 可視性の再検査 → 集計）は `aggregate.rs` の単一行経路と
//! 同一の規約を踏襲する（`.claude/rules/security.md`「テナント境界（P0）」）。
//! **不可視行のグループキーは結果に一切現れない**（他テナントにしか存在しない
//! グループ値からの存在推測を防ぐ。RLS-7・RLS-8 の `GROUP BY` 版）。
//!
//! グループ数・グループキー文字列の累計バイト数はいずれも [`MAX_GROUPS`]・
//! [`MAX_GROUP_KEY_TOTAL_BYTES`] で頭打ちにし、超過は
//! [`SqlSurfaceError::payload_too_large`]（`54000`）で fail-closed に拒否する
//! （`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」対応。
//! `TEXT` 値は 1 件あたり最大 4 MiB 許容されるため、件数上限だけでは有界にならない）。

use crate::catalog::{self, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::aggregate::{accumulator_bug, storage_internal, try_clone_str, Accumulator};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::parser::{BoundAggregate, OrderTarget, ProjectionColumn};
use crate::sql::udf_call::{self, BinOp, ExprValue};
use crate::storage;
use redb::ReadableTable;
use std::collections::BTreeMap;

/// `GROUP BY` が生成してよいグループ数の上限（無制限 `BTreeMap` 確保を避ける）。
pub(crate) const MAX_GROUPS: usize = 10_000;

/// グループキー文字列（`Some` 側）が累計で保持してよいバイト数の上限。`TEXT` 列は
/// 1 件あたり最大 4 MiB を許容するため、[`MAX_GROUPS`] 件数だけでは有界にならない。
const MAX_GROUP_KEY_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// グループキー（`GROUP BY` 対象列の値）。`None` は NULL 値のグループ（`TEXT` 列の
/// NULL は 1 つのグループへまとめる。PostgreSQL 互換）。`Ord` はバイト順、`None` は
/// 常に末尾（既定の昇順ソート・[`crate::sql::exec::ColumnMeta`] へ渡す前の表示順を
/// 決定的にする）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey(Option<String>);

/// グループ表への新規グループキー追加前に有界性を検査する（[`MAX_GROUPS`]・
/// [`MAX_GROUP_KEY_TOTAL_BYTES`]）。呼び出し元が「このキーは表に存在しない」ことを
/// 確認済みの場合にのみ呼ぶ（既存キーの更新では追加コストが発生しないため呼ばない）。
/// 成功時は `total_key_bytes` へ今回のキー分のバイト数を加算する。
fn check_new_group_budget(
    current_group_count: usize,
    total_key_bytes: &mut usize,
    key: &Option<String>,
) -> Result<(), SqlSurfaceError> {
    if current_group_count >= MAX_GROUPS {
        return Err(SqlSurfaceError::payload_too_large(
            "GROUP BY result exceeds the allowed number of groups",
        ));
    }
    let added = key.as_ref().map(|s| s.len()).unwrap_or(0);
    let next = total_key_bytes.checked_add(added).ok_or_else(|| {
        SqlSurfaceError::payload_too_large("GROUP BY key size accounting overflowed")
    })?;
    if next > MAX_GROUP_KEY_TOTAL_BYTES {
        return Err(SqlSurfaceError::payload_too_large(
            "GROUP BY key values exceed the allowed total size",
        ));
    }
    *total_key_bytes = next;
    Ok(())
}

/// `HAVING`/`WHERE` 相当ではなく、完了済みグループの集計結果 [`Cell`] と数値
/// リテラルを比較する（TASK-167・SQL-14）。`Cell::Null`（空グループはあり得ないが
/// `SUM`/`MIN`/`MAX` 等の空集合契約由来で `NULL` になりうる）との比較は常に偽
/// （PostgreSQL の `NULL` 比較契約と同じ）。`Cell::Integer` は `u64`（束縛段で
/// `2^53` 以下に制限済みのリテラルとの比較のため `f64` へ変換しても精度は失われ
/// ない）、`Cell::Float` は `total_cmp` 相当の通常比較（非有限値は
/// [`Accumulator`] 側が既に拒否済みのため到達しない）。
fn having_matches(cell: &Cell, op: BinOp, literal: f64) -> bool {
    let value = match cell {
        Cell::Integer(n) => *n as f64,
        Cell::Float(f) => *f,
        // 束縛段（`sql::parser::bind_group_by_clause`）が TEXT 型の集計結果を
        // HAVING の対象として拒否済みのため到達しない。fail-closed に「不一致」
        // として扱う。
        Cell::Null | Cell::Text(_) | Cell::Vector(_) | Cell::Bool(_) => return false,
    };
    match op {
        BinOp::Gt => value > literal,
        BinOp::Lt => value < literal,
        BinOp::Ge => value >= literal,
        BinOp::Le => value <= literal,
        BinOp::Eq => value == literal,
        // 構文段（`allowlist::Parser::expect_cmp_op`）が算術演算子を HAVING の
        // 比較演算子として構造上生成しないため到達しない。
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => false,
    }
}

/// `ORDER BY` 対象 1 つの並び替えキー。`GroupKey`（`Option<String>`）と
/// `Cell`（集計結果）を共通の [`Ordering`](std::cmp::Ordering) へ写像する。
/// `Cell::Null` は常に末尾（[`GroupKey`] の `None` と同じ既定順序規約）。
fn cmp_order_value(a: &Cell, b: &Cell) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Null, _) => Ordering::Greater,
        (_, Cell::Null) => Ordering::Less,
        (Cell::Integer(x), Cell::Integer(y)) => x.cmp(y),
        (Cell::Integer(x), Cell::Float(y)) => (*x as f64).total_cmp(y),
        (Cell::Float(x), Cell::Integer(y)) => x.total_cmp(&(*y as f64)),
        (Cell::Float(x), Cell::Float(y)) => x.total_cmp(y),
        (Cell::Text(x), Cell::Text(y)) => x.cmp(y),
        // 型不一致は束縛段で発生しないため到達しない。決定的な安定順序として
        // Equal を返す（並び替え全体が破綻しないようにする防御的フォールバック）。
        _ => Ordering::Equal,
    }
}

/// [`BoundAggregate`]（`group_by` が `Some` であることを前提。呼び出し元
/// [`crate::sql::aggregate::execute_aggregate`] が判定済み）を実行し、複数行の
/// [`QueryResult`] を返す（TASK-167・SQL-14）。RLS 適用順序・行走査は
/// `aggregate.rs::execute_aggregate` の単一行経路と同一の規約
/// （ヘッダのみで可視性判定 → 可視行のみ完全デコード → `WHERE` → 可視性再検査）を
/// 独立して踏襲する（責務分離のためモジュールを分けたことによる意図的な複製。
/// 変更する際は両モジュールの規約を揃えること）。
pub(crate) fn execute_grouped_aggregate(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundAggregate,
) -> Result<QueryResult, SqlSurfaceError> {
    let group_by = bound
        .group_by
        .as_ref()
        .ok_or_else(|| accumulator_bug("execute_grouped_aggregate called without a GROUP BY"))?;

    let expected_dim = schema.vector_dim();
    let row_table_name = catalog::user_rows_table_name(&bound.table);
    let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => Some(t),
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

    let mut groups: BTreeMap<GroupKey, Vec<Accumulator>> = BTreeMap::new();
    let mut total_key_bytes: usize = 0;

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // RLS 段（無条件・デコード前）: `aggregate.rs` の単一行経路と同一順序。
            let (tenant_id, visibility) =
                storage::decode_row_tenant_and_visibility(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            let row = storage::decode_row_for_key(key_tenant, id, buf).map_err(storage_internal)?;
            if let Some(dim) = expected_dim {
                if !row.embedding.is_empty() {
                    let found = u32::try_from(row.embedding.len()).unwrap_or(u32::MAX);
                    if found != dim {
                        return Err(SqlSurfaceError::Internal {
                            detail: "aggregate row scan failed: embedding dimension mismatch"
                                .to_string(),
                        });
                    }
                }
            }

            let scanned = row_codec::scan_scalar_columns(schema, &row.metadata)?;

            // SCALAR 段（WHERE）。
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            for expr in &bound.expr_filters {
                match udf_call::eval(expr, id, &row.embedding)? {
                    ExprValue::Bool(true) => {}
                    ExprValue::Bool(false) => continue 'rows,
                    _ => {
                        return Err(SqlSurfaceError::invalid_input(
                            "WHERE expression did not evaluate to a boolean",
                        ))
                    }
                }
            }

            // defense-in-depth（RlsSafetyNet と同趣旨）。
            if !ctx.is_visible(&row.tenant_id, row.visibility) {
                continue;
            }

            // GROUP 段: グループキーを確定してから、可視行のみをグループ表へ
            // 反映する（このため他テナントにしか存在しないキーはグループとして
            // 一切現れない＝RLS-7・RLS-8 の `GROUP BY` 版）。
            let key_value = scanned.get(group_by.column_index).copied().flatten();
            let key = GroupKey(match key_value {
                Some(v) => Some(try_clone_str(v)?),
                None => None,
            });

            if !groups.contains_key(&key) {
                check_new_group_budget(groups.len(), &mut total_key_bytes, &key.0)?;
                let mut accs = Vec::new();
                accs.try_reserve_exact(bound.items.len()).map_err(|_| {
                    SqlSurfaceError::payload_too_large(
                        "aggregate accumulator allocation exceeds available memory",
                    )
                })?;
                for item in &bound.items {
                    accs.push(Accumulator::new(item.func, &item.input)?);
                }
                groups.insert(key.clone(), accs);
            }
            let accs = groups
                .get_mut(&key)
                .ok_or_else(|| accumulator_bug("group entry disappeared after insertion"))?;

            for (accumulator, item) in accs.iter_mut().zip(&bound.items) {
                accumulator.observe(&item.input, id, &row.embedding, &scanned)?;
            }
        }
    }

    // FINISH 段: 各グループを確定 Cell へ変換し、HAVING で絞り込む。`h.item_index`・
    // `ProjectionColumn::Aggregate.item_index` は束縛段
    // （`sql::parser::bind_group_by_clause`）が `items.len()` 範囲内であることを
    // 保証済みの内部添字だが、untrusted 入力に由来する添字アクセスを避ける
    // 方針（`.claude/rules/coding-rust.md`）に従い、ここでも `.get()` で明示的に
    // 扱い、万一の不整合は panic ではなく [`accumulator_bug`]（`XX000`）へ落とす。
    let mut finished: Vec<(GroupKey, Vec<Cell>)> = Vec::with_capacity(groups.len());
    for (key, accs) in groups {
        let cells: Vec<Cell> = accs.into_iter().map(Accumulator::finish).collect();
        let mut keep = true;
        for h in &group_by.having {
            let cell = cells
                .get(h.item_index)
                .ok_or_else(|| accumulator_bug("HAVING item_index out of bounds"))?;
            if !having_matches(cell, h.op, h.literal) {
                keep = false;
                break;
            }
        }
        if keep {
            finished.push((key, cells));
        }
    }

    // ORDER BY: 未指定時はグループキー昇順（`GroupKey` の `Ord`。NULL は末尾）。
    // `sort_by` のクロージャは `Result` を返せないため、`.get()` の失敗（内部
    // 不整合。到達しない想定）は `Cell::Null` へ安全側にフォールバックする
    // （panic させない。誤った順序になり得るが、束縛段の保証によりそもそも
    // 到達しない防御的分岐）。
    match &group_by.order_by {
        Some(order_by) => {
            finished.sort_by(|(ka, ca), (kb, cb)| {
                let primary = match order_by.target {
                    OrderTarget::GroupKey => ka.cmp(kb),
                    OrderTarget::Aggregate(idx) => cmp_order_value(
                        ca.get(idx).unwrap_or(&Cell::Null),
                        cb.get(idx).unwrap_or(&Cell::Null),
                    ),
                };
                let primary = if order_by.descending {
                    primary.reverse()
                } else {
                    primary
                };
                // 安定した決定性のため、同値はグループキー順で tie-break する。
                primary.then_with(|| ka.cmp(kb))
            });
        }
        None => finished.sort_by(|(ka, _), (kb, _)| ka.cmp(kb)),
    }

    if let Some(limit) = group_by.limit {
        finished.truncate(limit);
    }

    // PROJECT 段: `bound.projection` の列順で `GroupKey`／集計結果を組み立てる。
    let mut columns = Vec::with_capacity(bound.projection.len());
    for col in &bound.projection {
        let name = match col {
            ProjectionColumn::GroupKey { name } => name.clone(),
            ProjectionColumn::Aggregate { name, .. } => name.clone(),
        };
        columns.push(ColumnMeta::Computed { name });
    }

    let mut rows = Vec::with_capacity(finished.len());
    for (key, cells) in finished {
        let mut row_cells = Vec::with_capacity(bound.projection.len());
        for col in &bound.projection {
            let cell =
                match col {
                    ProjectionColumn::GroupKey { .. } => match &key.0 {
                        Some(s) => Cell::Text(s.clone()),
                        None => Cell::Null,
                    },
                    ProjectionColumn::Aggregate { item_index, .. } => cells
                        .get(*item_index)
                        .cloned()
                        .ok_or_else(|| accumulator_bug("projection item_index out of bounds"))?,
                };
            row_cells.push(cell);
        }
        rows.push(ResultRow {
            id: 0,
            score: 0.0,
            cells: row_cells,
        });
    }

    Ok(QueryResult { columns, rows })
}
