//! プロトコル非依存のコア API 層（TASK-124・対象ビヘイビア: CORE-1）。
//!
//! `wire-server`（pg wire v3）や他の将来プロトコル実装は、本モジュールが定義する
//! [`VectorCore`] trait のみに依存する。コア API の変更なしにプロトコル実装を
//! 追加・変更できることを構造で担保する（trait は object-safe。シグネチャ安定性の
//! CI 機械チェックは TASK-125 の範囲）。
//!
//! [`EngineCore`] が製品実装で、`storage.rs`（永続化）・`catalog.rs`（スキーマ）・
//! `arena.rs`（検索対象のカラムナビュー構築）・`kernel.rs`（実行バックエンド provider）・
//! `policy.rs`（テナント境界・可視性判定）を束ねる。テナント判定は必ず
//! [`crate::policy::PolicyContext::is_visible`] の単一照合パス経由で行い、
//! 不可視行と不存在行の応答を区別しない（存在情報を漏らさない。security.md 準拠）。
//!
//! `EngineCore::search` は [`SearchProvider`] を `Box<dyn SearchProvider>`（CORE-13）で
//! 差し替え可能にしているが、`SearchInput::is_visible` は「provider が呼び出す」規約に
//! すぎず、provider 実装がそれを無視したり、データセットに存在しない・不可視な行の
//! `SearchHit` を返したりすることをコンパイラは防げない。そのため `EngineCore::search` は
//! provider の戻り値をコア側の可視行 id 集合（`is_visible` をコア自身が全走査して構築する、
//! provider の自己申告に依存しない集合）と突き合わせて再検証し、逸脱があれば結果を一切
//! 返さず `CoreError::ProviderResultRejected` で拒否する（fail-closed。AGENTS.md P0
//! 「テナント境界の弱体化」対応。テナント分離を provider 実装の正しさに依存させない）。

use std::collections::HashSet;
use std::path::Path;

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::CatalogError;
use crate::kernel::{CpuScalarProvider, KernelError, SearchHit, SearchInput, SearchProvider};
use crate::policy::{PolicyContext, PolicyError};
use crate::storage::{Row, Storage, StorageError};

/// 検索 `k` の上限。上限検証前にアロケーションへ使わないための防御的定数
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。`catalog.rs::MAX_LIST_TABLES`
/// と同程度の桁に揃える）。
const MAX_SEARCH_K: usize = 10_000;

