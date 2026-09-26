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
//! - `IN (SELECT ...)` が内側の各行を `WherePredicate::Equality`／
//!   `BoolEquality` 葉へ展開して `Or` に束ねる件数は、構文解析段の
//!   `MAX_WHERE_LEAVES`（通常の `WHERE` 述語 1 個を 1 葉と数える）とは独立の
//!   経路で生成されるため、[`MAX_SUBQUERY_IN_LEAVES`]（文全体で共有する
//!   `&mut usize` 予算）で総生成数を頭打ちにする（PR #1103 codex-review P1
//!   指摘対応。内側最大可視行数 [`crate::core::MAX_SEARCH_K`] ×
//!   [`MAX_SUBQUERY_EXECUTIONS`] の組合せだけでは、既存の `WHERE` 述語数上限
//!   より 2 桁以上大きい評価コストを 1 文から発生させられた）。

use super::allowlist::{Statement, TableLookup, WherePredicate};
use super::exec::Cell;
use super::lexer::Token;
use super::udf_call::UdfRegistry;
use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;

/// 1 文（トップレベル実行 1 回）あたりに実行できる内側サブクエリの総数
/// （実装既定値。Issue #927・TASK-213。ネスト深さ上限
/// [`super::allowlist::MAX_SUBQUERY_DEPTH`] と組み合わせて評価コストを
/// 有界にする）。
pub(crate) const MAX_SUBQUERY_EXECUTIONS: usize = 16;

/// 1 文（トップレベル実行 1 回。ネストしたサブクエリを含む）あたりに
/// `IN (SELECT ...)` の展開で生成できる `WherePredicate` 葉（`Equality`／
/// `BoolEquality`）の総数（実装既定値。PR #1103 codex-review P1 指摘対応）。
/// 通常の `WHERE` 述語の葉数上限
/// （`crate::declarative_filter::MAX_METADATA_FILTERS`）と同じ規模に揃える
/// ことで、サブクエリ経由の展開が通常の `WHERE` 句より大きな評価コストを
/// 発生させないようにする。
pub(crate) const MAX_SUBQUERY_IN_LEAVES: usize = crate::declarative_filter::MAX_METADATA_FILTERS;

