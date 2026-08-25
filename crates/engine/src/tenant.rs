//! 行単位テナント境界の行ストア統合層（TASK-89・対象ビヘイビア: TABLE-9, TABLE-11。
//! TASK-95・対象ビヘイビア: RECOVER-4 の書き込みガード API を追加）。
//!
//! `policy.rs::PolicyContext::is_visible` の単一照合パス（CORE-2）へすべての可視性
//! 判定を委譲し、本モジュール独自のテナント比較は持たない（security.md P0）。
//! 提供する API は大きく 2 系統:
//!
//! - 読み取り側（[`visible_rows`]・[`verify_hits`]）: 行ストア（`catalog.rs` のテーブル
//!   スコープ行 API）を安全な上限内で走査し、可視行だけを列挙・検証する統合層。
//!   呼び出し元は主に `tests/tenant_isolation.rs`（TABLE-11 の 200 試行 × 4 テナント
//!   巡回検証）で、独立に期待集合を算出するための参照実装として使う。
//! - 書き込み側（[`insert_row`]・[`update_row`]・[`delete_row`]）: `PolicyContext::is_owner`
//!   （書き込み認可の単一照合パス）による所有権判定を経由してのみ行ストアを変更する
//!   ガード API（RECOVER-4）。`crate::core::EngineCore` の薄い委譲メソッドを経由して
//!   wire 層が DML を行う唯一の入口として設計している。生の UPDATE/DELETE を
//!   `Storage` の公開 API として新設しない（ガードを迂回できる経路を増やさない）。
//!
//! ## 設計記録: テーブル単位の物理分離は本タスクのスコープ外
//!
//! テナント境界は本モジュールが提供する「行単位」の可視性フィルタ（`PolicyContext`
//! 経由）を主軸として MVP を構成し、テナントごとにテーブルを動的構築する物理分離は
//! 実装しない（対象ビヘイビア: TABLE-9。詳細は spec 側のポインタ参照）。将来
//! テーブル単位分離を検討する場合は、本モジュールの可視性フィルタと独立した設計判断
//! として扱うこと。

use redb::{ReadableTable, TableDefinition};

use crate::catalog::{
    require_table_schema_write, user_rows_table_name, validate_identifier, CatalogError,
};
use crate::policy::PolicyContext;
use crate::storage::{
    bump_generation_and_commit, decode_row_tenant_and_visibility, encode_row, Row, RowInput,
    Storage, StorageError,
};

/// 1 ページあたりの走査件数（`catalog.rs::Storage::scan_table_page` の内部上限
/// `MAX_SCAN_PAGE_LIMIT` と同じ桁）。
const PAGE_LIMIT: u32 = 10_000;

/// [`visible_rows`] が保持してよい可視行数の上限。無制限 `Vec` 確保を避ける
/// （coding-rust.md「長さフィールドは上限検証してからアロケーションに使う」対応）。
/// テーブル全体の総行数ではなく可視行数を上限にすることで、大量の不可視行を持つ
/// テーブルでも呼び出し元テナントの可視行数だけに比例した確保量に収まる。
const MAX_VISIBLE_ROWS: usize = 100_000;

/// [`visible_rows`] が 1 回の呼び出しで走査してよい総行数（可視・不可視を問わない）の
/// 上限。`MAX_VISIBLE_ROWS` は出力（確保量）を抑えるが、他テナントの不可視行を
/// 大量に格納したテーブルでは出力がほぼ増えないまま `next` が尽きるまで全ページの
/// デコード・`PolicyContext::is_visible` 評価が実行され、計算量 DoS 経路になる
/// （codex-review 指摘・PR #153）。総走査行数にも明示的な上限を設け、超過時は
/// 部分結果を返さず [`TenantError::TooManyRowsScanned`] で fail-closed に拒否する。
const MAX_SCANNED_ROWS: usize = 1_000_000;

