//! 事前フィルタ方式によるテナント境界の再利用可能インデックス
//! （TASK-133・対象ビヘイビア: RLS-1, RLS-2, RLS-3, RLS-4）。
//!
//! `core.rs`（TASK-124）の `EngineCore::search` はクエリ 1 件ごとにアリーナを再構築するが、
//! 本モジュールは可視行の部分集合インデックスを構築して使い回し、後続クエリはその
//! 部分集合だけを総当たりスキャンする「事前フィルタ方式」を提供する。可視率・ポリシーが
//! 変わらない連続クエリ列（将来の SQL 実行経路・TASK-134 のフォールバック切り替え元を
//! 想定）でアリーナ再構築コストを避けるための構造体である。方式選定はポインタ:
//! `docs/spec/03-poc/rls-search-integration/`。
//!
//! テナント境界の判定は [`crate::policy::PolicyContext::is_visible`] の単一照合パスに
//! 限定し、本モジュール独自のテナント比較を新設しない（security.md P0）。
//! [`PrefilterIndex::build`] は構築時に渡された `PolicyContext` の可視性述語で
//! [`crate::arena::VectorArena::build_filtered`] を呼び、可視行だけを保持する縮約ビューを
//! 作る。以後の検索はこの構築時スナップショットに対してのみ行われ、構築後の書き込みは
//! 反映しない（[`PrefilterIndex::build`] のドキュメント参照）。`PrefilterIndex` は構築時に
//! 束縛した `PolicyContext` の複製（テナント ID・許可可視性集合）を保持し、
//! [`PrefilterIndex::search`] は呼び出し時に渡された `PolicyContext` とこの複製の完全一致
//! （`PartialEq`）を fail-closed に照合する。別テナント・可視性が狭化/拡大された ctx で
//! 同一インデックスを転用しようとした場合は [`RlsError::ContextMismatch`] で拒否する
//! （テナント境界 P0。codex-review P0 指摘・PR #151 対応: 以前は `search` が ctx を
//! 受け取らず、構築時 ctx との一致を検証していなかった）。
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
    /// 不存在を区別しない。[`PrefilterIndex::build`] が `ArenaError::Catalog`
    /// （`CatalogError::TableNotFound`）を捕捉してこの variant へ丸め込み、`Display` へ
    /// テーブル名を含めない。他テナントの存在情報を漏らさないため
    /// （security.md P0「エラー経由で存在情報を漏らさない」）。
    NotFound,
    /// [`PrefilterIndex::search`] に渡された `PolicyContext` が構築時に束縛した
    /// `PolicyContext` と一致しない（テナント ID・許可可視性集合のいずれかが異なる）。
    /// 別テナントへの転用・可視性の狭化/拡大のいずれも区別せず本 variant で fail-closed
    /// に拒否する（`Display` はテナント ID・可視性集合を含まない。テナント境界 P0・
    /// codex-review P0 指摘・PR #151 対応）。
    ContextMismatch,
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

/// 事前フィルタ方式の再利用可能インデックス（RLS-1〜4）。
///
/// [`Self::build`] 構築時に束縛した [`PolicyContext`] の可視性述語で
/// [`VectorArena::build_filtered`] を呼び、可視行のみのカラムナ表現を保持する。
/// 構築後の書き込みはこのインデックスへ反映されない（構築時点のスナップショット）。
/// 可視率・ポリシーが変わる場合は [`Self::build`] を呼び直して再構築する必要がある。
///
/// [`Self::len`]・[`Self::is_empty`]・[`Self::dim`]・[`Self::table_name`] は `ctx` を
/// 要求しない点に注意: これらは構築時 ctx のテナントに束縛されたメタデータ（可視行数・
/// テーブル名等）を返すため、[`Self::search`] と同様に「構築時 ctx を保持する呼び出し元
/// のみが扱ってよい」という前提の上で成立する。呼び出し元がインデックスを別テナントの
/// 文脈へ渡さない運用を守る必要がある（現状は `search` のみが fail-closed な照合ゲートを
/// 持つ。アクセサへの ctx 必須化は本 P0 のスコープ外・将来の検討事項）。
pub struct PrefilterIndex {
    arena: VectorArena,
    /// `arena.ids()` と同一集合の `HashSet` キャッシュ（[`Self::build`] 時に一度だけ構築）。
    /// [`Self::search`] は provider 結果の可視性再検証（`provider_result_is_valid`）で
    /// このキャッシュを使い回し、クエリ毎の再構築コストを避ける（本モジュールが解決対象と
    /// する「クエリ毎の前段コスト」をここで再生産しないため。モジュール doc 参照）。
    visible_id_set: HashSet<u64>,
    /// [`Self::build`] に渡された `PolicyContext` の複製（テナント ID・許可可視性集合）。
    /// [`Self::search`] はこの複製と呼び出し時の `PolicyContext` の完全一致を照合する
    /// ゲートとして使う（`is_visible` の追加呼び出しには使わない。可視性判定そのものは
    /// [`Self::build`] 時点の [`VectorArena::build_filtered`] で完結している）。
    /// codex-review P0 指摘・PR #151 対応: 転用・ポリシー失効の検出に必須（モジュール
    /// doc 参照）。
    built_ctx: PolicyContext,
}

