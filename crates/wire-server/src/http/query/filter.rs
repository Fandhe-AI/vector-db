//! `POST /v1/query` の `filter` 配列（`search`／`scan`／`aggregate` 共通。
//! [`super::schema::FILTER_ITEM_SCHEMA`] で形の検証済み）を
//! `engine::declarative_filter::DeclarativeFilter` へ写像し、
//! `engine::declarative_filter::bind_all` を通して束縛済み `MetadataFilter` 列を
//! 得るモジュール（Issue #761・TASK-175・対象ビヘイビア NOSQL-7。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-7・EXT-3・SQL-2）。
//!
//! 責務境界: 受理する `op` は `eq`（[`engine::declarative_filter::DeclarativeFilter::equals`]）・
//! `prefix`（[`engine::declarative_filter::DeclarativeFilter::starts_with`]）の
//! 2 語彙のみで、複数要素は常に AND 結合として扱う（`or`・否定・範囲比較の構文は
//! そもそも受理形に存在しない）。SQL 表層（`sql::allowlist::Parser::parse_where`・
//! `sql::parser::bind_where_predicates`）と**同一の** `declarative_filter` 束縛へ
//! 委譲する第 2 の実行器は作らない。列名・スキーマ照合（未知列・`VECTOR` 列 →
//! `22000`）・リテラル長上限（`54000`）は `engine::declarative_filter::bind`／
//! `bind_all` の既存契約をそのまま透過させる。
//!
//! 呼び出し文脈: 後続 Issue（#763・#766・#768）の op 別ハンドラが
//! [`super::schema::Validated::optional_array`]`("filter")` で得た
//! `&[JsonValue]` を [`bind_filter`] へ渡し、`TableSchema` へ束縛済みの
//! `Vec<MetadataFilter>` を受け取って RLS 事前フィルタ通過後の可視行へ適用する
//! （`sql::exec::execute_statement` の SCALAR 段と同じ評価契約。
//! `engine::declarative_filter::matches_all` を呼ぶのは実行経路側の責務であり
//! 本モジュールの対象外）。
//!
//! 対象外: op 別ハンドラからの呼び出し結線・実行そのもの、HTTP 応答へのエラー
//! 射影（`http::status`／`http::error_body` が別途 `ErrorClass` から写像する）。

use engine::catalog::TableSchema;
use engine::declarative_filter::{self, DeclarativeFilter, MetadataFilter};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::{is_allowed_where_predicate_name, SqlSurfaceError};

use super::schema::{SchemaError, Validated, FILTER_ITEM_SCHEMA};

/// [`map_filter_items`]／[`bind_filter`] の失敗を表す。いずれも
/// [`ClassifiedError`] を実装し、`wire_code`／`client_message` を経由して
/// HTTP エラー応答へ射影される（呼び出し元が `to_string()` を使わない契約は
/// `SqlSurfaceError::client_message` の既存ドキュメントと同じ）。
#[derive(Debug, Clone)]
pub enum FilterError {
    /// `op` が許可語彙（`eq`／`prefix`）に厳密一致しない（大文字小文字読み替え
    /// なし。`or`・否定・範囲比較を含む語彙外の値はすべてここに落ちる）。
    /// untrusted な `op` 文字列は文言へ含めない固定文言（`security.md`
    /// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応。
    /// [`super::schema::SchemaError`] が `UnknownKey` でキー名を保持しないのと
    /// 同じ判断）。
    UnsupportedOperator,
    /// `column` が RLS 述語名（`is_allowed_where_predicate_name` が真。
    /// 例: `visible`／`visible()`、大文字小文字非区別）と一致した。RLS は
    /// サーバー側暗黙適用のみであり、`filter` 経由でクライアントが RLS 述語を
    /// 指定・解除できる経路を作らない（`security.md` P0「テナント境界」）。
    RlsPredicateNotAllowed,
    /// `filter` 要素が [`FILTER_ITEM_SCHEMA`] の形（必須キー・型）に適合しない。
    Shape(SchemaError),
    /// `engine::declarative_filter` 側の束縛エラー（件数上限超過 `54000`・
    /// 未知列／`VECTOR` 列／空 prefix `22000`）をそのまま透過する。
    Bind(SqlSurfaceError),
}

impl From<SchemaError> for FilterError {
    fn from(err: SchemaError) -> Self {
        FilterError::Shape(err)
    }
}

impl From<SqlSurfaceError> for FilterError {
    fn from(err: SqlSurfaceError) -> Self {
        FilterError::Bind(err)
    }
}