/// [`visible_rows`]・[`verify_hits`] のエラー型。`Display`・`Debug`・
/// `std::error::Error::source` のいずれにもテナント ID・行 id・テーブル名を含めず、
/// 他テナントの存在情報を漏らさない（`rls.rs::RlsError` と同じ契約。security.md P0）。
/// `CatalogError` を内部に保持するが、識別子を含む詳細は外部へ一切露出しない
/// （下記 `Debug`・`Error::source` の手書き実装を参照）。
pub enum TenantError {
    /// [`crate::catalog`] 側のエラー（テーブル不存在・行破損・redb バックエンドエラー等）。
    Catalog(CatalogError),
    /// 可視行数が [`MAX_VISIBLE_ROWS`] を超えたため、走査を打ち切って fail-closed に
    /// 拒否した（部分的な結果を黙って返さない）。
    TooManyVisibleRows { max: usize },
    /// 総走査行数（可視・不可視を問わない）が [`MAX_SCANNED_ROWS`] を超えたため、
    /// 走査を打ち切って fail-closed に拒否した。大量の不可視行を持つテーブルに対する
    /// 計算量 DoS（出力は増えないまま全ページのデコード・ポリシー評価を強制される
    /// 経路）を防ぐ（security.md テナント境界 P0。codex-review 指摘・PR #153）。
    TooManyRowsScanned { max: usize },
    /// [`verify_hits`] に渡された id が、走査対象テーブルの可視行集合に含まれない
    /// （不可視行・捏造 id のいずれも区別せず本 variant に統一する。他テナントの
    /// 存在情報を漏らさないため。security.md P0）。
    HitOutsideVisibleSet,
}

impl std::fmt::Display for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `CatalogError` の `Display`（`TableNotFound` のテーブル名・`RowNotFound` の
            // 行 ID を含む）をそのまま展開しない。認可前の呼び出し・エラーログ経由で
            // 他テナントの存在情報が漏れるのを防ぐため、識別子・バックエンド詳細を含まない
            // 固定文言に丸める（security.md テナント境界 P0）。原因の詳細は本型の外へは
            // 一切公開しない（`Debug`・`Error::source` も同様にサニタイズ済み。内部診断が
            // 必要な場合は本型を経由しない別経路を用意すること）。
            TenantError::Catalog(_) => write!(f, "tenant boundary catalog error"),
            TenantError::TooManyVisibleRows { max } => {
                write!(f, "too many visible rows: limit={max}")
            }
            TenantError::TooManyRowsScanned { max } => {
                write!(f, "too many rows scanned: limit={max}")
            }
            TenantError::HitOutsideVisibleSet => {
                write!(f, "hit id is outside the policy-visible row set")
            }
        }
    }
}

impl std::fmt::Debug for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `#[derive(Debug)]` は `CatalogError`（`TableNotFound` のテーブル名・
        // `RowNotFound` の行 ID 等）をそのまま展開してしまい、`Display` で
        // 隠した情報がパニック出力・`{:?}` ログ経由で再露出する（security.md
        // テナント境界 P0）。variant 名のみを出力し、内部の識別情報は含めない。
        match self {
            TenantError::Catalog(_) => f.write_str("Catalog(<redacted>)"),
            TenantError::TooManyVisibleRows { max } => f
                .debug_struct("TooManyVisibleRows")
                .field("max", max)
                .finish(),
            TenantError::TooManyRowsScanned { max } => f
                .debug_struct("TooManyRowsScanned")
                .field("max", max)
                .finish(),
            TenantError::HitOutsideVisibleSet => f.write_str("HitOutsideVisibleSet"),
        }
    }
}

impl std::error::Error for TenantError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `CatalogError` をそのまま `source()` で返すと、`Display` で固定文言に
        // 丸めた識別情報（テーブル名・行 ID 等）が一般的なエラーチェーン出力
        // （`anyhow` 等の `{:#}` 展開・ログ収集基盤）経由で再露出する
        // （security.md テナント境界 P0）。原因チェーンはここで打ち切り、常に
        // `None` を返す。
        None
    }
}

impl From<CatalogError> for TenantError {
    fn from(e: CatalogError) -> Self {
        TenantError::Catalog(e)
    }
}

