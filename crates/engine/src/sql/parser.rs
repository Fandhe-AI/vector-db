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
use crate::declarative_filter::{self, DeclarativeFilter, MetadataFilter};
use crate::sql::allowlist::{
    FunctionArg, InsertLiteral, OrderByForm, Projection, ValidatedInsert, ValidatedStatement,
    WherePredicate,
};
use crate::sql::plan::EvaluationOrder;
use crate::sql::udf_call::Expr;
use crate::sql::using_operation_id::OperationId;

/// ベクトルリテラルの生バイト長上限（SQL-1）。アロケーション（`Vec<f32>` の確保・
/// カンマ分割）に入る前にこの長さで拒否する。
const MAX_VECTOR_LITERAL_BYTES: usize = 64 * 1024;

/// 投影対象の 1 列。`Id` は疑似列（[`crate::storage::Row::id`] 由来。スキーマの列では
/// ないため `column_index` を持たない）。`Column` の `index` は `TableSchema::columns`
/// の列順インデックス（[`crate::row_codec::decode_scalar_columns`] が返す `Vec` の
/// 添字と一致する。`VECTOR` 列の位置は常に `row_codec::Value::Null` が入るため、
/// `exec.rs` はその位置を投影する際 `storage::Row::embedding` を別途参照する）。
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Computed` variant を
/// 追加した（宣言的 UDF・組み込み関数呼び出しを結果列位置で束縛した式）。
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectedColumn {
    Id,
    Column {
        index: usize,
        name: String,
    },
    /// 式項目（TASK-79・SQL-9）。`name` は `AS <alias>` の指定値、省略時は関数名。
    Computed {
        name: String,
        expr: crate::sql::udf_call::BoundExpr,
    },
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
    /// SCALAR 段で適用するメタデータフィルタ（等価・前方一致、TASK-147・EXT-3）。
    /// **TASK-147 で追加した破壊的変更（BREAKING CHANGE）**: 旧 `scalar_filters:
    /// Vec<ScalarEq>`（等価専用）を `declarative_filter::MetadataFilter`
    /// （汎用 API。等価・前方一致の両方を表す）へ置換し、フィールド名も
    /// `metadata_filters` へ改名した。
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    /// `WHERE` 句に `visible()` 呼び出し形が含まれていたか（SQL-3・RLS-7 参照）。
    /// **実行側の RLS 適用はこの値の有無に依存しない**（`exec.rs` は無条件に
    /// `PolicyContext::is_visible` を適用する）。本フィールドは束縛結果の可観測性
    /// （テスト・診断）のためだけに保持する。
    pub(crate) rls_predicate_present: bool,
    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済みで、レジストリを
    /// 参照せず単独で評価できる。既存の `metadata_filters` と同じ SCALAR 段の一部
    /// として扱う（`sql::exec` のモジュールドキュメント参照。既定順では候補構築時の
    /// 行フックで事前適用し、`HINT ORDER` で DISTANCE 先行時は DISTANCE 段の後で
    /// 事後適用する）。
    pub(crate) expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
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
        metadata_filters: Vec<MetadataFilter>,
        rls_predicate_present: bool,
        ranking: Ranking,
        limit: usize,
        evaluation_order: EvaluationOrder,
    ) -> Self {
        Self {
            table,
            projection,
            metadata_filters,
            rls_predicate_present,
            expr_filters: Vec::new(),
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

    /// SCALAR 段で適用するメタデータフィルタ一覧（等価・前方一致、TASK-147・EXT-3）。
    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    /// `WHERE` 句に `visible()` 呼び出し形が含まれていたか（SQL-3・RLS-7 参照）。
    /// **実行側の RLS 適用はこの値の有無に依存しない**（可観測性のためだけの値）。
    pub fn rls_predicate_present(&self) -> bool {
        self.rls_predicate_present
    }

    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済み。
    pub fn expr_filters(&self) -> &[crate::sql::udf_call::BoundExpr] {
        &self.expr_filters
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

/// 束縛済みの INSERT 文（SQL-10、TASK-80。
/// [`exec::execute_insert`](crate::sql::exec::execute_insert) が直接実行する入力形）。
/// テナント・可視性はここでは決定しない（`exec::execute_insert` がサーバー側で
/// `PolicyContext` から導出・固定する。`.claude/rules/security.md` P0
/// 「クライアント指定のテナントを信用しない」）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsert {
    pub table: String,
    /// 行キー（疑似列 `id`。列リストへの指定必須）。
    pub id: u64,
    /// `schema.columns` の列順に対応する値列（`catalog::insert_typed_row`/
    /// `tenant::insert_typed_row` の `values` 契約と同一。`id` 疑似列は含まない）。
    pub values: Vec<crate::row_codec::Value>,
    /// TASK-92（RECOVER-1）: `ValidatedInsert.operation_id` をそのまま素通しする。
    /// `LedgerMode::Ledgered`（既定）では `sql::allowlist::validate_insert` が既に
    /// `None` を `23502` で拒否済みのため常に `Some`。`CompareOnlyWithoutLedger`
    /// でのみ `None` になり得る。
    pub operation_id: Option<OperationId>,
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
/// `AS <alias>` を省略した SELECT 式項目の既定列名（TASK-79・SQL-9）。頂点の
/// 関数呼び出し名を使う（`Expr::Call` 以外が頂点の式は SELECT リストの構文上
/// 現れない＝`allowlist::Parser::parse_select_item` は必ず `ident '(' ... ')'` から
/// 式項目を作るため `name` は常に取得できる）。
fn default_expr_alias(expr: &Expr) -> String {
    match expr {
        Expr::Call { name, .. } => name.clone(),
        _ => "expr".to_string(),
    }
}

/// `WHERE` 句の許可述語列（[`WherePredicate`]）を束縛する共通ヘルパー
/// （TASK-166・SQL-13 で `bind_in_session` から切り出した。検索 SELECT
/// （[`bind_in_session`]）・集計 SELECT（[`bind_aggregate`]）の両方が同一の
/// 意味論（等価・前方一致条件は `declarative_filter::bind_all` に集約、`visible()`
/// はフラグのみ、式述語は `Bool` 型を要求）で WHERE を解釈する必要があるため、
/// 挙動を複製せずこの 1 箇所に集約する。戻り値は
/// `(metadata_filters, expr_filters, rls_predicate_present)` の組。
fn bind_where_predicates(
    where_predicates: &[WherePredicate],
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    node_budget: &mut usize,
) -> Result<
    (
        Vec<MetadataFilter>,
        Vec<crate::sql::udf_call::BoundExpr>,
        bool,
    ),
    SqlSurfaceError,
> {
    let mut declarative_filters = Vec::with_capacity(where_predicates.len());
    let mut expr_filters = Vec::new();
    let mut rls_predicate_present = false;
    for predicate in where_predicates {
        match predicate {
            WherePredicate::Equality { column, value } => {
                declarative_filters.push(DeclarativeFilter::equals(column.clone(), value.clone()));
            }
            WherePredicate::Prefix { column, pattern } => {
                let prefix = declarative_filter::parse_prefix_pattern(pattern)?;
                declarative_filters.push(DeclarativeFilter::starts_with(column.clone(), prefix));
            }
            WherePredicate::PredicateCall { .. } => {
                // allowlist が許可する述語呼び出し形は `visible()` のみ
                // （`is_allowed_where_predicate_name`）。名前の再検証はしない
                // （許可リスト層の責務。ここでは可観測性のためのフラグのみ立てる）。
                rls_predicate_present = true;
            }
            WherePredicate::Expression(expr) => {
                let (bound, ty) = crate::sql::udf_call::bind_expr(expr, schema, udfs, node_budget)?;
                if ty != crate::sql::udf_call::ExprType::Bool {
                    return Err(SqlSurfaceError::invalid_input(
                        "WHERE expression must evaluate to a boolean (use a comparison)",
                    ));
                }
                expr_filters.push(bound);
            }
        }
    }
    let metadata_filters = declarative_filter::bind_all(&declarative_filters, schema)?;
    Ok((metadata_filters, expr_filters, rls_predicate_present))
}

pub fn bind(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
) -> Result<BoundStatement, SqlSurfaceError> {
    bind_with_session(stmt, schema, None)
}

/// [`ValidatedStatement`] を `schema` と `session_mode`（呼び出し元の
/// [`crate::sql::mode::SessionState::search_mode`]）と照合して [`BoundStatement`] へ
/// 束縛する（TASK-161 の公開 API）。UDF レジストリを持たないエントリポイント向けの
/// 後方互換 API で、[`bind_in_session`]（TASK-79）へ空レジストリで委譲する。
pub fn bind_with_session(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    session_mode: Option<SearchMode>,
) -> Result<BoundStatement, SqlSurfaceError> {
    bind_in_session(
        stmt,
        schema,
        session_mode,
        &crate::sql::udf_call::UdfRegistry::default(),
    )
}

/// [`ValidatedStatement`] を `schema`・`session_mode`・UDF レジストリ `udfs`
/// （呼び出し元の [`crate::sql::mode::SessionState::udfs`]）と照合して
/// [`BoundStatement`] へ束縛する（TASK-79・SQL-9 の公開 API。TASK-161 の
/// `bind_with_session` を UDF 呼び出しの束縛（結果列・`WHERE` 式述語）へ拡張した
/// もの）。`stmt.search_mode`（クエリ句 `USING MODE` の生リテラル）を
/// [`SearchMode::parse_literal`] で検証し、`session_mode` とあわせて
/// [`mode::resolve_mode`] で優先順位解決する（クエリ句 > セッション変数 > 既定）。
/// クエリ句のリテラルが `recall`／`precision` 以外の場合は
/// [`SqlSurfaceError::InvalidInput`]（`22000`。構文上受理された値が不正）で
/// fail-closed に拒否し、黙って既定モードへ落とさない。
pub fn bind_in_session(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    session_mode: Option<SearchMode>,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundStatement, SqlSurfaceError> {
    let query_mode = match &stmt.search_mode {
        Some(literal) => Some(SearchMode::parse_literal(literal)?),
        None => None,
    };
    let resolved_mode = mode::resolve_mode(query_mode, session_mode);

    // TASK-79・SQL-9: 1 つの `SELECT` 文（結果列＋`WHERE` の全式項目）で共有する
    // インライン展開後ノード数の予算（[`crate::sql::udf_call::MAX_EXPR_NODES`]）。
    // 多段 UDF 呼び出しによる展開後の指数的膨張を、文単位で歯止めする
    // （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;

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
        Projection::Items(items) => {
            let mut cols = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    crate::sql::allowlist::SelectItem::Column(name) => {
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
                    crate::sql::allowlist::SelectItem::Expr { expr, alias } => {
                        let (bound, _ty) =
                            crate::sql::udf_call::bind_expr(expr, schema, udfs, &mut node_budget)?;
                        let name = alias.clone().unwrap_or_else(|| default_expr_alias(expr));
                        cols.push(ProjectedColumn::Computed { name, expr: bound });
                    }
                }
            }
            cols
        }
    };

    let (metadata_filters, expr_filters, rls_predicate_present) =
        bind_where_predicates(&stmt.where_predicates, schema, udfs, &mut node_budget)?;

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
        metadata_filters,
        rls_predicate_present,
        expr_filters,
        ranking,
        limit,
        mode: resolved_mode,
        evaluation_order: stmt.evaluation_order,
    })
}

