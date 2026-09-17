//! `POST /v1/query` の JSON クエリオブジェクトに対する意味的検証ヘルパー
//! （Issue #760。対象ビヘイビア TASK-175・NOSQL-8・HTTP-7。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-2〜NOSQL-10）。
//!
//! 責務境界: `engine::json::parse_json` は構文解析（untrusted テキスト →
//! [`engine::json::JsonValue`]）までが責務で、「意味的な検証は呼び出し元が
//! 行う」契約になっている（`json.rs` モジュールドキュメント参照）。本モジュール
//! はその呼び出し元側の共通部品で、op ごとに宣言した [`ObjectSchema`] に照らし
//! 「必須キー欠落」「未知キー」「型不一致（`null` を含む）」の 3 類型を検査する。
//! いずれも [`SchemaError`] として `42601`（`UnsupportedSqlSyntax`）へ fail-closed
//! に写像する（`engine::json::JsonError` と同じ分類。構文エラー・意味エラーが
//! クライアントには同じ `wire_code` として収束する）。
//!
//! op 名の許可リスト・未知 op の `0A000` 判定は [`super::op::Op`]
//! （Issue #759・TASK-179・NOSQL-1・NOSQL-9）が担い、[`schema_for`] は
//! `super::op::Op::parse` へ委譲する薄い表引きに留まる。
//!
//! 対象外（後続 Issue の担当。二重実装しない）:
//! - フィールド値の**語彙・範囲**検査（`filter[].op` の `eq`／`prefix`、
//!   `aggregates[].fn` の関数名、`limit` の非負性等。#761・#763・#766・
//!   #768・#769）
//! - `insert` の `operation_id` 欠落／`null`／空文字 → `23502`、`rows[*]` の
//!   列検証（#771）
//!
//! 未知キーは無視せず拒否する（NOSQL-8 の一般則。クライアント自己申告の
//! `tenant_id`（HTTP-7）・`HINT ORDER`／`SET search_mode` 相当フィールド
//! （NOSQL-9）・`scan` への `vector`／`plan`／`mode`／`hybrid` 付与拒否
//! （NOSQL-3）は、いずれもこの一般則で自然に成立する）。

use std::collections::BTreeMap;

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;

/// [`ObjectSchema::validate`] の失敗を表す 3 類型。いずれも
/// [`ErrorClass::UnsupportedSqlSyntax`]（`42601`）へ写像する。
///
/// - `key` はスキーマ表由来の `&'static str`（本モジュールのソースコードに
///   埋め込まれた定数のみ）であり、untrusted なクライアント入力のキー名を
///   保持しない。特に [`SchemaError::UnknownKey`] はキー名を一切保持しない
///   （JSON 文字列はバックスラッシュ `\u0000` エスケープ経由で NUL を含み
///   得るため、untrusted キー名をエラー文言へ埋め込むと
///   `error_response::encode` の NUL 拒否で `XX000` へ縮退しうる。
///   security.md の「エラー・ログ経由で他テナントのデータ・存在情報を
///   漏らさない」方針にも沿い、固定文言のみとする）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaError {
    /// 必須フィールドが欠落している。
    MissingRequired { key: &'static str },
    /// スキーマに存在しないフィールドが含まれている。
    UnknownKey,
    /// フィールドの JSON 型が期待と一致しない（`null` を含む）。
    TypeMismatch { key: &'static str },
}

impl SchemaError {
    /// SQLSTATE 風 `wire_code`（`.claude/rules/coding-rust.md`）。
    /// [`ClassifiedError`] へ委譲する（`engine::json::JsonError::wire_code` と
    /// 同じパターン。実装型ごとに再定義しない）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }
}

impl ClassifiedError for SchemaError {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::UnsupportedSqlSyntax
    }

    fn client_message(&self) -> String {
        match self {
            SchemaError::MissingRequired { key } => format!("missing required field: {key}"),
            SchemaError::UnknownKey => "unknown field in request object".to_string(),
            SchemaError::TypeMismatch { key } => format!("type mismatch for field: {key}"),
        }
    }
}

/// フィールドの必須／任意を表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Required,
    Optional,
}

