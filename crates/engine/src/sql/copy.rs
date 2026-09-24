//! COPY プロトコル（`COPY ... FROM STDIN`／`COPY (...) TO STDOUT`）の SQL 表層
//! 側実装（Issue #939・WIRE-17・TASK-220）。
//!
//! 責務境界: wire-server（`crates/wire-server/src/copy.rs`）は CopyIn／CopyOut
//! サブプロトコルのメッセージ層（フレーミング・状態遷移）のみを担い、行の
//! レコード分割・フィールドデコード・束縛・INDEX-4 上限判定・commit はすべて
//! ここへ集約する（第 2 の書き込み経路を作らない設計。TASK-190（SQL-16。
//! [`crate::core::EngineCore::execute_bound_insert_in_session`]）と同一の
//! commit 経路を [`crate::core::EngineCore::commit_copy_in`] 経由で共有する）。
//!
//! `FROM STDIN` は全 CopyData を受信し終える前（CopyDone より前）に INDEX-4 の
//! 4 上限を逐次判定する（[`crate::batch_limits`] の逐次判定ヘルパーを使う。
//! バッチ全体を揃えてから判定する既存の複数行 `INSERT`（[`crate::sql::exec::
//! execute_insert_batch`]）とは異なり、ストリーミング故に超過を CopyDone を
//! 待たずに検出できる）。`TO STDOUT` は広域取得（SQL-15・Issue #454）と同じ
//! 実行本体（[`crate::core::EngineCore::run_scan_plan`]）をそのまま使う
//! （第 2 の SELECT 実行器を持たない）。
//!
//! protocol v3 の COPY サブプロトコルは CopyDone（'c'）で終端を表現するため、
//! protocol v2 由来の `\.` 終端行は本実装では受理・要求しない（PostgreSQL
//! 本家との既知の相違点。詳細は `docs/design/wire-copy-protocol.md` 参照）。

use crate::batch_limits::{self, BatchLimits, BatchLimitsError};
use crate::catalog::{ColumnType, TableSchema};
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::{CopyFormat, SqlSurfaceError};
use crate::sql::parser::{
    bind_bytea_literal, bind_enum_literal, bind_json_literal, parse_array_literal,
    parse_vector_literal, BoundInsert,
};

/// wire 層のホットパスで `lexer::tokenize` を増やさないための安価な覗き見
/// （`core.rs::execute_sql_in_session` の `INSERT`／`TRUNCATE`／`DELETE`／
/// `UPDATE` 先頭トークン判定と同型。`handshake::post_auth_loop` の 'Q' 分岐が
/// COPY サブプロトコルへ委譲するかどうかをここで判定する）。誤って見逃した
/// 場合（コメント前置等）は通常の `validate_sql` 経路へ流れ `42601` で拒否
/// される（fail-closed。本関数は受理判定を狭める側にしか作用しない）。
pub fn is_copy_statement(sql: &str) -> bool {
    let trimmed = sql.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let rest = match trimmed.get(..4) {
        Some(prefix) if prefix.eq_ignore_ascii_case("COPY") => match trimmed.get(4..) {
            Some(r) => r,
            None => return false,
        },
        _ => return false,
    };
    match rest.chars().next() {
        None => true,
        Some(c) => c.is_ascii_whitespace() || c == '(',
    }
}

/// [`BatchLimitsError`] を SQL 表層のエラー型へ写像する（既存の
/// `validate_insert_batch_byte_and_chunk_limits` と同じ「全 variant が
/// `54000`」写像。`crate::core` も同じ写像を独自に持つが、依存を増やさず
/// ここでも同じ変換を行う）。
fn map_batch_err(e: BatchLimitsError) -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(e.to_string())
}

