//! `GROUP BY <TEXT 列>` 集計（複数行結果、TASK-167・SQL-14）の実行本体。
//!
//! 責務境界: [`crate::sql::aggregate::execute_aggregate`] が `BoundAggregate::group_by`
//! を検出した場合にのみ呼ばれる（`GROUP BY` なしの単一行集計は `aggregate.rs` が
//! 引き続き担う）。行の走査・RLS 適用順序（ヘッダのみで可視性判定 → 可視行のみ
//! ヘッダのオフセットを引き継いで本体デコード → `WHERE` → 可視性の再検査 → 集計）は
//! `aggregate.rs` の単一行経路と同一の規約を踏襲する（`.claude/rules/security.md`
//! 「テナント境界（P0）」。スクラッチ再利用による二重デコード排除は Issue #349・
//! Issue #314 の横展開）。
//! **不可視行のグループキーは結果に一切現れない**（他テナントにしか存在しない
//! グループ値からの存在推測を防ぐ。RLS-7・RLS-8 の `GROUP BY` 版）。
//!
//! グループ数・グループキー文字列の累計バイト数・`MIN`/`MAX(<TEXT 列>)` 集計状態
//! （`Accumulator::TextMin`/`TextMax`）の累計バイト数は、それぞれ [`MAX_GROUPS`]・
//! [`MAX_GROUP_KEY_TOTAL_BYTES`]・[`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES`] で頭打ちに
//! し、超過は [`SqlSurfaceError::payload_too_large`]（`54000`）で fail-closed に
//! 拒否する（`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」
//! 対応。`TEXT` 値は 1 件あたり最大 4 MiB 許容されるため、件数上限だけでは有界に
//! ならない。単一行集計〔`aggregate.rs`〕は `TextMin`/`TextMax` インスタンスが
//! 項目数分（高々 SELECT リスト長）で頭打ちだが、`GROUP BY` はグループ数倍に
//! なるため別途累計管理が必要。PR #230 codex-review 指摘対応）。

use crate::catalog::{self, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::aggregate::{accumulator_bug, storage_internal, try_clone_str, Accumulator};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::parser::{BoundAggregate, OrderTarget, ProjectionColumn};
use crate::sql::udf_call::{BinOp, ExprValue};
use crate::storage::{self, StorageError};
use redb::ReadableTable;
use std::collections::BTreeMap;

/// `GROUP BY` が生成してよいグループ数の上限（無制限 `BTreeMap` 確保を避ける）。
pub(crate) const MAX_GROUPS: usize = 10_000;

/// グループキー文字列（`Some` 側）が累計で保持してよいバイト数の上限。`TEXT` 列は
/// 1 件あたり最大 4 MiB を許容するため、[`MAX_GROUPS`] 件数だけでは有界にならない。
const MAX_GROUP_KEY_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// クエリ全体で `MIN`/`MAX(<TEXT 列>)` 集計項目（`Accumulator::TextMin`/`TextMax`）が
/// 保持してよい文字列の累計バイト数の上限。`TEXT` 列は 1 件あたり最大 4 MiB を
/// 許容し、`GROUP BY` は最大 [`MAX_GROUPS`] グループ×集計項目数だけ独立した
/// アキュムレータを保持しうるため、件数上限だけでは有界にならない
/// （[`MAX_GROUP_KEY_TOTAL_BYTES`] と同じ予算規模を採用）。
const MAX_TEXT_ACCUMULATOR_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// グループキー（`GROUP BY` 対象列の値）。`None` は NULL 値のグループ（`TEXT` 列の
/// NULL は 1 つのグループへまとめる。PostgreSQL 互換）。`Ord` はバイト順、`None` は
/// 常に末尾（既定の昇順ソート・[`crate::sql::exec::ColumnMeta`] へ渡す前の表示順を
/// 決定的にする）。`Option<String>` の派生 `Ord`（`None` が先頭）とは逆順になるため
/// 手動実装する（PR #230 codex-review/Bugbot 指摘: 派生 `Ord` のままだと既定順序・
/// `ORDER BY` 未指定時に `NULL` グループが先頭に来て `LIMIT` が意図した先頭の
/// 非 `NULL` グループを取りこぼす）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupKey(Option<String>);

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => a.cmp(b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    }
}

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