/// 配列要素に期待する JSON 型（[`FieldType::Array`] が使う）。
#[derive(Debug, Clone, Copy)]
pub enum ElementType {
    /// 要素の型を検査しない（`insert` の `rows[*]` のように任意列名・任意型を
    /// 持つ要素向け）。
    Any,
    String,
    Number,
    /// 要素が [`ObjectSchema`] に従うオブジェクトであることを再帰検証する。
    Object(&'static ObjectSchema),
}

/// フィールドに期待する JSON 型（値の範囲・語彙は検査しない。型のみ）。
#[derive(Debug, Clone, Copy)]
pub enum FieldType {
    Bool,
    Number,
    String,
    Array(ElementType),
    /// 値が [`ObjectSchema`] に従うオブジェクトであることを再帰検証する。
    Object(&'static ObjectSchema),
}

/// スキーマ 1 フィールドの宣言。
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    pub key: &'static str,
    pub presence: Presence,
    pub ty: FieldType,
    /// `true` のとき `null` を受理する（既定 `false` = `null` は
    /// [`SchemaError::TypeMismatch`]。fail-closed をデフォルトとするための
    /// 明示的な opt-in）。
    pub nullable: bool,
}

/// 1 つの JSON オブジェクトが満たすべきスキーマ（op トップレベル、または
/// ネストしたサブオブジェクト双方に使う）。
#[derive(Debug, Clone, Copy)]
pub struct ObjectSchema {
    /// 診断・エラー文言（ルート非オブジェクト時の [`SchemaError::TypeMismatch`]
    /// の `key`）に使う名前。
    pub name: &'static str,
    pub fields: &'static [FieldSpec],
}

/// フィールド 1 つの値を型検査する（[`ObjectSchema::validate`] から呼ばれる
/// 内部ヘルパー）。`null` は `nullable` を見て個別に処理するため、ここへは
/// 非 `Null` の値のみが渡ってくる前提。
fn check_field_type(
    value: &JsonValue,
    ty: &FieldType,
    key: &'static str,
) -> Result<(), SchemaError> {
    match (ty, value) {
        (FieldType::Bool, JsonValue::Bool(_)) => Ok(()),
        (FieldType::Number, JsonValue::Number(_)) => Ok(()),
        (FieldType::String, JsonValue::String(_)) => Ok(()),
        (FieldType::Array(elem_ty), JsonValue::Array(items)) => {
            for item in items {
                check_element_type(item, elem_ty, key)?;
            }
            Ok(())
        }
        (FieldType::Object(schema), JsonValue::Object(_)) => {
            schema.validate(value)?;
            Ok(())
        }
        _ => Err(SchemaError::TypeMismatch { key }),
    }
}

/// 配列要素 1 つを型検査する（[`check_field_type`] の `Array` 分岐から呼ばれる）。
/// 要素の型不一致も、包含するフィールドの `key`（配列全体のキー名）で
/// 報告する（要素の添字までは公開しない。固定文言の方針に沿う）。
fn check_element_type(
    value: &JsonValue,
    elem_ty: &ElementType,
    key: &'static str,
) -> Result<(), SchemaError> {
    match (elem_ty, value) {
        (ElementType::Any, _) => Ok(()),
        (ElementType::String, JsonValue::String(_)) => Ok(()),
        (ElementType::Number, JsonValue::Number(_)) => Ok(()),
        (ElementType::Object(schema), JsonValue::Object(_)) => {
            schema.validate(value)?;
            Ok(())
        }
        _ => Err(SchemaError::TypeMismatch { key }),
    }
}

impl ObjectSchema {
    /// `value` がこのスキーマに従うことを検証する。判定順序は
    /// 「必須欠落 → 型不一致 → 未知キー」に固定し、同一入力には常に同一の
    /// 単一エラーを返す（決定的）。
    ///
    /// - ルートが `Object` でなければ `TypeMismatch { key: self.name }`。
    /// - 各 [`FieldSpec`] について、キー欠落かつ `Required` なら
    ///   `MissingRequired`。値が `Null` なら `nullable` を見て通過／
    ///   `TypeMismatch`。それ以外は [`check_field_type`] で型照合する
    ///   （`Object`／`Array(Object(_))` は再帰的に本メソッドへ委譲する。
    ///   ネスト先でも未知キー拒否が成立する）。
    /// - 最後に、スキーマに存在しないキーが 1 つでもあれば `UnknownKey`
    ///   （`fields` は高々十数個の静的スライスのため線形探索で十分。
    ///   `BTreeSet` 等の追加アロケーションは行わない）。
    pub fn validate<'a>(&'static self, value: &'a JsonValue) -> Result<Validated<'a>, SchemaError> {
        let JsonValue::Object(map) = value else {
            return Err(SchemaError::TypeMismatch { key: self.name });
        };

        for field in self.fields {
            match map.get(field.key) {
                None => {
                    if field.presence == Presence::Required {
                        return Err(SchemaError::MissingRequired { key: field.key });
                    }
                }
                Some(JsonValue::Null) => {
                    if !field.nullable {
                        return Err(SchemaError::TypeMismatch { key: field.key });
                    }
                }
                Some(v) => {
                    check_field_type(v, &field.ty, field.key)?;
                }
            }
        }

        for key in map.keys() {
            if !self.fields.iter().any(|f| f.key == key.as_str()) {
                return Err(SchemaError::UnknownKey);
            }
        }

        Ok(Validated { map, schema: self })
    }
}

/// [`ObjectSchema::validate`] を通過したオブジェクトへの型付きアクセサ。
/// 借用のみでアロケーションを行わない。各アクセサは要求されたキー・型を
/// スキーマに照らして再検査し、スキーマ外のキーや型不一致の要求は `Err`
/// を返す（呼び出し元の誤用に対する多層防御。`validate` の検査結果を信頼
/// しきらない）。
#[derive(Debug, Clone, Copy)]
pub struct Validated<'a> {
    map: &'a BTreeMap<String, JsonValue>,
    schema: &'static ObjectSchema,
}

