//! スキーマカタログ層（TASK-85、対象ビヘイビア: TABLE-1, TABLE-4, TABLE-5, TABLE-6。
//! ポインタ: `docs/spec/05-tasks.md` TASK-85・`docs/spec/04-behavior/data-model.md`）。
//!
//! 責務境界: `VECTOR(N)` 列型を含むテーブル定義（[`TableSchema`]）の DDL
//! （`CREATE TABLE`・`ALTER TABLE ADD COLUMN`）と、その永続化（`storage.rs` の
//! `redb::Database` を共有する専用テーブル）を担う。行データそのもの
//! （`ROWS_TABLE`）には一切アクセスしない（TABLE-4/TABLE-5 の「既存行数に非依存」を
//! 実現するための設計上の境界）。行エンコーダーの列対応・NULL 解決（TASK-86）・
//! アリーナデコード（TASK-87）・テナント境界統合（TASK-89）・SQL surface からの
//! DDL 受理は本モジュールの責務外で、後続タスクが本モジュールの API に依存する。
//!
//! `storage.rs` との関係: `Storage::db()`（`pub(crate)`）を経由して同一
//! `redb::Database` ハンドルを共有し、カタログ専用のテーブル（[`CATALOG_TABLE`]）に
//! 書き込む。`ROWS_TABLE` の行エンコーディング（v2, RLS フィールド同居）とは
//! 独立したフォーマットを持つ。

use std::fmt;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::storage::Storage;

/// カタログ値を格納するテーブル。キーはテーブル名、値は [`encode_schema`] で
/// エンコードしたバイト列。`ROWS_TABLE`（`storage.rs`）とは別テーブルとし、
/// カタログの読み書き（TABLE-4/TABLE-5 の O(1) 契約）が行データに触れないようにする。
const CATALOG_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog");

/// カタログのテキスト形式フォーマットバージョン識別子。値の追加・変更は
/// 破壊的変更として扱い、この値を更新する。旧バージョンの読み出しは
/// マイグレーションを提供せず fail-closed に拒否する
/// （`storage.rs` の `ROW_FORMAT_VERSION` と同じ方針）。
const CATALOG_FORMAT_VERSION_LINE: &str = "v1";

/// 識別子（テーブル名・列名）のバイト長上限。PostgreSQL の識別子長慣習に整合させた
/// 実装ローカルな値（対象ビヘイビア: TABLE-6）。
const MAX_IDENTIFIER_LEN: usize = 63;

/// `VECTOR(N)` の次元数上限。`storage.rs::MAX_EMBEDDING_DIM` と同値を維持する
/// （カタログで宣言可能な次元が永続化層で扱える上限を超えないようにするため）。
/// 0 はテーブル単位で次元固定という TABLE-1 の趣旨に反するため、下限は 1 とする。
const MAX_VECTOR_DIM: u32 = 65_536;

// 上記コメントの「同値を維持する」という前提を、単なる複製定数のコメントに留めず
// コンパイル時に強制する。片方だけを変更するとここでビルドが失敗し、ドリフトを防ぐ。
const _: () = assert!(
    MAX_VECTOR_DIM == crate::storage::MAX_EMBEDDING_DIM,
    "catalog::MAX_VECTOR_DIM must stay in sync with storage::MAX_EMBEDDING_DIM"
);

/// 1 テーブルが持てる列数の上限。カタログ値のデコード時、この値を超える宣言列数は
/// アロケーション前に拒否する（.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
const MAX_COLUMN_COUNT: usize = 256;

/// カタログ値（エンコード済みバイト列）のバイト長上限。デコード前に検証し、
/// 無制限な文字列アロケーションを防ぐ。
const MAX_CATALOG_VALUE_LEN: usize = 1024 * 1024;

/// [`Storage::list_tables`] が返せるテーブル数の上限（無制限 `Vec` 確保を避ける。
/// security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_LIST_TABLES: usize = 10_000;

