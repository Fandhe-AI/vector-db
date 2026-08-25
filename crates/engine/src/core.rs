//! プロトコル非依存のコア API 層（TASK-124・対象ビヘイビア: CORE-1）。
//!
//! `wire-server`（pg wire v3）や他の将来プロトコル実装は、本モジュールが定義する
//! [`VectorCore`] trait のみに依存する。コア API の変更なしにプロトコル実装を
//! 追加・変更できることを構造で担保する（trait は object-safe。シグネチャ安定性の
//! CI 機械チェックは TASK-125 で導入済み。チェック実体は
//! `scripts/check_core_api.sh` と `tests/core_api_stability.rs`）。
//!
//! [`EngineCore`] が製品実装で、`storage.rs`（永続化）・`catalog.rs`（スキーマ）・
//! `arena.rs`（検索対象のカラムナビュー構築）・`kernel.rs`（実行バックエンド provider）・
//! `policy.rs`（テナント境界・可視性判定）を束ねる。テナント判定は必ず
//! [`crate::policy::PolicyContext::is_visible`] の単一照合パス経由で行い、
//! 不可視行と不存在行の応答を区別しない（存在情報を漏らさない。security.md 準拠）。
//!
//! `EngineCore::search` は [`SearchProvider`] を `Box<dyn SearchProvider>`（CORE-13）で
//! 差し替え可能にしているが、provider は untrusted 実装（GPU/ANN バックエンド差し替え等）
//! でありうる。テナント境界の適用を「provider が可視性チェックに従う」という規約だけに
//! 委ねると、provider 実装がそれを無視して不可視行（他テナント行を含む）のベクトル・id を
//! 読み取る／外部送信することを防げない（AGENTS.md P0「テナント境界の弱体化」）。そのため
//! `EngineCore::search` は二重の防御を持つ: (1) [`VectorArena::build_filtered`] へ
//! [`crate::policy::PolicyContext::is_visible`] をそのまま述語として渡し、不可視行を
//! アリーナ構築時点で確保しない（`arena.rs` のドキュメント参照。以前はアリーナ全行を
//! 構築してから可視行だけを別バッファへ再確保・全コピーしており、1 検索あたりの
//! ピークメモリが最大で 2 倍になっていたが、構築時フィルタにより単一確保で完結する。
//! codex P2 対応）。`kernel::SearchInput` へはこの構築時フィルタ済みアリーナの
//! `ids`/`vectors` をそのまま渡すため、不可視データはそもそも provider のアドレス空間へ
//! 渡らない。(2) それでも provider が戻り値へアリーナ外の `id`（捏造や実装バグ）を
//! 含めた場合に備え、戻り値をコア側で計算した可視行 id 集合と突き合わせて再検証し、
//! 逸脱があれば結果を一切返さず `CoreError::ProviderResultRejected` で拒否する
//! （fail-closed。テナント分離を provider 実装の正しさに依存させない）。

use std::collections::HashSet;
use std::path::Path;

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::CatalogError;
use crate::dispatch::{self, DispatchError, DispatchInput, ExecutionPath};
use crate::kernel::{KernelError, SearchHit, SearchInput, SearchProvider};
use crate::policy::{PolicyContext, PolicyError};
use crate::search_engine;
use crate::storage::{Row, Storage, StorageError};
use redb::ReadableDatabase;

/// 検索 `k` の上限。上限検証前にアロケーションへ使わないための防御的定数
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。`catalog.rs::MAX_LIST_TABLES`
/// と同程度の桁に揃える）。`rls.rs`（TASK-133）の `PrefilterIndex::search` も同一の上限を
/// 共有するため `pub(crate)`（二重定義しない）。
pub(crate) const MAX_SEARCH_K: usize = 10_000;

