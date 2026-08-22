//! コールドスタート・ベクトルアリーナ（TASK-87、対象ビヘイビア: TABLE-8。
//! ポインタ: `docs/spec/05-tasks.md` TASK-87・`docs/spec/04-behavior/data-model.md`
//! TABLE-8）。
//!
//! 責務境界: `storage.rs` はクエリの都度 `redb` から行を読み直してデコードする経路
//! （[`crate::storage::Storage::get`]・[`crate::storage::Storage::scan`] 等）しか
//! 提供しない。本モジュールは、単一の読み取りスナップショットから一度だけ全行を
//! 連続 `Vec<f32>` バッファへデコードし、以降の参照はそのバッファ上のスライスで
//! 完結させる「コールドスタート時の一括ロード」経路を追加する。検索カーネル本体
//! （スコアリング・top-k・SIMD/GPU 経路）は後続タスクの管轄であり、本モジュールは
//! 「一度デコードした連続バッファの提供」までを責務境界とする。
//!
//! スコープ境界（重要・TASK-91 と同根）: 行とカタログ上のユーザーテーブルを関連付ける
//! 永続化上の機構（行ごとのテーブル識別子）は本リポジトリにまだ実装されていない
//! （[`crate::storage::ROWS_TABLE`] は単一の平坦なテーブルで、異なる次元のテーブルの
//! 行が同居し得る。`crates/engine/tests/multi_dim_tables.rs` 参照）。そのため
//! [`VectorArena::build`] は呼び出し元にテーブル名を要求し、カタログ
//! （[`crate::catalog`]）に**そのテーブルしか存在しない**ことを確認してから走査する
//! （fail-closed ゲート）。カタログに他のユーザーテーブルが 1 つでも存在する場合は、
//! 行とテーブルを安全に対応付けられないため `Err` で拒否する（黙って全行を混在させる
//! fail-open は行わない）。次元不一致の行を検出した場合も同様に、部分的なアリーナを
//! 返さず `Err` で拒否する。複数テーブルが同居する DB からテーブル単位でアリーナを
//! 構築する機構（行への永続的なテーブル識別子付与）は後続タスクの管轄とする。
//!
//! RLS との関係: `tenant_id`・`visibility` はデータとして同居保持するのみで、
//! ポリシー評価（可視性判定・RLS 事前フィルタ）そのものは行わない
//! （`storage.rs`・`txn.rs` と同一の責務境界。評価は TASK-133 以降の呼び出し元の責務）。

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

use crate::catalog::CatalogError;
use crate::storage::{decode_row, Storage, StorageError, Visibility, ROWS_TABLE};

/// アリーナが保持してよい行数の上限（アロケーション前の事前検証に使う。
/// security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_ARENA_ROWS: usize = 1_000_000;

/// アリーナが確保してよい総バイト量（`vectors: Vec<f32>` 相当）の上限。
/// `MAX_ARENA_ROWS` だけでは次元数が大きい場合に依然として巨大確保になり得るため、
/// 行数とバイト量の両方で上限を課す（`storage.rs` の `MAX_SCAN_TOTAL_BYTES` と同方針）。
const MAX_ARENA_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// アリーナ構築層の公開エラー型。`storage.rs` の設計メモ（`StorageError` への
/// blanket `From<E: Into<redb::Error>>` impl）が存在するため coherence 制約上
/// 同種の blanket impl はこの型へ追加できない。`redb` へは必ず `StorageError` 経由で
/// 到達し、本型は `StorageError` からの明示的な `From` のみを持つ
/// （`catalog.rs` の `CatalogError` と同方針）。
#[derive(Debug)]
pub enum ArenaError {
    /// 永続化層側で発生したエラー（`redb` バックエンドエラー・行デコード失敗等）。
    Storage(StorageError),
    /// カタログ層側で発生したエラー（対象テーブル不存在・識別子不正等）。
    Catalog(CatalogError),
    /// `expected_dim` が不正（`0` または [`crate::storage::MAX_EMBEDDING_DIM`] 超過）。
    /// 対象テーブルが `VECTOR` 列を持たない場合もこの variant を返す。
    InvalidDim,
    /// アリーナ構築対象の行数・総バイト量がアロケーション前の上限
    /// （[`MAX_ARENA_ROWS`]・[`MAX_ARENA_TOTAL_BYTES`]）を超過した。fail-closed に拒否する。
    CapacityExceeded,
    /// `expected_dim` と一致しない次元の行を検出した。黙殺スキップせず拒否する
    /// （部分的なアリーナを返さない。fail-open を避けるための判断）。
    DimMismatch { id: u64, expected: u32, found: u32 },
    /// カタログに要求したテーブル以外のユーザーテーブルが存在するため、行と
    /// テーブルを安全に対応付けられない（モジュールドキュメントのスコープ境界参照）。
    /// fail-closed に拒否する。
    MultipleTablesPresent { requested: String, other: String },
}