/// カタログ層の公開エラー型。`redb` 操作由来のエラーは `Backend` に一本化し、
/// それ以外はすべて fail-closed な明示的な拒否理由を持つ。
///
/// `StorageError`（`storage.rs`）とは独立した型として定義する。`StorageError` は
/// `redb::Error` への変換元を一括で受ける blanket `From` 実装を持つため、
/// coherence 制約によりそこへ `CatalogError` からの変換を個別追加できない
/// （`storage.rs` の設計メモ参照）。
#[derive(Debug)]
pub enum CatalogError {
    /// `redb` 側で発生したエラー（I/O・トランザクション競合等）。
    Backend(redb::Error),
    /// 識別子・型・次元数・カタログ値のフォーマットが不正（TABLE-6）。
    /// 欠落フィールド・未知の型・不正次元・区切り文字混入等をすべてここに集約する。
    Invalid(String),
    /// 指定したテーブルがカタログに存在しない。
    TableNotFound(String),
    /// `CREATE TABLE` で同名テーブルが既に存在する（上書きしない。TABLE-4 前提）。
    TableAlreadyExists(String),
    /// `ALTER TABLE ADD COLUMN` で追加しようとした列名が既存列と重複する。
    ColumnAlreadyExists(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Backend(e) => write!(f, "catalog backend error: {e}"),
            CatalogError::Invalid(msg) => write!(f, "invalid catalog data: {msg}"),
            CatalogError::TableNotFound(name) => write!(f, "table not found: {name}"),
            CatalogError::TableAlreadyExists(name) => write!(f, "table already exists: {name}"),
            CatalogError::ColumnAlreadyExists(name) => {
                write!(f, "column already exists: {name}")
            }
        }
    }
}

impl std::error::Error for CatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CatalogError::Backend(e) => Some(e),
            CatalogError::Invalid(_)
            | CatalogError::TableNotFound(_)
            | CatalogError::TableAlreadyExists(_)
            | CatalogError::ColumnAlreadyExists(_) => None,
        }
    }
}

// `storage.rs` の `StorageError` と同じ橋渡し方針。`redb` の各操作が返す複数のエラー型を
// 一括して `CatalogError::Backend` へ変換する。
impl<E> From<E> for CatalogError
where
    E: Into<redb::Error>,
{
    fn from(e: E) -> Self {
        CatalogError::Backend(e.into())
    }
}

pub type Result<T> = std::result::Result<T, CatalogError>;

/// 列のデータ型（閉じた集合）。デコード時に未知の型名を検出した場合は
/// 既知の型へ黙殺フォールバックせず `CatalogError::Invalid` で拒否する（TABLE-6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// 可変長テキスト列。
    Text,
    /// 固定次元の埋め込み列（`VECTOR(N)`）。次元 `N` はテーブル単位で固定される
    /// （TABLE-1）。0 と `MAX_VECTOR_DIM` 超過は encode・decode 両側で拒否する。
    Vector(u32),
}

/// テーブル定義中の 1 列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    /// `ALTER TABLE ADD COLUMN` で追加された列は暗黙 nullable とする（TABLE-5）。
    /// 「新列バイトを持たない既存行は NULL 扱い」という契約を担う情報であり、
    /// 実際の行デコード時の NULL 解決は行エンコーダー（TASK-86）の責務。
    /// 本モジュールはこのフラグを保持・往復させるのみ。
    pub nullable: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, ty: ColumnType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable,
        }
    }
}

/// テーブル定義。列の宣言順を保持する（`ALTER TABLE ADD COLUMN` は末尾追記のみを
/// 許可し、既存行の再エンコードを要求しないレイアウトを前提とする。TABLE-5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

