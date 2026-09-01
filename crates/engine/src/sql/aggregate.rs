//! 集計 SELECT（`GROUP BY` なし・単一行結果、TASK-166・SQL-13）の実行本体。
//! `GROUP BY` ありの複数行実行は [`crate::sql::group_by::execute_grouped_aggregate`]
//! （TASK-167・SQL-14）が担い、[`execute_aggregate`] は `BoundAggregate::group_by`
//! の有無で振り分けるだけの薄い分岐を持つ。
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
//! `tenant_id`・`visibility`・本体デコード再開オフセットを取り出し
//! （[`crate::storage::decode_row_header`]）、[`crate::policy::PolicyContext::is_visible`]
//! が `false` を返す行は embedding・metadata を一切デコードせずスキップする。
//! 可視行のみ、ヘッダのオフセットを引き継いで本体デコール（[`crate::storage::decode_row_body_into`]。
//! スクラッチ再利用によりヘッダの二重デコード・行ごとの `Vec<f32>` 新規確保を
//! 避ける。Issue #349・Issue #314 の横展開）して SCALAR 段（`WHERE`）・集計へ進む。
//! `COUNT` 等の集計値から他テナント行の存在・件数を推測できないという契約
//! （RLS-7・RLS-8）は、この「不可視行は集約対象に一切現れない」という走査自体の
//! 構造で担保する。物理キー側 `tenant_id`（`key_tenant`）とヘッダ側 `tenant_id`
//! の整合検査（TABLE-12。従来 `storage::decode_row_for_key` が担っていた検査）は、
//! 本体デコード直前に明示比較として実行する（呼び出し元へ移設。検査自体は
//! 削除しない）。

use crate::catalog::{self, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::allowlist::{AggregateFunc, SqlSurfaceError};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::parser::{AggregateInput, BoundAggregate};
use crate::sql::udf_call::ExprValue;
use crate::storage::{self, StorageError};
use redb::ReadableTable;

/// 型不整合・NULL 契約違反等の意味論的問題ではなく、`bind_aggregate` の型検査を
/// 通過したはずの `(AggregateFunc, AggregateInput)` の組み合わせが
/// [`Accumulator::new`] の網羅から漏れていた場合に返す（実装バグの検出用。
/// untrusted 入力起因ではないため `wire_code` は `XX000`）。
pub(crate) fn accumulator_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("aggregate accumulator/input type mismatch: {detail}"),
    }
}