impl std::fmt::Display for ArenaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArenaError::Storage(e) => write!(f, "arena storage error: {e}"),
            ArenaError::Catalog(e) => write!(f, "arena catalog error: {e}"),
            ArenaError::InvalidDim => write!(f, "invalid expected_dim for arena build"),
            ArenaError::CapacityExceeded => write!(f, "arena capacity exceeded"),
            ArenaError::DimMismatch {
                id,
                expected,
                found,
            } => write!(
                f,
                "embedding dim mismatch at row id={id}: expected={expected} found={found}"
            ),
            ArenaError::MultipleTablesPresent { requested, other } => write!(
                f,
                "cannot scope arena to table={requested}: another table={other} is present in the catalog"
            ),
        }
    }
}

impl std::error::Error for ArenaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArenaError::Storage(e) => Some(e),
            ArenaError::Catalog(e) => Some(e),
            ArenaError::InvalidDim
            | ArenaError::CapacityExceeded
            | ArenaError::DimMismatch { .. }
            | ArenaError::MultipleTablesPresent { .. } => None,
        }
    }
}

impl From<StorageError> for ArenaError {
    fn from(e: StorageError) -> Self {
        ArenaError::Storage(e)
    }
}

impl From<CatalogError> for ArenaError {
    fn from(e: CatalogError) -> Self {
        ArenaError::Catalog(e)
    }
}

pub type Result<T> = std::result::Result<T, ArenaError>;

/// [`VectorArena::build`] のアロケーション前上限検証（行数・総バイト量の両方、
/// `checked_mul` によるオーバーフロー安全な演算）。成功時は確保すべき `f32` 要素数
/// （`row_count * dim`）を返す。
///
/// 上限値（`max_rows`・`max_bytes`）を引数として受け取る形に切り出しているのは、
/// `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES` が private 定数であり、統合テスト
/// （`tests/arena.rs`）からは境界値検証（ちょうど上限・上限+1 等）を再現できないため。
/// 境界値検証は本ファイル内の `#[cfg(test)]` モジュールで行う。
fn check_capacity(row_count: usize, dim: u32, max_rows: usize, max_bytes: usize) -> Result<usize> {
    if row_count > max_rows {
        return Err(ArenaError::CapacityExceeded);
    }
    let total_floats = row_count
        .checked_mul(dim as usize)
        .ok_or(ArenaError::CapacityExceeded)?;
    let total_bytes = total_floats
        .checked_mul(4)
        .ok_or(ArenaError::CapacityExceeded)?;
    if total_bytes > max_bytes {
        return Err(ArenaError::CapacityExceeded);
    }
    Ok(total_floats)
}