/// `Cell::Integer`（`u64`。`COUNT`/`SUM` 等の集計結果で `2^53` を超えうる）と
/// HAVING リテラル（`f64`。構文段 `parse_number_literal` が非有限値を拒否済み）
/// を精度損失なく比較し、両者の大小関係を返す（PR #230 codex-review 指摘対応:
/// 以前は `Cell::Integer` を無条件に `f64` へキャストしていたため、`2^53` 超の
/// 集計値が丸められ `HAVING` の等号・不等号比較が誤判定しうた）。
fn cmp_integer_to_literal(n: u64, literal: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if literal.is_nan() {
        // 到達しない想定（構文段が非有限値を拒否済み）。fail-closed に「常に
        // 不一致」となるよう Equal 以外を返す（呼び出し元の Eq 判定が false
        // になれば十分なため、方向は問わない）。
        return Ordering::Greater;
    }
    if literal < 0.0 {
        // `u64` は常に 0 以上のため、負のリテラルより常に大きい。
        return Ordering::Greater;
    }
    if literal >= 18_446_744_073_709_551_616.0 {
        // 2^64（`u64` の表現域の上限超）。`n` は常にこれより小さい。
        return Ordering::Less;
    }
    // 上の範囲チェックにより `literal.floor()` は [0, 2^64) に収まるため、
    // `as u64` は精度・範囲の両面で安全（Rust の float→int キャストは
    // 飽和変換であり未定義動作にならない）。
    let floor_u64 = literal.floor() as u64;
    match n.cmp(&floor_u64) {
        Ordering::Equal if literal.fract() != 0.0 => {
            // n == floor(literal) だが literal 自体は非整数 → 実際には n < literal。
            Ordering::Less
        }
        other => other,
    }
}

/// `HAVING`/`WHERE` 相当ではなく、完了済みグループの集計結果 [`Cell`] と数値
/// リテラルを比較する（TASK-167・SQL-14）。`Cell::Null`（空グループはあり得ないが
/// `SUM`/`MIN`/`MAX` 等の空集合契約由来で `NULL` になりうる）との比較は常に偽
/// （PostgreSQL の `NULL` 比較契約と同じ）。`Cell::Integer` は
/// [`cmp_integer_to_literal`] で精度損失なく比較する（`SUM(id)` 等 `2^53` を
/// 超えうる値を無条件に `f64` へキャストしない）。`Cell::Float` は `total_cmp`
/// 相当の通常比較（非有限値は [`Accumulator`] 側が既に拒否済みのため到達しない）。
fn having_matches(cell: &Cell, op: BinOp, literal: f64) -> bool {
    match cell {
        Cell::Integer(n) => {
            use std::cmp::Ordering;
            let ord = cmp_integer_to_literal(*n, literal);
            match op {
                BinOp::Gt => ord == Ordering::Greater,
                BinOp::Lt => ord == Ordering::Less,
                BinOp::Ge => ord != Ordering::Less,
                BinOp::Le => ord != Ordering::Greater,
                BinOp::Eq => ord == Ordering::Equal,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => false,
            }
        }
        Cell::Float(f) => match op {
            BinOp::Gt => *f > literal,
            BinOp::Lt => *f < literal,
            BinOp::Ge => *f >= literal,
            BinOp::Le => *f <= literal,
            BinOp::Eq => *f == literal,
            // 構文段（`allowlist::Parser::expect_cmp_op`）が算術演算子を HAVING の
            // 比較演算子として構造上生成しないため到達しない。
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => false,
        },
        // 束縛段（`sql::parser::bind_group_by_clause`）が TEXT 型の集計結果を
        // HAVING の対象として拒否済みのため到達しない。fail-closed に「不一致」
        // として扱う。
        Cell::Null | Cell::Text(_) | Cell::Vector(_) | Cell::Bool(_) => false,
    }
}