/// レコード（1 行分の CopyData バイト列）をチャンク境界をまたいで切り出す
/// 状態機械。text／CSV いずれも生の LF（`\n`）が行区切りである点は共通だが
/// （PostgreSQL の COPY テキストプロトコルは埋め込み改行を `\n`（2 文字の
/// エスケープ）としてしか表現できず、生の LF バイトは常に行終端である。
/// text 形式のバックスラッシュエスケープ自体はフィールドデコード側
/// （[`decode_text_field`]）の責務でありレコード分割には影響しない）、CSV は
/// 引用符で囲まれたフィールド内に生の改行を含み得るため、`"` の出現回数の
/// 偶奇（`in_quotes`）で「引用符の外側」を判定してから LF を区切りとみなす
/// （`""` によるエスケープされた引用符は 2 回連続でトグルするため、
/// 状態機械としては元の状態へ戻り正しく動作する）。
#[derive(Debug)]
struct RecordSplitter {
    format: CopyFormat,
    pending: Vec<u8>,
    in_quotes: bool,
}

impl RecordSplitter {
    fn new(format: CopyFormat) -> Self {
        Self {
            format,
            pending: Vec::new(),
            in_quotes: false,
        }
    }

    /// `chunk`（1 個の CopyData メッセージ本文）を末尾未確定分（`pending`）と
    /// 連結しながら走査し、完了したレコードごとに `on_record` を呼ぶ。
    /// `pending` の総量は呼び出し元（[`CopyInSession::feed`]）が CopyData の
    /// 生バイト量そのものを③（`max_batch_total_bytes`）で先に上限判定して
    /// いるため、本関数自体は追加の容量上限を持たない（すでに有界な入力を
    /// 受け取る契約）。
    fn feed(
        &mut self,
        chunk: &[u8],
        mut on_record: impl FnMut(&[u8]) -> Result<(), SqlSurfaceError>,
    ) -> Result<(), SqlSurfaceError> {
        for &b in chunk {
            if self.format == CopyFormat::Csv && b == b'"' {
                self.in_quotes = !self.in_quotes;
                self.pending.push(b);
                continue;
            }
            if b == b'\n' && !self.in_quotes {
                let mut record = std::mem::take(&mut self.pending);
                if record.last() == Some(&b'\r') {
                    record.pop();
                }
                on_record(&record)?;
                continue;
            }
            self.pending.push(b);
        }
        Ok(())
    }

    /// CopyDone 到達時に呼ぶ。改行なしで終わった末尾分（存在する場合）を返す。
    fn finish(self) -> Vec<u8> {
        self.pending
    }
}

/// text 形式のバックスラッシュエスケープを解決する（PostgreSQL 互換の
/// `\\`／`\t`／`\n`／`\r`／`\b`／`\f`／`\v`。それ以外のエスケープ（8 進・
/// 16 進を含む）は fail-closed に `22000` で拒否する。フィールド全体が
/// リテラル `\N` の場合のみ NULL とする（`\N` が末尾以外の位置に現れた
/// 場合は通常のエスケープ解決に入り、`N` は未対応エスケープとして拒否
/// される。これは PostgreSQL 本家がフィールド全体一致でのみ `\N` を特別
/// 扱いする契約と同じ）。
fn decode_text_field(raw: &[u8]) -> Result<Option<String>, SqlSurfaceError> {
    if raw == b"\\N" {
        return Ok(None);
    }
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut iter = raw.iter().copied();
    while let Some(b) = iter.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match iter.next() {
            Some(b'\\') => out.push(b'\\'),
            Some(b't') => out.push(b'\t'),
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b'b') => out.push(0x08),
            Some(b'f') => out.push(0x0C),
            Some(b'v') => out.push(0x0B),
            _ => {
                return Err(SqlSurfaceError::invalid_input(
                    "unsupported backslash escape in COPY text field",
                ))
            }
        }
    }
    String::from_utf8(out)
        .map(Some)
        .map_err(|_| SqlSurfaceError::invalid_input("COPY text field is not valid UTF-8"))
}

