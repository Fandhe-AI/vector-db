//! `op: search` の JSON クエリオブジェクトを `engine::sql::parser::
//! BoundStatement` へ束縛するモジュール（Issue #763・TASK-175。対象
//! ビヘイビア NOSQL-2・NOSQL-7・SQL-1〜5・SQL-12。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-2・`docs/spec/04-behavior/sql-surface.md` SQL-1〜5・SQL-12）。
//!
//! 責務境界: [`super::gate`] が認証済み要求（`SEARCH_SCHEMA` による形の検証
//! 済み [`super::schema::Validated`]）を得たあと呼び出す入口が [`bind_search`]。
//! SQL 表層の検索 `SELECT` が `sql::parser::bind_in_session` で得るのと
//! **同一形の `BoundStatement`** を、SQL テキストを一切生成せず
//! `BoundStatement::new` で直接構築する（第 2 の実行器を作らない方針。
//! `filter.rs`（Issue #761）が `declarative_filter::bind_all` を共有するのと
//! 同じ判断）。投影列解決・ベクトル値検証・本文列規約・`LIMIT` 範囲・
//! `mode` 解決は `engine::sql::parser`／`engine::sql::mode` の既存公開関数へ
//! 委譲し、束縛規則を wire-server 側に複製しない。
//!
//! `vector`／`plan` は排他（SQL の `ORDER BY`／`USING PLAN` が構文上
//! 相互排他なのと対応）。`plan` 指定は LLM 展開（`EngineCore` の private
//! フィールド `embedder`／`query_planner` 経由の I/O）が必要なため、本
//! モジュールでは束縛可能な部品（投影・フィルタ・`limit`・未解決 `mode`・
//! 原質問）を [`PlanSearch`] として返すに留める。`BoundStatement` までの
//! 完成は #764（`EngineCore` への binder-closure 型エントリ追加）の担当。
//!
//! RLS はサーバー側の `PolicyContext` 暗黙適用のみで、クライアントは述語を
//! 書けない（`security.md` P0「テナント境界」）。本モジュールは
//! `PolicyContext` を一切受け取らず、`BoundStatement::new` の
//! `rls_predicate_present` は常に `false` で構築する。
//!
//! 対象外: 実行そのもの（#764）・`explain` フィールドの判定利用（#765。
//! 本モジュールは値の保持のみ）。

use engine::catalog::TableSchema;
use engine::declarative_filter::MetadataFilter;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::{validate_using_plan_question, SqlSurfaceError};
use engine::sql::mode::{self, SearchMode};
use engine::sql::parser::{
    bind_body_text_column, bind_column_projection, bind_vector_values, require_vector_column,
    validate_search_limit, BoundStatement, ProjectedColumn, Ranking,
};
use engine::sql::plan::EvaluationOrder;

use super::filter::{bind_filter, FilterError};
use super::schema::{SchemaError, Validated};

/// `plan` 指定の束縛結果（Issue #763 時点での中間形）。LLM 展開・再埋め込みは
/// engine 内部 I/O（`EngineCore` の private フィールド経由）を要するため、
/// 本モジュールでは `BoundStatement` まで到達させない。#764 が
/// `crate::sql::using_plan::bind_expansion` 相当の処理を経て `BoundStatement`
/// を完成させる際、ここで束縛済みの部品（投影・フィルタ・`limit`）と
/// 原質問文字列をそのまま使う。
///
/// `query_mode`（`search.mode`）は**未解決のまま**保持する。SQL の
/// `USING PLAN` 経路（`core.rs::EngineCore::execute_sql_in_session`）は
/// `resolve_mode_with_planner(query, session, planner_hint)` で
/// `PlannerEstimate` 由来の推定とあわせて解決するため、ここで
/// `resolve_mode(query_mode, None)` へ確定してしまうとプランナー推定を
/// 採用する経路を失う（TASK-164・PLAN-11）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PlanSearch {
    table: String,
    projection: Vec<ProjectedColumn>,
    metadata_filters: Vec<MetadataFilter>,
    limit: usize,
    question: String,
    query_mode: Option<SearchMode>,
    explain: bool,
}

impl PlanSearch {
    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn projection(&self) -> &[ProjectedColumn] {
        &self.projection
    }

    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// `USING PLAN('<query>')` に渡す原質問文字列。sanitize は engine 側
    /// （`query_planner.rs`）の既存契約に委ねる。
    pub fn question(&self) -> &str {
        &self.question
    }

    /// 未解決のクエリ句モード指定（`search.mode`）。`None` は指定なし。
    pub fn query_mode(&self) -> Option<SearchMode> {
        self.query_mode
    }

