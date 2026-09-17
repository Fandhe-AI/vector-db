//! `POST /v1/query`（`op: aggregate`）を SQL 表層の集計実行計画（TASK-166・
//! SQL-13）へ**SQL テキストを経由せずに**写像し実行するモジュール（Issue #768。
//! 対象ビヘイビア TASK-177・NOSQL-4。ポインタ: `docs/spec/05-tasks.md`
//! TASK-177・`docs/spec/04-behavior/nosql-surface.md` NOSQL-4・
//! `docs/spec/04-behavior/sql-surface.md` SQL-13）。
//!
//! 責務境界: JSON → SQL 文字列の組み立ては行わない（`.claude/rules/
//! coding-rust.md`「SQL / プラン文字列の組み立てに未検証入力を連結しない」・
//! spec「第 2 の実行器を作らない」方針）。`engine::sql::parser::
//! BoundAggregateItem::bind`／`BoundAggregate::new`（Issue #768・TASK-186・
//! NOSQL-4 で公開）を直接呼び、列名解決・型不整合判定
//! （`resolve_aggregate_input`）を SQL テキスト経由の `bind_aggregate` と
//! 完全に共有する。
//!
//! `group_by`／`having`／`explain: true` は本モジュールの対象外
//! （NOSQL-5・NOSQL-10 は #769・#765 の担当）。黙って無視すると
//! fail-open になるため、`0A000`（[`super::gate::PLACEHOLDER_MESSAGE`] と
//! 同型の未実装扱い）で拒否し実行しない。
//!
//! `table`／`aggregates[].column` は [`super::ident::check_identifier`] で
//! 識別子形状を検査してから使う（SQL 表層の字句解析段階の拒否と同じ
//! `42601` 分類。untrusted 文字列は echo しない）。`filter` 配列は
//! [`super::filter::bind_filter`]（Issue #761・NOSQL-7）へ委譲する。
//!
//! RLS: `PolicyContext` は呼び出し元が渡す
//! [`crate::http::session::middleware::SessionPrincipal::policy_context`]
//! のみから導出する（本モジュールはヘッダ・JSON からテナントを読む経路を
//! シグネチャ上持たない）。`EngineCore::execute_bound_aggregate_in_session`
//! （Issue #728）が `ctx` から RLS を暗黙適用する既存契約をそのまま使う。

use engine::catalog::TableSchema;
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::parser::{AggregateTarget, BoundAggregate, BoundAggregateItem};

use super::filter::{self, FilterError};
use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};

use crate::http::session::middleware::SessionPrincipal;

/// `group_by`／`having`／`explain: true` を伴う `aggregate` 要求に返す固定文言
/// （[`super::gate::PLACEHOLDER_MESSAGE`] と同型の「未実装」扱い。`group_by`
/// キーの存在は空配列も含む——`GROUP BY` なしの単一行集計〔TASK-166・
/// SQL-13〕へ意図せず縮退させない）。
pub const NOT_YET_SUPPORTED_MESSAGE: &str =
    "aggregate group_by/having/explain is not yet available";

/// `fn`（[`engine::sql::allowlist::AggregateFunc`]）の受理語彙。`Op::parse`
/// （Issue #759）と同じ方針で**小文字完全一致のみ**を受理する（大文字小文字
/// 読み替え・trim なし）。`engine::sql::allowlist::AggregateFunc::from_name`
/// は大文字化して比較する private ヘルパーのため、ここでは独立に判定する。
pub fn parse_function(raw: &str) -> Option<engine::sql::allowlist::AggregateFunc> {
    use engine::sql::allowlist::AggregateFunc;
    match raw {
        "count" => Some(AggregateFunc::Count),
        "sum" => Some(AggregateFunc::Sum),
        "avg" => Some(AggregateFunc::Avg),
        "min" => Some(AggregateFunc::Min),
        "max" => Some(AggregateFunc::Max),
        _ => None,
    }
}

/// [`bind`]／[`execute`] の失敗を表す。いずれも [`ClassifiedError`] を実装し
/// HTTP エラー応答へ射影される。
#[derive(Debug, Clone)]
pub enum AggregateError {
    /// `aggregates` 配列要素が [`super::schema::AGGREGATE_ITEM_SCHEMA`] の形
    /// （必須キー・型）に適合しない。
    Shape(SchemaError),
    /// `fn` が `count`／`sum`／`avg`／`min`／`max` の小文字完全一致に一致しない。
    /// untrusted な `fn` 文字列は保持しない（[`super::op::UnsupportedOp`] と
    /// 同じ判断）。
    UnsupportedFunction,
    /// `table`／`aggregates[].column` が識別子として意味を持ちうる形状
    /// （[`super::ident::check_identifier`]）を満たさない。
    InvalidIdentifier,
    /// `filter` 配列の写像・束縛エラー（Issue #761・NOSQL-7）。
    Filter(FilterError),
    /// `engine::sql::parser::BoundAggregateItem::bind`／`BoundAggregate::new`／
    /// `EngineCore::execute_bound_aggregate_in_session` の束縛・実行エラー
    /// （型不整合 `22000`・オーバーフロー `22003`・テーブル不存在 `42P01`・
    /// 集計項目数上限超過 `54000` 等）をそのまま透過する。
    Engine(SqlSurfaceError),
    /// `group_by`／`having`／`explain: true` を伴う要求（本モジュールの対象外。
    /// NOSQL-5・NOSQL-10 の担当）。
    NotYetSupported,
}

