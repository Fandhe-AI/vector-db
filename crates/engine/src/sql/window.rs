//! ウィンドウ関数（`ROW_NUMBER`/`RANK`/`DENSE_RANK` と `OVER` 付き集計。SQL-30・
//! TASK-214、Issue #930）の実行本体。広域取得（[`crate::sql::scan`]）の投影に
//! `<func>(...) OVER (PARTITION BY ... ORDER BY ...)` を加えたもので、
//! `sql::scan::execute_scan_with_budget` から `bound.windows()` が空でない場合に
//! dispatch される（`sql::scan` モジュール本体への変更を dispatch 1 行に留める
//! ことで、他 PR との衝突を最小化する設計判断）。
//!
//! 責務境界: [`crate::sql::parser::bind_scan_with_dummy_flags`] が返す
//! [`crate::sql::parser::BoundScan`]（`windows` が非空）を受け取り、2 段階で
//! 実行する。
//!
//! 1. **materialize 段**（[`materialize_rows`]）: 対象テーブルを 1 回、`LIMIT` に
//!    よる早期終了なしで走査し、可視かつ `WHERE` を満たす行**全体**について
//!    ウィンドウ項目ごとの `PARTITION BY`／`ORDER BY` キー・集計引数の値を集める。
//!    RLS 適用順序（ヘッダのみで可視性判定 → TABLE-12 のキー/ヘッダ tenant 整合
//!    検査 → 必要範囲のみのデコード → SCALAR 段〔`WHERE`〕→ 可視性の再適用）は
//!    `sql::scan`／`sql::aggregate` の走査ループと同一の規約を踏襲する
//!    （`.claude/rules/security.md`「テナント境界（P0）」）。不可視行・`WHERE`
//!    不一致行はウィンドウ計算の母集合に一切現れない（RLS-7・RLS-8 のウィンドウ
//!    版）。
//! 2. **投影段**（[`build_result`]）: ウィンドウ以外の投影・`LIMIT`／`OFFSET` の
//!    適用は、同じ `WHERE`・`limit`・`offset` を持つ `windows` 空の [`BoundScan`]
//!    複製を [`crate::sql::scan::execute_scan`] へそのまま渡すことで、既存の
//!    実行器（RLS 適用順序・早期終了・結果バイト予算どれも同一）をそのまま再利用
//!    する（第 2 の実行器を作らない）。同じ `read_txn`（同一スナップショット）で
//!    呼ぶため、物理走査順は materialize 段と一致する。ウィンドウ列は
//!    [`crate::sql::allowlist::WindowSelectItem::position`] に基づいて元の
//!    SELECT リスト位置へ差し込む。
//!
//! 集計の数値計算（`SUM`/`AVG`/`MIN`/`MAX` の NULL 契約・桁あふれ検査・NUMERIC の
//! scale 保持）は [`crate::sql::aggregate::Accumulator`] を再利用し、集計 SELECT
//! （`sql::aggregate`）と同一の規約を共有する（第 2 の集計実装を作らない）。
//! ウィンドウ項目の集計引数は構文段（`sql::allowlist::Parser::parse_window_item`）で
//! 裸の列参照・`*` のみに制限しているため（複合式は対象外）、束縛結果
//! （[`AggregateInput`]）に `ScalarExpr` は現れない——本モジュールはこの前提のもとで
//! のみ `Accumulator::observe` を呼ぶ。`observe` は借用形（`scanned: &[Option<
//! row_codec::ScalarRef>]`・`vector: &RowVector`）を要求するため、materialize 段で
//! owned 化した値を、観測のたびにその場で最小限の借用形へ組み立て直す
//! （[`observe_window_row`]）。
//!
//! 対象外（`42601`／`54000` で fail-closed に拒否。allowlist・parser 側で既に拒否
//! 済みの形も含む）: フレーム句（`ROWS`/`RANGE`/`GROUPS`）・`NULLS FIRST/LAST`・
//! 名前付きウィンドウ・`FILTER (...)`・関数内 `DISTINCT`・複合式の引数・スカラー
//! `ORDER BY`／`USING PLAN`／ベクトル検索との併用・`GROUP BY`／集計 SELECT との併用。

use crate::catalog::{self, ColumnType, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::aggregate::{self, Accumulator, DecodeTier, RowVector};
use crate::sql::allowlist::{SqlSurfaceError, WindowFunc};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::expr_program::{ExprProgram, StackValue};
use crate::sql::parser::{
    AggregateInput, BoundScan, BoundWindowItem, ProjectedColumn, WindowKeyKind, WindowKeyRef,
};
use crate::sql::udf_call::{self, ExprValue};
use crate::storage::{self, StorageError};
use redb::ReadableTable;
use std::collections::HashMap;

/// パーティション数の上限（受入基準 2）。`sql::group_by` の `GROUP BY` グループ数
/// 上限（SQL-14）と同値の実装既定値を流用する。
const MAX_WINDOW_PARTITIONS: usize = crate::sql::group_by::MAX_GROUPS;

/// クエリ全体でウィンドウ計算のために materialize してよい行数の合計上限
/// （受入基準 2。無制限確保を避ける。`.claude/rules/security.md`「不安全な設計」
/// 対応）。実装既定値。
const MAX_WINDOW_ROWS: usize = 1_000_000;

/// 1 パーティションが保持してよい行数の上限（受入基準 2）。`MAX_WINDOW_ROWS` と
/// 同値の実装既定値。
const MAX_WINDOW_FRAME_ROWS: usize = 1_000_000;

/// キー値・引数値・行ごとの固定オーバーヘッドの累計バイト数上限（受入基準 2）。
const MAX_WINDOW_STATE_BYTES: usize = 64 * 1024 * 1024;

/// 型不整合・実装バグの検出用（untrusted 入力起因ではないため `wire_code` は
/// `XX000`。[`crate::sql::aggregate::accumulator_bug`] と同方針）。
fn window_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("window scan tier/value mismatch: {detail}"),
    }
}

fn storage_internal(e: impl Into<StorageError>) -> SqlSurfaceError {
    let _: StorageError = e.into();
    SqlSurfaceError::Internal {
        detail: "window scan row scan failed".to_string(),
    }
}

