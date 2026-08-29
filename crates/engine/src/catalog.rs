//! スキーマカタログ層（TASK-85、対象ビヘイビア: TABLE-1, TABLE-4, TABLE-5, TABLE-6。
//! ポインタ: `docs/spec/05-tasks.md` TASK-85・`docs/spec/04-behavior/data-model.md`）。
//!
//! 責務境界: `VECTOR(N)` 列型を含むテーブル定義（[`TableSchema`]）の DDL
//! （`CREATE TABLE`・`ALTER TABLE ADD COLUMN`・`DROP TABLE` 相当の
//! [`Storage::drop_table`]）と、その永続化（`storage.rs` の `redb::Database` を
//! 共有する専用テーブル）を担う。行データそのもの（`ROWS_TABLE`）には一切
//! アクセスしない設計上の境界とする（TABLE-4/TABLE-5）。[`Storage::drop_table`] は
//! 例外的にテーブルスコープ行ストア（`user_rows/{table}`）をカタログエントリと
//! 同一トランザクションで削除するが、これは DDL のライフサイクル管理としての
//! 削除であり、行の中身（値）を読み書きするものではない。
//! 行エンコーダーの列対応・NULL 解決（TASK-86）・
//! アリーナデコード（TASK-87）・テナント境界統合（TASK-89）・SQL surface からの
//! DDL 受理は本モジュールの責務外で、後続タスクが本モジュールの API に依存する。
//!
//! `storage.rs` との関係: `Storage::db()`（`pub(crate)`）を経由して同一
//! `redb::Database` ハンドルを共有し、カタログ専用のテーブル（[`CATALOG_TABLE`]）に
//! 書き込む。`ROWS_TABLE` の行エンコーディング（v2, RLS フィールド同居）とは
//! 独立したフォーマットを持つ。
//!
//! テーブルスコープ行 API（TASK-146、対象ビヘイビア: EXT-1, EXT-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-146・`docs/spec/04-behavior/extensions.md`）:
//! テーブルごとに動的な redb テーブル（`user_rows/{table_name}`）へ行を分離し、
//! 挿入時に本モジュールの [`TableSchema::validate_embedding_dim`] で宣言次元との
//! 完全一致を検証する。次元検証はここで完結し、RLS ポリシー評価（可視性判定）は
//! 従来どおり呼び出し元（TASK-133 以降）の責務のまま変えない。
//!
//! `sql::allowlist` との関係（TASK-74、対象ビヘイビア: SQL-8）: `impl TableLookup for
//! Storage`（本ファイル下部）が SQL 表層の FROM テーブル存在確認を橋渡しする。

use std::fmt;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::row_codec::{self, Value as RowCodecValue};
use crate::sql::allowlist::{SqlSurfaceError, TableLookup};
use crate::storage::{Row as StorageRow, RowInput, Storage, StorageError, Visibility};

/// カタログ値を格納するテーブル。キーはテーブル名、値は [`encode_schema`] で
/// エンコードしたバイト列。`ROWS_TABLE`（`storage.rs`）とは別テーブルとし、
/// カタログの読み書き（TABLE-4/TABLE-5）が行データに触れないようにする。
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
/// 下限は 1 とする（TABLE-1）。
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
    /// 識別子・型・次元数のフォーマットが不正（TABLE-6）。呼び出し側（ユーザー入力の
    /// 識別子・スキーマ定義）が渡した値そのものの検証失敗であり、`detail` は
    /// 呼び出し元が把握済みの情報のみを含む。
    Invalid(String),
    /// redb に格納済みのカタログ値（[`decode_schema`]）のデコードに失敗した。
    /// ユーザーが今回渡した入力の構文エラーではなく、ストレージ側の破損・想定外の
    /// 格納状態を示す。`detail` には格納済みバイト列由来の断片（`cols_line` 等）が
    /// 含まれ得るため、`Invalid` と区別し、wire クライアントへは detail を渡さず
    /// 汎用メッセージへ丸める（Issue #55 レビュー指摘。`.claude/rules/security.md`
    /// 「不安全な設計」「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応）。
    CorruptSchema(String),
    /// 指定したテーブルがカタログに存在しない。
    TableNotFound(String),
    /// `CREATE TABLE` で同名テーブルが既に存在する（上書きしない。TABLE-4 前提）。
    TableAlreadyExists(String),
    /// `ALTER TABLE ADD COLUMN` で追加しようとした列名が既存列と重複する。
    ColumnAlreadyExists(String),
    /// テーブルスコープ行 API（TASK-146）で、指定した行 ID がそのテーブル内に
    /// 存在しない。他テーブルの同一 ID は無関係（テーブル帰属した独立ストア）。
    RowNotFound(u64),
    /// 既存 DB の行テーブルが旧フォーマット（物理キーが `id` のみ）で、現行の
    /// `(tenant_id, id)` 複合キー（対象ビヘイビア: TABLE-12）と互換でない。
    /// 旧データを別テナントの行として読み出す fail-open を避けるため、
    /// マイグレーションは提供せず fail-closed に拒否する（`redb` の
    /// `TableError::TableTypeMismatch` を本 variant へ写像する）。エラー文言には
    /// テーブル名・テナント ID を含めない（存在情報を漏らさない。security.md P0）。
    IncompatibleRowKeyFormat,
    /// テーブル単位の世代カウンタ（[`bump_table_generation_in_txn`]）が `u64` の
    /// 上限に達した。現実的には到達しないが、`checked_add` の網羅性のため扱う
    /// （`storage.rs::StorageError::GenerationCounterOverflow` と同じ方針）。
    TableGenerationCounterOverflow,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Backend(e) => write!(f, "catalog backend error: {e}"),
            CatalogError::Invalid(msg) => write!(f, "invalid catalog data: {msg}"),
            CatalogError::CorruptSchema(msg) => write!(f, "corrupt catalog schema: {msg}"),
            CatalogError::TableNotFound(name) => write!(f, "table not found: {name}"),
            CatalogError::TableAlreadyExists(name) => write!(f, "table already exists: {name}"),
            CatalogError::ColumnAlreadyExists(name) => {
                write!(f, "column already exists: {name}")
            }
            CatalogError::RowNotFound(id) => write!(f, "row not found: id={id}"),
            CatalogError::IncompatibleRowKeyFormat => {
                write!(f, "incompatible row store key format: rebuild required")
            }
            CatalogError::TableGenerationCounterOverflow => {
                write!(f, "table generation counter overflow")
            }
        }
    }
}

impl std::error::Error for CatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CatalogError::Backend(e) => Some(e),
            CatalogError::Invalid(_)
            | CatalogError::CorruptSchema(_)
            | CatalogError::TableNotFound(_)
            | CatalogError::TableAlreadyExists(_)
            | CatalogError::ColumnAlreadyExists(_)
            | CatalogError::RowNotFound(_)
            | CatalogError::IncompatibleRowKeyFormat
            | CatalogError::TableGenerationCounterOverflow => None,
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
    /// 固定次元の埋め込み列（`VECTOR(N)`、TABLE-1）。0 と `MAX_VECTOR_DIM` 超過は
    /// encode・decode 両側で拒否する。
    Vector(u32),
}