impl From<SchemaError> for AggregateError {
    fn from(err: SchemaError) -> Self {
        AggregateError::Shape(err)
    }
}

impl From<FilterError> for AggregateError {
    fn from(err: FilterError) -> Self {
        AggregateError::Filter(err)
    }
}

impl From<SqlSurfaceError> for AggregateError {
    fn from(err: SqlSurfaceError) -> Self {
        AggregateError::Engine(err)
    }
}

impl From<InvalidIdentifier> for AggregateError {
    fn from(_err: InvalidIdentifier) -> Self {
        AggregateError::InvalidIdentifier
    }
}

impl ClassifiedError for AggregateError {
    fn error_class(&self) -> ErrorClass {
        match self {
            AggregateError::Shape(err) => err.error_class(),
            AggregateError::UnsupportedFunction | AggregateError::InvalidIdentifier => {
                ErrorClass::UnsupportedSqlSyntax
            }
            AggregateError::Filter(err) => err.error_class(),
            AggregateError::Engine(err) => err.error_class(),
            AggregateError::NotYetSupported => ErrorClass::FeatureNotSupported,
        }
    }

    fn client_message(&self) -> String {
        match self {
            AggregateError::Shape(err) => err.client_message(),
            AggregateError::UnsupportedFunction => {
                "unsupported aggregate function (only \"count\", \"sum\", \"avg\", \"min\", \
                 \"max\" are allowed)"
                    .to_string()
            }
            AggregateError::InvalidIdentifier => "invalid identifier".to_string(),
            AggregateError::Filter(err) => err.client_message(),
            AggregateError::Engine(err) => err.client_message(),
            AggregateError::NotYetSupported => NOT_YET_SUPPORTED_MESSAGE.to_string(),
        }
    }
}

/// `validated`（[`super::schema::AGGREGATE_SCHEMA`] を通過済みの `aggregate`
/// 要求本文）が `group_by`／`having`（空配列を含む）または `explain: true` を
/// 伴うかを判定する。伴う場合は `Err(AggregateError::NotYetSupported)` を
/// 返し、呼び出し元は束縛・実行を一切行わない（黙って無視すると `GROUP BY`
/// なしの単一行集計〔TASK-166・SQL-13〕として fail-open に実行してしまう
/// ため、キーの存在だけで判定する）。
fn reject_not_yet_supported(validated: &Validated<'_>) -> Result<(), AggregateError> {
    if validated.optional_array("group_by")?.is_some() {
        return Err(AggregateError::NotYetSupported);
    }
    if validated.optional_array("having")?.is_some() {
        return Err(AggregateError::NotYetSupported);
    }
    if validated.optional_bool("explain")? == Some(true) {
        return Err(AggregateError::NotYetSupported);
    }
    Ok(())
}

/// `aggregates` 配列の要素 1 件を [`BoundAggregateItem`] へ写像する。
fn bind_item(
    item: &engine::json::JsonValue,
    schema: &TableSchema,
) -> Result<BoundAggregateItem, AggregateError> {
    let validated = super::schema::AGGREGATE_ITEM_SCHEMA.validate(item)?;
    let raw_fn = validated.required_str("fn")?;
    let column = validated.required_str("column")?;

    let func = parse_function(raw_fn).ok_or(AggregateError::UnsupportedFunction)?;
    let target = if column == "*" {
        AggregateTarget::Star
    } else {
        ident::check_identifier(column)?;
        AggregateTarget::Column(column.to_string())
    };

    Ok(BoundAggregateItem::bind(func, target, schema)?)
}