/// `current` に `add` を加えた累計が `cap` を超えないことを確保前に検証する
/// （`sql::scan` の同名ヘルパーと同方針）。
fn try_accumulate_state_budget(
    current: usize,
    add: usize,
    cap: usize,
) -> Result<usize, SqlSurfaceError> {
    let next = current
        .checked_add(add)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("window state size overflowed"))?;
    if next > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window state exceeds the allowed total size",
        ));
    }
    Ok(next)
}

/// 新規パーティション追加前に [`MAX_WINDOW_PARTITIONS`] 上限を検査する
/// （受入基準 2）。`cap` を引数化しているのは、実データを大量投入せずに
/// 上限判定ロジックそのものを単体テストできるようにするため（本体は常に
/// [`MAX_WINDOW_PARTITIONS`] を渡す。PR #930 最終レビュー指摘 2）。
fn check_new_partition_capacity(
    existing_partition_count: usize,
    cap: usize,
) -> Result<(), SqlSurfaceError> {
    if existing_partition_count >= cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window PARTITION BY produces too many partitions",
        ));
    }
    Ok(())
}

/// 1 パーティションの行数が [`MAX_WINDOW_FRAME_ROWS`] 上限を超えないことを
/// 検査する（受入基準 2。`cap` 引数化の理由は
/// [`check_new_partition_capacity`] と同じ）。
fn check_partition_row_count(count: usize, cap: usize) -> Result<(), SqlSurfaceError> {
    if count > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window partition exceeds the allowed row count",
        ));
    }
    Ok(())
}

/// materialize 段で集計済みの総行数が [`MAX_WINDOW_ROWS`] 上限を超えないことを
/// 検査する（受入基準 2。`cap` 引数化の理由は [`check_new_partition_capacity`]
/// と同じ）。
fn check_total_row_count(count: usize, cap: usize) -> Result<(), SqlSurfaceError> {
    if count > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window scan materializes too many rows",
        ));
    }
    Ok(())
}

/// ウィンドウの `PARTITION BY`／`ORDER BY` キー 1 つの値（[`WindowKeyRef`] の
/// 実行時表現。NULL は `Option::None` で表す）。
#[derive(Debug, Clone)]
enum WindowKeyValue {
    Id(u64),
    Text(String),
    Integer(i32),
    BigInt(i64),
    Real(f32),
    Double(f64),
    Boolean(bool),
    Date(i32),
    Timestamp(i64),
    Numeric(crate::numeric::Decimal),
    Uuid(crate::uuid::Uuid),
}

/// ウィンドウ集計項目の引数値（実行時表現。SQL-30・TASK-214）。構文段が引数を
/// 裸の列参照・`*` のみに制限しているため、束縛結果は必ずこの網羅で尽くされる
/// （複合式は現れない）。
#[derive(Debug, Clone)]
enum WindowInputValue {
    /// 順位関数（`ROW_NUMBER`/`RANK`/`DENSE_RANK`）。引数を持たない。
    None,
    /// `COUNT(*)`・`SUM`/`AVG`/`MIN`/`MAX(id)` 以外の `COUNT(id)`。
    AllVisibleOrId,
    /// `VECTOR` 列の裸参照（`COUNT` 限定）。embedding 自体は不要で dim のみ。
    VectorPresence(u32),
    Text(Option<String>),
    Integer(Option<i32>),
    BigInt(Option<i64>),
    Real(Option<f32>),
    Double(Option<f64>),
    Date(Option<i32>),
    Timestamp(Option<i64>),
    Numeric(Option<crate::numeric::Decimal>),
    /// `BOOLEAN`／`ARRAY`／`BYTEA`／`JSON`／`JSONB`／`ENUM`／`UUID` 列の裸参照
    /// （いずれも `COUNT` 限定。`Accumulator::observe` はこれらの型で値の中身を
    /// 見ず非 NULL の有無だけを見るため、実際の値は保持しない）。
    PresenceOnly(bool),
}

/// 可視かつ WHERE を満たす 1 行の materialize 結果（SQL-30・TASK-214）。
/// `partition_keys`／`order_keys`／`agg_values` は `bound.windows()` と同じ順序の
/// `Vec`（外側添字がウィンドウ項目の添字）。
struct MaterializedRow {
    /// 走査順（`(tenant_id, id)` 物理順）の通し番号。同順位のタイブレークに使う。
    seq: usize,
    id: u64,
    partition_keys: Vec<Vec<Option<WindowKeyValue>>>,
    order_keys: Vec<Vec<Option<WindowKeyValue>>>,
    agg_values: Vec<WindowInputValue>,
}

/// [`BoundScan`] を実行する（`windows` が非空。`sql::scan::execute_scan_with_budget`
/// の dispatch から呼ばれる）。`max_result_bytes` は投影段（[`crate::sql::scan::
/// execute_scan`] への委譲）の結果バイト予算（`sql::scan` と同じ契約）で、
/// ウィンドウ状態の予算（[`MAX_WINDOW_STATE_BYTES`] 等）とは独立に計上する。
pub(crate) fn execute_window_scan(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
    max_result_bytes: usize,
) -> Result<QueryResult, SqlSurfaceError> {
    let materialized = materialize_rows(read_txn, ctx, schema, bound)?;

    // ウィンドウ項目ごとに独立してパーティション分割・安定ソート・peer 評価を行い、
    // `id -> Cell` の写像を作る。
    let mut window_values: Vec<HashMap<u64, Cell>> = Vec::with_capacity(bound.windows().len());
    for (item_index, item) in bound.windows().iter().enumerate() {
        window_values.push(evaluate_window_item(item, item_index, &materialized)?);
    }

    build_result(
        read_txn,
        ctx,
        schema,
        bound,
        &window_values,
        max_result_bytes,
    )
}