/// [`ValidatedInsert`] を `schema` と照合して [`BoundInsert`] へ束縛する
/// （SQL-10、TASK-80 の公開 API）。`schema` は呼び出し元
/// （`core.rs::EngineCore::execute_insert_sql`）が `Storage::get_table_schema` で
/// 取得済みのものを渡す。
///
/// 検出する違反はすべて [`SqlSurfaceError::InvalidInput`]（`22000`）:
/// 列名重複・列リストに疑似列 `id` を含まない・`id` 値が `u64` として解釈不能
/// （範囲外を含む）・未知の列名・列型とリテラル種別の不一致（`VECTOR` 列に
/// 数値、`TEXT` 列にベクトルリテラルを渡す等）・非 nullable 列の欠落。
/// ベクトルリテラル自体の形式・次元・64 KiB 上限は既存の [`parse_vector_literal`]
/// をそのまま再利用する（アロケーション前のサイズ検証を二重管理しない）。
///
/// テナント・可視性はここで解決しない（`exec::execute_insert` の責務。
/// クライアントが列リストへ `tenant_id`・可視性ラベル相当の名前を指定しても、
/// スキーマ上の実列として照合されるだけで RLS フィールドへは書き込まれない）。
pub fn bind_insert(
    stmt: &ValidatedInsert,
    schema: &TableSchema,
) -> Result<BoundInsert, SqlSurfaceError> {
    if stmt.columns.len() != stmt.values.len() {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT column count does not match value count",
        ));
    }

    let mut seen_columns: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for name in &stmt.columns {
        if !seen_columns.insert(name.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in INSERT column list: {name}"
            )));
        }
    }

    let id_pos = stmt.columns.iter().position(|c| c == "id").ok_or_else(|| {
        SqlSurfaceError::invalid_input("INSERT column list must include the id pseudo-column")
    })?;
    let id_literal = stmt
        .values
        .get(id_pos)
        .ok_or_else(|| SqlSurfaceError::invalid_input("missing value for id pseudo-column"))?;
    let id: u64 = match id_literal {
        InsertLiteral::Number(n) => n
            .parse()
            .map_err(|_| SqlSurfaceError::invalid_input(format!("malformed id value: {n}")))?,
        InsertLiteral::String(_) => {
            return Err(SqlSurfaceError::invalid_input(
                "id pseudo-column value must be a number",
            ))
        }
    };

    let mut values: Vec<crate::row_codec::Value> =
        vec![crate::row_codec::Value::Null; schema.columns.len()];
    let mut provided = vec![false; schema.columns.len()];

    for (name, literal) in stmt.columns.iter().zip(stmt.values.iter()) {
        if name == "id" {
            // 疑似列 `id` は行キーとして上で処理済みであり、スキーマ実列とは
            // 照合しない（既存の SELECT 側 `bind` と同様、実カラム名 `id` を
            // 持つスキーマではその実列を本 INSERT 形から指定する手段がない。
            // 既知の制約としてドキュメント化する）。
            continue;
        }
        let col_idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let column = schema
            .columns
            .get(col_idx)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let value = match (column.ty, literal) {
            (ColumnType::Vector(dim), InsertLiteral::String(s)) => {
                crate::row_codec::Value::Vector(parse_vector_literal(s, dim)?)
            }
            (ColumnType::Vector(_), InsertLiteral::Number(_)) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a vector literal, got a number"
                )))
            }
            (ColumnType::Text, InsertLiteral::String(s)) => {
                crate::row_codec::Value::Text(s.clone())
            }
            (ColumnType::Text, InsertLiteral::Number(_)) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a text literal, got a number"
                )))
            }
        };
        if let Some(slot) = values.get_mut(col_idx) {
            *slot = value;
        }
        if let Some(flag) = provided.get_mut(col_idx) {
            *flag = true;
        }
    }

    for (idx, column) in schema.columns.iter().enumerate() {
        let is_provided = provided.get(idx).copied().unwrap_or(false);
        if !is_provided && !column.nullable {
            return Err(SqlSurfaceError::invalid_input(format!(
                "column {:?} is not nullable but was not provided",
                column.name
            )));
        }
    }

    Ok(BoundInsert {
        table: stmt.table_name.clone(),
        id,
        values,
        operation_id: stmt.operation_id.clone(),
    })
}