/// コールドスタート時に一括デコードした連続ベクトルバッファ（対象ビヘイビア: TABLE-8）。
///
/// [`VectorArena::build`] が単一の読み取りスナップショットから構築する。構築後に
/// `Storage` へ加わった変更（他ライタによる書き込みを含む）は反映されない
/// （`redb::ReadTransaction` のスナップショット契約をそのまま引き継ぐ）。
#[derive(Debug)]
pub struct VectorArena {
    /// 構築対象のテーブル名（カタログ上の識別子）。行への永続的なテーブル識別子が
    /// まだ存在しないため（モジュールドキュメント参照）、[`VectorArena::build`] が
    /// カタログゲートで検証した対象を呼び出し元が失わないよう保持する。
    table_name: String,
    dim: u32,
    /// 行 ID 昇順・row-major の連続バッファ。長さは常に `ids.len() * dim` と一致する。
    vectors: Vec<f32>,
    /// `vectors` の第 i 行（`vectors[i*dim..(i+1)*dim]`）に対応する行 ID。
    ids: Vec<u64>,
    /// 後続の RLS 事前フィルタ（TASK-89/133 系）が redb 再読なしでテナント判定できる
    /// よう同居保持する（ポリシー評価自体は行わない。モジュールドキュメント参照）。
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
}