/// `table` の全行を上限付きページング（`Storage::scan_table_page`）で走査し、`ctx`
/// （[`PolicyContext::is_visible`]）が可視と判定する行だけを列挙する（TABLE-9・
/// TABLE-11 の参照実装）。
///
/// 可視行数が [`MAX_VISIBLE_ROWS`] を超える場合は部分結果を返さず
/// [`TenantError::TooManyVisibleRows`] で拒否する。総走査行数（可視・不可視を
/// 問わない）が [`MAX_SCANNED_ROWS`] を超える場合も同様に部分結果を返さず
/// [`TenantError::TooManyRowsScanned`] で拒否する（他テナントの不可視行を大量に
/// 格納したテーブルに対する計算量 DoS を防ぐ。security.md テナント境界 P0）。
/// テーブル不存在は [`CatalogError::TableNotFound`] のまま [`TenantError::Catalog`]
/// へ伝播する（存在情報の扱いは呼び出し元の責務）。
pub fn visible_rows(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
) -> Result<Vec<Row>, TenantError> {
    let mut out = Vec::new();
    let mut after: Option<u64> = None;
    let mut scanned: usize = 0;
    loop {
        let (page, next) = storage.scan_table_page(table, after, PAGE_LIMIT)?;
        if page.is_empty() && next.is_none() {
            break;
        }
        scanned = scanned.saturating_add(page.len());
        if scanned > MAX_SCANNED_ROWS {
            return Err(TenantError::TooManyRowsScanned {
                max: MAX_SCANNED_ROWS,
            });
        }
        for row in page {
            if ctx.is_visible(&row.tenant_id, row.visibility) {
                if out.len() >= MAX_VISIBLE_ROWS {
                    return Err(TenantError::TooManyVisibleRows {
                        max: MAX_VISIBLE_ROWS,
                    });
                }
                out.push(row);
            }
        }
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(out)
}

/// 検索結果の id 集合 `hits` が、`table` に対する `ctx` の可視集合へすべて収まって
/// いることを fail-closed に検証する（TABLE-11: 200 試行 × 4 テナント巡回検証の
/// 混入 0 件アサーションを、`EngineCore::search`/`PrefilterIndex::search` の内部実装と
/// 独立した経路で裏付けるためのヘルパ）。
///
/// 1 件でも可視集合外の id があれば、走査を打ち切り即座に
/// [`TenantError::HitOutsideVisibleSet`] を返す。
pub fn verify_hits(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    hits: &[u64],
) -> Result<(), TenantError> {
    let visible = visible_rows(storage, table, ctx)?;
    let visible_ids: std::collections::HashSet<u64> = visible.iter().map(|r| r.id).collect();
    if hits.iter().all(|id| visible_ids.contains(id)) {
        Ok(())
    } else {
        Err(TenantError::HitOutsideVisibleSet)
    }
}

/// [`insert_row`]・[`update_row`]・[`delete_row`] のエラー型（TASK-95・対象ビヘイビア:
/// RECOVER-4）。`Display`・`Debug`・`std::error::Error::source` のいずれにもテナント ID・
/// 行 id・テーブル名を含めず、他テナントの存在情報を漏らさない（[`TenantError`] と同じ
/// 契約。security.md P0）。
pub enum TenantWriteError {
    /// 呼び出し元が入力した `RowInput::tenant_id` が `ctx` のテナントと不一致
    /// （クライアント自身の入力に起因するため存在情報を含まない）。他テナント名義の
    /// 新規行の書き込み・自テナント行の他テナントへの付け替え試行の両方がここに入る。
    Forbidden,
    /// UPDATE/DELETE 対象行が不存在、または `ctx` が所有しない行（区別しない。
    /// 存在情報を漏らさないため fail-closed に統一する。security.md P0）。
    NotFound,
    /// INSERT 先 id に既存行がある（所有者を問わず同一 variant。上書きによる他テナント
    /// 行の破壊を遮断しつつ、所有テナントの存在情報を漏らさない）。
    IdConflict,
    /// [`crate::catalog`] 側のエラー（テーブル不存在・行破損・redb バックエンドエラー等）。
    Catalog(CatalogError),
    /// [`crate::storage`] 側のエンコード/デコードエラー（`RowInput` の入力検証失敗等）。
    Storage(StorageError),
}

impl TenantWriteError {
    /// SQLSTATE 風 `wire_code`（coding-rust.md「エラー型は SQLSTATE 風 wire_code の設計に
    /// 従う」）。対象ビヘイビア: RECOVER-4・ERR-2（`docs/spec/04-behavior/error-format.md`
    /// をポインタ参照。写像の具体値・採用理由は spec 側の管理事項であり、本コメントへは
    /// 転記しない。spec-confidentiality.md 参照）。
    pub fn wire_code(&self) -> &'static str {
        match self {
            TenantWriteError::Forbidden => "42501",
            TenantWriteError::NotFound => "P0002",
            TenantWriteError::IdConflict => "23505",
            TenantWriteError::Catalog(_) | TenantWriteError::Storage(_) => "XX000",
        }
    }
}

