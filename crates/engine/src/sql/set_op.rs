//! 集合演算（`UNION`／`UNION ALL`／`INTERSECT`／`EXCEPT`）の束縛・実行本体
//! （SQL-29 (c)・RLS-10 (b)・TASK-213）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` が構造検証した
//! [`crate::sql::allowlist::ValidatedSetOperation`]（構文木は
//! [`crate::sql::allowlist::SetTree`]。葉は単一テーブルの広域取得
//! [`crate::sql::allowlist::ValidatedScan`] に限定する——TASK-212（`JOIN`・複数
//! テーブル実行計画基盤）を前提にしない設計判断。TASK-213 は TASK-212 に後続
//! するが、本実装は各枝を独立した単一テーブル走査として扱うことで基盤の完成を
//! 待たずに導入する）を受け取り、`core.rs::EngineCore` の SQL 実行経路
//! （session-less `execute_sql`・セッション経由 `execute_validated_in_session`）
//! から呼ばれる。
//!
//! 各枝は既存の広域取得経路（[`crate::sql::scan::execute_scan_with_budget`]）を
//! そのまま通す。これにより RLS 暗黙適用（RLS-7 相当）・fail-closed のエラー契約を
//! 第 2 の実行器を作らずに継承する（各枝は呼び出しセッション自身の
//! [`crate::policy::PolicyContext`] で独立に評価されるため、他テナント行が
//! 中間結果・重複除去・`INTERSECT`／`EXCEPT` の判定・件数のいずれにも現れない。
//! RLS-10 (b)）。
//!
//! スコープ外（Issue #929 の対象外事項。詳細は PR 本文参照）:
//! - 枝ごとの累計バイト予算の共有（各枝は独立に
//!   [`crate::arena::MAX_ARENA_TOTAL_BYTES`] 相当の予算を持つ。全枝で共有する
//!   単一の累計予算は今回は実装しない）
//! - Describe（拡張クエリプロトコル）・明示トランザクション内での実行

use std::collections::{HashMap, HashSet};

use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{SetOperator, SetTree, SqlSurfaceError, ValidatedScan};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::udf_call::UdfRegistry;

/// 重複除去を伴う演算（`UNION`／`INTERSECT`／`EXCEPT`）の行キー集合の基数上限
/// （実装既定値。`core::MAX_SEARCH_K` と同値）。超過は `54000`。合成結果の行数
/// 上限も同じ定数を共有する（重複除去後の行数は必ずこの値以下になる）。
const MAX_SET_OP_ROWS: usize = crate::core::MAX_SEARCH_K;

/// 枝が参照するテーブル名を構文木から集める（重複は 1 回だけ収集する）。
/// `core.rs::EngineCore` が単一の `read_txn` 上で全テーブルのスキーマを解決する
/// ために使う（`read_txn_with_schemas`）。
pub(crate) fn collect_branch_tables(tree: &SetTree, out: &mut Vec<String>) {
    match tree {
        SetTree::Branch(scan) => {
            if !out.iter().any(|t| t == &scan.table_name) {
                out.push(scan.table_name.clone());
            }
        }
        SetTree::Op { left, right, .. } => {
            collect_branch_tables(left, out);
            collect_branch_tables(right, out);
        }
    }
}

/// 1 個の評価結果（枝またはノード合成後）。`has_vector` は結果列に `VECTOR` 列を
/// 含むかどうか（重複除去を伴う演算の可否判定に使う。§2.2 参照）。
struct EvalOutcome {
    columns: Vec<ColumnMeta>,
    rows: Vec<ResultRow>,
    has_vector: bool,
}

