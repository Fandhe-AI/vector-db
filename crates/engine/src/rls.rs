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
//!
//! 本モジュールはさらに [`SearchTimeFilter`]（TASK-134・対象ビヘイビア: RLS-1, RLS-3）を
//! 提供する。[`PrefilterIndex`] は構築時に `PolicyContext` を束縛するため、ポリシーが
//! リクエスト単位で動的に変わるワークロードでは毎回の再構築コストがかかる。
//! [`SearchTimeFilter`] はそのフォールバックで、無フィルタのアリーナを 1 度だけ構築して
//! 保持し、`search` 呼び出しごとに異なる `PolicyContext` を受け取れる。可視性判定は
//! 全行アリーナを外部（`SearchProvider`）へ渡すマスク方式ではなく、
//! 可視性判定とスコア計算を単一パスで trust boundary（本モジュール）内に閉じて行う
//! （[`SearchInput`] の「可視行のみを含む縮約ビュー」契約を維持するため）。
//! 静的ポリシー＝事前フィルタ（[`PrefilterIndex`]）／動的ポリシー＝検索時フィルタ
//! （[`SearchTimeFilter`]）の使い分け・切り替え判断は呼び出し元の責務とする
//! （本モジュールは両方の API を提供するのみ）。
//!
//! **可用性面の非対称性（呼び出し元は切り替え判断時に必ず確認すること）**:
//! [`PrefilterIndex::build`] は `ctx` の可視性述語を [`VectorArena::build_filtered`] へ
//! 渡すため、アリーナ容量上限（`arena.rs::MAX_ARENA_ROWS`/`MAX_ARENA_TOTAL_BYTES`）の
//! 判定は「呼び出しテナントの可視行数」基準になる。一方 [`SearchTimeFilter::build`] は
//! 無フィルタの [`VectorArena::build`]（内部で `build_filtered(|_,_| true)`）を呼ぶため、
//! 同じ容量上限判定が「テーブル全体（全テナント合算）の行数・バイト量」基準になる
//! ——他テナントのデータ量が対象テナントの検索可用性へ干渉しうる、
//! [`VectorArena::build_filtered`] のドキュメントが「以前のバグとして修正した」と記す
//! まさにその干渉を、本フォールバック経路で再導入する形になる。「1 個のアリーナで
//! 任意の ctx を後から評価する」という [`SearchTimeFilter`] の設計上不可避なトレード
//! オフだが、[`PrefilterIndex`] と同一の可用性契約であるかのように扱わないこと。

use std::collections::HashSet;

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::CatalogError;
use crate::core::{provider_result_is_valid, validate_search_k, MAX_SEARCH_K};
use crate::kernel::{self, KernelError, SearchHit, SearchInput, SearchProvider};
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
    /// [`crate::storage::Storage::current_generation`] の読み取り失敗（[`Self::build`]
    /// 時のみ発生。`search` 時は [`RlsError::IndexStale`] へ丸め込む）。
    Storage(crate::storage::StorageError),
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
            RlsError::Storage(e) => write!(f, "rls prefilter storage error: {e}"),
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