/// 対象テーブルを 1 回、`LIMIT` による早期終了なしで走査し、可視かつ `WHERE` を
/// 満たす行**全体**について [`MaterializedRow`] を集める（§モジュールドキュメント
/// 「materialize 段」参照）。
fn materialize_rows(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
) -> Result<Vec<MaterializedRow>, SqlSurfaceError> {
    let expected_dim = schema.vector_dim();
    let (tier, scalar_mask) = decode_tier_for_window(schema, bound);
    let expr_filter_programs: Vec<ExprProgram> = bound
        .expr_filters()
        .iter()
        .map(ExprProgram::compile)
        .collect();

    let row_table_name = catalog::user_rows_table_name(bound.table());
    let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => {
            return Err(SqlSurfaceError::Internal {
                detail: format!(
                    "window scan row scan failed: {}",
                    catalog::map_row_table_error(e)
                ),
            })
        }
    };

    let mut embedding_scratch: Vec<f32> = Vec::new();
    let mut expr_scratch: Vec<StackValue> = Vec::new();
    let mut materialized: Vec<MaterializedRow> = Vec::new();
    let mut seq: usize = 0;
    let mut state_bytes: usize = 0;
    let mut partition_counts: Vec<HashMap<Vec<u8>, usize>> =
        vec![HashMap::new(); bound.windows().len()];

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // RLS 段（無条件・デコード前）。`sql::scan`・`sql::aggregate` の走査
            // ループと同一の順序（security.md P0「テナント境界」）。
            let (tenant_id, visibility, offset) =
                storage::decode_row_header(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            storage::verify_row_key_tenant(key_tenant, tenant_id).map_err(storage_internal)?;

            let (dim, metadata): (u32, &[u8]) = match tier {
                DecodeTier::Fast | DecodeTier::DimAndScalar => {
                    storage::decode_row_dim_and_metadata_borrowed(buf).map_err(storage_internal)?
                }
                DecodeTier::Embedding => {
                    storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                        .map_err(storage_internal)?
                }
            };
            if let Some(expected) = expected_dim {
                if dim != 0 && dim != expected {
                    return Err(SqlSurfaceError::Internal {
                        detail: "window scan row scan failed: embedding dimension mismatch"
                            .to_string(),
                    });
                }
            }

            let scanned: Vec<Option<row_codec::ScalarRef<'_>>> = match tier {
                DecodeTier::Fast => {
                    row_codec::validate_scalar_columns(schema, metadata)?;
                    Vec::new()
                }
                DecodeTier::DimAndScalar | DecodeTier::Embedding => {
                    row_codec::scan_scalar_columns_masked(schema, metadata, Some(&scalar_mask))?
                }
            };

            if !declarative_filter::matches_all(bound.metadata_filters(), &scanned) {
                continue;
            }
            for (expr, program) in bound.expr_filters().iter().zip(&expr_filter_programs) {
                let references_embedding = udf_call::references_embedding(expr);
                if references_embedding && dim == 0 {
                    continue 'rows;
                }
                let embedding: &[f32] = if references_embedding {
                    match tier {
                        DecodeTier::Embedding => embedding_scratch.as_slice(),
                        DecodeTier::Fast | DecodeTier::DimAndScalar => {
                            return Err(window_bug(
                                "WHERE expression references the VECTOR column but tier did not decode it",
                            ))
                        }
                    }
                } else {
                    &[]
                };
                match program.eval(id, embedding, &mut expr_scratch)? {
                    ExprValue::Bool(true) => {}
                    ExprValue::Bool(false) => continue 'rows,
                    _ => {
                        return Err(SqlSurfaceError::invalid_input(
                            "WHERE expression did not evaluate to a boolean",
                        ))
                    }
                }
            }
            for group in bound.or_filters() {
                let group_embedding: &[f32] = match tier {
                    DecodeTier::Embedding => embedding_scratch.as_slice(),
                    DecodeTier::Fast | DecodeTier::DimAndScalar => &[],
                };
                if !group.matches(
                    &scanned,
                    id,
                    group_embedding,
                    dim as usize,
                    &mut expr_scratch,
                )? {
                    continue 'rows;
                }
            }

            // defense-in-depth（`sql::scan` と同趣旨）。
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            state_bytes = try_accumulate_state_budget(
                state_bytes,
                std::mem::size_of::<MaterializedRow>(),
                MAX_WINDOW_STATE_BYTES,
            )?;

            let mut partition_keys = Vec::with_capacity(bound.windows().len());
            let mut order_keys = Vec::with_capacity(bound.windows().len());
            let mut agg_values = Vec::with_capacity(bound.windows().len());
            for (item_index, item) in bound.windows().iter().enumerate() {
                let mut pkeys = Vec::with_capacity(item.partition_by.len());
                for key in &item.partition_by {
                    let value = extract_window_key(key, id, &scanned)?;
                    state_bytes = try_accumulate_state_budget(
                        state_bytes,
                        window_key_value_bytes(&value),
                        MAX_WINDOW_STATE_BYTES,
                    )?;
                    pkeys.push(value);
                }
                let partition_bytes = partition_key_bytes(&pkeys);
                let counts = &mut partition_counts[item_index];
                let is_new_partition = !counts.contains_key(&partition_bytes);
                if is_new_partition {
                    check_new_partition_capacity(counts.len(), MAX_WINDOW_PARTITIONS)?;
                }
                let count = counts.entry(partition_bytes).or_insert(0);
                *count += 1;
                check_partition_row_count(*count, MAX_WINDOW_FRAME_ROWS)?;

                let mut okeys = Vec::with_capacity(item.order_by.len());
                for (key, _descending) in &item.order_by {
                    let value = extract_window_key(key, id, &scanned)?;
                    state_bytes = try_accumulate_state_budget(
                        state_bytes,
                        window_key_value_bytes(&value),
                        MAX_WINDOW_STATE_BYTES,
                    )?;
                    okeys.push(value);
                }

                let agg_value = match &item.input {
                    None => WindowInputValue::None,
                    Some(input) => {
                        let value = extract_window_input(input, dim, &scanned)?;
                        state_bytes = try_accumulate_state_budget(
                            state_bytes,
                            window_input_value_bytes(&value),
                            MAX_WINDOW_STATE_BYTES,
                        )?;
                        value
                    }
                };

                partition_keys.push(pkeys);
                order_keys.push(okeys);
                agg_values.push(agg_value);
            }

            materialized.push(MaterializedRow {
                seq,
                id,
                partition_keys,
                order_keys,
                agg_values,
            });
            check_total_row_count(materialized.len(), MAX_WINDOW_ROWS)?;
            seq = seq.checked_add(1).ok_or_else(|| {
                SqlSurfaceError::payload_too_large("window scan row count overflowed")
            })?;
        }
    }

    Ok(materialized)
}