/// text 形式の 1 レコードをタブ区切りでフィールドへ分割する。生のタブ
/// バイトは常にフィールド区切り（埋め込みタブはクライアント側で `\t` へ
/// エスケープ済みである契約。[`RecordSplitter`] の LF と同じ考え方）。
fn decode_text_record(record: &[u8]) -> Result<Vec<Option<String>>, SqlSurfaceError> {
    record
        .split(|&b| b == b'\t')
        .map(decode_text_field)
        .collect()
}

/// CSV 形式の 1 レコードをカンマ区切り・二重引用符エスケープでフィールドへ
/// 分割する（RFC4180 風）。引用符で囲まれていない空フィールドは NULL、
/// 引用符で囲まれた空フィールド（`""`）は空文字列として区別する。
fn decode_csv_record(record: &[u8]) -> Result<Vec<Option<String>>, SqlSurfaceError> {
    let mut fields: Vec<Option<String>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut in_quotes = false;
    let mut quoted = false;
    let mut iter = record.iter().copied().peekable();
    while let Some(b) = iter.next() {
        if in_quotes {
            if b == b'"' {
                if iter.peek() == Some(&b'"') {
                    iter.next();
                    current.push(b'"');
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(b);
            }
            continue;
        }
        match b {
            b'"' => {
                in_quotes = true;
                quoted = true;
            }
            b',' => {
                fields.push(finish_csv_field(std::mem::take(&mut current), quoted)?);
                quoted = false;
            }
            _ => current.push(b),
        }
    }
    if in_quotes {
        return Err(SqlSurfaceError::invalid_input(
            "unterminated quoted CSV field in COPY record",
        ));
    }
    fields.push(finish_csv_field(current, quoted)?);
    Ok(fields)
}

fn finish_csv_field(bytes: Vec<u8>, quoted: bool) -> Result<Option<String>, SqlSurfaceError> {
    if bytes.is_empty() && !quoted {
        return Ok(None);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| SqlSurfaceError::invalid_input("COPY CSV field is not valid UTF-8"))
}

fn decode_record(
    format: CopyFormat,
    record: &[u8],
) -> Result<Vec<Option<String>>, SqlSurfaceError> {
    match format {
        CopyFormat::Text => decode_text_record(record),
        CopyFormat::Csv => decode_csv_record(record),
    }
}

/// デコード済み 1 行分のフィールド（`None` = NULL）を [`BoundInsert`] へ束縛
/// する。`sql::parser::bind_insert_row`（`INSERT ... VALUES` 専用）とは意図的に
/// 別実装とする理由: `InsertLiteral` は文字列／数値リテラルのみを表現でき
/// 明示的な NULL 値を持てない（`INSERT` の許可形状自体が VALUES 内の `NULL`
/// キーワードを受理しない）のに対し、COPY のフィールドは `\N`／引用符なし
/// 空欄という独自の NULL 表現を持つため、共有すると `InsertLiteral` へ
/// 破壊的変更（`Null` variant 追加）を要する。列名解決・非 nullable 列欠落・
/// 型照合の検査内容は `bind_insert_row` と同じ規則に揃えている。
fn bind_copy_record(
    table_name: &str,
    columns: &[String],
    fields: &[Option<String>],
    operation_id: &Option<OperationId>,
    schema: &TableSchema,
) -> Result<BoundInsert, SqlSurfaceError> {
    let id_pos = columns.iter().position(|c| c == "id").ok_or_else(|| {
        SqlSurfaceError::invalid_input("COPY column list must include the id pseudo-column")
    })?;
    let id_field = fields
        .get(id_pos)
        .and_then(|f| f.as_ref())
        .ok_or_else(|| SqlSurfaceError::invalid_input("id pseudo-column value must not be NULL"))?;
    let id: u64 = id_field
        .parse()
        .map_err(|_| SqlSurfaceError::invalid_input(format!("malformed id value: {id_field}")))?;

    let mut bound_values: Vec<Value> = vec![Value::Null; schema.columns.len()];
    let mut provided = vec![false; schema.columns.len()];

    for (name, field) in columns.iter().zip(fields.iter()) {
        if name == "id" {
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
        let value = match field {
            None => {
                if !column.nullable {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} does not accept NULL"
                    )));
                }
                Value::Null
            }
            Some(s) => match &column.ty {
                ColumnType::Vector(dim) => Value::Vector(parse_vector_literal(s, *dim)?),
                ColumnType::Text => Value::Text(s.clone()),
                ColumnType::Boolean => Value::Bool(parse_copy_boolean(s, name)?),
                ColumnType::Array(array_ty) => Value::Array(parse_array_literal(s, *array_ty)?),
                ColumnType::Bytea => bind_bytea_literal(s, name)?,
                ColumnType::Enum(def) => bind_enum_literal(def, s, name)?,
                ColumnType::Json | ColumnType::Jsonb => bind_json_literal(s, &column.ty, name)?,
            },
        };
        if let Some(slot) = bound_values.get_mut(col_idx) {
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
                "missing value for non-nullable column: {}",
                column.name
            )));
        }
    }

    Ok(BoundInsert {
        table: table_name.to_string(),
        id,
        values: bound_values,
        operation_id: operation_id.clone(),
    })
}