/// `VectorCore` 公開 API のエラー型。下位層（`storage`/`catalog`/`arena`/`kernel`/`policy`）
/// のエラーを一本化しつつ、不可視行と不存在行を [`CoreError::NotFound`] に統合する
/// （呼び出し元へ存在情報を漏らさないため。エラーメッセージはプログラム出力文字列のため英語）。
#[derive(Debug)]
pub enum CoreError {
    Storage(StorageError),
    Catalog(CatalogError),
    Arena(ArenaError),
    Kernel(KernelError),
    Policy(PolicyError),
    /// `k == 0` または [`MAX_SEARCH_K`] 超過。
    InvalidK {
        k: usize,
    },
    /// 指定行が存在しない、または呼び出し元のテナント・可視性から見えない
    /// （区別しない。fail-closed）。
    NotFound,
    /// `SearchProvider` が返却した [`SearchHit`] のうち少なくとも 1 件が、コア側で
    /// 計算した可視行 id 集合（`ctx` の下で可視な、対象テーブル実在行の id 集合）に
    /// 含まれていなかった。provider 実装が `SearchInput::is_visible` を無視した・
    /// データセットに存在しない id を捏造した、のいずれの場合も区別せず拒否する
    /// （fail-closed。他テナントの存在情報を漏らさないよう具体的な id は含めない）。
    ProviderResultRejected,
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Storage(e) => write!(f, "core storage error: {e}"),
            CoreError::Catalog(e) => write!(f, "core catalog error: {e}"),
            CoreError::Arena(e) => write!(f, "core arena error: {e}"),
            CoreError::Kernel(e) => write!(f, "core kernel error: {e}"),
            CoreError::Policy(e) => write!(f, "core policy error: {e}"),
            CoreError::InvalidK { k } => write!(f, "invalid k: {k} (must be 1..={MAX_SEARCH_K})"),
            CoreError::NotFound => write!(f, "not found"),
            CoreError::ProviderResultRejected => write!(
                f,
                "search provider returned a hit outside the policy-visible id set"
            ),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<StorageError> for CoreError {
    fn from(e: StorageError) -> Self {
        CoreError::Storage(e)
    }
}

impl From<CatalogError> for CoreError {
    fn from(e: CatalogError) -> Self {
        CoreError::Catalog(e)
    }
}

impl From<ArenaError> for CoreError {
    fn from(e: ArenaError) -> Self {
        CoreError::Arena(e)
    }
}

impl From<KernelError> for CoreError {
    fn from(e: KernelError) -> Self {
        CoreError::Kernel(e)
    }
}

impl From<PolicyError> for CoreError {
    fn from(e: PolicyError) -> Self {
        CoreError::Policy(e)
    }
}

/// wire-server（および将来の他プロトコル実装）が依存する唯一の窓口（CORE-1）。
/// 最小 API（policy 付き検索・行取得）のみを持ち、認証・SQL 表層は後続タスクで
/// 拡張する。object-safe を保つ（ジェネリクスなし）。
pub trait VectorCore: Send + Sync {
    /// `table` に対して `query` の Top-k 検索を行う。`ctx` のテナント・可視性を満たさない
    /// 行は結果に含めない（CORE-2）。
    fn search(
        &self,
        ctx: &PolicyContext,
        table: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, CoreError>;

    /// `table` から `id` の行を 1 件取得する。不可視・不存在は区別せず
    /// [`CoreError::NotFound`] を返す。
    fn get_row(&self, ctx: &PolicyContext, table: &str, id: u64) -> Result<Row, CoreError>;
}

/// `VectorCore` の製品実装。永続化・カタログ・アリーナ構築・検索 provider を束ねる。
///
/// 実行バックエンド実装型へ直接依存せず `Box<dyn SearchProvider>` で保持する（CORE-13）。
/// 既定コンストラクタ（[`Self::open`]）は CPU-only の [`CpuScalarProvider`] を注入し、
/// この構成だけで全機能が成立する。
pub struct EngineCore {
    storage: Storage,
    provider: Box<dyn SearchProvider>,
}

impl EngineCore {
    /// 指定パスの `redb` データベースを開き、既定の CPU provider を注入した
    /// `EngineCore` を構築する。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        Self::with_provider(path, Box::new(CpuScalarProvider))
    }

    /// 検索 provider を差し替えて構築する（テスト・将来の GPU/ANN provider 導入用）。
    pub fn with_provider(
        path: impl AsRef<Path>,
        provider: Box<dyn SearchProvider>,
    ) -> Result<Self, CoreError> {
        let storage = Storage::open(path)?;
        Ok(Self { storage, provider })
    }

    /// 直接 `Storage` から構築する（テスト用途。呼び出し元が既に開いたハンドルを
    /// 再利用したい場合に使う）。`test-support` feature 限定（下記 [`Self::storage`] 参照）。
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_storage(storage: Storage, provider: Box<dyn SearchProvider>) -> Self {
        Self { storage, provider }
    }