impl From<crate::storage::StorageError> for RlsError {
    fn from(e: crate::storage::StorageError) -> Self {
        RlsError::Storage(e)
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
    /// [`Self::build`] 時に読んだストレージ世代（[`Storage::current_generation`]）。
    /// [`Self::search`] の失効検出に使う。
    built_generation: u64,
}

impl<'s> PrefilterIndex<'s> {
    /// `table` に対し `ctx` の可視性述語で可視行のみのインデックスを構築する（RLS-1）。
    /// テーブル不存在は [`RlsError::NotFound`] へ丸め込む（存在情報を漏らさない。
    /// security.md P0）。容量超過・次元不整合は [`RlsError::Arena`] へ伝播する。
    pub fn build(storage: &'s Storage, table: &str, ctx: &PolicyContext) -> Result<Self, RlsError> {
        // 世代を先に読んでからアリーナを構築する（アリーナ構築中に別の書き込みが
        // コミットされても、その変更を見落とさない方向の順序。`search` の doc 参照）。
        let built_generation = storage.current_generation()?;
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
            built_generation,
        })
    }

    /// 保持済みインデックスに対して Top-k 検索を行う（over-fetch なし・RLS-3）。
    ///
    /// `ctx` は [`Self::build`] 時に束縛した `PolicyContext` と完全一致していなければ
    /// [`RlsError::ContextMismatch`] で fail-closed に拒否する。`k`・`query` の検証
    /// （`core.rs::EngineCore::search` と同一契約）の後、provider を呼ぶ**前**と
    /// provider から結果を受け取った**後**の 2 回、
    /// [`crate::storage::Storage::current_generation`] を呼んで [`Self::build`] 時の値と
    /// 比較する（いずれかで不一致なら [`RlsError::IndexStale`]）。世代はストレージ全体の
    /// 書き込みコミットのたびに単調増加する（`crate::storage::bump_generation_and_commit`）
    /// ため、両方で一致することは「事前確認から事後確認までの間、行集合・内容とも一切
    /// 変更されていない」ことを意味する（安全性はこの前後比較のみで担保しており、
    /// `current_generation` 自体は世代値以外の一貫性を保証しない）。事前確認を通過した
    /// 場合のみ provider を呼び、戻り値を `provider_result_is_valid`（`core.rs`）で検証する
    /// （違反は [`RlsError::ProviderResultRejected`]）。事前・事後の確認自体は互いに独立した
    /// 読み取りであり、事後確認と `Ok(hits)` 返却の間に残るごく短いウィンドウは次回検索の
    /// 世代照合で扱う。TASK-133・RLS-1〜4 参照。
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

        // 事前の失効検出（上記ドキュメント参照）。世代の読み取り自体に失敗した場合も
        // 「現在の状態を確認できない」ため fail-closed に `IndexStale` とする。
        let pre_generation = self
            .storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if pre_generation != self.built_generation {
            return Err(RlsError::IndexStale);
        }

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

        // 事後の失効再検証（上記ドキュメント参照）。事前確認〜ここまでの間に別の書き込みが
        // コミットされている可能性があるため、世代を読み直して不一致なら結果を破棄する。
        let post_generation = self
            .storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if post_generation != self.built_generation {
            return Err(RlsError::IndexStale);
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

/// 検索時フィルタ方式の再利用可能インデックス（TASK-134・RLS-1, RLS-3）。
///
/// [`PrefilterIndex`] との本質的な差分は「`PolicyContext` を構築時に束縛しない」こと。
/// [`Self::build`] は無フィルタの [`VectorArena`]（テーブル全行）を 1 度だけ構築して
/// 保持し、[`Self::search`] は呼び出しごとに異なる `PolicyContext` を受け取れる
/// （同一インスタンスをリクエスト単位で異なるポリシーへ使い回せる、動的ポリシー用の
/// フォールバック）。縮約テーブルを別途構築せず、[`Self::search`] が全行を単一パスで
/// 走査しながら可視性を先に判定し、不許可行はスコア計算自体をスキップする。
///
/// 可視性判定とスコア計算は本モジュール（trust boundary 内）で完結させ、不可視行の
/// id・ベクトルを [`SearchProvider`] のアドレス空間へ一切渡さない（[`SearchInput`] の
/// 「可視行のみを含む縮約ビュー」契約はコアが object-safe な `SearchProvider` へ渡す
/// 入力にのみ適用され、本型はスコア計算自体を自前で行うため provider を経由しない）。
pub struct SearchTimeFilter<'s> {
    /// `predicate` なしで構築した無フィルタアリーナ（テーブル全行を保持する）。
    arena: VectorArena,
    /// [`Self::build`] に渡された `&Storage`（世代の事前・事後照合に使う）。
    storage: &'s Storage,
    /// [`Self::build`] 時に読んだストレージ世代（[`Storage::current_generation`]）。
    /// [`Self::search`] の失効検出に使う（[`PrefilterIndex`] と同じ前後比較方式）。
    built_generation: u64,
}

impl<'s> SearchTimeFilter<'s> {
    /// `table` の全行を対象に無フィルタのアリーナを構築する。ポリシーはここでは
    /// 一切評価しない（[`Self::search`] 呼び出し時に都度評価する）。
    /// テーブル不存在は [`RlsError::NotFound`] へ丸め込む（存在情報を漏らさない。
    /// security.md P0。[`PrefilterIndex::build`] と同一契約）。
    ///
    /// **[`PrefilterIndex::build`] と異なる可用性契約**: 本メソッドは無フィルタの
    /// [`VectorArena::build`] を呼ぶため、アリーナ容量上限超過（`RlsError::Arena`
    /// 経由の `ArenaError::CapacityExceeded`）の判定基準は「テーブル全体（全テナント
    /// 合算）の行数・バイト量」になる（`PrefilterIndex::build` の「呼び出しテナントの
    /// 可視行数」基準とは非対称）。詳細はモジュール doc「可用性面の非対称性」の項を
    /// 参照。
    pub fn build(storage: &'s Storage, table: &str) -> Result<Self, RlsError> {
        // 世代を先に読んでからアリーナを構築する（`PrefilterIndex::build` と同じ順序。
        // アリーナ構築中に別の書き込みがコミットされても見落とさない方向）。
        let built_generation = storage.current_generation()?;
        let arena = match VectorArena::build(storage, table) {
            Ok(arena) => arena,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(RlsError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            arena,
            storage,
            built_generation,
        })
    }