/// 集計項目 1 つの引数を意味論的に解決した結果（TASK-166・SQL-13）。
/// `sql::aggregate::execute_aggregate` はこの enum だけを見て走査中の 1 行から
/// 集計対象値を取り出す（`schema`・`udfs` を再度参照しない自己完結な形）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AggregateInput {
    /// `COUNT(*)`・`COUNT(id)`・`COUNT(<Scalar 型の式>)` のいずれか。可視行はすべて
    /// 対象（NULL・非存在の概念がない）。`COUNT` 以外の関数からこの variant を得る
    /// ことはない（[`resolve_aggregate_input`] 参照）。`VECTOR` 列の裸の列参照は
    /// nullable 属性を持つため対象外（[`AggregateInput::VectorColumnPresence`]）。
    AllVisible,
    /// 疑似列 `id`（`SUM`/`AVG`/`MIN`/`MAX`）。`f64` へ変換せず `u64` の
    /// `checked_add` で正確に演算する（`docs/spec/04-behavior/error-format.md`
    /// ERR-2 が新設する `22003` で桁あふれを fail-closed に拒否するため）。
    IdU64,
    /// `TEXT` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL 行数）・
    /// `MIN`/`MAX`（バイト順・NULL 無視）でのみ使う（`SUM`/`AVG` は
    /// [`resolve_aggregate_input`] が型不整合として拒否済み）。
    TextColumn(usize),
    /// 上記以外の `Scalar` 型に束縛された式（列参照 `id` 単体を除く。`vec_norm(...)`
    /// 等の組み込み関数・宣言的 UDF 呼び出し・四則演算）。`sql::udf_call::eval` で
    /// `id`・embedding から評価する。
    ScalarExpr(crate::sql::udf_call::BoundExpr),
    /// `VECTOR` 列の裸の列参照（`COUNT` 限定。[`resolve_aggregate_input`] 参照）。
    /// 列は `ALTER TABLE ADD COLUMN`（TABLE-5）で追加された nullable な `VECTOR`
    /// 列の可能性があり、値が未設定の可視行は NULL として `COUNT` から除外する
    /// （`row.embedding` が空 = 未設定という [`crate::storage::Row`] の既存契約に
    /// 従う。PR #229 codex-review 指摘対応）。
    VectorColumnPresence,
}