/// [`ColumnMeta`] 同士の型整合判定（SQL-29 (c) §2.2）。枝の投影は構文検証段
/// （`sql::allowlist::parse_set_branch`）で `Computed` 項目を拒否済みのため、
/// ここで到達するのは `Id`／`Scalar` のみ（`Computed` が現れた場合は防御的に
/// 不一致として扱う）。
fn columns_compatible(a: &ColumnMeta, b: &ColumnMeta) -> bool {
    match (a, b) {
        (ColumnMeta::Id, ColumnMeta::Id) => true,
        (ColumnMeta::Scalar { ty: ta, .. }, ColumnMeta::Scalar { ty: tb, .. }) => ta == tb,
        _ => false,
    }
}

fn columns_have_vector(columns: &[ColumnMeta]) -> bool {
    columns.iter().any(|c| {
        matches!(
            c,
            ColumnMeta::Scalar {
                ty: ColumnType::Vector(_),
                ..
            }
        )
    })
}

/// `f64` の正準化（`-0.0` を `+0.0` へ、NaN を単一表現へ）。SQL-25 (c) の同値判定
/// 規約と同一（#1098 が `sql::distinct` へ `canon_f64` を追加した場合はそちらへ
/// 統合する。現時点では未マージのためローカル定義とする）。
fn canon_f64_bits(f: f64) -> u64 {
    let normalized = if f == 0.0 { 0.0 } else { f };
    if normalized.is_nan() {
        f64::NAN.to_bits()
    } else {
        normalized.to_bits()
    }
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(bytes.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("set operation row key exceeds length limit")
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// 行の正準バイト列表現（重複除去・`INTERSECT`／`EXCEPT` の同値判定に使う行
/// キー）。セルごとに「型タグ＋長さ接頭辞（`u32` BE）＋ペイロード」を連結する
/// （SQL-29 (c) §2.3）。`VECTOR` 列は §2.2 の型検証で重複除去を伴う演算からは
/// 排除済みのため、到達した場合は防御的に拒否する（fail-closed）。
fn row_key(row: &ResultRow) -> Result<Vec<u8>, SqlSurfaceError> {
    let mut out = Vec::new();
    for cell in &row.cells {
        match cell {
            Cell::Null => out.push(0),
            Cell::Integer(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Cell::Text(s) => {
                out.push(2);
                push_len_prefixed(&mut out, s.as_bytes())?;
            }
            Cell::Float(f) => {
                out.push(3);
                out.extend_from_slice(&canon_f64_bits(*f).to_be_bytes());
            }
            Cell::Bool(b) => {
                out.push(4);
                out.push(u8::from(*b));
            }
            Cell::SignedInteger(v) => {
                out.push(5);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Cell::Date(d) => {
                out.push(6);
                out.extend_from_slice(&d.to_be_bytes());
            }
            Cell::Timestamp(t) => {
                out.push(7);
                out.extend_from_slice(&t.to_be_bytes());
            }
            Cell::Bytes(b) => {
                out.push(8);
                push_len_prefixed(&mut out, b)?;
            }
            Cell::Json(s) => {
                out.push(9);
                push_len_prefixed(&mut out, s.as_bytes())?;
            }
            Cell::Numeric(d) => {
                out.push(10);
                push_len_prefixed(&mut out, d.to_string().as_bytes())?;
            }
            Cell::Uuid(u) => {
                out.push(11);
                out.extend_from_slice(u.as_bytes());
            }
            Cell::Array(arr) => {
                out.push(12);
                match arr {
                    crate::row_codec::ArrayValue::Text(items) => {
                        out.push(0);
                        let n = u32::try_from(items.len()).map_err(|_| {
                            SqlSurfaceError::payload_too_large(
                                "set operation row key exceeds length limit",
                            )
                        })?;
                        out.extend_from_slice(&n.to_be_bytes());
                        for item in items {
                            push_len_prefixed(&mut out, item.as_bytes())?;
                        }
                    }
                    crate::row_codec::ArrayValue::Bool(items) => {
                        out.push(1);
                        let n = u32::try_from(items.len()).map_err(|_| {
                            SqlSurfaceError::payload_too_large(
                                "set operation row key exceeds length limit",
                            )
                        })?;
                        out.extend_from_slice(&n.to_be_bytes());
                        for b in items {
                            out.push(u8::from(*b));
                        }
                    }
                }
            }
            // §2.2 の型検証（重複除去を伴う演算からの `VECTOR` 列排除）で本来
            // 到達しない。破損状態を防御的に拒否する（fail-closed。XX000）。
            Cell::Vector(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "VECTOR column reached set operation row key encoding".to_string(),
                });
            }
        }
    }
    Ok(out)
}

/// 単一の枝（`SELECT ... FROM <table> [WHERE ...]`）を束縛・実行する。可視かつ
/// `WHERE` 一致行が `core::MAX_SEARCH_K` を超える場合は `54000`（超過検出のため
/// `MAX_SEARCH_K + 1` を上限として走査する）。
fn eval_branch(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    validated: &ValidatedScan,
    udfs: &UdfRegistry,
) -> Result<EvalOutcome, SqlSurfaceError> {
    let mut bound = crate::sql::parser::bind_scan(validated, schema, udfs)?;
    // §2.3: 可視行数の上限判定は「可視行数だけに依存」させる（全体 `LIMIT` の
    // 有無で成否が変わらない単純な規則）。`bound.limit`（`pub(crate)`）を
    // `MAX_SEARCH_K + 1` へ差し替えて超過を検出できるようにする。
    bound.limit = MAX_SET_OP_ROWS + 1;
    let result = crate::sql::scan::execute_scan(read_txn, ctx, schema, &bound)?;
    if result.rows.len() > MAX_SET_OP_ROWS {
        return Err(SqlSurfaceError::payload_too_large(
            "set operation branch exceeds the visible row limit",
        ));
    }
    let has_vector = columns_have_vector(&result.columns);
    Ok(EvalOutcome {
        columns: result.columns,
        rows: result.rows,
        has_vector,
    })
}

/// 構文木を後行順（postorder）で評価する。各 `Op` ノードで型整合
/// （列数・列型。§2.2）を検証してから合成する。結果列名・メタデータは常に左の
/// 子（左端の枝）に揃える。
fn eval_tree(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
) -> Result<EvalOutcome, SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) => {
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            eval_branch(read_txn, ctx, schema, validated, udfs)
        }
        SetTree::Op { op, left, right } => {
            let l = eval_tree(read_txn, ctx, schemas, left, udfs)?;
            let r = eval_tree(read_txn, ctx, schemas, right, udfs)?;

            if l.columns.len() != r.columns.len() {
                return Err(SqlSurfaceError::datatype_mismatch(format!(
                    "column count mismatch: {} vs {}",
                    l.columns.len(),
                    r.columns.len()
                )));
            }
            for (i, (a, b)) in l.columns.iter().zip(r.columns.iter()).enumerate() {
                if !columns_compatible(a, b) {
                    return Err(SqlSurfaceError::datatype_mismatch(format!(
                        "column {i} type mismatch between set operation branches"
                    )));
                }
            }

            let dedup_needed = !matches!(op, SetOperator::UnionAll);
            if dedup_needed && (l.has_vector || r.has_vector) {
                return Err(SqlSurfaceError::invalid_input(
                    "VECTOR columns cannot be combined with UNION/INTERSECT/EXCEPT (use UNION ALL)",
                ));
            }

            let rows = match op {
                SetOperator::UnionAll => {
                    let mut rows = l.rows;
                    rows.extend(r.rows);
                    if rows.len() > MAX_SET_OP_ROWS {
                        return Err(SqlSurfaceError::payload_too_large(
                            "set operation result exceeds the row limit",
                        ));
                    }
                    rows
                }
                SetOperator::Union => {
                    let mut seen: HashSet<Vec<u8>> = HashSet::new();
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter().chain(r.rows) {
                        let key = row_key(&row)?;
                        if seen.contains(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        seen.insert(key);
                        rows.push(row);
                    }
                    rows
                }
                SetOperator::Intersect | SetOperator::Except => {
                    let mut right_keys: HashSet<Vec<u8>> = HashSet::new();
                    for row in &r.rows {
                        right_keys.insert(row_key(row)?);
                    }
                    let keep_if_present = matches!(op, SetOperator::Intersect);
                    let mut seen: HashSet<Vec<u8>> = HashSet::new();
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter() {
                        let key = row_key(&row)?;
                        let present = right_keys.contains(&key);
                        if present != keep_if_present || seen.contains(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        seen.insert(key);
                        rows.push(row);
                    }
                    rows
                }
            };

            Ok(EvalOutcome {
                columns: l.columns,
                rows,
                has_vector: l.has_vector,
            })
        }
    }
}

/// [`crate::sql::allowlist::ValidatedSetOperation`] を実行する（`core.rs::
/// EngineCore::execute_validated_in_session` の `Statement::SetOperation`
/// アーム・session-less `execute_sql` の同アームから呼ばれる）。`schemas` は
/// 呼び出し元が単一の `read_txn`（同一スナップショット）上で解決済みのものを
/// 渡す（`EngineCore::read_txn_with_schemas`）。
pub(crate) fn execute(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    limit: Option<u32>,
) -> Result<QueryResult, SqlSurfaceError> {
    let outcome = eval_tree(read_txn, ctx, schemas, tree, udfs)?;
    // §2.4: 上限判定（`MAX_SET_OP_ROWS`）は全体 `LIMIT` による切り詰めより前に
    // 完結している（`eval_tree` が既に検証済み）。ここでは範囲検証のみ行う。
    let rows = match limit {
        Some(n) => {
            let n = crate::sql::parser::validate_search_limit(n)?;
            outcome.rows.into_iter().take(n).collect()
        }
        None => outcome.rows,
    };
    Ok(QueryResult {
        columns: outcome.columns,
        rows,
    })
}

/// Describe（拡張クエリプロトコル）向けに、検索本体を実行せず結果列メタデータ
/// だけを導出する。`bind_scan_with_dummy_flags` で枝を束縛し、[`execute`] と
/// 同じ型整合検証（§2.2）を行う（行走査は一切行わない）。
pub(crate) fn describe_columns(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    dummy_equality_flags: &[bool],
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    Ok(describe_tree(schemas, tree, udfs, dummy_equality_flags)?.0)
}

fn describe_tree(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    dummy_equality_flags: &[bool],
) -> Result<(Vec<ColumnMeta>, bool), SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) => {
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                validated,
                schema,
                udfs,
                dummy_equality_flags,
            )?;
            let columns = crate::sql::describe::projected_columns(bound.projection(), schema);
            let has_vector = columns_have_vector(&columns);
            Ok((columns, has_vector))
        }
        SetTree::Op { op, left, right } => {
            let (l_columns, l_has_vector) =
                describe_tree(schemas, left, udfs, dummy_equality_flags)?;
            let (r_columns, r_has_vector) =
                describe_tree(schemas, right, udfs, dummy_equality_flags)?;
            if l_columns.len() != r_columns.len() {
                return Err(SqlSurfaceError::datatype_mismatch(format!(
                    "column count mismatch: {} vs {}",
                    l_columns.len(),
                    r_columns.len()
                )));
            }
            for (i, (a, b)) in l_columns.iter().zip(r_columns.iter()).enumerate() {
                if !columns_compatible(a, b) {
                    return Err(SqlSurfaceError::datatype_mismatch(format!(
                        "column {i} type mismatch between set operation branches"
                    )));
                }
            }
            let dedup_needed = !matches!(op, SetOperator::UnionAll);
            if dedup_needed && (l_has_vector || r_has_vector) {
                return Err(SqlSurfaceError::invalid_input(
                    "VECTOR columns cannot be combined with UNION/INTERSECT/EXCEPT (use UNION ALL)",
                ));
            }
            Ok((l_columns, l_has_vector))
        }
    }
}
