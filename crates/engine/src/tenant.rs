//! 行単位テナント境界の行ストア統合層（TASK-89・対象ビヘイビア: TABLE-9, TABLE-11）。
//!
//! `policy.rs::PolicyContext::is_visible` の単一照合パス（CORE-2）へすべての可視性
//! 判定を委譲し、本モジュール独自のテナント比較は持たない（security.md P0）。
//! 提供する API は「行ストア（`catalog.rs` のテーブルスコープ行 API）を安全な上限内で
//! 走査し、可視行だけを列挙・検証する」統合層のみ。呼び出し元は主に
//! `tests/tenant_isolation.rs`（TABLE-11 の 200 試行 × 4 テナント巡回検証）で、
//! 独立に期待集合を算出するための参照実装として使う。
//!
//! ## 設計記録: テーブル単位の物理分離は本タスクのスコープ外
//!
//! テナント境界は本モジュールが提供する「行単位」の可視性フィルタ（`PolicyContext`
//! 経由）を主軸として MVP を構成し、テナントごとにテーブルを動的構築する物理分離は
//! 実装しない（対象ビヘイビア: TABLE-9。詳細は spec 側のポインタ参照）。将来
//! テーブル単位分離を検討する場合は、本モジュールの可視性フィルタと独立した設計判断
//! として扱うこと。

use crate::catalog::CatalogError;
use crate::policy::PolicyContext;
use crate::storage::{Row, Storage};

/// 1 ページあたりの走査件数（`catalog.rs::Storage::scan_table_page` の内部上限
/// `MAX_SCAN_PAGE_LIMIT` と同じ桁）。
const PAGE_LIMIT: u32 = 10_000;

/// [`visible_rows`] が保持してよい可視行数の上限。無制限 `Vec` 確保を避ける
/// （coding-rust.md「長さフィールドは上限検証してからアロケーションに使う」対応）。
/// テーブル全体の総行数ではなく可視行数を上限にすることで、大量の不可視行を持つ
/// テーブルでも呼び出し元テナントの可視行数だけに比例した確保量に収まる。
const MAX_VISIBLE_ROWS: usize = 100_000;

/// [`visible_rows`]・[`verify_hits`] のエラー型。`Display` にテナント ID・行 id を含めず、
/// 他テナントの存在情報を漏らさない（`rls.rs::RlsError` と同じ契約。security.md P0）。
#[derive(Debug)]
pub enum TenantError {
    /// [`crate::catalog`] 側のエラー（テーブル不存在・行破損・redb バックエンドエラー等）。
    Catalog(CatalogError),
    /// 可視行数が [`MAX_VISIBLE_ROWS`] を超えたため、走査を打ち切って fail-closed に
    /// 拒否した（部分的な結果を黙って返さない）。
    TooManyVisibleRows { max: usize },
    /// [`verify_hits`] に渡された id が、走査対象テーブルの可視行集合に含まれない
    /// （不可視行・捏造 id のいずれも区別せず本 variant に統一する。他テナントの
    /// 存在情報を漏らさないため。security.md P0）。
    HitOutsideVisibleSet,
}

impl std::fmt::Display for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TenantError::Catalog(e) => write!(f, "tenant boundary catalog error: {e}"),
            TenantError::TooManyVisibleRows { max } => {
                write!(f, "too many visible rows: limit={max}")
            }
            TenantError::HitOutsideVisibleSet => {
                write!(f, "hit id is outside the policy-visible row set")
            }
        }
    }
}

impl std::error::Error for TenantError {}

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
/// [`TenantError::TooManyVisibleRows`] で拒否する。テーブル不存在は
/// [`CatalogError::TableNotFound`] のまま [`TenantError::Catalog`] へ伝播する
/// （存在情報の扱いは呼び出し元の責務）。
pub fn visible_rows(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
) -> Result<Vec<Row>, TenantError> {
    let mut out = Vec::new();
    let mut after: Option<u64> = None;
    loop {
        let (page, next) = storage.scan_table_page(table, after, PAGE_LIMIT)?;
        if page.is_empty() && next.is_none() {
            break;
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

    // 対象ビヘイビア: TABLE-9。他テナントの `Public` 行は可視行集合へ含まれる
    // （相互可視性）。
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
}
