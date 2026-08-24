//! スキーマカタログ層（TASK-85、対象ビヘイビア: TABLE-1, TABLE-4, TABLE-5, TABLE-6。
//! ポインタ: `docs/spec/05-tasks.md` TASK-85・`docs/spec/04-behavior/data-model.md`）。
//!
//! 責務境界: `VECTOR(N)` 列型を含むテーブル定義（[`TableSchema`]）の DDL
//! （`CREATE TABLE`・`ALTER TABLE ADD COLUMN`）と、その永続化（`storage.rs` の
//! `redb::Database` を共有する専用テーブル）を担う。行データそのもの
//! （`ROWS_TABLE`）には一切アクセスしない設計上の境界とする（TABLE-4/TABLE-5）。
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

use std::fmt;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

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
    /// 識別子・型・次元数・カタログ値のフォーマットが不正（TABLE-6）。
    /// 欠落フィールド・未知の型・不正次元・区切り文字混入等をすべてここに集約する。
    Invalid(String),
    /// 指定したテーブルがカタログに存在しない。
    TableNotFound(String),
    /// `CREATE TABLE` で同名テーブルが既に存在する（上書きしない。TABLE-4 前提）。
    TableAlreadyExists(String),
    /// `ALTER TABLE ADD COLUMN` で追加しようとした列名が既存列と重複する。
    ColumnAlreadyExists(String),
    /// テーブルスコープ行 API（TASK-146）で、指定した行 ID がそのテーブル内に
    /// 存在しない。他テーブルの同一 ID は無関係（テーブル帰属した独立ストア）。
    RowNotFound(u64),
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
            CatalogError::RowNotFound(id) => write!(f, "row not found: id={id}"),
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
            | CatalogError::ColumnAlreadyExists(_)
            | CatalogError::RowNotFound(_) => None,
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

/// ユーザーテーブル `table_name` に対応する行ストア用の動的 redb テーブル名を組み立てる
/// （TASK-146・EXT-2）。`validate_identifier` が `/` を含む文字列を許容しないため、
/// 固定テーブル（`rows`／`catalog`）ともユーザーテーブル同士とも名前衝突しない。
/// 呼び出し元は必ず先に `validate_identifier(table_name)` を通してから呼ぶこと
/// （本関数自身は検証を行わない）。
///
/// 将来 `drop_table` 相当の API を追加する実装者向けの申し送り: この動的テーブルは
/// [`CATALOG_TABLE`] のエントリとは別ライフサイクルで管理されている（`create_table` は
/// `CATALOG_TABLE` のみ書き込み、本関数が指す行テーブルは初回挿入まで未作成のまま）。
/// `drop_table` を実装する際は `CATALOG_TABLE` のエントリ削除と同一 write トランザクション内で
/// 本関数が返す行テーブルも削除しないと、テーブル再作成時に旧次元の行データが残留し
/// EXT-2 の次元固定の不変条件を静かに破る恐れがある。
///
/// `pub(crate)` で公開する: `arena.rs`（TASK-87、対象ビヘイビア: TABLE-8）が
/// コールドスタート・アリーナ構築時に、対象テーブルの行テーブルだけを単一の
/// `read_txn` 上で直接開くために必要（クレート外へは公開しない）。
pub(crate) fn user_rows_table_name(table_name: &str) -> String {
    format!("user_rows/{table_name}")
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
        // `scan_table_page` は `MAX_SCAN_PAGE_LIMIT` で事前にクランプしているため
        // 通常この分岐には到達しない（`Storage::scan`（無制限走査）側でのみ発生しうる
        // エラーの網羅性のためにここで扱う）。到達時の文言は「scan_table_page を使え」と
        // 自己言及的にならないよう、呼び出し元 API 名を挙げずに一般化して書く。
        StorageError::ScanLimitExceeded => {
            CatalogError::Invalid("scan limit exceeded: use a bounded page scan".to_string())
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
    }
}