/// 集計項目 1 つの実行時アキュムレータ（TASK-166・SQL-13）。すべて O(1) 状態
/// （`TextMin`/`TextMax` のみ、新しい極値を更新するたびに高々 1 本の `String` を
/// 保持し直す。`.claude/rules/security.md`「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。
pub(crate) enum Accumulator {
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
    pub(crate) fn new(
        func: AggregateFunc,
        input: &AggregateInput,
    ) -> Result<Self, SqlSurfaceError> {
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
            (Sum, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatSum(None),
            (Avg, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatAvg {
                sum: None,
                count: 0,
            },
            (Min, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatMin(None),
            (Max, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatMax(None),
            (Min, AggregateInput::TextColumn(_)) => Accumulator::TextMin(None),
            (Max, AggregateInput::TextColumn(_)) => Accumulator::TextMax(None),
            _ => return Err(accumulator_bug("unsupported (func, input) combination")),
        })
    }

    /// 可視行 1 件を観測して状態を更新する。`scanned` は同じ行の
    /// `row_codec::scan_scalar_columns` 結果（借用のまま）。
    /// `scratch` は [`crate::sql::expr_program::ExprProgram::eval`] が使う
    /// スクラッチスタック（呼び出し元の行ループの外で確保し使い回す。Issue #353
    /// で行ごとの再帰評価をなくすために導入。`ScalarExpr` 以外の入力では
    /// 参照されない）。
    pub(crate) fn observe(
        &mut self,
        input: &AggregateInput,
        id: u64,
        embedding: &[f32],
        scanned: &[Option<&str>],
        scratch: &mut Vec<ExprValue>,
    ) -> Result<(), SqlSurfaceError> {
        match input {
            AggregateInput::AllVisible => self.observe_present(),
            // nullable な `VECTOR` 列（TABLE-5 の `ALTER TABLE ADD COLUMN` で追加
            // された列を含む）の裸の列参照。`row.embedding` が空 = 未設定（NULL）
            // という `storage::Row` の既存契約に従い、NULL 行は数えない
            // （PR #229 codex-review 指摘対応）。
            AggregateInput::VectorColumnPresence => {
                if embedding.is_empty() {
                    Ok(())
                } else {
                    self.observe_present()
                }
            }
            AggregateInput::IdU64 => self.observe_id(id),
            AggregateInput::TextColumn(index) => {
                let value = scanned.get(*index).copied().flatten();
                self.observe_text(value)
            }
            AggregateInput::ScalarExpr { program, .. } => {
                match program.eval(id, embedding, scratch)? {
                    ExprValue::Scalar(v) => self.observe_float(v),
                    // `resolve_aggregate_input` が `ExprType::Scalar` のみを
                    // `ScalarExpr` として束縛するため到達しない（束縛段の型検査と
                    // 評価結果の型が食い違う実装バグの検出用）。
                    _ => Err(accumulator_bug(
                        "scalar-typed BoundExpr evaluated to a non-scalar value",
                    )),
                }
            }
        }
    }

    /// NULL・非存在の概念を持たない入力（`*`・`id`・`Scalar` 式）を数えるだけの
    /// 経路。`VECTOR` 列は nullable のため別経路
    /// （[`AggregateInput::VectorColumnPresence`]）を使う。`COUNT` 以外がこの
    /// 経路に来ることはない（[`Accumulator::new`] の型検査）。
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

    /// `TextMin`/`TextMax` が現在保持している文字列のバイト数（未保持なら 0、
    /// それ以外の集計種別は常に 0）。呼び出し元（`sql::group_by`）が `observe`
    /// 前後でこの値を比較し、`GROUP BY` 全体での TEXT アキュムレータ累計バイト数を
    /// 予算管理するために公開する（PR #230 codex-review 指摘対応: `GROUP BY` は
    /// グループ数に比例して `TextMin`/`TextMax` インスタンスが増えるため、
    /// 単一行集計〔TASK-166・SQL-13〕の「項目ごとに高々 1 本」という有界性だけでは
    /// 不十分）。
    pub(crate) fn text_len(&self) -> usize {
        match self {
            Accumulator::TextMin(m) | Accumulator::TextMax(m) => {
                m.as_ref().map(String::len).unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// 空集合契約: `COUNT` は `0`、それ以外は `NULL`（PostgreSQL 互換。TASK-166・
    /// SQL-13）。`AVG` は合計を保持したまま `finish` の時点で除算する。
    pub(crate) fn finish(self) -> Cell {
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
pub(crate) fn try_clone_str(value: &str) -> Result<String, SqlSurfaceError> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("aggregate text value exceeds available memory")
    })?;
    owned.push_str(value);
    Ok(owned)
}

pub(crate) fn storage_internal(e: impl Into<StorageError>) -> SqlSurfaceError {
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
    // TASK-167（SQL-14）: `GROUP BY` ありは複数行結果を返すため
    // `sql::group_by::execute_grouped_aggregate` へ分岐する（グループ表の有界化・
    // `HAVING`/`ORDER BY`/`LIMIT` はそちらの責務）。`GROUP BY` なしは以下の
    // TASK-166・SQL-13 の単一行経路を維持する（既存挙動は変更しない）。
    if bound.group_by.is_some() {
        return crate::sql::group_by::execute_grouped_aggregate(read_txn, ctx, schema, bound);
    }

    let mut accumulators = Vec::with_capacity(bound.items.len());
    for item in &bound.items {
        accumulators.push(Accumulator::new(item.func, &item.input)?);
    }
    // Issue #353: `ExprProgram::eval` の明示スタック。行ループの外で 1 回だけ
    // 確保し、WHERE 式述語・`ScalarExpr` 集計項目の評価で使い回す
    // （行ごとの再確保・再帰呼び出しをなくす）。
    let mut expr_scratch: Vec<ExprValue> = Vec::new();

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

    // 可視行ごとの embedding デコード先スクラッチバッファ（Issue #349・Issue #314
    // 横展開）。行ごとに新しい `Vec<f32>` を確保する代わりにループ外で 1 本だけ
    // 確保し、`storage::decode_row_body_into` が `clear()` 後に書き込む。同一
    // テーブルは全行同一次元のため、初回のみ確保が発生し 2 行目以降は再確保なしで
    // 使い回せる（`arena.rs::build_filtered_with_rows_and_limits_in_txn` と同じ方針）。
    let mut embedding_scratch: Vec<f32> = Vec::new();

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // RLS 段（無条件・デコード前）: `arena.rs` の走査ループと同一の順序
            // （ヘッダのみ読み `predicate` 相当を通過しない行は embedding・
            // metadata を一切デコードしない）。不可視行の破損状態が対象テナントの
            // クエリ可用性へ干渉しない設計を踏襲する（codex P0 対応・Issue #137、
            // security.md P0「テナント境界」）。`offset` は本体デコードの再開位置
            // （[`storage::decode_row_body_into`] へそのまま渡す。Issue #349:
            // 従来はここで一度ヘッダを読んだ後、`decode_row_for_key` の内部で
            // ヘッダをもう一度読んでいた二重デコードを排除する）。
            let (tenant_id, visibility, offset) =
                storage::decode_row_header(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // ここに到達するのは可視行のみ。複合キー側の `key_tenant` とヘッダ側
            // `tenant_id` の不一致（内部バグ・raw redb 書き込みによる異常）を
            // fail-closed に拒否する（TABLE-12 の整合検査。従来は
            // `storage::decode_row_for_key` の内部検査だったものを明示比較として
            // 呼び出し元へ移設。検査内容・拒否時のエラーメッセージは変えない。
            // codex P0 指摘対応・PR #229 の踏襲）。
            if tenant_id != key_tenant {
                return Err(storage_internal(StorageError::Codec(
                    "row key tenant mismatch".to_string(),
                )));
            }

            // 以降は本体（embedding・metadata）をデコードして SCALAR 段・集計へ
            // 進む。ヘッダは上で既にデコード済みのため、そのオフセットを引き継いで
            // 本体のみをデコードする（行ごとに新しい `Vec<f32>` を確保する
            // `decode_row` の代わりにループ外の `embedding_scratch` へ書き込む）。
            let (dim, metadata) =
                storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                    .map_err(storage_internal)?;
            // `dim == 0` = `VECTOR` 列が未設定（NULL、TABLE-5 の
            // `ALTER TABLE ADD COLUMN` で追加された nullable 列を含む）という
            // `storage::Row` の既存契約に従い、次元検証は値が実際に存在する行に
            // だけ行う。空を無条件に次元不一致として拒否すると、`COUNT(*)` 等
            // `VECTOR` 値を参照しない集計まで nullable 列の NULL 行で `XX000`
            // 失敗する（PR #229 codex-review 指摘対応）。
            if let Some(expected) = expected_dim {
                if dim != 0 && dim != expected {
                    return Err(SqlSurfaceError::Internal {
                        detail: "aggregate row scan failed: embedding dimension mismatch"
                            .to_string(),
                    });
                }
            }

            let scanned = row_codec::scan_scalar_columns(schema, metadata)?;

            // SCALAR 段（WHERE）: 既存の検索 SELECT 実行経路（`sql::exec`）と同じ
            // 意味論（等価・前方一致条件 → 式述語の順）で適用する。
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            for program in &bound.expr_filter_programs {
                match program.eval(id, &embedding_scratch, &mut expr_scratch)? {
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

            // 構造的なトリップワイヤ（Issue #349 で単一ヘッダデコードへ統合した
            // ため、独立した二重検証ではなくなった点に注意——`tenant_id`・
            // `visibility` は上の判定と同じ [`storage::decode_row_header`] 呼び出し
            // 由来の束縛で、この間に値を書き換える経路もない。したがって現状は
            // 論理的に到達不能だが、AGG 段の直前にもう一段 `is_visible` を通す
            // ゲートを残しておくことで、将来ここに `tenant_id`/`visibility` を
            // 再導出・再束縛するコードが差し込まれた場合でも RLS 判定を経由せずに
            // 集約へ抜けられないようにする（`arena.rs::build_filtered_with_rows_and_limits_in_txn`
            // も Issue #314 で同じ単一ゲート構成を採用済み。security.md P0）。
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // AGG 段。
            for (accumulator, item) in accumulators.iter_mut().zip(&bound.items) {
                accumulator.observe(
                    &item.input,
                    id,
                    &embedding_scratch,
                    &scanned,
                    &mut expr_scratch,
                )?;
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
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::allowlist::AggregateFunc;
    use crate::sql::parser::{BoundAggregate, BoundAggregateItem};
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    /// nullable な `VECTOR` 列を宣言するテーブルへ、`storage::encode_row`
    /// （低レベル API）で直接行を書き込む。TABLE-5 が想定する「既存行が対象列を
    /// 未設定のまま持つ」状態（`row.embedding` が空）を、現行の公開 INSERT 経路
    /// （`tenant::insert_row` 系。`TableSchema::validate_embedding_dim` を介し常に
    /// 次元一致を要求する）を経由せず直接再現するための検証専用ヘルパ（PR #229
    /// codex-review 指摘対応）。
    fn write_row_direct(
        storage: &Storage,
        table_name: &str,
        tenant_id: &str,
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
                tenant_id,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((tenant_id, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// 物理キー側 `tenant_id`（`key_tenant`）とヘッダ側 `tenant_id`
    /// （`header_tenant`）を意図的にずらして raw redb 書き込みする（TABLE-12 の
    /// 整合検査を検証するための専用ヘルパ。Issue #349: 旧 `decode_row_for_key`
    /// 内部検査から `execute_aggregate`/`execute_grouped_aggregate` 側の明示比較へ
    /// 移設した後も、この不一致が fail-closed に拒否されることを固定する）。
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

    /// nullable な `VECTOR` 列（TABLE-5 想定）を単一列として持つテーブルの
    /// スキーマ・空 `BoundAggregate` を組み立てる共通部。
    fn nullable_vector_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), true)],
        )
    }

    fn bound_single(func: AggregateFunc, input: AggregateInput) -> BoundAggregate {
        BoundAggregate {
            table: "docs".to_string(),
            items: vec![BoundAggregateItem {
                func,
                input,
                name: "result".to_string(),
            }],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            rls_predicate_present: false,
            projection: vec![crate::sql::parser::ProjectionColumn::Aggregate {
                item_index: 0,
                name: "result".to_string(),
            }],
            group_by: None,
        }
    }

    #[test]
    fn count_vector_column_skips_null_rows_but_count_star_does_not() {
        let path = unique_db_path("agg-nullable-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        // id=1: VECTOR 値あり、id=2: nullable 列が未設定（embedding 空 = NULL）。
        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        // COUNT(embedding) は NULL 行（id=2）を数えない。
        let bound_vec = bound_single(AggregateFunc::Count, AggregateInput::VectorColumnPresence);
        let result = execute_aggregate(&read_txn, &ctx, &schema, &bound_vec)
            .expect("COUNT(embedding) should succeed even with a NULL row present");
        assert_eq!(result.rows[0].cells[0], Cell::Integer(1));

        // COUNT(*) は VECTOR 値を参照しないため、nullable 列の NULL 行があっても
        // 次元不一致（旧 XX000）を返さず両方の可視行を数える（本 PR の中心的指摘）。
        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let result = execute_aggregate(&read_txn, &ctx, &schema, &bound_star)
            .expect("COUNT(*) must not fail on a nullable VECTOR column's NULL row");
        assert_eq!(result.rows[0].cells[0], Cell::Integer(2));
    }

    // Issue #349: TABLE-12 の整合検査（物理キー側 `tenant_id` とヘッダ側
    // `tenant_id` の不一致）が、`decode_row_for_key` 呼び出しをやめた後の
    // 明示比較でも従来どおり fail-closed（`XX000`・`SqlSurfaceError::Internal`）に
    // 拒否されることを固定する（旧 `decode_row_for_key` 内部検査への暗黙依存を
    // 解消するための回帰）。
    #[test]
    fn key_tenant_header_tenant_mismatch_is_rejected_fail_closed() {
        let path = unique_db_path("agg-table12-mismatch");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        // key 側は tenant-a、ヘッダ側は tenant-b（tenant-a から見て可視な
        // Public 行だが、key/ヘッダ不一致という異常な行）。
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

        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let err = execute_aggregate(&read_txn, &ctx, &schema, &bound_star)
            .expect_err("key/header tenant mismatch must be rejected fail-closed");
        assert_eq!(err.wire_code(), "XX000");
    }

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