/// `k` の範囲検証（`k == 0` または [`MAX_SEARCH_K`] 超過を拒否）。`core.rs::EngineCore::search`
/// と `rls.rs::PrefilterIndex::search` が同一の上限判定を共有するためのヘルパー
/// （二重管理を防ぐ）。呼び出し元はエラー型ごとに `Err(k)` を自分の variant へ写像する。
pub(crate) fn validate_search_k(k: usize) -> Result<(), usize> {
    if k == 0 || k > MAX_SEARCH_K {
        Err(k)
    } else {
        Ok(())
    }
}

/// `SearchProvider` 戻り値の Top-k 契約検証（本ファイルのモジュールドキュメント (1)〜(5)、
/// および [`VectorCore::search`] 実装内コメント参照）。`core.rs::EngineCore::search` と
/// `rls.rs::PrefilterIndex::search` の両方が provider を「untrusted 実装でありうる」前提で
/// 扱うため、単一走査の検証ロジックを本関数へ集約する（二重管理・契約の食い違いを防ぐ。
/// fail-closed: 1 件でも違反すれば `false` を返し、呼び出し元は結果を一切返さず拒否する）。
pub(crate) fn provider_result_is_valid(
    hits: &[SearchHit],
    k: usize,
    visible_id_set: &HashSet<u64>,
) -> bool {
    // (1) 件数が要求 k を超えない。
    if hits.len() > k {
        return false;
    }
    let mut seen_ids: HashSet<u64> = HashSet::with_capacity(hits.len());
    let mut prev: Option<&SearchHit> = None;
    for hit in hits {
        // (2) スコアが有限（NaN/Inf でない）。非有限スコアは全順序を持たず、後続の順序
        // 検証（`total_cmp`）が無意味になるため他の検証より先に弾く。
        if !hit.score.is_finite() {
            return false;
        }
        // (3) 縮約ビュー（＝可視行）の id 集合に属する（他テナント id・捏造 id の拒否）。
        if !visible_id_set.contains(&hit.id) {
            return false;
        }
        // (4) id が重複しない（同じ行が複数回返らない）。
        if !seen_ids.insert(hit.id) {
            return false;
        }
        // (5) スコア降順・同点は id 昇順（`kernel.rs::CpuScalarProvider` が実際に返す順序と
        // 同じ契約。`total_cmp` は (2) で有限性を確認済みのため NaN の順序上の扱いには
        // 依存しない）。
        if let Some(p) = prev {
            let out_of_order = match p.score.total_cmp(&hit.score) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => p.id >= hit.id,
                std::cmp::Ordering::Greater => false,
            };
            if out_of_order {
                return false;
            }
        }
        prev = Some(hit);
    }
    true
}

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
    /// `SearchProvider` が返却した `Vec<`[`SearchHit`]`>` が Top-k の契約
    /// （以下のいずれか）を満たさなかった（codex P0/P1・Issue #137 対応。fail-closed）:
    /// (1) 件数が要求 `k` を超える、(2) コア側で計算した可視行 id 集合に含まれない
    /// `id` を含む（他テナントの id 捏造・実装バグを含む）、(3) `id` が重複する、
    /// (4) スコアが非有限（NaN・Inf）、(5) スコア降順・同点は `id` 昇順という順序
    /// 契約に違反する。違反の種類ごとにエラーを分けると provider 内部の実装詳細が
    /// 呼び出し元へ漏れかねないため、区別せず本 variant に統一する（他テナントの
    /// 存在情報も含めない）。
    ProviderResultRejected,
    /// `dispatch.rs::select_execution_path`（TASK-155・CORE-11）への入力構築が失敗した
    /// （fail-closed。`dim`・`batch_size` の上限検証はここより前段（本モジュールの
    /// スキーマ照合）で既に一致条件を満たしているはずのため、通常到達しない防御的分岐）。
    Dispatch(DispatchError),
    /// `dispatch.rs::select_execution_path` が `ExecutionPath::Gpu` を返した
    /// （CORE-11/12）。単発クエリ経路（`core.rs::EngineCore::search`）は GPU
    /// capability を構造的に持たない（`DispatchInput::for_single_query` は GPU
    /// capability を引数に取らない）ため、決定表が正しく動作する限り到達しない
    /// はずの防御的分岐。GPU 実行を提供する `SearchProvider` 実装が後続タスクで
    /// 追加されるまで fail-closed に拒否する。
    GpuPathUnavailable,
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
            CoreError::Dispatch(e) => write!(f, "core dispatch error: {e}"),
            CoreError::GpuPathUnavailable => {
                write!(
                    f,
                    "dispatch selected a gpu execution path with no gpu-capable provider wired"
                )
            }
        }
    }
}