/// `validated`（`aggregate` op のスキーマ検証済み要求本文）を `schema` へ
/// 束縛し、[`BoundAggregate`] を得る（TASK-186・NOSQL-4。SQL テキストを
/// 一切組み立てない）。`group_by`／`having`／`explain: true` の拒否は
/// [`reject_not_yet_supported`] を呼び出し元（[`execute`]）が先に行う契約
/// のため、ここでは繰り返さない。
pub fn bind(
    validated: &Validated<'_>,
    schema: &TableSchema,
) -> Result<BoundAggregate, AggregateError> {
    let items_json = validated.required_array("aggregates")?;
    // `Vec` 確保・`String` 複製より前に件数上限を検査する
    // （`engine::sql::allowlist::check_aggregate_item_count`。
    // `.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」
    // 対応。`BoundAggregate::new` も同じ検査を行うが、`items` を組み立てる
    // 前に打ち切るための多層防御）。
    engine::sql::allowlist::check_aggregate_item_count(items_json.len())
        .map_err(AggregateError::Engine)?;

    let mut items = Vec::with_capacity(items_json.len());
    for item in items_json {
        items.push(bind_item(item, schema)?);
    }

    let metadata_filters = match validated.optional_array("filter")? {
        Some(filter_items) => filter::bind_filter(filter_items, schema)?,
        None => Vec::new(),
    };

    let bound = BoundAggregate::new(schema.name.clone(), items, metadata_filters, Vec::new())?;
    Ok(bound)
}