impl std::fmt::Display for TenantWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TenantWriteError::Forbidden => {
                write!(f, "tenant write forbidden: not the row owner")
            }
            TenantWriteError::NotFound => write!(f, "tenant write target row not found"),
            TenantWriteError::IdConflict => write!(f, "tenant write id conflict"),
            // `CatalogError`/`StorageError` の `Display` をそのまま展開しない（`TenantError`
            // と同じ理由。security.md テナント境界 P0）。
            TenantWriteError::Catalog(_) => write!(f, "tenant write catalog error"),
            TenantWriteError::Storage(_) => write!(f, "tenant write storage error"),
        }
    }
}

impl std::fmt::Debug for TenantWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `#[derive(Debug)]` は内部の `CatalogError`/`StorageError` をそのまま展開し、
        // `Display` で隠した情報がパニック出力・`{:?}` ログ経由で再露出する
        // （security.md テナント境界 P0）。variant 名のみを出力する。
        match self {
            TenantWriteError::Forbidden => f.write_str("Forbidden"),
            TenantWriteError::NotFound => f.write_str("NotFound"),
            TenantWriteError::IdConflict => f.write_str("IdConflict"),
            TenantWriteError::Catalog(_) => f.write_str("Catalog(<redacted>)"),
            TenantWriteError::Storage(_) => f.write_str("Storage(<redacted>)"),
        }
    }
}

impl std::error::Error for TenantWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `TenantError::source` と同じ理由で原因チェーンをここで打ち切る
        // （security.md テナント境界 P0）。
        None
    }
}

impl From<CatalogError> for TenantWriteError {
    fn from(e: CatalogError) -> Self {
        TenantWriteError::Catalog(e)
    }
}

impl From<StorageError> for TenantWriteError {
    fn from(e: StorageError) -> Self {
        TenantWriteError::Storage(e)
    }
}

/// `table` へ新規行を 1 件挿入する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// `row.tenant_id` が `ctx` のテナントと不一致なら
/// [`TenantWriteError::Forbidden`]（他テナント名義での新規行書き込み・テナント
/// 付け替えの試行を遮断。判定は [`PolicyContext::is_owner`] の単一照合パス経由）。
/// 挿入先 `id` に既存行がある場合は、その行の所有者を問わず
/// [`TenantWriteError::IdConflict`]（上書きによる他テナント行の破壊を防ぎつつ、
/// 所有テナントの存在情報を漏らさない）。
///
/// スキーマ取得・次元検証・所有権判定・書き込みを単一の write トランザクション内で
/// 行い、失敗時は commit せずトランザクションを破棄する（`redb::WriteTransaction` の
/// drop 契約により abort。判定と書き込みの間に TOCTOU を作らない。redb は単一
/// ライタで書き込みを直列化する）。
pub fn insert_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    // ストレージへ触れる前に、クライアント自己申告の `tenant_id` を ctx と照合する
    // （security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。
    if !ctx.is_owner(row.tenant_id) {
        return Err(TenantWriteError::Forbidden);
    }
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        schema.validate_embedding_dim(row.embedding.len())?;
        let row_table_name = user_rows_table_name(table);
        let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
        let mut row_table = write_txn
            .open_table(row_table_def)
            .map_err(CatalogError::from)?;
        if row_table.get(id).map_err(CatalogError::from)?.is_some() {
            return Err(TenantWriteError::IdConflict);
        }
        let encoded = encode_row(row)?;
        row_table
            .insert(id, encoded.as_slice())
            .map_err(CatalogError::from)?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

