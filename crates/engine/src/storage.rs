//! `redb` ベースの永続化層（TASK-140、対象ビヘイビア: PERSIST-1, PERSIST-2, PERSIST-4。
//! ポインタ: `docs/spec/05-tasks.md` TASK-140・`docs/spec/04-behavior/persistence.md`）。
//!
//! 責務境界: ベクトル行（id・埋め込み・メタデータ）の永続化 API を提供する。
//! 後続の検索カーネル・カタログ層（TASK-124〜、TASK-85〜）から呼び出される想定で、
//! 本モジュールは行の意味（テナント境界・可視性ラベル等の RLS フィールド、TASK-141）
//! には関与しない。呼び出し元がメタデータ列にどのようなバイト列を格納するかを決める。
//!
//! 分離レベル（PERSIST-4）: `redb` の契約をそのまま宣言する。書き込みトランザクションは
//! `redb::Database::begin_write` が排他ロックを取ることで直列化され、読み取りトランザクション
//! （`begin_read`）は開始時点のスナップショットを見る（進行中の未コミット書き込みは見えない）。
//! 本モジュールは独自のロック層を追加しない。

use std::fmt;
use std::path::Path;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

/// 行データを格納するテーブル。キーは行 ID（`u64`）、値は [`encode_row`] でエンコードした
/// バイト列。テーブル名は `docs/spec` 側の成果物指定に依存しないローカルな識別子。
const ROWS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("rows");

/// 行エンコーディングの先頭バイト。将来のスキーマ拡張（TASK-141 の RLS フィールド同居等）
/// に備え、デコード側は未知バージョンを fail-closed に拒否する。
const ROW_FORMAT_VERSION: u8 = 1;

/// 埋め込み次元数の上限。デコード時にこの値を超える `dim` を確認した場合、
/// `Vec::with_capacity` へ渡す前に拒否する（未検証の長さフィールドを無制限アロケーションに
/// 使わない。.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
const MAX_EMBEDDING_DIM: u32 = 65_536;

/// メタデータ列のバイト長上限。埋め込みと同様、デコード前に上限検証する。
const MAX_METADATA_LEN: u32 = 4 * 1024 * 1024;

/// 永続化層の公開エラー型。`redb` の複数のエラー型（`DatabaseError` 等）はすべて
/// `redb::Error` へ変換可能なため、それを内部に保持して一本化する。
/// ライブラリコードとして panic せず、すべての失敗を `Result` で返す
/// （coding-rust.md: engine では `Result` を返し panic させない）。
#[derive(Debug)]
pub enum StorageError {
    /// `redb` 側で発生したエラー（I/O・破損検出・トランザクション競合等）。
    Backend(redb::Error),
    /// 行データのエンコード/デコードで検出した不正値。fail-closed に拒否する
    /// （欠落・不正値を黙殺フォールバックしない）。
    Codec(String),
    /// 指定した行 ID が存在しない。
    NotFound(u64),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Backend(e) => write!(f, "storage backend error: {e}"),
            StorageError::Codec(msg) => write!(f, "row codec error: {msg}"),
            StorageError::NotFound(id) => write!(f, "row not found: id={id}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Backend(e) => Some(e),
            StorageError::Codec(_) | StorageError::NotFound(_) => None,
        }
    }
}

// `redb` の各操作（begin_write・open_table・commit 等）はそれぞれ異なるエラー型を返すが、
// すべて `redb::Error` へ変換可能なので、ここで一括して `StorageError` へ橋渡しする。
impl<E> From<E> for StorageError
where
    E: Into<redb::Error>,
{
    fn from(e: E) -> Self {
        StorageError::Backend(e.into())
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// 永続化予定の行データ（呼び出し側が構築する入力形）。
pub struct RowInput<'a> {
    pub embedding: &'a [f32],
    /// 呼び出し元が定義する不透明なメタデータバイト列（本モジュールは中身を解釈しない）。
    pub metadata: &'a [u8],
}

/// 読み出した行データ（呼び出し側へ返す出力形）。
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u64,
    pub embedding: Vec<f32>,
    pub metadata: Vec<u8>,
}

/// `redb::Database` を保持する永続化層のハンドル。
///
/// wire-server の接続ハンドラや検索カーネルからは直接ではなく、このハンドルを介して
/// 行データへアクセスする想定（呼び出し元は `Storage` を通じてのみ永続化状態を触る）。
pub struct Storage {
    db: redb::Database,
}

