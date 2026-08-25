//! SQL 表層の束縛層（TASK-75、対象ビヘイビア: SQL-1, SQL-2, SQL-3, SQL-4。
//! ポインタ: `docs/spec/05-tasks.md` TASK-75・`docs/spec/04-behavior/sql-surface.md`）。
//!
//! 責務境界: [`allowlist::validate_statement`](crate::sql::allowlist::validate_statement)
//! が返す [`ValidatedStatement`](crate::sql::allowlist::ValidatedStatement)
//! （構造は許可リストを通過済みだが、列名・リテラル値の意味論的妥当性は未検証）を、
//! `catalog.rs` の [`TableSchema`] と照合して意味論的に検証し、[`exec`](crate::sql::exec)
//! が直接実行できる [`BoundStatement`] へ変換する。ここで検出する違反
//! （未知の列名・列型不一致・ベクトルリテラルの不正形式・非有限値・次元不一致・
//! `LIMIT` 範囲外・hybrid の 2 引数形など「受理構文だが値が不正」）は
//! [`SqlSurfaceError::InvalidInput`]（`22000`）または、アロケーション前のサイズ上限
//! 超過は [`SqlSurfaceError::PayloadTooLarge`]（`54000`）で fail-closed に拒否する。
//!
//! `unwrap`/`expect`/添字アクセス `[]` を使わず `get()`・`checked_*` で untrusted な
//! リテラル文字列を解析する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{
    FunctionArg, OrderByForm, Projection, ValidatedStatement, WherePredicate,
};
use crate::sql::plan::EvaluationOrder;

/// ベクトルリテラルの生バイト長上限（SQL-1）。アロケーション（`Vec<f32>` の確保・
/// カンマ分割）に入る前にこの長さで拒否する。
const MAX_VECTOR_LITERAL_BYTES: usize = 64 * 1024;

/// 投影対象の 1 列。`Id` は疑似列（[`crate::storage::Row::id`] 由来。スキーマの列では
/// ないため `column_index` を持たない）。`Column` の `index` は `TableSchema::columns`
/// の列順インデックス（[`crate::row_codec::decode_scalar_columns`] が返す `Vec` の
/// 添字と一致する。`VECTOR` 列の位置は常に `row_codec::Value::Null` が入るため、
/// `exec.rs` はその位置を投影する際 `storage::Row::embedding` を別途参照する）。
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectedColumn {
    Id,
    Column { index: usize, name: String },
}

/// SCALAR 段（`WHERE <列> = '<literal>'`）で適用する等価条件 1 件。
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarEq {
    pub column_index: usize,
    pub value: String,
}

/// DISTANCE 段のランキング方式。C1/C2/C3（純粋・スカラー条件付き・RLS 適用 Top-k）は
/// `Distance`、C4（ハイブリッド）は `Hybrid` を使う（SQL-1〜4）。
#[derive(Debug, Clone, PartialEq)]
pub enum Ranking {
    Distance {
        query: Vec<f32>,
    },
    Hybrid {
        query: Vec<f32>,
        text_column_index: usize,
        query_text: String,
    },
}

