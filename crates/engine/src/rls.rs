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
//! 作る。以後の検索はこの構築時スナップショットのベクトル・id 集合に対してのみ行われる
//! （書き込みは検索対象の追加/除外としては反映されない）。ただし
//! [`PrefilterIndex::search`] は返却直前に候補ヒットの現在の行状態
//! （`tenant_id`・`visibility`）をストレージへ引き直して再検証するため、構築後に
//! update/delete で失効した行はスナップショットに残っていても返らない（下記・
//! codex-review P0 指摘・PR #151 対応）。`PrefilterIndex` は構築時に束縛した
//! `PolicyContext` の複製（テナント ID・許可可視性集合）を保持し、
//! [`PrefilterIndex::search`] は呼び出し時に渡された `PolicyContext` とこの複製の完全一致
//! （`PartialEq`）を fail-closed に照合する。別テナント・可視性が狭化/拡大された ctx で
//! 同一インデックスを転用しようとした場合は [`RlsError::ContextMismatch`] で拒否する
//! （テナント境界 P0。codex-review P0 指摘・PR #151 対応: 以前は `search` が ctx を
//! 受け取らず、構築時 ctx との一致を検証していなかった）。
//!
//! **失効行の再検証（codex-review P0 指摘・PR #151 対応）**: 構築時 ctx との一致だけでは
//! 「インデックス構築後にストレージ側で該当行の tenant/visibility が変更・削除された」
//! ケースを検出できない（ctx 自体は変わらないため）。[`PrefilterIndex::search`] は
//! provider から候補ヒットを受け取った後、[`crate::storage::Storage::get_row_headers_from_table`]
//! で該当 id の現在の `tenant_id`・`visibility` を読み直し、`ctx.is_visible` で
//! 再照合する。1 件でも「現在は不可視/不存在」であれば、部分的に結果を間引くのではなく
//! クエリ全体を [`RlsError::IndexStale`] で fail-closed に拒否する（呼び出し元へ
//! [`PrefilterIndex::build`] の再実行を要求する。テナント境界 P0）。
//! なお、tenant/visibility は変わらず embedding 本体だけが更新された場合のスコアは
//! 依然として構築時点の値のまま返る（この再検証はテナント境界の失効検出が目的で、
//! embedding 鮮度の保証はスコープ外。完全な鮮度が必要な呼び出し元は `build` を
//! 呼び直すこと）。
//!
//! **再検証対象ストレージの束縛（codex-review P0 指摘・PR #151 対応）**: 上記の再検証は
//! 「[`PrefilterIndex::search`] へ渡された `Storage` が [`PrefilterIndex::build`] に
//! 使ったものと同一である」ことに依存する。以前は `search` が呼び出し時に任意の
//! `&Storage` を受け取っており、同名テーブル・同じヒット id・同じ `tenant_id`/
//! `visibility` を持つ**別の** `Storage`（別 DB ファイル）を渡すと、再検証をすり抜けて
//! その別ストレージ上のベクトル由来の id・スコアを返せてしまった（テナント境界 P0）。
//! `PrefilterIndex<'s>` は構築時に渡された `&'s Storage` をフィールドとして保持し、
//! `search` は引数から `storage` を取り除いて `self` が保持する参照のみを使う。
//! ストレージの同一性はランタイム比較ではなく借用チェッカーにより型レベルで保証され、
//! `search` に別インスタンスを渡すこと自体が不可能になる（`build` 後に別 `Storage` を
//! 渡す経路は構造的に存在しない）。テーブルの削除・再作成については、本クレートに
//! テーブル削除 API が現時点で存在しないため到達不能である（`catalog.rs` の
//! `user_rows_table_name` ドキュメント中の申し送り参照。将来 `drop_table` 相当を
//! 追加する実装者は、テーブル再作成後に古い `PrefilterIndex` を無効化する仕組みを
//! 同時に検討すること）。
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
    /// [`PrefilterIndex::search`] が候補ヒットの現在の行状態を再検証した結果、構築時点の
    /// スナップショットに残っている行が検索時点では不可視・不存在になっていた
    /// （テーブル側の update/delete によるポリシー失効）。`Display` は id・テナント ID を
    /// 含めない（他テナントの存在情報を漏らさないため。security.md P0）。呼び出し元は
    /// [`PrefilterIndex::build`] を呼び直して再構築すること（codex-review P0 指摘・
    /// PR #151 対応）。
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