impl TableSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            columns,
        }
    }

    /// 宣言済みの埋め込み次元（`VECTOR(N)` 列のうち最初に見つかったもの）。
    /// TABLE-1 の「テーブル単位で次元固定」を表現するヘルパ。
    pub fn vector_dim(&self) -> Option<u32> {
        self.columns.iter().find_map(|c| match c.ty {
            ColumnType::Vector(dim) => Some(dim),
            ColumnType::Text => None,
        })
    }

    /// 挿入経路（TASK-86 以降）が、宣言済み次元と一致しない埋め込みを拒否するための
    /// 検証ヘルパ（TABLE-1）。`VECTOR` 列を持たないテーブルへの呼び出しも fail-closed
    /// に拒否する。
    pub fn validate_embedding_dim(&self, dim: usize) -> Result<()> {
        let expected = self
            .vector_dim()
            .ok_or_else(|| CatalogError::Invalid("table has no VECTOR column".to_string()))?;
        if dim as u64 != expected as u64 {
            return Err(CatalogError::Invalid(format!(
                "embedding dim mismatch: expected {expected}, got {dim}"
            )));
        }
        Ok(())
    }
}

/// 識別子（テーブル名・列名）の検証（TABLE-6）。`[A-Za-z_][A-Za-z0-9_]*` のみを許容し、
/// 空文字列・非 ASCII・区切り文字混入・長さ上限超過を fail-closed に拒否する。
/// encode 側・decode 側の両方から呼ばれ、永続データが手で書き換えられた場合も
/// 同じ検証を通す。
fn validate_identifier(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(CatalogError::Invalid("identifier is empty".to_string()));
    }
    if s.len() > MAX_IDENTIFIER_LEN {
        return Err(CatalogError::Invalid(format!(
            "identifier too long: {} bytes",
            s.len()
        )));
    }
    let mut chars = s.chars();
    // 上記 is_empty チェック済みのため先頭文字は必ず存在する。
    let first = chars
        .next()
        .ok_or_else(|| CatalogError::Invalid("identifier is empty".to_string()))?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(CatalogError::Invalid(format!(
            "identifier must start with [A-Za-z_]: {s:?}"
        )));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(CatalogError::Invalid(format!(
                "identifier contains invalid character: {s:?}"
            )));
        }
    }
    Ok(())
}

/// `VECTOR(N)` の次元数検証（TABLE-6）。0 と `MAX_VECTOR_DIM` 超過を拒否する。
fn validate_vector_dim(dim: u32) -> Result<()> {
    if dim == 0 {
        return Err(CatalogError::Invalid(
            "VECTOR dimension must not be zero".to_string(),
        ));
    }
    if dim > MAX_VECTOR_DIM {
        return Err(CatalogError::Invalid(format!(
            "VECTOR dimension too large: {dim}"
        )));
    }
    Ok(())
}

fn validate_column(column: &ColumnDef) -> Result<()> {
    validate_identifier(&column.name)?;
    if let ColumnType::Vector(dim) = column.ty {
        validate_vector_dim(dim)?;
    }
    Ok(())
}