    /// 保持している永続化ハンドルへの参照（テスト用途限定）。
    ///
    /// `Storage` はテナント境界を判定しない生ハンドルであり、[`VectorCore::get_row`]・
    /// [`VectorCore::search`] が経由する [`crate::policy::PolicyContext::is_visible`] の
    /// 単一照合パスを迂回できる（security.md P0「テナント分離の検査を外す/緩める/
    /// バイパス経路を作らない」）。そのため通常ビルド（`wire-server` を含む）の公開面には
    /// 含めず、`test-support` feature 限定で `tests/` 配下の結合テストにのみ公開する
    /// （`Cargo.toml` の self dev-dependency 経由で結合テストビルド時のみ有効化）。
    #[cfg(any(test, feature = "test-support"))]
    pub fn storage(&self) -> &Storage {
        &self.storage
    }
}

impl VectorCore for EngineCore {
    fn search(
        &self,
        ctx: &PolicyContext,
        table: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, CoreError> {
        if k == 0 || k > MAX_SEARCH_K {
            return Err(CoreError::InvalidK { k });
        }

        // `query` の次元をカタログ照会だけで早期検証する（`VectorArena::build` へ進む前）。
        // `VectorArena::build` は対象テーブル全行（最大 `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES`）
        // をデコード・確保してから初めて `kernel.rs::CpuScalarProvider::search` が
        // 次元不一致を検出する構造だと、次元不一致という軽量に判定できる入力であっても
        // 全行デコード分のコスト（リソース増幅）を強いられてしまう
        // （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。Issue #32 レビュー
        // 指摘）。ここでの早期拒否はエラー契約を変えず、`kernel.rs` が返すのと同じ
        // `KernelError::DimMismatch` を用いる。
        //
        // テーブル不存在は [`Self::get_row`] と対称に [`CoreError::NotFound`] へ丸め込む
        // （存在情報を漏らさない。security.md「アクセス制御の不備」）。それ以外のカタログ
        // エラー（データ破損等）はそのまま伝播する。
        let schema = match self.storage.get_table_schema(table) {
            Ok(schema) => schema,
            Err(CatalogError::TableNotFound(_)) => return Err(CoreError::NotFound),
            Err(e) => return Err(CoreError::Catalog(e)),
        };
        let expected_dim = match schema.vector_dim() {
            Some(dim) if dim != 0 && dim <= crate::storage::MAX_EMBEDDING_DIM => dim,
            _ => return Err(CoreError::Arena(ArenaError::InvalidDim)),
        };
        if query.len() != expected_dim as usize {
            return Err(CoreError::Kernel(KernelError::DimMismatch {
                expected: expected_dim,
                found: query.len(),
            }));
        }
        // 次元検証と同じ理由で、非有限（NaN・Inf）query も `VectorArena::build` へ進む前に
        // 早期拒否する（Cursor Bugbot Medium 指摘・Issue #32 #137）。`query` は wire 経路
        // からの untrusted 入力であり得るため、正しい次元であっても
        // `kernel.rs::CpuScalarProvider::search` 側の検証（`KernelError::NonFiniteQuery`）
        // だけに委ねると、次元一致・値だけ不正なクエリで同種のリソース増幅（全行デコード後に
        // 拒否）が残る。ここでの早期拒否はエラー契約を変えず、`kernel.rs` が返すのと同じ
        // `KernelError::NonFiniteQuery` を用いる。
        if query.iter().any(|v| !v.is_finite()) {
            return Err(CoreError::Kernel(KernelError::NonFiniteQuery));
        }

        // 次元・有限性検証を通過した後にのみアリーナを構築する。ここでの `TableNotFound` は
        // 上記の早期照会と同一スナップショットではない（別トランザクション）ため、
        // 直前の照会成立後にテーブルが削除された場合の理論的な競合窓のみで発生しうる。
        // その場合も同様に存在情報を漏らさず `NotFound` へ丸め込む。
        let arena = match VectorArena::build(&self.storage, table) {
            Ok(arena) => arena,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(CoreError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };

        let is_visible = |idx: usize| -> bool {
            // `arena.tenant_id`/`arena.visibility` は `idx` がアリーナの行範囲内であれば
            // 必ず `Some` を返す（`VectorArena::build` の不変条件）。範囲外を渡すことは
            // ないため、万一 `None` が来ても false 側（不可視）に倒す（fail-closed）。
            match (arena.tenant_id(idx), arena.visibility(idx)) {
                (Some(tenant), Some(visibility)) => ctx.is_visible(tenant, visibility),
                _ => false,
            }
        };

        // provider の戻り値をコア側で再検証するための可視行 id 集合。`is_visible` は
        // 「provider が呼び出す」規約に過ぎず、provider が無視する・不正な `SearchHit` を
        // 返すことを型システムは防げないため、provider の自己申告に依存せずコア自身が
        // アリーナ全体を走査して構築する（AGENTS.md P0「テナント境界の弱体化」対応）。
        let visible_ids: HashSet<u64> = arena
            .ids()
            .iter()
            .enumerate()
            .filter_map(|(idx, &id)| is_visible(idx).then_some(id))
            .collect();

        let input = SearchInput {
            ids: arena.ids(),
            vectors: arena.vectors(),
            dim: arena.dim(),
            query,
            k,
            is_visible: &is_visible,
        };
        let hits = self.provider.search(input)?;

        // provider が返した各 hit の id が可視行 id 集合に属することを確認する。
        // 1 件でも逸脱していれば結果を一切返さず fail-closed に拒否する（部分的な
        // フィルタリングはしない。fail-open を避けるための判断）。
        if hits.iter().any(|hit| !visible_ids.contains(&hit.id)) {
            return Err(CoreError::ProviderResultRejected);
        }
        Ok(hits)
    }

    fn get_row(&self, ctx: &PolicyContext, table: &str, id: u64) -> Result<Row, CoreError> {
        let row = match self.storage.get_row_from_table(table, id) {
            Ok(row) => row,
            // テーブル不存在・行不存在はいずれも「不可視と不存在を区別しない」契約に
            // 合流させる。それ以外（デコード不正等のデータ破損・バックエンドエラー）は
            // `NotFound` に丸め込まず `CoreError::Catalog` としてそのまま伝播する
            // （アクセス不可とデータ破損を区別する）。`search` 経路もテーブル不存在
            // （`ArenaError::Catalog(CatalogError::TableNotFound)`）を同じく `NotFound` へ
            // 丸め込んでおり、両経路は対称（Issue #32 レビュー指摘対応）。
            Err(CatalogError::TableNotFound(_) | CatalogError::RowNotFound(_)) => {
                return Err(CoreError::NotFound)
            }
            Err(e) => return Err(CoreError::Catalog(e)),
        };
        if !ctx.is_visible(&row.tenant_id, row.visibility) {
            return Err(CoreError::NotFound);
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::storage::{RowInput, Visibility};

    fn schema_for(table_name: &str, dim: u32) -> TableSchema {
        TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    fn new_core(dir: &std::path::Path) -> EngineCore {
        EngineCore::open(dir.join("db.redb")).expect("open engine core")
    }

    // 対象ビヘイビア: CORE-1。object-safety の固定（プロトコルアダプタは `&dyn VectorCore`
    // のみを介して呼び出せる）。
    fn _assert_object_safe(_: &dyn VectorCore) {}

    #[test]
    fn search_rejects_k_zero_and_over_limit() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], 0),
            Err(CoreError::InvalidK { k: 0 })
        ));
        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], MAX_SEARCH_K + 1),
            Err(CoreError::InvalidK { .. })
        ));
    }

    #[test]
    fn search_on_empty_table_returns_empty() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let hits = core
            .search(&ctx, "docs", &[1.0, 0.0], 5)
            .expect("search ok");
        assert!(hits.is_empty());
    }

    #[test]
    fn get_row_not_found_and_invisible_are_indistinguishable() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let not_found = core.get_row(&ctx, "docs", 999);
        let invisible = core.get_row(&ctx, "docs", 1);
        assert!(matches!(not_found, Err(CoreError::NotFound)));
        assert!(matches!(invisible, Err(CoreError::NotFound)));
    }

    // 簡易テンポラリディレクトリ（外部クレート非依存。dependency-policy 準拠）。
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
            "engine-core-test-{}-{}",
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
}