/// `ORDER BY` 対象 1 つの並び替えキーのうち、非 `NULL` 値どうしの比較のみを行う
/// （`Cell`（集計結果）を共通の [`Ordering`](std::cmp::Ordering) へ写像する）。
/// `NULL` 配置（常に末尾）の判定は呼び出し元 [`order_with_nulls_last`] が方向反転
/// より外側で行うため、ここでは非 `NULL` 値どうしの大小関係のみを返す（PR #230
/// codex-review 指摘: 以前は `Cell::Null` の末尾配置を含めた `Ordering` 全体を
/// `ORDER BY ... DESC` で `.reverse()` していたため、`NULL` が先頭に来て `LIMIT`
/// が非 `NULL` グループを取りこぼしていた）。
fn cmp_cell_values(a: &Cell, b: &Cell) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Cell::Integer(x), Cell::Integer(y)) => x.cmp(y),
        (Cell::Integer(x), Cell::Float(y)) => (*x as f64).total_cmp(y),
        (Cell::Float(x), Cell::Integer(y)) => x.total_cmp(&(*y as f64)),
        (Cell::Float(x), Cell::Float(y)) => x.total_cmp(y),
        (Cell::Text(x), Cell::Text(y)) => x.cmp(y),
        // `Cell::Null` は呼び出し元が別途処理するため、ここへ渡ってきても
        // （防御的フォールバックとして）到達しない想定。型不一致も同様に
        // Equal を返す（並び替え全体が破綻しないようにする防御的フォールバック）。
        _ => Ordering::Equal,
    }
}

/// `ORDER BY` の並び順を、`NULL` 配置（常に末尾）を [`BoundOrderBy::descending`]
/// による方向反転の外側で確定させたうえで返す（PR #230 codex-review 指摘対応）。
/// `a_is_null`/`b_is_null` は比較対象（`GroupKey` の `None` または
/// `Cell::Null`）が `NULL` かどうか、`value_cmp` は両者が非 `NULL` の場合の
/// 大小関係（[`cmp_cell_values`] 等）。`DESC` 指定時も `NULL` は常に末尾に残る
/// （`GroupKey`/[`cmp_order_value`] 系がこれまで守ってきた既定順序規約と同じ）。
fn order_with_nulls_last(
    a_is_null: bool,
    b_is_null: bool,
    value_cmp: std::cmp::Ordering,
    descending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a_is_null, b_is_null) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            if descending {
                value_cmp.reverse()
            } else {
                value_cmp
            }
        }
    }
}

