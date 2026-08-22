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
//! 行が同居し得る。`crates/engine/tests/multi_dim_tables.rs` 参照）。[`VectorArena::build`]
//! はテーブル名を受け取り、カタログ（[`crate::catalog`]）に**そのテーブルしか存在しない**
//! ことを確認してから走査する（他のユーザーテーブルが 1 つでも存在する場合は
//! `Err(MultipleTablesPresent)` で拒否する）。
//!
//! **このゲートだけでは行の帰属を証明できない**（TASK-87 P1 レビュー指摘）:
//! [`crate::storage::Storage::put`]・[`crate::storage::Storage::put_batch`] は
//! テーブル名・スキーマを要求せず、カタログにテーブルが 1 つも存在しない状態でも
//! `ROWS_TABLE` へ行を書き込める。そのため「対象テーブルしかカタログに存在しない」
//! ことは、「`ROWS_TABLE` の全行が対象テーブルの書き込みによるもの」を含意しない
//! （対象テーブル作成より前に書かれた行が存在しても検出できない）。この帰属を
//! 検証可能にする機構（行への永続的なテーブル識別子付与）は TASK-91 の管轄である。
//! そのため [`VectorArena::build`] の契約は「テーブルスコープ」ではなく
//! **「ストアスコープ」**（`ROWS_TABLE` 全体を、カタログが単一のベクトルテーブルしか
//! 持たないという条件下で走査する）として公開する。孤立行（対象テーブル作成前に
//! 書かれ、次元が一致する行）が含まれ得ることは既知の限界として明文化し（黙殺せず
//! テスト・ドキュメントで固定する）、テーブル単位の帰属保証は TASK-91 まで持ち越す。
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
    /// カタログに要求したテーブル以外のユーザーテーブルが存在する（fail-closed に
    /// 拒否する）。このゲートは「他のユーザーテーブルが存在しない」ことのみを保証し、
    /// 対象テーブル作成前に書かれた孤立行の混入までは検出できない
    /// （モジュールドキュメントのスコープ境界参照。TASK-87 P1 レビュー指摘）。
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
/// `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES` が private 定数であり、境界値検証
/// （ちょうど上限・上限+1 等）を本ファイル内の `#[cfg(test)]` モジュールから
/// 直接パラメータ化して再現するため。
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
    /// 構築時に `build` へ渡されたテーブル名（カタログゲートで「このテーブルしか
    /// 存在しないことを検証した」対象。行への永続的なテーブル識別子がまだ存在しない
    /// ため、`vectors`/`ids` の全行がこのテーブルに帰属することの保証ではない
    /// （モジュールドキュメントのストアスコープ契約を参照）。
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
    /// `Err(ArenaError::MultipleTablesPresent)` で拒否する。ただし、このゲートは
    /// 行の帰属を証明する十分条件ではない（モジュールドキュメントのスコープ境界参照。
    /// TASK-87 P1 レビュー指摘）。契約は「ストアスコープ」（`ROWS_TABLE` 全体を、
    /// カタログが単一のベクトルテーブルしか持たない条件下で走査する）であり、対象
    /// テーブル作成前に書かれた次元一致の孤立行が含まれ得ることは既知の限界として
    /// 明文化する。テーブル単位の帰属保証（行への永続的なテーブル識別子付与）は
    /// TASK-91 の管轄。
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
    ///
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

    /// 構築時に `build` へ渡されたテーブル名（カタログゲートで検証した対象。
    /// 全行の帰属保証ではない。上記フィールドのドキュメント参照）。
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
    use crate::storage::RowInput;

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

    // 以下は旧 `tests/arena.rs`・`tests/arena_perf.rs`（統合テスト）からの移設分。
    // `VectorArena::build` は `pub` だが、`schema_and_tables_in_txn`（`pub(crate)`）を
    // 経由するテスト（TOCTOU 回帰テスト）と同居させるため、クレート内の
    // `#[cfg(test)]` モジュールへ移設したまま保持している。

    struct CleanupGuard(std::path::PathBuf);

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    const TENANT_ID: &str = "tenant-a";

    /// 外部クレート非依存の決定的擬似乱数生成器（xorshift32）。テストデータ生成にのみ使う。
    struct Xorshift32(u32);

    impl Xorshift32 {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }

        fn next_f32(&mut self) -> f32 {
            (self.next_u32() as f64 / u32::MAX as f64) as f32
        }
    }

    fn make_embedding(rng: &mut Xorshift32, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| rng.next_f32()).collect()
    }

    /// `table_name` の schema を組み立てる（`embedding` 列 1 本のみを持つ最小構成。
    /// `multi_dim_tables.rs` と同方針）。
    fn schema_for(table_name: &str, dim: u32) -> crate::catalog::TableSchema {
        use crate::catalog::{ColumnDef, ColumnType};
        crate::catalog::TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    // 対象ビヘイビア: TABLE-8。複数行を投入して build した結果が、行数・次元・各行の
    // 内容とも Storage::get の読み直し結果と一致し、連続バッファの長さが len * dim と
    // 一致すること（コールドスタート・アリーナの基本契約）を検証する。
    #[test]
    fn build_produces_contiguous_arena_matching_storage_rows() {
        let path = unique_db_path("basic");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let dim: usize = 8;
        storage
            .create_table(&schema_for("docs", dim as u32))
            .expect("create_table");

        let mut rng = Xorshift32(0x1234_5678);
        let embeddings: Vec<Vec<f32>> = (0..10).map(|_| make_embedding(&mut rng, dim)).collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, emb)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility: if i % 2 == 0 {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                        embedding: emb,
                        metadata: b"m",
                    },
                )
            })
            .collect();
        storage.put_batch(&rows).expect("seed rows");

        let arena = VectorArena::build(&storage, "docs").expect("build arena");
        assert_eq!(arena.table_name(), "docs");
        assert_eq!(arena.dim(), dim as u32);
        assert_eq!(arena.len(), 10);
        assert!(!arena.is_empty());
        assert_eq!(arena.vectors().len(), 10 * dim);
        assert_eq!(arena.ids(), &(0u64..10).collect::<Vec<_>>()[..]);

        for i in 0..10usize {
            let expected_row = storage.get(i as u64).expect("read row back via storage");
            assert_eq!(arena.vector(i), Some(expected_row.embedding.as_slice()));
            assert_eq!(arena.tenant_id(i), Some(expected_row.tenant_id.as_str()));
            assert_eq!(arena.visibility(i), Some(expected_row.visibility));
        }

        // 範囲外は panic せず None を返す。
        assert_eq!(arena.vector(10), None);
        assert_eq!(arena.tenant_id(10), None);
        assert_eq!(arena.visibility(10), None);
    }

    // 対象ビヘイビア: TABLE-8。カタログに登録済みだが 1 行も書き込んでいないテーブル
    // （ROWS_TABLE 未作成）は空アリーナとして成功すること。
    #[test]
    fn build_on_empty_table_returns_empty_arena() {
        let path = unique_db_path("empty");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 16))
            .expect("create_table");

        let arena = VectorArena::build(&storage, "docs").expect("build arena on empty table");
        assert_eq!(arena.dim(), 16);
        assert_eq!(arena.len(), 0);
        assert!(arena.is_empty());
        assert!(arena.vectors().is_empty());
        assert!(arena.ids().is_empty());
    }

    // 対象ビヘイビア: TABLE-8。次元不一致の行が 1 行でも混在していれば、部分的な
    // アリーナを返さず Err(DimMismatch) で fail-closed に拒否すること
    // （黙殺スキップは検索結果の欠落＝fail-open に相当するため禁止）。
    #[test]
    fn build_rejects_dimension_mismatch_without_partial_result() {
        let path = unique_db_path("dim-mismatch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        storage
            .put(
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"m",
                },
            )
            .expect("seed matching-dim row");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                },
            )
            .expect("seed mismatched-dim row");

        let err = VectorArena::build(&storage, "docs").expect_err("dim mismatch must be rejected");
        match err {
            ArenaError::DimMismatch {
                id,
                expected,
                found,
            } => {
                assert_eq!(id, 1);
                assert_eq!(expected, 4);
                assert_eq!(found, 2);
            }
            other => panic!("expected DimMismatch, got {other:?}"),
        }
    }

    // 対象ビヘイビア: TABLE-8。対象テーブルがカタログに存在しない場合、および
    // `VECTOR` 列を持たない場合は `Err(InvalidDim)` で拒否すること。
    #[test]
    fn build_rejects_missing_table_and_table_without_vector_column() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};

        let path = unique_db_path("invalid-dim");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // カタログに未登録のテーブル名。
        assert!(VectorArena::build(&storage, "not_registered").is_err());

        // VECTOR 列を持たないテーブル。
        let text_only = TableSchema::new(
            "notes",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        storage.create_table(&text_only).expect("create_table");
        assert!(matches!(
            VectorArena::build(&storage, "notes"),
            Err(ArenaError::InvalidDim)
        ));
    }

    // 対象ビヘイビア: TABLE-8（P1 レビュー指摘対応）。カタログに対象テーブル以外の
    // ユーザーテーブルが存在する場合、たとえ同一次元であっても `build` は
    // `Err(MultipleTablesPresent)` で拒否し、他テーブルの行を混入させないこと。
    #[test]
    fn build_rejects_when_another_table_coexists_even_with_same_dim() {
        let path = unique_db_path("multi-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        storage
            .create_table(&schema_for("docs_a", 4))
            .expect("create_table docs_a");
        storage
            .create_table(&schema_for("docs_b", 4))
            .expect("create_table docs_b");

        // 同じ ROWS_TABLE へテーブル帰属の区別なく書き込まれる（永続化層の現行制約。
        // モジュールドキュメント参照）。docs_b 側の行のみを書き込む。
        storage
            .put(
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"table=docs_b",
                },
            )
            .expect("seed docs_b row");

        let err =
            VectorArena::build(&storage, "docs_a").expect_err("must reject when docs_b coexists");
        match err {
            ArenaError::MultipleTablesPresent { requested, other } => {
                assert_eq!(requested, "docs_a");
                assert_eq!(other, "docs_b");
            }
            other => panic!("expected MultipleTablesPresent, got {other:?}"),
        }

        // docs_b を対象に build しても、カタログに docs_a が残る限り同じゲートで拒否される
        // （テーブル単位で安全に走査できるのは「カタログ上のユーザーテーブルが 1 つだけ」の
        // ときに限られる。モジュールドキュメント参照）。
        let err_b =
            VectorArena::build(&storage, "docs_b").expect_err("must reject when docs_a coexists");
        assert!(matches!(err_b, ArenaError::MultipleTablesPresent { .. }));
    }

    // 対象ビヘイビア: TABLE-8。アリーナは構築時点のスナップショットであり、build 後に
    // 追加された行は反映されない（単一スナップショットで構築する契約）。再 build すれば
    // 反映される。
    #[test]
    fn build_captures_a_snapshot_not_reflecting_later_writes() {
        let path = unique_db_path("snapshot");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        storage
            .put(
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                },
            )
            .expect("seed initial row");

        let arena_before = VectorArena::build(&storage, "docs").expect("build before extra write");
        assert_eq!(arena_before.len(), 1);

        storage
            .put(
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[3.0, 4.0],
                    metadata: b"m",
                },
            )
            .expect("seed additional row after snapshot");

        // build 前に取得した arena_before はそのまま（後続の put の影響を受けない）。
        assert_eq!(arena_before.len(), 1);

        let arena_after = VectorArena::build(&storage, "docs").expect("rebuild after extra write");
        assert_eq!(arena_after.len(), 2);
    }

    // 対象ビヘイビア: TABLE-8（TASK-87 P1 レビュー指摘への回帰テスト。攻撃シナリオの再現）。
    // カタログにテーブルが 1 つも存在しない状態で `Storage::put` により孤立行
    // （どのテーブルにも属さない行）を書き込み、その後で対象テーブルを作成すると、
    // カタログゲート（「対象テーブルしか存在しない」）は通過してしまう。しかし
    // モジュールドキュメントのスコープ境界のとおり、このゲートは行の帰属を証明する
    // 十分条件ではない。`build` の契約は「ストアスコープ」であり、この回帰テストで
    // 「孤立行が混入した場合の挙動」を固定する。現状の実装は孤立行の有無を検出できない
    // ため、混入を許容してしまう（既知の限界。行への永続的なテーブル識別子付与は
    // TASK-91 の管轄）。
    #[test]
    fn build_documents_orphan_row_limitation() {
        let path = unique_db_path("orphan-row");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // カタログに 1 つもテーブルが存在しない状態で、同次元の行を書き込む
        // （`Storage::put` はテーブル名・スキーマを要求しないため書き込める）。
        storage
            .put(
                999,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"orphan",
                },
            )
            .expect("seed orphan row before any table exists");

        // その後に対象テーブルを作成する。カタログゲート（対象テーブルしか存在しない）は
        // 通過するが、孤立行 id=999 は対象テーブルに属さない。
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table after orphan row was written");

        let arena = VectorArena::build(&storage, "docs").expect("build arena");
        // 既知の限界: 現状の実装は孤立行を検出できず、混入したまま返す。この事実を
        // 固定するのが本テストの目的（`pub(crate)` 化・ドキュメント修正の裏付け）。
        // 行への永続的なテーブル識別子付与（TASK-91）が実装され次第、このアサーションを
        // 「孤立行を検出して Err を返す」側へ書き換える必要がある。
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.ids(), &[999u64]);
    }

    // 以下、旧 `tests/arena_perf.rs`（統合テスト）からの移設分。「コールドスタート時に
    // 一度だけアリーナを構築し、以降の検索はアリーナ上のスライスを参照する経路」が
    // 「クエリの都度 `Storage::scan` で全行を読み直してデコードする経路」より十分速い
    // ことを CI で検出可能にする（`tests/incremental_write_perf.rs`（TASK-143）と同一の
    // 計測方針: ウォームアップ 1 回を除外・複数ラウンドの中央値比較・
    // `Duration::saturating_mul` の整数比較で判定・判定閾値は本テスト固有の計測パラメータで
    // spec の実測比そのものは転記しない）。
    //
    // 規模の選定: `Storage::scan()` は総バイト量 64MiB 超で `ScanLimitExceeded` を返す
    // （`storage.rs` 参照）。本テストの行数・次元は、その上限に対して十分な余裕を残し、かつ
    // debug ビルドでも CI 実行時間が長くなりすぎないよう小さく抑えている
    // （ROWS × DIM × 4 バイト ≈ 5.1 MiB で 64MiB に対して十分小さい）。
    mod perf {
        use super::*;
        use std::time::{Duration, Instant};

        /// 計測対象テーブル名。カタログにこのテーブルのみを登録し、`VectorArena::build`
        /// のテーブルスコープゲート（TASK-87 P1 レビュー指摘対応）を満たす。
        const TABLE_NAME: &str = "docs";

        /// 行数・次元（モジュールドキュメントの規模選定を参照）。
        const ROWS: u64 = 5_000;
        const DIM: usize = 128;

        /// 1 ラウンドで実行するクエリ本数。
        const QUERY_COUNT: usize = 40;

        /// ノイズ対策として、両経路それぞれを複数回計測し中央値を取る回数。
        const MEASUREMENT_ROUNDS: usize = 3;

        /// 判定閾値の分母（アリーナ経路は都度読み直し経路の `1 / RATIO_THRESHOLD_DENOM`
        /// 以下の時間で完了すること）。本テストの計測パラメータであり、アサーション
        /// 弱体化は行わない（`.claude/rules/coding-rust.md` 参照）。
        const RATIO_THRESHOLD_DENOM: u32 = 4;

        fn make_vector(rng: &mut Xorshift32, dim: usize) -> Vec<f32> {
            (0..dim).map(|_| rng.next_f32()).collect()
        }

        fn median(mut values: Vec<Duration>) -> Duration {
            values.sort();
            values[values.len() / 2]
        }

        /// 単純な内積（テスト内の素朴なスコアリング。検索カーネル本体は後続タスクの管轄。
        /// モジュールドキュメント参照）。
        fn dot(a: &[f32], b: &[f32]) -> f32 {
            a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
        }

        fn best_score_over_arena(arena: &VectorArena, query: &[f32]) -> f32 {
            let mut best = f32::MIN;
            for i in 0..arena.len() {
                let v = arena.vector(i).expect("index within arena bounds");
                let score = dot(v, query);
                if score > best {
                    best = score;
                }
            }
            best
        }

        fn best_score_over_rescan(storage: &Storage, query: &[f32]) -> f32 {
            let rows = storage.scan().expect("scan within configured limits");
            let mut best = f32::MIN;
            for row in &rows {
                let score = dot(&row.embedding, query);
                if score > best {
                    best = score;
                }
            }
            best
        }

        fn seed_storage(path: &std::path::Path) -> Storage {
            let storage = Storage::open(path).expect("open storage");
            storage
                .create_table(&schema_for(TABLE_NAME, DIM as u32))
                .expect("create_table");
            let mut rng = Xorshift32(0x2545_f491);
            let rows: Vec<(u64, Vec<f32>)> = (0..ROWS)
                .map(|id| (id, make_vector(&mut rng, DIM)))
                .collect();
            let batch: Vec<(u64, RowInput<'_>)> = rows
                .iter()
                .map(|(id, emb)| {
                    (
                        *id,
                        RowInput {
                            tenant_id: TENANT_ID,
                            visibility: Visibility::Public,
                            embedding: emb,
                            metadata: b"m",
                        },
                    )
                })
                .collect();
            storage.put_batch(&batch).expect("seed dataset");
            storage
        }

        fn make_queries(seed: u32) -> Vec<Vec<f32>> {
            let mut rng = Xorshift32(seed | 1);
            (0..QUERY_COUNT)
                .map(|_| make_vector(&mut rng, DIM))
                .collect()
        }

        // 対象ビヘイビア: TABLE-8。「コールドスタート時に一度だけアリーナを構築し、以降の
        // クエリはアリーナ走査で完結する経路」が「クエリの都度 Storage::scan で全行を
        // 読み直しデコードする経路」より十分速いことを、判定閾値（RATIO_THRESHOLD_DENOM）で
        // 検証する。
        #[test]
        fn table8_arena_query_path_completes_within_ratio_threshold_of_rescan_path() {
            let path = unique_db_path("perf-dataset");
            let _cleanup = CleanupGuard(path.clone());
            let storage = seed_storage(&path);

            // ウォームアップ 1 回（ファイルシステムキャッシュ等の初回コストを計測から
            // 除外する。既存 perf テスト tests/incremental_write_perf.rs と同方針）。
            {
                let warmup_queries = make_queries(0xabad_1dea);
                let arena = VectorArena::build(&storage, TABLE_NAME).expect("warmup build arena");
                for q in &warmup_queries {
                    std::hint::black_box(best_score_over_arena(&arena, q));
                }
                for q in &warmup_queries {
                    std::hint::black_box(best_score_over_rescan(&storage, q));
                }
            }

            let mut arena_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);
            let mut rescan_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);

            for round in 0..MEASUREMENT_ROUNDS as u32 {
                let queries = make_queries(0x9e37_79b9u32.wrapping_add(round));

                // 経路 (a): コールドスタート・アリーナを一度 build し、各クエリはアリーナ
                // 走査で完結する。build 自体もこの経路のコストとして計測に含める
                // （都度読み直し経路の各クエリが redb からの読み直しコストを含むのと
                // 対称にするため）。
                let started = Instant::now();
                let arena =
                    VectorArena::build(&storage, TABLE_NAME).expect("build arena (measured)");
                for q in &queries {
                    std::hint::black_box(best_score_over_arena(&arena, q));
                }
                arena_durations.push(started.elapsed());

                // 経路 (b): 各クエリごとに Storage::scan() で全行を読み直しデコードする。
                let started = Instant::now();
                for q in &queries {
                    std::hint::black_box(best_score_over_rescan(&storage, q));
                }
                rescan_durations.push(started.elapsed());
            }

            let t_arena = median(arena_durations);
            let t_rescan = median(rescan_durations);
            let ratio = t_arena.as_secs_f64() / t_rescan.as_secs_f64().max(f64::EPSILON);

            // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
            println!(
                "table8 arena vs rescan perf: t_arena={t_arena:?} t_rescan={t_rescan:?} ratio={ratio:.4}"
            );

            assert!(
                t_arena.saturating_mul(RATIO_THRESHOLD_DENOM) <= t_rescan,
                "arena query path ({t_arena:?}) must complete within 1/{RATIO_THRESHOLD_DENOM} of the \
                 rescan path ({t_rescan:?}), ratio={ratio:.4}"
            );
        }
    }
}