/// write トランザクション内でカタログテーブルから `table_name` のスキーマを取得する
/// （TASK-146）。`insert_row_into_table` / `insert_rows_into_table` の共通前段処理。
/// カタログテーブル自体が未作成の場合・該当エントリが存在しない場合のいずれも
/// `CatalogError::TableNotFound` に一本化する（他テーブルの存在情報を漏らさない
/// fail-closed な扱い。security.md「アクセス制御の不備」）。
fn require_table_schema_write(
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
        write_txn.commit()?;
        Ok(())
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
        write_txn.commit()?;
        Ok(())
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
    pub fn insert_row_into_table(
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
            let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
            let mut row_table = write_txn.open_table(row_table_def)?;
            row_table.insert(id, encoded.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// テーブルスコープで複数行を単一トランザクションで挿入する（TASK-146、対象ビヘイビア:
    /// EXT-1, EXT-2）。[`insert_row_into_table`](Self::insert_row_into_table) のバッチ版。
    /// 1 行でもスキーマ取得・次元検証・エンコードに失敗した場合、write トランザクションは
    /// commit されずに破棄されるため（`redb::WriteTransaction` の drop 契約）、全体が
    /// 未反映のまま拒否される。空スライスの場合も write トランザクション内でカタログ上の
    /// テーブル存在を確認してから成功を返す（レビュー指摘対応: `rows.is_empty()` を
    /// 存在確認より先に判定すると、存在しないテーブルへの空バッチ挿入が `Ok(())` になり
    /// 「テーブル不存在は fail-closed に `Err`」という契約を空バッチで迂回できてしまう）。
    pub fn insert_rows_into_table(
        &self,
        table_name: &str,
        rows: &[(u64, RowInput<'_>)],
    ) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.db().begin_write()?;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            if rows.is_empty() {
                // 行テーブルを開く必要はないが、上記の存在確認は既に済ませたうえで
                // write txn を commit する（`storage.rs::Storage::put_batch` と同様、
                // 空バッチは行データに触れず即座に成功として扱う）。
                write_txn.commit()?;
                return Ok(());
            }
            let row_table_name = user_rows_table_name(table_name);
            let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
            let mut row_table = write_txn.open_table(row_table_def)?;
            for (id, row) in rows {
                schema.validate_embedding_dim(row.embedding.len())?;
                let encoded = crate::storage::encode_row(row).map_err(convert_storage_error)?;
                row_table.insert(*id, encoded.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// テーブルスコープで 1 行取得する（スナップショット読み取り。TASK-146、対象ビヘイビア:
    /// EXT-1, EXT-2）。他テーブルの同一 ID は見えない（テーブル帰属した独立ストア）。
    /// テーブル不存在・行不存在はいずれも fail-closed に `Err` を返す
    /// （エラー内容に他テーブルの存在情報を含めない）。
    pub fn get_row_from_table(&self, table_name: &str, id: u64) -> Result<StorageRow> {
        validate_identifier(table_name)?;
        let read_txn = self.db().begin_read()?;
        require_table_exists_read(&read_txn, table_name)?;
        let row_table_name = user_rows_table_name(table_name);
        let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
        let row_table = match read_txn.open_table(row_table_def) {
            Ok(t) => t,
            // テーブルは定義済みだが 1 行も挿入していない（行テーブル自体が未作成）。
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(CatalogError::RowNotFound(id))
            }
            Err(e) => return Err(e.into()),
        };
        let guard = row_table.get(id)?.ok_or(CatalogError::RowNotFound(id))?;
        crate::storage::decode_row(id, guard.value()).map_err(convert_storage_error)
    }

    /// テーブルスコープで複数行の `tenant_id`・`visibility`（ヘッダのみ）を一括取得する
    /// （TASK-133、対象ビヘイビア: RLS-1〜4。`rls.rs::PrefilterIndex::search` が provider を
    /// 呼ぶ**前**にアリーナの全 id について検索時点の現在の行状態を再検証し、インデックス
    /// 構築後の update/delete による失効行のベクトルが provider へ渡ることを防ぐために呼ぶ
    /// （codex-review P0 指摘・PR #151 対応）。embedding・metadata はデコードせず
    /// `decode_row_tenant_and_visibility` のみを使う（[`Self::get_row_from_table`] と異なり
    /// 埋め込み全体を読まないため、アリーナの全行数分呼んでも DoS 耐性を保つ）。
    ///
    /// `ids` に対応する 1 回の `read_txn` だけを張る（id ごとに個別トランザクションを
    /// 張らない）。これにより、1 回の呼び出しで検証する id 集合は単一のストレージ
    /// スナップショットに対して一貫する（`rls.rs::PrefilterIndex::search` の「一貫性契約」
    /// ドキュメント参照）。戻り値は `ids` と同じ順序・同じ長さの `Vec` で、該当行が存在
    /// しない（削除済み、または行テーブル自体が未作成）場合はその位置に `None` を入れる
    /// （`CatalogError::RowNotFound` へ丸め込まない。呼び出し元が「削除済み行は不可視扱いに
    /// する」という fail-closed 判断を行えるようにするため。テーブル自体が不存在の場合のみ
    /// 通常どおり `CatalogError::TableNotFound` を返す）。
    ///
    /// `ids.len()` に上限は課さない（無制限確保を避けるための呼び出し元側の責務。現在の
    /// 唯一の呼び出し元 `rls.rs::PrefilterIndex::search` は、`PrefilterIndex::build`
    /// （`VectorArena::build_filtered` 経由の構築）時点で `arena.rs::MAX_ARENA_ROWS`
    /// （1,000,000 行）により上限が課されたアリーナの全 id（`self.arena.ids()`。呼び出し元・
    /// provider の入力に依存しない値）を渡すため無制限にならない。将来別の呼び出し元を
    /// 追加する場合は同様の上限を先に満たすこと）。
    pub(crate) fn get_row_headers_from_table(
        &self,
        table_name: &str,
        ids: &[u64],
    ) -> Result<Vec<Option<(String, Visibility)>>> {
        validate_identifier(table_name)?;
        let read_txn = self.db().begin_read()?;
        require_table_exists_read(&read_txn, table_name)?;
        let row_table_name = user_rows_table_name(table_name);
        let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
        let row_table = match read_txn.open_table(row_table_def) {
            Ok(t) => t,
            // 行テーブル自体が未作成（1 行も挿入していない）= 全 id が不存在。
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![None; ids.len()]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            match row_table.get(id)? {
                None => out.push(None),
                Some(guard) => {
                    let header = crate::storage::decode_row_tenant_and_visibility(guard.value())
                        .map_err(convert_storage_error)?;
                    out.push(Some(header));
                }
            }
        }
        Ok(out)
    }

    /// テーブルスコープで行 ID 昇順に最大 `limit` 件を走査する上限付きページング API
    /// （TASK-146、対象ビヘイビア: EXT-1, EXT-2）。`storage.rs::Storage::scan_page` と同じ
    /// 行数上限（`MAX_SCAN_PAGE_LIMIT`）・バイト量上限（`MAX_SCAN_PAGE_BYTES`）を適用し、
    /// 自テーブルの行のみを返す（他テーブルとの混線なし）。カーソル・打ち切り契約は
    /// `scan_page` と同一。
    pub fn scan_table_page(
        &self,
        table_name: &str,
        after: Option<u64>,
        limit: u32,
    ) -> Result<(Vec<StorageRow>, Option<u64>)> {
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
        let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
        let row_table = match read_txn.open_table(row_table_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), None)),
            Err(e) => return Err(e.into()),
        };

        // カーソルの直後（`after + 1`）から走査を開始する。`storage.rs::scan_page` と同じ
        // 契約: `after` が `u64::MAX` なら「これ以上続きがない」ため空ページを返す。
        let start = match after {
            Some(cursor) => match cursor.checked_add(1) {
                Some(next) => next,
                None => return Ok((Vec::new(), None)),
            },
            None => 0,
        };

        let mut out = Vec::new();
        let mut bytes_used: usize = 0;
        let mut capped = false;
        for entry in row_table.range(start..)? {
            if out.len() == limit {
                capped = true;
                break;
            }
            let (k, v) = entry?;
            let id = k.value();
            let raw = v.value();
            if !out.is_empty()
                && bytes_used.saturating_add(raw.len()) > crate::storage::MAX_SCAN_PAGE_BYTES
            {
                capped = true;
                break;
            }
            out.push(crate::storage::decode_row(id, raw).map_err(convert_storage_error)?);
            bytes_used = bytes_used.saturating_add(raw.len());
        }

        let cursor_for_next = if capped {
            out.last().map(|r| r.id)
        } else {
            None
        };
        Ok((out, cursor_for_next))
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

    // --- Storage::get_row_headers_from_table -------------------------------
    // `rls.rs::PrefilterIndex::search` が失効行の再検証に使うヘルパー（TASK-133、
    // 対象ビヘイビア: RLS-1〜4。codex-review P0 指摘・PR #151 対応）の `None` 分岐
    // （該当 id の行が存在しない）を直接カバーする。本クレートには行削除 API がまだ
    // 存在しないため、削除された行ではなく最初から未挿入の id で再現する
    // （`rls.rs` 側の単体テストがこの分岐を担っていたが、`PrefilterIndex::search` から
    // `storage` 引数を除去した設計変更により、そちらでは再現不能になったため本テストへ
    // 移設した）。

    #[test]
    fn get_row_headers_from_table_returns_none_for_a_missing_id() {
        let path = unique_db_path("row-headers-missing-id");
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

        // id=1 は存在するが、id=999 は未挿入（削除相当）。行テーブル自体は存在するため
        // `TableNotFound` ではなく `None` 分岐（`row_table.get(id)? == None`）を通る。
        let headers = storage
            .get_row_headers_from_table("docs", &[1, 999])
            .expect("get_row_headers_from_table ok");
        assert_eq!(headers.len(), 2);
        assert!(headers[0].is_some());
        assert!(headers[1].is_none());
    }
}