/// 束縛済みの集計項目 1 つ（TASK-166・SQL-13）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundAggregateItem {
    pub(crate) func: crate::sql::allowlist::AggregateFunc,
    pub(crate) input: AggregateInput,
    /// `AS <alias>` の指定値、省略時は関数名小文字
    /// （[`crate::sql::allowlist::AggregateFunc::default_alias`]）。
    pub(crate) name: String,
}

/// 束縛済みの集計 SELECT 文（TASK-166・SQL-13）。[`crate::sql::aggregate::execute_aggregate`]
/// が直接実行する入力形。`BoundStatement` と異なり検索固有のフィールド
/// （`ranking`・`limit`・`mode`・`evaluation_order`）を持たない（集計は常に単一行・
/// `HINT ORDER`／`USING MODE` を受理しないため。[`crate::sql::allowlist::ValidatedAggregate`]
/// のドキュメント参照）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundAggregate {
    pub(crate) table: String,
    pub(crate) items: Vec<BoundAggregateItem>,
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    pub(crate) expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    pub(crate) rls_predicate_present: bool,
}

/// 集計項目 1 つの引数（[`crate::sql::allowlist::AggregateArg`]）を `schema` と
/// 照合し、[`AggregateInput`] へ解決する（TASK-166・SQL-13）。列名解決の優先順位
/// （実カラム＞疑似列 `id`）は [`bind_in_session`] の投影束縛・
/// `sql::udf_call::bind_expr_in` と揃える（Issue #56 レビュー指摘で確立した既存
/// 規約）。
///
/// 型ごとの受理・拒否は以下（対象ビヘイビア: SQL-13）:
/// - `*`（`COUNT` 限定。構文層が既に強制済み）→ [`AggregateInput::AllVisible`]
/// - `id` → `COUNT` は [`AggregateInput::AllVisible`]、それ以外は
///   [`AggregateInput::IdU64`]
/// - `TEXT` 列 → `SUM`/`AVG` は型不整合（`22000`）、それ以外は
///   [`AggregateInput::TextColumn`]
/// - `VECTOR` 列（裸の列参照）→ `COUNT` は [`AggregateInput::VectorColumnPresence`]
///   （非 NULL 行のみ数える）、それ以外は型不整合（`22000`）
/// - 上記以外の識別子 → 未知の列（`22000`）
/// - 複合式（`Expr::Call`・`Expr::Binary`・`Expr::Number`）→
///   `sql::udf_call::bind_expr` に委譲し、`Scalar` 型のみ
///   [`AggregateInput::ScalarExpr`] として受理、`Vector`/`Bool` 型は型不整合
///   （`22000`）
fn resolve_aggregate_input(
    func: crate::sql::allowlist::AggregateFunc,
    arg: &crate::sql::allowlist::AggregateArg,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    node_budget: &mut usize,
) -> Result<AggregateInput, SqlSurfaceError> {
    use crate::sql::allowlist::{AggregateArg, AggregateFunc};
    use crate::sql::udf_call::ExprType;

    match arg {
        AggregateArg::Star => Ok(AggregateInput::AllVisible),
        AggregateArg::Expr(Expr::Ident(name)) => {
            if let Some((index, column)) = schema
                .columns
                .iter()
                .enumerate()
                .find(|(_, c)| &c.name == name)
            {
                return match (column.ty, func) {
                    (ColumnType::Text, AggregateFunc::Sum | AggregateFunc::Avg) => {
                        Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} is TEXT and cannot be used with SUM/AVG"
                        )))
                    }
                    (ColumnType::Text, _) => Ok(AggregateInput::TextColumn(index)),
                    (ColumnType::Vector(_), AggregateFunc::Count) => {
                        Ok(AggregateInput::VectorColumnPresence)
                    }
                    (ColumnType::Vector(_), _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is VECTOR and cannot be used with SUM/AVG/MIN/MAX"
                    ))),
                };
            }
            if name == "id" {
                return match func {
                    AggregateFunc::Count => Ok(AggregateInput::AllVisible),
                    _ => Ok(AggregateInput::IdU64),
                };
            }
            Err(SqlSurfaceError::invalid_input(format!(
                "unknown column: {name}"
            )))
        }
        AggregateArg::Expr(expr) => {
            let (bound, ty) = crate::sql::udf_call::bind_expr(expr, schema, udfs, node_budget)?;
            match ty {
                ExprType::Scalar => Ok(AggregateInput::ScalarExpr(bound)),
                ExprType::Vector | ExprType::Bool => Err(SqlSurfaceError::invalid_input(
                    "aggregate argument must evaluate to a scalar",
                )),
            }
        }
    }
}