/// [`decode_tier_for`](crate::sql::scan) と同じ意図（Issue #350）だが、ウィンドウ
/// 項目の `PARTITION BY`／`ORDER BY` キー・集計引数も参照集合へ反映する
/// （SQL-30・TASK-214）。
fn decode_tier_for_window(schema: &TableSchema, bound: &BoundScan) -> (DecodeTier, Vec<bool>) {
    let mut scalar_mask = vec![false; schema.columns.len()];
    let mut needs_embedding = false;
    let mut has_scalar_reference = false;

    for col in bound.projection() {
        match col {
            ProjectedColumn::Column { index, .. } => {
                if let Some(column) = schema.columns.get(*index) {
                    match &column.ty {
                        ColumnType::Vector(_) => needs_embedding = true,
                        _ => {
                            has_scalar_reference = true;
                            if let Some(slot) = scalar_mask.get_mut(*index) {
                                *slot = true;
                            }
                        }
                    }
                }
            }
            ProjectedColumn::Computed { expr, .. } => {
                if udf_call::references_embedding(expr) {
                    needs_embedding = true;
                }
            }
            ProjectedColumn::Id => {}
        }
    }
    if !bound.metadata_filters().is_empty() {
        has_scalar_reference = true;
    }
    for filter in bound.metadata_filters() {
        if let Some(slot) = scalar_mask.get_mut(filter.column_index()) {
            *slot = true;
        }
    }
    for expr in bound.expr_filters() {
        if udf_call::references_embedding(expr) {
            needs_embedding = true;
        }
    }
    if !bound.or_filters().is_empty() {
        has_scalar_reference = true;
    }
    for group in bound.or_filters() {
        group.visit_column_indices(&mut |idx| {
            if let Some(slot) = scalar_mask.get_mut(idx) {
                *slot = true;
            }
        });
        if group.references_embedding() {
            needs_embedding = true;
        }
    }

    for item in bound.windows() {
        for key in item
            .partition_by
            .iter()
            .chain(item.order_by.iter().map(|(k, _)| k))
        {
            if let WindowKeyRef::Column { index, .. } = key {
                has_scalar_reference = true;
                if let Some(slot) = scalar_mask.get_mut(*index) {
                    *slot = true;
                }
            }
        }
        match &item.input {
            None | Some(AggregateInput::AllVisible) | Some(AggregateInput::IdU64) => {}
            Some(AggregateInput::VectorColumnPresence) => {
                has_scalar_reference = true;
            }
            Some(
                AggregateInput::TextColumn(index)
                | AggregateInput::BooleanColumn(index)
                | AggregateInput::DateColumn(index)
                | AggregateInput::TimestampColumn(index)
                | AggregateInput::ArrayColumn(index)
                | AggregateInput::ByteaColumn(index)
                | AggregateInput::JsonColumn(index)
                | AggregateInput::EnumColumn(index)
                | AggregateInput::UuidColumn(index)
                | AggregateInput::IntegerColumn(index)
                | AggregateInput::BigIntColumn(index)
                | AggregateInput::RealColumn(index)
                | AggregateInput::DoubleColumn(index),
            ) => {
                has_scalar_reference = true;
                if let Some(slot) = scalar_mask.get_mut(*index) {
                    *slot = true;
                }
            }
            Some(AggregateInput::NumericColumn { index, .. }) => {
                has_scalar_reference = true;
                if let Some(slot) = scalar_mask.get_mut(*index) {
                    *slot = true;
                }
            }
            // 構文段が複合式の引数を受理しないため到達しない
            // （`Parser::parse_window_item` 参照）。
            Some(AggregateInput::ScalarExpr { .. }) => {
                has_scalar_reference = true;
            }
        }
    }

    let tier = if needs_embedding {
        DecodeTier::Embedding
    } else if has_scalar_reference || scalar_mask.iter().any(|&wanted| wanted) {
        DecodeTier::DimAndScalar
    } else {
        DecodeTier::Fast
    };
    (tier, scalar_mask)
}