/// 束縛済みの SQL 文（[`exec::execute_statement`](crate::sql::exec::execute_statement)
/// が直接実行する入力形）。
///
/// **TASK-161 で意図的に非公開化した破壊的変更（BREAKING CHANGE）**: 全フィールドを
/// `pub` から `pub(crate)` へ変更し `#[non_exhaustive]` を付与した。クレート外からの
/// 直接のフィールド参照・構造体リテラル構築は今後不可能。構築は [`BoundStatement::new`]
/// ／[`BoundStatement::with_mode`]、読み取りは [`BoundStatement::table`] 等の各アクセサー
/// メソッドを使う（詳細は PR #188 の Breaking Changes 節を参照。TASK-164 拡張点の前方
/// 互換確保とカプセル化のため）。
///
/// `#[non_exhaustive]`: TASK-161（SQL-12）で `mode` フィールドを追加した際、既存の
/// 構造体リテラル構築コードが必須フィールド不足でコンパイル不能になる破壊的変更と
/// なった（AGENTS.md「公開 API・エラー契約の互換性（P1）」）。今後のフィールド追加が
/// 同様の破壊を再発させないよう、外部クレートからの構造体リテラル構築を非対応にする。
/// フィールドはカプセル化のため `pub(crate)` とし（クレート外からの直読み・直書きは
/// 不可。コード内では [`BoundStatement::table`] 等のアクセサーメソッドを経由する）、
/// クレート外からの構築は [`BoundStatement::new`]（既存フィールド相当の引数を取り、
/// `mode` は既定値 [`crate::sql::mode::resolve_mode`]`(None, None)` で構築する）と
/// [`BoundStatement::with_mode`]（TASK-161 で追加した `mode` を設定するビルダー的
/// メソッド）を経由する。本構造体は通常 [`bind_with_session`] の戻り値として取得
/// するが、上記 constructor 経由でも構築できる（PR #188 レビュー指摘対応: 破壊的
/// 変更の移行経路を用意しつつ、直接のフィールド読み書きは許可しない）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BoundStatement {
    pub(crate) table: String,
    pub(crate) projection: Vec<ProjectedColumn>,
    pub(crate) scalar_filters: Vec<ScalarEq>,
    /// `WHERE` 句に `visible()` 呼び出し形が含まれていたか（SQL-3・RLS-7 参照）。
    /// **実行側の RLS 適用はこの値の有無に依存しない**（`exec.rs` は無条件に
    /// `PolicyContext::is_visible` を適用する）。本フィールドは束縛結果の可観測性
    /// （テスト・診断）のためだけに保持する。
    pub(crate) rls_predicate_present: bool,
    pub(crate) ranking: Ranking,
    pub(crate) limit: usize,
    /// 取得モードの優先順位解決結果（TASK-161・SQL-12）。クエリ句 `USING MODE`
    /// （[`ValidatedStatement::search_mode`](crate::sql::allowlist::ValidatedStatement)）
    /// とセッション変数（呼び出し元 `core.rs::EngineCore::execute_sql_in_session` が
    /// 渡す [`crate::sql::mode::SessionState`]）から [`crate::sql::mode::resolve_mode`]
    /// が決定する。カーネル選択（`dispatch.rs`）の入力には含めない（`precision` の
    /// 実行契約は TASK-162・SEARCH-9 の管轄。`sql::exec` が本フィールドを見て
    /// 実行可否を判定する）。
    pub(crate) mode: crate::sql::mode::ResolvedMode,
    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。`allowlist` が
    /// 検証済みの [`EvaluationOrder`] をそのまま素通しする（意味論的な束縛の必要は
    /// ない。実行意味論の解釈は [`crate::sql::plan::ExecutionPlan`] の管轄）。
    pub(crate) evaluation_order: EvaluationOrder,
}

impl BoundStatement {
    /// クレート外から構築するための constructor（TASK-161 で `mode` フィールドを
    /// 追加する以前の既存フィールド相当の引数を取る）。`mode` は
    /// `resolve_mode(None, None)`（クエリ句・セッション変数いずれも未指定時の既定値、
    /// `recall`・[`crate::sql::mode::ModeSource::Default`]）で構築され、必要なら
    /// [`Self::with_mode`] を続けて呼ぶ。フィールドが `pub(crate)` のため、
    /// クレート外から `BoundStatement` を得るにはこの constructor か
    /// [`bind_with_session`] の戻り値を経由するしかない。
    pub fn new(
        table: String,
        projection: Vec<ProjectedColumn>,
        scalar_filters: Vec<ScalarEq>,
        rls_predicate_present: bool,
        ranking: Ranking,
        limit: usize,
        evaluation_order: EvaluationOrder,
    ) -> Self {
        Self {
            table,
            projection,
            scalar_filters,
            rls_predicate_present,
            ranking,
            limit,
            mode: crate::sql::mode::resolve_mode(None, None),
            evaluation_order,
        }
    }

    /// `mode`（TASK-161・SQL-12）を設定したコピーを返すビルダー的メソッド。
    /// [`Self::new`] と組み合わせて `mode` を含む値を外部から構築する。
    #[must_use]
    pub fn with_mode(mut self, mode: crate::sql::mode::ResolvedMode) -> Self {
        self.mode = mode;
        self
    }

    /// 束縛対象のテーブル名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 投影対象の列一覧（`Row::id` 疑似列を含みうる）。
    pub fn projection(&self) -> &[ProjectedColumn] {
        &self.projection
    }