/// 事前フィルタ方式の再利用可能インデックス（RLS-1〜4）。
///
/// [`Self::build`] 構築時に束縛した [`PolicyContext`] の可視性述語で
/// [`VectorArena::build_filtered`] を呼び、可視行のみのカラムナ表現を保持する。
/// 検索対象の候補集合（構築時点でどの行が挙がるか）は構築後の書き込みを反映しない
/// スナップショットのままだが、[`Self::search`] は返却直前にヒット行の現在の
/// tenant/visibility をストレージへ再照合するため、構築後に失効した行は結果に混入しない
/// （モジュールドキュメント「失効行の再検証」参照）。可視率・ポリシーが大きく変わる場合や
/// embedding 本体の鮮度が必要な場合は [`Self::build`] を呼び直して再構築する必要がある。
///
/// アクセサの `ctx` 必須化方針（codex-review P0 指摘・PR #151 対応）: [`Self::len`]・
/// [`Self::is_empty`] は構築元テナントの可視行数・行の有無という**存在情報**を返すため、
/// `ctx` を必須化し [`Self::built_ctx`] との完全一致を [`Self::search`] と同一の
/// fail-closed ゲートで照合する（一致しなければ [`RlsError::ContextMismatch`]）。
/// キャッシュ取り違え等で別テナント用の `PrefilterIndex` を受け取った呼び出し元が、
/// `search` を拒否されてもこれらのアクセサ経由で存在情報を得られてしまう経路を塞ぐ。
/// 一方 [`Self::dim`]・[`Self::table_name`] は `ctx` を要求しない: `dim` はテーブル定義
/// （`CREATE TABLE` 時に宣言される次元数）であり全テナント共通でテナント間の違いを
/// 持たない値、`table_name` は呼び出し元が [`Self::build`] へ自ら渡した引数の単純な
/// 反映であり、いずれも本インデックスから新たに得られる情報を持たない
/// （非機微と判断し対象外とした）。
pub struct PrefilterIndex<'s> {
    arena: VectorArena,
    /// [`Self::build`] に渡された `&Storage` をそのまま保持する。[`Self::search`] は
    /// この参照だけを失効行の再検証に使い、引数として別途 `Storage` を受け取らない
    /// （codex-review P0 指摘・PR #151 対応。モジュール doc「再検証対象ストレージの束縛」
    /// 参照。同一性を借用チェッカーで型レベルに保証し、別ストレージのすり替えを構造的に
    /// 排除する）。
    storage: &'s Storage,
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

impl<'s> PrefilterIndex<'s> {
    /// `table` に対し `ctx` の可視性述語で可視行のみのインデックスを構築する
    /// （事前フィルタ・RLS-1: 不可視行はこの構築時点でアリーナへ確保されない）。
    ///
    /// `ctx` は可視行の絞り込み述語として使うと同時に、[`Self::search`] での転用検出用に
    /// 複製して保持する（モジュールドキュメント参照）。`storage` への参照も
    /// [`PrefilterIndex`] のライフタイム `'s` として保持し、[`Self::search`] が別の
    /// `Storage` を受け取れない構造にする（モジュール doc「再検証対象ストレージの束縛」
    /// 参照）。テーブル不存在は `core.rs::EngineCore::search`/`get_row` と対称に
    /// [`RlsError::NotFound`] へ丸め込む（存在情報を漏らさない。security.md P0）。
    /// 容量超過・次元不整合はそのまま [`RlsError::Arena`] へ伝播する
    /// （`VectorArena::build_filtered` の契約をそのまま継承）。
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
    /// モジュールドキュメントの二重防御と同じ設計。この検証は構築時アリーナの
    /// `visible_id_set`（メモリ上・I/O なし）とだけ突き合わせるため、ストレージへの
    /// 再検証読み取りより先に行う。provider が捏造した id で本モジュールを
    /// ストレージ探索オラクルにできないようにするため）。
    ///
    /// 上記を通過したヒットについて、[`Self::build`] に渡されたストレージ（`self.storage`。
    /// 引数では受け取らない。モジュール doc「再検証対象ストレージの束縛」参照）から該当 id
    /// の**現在の** `tenant_id`・`visibility` を読み直し、`ctx.is_visible` で再照合する
    /// （構築後の update/delete による失効検出。モジュールドキュメント参照）。1 件でも
    /// 「現在は不可視/不存在」であれば、部分的に間引かずクエリ全体を
    /// [`RlsError::IndexStale`] で fail-closed に拒否する。
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