/// [`WindowKeyRef`] の値を `scanned`（同じ行の `scan_scalar_columns_masked`
/// 結果）から取り出す。`kind` はスキーマ束縛時（`sql::parser::resolve_window_key`）
/// に確定済みで、`scanned` は同じ `schema` から得ているため型不一致は実装バグと
/// して fail-closed に拒否する。
fn extract_window_key(
    key: &WindowKeyRef,
    id: u64,
    scanned: &[Option<row_codec::ScalarRef<'_>>],
) -> Result<Option<WindowKeyValue>, SqlSurfaceError> {
    let (index, kind) = match key {
        WindowKeyRef::Id => return Ok(Some(WindowKeyValue::Id(id))),
        WindowKeyRef::Column { index, kind } => (*index, *kind),
    };
    let raw = scanned.get(index).copied().flatten();
    Ok(match (kind, raw) {
        (WindowKeyKind::Text, Some(row_codec::ScalarRef::Text(s))) => {
            Some(WindowKeyValue::Text(aggregate::try_clone_str(s)?))
        }
        (WindowKeyKind::Text, None) => None,
        (WindowKeyKind::Integer, Some(row_codec::ScalarRef::Integer(v))) => {
            Some(WindowKeyValue::Integer(v))
        }
        (WindowKeyKind::Integer, None) => None,
        (WindowKeyKind::BigInt, Some(row_codec::ScalarRef::BigInt(v))) => {
            Some(WindowKeyValue::BigInt(v))
        }
        (WindowKeyKind::BigInt, None) => None,
        (WindowKeyKind::Real, Some(row_codec::ScalarRef::Real(v))) => Some(WindowKeyValue::Real(v)),
        (WindowKeyKind::Real, None) => None,
        (WindowKeyKind::Double, Some(row_codec::ScalarRef::Double(v))) => {
            Some(WindowKeyValue::Double(v))
        }
        (WindowKeyKind::Double, None) => None,
        (WindowKeyKind::Boolean, Some(row_codec::ScalarRef::Bool(v))) => {
            Some(WindowKeyValue::Boolean(v))
        }
        (WindowKeyKind::Boolean, None) => None,
        (WindowKeyKind::Date, Some(v)) => match v.as_date() {
            Some(d) => Some(WindowKeyValue::Date(d)),
            None => return Err(window_bug("DATE key scan yielded a non-Date scalar value")),
        },
        (WindowKeyKind::Date, None) => None,
        (WindowKeyKind::Timestamp, Some(v)) => match v.as_timestamp() {
            Some(t) => Some(WindowKeyValue::Timestamp(t)),
            None => {
                return Err(window_bug(
                    "TIMESTAMP key scan yielded a non-Timestamp scalar value",
                ))
            }
        },
        (WindowKeyKind::Timestamp, None) => None,
        (WindowKeyKind::Numeric, Some(v)) => match v.as_numeric() {
            Some(d) => Some(WindowKeyValue::Numeric(d)),
            None => {
                return Err(window_bug(
                    "NUMERIC key scan yielded a non-Numeric scalar value",
                ))
            }
        },
        (WindowKeyKind::Numeric, None) => None,
        (WindowKeyKind::Uuid, Some(v)) => match v.as_uuid() {
            Some(u) => Some(WindowKeyValue::Uuid(u)),
            None => return Err(window_bug("UUID key scan yielded a non-Uuid scalar value")),
        },
        (WindowKeyKind::Uuid, None) => None,
        (_, Some(_)) => return Err(window_bug("window key scan type mismatch")),
    })
}

/// [`AggregateInput`] の値を `scanned`／`dim` から取り出す（SQL-30・TASK-214）。
/// 構文段が複合式を受理しないため `ScalarExpr` は到達しない
/// （到達した場合は実装バグとして fail-closed に拒否する）。
fn extract_window_input(
    input: &AggregateInput,
    dim: u32,
    scanned: &[Option<row_codec::ScalarRef<'_>>],
) -> Result<WindowInputValue, SqlSurfaceError> {
    Ok(match input {
        AggregateInput::AllVisible | AggregateInput::IdU64 => WindowInputValue::AllVisibleOrId,
        AggregateInput::VectorColumnPresence => WindowInputValue::VectorPresence(dim),
        AggregateInput::TextColumn(index) => WindowInputValue::Text(
            scanned
                .get(*index)
                .copied()
                .flatten()
                .and_then(|v| v.as_text())
                .map(aggregate::try_clone_str)
                .transpose()?,
        ),
        AggregateInput::IntegerColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(row_codec::ScalarRef::Integer(v)) => WindowInputValue::Integer(Some(v)),
            Some(_) => return Err(window_bug("INTEGER input scan type mismatch")),
            None => WindowInputValue::Integer(None),
        },
        AggregateInput::BigIntColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(row_codec::ScalarRef::BigInt(v)) => WindowInputValue::BigInt(Some(v)),
            Some(_) => return Err(window_bug("BIGINT input scan type mismatch")),
            None => WindowInputValue::BigInt(None),
        },
        AggregateInput::RealColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(row_codec::ScalarRef::Real(v)) => WindowInputValue::Real(Some(v)),
            Some(_) => return Err(window_bug("REAL input scan type mismatch")),
            None => WindowInputValue::Real(None),
        },
        AggregateInput::DoubleColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(row_codec::ScalarRef::Double(v)) => WindowInputValue::Double(Some(v)),
            Some(_) => return Err(window_bug("DOUBLE input scan type mismatch")),
            None => WindowInputValue::Double(None),
        },
        AggregateInput::DateColumn(index) => {
            match scanned.get(*index).copied().flatten() {
                Some(v) => WindowInputValue::Date(Some(v.as_date().ok_or_else(|| {
                    window_bug("DATE input scan yielded a non-Date scalar value")
                })?)),
                None => WindowInputValue::Date(None),
            }
        }
        AggregateInput::TimestampColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(v) => WindowInputValue::Timestamp(Some(v.as_timestamp().ok_or_else(|| {
                window_bug("TIMESTAMP input scan yielded a non-Timestamp scalar value")
            })?)),
            None => WindowInputValue::Timestamp(None),
        },
        AggregateInput::NumericColumn { index, .. } => match scanned.get(*index).copied().flatten()
        {
            Some(v) => WindowInputValue::Numeric(Some(v.as_numeric().ok_or_else(|| {
                window_bug("NUMERIC input scan yielded a non-Numeric scalar value")
            })?)),
            None => WindowInputValue::Numeric(None),
        },
        AggregateInput::BooleanColumn(index)
        | AggregateInput::ArrayColumn(index)
        | AggregateInput::ByteaColumn(index)
        | AggregateInput::JsonColumn(index)
        | AggregateInput::EnumColumn(index)
        | AggregateInput::UuidColumn(index) => {
            WindowInputValue::PresenceOnly(scanned.get(*index).copied().flatten().is_some())
        }
        // 構文段（`Parser::parse_window_item`）が裸の列参照・`*` のみを受理する
        // ため到達しない。
        AggregateInput::ScalarExpr { .. } => {
            return Err(window_bug(
                "window aggregate input unexpectedly carries a compound expression",
            ))
        }
    })
}

/// `Option<WindowKeyValue>` 1 つの概算メモリコスト（[`MAX_WINDOW_STATE_BYTES`]
/// 予算計上用）。
fn window_key_value_bytes(value: &Option<WindowKeyValue>) -> usize {
    let base = std::mem::size_of::<Option<WindowKeyValue>>();
    match value {
        Some(WindowKeyValue::Text(s)) => base + s.len(),
        _ => base,
    }
}

/// [`WindowInputValue`] 1 つの概算メモリコスト（同上）。
fn window_input_value_bytes(value: &WindowInputValue) -> usize {
    let base = std::mem::size_of::<WindowInputValue>();
    match value {
        WindowInputValue::Text(Some(s)) => base + s.len(),
        _ => base,
    }
}