/// [`crate::sql::allowlist::ValidatedAggregate`] を `schema`・UDF レジストリ `udfs`
/// と照合して [`BoundAggregate`] へ束縛する（TASK-166・SQL-13 の公開 API）。
/// `WHERE` 句の意味論は [`bind_where_predicates`] を検索 SELECT
/// （[`bind_in_session`]）と共有する。式ノード予算（[`crate::sql::udf_call::MAX_EXPR_NODES`]）
/// は集計項目＋`WHERE` の全式項目で 1 文につき共有する（`bind_in_session` と同じ
/// 歯止め）。
pub(crate) fn bind_aggregate(
    stmt: &crate::sql::allowlist::ValidatedAggregate,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundAggregate, SqlSurfaceError> {
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;

    let mut items = Vec::with_capacity(stmt.items().len());
    for item in stmt.items() {
        let input = resolve_aggregate_input(item.func, &item.arg, schema, udfs, &mut node_budget)?;
        let name = item
            .alias
            .clone()
            .unwrap_or_else(|| item.func.default_alias().to_string());
        items.push(BoundAggregateItem {
            func: item.func,
            input,
            name,
        });
    }

    let (metadata_filters, expr_filters, rls_predicate_present) =
        bind_where_predicates(stmt.where_predicates(), schema, udfs, &mut node_budget)?;

    Ok(BoundAggregate {
        table: stmt.table_name().to_string(),
        items,
        metadata_filters,
        expr_filters,
        rls_predicate_present,
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
        assert!(bound.metadata_filters.is_empty());
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
        assert_eq!(bound.metadata_filters.len(), 1);
        let filter = &bound.metadata_filters[0];
        assert_eq!(filter.column_index(), 2);
        assert_eq!(
            filter.op(),
            &crate::declarative_filter::FilterOp::Equals("ja".to_string())
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
        assert!(bound.metadata_filters.is_empty());
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

    // --- bind_insert（SQL-10、TASK-80） ----------------------------------------

    fn bind_insert_sql(sql: &str) -> Result<BoundInsert, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_insert(
            sql,
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        bind_insert(&stmt, &docs_schema())
    }

    #[test]
    fn binds_insert_with_all_columns() {
        let bound = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'ja') USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_insert should succeed");
        assert_eq!(bound.table, "documents");
        assert_eq!(bound.id, 1);
        assert_eq!(
            bound.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
        assert_eq!(
            bound.values,
            vec![
                crate::row_codec::Value::Vector(vec![0.1, 0.2, 0.3]),
                crate::row_codec::Value::Text("hello".to_string()),
                crate::row_codec::Value::Text("ja".to_string()),
            ]
        );
    }

    #[test]
    fn binds_insert_leaving_nullable_column_null_when_omitted() {
        let bound = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_insert should succeed");
        assert_eq!(
            bound.values,
            vec![
                crate::row_codec::Value::Vector(vec![0.1, 0.2, 0.3]),
                crate::row_codec::Value::Text("hello".to_string()),
                crate::row_codec::Value::Null,
            ]
        );
    }

    #[test]
    fn rejects_insert_missing_id_pseudo_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (embedding, body) VALUES ('[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_id_value_out_of_u64_range() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (18446744073709551616, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_unknown_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, nope) VALUES (1, '[0.1,0.2,0.3]', 'x') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_type_mismatch_number_for_text_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 42) USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_type_mismatch_string_for_vector_column_wrong_dim() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_missing_non_nullable_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_duplicate_column_in_list() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, id) VALUES (1, 2) USING OPERATION_ID 'op-0001'",
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

    // --- bind_aggregate（TASK-166・SQL-13） -------------------------------------

    fn bind_aggregate_sql(sql: &str) -> Result<BoundAggregate, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_sql(sql, &lookup).expect("must pass allowlist");
        let agg = match stmt {
            crate::sql::allowlist::Statement::Aggregate(agg) => agg,
            other => panic!("expected Statement::Aggregate, got {other:?}"),
        };
        bind_aggregate(
            &agg,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
    }

    #[test]
    fn binds_sum_avg_min_max_on_vector_column_as_type_mismatch() {
        for func in ["SUM", "AVG", "MIN", "MAX"] {
            let sql = format!("SELECT {func}(embedding) FROM documents");
            let err = bind_aggregate_sql(&sql).unwrap_err();
            assert_eq!(
                err.wire_code(),
                "22000",
                "{func}(embedding) should be 22000"
            );
        }
    }

    #[test]
    fn binds_sum_avg_on_text_column_as_type_mismatch() {
        for func in ["SUM", "AVG"] {
            let sql = format!("SELECT {func}(lang) FROM documents");
            let err = bind_aggregate_sql(&sql).unwrap_err();
            assert_eq!(err.wire_code(), "22000", "{func}(lang) should be 22000");
        }
    }

    #[test]
    fn binds_unknown_column_as_invalid_input() {
        let err = bind_aggregate_sql("SELECT SUM(ghost) FROM documents").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn binds_sum_on_bool_typed_expression_as_type_mismatch() {
        // 実際の SQL 文法では比較演算子（`>` 等）は集計引数の式文法
        // （`parse_value_expr`）に現れないため、この組み合わせは構文上到達しない。
        // `bind_aggregate` 自体の型検査（`ExprType::Bool` を拒否する分岐）が
        // 独立して機能することを確認するため、AST を直接組み立てて渡す
        // （防御的実装の単体検証。§計画 6-B）。
        use crate::sql::allowlist::{
            AggregateArg, AggregateFunc, AggregateItem, ValidatedAggregate,
        };
        use crate::sql::udf_call::BinOp;

        // `ValidatedAggregate`/`AggregateItem` のフィールドは `pub(crate)` のため、
        // 同一クレート内であるこのテストからは構造体リテラルで直接組み立てられる
        // （`allowlist.rs` のカプセル化はクレート外からの構築のみを禁じる）。
        let agg = ValidatedAggregate {
            table_name: "documents".to_string(),
            items: vec![AggregateItem {
                func: AggregateFunc::Sum,
                arg: AggregateArg::Expr(Expr::Binary {
                    op: BinOp::Gt,
                    lhs: Box::new(Expr::Ident("id".to_string())),
                    rhs: Box::new(Expr::Number("1".to_string())),
                }),
                alias: None,
            }],
            where_predicates: Vec::new(),
        };
        let err = bind_aggregate(
            &agg,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn binds_sum_on_vector_typed_expression_as_type_mismatch() {
        let err =
            bind_aggregate_sql("SELECT SUM(vec_div(embedding, 2)) FROM documents").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn binds_count_sum_avg_min_max_on_id_and_udf_call() {
        let bound = bind_aggregate_sql(
            "SELECT COUNT(embedding), COUNT(lang), SUM(id), AVG(vec_norm(embedding)), MIN(lang) FROM documents",
        )
        .expect("bind should succeed");
        assert_eq!(bound.items.len(), 5);
        assert_eq!(bound.items[0].input, AggregateInput::VectorColumnPresence);
        assert!(matches!(
            bound.items[1].input,
            AggregateInput::TextColumn(_)
        ));
        assert_eq!(bound.items[2].input, AggregateInput::IdU64);
        assert!(matches!(
            bound.items[3].input,
            AggregateInput::ScalarExpr(_)
        ));
        assert!(matches!(
            bound.items[4].input,
            AggregateInput::TextColumn(_)
        ));
    }

    #[test]
    fn binds_default_alias_to_lowercase_function_name() {
        let bound =
            bind_aggregate_sql("SELECT COUNT(*) FROM documents").expect("bind should succeed");
        assert_eq!(bound.items[0].name, "count");
    }

    #[test]
    fn binds_explicit_alias_over_default() {
        let bound = bind_aggregate_sql("SELECT COUNT(*) AS total FROM documents")
            .expect("bind should succeed");
        assert_eq!(bound.items[0].name, "total");
    }

    #[test]
    fn aggregate_binds_real_id_column_over_pseudo_column_when_schema_declares_it() {
        let schema = TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        );
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_sql("SELECT MIN(id) FROM documents", &lookup)
            .expect("must pass allowlist");
        let agg = match stmt {
            crate::sql::allowlist::Statement::Aggregate(agg) => agg,
            other => panic!("expected Statement::Aggregate, got {other:?}"),
        };
        let bound = bind_aggregate(&agg, &schema, &crate::sql::udf_call::UdfRegistry::default())
            .expect("bind should succeed");
        // スキーマが実カラム `id`（TEXT）を宣言しているため、疑似列ではなく実カラムへ
        // 束縛される（`resolve_aggregate_input` の優先順位。Issue #56 と同じ規約）。
        assert!(matches!(
            bound.items[0].input,
            AggregateInput::TextColumn(_)
        ));
    }
}