/// COPY テキスト／CSV 形式の `BOOLEAN` フィールドを解釈する。配列リテラルの
/// 要素解釈（`parse_array_literal` 内、`t|f|true|false` を大小無視で受理）と
/// 同じ語彙に揃え、`INSERT ... VALUES` のブール識別子（`sql::allowlist` の
/// 字句判定）とは別経路のまま維持する（COPY のフィールドは常に文字列であり
/// SQL 識別子ではないため）。
fn parse_copy_boolean(raw: &str, column_name: &str) -> Result<bool, SqlSurfaceError> {
    if raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("t") {
        Ok(true)
    } else if raw.eq_ignore_ascii_case("false") || raw.eq_ignore_ascii_case("f") {
        Ok(false)
    } else {
        Err(SqlSurfaceError::invalid_input(format!(
            "column {column_name:?} expects a boolean value (t/f/true/false)"
        )))
    }
}

/// `Σ` 各値のバイト長（既存の `EngineCore::validate_insert_batch_byte_and_
/// chunk_limits`・`core.rs` の複数行 `INSERT` バッチ判定と同一の②③判定対象量
/// 定義を COPY 側でも使う。`BOOLEAN`/`ARRAY`/`BYTEA`/`ENUM` の各定義は
/// Issue #883・#888・#886・#890 の判断をそのまま踏襲する）。
fn bound_insert_byte_len(bound: &BoundInsert) -> Result<usize, SqlSurfaceError> {
    let mut total: usize = 0;
    for v in &bound.values {
        let value_len = match v {
            Value::Null => 0,
            Value::Text(s) => s.len(),
            Value::Vector(vec) => vec.len().saturating_mul(std::mem::size_of::<f32>()),
            Value::Bool(_) => 1,
            Value::Array(array_value) => {
                let entry_len =
                    crate::row_codec::scalar_array_entry_len(array_value.elem(), array_value)
                        .map_err(|e| SqlSurfaceError::payload_too_large(e.to_string()))?;
                usize::try_from(entry_len).map_err(|_| {
                    SqlSurfaceError::payload_too_large("COPY row array entry length overflow")
                })?
            }
            Value::Bytes(b) => b.len(),
            Value::Enum(s) => s.len(),
            Value::Json(s) => s.len(),
        };
        total = total
            .checked_add(value_len)
            .ok_or_else(|| SqlSurfaceError::payload_too_large("COPY row byte size overflow"))?;
    }
    Ok(total)
}

/// `COPY <table> (<cols>) FROM STDIN` の逐次取り込み状態（Issue #939・
/// WIRE-17）。構築は [`crate::core::EngineCore::begin_copy`] のみが行う。
/// [`Self::feed`] を CopyData ごとに呼び、INDEX-4 の 4 上限を CopyDone より
/// 前に逐次判定する。[`Self::finish`] で末尾レコードを確定させ、実際の
/// commit（[`crate::core::EngineCore::commit_copy_in`]）へ渡す
/// [`CopyInBatch`] を返す。
#[derive(Debug)]
pub struct CopyInSession {
    table: String,
    columns: Vec<String>,
    format: CopyFormat,
    operation_id: Option<OperationId>,
    schema: TableSchema,
    limits: BatchLimits,
    splitter: RecordSplitter,
    bounds: Vec<BoundInsert>,
    running_bytes: usize,
}