/// パーティションキー（複数列）の同値判定用の正準バイト列を作る。NULL 同士は
/// 常に同じタグ（`0`）にエンコードされるため、`GROUP BY` と同じ「NULL 同士は
/// 同値」の規則を自然に満たす。ハッシュ照合専用の識別子であり、順序は保証しない
/// （順序が必要な箇所は [`cmp_window_key`] を使う）。
fn partition_key_bytes(keys: &[Option<WindowKeyValue>]) -> Vec<u8> {
    let mut out = Vec::new();
    for key in keys {
        match key {
            None => out.push(0u8),
            Some(WindowKeyValue::Id(v)) => {
                out.push(1);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Text(s)) => {
                out.push(2);
                out.extend_from_slice(&(s.len() as u64).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Some(WindowKeyValue::Integer(v)) => {
                out.push(3);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::BigInt(v)) => {
                out.push(4);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Real(v)) => {
                out.push(5);
                out.extend_from_slice(&v.to_bits().to_be_bytes());
            }
            Some(WindowKeyValue::Double(v)) => {
                out.push(6);
                out.extend_from_slice(&v.to_bits().to_be_bytes());
            }
            Some(WindowKeyValue::Boolean(v)) => {
                out.push(7);
                out.push(u8::from(*v));
            }
            Some(WindowKeyValue::Date(v)) => {
                out.push(8);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Timestamp(v)) => {
                out.push(9);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Numeric(d)) => {
                out.push(10);
                out.extend_from_slice(&d.unscaled().to_be_bytes());
                out.push(d.scale());
            }
            Some(WindowKeyValue::Uuid(u)) => {
                out.push(11);
                out.extend_from_slice(u.as_bytes());
            }
        }
    }
    out
}

/// `PARTITION BY`／`ORDER BY` の同値・大小比較で使う型ごとの比較（NULL は
/// PostgreSQL 既定〔ASC は末尾・DESC は先頭〕）。同じ添字のキーは束縛時
/// （`resolve_window_key`）にすべて同じ `kind` を持つため、異なる variant 同士の
/// 比較は到達しない（安定ソートを壊さないよう `Equal` へ倒す）。
fn cmp_window_key(
    a: &Option<WindowKeyValue>,
    b: &Option<WindowKeyValue>,
    descending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if descending {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if descending {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            let raw = cmp_window_key_value(x, y);
            if descending {
                raw.reverse()
            } else {
                raw
            }
        }
    }
}

fn cmp_window_key_value(a: &WindowKeyValue, b: &WindowKeyValue) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (WindowKeyValue::Id(x), WindowKeyValue::Id(y)) => x.cmp(y),
        (WindowKeyValue::Text(x), WindowKeyValue::Text(y)) => x.cmp(y),
        (WindowKeyValue::Integer(x), WindowKeyValue::Integer(y)) => x.cmp(y),
        (WindowKeyValue::BigInt(x), WindowKeyValue::BigInt(y)) => x.cmp(y),
        (WindowKeyValue::Real(x), WindowKeyValue::Real(y)) => {
            f64::from(*x).total_cmp(&f64::from(*y))
        }
        (WindowKeyValue::Double(x), WindowKeyValue::Double(y)) => x.total_cmp(y),
        (WindowKeyValue::Boolean(x), WindowKeyValue::Boolean(y)) => x.cmp(y),
        (WindowKeyValue::Date(x), WindowKeyValue::Date(y)) => x.cmp(y),
        (WindowKeyValue::Timestamp(x), WindowKeyValue::Timestamp(y)) => x.cmp(y),
        (WindowKeyValue::Numeric(x), WindowKeyValue::Numeric(y)) => crate::numeric::cmp_exact(x, y),
        (WindowKeyValue::Uuid(x), WindowKeyValue::Uuid(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => Ordering::Equal,
    }
}

/// ウィンドウ項目 1 つ（`bound.windows()[item_index]`）を評価し、`id -> Cell` の
/// 写像を返す（SQL-30・TASK-214）。
fn evaluate_window_item(
    item: &BoundWindowItem,
    item_index: usize,
    materialized: &[MaterializedRow],
) -> Result<HashMap<u64, Cell>, SqlSurfaceError> {
    let mut partitions: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (row_idx, row) in materialized.iter().enumerate() {
        let bytes = partition_key_bytes(&row.partition_keys[item_index]);
        partitions.entry(bytes).or_default().push(row_idx);
    }

    let mut result = HashMap::with_capacity(materialized.len());
    for indices in partitions.into_values() {
        evaluate_partition(item, item_index, materialized, indices, &mut result)?;
    }
    Ok(result)
}

/// 1 パーティション分のウィンドウ値を計算する（安定ソート → peer 分割 →
/// 順位/集計評価。§モジュールドキュメント「評価の意味」参照）。
fn evaluate_partition(
    item: &BoundWindowItem,
    item_index: usize,
    materialized: &[MaterializedRow],
    mut indices: Vec<usize>,
    out: &mut HashMap<u64, Cell>,
) -> Result<(), SqlSurfaceError> {
    // 安定ソート。比較順は ORDER BY キー → `id` 昇順 → `seq` 昇順（走査順の
    // タイブレーク。`sort_by`〔安定〕のみを使い `sort_unstable*` は使わない
    // ——`scripts/check_sort_determinism.sh` が CI で検知する）。
    indices.sort_by(|&a, &b| {
        let ra = &materialized[a];
        let rb = &materialized[b];
        for (key_idx, (_, descending)) in item.order_by.iter().enumerate() {
            let ord = cmp_window_key(
                &ra.order_keys[item_index][key_idx],
                &rb.order_keys[item_index][key_idx],
                *descending,
            );
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        ra.id.cmp(&rb.id).then(ra.seq.cmp(&rb.seq))
    });

    let mut scratch: Vec<StackValue> = Vec::new();
    let has_order_by = !item.order_by.is_empty();

    let mut acc: Option<Accumulator> = match &item.input {
        None => None,
        Some(input) => Some(Accumulator::new(
            window_func_to_aggregate_func(item.func)?,
            input,
        )?),
    };

    let mut rank = 0usize;
    let mut dense_rank = 0usize;
    let mut idx = 0usize;
    while idx < indices.len() {
        let mut peer_end = idx + 1;
        if has_order_by {
            while peer_end < indices.len()
                && peer_group_equal(
                    item,
                    item_index,
                    materialized,
                    indices[idx],
                    indices[peer_end],
                )
            {
                peer_end += 1;
            }
        } else {
            peer_end = indices.len();
        }

        if let (Some(acc_mut), Some(input)) = (acc.as_mut(), &item.input) {
            for &row_idx in &indices[idx..peer_end] {
                observe_window_row(
                    acc_mut,
                    input,
                    materialized[row_idx].id,
                    &materialized[row_idx].agg_values[item_index],
                    &mut scratch,
                )?;
            }
        }

        // 集計関数（`Count`/`Sum`/`Avg`/`Min`/`Max`）の値は peer グループ内の
        // 全行で同一（`acc` は peer グループ単位でしか変化しない累積器のため）。
        // 以前は行ごとに `acc.clone().finish()` を呼んでいたが、peer グループに
        // つき 1 回だけ計算し `Cell::clone()` で配るよう変更（挙動不変・
        // レビュー指摘対応。PR #930 最終レビュー指摘 3）。順位関数
        // （`RowNumber`/`Rank`/`DenseRank`）は行ごとに値が変わるため対象外。
        let aggregate_cell: Option<Cell> = match item.func {
            WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => None,
            WindowFunc::Count
            | WindowFunc::Sum
            | WindowFunc::Avg
            | WindowFunc::Min
            | WindowFunc::Max => {
                let acc_ref = acc.clone().ok_or_else(|| {
                    window_bug("aggregate window function is missing its accumulator")
                })?;
                Some(acc_ref.finish()?)
            }
        };

        let row_number_base = idx;
        for (offset, &row_idx) in indices[idx..peer_end].iter().enumerate() {
            let row_number = row_number_base + offset + 1;
            let cell = match item.func {
                WindowFunc::RowNumber => Cell::Integer(row_number as u64),
                WindowFunc::Rank => Cell::Integer((rank + 1) as u64),
                WindowFunc::DenseRank => Cell::Integer((dense_rank + 1) as u64),
                WindowFunc::Count
                | WindowFunc::Sum
                | WindowFunc::Avg
                | WindowFunc::Min
                | WindowFunc::Max => aggregate_cell.clone().ok_or_else(|| {
                    window_bug("aggregate window function is missing its finished cell")
                })?,
            };
            out.insert(materialized[row_idx].id, cell);
        }

        rank = peer_end;
        dense_rank += 1;
        idx = peer_end;
    }

    Ok(())
}

fn peer_group_equal(
    item: &BoundWindowItem,
    item_index: usize,
    materialized: &[MaterializedRow],
    a: usize,
    b: usize,
) -> bool {
    for key_idx in 0..item.order_by.len() {
        if !window_key_value_eq(
            &materialized[a].order_keys[item_index][key_idx],
            &materialized[b].order_keys[item_index][key_idx],
        ) {
            return false;
        }
    }
    true
}

fn window_key_value_eq(a: &Option<WindowKeyValue>, b: &Option<WindowKeyValue>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => cmp_window_key_value(x, y) == std::cmp::Ordering::Equal,
        _ => false,
    }
}

fn window_func_to_aggregate_func(
    func: WindowFunc,
) -> Result<crate::sql::allowlist::AggregateFunc, SqlSurfaceError> {
    use crate::sql::allowlist::AggregateFunc;
    match func {
        WindowFunc::Count => Ok(AggregateFunc::Count),
        WindowFunc::Sum => Ok(AggregateFunc::Sum),
        WindowFunc::Avg => Ok(AggregateFunc::Avg),
        WindowFunc::Min => Ok(AggregateFunc::Min),
        WindowFunc::Max => Ok(AggregateFunc::Max),
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => Err(window_bug(
            "ranking window function unexpectedly requires an accumulator",
        )),
    }
}

/// `index + 1` の長さで `index` にのみ値を持つ `scanned` を組み立てる
/// （[`Accumulator::observe`] が `scanned.get(index)` でしか参照しないため、
/// 他の添字はすべて `None` でよい）。
fn build_scanned(
    index: usize,
    value: Option<row_codec::ScalarRef<'_>>,
) -> Vec<Option<row_codec::ScalarRef<'_>>> {
    let mut out = vec![None; index + 1];
    if let Some(slot) = out.get_mut(index) {
        *slot = value;
    }
    out
}

/// [`WindowInputValue`]（materialize 段で owned 化した値）を、
/// [`Accumulator::observe`] が期待する借用形（`scanned: &[Option<ScalarRef>]`・
/// `vector: &RowVector`）へその場で組み立てて 1 回観測する。owned 値は本関数の
/// スコープ内でのみ借用されるため、借用の生存期間問題は起きない。
fn observe_window_row(
    acc: &mut Accumulator,
    input: &AggregateInput,
    id: u64,
    value: &WindowInputValue,
    scratch: &mut Vec<StackValue>,
) -> Result<(), SqlSurfaceError> {
    let no_vector = RowVector {
        dim: 0,
        values: None,
    };
    match (input, value) {
        (AggregateInput::AllVisible | AggregateInput::IdU64, _) => {
            acc.observe(input, id, &no_vector, &[], scratch)
        }
        (AggregateInput::VectorColumnPresence, WindowInputValue::VectorPresence(dim)) => {
            let vector = RowVector {
                dim: *dim,
                values: None,
            };
            acc.observe(input, id, &vector, &[], scratch)
        }
        (AggregateInput::TextColumn(index), WindowInputValue::Text(v)) => {
            let scanned = build_scanned(*index, v.as_deref().map(row_codec::ScalarRef::Text));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::IntegerColumn(index), WindowInputValue::Integer(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Integer));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::BigIntColumn(index), WindowInputValue::BigInt(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::BigInt));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::RealColumn(index), WindowInputValue::Real(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Real));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::DoubleColumn(index), WindowInputValue::Double(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Double));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::DateColumn(index), WindowInputValue::Date(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Date));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::TimestampColumn(index), WindowInputValue::Timestamp(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Timestamp));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::NumericColumn { index, .. }, WindowInputValue::Numeric(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Numeric));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (
            AggregateInput::BooleanColumn(index)
            | AggregateInput::ArrayColumn(index)
            | AggregateInput::ByteaColumn(index)
            | AggregateInput::JsonColumn(index)
            | AggregateInput::EnumColumn(index)
            | AggregateInput::UuidColumn(index),
            WindowInputValue::PresenceOnly(present),
        ) => {
            // これらの型は `Accumulator::observe` が非 NULL の有無だけを見る
            // （値の中身は見ない）ため、任意の `Some`/`None` の
            // `ScalarRef::Bool` で代用できる（`sql::aggregate` の該当分岐参照）。
            let scanned = build_scanned(
                *index,
                if *present {
                    Some(row_codec::ScalarRef::Bool(true))
                } else {
                    None
                },
            );
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        _ => Err(window_bug(
            "window aggregate input/value combination mismatch",
        )),
    }
}