/// `validated`（`aggregate` op のスキーマ検証済み要求本文）を `engine` 上で
/// 実行する。`principal` の [`SessionPrincipal::policy_context`] のみから
/// RLS 境界（テナント）を導出し、`table`／`aggregates[].column` の識別子
/// 形状検査・`group_by`／`having`／`explain: true` の拒否をここで行う。
pub fn execute(
    engine: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<engine::sql::exec::QueryResult, AggregateError> {
    reject_not_yet_supported(validated)?;

    let table = validated.required_str("table")?;
    ident::check_identifier(table)?;

    let session = SessionState::default();
    let result = engine.execute_bound_aggregate_in_session(
        principal.policy_context(),
        &session,
        table,
        |schema, _udfs| bind(validated, schema).map_err(to_sql_surface_error),
    )?;
    Ok(result)
}

/// binder closure（`FnOnce(...) -> Result<BoundAggregate, SqlSurfaceError>`）の
/// 戻り値型に [`AggregateError`] をそのまま渡せないため、`Engine`
/// （engine 側の分類をそのまま持つ）・`Filter(FilterError::Bind(_))`
/// （`declarative_filter::bind_all` 由来。`22000`／`54000` 等の分類を保つ）は
/// 中身の `SqlSurfaceError` をそのまま使う。それ以外（形状・関数語彙・
/// 識別子形状・`filter` の演算子語彙／RLS 述語名違反）はいずれも
/// `SqlSurfaceError::UnsupportedSyntax` 自身と同じ分類（`42601`）のため、
/// `SqlSurfaceError::UnsupportedSyntax { detail }` として復元してよい
/// （`SqlSurfaceError` は `pub enum`（`#[non_exhaustive]` 無し）のため
/// variant を engine クレート外から直接構築できる。`EngineCore::
/// execute_bound_aggregate_in_session` は binder のエラーをそのまま
/// 呼び出し元へ返す契約のため、[`execute`] 側の `From<SqlSurfaceError>
/// for AggregateError` により最終的な `wire_code`／`client_message` は
/// ここでの分類のまま保たれる）。
fn to_sql_surface_error(err: AggregateError) -> SqlSurfaceError {
    match err {
        AggregateError::Engine(inner) | AggregateError::Filter(FilterError::Bind(inner)) => inner,
        other => SqlSurfaceError::UnsupportedSyntax {
            detail: other.client_message(),
        },
    }
}

/// `POST /v1/query`（`op: aggregate`）を実行し HTTP 応答バイト列を返す
/// （[`super::gate::handle`] の `Op::Aggregate` 分岐から呼ばれる唯一の入口）。
/// 成功時は [`super::response::encode`]（`QueryResult` → JSON 本文）を経て
/// `200 OK`、失敗時は [`AggregateError::error_class`]／[`AggregateError::
/// client_message`] を [`crate::http::response::encode_error`] へ渡す。
/// `response::encode` 自身の失敗（非有限値。`ResponseEncodeError`）は
/// `XX000` へ写像する（多層防御。engine 側は評価時に非有限を `22000` で
/// 拒否する契約だが、[`super::response`] モジュール自身の契約として
/// `ErrorClass::InternalError` を用いる）。
pub fn handle(
    engine: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute(engine, principal, validated) {
        Ok(result) => match super::response::encode(&result) {
            Ok(body) => crate::http::response::encode_ok(&body, now_wall),
            Err(encode_err) => crate::http::response::encode_error(
                encode_err.error_class(),
                &encode_err.client_message(),
                now_wall,
            ),
        },
        Err(err) => {
            crate::http::response::encode_error(err.error_class(), &err.client_message(), now_wall)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, ColumnType};
    use engine::json::{parse_json, JsonValue};

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    fn obj(json: &str) -> JsonValue {
        parse_json(json).expect("test fixture must be valid JSON")
    }

    // `Validated<'a>` は `value` への借用を保持するため、fixture の `JsonValue`
    // をテスト側で先に確保してから `AGGREGATE_SCHEMA.validate` を呼ぶ
    // （`schema.rs` の既存テストと同じ流儀）。
    macro_rules! validated_aggregate {
        ($name:ident, $json:expr) => {
            let value = obj($json);
            let $name = super::super::schema::AGGREGATE_SCHEMA
                .validate(&value)
                .expect("schema fixture must be valid");
        };
    }

    #[test]
    fn parse_function_accepts_five_lowercase_names() {
        use engine::sql::allowlist::AggregateFunc;
        assert_eq!(parse_function("count"), Some(AggregateFunc::Count));
        assert_eq!(parse_function("sum"), Some(AggregateFunc::Sum));
        assert_eq!(parse_function("avg"), Some(AggregateFunc::Avg));
        assert_eq!(parse_function("min"), Some(AggregateFunc::Min));
        assert_eq!(parse_function("max"), Some(AggregateFunc::Max));
    }

    #[test]
    fn parse_function_rejects_case_variants_and_unknown() {
        for raw in ["COUNT", "Count", " count", "count ", "total", ""] {
            assert_eq!(parse_function(raw), None, "raw={raw:?}");
        }
    }

    #[test]
    fn bind_accepts_star_count_and_column_forms() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"},{"fn":"min","column":"lang"}]}"#
        );
        let bound = bind(&v, &schema()).expect("bind should succeed");
        assert_eq!(bound.items().len(), 2);
    }

    #[test]
    fn bind_rejects_star_with_non_count_function() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"*"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("SUM(*) must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_unknown_function_name() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"COUNT","column":"id"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("unknown fn must be rejected");
        assert!(matches!(err, AggregateError::UnsupportedFunction));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_malformed_identifier_column() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"do cs"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("malformed identifier must be rejected");
        assert!(matches!(err, AggregateError::InvalidIdentifier));
        assert_eq!(err.wire_code(), "42601");
        // untrusted な列名文字列を含まない固定文言であること。
        assert_eq!(err.client_message(), "invalid identifier");
    }

    #[test]
    fn bind_rejects_over_limit_item_count_before_binding() {
        let items: Vec<String> = (0..=engine::sql::allowlist::MAX_AGGREGATE_ITEMS)
            .map(|_| r#"{"fn":"count","column":"*"}"#.to_string())
            .collect();
        let json = format!(
            r#"{{"op":"aggregate","table":"docs","aggregates":[{}]}}"#,
            items.join(",")
        );
        validated_aggregate!(v, &json);
        let err = bind(&v, &schema()).expect_err("over-limit must be rejected");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn bind_item_rejects_malformed_shape_as_multi_layer_defense() {
        // `AGGREGATE_SCHEMA.validate` は `aggregates[]` を `AGGREGATE_ITEM_SCHEMA`
        // 経由で既に検証済みのため、`bind()` 経由では本パスに到達しない。
        // `bind_item` 単体が呼ばれた場合（多層防御。`filter.rs::map_filter_item`
        // と同じ判断）でも形状不正を独立に検出することを固定する。
        let malformed = obj(r#"{"fn":"count"}"#);
        let err = bind_item(&malformed, &schema()).expect_err("missing column must be rejected");
        assert!(matches!(err, AggregateError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_applies_filter_matching_declarative_filter_bind_all() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "filter":[{"column":"lang","op":"eq","value":"ja"}]}"#
        );
        let bound = bind(&v, &schema()).expect("bind should succeed");
        assert_eq!(bound.metadata_filters().len(), 1);
    }

    #[test]
    fn bind_rejects_unknown_column_with_22000() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"nope"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("unknown column must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_vector_column_for_sum_with_22000_but_accepts_count() {
        validated_aggregate!(
            v_sum,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"embedding"}]}"#
        );
        let err = bind(&v_sum, &schema()).expect_err("SUM(VECTOR) must be rejected");
        assert_eq!(err.wire_code(), "22000");

        validated_aggregate!(
            v_count,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"embedding"}]}"#
        );
        bind(&v_count, &schema()).expect("COUNT(VECTOR) must be accepted");
    }

    #[test]
    fn reject_not_yet_supported_detects_group_by_having_and_explain() {
        for json in [
            r#"{"op":"aggregate","table":"docs","aggregates":[],"group_by":[]}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[],"having":[]}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[],"explain":true}"#,
        ] {
            validated_aggregate!(v, json);
            let err = reject_not_yet_supported(&v).expect_err("must reject: {json}");
            assert!(matches!(err, AggregateError::NotYetSupported));
            assert_eq!(err.wire_code(), "0A000");
        }
    }

    #[test]
    fn reject_not_yet_supported_allows_plain_aggregate() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],
               "explain":false}"#
        );
        assert!(reject_not_yet_supported(&v).is_ok());
    }
}
