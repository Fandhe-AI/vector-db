//! カタログスキーマ駆動の行エンコーダー（TASK-86、対象ビヘイビア: TABLE-7。
//! ポインタ: `docs/spec/05-tasks.md` TASK-86・`docs/spec/04-behavior/data-model.md`）。
//!
//! 責務境界: `catalog.rs` の [`TableSchema`] を入力に取り、その列定義（列順・型・
//! nullable）に従って行データをバイト列へ/から変換する。`catalog.rs` の
//! モジュールコメントが「行エンコーダーの列対応・NULL 解決は TASK-86 の管轄」と
//! 明示している責務をここで実装し、`ALTER TABLE ADD COLUMN`（TABLE-5）で
//! 追加された列を持たない既存行を NULL として読む前提もここで扱う。
//!
//! `storage.rs` との関係: 本モジュールは `storage.rs` の行フォーマット v2
//! （`encode_row`/`decode_row`、tenant_id・visibility 同居の RLS 行フォーマット）を
//! 変更・置き換えしない。独立したバイトレイアウト（本モジュールローカルな v1）を持つ
//! 次世代コーデックとして追加し、`Storage` の行テーブルへの統合（テーブル帰属機構）は
//! 後続タスク（TASK-87・TASK-89・TASK-90 系）の管轄とする。`Visibility` 型のみ
//! `storage.rs` から再利用する（`to_byte`/`from_byte` の未知バイト拒否をそのまま活かす）。
//!
//! body 列の複製方針: 本タスクでは「行ストアの Text 列として単一保管し、別ストアへの
//! 複製・圧縮は行わない」と確定する（[`MAX_TEXT_FIELD_LEN`] で fail-closed に制限）。
//! 全文検索・UDF が本文参照を要求した時点で再検討する。

use std::fmt;

use crate::catalog::{ColumnType, TableSchema};
use crate::storage::Visibility;

/// 行フォーマットの先頭バイト。値の追加・変更は破壊的変更として扱い、この値を
/// 更新する。未知バージョンは fail-closed に拒否する（`storage.rs::ROW_FORMAT_VERSION`
/// と同じ方針。マイグレーションは提供しない）。
const ROW_CODEC_FORMAT_VERSION: u8 = 1;

/// ヘッダの `tenant_id` 長フィールド（`u8`）が表現できる上限。本モジュールの
/// ヘッダレイアウトはカタログスキーマとは独立に `tenant_id` を持つため、
/// `storage.rs::MAX_TENANT_ID_LEN`（`u16` 表現）とは別の実装ローカルな上限を持つ。
const MAX_TENANT_ID_LEN: u8 = u8::MAX;

/// `Text` 列 1 つあたりのバイト長上限。`storage.rs::MAX_METADATA_LEN` と同値方針
/// （data-model.md 2026-08-22 追記・CORE-15 方針のポインタ準拠）。検証通過後の
/// 長さのみをアロケーションに使う。
const MAX_TEXT_FIELD_LEN: u32 = 4 * 1024 * 1024;

/// `Vector` 列の次元数上限。`storage.rs::MAX_EMBEDDING_DIM`（永続化層が扱える上限）と
/// 同値を維持する。片方だけの変更を防ぐため下部の const assert でコンパイル時に強制する。
const MAX_EMBEDDING_DIM: u32 = 65_536;

const _: () = assert!(
    MAX_EMBEDDING_DIM == crate::storage::MAX_EMBEDDING_DIM,
    "row_codec::MAX_EMBEDDING_DIM must stay in sync with storage::MAX_EMBEDDING_DIM"
);

/// 列値の有無を示すタグバイト。未知の値は fail-closed に拒否する（presence の
/// 黙殺フォールバックは NULL/値ありの取り違えに直結するため許容しない）。
const PRESENCE_NULL: u8 = 0x00;
const PRESENCE_VALUE: u8 = 0x01;

/// 行エンコーダー層の公開エラー型。`catalog.rs`/`storage.rs` と同じ流儀で、
/// 欠落・上限超過・未知値・型不一致・切り詰め検出をすべて `Invalid` に集約する
/// （fail-closed。既定値へのフォールバックは行わない）。
///
/// エラーメッセージにはフィールドの内容（`tenant_id` の値・`body` 本文等）を含めず、
/// 長さ・上限値のみを含める（.claude/rules/security.md「テナント境界」: エラー経由での
/// 情報漏えい防止）。
#[derive(Debug)]
pub enum RowCodecError {
    Invalid(String),
}

