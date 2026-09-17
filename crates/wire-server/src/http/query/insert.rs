//! `POST /v1/query` の `insert` op を SQL 表層の `INSERT ... USING OPERATION_ID`
//! （SQL-10）と同じ engine 書き込み契約へ写像するモジュール（Issue #771・
//! TASK-178・対象ビヘイビア NOSQL-6。ポインタ: `docs/spec/05-tasks.md` TASK-178・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-6）。
//!
//! 責務境界: [`super::schema::INSERT_SCHEMA`] が形（必須キー・型）を検証済みの
//! JSON オブジェクトから `table`／`rows`／`operation_id` を取り出し、
//! [`bind_rows`]（純関数・engine 非依存）で `engine::sql::parser::BoundInsert` の
//! 列へ束縛したうえで、[`execute`] が
//! `engine::core::EngineCore::execute_bound_insert_in_session`（Issue #728 と同型の
//! セッション対応エントリ）へ委譲する。SQL テキストを一切生成せず、第 2 の実行器も
//! 作らない（`filter.rs`・`docs/design/scalar-index-mask-search.md` と同じ設計方針）。
//!
//! テナントは `principal`（[`crate::http::session::middleware::SessionPrincipal`]。
//! 唯一の入口）からのみ導出し、JSON からテナント相当の値を読む経路をシグネチャ上
//! 持たない（`security.md` P0）。可視性は engine 側 [`engine::sql::exec::execute_insert_batch`]
//! が常に `Private` 固定で書き込む（SQL-10 `execute_insert` と同じ fail-closed 判断）。
//!
//! `operation_id` の欠落・`null`・空文字はいずれも `23502`
//! （[`engine::sql::using_operation_id::OperationId::parse`] が空文字を句の省略と
//! 同等に扱う契約をそのまま透過する）。同一 `operation_id` への再送は台帳照合
//! （TASK-101・RECOVER-10）により内容一致なら `23505`・不一致なら `22023` へ収束する
//! （`execute_insert_batch`・`insert_typed_rows_unchecked` のドキュメント参照）。
//! INDEX-4 の 4 上限（バッチ相当の判定を「1 要求の `rows` 行数」に読み替えたもの）は
//! `EngineCore::execute_bound_insert_in_session` が判定する。
//!
//! `table` は兄弟 op（`search`／`scan`／`aggregate`）と同じ識別子形状検査
//! （[`super::ident::check_identifier`]）を engine へ渡す前に適用する（`42601`。
//! Cursor Bugbot 指摘・PR #823）。
//!
//! 対象外: `gate.rs` の placeholder 置換・`Router` への `EngineCore` 注入・成功応答
//! JSON（`{"inserted", "operation_id"}`）への写像（後続 Issue の担当）。

use engine::catalog::{ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::InsertOutcome;
use engine::sql::parser::BoundInsert;

use crate::http::session::middleware::SessionPrincipal;

use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};

/// [`bind_row`] が返す `InsertError::Bind` 用の `SqlSurfaceError::InvalidInput`
/// 構築ヘルパー。`SqlSurfaceError::invalid_input`（切り詰め処理を持つコンストラクタ）
/// は `pub(crate)`（engine クレート内限定）のため、wire-server からは列挙子の
/// フィールドを直接構築する（`InvalidInput` variant は enum の可視性に従い pub）。
/// 固定文言（`&'static str` リテラル）のみを渡す前提のため切り詰めは行わない。
fn invalid_input_error(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::InvalidInput {
        detail: detail.to_string(),
    }
}

/// [`bind_rows`]・[`execute`] の失敗を表す。いずれも [`ClassifiedError`] を実装し
/// `wire_code`／`client_message` を経由して HTTP エラー応答へ射影される
/// （`filter.rs::FilterError` と同じ設計）。
#[derive(Debug, Clone)]
pub enum InsertError {
    /// `rows` の形（`schema.rs` の意味的検証を通過した後の、行要素単位の検証）が
    /// 不正。`SchemaError` は untrusted な値・キー名を保持しない固定文言。
    Shape(SchemaError),
    /// `table` が識別子として意味を持ちうる形状（[`super::ident::check_identifier`]）
    /// を満たさない。兄弟 op（`search`／`scan`／`aggregate`）と同じ検査を `table` を
    /// `EngineCore::execute_bound_insert_in_session` へ渡す前に適用する（`42601`）。
    /// この検査がないと、NUL 等の制御文字を含む `table` 文字列が engine 側の
    /// `validate_identifier`（`22000`／`42P01`／`XX000` のいずれかに丸められる）まで
    /// 素通りし、兄弟 op と異なる `wire_code` として露出しうる（Cursor Bugbot 指摘・
    /// PR #823）。
    InvalidIdentifier,
    /// `engine::sql::parser::BoundInsert` への束縛時のエラー（未知列・型不一致・
    /// 次元不一致・非 nullable 列の欠落等。`22000`）。
    Bind(SqlSurfaceError),
    /// `EngineCore::execute_bound_insert_in_session` 側のエラー（`operation_id`
    /// 必須化・台帳照合・INDEX-4 上限・テーブル不存在等）をそのまま透過する。
    Exec(SqlSurfaceError),
}