    /// SCALAR 段で適用する等価条件一覧。
    pub fn scalar_filters(&self) -> &[ScalarEq] {
        &self.scalar_filters
    }

    /// `WHERE` 句に `visible()` 呼び出し形が含まれていたか（SQL-3・RLS-7 参照）。
    /// **実行側の RLS 適用はこの値の有無に依存しない**（可観測性のためだけの値）。
    pub fn rls_predicate_present(&self) -> bool {
        self.rls_predicate_present
    }

    /// DISTANCE 段のランキング方式。
    pub fn ranking(&self) -> &Ranking {
        &self.ranking
    }

    /// `LIMIT` 句の値。
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// 取得モードの優先順位解決結果（TASK-161・SQL-12）。
    pub fn mode(&self) -> crate::sql::mode::ResolvedMode {
        self.mode
    }

    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。
    pub fn evaluation_order(&self) -> EvaluationOrder {
        self.evaluation_order
    }
}

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::{self, SearchMode};

/// `[f1,f2,...]` 形式のベクトルリテラルを解析する（SQL-1）。
///
/// 検証順序: (1) 生バイト長が [`MAX_VECTOR_LITERAL_BYTES`] を超えないこと
/// （超過は [`SqlSurfaceError::PayloadTooLarge`]。カンマ分割・`Vec<f32>` 確保より前に
/// 行う）。(2) `[`〜`]` で囲まれていること。(3) 各要素が `f32` としてパース可能かつ
/// 有限であること。(4) 要素数が `expected_dim` と一致すること。(2)〜(4) の違反は
/// [`SqlSurfaceError::InvalidInput`]。
pub fn parse_vector_literal(literal: &str, expected_dim: u32) -> Result<Vec<f32>, SqlSurfaceError> {
    if literal.len() > MAX_VECTOR_LITERAL_BYTES {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "vector literal length {} exceeds limit {MAX_VECTOR_LITERAL_BYTES}",
            literal.len()
        )));
    }

    let trimmed = literal.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| {
            SqlSurfaceError::invalid_input("vector literal must be of the form [f1,f2,...]")
        })?;

    let mut values: Vec<f32> = Vec::new();
    if !inner.trim().is_empty() {
        for part in inner.split(',') {
            let part = part.trim();
            let v: f32 = part.parse().map_err(|_| {
                SqlSurfaceError::invalid_input(format!(
                    "vector literal element is not a number: {part:?}"
                ))
            })?;
            if !v.is_finite() {
                return Err(SqlSurfaceError::invalid_input(
                    "vector literal element must be finite (NaN/Inf are not allowed)",
                ));
            }
            values.push(v);
        }
    }

    let dim = u32::try_from(values.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large(format!(
            "vector literal element count {} exceeds representable range",
            values.len()
        ))
    })?;
    if dim != expected_dim {
        return Err(SqlSurfaceError::invalid_input(format!(
            "vector literal dimension mismatch: expected {expected_dim}, got {dim}"
        )));
    }

    Ok(values)
}

/// スキーマの唯一の `VECTOR` 列（インデックス・宣言次元）を返す。`VECTOR` 列を
/// 持たないテーブルは束縛不能（`catalog.rs::validate_schema` が「`VECTOR` 列は
/// 高々 1 つ」を DDL 時点で強制済みのため、複数該当は構造上起こらない）。
fn vector_column(schema: &TableSchema) -> Result<(usize, u32), SqlSurfaceError> {
    schema
        .columns
        .iter()
        .enumerate()
        .find_map(|(idx, c)| match c.ty {
            ColumnType::Vector(dim) => Some((idx, dim)),
            ColumnType::Text => None,
        })
        .ok_or_else(|| SqlSurfaceError::invalid_input("table has no VECTOR column"))
}

/// `name` に一致する `Text` 列のインデックスを返す（`id` 疑似列は対象外）。
fn text_column_index(schema: &TableSchema, name: &str) -> Result<usize, SqlSurfaceError> {
    schema
        .columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))
        .and_then(|idx| {
            let column = schema
                .columns
                .get(idx)
                .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
            match column.ty {
                ColumnType::Text => Ok(idx),
                ColumnType::Vector(_) => Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} is not a TEXT column"
                ))),
            }
        })
}

