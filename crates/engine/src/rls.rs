//! 事前フィルタ方式によるテナント境界の再利用可能インデックス
//! （TASK-133・対象ビヘイビア: RLS-1, RLS-2, RLS-3, RLS-4）。
//!
//! テナント境界の判定は [`crate::policy::PolicyContext::is_visible`] の単一照合パスに
//! 限定し、本モジュール独自のテナント比較を新設しない（security.md P0）。
//! [`PrefilterIndex::build`] は構築時に渡された `PolicyContext` の可視性述語で
//! [`crate::arena::VectorArena::build_filtered`] を呼び、可視行だけを保持する縮約ビューを
//! 作る。[`PrefilterIndex`] は構築時に束縛した `PolicyContext`・`Storage` 参照を保持し、
//! [`PrefilterIndex::search`] は呼び出し時の `PolicyContext` との完全一致（不一致は
//! [`RlsError::ContextMismatch`]）と、provider 呼び出し前のアリーナ全行の現在の
//! tenant/visibility 再検証（構築時との不一致は [`RlsError::IndexStale`]）を fail-closed に
//! 行う。契約の詳細は [`PrefilterIndex::search`] のドキュメント参照。
//!
//! `core.rs::EngineCore`（`VectorCore::search`）への prefilter インデックスのキャッシュ
//! 統合・API 変更は本タスクのスコープ外（`VectorCore` trait のシグネチャは変更しない）。

use std::collections::HashSet;

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::CatalogError;
use crate::core::{provider_result_is_valid, validate_search_k, MAX_SEARCH_K};
use crate::kernel::{KernelError, SearchHit, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::storage::Storage;

/// [`PrefilterIndex`] のエラー型。`core.rs::CoreError` とおおむね対称の設計だが、
/// `Policy` は持たず（`PolicyContext` の構築時検証は呼び出し元の責務で本モジュールには
/// 到達しない）、[`RlsError::ContextMismatch`] は `core.rs` 側に対応がない
/// （`EngineCore::search` はクエリ毎にアリーナを再構築するため構築時 ctx と検索時 ctx の
/// 食い違いという状態自体が存在しない。本モジュール特有のインデックス再利用に伴う
/// エラー種別）。
#[derive(Debug)]
pub enum RlsError {
    Arena(ArenaError),
    Kernel(KernelError),
    /// `k == 0` または [`MAX_SEARCH_K`] 超過。
    InvalidK {
        k: usize,
    },
    /// 指定テーブルが存在しない（`core.rs::CoreError::NotFound` と同一契約: 不可視と
    /// 不存在を区別しない。`Display` へテーブル名を含めない。security.md P0）。
    NotFound,
    /// 検索時 `PolicyContext` が構築時と一致しない場合の fail-closed 拒否
    /// （`Display` はテナント ID・可視性集合を含まない。security.md P0）。
    ContextMismatch,
    /// provider 呼び出し前のアリーナ全行再検証で、構築時と現在の tenant/visibility が
    /// 一致しない行を検出した場合の fail-closed 拒否（`Display` は id・テナント ID を
    /// 含まない。呼び出し元は [`PrefilterIndex::build`] を呼び直すこと。security.md P0）。
    IndexStale,
    /// `SearchProvider` が返却した `Vec<`[`SearchHit`]`>` が Top-k の契約に違反した
    /// （`core.rs::CoreError::ProviderResultRejected` と同一契約。判定は共有ヘルパ
    /// `provider_result_is_valid` で行う。fail-closed: 違反があれば結果を一切返さない）。
    ProviderResultRejected,
}

impl std::fmt::Display for RlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RlsError::Arena(e) => write!(f, "rls prefilter arena error: {e}"),
            RlsError::Kernel(e) => write!(f, "rls prefilter kernel error: {e}"),
            RlsError::InvalidK { k } => {
                write!(f, "invalid k: {k} (must be 1..={MAX_SEARCH_K})")
            }
            RlsError::NotFound => write!(f, "not found"),
            RlsError::ContextMismatch => write!(
                f,
                "policy context does not match the context the index was built with"
            ),
            RlsError::IndexStale => write!(
                f,
                "prefilter index is stale: rebuild required before searching again"
            ),
            RlsError::ProviderResultRejected => write!(
                f,
                "search provider returned a hit outside the policy-visible id set"
            ),
        }
    }
}