impl fmt::Display for RowCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowCodecError::Invalid(msg) => write!(f, "invalid row data: {msg}"),
        }
    }
}

impl std::error::Error for RowCodecError {}

pub type Result<T> = std::result::Result<T, RowCodecError>;

/// 1 列分の値。[`ColumnType`] に対応する（`Null` は nullable 列にのみ許容される）。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Text(String),
    Vector(Vec<f32>),
}

/// デコード結果。行レベルの RLS フィールド（`tenant_id`・`visibility`）と、
/// スキーマの列順に対応する値列を保持する。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedRow {
    pub tenant_id: String,
    pub visibility: Visibility,
    pub values: Vec<Value>,
}

/// [`TableSchema`] の列定義（列順・型・nullable）に従い、行データをバイト列へ
/// エンコードする（TABLE-7）。`values` はスキーマの列順に対応させる。
///
/// - `values.len()` がスキーマの列数を超える場合は `Err`。
/// - 末尾の値が不足する場合、対応する列が nullable なら `Value::Null` を
///   補って扱う（TABLE-5: `ALTER TABLE ADD COLUMN` で追加された列を持たない
///   既存行の書き込み経路を想定）。non-nullable な列が不足する場合は `Err`。
/// - non-nullable 列へ `Value::Null` を渡した場合は `Err`。
/// - `Value::Vector` の次元がスキーマの `VECTOR(N)` と一致しない場合は `Err`。
/// - 長さフィールドの数値変換はすべて `try_from` で行い、失敗（上限超過）を
///   `Err` とする（`as` キャストによる剰余切り詰めは行わない。TABLE-7 の核心）。
pub fn encode_row(
    schema: &TableSchema,
    tenant_id: &str,
    visibility: Visibility,
    values: &[Value],
) -> Result<Vec<u8>> {
    if values.len() > schema.columns.len() {
        return Err(RowCodecError::Invalid(format!(
            "too many values: schema has {} columns, got {}",
            schema.columns.len(),
            values.len()
        )));
    }

    if tenant_id.is_empty() {
        return Err(RowCodecError::Invalid(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_bytes = tenant_id.as_bytes();
    // MAX_TENANT_ID_LEN(= u8::MAX) はヘッダの長さフィールド幅そのものであり、
    // u8::try_from の失敗が上限超過検出を兼ねる（`tenant_len > MAX_TENANT_ID_LEN` は
    // u8 の値域上常に false になるため、冗長な比較を書かない）。
    let tenant_len = u8::try_from(tenant_bytes.len()).map_err(|_| {
        RowCodecError::Invalid(format!(
            "tenant_id length {} exceeds limit {MAX_TENANT_ID_LEN}",
            tenant_bytes.len()
        ))
    })?;

    let mut buf = Vec::new();
    buf.push(ROW_CODEC_FORMAT_VERSION);
    buf.push(visibility.to_byte());
    buf.push(tenant_len);
    buf.extend_from_slice(tenant_bytes);

    for (idx, column) in schema.columns.iter().enumerate() {
        // スキーマの列数が values より多い場合、末尾の欠落列は「値なし」として
        // 扱う（TABLE-5 前提: nullable なら Null、non-nullable なら下の分岐で Err）。
        let value = values.get(idx).unwrap_or(&Value::Null);
        match value {
            Value::Null => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is missing",
                        column.name
                    )));
                }
                buf.push(PRESENCE_NULL);
            }
            Value::Text(text) => {
                if !matches!(column.ty, ColumnType::Text) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Text value, got Text",
                        column.name
                    )));
                }
                let text_bytes = text.as_bytes();
                let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!(
                        "text field too long: {} bytes",
                        text_bytes.len()
                    ))
                })?;
                if text_len > MAX_TEXT_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&text_len.to_le_bytes());
                buf.extend_from_slice(text_bytes);
            }
            Value::Vector(vector) => {
                let expected_dim = match column.ty {
                    ColumnType::Vector(dim) => dim,
                    ColumnType::Text => {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Vector value, got Vector",
                            column.name
                        )))
                    }
                };
                let dim = u32::try_from(vector.len()).map_err(|_| {
                    RowCodecError::Invalid(format!("embedding dim too large: {}", vector.len()))
                })?;
                if dim != expected_dim {
                    return Err(RowCodecError::Invalid(format!(
                        "embedding dim mismatch for column {:?}: expected {expected_dim}, got {dim}",
                        column.name
                    )));
                }
                if dim > MAX_EMBEDDING_DIM {
                    return Err(RowCodecError::Invalid(format!(
                        "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&dim.to_le_bytes());
                for v in vector {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
    }

    Ok(buf)
}

/// [`encode_row`] の逆変換。欠落・不正値・切り詰め・未知タグをすべて `Err` で拒否する
/// （fail-closed。黙殺フォールバックで既定値へ落とさない）。添字アクセス `[]` ではなく
/// `get()`・`checked_add` を使い、境界外アクセス・オーバーフローを未定義動作にしない。
///
/// バッファが列の途中で終わっている場合、その列以降は「nullable なら `Value::Null`、
/// non-nullable なら `Err`」として扱う（TABLE-5: `ALTER TABLE ADD COLUMN` 後の
/// 既存行デコード前提）。
pub fn decode_row(schema: &TableSchema, buf: &[u8]) -> Result<DecodedRow> {
    let version = *buf
        .first()
        .ok_or_else(|| RowCodecError::Invalid("row buffer is empty".to_string()))?;
    if version != ROW_CODEC_FORMAT_VERSION {
        return Err(RowCodecError::Invalid(format!(
            "unsupported row format version: {version}"
        )));
    }

    let visibility_byte = *buf.get(1).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at visibility field".to_string())
    })?;
    let visibility = Visibility::from_byte(visibility_byte)
        .map_err(|e| RowCodecError::Invalid(format!("invalid visibility byte: {e}")))?;

    // tenant_len は u8 のヘッダフィールドとして読み出すため、値域は常に
    // 0..=MAX_TENANT_ID_LEN(= u8::MAX) に収まる（上限超過チェックは不要）。
    let tenant_len = *buf.get(2).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at tenant_len field".to_string())
    })?;

    let mut offset = 3usize;
    let tenant_end = offset.checked_add(tenant_len as usize).ok_or_else(|| {
        RowCodecError::Invalid("offset overflow after tenant_len field".to_string())
    })?;
    let tenant_bytes = buf.get(offset..tenant_end).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at tenant_id field".to_string())
    })?;
    if tenant_bytes.is_empty() {
        return Err(RowCodecError::Invalid(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_id = std::str::from_utf8(tenant_bytes)
        .map_err(|_| RowCodecError::Invalid("tenant_id is not valid UTF-8".to_string()))?
        .to_string();
    offset = tenant_end;

    let mut values = Vec::with_capacity(schema.columns.len());
    for column in &schema.columns {
        // バッファ末尾に達した場合、以降の列はすべて「欠落」として扱う
        // （TABLE-5: ADD COLUMN 後の既存行を NULL として読む）。
        let presence = match buf.get(offset) {
            Some(&b) => b,
            None => {
                if column.nullable {
                    values.push(Value::Null);
                    continue;
                } else {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but row buffer is truncated",
                        column.name
                    )));
                }
            }
        };
        offset = offset.checked_add(1).ok_or_else(|| {
            RowCodecError::Invalid("offset overflow after presence field".to_string())
        })?;

        match presence {
            PRESENCE_NULL => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is NULL",
                        column.name
                    )));
                }
                values.push(Value::Null);
            }
            PRESENCE_VALUE => match column.ty {
                ColumnType::Text => {
                    let len_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before text length field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at text length field".to_string(),
                            )
                        })?;
                    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("text length field is not 4 bytes".to_string())
                    })?;
                    let text_len = u32::from_le_bytes(len_arr);
                    if text_len > MAX_TEXT_FIELD_LEN {
                        return Err(RowCodecError::Invalid(format!(
                            "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after text length field".to_string(),
                        )
                    })?;
                    let text_end = offset.checked_add(text_len as usize).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after text field".to_string())
                    })?;
                    let text_bytes = buf.get(offset..text_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at text field".to_string())
                    })?;
                    let text = std::str::from_utf8(text_bytes)
                        .map_err(|_| {
                            RowCodecError::Invalid("text field is not valid UTF-8".to_string())
                        })?
                        .to_string();
                    offset = text_end;
                    values.push(Value::Text(text));
                }
                ColumnType::Vector(expected_dim) => {
                    let dim_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before vector dim field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at vector dim field".to_string(),
                            )
                        })?;
                    let dim_arr: [u8; 4] = dim_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("vector dim field is not 4 bytes".to_string())
                    })?;
                    let dim = u32::from_le_bytes(dim_arr);
                    if dim > MAX_EMBEDDING_DIM {
                        return Err(RowCodecError::Invalid(format!(
                            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
                        )));
                    }
                    if dim != expected_dim {
                        return Err(RowCodecError::Invalid(format!(
                            "embedding dim mismatch for column {:?}: expected {expected_dim}, got {dim}",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after vector dim field".to_string())
                    })?;
                    let vector_bytes_len = (dim as usize).checked_mul(4).ok_or_else(|| {
                        RowCodecError::Invalid("vector byte length overflow".to_string())
                    })?;
                    let vector_end = offset.checked_add(vector_bytes_len).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after vector field".to_string())
                    })?;
                    let vector_bytes = buf.get(offset..vector_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at vector field".to_string())
                    })?;
                    // 上限検証済みの dim に基づくため、無制限確保にはならない。
                    let mut vector = Vec::with_capacity(dim as usize);
                    for chunk in vector_bytes.chunks_exact(4) {
                        let arr: [u8; 4] = chunk.try_into().map_err(|_| {
                            RowCodecError::Invalid("vector chunk is not 4 bytes".to_string())
                        })?;
                        vector.push(f32::from_le_bytes(arr));
                    }
                    offset = vector_end;
                    values.push(Value::Vector(vector));
                }
            },
            other => {
                return Err(RowCodecError::Invalid(format!(
                    "unknown presence byte: {other}"
                )))
            }
        }
    }

    if offset != buf.len() {
        return Err(RowCodecError::Invalid(
            "row buffer has trailing bytes beyond declared columns".to_string(),
        ));
    }

    Ok(DecodedRow {
        tenant_id,
        visibility,
        values,
    })
}