    pub fn explain(&self) -> bool {
        self.explain
    }
}

/// `search` op の束縛結果。`vector` 指定は SQL の `bind_in_session` と
/// 同一形の [`BoundStatement`]、`plan` 指定は束縛済み部品のみの
/// [`PlanSearch`]（完成は #764）。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundSearch {
    Vector(BoundStatement),
    Plan(PlanSearch),
}

/// [`bind_search`] の失敗を表す。いずれも [`ClassifiedError`] を実装し
/// `wire_code`／`client_message` を経由して HTTP エラー応答へ射影される。
#[derive(Debug, Clone)]
pub enum SearchError {
    /// [`super::schema::Validated`] アクセサの型・キー不一致（`42601`）。
    Shape(SchemaError),
    /// `vector` と `plan` が両方指定されている（SQL の `ORDER BY`／
    /// `USING PLAN` が構文上相互排他なのと対応。`42601`）。
    VectorAndPlanBothPresent,
    /// `vector`・`plan` のいずれも指定されていない（`search` はいずれか
    /// 必須。`42601`）。
    VectorAndPlanBothMissing,
    /// `plan` と `hybrid` が同時に指定されている（SQL-5: `USING PLAN` は
    /// `ORDER BY` と相互排他であり、`hybrid` は `ORDER BY HYBRID(...)` 形の
    /// 一部のため両立しない。`42601`）。
    PlanWithHybrid,
    /// `columns: []`（空配列）。SQL 表層に等価形（空の SELECT リスト）が
    /// 存在しないため fail-closed に拒否する（`42601`）。
    EmptyColumns,
    /// `filter` 配列の写像・束縛エラー（[`FilterError`] をそのまま透過）。
    Filter(FilterError),
    /// engine の束縛ヘルパー（投影・ベクトル値・本文列・`LIMIT`・`mode`）の
    /// エラーをそのまま透過する（`22000`／`54000`）。
    Bind(SqlSurfaceError),
}

impl From<SchemaError> for SearchError {
    fn from(err: SchemaError) -> Self {
        SearchError::Shape(err)
    }
}

impl From<FilterError> for SearchError {
    fn from(err: FilterError) -> Self {
        SearchError::Filter(err)
    }
}

impl From<SqlSurfaceError> for SearchError {
    fn from(err: SqlSurfaceError) -> Self {
        SearchError::Bind(err)
    }
}

impl ClassifiedError for SearchError {
    fn error_class(&self) -> ErrorClass {
        match self {
            SearchError::Shape(err) => err.error_class(),
            SearchError::VectorAndPlanBothPresent
            | SearchError::VectorAndPlanBothMissing
            | SearchError::PlanWithHybrid
            | SearchError::EmptyColumns => ErrorClass::UnsupportedSqlSyntax,
            SearchError::Filter(err) => err.error_class(),
            SearchError::Bind(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            SearchError::Shape(err) => err.client_message(),
            SearchError::VectorAndPlanBothPresent => {
                "search request must not specify both \"vector\" and \"plan\"".to_string()
            }
            SearchError::VectorAndPlanBothMissing => {
                "search request must specify exactly one of \"vector\" or \"plan\"".to_string()
            }
            SearchError::PlanWithHybrid => {
                "search request must not combine \"plan\" with \"hybrid\"".to_string()
            }
            SearchError::EmptyColumns => {
                "search request \"columns\" must not be an empty array".to_string()
            }
            SearchError::Filter(err) => err.client_message(),
            SearchError::Bind(err) => err.client_message(),
        }
    }
}

/// `vector`／`plan` の排他判定（[`bind_search`]）が、判定と同時に以降の
/// 分岐が使う値を確定するための内部区分。受信データ経路で
/// `Option::expect` により「到達しないはずの分岐」を後段に残さないための
/// 構造（`.claude/rules/coding-rust.md`「受信データ経路では `unwrap`/
/// `expect`... を禁止する」）。
enum VectorOrPlan<'a> {
    Vector(&'a [JsonValue]),
    Plan(&'a str),
}

/// `columns` フィールド（`Option<&[JsonValue]>`。要素はスキーマ検証済みの
/// `String` のはずだが多層防御として再検査する）を `Vec<String>` へ写像する。
fn columns_as_strings(items: &[JsonValue]) -> Result<Vec<String>, SearchError> {
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        match item {
            JsonValue::String(s) => names.push(s.clone()),
            _ => {
                return Err(SearchError::Shape(SchemaError::TypeMismatch {
                    key: "columns",
                }))
            }
        }
    }
    Ok(names)
}

/// `vector` フィールド（`&[JsonValue]`。要素はスキーマ検証済みの `Number`
/// のはずだが多層防御として再検査する）を `Vec<f64>` へ写像する。
fn vector_as_f64(items: &[JsonValue]) -> Result<Vec<f64>, SearchError> {
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        match item {
            JsonValue::Number(n) => values.push(*n),
            _ => {
                return Err(SearchError::Shape(SchemaError::TypeMismatch {
                    key: "vector",
                }))
            }
        }
    }
    Ok(values)
}