impl std::error::Error for RlsError {}

impl From<ArenaError> for RlsError {
    fn from(e: ArenaError) -> Self {
        RlsError::Arena(e)
    }
}

impl From<KernelError> for RlsError {
    fn from(e: KernelError) -> Self {
        RlsError::Kernel(e)
    }
}

/// 事前フィルタ方式の再利用可能インデックス（TASK-133・RLS-1〜4）。
///
/// [`Self::build`] 構築時に束縛した [`PolicyContext`] の可視性述語で
/// [`VectorArena::build_filtered`] を呼び、可視行のみのカラムナ表現を保持する。
/// [`Self::search`] の契約は同メソッドのドキュメント参照。
///
/// [`Self::len`]・[`Self::is_empty`] は可視行数・行の有無という存在情報を返すため、
/// [`Self::built_ctx`] との完全一致を要求する（不一致は [`RlsError::ContextMismatch`]）。
/// [`Self::dim`]・[`Self::table_name`] は非機微情報のため `ctx` を要求しない。
pub struct PrefilterIndex<'s> {
    arena: VectorArena,
    /// [`Self::build`] に渡された `&Storage`（[`Self::search`] へは渡さず、この参照のみ
    /// 使う）。
    storage: &'s Storage,
    /// `arena.ids()` の `HashSet` キャッシュ（provider 結果の可視性検証に使う）。
    visible_id_set: HashSet<u64>,
    /// [`Self::build`] に渡された `PolicyContext` の複製。`ctx` 照合ゲートに使う。
    built_ctx: PolicyContext,
}