impl CopyInSession {
    /// `columns`（`id` 疑似列を含む列リスト）が `schema` に対して構文的に
    /// 自明な誤りを持たないかを、`CopyInResponse`（`G`）送出前に検証する
    /// （codex-review 指摘・Issue #939 レビュー対応）。列名の typo や
    /// 非 nullable 列の欠落は、修正前は各行のデコード時（1 行目の CopyData
    /// 受信後）に初めて [`bind_copy_record`] が検出していたため、クライアントは
    /// 既に copy mode（`CopyInResponse` 受領後）に入ってからデータ送信を
    /// 開始してしまい、PostgreSQL 本家の「copy mode へ入る前に列リストを
    /// テーブル定義と照合して拒否する」契約と乖離していた。本関数はその
    /// 列リストの網羅性（列名の実在性・非 nullable 列の被覆）だけを検証し、
    /// 個々の値の型・NULL 可否は引き続き [`bind_copy_record`] が行単位で担う
    /// （第 2 の検証経路を増やさず、責務を「文レベル」と「行レベル」で分ける）。
    fn validate_columns_against_schema(
        columns: &[String],
        schema: &TableSchema,
    ) -> Result<(), SqlSurfaceError> {
        for name in columns {
            if name == "id" {
                continue;
            }
            if !schema.columns.iter().any(|c| &c.name == name) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "unknown column: {name}"
                )));
            }
        }
        for column in &schema.columns {
            if !column.nullable && !columns.iter().any(|c| c == &column.name) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "missing value for non-nullable column: {}",
                    column.name
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn new(
        table: String,
        columns: Vec<String>,
        format: CopyFormat,
        operation_id: Option<OperationId>,
        schema: TableSchema,
        limits: BatchLimits,
    ) -> Result<Self, SqlSurfaceError> {
        Self::validate_columns_against_schema(&columns, &schema)?;
        Ok(Self {
            table,
            columns,
            splitter: RecordSplitter::new(format),
            format,
            operation_id,
            schema,
            limits,
            bounds: Vec::new(),
            running_bytes: 0,
        })
    }

    /// FROM 対象テーブル名（wire-server の CopyInResponse 送出前の分岐でも
    /// 使わないが、テスト・ログ文脈での確認用に公開する）。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 列リストの列数（`id` 疑似列を含む）。`wire-server::copy` が
    /// `CopyInResponse` の列数フィールドを組み立てるために参照する。
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// 1 個の CopyData メッセージ本文を取り込む。判定順序:
    /// 1. ③（生 CopyData バイト量の累計）を `pending` へ追加する前に判定
    ///    （`54000`。超過時点で拒否し、それ以降のバイトは一切保持しない）。
    /// 2. 完了したレコードごとに、①（行数）→④（生成チャンク数。1 行=1
    ///    チャンクと読み替える。既存の単文 INSERT 経路と同じ読み替え規則）→
    ///    フィールド分割・エスケープ解決・列型照合（`22000`）→②（当該行の
    ///    デコード後バイト量）の順に判定する。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), SqlSurfaceError> {
        self.running_bytes =
            batch_limits::check_running_total(self.running_bytes, chunk.len(), &self.limits)
                .map_err(map_batch_err)?;

        let format = self.format;
        let table = self.table.clone();
        let columns = self.columns.clone();
        let operation_id = self.operation_id.clone();
        let limits = self.limits;
        let schema = &self.schema;
        let bounds = &mut self.bounds;
        let splitter = &mut self.splitter;

        splitter.feed(chunk, |record| {
            let fields = decode_record(format, record)?;
            if fields.len() != columns.len() {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "COPY record field count {} does not match column count {}",
                    fields.len(),
                    columns.len()
                )));
            }
            let next_count = bounds.len().saturating_add(1);
            batch_limits::check_row_count(next_count, &limits).map_err(map_batch_err)?;
            batch_limits::validate_chunk_total(next_count, &limits).map_err(map_batch_err)?;
            let bound = bind_copy_record(&table, &columns, &fields, &operation_id, schema)?;
            let row_bytes = bound_insert_byte_len(&bound)?;
            batch_limits::check_row_body_len(bounds.len(), row_bytes, &limits)
                .map_err(map_batch_err)?;
            bounds.push(bound);
            Ok(())
        })
    }

    /// CopyDone 到達時に呼ぶ。改行なしで終わった末尾レコード（存在する場合）を
    /// 確定させたうえで、0 行のままなら拒否する（既存の複数行 `INSERT` バッチと
    /// 同じ「空バッチ拒否」契約）。
    pub fn finish(mut self) -> Result<CopyInBatch, SqlSurfaceError> {
        let tail = self.splitter.finish();
        if !tail.is_empty() {
            let fields = decode_record(self.format, &tail)?;
            if fields.len() != self.columns.len() {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "COPY record field count {} does not match column count {}",
                    fields.len(),
                    self.columns.len()
                )));
            }
            let next_count = self.bounds.len().saturating_add(1);
            batch_limits::check_row_count(next_count, &self.limits).map_err(map_batch_err)?;
            batch_limits::validate_chunk_total(next_count, &self.limits).map_err(map_batch_err)?;
            let bound = bind_copy_record(
                &self.table,
                &self.columns,
                &fields,
                &self.operation_id,
                &self.schema,
            )?;
            let row_bytes = bound_insert_byte_len(&bound)?;
            batch_limits::check_row_body_len(self.bounds.len(), row_bytes, &self.limits)
                .map_err(map_batch_err)?;
            self.bounds.push(bound);
        }

        if self.bounds.is_empty() {
            return Err(SqlSurfaceError::invalid_input(
                "COPY FROM STDIN received no data rows",
            ));
        }

        Ok(CopyInBatch {
            table: self.table,
            row_count: self.bounds.len(),
            operation_id: self.operation_id,
            bounds: self.bounds,
            schema: self.schema,
        })
    }
}