impl ClassifiedError for FilterError {
    fn error_class(&self) -> ErrorClass {
        match self {
            FilterError::UnsupportedOperator | FilterError::RlsPredicateNotAllowed => {
                ErrorClass::UnsupportedSqlSyntax
            }
            FilterError::Shape(err) => err.error_class(),
            FilterError::Bind(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            FilterError::UnsupportedOperator => {
                "unsupported filter operator (only \"eq\" and \"prefix\" are allowed)".to_string()
            }
            FilterError::RlsPredicateNotAllowed => {
                "filter column must not reference an RLS predicate name".to_string()
            }
            FilterError::Shape(err) => err.client_message(),
            FilterError::Bind(err) => err.client_message(),
        }
    }
}

/// `column` が RLS 述語呼び出し形の許可名（現状 `visible`）に一致するかを、
/// 末尾の `()` の有無を問わず大文字小文字非区別で判定する。
/// `engine::sql::allowlist::is_allowed_where_predicate_name` は述語名そのもの
/// （`()` を含まない）を受け取る契約のため、NoSQL 表層の `column` 文字列に
/// クライアントが `"visible()"` のように書いた場合も同じ判定へ正規化する。
fn is_rls_predicate_column(column: &str) -> bool {
    let name = column.strip_suffix("()").unwrap_or(column);
    is_allowed_where_predicate_name(name)
}

/// `filter` 配列の要素 1 件を [`DeclarativeFilter`] へ写像する。`op` は厳密一致
/// （trim・大文字小文字読み替えなし）で判定し、`eq`／`prefix` 以外はすべて
/// [`FilterError::UnsupportedOperator`] とする。
fn map_filter_item(item: &JsonValue) -> Result<DeclarativeFilter, FilterError> {
    // 上位スキーマ（呼び出し元の `optional_array` 経由）の検証結果を信頼し
    // きらず、要素単位でも再検証する（多層防御。`schema.rs` の
    // `check_element_type` が `ElementType::Object` 経由で既に検証している
    // 経路とは独立に、本モジュール単体で呼ばれても安全な契約にする）。
    let validated: Validated<'_> = FILTER_ITEM_SCHEMA.validate(item)?;
    let column = validated.required_str("column")?;
    let op = validated.required_str("op")?;
    let value = validated.required_str("value")?;

    if is_rls_predicate_column(column) {
        return Err(FilterError::RlsPredicateNotAllowed);
    }

    match op {
        "eq" => Ok(DeclarativeFilter::equals(column, value)),
        "prefix" => Ok(DeclarativeFilter::starts_with(column, value)),
        _ => Err(FilterError::UnsupportedOperator),
    }
}

/// `items`（`filter` 配列。[`Validated::optional_array`]`("filter")` が返す形）を
/// 未束縛の [`DeclarativeFilter`] 列へ写像する。
///
/// 件数検査（[`declarative_filter::check_filter_count`]、`54000`）を `Vec`
/// 確保・`String` 複製より**前**に行う（`.claude/rules/security.md`
/// 「不安全な設計｜無制限リソース確保（DoS）」対応。`items` は JSON パーサの
/// コンテナ要素数上限〔65,536〕までメモリ上に存在しうるため、写像前の件数
/// 検査は必須）。
pub fn map_filter_items(items: &[JsonValue]) -> Result<Vec<DeclarativeFilter>, FilterError> {
    declarative_filter::check_filter_count(items.len())?;
    let mut mapped = Vec::with_capacity(items.len());
    for item in items {
        mapped.push(map_filter_item(item)?);
    }
    Ok(mapped)
}

/// [`map_filter_items`] の結果を `schema` へ束縛し、SQL 表層の
/// `WHERE <col> = '<lit>' AND <col> LIKE '<prefix>%'` と同一の
/// `Vec<MetadataFilter>` を得る（宣言順を保った AND 結合。列名解決・`TEXT`
/// 列限定・リテラル長上限は `engine::declarative_filter::bind_all` の既存
/// 契約に委ねる）。
pub fn bind_filter(
    items: &[JsonValue],
    schema: &TableSchema,
) -> Result<Vec<MetadataFilter>, FilterError> {
    let declared = map_filter_items(items)?;
    let bound = declarative_filter::bind_all(&declared, schema)?;
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, ColumnType};
    use engine::json::parse_json;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
            ],
        )
    }

    fn filter_items(json: &str) -> Vec<JsonValue> {
        let JsonValue::Array(items) = parse_json(json).expect("valid JSON") else {
            panic!("expected array");
        };
        items
    }

    #[test]
    fn eq_maps_to_declarative_equals() {
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":"ja"}]"#);
        let mapped = map_filter_items(&items).expect("map ok");
        assert_eq!(mapped, vec![DeclarativeFilter::equals("lang", "ja")]);
    }

    #[test]
    fn prefix_maps_to_declarative_starts_with() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":"src/"}]"#);
        let mapped = map_filter_items(&items).expect("map ok");
        assert_eq!(mapped, vec![DeclarativeFilter::starts_with("path", "src/")]);
    }

    #[test]
    fn prefix_value_wildcards_are_kept_literal() {
        // `parse_prefix_pattern`（SQL `LIKE` 側）とは異なり、NoSQL `filter` の
        // `value` は生の prefix でありワイルドカード解釈をしない
        // （`DeclarativeFilter::starts_with` は `str::starts_with` のみ）。
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":"a%b_c"}]"#);
        let mapped = map_filter_items(&items).expect("map ok");
        assert_eq!(
            mapped,
            vec![DeclarativeFilter::starts_with("path", "a%b_c")]
        );
    }

    #[test]
    fn multiple_items_preserve_order_as_and() {
        let items = filter_items(
            r#"[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"prefix","value":"src/"}]"#,
        );
        let mapped = map_filter_items(&items).expect("map ok");
        assert_eq!(
            mapped,
            vec![
                DeclarativeFilter::equals("lang", "ja"),
                DeclarativeFilter::starts_with("path", "src/"),
            ]
        );
    }

    #[test]
    fn empty_array_maps_to_empty_vec() {
        let items = filter_items("[]");
        assert_eq!(map_filter_items(&items).expect("map ok"), vec![]);
    }

    #[test]
    fn unsupported_operators_are_rejected() {
        for op in [
            "or", "not", "neq", "gt", "lt", "gte", "lte", "between", "range", "in", "EQ", "Prefix",
            "visible",
        ] {
            let items = filter_items(&format!(r#"[{{"column":"lang","op":"{op}","value":"x"}}]"#));
            let err = map_filter_items(&items).expect_err("must reject");
            assert!(matches!(err, FilterError::UnsupportedOperator));
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn rls_predicate_columns_are_rejected() {
        for column in ["visible", "VISIBLE", "visible()", "Visible()"] {
            let items = filter_items(&format!(
                r#"[{{"column":"{column}","op":"eq","value":"x"}}]"#
            ));
            let err = map_filter_items(&items).expect_err("must reject");
            assert!(matches!(err, FilterError::RlsPredicateNotAllowed));
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn malformed_shape_is_rejected() {
        // 非オブジェクト要素。
        let items = filter_items(r#"["not-an-object"]"#);
        let err = map_filter_items(&items).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");

        // 欠落キー。
        let items = filter_items(r#"[{"column":"lang","op":"eq"}]"#);
        let err = map_filter_items(&items).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));

        // 未知キー。
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":"ja","extra":"x"}]"#);
        let err = map_filter_items(&items).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));

        // 型不一致（`value` が数値）。
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":1}]"#);
        let err = map_filter_items(&items).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
    }

    #[test]
    fn over_limit_count_is_rejected_by_map_filter_items() {
        let items: Vec<JsonValue> = (0..=declarative_filter::MAX_METADATA_FILTERS)
            .map(|i| {
                let mut map = std::collections::BTreeMap::new();
                map.insert("column".to_string(), JsonValue::String("lang".to_string()));
                map.insert("op".to_string(), JsonValue::String("eq".to_string()));
                map.insert("value".to_string(), JsonValue::String(i.to_string()));
                JsonValue::Object(map)
            })
            .collect();
        let err = map_filter_items(&items).expect_err("must reject");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn at_limit_count_is_accepted() {
        let items: Vec<JsonValue> = (0..declarative_filter::MAX_METADATA_FILTERS)
            .map(|i| {
                let mut map = std::collections::BTreeMap::new();
                map.insert("column".to_string(), JsonValue::String("lang".to_string()));
                map.insert("op".to_string(), JsonValue::String("eq".to_string()));
                map.insert("value".to_string(), JsonValue::String(i.to_string()));
                JsonValue::Object(map)
            })
            .collect();
        assert_eq!(
            map_filter_items(&items).expect("map ok").len(),
            declarative_filter::MAX_METADATA_FILTERS
        );
    }

    #[test]
    fn bind_filter_rejects_unknown_column() {
        let items = filter_items(r#"[{"column":"nope","op":"eq","value":"x"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_filter_rejects_vector_column() {
        let items = filter_items(r#"[{"column":"embedding","op":"eq","value":"x"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_filter_rejects_empty_prefix() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":""}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_filter_matches_declarative_filter_bind_all() {
        let items = filter_items(
            r#"[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"prefix","value":"src/"}]"#,
        );
        let via_wire = bind_filter(&items, &schema()).expect("bind ok");
        let via_engine = declarative_filter::bind_all(
            &[
                DeclarativeFilter::equals("lang", "ja"),
                DeclarativeFilter::starts_with("path", "src/"),
            ],
            &schema(),
        )
        .expect("bind ok");
        assert_eq!(via_wire, via_engine);
    }
}