impl Storage {
    /// 指定パスの `redb` データベースを開く。ファイルが存在しなければ新規作成する
    /// （`redb::Database::create` の契約）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = redb::Database::create(path)?;
        Ok(Self { db })
    }

    /// 単一行を書き込み、コミットする。
    ///
    /// PERSIST-1（クラッシュ耐性）: このメソッド内で `commit()` が返るまでは
    /// 永続化状態への反映は確定しない。呼び出し元が `commit()` 前にプロセスが
    /// 終了した場合でも、直前にコミット済みのデータは無傷であることを
    /// `redb` の ACID 保証に委ねる。
    pub fn put(&self, id: u64, row: &RowInput<'_>) -> Result<()> {
        let encoded = encode_row(row)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROWS_TABLE)?;
            table.insert(id, encoded.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// 複数行を単一トランザクションで書き込む（PERSIST-2: 増分書き込み。既存データを
    /// 保持したまま追加分だけを反映し、全体再構築を伴わない）。空スライスの場合は
    /// トランザクションを開かず即座に成功を返す。
    pub fn put_batch(&self, rows: &[(u64, RowInput<'_>)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROWS_TABLE)?;
            for (id, row) in rows {
                let encoded = encode_row(row)?;
                table.insert(*id, encoded.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// 行 ID を指定して 1 行取得する（スナップショット読み取り）。
    pub fn get(&self, id: u64) -> Result<Row> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            // テーブル未作成（1 行も書き込んでいない）は「存在しない」として扱う。
            Err(redb::TableError::TableDoesNotExist(_)) => return Err(StorageError::NotFound(id)),
            Err(e) => return Err(e.into()),
        };
        let guard = table.get(id)?.ok_or(StorageError::NotFound(id))?;
        decode_row(id, guard.value())
    }

    /// 全行をスナップショット読み取りで走査する（呼び出し時点でコミット済みの状態のみを
    /// 返し、走査中に他トランザクションが書き込んでも反映されない。PERSIST-4）。
    pub fn scan(&self) -> Result<Vec<Row>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            out.push(decode_row(k.value(), v.value())?);
        }
        Ok(out)
    }
}

/// 行を固定レイアウトでエンコードする（serde 系依存を増やさない方針。dependency-policy.md）。
///
/// レイアウト:
/// `[version: u8][dim: u32 le][embedding: dim * f32 le][metadata_len: u32 le][metadata bytes]`
///
/// バージョンバイトと非構造化のメタデータバイト列により、TASK-141（RLS フィールド同居）・
/// TASK-146（次元固定カタログ）等の後続スキーマ拡張が非互換変更なしに行えるようにしている。
fn encode_row(row: &RowInput<'_>) -> Result<Vec<u8>> {
    let dim = u32::try_from(row.embedding.len()).map_err(|_| {
        StorageError::Codec(format!("embedding dim too large: {}", row.embedding.len()))
    })?;
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    let metadata_len = u32::try_from(row.metadata.len())
        .map_err(|_| StorageError::Codec(format!("metadata too large: {}", row.metadata.len())))?;
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }

    let mut buf = Vec::with_capacity(1 + 4 + row.embedding.len() * 4 + 4 + row.metadata.len());
    buf.push(ROW_FORMAT_VERSION);
    buf.extend_from_slice(&dim.to_le_bytes());
    for v in row.embedding {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(&metadata_len.to_le_bytes());
    buf.extend_from_slice(row.metadata);
    Ok(buf)
}

/// [`encode_row`] の逆変換。欠落・不正値はすべて `Err` で拒否する（fail-closed。
/// 黙殺フォールバックで既知の型・デフォルト値へ落とさない）。添字アクセス `[]` ではなく
/// `get()` を使い、境界外アクセスを未定義動作にしない。
fn decode_row(id: u64, buf: &[u8]) -> Result<Row> {
    let version = *buf
        .first()
        .ok_or_else(|| StorageError::Codec("row buffer is empty".to_string()))?;
    if version != ROW_FORMAT_VERSION {
        return Err(StorageError::Codec(format!(
            "unsupported row format version: {version}"
        )));
    }
    let mut offset = 1usize;

    let dim_bytes = buf
        .get(offset..offset + 4)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at dim field".to_string()))?;
    let dim_arr: [u8; 4] = dim_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("dim field is not 4 bytes".to_string()))?;
    let dim = u32::from_le_bytes(dim_arr);
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    offset = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow after dim field".to_string()))?;

    let embedding_bytes_len = (dim as usize)
        .checked_mul(4)
        .ok_or_else(|| StorageError::Codec("embedding byte length overflow".to_string()))?;
    let embedding_end = offset
        .checked_add(embedding_bytes_len)
        .ok_or_else(|| StorageError::Codec("offset overflow after embedding field".to_string()))?;
    let embedding_bytes = buf.get(offset..embedding_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at embedding field".to_string())
    })?;
    // 上限検証済みの dim に基づくため、無制限確保にはならない。
    let mut embedding = Vec::with_capacity(dim as usize);
    for chunk in embedding_bytes.chunks_exact(4) {
        let arr: [u8; 4] = chunk
            .try_into()
            .map_err(|_| StorageError::Codec("embedding chunk is not 4 bytes".to_string()))?;
        embedding.push(f32::from_le_bytes(arr));
    }
    offset = embedding_end;

    let metadata_len_bytes = buf.get(offset..offset + 4).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at metadata_len field".to_string())
    })?;
    let metadata_len_arr: [u8; 4] = metadata_len_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("metadata_len field is not 4 bytes".to_string()))?;
    let metadata_len = u32::from_le_bytes(metadata_len_arr);
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }
    offset = offset.checked_add(4).ok_or_else(|| {
        StorageError::Codec("offset overflow after metadata_len field".to_string())
    })?;

    let metadata_end = offset
        .checked_add(metadata_len as usize)
        .ok_or_else(|| StorageError::Codec("offset overflow after metadata field".to_string()))?;
    let metadata_bytes = buf
        .get(offset..metadata_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at metadata field".to_string()))?;
    if metadata_end != buf.len() {
        return Err(StorageError::Codec(
            "row buffer has trailing bytes beyond declared metadata length".to_string(),
        ));
    }

    Ok(Row {
        id,
        embedding,
        metadata: metadata_bytes.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(embedding: &[f32], metadata: &[u8]) -> Vec<u8> {
        encode_row(&RowInput {
            embedding,
            metadata,
        })
        .unwrap()
    }

    #[test]
    fn row_roundtrips_through_encode_decode() {
        let embedding = vec![0.5_f32, -1.0, 2.25];
        let metadata = b"opaque".to_vec();
        let buf = sample_row(&embedding, &metadata);
        let row = decode_row(7, &buf).unwrap();
        assert_eq!(row.id, 7);
        assert_eq!(row.embedding, embedding);
        assert_eq!(row.metadata, metadata);
    }

    #[test]
    fn row_roundtrips_with_empty_embedding_and_metadata() {
        let buf = sample_row(&[], &[]);
        let row = decode_row(1, &buf).unwrap();
        assert!(row.embedding.is_empty());
        assert!(row.metadata.is_empty());
    }

    #[test]
    fn decode_row_rejects_truncated_buffer() {
        let buf = sample_row(&[1.0, 2.0], b"meta");
        for cut in 1..buf.len() {
            assert!(
                decode_row(1, &buf[..cut]).is_err(),
                "truncated buffer of length {cut} should not decode successfully"
            );
        }
    }

    #[test]
    fn decode_row_rejects_unknown_version() {
        let mut buf = sample_row(&[1.0], b"m");
        buf[0] = 0xFF;
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn decode_row_rejects_trailing_garbage() {
        let mut buf = sample_row(&[1.0], b"m");
        buf.push(0);
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn decode_row_rejects_oversized_dim_without_allocating() {
        // dim をアロケーション上限より大きい値に書き換えたバッファ。
        // decode_row は Vec::with_capacity を呼ぶ前に dim の上限検証で拒否するべき。
        let mut buf = sample_row(&[1.0], b"m");
        let oversized = MAX_EMBEDDING_DIM + 1;
        buf[1..5].copy_from_slice(&oversized.to_le_bytes());
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn encode_row_rejects_oversized_metadata() {
        let huge = vec![0u8; (MAX_METADATA_LEN as usize) + 1];
        let result = encode_row(&RowInput {
            embedding: &[],
            metadata: &huge,
        });
        assert!(result.is_err());
    }
}