impl<'s> PrefilterIndex<'s> {
    /// `table` に対し `ctx` の可視性述語で可視行のみのインデックスを構築する（RLS-1）。
    /// テーブル不存在は [`RlsError::NotFound`] へ丸め込む（存在情報を漏らさない。
    /// security.md P0）。容量超過・次元不整合は [`RlsError::Arena`] へ伝播する。
    pub fn build(storage: &'s Storage, table: &str, ctx: &PolicyContext) -> Result<Self, RlsError> {
        let arena = match VectorArena::build_filtered(storage, table, |tenant, visibility| {
            ctx.is_visible(tenant, visibility)
        }) {
            Ok(arena) => arena,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(RlsError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };
        let visible_id_set: HashSet<u64> = arena.ids().iter().copied().collect();
        Ok(Self {
            arena,
            storage,
            visible_id_set,
            built_ctx: ctx.clone(),
        })
    }

    /// 保持済みインデックスに対して Top-k 検索を行う（over-fetch なし・RLS-3）。
    ///
    /// `ctx` は [`Self::build`] 時に束縛した `PolicyContext` と完全一致していなければ
    /// [`RlsError::ContextMismatch`] で fail-closed に拒否する。`k`・`query` の検証
    /// （`core.rs::EngineCore::search` と同一契約）の後、provider を呼ぶ**前**に
    /// アリーナ全行の現在の `tenant_id`・`visibility` を再取得し、構築時にアリーナへ
    /// 格納した値と厳密に一致するか検証する。1 件でも不一致・不存在があれば provider を
    /// 呼ばずに [`RlsError::IndexStale`] で拒否する（呼び出し元は [`Self::build`] を
    /// 呼び直すこと）。全件一致後に provider を 1 回呼び、戻り値を
    /// `provider_result_is_valid`（`core.rs`）で検証する（違反は
    /// [`RlsError::ProviderResultRejected`]）。返却結果はこの再検証時点のストレージ
    /// スナップショットに対して一貫する。TASK-133・RLS-1〜4 参照。
    pub fn search(
        &self,
        ctx: &PolicyContext,
        provider: &dyn SearchProvider,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        validate_search_k(k).map_err(|k| RlsError::InvalidK { k })?;

        if query.len() != self.arena.dim() as usize {
            return Err(RlsError::Kernel(KernelError::DimMismatch {
                expected: self.arena.dim(),
                found: query.len(),
            }));
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(RlsError::Kernel(KernelError::NonFiniteQuery));
        }

        // 失効行の全件事前検証（上記ドキュメント参照）。`arena_ids` は構築時に確定済みの
        // 信頼できる id 一覧で、provider・呼び出し元の入力を経由しない。`CatalogError` は
        // 種別を区別せず一律 `IndexStale` に丸め込む（fail-closed。他テナントの存在情報も
        // 含めない。security.md P0）。
        let arena_ids = self.arena.ids();
        let headers = self
            .storage
            .get_row_headers_from_table(self.arena.table_name(), arena_ids)
            .map_err(|_| RlsError::IndexStale)?;
        if headers.len() != arena_ids.len() {
            return Err(RlsError::IndexStale);
        }
        for (index, header) in headers.iter().enumerate() {
            let Some((current_tenant, current_visibility)) = header else {
                // 構築時には存在した行が検索時点では存在しない（削除相当）。
                return Err(RlsError::IndexStale);
            };
            // 構築時アリーナが保持する tenant_id・visibility との厳密な等値比較
            // （`ctx.is_visible` の再評価ではない）。
            let built_tenant = self.arena.tenant_id(index);
            let built_visibility = self.arena.visibility(index);
            if built_tenant != Some(current_tenant.as_str())
                || built_visibility != Some(*current_visibility)
            {
                return Err(RlsError::IndexStale);
            }
        }

        // 上記の全件検証を通過したため、`ids`/`vectors` をそのまま provider へ渡せる
        // （不可視・失効データは provider のアドレス空間へ渡らない）。
        let input = SearchInput {
            ids: self.arena.ids(),
            vectors: self.arena.vectors(),
            dim: self.arena.dim(),
            query,
            k,
        };
        let hits = provider.search(input)?;

        if !provider_result_is_valid(&hits, k, &self.visible_id_set) {
            return Err(RlsError::ProviderResultRejected);
        }

        Ok(hits)
    }

    /// インデックスが保持する可視行数を返す。`ctx` は構築時 `PolicyContext` と完全一致
    /// していなければ [`RlsError::ContextMismatch`]（存在情報を漏らさない。security.md P0）。
    pub fn len(&self, ctx: &PolicyContext) -> Result<usize, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.len())
    }

    /// 可視行が 0 件かを返す（`ctx` 照合は [`Self::len`] と同じ）。
    pub fn is_empty(&self, ctx: &PolicyContext) -> Result<bool, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.is_empty())
    }

    /// 検索対象ベクトルの次元（`ctx` 不要。テーブル定義由来の非機微情報）。
    pub fn dim(&self) -> u32 {
        self.arena.dim()
    }

    /// 構築元のテーブル名（`ctx` 不要。呼び出し元が渡した引数の反映）。
    pub fn table_name(&self) -> &str {
        self.arena.table_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::kernel::CpuScalarProvider;
    use crate::storage::{RowInput, Visibility};

    fn schema_for(table_name: &str, dim: u32) -> TableSchema {
        TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    // 簡易テンポラリディレクトリ（外部クレート非依存。dependency-policy 準拠。
    // `core.rs::tests::TempDir` と同型の複製）。
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    // プロセス内グローバル通番。`SystemTime::now()` の実測分解能はプラットフォームにより
    // ナノ秒より粗いため、同一 tick で並行実行された複数スレッドが `duration_since` の値だけで
    // 一時ディレクトリ名を組み立てると衝突しうる（`storage.rs::tests::unique_db_path`・
    // `arena.rs` の同種ヘルパーと同じ `SEQ.fetch_add` 対策。並列テスト実行時の
    // `DatabaseAlreadyOpen` フレーク回避）。
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tempdir() -> TempDir {
        let mut dir = std::env::temp_dir();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unique = format!(
            "engine-rls-test-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        dir.push(unique);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn open_storage(dir: &std::path::Path) -> Storage {
        Storage::open(dir.join("db.redb")).expect("open storage")
    }

    fn insert(storage: &Storage, table: &str, id: u64, tenant: &str, vis: Visibility, v: &[f32]) {
        storage
            .insert_row_into_table(
                table,
                id,
                &RowInput {
                    tenant_id: tenant,
                    visibility: vis,
                    embedding: v,
                    metadata: &[],
                },
            )
            .expect("insert row");
    }

    // 対象ビヘイビア: RLS-1。他テナント・不可視行は構築時点でインデックスへ含まれず、
    // 検索結果にも混入しない。
    #[test]
    fn build_excludes_invisible_rows_and_search_never_returns_them() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");
        assert_eq!(index.len(&ctx).expect("len ok"), 1);

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1。可視行が 0 件でも空結果を返す（拒否ではない）。
    #[test]
    fn empty_visible_set_returns_empty_result() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");
        assert!(index.is_empty(&ctx).expect("is_empty ok"));

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 5)
            .expect("search ok");
        assert!(hits.is_empty());
    }

    // `core.rs::EngineCore::search`/`get_row` と対称: テーブル不存在は他テナントの
    // 存在情報を漏らさず `RlsError::NotFound` へ丸め込まれ、`Display` にテーブル名を
    // 含まない（本 Issue のレビュー指摘対応）。
    #[test]
    fn build_returns_not_found_without_leaking_table_name_for_missing_table() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let result = PrefilterIndex::build(&storage, "no_such_table", &ctx);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("missing table must be rejected"),
        };
        assert!(matches!(err, RlsError::NotFound));
        assert_eq!(err.to_string(), "not found");
        assert!(!err.to_string().contains("no_such_table"));
    }

    #[test]
    fn search_rejects_k_zero_and_over_limit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 0),
            Err(RlsError::InvalidK { k: 0 })
        ));
        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], MAX_SEARCH_K + 1),
            Err(RlsError::InvalidK { .. })
        ));
    }

    #[test]
    fn search_rejects_dim_mismatch_and_non_finite_query() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0, 0.0], 1),
            Err(RlsError::Kernel(KernelError::DimMismatch { .. }))
        ));
        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[f32::NAN, 0.0], 1),
            Err(RlsError::Kernel(KernelError::NonFiniteQuery))
        ));
    }

    // ctx 束縛の検証: 同一テーブルでも構築時 ctx のテナントに紐づく行しか返らない
    // （一致 ctx での正常系）。
    #[test]
    fn index_is_bound_to_the_tenant_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");
        let hits = index_a
            .search(&ctx_a, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert!(hits.iter().all(|h| h.id == 1));

        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let index_b = PrefilterIndex::build(&storage, "docs", &ctx_b).expect("build index");
        let hits = index_b
            .search(&ctx_b, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert!(hits.iter().all(|h| h.id == 2));
    }

    // 構築時とは別テナントの ctx でインデックスを転用しようとした場合、可視行が
    // 存在していても `RlsError::ContextMismatch` で拒否される。
    #[test]
    fn search_rejects_a_different_tenant_context_than_the_one_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");

        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let result = index_a.search(&ctx_b, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::ContextMismatch)));
    }

    // 構築時とは許可可視性集合が狭い ctx（Private 許可の取り消し）は転用とみなし拒否する。
    #[test]
    fn search_rejects_a_context_narrowed_from_build_time_visibility() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx_private =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx_private).expect("build index");

        let ctx_narrowed = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(matches!(
            index.search(&ctx_narrowed, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::ContextMismatch)
        ));

        // 構築時と完全一致する ctx（別インスタンスだが値は等しい）は受理される。
        let ctx_same =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert_eq!(ctx_same, ctx_private);
        let hits = index
            .search(&ctx_same, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("identical context must be accepted");
        assert_eq!(hits.len(), 1);
    }

    // 構築時よりも許可可視性集合が広い ctx も転用とみなし拒否する。
    #[test]
    fn search_rejects_a_context_widened_from_build_time_visibility() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_public_only = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx_public_only).expect("build index");

        let ctx_widened =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert!(matches!(
            index.search(&ctx_widened, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::ContextMismatch)
        ));
    }

    // 不正 provider（可視集合外の id を捏造して返す）は fail-closed に拒否される
    // （`core.rs::CoreError::ProviderResultRejected` と同一契約の再現）。
    struct RogueProvider;
    impl SearchProvider for RogueProvider {
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
            Ok(vec![SearchHit {
                id: 9_999_999,
                score: 1.0,
            }])
        }
    }

    #[test]
    fn rogue_provider_result_outside_visible_set_is_rejected() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let result = index.search(&ctx, &RogueProvider, &[1.0, 0.0], 1);
        assert!(matches!(result, Err(RlsError::ProviderResultRejected)));
    }

    // 構築後に行の tenant_id がストレージ側で書き換わった場合、同一 ctx で検索しても
    // 旧行を返さず `RlsError::IndexStale` で拒否する。
    #[test]
    fn search_rejects_when_a_hit_row_tenant_changed_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後に行 1 を他テナントへ書き換える（同一 id への upsert）。
        insert(
            &storage,
            "docs",
            1,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // tenant_id は変わらず visibility だけが構築時 ctx の許可範囲外へ書き換わった場合も
    // 同様に `RlsError::IndexStale` で拒否する。
    #[test]
    fn search_rejects_when_a_hit_row_visibility_changed_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後に行 1 の visibility を ctx が許可しない Private へ書き換える。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // 未変更のストレージに対しては引き続き正常にヒットを返すことを確認する
    // （再検証の追加で過剰拒否（over-rejection）を起こしていないことのガード）。
    #[test]
    fn search_still_returns_hits_when_storage_is_unchanged_since_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 行不存在（削除相当）の再検証パスは `catalog.rs::tests::` 側でカバーする
    // （`get_row_headers_from_table` の `None` 分岐）。

    // `len`/`is_empty` は構築時 ctx との一致を要求し、別テナントの ctx（tenant-b。
    // 自身の可視行を持つ）を渡した場合はどちらも `RlsError::ContextMismatch` になる。
    #[test]
    fn len_and_is_empty_reject_a_context_different_from_the_one_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");

        // 一致する ctx では引き続き正常に値を返す（過剰拒否のガード）。
        assert_eq!(index_a.len(&ctx_a).expect("len ok"), 1);
        assert!(!index_a.is_empty(&ctx_a).expect("is_empty ok"));

        // 別テナントの ctx（tenant-b。tenant-a のインデックスへ渡す）はどちらも拒否する。
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        assert!(matches!(
            index_a.len(&ctx_b),
            Err(RlsError::ContextMismatch)
        ));
        assert!(matches!(
            index_a.is_empty(&ctx_b),
            Err(RlsError::ContextMismatch)
        ));
    }

    /// 呼び出し回数を記録してから [`CpuScalarProvider`] へ委譲する計装 provider
    /// （`tests/rls_prefilter.rs::CountingProvider` と同型の複製）。
    struct RecordingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl RecordingProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl SearchProvider for RecordingProvider {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            CpuScalarProvider.search(input)
        }
    }

    // 失効した行が Top-k の外であっても provider は一切呼ばれないことを検証する。
    #[test]
    fn search_rejects_before_calling_provider_even_when_the_stale_row_is_outside_top_k() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // クエリ [1.0, 0.0] に対する内積スコア: id=1 が最高位（1.0）、id=2 が中位（0.5）、
        // id=3 が最下位（0.1）。k=1 なら id=3 は本来 Top-k に入らない。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Public,
            &[0.1, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後、Top-1 には入らない id=3 だけを他テナントへ書き換える。
        insert(
            &storage,
            "docs",
            3,
            "tenant-b",
            Visibility::Public,
            &[0.1, 0.0],
        );

        let provider = RecordingProvider::new();
        let result = index.search(&ctx, &provider, &[1.0, 0.0], 1);
        assert!(matches!(result, Err(RlsError::IndexStale)));
        assert_eq!(
            provider.call_count(),
            0,
            "provider must not be called once any arena row fails the pre-check"
        );
    }

    // ストレージが構築後に変化していない場合は provider がちょうど 1 回呼ばれる
    // （過剰拒否のガード）。
    #[test]
    fn search_calls_provider_exactly_once_when_no_row_is_stale() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let provider = RecordingProvider::new();
        let hits = index
            .search(&ctx, &provider, &[1.0, 0.0], 1)
            .expect("search ok");
        assert_eq!(provider.call_count(), 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }
}