/// [`CopyInSession::finish`] が返す、commit 直前の束縛済みバッチ
/// （[`crate::core::EngineCore::commit_copy_in`] のみが分解して使う）。
pub struct CopyInBatch {
    pub(crate) table: String,
    pub(crate) row_count: usize,
    pub(crate) operation_id: Option<OperationId>,
    pub(crate) bounds: Vec<BoundInsert>,
    pub(crate) schema: TableSchema,
}

impl CopyInBatch {
    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn text_schema() -> TableSchema {
        TableSchema::new(
            "t",
            vec![
                ColumnDef {
                    name: "body".to_string(),
                    ty: ColumnType::Text,
                    nullable: false,
                },
                ColumnDef {
                    name: "note".to_string(),
                    ty: ColumnType::Text,
                    nullable: true,
                },
            ],
        )
    }

    #[test]
    fn is_copy_statement_matches_case_insensitively_with_leading_whitespace() {
        assert!(is_copy_statement("  copy t (id) FROM STDIN"));
        assert!(is_copy_statement("COPY (SELECT 1) TO STDOUT"));
        assert!(is_copy_statement("COPY"));
    }

    #[test]
    fn is_copy_statement_rejects_identifier_prefix() {
        assert!(!is_copy_statement("COPYFOO t (id) FROM STDIN"));
        assert!(!is_copy_statement("SELECT 1"));
    }