/// 投影段（§モジュールドキュメント参照）: ウィンドウ以外の投影・`LIMIT`／
/// `OFFSET` は `windows` を空にした複製を [`crate::sql::scan::execute_scan`] へ
/// 渡すことで既存の実行器をそのまま再利用し、その結果へウィンドウ列を
/// [`crate::sql::parser::BoundWindowItem::position`] に基づいて差し込む。
fn build_result(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
    window_values: &[HashMap<u64, Cell>],
    max_result_bytes: usize,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut base_bound = bound.clone();
    base_bound.windows = Vec::new();
    let base_result = crate::sql::scan::execute_scan_with_budget(
        read_txn,
        ctx,
        schema,
        &base_bound,
        max_result_bytes,
    )?;

    let plain_len = base_result.columns.len();
    let total_len = plain_len + window_values.len();

    // ウィンドウ項目が占める SELECT リスト内位置（`position`）を除いた残りへ、
    // 通常項目を出現順に割り当てる（`Projection` 自体はウィンドウ項目の位置情報を
    // 保持しないため、ここで復元する）。
    let mut window_positions: Vec<usize> = bound.windows().iter().map(|w| w.position).collect();
    window_positions.sort_unstable();
    let mut plain_positions: Vec<usize> = Vec::with_capacity(plain_len);
    {
        let mut wpos_iter = window_positions.iter().peekable();
        for pos in 0..total_len {
            if wpos_iter.peek() == Some(&&pos) {
                wpos_iter.next();
            } else {
                plain_positions.push(pos);
            }
        }
    }
    if plain_positions.len() != plain_len {
        // 通常項目の位置数と `base_result` の列数が一致しない状態は、束縛段
        // （`sql::parser::bind_scan_with_dummy_flags`）の不変条件違反であり、
        // untrusted 入力起因ではないため fail-closed に内部エラーとする。
        return Err(SqlSurfaceError::Internal {
            detail: "window scan projection position mismatch".to_string(),
        });
    }

    let mut final_columns: Vec<Option<ColumnMeta>> = vec![None; total_len];
    for (pos, meta) in plain_positions.iter().zip(base_result.columns.iter()) {
        if let Some(slot) = final_columns.get_mut(*pos) {
            *slot = Some(meta.clone());
        }
    }
    for item in bound.windows() {
        if let Some(slot) = final_columns.get_mut(item.position) {
            *slot = Some(ColumnMeta::Computed {
                name: item.name.clone(),
            });
        }
    }
    let columns: Vec<ColumnMeta> = final_columns
        .into_iter()
        .enumerate()
        .map(|(pos, c)| {
            c.ok_or_else(|| SqlSurfaceError::Internal {
                detail: format!("window scan column metadata missing at position {pos}"),
            })
        })
        .collect::<Result<_, _>>()?;

    let mut rows = Vec::with_capacity(base_result.rows.len());
    for base_row in base_result.rows {
        let mut cells: Vec<Cell> = Vec::with_capacity(total_len);
        let mut plain_iter = base_row.cells.into_iter();
        for pos in 0..total_len {
            if let Some(item_index) = bound.windows().iter().position(|w| w.position == pos) {
                // `materialize_rows` の走査と `execute_scan_with_budget`（`base_result`）の
                // 走査は独立実装であり、本来は同一の行集合に一致するはずの不変条件を持つ
                // （テストで一致を確認済み）。将来のドリフトで行 id が食い違った場合に
                // 静かに NULL を返すと不正確な結果を返してしまうため、この不一致は
                // untrusted 入力起因ではない実装バグとして fail-closed にする。
                let cell = window_values
                    .get(item_index)
                    .ok_or_else(|| window_bug("window item index out of range in build_result"))?
                    .get(&base_row.id)
                    .cloned()
                    .ok_or_else(|| {
                        window_bug("window_values missing entry for a base scan row id")
                    })?;
                cells.push(cell);
            } else {
                match plain_iter.next() {
                    Some(cell) => cells.push(cell),
                    None => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "window scan projection cell count mismatch".to_string(),
                        })
                    }
                }
            }
        }
        rows.push(ResultRow {
            id: base_row.id,
            score: base_row.score,
            cells,
        });
    }

    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod limit_tests {
    //! [`MAX_WINDOW_PARTITIONS`]／[`MAX_WINDOW_ROWS`]／[`MAX_WINDOW_FRAME_ROWS`]／
    //! [`MAX_WINDOW_STATE_BYTES`] の実行時超過が `54000`
    //! （[`SqlSurfaceError::payload_too_large`]）で fail-closed に拒否されることの
    //! 単体テスト（PR #930 最終レビュー指摘 2）。`MAX_WINDOW_ROWS`／
    //! `MAX_WINDOW_FRAME_ROWS` は実装既定値が 100 万行と大きく、実データでの
    //! 結合テストは重すぎるため、判定ロジックを切り出した
    //! [`check_new_partition_capacity`]／[`check_partition_row_count`]／
    //! [`check_total_row_count`] を小さい `cap` を渡して直接検証する
    //! （本体は常に実装既定の `MAX_WINDOW_*` 定数を渡すため、この単体テストは
    //! 定数値そのものではなく比較ロジック〔`>=`/`>`〕の正しさを固定する）。
    //! `MAX_WINDOW_PARTITIONS`（`sql::group_by::MAX_GROUPS` = 10,000）超過は
    //! 実データでも現実的なため `tests/sql30_window.rs` に別途結合テストを持つ
    //! （本モジュールの単体テストと二重に固定することで、判定ロジックと実行系
    //! 統合の両方を回帰対象にする）。

    use super::*;

    #[test]
    fn new_partition_capacity_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 3usize;
        // 既存パーティション数が cap 未満なら新規パーティションを受理する。
        assert!(check_new_partition_capacity(0, cap).is_ok());
        assert!(check_new_partition_capacity(cap - 1, cap).is_ok());
        // 既存パーティション数が cap に達した状態で新規パーティションを
        // 追加しようとすると拒否する（`>=` 比較。ちょうど cap 件までは
        // 既存パーティションとして許容し、cap+1 件目の新規作成を拒否する）。
        let err = check_new_partition_capacity(cap, cap).expect_err("cap 到達時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn partition_row_count_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 5usize;
        assert!(check_partition_row_count(cap, cap).is_ok());
        let err = check_partition_row_count(cap + 1, cap).expect_err("cap 超過時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn total_row_count_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 7usize;
        assert!(check_total_row_count(cap, cap).is_ok());
        let err = check_total_row_count(cap + 1, cap).expect_err("cap 超過時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn state_budget_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 100usize;
        assert_eq!(try_accumulate_state_budget(90, 10, cap).unwrap(), 100);
        let err = try_accumulate_state_budget(90, 11, cap).expect_err("cap 超過時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn state_budget_rejects_on_addition_overflow() {
        let err = try_accumulate_state_budget(usize::MAX, 1, usize::MAX)
            .expect_err("オーバーフローは拒否");
        assert_eq!(err.wire_code(), "54000");
    }
}