/// テーブル定義中の 1 列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    /// `ALTER TABLE ADD COLUMN` で追加された列は暗黙 nullable とする（TABLE-5）。
    /// 実際の行デコード時の NULL 解決は行エンコーダー（TASK-86）の責務であり、
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
/// 許可する。TABLE-5）。
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

    /// 宣言済みの埋め込み次元（`VECTOR(N)` 列のうち最初に見つかったもの、TABLE-1）。
    pub fn vector_dim(&self) -> Option<u32> {
        self.columns.iter().find_map(|c| match c.ty {
            ColumnType::Vector(dim) => Some(dim),
            ColumnType::Text => None,
        })
    }

    /// 挿入経路（TASK-86 以降）が、宣言済み次元と一致しない埋め込みを拒否するための
    /// 検証ヘルパ（TABLE-1）。`VECTOR` 列を持たないテーブルへの呼び出しも
    /// fail-closed に拒否する。
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
///
/// `pub(crate)`: `tenant.rs`（TASK-95・対象ビヘイビア: RECOVER-4）が書き込みガード API
/// （`insert_row`/`update_row`/`delete_row`）内の同一 write トランザクションから、
/// テーブル名検証をここへ委譲する（重複実装を作らない。クレート外へは公開しない）。
pub(crate) fn validate_identifier(s: &str) -> Result<()> {
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
/// `VECTOR` 列は高々 1 つに制限する（TABLE-1）。複数の `VECTOR` 列を許すと
/// [`TableSchema::vector_dim`] が先頭列のみを見て後続列を黙殺する fail-open な
/// 状態になり得るため、ここで拒否する（.claude/rules/security.md「不安全な設計」）。
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
            "table must declare at most one VECTOR column, got {vector_column_count}"
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
///
/// 返すエラーは常に [`CatalogError::CorruptSchema`]（[`decode_schema_body`] が
/// 内部で使う `validate_identifier`・`validate_vector_dim`・`validate_schema` は
/// 汎用の `CatalogError::Invalid` を返すため、ここで格納済みデータのデコード失敗
/// として明示的に読み替える）。呼び出し元（[`TableLookup for Storage`](Storage)）は
/// この変換を前提に `Invalid`（ユーザー入力の識別子形式不正）と区別して wire_code を
/// 割り当てる（Issue #55 レビュー指摘）。
fn decode_schema(table_name: &str, bytes: &[u8]) -> Result<TableSchema> {
    decode_schema_body(table_name, bytes).map_err(|e| match e {
        CatalogError::Invalid(msg) => CatalogError::CorruptSchema(msg),
        other => other,
    })
}

fn decode_schema_body(table_name: &str, bytes: &[u8]) -> Result<TableSchema> {
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

/// ユーザーテーブル `table_name` に対応する行ストア用の動的 redb テーブル名を組み立てる
/// （TASK-146・EXT-2）。`validate_identifier` が `/` を含む文字列を許容しないため、
/// 固定テーブル（`rows`／`catalog`）ともユーザーテーブル同士とも名前衝突しない。
/// 呼び出し元は必ず先に `validate_identifier(table_name)` を通してから呼ぶこと
/// （本関数自身は検証を行わない）。
///
/// この動的テーブルは [`CATALOG_TABLE`] のエントリとは別ライフサイクルで管理されている
/// （`create_table` は `CATALOG_TABLE` のみ書き込み、本関数が指す行テーブルは初回挿入まで
/// 未作成のまま）。[`Storage::drop_table`] が `CATALOG_TABLE` のエントリ削除と同一 write
/// トランザクション内で本関数が返す行テーブルも削除する。残留を許すとテーブル再作成時に
/// 旧次元の行データが残り、EXT-2 の次元固定の不変条件を静かに破るため（Issue #179・
/// PR #151 レビュー据え置き事項）。
///
/// `pub(crate)` で公開する: `arena.rs`（TASK-87、対象ビヘイビア: TABLE-8）が
/// コールドスタート・アリーナ構築時に、対象テーブルの行テーブルだけを単一の
/// `read_txn` 上で直接開くために必要（クレート外へは公開しない）。
pub(crate) fn user_rows_table_name(table_name: &str) -> String {
    format!("user_rows/{table_name}")
}

/// 行ストア（`user_rows/{table_name}`）の物理キー型（対象ビヘイビア: TABLE-12・RLS-9。
/// ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-12・`rls.md` RLS-9）。
///
/// キーはサーバー側導出テナント（`policy.rs::PolicyContext::tenant_id`）と行 `id` の
/// 複合キーで、行 `id` の一意性スコープをテナント内に閉じる。異なるテナントは同一の
/// `id` を独立に保持でき、`crate::tenant::insert_row` の重複検出は自テナントの
/// 名前空間内だけを見るため、他テナント行の存在有無が `23505` の有無として観測される
/// 経路が構造的に消える（codex-review P0 指摘・PR #194）。`redb` のタプルキーは
/// 要素順（tenant_id 昇順 → id 昇順）で全順序を持つため、全件走査は従来どおり
/// 単一の range 走査で列挙できる。
///
/// `storage.rs::RowStoreTableDef` へのエイリアス（Issue #206。旧 `rows` テーブル
/// （`storage.rs::ROWS_TABLE`）とテーブル名だけが異なる同一契約のため、キー型定義を
/// `storage.rs` 側へ一元化しドリフトを防ぐ）。
pub(crate) type UserRowsTableDef<'a> = crate::storage::RowStoreTableDef<'a>;

/// [`Storage::scan_table_page`] のページングカーソル（行ストアの物理キーと同形の
/// `(tenant_id, id)`。対象ビヘイビア: TABLE-12。`id` 単独では再開位置を表現できない）。
///
/// `storage.rs::RowCursor` の re-export（生成点は `storage.rs` に一元化。Issue #206）。
pub use crate::storage::RowCursor;

/// [`Storage::scan_table_page`] の戻り値（1 ページ分の行と、続きがある場合の
/// [`RowCursor`]）。
pub type RowPage = (Vec<StorageRow>, Option<RowCursor>);

/// 行テーブル定義を組み立てる（[`UserRowsTableDef`] の唯一の生成点）。
/// 呼び出し元（`catalog.rs`・`tenant.rs`・`arena.rs`・`rls.rs`）がキー型を各所で
/// 書き下すとドリフトするため、ここへ集約する。
pub(crate) fn user_rows_table_def(row_table_name: &str) -> UserRowsTableDef<'_> {
    TableDefinition::new(row_table_name)
}

/// 行テーブル `open_table` のエラー写像（[`UserRowsTableDef`] 専用）。
///
/// 旧フォーマット（物理キーが `id` のみ）の DB を開くと `redb` は
/// `TableError::TableTypeMismatch` を返す。これを黙って握りつぶすと旧行を
/// 別テナントの行として扱う fail-open になりうるため、
/// [`CatalogError::IncompatibleRowKeyFormat`] へ明示的に写像して拒否する
/// （マイグレーションは提供しない。TABLE-12 の物理キー変更に伴う恒久契約）。
pub(crate) fn map_row_table_error(e: redb::TableError) -> CatalogError {
    match e {
        redb::TableError::TableTypeMismatch { .. } => CatalogError::IncompatibleRowKeyFormat,
        other => CatalogError::from(other),
    }
}

/// `storage.rs::StorageError` を `CatalogError` へ明示変換する。`CatalogError` は
/// `redb::Error` への blanket `From` 実装を持つため（`storage.rs` の設計メモと同じ
/// coherence 制約）、`redb::Error` そのものではない複合エラー型 `StorageError` からの
/// 変換はここで個別に定義する。
fn convert_storage_error(e: StorageError) -> CatalogError {
    match e {
        StorageError::Backend(err) => CatalogError::Backend(err),
        StorageError::Codec(msg) => CatalogError::Invalid(msg),
        StorageError::NotFound(id) => CatalogError::RowNotFound(id),
        // `StorageError::ScanLimitExceeded` は `Storage::scan`（無制限走査）と
        // `Storage::scan_batch_log`（バッチ台帳）の 2 経路で共有される単一 variant
        // （Issue #131・PR #193 codex レビュー PRRT_kwDOUAKASM6cCITT 対応。バッチ台帳専用の
        // variant を新設する案は公開 enum への破壊的変更にあたるとして差し戻された。詳細は
        // `storage.rs::StorageError::ScanLimitExceeded` のドキュメンテーションコメント参照）。
        // `convert_storage_error` はカタログ層（テーブルスコープの `scan_table_page`）
        // からのみ呼ばれ `Storage::scan_batch_log` を経由しないため、ここでは
        // 「呼び出し元が自分の経路を知っている（= 内部コンテキスト）」という前提の下、
        // テーブルスコープの正確な代替手段 `scan_table_page` を案内してよい。
        // なお `scan_table_page` は `MAX_SCAN_PAGE_LIMIT` で事前にクランプしているため
        // 通常この分岐自体には到達しない（`StorageError` の網羅性のためにここで扱う）。
        StorageError::ScanLimitExceeded => {
            CatalogError::Invalid("scan limit exceeded: use scan_table_page".to_string())
        }
        // `log_batch`（バッチ台帳）専用のエラーだが、カタログ層は行テーブル
        // （`user_rows_table_name`）しか扱わずバッチ台帳を経由しない。到達しない
        // 分岐だが `StorageError` の網羅性のためここでも扱い、Invalid へ一般化する。
        StorageError::DuplicateBatchSeq(seq) => {
            CatalogError::Invalid(format!("duplicate batch seq: seq={seq}"))
        }
        // `WriteTxn` の内部カウンタ（バッチ台帳専用）のエラーで、カタログ層の
        // 行テーブル操作からは到達しない。`StorageError` の網羅性のためここでも扱う。
        StorageError::PendingRowCountOverflow => {
            CatalogError::Invalid("pending row count overflow".to_string())
        }
        // `log_batch`/`commit` の未台帳行チェック・空バッチ拒否も同様にバッチ台帳
        // 専用のエラーで、カタログ層（`db().begin_write()` による生 redb トランザクション
        // 経由）からは到達しない。`StorageError` の網羅性のためここでも扱う。
        StorageError::UnloggedRows(count) => {
            CatalogError::Invalid(format!("unlogged rows before commit: count={count}"))
        }
        StorageError::EmptyBatch => {
            CatalogError::Invalid("empty batch: no rows put since last log_batch".to_string())
        }
        // `bump_generation_and_commit`（TASK-133 P1 対応）はカタログ層の DDL/DML commit
        // からも呼ばれるため到達しうる。u64 の枯渇は現実的に起こらないが網羅性のため扱う。
        StorageError::GenerationCounterOverflow => {
            CatalogError::Invalid("storage generation counter overflow".to_string())
        }
        // カタログ層は `catalog.rs::ROWS_TABLE`（`user_rows/{table}`）を経由し
        // `storage.rs::ROWS_TABLE`（旧 `rows` テーブル）は経由しないため通常到達
        // しないが、`StorageError` の網羅性のためここでも扱う。両テーブルは
        // `CatalogError::IncompatibleRowKeyFormat` と完全に同一の文言を持つため、
        // 単純な写像で挙動が揃う（Issue #206）。
        StorageError::IncompatibleRowKeyFormat => CatalogError::IncompatibleRowKeyFormat,
    }
}

/// write トランザクション内でカタログテーブルから `table_name` のスキーマを取得する
/// （TASK-146）。`insert_row_into_table` / `insert_rows_into_table` の共通前段処理。
/// カタログテーブル自体が未作成の場合・該当エントリが存在しない場合のいずれも
/// `CatalogError::TableNotFound` に一本化する（他テーブルの存在情報を漏らさない
/// fail-closed な扱い。security.md「アクセス制御の不備」）。
///
/// `pub(crate)`: `tenant.rs`（TASK-95・対象ビヘイビア: RECOVER-4）の書き込みガード API が、
/// 同一 write トランザクション内で「スキーマ取得 → 所有権判定 → 書き込み」を行うために
/// ここへ委譲する（クレート外へは公開しない）。
pub(crate) fn require_table_schema_write(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
) -> Result<TableSchema> {
    let catalog_table = match write_txn.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TableNotFound(table_name.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    let guard = catalog_table
        .get(table_name)?
        .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
    decode_schema(table_name, guard.value())
}

/// read トランザクション内でカタログテーブルに `table_name` が定義済みかを確認する
/// （TASK-146）。`get_row_from_table` / `scan_table_page` の共通前段処理。スキーマ本体は
/// 呼び出し元が使わないため取得・デコードしない（[`require_table_schema_write`] と異なり
/// 存在確認のみ）。判定方針は同様に fail-closed。
fn require_table_exists_read(read_txn: &redb::ReadTransaction, table_name: &str) -> Result<()> {
    let catalog_table = match read_txn.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TableNotFound(table_name.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    if catalog_table.get(table_name)?.is_none() {
        return Err(CatalogError::TableNotFound(table_name.to_string()));
    }
    Ok(())
}

/// テーブル単位の世代カウンタ（codex-review P1 指摘対応、PR #266）。キーは論理
/// テーブル名（[`validate_identifier`] 通過済み）、値は当該テーブルへの書き込み
/// commit 回数を表す単調増加カウンタ。`crate::storage::GENERATION_TABLE`
/// （ストレージ全体で任意の write commit ごとに増える世代）とは別テーブルで、
/// こちらは対象テーブル（カタログ定義の DDL・`user_rows/{table_name}` への
/// 行書き込み）に限定して増加する。無関係な他テーブルへの書き込みが本カウンタへ
/// 影響しないことが `USING PLAN` の I/O 前後世代照合（`core.rs`
/// `EngineCore::execute_sql_in_session` の `Statement::Select` アーム参照）の
/// 可用性契約（「テナント境界を跨いだ通常の書き込みトラフィックで USING PLAN が
/// 恒常的に拒否されない」）の土台になる。粒度がテーブル単位・全テナント共通
/// （テナント単位・可視性境界単位への細分化は行わない）であることの設計判断は
/// Issue #285 で現状維持として確定した。根拠・移行トリガーは
/// `docs/design/table-generation-rejection-granularity.md` を参照。
const TABLE_GENERATION_TABLE: TableDefinition<&str, u64> = TableDefinition::new("table_generation");

/// [`TABLE_GENERATION_TABLE`] を 1 つ進める（`write_txn.commit()` 前に呼ぶ）。
///
/// 呼び出し元は、当該 `table_name` の `CATALOG_TABLE` エントリ（DDL）または
/// `user_rows/{table_name}`（DML）のいずれかを同一 `write_txn` 内で変更した
/// すべての箇所（`Storage::create_table`・`drop_table`・`alter_table_add_column`・
/// `insert_row_into_table`・`insert_rows_into_table`・`insert_typed_row`、および
/// `tenant.rs` の `insert_row_unchecked`・`insert_rows_unchecked`・
/// `insert_typed_row_unchecked`・`update_row_unchecked`・`delete_row_unchecked`・
/// `replace_typed_rows_by_text_key`）。新たに対象テーブルの行・スキーマを変更する
/// 書き込み経路を追加する場合は、その commit 前にも本関数を呼ぶこと（呼び忘れは
/// `USING PLAN` の世代照合が対象テーブルの実変更を見逃す fail-open に直結する）。
/// 「変更なしの early return（空バッチ・削除 0 件等）で commit 自体を行わない」
/// 経路は本関数を呼ばない（world 全体の [`crate::storage::bump_generation_and_commit`]
/// と同じく、commit しない = 世代を進めない、が既存契約）。
///
/// `DROP TABLE`→同名再作成の場合もカウンタは単純に増加し続ける（drop 時にリセット
/// しない）。前後比較で「変化したか」だけを見る呼び出し元にとっては、リセットの
/// 有無は無関係（drop→再作成でも必ず値が変わることが重要）。
pub(crate) fn bump_table_generation_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
) -> Result<()> {
    let mut gen_table = write_txn.open_table(TABLE_GENERATION_TABLE)?;
    let current = gen_table.get(table_name)?.map(|v| v.value()).unwrap_or(0);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    gen_table.insert(table_name, next)?;
    Ok(())
}

/// [`bump_table_generation_in_txn`] の読み取り側。テーブルが未作成（1 度も
/// 書き込まれていない）場合は `0` を返す（`crate::storage::current_generation_in_txn`
/// と同じ「未作成 = 世代 0」の方針）。
pub(crate) fn table_generation_in_txn(
    read_txn: &redb::ReadTransaction,
    table_name: &str,
) -> Result<u64> {
    match read_txn.open_table(TABLE_GENERATION_TABLE) {
        Ok(t) => Ok(t.get(table_name)?.map(|v| v.value()).unwrap_or(0)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// カタログ DDL API。`Storage`（`storage.rs`）の拡張として実装し、
/// `Storage::db()` を経由して `ROWS_TABLE` とは別のテーブル（[`CATALOG_TABLE`]）
/// のみを読み書きする。行データへは一切アクセスしない（TABLE-4/TABLE-5）。
impl Storage {
    /// 新規テーブルを定義する（TABLE-4）。同名テーブルが既に存在する場合は
    /// 上書きせず `Err` を返す。カタログテーブルのみを触る単一 write txn で完結する。
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
        bump_table_generation_in_txn(&write_txn, &schema.name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// テーブル定義（`CATALOG_TABLE` エントリ）と、対応する行ストア
    /// （`user_rows/{table_name}`）を同一 write txn で削除する `DROP TABLE` 相当の DDL
    /// （Issue #179。PR #151 レビューで据え置かれた stale `PrefilterIndex` 対策）。
    ///
    /// 失効契約: `bump_generation_and_commit` 経由で commit するため、drop 前に構築された
    /// [`crate::rls::PrefilterSnapshot`]／[`crate::core`] の `PrefilterCache` エントリは
    /// 以後の世代照合でいずれも stale（`RlsError::IndexStale`／キャッシュ破棄）になり、
    /// 削除済み行・旧スキーマの行に基づく結果を返す経路がない。drop 専用の新たな失効機構は
    /// 追加しない（世代カウンタへの一本化）。
    ///
    /// 安全性: `create_table`／`alter_table_add_column` と同じく
    /// [`crate::policy::PolicyContext`] を取らない生の DDL であり、全テナントの行を
    /// 不可逆に削除する。DDL 認可の設計を経ないまま untrusted 経路（SQL 表層・
    /// wire-server）へ配線しない（`DROP TABLE` 文は引き続き許可リスト外で `42601`）。
    ///
    /// 存在しないテーブル名は `Err(CatalogError::TableNotFound)`、識別子として不正な
    /// 名前は `Err(CatalogError::Invalid)`（fail-closed。冪等に `Ok` へ丸めない）。
    /// 行ストア（`user_rows/{table_name}`）は初回挿入まで物理的に未作成のことがあり、
    /// その場合の `delete_table` は「元々存在しなかった」ことを表す `Ok(false)` を返すため
    /// エラーにしない。`ROWS_TABLE`（旧・非テーブルスコープ API）・世代テーブルには
    /// 一切触れない。`operation_id` 台帳（`op_ledger`）は同一トランザクション内で
    /// [`crate::recovery::ledger::delete_table_in_txn`] により当該テーブル名分を
    /// 削除する（Issue #226 レビュー対応: drop 後の同名テーブル再作成で旧台帳
    /// エントリが引き継がれ、正当な書き込みを誤って重複拒否する事故を防ぐ）。
    pub fn drop_table(&self, table_name: &str) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.db().begin_write()?;
        {
            let mut table = match write_txn.open_table(CATALOG_TABLE) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    return Err(CatalogError::TableNotFound(table_name.to_string()));
                }
                Err(e) => return Err(CatalogError::from(e)),
            };
            if table.remove(table_name)?.is_none() {
                return Err(CatalogError::TableNotFound(table_name.to_string()));
            }
        }
        // `CATALOG_TABLE` のハンドルは上のブロックを抜けた時点で解放済み。ここで
        // 行ストアを同一 txn 内で `open_table` すると `TableAlreadyOpen` になるため、
        // `delete_table` は既存ハンドルを介さず直接呼ぶ。
        write_txn.delete_table(user_rows_table_def(&user_rows_table_name(table_name)))?;
        // op_ledger も同一 txn・同一 commit で整合させる（上記ドキュメンテーション
        // コメント参照）。行ストア削除と異なりテーブル自体は残す（他テーブル分の
        // エントリが同居するため）。
        crate::recovery::ledger::delete_table_in_txn(&write_txn, table_name)
            .map_err(convert_storage_error)?;
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// 既存テーブルへ列を末尾追記する（TABLE-5）。追加列は暗黙 nullable として
    /// 保持され、既存行のバイト列には一切触れない（`ROWS_TABLE` 非アクセス）。
    /// `column.nullable == false` は fail-closed に拒否する
    /// （security.md「不安全な設計」）。対象テーブル不存在・列名重複も `Err`。
    pub fn alter_table_add_column(&self, table_name: &str, column: ColumnDef) -> Result<()> {
        validate_identifier(table_name)?;
        validate_column(&column)?;
        if !column.nullable {
            return Err(CatalogError::Invalid(
                "column added via ALTER TABLE ADD COLUMN must be nullable".to_string(),
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
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// テーブル定義を読み出す（スナップショット読み取り）。存在しない場合は
    /// `Err(CatalogError::TableNotFound)`。
    pub fn get_table_schema(&self, table_name: &str) -> Result<TableSchema> {
        let read_txn = self.db().begin_read()?;
        get_table_schema_in_txn(&read_txn, table_name)
    }

    /// 定義済みテーブル名の一覧をスナップショット読み取りで返す。件数上限
    /// （[`MAX_LIST_TABLES`]）を超える場合は `Err`（無制限 `Vec` 確保を防ぐ）。
    pub fn list_tables(&self) -> Result<Vec<String>> {
        let read_txn = self.db().begin_read()?;
        list_tables_in_txn(&read_txn)
    }

    /// テーブルスコープで 1 行挿入する（TASK-146、対象ビヘイビア: EXT-1, EXT-2）。
    ///
    /// カタログからのスキーマ取得・次元検証・行書き込みを単一の write トランザクション内で
    /// 行うことで、並行する DDL（`alter_table_add_column` 等）との整合を確保する。
    /// テーブル不存在・`VECTOR` 列なし・次元不一致はすべて fail-closed に `Err` で拒否する
    /// （security.md「不安全な設計」）。
    ///
    /// `pub(crate)`: 本メソッドはテナント境界チェック（`PolicyContext::is_owner`）を
    /// 一切行わない生の書き込み経路であり、クレート外（wire-server・結合テスト等）へ
    /// 公開するとテナント境界を完全に迂回できてしまう（codex-review P0 指摘・PR #194。
    /// security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。
    /// クレート外・テストからの新規行投入は [`crate::tenant::insert_row`]（テナント境界付き
    /// 書き込みガード。TASK-95・RECOVER-4）を経由すること。
    ///
    /// `#[cfg_attr(not(test), allow(dead_code))]`: 現状の呼び出し元はすべて各モジュールの
    /// `#[cfg(test)]` ユニットテスト（`arena.rs`・`core.rs`・`rls.rs`・本ファイルの
    /// `tenant.rs`）のみのため、`cfg(test)` を含まない通常ビルド（wire-server が依存する
    /// ビルド単位）では本メソッドが到達不能になり `dead_code` lint が発火する。これは
    /// 上記の意図的な `pub(crate)` 制限の帰結であり黙殺してよい。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert_row_into_table(
        &self,
        table_name: &str,
        id: u64,
        row: &RowInput<'_>,
    ) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.db().begin_write()?;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            schema.validate_embedding_dim(row.embedding.len())?;
            let encoded = crate::storage::encode_row(row).map_err(convert_storage_error)?;
            let row_table_name = user_rows_table_name(table_name);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            // 物理キーは `(tenant_id, id)`（TABLE-12）。テナント境界チェックを行わない
            // 生の経路のため、キーの名前空間は入力 `row.tenant_id` に従う
            // （認可済みの名前空間で書くのは `crate::tenant::insert_row` の責務）。
            row_table.insert((row.tenant_id, id), encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// テーブルスコープで複数行を単一トランザクションで挿入する（TASK-146、対象ビヘイビア:
    /// EXT-1, EXT-2）。`insert_row_into_table` のバッチ版。
    /// 1 行でもスキーマ取得・次元検証・エンコードに失敗した場合、write トランザクションは
    /// commit されずに破棄されるため（`redb::WriteTransaction` の drop 契約）、全体が
    /// 未反映のまま拒否される。空スライスの場合も write トランザクション内でカタログ上の
    /// テーブル存在を確認してから成功を返す（レビュー指摘対応: `rows.is_empty()` を
    /// 存在確認より先に判定すると、存在しないテーブルへの空バッチ挿入が `Ok(())` になり
    /// 「テーブル不存在は fail-closed に `Err`」という契約を空バッチで迂回できてしまう）。
    ///
    /// `pub(crate)`（codex-review P0 指摘・PR #194 対応）: 本メソッドはテナント境界
    /// チェックを一切行わない生の書き込み経路で、クレート外へ公開すると任意の
    /// `tenant_id` 名義での書き込み・既存行の上書きが可能になる
    /// （security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。
    /// クレート外・テストからのバッチ投入は [`crate::tenant::insert_rows`]
    /// （`PolicyContext` 必須のガード付きバッチ API）を経由すること。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert_rows_into_table(
        &self,
        table_name: &str,
        rows: &[(u64, RowInput<'_>)],
    ) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.db().begin_write()?;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            if rows.is_empty() {
                // 存在確認以外に何も変更しないため、commit（＝世代を進める）せず
                // write txn を破棄する（`redb::WriteTransaction` は commit/abort の
                // どちらも呼ばずに drop すると自動的に abort される契約。TASK-133 P2
                // 対応: 空バッチだけで既存 `PrefilterIndex` を不要に失効させない）。
                // `storage.rs::Storage::put_batch` と同様、空バッチは行データに
                // 触れず即座に成功として扱う。
                drop(write_txn);
                return Ok(());
            }
            let row_table_name = user_rows_table_name(table_name);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            for (id, row) in rows {
                schema.validate_embedding_dim(row.embedding.len())?;
                let encoded = crate::storage::encode_row(row).map_err(convert_storage_error)?;
                // 物理キーは `(tenant_id, id)`（TABLE-12。`insert_row_into_table` と同じ）。
                row_table.insert((row.tenant_id, *id), encoded.as_slice())?;
            }
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// スキーマ列順の型付き値列（[`row_codec::Value`]）から 1 行挿入する（TASK-75、
    /// 対象ビヘイビア: SQL-1〜4 の実行経路が `INSERT` 文を持たない本タスクで、
    /// 結合テスト（`tests/sql_surface.rs`）が決定的なコーパスを投入するための共通入口）。
    /// `values` は `schema.columns` の列順に対応させる。
    ///
    /// スキーマ取得・`VECTOR` 列の抽出・スカラーペイロード生成
    /// （[`row_codec::encode_scalar_columns`]）・行書き込みを単一の write トランザクション
    /// 内で行う（`insert_row_into_table` と同じ理由で並行 DDL との整合を確保する）。
    /// `VECTOR` 列を持たない・`values` の対応する位置が `Value::Vector` でない場合は
    /// fail-closed に `Err`。
    ///
    /// `pub(crate)`（codex-review P0 指摘・PR #194 対応）: [`Self::insert_rows_into_table`]
    /// と同じ理由でクレート外へは公開しない（`tenant_id` を引数で受け取る生の経路）。
    /// クレート外・テストからの型付き行投入は [`crate::tenant::insert_typed_row`]
    /// （`PolicyContext` から `tenant_id` を導出するガード付き API）を経由すること。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert_typed_row(
        &self,
        table_name: &str,
        id: u64,
        tenant_id: &str,
        visibility: Visibility,
        values: &[RowCodecValue],
    ) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.db().begin_write()?;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            let vector_idx = schema
                .columns
                .iter()
                .position(|c| matches!(c.ty, ColumnType::Vector(_)))
                .ok_or_else(|| CatalogError::Invalid("table has no VECTOR column".to_string()))?;
            let embedding = match values.get(vector_idx) {
                Some(RowCodecValue::Vector(v)) => v.clone(),
                _ => {
                    return Err(CatalogError::Invalid(
                        "VECTOR column value missing or not a Vector".to_string(),
                    ))
                }
            };
            schema.validate_embedding_dim(embedding.len())?;
            let metadata = row_codec::encode_scalar_columns(&schema, values)
                .map_err(|e| CatalogError::Invalid(e.to_string()))?;
            let row_input = RowInput {
                tenant_id,
                visibility,
                embedding: &embedding,
                metadata: &metadata,
            };
            let encoded = crate::storage::encode_row(&row_input).map_err(convert_storage_error)?;
            let row_table_name = user_rows_table_name(table_name);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            // 物理キーは `(tenant_id, id)`（TABLE-12）。`tenant_id` は本 API の引数
            // （呼び出し元がテナントを明示する契約）。
            row_table.insert((tenant_id, id), encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// テーブルスコープで、指定テナントの名前空間から 1 行取得する（スナップショット
    /// 読み取り。TASK-146、対象ビヘイビア: EXT-1, EXT-2。物理キーは TABLE-12 の
    /// `(tenant_id, id)`）。他テーブル・他テナントの同一 `id` は見えない。
    ///
    /// `tenant_id` を引数で必須化しているのは TABLE-12 で行 `id` の一意性スコープが
    /// テナント内に閉じたためで、`id` 単独ではもはや行を一意に指せない。本メソッド自体は
    /// 認可を行わない生の取得経路であり（`tenant_id` は「どの名前空間を引くか」の指定に
    /// すぎない）、可視性判定は呼び出し元（`core.rs::EngineCore::get_row` →
    /// `PolicyContext::is_visible`）が行う。呼び出し元は不存在と不可視を区別せず
    /// `NotFound` に統一する契約のため、本 API 経由で他テナント行の存在を観測できる
    /// 公開経路は生まれない（fail-closed。RLS-9）。
    /// テーブル不存在・行不存在はいずれも fail-closed に `Err` を返す
    /// （エラー内容に他テーブル・他テナントの存在情報を含めない）。
    pub fn get_row_from_table(
        &self,
        table_name: &str,
        tenant_id: &str,
        id: u64,
    ) -> Result<StorageRow> {
        validate_identifier(table_name)?;
        let read_txn = self.db().begin_read()?;
        require_table_exists_read(&read_txn, table_name)?;
        let row_table_name = user_rows_table_name(table_name);
        let row_table = match read_txn.open_table(user_rows_table_def(&row_table_name)) {
            Ok(t) => t,
            // テーブルは定義済みだが 1 行も挿入していない（行テーブル自体が未作成）。
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(CatalogError::RowNotFound(id))
            }
            Err(e) => return Err(map_row_table_error(e)),
        };
        let guard = row_table
            .get(&(tenant_id, id))?
            .ok_or(CatalogError::RowNotFound(id))?;
        crate::storage::decode_row(id, guard.value()).map_err(convert_storage_error)
    }

    /// テーブルスコープで物理キー昇順（`(tenant_id, id)`。TABLE-12）に最大 `limit` 件を
    /// 走査する上限付きページング API（TASK-146、対象ビヘイビア: EXT-1, EXT-2）。
    /// `storage.rs::Storage::scan_page` と同じ行数上限（`MAX_SCAN_PAGE_LIMIT`）・
    /// バイト量上限（`MAX_SCAN_PAGE_BYTES`）を適用し、自テーブルの行のみを返す
    /// （他テーブルとの混線なし）。
    ///
    /// **走査範囲はテーブル全行**（テナントで絞らない）: 可視性判定は呼び出し元
    /// （`crate::tenant::visible_rows` → `PolicyContext::is_visible`）の単一照合パスに
    /// 委譲する契約を維持するため、本 API はテナント境界を判断しない。他テナントの
    /// `Public` 行も列挙対象に含まれる（`(tenant_id, id)` 順では連続範囲にならないため、
    /// テナント絞り込みでは可視集合を構成できない）。
    ///
    /// カーソルは物理キーと同じ `(tenant_id, id)` 形（`id` 単独では再開位置を表現できず、
    /// 行の取りこぼしになる）。打ち切り契約は `scan_page` と同一。
    pub fn scan_table_page(
        &self,
        table_name: &str,
        after: Option<(&str, u64)>,
        limit: u32,
    ) -> Result<RowPage> {
        validate_identifier(table_name)?;
        let limit = limit.min(crate::storage::MAX_SCAN_PAGE_LIMIT) as usize;

        let read_txn = self.db().begin_read()?;
        require_table_exists_read(&read_txn, table_name)?;
        // `limit == 0` の早期 return は存在確認より後に置く（レビュー指摘対応: 先に
        // 判定すると、存在しないテーブルへの limit=0 走査が空ページで成功してしまい、
        // 「テーブル不存在は fail-closed に `Err`」という契約を迂回できてしまう）。
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        let row_table_name = user_rows_table_name(table_name);
        let row_table = match read_txn.open_table(user_rows_table_def(&row_table_name)) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), None)),
            Err(e) => return Err(map_row_table_error(e)),
        };

        // カーソルの直後から走査を開始する（`Bound::Excluded`）。複合キーでは
        // 「次のキー」を算術で導出できないため、除外境界付き range で表現する。
        let range_start = match after {
            Some(cursor) => std::ops::Bound::Excluded(cursor),
            None => std::ops::Bound::Unbounded,
        };

        let mut out = Vec::new();
        let mut bytes_used: usize = 0;
        let mut capped = false;
        let mut last_key: Option<(String, u64)> = None;
        for entry in row_table.range::<(&str, u64)>((range_start, std::ops::Bound::Unbounded))? {
            if out.len() == limit {
                capped = true;
                break;
            }
            let (k, v) = entry?;
            let (tenant_id, id) = k.value();
            let raw = v.value();
            if !out.is_empty()
                && bytes_used.saturating_add(raw.len()) > crate::storage::MAX_SCAN_PAGE_BYTES
            {
                capped = true;
                break;
            }
            out.push(crate::storage::decode_row(id, raw).map_err(convert_storage_error)?);
            last_key = Some((tenant_id.to_string(), id));
            bytes_used = bytes_used.saturating_add(raw.len());
        }

        let cursor_for_next = if capped { last_key } else { None };
        Ok((out, cursor_for_next))
    }
}

/// `sql::allowlist::validate_statement`（TASK-74・SQL-8 参照）から FROM テーブルの
/// カタログ存在確認に使われる橋渡し実装。`get_table_schema` が返す `CatalogError`
/// を SQL 表層のエラー契約へ分類し直し、識別子形式不正のみ拒否側（構文エラー）へ
/// 倒す。それ以外（カタログ照会自体の失敗を含む）は受理側へ倒さず fail-closed に
/// エラー伝播する（`.claude/rules/security.md`「不安全な設計」対応）。格納済み
/// スキーマのデコード失敗は識別子形式不正と区別し、内部データ断片がエラー
/// メッセージ経由で漏れないよう汎用メッセージへ丸める（security.md「情報漏えい」対応）。
impl TableLookup for Storage {
    fn table_exists(&self, name: &str) -> std::result::Result<bool, SqlSurfaceError> {
        match self.get_table_schema(name) {
            Ok(_) => Ok(true),
            Err(CatalogError::TableNotFound(_)) => Ok(false),
            Err(CatalogError::Invalid(detail)) => Err(SqlSurfaceError::unsupported(format!(
                "malformed table reference: {detail}"
            ))),
            Err(
                CatalogError::Backend(_)
                | CatalogError::CorruptSchema(_)
                | CatalogError::TableAlreadyExists(_)
                | CatalogError::ColumnAlreadyExists(_)
                | CatalogError::RowNotFound(_)
                | CatalogError::IncompatibleRowKeyFormat
                | CatalogError::TableGenerationCounterOverflow,
            ) => Err(SqlSurfaceError::Internal {
                detail: "catalog lookup failed".to_string(),
            }),
        }
    }
}

/// [`Storage::get_table_schema`]・[`crate::arena::VectorArena::build`]（TASK-87、
/// 対象ビヘイビア: TABLE-8）が共有するトランザクションスコープの実装本体。
/// `pub(crate)` で公開し、`arena.rs` が単一の `read_txn` 上でスキーマ取得と
/// テーブルスコープ行テーブル（[`user_rows_table_name`]）のオープンを同一
/// スナップショットで行えるようにする（TOCTOU 対策）。
pub(crate) fn get_table_schema_in_txn(
    read_txn: &redb::ReadTransaction,
    table_name: &str,
) -> Result<TableSchema> {
    // `alter_table_add_column` と同様、redb キーとして引く前に識別子を検証する。
    // 不正形式の名前は `TableNotFound`（存在しない）ではなく `Invalid`（形式不正）で
    // 拒否し、両 API 間でエラーバリアントを揃える。
    validate_identifier(table_name)?;
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

/// [`Storage::list_tables`] が共有するトランザクションスコープの実装本体。
fn list_tables_in_txn(read_txn: &redb::ReadTransaction) -> Result<Vec<String>> {
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
        // `get_table_schema_in_txn` と同じ検証をここでも通す。通常経路で書かれるキーは
        // すべて `create_table` の `validate_schema` を経ているため常に合法だが、
        // 手書きの不正データが直接 redb へ書き込まれていた場合に、そのまま
        // 一覧へ紛れ込ませない（`decode_schema` と同じ fail-closed 方針）。
        validate_identifier(name)?;
        names.push(name.to_string());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

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
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_column_lines() {
        let bytes = b"v1\ncols:2\nfoo:text:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_unknown_type() {
        let bytes = b"v1\ncols:1\nfoo:blob:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_bad_dimension() {
        let bytes = b"v1\ncols:1\nfoo:vector:0:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
        let bytes = b"v1\ncols:1\nfoo:vector:not-a-number:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let bytes = vec![0xff, 0xfe, 0xfd];
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
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

    // --- Storage::drop_table -----------------------------------------------
    // `drop_table` の内部不変条件（世代 +1・`ROWS_TABLE` 非接触）をこの unit test
    // モジュールに置き、公開契約（未存在・識別子不正・再作成別次元）の固定は
    // `tests/catalog.rs`（クレート外の統合テスト）側に委譲する（Issue #179）。

    #[test]
    fn drop_table_removes_catalog_entry() {
        let path = unique_db_path("drop-table-removes-entry");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        storage.drop_table("docs").expect("drop table");

        assert!(matches!(
            storage.get_table_schema("docs"),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(!storage
            .list_tables()
            .expect("list tables")
            .iter()
            .any(|t| t == "docs"));
    }

    // codex-review P1 再指摘（PR #266）「新設する場合は書き込み経路での更新漏れが
    // ないことをテストで担保」対応: `bump_table_generation_in_txn` を呼ぶすべての
    // カタログ層 API（`create_table`・`alter_table_add_column`・
    // `insert_row_into_table`・`insert_rows_into_table`・`Storage::insert_typed_row`・
    // `drop_table`）が対象テーブル（`docs`）の世代を実際に進めること、かつ無関係な
    // 別テーブル（`sibling`）の世代には一切影響しないことを固定する。空バッチ
    // （`insert_rows_into_table` の `rows.is_empty()` 早期 return）は commit 自体を
    // 行わない既存契約のとおり世代を進めないことも合わせて固定する
    // （`tenant.rs` 側の書き込み API は
    // `write_apis_bump_only_the_written_tables_generation` で別途カバーする）。
    #[test]
    fn catalog_write_apis_bump_only_the_written_tables_generation() {
        let path = unique_db_path("table-generation-bump-coverage-catalog");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let read_gen = |name: &str| -> u64 {
            let read_txn = storage.db().begin_read().expect("begin read");
            table_generation_in_txn(&read_txn, name).expect("read table generation")
        };

        // 無関係な「sibling」テーブルを先に作る。以降の全操作を通じて
        // `sibling` の世代が一切変化しないことを都度確認する。
        storage
            .create_table(&TableSchema::new(
                "sibling",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create sibling");
        let sibling_gen = read_gen("sibling");
        assert_eq!(
            sibling_gen, 1,
            "create_table must bump its own table's generation"
        );

        assert_eq!(read_gen("docs"), 0, "未作成テーブルの世代は 0");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create docs");
        let mut prev = read_gen("docs");
        assert!(prev > 0, "create_table must bump docs' generation");
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .alter_table_add_column("docs", ColumnDef::new("path", ColumnType::Text, true))
            .expect("alter_table_add_column");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "alter_table_add_column must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.1, 0.2],
                    metadata: &[],
                },
            )
            .expect("insert_row_into_table");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "insert_row_into_table must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .insert_rows_into_table(
                "docs",
                &[(
                    2,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.3, 0.4],
                        metadata: &[],
                    },
                )],
            )
            .expect("insert_rows_into_table (non-empty)");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "insert_rows_into_table (non-empty) must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        // 空バッチは commit 自体を行わない既存契約（`insert_rows_into_table` の
        // ドキュメントコメント参照）のとおり、世代を進めない。
        storage
            .insert_rows_into_table("docs", &[])
            .expect("insert_rows_into_table (empty)");
        assert_eq!(
            read_gen("docs"),
            prev,
            "insert_rows_into_table with an empty batch must not bump the generation"
        );
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .insert_typed_row(
                "docs",
                3,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![0.5, 0.6]),
                    RowCodecValue::Text("typed-path".to_string()),
                ],
            )
            .expect("insert_typed_row");
        let next = read_gen("docs");
        assert!(next > prev, "insert_typed_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage.drop_table("docs").expect("drop_table");
        let next = read_gen("docs");
        assert!(next > prev, "drop_table must bump docs' generation");
        assert_eq!(read_gen("sibling"), sibling_gen);
    }

    #[test]
    fn drop_table_rejects_missing_table_and_invalid_identifier() {
        let path = unique_db_path("drop-table-rejects-missing");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // カタログテーブル自体が未作成（1 テーブルも定義していない DB）でも
        // `TableNotFound` に丸め込む（`Backend` 等の内部エラー種別を露出しない）。
        assert!(matches!(
            storage.drop_table("docs"),
            Err(CatalogError::TableNotFound(_))
        ));

        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        assert!(matches!(
            storage.drop_table("missing"),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(matches!(
            storage.drop_table("bad/name"),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn drop_table_then_recreate_with_different_dim_starts_empty() {
        let path = unique_db_path("drop-table-recreate-dim");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        storage.drop_table("docs").expect("drop table");

        // drop 直後（再作成前）はカタログ・行の双方が不存在扱いになる。
        assert!(matches!(
            storage.get_row_from_table("docs", "tenant-a", 1),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(matches!(
            storage.scan_table_page("docs", None, 10),
            Err(CatalogError::TableNotFound(_))
        ));

        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
            ))
            .expect("recreate table with different dim");
        storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0, 0.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row with new dim");

        let (rows, _cursor) = storage
            .scan_table_page("docs", None, 10)
            .expect("scan after recreate");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 2);
    }

    #[test]
    fn drop_table_bumps_generation_exactly_once() {
        let path = unique_db_path("drop-table-generation");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        let before = storage.current_generation().expect("read generation");
        storage.drop_table("docs").expect("drop table");
        let after = storage.current_generation().expect("read generation");

        assert_eq!(after, before + 1);
    }

    // drop 後の同名テーブル再作成で旧 `op_ledger` エントリが引き継がれ、正当な
    // 書き込みを誤って重複拒否させない（Issue #226 レビュー対応: TASK-93/RECOVER-2）。
    #[test]
    fn drop_table_removes_op_ledger_entries_for_that_table() {
        use crate::recovery::content_hash::ContentHash;
        use crate::recovery::ledger::{contains_in_read_txn, record_in_txn, LedgerWrite};
        use crate::recovery::required_op_id::OperationId;

        let path = unique_db_path("drop-table-op-ledger");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        let op_id = OperationId::parse("op-drop-1").expect("valid operation_id");
        let write_txn = storage.db().begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "docs",
            LedgerWrite::Record(&op_id),
            &ContentHash::for_test(b"content"),
        )
        .expect("record op ledger entry");
        write_txn.commit().expect("commit ledger record");

        storage.drop_table("docs").expect("drop table");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("recreate table with same name");

        let read_txn = storage.db().begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "docs", &op_id)
            .expect("contains after recreate");
        assert!(
            !found,
            "op_ledger entry from the dropped table must not survive into the recreated table"
        );
    }

    #[test]
    fn drop_table_does_not_touch_legacy_rows_table() {
        let path = unique_db_path("drop-table-legacy-rows");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("put legacy row");

        storage.drop_table("docs").expect("drop table");

        let row = storage
            .get("tenant-a", 1)
            .expect("legacy row still readable");
        assert_eq!(row.embedding, vec![1.0, 0.0]);
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
        // 単一の write txn へ直接まとめて挿入する（create_table 自体の性能特性検証は
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

    // --- Storage::insert_rows_into_table（空バッチ） -------------------------
    // TASK-133 P2 対応: 空バッチは既存行・スキーマを一切変更しないため、世代カウンタ
    // （`crate::storage::bump_generation_and_commit` が管理）を進めてはならない
    // （進めると空バッチだけで既存 `PrefilterIndex` を不要に失効させてしまう）。

    #[test]
    fn insert_rows_into_table_with_empty_batch_does_not_bump_generation() {
        let path = unique_db_path("insert-rows-empty-batch-generation");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        // `create_table` 自体の世代（DDL も commit のたびに世代を進める）を基準にする。
        let generation_before = storage.current_generation().expect("generation before");

        storage
            .insert_rows_into_table("docs", &[])
            .expect("empty batch insert must succeed as a no-op");

        assert_eq!(
            storage.current_generation().expect("generation after"),
            generation_before,
            "empty batch insert must not bump the storage generation counter"
        );

        // 対称性の確認: 実際に行を書き込むバッチは引き続き世代を進める。
        storage
            .insert_rows_into_table(
                "docs",
                &[(
                    1,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: crate::storage::Visibility::Public,
                        embedding: &[1.0, 0.0],
                        metadata: &[],
                    },
                )],
            )
            .expect("non-empty batch insert");
        assert_eq!(
            storage
                .current_generation()
                .expect("generation after non-empty batch"),
            generation_before + 1,
            "a non-empty batch insert must still bump the storage generation counter"
        );
    }

    // --- insert_typed_row（TASK-75、SQL-1〜4 の結合テスト共通入口） -----------------

    #[test]
    fn insert_typed_row_round_trips_embedding_and_scalar_columns() {
        let path = unique_db_path("insert-typed-row-roundtrip");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(3), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                    ColumnDef::new("lang", ColumnType::Text, true),
                ],
            ))
            .expect("create table");

        storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![1.0, 2.0, 3.0]),
                    RowCodecValue::Text("hello".to_string()),
                    RowCodecValue::Text("ja".to_string()),
                ],
            )
            .expect("insert typed row");

        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row");
        assert_eq!(row.embedding, vec![1.0, 2.0, 3.0]);
        let schema = storage.get_table_schema("docs").expect("get schema");
        let decoded =
            row_codec::decode_scalar_columns(&schema, &row.metadata).expect("decode scalar");
        assert_eq!(decoded[1], RowCodecValue::Text("hello".to_string()));
        assert_eq!(decoded[2], RowCodecValue::Text("ja".to_string()));
    }

    #[test]
    fn insert_typed_row_rejects_embedding_dim_mismatch() {
        let path = unique_db_path("insert-typed-row-dim-mismatch");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
            ))
            .expect("create table");

        let result = storage.insert_typed_row(
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[RowCodecValue::Vector(vec![1.0, 2.0])],
        );
        assert!(matches!(result, Err(CatalogError::Invalid(_))));
    }

    #[test]
    fn insert_typed_row_rejects_missing_vector_column_value() {
        let path = unique_db_path("insert-typed-row-missing-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
            ))
            .expect("create table");

        let result = storage.insert_typed_row(
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[RowCodecValue::Null],
        );
        assert!(matches!(result, Err(CatalogError::Invalid(_))));
    }

    // Issue #131: convert_storage_error はカタログ層（テーブルスコープ）専用の呼び出し元
    // であるという内部コンテキストを前提に、`scan_table_page` への正確な代替手段案内を
    // 生成することを固定する（`Storage::scan_batch_log` はこの経路を通らない）。

    #[test]
    fn convert_storage_error_maps_scan_limit_to_table_page_guidance() {
        let err = convert_storage_error(crate::storage::StorageError::ScanLimitExceeded);
        match err {
            CatalogError::Invalid(msg) => assert!(msg.contains("scan_table_page"), "{msg}"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