/// `ORDER BY` 式（[`OrderByForm`]）を [`Ranking`] へ束縛する。
fn bind_ranking(order_by: &OrderByForm, schema: &TableSchema) -> Result<Ranking, SqlSurfaceError> {
    let (vec_idx, vec_dim) = vector_column(schema)?;
    match order_by {
        OrderByForm::Distance { column, literal } => {
            let column_idx = schema
                .columns
                .iter()
                .position(|c| &c.name == column)
                .ok_or_else(|| {
                    SqlSurfaceError::invalid_input(format!("unknown column: {column}"))
                })?;
            if column_idx != vec_idx {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {column:?} is not the table's VECTOR column"
                )));
            }
            let query = parse_vector_literal(literal, vec_dim)?;
            Ok(Ranking::Distance { query })
        }
        OrderByForm::FunctionCall { args, .. } => {
            // 4 引数形（<vec列>, '<vec リテラル>', <text列>, '<query text>'）のみ
            // 実行可能。2 引数形は allowlist（TASK-74）が構造としては受理するが、
            // 密側クエリベクトルをテキストから導出する経路を engine は持たないため
            // 実行不能として拒否する（advisor 方針: 既存 2 引数形の受理自体は
            // 変更しない。実行不能の判定はこの束縛層に閉じる）。
            let (vec_col, vec_literal, text_col, query_text) = match args.as_slice() {
                [FunctionArg::Ident(vec_col), FunctionArg::StringLiteral(vec_literal), FunctionArg::Ident(text_col), FunctionArg::StringLiteral(query_text)] => {
                    (vec_col, vec_literal, text_col, query_text)
                }
                [FunctionArg::Ident(_), FunctionArg::StringLiteral(_)] => {
                    return Err(SqlSurfaceError::invalid_input(
                        "hybrid ORDER BY function requires 4 arguments (vector column, vector literal, text column, query text); the 2-argument form is not executable",
                    ));
                }
                _ => {
                    return Err(SqlSurfaceError::invalid_input(
                        "unsupported hybrid ORDER BY function argument shape",
                    ));
                }
            };
            let vec_col_idx = schema
                .columns
                .iter()
                .position(|c| c.name == *vec_col)
                .ok_or_else(|| {
                    SqlSurfaceError::invalid_input(format!("unknown column: {vec_col}"))
                })?;
            if vec_col_idx != vec_idx {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {vec_col:?} is not the table's VECTOR column"
                )));
            }
            let query = parse_vector_literal(vec_literal, vec_dim)?;
            let text_column_index = text_column_index(schema, text_col)?;
            Ok(Ranking::Hybrid {
                query,
                text_column_index,
                query_text: query_text.clone(),
            })
        }
    }
}

/// [`ValidatedStatement`] を `schema` と照合して [`BoundStatement`] へ束縛する
/// （TASK-75 の公開 API）。`schema` は呼び出し元（`core.rs::EngineCore::execute_sql`）が
/// `Storage::get_table_schema` で取得済みのものを渡す。セッション変数を持たない
/// エントリポイント向けの後方互換 API で、[`bind_with_session`]（TASK-161）へ
/// `session_mode: None` で委譲する。
pub fn bind(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
) -> Result<BoundStatement, SqlSurfaceError> {
    bind_with_session(stmt, schema, None)
}