impl PrefilterIndex {
    /// `table` に対し `ctx` の可視性述語で可視行のみのインデックスを構築する
    /// （事前フィルタ・RLS-1: 不可視行はこの構築時点でアリーナへ確保されない）。
    ///
    /// `ctx` は可視行の絞り込み述語として使うと同時に、[`Self::search`] での転用検出用に
    /// 複製して保持する（モジュールドキュメント参照）。テーブル不存在は
    /// `core.rs::EngineCore::search`/`get_row` と対称に [`RlsError::NotFound`] へ丸め込む
    /// （存在情報を漏らさない。security.md P0）。容量超過・次元不整合はそのまま
    /// [`RlsError::Arena`] へ伝播する（`VectorArena::build_filtered` の契約をそのまま
    /// 継承）。
    pub fn build(storage: &Storage, table: &str, ctx: &PolicyContext) -> Result<Self, RlsError> {
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
            visible_id_set,
            built_ctx: ctx.clone(),
        })
    }

    /// 保持済みインデックスに対して Top-k 検索を行う（over-fetch なし・RLS-3:
    /// 要求 `k` のまま provider を 1 回だけ呼び、追加フェッチを行わない）。
    ///
    /// `ctx` は [`Self::build`] 時に束縛した `PolicyContext` と完全一致
    /// （テナント ID・許可可視性集合の両方）していなければならない。一致しない場合は
    /// 別テナントへの転用・可視性の狭化/拡大のいずれであっても区別せず
    /// [`RlsError::ContextMismatch`] で fail-closed に拒否する（テナント境界 P0・
    /// codex-review P0 指摘・PR #151 対応。`batch_search.rs` が `PolicyContext` を
    /// クエリ引数として型レベルで要求する既存パターンと整合させ、`search` 単体で
    /// テナント境界の照合が完結する構造にする）。
    ///
    /// 一致後は `core.rs::EngineCore::search` と同一の前段検証（`k` の範囲・`query` の
    /// 次元/有限性）を行った上で provider を 1 回だけ呼び、戻り値を共有ヘルパ
    /// `provider_result_is_valid`（`core.rs`）で再検証する。provider は untrusted
    /// 実装でありうるため、1 件でも契約違反があれば結果を一切返さず
    /// [`RlsError::ProviderResultRejected`] で拒否する（fail-closed。`core.rs`
    /// モジュールドキュメントの二重防御と同じ設計）。
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

        // 保持済みアリーナは構築時点で可視行だけへ絞り込み済みのため、`ids`/`vectors` を
        // そのまま provider へ渡せる（不可視データは provider のアドレス空間へ渡らない。
        // `core.rs::EngineCore::search` と同じ境界）。
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

    /// インデックスが保持する可視行数。
    pub fn len(&self) -> usize {
        self.arena.len()
    }

    /// 可視行が 0 件か。
    pub fn is_empty(&self) -> bool {
        self.arena.is_empty()
    }

    /// 検索対象ベクトルの次元。
    pub fn dim(&self) -> u32 {
        self.arena.dim()
    }

    /// 構築元のテーブル名。
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
    fn tempdir() -> TempDir {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "engine-rls-test-{}-{}",
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
        assert_eq!(index.len(), 1);

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
        assert!(index.is_empty());

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

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 構築時とは別テナントの ctx
    // でインデックスを転用しようとした場合、可視行が存在していても
    // `RlsError::ContextMismatch` で fail-closed に拒否される（構築時可視行の内容に
    // 関わらず拒否されることを示すため、あえて `tenant-b` にも可視行を用意する）。
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

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 構築時とは許可可視性集合が
    // 狭い ctx（Private 許可の取り消し。構築後のポリシー失効を模す）は転用とみなし拒否する。
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

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 構築時よりも許可可視性集合が
    // 広い ctx（構築時に持たなかった Private 許可が事後付与された ctx）も転用とみなし
    // 拒否する。構築時インデックスは Public 行しか保持していないため
    // `VectorArena::build_filtered` の再絞り込みは行われず、拡大された許可がそのまま
    // 反映されてしまわないことを確認する。
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
}