    /// `ctx` の可視性述語で Top-k 検索を行う（RLS-1: 不許可行の混入 0 件・RLS-3:
    /// over-fetch なしで k 件充足）。
    ///
    /// `k`・`query` の検証（[`PrefilterIndex::search`] と同一契約）の後、世代の事前照合
    /// （不一致・読み取り失敗は [`RlsError::IndexStale`]。fail-closed）を行う。通過後、
    /// アリーナの全行を単一パスで走査し、各行の `tenant_id`/`visibility` を取得できない
    /// （`None`＝不変条件破れ）行、および `ctx.is_visible` が偽を返す行はスコア計算前に
    /// スキップする（可視行のみ [`kernel::dot`] でスコア計算し
    /// [`kernel::TopKSelector::push`] へ渡す）。走査中に可視 id 集合を収集し、選出結果を
    /// `provider_result_is_valid`（`core.rs`。件数上限・スコア有限性・可視 id 集合内・
    /// 重複なし・順序の 5 点を検証）で返却直前に機械照合する二重防御を行う（違反は
    /// [`RlsError::ProviderResultRejected`]）。最後に世代の事後照合を行い、走査中に
    /// 別の書き込みがコミットされていれば [`RlsError::IndexStale`] とする
    /// （前後比較の意味は [`PrefilterIndex::search`] のドキュメント参照）。
    pub fn search(
        &self,
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
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

        // 事前の失効検出（[`PrefilterIndex::search`] と同じ方針。世代の読み取り自体に
        // 失敗した場合も fail-closed に `IndexStale` とする）。
        let pre_generation = self
            .storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if pre_generation != self.built_generation {
            return Err(RlsError::IndexStale);
        }

        let mut selector = kernel::TopKSelector::new(k);
        for idx in 0..self.arena.len() {
            let (Some(tenant), Some(visibility)) =
                (self.arena.tenant_id(idx), self.arena.visibility(idx))
            else {
                // アリーナの不変条件（`ids`/`tenant_ids`/`visibilities` が同じ長さ）が
                // 破れている行。破損行として除外する（`CpuScalarProvider` の破損行除外と
                // 同方針。untrusted 入力経路ではないため添字アクセスは使わず `Option` の
                // まま処理する）。
                continue;
            };
            if !ctx.is_visible(tenant, visibility) {
                continue;
            }
            let Some(&id) = self.arena.ids().get(idx) else {
                continue;
            };
            let Some(vector) = self.arena.vector(idx) else {
                continue;
            };
            let score = kernel::dot(vector, query);
            if !score.is_finite() {
                // 格納ベクトルの NaN/Inf 混入・オーバーフローによる非有限化を除外する
                // （`kernel.rs::CpuScalarProvider` と同方針）。
                continue;
            }
            selector.push(SearchHit { id, score });
        }
        let hits = selector.into_sorted_vec();

        // 返却直前の機械検証（件数上限・スコア有限性・重複なし・順序の 4 点。
        // `PrefilterIndex::search` と異なり本型は `dyn SearchProvider` を経由せず、
        // `hits` は上記ループで `ctx.is_visible` を通過した行からのみ inline 生成される
        // ため、id 集合が可視行に属するかの検証は自己ループの同語反復になり
        // `dyn SearchProvider` 越しの防御という本来の意義を持たない。そのため
        // `visible_id_set` は全行分ではなく `hits` 自身の id から構築し、他 4 項目の
        // 検証にのみ使う（`hits` の id は元々可視行由来なので (3) は常に真になる）。
        let hit_id_set: HashSet<u64> = hits.iter().map(|hit| hit.id).collect();
        if !provider_result_is_valid(&hits, k, &hit_id_set) {
            return Err(RlsError::ProviderResultRejected);
        }

        // 事後の失効再検証（[`PrefilterIndex::search`] と同じ方針）。
        let post_generation = self
            .storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if post_generation != self.built_generation {
            return Err(RlsError::IndexStale);
        }

        Ok(hits)
    }