/// `where_predicates`（トップレベルの述語列。`WherePredicate::Or` の分岐も
/// 再帰的に辿る）に含まれる `InSubquery`／`Exists` をすべて解決し、具体的な
/// `WherePredicate` へ書き換えた新しい述語列を返す。`core.rs` の
/// `Statement::Scan`／`Statement::Aggregate` 実行アームが、束縛
/// （`bind_scan`／`bind_aggregate`）の直前に呼ぶ。
///
/// `outer_schema` は `predicates` が参照する列（この呼び出しにとっての
/// 「外側」＝これから束縛される文自身のテーブル）のスキーマ。`IN (SELECT
/// ...)` の対象列の存在・型検証（[`validate_in_target_column`]）に使う
/// （PR #1103 codex-review P1 指摘対応: 内側の結果行数に関わらず必ず検証する。
/// ネストしたサブクエリを解決する再帰呼び出し〔[`execute_inner_scan`]〕では、
/// その内側クエリ自身のスキーマを渡す）。
///
/// `budget` は呼び出し階層全体（ネストしたサブクエリを含む）で共有する残り
/// 実行回数。呼び出し元は [`MAX_SUBQUERY_EXECUTIONS`] で初期化する。
/// `in_leaf_budget` も同様に呼び出し階層全体で共有する、`IN` 展開で生成
/// できる残り葉数。呼び出し元は [`MAX_SUBQUERY_IN_LEAVES`] で初期化する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_where_predicates(
    predicates: Vec<WherePredicate>,
    outer_schema: &TableSchema,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_leaf_budget: &mut usize,
) -> Result<Vec<WherePredicate>, crate::sql::allowlist::SqlSurfaceError> {
    let mut out = Vec::with_capacity(predicates.len());
    for predicate in predicates {
        match predicate {
            WherePredicate::Or(branches) => {
                let mut resolved_branches = Vec::with_capacity(branches.len());
                for branch in branches {
                    resolved_branches.push(resolve_where_predicates(
                        branch,
                        outer_schema,
                        read_txn,
                        ctx,
                        lookup,
                        udfs,
                        budget,
                        in_leaf_budget,
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
                    outer_schema,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    budget,
                    in_leaf_budget,
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
                    in_leaf_budget,
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
///
/// 自身の WHERE に含まれるさらに深いサブクエリを解決する際の「外側スキーマ」
/// （[`resolve_where_predicates`] の `outer_schema`）は、この内側クエリ自身の
/// テーブルのスキーマになる（相対的に見て、そのネスト位置での「外側」は
/// このクエリ自身）。そのため `inner_schema` の取得を `bind_scan` 呼び出しより
/// 前へ移し、ネストした `resolve_where_predicates` 呼び出しへ渡す
/// （PR #1103 codex-review P1 指摘対応）。
#[allow(clippy::too_many_arguments)]
fn execute_inner_scan(
    inner_tokens: &[Token],
    depth: usize,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_leaf_budget: &mut usize,
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

    let inner_schema = crate::catalog::get_table_schema_in_txn(read_txn, &validated.table_name)
        .map_err(|e| match e {
            crate::catalog::CatalogError::TableNotFound(name) => {
                SqlSurfaceError::undefined_table(name)
            }
            other => crate::catalog::table_lookup_error(other),
        })?;

    // 自身の WHERE に含まれるさらに深いサブクエリを、束縛（`bind_scan`）の前に
    // 解決する（深さ優先。`depth` は構文解析段で `MAX_SUBQUERY_DEPTH` 検査
    // 済みのため、ここでは budget のみ検査すれば足りる）。`outer_schema` には
    // このクエリ自身の `inner_schema` を渡す（このネスト位置での「外側」）。
    validated.where_predicates = resolve_where_predicates(
        validated.where_predicates,
        &inner_schema,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_leaf_budget,
    )?;

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

/// `<column> IN (SELECT ...)` の対象列 `column` を `outer_schema`（この
/// サブクエリを含む文自身のテーブルのスキーマ）に対して検証する。存在しない
/// 列は `22000`（`unknown column`。通常の `WHERE` 等価述語束縛
/// ［`crate::declarative_filter::DeclarativeFilter::bind`］と同じ文言・
/// `wire_code`）、存在しても [`WherePredicate::Equality`]／
/// [`WherePredicate::BoolEquality`] のいずれも束縛できない型（`TEXT`／
/// `ENUM`／`BOOLEAN` 以外）は同じく `22000` で拒否する。
///
/// この検証は内側サブクエリの結果行数（0 行・NULL のみを含む）に一切
/// 依存しない（PR #1103 codex-review P1 指摘対応: 従来は内側の結果行から
/// 変換された葉が実際に束縛される時点でしか列名・型検証が働かず、内側が
/// 0 行／NULL のみの場合は空の `Or`〔常に偽〕へ静かに置き換わり、存在しない
/// 列・非対応型の列を指定しても列名・型検証を回避したまま「空結果で成功」
/// してしまっていた）。
fn validate_in_target_column<'a>(
    column: &str,
    outer_schema: &'a TableSchema,
) -> Result<&'a ColumnType, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    let column_def = outer_schema
        .columns
        .iter()
        .find(|c| c.name == column)
        .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {column}")))?;
    match &column_def.ty {
        ColumnType::Text | ColumnType::Enum(_) | ColumnType::Boolean => Ok(&column_def.ty),
        _ => Err(SqlSurfaceError::invalid_input(format!(
            "column {column:?} is not a TEXT/ENUM/BOOLEAN column (subquery IN target)"
        ))),
    }
}

/// `<column> IN (SELECT ...)` を解決する。内側は投影列がちょうど 1 列である
/// ことを要求し（`22000`）、[`validate_in_target_column`] で対象列
/// `column`（外側スキーマ）を検証した上で、各行のセルを
/// [`cell_to_equality_predicate`] で `<column> = <値>` 相当の葉へ変換し
/// `WherePredicate::Or` として束ねる（0 行なら空の `Or` ＝常に偽）。
///
/// `in_leaf_budget` は文全体で共有する残り葉数予算（[`MAX_SUBQUERY_IN_LEAVES`]
/// 参照）。生成する葉ごとに 1 消費し、枯渇したら `54000` で拒否する
/// （PR #1103 codex-review P1 指摘対応: 内側最大可視行数×内側実行回数上限の
/// 組合せだけでは、通常の `WHERE` 述語数上限より大きな評価コストを 1 文から
/// 発生させられた）。
#[allow(clippy::too_many_arguments)]
fn resolve_in_subquery(
    column: &str,
    inner_tokens: &[Token],
    depth: usize,
    outer_schema: &TableSchema,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_leaf_budget: &mut usize,
) -> Result<WherePredicate, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    let result = execute_inner_scan(
        inner_tokens,
        depth,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_leaf_budget,
    )?;
    if result.columns.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used with IN must select exactly one column",
        ));
    }
    // 内側の結果行数（0 行を含む）に関わらず、対象列の存在・型を必ず検証する。
    validate_in_target_column(column, outer_schema)?;

    let mut branches = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let cell = row.cells.first().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery row missing projected cell".to_string(),
        })?;
        if let Some(leaf) = cell_to_equality_predicate(column, cell)? {
            *in_leaf_budget = in_leaf_budget.checked_sub(1).ok_or_else(|| {
                SqlSurfaceError::payload_too_large(format!(
                    "subquery IN expansion leaf count exceeds limit {MAX_SUBQUERY_IN_LEAVES}"
                ))
            })?;
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
    in_leaf_budget: &mut usize,
) -> Result<bool, crate::sql::allowlist::SqlSurfaceError> {
    let result = execute_inner_scan(
        inner_tokens,
        depth,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_leaf_budget,
    )?;
    Ok(!result.rows.is_empty())
}

/// 内側の投影セル 1 件を `<column> = <値>` 相当の `WherePredicate` 葉へ変換する。
/// `NULL` は `Ok(None)`（呼び出し元が集合から除外する）。対応するのは `TEXT`
/// （`Cell::Text`）・`BOOLEAN`（`Cell::Bool`）のみ。
///
/// `INTEGER`/`BIGINT` 列（`Cell::SignedInteger`）は対象外とする（レビュー
/// 指摘対応。Issue #927 push 前 Review）。`WherePredicate::Equality` は
/// `TEXT`／`ENUM` 列専用で `INTEGER`/`BIGINT` 列を「TEXT 列でない」として
/// 拒否する契約であり（`sql::parser::bind_where_predicates_recursive`）、
/// `INTEGER`/`BIGINT` 列の等価比較自体がこのリポでは未実装（`レーン A`。
/// `sql::udf_call::bind_expr_in` が `INTEGER`/`BIGINT` 列参照を式評価から
/// 一律拒否する契約。`sql::check_constraint` モジュールコメント参照）。
/// 通常の `<col> = <整数リテラル>` も同じ理由で現状は受理されないため、
/// 本関数だけが先取りして対応する処置は取らず、既存の実装既定値の範囲外
/// （`22000`）として明示的に拒否する（`docs/design/sql-subquery.md` 参照）。
///
/// `Cell::Integer`（疑似列 `id`／`COUNT` 相当）も同じ理由で本関数の到達範囲
/// としては残すが対象外（このリポの既存 `WherePredicate::Equality` 束縛
/// 自体が疑似列 `id` を対象にしていないため。`tests/sql29_subquery.rs`
/// 参照）。それ以外（`VECTOR`・`DATE`・`TIMESTAMP`・`NUMERIC`・`UUID`・
/// `BYTEA`・配列・JSON・式評価の `Float`）も同様に `22000`（実装既定値の
/// スコープ外。`docs/design/sql-subquery.md` 参照）。
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
        Cell::Bool(b) => Ok(Some(WherePredicate::BoolEquality {
            column: column.to_string(),
            value: *b,
        })),
        Cell::SignedInteger(_)
        | Cell::Vector(_)
        | Cell::Float(_)
        | Cell::Date(_)
        | Cell::Timestamp(_)
        | Cell::Array(_)
        | Cell::Bytes(_)
        | Cell::Json(_)
        | Cell::Numeric(_)
        | Cell::Uuid(_) => Err(SqlSurfaceError::invalid_input(
            "unsupported column type for subquery IN target (implementation scope: \
             TEXT / BOOLEAN / id only; INTEGER/BIGINT equality is not yet implemented)",
        )),
    }
}