/// `SEARCH_SCHEMA` 検証済みの `validated`（[`super::gate::handle`] の手順 4
/// を通過した要求）を `schema`（呼び出し元が同一 `read_txn` 内で
/// `Storage::get_table_schema` 等から供給する）へ束縛する。
///
/// 処理順序（fail-closed。順序自体が契約の一部）:
/// 1. `table`・`limit`・`explain` を読み取る（`limit` は
///    [`validate_search_limit`] で範囲検証）。
/// 2. `vector`／`plan` の排他判定（4 ケース）。
/// 3. `plan` かつ `hybrid` 同時指定を拒否。
/// 4. `columns` を投影へ束縛（`Some([])` は拒否）。
/// 5. `filter` を束縛。
/// 6. `mode` を検証（値は分岐によって確定／未解決のまま保持）。
/// 7. `vector`／`plan` いずれかへ分岐して最終形を組み立てる。
pub fn bind_search(
    validated: &Validated<'_>,
    schema: &TableSchema,
) -> Result<BoundSearch, SearchError> {
    let table = validated.required_str("table")?;
    let limit_raw = validated.required_u32("limit")?;
    let limit = validate_search_limit(limit_raw)?;
    let explain = validated.optional_bool("explain")?.unwrap_or(false);

    let vector_items = validated.optional_array("vector")?;
    let plan = validated.optional_str("plan")?;
    let hybrid = validated.optional_object("hybrid")?;

    // 排他判定と同時に、以降の分岐が使う値をここで確定する（`.expect()` で
    // 「到達しないはず」の分岐を後段に残さない。受信データ経路では
    // `unwrap`/`expect` を使わない方針 `.claude/rules/coding-rust.md`）。
    let vector_or_plan = match (vector_items, plan) {
        (Some(items), None) => VectorOrPlan::Vector(items),
        (None, Some(question)) => VectorOrPlan::Plan(question),
        (Some(_), Some(_)) => return Err(SearchError::VectorAndPlanBothPresent),
        (None, None) => return Err(SearchError::VectorAndPlanBothMissing),
    };
    if plan.is_some() && hybrid.is_some() {
        return Err(SearchError::PlanWithHybrid);
    }

    let columns = match validated.optional_array("columns")? {
        None => None,
        Some([]) => return Err(SearchError::EmptyColumns),
        Some(items) => Some(columns_as_strings(items)?),
    };
    let projection = bind_column_projection(columns.as_deref(), schema)?;

    let filter_items = validated.optional_array("filter")?.unwrap_or(&[]);
    let metadata_filters = bind_filter(filter_items, schema)?;

    let query_mode = match validated.optional_str("mode")? {
        Some(literal) => Some(SearchMode::parse_literal(literal)?),
        None => None,
    };

    if let VectorOrPlan::Vector(vector_items) = vector_or_plan {
        let values = vector_as_f64(vector_items)?;
        let query = bind_vector_values(&values, schema)?;
        let ranking = match hybrid {
            Some(hybrid_obj) => {
                let text_column_index = bind_body_text_column(schema)?;
                let query_text = match hybrid_obj.get("text") {
                    Some(JsonValue::String(s)) => s.clone(),
                    // `HYBRID_SCHEMA` が既に `text: Required String` を検証
                    // 済みだが、多層防御として再検査する（`filter.rs`
                    // `map_filter_item` と同じ判断）。
                    _ => {
                        return Err(SearchError::Shape(SchemaError::TypeMismatch {
                            key: "hybrid",
                        }))
                    }
                };
                Ranking::Hybrid {
                    query,
                    text_column_index,
                    query_text,
                }
            }
            None => Ranking::Distance { query },
        };
        let resolved_mode = mode::resolve_mode(query_mode, None);
        let bound = BoundStatement::new(
            table.to_string(),
            projection,
            metadata_filters,
            false,
            ranking,
            limit,
            EvaluationOrder::DEFAULT,
        )
        .with_mode(resolved_mode);
        return Ok(BoundSearch::Vector(bound));
    }

    // ここへ到達するのは `vector_or_plan` が `Plan` の場合のみ（直上の
    // `if let` が `Vector` を早期 return 済み）。
    let VectorOrPlan::Plan(question) = vector_or_plan else {
        return Err(SearchError::VectorAndPlanBothMissing);
    };
    validate_using_plan_question(question)?;
    // `USING PLAN`（SQL-5）は `VECTOR` 列必須・本文列必須の契約を束縛時に
    // 確認する（`sql::using_plan::pre_check_bindable`／`bind_expansion` と
    // 同じ判断。`VECTOR` 列なしテーブルへの `plan` 受理をここで塞がないと、
    // 失敗が本来のバインド契約より後——プラン検索の完了・実行時点、#764 で
    // LLM I/O が追加された後は高価な LLM 呼び出しの後——まで遅延する）。
    require_vector_column(schema)?;
    bind_body_text_column(schema)?;

    Ok(BoundSearch::Plan(PlanSearch {
        table: table.to_string(),
        projection,
        metadata_filters,
        limit,
        question: question.to_string(),
        query_mode,
        explain,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, ColumnType};
    use engine::json::parse_json;

    use crate::http::query::schema::SEARCH_SCHEMA;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    fn validate(text: &str) -> JsonValue {
        parse_json(text).expect("valid JSON fixture")
    }

    fn bind(text: &str) -> Result<BoundSearch, SearchError> {
        let value = validate(text);
        let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
        bind_search(&validated, &schema())
    }

    #[test]
    fn vector_only_binds_to_ranking_distance() {
        let bound = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10}"#)
            .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        assert!(matches!(stmt.ranking(), Ranking::Distance { .. }));
        assert_eq!(stmt.limit(), 10);
        assert!(!stmt.rls_predicate_present());
        assert_eq!(stmt.evaluation_order(), EvaluationOrder::DEFAULT);
    }

    #[test]
    fn plan_only_is_accepted_and_returns_plan_search() {
        let bound = bind(r#"{"op":"search","table":"docs","plan":"find something","limit":5}"#)
            .expect("bind ok");
        let BoundSearch::Plan(plan) = bound else {
            panic!("expected BoundSearch::Plan");
        };
        assert_eq!(plan.question(), "find something");
        assert_eq!(plan.limit(), 5);
        assert_eq!(plan.query_mode(), None);
        assert!(!plan.explain());
    }

    #[test]
    fn vector_and_plan_both_present_is_rejected() {
        let err = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"plan":"x","limit":5}"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, SearchError::VectorAndPlanBothPresent));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn vector_and_plan_both_missing_is_rejected() {
        let err = bind(r#"{"op":"search","table":"docs","limit":5}"#).expect_err("must reject");
        assert!(matches!(err, SearchError::VectorAndPlanBothMissing));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn plan_with_hybrid_is_rejected() {
        let err =
            bind(r#"{"op":"search","table":"docs","plan":"x","hybrid":{"text":"y"},"limit":5}"#)
                .expect_err("must reject");
        assert!(matches!(err, SearchError::PlanWithHybrid));
        assert_eq!(err.wire_code(), "42601");
    }

    /// `VECTOR` 列を持たないテーブル（広域取得専用。SQL-15・Issue #454）の
    /// 束縛。`plan` 経路の `VECTOR` 列存在チェック（Issue #763 PR #820
    /// Bugbot 指摘）を単体で検証するための fixture。
    fn scan_only_schema() -> TableSchema {
        TableSchema::new(
            "notes",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    #[test]
    fn plan_on_table_without_vector_column_is_rejected_at_bind_time() {
        let value =
            validate(r#"{"op":"search","table":"notes","plan":"find something","limit":5}"#);
        let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
        let err = bind_search(&validated, &scan_only_schema()).expect_err("must reject");
        assert!(matches!(err, SearchError::Bind(_)));
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn empty_columns_array_is_rejected() {
        let err = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"columns":[]}"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, SearchError::EmptyColumns));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn columns_selects_named_projection() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"columns":["id","lang"]}"#,
        )
        .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        assert_eq!(stmt.projection().len(), 2);
        assert_eq!(stmt.projection()[0], ProjectedColumn::Id);
        assert!(
            matches!(&stmt.projection()[1], ProjectedColumn::Column { name, .. } if name == "lang")
        );
    }

    #[test]
    fn mode_recall_and_precision_are_resolved_via_query_clause() {
        for (literal, expected) in [
            ("recall", SearchMode::Recall),
            ("precision", SearchMode::Precision),
        ] {
            let bound = bind(&format!(
                r#"{{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"mode":"{literal}"}}"#
            ))
            .expect("bind ok");
            let BoundSearch::Vector(stmt) = bound else {
                panic!("expected BoundSearch::Vector");
            };
            assert_eq!(stmt.mode().mode(), expected);
            assert_eq!(stmt.mode().source(), mode::ModeSource::QueryClause);
        }
    }

    #[test]
    fn mode_absent_resolves_to_default() {
        let bound = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5}"#)
            .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        assert_eq!(stmt.mode().source(), mode::ModeSource::Default);
    }

    #[test]
    fn plan_mode_is_left_unresolved() {
        let bound =
            bind(r#"{"op":"search","table":"docs","plan":"x","limit":5,"mode":"precision"}"#)
                .expect("bind ok");
        let BoundSearch::Plan(plan) = bound else {
            panic!("expected BoundSearch::Plan");
        };
        assert_eq!(plan.query_mode(), Some(SearchMode::Precision));
    }

    #[test]
    fn mode_invalid_literal_is_rejected() {
        let err = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"mode":"fast"}"#,
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn hybrid_binds_to_ranking_hybrid_with_body_column() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"hybrid":{"text":"hello world"}}"#,
        )
        .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        let Ranking::Hybrid {
            text_column_index,
            query_text,
            ..
        } = stmt.ranking()
        else {
            panic!("expected Ranking::Hybrid");
        };
        assert_eq!(schema().columns[*text_column_index].name, "body");
        assert_eq!(query_text, "hello world");
    }

    #[test]
    fn hybrid_missing_body_column_is_rejected() {
        let schema_without_body = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        );
        let value = validate(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"hybrid":{"text":"x"}}"#,
        );
        let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
        let err = bind_search(&validated, &schema_without_body).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn vector_dimension_mismatch_is_rejected() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2],"limit":5}"#)
            .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn limit_zero_is_rejected() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":0}"#)
            .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn limit_over_max_search_k_is_rejected() {
        // `engine::core::MAX_SEARCH_K` は `pub(crate)`（engine クレート内部）
        // のため、wire-server 側のテストからは直接参照できない。SQL 表層の
        // 既存テスト（例: `wire_scan.rs` の `LIMIT 10000`）と同じ 10_000 を
        // 実測値として直接ハードコードする（値は
        // `docs/spec/04-behavior/sql-surface.md` 由来ではなく本リポ実装の
        // 既定値。CLAUDE.md ステータス欄「TASK-166」等の記載でも公開済み）。
        let text = r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10001}"#;
        let err = bind(text).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn limit_fractional_is_rejected_at_schema_layer() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":1.5}"#)
            .expect_err("must reject");
        assert!(matches!(err, SearchError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn limit_negative_is_rejected_at_schema_layer() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":-1}"#)
            .expect_err("must reject");
        assert!(matches!(err, SearchError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn plan_oversized_question_is_rejected() {
        let huge = "x".repeat(64 * 1024 + 1);
        let text = format!(r#"{{"op":"search","table":"docs","plan":"{huge}","limit":5}}"#);
        let err = bind(&text).expect_err("must reject");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn filter_is_bound_for_vector_search() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        )
        .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        let expected = engine::declarative_filter::bind_all(
            &[engine::declarative_filter::DeclarativeFilter::equals(
                "lang", "ja",
            )],
            &schema(),
        )
        .expect("bind ok");
        assert_eq!(stmt.metadata_filters(), expected.as_slice());
    }

    #[test]
    fn filter_is_bound_for_plan_search() {
        let bound = bind(
            r#"{"op":"search","table":"docs","plan":"x","limit":5,"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        )
        .expect("bind ok");
        let BoundSearch::Plan(plan) = bound else {
            panic!("expected BoundSearch::Plan");
        };
        let expected = engine::declarative_filter::bind_all(
            &[engine::declarative_filter::DeclarativeFilter::equals(
                "lang", "ja",
            )],
            &schema(),
        )
        .expect("bind ok");
        assert_eq!(plan.metadata_filters(), expected.as_slice());
    }

    #[test]
    fn explain_flag_is_preserved() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#,
        )
        .expect("bind ok");
        // `explain` はプランナー実行系（#764）・`explain` op（#765）が使う
        // ため `BoundStatement` 自体には保持しない。`Vector` 経路では
        // 「拒否されないこと」だけを固定する（`Plan` 経路は `PlanSearch`
        // へ保持する。下の `plan_explain_flag_is_preserved` 参照）。
        assert!(matches!(bound, BoundSearch::Vector(_)));

        let plan_bound =
            bind(r#"{"op":"search","table":"docs","plan":"x","limit":5,"explain":true}"#)
                .expect("bind ok");
        let BoundSearch::Plan(plan) = plan_bound else {
            panic!("expected BoundSearch::Plan");
        };
        assert!(plan.explain());
    }
}