impl VectorArena {
    /// `storage` の現時点のスナップショットから、カタログ上のテーブル `table_name`
    /// を対象としたアリーナを構築する（対象ビヘイビア: TABLE-8）。
    ///
    /// `expected_dim` はカタログ（[`Storage::get_table_schema`] →
    /// [`crate::catalog::TableSchema::vector_dim`]）から取得し、呼び出し元からは受け取らない
    /// （呼び出し元が渡す次元値をテーブル識別の代用にすると、同一次元の別テーブルが
    /// 混入しても検出できないため。TASK-87 P1 レビュー指摘への対応）。
    ///
    /// カタログに `table_name` 以外のユーザーテーブルが 1 つでも存在する場合は
    /// `Err(ArenaError::MultipleTablesPresent)` で拒否する。[`crate::storage::ROWS_TABLE`]
    /// は行ごとの永続的なテーブル識別子を持たないため（モジュールドキュメント参照）、
    /// 「カタログ上のユーザーテーブルが対象の 1 つだけ」であることのみが、走査した
    /// 全行が対象テーブルに帰属することを保証できる条件だからである。
    ///
    /// 単一の `storage.db().begin_read()` で全行を走査し（[`crate::storage::Storage::scan_page`]
    /// は呼び出しごとに別トランザクションを開くためページ間のスナップショット一貫性がなく、
    /// アリーナ構築には使えない）、アロケーション前に行数・総バイト量の上限を検証してから
    /// 確保する（無制限 `Vec::with_capacity` 禁止。.claude/rules/coding-rust.md）。
    ///
    /// 対象テーブルがカタログに存在しない場合・`VECTOR` 列を持たない場合は
    /// `Err`（テーブル未作成の空アリーナという特別扱いはしない。カタログに登録されて
    /// いて 1 行も書き込まれていない場合のみ空アリーナとして成功する）。次元不一致の
    /// 行を検出した場合はスキップせず `Err(ArenaError::DimMismatch)` を返す
    /// （モジュールドキュメントのスコープ境界を参照）。
    pub fn build(storage: &Storage, table_name: &str) -> Result<Self> {
        // スキーマ取得・カタログゲート（他テーブル存在チェック）・`ROWS_TABLE` 走査を
        // すべて単一の `read_txn`（同一スナップショット）上で行う。別トランザクションに
        // 分かれていると、ゲート判定通過後・走査前に他テーブルの行が並行挿入されても
        // 検出できない TOCTOU が生じる（TASK-87 P1 レビュー指摘対応。モジュール
        // ドキュメントのスコープ境界参照）。
        let read_txn = storage.db().begin_read().map_err(StorageError::from)?;
        let (schema, tables) = Storage::schema_and_tables_in_txn(&read_txn, table_name)?;
        let expected_dim = schema.vector_dim().ok_or(ArenaError::InvalidDim)?;
        if expected_dim == 0 || expected_dim > crate::storage::MAX_EMBEDDING_DIM {
            return Err(ArenaError::InvalidDim);
        }

        // カタログゲート: 対象テーブル以外のユーザーテーブルが存在する場合、
        // `ROWS_TABLE` の全行を対象テーブルへ安全に帰属させられないため拒否する。
        for other in tables {
            if other != table_name {
                return Err(ArenaError::MultipleTablesPresent {
                    requested: table_name.to_string(),
                    other,
                });
            }
        }

        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(VectorArena {
                    table_name: table_name.to_string(),
                    dim: expected_dim,
                    vectors: Vec::new(),
                    ids: Vec::new(),
                    tenant_ids: Vec::new(),
                    visibilities: Vec::new(),
                });
            }
            Err(e) => return Err(StorageError::from(e).into()),
        };

        // アロケーション前の上限検証（.claude/rules/security.md「不安全な設計｜
        // 無制限リソース確保（DoS）」対応）。`table.len()` は redb 側の集計値で、
        // ここではまだ 1 バイトもデコードしない。
        let row_count = usize::try_from(table.len().map_err(StorageError::from)?)
            .map_err(|_| ArenaError::CapacityExceeded)?;
        let total_floats = check_capacity(
            row_count,
            expected_dim,
            MAX_ARENA_ROWS,
            MAX_ARENA_TOTAL_BYTES,
        )?;

        // 検証を通過した後にのみ確保する。
        let mut vectors = Vec::with_capacity(total_floats);
        let mut ids = Vec::with_capacity(row_count);
        let mut tenant_ids = Vec::with_capacity(row_count);
        let mut visibilities = Vec::with_capacity(row_count);

        for entry in table.iter().map_err(StorageError::from)? {
            let (k, v) = entry.map_err(StorageError::from)?;
            let id = k.value();
            let row = decode_row(id, v.value()).map_err(ArenaError::from)?;
            let found_dim =
                u32::try_from(row.embedding.len()).map_err(|_| ArenaError::DimMismatch {
                    id,
                    expected: expected_dim,
                    found: u32::MAX,
                })?;
            if found_dim != expected_dim {
                return Err(ArenaError::DimMismatch {
                    id,
                    expected: expected_dim,
                    found: found_dim,
                });
            }
            vectors.extend_from_slice(&row.embedding);
            ids.push(id);
            tenant_ids.push(row.tenant_id);
            visibilities.push(row.visibility);
        }

        Ok(VectorArena {
            table_name: table_name.to_string(),
            dim: expected_dim,
            vectors,
            ids,
            tenant_ids,
            visibilities,
        })
    }

    /// 構築対象のテーブル名（カタログ上の識別子）。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// 埋め込みの次元数。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 保持している行数。
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// 行を 1 件も保持していないか。
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// 行 ID の一覧（構築時のスキャン順＝行 ID 昇順）。
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// row-major の連続ベクトルバッファ全体（長さ = `len() * dim()`）。
    pub fn vectors(&self) -> &[f32] {
        &self.vectors
    }

    /// `index` 番目の行の埋め込みスライスを返す。範囲外は `None`
    /// （`.claude/rules/coding-rust.md`: 添字アクセス `[]` を production コードで使わない）。
    pub fn vector(&self, index: usize) -> Option<&[f32]> {
        let dim = self.dim as usize;
        let start = index.checked_mul(dim)?;
        let end = start.checked_add(dim)?;
        self.vectors.get(start..end)
    }

    /// `index` 番目の行のテナント識別子。範囲外は `None`。
    pub fn tenant_id(&self, index: usize) -> Option<&str> {
        self.tenant_ids.get(index).map(String::as_str)
    }

    /// `index` 番目の行の可視性ラベル。範囲外は `None`。
    pub fn visibility(&self, index: usize) -> Option<Visibility> {
        self.visibilities.get(index).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_check_accepts_at_row_limit() {
        assert_eq!(
            check_capacity(100, 4, 100, usize::MAX).expect("within row limit"),
            400
        );
    }

    #[test]
    fn capacity_check_rejects_over_row_limit() {
        assert!(matches!(
            check_capacity(101, 4, 100, usize::MAX),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn capacity_check_accepts_at_byte_limit() {
        // row_count * dim * 4 == max_bytes ちょうど。
        assert_eq!(
            check_capacity(10, 4, usize::MAX, 160).expect("within byte limit"),
            40
        );
    }

    #[test]
    fn capacity_check_rejects_over_byte_limit() {
        assert!(matches!(
            check_capacity(10, 4, usize::MAX, 159),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn capacity_check_does_not_overflow_on_huge_dim() {
        // usize::MAX 近傍の dim を渡しても checked_mul が Err に落ちるだけで panic しない。
        let result = check_capacity(usize::MAX / 2, u32::MAX, usize::MAX, usize::MAX);
        assert!(matches!(result, Err(ArenaError::CapacityExceeded)));
    }

    /// `tests/arena.rs`（統合テスト）と同方針の一意 DB パス払い出しヘルパー。
    /// `schema_and_tables_in_txn` は `pub(crate)` のため、統合テストからは呼べず
    /// このモジュール内 unit test でのみ検証できる。
    fn unique_db_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vector-db-engine-arena-unit-{label}-{}-{seq}.redb",
            std::process::id()
        ));
        path
    }

    // TASK-87 P1 レビュー指摘（カタログゲートと行走査の TOCTOU）への回帰テスト。
    // 対象テーブル `a` について `schema_and_tables_in_txn` を呼んだ read_txn を
    // 保持したまま、*その後* に別テーブル `b` を作成・行を書き込んでも、同一
    // read_txn 上で見えるテーブル一覧・ROWS_TABLE の行数はどちらも書き込み前の
    // スナップショットのまま（ゲート判定と行走査が同一スナップショットで一致する
    // こと）を検証する。
    #[test]
    fn schema_and_tables_in_txn_observes_a_single_snapshot_across_concurrent_writes() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::storage::{RowInput, Visibility};

        let path = unique_db_path("toctou");
        let storage = Storage::open(&path).expect("open storage");

        let schema_a = TableSchema::new(
            "a",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        );
        storage.create_table(&schema_a).expect("create table a");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[0.0, 1.0, 2.0, 3.0],
                    metadata: &[],
                },
            )
            .expect("put row into a");

        // ゲート判定用のスナップショットを先に確立する。
        let read_txn = storage.db().begin_read().expect("begin_read");
        let (_schema, tables) =
            Storage::schema_and_tables_in_txn(&read_txn, "a").expect("schema_and_tables_in_txn");
        assert_eq!(tables, vec!["a".to_string()]);

        // read_txn 確立後に別テーブル・同次元の行を並行挿入する（TOCTOU 再現）。
        let schema_b = TableSchema::new(
            "b",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        );
        storage.create_table(&schema_b).expect("create table b");
        storage
            .put(
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[9.0, 9.0, 9.0, 9.0],
                    metadata: &[],
                },
            )
            .expect("put row into b");

        // 同一 read_txn 上では、後発の書き込みは一切見えない。テーブル一覧・
        // ROWS_TABLE の行数のどちらも、read_txn 確立時点のスナップショットのまま。
        let (_schema, tables_again) =
            Storage::schema_and_tables_in_txn(&read_txn, "a").expect("schema_and_tables_in_txn");
        assert_eq!(tables_again, vec!["a".to_string()]);

        let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");
        assert_eq!(table.len().expect("table len"), 1);

        drop(read_txn);
        let _ = std::fs::remove_file(&path);
    }
}