/// スキーマ全体の検証（テーブル名・列定義・列数上限・列名重複・`VECTOR` 列数）。
/// `create_table`・`alter_table_add_column`（追加後のスキーマ）の両方から呼ばれる。
///
/// TABLE-1（「次元はテーブル単位で固定」）を満たすため、`VECTOR` 列は高々 1 つに
/// 制限する。複数の `VECTOR` 列を許すと [`TableSchema::vector_dim`] が先頭列のみを
/// 見て後続列を黙殺する fail-open な状態になり得るため、ここで拒否する
/// （.claude/rules/security.md「不安全な設計」）。
fn validate_schema(schema: &TableSchema) -> Result<()> {
    validate_identifier(&schema.name)?;
    if schema.columns.is_empty() {
        return Err(CatalogError::Invalid(
            "table must have at least one column".to_string(),
        ));
    }
    if schema.columns.len() > MAX_COLUMN_COUNT {
        return Err(CatalogError::Invalid(format!(
            "too many columns: {}",
            schema.columns.len()
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(schema.columns.len());
    let mut vector_column_count = 0u32;
    for column in &schema.columns {
        validate_column(column)?;
        if seen.contains(&column.name.as_str()) {
            return Err(CatalogError::Invalid(format!(
                "duplicate column name: {}",
                column.name
            )));
        }
        seen.push(column.name.as_str());
        if matches!(column.ty, ColumnType::Vector(_)) {
            vector_column_count += 1;
        }
    }
    if vector_column_count > 1 {
        return Err(CatalogError::Invalid(format!(
            "table must declare at most one VECTOR column (TABLE-1: dimension is fixed per table), got {vector_column_count}"
        )));
    }
    Ok(())
}

/// [`TableSchema`] をカタログのテキスト形式へエンコードする。1 行目に
/// フォーマットバージョン、2 行目に列数、以降 1 行 1 列（`name:type:dim:nullable`
/// の 4 フィールドを `:` 区切り。識別子は `validate_identifier` により `:` を
/// 含み得ないため、区切り文字との衝突は起きない）。エンコード時にも
/// `validate_schema` を通し、不正なスキーマを永続化しない（fail-closed）。
fn encode_schema(schema: &TableSchema) -> Result<Vec<u8>> {
    validate_schema(schema)?;
    let mut out = String::new();
    out.push_str(CATALOG_FORMAT_VERSION_LINE);
    out.push('\n');
    out.push_str(&format!("cols:{}\n", schema.columns.len()));
    for column in &schema.columns {
        let (type_name, dim_field) = match column.ty {
            ColumnType::Text => ("text", "-".to_string()),
            ColumnType::Vector(dim) => ("vector", dim.to_string()),
        };
        let nullable_field = if column.nullable { "1" } else { "0" };
        out.push_str(&format!(
            "{}:{}:{}:{}\n",
            column.name, type_name, dim_field, nullable_field
        ));
    }
    if out.len() > MAX_CATALOG_VALUE_LEN {
        return Err(CatalogError::Invalid(format!(
            "encoded catalog value too large: {} bytes",
            out.len()
        )));
    }
    Ok(out.into_bytes())
}

/// カタログのテキスト形式から [`TableSchema`] をデコードする。`table_name` は
/// redb のキー（呼び出し元が既知）から渡され、値バイト列には含まれない。
/// 欠落フィールド・余剰フィールド・未知バージョン・不正 UTF-8・不正次元・
/// 切り詰め・識別子違反はすべて `Err`（黙殺フォールバックしない。TABLE-6）。
fn decode_schema(table_name: &str, bytes: &[u8]) -> Result<TableSchema> {
    if bytes.len() > MAX_CATALOG_VALUE_LEN {
        return Err(CatalogError::Invalid(format!(
            "catalog value too large: {} bytes",
            bytes.len()
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CatalogError::Invalid("catalog value is not valid UTF-8".to_string()))?;

    let mut lines = text.split('\n');

    let version_line = lines
        .next()
        .ok_or_else(|| CatalogError::Invalid("catalog value is empty".to_string()))?;
    if version_line != CATALOG_FORMAT_VERSION_LINE {
        return Err(CatalogError::Invalid(format!(
            "unknown catalog format version: {version_line:?}"
        )));
    }

    let cols_line = lines.next().ok_or_else(|| {
        CatalogError::Invalid("catalog value truncated: missing cols line".to_string())
    })?;
    let count_str = cols_line
        .strip_prefix("cols:")
        .ok_or_else(|| CatalogError::Invalid(format!("malformed cols line: {cols_line:?}")))?;
    let col_count: usize = count_str
        .parse()
        .map_err(|_| CatalogError::Invalid(format!("malformed column count: {count_str:?}")))?;
    if col_count > MAX_COLUMN_COUNT {
        return Err(CatalogError::Invalid(format!(
            "too many columns: {col_count}"
        )));
    }

    // 残り行を集める。末尾の空行（トレーリング改行）を許容しつつ、
    // 宣言された列数と実際の行数が一致しない場合は切り詰め・余剰として拒否する。
    let remaining: Vec<&str> = lines.collect();
    let remaining = match remaining.last() {
        Some(&"") => &remaining[..remaining.len() - 1],
        _ => &remaining[..],
    };
    if remaining.len() != col_count {
        return Err(CatalogError::Invalid(format!(
            "catalog value line count mismatch: expected {col_count} columns, got {} lines",
            remaining.len()
        )));
    }

    let mut columns = Vec::with_capacity(col_count);
    for line in remaining {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() != 4 {
            return Err(CatalogError::Invalid(format!(
                "malformed column line: {line:?}"
            )));
        }
        let (name, type_name, dim_field, nullable_field) =
            (fields[0], fields[1], fields[2], fields[3]);
        validate_identifier(name)?;

        let ty = match type_name {
            "text" => {
                if dim_field != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "text column must not declare a dimension: {line:?}"
                    )));
                }
                ColumnType::Text
            }
            "vector" => {
                let dim: u32 = dim_field.parse().map_err(|_| {
                    CatalogError::Invalid(format!("malformed vector dimension: {line:?}"))
                })?;
                validate_vector_dim(dim)?;
                ColumnType::Vector(dim)
            }
            other => {
                return Err(CatalogError::Invalid(format!(
                    "unknown column type: {other:?}"
                )))
            }
        };

        let nullable = match nullable_field {
            "0" => false,
            "1" => true,
            other => {
                return Err(CatalogError::Invalid(format!(
                    "malformed nullable field: {other:?}"
                )))
            }
        };

        columns.push(ColumnDef::new(name, ty, nullable));
    }

    let schema = TableSchema::new(table_name, columns);
    // デコード結果を再度検証する（列数上限・列名重複・識別子）。手書きの不正データが
    // フィールドごとの検証をすり抜けても、スキーマ全体の不変条件はここで担保する。
    validate_schema(&schema)?;
    Ok(schema)
}

/// カタログ DDL API。`Storage`（`storage.rs`）の拡張として実装し、
/// `Storage::db()` を経由して `ROWS_TABLE` とは別のテーブル（[`CATALOG_TABLE`]）
/// のみを読み書きする。行データへは一切アクセスしない
/// （TABLE-4/TABLE-5 の「既存行数に非依存」の根拠）。
impl Storage {
    /// 新規テーブルを定義する（TABLE-4）。同名テーブルが既に存在する場合は
    /// 上書きせず `Err` を返す。カタログテーブルのみを触る単一 write txn で完結し、
    /// `ROWS_TABLE` の内容・行数に依存しない O(1) 操作となる。
    pub fn create_table(&self, schema: &TableSchema) -> Result<()> {
        // スキーマ検証は `encode_schema` 内の `validate_schema` に集約する（write txn を
        // 開く前に fail-closed に拒否される。ここで別途 `validate_schema` を呼ぶ必要はない）。
        let encoded = encode_schema(schema)?;
        let write_txn = self.db().begin_write()?;
        {
            let mut table = write_txn.open_table(CATALOG_TABLE)?;
            if table.get(schema.name.as_str())?.is_some() {
                return Err(CatalogError::TableAlreadyExists(schema.name.clone()));
            }
            table.insert(schema.name.as_str(), encoded.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// 既存テーブルへ列を末尾追記する（TABLE-5）。追加列は暗黙 nullable として
    /// 保持され、既存行のバイト列には一切触れない（`ROWS_TABLE` 非アクセス）。
    /// 「新列バイトが無い既存行は NULL 扱い」という TABLE-5 の前提は追加列が
    /// nullable であることに依存するため、`column.nullable == false` は
    /// fail-closed に拒否する（NOT NULL 制約を、バイトを持たない既存行に対して
    /// 事実上満たせないまま永続化することを防ぐ。security.md「不安全な設計」）。
    /// 対象テーブル不存在・列名重複も `Err`。
    pub fn alter_table_add_column(&self, table_name: &str, column: ColumnDef) -> Result<()> {
        validate_identifier(table_name)?;
        validate_column(&column)?;
        if !column.nullable {
            return Err(CatalogError::Invalid(
                "column added via ALTER TABLE ADD COLUMN must be nullable (TABLE-5)".to_string(),
            ));
        }
        let write_txn = self.db().begin_write()?;
        {
            let mut table = write_txn.open_table(CATALOG_TABLE)?;
            let existing: Vec<u8> = {
                let guard = table
                    .get(table_name)?
                    .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
                guard.value().to_vec()
            };
            let mut schema = decode_schema(table_name, &existing)?;
            if schema.columns.iter().any(|c| c.name == column.name) {
                return Err(CatalogError::ColumnAlreadyExists(column.name.clone()));
            }
            if schema.columns.len() >= MAX_COLUMN_COUNT {
                return Err(CatalogError::Invalid(format!(
                    "too many columns: {}",
                    schema.columns.len() + 1
                )));
            }
            schema.columns.push(column);
            let encoded = encode_schema(&schema)?;
            table.insert(table_name, encoded.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// テーブル定義を読み出す（スナップショット読み取り）。存在しない場合は
    /// `Err(CatalogError::TableNotFound)`。
    pub fn get_table_schema(&self, table_name: &str) -> Result<TableSchema> {
        // `alter_table_add_column` と同様、redb キーとして引く前に識別子を検証する。
        // 不正形式の名前は `TableNotFound`（存在しない）ではなく `Invalid`（形式不正）で
        // 拒否し、両 API 間でエラーバリアントを揃える。
        validate_identifier(table_name)?;
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(CATALOG_TABLE) {
            Ok(t) => t,
            // カタログテーブル未作成（1 テーブルも定義していない）は「存在しない」として扱う。
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(CatalogError::TableNotFound(table_name.to_string()))
            }
            Err(e) => return Err(e.into()),
        };
        let guard = table
            .get(table_name)?
            .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
        decode_schema(table_name, guard.value())
    }

    /// 定義済みテーブル名の一覧をスナップショット読み取りで返す。件数上限
    /// （[`MAX_LIST_TABLES`]）を超える場合は `Err`（無制限 `Vec` 確保を防ぐ）。
    pub fn list_tables(&self) -> Result<Vec<String>> {
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(CATALOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut names = Vec::new();
        for entry in table.iter()? {
            let (key, _value) = entry?;
            if names.len() >= MAX_LIST_TABLES {
                return Err(CatalogError::Invalid(format!(
                    "too many tables: exceeds {MAX_LIST_TABLES}"
                )));
            }
            let name = key.value();
            // `get_table_schema` と同じ検証をここでも通す。通常経路で書かれるキーは
            // すべて `create_table` の `validate_schema` を経ているため常に合法だが、
            // 手書きの不正データが直接 redb へ書き込まれていた場合に、そのまま
            // 一覧へ紛れ込ませない（`decode_schema` と同じ fail-closed 方針）。
            validate_identifier(name)?;
            names.push(name.to_string());
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストごとに一意な DB ファイルパスを払い出す（`storage.rs` の同名ヘルパーと
    /// 同じ方針）。`list_tables` のテストは `Storage`（redb ファイル）を必要とするため、
    /// この unit test モジュールにも複製する。
    fn unique_db_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vector-db-engine-catalog-unit-{label}-{}-{seq}.redb",
            std::process::id()
        ));
        path
    }

    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn validate_schema_rejects_more_than_one_vector_column() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(384), false),
                ColumnDef::new("other", ColumnType::Vector(8), false),
            ],
        );
        assert!(matches!(
            validate_schema(&schema),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn validate_identifier_accepts_valid_forms() {
        assert!(validate_identifier("a").is_ok());
        assert!(validate_identifier("_foo").is_ok());
        assert!(validate_identifier("foo_bar123").is_ok());
        assert!(validate_identifier("A1").is_ok());
    }

    #[test]
    fn validate_identifier_rejects_invalid_forms() {
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("1abc").is_err());
        assert!(validate_identifier("-abc").is_err());
        assert!(validate_identifier("a b").is_err());
        assert!(validate_identifier("a:b").is_err());
        assert!(validate_identifier("a\nb").is_err());
        assert!(validate_identifier("héllo").is_err());
        assert!(validate_identifier(&"a".repeat(MAX_IDENTIFIER_LEN + 1)).is_err());
    }

    #[test]
    fn validate_vector_dim_rejects_zero_and_overflow() {
        assert!(validate_vector_dim(0).is_err());
        assert!(validate_vector_dim(MAX_VECTOR_DIM + 1).is_err());
        assert!(validate_vector_dim(1).is_ok());
        assert!(validate_vector_dim(MAX_VECTOR_DIM).is_ok());
    }

    #[test]
    fn encode_decode_roundtrip_preserves_schema() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(384), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let bytes = b"v99\ncols:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_column_lines() {
        let bytes = b"v1\ncols:2\nfoo:text:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn decode_rejects_unknown_type() {
        let bytes = b"v1\ncols:1\nfoo:blob:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn decode_rejects_bad_dimension() {
        let bytes = b"v1\ncols:1\nfoo:vector:0:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::Invalid(_))
        ));
        let bytes = b"v1\ncols:1\nfoo:vector:not-a-number:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let bytes = vec![0xff, 0xfe, 0xfd];
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn table_schema_validate_embedding_dim() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(384), false)],
        );
        assert!(schema.validate_embedding_dim(384).is_ok());
        assert!(schema.validate_embedding_dim(128).is_err());

        let no_vector = TableSchema::new(
            "docs2",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        assert!(no_vector.validate_embedding_dim(384).is_err());
    }

    // --- Storage::list_tables ---------------------------------------------
    // MAX_LIST_TABLES 上限超過時の Err 分岐（security.md「無制限リソース確保」対応）と、
    // カタログテーブル未作成（空 DB）時の Ok(Vec::new()) 分岐を検証する。
    // `MAX_LIST_TABLES` / `CATALOG_TABLE` が非公開のため、`tests/catalog.rs`
    // （クレート外の統合テスト）ではなくこの unit test モジュールに置く。

    #[test]
    fn list_tables_returns_empty_vec_when_catalog_table_not_yet_created() {
        let path = unique_db_path("list-tables-empty");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // 1 テーブルも create_table していない状態（catalog テーブル自体が未作成）。
        let tables = storage.list_tables().expect("list_tables on empty db");
        assert!(tables.is_empty());
    }

    #[test]
    fn list_tables_rejects_when_exceeding_max_list_tables() {
        let path = unique_db_path("list-tables-exceeds-max");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // MAX_LIST_TABLES を超える件数を用意する。create_table を MAX_LIST_TABLES+1 回
        // 呼ぶと write txn ごとのコミットコストでテストが極端に遅くなるため、
        // 単一の write txn へ直接まとめて挿入する（create_table の O(1) 契約検証は
        // table4 系の統合テストの責務であり、ここでの目的は list_tables 自体の
        // DoS 対策（security.md「無制限リソース確保」）の検証）。
        let schema = TableSchema::new(
            "seed",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let encoded = encode_schema(&schema).expect("encode seed schema");
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let mut table = write_txn.open_table(CATALOG_TABLE).expect("open_table");
                for i in 0..=MAX_LIST_TABLES {
                    let name = format!("t{i}");
                    table
                        .insert(name.as_str(), encoded.as_slice())
                        .expect("insert seed row");
                }
            }
            write_txn.commit().expect("commit");
        }

        let result = storage.list_tables();
        assert!(
            matches!(result, Err(CatalogError::Invalid(_))),
            "expected Err(Invalid) once table count exceeds MAX_LIST_TABLES, got {result:?}"
        );
    }
}