impl std::error::Error for CoreError {}

impl From<DispatchError> for CoreError {
    fn from(e: DispatchError) -> Self {
        CoreError::Dispatch(e)
    }
}

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
/// 既定コンストラクタ（[`Self::open`]）は `search_engine::default_engine()`（TASK-131・
/// CORE-9。現時点は CPU-only のマルチスレッド並列総当たり Top-k、ベクトル化は行わない）を
/// 注入し、この構成だけで全機能が成立する。将来の ANN provider 追加は
/// `search_engine.rs` 側の選択肢拡張で完結し、本構造体・`VectorCore` の API は変わらない。
pub struct EngineCore {
    storage: Storage,
    provider: Box<dyn SearchProvider>,
}

impl EngineCore {
    /// 指定パスの `redb` データベースを開き、既定の検索エンジン
    /// （[`crate::search_engine::default_engine`]）を注入した `EngineCore` を構築する。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        Self::with_provider(path, search_engine::default_engine())
    }

    /// 検索 provider を差し替えて構築する（テスト・将来の GPU/ANN provider 導入用）。
    pub fn with_provider(
        path: impl AsRef<Path>,
        provider: Box<dyn SearchProvider>,
    ) -> Result<Self, CoreError> {
        let storage = Storage::open(path)?;
        Ok(Self { storage, provider })
    }

    /// 既に開いた `Storage` の所有権を受け取って構築する（呼び出し元がテーブル作成・
    /// 行投入等を `Storage` の公開 API で済ませてから `EngineCore` へ引き渡す用途。
    /// `Storage::open` 自体は既に公開 API であるため、本関数はテナント境界の迂回経路を
    /// 新設しない）。
    ///
    /// 一方向の所有権移動のみを許し、`EngineCore` から生の `Storage` を取り出す経路は
    /// 公開しない（旧 `Self::storage` アクセサ・`test-support` feature を廃止した。
    /// codex P0-2・Issue #137 対応）。構築後は [`VectorCore::get_row`]・
    /// [`VectorCore::search`] が経由する [`crate::policy::PolicyContext::is_visible`] の
    /// 単一照合パスだけが `Storage` への到達経路になる（security.md P0「テナント分離の
    /// 検査を外す/緩める/バイパス経路を作らない」）。
    pub fn from_storage(storage: Storage, provider: Box<dyn SearchProvider>) -> Self {
        Self { storage, provider }
    }

    /// SQL 表層の単一文実行エントリポイント（TASK-75、対象ビヘイビア: SQL-1〜4）。
    /// `VectorCore` trait への昇格は行わない固有メソッドとする（`crates/engine/api/
    /// core_api.snapshot`・`make core-api-check` が対象とするのは `VectorCore`
    /// trait 本体のみのため、本メソッドの追加はコア API シグネチャ安定性チェックに
    /// 影響しない。trait への統合可否は wire 統合タスク（TASK-68〜73）が判断する）。
    ///
    /// `sql::allowlist::validate_statement`（構造検証）→
    /// `sql::parser::bind`（意味論検証・束縛）→ `sql::exec::execute_statement`
    /// （RLS→SCALAR→DISTANCE 固定順の実行）の順に呼ぶ。RLS 適用は `ctx` の下で
    /// 無条件に行われ、SQL 文中の `visible()` 呼び出しの有無に依存しない（SQL-3・
    /// RLS-7。`sql::exec` のモジュールドキュメント参照）。
    ///
    /// スキーマ取得（`bind` 用）・候補走査（`sql::exec::execute_statement` 内の
    /// `VectorArena::build_filtered_with_rows_in_txn`）を単一の `read_txn`
    /// （同一スナップショット）上で行う（Issue #56 レビュー指摘対応・codex P1:
    /// 以前は `Storage::get_table_schema` が別トランザクションでスキーマを取得し、
    /// `execute_statement` がさらに別トランザクションで行走査していたため、この間に
    /// `alter_table_add_column` がコミットされると、`bind` が束縛した旧スキーマで
    /// 新スナップショットの行を検索することになり、新設列の欠落や `row_codec`
    /// デコード失敗（`XX000` 相当）を招き得た。`catalog::get_table_schema_in_txn` で
    /// スキーマを取得したのと同一の `read_txn` を `execute_statement` へ渡すことで、
    /// スキーマ取得・bind・候補走査を単一スナップショットへ閉じ込める）。
    pub fn execute_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_statement(sql, &self.storage)?;
        let read_txn = self.storage.db().begin_read().map_err(|e| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!(
                    "failed to begin read transaction: {}",
                    StorageError::from(e)
                ),
            }
        })?;
        let schema =
            crate::catalog::get_table_schema_in_txn(&read_txn, &stmt.table_name).map_err(|e| {
                match e {
                    CatalogError::TableNotFound(name) => {
                        crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                    }
                    other => crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: format!("failed to load table schema: {other}"),
                    },
                }
            })?;
        let bound = crate::sql::parser::bind(&stmt, &schema)?;
        crate::sql::exec::execute_statement(&read_txn, self.provider.as_ref(), ctx, &schema, &bound)
    }

    /// SQL 表層の単一 INSERT 文実行エントリポイント（TASK-80、対象ビヘイビア:
    /// SQL-10）。`execute_sql`（TASK-75、SELECT 専用）とは独立した固有メソッドと
    /// する。`VectorCore` trait への昇格は行わない（`crates/engine/api/
    /// core_api.snapshot` が対象とするのは `VectorCore` trait 本体のみのため、
    /// 本メソッドの追加はコア API シグネチャ安定性チェックに影響しない。
    /// `execute_sql` と同じ理由）。
    ///
    /// `sql::allowlist::validate_insert`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略はこの段階で `23502` として拒否され、
    /// 書き込みトランザクションは一切開始されない）→ `Storage::get_table_schema`
    /// （スキーマ取得）→ `sql::parser::bind_insert`（意味論検証・束縛）→
    /// `sql::exec::execute_insert`（単一 write トランザクションでの実行）の順に呼ぶ。
    pub fn execute_insert_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_insert(sql, &self.storage)?;
        let schema = self
            .storage
            .get_table_schema(&stmt.table_name)
            .map_err(|e| match e {
                CatalogError::TableNotFound(name) => {
                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                }
                other => crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: format!("failed to load table schema: {other}"),
                },
            })?;
        let bound = crate::sql::parser::bind_insert(&stmt, &schema)?;
        crate::sql::exec::execute_insert(&self.storage, ctx, &bound)
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
        validate_search_k(k).map_err(|k| CoreError::InvalidK { k })?;

        // `query` の次元をカタログ照会だけで早期検証する（`VectorArena::build` へ進む前）。
        // `VectorArena::build` は対象テーブル全行（最大 `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES`）
        // をデコード・確保してから初めて provider（`kernel.rs::SearchProvider` 実装。既定は
        // `parallel_search.rs::ParallelSearchProvider`）が次元不一致を検出する構造だと、次元不一致
        // という軽量に判定できる入力であっても
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
        // provider 側の検証（`KernelError::NonFiniteQuery`。`kernel.rs::CpuScalarProvider`・
        // `parallel_search.rs::ParallelSearchProvider` 共通の契約）だけに委ねると、次元一致・値だけ
        // 不正なクエリで同種のリソース増幅（全行デコード後に
        // 拒否）が残る。ここでの早期拒否はエラー契約を変えず、`kernel.rs` が返すのと同じ
        // `KernelError::NonFiniteQuery` を用いる。
        if query.iter().any(|v| !v.is_finite()) {
            return Err(CoreError::Kernel(KernelError::NonFiniteQuery));
        }

        // 実行経路の決定（TASK-155・対象ビヘイビア: CORE-11, CORE-12）。単発クエリ経路は
        // 待機キューを持たない（`pending_after_pop` は常に `false`）ため決定表は常に
        // `ExecutionPath::CpuSimd` を返す。`DispatchInput::for_single_query` は GPU
        // capability を引数に取らない設計のため `ExecutionPath::Gpu` は構造的に
        // 到達しないが、決定表が返しうる全 variant を網羅させることで、将来
        // 単発クエリ経路へ GPU capability を持ち込む変更が発生した際にコンパイル
        // エラーで検出できるようにする（`dispatch.rs` モジュールドキュメント参照）。
        let dispatch_input = DispatchInput::for_single_query(expected_dim as usize, false)?;
        match dispatch::select_execution_path(dispatch_input)? {
            ExecutionPath::CpuSimd { .. } => {}
            ExecutionPath::Gpu => return Err(CoreError::GpuPathUnavailable),
        }

        // 次元・有限性検証を通過した後にのみアリーナを構築する。ここでの `TableNotFound` は
        // 上記の早期照会と同一スナップショットではない（別トランザクション）ため、
        // 直前の照会成立後にテーブルが削除された場合の理論的な競合窓のみで発生しうる。
        // その場合も同様に存在情報を漏らさず `NotFound` へ丸め込む。
        //
        // `VectorArena::build_filtered` へ `ctx.is_visible` をそのまま述語として渡し、
        // 不可視行（他テナント行を含む）をアリーナ構築時点で確保しない
        // （codex P0/P2・Issue #137 対応。以前はアリーナ全行を構築してから可視行だけを
        // 別バッファへ再確保・全コピーしており、1 検索あたりのピークメモリが最大で
        // 2 倍になっていた。構築時フィルタにより単一確保で完結する。`arena.rs` の
        // ドキュメント参照）。構築後のアリーナは `ctx` の下で可視な行だけを保持する。
        let arena = match VectorArena::build_filtered(&self.storage, table, |tenant, visibility| {
            ctx.is_visible(tenant, visibility)
        }) {
            Ok(arena) => arena,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(CoreError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };

        // アリーナは構築時点で可視行だけへ絞り込み済みのため、`ids`/`vectors` を
        // そのまま `SearchInput` として provider へ渡せる（不可視データはそもそも
        // provider のアドレス空間へ渡らない。`SearchInput` に `is_visible` フィールドは
        // 存在しない。`kernel.rs` のドキュメント参照）。
        let input = SearchInput {
            ids: arena.ids(),
            vectors: arena.vectors(),
            dim: arena.dim(),
            query,
            k,
        };
        let hits = self.provider.search(input)?;

        // provider が返した結果が Top-k の契約を満たすことをコア側で検証する
        // （codex P1・Issue #137 対応）。可視集合所属だけでは「他テナントの行は
        // 混入していないが、件数超過・id 重複・非有限スコア・順序違反」を見逃す
        // ため、共有ヘルパ `provider_result_is_valid`（`rls.rs::PrefilterIndex::search`
        // と共通、TASK-133）で単一走査を確認し、1 件でも違反すれば結果を一切返さず
        // fail-closed に拒否する（部分的なフィルタリング・並べ替えはしない）。
        //
        // アリーナは構築時点で可視行だけへ絞り込み済みのため、`arena.ids()` がそのまま
        // 可視行 id 集合になる。
        let visible_id_set: HashSet<u64> = arena.ids().iter().copied().collect();
        if !provider_result_is_valid(&hits, k, &visible_id_set) {
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
