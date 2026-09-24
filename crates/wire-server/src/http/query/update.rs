//! `POST /v1/query` の `update` op を SQL 表層の
//! `UPDATE ... WHERE id = <n> USING OPERATION_ID`（SQL-17）と**同一の実行器**
//! （`engine::sql::exec::execute_update_with_schema`）へ、SQL テキストを
//! 組み立てずに束縛済み計画で到達させるモジュール（Issue #876・TASK-186・
//! 対象ビヘイビア NOSQL-6・NOSQL-12。ポインタ: `docs/spec/05-tasks.md`
//! TASK-178・TASK-186・`docs/spec/04-behavior/nosql-surface.md` NOSQL-6・
//! NOSQL-12・`docs/spec/04-behavior/sql-surface.md` SQL-17）。
//!
//! 責務境界: [`super::schema::UPDATE_SCHEMA`] が形を検証済みの JSON
//! オブジェクトから `table`／`set`／`where`（または `filter`）／
//! `operation_id` を取り出し、[`map_set_assignments`]（純関数・engine 非依存。
//! JSON `set` を `engine::sql::allowlist::InsertLiteral` 列へ写像する）と
//! [`super::dml_target::bind_target_form`]（`where`／`filter` の排他判定）で
//! 束縛したうえで、[`execute`] が
//! `engine::core::EngineCore::execute_bound_update_in_session`
//! （[`super::insert`] と同型のセッション対応エントリ）へ委譲する。
//! `engine::sql::parser::bind_update` が SET 対象の禁止列（`id`／`tenant_id`／
//! `visibility`）・重複列・未知列・型不一致の検査を担い、第 2 の実行器は
//! 作らない。
//!
//! テナントは `principal`（唯一の入口）からのみ導出する（`security.md` P0）。
//!
//! `set` の JSON → `InsertLiteral` 写像は **配列形のみ**を `VECTOR` 列の値と
//! して受理する（`insert` op と同じ「文字列形のベクトルリテラルは受理しない」
//! 非対称。`docs/design/nosql-update-delete-mapping.md` 参照）。`VECTOR` 列
//! 要素の `f32` 変換は `engine::json::JsonNumber::as_f32`（`insert.rs::
//! bind_row` と同一の単一丸め経路）を経由し、`NegInt(0)`（JSON `-0`）は
//! 明示的に `"-0"` として直列化する——SQL・NoSQL を跨いだ同一 `operation_id`
//! 再送時の `content_hash` 一致を保証するため
//! （`insert.rs::bind_row` のドキュメント参照）。
//!
//! `set` の各キーは engine へ渡す前に [`super::ident::check_identifier`]
//! （`42601`）を通す（`table` に対する既存の同判断・PR #823 と同じ理由。
//! untrusted な列名が engine 側のエラー文言へ埋め込まれて `XX000` へ縮退する
//! 経路を塞ぐ）。`set` が空オブジェクトの場合も `42601`（SQL 文法は SET 項目
//! ≥ 1 を要求する）。
//!
//! `operation_id` の欠落・`null`・空文字はいずれも `23502`
//! （`engine::sql::using_operation_id::OperationId::parse` の契約をそのまま
//! 透過する）。同一 `operation_id` への再送は台帳照合（TASK-101・
//! RECOVER-10）により内容一致なら `23505`・不一致なら `22023` へ収束する
//! （SQL 表層と同じ台帳キー空間 `(tenant, table, operation_id)` を共有する。
//! `crates/engine/tests/sql_update_delete_session_public_api.rs` で固定）。
//! 本モジュールの `set`（[`Validated::optional_object`] 由来の `BTreeMap`）は
//! キーがアルファベット順へ正規化されるが、内容一致判定に使う
//! `content_hash::for_update_columns` はスキーマ列順（`tenant::
//! update_row_columns_unchecked` が呼び出し前に正規化する）で計算されるため、
//! SQL 表層が非アルファベット順の宣言で書いた `UPDATE` と同一の
//! `operation_id`・同一内容で本 op から再送しても列の記述順の違いだけで
//! `22023` に誤判定されることはない（Issue #876 レビュー指摘の是正。
//! `crates/wire-server/tests/nosql12_update_delete.rs::
//! cross_surface_multi_column_set_declared_out_of_alphabetical_order_is_treated_as_duplicate`
//! で固定）。
//!
//! [`execute`] は成功時 [`UpdateSuccess`] を返し、[`handle`] が
//! [`encode_success_body`]（`{"updated":<n>,"operation_id":"<escaped>"}`。
//! キー順固定・空白なし）を経由して `gate.rs` の `Op::Update` アームへ
//! ディスパッチ可能な応答バイト列へ写像する（`insert::encode_success_body`
//! と同型）。他テナント所有 id・未存在 id はいずれも `updated:0`・`200`
//! （RLS-9。`execute_update_with_schema` のドキュメント参照）。
//!
//! 対象外: `filter`（述語形）の実行結線（Issue #871 の担当。本モジュールは
//! `filter` のみの要求を [`super::dml_target::DmlTargetError::
//! PredicateFormUnavailable`] で拒否する）・`RETURNING`（Issue #873）。