/// `sql::exec`（TASK-75、対象ビヘイビア: SQL-2）から呼ばれる、スキーマの非
/// `VECTOR` 列（`Text` 列）のみを列順にエンコードするペイロード。`storage.rs::RowInput`
/// は `embedding`（`VECTOR` 列 1 本）と不透明な `metadata` バイト列しか持たないため、
/// `VECTOR` 列は `embedding` スロットへ、それ以外は本関数の出力を `metadata` へ格納する
/// という規約を SQL 表層のローカルな契約として定義する（`encode_row`/`decode_row` の
/// フルスキーマコーデックは `tenant_id`/`visibility` ヘッダごと持つため、`storage.rs` 側で
/// 既に保持しているそれらと二重管理・二重保存になってしまい、本関数では使わない）。
///
/// `values` は [`TableSchema::columns`] の列順に対応させる（`VECTOR` 列の位置は
/// 無条件にスキップされ、`values` に何を渡しても読まれない）。欠落・上限超過・
/// 型不一致の規則は [`encode_row`] と同一（nullable 列の末尾欠落は `Value::Null`、
/// non-nullable な欠落は `Err`）。
pub fn encode_scalar_columns(schema: &TableSchema, values: &[Value]) -> Result<Vec<u8>> {
    if values.len() > schema.columns.len() {
        return Err(RowCodecError::Invalid(format!(
            "too many values: schema has {} columns, got {}",
            schema.columns.len(),
            values.len()
        )));
    }

    let mut buf = Vec::new();
    for (idx, column) in schema.columns.iter().enumerate() {
        if matches!(column.ty, ColumnType::Vector(_)) {
            // VECTOR 列は storage.rs 側の embedding スロットが担当するため、
            // スカラーペイロードには一切含めない（値の有無・内容を問わずスキップ）。
            continue;
        }
        let value = values.get(idx).unwrap_or(&Value::Null);
        match value {
            Value::Null => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is missing",
                        column.name
                    )));
                }
                buf.push(PRESENCE_NULL);
            }
            Value::Text(text) => {
                let text_bytes = text.as_bytes();
                let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!(
                        "text field too long: {} bytes",
                        text_bytes.len()
                    ))
                })?;
                if text_len > MAX_TEXT_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&text_len.to_le_bytes());
                buf.extend_from_slice(text_bytes);
            }
            Value::Vector(_) => {
                return Err(RowCodecError::Invalid(format!(
                    "column {:?} expects a non-Vector value, got Vector",
                    column.name
                )))
            }
        }
    }

    Ok(buf)
}