/// [`BoundAggregate`]（`group_by` が `Some` であることを前提。呼び出し元
/// [`crate::sql::aggregate::execute_aggregate`] が判定済み）を実行し、複数行の
/// [`QueryResult`] を返す（TASK-167・SQL-14）。RLS 適用順序・行走査は
/// `aggregate.rs::execute_aggregate` の単一行経路と同一の規約
/// （ヘッダのみで可視性判定 → 可視行のみヘッダのオフセットを引き継いで本体デコード
/// → `WHERE` → 可視性再検査）を独立して踏襲する（責務分離のためモジュールを分けた
/// ことによる意図的な複製。変更する際は両モジュールの規約を揃えること）。
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
    let mut total_text_accumulator_bytes: usize = 0;
    // Issue #353: `ExprProgram::eval` の明示スタック。行ループの外で 1 回だけ
    // 確保し、WHERE 式述語・`ScalarExpr` 集計項目の評価で使い回す
    // （`sql::aggregate::execute_aggregate` と同方針）。
    let mut expr_scratch: Vec<ExprValue> = Vec::new();
    // 可視行ごとの embedding デコード先スクラッチバッファ（Issue #349・Issue #314
    // 横展開。`aggregate.rs::execute_aggregate` と同じ方針）。
    let mut embedding_scratch: Vec<f32> = Vec::new();

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // RLS 段（無条件・デコード前）: `aggregate.rs` の単一行経路と同一順序。
            // `offset` は本体デコードの再開位置（Issue #349: ヘッダの二重デコード
            // 排除。`aggregate.rs::execute_aggregate` のドキュメント参照）。
            let (tenant_id, visibility, offset) =
                storage::decode_row_header(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // TABLE-12 の整合検査（`aggregate.rs::execute_aggregate` と同一。
            // 従来 `storage::decode_row_for_key` の内部検査だったものを明示比較へ
            // 移設）。
            if tenant_id != key_tenant {
                return Err(storage_internal(StorageError::Codec(
                    "row key tenant mismatch".to_string(),
                )));
            }

            let (dim, metadata) =
                storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                    .map_err(storage_internal)?;
            if let Some(expected) = expected_dim {
                if dim != 0 && dim != expected {
                    return Err(SqlSurfaceError::Internal {
                        detail: "aggregate row scan failed: embedding dimension mismatch"
                            .to_string(),
                    });
                }
            }

            let scanned = row_codec::scan_scalar_columns(schema, metadata)?;

            // SCALAR 段（WHERE）。
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            for program in &bound.expr_filter_programs {
                match program.eval(id, &embedding_scratch, &mut expr_scratch)? {
                    ExprValue::Bool(true) => {}
                    ExprValue::Bool(false) => continue 'rows,
                    _ => {
                        return Err(SqlSurfaceError::invalid_input(
                            "WHERE expression did not evaluate to a boolean",
                        ))
                    }
                }
            }

            // 構造的なトリップワイヤ（独立した二重検証ではない点を含め
            // `aggregate.rs::execute_aggregate` の同一箇所のドキュメント参照）。
            if !ctx.is_visible(tenant_id, visibility) {
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
                // `MIN`/`MAX(<TEXT 列>)` は 1 グループ・1 項目あたり高々 1 本の
                // `String` を保持するが、`GROUP BY` はグループ数倍に増えるため
                // クエリ全体の累計バイト数を予算管理する（before/after 比較で、
                // 増加方向は加算・縮小方向〔より短い極値への更新〕は減算し、
                // 実際の保持量を正確に反映する）。
                let before = accumulator.text_len();
                accumulator.observe(
                    &item.input,
                    id,
                    &embedding_scratch,
                    &scanned,
                    &mut expr_scratch,
                )?;
                let after = accumulator.text_len();
                if after > before {
                    let delta = after - before;
                    total_text_accumulator_bytes = total_text_accumulator_bytes
                        .checked_add(delta)
                        .ok_or_else(|| {
                            SqlSurfaceError::payload_too_large(
                                "GROUP BY TEXT aggregate size accounting overflowed",
                            )
                        })?;
                    if total_text_accumulator_bytes > MAX_TEXT_ACCUMULATOR_TOTAL_BYTES {
                        return Err(SqlSurfaceError::payload_too_large(
                            "GROUP BY TEXT aggregate state exceeds the allowed total size",
                        ));
                    }
                } else if after < before {
                    // MIN/MAX(TEXT) の極値がより短い文字列へ更新された縮小方向。
                    // 実際の保持量を正確に反映するため減算する（`checked_sub` の
                    // 失敗＝内部不整合は `XX000` の accumulator_bug へ落とし、
                    // fail-open にはしない）。減算しないと過去の増加量が
                    // 累積し続け、実保持量が予算内でも正常なクエリを
                    // 誤って 54000 で拒否してしまう。
                    let delta = before - after;
                    total_text_accumulator_bytes = total_text_accumulator_bytes
                        .checked_sub(delta)
                        .ok_or_else(|| {
                            accumulator_bug("GROUP BY TEXT aggregate size accounting underflowed")
                        })?;
                }
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
                    OrderTarget::GroupKey => order_with_nulls_last(
                        ka.0.is_none(),
                        kb.0.is_none(),
                        match (&ka.0, &kb.0) {
                            (Some(a), Some(b)) => a.cmp(b),
                            _ => std::cmp::Ordering::Equal,
                        },
                        order_by.descending,
                    ),
                    OrderTarget::Aggregate(idx) => {
                        let ca_cell = ca.get(idx).unwrap_or(&Cell::Null);
                        let cb_cell = cb.get(idx).unwrap_or(&Cell::Null);
                        order_with_nulls_last(
                            matches!(ca_cell, Cell::Null),
                            matches!(cb_cell, Cell::Null),
                            cmp_cell_values(ca_cell, cb_cell),
                            order_by.descending,
                        )
                    }
                };
                // 安定した決定性のため、同値はグループキー順で tie-break する
                // （`DESC` でも `NULL` は末尾のまま。`GroupKey::Ord` を使う昇順の
                // tie-break はそもそも方向反転の対象外）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::allowlist::AggregateFunc;
    use crate::sql::parser::{AggregateInput, BoundAggregate, BoundAggregateItem, BoundGroupBy};
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    /// 物理キー側 `tenant_id`（`key_tenant`）とヘッダ側 `tenant_id`
    /// （`header_tenant`）を意図的にずらして raw redb 書き込みする（TABLE-12 の
    /// 整合検査を検証するための専用ヘルパ。`sql::aggregate::tests` の同名ヘルパと
    /// 同じ方針。Issue #349）。
    fn write_row_with_mismatched_key_tenant(
        storage: &Storage,
        table_name: &str,
        key_tenant: &str,
        header_tenant: &str,
        id: u64,
        embedding: &[f32],
    ) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id: header_tenant,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((key_tenant, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    fn schema_with_text_group_column() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    fn bound_count_star_grouped_by_lang() -> BoundAggregate {
        BoundAggregate {
            table: "docs".to_string(),
            items: vec![BoundAggregateItem {
                func: AggregateFunc::Count,
                input: AggregateInput::AllVisible,
                name: "result".to_string(),
            }],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            rls_predicate_present: false,
            projection: vec![
                crate::sql::parser::ProjectionColumn::GroupKey {
                    name: "lang".to_string(),
                },
                crate::sql::parser::ProjectionColumn::Aggregate {
                    item_index: 0,
                    name: "result".to_string(),
                },
            ],
            group_by: Some(BoundGroupBy {
                column_index: 1,
                having: Vec::new(),
                order_by: None,
                limit: None,
            }),
        }
    }

    // Issue #349: TABLE-12 の整合検査（物理キー側 `tenant_id` とヘッダ側
    // `tenant_id` の不一致）が、`decode_row_for_key` 呼び出しをやめた後の
    // 明示比較でも従来どおり fail-closed（`XX000`・`SqlSurfaceError::Internal`）に
    // 拒否されることを固定する（`sql::aggregate::tests` の単一行経路と同一の
    // 回帰を `GROUP BY` 経路で検証する）。
    #[test]
    fn key_tenant_header_tenant_mismatch_is_rejected_fail_closed() {
        let path = unique_db_path("group-by-table12-mismatch");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text_group_column();
        storage.create_table(&schema).expect("create table");

        write_row_with_mismatched_key_tenant(
            &storage,
            "docs",
            "tenant-a",
            "tenant-b",
            1,
            &[1.0, 2.0, 3.0],
        );

        let ctx = PolicyContext::new("tenant-b").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound = bound_count_star_grouped_by_lang();
        let err = execute_grouped_aggregate(&read_txn, &ctx, &schema, &bound)
            .expect_err("key/header tenant mismatch must be rejected fail-closed");
        assert_eq!(err.wire_code(), "XX000");
    }
}