impl From<SchemaError> for InsertError {
    fn from(err: SchemaError) -> Self {
        InsertError::Shape(err)
    }
}

impl From<InvalidIdentifier> for InsertError {
    fn from(_err: InvalidIdentifier) -> Self {
        InsertError::InvalidIdentifier
    }
}

impl ClassifiedError for InsertError {
    fn error_class(&self) -> ErrorClass {
        match self {
            InsertError::Shape(err) => err.error_class(),
            InsertError::InvalidIdentifier => ErrorClass::UnsupportedSqlSyntax,
            InsertError::Bind(err) | InsertError::Exec(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            InsertError::Shape(err) => err.client_message(),
            InsertError::InvalidIdentifier => "invalid identifier".to_string(),
            InsertError::Bind(err) | InsertError::Exec(err) => err.client_message(),
        }
    }
}

/// `rows` 配列の要素 1 件（`JsonValue::Object` 前提。非オブジェクトは
/// `InsertError::Shape`）から `schema` の列順に対応する `Vec<Value>` と行キー
/// `id` を取り出す。`sql::parser::bind_insert` の束縛規則を JSON 入力向けに
/// 焼き直したもので、束縛結果の意味（未知列・型不一致・非 nullable 列欠落は
/// すべて `22000`）は SQL 表層と同一に保つ。
///
/// untrusted な列名・値をエラー文言へ埋め込まない（JSON 文字列はバックスラッシュ
/// `\u0000` エスケープ経由で NUL を含み得るため、`error_response::encode` の NUL
/// 拒否で `XX000` へ縮退しうる。`schema.rs::SchemaError::UnknownKey`・
/// `filter.rs::FilterError::UnsupportedOperator` と同じ判断）。
fn bind_row(item: &JsonValue, schema: &TableSchema) -> Result<(u64, Vec<Value>), InsertError> {
    let JsonValue::Object(map) = item else {
        return Err(InsertError::Bind(invalid_input_error(
            "INSERT row must be a JSON object",
        )));
    };

    // `id` 疑似列: `engine::json::JsonNumber::PosInt`（小数点・指数部を含まない
    // 非負整数リテラルで、パース時点で `u64` として無損失にパース済みの表現）
    // のときのみ受理する。SQL 表層 `sql::parser::bind_insert` が SQL リテラルの
    // 生テキストから `u64` を `.parse()` するのと同じ「テキスト → 整数」の
    // 精度を持たせるための判定であり、`u64` 全域を許容範囲とする（SQL 表層との
    // パリティ。`sql::parser::bind_insert` 参照）。
    //
    // 以前は `JsonValue::Number(f64)` の丸め後の値を `fract() == 0.0` 等で
    // 事後判定していたが、`f64` 変換自体が精度を失うため
    // `9007199254740993`（2^53+1）のような safe range 境界直上の整数が
    // 最近傍の偶数 `9007199254740992` へ丸められ、上限検査をすり抜けて
    // 別の行 id へ書き込まれ得た（PR #823 レビュー指摘）。`JsonNumber` は
    // [`engine::json::JsonParser::parse_number`] が `f64` へ変換する前に
    // 整数リテラルを分類するため、この丸めが発生しない。
    //
    // `NegInt`（負の整数）・`Float`（小数点／指数部を含むリテラル。整数値に
    // 丸められる小数——例: `1.0`——を含む）はいずれも `id` として拒否する
    // （`id` は非負整数のみが妥当なため。文字列形の `id` も受理しない）。
    let id: u64 = match map.get("id") {
        Some(JsonValue::Number(n)) => match n.as_exact_u64() {
            Some(id) => id,
            None => {
                return Err(InsertError::Bind(invalid_input_error(
                    "INSERT row id must be a non-negative integer JSON number",
                )))
            }
        },
        _ => {
            return Err(InsertError::Bind(invalid_input_error(
                "INSERT row id must be a non-negative integer JSON number",
            )))
        }
    };

    let mut values: Vec<Value> = vec![Value::Null; schema.columns.len()];
    let mut provided = vec![false; schema.columns.len()];

    for (key, raw) in map.iter() {
        if key == "id" {
            continue;
        }
        let Some(col_idx) = schema.columns.iter().position(|c| &c.name == key) else {
            return Err(InsertError::Bind(invalid_input_error(
                "INSERT row references an unknown column",
            )));
        };
        let column = &schema.columns[col_idx];
        let value = match (column.ty, raw) {
            (ColumnType::Text, JsonValue::String(s)) => Value::Text(s.clone()),
            (ColumnType::Text, JsonValue::Null) if column.nullable => Value::Null,
            (ColumnType::Vector(dim), JsonValue::Array(items)) => {
                if items.len() != dim as usize {
                    return Err(InsertError::Bind(invalid_input_error(
                        "INSERT row VECTOR column length does not match the table dimension",
                    )));
                }
                let mut vec_values: Vec<f32> = Vec::with_capacity(items.len());
                for item in items {
                    let JsonValue::Number(n) = item else {
                        return Err(InsertError::Bind(invalid_input_error(
                            "INSERT row VECTOR column element must be a JSON number",
                        )));
                    };
                    // `n.as_f64() as f32`（`str -> f64 -> f32` の 2 回丸め）ではなく
                    // `as_f32()`（保持した生リテラル文字列を SQL 表層
                    // `sql::parser::parse_vector_literal` と同一の `str -> f32`
                    // 単一丸めで変換）を使う。同一リテラル・同一 `operation_id` を
                    // NoSQL・SQL 表層を跨いで再送した際、ここで丸めが食い違うと
                    // `content_hash` が一致せず「同一内容の再送」（`23505`）ではなく
                    // 「内容不一致」（`22023`）に誤判定されるため（Issue #771 レビュー
                    // 指摘対応）。
                    let f = n.as_f32().ok_or_else(|| {
                        InsertError::Bind(invalid_input_error(
                            "INSERT row VECTOR column element must be finite",
                        ))
                    })?;
                    vec_values.push(f);
                }
                Value::Vector(vec_values)
            }
            _ => {
                return Err(InsertError::Bind(invalid_input_error(
                    "INSERT row column value has an unexpected JSON type for its column",
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
        // VECTOR 列は `nullable` の値に関わらず常に必須として扱う。
        // `tenant::insert_typed_rows_unchecked`（実行層。`EngineCore::
        // execute_bound_insert_in_session` から呼ばれる）が VECTOR 列の値を
        // 無条件に `Value::Vector` として要求し、欠落・`Null` を
        // `CatalogError::Invalid` で拒否する契約のため（`sql::parser::
        // bind_insert` の SQL 表層束縛と同じ既存の制約で、schema 上
        // `nullable: true` の VECTOR 列を宣言できても実行層では意味を持たない）。
        // これを bind 層で先取りして検査しないと、明示 `null` は本関数の
        // 直前の `match` で `22000`（`InsertError::Bind`）になる一方、値を
        // 丸ごと省略した場合だけ実行層まで素通りし別の `wire_code` で
        // 拒否される非対称が生じる（PR #823 レビュー指摘）。
        let is_required = !column.nullable || matches!(column.ty, ColumnType::Vector(_));
        if !is_provided && is_required {
            return Err(InsertError::Bind(invalid_input_error(
                "INSERT row is missing a value for a required column",
            )));
        }
    }

    Ok((id, values))
}

/// `rows`（[`Validated::required_array`]`("rows")` が返す形。要素の型は
/// [`super::schema::INSERT_SCHEMA`] では `Any` としてしか検証されていないため
/// 各要素をここで検証する）を `table`／`operation_id` と合わせて
/// `Vec<BoundInsert>` へ束縛する。件数（`rows.len()`）は呼び出し元
/// （[`execute`]）が `EngineCore::execute_bound_insert_in_session` の row_count
/// 引数として先に渡し、①（件数上限）判定を通過した後にのみ本関数が呼ばれる。
pub fn bind_rows(
    items: &[JsonValue],
    table: &str,
    operation_id: Option<&OperationId>,
    schema: &TableSchema,
) -> Result<Vec<BoundInsert>, InsertError> {
    let mut bounds: Vec<BoundInsert> = Vec::new();
    bounds.try_reserve_exact(items.len()).map_err(|_| {
        InsertError::Bind(SqlSurfaceError::Internal {
            detail: "failed to reserve INSERT row buffer".to_string(),
        })
    })?;
    for item in items {
        let (id, values) = bind_row(item, schema)?;
        bounds.push(BoundInsert {
            table: table.to_string(),
            id,
            values,
            operation_id: operation_id.cloned(),
        });
    }
    Ok(bounds)
}

/// `POST /v1/query` の `insert` op を実行する。`validated` は
/// [`super::schema::ObjectSchema::validate`]（`super::schema::INSERT_SCHEMA` に対する呼び出し）
/// を通過済みの JSON オブジェクト。
///
/// `operation_id` の欠落・`null`・空文字はいずれも `OperationId::parse` により
/// `23502` へ収束する（`optional_str` は欠落・`null` の双方を `None` として返す
/// 契約のため、`None` を空文字と同じ扱いで `OperationId::parse("")` へ通す）。
pub fn execute(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<InsertOutcome, InsertError> {
    let table = validated
        .required_str("table")
        .map_err(InsertError::Shape)?;
    ident::check_identifier(table)?;
    let rows = validated
        .required_array("rows")
        .map_err(InsertError::Shape)?;
    let operation_id_raw = validated
        .optional_str("operation_id")
        .map_err(InsertError::Shape)?
        .unwrap_or("");
    let operation_id = OperationId::parse(operation_id_raw).map_err(InsertError::Exec)?;

    core.execute_bound_insert_in_session(
        principal.policy_context(),
        table,
        rows.len(),
        Some(&operation_id),
        |schema| {
            bind_rows(rows, table, Some(&operation_id), schema).map_err(|e| match e {
                InsertError::Bind(err) | InsertError::Exec(err) => err,
                // `bind_rows` は `InsertError::Shape`／`InsertError::InvalidIdentifier`
                // を構築しない（前者は `schema.rs` の意味的検証、後者は本関数冒頭の
                // `ident::check_identifier` がそれぞれ独立に検査する）。到達不能だが
                // `SqlSurfaceError` へ丸めて fail-closed のまま `match` を網羅する。
                InsertError::Shape(_) | InsertError::InvalidIdentifier => {
                    SqlSurfaceError::Internal {
                        detail: "unexpected shape error during INSERT row binding".to_string(),
                    }
                }
            })
        },
    )
    .map_err(InsertError::Exec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, TableSchema};
    use engine::json::parse_json;
    use engine::kernel::CpuScalarProvider;
    use engine::policy::PolicyContext;
    use engine::storage::Storage;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    fn rows_from(json: &str) -> Vec<JsonValue> {
        let JsonValue::Array(items) = parse_json(json).expect("valid JSON") else {
            panic!("expected array");
        };
        items
    }

    #[test]
    fn bind_rows_maps_id_and_columns() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let bounds = bind_rows(&items, "docs", None, &schema()).expect("bind ok");
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].id, 1);
        assert_eq!(bounds[0].table, "docs");
        assert_eq!(
            bounds[0].values,
            vec![
                Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ]
        );
    }

    #[test]
    fn bind_rows_rejects_non_object_row() {
        let items = rows_from(r#"["not-an-object"]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert!(matches!(err, InsertError::Bind(_)));
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_missing_id() {
        let items = rows_from(r#"[{"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_string_id() {
        let items = rows_from(r#"[{"id":"1","embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_unknown_column() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"lang":"ja","nope":"x"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_vector_dim_mismatch() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_non_finite_vector_element() {
        // JSON は NaN／Infinity を表現できないため（`engine::json::parse_json` が
        // 構文段階で拒否する）、ここでは非有限になり得る値そのものを渡せない。
        // 代わりに型不一致（文字列要素）で fail-closed 経路を固定する。
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,"x"],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_missing_non_nullable_column() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0]}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    // id 精度（PR #823 レビュー指摘対応）: `JsonNumber::PosInt` は `f64` へ丸める
    // 前の整数リテラルをそのまま `u64` として保持するため、2^53 を超える整数でも
    // 別の値へエイリアスせず正確に束縛されることを固定する。
    #[test]
    fn bind_rows_preserves_id_precision_beyond_f64_safe_integer_range() {
        // 2^53 + 1。旧実装（`f64` の丸め後に safe range 上限で判定）では
        // 最近傍の偶数 `9007199254740992`（2^53）へ丸められて上限検査を通過し、
        // 別の id の行として束縛され得た。
        let items = rows_from(r#"[{"id":9007199254740993,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let bounds = bind_rows(&items, "docs", None, &schema()).expect("bind ok");
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].id, 9_007_199_254_740_993);
    }

    #[test]
    fn bind_rows_preserves_id_precision_at_u64_max() {
        let body = format!(
            r#"[{{"id":{},"embedding":[1,0,0,0],"lang":"ja"}}]"#,
            u64::MAX
        );
        let items = rows_from(&body);
        let bounds = bind_rows(&items, "docs", None, &schema()).expect("bind ok");
        assert_eq!(bounds[0].id, u64::MAX);
    }

    #[test]
    fn bind_rows_rejects_negative_id() {
        let items = rows_from(r#"[{"id":-1,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_decimal_id_even_when_numerically_integral() {
        // `1.0` は数値としては整数だが、リテラルの構文上は小数点付き（`Float`
        // variant）であり、`id` は `PosInt`（整数リテラル）のみを受理する。
        let items = rows_from(r#"[{"id":1.0,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    // P2（nullable VECTOR 列省略時の bind/exec 非対称。PR #823 レビュー指摘対応）:
    // 明示 `null` は既存の `match` で拒否されるが、省略時も同じく `22000` で
    // 拒否されることを固定する（`tenant::insert_typed_rows_unchecked` が
    // VECTOR 列を無条件必須として扱う実行層の契約に bind 層を合わせる）。
    fn schema_with_nullable_vector() -> TableSchema {
        TableSchema::new(
            "docs_nullable_vec",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), true),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    #[test]
    fn bind_rows_rejects_omitted_nullable_vector_column() {
        let items = rows_from(r#"[{"id":1,"lang":"ja"}]"#);
        let err = bind_rows(
            &items,
            "docs_nullable_vec",
            None,
            &schema_with_nullable_vector(),
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_explicit_null_vector_column() {
        let items = rows_from(r#"[{"id":1,"embedding":null,"lang":"ja"}]"#);
        let err = bind_rows(
            &items,
            "docs_nullable_vec",
            None,
            &schema_with_nullable_vector(),
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_carries_operation_id_to_every_row() {
        let items = rows_from(
            r#"[{"id":1,"embedding":[1,0,0,0],"lang":"ja"},{"id":2,"embedding":[0,1,0,0],"lang":"en"}]"#,
        );
        let op_id = OperationId::parse("op-1").expect("valid operation_id");
        let bounds = bind_rows(&items, "docs", Some(&op_id), &schema()).expect("bind ok");
        assert_eq!(bounds.len(), 2);
        for b in &bounds {
            assert_eq!(b.operation_id, Some(op_id.clone()));
        }
    }

    fn open_core() -> (EngineCore, std::path::PathBuf) {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "wire-insert-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        (
            EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
            path,
        )
    }

    fn principal(tenant: &str) -> SessionPrincipal {
        use crate::http::headers::{parse_headers, HeaderParse};
        use crate::http::session::middleware;
        use crate::http::session::store::SessionStore;
        use std::time::Instant;

        let sessions = SessionStore::new();
        let now = Instant::now();
        let ctx = PolicyContext::new(tenant).expect("valid ctx");
        let token = sessions.issue(ctx, now).expect("issue");
        let raw = format!(
            "Authorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
            token.encoded()
        );
        let leaked: &'static [u8] = Box::leak(raw.into_bytes().into_boxed_slice());
        let headers = match parse_headers(leaked).expect("header parse") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        };
        middleware::authenticate(&sessions, &headers, move || now).expect("authenticate")
    }

    #[test]
    fn execute_writes_rows_via_engine_core() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let outcome = execute(&core, &principal, &validated).expect("insert ok");
        assert_eq!(outcome.rows_affected, 1);
        let _ = std::fs::remove_file(&path);
    }

    // Cursor Bugbot 指摘（PR #823）: `table` が識別子形状検査
    // （`ident::check_identifier`）を通らずに engine へ渡ると、NUL 等の制御文字を
    // 含む値が `22000`／`42P01`／`XX000` のいずれかへ丸められ、兄弟 op（`search`／
    // `scan`／`aggregate`）と異なる `wire_code` として露出しうる。JSON 文字列は
    // `\u0000` エスケープ経由で NUL を表現できるため、この経路で `42601` が
    // 返ることを固定する。
    #[test]
    fn execute_rejects_table_name_containing_nul_as_invalid_identifier() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs\u0000","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_missing_operation_id_is_23502() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body =
            r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}]}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_null_operation_id_is_23502() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":null}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_empty_operation_id_is_23502() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":""}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_resend_same_operation_id_same_content_is_23505() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");
        execute(&core, &principal, &validated).expect("first insert ok");

        let err = execute(&core, &principal, &validated).expect_err("resend must fail");
        assert_eq!(err.wire_code(), "23505");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_resend_same_operation_id_different_content_is_22023() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let first = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let first_value = parse_json(first).expect("valid json");
        let first_validated = super::super::schema::INSERT_SCHEMA
            .validate(&first_value)
            .expect("schema ok");
        execute(&core, &principal, &first_validated).expect("first insert ok");

        let second = r#"{"op":"insert","table":"docs","rows":[{"id":2,"embedding":[0,1,0,0],"lang":"en"}],"operation_id":"op-1"}"#;
        let second_value = parse_json(second).expect("valid json");
        let second_validated = super::super::schema::INSERT_SCHEMA
            .validate(&second_value)
            .expect("schema ok");
        let err =
            execute(&core, &principal, &second_validated).expect_err("mismatched resend must fail");
        assert_eq!(err.wire_code(), "22023");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_multi_row_batch_shares_one_operation_id() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"},{"id":2,"embedding":[0,1,0,0],"lang":"en"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let outcome = execute(&core, &principal, &validated).expect("insert ok");
        assert_eq!(outcome.rows_affected, 2);
        let _ = std::fs::remove_file(&path);
    }

    // 表層横断の content_hash 一貫性（Issue #771 レビュー指摘対応）: SQL 表層
    // `execute_sql_in_session`（`sql::parser::parse_vector_literal`。`str -> f32`
    // 単一丸め）と NoSQL 表層 `execute`（`bind_row`。かつては
    // `JsonNumber::as_f64() as f32` の `str -> f64 -> f32` 2 回丸めだった）は
    // 同一のベクトルリテラル文字列を異なる丸めで `f32` へ変換しうるため、
    // 同一テナント・同一テーブル・同一 `id`・同一 `operation_id` で表層を跨いで
    // 再送しても `content_hash`（`recovery::content_hash::push_vector`）が
    // 一致せず「同一内容の再送」（`23505`）ではなく「内容不一致」（`22023`）と
    // 誤判定されうる。`1.0000000596046448` は 2 回丸め経路だと `1.0` になる一方
    // 単一丸め（`str -> f32` 直接パース）では異なるビットパターンになる値
    // （`json.rs::as_f32_matches_direct_str_parse_and_differs_from_f64_roundtrip`
    // と同じ数値）を使い、SQL → NoSQL・NoSQL → SQL の双方向で `23505` になる
    // ことを固定する。
    const DOUBLE_ROUNDING_LITERAL: &str = "1.0000000596046448";

    #[test]
    fn cross_surface_resend_same_vector_literal_is_23505_sql_then_nosql() {
        let (core, path) = open_core();
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut session = engine::sql::mode::SessionState::default();

        core.execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO docs (id, embedding, lang) VALUES (1, '[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4]', 'ja') USING OPERATION_ID 'op-cross-1'"
            ),
        )
        .expect("SQL insert ok");

        let http_principal = principal("tenant-a");
        let body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4],"lang":"ja"}}],"operation_id":"op-cross-1"}}"#
        );
        let value = parse_json(&body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &http_principal, &validated)
            .expect_err("resend of identical content across surfaces must be detected");
        assert_eq!(
            err.wire_code(),
            "23505",
            "SQL insert followed by identical NoSQL resend must be recognized as same-content"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cross_surface_resend_same_vector_literal_is_23505_nosql_then_sql() {
        let (core, path) = open_core();
        let http_principal = principal("tenant-a");
        let body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4],"lang":"ja"}}],"operation_id":"op-cross-2"}}"#
        );
        let value = parse_json(&body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");
        execute(&core, &http_principal, &validated).expect("NoSQL insert ok");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut session = engine::sql::mode::SessionState::default();
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!(
                    "INSERT INTO docs (id, embedding, lang) VALUES (1, '[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4]', 'ja') USING OPERATION_ID 'op-cross-2'"
                ),
            )
            .expect_err("resend of identical content across surfaces must be detected");
        assert_eq!(
            err.wire_code(),
            "23505",
            "NoSQL insert followed by identical SQL resend must be recognized as same-content"
        );
        let _ = std::fs::remove_file(&path);
    }
}