    /// `ctx` の可視性述語で数えた可視行数を返す（存在情報のため `ctx` 必須。
    /// [`PrefilterIndex::len`] と異なり `ContextMismatch` は返さず、渡された `ctx` で
    /// 都度判定する——[`Self`] は構築時にポリシーを束縛しないため）。
    pub fn len(&self, ctx: &PolicyContext) -> usize {
        (0..self.arena.len())
            .filter(
                |&idx| match (self.arena.tenant_id(idx), self.arena.visibility(idx)) {
                    (Some(tenant), Some(visibility)) => ctx.is_visible(tenant, visibility),
                    _ => false,
                },
            )
            .count()
    }

    /// 可視行が 0 件かを返す（`ctx` 判定は [`Self::len`] と同じ述語。先頭から可視行が
    /// 見つかり次第打ち切るため全件走査の [`Self::len`] より軽い）。
    pub fn is_empty(&self, ctx: &PolicyContext) -> bool {
        !(0..self.arena.len()).any(|idx| {
            match (self.arena.tenant_id(idx), self.arena.visibility(idx)) {
                (Some(tenant), Some(visibility)) => ctx.is_visible(tenant, visibility),
                _ => false,
            }
        })
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

    // tenant_id・visibility は変わらず embedding だけが構築後に書き換わった場合も
    // 世代カウンタの不一致により `RlsError::IndexStale` で拒否する（TASK-133 P1 対応）。
    #[test]
    fn search_rejects_when_embedding_only_is_updated_after_build() {
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

        // tenant/visibility は同一のまま embedding だけを書き換える。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[0.0, 1.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // 構築後に新規可視行が挿入された場合も世代カウンタの不一致により
    // `RlsError::IndexStale` で拒否する（TASK-133 P1 対応）。
    #[test]
    fn search_rejects_when_a_new_visible_row_is_inserted_after_build() {
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

        // 構築後に ctx から可視な新規行を挿入する。
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.0, 1.0],
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

    /// provider 自身が `search` 呼び出し内で（`self.storage` を通じて）ストレージへ
    /// 書き込みコミットを行う計装 provider。事前の世代確認〜事後の世代再確認の間に
    /// 別の書き込みが割り込むケースを決定的に再現するために使う。
    struct WritingDuringSearchProvider<'s> {
        storage: &'s Storage,
    }
    impl SearchProvider for WritingDuringSearchProvider<'_> {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<SearchHit>, KernelError> {
            self.storage
                .insert_row_into_table(
                    "docs",
                    999,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.0, 0.0],
                        metadata: &[],
                    },
                )
                .expect("write during provider search");
            CpuScalarProvider.search(input)
        }
    }

    // 事後の世代再検証（codex 指摘対応）: provider 実行中にストレージへの書き込みが
    // コミットされた場合、事前確認は通過していても事後の世代再確認で不一致が検出され
    // `RlsError::IndexStale` になることを検証する。
    #[test]
    fn search_rejects_when_storage_is_written_to_during_provider_execution() {
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

        let provider = WritingDuringSearchProvider { storage: &storage };
        let result = index.search(&ctx, &provider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // ------------------------------------------------------------------
    // SearchTimeFilter（TASK-134・RLS-1, RLS-3）
    // ------------------------------------------------------------------

    // 対象ビヘイビア: RLS-1。複数テナント・可視性混在データで、検索結果に不許可行の
    // 混入が 0 件であること（結果全件を `ctx.is_visible` で機械照合する）。
    #[test]
    fn search_time_filter_never_returns_invisible_rows() {
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
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 10).expect("search ok");
        // id=2（他テナント）・id=3（自テナントだが Private で ctx 未許可）はいずれも
        // 混入しない（RLS-1）。
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1（動的ポリシー）。同一 `SearchTimeFilter` インスタンスに対し
    // 異なる `PolicyContext` で連続検索し、各回とも当該 ctx の可視行のみが返ること
    // （再構築不要のフォールバック特性の確認。`PrefilterIndex` はこの用途では
    // ctx ごとに再構築が必要）。
    #[test]
    fn search_time_filter_reuses_the_same_instance_across_different_policy_contexts() {
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

        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let hits_a = filter.search(&ctx_a, &[1.0, 0.0], 10).expect("search ok");
        assert_eq!(hits_a.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1]);

        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let hits_b = filter.search(&ctx_b, &[1.0, 0.0], 10).expect("search ok");
        assert_eq!(hits_b.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
    }

    // 対象ビヘイビア: RLS-3。可視行数 >= k のとき結果がちょうど k 件で、単一パスのみで
    // 充足する（over-fetch なし）。
    #[test]
    fn search_time_filter_returns_exactly_k_hits_when_enough_visible_rows_exist() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        for (id, x) in [(1, 1.0), (2, 0.8), (3, 0.6), (4, 0.4)] {
            insert(
                &storage,
                "docs",
                id,
                "tenant-a",
                Visibility::Public,
                &[x, 0.0],
            );
        }
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 2).expect("search ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 1);
        assert_eq!(hits[1].id, 2);
    }

    // 対象ビヘイビア: RLS-3。可視行数 < k のときは可視行全件を返し、エラーにしない。
    #[test]
    fn search_time_filter_returns_all_visible_rows_when_fewer_than_k() {
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
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 10).expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1 と RLS-3 の判別テスト（本 Issue のレビュー指摘対応）。
    // 不可視行が可視行よりスコア上位に来て k 枠を奪う状況を作る
    // （tenant-b の 2 行が最上位スコアを占め、ctx=tenant-a・k=2 では tenant-a の
    // 2 行のみが正解）。「全行で top-k を選んでから可視性でフィルタする」誤実装
    // （可視性を最後に後付けする RLS-3 違反の典型パターン）だと、上位 k 件がすべて
    // 不可視行で埋まり最終結果が空になる。可視性判定をスコア計算前に行う正しい実装
    // でのみ `[3, 4]` が返る。
    #[test]
    fn search_time_filter_visibility_is_applied_before_top_k_selection_not_after() {
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
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[0.9, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        insert(
            &storage,
            "docs",
            4,
            "tenant-a",
            Visibility::Public,
            &[0.4, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 2).expect("search ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 3);
        assert_eq!(hits[1].id, 4);
    }

    // fail-closed 系: k == 0 / MAX_SEARCH_K 超過は InvalidK。
    #[test]
    fn search_time_filter_rejects_k_zero_and_over_limit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        assert!(matches!(
            filter.search(&ctx, &[1.0, 0.0], 0),
            Err(RlsError::InvalidK { k: 0 })
        ));
        assert!(matches!(
            filter.search(&ctx, &[1.0, 0.0], MAX_SEARCH_K + 1),
            Err(RlsError::InvalidK { .. })
        ));
    }

    // fail-closed 系: 次元不一致・非有限クエリは Kernel(...)。
    #[test]
    fn search_time_filter_rejects_dim_mismatch_and_non_finite_query() {
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
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        assert!(matches!(
            filter.search(&ctx, &[1.0, 0.0, 0.0], 1),
            Err(RlsError::Kernel(KernelError::DimMismatch { .. }))
        ));
        assert!(matches!(
            filter.search(&ctx, &[f32::NAN, 0.0], 1),
            Err(RlsError::Kernel(KernelError::NonFiniteQuery))
        ));
    }

    // fail-closed 系: 存在しないテーブルは NotFound（存在情報を漏らさない）。
    #[test]
    fn search_time_filter_build_returns_not_found_for_missing_table() {
        let dir = tempdir();
        let storage = open_storage(dir.path());

        let result = SearchTimeFilter::build(&storage, "no_such_table");
        assert!(matches!(result, Err(RlsError::NotFound)));
    }

    // fail-closed 系: build 後に書き込みコミットで世代が進んだ後の search は IndexStale。
    #[test]
    fn search_time_filter_rejects_when_storage_changed_after_build() {
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
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );

        let result = filter.search(&ctx, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // 選出規約: スコア降順・同点 id 昇順が `CpuScalarProvider` の結果と一致する
    // （`TopKSelector` 共用の回帰確認）。
    #[test]
    fn search_time_filter_selection_matches_cpu_scalar_provider_tie_break() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // 3 行とも同スコア（1.0）。タイブレークは id 昇順。
        for id in [3u64, 2, 1] {
            insert(
                &storage,
                "docs",
                id,
                "tenant-a",
                Visibility::Public,
                &[1.0, 0.0],
            );
        }
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 1).expect("search ok");
        assert_eq!(hits, vec![SearchHit { id: 1, score: 1.0 }]);
    }

    // `len`/`is_empty` は渡された ctx の可視性述語で都度判定する（`PrefilterIndex` と
    // 異なり ContextMismatch を返さない）。
    #[test]
    fn search_time_filter_len_and_is_empty_use_the_given_context_each_time() {
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

        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        assert_eq!(filter.len(&ctx_a), 1);
        assert!(!filter.is_empty(&ctx_a));

        let ctx_c = PolicyContext::new("tenant-c").expect("valid tenant");
        assert_eq!(filter.len(&ctx_c), 0);
        assert!(filter.is_empty(&ctx_c));
    }

    #[test]
    fn search_time_filter_dim_and_table_name_do_not_require_a_context() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 3))
            .expect("create table");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");
        assert_eq!(filter.dim(), 3);
        assert_eq!(filter.table_name(), "docs");
    }
}