use std::fmt::Write as _;

use engine::catalog::{ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::{JsonNumber, JsonValue};
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::{InsertLiteral, SqlSurfaceError, ValidatedUpdate};
use engine::sql::exec::UpdateOutcome;
use engine::sql::parser::bind_update;

use crate::http::error_body::escape_json_string_into;
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::dml_target::{bind_target_form, DmlTargetError, TargetForm};
use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};

/// [`SqlSurfaceError::InvalidInput`] 構築ヘルパー（`insert.rs::
/// invalid_input_error` と同じ理由。`SqlSurfaceError::invalid_input`
/// コンストラクタは `pub(crate)`〔engine クレート内限定〕のため、wire-server
/// からは列挙子のフィールドを直接構築する）。固定文言（`&'static str`）のみ
/// を渡す前提のため切り詰めは行わない。
fn invalid_input_error(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::InvalidInput {
        detail: detail.to_string(),
    }
}

/// [`map_set_assignments`]・[`execute`] の失敗を表す。いずれも
/// [`ClassifiedError`] を実装する（`insert.rs::InsertError` と同じ設計）。
#[derive(Debug, Clone)]
pub enum UpdateError {
    /// [`Validated`] アクセサの型・キー不整合（多層防御）。
    Shape(SchemaError),
    /// `table`／`set` のキーが識別子として意味を持ちうる形状を満たさない。
    InvalidIdentifier,
    /// `where`／`filter` の排他判定・述語形未対応（[`super::dml_target`]）。
    Target(DmlTargetError),
    /// `set` が空オブジェクト（`42601`。SQL 文法が SET 項目 ≥ 1 を要求する
    /// ことと同じ判断）。
    EmptySet,
    /// `set` の JSON → `InsertLiteral` 写像エラー（値・型不一致等。
    /// `22000`。禁止列・未知列は wire 側で判定せず `bind_update`
    /// （`Engine` アーム）へ委譲する）。
    Set(&'static str),
    /// `engine::sql::parser::bind_update`（禁止列・未知列・型不一致）・
    /// `EngineCore::execute_bound_update_in_session`（`operation_id` 必須化・
    /// 台帳照合・テーブル不存在）のエラーをそのまま透過する。
    Engine(SqlSurfaceError),
    /// `BYTEA` 列の SET 値が base64 の JSON string でない、または不正な
    /// base64（`42601`。B10・Issue #886。`insert.rs::InsertError::InvalidBytea`
    /// と同じ分類判断）。
    InvalidBytea(&'static str),
    /// `BYTEA` 列の base64 値が復号後 [`engine::bytea::MAX_BYTEA_FIELD_LEN`] を
    /// 超える（`54000`）。
    ByteaTooLarge,
    /// `JSON`／`JSONB` 列の SET 値が JSON オブジェクト／配列でない、または
    /// 構文不正（`42601`。Issue #889 D6。`insert.rs::InsertError::InvalidJson`
    /// と同じ分類判断）。
    InvalidJson(&'static str),
    /// `JSON`／`JSONB` 列の SET 値が正規化後
    /// [`engine::json::MAX_JSON_FIELD_LEN`] を超える（`54000`）。
    JsonTooLarge,
}

impl From<SchemaError> for UpdateError {
    fn from(err: SchemaError) -> Self {
        UpdateError::Shape(err)
    }
}

impl From<InvalidIdentifier> for UpdateError {
    fn from(_err: InvalidIdentifier) -> Self {
        UpdateError::InvalidIdentifier
    }
}

impl From<DmlTargetError> for UpdateError {
    fn from(err: DmlTargetError) -> Self {
        UpdateError::Target(err)
    }
}

impl From<SqlSurfaceError> for UpdateError {
    fn from(err: SqlSurfaceError) -> Self {
        UpdateError::Engine(err)
    }
}

impl ClassifiedError for UpdateError {
    fn error_class(&self) -> ErrorClass {
        match self {
            UpdateError::Shape(err) => err.error_class(),
            UpdateError::InvalidIdentifier => ErrorClass::UnsupportedSqlSyntax,
            UpdateError::Target(err) => err.error_class(),
            UpdateError::EmptySet => ErrorClass::UnsupportedSqlSyntax,
            // `Set` の固定文言は値・型不一致（`22000`）に限る。禁止列
            // （`id`／`tenant_id`／`visibility`）・未知列は wire 側で判定せず
            // `bind_update` へ委譲するため、engine へ到達した場合は `Engine`
            // アーム側で分類される（`42601`／`22000`）。
            UpdateError::Set(_) => ErrorClass::InvalidInput,
            UpdateError::Engine(err) => err.error_class(),
            UpdateError::InvalidBytea(_) => ErrorClass::UnsupportedSqlSyntax,
            UpdateError::ByteaTooLarge => ErrorClass::PayloadTooLarge,
            UpdateError::InvalidJson(_) => ErrorClass::UnsupportedSqlSyntax,
            UpdateError::JsonTooLarge => ErrorClass::PayloadTooLarge,
        }
    }

    fn client_message(&self) -> String {
        match self {
            UpdateError::Shape(err) => err.client_message(),
            UpdateError::InvalidIdentifier => "invalid identifier".to_string(),
            UpdateError::Target(err) => err.client_message(),
            UpdateError::EmptySet => "SET clause must specify at least one column".to_string(),
            UpdateError::InvalidBytea(detail) => detail.to_string(),
            UpdateError::ByteaTooLarge => "BYTEA value exceeds the length limit".to_string(),
            UpdateError::InvalidJson(detail) => detail.to_string(),
            UpdateError::JsonTooLarge => "JSON value exceeds the length limit".to_string(),
            UpdateError::Set(detail) => detail.to_string(),
            UpdateError::Engine(err) => err.client_message(),
        }
    }
}

/// JSON 数値の配列（`VECTOR` 列の値。**配列形のみ**を受理する）を SQL
/// ベクトルリテラル文字列 `"[t1,t2,...]"` へ直列化する。各要素は
/// [`JsonNumber::as_f32`] が有限値として解釈できることを要求する（非有限・
/// 非数値要素は `Err`）。`NegInt(0)`（JSON `-0`）は `"-0"` として直列化し、
/// SQL 表層の `-0.0` 保持契約（`content_hash` 一致）と揃える
/// （`insert.rs::bind_row` の同種コメント参照）。
fn vector_literal_text(items: &[JsonValue]) -> Result<String, UpdateError> {
    let mut parts: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let JsonValue::Number(n) = item else {
            return Err(UpdateError::Set(
                "SET VECTOR column element must be a JSON number",
            ));
        };
        // 有限性は `as_f32` が確認済み（非有限は `None`）。生テキストの直列化
        // 自体は `PosInt`／`NegInt`（`-0` 特別扱い）／`Float`（保持済み生
        // テキスト）で分岐し、SQL 表層 `parse_vector_literal` が `str -> f32`
        // 単一丸めで再解釈できる形にする。
        if n.as_f32().is_none() {
            return Err(UpdateError::Set("SET VECTOR column element must be finite"));
        }
        let text = match n {
            JsonNumber::PosInt(v) => v.to_string(),
            JsonNumber::NegInt(0) => "-0".to_string(),
            JsonNumber::NegInt(v) => v.to_string(),
            JsonNumber::Float { text, .. } => text.to_string(),
        };
        parts.push(text);
    }
    Ok(format!("[{}]", parts.join(",")))
}

/// `set`（[`Validated::optional_object`]`("set")` が返す `BTreeMap`）を
/// `schema` の列型で分岐しながら `Vec<(String, InsertLiteral)>` へ写像する
/// （`insert.rs::bind_row` と同じ判定順序: 禁止列 → 列名解決 → 型）。
///
/// `set` の各キーは呼び出し元（[`execute`]）が [`ident::check_identifier`]
/// で先に形状検査済みであることを前提とする（本関数自身は形状を再検査
/// しない。`engine::sql::parser::bind_update` が改めて禁止列・未知列を
/// 検査するため、多層防御は保たれる）。
fn map_set_assignments(
    set: &std::collections::BTreeMap<String, JsonValue>,
    schema: &TableSchema,
) -> Result<Vec<(String, InsertLiteral)>, UpdateError> {
    if set.is_empty() {
        return Err(UpdateError::EmptySet);
    }
    let mut assignments: Vec<(String, InsertLiteral)> = Vec::with_capacity(set.len());
    for (key, raw) in set.iter() {
        // 禁止列（`id`／`tenant_id`／`visibility`）は `bind_update` が `42601`
        // で拒否する契約のため、ここでは列名解決を先に試み、`schema` に無い
        // 列名（禁止列を含む）はそのまま `bind_update` へ委譲する
        // （wire 側で先取りして `22000` に丸めない。禁止列専用の判定は
        // engine 側の単一情報源に保つ）。
        let column = schema.columns.iter().find(|c| &c.name == key);
        let Some(column) = column else {
            // 未知列（禁止列も含む可能性がある）は `bind_update` へそのまま
            // 委譲する必要があるため、ここでは型を特定できない。プレース
            // ホルダーの `InsertLiteral::String` を渡し、実際の判定は
            // `bind_update` に一任する（列名だけで完結する判定のため値の
            // 種類は結果に影響しない）。
            assignments.push((key.clone(), InsertLiteral::String(String::new())));
            continue;
        };
        let literal = match (&column.ty, raw) {
            (ColumnType::Text, JsonValue::String(s)) => InsertLiteral::String(s.clone()),
            // ENUM 列は TEXT と同じ JSON string 表現を使い、語彙検証は
            // `engine::sql::parser::bind_update`（`bind_enum_literal`）へ委譲する
            // （Issue #890。列名だけで完結しない値検証は engine 側の単一
            // 情報源に保つ設計。`bind_update` の再検査で多層防御が保たれる）。
            (ColumnType::Enum(_), JsonValue::String(s)) => InsertLiteral::String(s.clone()),
            (ColumnType::Enum(_), _) => {
                return Err(UpdateError::Set(
                    "SET ENUM column value must be a JSON string",
                ))
            }
            (ColumnType::Vector(_), JsonValue::Array(items)) => {
                InsertLiteral::String(vector_literal_text(items)?)
            }
            (ColumnType::Text, _) => {
                return Err(UpdateError::Set(
                    "SET TEXT column value must be a JSON string",
                ))
            }
            (ColumnType::Vector(_), _) => {
                return Err(UpdateError::Set(
                    "SET VECTOR column value must be a JSON array of numbers",
                ))
            }
            // F10（Issue #882 計画）: REAL/DOUBLE 列の JSON 束縛は #896 の担当。
            // 現時点では非対応列として一律拒否する（`22000`。既存の型不一致と
            // 同じ応答形へ合流させる）。
            (ColumnType::Real | ColumnType::Double, _) => {
                return Err(UpdateError::Set(
                    "SET REAL/DOUBLE PRECISION columns are not supported yet",
                ))
            }
            // BOOLEAN 列は JSON 真偽値のみ受理する（NOSQL-17 と同じ規則。
            // Issue #883）。
            (ColumnType::Boolean, JsonValue::Bool(b)) => InsertLiteral::Bool(*b),
            (ColumnType::Boolean, _) => {
                return Err(UpdateError::Set(
                    "SET BOOLEAN column value must be a JSON boolean",
                ))
            }
            // 配列列（TABLE-14・Issue #888）の JSON 配列束縛は本 Issue の対象外
            // （NoSQL 表層の JSON 配列束縛は #896・NOSQL-17 の担当）。BOOLEAN と
            // 同じく明示的に拒否する。
            (ColumnType::Array(_), _) => {
                return Err(UpdateError::Set(
                    "SET ARRAY column is not supported via the NoSQL surface",
                ))
            }
            // `BYTEA` 列は base64 の JSON string のみ受理し、復号したバイト列を
            // 正準形（`\x` ＋ 小文字 hex）の `InsertLiteral::String` へ再エンコード
            // する（B9・Issue #886）。engine 側の束縛経路を hex 解析の 1 本に
            // 保つ設計判断（`insert.rs::bind_row` とは異なり `InsertLiteral::Bytes`
            // variant を新設しない）。
            (ColumnType::Bytea, JsonValue::String(s)) => {
                let decoded =
                    super::base64_std::decode_base64_std(s, engine::bytea::MAX_BYTEA_FIELD_LEN)
                        .map_err(|e| match e {
                            super::base64_std::Base64StdError::TooLong => {
                                UpdateError::ByteaTooLarge
                            }
                            _ => UpdateError::InvalidBytea(
                                "SET BYTEA column value must be valid base64",
                            ),
                        })?;
                InsertLiteral::String(engine::bytea::format_hex_text(&decoded))
            }
            (ColumnType::Bytea, _) => {
                return Err(UpdateError::InvalidBytea(
                    "SET BYTEA column value must be a base64 JSON string",
                ))
            }
            // `JSON`／`JSONB` 列は `insert.rs::bind_row` と同じ規則で JSON
            // オブジェクト／配列を正規化テキストへ写像し、`InsertLiteral::String`
            // として engine の SQL 束縛経路（`bind_json_literal`）へ渡す
            // （B9・Issue #889 D6。BYTEA と同型の判断で束縛経路を 1 本に保つ）。
            (ColumnType::Json | ColumnType::Jsonb, JsonValue::Object(_) | JsonValue::Array(_)) => {
                let mut canonical = String::new();
                engine::json::write_canonical(raw, &mut canonical);
                if canonical.len() > engine::json::MAX_JSON_FIELD_LEN {
                    return Err(UpdateError::JsonTooLarge);
                }
                InsertLiteral::String(canonical)
            }
            // JSON `null` かつ nullable 列は SQL `NULL` として扱う
            // （`insert.rs::bind_row` の同型分岐・design doc
            // `docs/design/column-type-extension.md`「#889 追記」節「JSON
            // `null` は nullable 列なら `NULL`」と同じ契約。Issue #889
            // レビュー指摘・PR #1014）。非 nullable 列は次の catch-all 分岐で
            // 従来どおり `42601` へ倒れる（`bind_set_assignments` 側でも
            // `column.nullable` を再検査するが、ここで先に拒否することで
            // engine 側のエラー文言に依存せず wire-server 側の分類を保つ）。
            (ColumnType::Json | ColumnType::Jsonb, JsonValue::Null) if column.nullable => {
                InsertLiteral::Null
            }
            (ColumnType::Json | ColumnType::Jsonb, _) => {
                return Err(UpdateError::InvalidJson(
                    "SET JSON column value must be a JSON object or array",
                ))
            }
        };
        assignments.push((key.clone(), literal));
    }
    Ok(assignments)
}

/// `update` op 成功時の応答材料（[`encode_success_body`] の唯一の情報源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSuccess {
    pub updated: u64,
    pub operation_id: OperationId,
}

/// [`UpdateSuccess`] を成功応答本文（JSON）へ写像する（infallible）。
/// 出力形 `{"updated":<n>,"operation_id":"<escaped>"}`（`insert::
/// encode_success_body` と同型のキー順固定・空白なし規約）。
pub fn encode_success_body(success: &UpdateSuccess) -> String {
    let mut out = String::with_capacity(success.operation_id.as_str().len() + 64);
    out.push_str("{\"updated\":");
    let _ = write!(out, "{}", success.updated);
    out.push_str(",\"operation_id\":\"");
    escape_json_string_into(&mut out, success.operation_id.as_str());
    out.push_str("\"}");
    out
}

/// `POST /v1/query` の `update` op を実行する。`validated` は
/// [`super::schema::UPDATE_SCHEMA::validate`] を通過済みの JSON オブジェクト。
pub fn execute(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<UpdateSuccess, UpdateError> {
    let table = validated
        .required_str("table")
        .map_err(UpdateError::Shape)?;
    ident::check_identifier(table)?;

    let TargetForm::RowId(id) = bind_target_form(validated)?;

    let set = validated
        .required_object("set")
        .map_err(UpdateError::Shape)?;
    for key in set.keys() {
        ident::check_identifier(key)?;
    }

    let operation_id_raw = validated
        .optional_str("operation_id")
        .map_err(UpdateError::Shape)?
        .unwrap_or("");
    let operation_id = OperationId::parse(operation_id_raw)?;

    let outcome: UpdateOutcome = core.execute_bound_update_in_session(
        principal.policy_context(),
        table,
        Some(&operation_id),
        |schema| {
            let assignments = map_set_assignments(set, schema).map_err(|e| match e {
                UpdateError::Engine(err) => err,
                UpdateError::Set(detail) => invalid_input_error(detail),
                UpdateError::EmptySet => SqlSurfaceError::UnsupportedSyntax {
                    detail: "SET clause must specify at least one column".to_string(),
                },
                // `bind_target_form`／`Shape`／`InvalidIdentifier` はこの
                // closure より前に確定済みのため到達不能だが、fail-closed の
                // まま網羅する。
                UpdateError::Shape(_) | UpdateError::InvalidIdentifier | UpdateError::Target(_) => {
                    SqlSurfaceError::Internal {
                        detail: "unexpected error during UPDATE SET binding".to_string(),
                    }
                }
                // `InvalidBytea`（`42601`）／`ByteaTooLarge`（`54000`）の分類を
                // `SqlSurfaceError` へ写像しても維持する（B10・Issue #886）。
                UpdateError::InvalidBytea(detail) => SqlSurfaceError::UnsupportedSyntax {
                    detail: detail.to_string(),
                },
                UpdateError::ByteaTooLarge => SqlSurfaceError::PayloadTooLarge {
                    detail: "BYTEA value exceeds the length limit".to_string(),
                },
                // `InvalidJson`（`42601`）／`JsonTooLarge`（`54000`）の分類を
                // `SqlSurfaceError` へ写像しても維持する（Issue #889 D6）。
                UpdateError::InvalidJson(detail) => SqlSurfaceError::UnsupportedSyntax {
                    detail: detail.to_string(),
                },
                UpdateError::JsonTooLarge => SqlSurfaceError::PayloadTooLarge {
                    detail: "JSON value exceeds the length limit".to_string(),
                },
            })?;
            let stmt = ValidatedUpdate {
                table_name: table.to_string(),
                assignments,
                id_literal: id.to_string(),
                operation_id: Some(operation_id.clone()),
                // NoSQL 表層 `op: update` は `RETURNING` を公開しない
                // （Issue #876 のスコープ外。SQL 表層の `RETURNING`
                // 実行結線は Issue #873・SQL-21 が別途担当し、
                // `ValidatedUpdate::returning` を `Some` にする経路は
                // ここには無い）。
                returning: None,
            };
            bind_update(&stmt, schema)
        },
    )?;

    Ok(UpdateSuccess {
        updated: outcome.rows_affected,
        operation_id,
    })
}

/// `POST /v1/query` の `update` op を処理し応答バイト列を返す（`gate.rs` から
/// `engine` 接続済み時のみ呼ばれる。`insert::handle` と同一シグネチャ形）。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute(core, principal, validated) {
        Ok(success) => http_response::encode_ok(&encode_success_body(&success), now_wall),
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::ColumnDef;
    use engine::json::parse_json;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    fn set_map(json: &str) -> std::collections::BTreeMap<String, JsonValue> {
        let JsonValue::Object(m) = parse_json(json).expect("valid json") else {
            panic!("expected object");
        };
        m
    }

    #[test]
    fn map_set_assignments_maps_text_column() {
        let set = set_map(r#"{"lang":"en"}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![("lang".to_string(), InsertLiteral::String("en".to_string()))]
        );
    }

    #[test]
    fn map_set_assignments_maps_vector_array() {
        let set = set_map(r#"{"embedding":[1,2,3]}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "embedding".to_string(),
                InsertLiteral::String("[1,2,3]".to_string())
            )]
        );
    }

    #[test]
    fn map_set_assignments_preserves_negative_zero() {
        let set = set_map(r#"{"embedding":[-0,1,2]}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "embedding".to_string(),
                InsertLiteral::String("[-0,1,2]".to_string())
            )]
        );
    }

    #[test]
    fn map_set_assignments_preserves_float_text() {
        let set = set_map(r#"{"embedding":[1.5,2.25,3.0]}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "embedding".to_string(),
                InsertLiteral::String("[1.5,2.25,3.0]".to_string())
            )]
        );
    }

    #[test]
    fn map_set_assignments_rejects_string_for_vector_column() {
        let set = set_map(r#"{"embedding":"[1,2,3]"}"#);
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::Set(_))
        ));
    }

    #[test]
    fn map_set_assignments_rejects_number_for_text_column() {
        let set = set_map(r#"{"lang":1}"#);
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::Set(_))
        ));
    }

    #[test]
    fn map_set_assignments_rejects_null() {
        let set = set_map(r#"{"lang":null}"#);
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::Set(_))
        ));
    }

    #[test]
    fn map_set_assignments_rejects_bool_and_object() {
        for json in [r#"{"lang":true}"#, r#"{"lang":{}}"#] {
            let set = set_map(json);
            assert!(matches!(
                map_set_assignments(&set, &schema()),
                Err(UpdateError::Set(_))
            ));
        }
    }

    #[test]
    fn map_set_assignments_rejects_empty_set() {
        let set = set_map("{}");
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::EmptySet)
        ));
    }

    #[test]
    fn map_set_assignments_rejects_vector_dimension_mismatch_via_bind_update() {
        // 次元検証は `bind_update`（`parse_vector_literal`）側の責務であり、
        // 本関数はリテラル文字列を組み立てるのみ（次元不一致はここでは
        // 拒否しない）。
        let set = set_map(r#"{"embedding":[1,2]}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok (dimension checked later)");
        assert_eq!(
            bound,
            vec![(
                "embedding".to_string(),
                InsertLiteral::String("[1,2]".to_string())
            )]
        );
    }

    #[test]
    fn update_error_wire_codes_match_expected_classes() {
        assert_eq!(UpdateError::InvalidIdentifier.wire_code(), "42601");
        assert_eq!(UpdateError::EmptySet.wire_code(), "42601");
        assert_eq!(UpdateError::Set("x").wire_code(), "22000");
        assert_eq!(UpdateError::InvalidBytea("x").wire_code(), "42601");
        assert_eq!(UpdateError::ByteaTooLarge.wire_code(), "54000");
    }

    // --- BYTEA 列（Issue #886）の base64 → 正準 hex 再エンコード ---------------

    fn bytea_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("blob", ColumnType::Bytea, true),
            ],
        )
    }

    #[test]
    fn map_set_assignments_reencodes_base64_bytea_to_canonical_hex() {
        // "3q2+7w==" は [0xde, 0xad, 0xbe, 0xef] の標準 base64 表現。
        let set = set_map(r#"{"blob":"3q2+7w=="}"#);
        let bound = map_set_assignments(&set, &bytea_schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "blob".to_string(),
                InsertLiteral::String("\\xdeadbeef".to_string())
            )]
        );
    }

    #[test]
    fn map_set_assignments_rejects_non_string_bytea() {
        let set = set_map(r#"{"blob":true}"#);
        let err = map_set_assignments(&set, &bytea_schema()).expect_err("must reject");
        assert!(matches!(err, UpdateError::InvalidBytea(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn map_set_assignments_rejects_malformed_base64_bytea() {
        let set = set_map(r#"{"blob":"3q2+7w=a"}"#);
        let err = map_set_assignments(&set, &bytea_schema()).expect_err("must reject");
        assert!(matches!(err, UpdateError::InvalidBytea(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    // --- JSON／JSONB 列（Issue #889）の null 分岐（PR #1014 レビュー指摘対応） ---

    fn json_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("doc", ColumnType::Json, true),
                ColumnDef::new("docb", ColumnType::Jsonb, false),
            ],
        )
    }

    #[test]
    fn map_set_assignments_maps_json_null_on_nullable_column_to_insert_literal_null() {
        let set = set_map(r#"{"doc":null}"#);
        let bound = map_set_assignments(&set, &json_schema()).expect("ok");
        assert_eq!(bound, vec![("doc".to_string(), InsertLiteral::Null)]);
    }

    #[test]
    fn map_set_assignments_rejects_json_null_on_non_nullable_column() {
        let set = set_map(r#"{"docb":null}"#);
        let err = map_set_assignments(&set, &json_schema()).expect_err("must reject");
        assert!(matches!(err, UpdateError::InvalidJson(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn map_set_assignments_maps_json_object_column() {
        let set = set_map(r#"{"doc":{"a":1}}"#);
        let bound = map_set_assignments(&set, &json_schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "doc".to_string(),
                InsertLiteral::String(r#"{"a":1}"#.to_string())
            )]
        );
    }
}