impl<'a> Validated<'a> {
    /// この値の検証元スキーマ。
    pub fn schema(&self) -> &'static ObjectSchema {
        self.schema
    }

    fn field_spec(&self, key: &'static str) -> Option<&'static FieldSpec> {
        self.schema.fields.iter().find(|f| f.key == key)
    }

    pub fn required_str(&self, key: &'static str) -> Result<&'a str, SchemaError> {
        self.optional_str(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_str(&self, key: &'static str) -> Result<Option<&'a str>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::String) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::String(s)) => Ok(Some(s.as_str())),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    pub fn required_number(&self, key: &'static str) -> Result<f64, SchemaError> {
        self.optional_number(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_number(&self, key: &'static str) -> Result<Option<f64>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Number) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Number(n)) => Ok(Some(*n)),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    pub fn required_bool(&self, key: &'static str) -> Result<bool, SchemaError> {
        self.optional_bool(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_bool(&self, key: &'static str) -> Result<Option<bool>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Bool) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Bool(b)) => Ok(Some(*b)),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    pub fn required_array(&self, key: &'static str) -> Result<&'a [JsonValue], SchemaError> {
        self.optional_array(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_array(
        &self,
        key: &'static str,
    ) -> Result<Option<&'a [JsonValue]>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Array(_)) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Array(items)) => Ok(Some(items.as_slice())),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    pub fn required_object(
        &self,
        key: &'static str,
    ) -> Result<&'a BTreeMap<String, JsonValue>, SchemaError> {
        self.optional_object(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_object(
        &self,
        key: &'static str,
    ) -> Result<Option<&'a BTreeMap<String, JsonValue>>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Object(_)) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Object(m)) => Ok(Some(m)),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }
}

/// `hybrid` フィールドのサブスキーマ（`search` op）。
pub static HYBRID_SCHEMA: ObjectSchema = ObjectSchema {
    name: "hybrid",
    fields: &[FieldSpec {
        key: "text",
        presence: Presence::Required,
        ty: FieldType::String,
        nullable: false,
    }],
};

/// `filter` 配列要素のサブスキーマ（`search`／`scan`／`aggregate` 共通）。
pub static FILTER_ITEM_SCHEMA: ObjectSchema = ObjectSchema {
    name: "filter_item",
    fields: &[
        FieldSpec {
            key: "column",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "value",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// `aggregates` 配列要素のサブスキーマ（`aggregate` op）。
pub static AGGREGATE_ITEM_SCHEMA: ObjectSchema = ObjectSchema {
    name: "aggregate_item",
    fields: &[
        FieldSpec {
            key: "fn",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "column",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// `having` 配列要素のサブスキーマ（`aggregate` op）。
pub static HAVING_ITEM_SCHEMA: ObjectSchema = ObjectSchema {
    name: "having_item",
    fields: &[
        FieldSpec {
            key: "fn",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "column",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "value",
            presence: Presence::Required,
            ty: FieldType::Number,
            nullable: false,
        },
    ],
};

/// `search` op のトップレベルスキーマ（NOSQL-2・NOSQL-3・NOSQL-9・NOSQL-10
/// ポインタ）。`vector`／`plan` の排他は #763 が担う（本ヘルパーは型のみ
/// 検査するため両方同時に指定した入力も通過し得る）。
pub static SEARCH_SCHEMA: ObjectSchema = ObjectSchema {
    name: "search",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "limit",
            presence: Presence::Required,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "vector",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Number),
            nullable: false,
        },
        FieldSpec {
            key: "plan",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "columns",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&FILTER_ITEM_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "hybrid",
            presence: Presence::Optional,
            ty: FieldType::Object(&HYBRID_SCHEMA),
            nullable: false,
        },
        FieldSpec {
            key: "mode",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "explain",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
    ],
};

/// `scan` op のトップレベルスキーマ（NOSQL-3 ポインタ）。`vector`／`plan`／
/// `mode`／`hybrid` は未知キーとして `42601` で拒否される（スキーマに宣言
/// しないことで一般則から自然に成立する。個別の除外ロジックは持たない）。
pub static SCAN_SCHEMA: ObjectSchema = ObjectSchema {
    name: "scan",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "limit",
            presence: Presence::Required,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&FILTER_ITEM_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "columns",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "explain",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
    ],
};

/// `aggregate` op のトップレベルスキーマ（NOSQL-4〜NOSQL-7 ポインタ）。
/// `group_by`／`having` の意味検証は #769 が担う。
pub static AGGREGATE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "aggregate",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "aggregates",
            presence: Presence::Required,
            ty: FieldType::Array(ElementType::Object(&AGGREGATE_ITEM_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&FILTER_ITEM_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "group_by",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "having",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&HAVING_ITEM_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "explain",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
    ],
};

/// `insert` op のトップレベルスキーマ（NOSQL-6 ポインタ）。`operation_id` の
/// 欠落／`null`／空文字 → `23502`、`rows[*]` の列検証は #771 が担う（本
/// ヘルパーでは `operation_id` を Required にせず、`rows` は任意列名を
/// 持つため `Array(Any)` として型のみ検査する）。
pub static INSERT_SCHEMA: ObjectSchema = ObjectSchema {
    name: "insert",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "rows",
            presence: Presence::Required,
            ty: FieldType::Array(ElementType::Any),
            nullable: false,
        },
        FieldSpec {
            key: "operation_id",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: true,
        },
    ],
};

/// op 名（厳密一致。trim・大文字小文字の読み替えはしない）→ [`ObjectSchema`]
/// の対応表。`schema_for` の実体であり、単一情報源として扱う。
pub const OP_SCHEMAS: [(&str, &ObjectSchema); 4] = [
    ("search", &SEARCH_SCHEMA),
    ("scan", &SCAN_SCHEMA),
    ("aggregate", &AGGREGATE_SCHEMA),
    ("insert", &INSERT_SCHEMA),
];

/// `op` 名からスキーマを引く。語彙外は `None`（`0A000` への写像・応答は
/// [`super::op::classify_op`] の呼び出し元が行う。本モジュールは表引きの
/// みで、実体は [`super::op::Op::parse`] への委譲）。
pub fn schema_for(op: &str) -> Option<&'static ObjectSchema> {
    super::op::Op::parse(op).map(super::op::Op::schema)
}

/// スキーマ本体（`op` 以外のキー）を検証する前段として、トップレベルから
/// `op` 文字列だけを取り出す入口。ルートが `Object` でない場合・`op` が
/// 欠落／非文字列の場合を区別して報告する。
pub fn extract_op(value: &JsonValue) -> Result<&str, SchemaError> {
    let JsonValue::Object(map) = value else {
        return Err(SchemaError::TypeMismatch { key: "$" });
    };
    match map.get("op") {
        None => Err(SchemaError::MissingRequired { key: "op" }),
        Some(JsonValue::String(s)) => Ok(s.as_str()),
        Some(_) => Err(SchemaError::TypeMismatch { key: "op" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::error_format::ErrorClass;
    use engine::json::parse_json;

    fn obj(json: &str) -> JsonValue {
        parse_json(json).expect("test fixture must be valid JSON")
    }

    // --- 表網羅 ---------------------------------------------------------

    #[test]
    fn op_schemas_cover_expected_ops_and_schema_for_matches() {
        let ops: Vec<&str> = OP_SCHEMAS.iter().map(|(name, _)| *name).collect();
        assert_eq!(ops, vec!["search", "scan", "aggregate", "insert"]);
        for (name, schema) in OP_SCHEMAS.iter() {
            assert!(std::ptr::eq(schema_for(name).unwrap(), *schema));
        }
    }

    #[test]
    fn op_schemas_all_require_op_and_table() {
        for (name, schema) in OP_SCHEMAS.iter() {
            let op_spec = schema.fields.iter().find(|f| f.key == "op");
            let table_spec = schema.fields.iter().find(|f| f.key == "table");
            assert!(
                matches!(
                    op_spec,
                    Some(FieldSpec {
                        presence: Presence::Required,
                        ty: FieldType::String,
                        ..
                    })
                ),
                "op {name} must declare required string `op`"
            );
            assert!(
                matches!(
                    table_spec,
                    Some(FieldSpec {
                        presence: Presence::Required,
                        ty: FieldType::String,
                        ..
                    })
                ),
                "op {name} must declare required string `table`"
            );
        }
    }

    #[test]
    fn op_schemas_have_no_duplicate_field_keys() {
        for (name, schema) in OP_SCHEMAS.iter() {
            let mut seen: Vec<&str> = Vec::new();
            for field in schema.fields {
                assert!(
                    !seen.contains(&field.key),
                    "op {name} declares duplicate key {}",
                    field.key
                );
                seen.push(field.key);
            }
        }
    }

    #[test]
    fn schema_for_returns_none_for_unknown_vocabulary() {
        assert!(schema_for("select").is_none());
        assert!(schema_for("SEARCH").is_none());
        assert!(schema_for("").is_none());
        assert!(schema_for("searchx").is_none());
    }

    // --- 受理ケース -------------------------------------------------------

    #[test]
    fn search_accepts_full_valid_object() {
        let v = obj(
            r#"{"op":"search","table":"docs","limit":10,"vector":[0.1,0.2],
               "plan":"find x","columns":["id","body"],
               "filter":[{"column":"lang","op":"eq","value":"ja"}],
               "hybrid":{"text":"x"},"mode":"precision","explain":true}"#,
        );
        assert!(SEARCH_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn scan_accepts_full_valid_object() {
        let v = obj(r#"{"op":"scan","table":"docs","limit":500,
               "filter":[{"column":"lang","op":"eq","value":"ja"}],
               "columns":["id"],"explain":false}"#);
        assert!(SCAN_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn aggregate_accepts_full_valid_object() {
        let v = obj(r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"id"}],
               "filter":[{"column":"lang","op":"eq","value":"ja"}],
               "group_by":["lang"],
               "having":[{"fn":"count","column":"id","op":"gt","value":1}],
               "explain":true}"#);
        assert!(AGGREGATE_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn insert_accepts_full_valid_object() {
        let v = obj(
            r#"{"op":"insert","table":"docs","rows":[{"id":"1","body":"x"}],
               "operation_id":"op-1"}"#,
        );
        assert!(INSERT_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn insert_accepts_missing_and_null_operation_id() {
        let missing = obj(r#"{"op":"insert","table":"docs","rows":[]}"#);
        assert!(INSERT_SCHEMA.validate(&missing).is_ok());
        let null_id = obj(r#"{"op":"insert","table":"docs","rows":[],"operation_id":null}"#);
        assert!(INSERT_SCHEMA.validate(&null_id).is_ok());
    }

    // --- 必須欠落 ---------------------------------------------------------

    #[test]
    fn search_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "limit"] {
            let full = obj(r#"{"op":"search","table":"docs","limit":10}"#);
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            let err = SEARCH_SCHEMA.validate(&v).unwrap_err();
            assert_eq!(err, SchemaError::MissingRequired { key: missing });
            assert_eq!(err.wire_code(), "42601");
            assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
        }
    }

    #[test]
    fn scan_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "limit"] {
            let full = obj(r#"{"op":"scan","table":"docs","limit":10}"#);
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            assert_eq!(
                SCAN_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::MissingRequired { key: missing }
            );
        }
    }

    #[test]
    fn aggregate_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "aggregates"] {
            let full = obj(
                r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
            );
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            assert_eq!(
                AGGREGATE_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::MissingRequired { key: missing }
            );
        }
    }

    #[test]
    fn insert_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "rows"] {
            let full = obj(r#"{"op":"insert","table":"docs","rows":[]}"#);
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            assert_eq!(
                INSERT_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::MissingRequired { key: missing }
            );
        }
    }

    // --- 未知キー ---------------------------------------------------------

    #[test]
    fn unknown_key_is_rejected_per_op() {
        let cases: [(&'static ObjectSchema, &str); 4] = [
            (
                &SEARCH_SCHEMA,
                r#"{"op":"search","table":"docs","limit":1,"bogus":1}"#,
            ),
            (
                &SCAN_SCHEMA,
                r#"{"op":"scan","table":"docs","limit":1,"bogus":1}"#,
            ),
            (
                &AGGREGATE_SCHEMA,
                r#"{"op":"aggregate","table":"docs","aggregates":[],"bogus":1}"#,
            ),
            (
                &INSERT_SCHEMA,
                r#"{"op":"insert","table":"docs","rows":[],"bogus":1}"#,
            ),
        ];
        for (schema, json) in cases {
            let v = obj(json);
            assert_eq!(schema.validate(&v).unwrap_err(), SchemaError::UnknownKey);
        }
    }

    #[test]
    fn tenant_id_self_declaration_is_rejected_as_unknown_key() {
        // HTTP-7 ポインタ: クライアント自己申告の tenant_id はテナント文脈
        // (PolicyContext) を上書きする経路になってはならず、未知キーとして
        // 一律 42601 で拒否する。
        for (schema, json) in [
            (
                &SEARCH_SCHEMA,
                r#"{"op":"search","table":"docs","limit":1,"tenant_id":"evil"}"#,
            ),
            (
                &INSERT_SCHEMA,
                r#"{"op":"insert","table":"docs","rows":[],"tenant_id":"evil"}"#,
            ),
        ] {
            let v = obj(json);
            assert_eq!(schema.validate(&v).unwrap_err(), SchemaError::UnknownKey);
        }
    }

    #[test]
    fn hint_order_and_search_mode_style_fields_are_rejected_as_unknown_keys() {
        // NOSQL-9 ポインタ: SQL 表層の HINT ORDER／SET search_mode に相当する
        // フィールドを JSON トップレベルへ自己申告しても未知キーとして拒否
        // される（`mode` は search 専用の既存フィールドとして受理されるが、
        // それ以外の別名は拒否）。
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"hint_order":"path"}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::UnknownKey
        );
        let v2 = obj(r#"{"op":"search","table":"docs","limit":1,"search_mode":"precision"}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v2).unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    #[test]
    fn scan_rejects_search_only_fields() {
        // NOSQL-3 ポインタ: scan は vector/plan/mode/hybrid を受理しない。
        for field_json in [
            r#""vector":[0.1]"#,
            r#""plan":"x""#,
            r#""mode":"precision""#,
            r#""hybrid":{"text":"x"}"#,
        ] {
            let json = format!(r#"{{"op":"scan","table":"docs","limit":1,{field_json}}}"#);
            let v = obj(&json);
            assert_eq!(
                SCAN_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::UnknownKey
            );
        }
    }

    #[test]
    fn unknown_key_containing_escaped_nul_is_rejected_without_leaking_key_name() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"a\u0000b":1}"#);
        let err = SEARCH_SCHEMA.validate(&v).unwrap_err();
        assert_eq!(err, SchemaError::UnknownKey);
        let msg = err.client_message();
        assert!(
            !msg.contains('\u{0}'),
            "message must not leak untrusted key bytes: {msg:?}"
        );
        assert_eq!(msg, "unknown field in request object");
    }

    // --- 型不一致 ---------------------------------------------------------

    #[test]
    fn search_type_mismatches_are_rejected() {
        let cases = [
            (r#"{"op":"search","table":"docs","limit":"10"}"#, "limit"),
            (
                r#"{"op":"search","table":"docs","limit":1,"columns":"id"}"#,
                "columns",
            ),
            (
                r#"{"op":"search","table":"docs","limit":1,"hybrid":[]}"#,
                "hybrid",
            ),
            (
                r#"{"op":"search","table":"docs","limit":1,"explain":1}"#,
                "explain",
            ),
            (
                r#"{"op":"search","table":"docs","limit":1,"vector":["a"]}"#,
                "vector",
            ),
        ];
        for (json, key) in cases {
            let v = obj(json);
            assert_eq!(
                SEARCH_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::TypeMismatch { key },
                "input={json}"
            );
        }
    }

    #[test]
    fn non_nullable_field_rejects_null() {
        let v = obj(r#"{"op":"search","table":"docs","limit":null}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "limit" }
        );
        let v2 = obj(r#"{"op":null,"table":"docs","limit":1}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v2).unwrap_err(),
            SchemaError::TypeMismatch { key: "op" }
        );
    }

    // --- ネスト -------------------------------------------------------

    #[test]
    fn nested_hybrid_rejects_unknown_key() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"hybrid":{"text":"x","extra":1}}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    #[test]
    fn nested_filter_item_reports_missing_required() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"filter":[{"column":"a"}]}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::MissingRequired { key: "op" }
        );
    }

    #[test]
    fn nested_having_item_type_mismatch() {
        let v = obj(
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],
               "having":[{"fn":"count","column":"id","op":"gt","value":"1"}]}"#,
        );
        assert_eq!(
            AGGREGATE_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "value" }
        );
    }

    // --- ルート非オブジェクト ---------------------------------------------

    #[test]
    fn non_object_root_is_rejected() {
        for json in ["[]", "\"x\"", "1", "true", "null"] {
            let v = obj(json);
            let err = SEARCH_SCHEMA.validate(&v).unwrap_err();
            assert_eq!(err, SchemaError::TypeMismatch { key: "search" });
        }
    }

    // --- extract_op --------------------------------------------------------

    #[test]
    fn extract_op_returns_op_string() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1}"#);
        assert_eq!(extract_op(&v).unwrap(), "search");
    }

    #[test]
    fn extract_op_reports_missing_op() {
        let v = obj(r#"{"table":"docs"}"#);
        assert_eq!(
            extract_op(&v).unwrap_err(),
            SchemaError::MissingRequired { key: "op" }
        );
    }

    #[test]
    fn extract_op_reports_non_string_op() {
        let v = obj(r#"{"op":1,"table":"docs"}"#);
        assert_eq!(
            extract_op(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "op" }
        );
    }

    #[test]
    fn extract_op_reports_non_object_root() {
        let v = obj("[]");
        assert_eq!(
            extract_op(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "$" }
        );
    }

    // --- ClassifiedError ----------------------------------------------------

    #[test]
    fn all_schema_error_variants_classify_as_unsupported_sql_syntax() {
        let variants = [
            SchemaError::MissingRequired { key: "op" },
            SchemaError::UnknownKey,
            SchemaError::TypeMismatch { key: "limit" },
        ];
        for err in variants {
            assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
            assert_eq!(err.wire_code(), "42601");
        }
    }

    // --- Validated アクセサ ---------------------------------------------

    #[test]
    fn validated_accessors_read_present_and_absent_fields() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10,"explain":true}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(validated.required_str("op").unwrap(), "search");
        assert_eq!(validated.required_str("table").unwrap(), "docs");
        assert_eq!(validated.required_number("limit").unwrap(), 10.0);
        assert_eq!(validated.optional_bool("explain").unwrap(), Some(true));
        assert_eq!(validated.optional_str("plan").unwrap(), None);
        assert_eq!(validated.optional_array("vector").unwrap(), None);
        assert!(std::ptr::eq(validated.schema(), &SEARCH_SCHEMA));
    }

    #[test]
    fn validated_accessors_reject_schema_foreign_keys() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(
            validated.optional_str("not_a_field").unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    #[test]
    fn validated_accessors_reject_type_mismatched_requests() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        // `limit` はスキーマ上 Number なので required_str での取得は拒否する。
        assert_eq!(
            validated.required_str("limit").unwrap_err(),
            SchemaError::TypeMismatch { key: "limit" }
        );
    }

    #[test]
    fn validated_object_accessor_returns_nested_map() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"hybrid":{"text":"x"}}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        let hybrid = validated.required_object("hybrid").unwrap();
        assert_eq!(
            hybrid.get("text"),
            Some(&JsonValue::String("x".to_string()))
        );
    }

    // --- 判定順序の決定性 ---------------------------------------------------

    #[test]
    fn missing_required_takes_precedence_over_unknown_key() {
        // table 欠落 + 未知キー bogus の両方を含む入力は、必須欠落が先に
        // 検出される（フィールド走査を先に行うため）。
        let v = obj(r#"{"op":"search","limit":1,"bogus":1}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::MissingRequired { key: "table" }
        );
    }
}