/// [`ValidatedStatement`] を `schema` と `session_mode`（呼び出し元の
/// [`crate::sql::mode::SessionState::search_mode`]）と照合して [`BoundStatement`] へ
/// 束縛する（TASK-161 の公開 API）。`stmt.search_mode`（クエリ句 `USING MODE` の生
/// リテラル）を [`SearchMode::parse_literal`] で検証し、`session_mode` とあわせて
/// [`mode::resolve_mode`] で優先順位解決する（クエリ句 > セッション変数 > 既定）。
/// クエリ句のリテラルが `recall`／`precision` 以外の場合は
/// [`SqlSurfaceError::InvalidInput`]（`22000`。構文上受理された値が不正）で
/// fail-closed に拒否し、黙って既定モードへ落とさない。
pub fn bind_with_session(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    session_mode: Option<SearchMode>,
) -> Result<BoundStatement, SqlSurfaceError> {
    let query_mode = match &stmt.search_mode {
        Some(literal) => Some(SearchMode::parse_literal(literal)?),
        None => None,
    };
    let resolved_mode = mode::resolve_mode(query_mode, session_mode);

    let projection = match &stmt.projection {
        Projection::All => {
            let mut cols = Vec::with_capacity(schema.columns.len() + 1);
            cols.push(ProjectedColumn::Id);
            for (index, column) in schema.columns.iter().enumerate() {
                cols.push(ProjectedColumn::Column {
                    index,
                    name: column.name.clone(),
                });
            }
            cols
        }
        Projection::Columns(names) => {
            let mut cols = Vec::with_capacity(names.len());
            for name in names {
                // カタログ上の実カラムを疑似列 `id` より優先して照合する（Issue #56
                // レビュー指摘対応: 以前は `name == "id"` を先に判定していたため、
                // スキーマが `id` という実カラムを持っていても常に行キー疑似列へ
                // マップされ、実カラムの値を `SELECT id` で取得する経路がなかった）。
                if let Some(index) = schema.columns.iter().position(|c| &c.name == name) {
                    cols.push(ProjectedColumn::Column {
                        index,
                        name: name.clone(),
                    });
                    continue;
                }
                if name == "id" {
                    cols.push(ProjectedColumn::Id);
                    continue;
                }
                return Err(SqlSurfaceError::invalid_input(format!(
                    "unknown column: {name}"
                )));
            }
            cols
        }
    };

    let mut scalar_filters = Vec::with_capacity(stmt.where_predicates.len());
    let mut rls_predicate_present = false;
    for predicate in &stmt.where_predicates {
        match predicate {
            WherePredicate::Equality { column, value } => {
                let index = text_column_index(schema, column)?;
                scalar_filters.push(ScalarEq {
                    column_index: index,
                    value: value.clone(),
                });
            }
            WherePredicate::PredicateCall { .. } => {
                // allowlist が許可する述語呼び出し形は `visible()` のみ
                // （`is_allowed_where_predicate_name`）。名前の再検証はしない
                // （許可リスト層の責務。ここでは可観測性のためのフラグのみ立てる）。
                rls_predicate_present = true;
            }
        }
    }

    let ranking = bind_ranking(&stmt.order_by, schema)?;

    let limit = usize::try_from(stmt.limit).map_err(|_| {
        SqlSurfaceError::invalid_input(format!("malformed LIMIT value: {}", stmt.limit))
    })?;
    if limit == 0 || limit > crate::core::MAX_SEARCH_K {
        return Err(SqlSurfaceError::invalid_input(format!(
            "LIMIT {limit} out of range (must be 1..={})",
            crate::core::MAX_SEARCH_K
        )));
    }

    Ok(BoundStatement {
        table: stmt.table_name.clone(),
        projection,
        scalar_filters,
        rls_predicate_present,
        ranking,
        limit,
        mode: resolved_mode,
        evaluation_order: stmt.evaluation_order,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::sql::allowlist::validate_statement;
    use std::collections::HashSet;

    struct FakeCatalog {
        tables: HashSet<&'static str>,
    }
    impl crate::sql::allowlist::TableLookup for FakeCatalog {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(self.tables.contains(name))
        }
    }

    fn docs_schema() -> TableSchema {
        TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    fn bind_sql(sql: &str) -> Result<BoundStatement, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = validate_statement(sql, &lookup).expect("must pass allowlist");
        bind(&stmt, &docs_schema())
    }

    // --- parse_vector_literal --------------------------------------------------

    #[test]
    fn parse_vector_literal_accepts_matching_dim() {
        let v = parse_vector_literal("[1.0,2.0,3.0]", 3).expect("valid literal");
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn parse_vector_literal_rejects_dim_mismatch() {
        let err = parse_vector_literal("[1.0,2.0]", 3).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_vector_literal_rejects_non_finite() {
        let err = parse_vector_literal("[1.0,nan,3.0]", 3).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
        let err = parse_vector_literal("[1.0,inf,3.0]", 3).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_vector_literal_rejects_malformed_brackets() {
        assert_eq!(
            parse_vector_literal("1.0,2.0,3.0", 3)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        assert_eq!(
            parse_vector_literal("[1.0,2.0,3.0", 3)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_vector_literal_rejects_oversized_payload() {
        let huge = format!("[{}]", "1.0,".repeat(20_000));
        let err = parse_vector_literal(&huge, 20_000).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn parse_vector_literal_accepts_boundary_length() {
        // ちょうど MAX_VECTOR_LITERAL_BYTES に収まる場合は PayloadTooLarge にならない
        // （不正形式であっても 22000 になる＝バイト長検証自体は通過する）。
        let padding = "0".repeat(MAX_VECTOR_LITERAL_BYTES - 2);
        let literal = format!("[{padding}"); // 閉じ括弧なしで意図的に不正形状にする
        assert_eq!(literal.len(), MAX_VECTOR_LITERAL_BYTES - 1);
        let err = parse_vector_literal(&literal, 1).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: C1（純粋 Top-k） --------------------------------------------------

    #[test]
    fn binds_distance_form_to_ranking_distance() {
        let bound =
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .expect("bind should succeed");
        assert_eq!(bound.table, "documents");
        assert_eq!(bound.limit, 5);
        assert!(matches!(bound.ranking, Ranking::Distance { .. }));
        assert!(bound.scalar_filters.is_empty());
        assert!(!bound.rls_predicate_present);
    }

    #[test]
    fn binds_projection_all_to_id_plus_schema_columns() {
        let bound =
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .expect("bind should succeed");
        assert_eq!(
            bound.projection,
            vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 0,
                    name: "embedding".to_string()
                },
                ProjectedColumn::Column {
                    index: 1,
                    name: "body".to_string()
                },
                ProjectedColumn::Column {
                    index: 2,
                    name: "lang".to_string()
                },
            ]
        );
    }

    #[test]
    fn binds_explicit_projection_including_id_pseudo_column() {
        let bound = bind_sql(
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("bind should succeed");
        assert_eq!(
            bound.projection,
            vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 1,
                    name: "body".to_string()
                },
            ]
        );
    }

    #[test]
    fn binds_real_id_column_over_pseudo_column_when_schema_declares_it() {
        // Issue #56 レビュー指摘対応（P1/Medium: User id column is shadowed）:
        // カタログ上に実カラム `id`（`ColumnType::Text`）が存在する場合、
        // `SELECT id` は行キー疑似列ではなくその実カラムへ束縛されなければならない。
        let schema = TableSchema::new(
            "labeled_docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        );
        let lookup = FakeCatalog {
            tables: ["labeled_docs"].into_iter().collect(),
        };
        let stmt = validate_statement(
            "SELECT id FROM labeled_docs ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
            &lookup,
        )
        .expect("must pass allowlist");
        let bound = bind(&stmt, &schema).expect("bind should succeed");
        assert_eq!(
            bound.projection,
            vec![ProjectedColumn::Column {
                index: 1,
                name: "id".to_string()
            }]
        );
    }

    #[test]
    fn rejects_unknown_projected_column() {
        let err =
            bind_sql("SELECT nope FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_distance_order_by_column_not_the_vector_column() {
        let err =
            bind_sql("SELECT * FROM documents ORDER BY body <=> '[0.1]' LIMIT 5").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_limit_zero_and_over_max() {
        assert!(
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 1")
                .is_ok()
        );
        let err = bind_sql(&format!(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT {}",
            crate::core::MAX_SEARCH_K + 1
        ))
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: C2（スカラー条件付き） ----------------------------------------------

    #[test]
    fn binds_scalar_equality_filter() {
        let bound = bind_sql(
            "SELECT * FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("bind should succeed");
        assert_eq!(
            bound.scalar_filters,
            vec![ScalarEq {
                column_index: 2,
                value: "ja".to_string(),
            }]
        );
    }

    #[test]
    fn rejects_scalar_equality_on_vector_column() {
        let err = bind_sql(
            "SELECT * FROM documents WHERE embedding = 'x' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_scalar_equality_on_unknown_column() {
        let err = bind_sql(
            "SELECT * FROM documents WHERE nope = 'x' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: C3（RLS。`visible()` の有無だけを観測する） --------------------------

    #[test]
    fn binds_visible_predicate_call_sets_flag_only() {
        let bound = bind_sql(
            "SELECT * FROM documents WHERE visible() ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("bind should succeed");
        assert!(bound.rls_predicate_present);
        assert!(bound.scalar_filters.is_empty());
    }

    // --- bind: C4（ハイブリッド） --------------------------------------------------

    #[test]
    fn binds_hybrid_four_arg_form_to_ranking_hybrid() {
        let bound = bind_sql(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, '[0.1,0.2,0.3]', body, 'query text') LIMIT 5",
        )
        .expect("bind should succeed");
        match bound.ranking {
            Ranking::Hybrid {
                query,
                text_column_index,
                query_text,
            } => {
                assert_eq!(query, vec![0.1, 0.2, 0.3]);
                assert_eq!(text_column_index, 1);
                assert_eq!(query_text, "query text");
            }
            other => panic!("expected Ranking::Hybrid, got {other:?}"),
        }
    }

    #[test]
    fn binds_hybrid_alternate_name_four_arg_form() {
        let bound = bind_sql(
            "SELECT * FROM documents ORDER BY HYBRID(embedding, '[0.1,0.2,0.3]', body, 'query text') LIMIT 5",
        )
        .expect("bind should succeed");
        assert!(matches!(bound.ranking, Ranking::Hybrid { .. }));
    }

    #[test]
    fn rejects_hybrid_two_arg_form_as_not_executable() {
        let err = bind_sql(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'query text') LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: evaluation_order 素通し（TASK-76・SQL-7） -------------------------

    #[test]
    fn binds_default_evaluation_order_when_hint_order_absent() {
        let bound =
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .expect("bind should succeed");
        assert_eq!(bound.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn binds_explicit_evaluation_order_from_hint_order() {
        let bound = bind_sql(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect("bind should succeed");
        assert_eq!(
            bound.evaluation_order.stages(),
            [
                crate::sql::plan::Stage::Distance,
                crate::sql::plan::Stage::Scalar,
                crate::sql::plan::Stage::Rls,
            ]
        );
    }

    #[test]
    fn rejects_hybrid_four_arg_form_with_non_text_second_column() {
        let err = bind_sql(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, '[0.1,0.2,0.3]', embedding, 'q') LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_with_session: 取得モードの優先順位解決（TASK-161・SQL-12） -------------

    fn bind_sql_with_session(
        sql: &str,
        session_mode: Option<SearchMode>,
    ) -> Result<BoundStatement, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = validate_statement(sql, &lookup).expect("must pass allowlist");
        bind_with_session(&stmt, &docs_schema(), session_mode)
    }

    const SELECT_NO_MODE: &str =
        "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";

    #[test]
    fn bind_defaults_to_recall_when_no_clause_and_no_session() {
        let bound = bind_sql_with_session(SELECT_NO_MODE, None).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Recall);
        assert_eq!(bound.mode.source, mode::ModeSource::Default);
    }

    #[test]
    fn bind_uses_session_variable_when_no_query_clause() {
        let bound = bind_sql_with_session(SELECT_NO_MODE, Some(SearchMode::Precision))
            .expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Precision);
        assert_eq!(bound.mode.source, mode::ModeSource::SessionVariable);
    }

    #[test]
    fn bind_query_clause_wins_over_session_variable() {
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE 'recall'";
        let bound =
            bind_sql_with_session(sql, Some(SearchMode::Precision)).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Recall);
        assert_eq!(bound.mode.source, mode::ModeSource::QueryClause);
    }

    #[test]
    fn bind_query_clause_alone_resolves_without_session() {
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE 'precision'";
        let bound = bind_sql_with_session(sql, None).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Precision);
        assert_eq!(bound.mode.source, mode::ModeSource::QueryClause);
    }

    #[test]
    fn bind_rejects_unknown_query_clause_mode_value() {
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE 'fuzzy'";
        let err = bind_sql_with_session(sql, None).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn plain_bind_resolves_default_mode_without_session_argument() {
        // 既存 `bind`（後方互換 API）は `bind_with_session(.., None)` へ委譲するため、
        // 句・セッションいずれも無指定なら既定 `recall` を解決する。
        let bound = bind_sql(SELECT_NO_MODE).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Recall);
        assert_eq!(bound.mode.source, mode::ModeSource::Default);
    }
}