/// `table` の既存行を 1 件更新する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// `row.tenant_id` が `ctx` のテナントと不一致なら
/// [`TenantWriteError::Forbidden`]（自テナント行を他テナントへ付け替える試行を含む）。
/// 対象行が不存在、または既存行の所有者が `ctx` と一致しない場合は
/// **区別せず** [`TenantWriteError::NotFound`]（他テナントの存在情報を漏らさない。
/// security.md P0）。
///
/// スキーマ取得・次元検証・既存行の所有権判定・書き込みを単一の write トランザクション
/// 内で行う（[`insert_row`] と同じ TOCTOU 対策）。
pub fn update_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    row: &RowInput<'_>,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    if !ctx.is_owner(row.tenant_id) {
        return Err(TenantWriteError::Forbidden);
    }
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        let schema = require_table_schema_write(&write_txn, table)?;
        schema.validate_embedding_dim(row.embedding.len())?;
        let row_table_name = user_rows_table_name(table);
        let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
        let mut row_table = write_txn
            .open_table(row_table_def)
            .map_err(CatalogError::from)?;
        // `AccessGuard` の借用をこのブロック内に閉じ込め、後続の可変借用（`insert`）と
        // 衝突しないようにする。
        let owns_existing = match row_table.get(id).map_err(CatalogError::from)? {
            Some(guard) => {
                let (existing_tenant, _existing_visibility) =
                    decode_row_tenant_and_visibility(guard.value())?;
                ctx.is_owner(&existing_tenant)
            }
            None => false,
        };
        if !owns_existing {
            return Err(TenantWriteError::NotFound);
        }
        let encoded = encode_row(row)?;
        row_table
            .insert(id, encoded.as_slice())
            .map_err(CatalogError::from)?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

