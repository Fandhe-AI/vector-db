//! WHERE 句のサブクエリ（`IN (SELECT ...)`・`EXISTS (SELECT ...)`）を実行前に
//! 解決する（Issue #927・SQL-29 (a)・RLS-10 (b)・TASK-213）。
//!
//! 責務境界: `sql::allowlist::Parser` が構文的に受理した
//! [`WherePredicate::InSubquery`]／[`WherePredicate::Exists`]（内側の生
//! トークン列を保持するのみで未評価）を、`core.rs` の `Statement::Scan`／
//! `Statement::Aggregate` 実行アームが束縛（`sql::parser::bind_scan`／
//! `bind_aggregate`）の**前**に本モジュールへ渡す。内側は外側と同じ
//! `PolicyContext`（RLS 暗黙適用）・同じ `read_txn`（同一スナップショット）で
//! `sql::allowlist::validate_sql_tokens_with_subquery_ctx` → `sql::parser::
//! bind_scan` → `sql::scan::execute_scan` という、通常の広域取得 SELECT と
//! 完全に同じ経路で実行し、結果を具体的な `WherePredicate`（`Or`／
//! `Equality`／`BoolEquality`）へ書き換える。これにより `sql::where_tree`・
//! `sql::parser::bind_where_predicates` は一切変更せず、サブクエリを含まない
//! 文と同じ評価器を再利用する（CLAUDE.md「委譲方針」＝第 2 の評価器を作らない）。
//!
//! 対応する内側の形は `SELECT <単一列> FROM <table> [WHERE ...] LIMIT <n>`
//! （広域取得＝[`Statement::Scan`]）のみ。ランキング付き検索 SELECT・集計
//! （`GROUP BY`）を内側に書く形・内側の `LIMIT` 省略・相関参照（外側の列を
//! 内側から参照する形）は本 Issue のスコープ外で `42601`／`22000` にする
//! （`docs/design/sql-subquery.md` 参照）。相関の検出は専用の構造解析を持たず、
//! 内側の束縛（`bind_scan`）が「内側テーブルのスキーマに存在しない列」として
//! 既存の `22000`（unknown column）へ落とすことに委ねる（内側は常に自分の
//! FROM テーブルのスキーマのみで束縛されるため、外側の列を参照しても
//! 構造的に解決できない。fail-closed）。
//!
//! DoS 対策（security.md「不安全な設計｜無制限リソース確保」対応）:
//! - ネスト深さは構文解析段（[`super::allowlist::MAX_SUBQUERY_DEPTH`]・
//!   `Parser::require_subquery_depth`）が担う。
//! - 1 文（トップレベル実行 1 回）あたりの内側クエリ実行回数は本モジュールの
//!   [`MAX_SUBQUERY_EXECUTIONS`] で頭打ちにする（`budget` を呼び出し階層全体で
//!   共有する `&mut usize` として引き回す）。
//! - 内側の可視行数は [`crate::core::MAX_SEARCH_K`] を超えたら `54000`
//!   （内側の `LIMIT` 自体の範囲検証は `bind_scan` の既存契約に委ねた上での
//!   追加の防御的上限）。

use super::allowlist::{Statement, TableLookup, WherePredicate};
use super::exec::Cell;
use super::lexer::Token;
use super::udf_call::UdfRegistry;
use crate::policy::PolicyContext;

/// 1 文（トップレベル実行 1 回）あたりに実行できる内側サブクエリの総数
/// （実装既定値。Issue #927・TASK-213。ネスト深さ上限
/// [`super::allowlist::MAX_SUBQUERY_DEPTH`] と組み合わせて評価コストを
/// 有界にする）。
pub(crate) const MAX_SUBQUERY_EXECUTIONS: usize = 16;