    #[test]
    fn record_splitter_splits_on_raw_newline_and_strips_cr() {
        let mut splitter = RecordSplitter::new(CopyFormat::Text);
        let mut records: Vec<Vec<u8>> = Vec::new();
        splitter
            .feed(b"a\tb\r\nc\td\n", |r| {
                records.push(r.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(records, vec![b"a\tb".to_vec(), b"c\td".to_vec()]);
    }

    #[test]
    fn record_splitter_resumes_across_chunk_boundaries() {
        let mut splitter = RecordSplitter::new(CopyFormat::Text);
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0..b"a\tbc\n".len() {
            let byte = b"a\tbc\n"[i..=i].to_vec();
            splitter
                .feed(&byte, |r| {
                    records.push(r.to_vec());
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(records, vec![b"a\tbc".to_vec()]);
    }

    #[test]
    fn record_splitter_keeps_newline_inside_csv_quotes() {
        let mut splitter = RecordSplitter::new(CopyFormat::Csv);
        let mut records: Vec<Vec<u8>> = Vec::new();
        splitter
            .feed(b"1,\"a\nb\"\n2,c\n", |r| {
                records.push(r.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(records, vec![b"1,\"a\nb\"".to_vec(), b"2,c".to_vec()]);
    }

    #[test]
    fn record_splitter_handles_escaped_quote_pair_without_ending_quoted_region() {
        let mut splitter = RecordSplitter::new(CopyFormat::Csv);
        let mut records: Vec<Vec<u8>> = Vec::new();
        // `"a""b\nc"` は 1 個の引用符フィールド内に `""`（エスケープされた
        // 引用符）と改行を含む。エスケープ後もまだ引用符内であることを
        // 状態機械が正しく維持できるかを確認する。
        splitter
            .feed(b"\"a\"\"b\nc\"\n", |r| {
                records.push(r.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(records, vec![b"\"a\"\"b\nc\"".to_vec()]);
    }

    #[test]
    fn decode_text_field_resolves_standard_escapes() {
        assert_eq!(
            decode_text_field(b"a\\tb\\nc\\\\d").unwrap(),
            Some("a\tb\nc\\d".to_string())
        );
    }

    #[test]
    fn decode_text_field_whole_field_backslash_n_is_null() {
        assert_eq!(decode_text_field(b"\\N").unwrap(), None);
    }

    #[test]
    fn decode_text_field_rejects_unknown_escape() {
        assert!(decode_text_field(b"a\\xb").is_err());
    }

    #[test]
    fn decode_csv_record_distinguishes_null_and_empty_string() {
        let fields = decode_csv_record(b",\"\",value").unwrap();
        assert_eq!(
            fields,
            vec![None, Some(String::new()), Some("value".to_string())]
        );
    }

    #[test]
    fn decode_csv_record_unescapes_doubled_quotes() {
        let fields = decode_csv_record(b"\"a\"\"b\"").unwrap();
        assert_eq!(fields, vec![Some("a\"b".to_string())]);
    }

    #[test]
    fn decode_csv_record_rejects_unterminated_quote() {
        assert!(decode_csv_record(b"\"abc").is_err());
    }

    #[test]
    fn bind_copy_record_rejects_null_for_non_nullable_column() {
        let schema = text_schema();
        let columns = vec!["id".to_string(), "body".to_string()];
        let fields = vec![Some("1".to_string()), None];
        let err = bind_copy_record("t", &columns, &fields, &None, &schema).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_copy_record_accepts_null_for_nullable_column() {
        let schema = text_schema();
        let columns = vec!["id".to_string(), "body".to_string(), "note".to_string()];
        let fields = vec![Some("1".to_string()), Some("hello".to_string()), None];
        let bound = bind_copy_record("t", &columns, &fields, &None, &schema).unwrap();
        assert_eq!(bound.id, 1);
        assert_eq!(
            bound.values,
            vec![Value::Text("hello".to_string()), Value::Null]
        );
    }

    #[test]
    fn bind_copy_record_rejects_missing_id_column() {
        let schema = text_schema();
        let columns = vec!["body".to_string()];
        let fields = vec![Some("hello".to_string())];
        let err = bind_copy_record("t", &columns, &fields, &None, &schema).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }
}