        // 失効行の再検証（codex-review P0 指摘・PR #151 対応）: ここまでの検証は構築時
        // スナップショットの範囲内にヒットが収まっていることしか確認しない。構築後に
        // ストレージ側で該当行の tenant/visibility が変更・削除されていれば、依然として
        // 古い状態のまま返してしまう。1 回の read トランザクションでヒット id 分の
        // ヘッダだけを引き直し、`ctx.is_visible` で現在の可視性を再照合する。
        // `get_row_headers_from_table` の呼び出し件数は `hits.len()`。`hits` は直前の
        // `provider_result_is_valid` で `hits.len() <= k <= MAX_SEARCH_K` を確認済みのため、
        // ここで無制限にはならない（`get_row_headers_from_table` 自体は `pub(crate)` で
        // 上限を持たないため、将来別の呼び出し元を追加する場合はこの前提を呼び出し側で
        // 満たすこと）。
        let hit_ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
        // `CatalogError`（redb I/O エラー・行ヘッダのデコード不正のいずれも含む）は
        // 種別を区別せず一律 `IndexStale` に丸め込む。`build` は同種のエラーを
        // `RlsError::Arena` へ透過するのに対し非対称だが、ここでは「現在の可視性を
        // 確認できない」こと自体を fail-closed に「再構築が必要」として扱うのが目的であり、
        // エラー種別の詳細を呼び出し元へ伝える必要がない（他テナントの存在情報も
        // 含めない。security.md P0）。
        let headers = self
            .storage
            .get_row_headers_from_table(self.arena.table_name(), &hit_ids)
            .map_err(|_| RlsError::IndexStale)?;
        let all_still_visible = headers.len() == hit_ids.len()
            && headers
                .iter()
                .all(|h| matches!(h, Some((tenant, vis)) if ctx.is_visible(tenant, *vis)));
        if !all_still_visible {
            return Err(RlsError::IndexStale);
        }

        Ok(hits)
    }

    /// インデックスが保持する可視行数を返す（テナント境界 P0・codex-review P0 指摘・
    /// PR #151 対応）。
    ///
    /// `ctx` は [`Self::build`] 時に束縛した `PolicyContext` と完全一致していなければ
    /// ならない。[`Self::search`] と同一のゲートで、一致しない場合は可視行数（存在情報）を
    /// 一切返さず [`RlsError::ContextMismatch`] で fail-closed に拒否する（キャッシュ
    /// 取り違え等で別テナント用インデックスを受け取った呼び出し元が、検索を拒否されても
    /// 本メソッド経由で行数を取得できてしまう経路を塞ぐため。struct doc 参照）。
    pub fn len(&self, ctx: &PolicyContext) -> Result<usize, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.len())
    }

    /// 可視行が 0 件かを返す（テナント境界 P0・codex-review P0 指摘・PR #151 対応）。
    /// `ctx` の照合方針は [`Self::len`] と同一（構築時 ctx との完全一致必須・
    /// 不一致は [`RlsError::ContextMismatch`]）。
    pub fn is_empty(&self, ctx: &PolicyContext) -> Result<bool, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.is_empty())
    }

    /// 検索対象ベクトルの次元。テーブル定義（`CREATE TABLE` 宣言次元）であり全テナント
    /// 共通の値のため `ctx` を要求しない（struct doc「アクセサの ctx 必須化方針」参照）。
    pub fn dim(&self) -> u32 {
        self.arena.dim()
    }

    /// 構築元のテーブル名。呼び出し元が [`Self::build`] へ自ら渡した引数の単純な反映で、
    /// 本インデックスから新たに得られる情報がないため `ctx` を要求しない（struct doc
    /// 「アクセサの ctx 必須化方針」参照）。
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

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: インデックス構築後に対象行の
    // tenant_id がストレージ側で書き換わり（構築時に可視だった行が他テナントへ移った）、
    // 同一 ctx で検索してもスナップショットに残った旧行を返さず `RlsError::IndexStale` で
    // fail-closed に拒否する。これが本 P0 指摘そのもの（build 後の失効行の漏えい）。
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

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: tenant_id は変わらず
    // visibility だけが構築時 ctx の許可範囲外へ書き換わった場合も同様に
    // `RlsError::IndexStale` で拒否する。
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

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: 「検索時に構築時とは別の
    // `Storage` を渡し、同名テーブル・同じ id・同じ tenant/visibility を持つ別 DB から
    // ヒットを返させる」という取り違えシナリオは、`search` の引数から `storage` を
    // 削除しフィールド `PrefilterIndex::storage`（構築時に束縛したライフタイム `'s` の
    // 参照）だけを使う構造にしたことで、コンパイル時に構文として表現不能になった
    // （以前この位置にあった単体テストは `search(&ctx, &storage_b, ...)` という
    // 呼び出し自体が今はコンパイルエラーになるため削除した。モジュール doc
    // 「再検証対象ストレージの束縛」参照）。行削除相当の「ヒット id がストレージ上に
    // 存在しない」再検証パス（`get_row_headers_from_table` の `None` 分岐）は
    // `catalog.rs::tests::get_row_headers_from_table_returns_none_for_a_deleted_or_missing_id`
    // で直接カバーする（本クレートに行削除 API 自体が存在しないため、削除ではなく
    // 未挿入 id で `None` 分岐を再現する）。

    // codex-review P0 指摘・PR #151 対応（テナント境界 P0）: `len`/`is_empty` は構築元
    // テナントの可視行数・行の有無という存在情報を返すため、`search` と同一の ctx 照合
    // ゲートを持つ。別テナント（tenant-b。自身の可視行を持つ）の ctx を tenant-a の
    // インデックスへ渡した場合、`search` は拒否されるだけでなく `len`/`is_empty` からも
    // 存在情報を得られないことを確認する。
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
}