/// `where_predicates`（トップレベルの述語列。`WherePredicate::Or` の分岐も
/// 再帰的に辿る）に含まれる `InSubquery`／`Exists` をすべて解決し、具体的な
/// `WherePredicate` へ書き換えた新しい述語列を返す。`core.rs` の
/// `Statement::Scan`／`Statement::Aggregate` 実行アームが、束縛
/// （`bind_scan`／`bind_aggregate`）の直前に呼ぶ。
///
/// `budget` は呼び出し階層全体（ネストしたサブクエリを含む）で共有する残り
/// 実行回数。呼び出し元は [`MAX_SUBQUERY_EXECUTIONS`] で初期化する。
pub(crate) fn resolve_where_predicates(
    predicates: Vec<WherePredicate>,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
) -> Result<Vec<WherePredicate>, crate::sql::allowlist::SqlSurfaceError> {
    let mut out = Vec::with_capacity(predicates.len());
    for predicate in predicates {
        match predicate {
            WherePredicate::Or(branches) => {
                let mut resolved_branches = Vec::with_capacity(branches.len());
                for branch in branches {
                    resolved_branches.push(resolve_where_predicates(
                        branch, read_txn, ctx, lookup, udfs, budget,
                    )?);
                }
                out.push(WherePredicate::Or(resolved_branches));
            }
            WherePredicate::InSubquery {
                column,
                inner_tokens,
                depth,
            } => {
                let resolved = resolve_in_subquery(
                    &column,
                    &inner_tokens,
                    depth,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    budget,
                )?;
                out.push(resolved);
            }
            WherePredicate::Exists {
                inner_tokens,
                depth,
            } => {
                let exists = resolve_exists_subquery(
                    &inner_tokens,
                    depth,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    budget,
                )?;
                if !exists {
                    // 常に偽: 分岐 0 個の `Or` は `where_tree::BoundOrGroup::matches`
                    // が必ず `false` を返す（`sql::where_tree` モジュール
                    // ドキュメント参照）。`EXISTS` が偽の行を除外しつつ、
                    // `has_where_filters()` は真のままに保つ（フィルタなし専用
                    // キャッシュへ誤って乗せない）。
                    out.push(WherePredicate::Or(Vec::new()));
                }
                // 真の場合は述語自体を追加しない（制約を課さない＝常に真）。
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// 内側トークン列を検証・実行し、`sql::allowlist::Statement::Scan` として
/// 妥当であることを確認した上で、束縛前の残りの解決（自身の WHERE に含まれる
/// さらに深いサブクエリ）を行い、`sql::scan::execute_scan` で実行する。
fn execute_inner_scan(
    inner_tokens: &[Token],
    depth: usize,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
) -> Result<super::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    *budget = budget.checked_sub(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large(format!(
            "subquery execution count exceeds limit {MAX_SUBQUERY_EXECUTIONS}"
        ))
    })?;

    let stmt =
        super::allowlist::validate_sql_tokens_with_subquery_ctx(inner_tokens, lookup, depth)?;
    let mut validated = match stmt {
        Statement::Scan(validated) => validated,
        _ => {
            return Err(SqlSurfaceError::unsupported(
                "subquery must be a plain SELECT ... FROM ... [WHERE ...] LIMIT n \
                 (no ORDER BY / HYBRID / USING PLAN / GROUP BY)",
            ))
        }
    };

    // 自身の WHERE に含まれるさらに深いサブクエリを、束縛（`bind_scan`）の前に
    // 解決する（深さ優先。`depth` は構文解析段で `MAX_SUBQUERY_DEPTH` 検査
    // 済みのため、ここでは budget のみ検査すれば足りる）。
    validated.where_predicates = resolve_where_predicates(
        validated.where_predicates,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
    )?;

    let inner_schema = crate::catalog::get_table_schema_in_txn(read_txn, &validated.table_name)
        .map_err(|e| match e {
            crate::catalog::CatalogError::TableNotFound(name) => {
                SqlSurfaceError::undefined_table(name)
            }
            other => crate::catalog::table_lookup_error(other),
        })?;
    let bound = super::parser::bind_scan(&validated, &inner_schema, udfs)?;
    let result = super::scan::execute_scan(read_txn, ctx, &inner_schema, &bound)?;

    // `bind_scan` の LIMIT 範囲検証とは独立に、内側の可視結果行数を
    // `MAX_SEARCH_K` で頭打ちにする防御的な上限（security.md「不安全な設計」
    // 対応。内側 LIMIT の値自体は利用者が指定できるため、既存の範囲検証の
    // 上限がこの値より緩い場合の保険）。
    if result.rows.len() > crate::core::MAX_SEARCH_K {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "subquery result row count exceeds limit {}",
            crate::core::MAX_SEARCH_K
        )));
    }

    Ok(result)
}