/// [`encode_scalar_columns`] の逆変換。戻り値は `schema.columns` と同じ長さ・順序を
/// 持ち、`VECTOR` 列の位置は常に `Value::Null`（本関数はその位置のバイトを一切
/// 読み書きしないダミー値。呼び出し元は embedding を `storage.rs::Row::embedding` から
/// 別途参照する）。`Text` 列は [`decode_row`] と同じ規則（バッファ末尾で打ち切られた
/// nullable 列は `Value::Null`、non-nullable は `Err`。presence タグ不正・宣言長が
/// 残りバッファを超える場合は `Err`）でデコードする。
pub fn decode_scalar_columns(schema: &TableSchema, buf: &[u8]) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(schema.columns.len());
    let mut offset = 0usize;
    for column in &schema.columns {
        if matches!(column.ty, ColumnType::Vector(_)) {
            values.push(Value::Null);
            continue;
        }
        let presence = match buf.get(offset) {
            Some(&b) => b,
            None => {
                if column.nullable {
                    values.push(Value::Null);
                    continue;
                } else {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but scalar payload is truncated",
                        column.name
                    )));
                }
            }
        };
        offset = offset.checked_add(1).ok_or_else(|| {
            RowCodecError::Invalid("offset overflow after presence field".to_string())
        })?;

        match presence {
            PRESENCE_NULL => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is NULL",
                        column.name
                    )));
                }
                values.push(Value::Null);
            }
            PRESENCE_VALUE => {
                let len_bytes = buf
                    .get(
                        offset..offset.checked_add(4).ok_or_else(|| {
                            RowCodecError::Invalid(
                                "offset overflow before text length field".to_string(),
                            )
                        })?,
                    )
                    .ok_or_else(|| {
                        RowCodecError::Invalid(
                            "scalar payload truncated at text length field".to_string(),
                        )
                    })?;
                let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                    RowCodecError::Invalid("text length field is not 4 bytes".to_string())
                })?;
                let text_len = u32::from_le_bytes(len_arr);
                if text_len > MAX_TEXT_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                    )));
                }
                offset = offset.checked_add(4).ok_or_else(|| {
                    RowCodecError::Invalid("offset overflow after text length field".to_string())
                })?;
                let text_end = offset.checked_add(text_len as usize).ok_or_else(|| {
                    RowCodecError::Invalid("offset overflow after text field".to_string())
                })?;
                let text_bytes = buf.get(offset..text_end).ok_or_else(|| {
                    RowCodecError::Invalid("scalar payload truncated at text field".to_string())
                })?;
                let text = std::str::from_utf8(text_bytes)
                    .map_err(|_| {
                        RowCodecError::Invalid("text field is not valid UTF-8".to_string())
                    })?
                    .to_string();
                offset = text_end;
                values.push(Value::Text(text));
            }
            other => {
                return Err(RowCodecError::Invalid(format!(
                    "unknown presence byte: {other}"
                )))
            }
        }
    }

    if offset != buf.len() {
        return Err(RowCodecError::Invalid(
            "scalar payload has trailing bytes beyond declared columns".to_string(),
        ));
    }

    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn text_vector_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        )
    }

    #[test]
    fn encode_decode_roundtrip_preserves_row() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello world".to_string()),
            Value::Null,
        ];
        let encoded =
            encode_row(&schema, "tenant-a", Visibility::Private, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.tenant_id, "tenant-a");
        assert_eq!(decoded.visibility, Visibility::Private);
        assert_eq!(decoded.values, values);
    }

    #[test]
    fn encode_rejects_short_field_length_overflow_without_truncating() {
        // MAX_TENANT_ID_LEN(=255) を超える tenant_id は Err になり、
        // 剰余に切り詰めた値で成功してはならない（TABLE-7 の核心）。
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("body".to_string()),
            Value::Null,
        ];
        let huge_tenant = "t".repeat(256);
        let result = encode_row(&schema, &huge_tenant, Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_rejects_text_field_length_overflow() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let huge_text = "x".repeat((MAX_TEXT_FIELD_LEN as usize) + 1);
        let values = vec![Value::Text(huge_text)];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_decode_rejects_vector_dim_mismatch() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0]), // 宣言次元 3 に対し 2
            Value::Text("body".to_string()),
            Value::Null,
        ];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_decode_rejects_vector_dim_exceeding_max() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "embedding",
                ColumnType::Vector(MAX_EMBEDDING_DIM + 1),
                false,
            )],
        );
        // catalog 層の validate_vector_dim は MAX_VECTOR_DIM 超過を schema 作成時点で
        // 拒否するが、本テストは row_codec 単体での上限検証（多層防御）を確認する。
        let huge_vector = vec![0.0f32; (MAX_EMBEDDING_DIM as usize) + 1];
        let values = vec![Value::Vector(huge_vector)];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_rejects_null_for_non_nullable_column() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Null, // body は non-nullable
            Value::Null,
        ];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_unknown_visibility_byte() {
        let schema = text_vector_schema();
        let mut buf = vec![ROW_CODEC_FORMAT_VERSION, 0xff, 1, b't'];
        for column in &schema.columns {
            buf.push(PRESENCE_NULL);
            let _ = column;
        }
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_unknown_presence_byte() {
        let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
        let mut buf = vec![
            ROW_CODEC_FORMAT_VERSION,
            Visibility::Public.to_byte(),
            1,
            b't',
        ];
        buf.push(0xaa); // 未知の presence バイト
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_unknown_format_version() {
        let schema = text_vector_schema();
        let buf = vec![0xff];
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_truncated_buffer_at_each_field_boundary() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Null,
        ];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        for cut in 1..encoded.len() {
            let truncated = &encoded[..cut];
            let result = decode_row(&schema, truncated);
            // 末尾の nullable 列欠落は許容されるため、途中切断のうち少なくとも
            // ヘッダ・embedding・body 境界の切断はすべて Err になることを確認する。
            if cut < encoded.len() - 1 {
                assert!(
                    result.is_err(),
                    "expected Err when truncated at byte {cut}, got {result:?}"
                );
            }
        }
    }

    #[test]
    fn decode_treats_missing_trailing_nullable_column_as_null() {
        // TABLE-5 前提: ADD COLUMN で追加された nullable 列を持たない既存行を
        // デコードすると、その列は Null として読める。
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
        ];
        // tag 列（nullable, 末尾）を含めずにエンコードする（欠落バイト列を模す）。
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&3u32.to_le_bytes());
        for v in [1.0f32, 2.0, 3.0] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(b"hello");
        // tag 列を書かない（バッファをここで終える）。

        let decoded = decode_row(&schema, &buf).expect("decode should succeed");
        assert_eq!(decoded.values.len(), 3);
        assert_eq!(decoded.values[2], Value::Null);
        let _ = values;
    }

    #[test]
    fn decode_rejects_missing_trailing_non_nullable_column() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        // body 列（non-nullable）を書かずにバッファを終える。
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_rejects_more_values_than_schema_columns() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let values = vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()), // スキーマの列数を超える
        ];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_declared_length_exceeding_remaining_buffer() {
        // 宣言長が残りバッファ長を超える不正入力は、アロケーション前に Err になる
        // （text_len を故意に巨大な値にする）。
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&u32::MAX.to_le_bytes()); // 宣言長が残りバッファを大幅に超過
        buf.extend_from_slice(b"short");
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn non_empty_body_roundtrips() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let body = "この行の body 列は単一保管され、別ストアへ複製・圧縮しない（TASK-86 判断）。"
            .repeat(10);
        let values = vec![Value::Text(body.clone())];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.values[0], Value::Text(body));
    }

    // --- encode_scalar_columns / decode_scalar_columns（TASK-75、SQL-2） -----------

    #[test]
    fn scalar_columns_roundtrip_skips_vector_column() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello world".to_string()),
            Value::Text("tag-a".to_string()),
        ];
        let encoded = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let decoded = decode_scalar_columns(&schema, &encoded).expect("decode scalar");
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], Value::Null); // VECTOR 列はダミー Null
        assert_eq!(decoded[1], Value::Text("hello world".to_string()));
        assert_eq!(decoded[2], Value::Text("tag-a".to_string()));
    }

    #[test]
    fn scalar_columns_treats_missing_trailing_nullable_column_as_null() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            // tag（nullable, 末尾）を渡さない。
        ];
        let encoded = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let decoded = decode_scalar_columns(&schema, &encoded).expect("decode scalar");
        assert_eq!(decoded[2], Value::Null);
    }

    #[test]
    fn scalar_columns_rejects_missing_trailing_non_nullable_column() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        );
        let values = vec![Value::Vector(vec![1.0, 2.0])];
        let result = encode_scalar_columns(&schema, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scalar_columns_decode_rejects_unknown_presence_byte() {
        let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
        let buf = vec![0xaa]; // 未知の presence バイト
        let result = decode_scalar_columns(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scalar_columns_decode_rejects_declared_length_exceeding_remaining_buffer() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let mut buf = Vec::new();
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.extend_from_slice(b"short");
        let result = decode_scalar_columns(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scalar_columns_encode_rejects_text_field_length_overflow() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let huge_text = "x".repeat((MAX_TEXT_FIELD_LEN as usize) + 1);
        let values = vec![Value::Text(huge_text)];
        let result = encode_scalar_columns(&schema, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }
}