/// `table` の既存行を 1 件削除する（TASK-95・対象ビヘイビア: RECOVER-4）。
///
/// 対象行が不存在、または既存行の所有者が `ctx` と一致しない場合は
/// **区別せず** [`TenantWriteError::NotFound`]（[`update_row`] と同じ契約。
/// security.md P0）。
pub fn delete_row(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
) -> Result<(), TenantWriteError> {
    validate_identifier(table)?;
    let write_txn = storage.db().begin_write().map_err(CatalogError::from)?;
    {
        // 次元検証は不要だが、テーブル不存在の判定・並行 DDL との整合のため
        // `insert_row`/`update_row` と同じ前段を通す。
        require_table_schema_write(&write_txn, table)?;
        let row_table_name = user_rows_table_name(table);
        let row_table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&row_table_name);
        let mut row_table = write_txn
            .open_table(row_table_def)
            .map_err(CatalogError::from)?;
        let owns_existing = match row_table.get(id).map_err(CatalogError::from)? {
            Some(guard) => {
                let (existing_tenant, _existing_visibility) =
                    decode_row_tenant_and_visibility(guard.value())?;
                ctx.is_owner(&existing_tenant)
            }
            None => false,
        };
        if !owns_existing {
            return Err(TenantWriteError::NotFound);
        }
        row_table.remove(id).map_err(CatalogError::from)?;
    }
    bump_generation_and_commit(write_txn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::storage::{RowInput, Visibility};

    fn unique_db_path(label: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vector-db-engine-tenant-unit-{label}-{}-{seq}.redb",
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

    fn schema(table: &str) -> TableSchema {
        TableSchema::new(
            table,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        )
    }

    // 対象ビヘイビア: TABLE-9。
    #[test]
    fn visible_rows_includes_other_tenant_public_rows() {
        let path = unique_db_path("visible-public");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");
        storage
            .insert_rows_into_table(
                "docs",
                &[
                    (
                        1,
                        RowInput {
                            tenant_id: "tenant-a",
                            visibility: Visibility::Public,
                            embedding: &[1.0, 0.0],
                            metadata: &[],
                        },
                    ),
                    (
                        2,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Public,
                            embedding: &[0.0, 1.0],
                            metadata: &[],
                        },
                    ),
                    (
                        3,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Private,
                            embedding: &[1.0, 1.0],
                            metadata: &[],
                        },
                    ),
                ],
            )
            .expect("seed rows");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut rows = visible_rows(&storage, "docs", &ctx).expect("visible_rows ok");
        rows.sort_by_key(|r| r.id);
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2],
            "tenant-a ctx must see its own row and the other tenant's Public row, \
             but not the other tenant's Private row"
        );
    }

    // 対象ビヘイビア: TABLE-11。`verify_hits` は可視集合外の id を fail-closed に拒否する。
    #[test]
    fn verify_hits_rejects_id_outside_visible_set() {
        let path = unique_db_path("verify-hits");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");
        storage
            .insert_rows_into_table(
                "docs",
                &[
                    (
                        1,
                        RowInput {
                            tenant_id: "tenant-a",
                            visibility: Visibility::Public,
                            embedding: &[1.0, 0.0],
                            metadata: &[],
                        },
                    ),
                    (
                        2,
                        RowInput {
                            tenant_id: "tenant-b",
                            visibility: Visibility::Private,
                            embedding: &[0.0, 1.0],
                            metadata: &[],
                        },
                    ),
                ],
            )
            .expect("seed rows");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(verify_hits(&storage, "docs", &ctx, &[1]).is_ok());
        assert!(matches!(
            verify_hits(&storage, "docs", &ctx, &[1, 2]),
            Err(TenantError::HitOutsideVisibleSet)
        ));
    }

    // 対象ビヘイビア: RECOVER-4（負方向・生 API の到達範囲確認）。
    // `crate::catalog::Storage::insert_row_into_table` は codex-review P0 指摘
    // （PR #194）を受けて `pub(crate)` 化し、クレート外（`tests/` 配下の結合テスト・
    // wire-server 等）からは到達不能にした。この生 API は本モジュール内では
    // （例: 将来の移行ツール等で）引き続き参照しうるため、クレート内ユニットテストとして
    // 「テナント境界チェックを経由しない書き込みは実際に行を書き換える」ことを確認する。
    // 旧・結合テスト版（`tests/tenant_breach.rs::recover4_checker_detects_unguarded_mutation`）
    // は `pub(crate)` 化に伴いクレート外から呼べなくなったため、このユニットテストへ
    // 移設した。
    #[test]
    fn raw_insert_row_into_table_bypasses_tenant_guard() {
        let path = unique_db_path("raw-insert-bypass");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema("docs")).expect("create table");

        // ガード付き経路（`insert_row`）で tenant-b 名義の行を正規に投入する。
        let owner = PolicyContext::new("tenant-b").expect("valid tenant");
        insert_row(
            &storage,
            "docs",
            &owner,
            1,
            &RowInput {
                tenant_id: "tenant-b",
                visibility: Visibility::Public,
                embedding: &[1.0, 0.0],
                metadata: &[],
            },
        )
        .expect("seed tenant-b row via guarded path");

        // ガードを経由しない生の `Storage::insert_row_into_table`（`pub(crate)`）で
        // 同じ id を tenant-a 名義へ上書きできてしまうことを確認する（クレート内から
        // 到達可能である以上、この経路自体は塞がっていないことの記録。クレート外からの
        // 到達不能性が本対応の主眼）。
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[9.0, 9.0],
                    metadata: &[],
                },
            )
            .expect("unguarded write succeeds by construction");

        let after = storage
            .get_row_from_table("docs", 1)
            .expect("read back row");
        assert_eq!(after.tenant_id, "tenant-a");
    }
}