/// `<column> IN (SELECT ...)` を解決する。内側は投影列がちょうど 1 列である
/// ことを要求し（`22000`）、各行のセルを [`cell_to_equality_value`] で
/// `<column> = <値>` 相当の葉へ変換した上で `WherePredicate::Or` として
/// 束ねる（0 行なら空の `Or` ＝常に偽）。
#[allow(clippy::too_many_arguments)]
fn resolve_in_subquery(
    column: &str,
    inner_tokens: &[Token],
    depth: usize,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
) -> Result<WherePredicate, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    let result = execute_inner_scan(inner_tokens, depth, read_txn, ctx, lookup, udfs, budget)?;
    if result.columns.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used with IN must select exactly one column",
        ));
    }

    let mut branches = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let cell = row.cells.first().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery row missing projected cell".to_string(),
        })?;
        if let Some(leaf) = cell_to_equality_predicate(column, cell)? {
            branches.push(vec![leaf]);
        }
        // NULL セルは照合から除く（`WherePredicate::InSubquery` ドキュメント・
        // NULL の意味論参照）。
    }
    Ok(WherePredicate::Or(branches))
}

/// `EXISTS (SELECT ...)` を解決し、可視行が 1 件以上存在するかどうかを返す。
#[allow(clippy::too_many_arguments)]
fn resolve_exists_subquery(
    inner_tokens: &[Token],
    depth: usize,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
) -> Result<bool, crate::sql::allowlist::SqlSurfaceError> {
    let result = execute_inner_scan(inner_tokens, depth, read_txn, ctx, lookup, udfs, budget)?;
    Ok(!result.rows.is_empty())
}

/// 内側の投影セル 1 件を `<column> = <値>` 相当の `WherePredicate` 葉へ変換する。
/// `NULL` は `Ok(None)`（呼び出し元が集合から除外する）。対応するのは `TEXT`
/// （`Cell::Text`）・`INTEGER`/`BIGINT`（`Cell::SignedInteger`）・`BOOLEAN`
/// （`Cell::Bool`）のみ。`Cell::Integer`（疑似列 `id`／`COUNT` 相当）は本関数の
/// 到達範囲としては残すが、外側の対象列に疑似列 `id` を指定する形
/// （`id IN (SELECT ...)`）は本 Issue の実測範囲外（このリポの既存
/// `WherePredicate::Equality` 束縛自体が疑似列 `id` を対象にしていないため。
/// `tests/sql29_subquery.rs` 参照）。それ以外（`VECTOR`・`DATE`・`TIMESTAMP`・
/// `NUMERIC`・`UUID`・`BYTEA`・配列・JSON・式評価の `Float`）は `22000`
/// （実装既定値のスコープ外。`docs/design/sql-subquery.md` 参照）。
fn cell_to_equality_predicate(
    column: &str,
    cell: &Cell,
) -> Result<Option<WherePredicate>, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    match cell {
        Cell::Null => Ok(None),
        Cell::Text(s) => Ok(Some(WherePredicate::Equality {
            column: column.to_string(),
            value: s.clone(),
        })),
        Cell::Integer(n) => Ok(Some(WherePredicate::Equality {
            column: column.to_string(),
            value: n.to_string(),
        })),
        Cell::SignedInteger(n) => Ok(Some(WherePredicate::Equality {
            column: column.to_string(),
            value: n.to_string(),
        })),
        Cell::Bool(b) => Ok(Some(WherePredicate::BoolEquality {
            column: column.to_string(),
            value: *b,
        })),
        Cell::Vector(_)
        | Cell::Float(_)
        | Cell::Date(_)
        | Cell::Timestamp(_)
        | Cell::Array(_)
        | Cell::Bytes(_)
        | Cell::Json(_)
        | Cell::Numeric(_)
        | Cell::Uuid(_) => Err(SqlSurfaceError::invalid_input(
            "unsupported column type for subquery IN target (implementation scope: \
             TEXT / INTEGER / BIGINT / BOOLEAN / id only)",
        )),
    }
}
