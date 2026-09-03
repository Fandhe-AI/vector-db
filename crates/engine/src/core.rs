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
//! [`crate::rls::ImplicitRlsHook::predicate`]（TASK-137・対象ビヘイビア: RLS-6, RLS-7）を
//! 渡し、不可視行を
//! アリーナ構築時点で確保しない（`arena.rs` のドキュメント参照。以前はアリーナ全行を
//! 構築してから可視行だけを別バッファへ再確保・全コピーしており、1 検索あたりの
//! ピークメモリが最大で 2 倍になっていたが、構築時フィルタにより単一確保で完結する。
//! codex P2 対応）。`kernel::SearchInput` へはこの構築時フィルタ済みアリーナの
//! `ids`/`vectors` をそのまま渡すため、不可視データはそもそも provider のアドレス空間へ
//! 渡らない。(2) それでも provider が戻り値へアリーナ外の `id`（捏造や実装バグ）を
//! 含めた場合に備え、戻り値をコア側で計算した可視行 id 集合と突き合わせて再検証し、
//! 逸脱があれば結果を一切返さず `CoreError::ProviderResultRejected` で拒否する
//! （fail-closed。テナント分離を provider 実装の正しさに依存させない）。
//!
//! `EngineCore::search` は `rls.rs::PrefilterSnapshot`（TASK-133）を
//! [`PrefilterCache`] 経由でキャッシュし、毎クエリの `VectorArena::build_filtered`
//! 再構築を避ける（TASK-169・対象ビヘイビア: RLS-1〜4）。キャッシュキーは
//! `(table, ctx)` の完全一致で、失効は `storage.rs::bump_generation_and_commit`
//! による単調増加世代（[`crate::storage::Storage::current_generation`]）との
//! 前後照合で検出する。世代不一致・ロック毒化・スナップショット自身の
//! `ContextMismatch`/`IndexStale` はいずれも「キャッシュを使わない」側へ縮退し
//! （[`EngineCore::search_uncached`] を都度 1 回だけ呼ぶ）、stale なインデックスの
//! 結果を返す経路を作らない（fail-closed。詳細は [`PrefilterCache`] のドキュメント
//! 参照）。構築完了から挿入までの間に世代が進んだ（挿入対象自身が stale になった）場合も
//! 同様に [`PrefilterCache::insert`] が `None` を返し、呼び出し元は
//! `search_uncached` へ 1 回だけ縮退する（TASK-169・Issue #280。stale な
//! スナップショットを呼び出し元へ露出させない）。可視性判定・provider 結果の
//! 二重防御（構築時フィルタ＋`provider_result_is_valid`）はキャッシュ経路・
//! 非キャッシュ経路のいずれでも
//! `PrefilterSnapshot::search_with`／[`EngineCore::search_uncached`] が同一に適用する。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::CatalogError;
use crate::dispatch::{self, DispatchError, DispatchInput, ExecutionPath};
use crate::kernel::{CandidateHit, KernelError, SearchHit, SearchInput, SearchProvider};
use crate::policy::{PolicyContext, PolicyError};
use crate::recovery::required_op_id::{LedgerMode, OperationId};
use crate::rls::{ImplicitRlsHook, PrefilterSnapshot, RlsError};
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
///
/// 第 3 引数は「可視行の id → その id を持つ可視行の件数」の多重集合（[`visible_id_counts`]
/// で構築する）。単なる `HashSet<u64>` ではないのは、行 `id` の一意性スコープが
/// テナント内に閉じた（対象ビヘイビア: TABLE-12）ことで、1 つの `PolicyContext` から
/// 可視な行に同一 `id` が複数現れうるため（自テナント行と、他テナントの `Public` 行が
/// 同じ `id` を持つ場合）。件数を上限として重複を許すことで、(4) の防御力
/// （「provider は実在する可視行の数を超えて同じ id を返せない」）を落とさずに
/// TABLE-12 が正当化する構成を受理する。set へ戻す（＝重複を一律拒否する）と、
/// 他テナントが同じ `id` の `Public` 行を作るだけで検索が失敗する
/// テナント間の可用性干渉になるため戻してはならない。
pub(crate) fn provider_result_is_valid(
    hits: &[CandidateHit],
    k: usize,
    visible_id_counts: &HashMap<u64, usize>,
) -> bool {
    // (1) 件数が要求 k を超えない。
    if hits.len() > k {
        return false;
    }
    let mut seen_ids: HashMap<u64, usize> = HashMap::with_capacity(hits.len());
    let mut prev: Option<&CandidateHit> = None;
    for hit in hits {
        // (2) スコアが有限（NaN/Inf でない）。非有限スコアは全順序を持たず、後続の順序
        // 検証（`total_cmp`）が無意味になるため他の検証より先に弾く。
        if !hit.score.is_finite() {
            return false;
        }
        // (3) 縮約ビュー（＝可視行）の id 集合に属する（不可視 id・捏造 id の拒否）。
        let Some(available) = visible_id_counts.get(&hit.id) else {
            return false;
        };
        // (4) 同じ id の出現回数が、その id を持つ可視行の実数を超えない
        // （同じ行を複数回返す provider の拒否。TABLE-12 により id の重複自体は
        // 起こりうるため、集合ではなく多重集合で判定する。上記ドキュメント参照）。
        let seen = seen_ids.entry(hit.id).or_insert(0);
        *seen = match seen.checked_add(1) {
            Some(next) => next,
            None => return false,
        };
        if *seen > *available {
            return false;
        }
        // (5) スコア降順・同点は**候補識別子**の昇順（`kernel.rs::CpuScalarProvider` が
        // 実際に返す順序と同じ契約。識別子は呼び出し元定義で、`core.rs`・`sql/exec.rs` は
        // アリーナのスロット番号を渡すため実質 `(tenant_id, id)` 昇順になり、単一テナント
        // 内では従来どおり行 `id` 昇順と一致する。`docs/design/rrf-tie-break-determinism.md`
        // の順序契約〈安定ソート + 識別子昇順タイブレーク〉と整合）。`total_cmp` は (2) で
        // 有限性を確認済みのため NaN の順序上の扱いには依存しない。
        if let Some(p) = prev {
            // 同点時は識別子の昇順。TABLE-12 により同点・同 id の並びが正当に起こりうる
            // （異なるテナントの同一 id 行が同じスコアになる場合）ため、狭義単調増加
            // （`p.id >= hit.id` を違反とする）ではなく広義単調増加で判定する。
            // 同一行の重複返却は (4) の多重集合上限が引き続き遮断する。
            let out_of_order = match p.score.total_cmp(&hit.score) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => p.id > hit.id,
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

/// 可視行の id 列（`arena.ids()` 等）から [`provider_result_is_valid`] の第 3 引数
/// （id → 可視行件数の多重集合）を構築する共通ヘルパ。`core.rs`・`rls.rs`・
/// `sql/exec.rs` の 3 経路が同一の構築方法を共有し、片側だけが集合へ退化するのを防ぐ
/// （TABLE-12 の重複 id の扱いは 1 か所で決める）。
/// 候補アリーナのスロット番号（0..n）を provider 入力用の `Vec<u64>` として作る
/// （対象ビヘイビア: TABLE-12）。
///
/// provider へ渡す識別子に行 `id` を使えないのは、行 `id` の一意性スコープが
/// テナント内に閉じており、1 つの可視集合に同じ `id` の行が複数含まれうるため
/// （自テナント行と他テナントの `Public` 行）。スロット番号は可視集合内で一意かつ
/// `(tenant_id, id)` の行と 1 対 1 に対応するので、provider 結果の重複検証・
/// ヒットの行解決の双方が曖昧にならない（`sql/exec.rs` も同じ方式）。
/// 確保はフォールブル（`try_reserve_exact`）で行う（coding-rust.md「無制限確保禁止」）。
pub(crate) fn slot_ids_for(arena: &VectorArena) -> Result<Vec<u64>, ArenaError> {
    let len = arena.ids().len();
    let mut slot_ids: Vec<u64> = Vec::new();
    slot_ids
        .try_reserve_exact(len)
        .map_err(|e| ArenaError::AllocationFailed(format!("failed to reserve slot ids: {e}")))?;
    for slot in 0..len {
        let slot_id = u64::try_from(slot).map_err(|_| ArenaError::CapacityExceeded)?;
        slot_ids.push(slot_id);
    }
    Ok(slot_ids)
}

/// provider が返した候補ヒット（識別子はスロット番号）を、テナント修飾済みの
/// 公開ヒット [`CandidateHit`] → [`crate::kernel::SearchHit`] へ解決する
/// （対象ビヘイビア: TABLE-12・RLS-9。codex-review P1 指摘・PR #194）。
///
/// スロットが範囲外（provider の契約違反・データ不整合）の場合は `None` を返し、
/// 呼び出し元は結果を一切返さず fail-closed に拒否する（部分的な解決はしない）。
/// `String` 確保はここ（高々 `k` 件）でのみ発生し、候補行ごとに走る
/// `TopKSelector::push` のホットパスには入らない。
pub(crate) fn resolve_slot_hits(
    arena: &VectorArena,
    candidates: &[CandidateHit],
) -> Option<Vec<SearchHit>> {
    let mut out = Vec::new();
    out.try_reserve_exact(candidates.len()).ok()?;
    for hit in candidates {
        let slot = usize::try_from(hit.id).ok()?;
        let tenant = arena.tenant_id(slot)?;
        let id = *arena.ids().get(slot)?;
        out.push(SearchHit::new(tenant, id, hit.score));
    }
    Some(out)
}

pub(crate) fn visible_id_counts(ids: &[u64]) -> HashMap<u64, usize> {
    let mut counts: HashMap<u64, usize> = HashMap::with_capacity(ids.len());
    for id in ids {
        let entry = counts.entry(*id).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    counts
}

/// [`PrefilterCache`] のエントリ数上限（TASK-169・security.md「不安全な設計｜
/// 無制限リソース確保（DoS）」対応）。線形走査で照合するキャッシュのため、上限は
/// 実用上の再利用シナリオ（少数テーブル × 少数ポリシーの組み合わせ）を十分満たしつつ、
/// 走査コストが問題にならない桁に留める。
const MAX_PREFILTER_CACHE_ENTRIES: usize = 32;

/// [`PrefilterCache`] が保持するスナップショット群の概算バイト量の合計上限（TASK-169）。
/// アリーナ本体の常駐上限（[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）と同じ桁に揃え、
/// キャッシュ全体の常駐メモリがアリーナ 1 個分の上限を大きく超えないようにする。
const MAX_PREFILTER_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`PrefilterCache`] の観測用統計（TASK-169）。テナント ID・行 ID 等の機微情報は
/// 一切含まない（カウンタのみ。security.md「情報漏えい」対応）。`VectorCore` trait
/// には載せない固有 API（`EngineCore::prefilter_cache_stats`）としてのみ公開する。
#[derive(Debug, Clone, Copy, Default)]
pub struct PrefilterCacheStats {
    /// キャッシュヒット数（世代整合まで確認できた再利用）。
    pub hits: u64,
    /// キャッシュミス数（未登録、または世代不一致で破棄した後の再構築）。
    pub misses: u64,
    /// 世代不一致・`search_with` 内の `IndexStale` 検出による破棄回数。
    pub stale_evictions: u64,
    /// 容量上限超過による LRU 追い出し回数。
    pub capacity_evictions: u64,
    /// 現在キャッシュが保持しているエントリ数。
    pub entries: usize,
}

/// [`PrefilterCache`] の 1 エントリ（TASK-169）。`table`・`ctx` の組がキャッシュキー
/// （`PolicyContext` は `Hash` を実装しないため `HashMap` ではなく `Vec` 線形走査で
/// 照合する。エントリ数は [`MAX_PREFILTER_CACHE_ENTRIES`] で小さく抑えるため走査コストは
/// 問題にならない）。
struct CacheEntry {
    table: String,
    snapshot: Arc<PrefilterSnapshot>,
    /// LRU 追い出し判定用の単調シーケンス（アクセスのたびに更新）。
    last_used: u64,
}

/// ロックが保護する可変状態（[`RwLock`] 内側）。
#[derive(Default)]
struct CacheState {
    entries: Vec<CacheEntry>,
}

/// `EngineCore::search` が再利用する `rls.rs::PrefilterSnapshot` の世代整合キャッシュ
/// （TASK-133 の `PrefilterIndex` を製品検索経路へ配線する TASK-169・対象ビヘイビア:
/// RLS-1〜4）。
///
/// **キー**: `(table, ctx)` の完全一致。`PolicyContext` はテナント ID・許可可視性集合を
/// 含む値のため、キーが一致する限り「同じテナント・同じ可視性境界で構築された
/// スナップショット」であることが保証される（構築時フィルタの根拠は
/// `PrefilterSnapshot::build` 側にある。本キャッシュはそれを取り違えないことのみ担保する）。
///
/// **失効**: [`Self::lookup`] はヒット時に `snapshot.built_generation()` を
/// `storage.current_generation()` と突き合わせ、不一致（または世代読み取り自体の失敗）
/// なら該当エントリを破棄してミス扱いにする（fail-closed: 古い可能性のあるものは
/// 使わない）。それでも `search_with` 内で `IndexStale`/`ContextMismatch` を検出した
/// 場合（他スレッドによる直後の書き込みでの競合等）は [`Self::evict`] で破棄し、
/// 呼び出し元（`EngineCore::search`）が非キャッシュ経路へ 1 回だけ縮退する
/// （`core.rs` モジュールドキュメント参照。stale な結果を返す経路は存在しない）。
///
/// **同期**: [`RwLock`] は書き込みロック（LRU の `last_used` 更新のため）で `Arc` を
/// clone した後すぐに解放し、検索自体はロック外で実行する（ロック保持を lookup/insert
/// の短時間に限定する）。ロック毒化
/// （パニックしたスレッドが保持中だった場合）は `unwrap` せず「キャッシュ無効」として
/// 縮退する（fail-closed。.claude/rules/coding-rust.md: 受信データ経路で `unwrap` を
/// 使わない方針をキャッシュ層にも適用する）。
///
/// **容量**: [`MAX_PREFILTER_CACHE_ENTRIES`]・[`MAX_PREFILTER_CACHE_TOTAL_BYTES`] を
/// 超えないよう、挿入時に (1) 現在世代と一致しないエントリを先に全破棄し、(2) それでも
/// 超過するなら `last_used` が最小のエントリから追い出す（LRU）。単体で総量上限を
/// 超えるスナップショットはキャッシュへ挿入せず、その 1 回の検索限りで使い捨てる
/// （常駐させない）。
pub(crate) struct PrefilterCache {
    state: RwLock<CacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
}

impl PrefilterCache {
    fn new() -> Self {
        Self {
            state: RwLock::new(CacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
        }
    }

    /// `(table, ctx)` に一致し、かつ現在の世代と整合するエントリを探す。
    /// 世代不一致のエントリは見つけ次第破棄する（fail-closed。上記型ドキュメント参照）。
    /// ロック毒化・世代読み取り失敗はいずれも「見つからなかった」として扱う。
    fn lookup(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<Arc<PrefilterSnapshot>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        let mut guard = self.state.write().ok()?;
        // 世代はロック取得後に読み直す（github-actions/codex-review P1 指摘）。
        // ロック取得を待っている間に他スレッドが新しい世代のエントリを挿入し得るため、
        // ロック取得前に読んだ古い世代値のままだと、その新しい有効エントリを
        // 「不一致」と誤判定して破棄してしまう（fail-closed の意図に反する誤破棄）。
        // ロック保持中に読む値なら、この呼び出し内で以降エントリが変化しないことを
        // 保証できる。
        let current_generation = storage.current_generation().ok()?;
        let position = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)?;
        // Vec::get だけでは境界チェック後に別スレッドが縮めうる余地はない
        // （書き込みロック保持中のため、この関数呼び出し内で `guard.entries` は
        // 単独所有されている）。添字アクセス自体は自明に有効な `position` に対してのみ
        // 行う。
        let stale = guard
            .entries
            .get(position)
            .map(|e| e.snapshot.built_generation() != current_generation)
            .unwrap_or(true);
        if stale {
            guard.entries.remove(position);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let snapshot = {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            Arc::clone(&entry.snapshot)
        };
        drop(guard);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(snapshot)
    }

    /// 新規構築したスナップショットを挿入する。挿入対象自身が既に古い（並行書き込みで
    /// 世代が進んだ）場合・世代を確認できない場合は `None` を返し、キャッシュへ
    /// 反映しないだけでなく呼び出し元へも一切渡さない（Issue #280。
    /// [`DictionaryCache::insert`] と同じ契約に統一。以前は判定不能・stale のいずれも
    /// 構築済みの `Arc` をそのまま返しており、呼び出し元は世代不一致を承知の上で
    /// stale なスナップショットを一時的に検索へ使う経路が残っていた。`search_with`
    /// 側の前後世代照合で結果自体が漏れることはなかったが〔`core.rs` モジュール
    /// ドキュメント参照〕、契約としては [`DictionaryCache::insert`] と食い違って
    /// いた）。単体で総量上限を超える場合はキャッシュしないが、現在世代と整合済み
    /// なので呼び出し元へは `Some` で返し、その場限りで使う（型ドキュメント参照）。
    ///
    /// `storage` は (0) 挿入対象自身の世代整合チェックと (1) の世代不整合エントリの
    /// 一括破棄で「現在の実世代」（[`Storage::current_generation`]）を判定するために
    /// 使う。以前は `snapshot.built_generation()`（= このスナップショット自身の構築
    /// 時点の世代）を現在世代の代用にしていたが、これは挿入対象のスナップショットが
    /// 並行書き込みで既に古くなっている場合、真に新しい（現在世代と一致する）既存
    /// エントリまで「不一致」として誤って全破棄してしまう不具合があった
    /// （Cursor Bugbot 指摘）。
    /// `storage.current_generation()` の読み取りに失敗した場合、およびロックが
    /// 毒化している場合は世代整合を判定できないため `None` を返す（fail-closed:
    /// 「判定できないなら使わせない」側へ倒す。[`DictionaryCache::insert`] と同じ方針）。
    ///
    /// 世代の読み取りは書き込みロック取得後に行う（github-actions/codex-review P1
    /// 指摘）。ロック取得前に読むと、ロック待機中に他スレッドがより新しい世代の
    /// エントリを挿入し得るため、その古い世代値のまま (1) の一括破棄を行うと、
    /// 直後に挿入されたばかりの真に新しい有効エントリまで「不一致」として誤って
    /// 削除してしまう。ロック保持中に読んだ値なら、この呼び出し内で以降エントリが
    /// 変化しないことを保証できる。
    fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        snapshot: PrefilterSnapshot,
    ) -> Option<Arc<PrefilterSnapshot>> {
        let snapshot = Arc::new(snapshot);
        self.misses.fetch_add(1, Ordering::Relaxed);

        // ロック毒化時は世代整合を判定できないため `None`（fail-closed。
        // [`DictionaryCache::insert`] と同じ契約に統一。Issue #280）。
        let Ok(mut guard) = self.state.write() else {
            return None;
        };

        // (0) 挿入対象自身が現在世代と一致するか確認する（対象外スレッド指摘の修正）。
        // 世代が読み取れない、または挿入対象が既に古い場合はキャッシュへ反映せず、
        // 呼び出し元へも一切渡さない（`None`。Issue #280）。ここでリターンすることで、
        // 後続の「同一キー破棄」ステップに到達させない。すなわち並行書き込みで自身が
        // stale になった挿入が、別スレッドが直前に挿入した現在世代の有効エントリを
        // 上書き・削除する経路を断つ（型ドキュメント参照）。ロック保持中に読むため、
        // 以降のこの関数内の判定と齟齬が生じない。
        let Ok(current_generation) = storage.current_generation() else {
            return None;
        };
        if snapshot.built_generation() != current_generation {
            return None;
        }

        // ここまで到達した時点で `snapshot` 自身は現在世代と一致していることが
        // 保証されている。以降の容量判定・LRU 追い出しはキャッシュへの常駐可否のみを
        // 左右し、呼び出し元へ `Some` で返すことは変えない。
        let own_bytes = snapshot.approx_heap_bytes();
        if own_bytes > MAX_PREFILTER_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超えるスナップショットは常駐させない（DoS 対策。
            // 型ドキュメント参照）。呼び出し元へはそのまま返し、1 回の検索限りで使う。
            return Some(snapshot);
        }

        // 同一 (table, ctx) キーの既存エントリは挿入前に取り除く（Cursor Bugbot 指摘:
        // 常に push するだけだと同一キーが重複登録され、[`Self::lookup`] は先頭一致
        // しか参照しないため後続の重複が [`MAX_PREFILTER_CACHE_ENTRIES`] を無駄に
        // 消費し続ける）。キーは `(table, ctx)` の完全一致（型ドキュメント参照）。
        // 上記 (0) により、ここに到達する時点で `snapshot` 自身は現在世代と一致
        // していることが保証されている（stale な挿入で既存の有効エントリを消さない）。
        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)
        {
            guard.entries.remove(pos);
        }

        // (1) 現在世代と不整合なエントリを先に全破棄する（型ドキュメント参照）。
        // 現在世代は (0) で読み取り済みの `current_generation`（挿入対象自身の世代と
        // 一致することを確認済み）をそのまま使う（挿入対象スナップショット自身の世代を
        // 代用しない、という以前の修正意図は変わらない。二重読み取りによるレースを
        // 避けるため同一ロック内で 1 回だけ読んだ値を使い回す）。
        let before = guard.entries.len();
        guard
            .entries
            .retain(|e| e.snapshot.built_generation() == current_generation);
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        // (2) それでも件数・総量が上限を超えるなら `last_used` 最小から追い出す。
        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.snapshot.approx_heap_bytes())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_PREFILTER_CACHE_ENTRIES
            || total_bytes.saturating_add(own_bytes) > MAX_PREFILTER_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                // これ以上追い出せるエントリがない（空）のに超過している場合は、
                // 挿入自体を諦めてキャッシュを汚さない（type ドキュメント参照）。
                // 挿入対象自身は現在世代と整合済みなので `Some` で返す。
                return Some(snapshot);
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.snapshot.approx_heap_bytes());
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(CacheEntry {
            table: table.to_string(),
            snapshot: Arc::clone(&snapshot),
            last_used: seq,
        });
        Some(snapshot)
    }

    /// `search_with` が返した `IndexStale`/`ContextMismatch` を受けて、該当エントリを
    /// 破棄する（他スレッドの直後の書き込みとの競合等。型ドキュメント参照）。
    /// `Arc::ptr_eq` で対象を特定するため、既に別のスナップショットへ差し替わっていた
    /// 場合は何もしない（誤って無関係なエントリを破棄しない）。
    fn evict(&self, table: &str, snapshot: &Arc<PrefilterSnapshot>) {
        let Ok(mut guard) = self.state.write() else {
            return;
        };
        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && Arc::ptr_eq(&e.snapshot, snapshot))
        {
            guard.entries.remove(pos);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> PrefilterCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        PrefilterCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            entries,
        }
    }
}

/// [`DictionaryCache`] のエントリ数上限（TASK-109・PLAN-5。[`PrefilterCache`]
/// （TASK-169）と同じ DoS 対策方針を踏襲する）。
const MAX_DICTIONARY_CACHE_ENTRIES: usize = 16;

/// [`DictionaryCache`] が保持する [`crate::dictionary::Dictionary`] 群の概算バイト量の
/// 合計上限（[`MAX_PREFILTER_CACHE_TOTAL_BYTES`] と同じ桁に揃える）。
const MAX_DICTIONARY_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`DictionaryCache`] の 1 エントリ。`table`・`ctx` の組がキャッシュキー
/// （[`PrefilterCache`] の `CacheEntry` と同じ理由で `HashMap` ではなく `Vec` 線形走査）。
struct DictCacheEntry {
    table: String,
    ctx: PolicyContext,
    dictionary: Arc<crate::dictionary::Dictionary>,
    /// `dictionary` から 1 度だけ構築した正規化索引（[`crate::tiering::classify`]
    /// が使う）。`dictionary` と同じ寿命（同一世代の間だけキャッシュされ、世代が
    /// 変われば両方まとめて破棄・再構築される）で共有することで、
    /// `crate::tiering::classify` がリクエストごとに辞書全量を再正規化する経路を
    /// 断つ（codex-review P1 指摘対応・PR #261。`tiering.rs::NormalizedDictionaryIndex`
    /// ドキュメント参照）。
    normalized_index: Arc<crate::tiering::NormalizedDictionaryIndex>,
    built_generation: u64,
    approx_bytes: usize,
    /// LRU 追い出し判定用の単調シーケンス（アクセスのたびに更新）。
    last_used: u64,
}

/// ロックが保護する可変状態（[`RwLock`] 内側）。
#[derive(Default)]
struct DictCacheState {
    entries: Vec<DictCacheEntry>,
}

/// `EngineCore::dictionary_snapshot` が再利用する辞書的情報源（TASK-109・PLAN-5。
/// ポインタ: `docs/spec/04-behavior/query-planning.md` PLAN-5）の世代整合キャッシュ。
/// [`PrefilterCache`]（TASK-169）と同一の失効規約（fail-closed・世代不一致で破棄・
/// ロック取得後に世代を読み直す・容量超過は LRU 追い出し）を踏襲する。
///
/// **失効の粒度**: `storage.current_generation()` はテーブル・書き込み種別を問わず
/// 任意の write commit で単調増加する（[`Storage::current_generation`] ドキュメント
/// 参照）。そのため本キャッシュはこのテーブル自身への書き込みだけでなく、無関係な
/// 他テーブルへの書き込みでも保守的に失効する（テーブル単位の精密な失効は持たない）。
/// これは意図的な単純化であり、誤って古い辞書を返す経路（fail-open）よりも安全側
/// （過剰な再構築）に倒す設計判断である（security.md「fail-closed を維持する」）。
///
/// **再構築のトリガー**: ファイル形 `INSERT`（単発・バッチとも）は
/// `tenant::replace_typed_rows_by_text_key` が世代を bump するため、次回
/// `dictionary_snapshot` 呼び出し時に自動的に再構築され増分インデックスの結果が
/// 反映される（TASK-120 との連動）。本キャッシュは post-commit フックを持たず、
/// 参照時に世代を突き合わせるだけの構成のため、バッチ途中失敗時の不整合や
/// プロセス再起動時の消失を気にする必要がない（`redb` からの再構築で自己回復する）。
pub(crate) struct DictionaryCache {
    state: RwLock<DictCacheState>,
    seq: AtomicU64,
}

impl DictionaryCache {
    fn new() -> Self {
        Self {
            state: RwLock::new(DictCacheState::default()),
            seq: AtomicU64::new(0),
        }
    }

    /// `(table, ctx)` に一致し、現在世代と整合するエントリを探す。世代不一致・
    /// ロック毒化・世代読み取り失敗はいずれも「見つからなかった」として扱う
    /// （fail-closed。[`PrefilterCache::lookup`] と同じ方針）。
    /// キャッシュヒット時は `(dictionary, normalized_index)` を返す（両者は
    /// 同一エントリ・同一世代から `Arc::clone` するだけで、辞書の再走査・
    /// 正規化索引の再構築のいずれも発生しない）。
    fn lookup(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<(
        Arc<crate::dictionary::Dictionary>,
        Arc<crate::tiering::NormalizedDictionaryIndex>,
    )> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.state.write().ok()?;
        // 世代はロック取得後に読み直す（`PrefilterCache::lookup` と同じ理由。
        // ロック取得前に読むとロック待機中の他スレッドの挿入を誤って「不一致」と
        // 判定しうる）。
        let current_generation = storage.current_generation().ok()?;
        let position = guard
            .entries
            .iter()
            .position(|e| e.table == table && &e.ctx == ctx)?;
        let stale = guard
            .entries
            .get(position)
            .map(|e| e.built_generation != current_generation)
            .unwrap_or(true);
        if stale {
            guard.entries.remove(position);
            return None;
        }
        let result = {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            (
                Arc::clone(&entry.dictionary),
                Arc::clone(&entry.normalized_index),
            )
        };
        Some(result)
    }

    /// 新規構築した辞書を挿入する。単体で総量上限を超える場合はキャッシュしないが
    /// 呼び出し元へは返す（[`PrefilterCache::insert`] と同じ方針）。挿入対象自身が
    /// 既に古い（並行書き込みで世代が進んだ）場合・世代を確認できない場合は
    /// `None` を返し、呼び出し元へは一切渡さない（PR #249 codex-review P1
    /// 指摘対応: 従来はこの場合もキャッシュへの反映だけを諦め、構築済みの
    /// `Arc` はそのまま呼び出し元へ返していた。`dictionary_snapshot` 側は
    /// 行走査完了後に一度 `observed_generation` を確認しているが、その確認と
    /// 本関数の書き込みロック取得の間に別の書き込みがコミットされる競合が
    /// あり、その場合でも「正常に構築できたスナップショット」として書き込み
    /// 未反映の辞書を返してしまっていた。世代確認・キャッシュ反映可否の決定を
    /// この関数内の単一のロック区間へ統合し、呼び出し元
    /// `EngineCore::dictionary_snapshot` は `None` を「再試行のシグナル」として
    /// 扱う）。
    fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        dictionary: crate::dictionary::Dictionary,
        built_generation: u64,
        approx_bytes: usize,
    ) -> Option<(
        Arc<crate::dictionary::Dictionary>,
        Arc<crate::tiering::NormalizedDictionaryIndex>,
    )> {
        // 正規化索引（`tiering::classify` が使う小文字化済みパス・シンボル名）は
        // ここ・辞書スナップショット構築の 1 回だけ構築する（codex-review P1
        // 指摘対応・PR #261。[`DictCacheEntry::normalized_index`] ドキュメント
        // 参照）。以後は `dictionary` と同じ `Arc` 共有・同じ失効規約で使い回す。
        let normalized_index = Arc::new(crate::tiering::NormalizedDictionaryIndex::build(
            &dictionary,
        ));
        // 正規化索引の保持分（小文字化パス・シンボル名の複製）も容量上限の対象に
        // 含める（漏らすと CPU-DoS 対策のつもりが総量上限の過小評価という別の
        // メモリ上限 P1 になる）。
        let approx_bytes = approx_bytes.saturating_add(normalized_index.approx_heap_bytes());
        let dictionary = Arc::new(dictionary);

        let Ok(mut guard) = self.state.write() else {
            // ロック毒化時は世代整合を検証できないため fail-closed 側へ倒し、
            // 呼び出し元へは何も返さない（Issue #280 で `PrefilterCache::insert` も
            // 同契約に統一済み。本関数は世代確認自体をこのロック区間に統合したため、
            // ロックが取れない時点で世代確認もできていない）。
            return None;
        };

        let Ok(current_generation) = storage.current_generation() else {
            return None;
        };
        if built_generation != current_generation {
            // 挿入対象自身が既に古い（並行書き込みで世代が進んだ）。キャッシュへ
            // 反映しないだけでなく、呼び出し元へも渡さない（上記ドキュメント参照）。
            return None;
        }

        if approx_bytes > MAX_DICTIONARY_CACHE_TOTAL_BYTES {
            return Some((dictionary, normalized_index));
        }

        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && &e.ctx == ctx)
        {
            guard.entries.remove(pos);
        }
        guard
            .entries
            .retain(|e| e.built_generation == current_generation);

        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.approx_bytes)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_DICTIONARY_CACHE_ENTRIES
            || total_bytes.saturating_add(approx_bytes) > MAX_DICTIONARY_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return Some((dictionary, normalized_index));
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.approx_bytes);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(DictCacheEntry {
            table: table.to_string(),
            ctx: ctx.clone(),
            dictionary: Arc::clone(&dictionary),
            normalized_index: Arc::clone(&normalized_index),
            built_generation,
            approx_bytes,
            last_used: seq,
        });
        Some((dictionary, normalized_index))
    }
}

/// `rls.rs::RlsError` を `CoreError` へ写像する（TASK-169）。呼び出し元
/// （[`EngineCore::search`]）は `IndexStale`/`ContextMismatch` をキャッシュ縮退の
/// トリガーとして先に `match` で処理するため、本関数へはそれ以外の 6 variant のみが
/// 渡る契約とする（呼び出し元の網羅性はコンパイラの exhaustiveness ではなく
/// 呼び出し順序で担保するため、防御的に到達した場合も fail-closed な variant
/// （`ProviderResultRejected`）へ丸め込まず、意味的に対応する variant へ写像する）。
fn map_rls_error(e: RlsError) -> CoreError {
    match e {
        RlsError::Arena(e) => CoreError::Arena(e),
        RlsError::Kernel(e) => CoreError::Kernel(e),
        RlsError::Storage(e) => CoreError::Storage(e),
        RlsError::InvalidK { k } => CoreError::InvalidK { k },
        RlsError::NotFound => CoreError::NotFound,
        RlsError::ProviderResultRejected => CoreError::ProviderResultRejected,
        // 呼び出し元が先に処理する契約の防御的分岐（型ドキュメント参照）。ここへ到達した
        // 場合も fail-closed に拒否する。
        RlsError::ContextMismatch | RlsError::IndexStale => CoreError::ProviderResultRejected,
    }
}

/// `VectorCore` 公開 API のエラー型。下位層（`storage`/`catalog`/`arena`/`kernel`/`policy`）
/// のエラーを一本化しつつ、不可視行と不存在行を [`CoreError::NotFound`] に統合する
/// （呼び出し元へ存在情報を漏らさないため。エラーメッセージはプログラム出力文字列のため英語）。
///
/// `#[non_exhaustive]` は付与しない: 本 enum は既に公開済みであり、後付けで
/// `#[non_exhaustive]` を付けると下流の網羅的 `match` がコンパイル不能になる
/// （それ自体が破壊的変更のため、`#[non_exhaustive]` 化で互換性を装うのではなく
/// 付けないままにする。codex-review PR #252 P1 指摘）。`QueryPlannerUnavailable`・
/// `QueryPlanning`（TASK-110・PLAN-1）・`EmbedderUnavailable`・`QueryEmbedding`
/// （TASK-114・PLAN-10）を含む variant の追加は、`non_exhaustive` 化の有無に
/// 関わらず既存の網羅的 `match` を壊す破壊的変更であることに変わりはない
/// （PR 本文の変更点に明記する）。
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
    /// `SearchProvider` が返却した `Vec<`[`CandidateHit`]`>` が Top-k の契約
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
    /// LLM クエリプランニング（TASK-110・PLAN-1）の [`Self::query_planner`] が
    /// 未注入だった（`embedder` 未構成時のファイル形 `INSERT` 拒否と同じ
    /// fail-closed 方針。既定で参照実装を暗黙採用しない）。
    QueryPlannerUnavailable,
    /// LLM クエリプランニング（TASK-110・PLAN-1）のプロンプト組み立て・LLM 呼び出し・
    /// 応答パースのいずれかが失敗した（詳細は [`crate::query_planner::PlanError`]）。
    QueryPlanning(crate::query_planner::PlanError),
    /// 再埋め込み規則（TASK-114・PLAN-10）の [`Self::plan_and_embed_query`] が
    /// 呼ばれたが [`Self::embedder`] が未注入だった（ファイル形 `INSERT` の
    /// embedder 未構成拒否と同じ fail-closed 方針。既定で参照実装を暗黙採用しない）。
    EmbedderUnavailable,
    /// 再埋め込み規則（TASK-114・PLAN-10）の [`Self::plan_and_embed_query`] における
    /// 再埋め込み（[`crate::query_planner::reembed_expansion`]）が失敗した
    /// （テーブル宣言次元と embedder 次元の不一致・埋め込みサービスの不正応答等。
    /// 詳細は [`crate::embedding::EmbedError`]）。
    QueryEmbedding(crate::embedding::EmbedError),
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
            CoreError::QueryPlannerUnavailable => write!(f, "no query planner configured"),
            CoreError::QueryPlanning(e) => write!(f, "core query planning error: {e}"),
            CoreError::EmbedderUnavailable => write!(f, "no embedder configured"),
            CoreError::QueryEmbedding(e) => write!(f, "core query embedding error: {e}"),
        }
    }
}

impl std::error::Error for CoreError {}

/// [`EngineCore::open_with_engine`] 専用のエラー型（Issue #407・ANN opt-in 結線）。
///
/// `open_with_engine` は本 Issue で新設する API のため、既に公開済みの
/// [`CoreError`]（`#[non_exhaustive]` を付けない既存方針。新規 variant 追加は
/// 既存利用者の網羅的 `match` を壊す破壊的変更になる）へ variant を追加せず、
/// 独立の型として新設した（codex-review P1 指摘・Issue #407 追記。AGENTS.md
/// 「公開 API・エラー契約の互換性（P1）」が要求する spec 側の対応する定義変更が
/// 無いまま `CoreError` を破壊的変更するのを避けるため）。`CoreError` への
/// 暗黙 `From` 変換は用意しない（`?` 経由で意図せず `CoreError` 側へ新しい
/// エラー経路が生まれるのを防ぐ）。
#[derive(Debug)]
pub enum OpenWithEngineError {
    /// `Storage::open` が失敗した（詳細は [`StorageError`]）。
    Storage(StorageError),
    /// `kind` の [`crate::hnsw::HnswParams::validate`] が拒否した（詳細は
    /// [`crate::search_engine::SearchEngineError`]）。
    SearchEngine(crate::search_engine::SearchEngineError),
}

impl std::fmt::Display for OpenWithEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenWithEngineError::Storage(e) => write!(f, "core storage error: {e}"),
            OpenWithEngineError::SearchEngine(e) => write!(f, "core search engine error: {e}"),
        }
    }
}

impl std::error::Error for OpenWithEngineError {}

impl From<DispatchError> for CoreError {
    fn from(e: DispatchError) -> Self {
        CoreError::Dispatch(e)
    }
}

impl From<crate::query_planner::PlanError> for CoreError {
    fn from(e: crate::query_planner::PlanError) -> Self {
        CoreError::QueryPlanning(e)
    }
}

/// [`EngineCore::query_planner`] が保持する注入形態（TASK-115・PLAN-8）。単一クライアント
/// （TASK-110 の既存契約。[`EngineCore::with_query_planner`]）と、質問類型（`crate::tiering`）
/// に応じてティア別クライアントを振り分ける構成（[`EngineCore::with_tiered_query_planner`]）の
/// 二択を型で表し、両方同時設定という不整合状態を作れなくする。
enum PlannerBinding {
    /// TASK-110 時点の単一クライアント注入（既存の [`EngineCore::with_query_planner`]・
    /// [`EngineCore::plan_query`] の挙動・契約を変えない）。
    Single(Box<dyn crate::query_planner::LlmClient>),
    /// TASK-115・PLAN-8 のティア別クライアント注入（`crate::tiering::TieredPlanner`）。
    Tiered(crate::tiering::TieredPlanner),
}

impl From<crate::embedding::EmbedError> for CoreError {
    fn from(e: crate::embedding::EmbedError) -> Self {
        CoreError::QueryEmbedding(e)
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
    ///
    /// 返却される [`SearchHit`] は `(tenant_id, id)` で行を一意に指す（対象ビヘイビア:
    /// TABLE-12・RLS-9。codex-review P1 指摘・PR #194 対応）。行 `id` の一意性スコープは
    /// テナント内に閉じているため**同一 `id` の hit が複数返り得る**（自テナントの行と、
    /// 他テナントの `Public` 行が同じ `id` を持つ場合。いずれも `ctx` から可視な行であり、
    /// テナント境界の侵害ではない）が、`tenant_id` が付随するため呼び出し元は常に
    /// 両者を判別でき、`get_row(ctx, table, &hit.tenant_id, hit.id)` でそのヒット自身の
    /// 行を取得できる。
    ///
    /// 順序はスコア降順・同点は候補識別子（内部のスロット番号。実質 `(tenant_id, id)`
    /// 昇順で、単一テナント内では行 `id` 昇順と一致）で決定的
    /// （`docs/design/rrf-tie-break-determinism.md` の順序契約）。
    fn search(
        &self,
        ctx: &PolicyContext,
        table: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, CoreError>;

    /// `table` から `id` の行を 1 件取得する。不可視・不存在は区別せず
    /// [`CoreError::NotFound`] を返す。
    /// `(tenant_id, id)` で行を 1 件取得する（対象ビヘイビア: TABLE-12・RLS-9）。
    ///
    /// 行 `id` の一意性スコープはテナント内に閉じているため、行を指すには所有テナントが
    /// 必要（[`crate::kernel::SearchHit::tenant_id`] をそのまま渡せる契約。
    /// codex-review P1 指摘・PR #194）。取得できるのは `ctx` から**可視な**行のみで、
    /// 不存在と不可視は区別せず [`CoreError::NotFound`] に統一する（他テナント行の
    /// 存在探査に使えないようにする fail-closed な扱い。security.md P0）。
    fn get_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        tenant_id: &str,
        id: u64,
    ) -> Result<Row, CoreError>;
}

/// [`EngineCore::plan_using_plan_expansion`] の戻り値。`USING PLAN` 経路の
/// I/O（LLM 展開・再埋め込み）結果のみを保持し、列インデックス解決（スキーマ
/// 依存）は含まない（呼び出し元が I/O 完了後に取得し直した最新スキーマで
/// `sql::using_plan::bind_expansion` へ渡す。同メソッドのドキュメント参照）。
struct UsingPlanExpansionResult {
    expansion: crate::query_planner::QueryExpansion,
    query_vector: Vec<f32>,
    resolved_mode: crate::sql::mode::ResolvedMode,
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
    /// `rls.rs::PrefilterSnapshot` の世代整合キャッシュ（TASK-169）。
    /// [`VectorCore::search`] 実装がこれを経由して事前フィルタインデックスを再利用する
    /// （詳細は [`PrefilterCache`] のドキュメント参照）。
    prefilter_cache: PrefilterCache,
    /// `precision` モードの実行契約を制御するサーバー側設定値（TASK-162・SEARCH-9）。
    /// クエリ・セッション変数から到達できる経路を持たない（差し替えは
    /// [`Self::with_precision_policy`] のみ。`crate::precision` モジュール
    /// ドキュメントの fail-open 不在の設計制約を参照）。
    precision_policy: crate::precision::PrecisionPolicy,
    /// ファイル形 `INSERT`（TASK-120・INDEX-1, INDEX-2）のチャンク本文をベクトルへ
    /// 変換する注入点。未設定（`None`）はファイル形 `INSERT` を fail-closed に
    /// 拒否する契約（既定で参照実装を暗黙採用しない。`embedding.rs` モジュール
    /// ドキュメント参照）。差し替えは [`Self::with_embedder`] のみ。
    embedder: Option<Box<dyn crate::embedding::Embedder>>,
    /// LLM クエリプランニング（TASK-110・PLAN-1／TASK-115・PLAN-8）のクエリ展開
    /// クライアント注入点。未設定（`None`）は [`Self::plan_query`] を fail-closed に
    /// 拒否する契約（`embedder` と同じ流儀。既定で参照実装を暗黙採用しない）。
    /// 単一クライアント注入（[`Self::with_query_planner`]）とティア別クライアント注入
    /// （[`Self::with_tiered_query_planner`]・TASK-115・`crate::tiering`）のどちらか
    /// 一方のみを保持する契約を [`PlannerBinding`] の型で表す（両方同時設定という
    /// 不整合状態を型で作れなくする。`Option<PlannerBinding>` の内側に 2 つ目の
    /// `Option` を並べる設計だと両方 `Some` の状態を許してしまうため採らない）。
    query_planner: Option<PlannerBinding>,
    /// ファイル形 `INSERT` のチャンク化・チャンク数上限設定（TASK-120）。
    /// 差し替えは [`Self::with_incremental_config`] のみ。
    incremental_config: crate::incremental::IncrementalConfig,
    /// `operation_id` 必須化ガード（TASK-92・対象ビヘイビア: RECOVER-1）を制御する
    /// サーバー側構成。クエリ・セッション変数から到達できる経路を持たない（差し替えは
    /// [`Self::with_ledger_mode`] のみ。`crate::recovery::required_op_id` モジュール
    /// ドキュメント参照）。
    ledger_mode: LedgerMode,
    /// 一括投入（複数ファイルのバッチ投入）に対する 4 種の処理量上限（TASK-122・
    /// 対象ビヘイビア: INDEX-4）。差し替えは [`Self::with_batch_limits`] のみ。
    /// `crate::batch_limits` モジュールドキュメント参照。
    batch_limits: crate::batch_limits::BatchLimits,
    /// `dictionary.rs` の辞書的情報源（TASK-109・PLAN-5）の世代整合キャッシュ。
    /// [`Self::dictionary_snapshot`] がこれを経由して再構築を再利用する（詳細は
    /// [`DictionaryCache`] のドキュメント参照）。
    dictionary_cache: DictionaryCache,
    /// 辞書的情報源抽出の設定（TASK-109・PLAN-5）。差し替えは
    /// [`Self::with_dictionary_config`] のみ。クエリ・セッション変数から到達できる
    /// 経路は持たない（[`Self::with_precision_policy`] と同じ流儀）。
    dictionary_config: crate::dictionary::DictionaryConfig,
    /// `sql::exec::execute_statement` の hybrid 実行が参照する
    /// `crate::sparse::SparseIndex`（BM25 語彙・統計）のテーブル世代整合キャッシュ
    /// （Issue #357）。詳細は `sql/sparse_cache.rs::SparseIndexCache` のドキュメント
    /// 参照。
    sparse_index_cache: crate::sql::sparse_cache::SparseIndexCache,
    /// SQL 表層（`sql::exec::execute_statement_with_cache`）専用の `VectorArena`
    /// テーブル世代整合キャッシュ（Issue #363）。[`Self::execute_validated_in_session`]
    /// の `Statement::Select` アームがこれを経由してアリーナ再構築（redb 全行走査・
    /// デコード）を同一テーブル世代内で再利用する（詳細は
    /// `sql::arena_cache::SqlArenaCache` のドキュメント参照）。
    sql_arena_cache: crate::sql::arena_cache::SqlArenaCache,
    /// 構築時に明示指定された [`crate::search_engine::SearchEngineKind`]（Issue #407）。
    /// [`Self::open_with_engine`]／[`Self::from_storage_with_engine`] 経由なら
    /// `Some(kind)`、任意 provider を直接注入する [`Self::with_provider`]／
    /// [`Self::from_storage`] 経由（`kind` と `provider` の対応をこの構造体自身は
    /// 検証できない）なら `None`。`#[non_exhaustive]` な `SearchEngineKind` を返す
    /// 診断用のアクセサ（[`Self::search_engine_kind`]）で、#411 の `EXPLAIN` 露出が
    /// 参照する契約点。
    search_engine_kind: Option<crate::search_engine::SearchEngineKind>,
}

/// [`EngineCore::dictionary_snapshot`] が要求する `path`/`body` 列
/// （列名の存在・`ColumnType::Text`・non-nullable の 3 条件、TASK-109・PLAN-5）を
/// `schema` が満たすか検証し、満たす場合はそれぞれの列インデックスを返す。
///
/// `USING PLAN('<query>')`（TASK-77・SQL-5）の LLM 呼び出し（`EngineCore::
/// plan_using_plan_expansion` 内の `plan_query`）は内部で `dictionary_snapshot` を
/// 呼ぶため、この判定条件を満たさないスキーマは最終的に `dictionary_snapshot` の
/// 失敗として現れる。呼び出し元ごとに要求するエラー型が異なる
/// （`dictionary_snapshot` は `CoreError`、`USING PLAN` の事前検証は
/// `SqlSurfaceError::invalid_input`〔`22000`〕）ため、本関数は判定条件のみを
/// 共有し、固定メッセージの `String` を返す（各呼び出し元が自分の契約へ変換する。
/// codex-review P1 指摘対応、PR #266）。
fn dictionary_required_columns(
    schema: &crate::catalog::TableSchema,
) -> Result<(usize, usize), String> {
    let path_idx = schema
        .columns
        .iter()
        .position(|c| c.name == "path" && c.ty == crate::catalog::ColumnType::Text && !c.nullable)
        .ok_or_else(|| {
            "table has no non-nullable text path column required for dictionary extraction"
                .to_string()
        })?;
    let body_idx = schema
        .columns
        .iter()
        .position(|c| c.name == "body" && c.ty == crate::catalog::ColumnType::Text && !c.nullable)
        .ok_or_else(|| {
            "table has no non-nullable text body column required for dictionary extraction"
                .to_string()
        })?;
    Ok((path_idx, body_idx))
}

impl EngineCore {
    /// 指定パスの `redb` データベースを開き、既定の検索エンジン
    /// （[`crate::search_engine::default_engine`]）を注入した `EngineCore` を構築する。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        // `open_with_engine`（`OpenWithEngineError` を返す opt-in 経路）へは
        // 委譲しない: 既定 kind（`default_kind()`）は常に
        // `HnswParams::validate` を要さない `ParallelBruteForce` のため
        // `SearchEngineError` 系の失敗は構造的に発生せず、`build_unchecked` 相当の
        // infallible な `search_engine::default_engine()` を直接使うことで
        // `CoreError` 側の型を一切変えずに済む（codex-review P1 指摘・Issue #407
        // 追記。`OpenWithEngineError` 新設の経緯は
        // `docs/design/hnsw-search-engine-wiring.md` 参照）。
        let provider = search_engine::default_engine();
        let storage = Storage::open(path)?;
        Ok(Self::assemble(
            storage,
            provider,
            Some(search_engine::default_kind()),
        ))
    }

    /// 検索 provider を差し替えて構築する（テスト・将来の GPU/ANN provider 導入用）。
    ///
    /// `provider` は呼び出し元が直接組み立てた値であり、対応する
    /// [`crate::search_engine::SearchEngineKind`] を構造的に持たないため
    /// [`Self::search_engine_kind`] は常に `None` を返す（Issue #407。opt-in で
    /// `kind` を伴う構築は [`Self::open_with_engine`] を使う）。
    pub fn with_provider(
        path: impl AsRef<Path>,
        provider: Box<dyn SearchProvider>,
    ) -> Result<Self, CoreError> {
        let storage = Storage::open(path)?;
        Ok(Self::assemble(storage, provider, None))
    }

    /// 指定パスの `redb` データベースを開き、`kind` が構築する検索エンジンを
    /// 注入した `EngineCore` を構築する（Issue #407・opt-in 経路。ADR
    /// `docs/design/ann-index-adoption.md` B 案「テーブル単位カタログ属性は対象外」の
    /// とおり、本関数の呼び出し元がプロセス起動時に明示指定する以外の経路
    /// （環境変数・SQL 構文・セッション変数）は持たない）。
    ///
    /// `kind` が `SearchEngineKind::Hnsw(params)` で `params` が
    /// [`crate::hnsw::HnswParams::validate`] を拒否する値の場合、`EngineCore` は
    /// 構築されず [`OpenWithEngineError`] として fail-closed に拒否する
    /// （[`crate::search_engine::build_validated`] 経由。不正パラメータを保持した
    /// `EngineCore` が生存する状態を構造的に作らない）。
    ///
    /// 戻り値の型は既存の公開 [`CoreError`] ではなく専用の [`OpenWithEngineError`]
    /// とする（`CoreError` は既に公開済みの `#[non_exhaustive]` を付けない enum
    /// であり、本関数のためだけに新規 variant を追加すると既存利用者の網羅的
    /// `match` を壊す破壊的変更になる。本関数自体は Issue #407 で新設する API
    /// のため、独立の戻り値型を選ぶこと自体は破壊的変更に当たらない。
    /// codex-review P1 指摘・Issue #407 追記。経緯は
    /// `docs/design/hnsw-search-engine-wiring.md` 参照）。
    pub fn open_with_engine(
        path: impl AsRef<Path>,
        kind: crate::search_engine::SearchEngineKind,
    ) -> Result<Self, OpenWithEngineError> {
        // fail-closed: 不正な `HnswParams` は `Storage::open`（ファイル
        // オープン・ロック取得、場合により空 DB ファイル作成を伴う）より前に
        // 検証して弾く（codex-review P2 指摘・Issue #407 追記）。
        let provider =
            search_engine::build_validated(kind).map_err(OpenWithEngineError::SearchEngine)?;
        let storage = Storage::open(path).map_err(OpenWithEngineError::Storage)?;
        Ok(Self::assemble(storage, provider, Some(kind)))
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
    ///
    /// [`Self::with_provider`] と同じ理由で [`Self::search_engine_kind`] は常に
    /// `None`（Issue #407。`kind` を伴う構築は [`Self::from_storage_with_engine`]）。
    pub fn from_storage(storage: Storage, provider: Box<dyn SearchProvider>) -> Self {
        Self::assemble(storage, provider, None)
    }

    /// [`Self::from_storage`] の `kind` 版（Issue #407・opt-in 経路）。`kind` が
    /// `SearchEngineKind::Hnsw(params)` で `params` が不正な場合は
    /// [`crate::search_engine::SearchEngineError`] を返し `EngineCore` を構築しない
    /// （[`Self::open_with_engine`] と同じ fail-closed 契約）。
    pub fn from_storage_with_engine(
        storage: Storage,
        kind: crate::search_engine::SearchEngineKind,
    ) -> Result<Self, crate::search_engine::SearchEngineError> {
        let provider = search_engine::build_validated(kind)?;
        Ok(Self::assemble(storage, provider, Some(kind)))
    }

    /// [`Self::with_provider`]／[`Self::open_with_engine`]／[`Self::from_storage`]／
    /// [`Self::from_storage_with_engine`] が共有する構築処理。フィールド初期値は
    /// 4 経路すべてで同一であり（`storage`・`provider`・`search_engine_kind` 以外は
    /// すべて既定値）、重複する struct literal を 1 箇所へ集約する
    /// （Issue #407。以前は `with_provider`／`from_storage` の 2 箇所に同一の
    /// struct literal があった）。
    fn assemble(
        storage: Storage,
        provider: Box<dyn SearchProvider>,
        search_engine_kind: Option<crate::search_engine::SearchEngineKind>,
    ) -> Self {
        Self {
            storage,
            provider,
            prefilter_cache: PrefilterCache::new(),
            precision_policy: crate::precision::PrecisionPolicy::default(),
            embedder: None,
            query_planner: None,
            incremental_config: crate::incremental::IncrementalConfig::default(),
            ledger_mode: LedgerMode::default(),
            batch_limits: crate::batch_limits::BatchLimits::default(),
            dictionary_cache: DictionaryCache::new(),
            dictionary_config: crate::dictionary::DictionaryConfig::default(),
            sparse_index_cache: crate::sql::sparse_cache::SparseIndexCache::new(),
            sql_arena_cache: crate::sql::arena_cache::SqlArenaCache::new(),
            search_engine_kind,
        }
    }

    /// 構築時に明示指定された [`crate::search_engine::SearchEngineKind`] を返す
    /// （Issue #407。[`Self::open`]／[`Self::open_with_engine`] 経由なら値を持ち、
    /// [`Self::with_provider`]／[`Self::from_storage`] 経由（provider を直接注入し
    /// `kind` との対応を検証できない）なら `None`。`VectorCore` trait には載せない
    /// 固有メソッド（`core_api.snapshot` の対象外。`prefilter_cache_stats` と同じ
    /// 方針）。#411 の `EXPLAIN` 露出が参照する契約点）。
    pub fn search_engine_kind(&self) -> Option<crate::search_engine::SearchEngineKind> {
        self.search_engine_kind
    }

    /// [`PrefilterCache`] の現在の統計を返す（TASK-169。テスト・運用観測用）。
    /// テナント ID・行 ID 等の機微情報は含まない（[`PrefilterCacheStats`] 参照）。
    /// `VectorCore` trait には載せない固有メソッド（`core_api.snapshot` の対象外）。
    pub fn prefilter_cache_stats(&self) -> PrefilterCacheStats {
        self.prefilter_cache.stats()
    }

    /// [`crate::sql::sparse_cache::SparseIndexCache`] の現在の統計を返す
    /// （Issue #357。テスト・運用観測用）。テナント ID・行 ID 等の機微情報は含まない
    /// （`SparseIndexCacheStats` 参照）。`VectorCore` trait には載せない固有メソッド
    /// （`core_api.snapshot` の対象外）。
    pub fn sparse_index_cache_stats(&self) -> crate::sql::sparse_cache::SparseIndexCacheStats {
        self.sparse_index_cache.stats()
    }

    /// `sql::arena_cache::SqlArenaCache` の現在の統計を返す（Issue #363。
    /// テスト・運用観測用）。テナント ID・行 ID 等の機微情報は含まない
    /// （`SqlArenaCacheStats` 参照）。`VectorCore` trait には載せない固有メソッド
    /// （`core_api.snapshot` の対象外。`prefilter_cache_stats` と同じ方針）。
    pub fn sql_arena_cache_stats(&self) -> crate::sql::arena_cache::SqlArenaCacheStats {
        self.sql_arena_cache.stats()
    }

    /// `precision` モードの実行契約に使う [`crate::precision::PrecisionPolicy`] を
    /// 差し替えたビルダーを返す（TASK-162・SEARCH-9）。所有権を消費するビルダー
    /// メソッドとし、`&mut self` セッターは公開しない（構築後に一部だけ差し替えて
    /// 中途半端な状態を作れないようにする）。`SessionState`・SQL 構文からはこの値へ
    /// 到達できない（`crate::precision` モジュールドキュメント参照）。
    pub fn with_precision_policy(mut self, policy: crate::precision::PrecisionPolicy) -> Self {
        self.precision_policy = policy;
        self
    }

    /// ファイル形 `INSERT`（TASK-120・INDEX-1, INDEX-2）のベクトル化に使う
    /// [`crate::embedding::Embedder`] を注入したビルダーを返す（所有権を消費する
    /// ビルダーメソッドとし、[`Self::with_precision_policy`] と同じ流儀。未呼び出し
    /// なら `None` のままで、ファイル形 `INSERT` は fail-closed に拒否される）。
    pub fn with_embedder(mut self, embedder: Box<dyn crate::embedding::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// LLM クエリプランニング（TASK-110・PLAN-1）のクエリ展開に使う
    /// [`crate::query_planner::LlmClient`] を注入したビルダーを返す（所有権を消費する
    /// ビルダーメソッドとし、[`Self::with_embedder`] と同じ流儀。未呼び出しなら
    /// `None` のままで、[`Self::plan_query`] は fail-closed に拒否される）。
    pub fn with_query_planner(mut self, client: Box<dyn crate::query_planner::LlmClient>) -> Self {
        self.query_planner = Some(PlannerBinding::Single(client));
        self
    }

    /// 質問類型推定・ティアリング（TASK-115・PLAN-8）に基づき、対話ティア／高精度
    /// ティアそれぞれの [`crate::query_planner::LlmClient`] を注入したビルダーを返す
    /// （所有権を消費するビルダーメソッドとし、[`Self::with_query_planner`] と同じ流儀。
    /// [`Self::with_query_planner`] と排他: 後に呼んだ側が [`Self::query_planner`] を
    /// 上書きする。判定基準 [`crate::tiering::TieringCriteria`] は本リポの実装既定値で、
    /// 呼び出し元が差し替え可能（`docs/design/query-tiering-criteria.md` 参照）。
    pub fn with_tiered_query_planner(
        mut self,
        dialogue: Box<dyn crate::query_planner::LlmClient>,
        high_precision: Box<dyn crate::query_planner::LlmClient>,
        criteria: crate::tiering::TieringCriteria,
    ) -> Self {
        self.query_planner = Some(PlannerBinding::Tiered(crate::tiering::TieredPlanner::new(
            dialogue,
            high_precision,
            criteria,
        )));
        self
    }

    /// ファイル形 `INSERT` のチャンク化・チャンク数上限設定
    /// （[`crate::incremental::IncrementalConfig`]）を差し替えたビルダーを返す
    /// （TASK-120）。未呼び出しなら `IncrementalConfig::default()`。
    pub fn with_incremental_config(
        mut self,
        config: crate::incremental::IncrementalConfig,
    ) -> Self {
        self.incremental_config = config;
        self
    }

    /// `operation_id` 必須化ガード（TASK-92・対象ビヘイビア: RECOVER-1）を制御する
    /// [`LedgerMode`] を差し替えたビルダーを返す（[`Self::with_precision_policy`] と
    /// 同型: 所有権を消費するビルダーメソッドとし、`&mut self` セッターは公開しない）。
    /// `SessionState`・SQL 構文からはこの値へ到達できない（`crate::recovery::
    /// required_op_id` モジュールドキュメント参照）。
    pub fn with_ledger_mode(mut self, mode: LedgerMode) -> Self {
        self.ledger_mode = mode;
        self
    }

    /// 一括投入（複数ファイルのバッチ投入）に対する 4 種の処理量上限
    /// （[`crate::batch_limits::BatchLimits`]）を差し替えたビルダーを返す
    /// （TASK-122・対象ビヘイビア: INDEX-4。[`Self::with_incremental_config`] と同じ
    /// 流儀。未呼び出しなら `BatchLimits::default()`）。
    pub fn with_batch_limits(mut self, limits: crate::batch_limits::BatchLimits) -> Self {
        self.batch_limits = limits;
        self
    }

    /// 辞書的情報源抽出（TASK-109・PLAN-5）の設定
    /// （[`crate::dictionary::DictionaryConfig`]）を差し替えたビルダーを返す
    /// （[`Self::with_precision_policy`] と同じ流儀。未呼び出しなら
    /// `DictionaryConfig::default()`）。
    ///
    /// `dictionary_cache`（[`DictionaryCache`]）も同時に再初期化する（PR #249
    /// codex-review P1 指摘対応）。`DictionaryCache` のキャッシュキーは
    /// `(table, ctx)` と `storage.current_generation()` のみで、設定値
    /// （`enable_file_tree`・`enable_term_index`・`top_terms` 等）を含まない。
    /// 設定だけを差し替えて既存キャッシュを温存すると、`dictionary_snapshot` を
    /// 一度でも呼んだ後に本メソッドで設定変更しても、書き込みで世代が進むまでは
    /// 旧設定で構築した `Arc<Dictionary>` を返し続けてしまう。設定変更は稀な操作
    /// のため、キャッシュ全体を破棄して次回参照時に新設定で再構築させる単純な
    /// 方針を採る（fail-closed。古い設定の結果を新設定のものとして黙って返す
    /// 経路を残さない）。
    pub fn with_dictionary_config(mut self, config: crate::dictionary::DictionaryConfig) -> Self {
        self.dictionary_config = config;
        self.dictionary_cache = DictionaryCache::new();
        self
    }

    /// `table` の辞書的情報源スナップショットを返す（TASK-109・PLAN-5。TASK-110 の
    /// LLM クエリプランニングが固定接頭辞コンテキストとして消費する入口）。
    /// `VectorCore` trait へは昇格しない固有メソッド（`core-api-check` の対象外。
    /// `Self::with_incremental_config` 等と同じ理由）。
    ///
    /// [`DictionaryCache`] を経由し、世代整合が取れていれば再構築せず再利用する
    /// （[`DictionaryCache`] のドキュメント参照）。キャッシュミス時は
    /// `tenant::visible_rows`（`ctx` の可視性判定込み。テナント境界は完全にこの
    /// 経路が担う）でテーブル全行を取得し、スキーマから `path`/`body` 列を解決して
    /// `crate::dictionary::DictionaryBuilder` へ供給する。`path`/`body` 列を持たない
    /// テーブルは `CatalogError::Invalid` の固定英語メッセージで拒否する
    /// （既存 `execute_file_insert` 系と同じ「存在情報を漏らさない」写像方針）。
    ///
    /// スキーマ・世代の取得（`schema_read_txn`）と行走査（`tenant::visible_rows`）は
    /// 別スナップショットになりうる（`visible_rows` はページング走査中に複数回
    /// `read_txn` を開くため、単一 `read_txn` へ統合できない）。そのため行走査後に
    /// `Storage::current_generation` を再確認し、スキーマ取得時点の世代
    /// （`built_generation`）と食い違っていれば「新スキーマ前提の行を旧スキーマで
    /// デコードして破損行として黙ってスキップし、不完全な辞書をあたかも正規の
    /// ものとして返す」（PR #249 codex-review P1 指摘）事態を防ぐため、その回の
    /// 結果を破棄して最新スナップショットで再試行する（[`MAX_SNAPSHOT_RETRIES`]
    /// 回まで。以降も食い違い続ける場合は書き込みが止まらないとみなしエラーで
    /// 拒否する。fail-closed。security.md「不安全な設計」対応）。
    pub fn dictionary_snapshot(
        &self,
        ctx: &PolicyContext,
        table: &str,
    ) -> Result<Arc<crate::dictionary::Dictionary>, CoreError> {
        self.dictionary_snapshot_with_index(ctx, table)
            .map(|(dictionary, _index)| dictionary)
    }

    /// [`Self::dictionary_snapshot`] と同じ辞書スナップショットに加え、
    /// [`crate::tiering::classify`] 用に 1 度だけ構築した正規化索引
    /// （[`crate::tiering::NormalizedDictionaryIndex`]）も返す（codex-review P1
    /// 指摘対応・PR #261）。呼び出し元は `Self::expand_query`（[`PlannerBinding::Tiered`]
    /// 構成時のティア判定）のみ。`dictionary`・`normalized_index` は
    /// [`DictionaryCache`] の同一エントリから `Arc::clone` するため、キャッシュ
    /// ヒット時は辞書の再走査・索引の再構築のいずれも発生しない
    /// （[`DictCacheEntry::normalized_index`] ドキュメント参照）。
    fn dictionary_snapshot_with_index(
        &self,
        ctx: &PolicyContext,
        table: &str,
    ) -> Result<
        (
            Arc<crate::dictionary::Dictionary>,
            Arc<crate::tiering::NormalizedDictionaryIndex>,
        ),
        CoreError,
    > {
        if let Some(hit) = self.dictionary_cache.lookup(&self.storage, table, ctx) {
            return Ok(hit);
        }

        const MAX_SNAPSHOT_RETRIES: u32 = 5;
        for _ in 0..MAX_SNAPSHOT_RETRIES {
            // スキーマと「構築時の世代」を単一の `read_txn`（同一スナップショット）
            // から読む（TASK-109・PLAN-5 レビュー対応: Cursor Bugbot Medium
            // "Snapshot mixes schema and rows"）。以前はスキーマ取得
            // （`Storage::get_table_schema`）と世代取得（`Storage::current_generation`）
            // が別々の `read_txn` だったため、両呼び出しの間に並行
            // `ALTER TABLE ADD COLUMN` がコミットされると、`built_generation` は
            // 新世代を指すのに `schema` は旧世代のまま、という食い違ったペアを
            // 観測しうった。スキーマと世代を同一スナップショットで揃えることで、
            // `built_generation` が常に `schema` を取得した時点の世代と一致する
            // ことを保証する。
            let schema_read_txn = self.storage.db().begin_read().map_err(StorageError::from)?;
            let schema = crate::catalog::get_table_schema_in_txn(&schema_read_txn, table)?;
            let built_generation = crate::storage::current_generation_in_txn(&schema_read_txn)?;
            drop(schema_read_txn);

            // `path`/`body` は列名の存在・`ColumnType::Text` に加え non-nullable
            // であることまで検証する（PR #249 codex-review P1 指摘: 同名の非 Text
            // 列や nullable な列を持つテーブルを受理すると、後段で `Value::Text`
            // 以外（型不一致）または `Value::Null`（nullable 列に NULL が入った行）
            // に一致した行が黙ってスキップされ、成功応答の空/不完全な辞書を
            // 返してしまう。スキーマ不整合は fail-closed で固定メッセージの
            // `CatalogError::Invalid` として拒否し、他テナントのデータ・存在情報は
            // 含めない）。判定条件は [`dictionary_required_columns`] へ切り出し、
            // `USING PLAN`（TASK-77・SQL-5）の LLM 呼び出し前スキーマ事前検証
            // （`Self::execute_sql_in_session` の `Statement::Select` アーム、
            // `using_plan()` が `Some` の分岐を参照）と単一の判定基準を共有する
            // （codex-review P1 指摘対応、PR #266: 事前検証が無いと、この判定結果は
            // 呼び出し元で `CoreError::Catalog(CatalogError::Invalid)` を経由して
            // 一律 `Internal`〔`XX000`〕へ丸められ、body 列欠落等の通常の利用者
            // スキーマ不備が本来の `SqlSurfaceError::InvalidInput`〔`22000`〕として
            // 返らなかった）。
            let (path_idx, body_idx) = dictionary_required_columns(&schema)
                .map_err(|msg| CoreError::from(CatalogError::Invalid(msg)))?;

            let rows =
                crate::tenant::visible_rows(&self.storage, table, ctx).map_err(|e| match e {
                    crate::tenant::TenantError::Catalog(e) => CoreError::from(e),
                    // 走査量上限超過は他テナントの存在情報を含まない固定メッセージへ
                    // 丸める（`TenantError` 自体が既にその契約を満たすが、
                    // `CoreError` 側の型に昇格し直す。security.md「エラー・ログ
                    // 経由で他テナントのデータ・存在情報を漏らさない」）。
                    crate::tenant::TenantError::TooManyVisibleRows { .. }
                    | crate::tenant::TenantError::TooManyRowsScanned { .. } => {
                        CoreError::from(CatalogError::Invalid(
                            "too many rows to build dictionary snapshot".to_string(),
                        ))
                    }
                    // `verify_hits` 専用の variant で `visible_rows` からは返らない
                    // 防御的分岐。
                    crate::tenant::TenantError::HitOutsideVisibleSet => {
                        CoreError::from(CatalogError::Invalid(
                            "unexpected tenant boundary error while building dictionary snapshot"
                                .to_string(),
                        ))
                    }
                })?;

            // 行走査完了後の実世代を確認する。`schema`/`built_generation` 取得後
            // かつ `visible_rows` の走査完了前に書き込みがコミットされていると、
            // 走査結果に旧スキーマでは正しくデコードできない新世代の行が混じり
            // うる。ここで不一致を検出したら、その回の `rows`/`schema` を丸ごと
            // 破棄し、最新スナップショットを取り直して再試行する（不完全な辞書を
            // 正規の結果として返さない）。
            let observed_generation = self.storage.current_generation()?;
            if observed_generation != built_generation {
                continue;
            }

            let config = self.dictionary_config.clone();
            let mut builder = crate::dictionary::DictionaryBuilder::new(config);
            for row in &rows {
                let Ok(values) = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
                else {
                    // デコード失敗行を黙ってスキップすると、内容を欠いた
                    // `Dictionary` が `truncated: false` のまま正常なキャッシュ
                    // エントリとして保存され、後続の LLM プランニングから完全な
                    // スナップショットと区別できなくなる（PR #249 codex-review P1
                    // 指摘）。データ破損は recall 側の安全劣化（切り詰め）とは
                    // 性質が異なるため、fail-closed に構築全体を中止しキャッシュへ
                    // 保存しない。他テナントのデータ・存在情報は含めない固定
                    // メッセージで拒否する。
                    return Err(CoreError::from(CatalogError::Invalid(
                        "failed to decode a visible row while building dictionary snapshot"
                            .to_string(),
                    )));
                };
                // 上記のスキーマ検証で path_idx/body_idx は non-nullable な
                // `ColumnType::Text` 列を指すことを保証しているため、ここで
                // `Value::Text` 以外（`Value::Null`・型不一致）に一致することは
                // 想定しない。想定外のズレを黙って読み飛ばすと不完全な辞書が
                // 正常なキャッシュエントリとして保存されうる（PR #249
                // codex-review P1 指摘と同種の懸念）ため、防御的に fail-closed で
                // 拒否する（他テナントのデータ・存在情報を含めない固定メッセージ）。
                let path = match values.get(path_idx) {
                    Some(crate::row_codec::Value::Text(s)) => s.as_str(),
                    _ => {
                        return Err(CoreError::from(CatalogError::Invalid(
                            "path column value was unexpectedly not text while building \
                             dictionary snapshot"
                                .to_string(),
                        )));
                    }
                };
                let body = match values.get(body_idx) {
                    Some(crate::row_codec::Value::Text(s)) => s.as_str(),
                    _ => {
                        return Err(CoreError::from(CatalogError::Invalid(
                            "body column value was unexpectedly not text while building \
                             dictionary snapshot"
                                .to_string(),
                        )));
                    }
                };
                builder.ingest(path, body);
            }
            let dictionary = builder.finish();
            let approx_bytes = dictionary.approx_heap_bytes();
            // `DictionaryCache::insert` は自身のロック区間内で世代を再確認し、
            // 挿入対象（このスナップショット）が既に古くなっていれば `None` を
            // 返す（PR #249 codex-review P1 指摘対応: 上の `observed_generation`
            // チェックとこの呼び出しの間に別の書き込みがコミットされる競合を
            // 検出する最後の砦）。`None` は書き込み未反映のスナップショットを
            // 呼び出し元へ渡さないためのシグナルであり、ループ先頭へ戻って
            // 最新スナップショットで再試行する（不完全な辞書を正規の結果として
            // 返さない）。
            if let Some(hit) = self.dictionary_cache.insert(
                &self.storage,
                table,
                ctx,
                dictionary,
                built_generation,
                approx_bytes,
            ) {
                return Ok(hit);
            }
        }

        // 継続的な並行書き込みで整合したスナップショットを得られなかった。
        // 不完全な辞書を黙って返さず fail-closed に拒否する。
        Err(CoreError::from(CatalogError::Invalid(
            "dictionary snapshot generation kept changing during row scan; retry later".to_string(),
        )))
    }

    /// `table` に対する自然言語 `question` を LLM クエリプランニング（TASK-110・
    /// PLAN-1）で展開し、[`crate::query_planner::QueryExpansion`] を返す。
    /// `VectorCore` trait へは昇格しない固有メソッド（`core-api-check` の対象外。
    /// `Self::dictionary_snapshot` 等と同じ理由）。
    ///
    /// [`Self::query_planner`] が未注入（`None`）の場合は
    /// [`CoreError::QueryPlannerUnavailable`] で fail-closed に拒否する（`embedder`
    /// 未構成時のファイル形 `INSERT` 拒否と同じ方針。既定で参照実装を暗黙採用しない）。
    ///
    /// 固定接頭辞は必ず `(table, ctx)` 単位の [`Self::dictionary_snapshot`] から
    /// 都度レンダリングし、テナントをまたぐ接頭辞キャッシュは持たない
    /// （security.md「テナント境界」対応。レンダリング自体は
    /// [`DictionaryCache`] 経由で世代整合キャッシュされるため、LLM レイテンシ比で
    /// 無視できるコストに収まる）。LLM 応答は
    /// [`crate::query_planner::parse_expansion`] が厳格パースするため、
    /// プロンプトインジェクションによる異常出力の影響は検証済みの
    /// `QueryExpansion` に閉じる（`query_planner.rs` モジュールドキュメント参照）。
    pub fn plan_query(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<crate::query_planner::QueryExpansion, CoreError> {
        let (expansion, _classification) = self.expand_query(ctx, table, question)?;
        Ok(expansion)
    }

    /// [`Self::plan_query`] と同じ契約に加え、[`Self::query_planner`] が
    /// [`PlannerBinding::Tiered`] 構成の場合に採用した質問類型・ティア
    /// （[`crate::tiering::Classification`]）も `Some` で返す（TASK-115・PLAN-8。
    /// TASK-116 のティア別レイテンシ検証、将来の EXPLAIN 露出〔SQL-6・PLAN-11／
    /// TASK-164〕の足場）。[`PlannerBinding::Single`] 構成の場合は分類自体を行わず
    /// `None` を返す（単一クライアントへそのまま委譲する `Self::plan_query` の既存
    /// 挙動を変えないため）。
    ///
    /// `VectorCore` trait へは昇格しない固有メソッド（`core-api-check` の対象外。
    /// `Self::plan_query`・`Self::dictionary_snapshot` と同じ理由）。
    pub fn plan_query_with_classification(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<
        (
            crate::query_planner::QueryExpansion,
            Option<crate::tiering::Classification>,
        ),
        CoreError,
    > {
        self.expand_query(ctx, table, question)
    }

    /// `table` に対する自然言語 `question` を LLM クエリプランニング（TASK-110・
    /// PLAN-1）で展開し、明示指定（`query_mode`・`session_mode`）とプランナー推定
    /// （展開結果の `mode_hint`）から解決した実効モードまで含めて返す
    /// （TASK-164・PLAN-11。解決契約は `sql::mode` モジュールドキュメント、
    /// spec のビヘイビア定義〔PLAN-11〕を参照）。`VectorCore` trait へは昇格しない
    /// 固有メソッド（[`Self::plan_query`] と同じ理由。`core-api-check` の対象外）。
    ///
    /// モード解決は `sql::mode::resolve_mode_with_planner` へ委譲する。呼び出し元
    /// （wire-server の接続ハンドラ・将来の `USING PLAN` 結線。TASK-77/78 の管轄）は
    /// `query_mode`（クエリ句由来）・`session_mode`（`SessionState::search_mode()`
    /// 由来）を渡す。展開自体の fail-closed 契約（LLM 未接続・プロンプト超過・
    /// LLM 応答不正はすべて `Err`）は [`Self::plan_query`] と共有する（モードの
    /// fail-safe とは独立したエラー系統。`query_planner.rs` モジュールドキュメント
    /// 参照）。質問類型・ティア（TASK-115・PLAN-8）の判定結果は本メソッドの戻り値
    /// には含まれない（必要な呼び出し元は [`Self::plan_query_with_classification`]
    /// を使う）。
    pub fn plan_query_with_mode(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
        query_mode: Option<crate::sql::mode::SearchMode>,
        session_mode: Option<crate::sql::mode::SearchMode>,
    ) -> Result<crate::query_planner::PlannedQuery, CoreError> {
        let (expansion, _classification) = self.expand_query(ctx, table, question)?;
        let resolved = crate::sql::mode::resolve_mode_with_planner(
            query_mode,
            session_mode,
            expansion.mode_hint,
        );
        Ok(crate::query_planner::PlannedQuery::new(expansion, resolved))
    }

    /// [`Self::plan_query`]・[`Self::plan_query_with_classification`]・
    /// [`Self::plan_query_with_mode`] が共有する展開フロー本体（辞書スナップショット
    /// 取得 → ティア判定〔[`PlannerBinding::Tiered`] 構成時のみ〕 → 固定接頭辞
    /// レンダリング → LLM 呼び出し → 厳格パース）。
    fn expand_query(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<
        (
            crate::query_planner::QueryExpansion,
            Option<crate::tiering::Classification>,
        ),
        CoreError,
    > {
        let binding = self
            .query_planner
            .as_ref()
            .ok_or(CoreError::QueryPlannerUnavailable)?;
        let (dictionary, normalized_index) = self.dictionary_snapshot_with_index(ctx, table)?;

        let (client, classification): (&dyn crate::query_planner::LlmClient, _) = match binding {
            PlannerBinding::Single(client) => (client.as_ref(), None),
            PlannerBinding::Tiered(tiered) => {
                // `normalized_index` は辞書スナップショット（`dictionary`）と同じ
                // `DictionaryCache` エントリから 1 度だけ構築済み（codex-review P1
                // 指摘対応・PR #261）。`tiered.select` はここでは借用のみで済み、
                // リクエストごとの辞書全量の複製・走査は発生しない。
                let (client, classification) = tiered.select(question, &normalized_index);
                (client, Some(classification))
            }
        };

        let prefix = crate::query_planner::render_prompt_prefix(&dictionary);
        let prompt = crate::query_planner::render_full_prompt(&prefix, question)?;
        let response = client.complete(&prompt)?;
        let expansion = crate::query_planner::parse_expansion(&response)?;
        Ok((expansion, classification))
    }

    /// `table` に対する自然言語 `question` を [`Self::plan_query`]（TASK-110・PLAN-1）で
    /// 展開し、展開結果を再埋め込み規則（TASK-114・PLAN-10）に従って再埋め込みした
    /// [`crate::query_planner::EmbeddedQuery`] を返す。`VectorCore` trait へは昇格しない
    /// 固有メソッド（[`Self::plan_query`] と同じ理由。`core-api-check` の対象外）。
    ///
    /// 処理順序（fail-closed。`incremental.rs::chunk_phase` が embedder 次元不一致を
    /// チャンク化・埋め込みサービス呼び出しより前に検出する前例と同じ流儀で、
    /// LLM 呼び出しという比較的高コストな I/O の前に構成不備を検出する）:
    /// 1. [`Self::embedder`] 未注入は [`CoreError::EmbedderUnavailable`] で拒否
    /// 2. 対象テーブルの宣言次元（`VECTOR(N)`）と `embedder.dim()` の不一致は
    ///    [`CoreError::QueryEmbedding`]（[`crate::embedding::EmbedError::DimMismatch`]）で拒否
    /// 3. [`Self::plan_query`] で LLM 展開（`query_planner` 未注入は既存の
    ///    [`CoreError::QueryPlannerUnavailable`]。LLM 展開が失敗した場合はここで
    ///    エラーが伝播し、以降の埋め込み呼び出しは実行しない）
    /// 4. [`crate::query_planner::reembed_expansion`] で再埋め込み
    ///
    /// テーブルスキーマ参照は `execute_insert_sql`
    /// 等の既存経路（`self.storage.get_table_schema`）と同じ流儀を用い、新規の
    /// RLS バイパス経路は作らない（対象はテーブル構造メタデータでありテナント行
    /// データではないため、`ctx` によるフィルタ対象外。`(table, ctx)` に対する
    /// テナント境界の担保は [`Self::plan_query`] 内の [`Self::dictionary_snapshot`]
    /// が引き続き担う）。
    pub fn plan_and_embed_query(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<crate::query_planner::EmbeddedQuery, CoreError> {
        let embedder = self
            .embedder
            .as_deref()
            .ok_or(CoreError::EmbedderUnavailable)?;

        let schema = self.storage.get_table_schema(table)?;
        let table_dim = schema.vector_dim().ok_or_else(|| {
            CoreError::Catalog(CatalogError::Invalid(
                "table has no VECTOR column".to_string(),
            ))
        })?;
        let embedder_dim = embedder.dim();
        if embedder_dim != table_dim {
            return Err(CoreError::QueryEmbedding(
                crate::embedding::EmbedError::DimMismatch {
                    expected: table_dim,
                    got: embedder_dim as usize,
                },
            ));
        }

        let expansion = self.plan_query(ctx, table, question)?;
        let embedding = crate::query_planner::reembed_expansion(embedder, question, &expansion)?;
        Ok(crate::query_planner::EmbeddedQuery {
            expansion,
            embedding,
        })
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
        // セッション変数を持たない後方互換 API（TASK-75 由来）。`SET search_mode` 等
        // セッションを要する statement はこのエントリポイントでは受理しない
        // （黙った no-op にしない）。
        //
        // statement 種別の判定を [`crate::sql::allowlist::validate_sql`] で先に行い、
        // `SetSearchMode` はリテラル値の妥当性を検証する前に一律 `42601` で拒否する
        // （codex-review P1 指摘対応: 以前は `execute_sql_in_session` へ委譲していたため、
        // 内部で `SearchMode::parse_literal` が先に走り、無効なリテラル値（例:
        // `SET search_mode = 'fuzzy'`）は `22000` を返す一方、有効な値は `42601` を
        // 返すという、同じ「このエントリポイントでは非対応」の statement が値によって
        // 異なるエラーコードを返す非決定的な契約になっていた。fail-closed の観点でも
        // エラー契約は入力の意味論的妥当性に関わらず一貫させるべきであり、statement
        // 種別のみで判定する）。
        match crate::sql::allowlist::validate_sql(sql, &self.storage)? {
            crate::sql::allowlist::Statement::SetSearchMode { .. } => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "SET search_mode requires a session-aware entry point",
                ))
            }
            // TASK-79（SQL-9）: UDF 定義はセッションへ登録する statement のため、
            // セッションを持たないこのエントリポイントでは受理しない（`SET` と同じ
            // 「値の妥当性に関わらず一律 `42601`」の決定的な契約を踏襲する）。
            crate::sql::allowlist::Statement::CreateFunction { .. } => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "CREATE FUNCTION requires a session-aware entry point",
                ))
            }
            stmt @ crate::sql::allowlist::Statement::Select(_) => {
                // `SELECT` と判定済みのため、[`Self::execute_validated_in_session`] が
                // `SqlOutcome::SetSearchMode`／`SqlOutcome::CreateFunction` を返すことは
                // ない。ここで得た `stmt`（`validate_sql` 済み）をそのまま
                // `execute_validated_in_session` へ渡し、SQL 文字列の再パースを
                // 避ける（Issue #314・SQL-1・TASK-83 条件7: SQL 表層 C1 p95 の
                // 固定コスト削減。以前は `execute_sql_in_session(ctx, &mut session,
                // sql)` を呼び直しており、同一 `sql` を 2 回構文解析していた）。
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_validated_in_session(ctx, &mut session, stmt)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_) => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as Select"
                                .to_string(),
                        })
                    }
                }
            }
            // TASK-166（SQL-13）: 集計 SELECT は UDF 呼び出しをその引数に含みうる
            // ため（セッション UDF レジストリ参照）、`Select` と同じくセッションを
            // 要する実行本体（`execute_validated_in_session`）へ委譲する。UDF を
            // 持たない空セッションで束縛するため、セッションに定義済みの UDF を
            // 参照する集計は（`Select` と同様）このセッションなしエントリポイント
            // では使えない（`22000`。「未知の関数」として拒否される）。
            stmt @ crate::sql::allowlist::Statement::Aggregate(_) => {
                // `stmt` は上と同じく `validate_sql` 済みのため再パースしない
                // （Issue #314・SQL-1・TASK-83 条件7）。
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_validated_in_session(ctx, &mut session, stmt)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_) => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as Aggregate"
                                .to_string(),
                        })
                    }
                }
            }
            // TASK-78（SQL-6）: `EXPLAIN` は検索本体を実行しない別の応答形
            // （`SqlOutcome::Explain`）を返すため、`QueryResult` のみを返す本
            // エントリポイントでは受理しない（`SET`・`CREATE FUNCTION` と同じ
            // 「値の妥当性に関わらず一律 `42601`」の決定的な契約を踏襲する）。
            crate::sql::allowlist::Statement::Explain(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "EXPLAIN requires a session-aware entry point",
                ))
            }
        }
    }

    /// 接続（セッション）単位の [`crate::sql::mode::SessionState`] を受け取って SQL 文を
    /// 実行する（TASK-161・SQL-12 の公開 API）。`wire-server` の接続ハンドラ
    /// （TASK-73・TASK-165 の管轄）が 1 接続につき 1 個の `SessionState` を所有し、
    /// クエリのたびに `&mut` で本メソッドへ渡す想定（`EngineCore` 自体は
    /// セッション状態を保持しない。接続間でモードが混線しない構造を型で担保する
    /// ため。`sql::mode` モジュールドキュメント参照）。
    ///
    /// `SET search_mode = '<literal>'` はリテラル値が `recall`／`precision` のいずれか
    /// である場合にのみ `session` を更新する（検証→代入の順）。失敗した `SET` は
    /// `session` を一切変更しない（部分更新＝黙った既定化と同種の fail-open を防ぐ）。
    /// `table_name` の `read_txn` を新規に開き、同一スナップショット上でスキーマを
    /// 取得して両方を返す（[`Self::execute_sql_in_session`] の各 `Statement` アームが
    /// 共有する定型処理）。呼び出し元は返した `read_txn` を、必要な処理
    /// （束縛・`execute_statement`）が終わるまでにドロップしてよい――本メソッド自体は
    /// 開いた `read_txn` を長時間保持しない（`USING PLAN` 経路が LLM 呼び出し・
    /// 再埋め込みの間 `read_txn` を保持しないようにする分割の一部。呼び出し元の
    /// モジュールドキュメント参照）。テーブル不存在は
    /// [`crate::sql::allowlist::SqlSurfaceError::UndefinedTable`] へ丸め込む。
    fn read_txn_with_schema(
        &self,
        table_name: &str,
    ) -> Result<
        (redb::ReadTransaction, crate::catalog::TableSchema),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        let read_txn = self.storage.db().begin_read().map_err(|e| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!(
                    "failed to begin read transaction: {}",
                    StorageError::from(e)
                ),
            }
        })?;
        let schema = crate::catalog::get_table_schema_in_txn(&read_txn, table_name).map_err(
            |e| match e {
                CatalogError::TableNotFound(name) => {
                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                }
                other => crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: format!("failed to load table schema: {other}"),
                },
            },
        )?;
        Ok((read_txn, schema))
    }

    pub fn execute_sql_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // TASK-82（SQL-10）: セッション経由の SQL 実行経路へ `INSERT` を接続する。
        // `sql::allowlist::validate_sql` は SELECT／SET／CREATE FUNCTION／EXPLAIN
        // のみを受理し `INSERT` を許可形状に含めない（`INSERT` は
        // `sql::allowlist::validate_insert`（TASK-80）が独立した専用検証経路を
        // 持つため。`validate_insert` のドキュメント参照）。ここでは
        // `validate_sql` を呼ぶ前に先頭トークンだけを覗いて `INSERT` を検出し、
        // 検出した場合は本メソッドの残りをスキップして既存の
        // [`Self::execute_insert_sql`]（`validate_insert` → `bind_insert_form` →
        // 行形/ファイル形実行。`self.ledger_mode` を尊重）へそのまま委譲する
        // （検証・実行本体の二重実装を避ける）。`EXPLAIN` の前置（`EXPLAIN
        // INSERT ...`）は先頭トークンが `INSERT` ではなく `EXPLAIN` になるため
        // ここでは捕捉されず、後続の `validate_sql` の `EXPLAIN` 分岐（次の
        // トークンが `SELECT` であることを要求）へ流れて `42601` で拒否される
        // （挙動は本変更の前後で不変）。検索モード句（`USING MODE` 等）は
        // `INSERT` の許可形状に存在しないため `validate_insert` 側の構文検証で
        // 同じく拒否される。覗き見トークナイズ自体が失敗した場合は分岐せず
        // `validate_sql` へフォールスルーし、同一入力に対して同じ構文エラーを
        // 返す（fail-closed。二重トークナイズによる無駄はあるが untrusted 入力の
        // 長さは wire 層で既に上限検証済みのため許容する）。
        let is_insert_statement = crate::sql::lexer::tokenize(sql).is_ok_and(|tokens| {
            matches!(
                tokens.first(),
                Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("INSERT")
            )
        });
        if is_insert_statement {
            let outcome = self.execute_insert_sql(ctx, sql)?;
            return Ok(crate::sql::SqlOutcome::Insert(outcome));
        }

        let stmt = crate::sql::allowlist::validate_sql(sql, &self.storage)?;
        self.execute_validated_in_session(ctx, session, stmt)
    }

    /// [`Self::execute_sql_in_session`] の実行本体。`validate_sql` 済みの
    /// [`crate::sql::allowlist::Statement`] を受け取ることで、呼び出し元
    /// （[`Self::execute_sql`] の `Select`／`Aggregate` アーム）が既に構文解析
    /// 済みの `Statement` を持つ場合に同一 SQL 文字列の再パースを避けられる
    /// （Issue #314・SQL-1・TASK-83 条件7: SQL 表層 C1 経路の固定コスト削減。
    /// `execute_sql_in_session` 自身（`sql: &str` を受け取る公開 API）は本メソッド
    /// より前に `INSERT` 判定・`validate_sql` を済ませてから委譲するため、挙動・
    /// エラー契約は分割前と不変）。
    fn execute_validated_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        stmt: crate::sql::allowlist::Statement,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        match stmt {
            crate::sql::allowlist::Statement::SetSearchMode { value } => {
                let mode = crate::sql::mode::SearchMode::parse_literal(&value)?;
                session.set_search_mode(mode);
                Ok(crate::sql::SqlOutcome::SetSearchMode(mode))
            }
            // TASK-79（SQL-9）: `CREATE FUNCTION` の検証・登録は
            // `sql::udf_call::define_function` に委譲する（検証→登録の順を守り、
            // 失敗時は `session.udfs()` を一切変更しない。`SET search_mode` と同じ
            // 「部分更新＝黙った既定化を防ぐ」方針）。
            crate::sql::allowlist::Statement::CreateFunction { name, params, body } => {
                crate::sql::udf_call::define_function(session.udfs_mut(), &name, &params, &body)?;
                Ok(crate::sql::SqlOutcome::CreateFunction { name })
            }
            crate::sql::allowlist::Statement::Select(validated) => {
                // TASK-77（SQL-5）: `USING PLAN('<query>')` は `ORDER BY` の代替
                // （相互排他）のため、`validated.using_plan()` の有無で束縛経路を
                // 分岐する。この経路は `sql::parser::bind_in_session` を呼ばない
                // （`bind_ranking` が `OrderByForm::UsingPlan` を防御的に拒否する
                // ことと対になる：正規の分岐は必ずここで行われる）。
                //
                // `USING PLAN` 経路は `plan_using_plan_expansion` 内で LLM 呼び出し・
                // 再埋め込み（`plan_query`・`Embedder::embed_batch`）という
                // 長時間の I/O を行う。この I/O の間、`execute_statement` 用の
                // `read_txn` を保持し続けると、redb のスナップショットが I/O 完了
                // まで固定され続けページ回収不能になるうえ、`plan_query` が内部で
                // 取得する `dictionary_snapshot` が後続のハイブリッドスキャンと
                // 異なる世代のデータを読む可能性がある（Cursor Bugbot Medium
                // 指摘対応）。そのため I/O 自体（`plan_using_plan_expansion`）は
                // スキーマに依存しない形（展開結果と再埋め込み済み query vector の
                // 生成のみ）で `read_txn` を開かずに完了させ、I/O 完了後に
                // `execute_statement` 用の `read_txn` とスキーマを新規に取得し、
                // その同一スナップショット上で `sql::using_plan::bind_expansion`
                // （列インデックス解決を含む束縛）を行ってから `execute_statement`
                // へ渡す（codex-review P1 指摘対応: 以前は I/O 前に取得した旧
                // スキーマで列インデックスを含む `BoundStatement` を確定し、I/O 後に
                // 取得し直した最新スキーマとの一致検証なしに旧束縛を
                // `execute_statement` へ渡していたため、I/O 中に同名テーブルの
                // `DROP TABLE`→再作成等でレイアウトが変わると、束縛済みの列
                // インデックスが別の列を指す状態になり得た。他経路と同じく
                // 列インデックス解決を含む束縛〜`execute_statement` は単一
                // スナップショットへ閉じ込める）。
                //
                // 不変条件の防御的検証（codex-review P1 指摘対応、PR #266）:
                // `order_by` が [`crate::sql::allowlist::OrderByForm::UsingPlan`] で
                // あることと `using_plan()` が `Some` であることは本来一対一で
                // 対応する必要がある（`sql::allowlist::ValidatedStatement::
                // with_using_plan` のドキュメント参照）。`validate_sql` の内部
                // パーサーは常に両者を揃えて構築するが、`ValidatedStatement::new`
                // （`order_by` を無検証で受け取る公開 constructor）経由で外部から
                // `order_by == UsingPlan` かつ `using_plan() == None` の矛盾した
                // 値を構築される可能性を完全には排除できないため、分岐前に
                // ここで検証し、矛盾していれば内部エラーとして拒否する
                // （fail-closed。到達は公開 API の誤用時のみの防御的経路）。
                let order_by_is_using_plan = matches!(
                    validated.order_by(),
                    crate::sql::allowlist::OrderByForm::UsingPlan
                );
                if order_by_is_using_plan != validated.using_plan().is_some() {
                    return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "inconsistent USING PLAN state: order_by and using_plan disagree"
                            .to_string(),
                    });
                }
                let bound_result = if let Some(question) = validated.using_plan() {
                    // `LIMIT` の範囲検証（codex-review P1 指摘対応、PR #266）:
                    // 従来は範囲検証（`22000`）が `sql::using_plan::bind_expansion`
                    // 内、すなわち下記の `plan_using_plan_expansion`（辞書スナップ
                    // ショット構築＋LLM クエリ展開＋再埋め込み）・スキーマ事前検証用
                    // `read_txn` のいずれよりも後で行われていた。`LIMIT 0`／
                    // `LIMIT 4294967295` のように構文上は受理されるが必ず拒否される
                    // 入力でも、検証が高コスト処理・DB I/O の後段にあると外部 API・
                    // CPU・メモリ・DB スナップショット取得を consume させてしまい、
                    // untrusted 入力によるリソース増幅になる。fail-closed な拒否は
                    // I/O 開始前に完結させる。`bind_expansion` 側の検証（下記）は
                    // 多層防御として残し、この前倒しチェックとの間で挙動・
                    // `wire_code`・メッセージが食い違わないよう同一関数
                    // （[`crate::sql::parser::validate_search_limit`]）を共有する。
                    crate::sql::parser::validate_search_limit(validated.limit())?;

                    // I/O（LLM 呼び出し）前のスキーマ事前検証（codex-review P1 指摘
                    // 対応、PR #266）: `plan_using_plan_expansion` 内の `plan_query` は
                    // `dictionary_snapshot`（LLM プロンプトの固定接頭辞構築用）を
                    // 経由するが、`dictionary_snapshot` が `path`/`body` 列の存在・
                    // 型・nullability 不備で失敗すると `CoreError::Catalog(
                    // CatalogError::Invalid)` に丸め込まれ、`plan_using_plan_expansion`
                    // の `map_err` で一律 `Internal`（`XX000`）へ変換されてしまう。
                    // body 列欠落・非 TEXT 等の通常の利用者スキーマ不備は本来
                    // `SqlSurfaceError::InvalidInput`（`22000`）であるべきなので、
                    // 同じ判定条件（[`dictionary_required_columns`]）を LLM 呼び出し
                    // 前にこの位置で検証し、満たさなければ `22000` で即座に拒否する
                    // （`read_txn` は判定用のスキーマを読んだら即 drop し、I/O の間
                    // 保持しない。上記コメントの分割方針を踏襲）。この事前検証を
                    // 通過した後に `dictionary_snapshot` 自身が失敗する場合
                    // （デコード不整合・世代競合の再試行枯渇・走査量上限超過等）は
                    // 真にサーバー側の内部/一時的障害であり、引き続き `Internal`
                    // として扱う。
                    // 計画開始時の世代を記録する（codex-review P1 指摘対応、PR #266。
                    // 対象テーブル限定化: codex-review P1 再指摘、PR #266）:
                    // `plan_using_plan_expansion` は `dictionary_snapshot`（LLM プロンプト
                    // 用の固定接頭辞）を、下記の I/O 前スキーマ検証に使う
                    // `pre_check_txn` とは別スナップショットの内部 `read_txn` から構築する
                    // （`DictionaryCache` のドキュメント参照）。I/O（LLM 呼び出し・
                    // 再埋め込み）の間に対象テーブルが `DROP TABLE`→同名再作成される、
                    // またはデータ・スキーマが更新されると、計画時の辞書語彙（旧テーブル
                    // 由来）による展開・再埋め込みベクトルが、I/O 完了後に新規取得する
                    // 最新スキーマ・行データ（新テーブル）へ適用され、テーブルの同一性を
                    // 跨いだ不整合な結果になりうる。当初 `storage.current_generation()`
                    // （ストレージ全体で任意の write commit ごとに単調増加する世代）で
                    // 照合していたが、書き込みが継続する運用では無関係な他テーブル・
                    // 他テナントへの通常の書き込みが 1 回でも I/O 中に完了しただけで
                    // `USING PLAN` が恒常的に `XX000` 拒否される可用性問題を生む
                    // （codex-review P1 再指摘）。対象テーブル（`validated.table_name`）
                    // 固有の世代 [`crate::catalog::table_generation_in_txn`] へ切り替え、
                    // 当該テーブルの DDL（`CREATE`/`DROP`/`ALTER TABLE`）・行書き込みが
                    // あった場合にのみ拒否する（`crate::catalog::
                    // bump_table_generation_in_txn` のドキュメント参照。書き込み経路
                    // すべてで commit 前に呼ばれる契約）。無関係な他テーブルへの書き込みは
                    // 本世代へ影響しない。`user_rows/{table_name}` は複数テナントの行を
                    // 同居させる単一の物理テーブルのため、同一テーブルへの他テナントの
                    // 書き込みは本世代の対象に含める（拒否側に倒す）: `dictionary_snapshot`
                    // が読む行集合は `tenant::visible_rows`（`ctx` に基づく RLS 可視性
                    // 判定。TASK-137・RLS-6, RLS-7）を経由するため、他テナントが
                    // `Visibility::Public` で書き込んだ行は要求元テナントの辞書内容にも
                    // 影響しうる（可視性は `Public`/`Private` の 2 値のみ）。行ごとの
                    // 可視性を見ずにテーブル単位で一括して拒否側へ倒すのは過剰検知
                    // （他テナントの `Private` 専用の書き込みまで拒否対象に含む）を
                    // 許容する設計判断であり、テナント単位の精密な世代を持たないことの
                    // 限界だが、fail-open で見逃すよりも安全側に倒す（security.md
                    // 「fail-closed を維持する」）。この粒度の是非は Issue #285 で
                    // 現状維持として確定した設計判断であり、根拠・移行トリガーは
                    // `docs/design/table-generation-rejection-granularity.md` を参照。
                    // 再計画（辞書再構築・再展開）は行わず、単純に拒否する。
                    let (pre_check_schema, planning_generation) = {
                        let (pre_check_txn, schema) =
                            self.read_txn_with_schema(&validated.table_name)?;
                        let generation = crate::catalog::table_generation_in_txn(
                            &pre_check_txn,
                            &validated.table_name,
                        )
                        .map_err(|e| {
                            crate::sql::allowlist::SqlSurfaceError::Internal {
                                detail: format!("failed to read table generation: {e}"),
                            }
                        })?;
                        drop(pre_check_txn);
                        (schema, generation)
                    };
                    dictionary_required_columns(&pre_check_schema).map_err(|msg| {
                        crate::sql::allowlist::SqlSurfaceError::invalid_input(msg)
                    })?;

                    // `USING MODE` リテラル・`VECTOR` 列の存在・投影列／`WHERE` 述語の
                    // 事前束縛検証（codex-review P1 指摘対応、PR #266）: 上記の辞書用
                    // `path`/`body` 列検証・`LIMIT` 範囲検証と同じ理由で、これらも
                    // I/O（`plan_using_plan_expansion`）より前に完結させる（多層防御
                    // として I/O 後の再束縛でも同じ検証を通す。詳細は
                    // [`crate::sql::using_plan::pre_check_bindable`] のドキュメント参照）。
                    crate::sql::using_plan::pre_check_bindable(
                        &validated,
                        &pre_check_schema,
                        session.udfs(),
                    )?;

                    let planned =
                        self.plan_using_plan_expansion(ctx, session, &validated, question)?;
                    let (read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;

                    // I/O 完了後の世代照合（codex-review P1 指摘対応、PR #266。対象
                    // テーブル限定化: codex-review P1 再指摘、PR #266）: 上記コメントの
                    // とおり、`planning_generation` と現在の**対象テーブル**世代が
                    // 一致しなければ、計画時に使った辞書スナップショット・展開結果・
                    // 再埋め込みベクトルが現在のテーブル世代に対して有効である保証が
                    // ないため、fail-closed に拒否する（`SqlSurfaceError::Internal`。
                    // `plan_using_plan_expansion` 自体の既存 fail-closed 契約
                    // 〔本メソッドのドキュメント参照〕と同じ `XX000` 分類を使い、新規
                    // 分類は追加しない。クライアントへは `Internal::client_message()`
                    // による固定の一般化メッセージのみを返し、他テナント・他クエリの
                    // 書き込み有無という存在情報を漏らさない）。無関係な他テーブルへの
                    // 書き込みでは変化しない世代（[`crate::catalog::table_generation_in_txn`]。
                    // 上記の計画開始時取得箇所のコメント参照）を使うため、書き込みが
                    // 継続する運用でも対象テーブル・辞書が無変化であれば拒否されない。
                    let current_generation =
                        crate::catalog::table_generation_in_txn(&read_txn, &validated.table_name)
                            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: format!("failed to read table generation: {e}"),
                        })?;
                    if current_generation != planning_generation {
                        return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "table generation changed during USING PLAN query \
                                     expansion; rejecting stale plan"
                                .to_string(),
                        });
                    }

                    // I/O 完了後の最新スキーマにも辞書必須列の検証を再適用する
                    // （codex-review P1 指摘対応、PR #266）: 上記の世代照合は対象
                    // テーブル単位の世代（[`crate::catalog::table_generation_in_txn`]。
                    // 粒度の設計判断は `docs/design/
                    // table-generation-rejection-granularity.md` を参照）のみを見る
                    // ため、同一世代内であってもこのスキーマが `pre_check_schema` と
                    // 異なる可能性を狭義には排除できない（世代不変条件が将来変わった
                    // 場合の多層防御。現行の `bump_table_generation_in_txn` 実装では
                    // 対象テーブルへの書き込みごとに必ず世代が進むため通常到達しないが、
                    // `dictionary_required_columns` は軽量な検証であり多層防御として
                    // 維持する）。
                    dictionary_required_columns(&schema).map_err(|msg| {
                        crate::sql::allowlist::SqlSurfaceError::invalid_input(msg)
                    })?;

                    let bound = crate::sql::using_plan::bind_expansion(
                        &validated,
                        &schema,
                        question,
                        &planned.expansion,
                        planned.query_vector,
                        session.udfs(),
                        planned.resolved_mode,
                    )?;
                    (read_txn, schema, bound)
                } else {
                    let (read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;
                    let bound = crate::sql::parser::bind_in_session(
                        &validated,
                        &schema,
                        session.search_mode(),
                        session.udfs(),
                    )?;
                    (read_txn, schema, bound)
                };
                let (read_txn, schema, bound) = bound_result;
                // Issue #357: hybrid 実行が参照する SparseIndex のテーブル世代整合
                // キャッシュ。Issue #363: SQL 表層の SELECT（`USING PLAN` 展開経由を
                // 含む、本アーム全体）は sql_arena_cache（テーブル世代整合キャッシュ）
                // を経由して VectorArena の再構築（redb 全行走査・デコード）を同一
                // テーブル世代内で再利用する（詳細は `SqlArenaCache`・
                // `sql::exec::execute_statement_with_cache` のドキュメント参照）。
                let result = crate::sql::exec::execute_statement_with_cache(
                    &read_txn,
                    self.provider.as_ref(),
                    ctx,
                    &schema,
                    &bound,
                    &self.precision_policy,
                    Some(crate::sql::sparse_cache::SparseCacheAccess {
                        storage: &self.storage,
                        cache: &self.sparse_index_cache,
                    }),
                    Some(crate::sql::arena_cache::ArenaCacheAccess {
                        storage: &self.storage,
                        cache: &self.sql_arena_cache,
                    }),
                )?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // TASK-166（SQL-13）: 集計 SELECT はスキーマ取得（`bind_aggregate` 用）・
            // 行走査（`sql::aggregate::execute_aggregate`）を、既存の検索 SELECT
            // （`Statement::Select` アーム）と同じく単一の `read_txn`（同一
            // スナップショット）上で行う（Issue #56 レビュー指摘対応の踏襲。上記
            // `Statement::Select` アームのドキュメント参照）。
            crate::sql::allowlist::Statement::Aggregate(validated) => {
                let read_txn = self.storage.db().begin_read().map_err(|e| {
                    crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: format!(
                            "failed to begin read transaction: {}",
                            StorageError::from(e)
                        ),
                    }
                })?;
                let schema =
                    crate::catalog::get_table_schema_in_txn(&read_txn, &validated.table_name)
                        .map_err(|e| match e {
                            CatalogError::TableNotFound(name) => {
                                crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                            }
                            other => crate::sql::allowlist::SqlSurfaceError::Internal {
                                detail: format!("failed to load table schema: {other}"),
                            },
                        })?;
                let bound =
                    crate::sql::parser::bind_aggregate(&validated, &schema, session.udfs())?;
                let result =
                    crate::sql::aggregate::execute_aggregate(&read_txn, ctx, &schema, &bound)?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // TASK-78（SQL-6）: `EXPLAIN SELECT ... USING PLAN(...)` は検索本体
            // （ハイブリッド実行）を実行しない。行うのは LIMIT 範囲検証 →
            // 辞書必須列（`path`/`body`）の事前スキーマ検証 → `USING MODE`
            // リテラル・`VECTOR` 列・投影列／`WHERE` 述語の事前束縛検証
            // （[`crate::sql::using_plan::pre_check_bindable`]。PR #267 の是正
            // 対応）→ LLM クエリ展開・モード解決（`Self::plan_query_with_mode`）
            // までで、すべての拒否を LLM I/O 開始前に完結させる（`Statement::Select`
            // アームの `USING PLAN` 経路〔PR #266・#267 の是正方針〕を踏襲。
            // security.md「不安全な設計」対応）。再埋め込み（`Embedder`）は
            // 応答に不要なため呼ばない（`embedder` 未注入でも `EXPLAIN` 可能）。
            crate::sql::allowlist::Statement::Explain(validated) => {
                // `allowlist::validate_sql` は `using_plan` が `Some` の場合のみ
                // `Statement::Explain` を構築する（`sql::allowlist` モジュール
                // ドキュメント参照）ため、ここでの `None` 到達は公開 API の誤用
                // （`ValidatedStatement::new` 等の外部 constructor 経由）時のみの
                // 防御的経路として fail-closed に拒否する。
                let question = validated.using_plan().ok_or_else(|| {
                    crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "EXPLAIN statement missing USING PLAN question".to_string(),
                    }
                })?;

                crate::sql::parser::validate_search_limit(validated.limit())?;

                let query_mode = match validated.search_mode() {
                    Some(literal) => Some(crate::sql::mode::SearchMode::parse_literal(literal)?),
                    None => None,
                };

                // 計画開始時の対象テーブル世代を記録する（codex-review P1 指摘対応、
                // PR #267。`Statement::Select` アームの `USING PLAN` 経路〔上記
                // ドキュメント・[`crate::catalog::table_generation_in_txn`] 参照〕と
                // 同じ理由: `plan_query_with_mode` 内の辞書スナップショット構築・
                // LLM クエリ展開の間に対象テーブルへの DDL（`DROP`/同名再作成含む）・
                // 行書き込みが起きると、`EXPLAIN` が無効化された辞書由来の検索語・
                // ヒントをあたかも現在有効な計画として返してしまう。`EXPLAIN` は
                // 検索本体を実行しないため実データ不整合は生じないが、返す
                // `QUERY PLAN` 自体が古いテーブル世代を前提にした偽の計画になり、
                // 通常 `SELECT ... USING PLAN(...)` 経路の fail-closed 契約との
                // 一貫性を欠く（security.md「不安全な設計」対応）。
                let (pre_check_schema, planning_generation) = {
                    let (pre_check_txn, schema) =
                        self.read_txn_with_schema(validated.table_name())?;
                    let generation = crate::catalog::table_generation_in_txn(
                        &pre_check_txn,
                        validated.table_name(),
                    )
                    .map_err(|e| {
                        crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: format!("failed to read table generation: {e}"),
                        }
                    })?;
                    drop(pre_check_txn);
                    (schema, generation)
                };
                dictionary_required_columns(&pre_check_schema)
                    .map_err(crate::sql::allowlist::SqlSurfaceError::invalid_input)?;

                // `USING MODE` リテラル・`VECTOR` 列の存在・投影列／`WHERE` 述語の
                // 事前束縛検証（codex-review P1 指摘・Cursor Bugbot 指摘対応、PR
                // #267）: `Statement::Select` アームの `USING PLAN` 経路（上記
                // ドキュメント・[`crate::sql::using_plan::pre_check_bindable`]
                // 参照）と同じ理由で、`EXPLAIN` 経路もこの検証を LLM I/O
                // （`plan_query_with_mode` 内のクエリ展開）より前に完結させる。
                // 従来この検証を欠いていたため、対応する通常 `SELECT ... USING
                // PLAN(...)` なら `22000`（未知列・型不正 WHERE・未登録 UDF）で
                // 拒否されるはずのクエリが、`EXPLAIN` 経由では LLM I/O まで実行した
                // うえで成功してしまっていた。
                crate::sql::using_plan::pre_check_bindable(
                    &validated,
                    &pre_check_schema,
                    session.udfs(),
                )?;

                let planned = self
                    .plan_query_with_mode(
                        ctx,
                        validated.table_name(),
                        question,
                        query_mode,
                        session.search_mode(),
                    )
                    .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: format!("EXPLAIN query expansion failed: {e}"),
                    })?;

                // I/O 完了後の世代照合（codex-review P1 指摘対応、PR #267）:
                // `Statement::Select` アームの `USING PLAN` 経路（上記ドキュメント参照）
                // と同じ契約を `EXPLAIN` にも適用する。新しい `read_txn` で対象
                // テーブルの現在世代を取得し、`planning_generation` と一致しなければ
                // `plan_query_with_mode` が使った辞書スナップショット・LLM 展開結果が
                // 現在のテーブル世代に対して有効である保証がないため、fail-closed に
                // 拒否する（`Internal`／`XX000`。クライアントへは
                // `Internal::client_message()` の固定の一般化メッセージのみを返し、
                // 他テナント・他クエリの書き込み有無という存在情報を漏らさない）。
                let (post_check_txn, post_check_schema) =
                    self.read_txn_with_schema(validated.table_name())?;
                let current_generation = crate::catalog::table_generation_in_txn(
                    &post_check_txn,
                    validated.table_name(),
                )
                .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: format!("failed to read table generation: {e}"),
                })?;
                drop(post_check_txn);
                if current_generation != planning_generation {
                    return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "table generation changed during EXPLAIN USING PLAN query \
                                 expansion; rejecting stale plan"
                            .to_string(),
                    });
                }

                // I/O 完了後の最新スキーマにも辞書必須列の検証を再適用する
                // （`Statement::Select` アームの `USING PLAN` 経路〔上記ドキュメント参照〕
                // と同じ多層防御）: 上記の世代照合はストレージ全体の粗い世代のみを見る
                // ため、同一世代内であってもこのスキーマが `pre_check_schema` と異なる
                // 可能性を狭義には排除できない。現行の `bump_generation_and_commit`
                // 実装では書き込みごとに必ず世代が進むため通常到達しないが、
                // `dictionary_required_columns` は軽量な検証であり多層防御として維持する。
                dictionary_required_columns(&post_check_schema)
                    .map_err(crate::sql::allowlist::SqlSurfaceError::invalid_input)?;

                let result = crate::sql::explain::build_explain_result(&planned);
                Ok(crate::sql::SqlOutcome::Explain(result))
            }
        }
    }

    /// `USING PLAN('<query>')`（TASK-77・SQL-5）経路のうち、スキーマに依存しない
    /// I/O 部分（LLM によるクエリ展開・再埋め込み）だけを行う。呼び出し元は
    /// [`Self::execute_sql_in_session`] の `Statement::Select` アームのみ
    /// （`validated.using_plan()` が `Some` のとき）。`self.embedder`／
    /// `self.query_planner` はいずれも private フィールドで `sql::using_plan`
    /// （束縛の純粋なロジックのみを持つ）からは不可視なため、これらへアクセスする
    /// 処理（LLM 展開・再埋め込みの実行そのもの）は本メソッドに置く。
    ///
    /// 列インデックス解決（`sql::using_plan::bind_expansion`）は本メソッドに含めない
    /// （codex-review P1 指摘対応。呼び出し元が本メソッドの結果を使って I/O 完了後に
    /// 取得し直した最新スキーマ・`read_txn` の下で `bind_expansion` を呼ぶことで、
    /// I/O 中の DDL によるスキーマ食い違いを避ける。上記呼び出し元アームの
    /// ドキュメント参照）。
    ///
    /// fail-closed: プランナー未注入・埋め込み未注入・展開失敗・再埋め込み失敗は
    /// いずれも [`crate::sql::allowlist::SqlSurfaceError::Internal`]（`XX000`。ERR-2
    /// の既存分類。新規分類は追加しない）で拒否する。detail には [`CoreError`]／
    /// [`crate::embedding::EmbedError`] の固定文言（プロンプト本文・LLM 応答本文を
    /// 含まない、`query_planner.rs`・`embedding.rs` の P0 方針）のみを使う。
    fn plan_using_plan_expansion(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        validated: &crate::sql::allowlist::ValidatedStatement,
        question: &str,
    ) -> Result<UsingPlanExpansionResult, crate::sql::allowlist::SqlSurfaceError> {
        let embedder = self.embedder.as_deref().ok_or_else(|| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "no embedder configured for USING PLAN".to_string(),
            }
        })?;

        // `plan_query`（辞書スナップショット構築＋ LLM 呼び出し、高コスト I/O）より
        // 前に、展開前の原質問（`query_planner::sanitize_question` を通した後）だけで
        // 疎側の入力上限（`sparse::validate_query_bounds`）を満たせるか検証する
        // （codex-review P1 + Cursor Bugbot Medium 指摘対応、PR #266）。展開は検索語を
        // 追加するだけで既存の語を減らさないため、原質問の時点で `MAX_QUERY_BYTES`・
        // `MAX_QUERY_TERMS` を超える場合、展開後の検証（本メソッド末尾。多層防御として
        // 残す）まで待っても絶対に成功しえない。特に `MAX_QUESTION_CHARS` 文字以内の
        // CJK 質問は、`sparse::tokenize` が CJK 文字ごとに unigram／隣接文字との
        // bigram を生成するため、バイト長は上限内でも一意語数だけが
        // `MAX_QUERY_TERMS` を超えうる。ここで前倒し拒否しないと、成功不能な入力の
        // ためだけに辞書スナップショット構築・LLM 呼び出しという I/O を消費してから
        // 22000 で拒否することになり、untrusted 入力によるリソース増幅になる。
        let sanitized_question = crate::query_planner::sanitize_question(question);
        crate::sparse::validate_query_bounds(&sanitized_question).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "hybrid query text exceeds allowed length",
            )
        })?;

        let expansion = self
            .plan_query(ctx, validated.table_name(), question)
            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("USING PLAN query expansion failed: {e}"),
            })?;

        // PLAN-10 ポインタ: 密側（再埋め込み）と疎側（`hybrid_search` の全文検索側）は
        // 別々のテキストを使う（codex-review P1 指摘対応、PR #266）。密側は
        // `query_planner::render_reembedding_text` が課す既存の再埋め込み規則
        // （固定接頭辞 `search_query: `・`MAX_SEARCH_TERMS`/`MAX_TERM_LEN` による
        // 検索語の防御的上限）に必ず従わせる。`sql::using_plan::expanded_query_text`
        // （接頭辞なし・上限なしの単純結合）をそのまま `embed_batch` へ渡すと、既存の
        // 増分索引・クエリ展開受け入れ検証（TASK-114・PLAN-10）が前提とする再埋め込み
        // 入力と異なるベクトルになり、両経路の Recall 特性が食い違う。疎側の検索
        // テキストは従来どおり `expanded_query_text`（`sql::using_plan::bind_expansion`
        // が疎側へ渡す）を使う。
        let sparse_query_text = crate::sql::using_plan::expanded_query_text(question, &expansion);

        // 疎側 `hybrid_search`（`sparse::SparseIndex::search`/`search_within`）が
        // 課すクエリ入力検証（`MAX_QUERY_BYTES`・`MAX_QUERY_TERMS`）を、密側の
        // 再埋め込み（`embedder.embed_batch`、高コスト I/O）より前に行う
        // （codex-review P1 指摘対応、PR #266）。`expanded_query_text` は原質問
        // （最大 `MAX_QUESTION_CHARS` 文字）＋展開検索語（最大 `MAX_SEARCH_TERMS` 件 ×
        // `MAX_TERM_LEN` 文字）を無条件に連結するため、CJK のような多バイト文字を
        // 多用する展開結果では文字数上限内でも結合後のバイト長が `MAX_QUERY_BYTES` を
        // 超えうる。検証を後段の `hybrid_search` 呼び出し時
        // （`sql::exec::map_hybrid_error`）にのみ委ねると、再埋め込みという高コスト
        // I/O を消費してから拒否することになり、untrusted 入力によるリソース増幅に
        // なる。fail-closed（`22000`。`map_hybrid_error` が `hybrid_search` 経由で
        // 課す既存のエラー契約と同一の `wire_code`・文言）でここで前倒し拒否する
        // （`map_hybrid_error` 側の検証は多層防御として残る）。
        crate::sparse::validate_query_bounds(&sparse_query_text).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "hybrid query text exceeds allowed length",
            )
        })?;

        // 密側の再埋め込み対象は `query_planner::render_reembedding_text`
        // （TASK-114・PLAN-10）が課す既存の再埋め込み規則（固定接頭辞・検索語の
        // 防御的上限）に従わせる。次元検証・非有限値検証は本メソッドでは行わず、
        // 呼び出し元 `execute_sql_in_session` が I/O 完了後に呼ぶ
        // `sql::using_plan::bind_expansion` に一本化する（TASK-77 が当初から採る
        // 既存の役割分担。両検証の実装は `bind_expansion` 内、`vector_column` に
        // よる次元突き合わせ・非有限値拒否の箇所を参照）。
        let dense_query_text = crate::query_planner::render_reembedding_text(question, &expansion);
        let embedded = embedder
            .embed_batch(&[dense_query_text.as_str()])
            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("USING PLAN re-embedding failed: {e}"),
            })?;
        // 要求は 1 件（`dense_query_text` の 1 要素スライス）のため、応答も
        // 厳密に 1 件でなければ untrusted な `Embedder` 実装の契約違反として
        // fail-closed に拒否する（codex-review P1 指摘対応、PR #266）。
        // 以前は `into_iter().next()` で先頭 1 件のみを黙って採用しており、
        // 複数ベクトルを返す契約違反応答を成功として扱っていた。
        // `query_planner::reembed_expansion` が課す同種の検証
        // （`vectors.len() != 1` を `EmbedError::InvalidResponse` で拒否）と
        // 揃えつつ、本メソッドの既存エラー契約（`SqlSurfaceError::Internal`・
        // `XX000`）は変えない最小差分とする。
        if embedded.len() != 1 {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "embedder returned unexpected vector count for USING PLAN query"
                    .to_string(),
            });
        }
        let query_vector = embedded.into_iter().next().ok_or_else(|| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "embedder returned no vector for USING PLAN query".to_string(),
            }
        })?;

        // `USING MODE`／`SET search_mode` の優先順位解決は既存の検索 SELECT 経路
        // （`sql::parser::bind_in_session`）と同一の規則（クエリ句 > セッション変数 >
        // 既定）を踏襲する。スキーマに依存しないためここで解決してよい。
        let query_mode = match validated.search_mode() {
            Some(literal) => Some(crate::sql::mode::SearchMode::parse_literal(literal)?),
            None => None,
        };
        // TASK-164（PLAN-11）: プランナー推定（`expansion.mode_hint`）も優先順位
        // 解決へ含める（明示指定〔クエリ句・セッション変数〕> プランナー推定 >
        // 既定。codex-review P1 指摘対応: 従来はここで `resolve_mode` を使い
        // `expansion.mode_hint` を素通しで捨てていたため、明示指定もセッション
        // 設定もない `USING PLAN` クエリでプランナーが `precision` と推定しても
        // 常に既定の `recall` になっていた）。
        let resolved_mode = crate::sql::mode::resolve_mode_with_planner(
            query_mode,
            session.search_mode(),
            expansion.mode_hint,
        );

        Ok(UsingPlanExpansionResult {
            expansion,
            query_vector,
            resolved_mode,
        })
    }

    /// `table` へ新規行を 1 件挿入する（TASK-95・対象ビヘイビア: RECOVER-4）。
    /// `crate::tenant::insert_row`（テナント境界付き書き込みガード）への薄い委譲のみで、
    /// テナント比較・所有権判定のロジックは本メソッドに書かない。`VectorCore` trait
    /// へは昇格しない固有メソッド（`execute_sql` と同じ理由。`core-api-check` の対象外。
    /// wire 層が DML を行う際の入口はこのメソッド経由を想定し、SQL `INSERT` 表層
    /// （TASK-80/81/120）はこの経路の上に載せる前提）。
    ///
    /// TASK-92（対象ビヘイビア: RECOVER-1）: `crate::tenant::insert_row` へ委譲する前に
    /// `self.ledger_mode.require(operation_id)` を通す（詳細は
    /// `recovery::required_op_id` モジュールドキュメント参照）。
    pub fn insert_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        id: u64,
        row: &crate::storage::RowInput<'_>,
        operation_id: Option<&OperationId>,
    ) -> Result<(), crate::tenant::TenantWriteError> {
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::insert_row_unchecked(&self.storage, table, ctx, id, row, ledger_write)
    }

    /// `table` の既存行を 1 件更新する（TASK-95・対象ビヘイビア: RECOVER-4）。
    /// [`Self::insert_row`] と同じく `crate::tenant::update_row` への薄い委譲だが、
    /// 委譲前に同じ `operation_id` 必須化ガードを通す（TASK-92・RECOVER-1）。
    pub fn update_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        id: u64,
        row: &crate::storage::RowInput<'_>,
        operation_id: Option<&OperationId>,
    ) -> Result<(), crate::tenant::TenantWriteError> {
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::update_row_unchecked(&self.storage, table, ctx, id, row, ledger_write)
    }

    /// `table` の既存行を 1 件削除する（TASK-95・対象ビヘイビア: RECOVER-4）。
    /// [`Self::insert_row`] と同じく `crate::tenant::delete_row` への薄い委譲だが、
    /// 委譲前に同じ `operation_id` 必須化ガードを通す（TASK-92・RECOVER-1）。
    pub fn delete_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        id: u64,
        operation_id: Option<&OperationId>,
    ) -> Result<(), crate::tenant::TenantWriteError> {
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::delete_row_unchecked(&self.storage, table, ctx, id, ledger_write)
    }

    /// `table` に `op_id` が台帳記録済みかを照会する（TASK-93、対象ビヘイビア:
    /// RECOVER-2）。`crate::tenant::operation_recorded` への薄い委譲のみ。
    ///
    /// `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）では台帳テーブルへ
    /// 一切触れず [`LedgerLookup::NoLedger`] を返す（「未記録」と誤認させない
    /// fail-closed な区別。`recovery::ledger` モジュールドキュメント参照）。
    /// `VectorCore` trait へは昇格しない（`execute_insert_sql` と同じ理由。
    /// `core-api-check` の対象外）。
    pub fn operation_recorded(
        &self,
        ctx: &PolicyContext,
        table: &str,
        op_id: &OperationId,
    ) -> Result<crate::recovery::ledger::LedgerLookup, crate::tenant::TenantWriteError> {
        use crate::recovery::ledger::LedgerLookup;
        if matches!(self.ledger_mode, LedgerMode::CompareOnlyWithoutLedger) {
            return Ok(LedgerLookup::NoLedger);
        }
        let recorded = crate::tenant::operation_recorded(&self.storage, table, ctx, op_id)?;
        Ok(if recorded {
            LedgerLookup::Recorded
        } else {
            LedgerLookup::NotRecorded
        })
    }

    /// `table` の最終 commit 済み `operation_id` を照会する（TASK-98、対象ビヘイビア:
    /// RECOVER-7。契約の詳細は spec 参照）。`crate::tenant::last_operation` への
    /// 薄い委譲のみ。
    ///
    /// `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）では
    /// [`Self::operation_recorded`] と同じく台帳テーブルへ一切触れず
    /// [`LastOperationLookup::NoLedger`] を返す（`NotFound` へ丸めない）。
    /// `LastOperationLookup::Unavailable` を返すべきケースについては
    /// `recovery::ledger` モジュールドキュメント参照（codex-review P1 指摘対応）。
    /// `VectorCore` trait へは昇格しない（`operation_recorded` と同じ理由。
    /// `core-api-check` の対象外）。
    pub fn last_operation_id(
        &self,
        ctx: &PolicyContext,
        table: &str,
    ) -> Result<crate::recovery::ledger::LastOperationLookup, crate::tenant::TenantWriteError> {
        use crate::recovery::ledger::{LastOperationLookup, LastOperationRaw};
        if matches!(self.ledger_mode, LedgerMode::CompareOnlyWithoutLedger) {
            return Ok(LastOperationLookup::NoLedger);
        }
        let found = crate::tenant::last_operation(&self.storage, table, ctx)?;
        Ok(match found {
            LastOperationRaw::Found(op_id) => LastOperationLookup::Committed(op_id),
            LastOperationRaw::NotFound => LastOperationLookup::NotFound,
            // `last_op` テーブル導入（TASK-98）前の DB に旧 `op_ledger` 記録だけが
            // 残っているケース。正確な最終 `operation_id` を復元できないため
            // `NotFound` へ丸めず fail-closed に区別する（codex-review P1 指摘対応。
            // `LastOperationLookup::Unavailable` のドキュメント参照）。
            LastOperationRaw::LegacyLedgerWithoutLastOp => LastOperationLookup::Unavailable,
        })
    }

    /// 新規構築した `PrefilterSnapshot` を [`PrefilterCache::insert`] へ渡し、その
    /// 結果に応じて検索経路を分岐する（`VectorCore::search` のキャッシュミス経路
    /// からのみ呼ぶ。テスト（[`Self::search_with_built_snapshot`] を直接呼ぶ判別
    /// テスト）から呼びやすいよう `VectorCore::search` 本体から切り出した。
    /// Issue #280）。`insert` が `Some` を返せば挿入対象は現在世代と整合済みなので
    /// そのまま [`Self::search_with_snapshot`] で検索する。`None`（挿入対象自身が
    /// 構築完了から挿入までの間に stale になった、またはロック毒化・世代読み取り
    /// 失敗で判定不能）の場合はキャッシュへ一切触れず [`Self::search_uncached`] へ
    /// 1 回だけ縮退する。この縮退は「`search_with` が事後に `IndexStale` を検出して
    /// `search_uncached` へ縮退する」既存経路と観測可能な結果が同値である（世代は
    /// 単調増加のため、stale なスナップショットの `search_with` は必ず
    /// `IndexStale` を返す。無駄な `search_with` 試行を 1 回省くだけで、結果側の
    /// fail-closed 契約自体は変えない）。
    fn search_with_built_snapshot(
        &self,
        table: &str,
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
        snapshot: PrefilterSnapshot,
    ) -> Result<Vec<SearchHit>, CoreError> {
        match self
            .prefilter_cache
            .insert(&self.storage, table, ctx, snapshot)
        {
            Some(snapshot) => self.search_with_snapshot(table, ctx, query, k, snapshot),
            None => self.search_uncached(ctx, table, query, k),
        }
    }

    /// キャッシュ済み（または挿入直後の）`PrefilterSnapshot` に対して検索する
    /// （TASK-169）。`snapshot.search_with` が `IndexStale`/`ContextMismatch` を返した
    /// 場合は [`PrefilterCache::evict`] で該当エントリを破棄し、非キャッシュ経路
    /// （[`Self::search_uncached`]）で 1 回だけ検索し直す（stale なインデックスの結果を
    /// 返す経路を作らない。再構築の無限ループも発生しない。`core.rs` モジュール
    /// ドキュメント・[`PrefilterCache`] のドキュメント参照）。
    fn search_with_snapshot(
        &self,
        table: &str,
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
        snapshot: Arc<PrefilterSnapshot>,
    ) -> Result<Vec<SearchHit>, CoreError> {
        match snapshot.search_with(&self.storage, ctx, self.provider.as_ref(), query, k) {
            Ok(hits) => Ok(hits),
            Err(RlsError::IndexStale) | Err(RlsError::ContextMismatch) => {
                self.prefilter_cache.evict(table, &snapshot);
                self.search_uncached(ctx, table, query, k)
            }
            Err(e) => Err(map_rls_error(e)),
        }
    }

    /// キャッシュを使わずアリーナを都度構築して検索する（TASK-169 以前の
    /// `EngineCore::search` 本体と同一の実装。[`Self::search_with_snapshot`] が
    /// stale 縮退時に 1 回だけ呼ぶ経路として切り出した）。呼び出し元
    /// （[`VectorCore::search`]）が `k`・次元・有限性の早期検証を既に済ませている前提で、
    /// アリーナ構築〜provider 呼び出し〜Top-k 契約検証までを行う。
    fn search_uncached(
        &self,
        ctx: &PolicyContext,
        table: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, CoreError> {
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
        // provider へ渡す識別子はスロット番号（[`slot_ids_for`] のドキュメント参照。
        // 行 `id` は 1 つの可視集合内で一意とは限らないため識別子に使えない。TABLE-12）。
        let slot_ids = slot_ids_for(&arena)?;
        let input = SearchInput {
            ids: &slot_ids,
            vectors: arena.vectors(),
            dim: arena.dim(),
            query,
            k,
        };
        let hits = self.provider.search(input)?;

        // provider が返した結果が Top-k の契約を満たすことをコア側で検証する
        // （codex P1・Issue #137 対応）。可視集合所属だけでは「他テナントの行は
        // 混入していないが、件数超過・id 重複・非有限スコア・順序違反」を見逃す
        // ため、共有ヘルパ `provider_result_is_valid`（`rls.rs::PrefilterSnapshot::
        // search_with` と共通、TASK-133）で単一走査を確認し、1 件でも違反すれば結果を
        // 一切返さず fail-closed に拒否する（部分的なフィルタリング・並べ替えはしない）。
        //
        // 検証対象はスロット識別子の集合（スロットは重複しないため各件数は 1）。
        let visible_id_counts = visible_id_counts(&slot_ids);
        if !provider_result_is_valid(&hits, k, &visible_id_counts) {
            return Err(CoreError::ProviderResultRejected);
        }
        // 検証を通過した候補だけをテナント修飾済みヒットへ解決する（TABLE-12・RLS-9）。
        resolve_slot_hits(&arena, &hits).ok_or(CoreError::ProviderResultRejected)
    }

    /// SQL 表層の単一 INSERT 文実行エントリポイント（TASK-80、対象ビヘイビア:
    /// SQL-10）。`execute_sql`（TASK-75、SELECT 専用）とは独立した固有メソッドと
    /// する。`VectorCore` trait への昇格は行わない（`crates/engine/api/
    /// core_api.snapshot` が対象とするのは `VectorCore` trait 本体のみのため、
    /// 本メソッドの追加はコア API シグネチャ安定性チェックに影響しない。
    /// `execute_sql` と同じ理由）。
    ///
    /// `sql::allowlist::validate_insert`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略（明示 `NULL` を含む）は、`self.ledger_mode`
    /// が `LedgerMode::Ledgered`（既定）である限りこの段階で `23502` として拒否され、
    /// 書き込みトランザクションは一切開始されない。TASK-92・対象ビヘイビア:
    /// RECOVER-1）→ `Storage::get_table_schema`（スキーマ取得）→
    /// `sql::parser::bind_insert`（意味論検証・束縛）→ `sql::exec::execute_insert`
    /// （単一 write トランザクションでの実行）の順に呼ぶ。
    pub fn execute_insert_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_insert(sql, &self.storage, self.ledger_mode)?;
        let schema = self
            .storage
            .get_table_schema(&stmt.table_name)
            .map_err(|e| match e {
                CatalogError::TableNotFound(name) => {
                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                }
                // `TableNotFound` 以外（`CorruptSchema` の格納済みカタログ断片・
                // `Backend` の redb I/O 情報等）は detail へ一切展開しない固定文言に
                // 丸める（codex-review P0 指摘・PR #189。`CatalogError::CorruptSchema`
                // 自身の「wire クライアントへは detail を渡さない」契約と
                // security.md P0「エラー経由で内部情報・存在情報を漏らさない」対応）。
                _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "failed to load table schema".to_string(),
                },
            })?;
        let bound = crate::sql::parser::bind_insert_form(&stmt, &schema)?;
        match bound {
            crate::sql::parser::BoundInsertForm::Row(bound) => {
                crate::sql::exec::execute_insert(&self.storage, ctx, &bound, self.ledger_mode)
            }
            crate::sql::parser::BoundInsertForm::File(bound) => {
                crate::sql::exec::execute_file_insert(
                    &self.storage,
                    ctx,
                    self.embedder.as_deref(),
                    &self.incremental_config,
                    &bound,
                    self.ledger_mode,
                )
            }
        }
    }

    /// SQL 表層のバッチ INSERT 実行エントリポイント（TASK-122、対象ビヘイビア:
    /// INDEX-4）。[`Self::execute_insert_sql`] の複数ファイル版で、複数ファイルを
    /// 1 バッチとして受け取る engine ローカル API の入口（SQL 表層に複数文・複数行
    /// VALUES の構文拡張は導入しない。1 文 = 1 ファイルの検証済み `INSERT` 文の列
    /// （`sqls`）を 1 バッチとして受け取る）。`VectorCore` trait への昇格は行わない
    /// （`execute_insert_sql` と同じ理由）。
    ///
    /// 手順:
    /// 1. `sqls` が空なら `22000`（invalid input）で拒否する。
    /// 2. 各文を `sql::allowlist::validate_insert`（`operation_id` 必須化ガード
    ///    （TASK-92・RECOVER-1）を含む）→ `sql::parser::bind_insert_form` で束縛する。
    ///    **全文がファイル形であることを要求**し、行形が 1 件でも混在したら `22000`
    ///    で拒否する（黙って別セマンティクスへ丸めない）。
    /// 3. 束縛結果から `operation_id` を `self.ledger_mode.resolve` で台帳書き込み
    ///    指示へ解決し（TASK-93・RECOVER-2。行形・単一ファイル形と同じ契約）、
    ///    `incremental::index_file_batch` へまとめて委譲する。一括投入 4 上限
    ///    （`self.batch_limits`。TASK-122）の判定自体は `index_file_batch` が
    ///    埋め込み・write トランザクションのいずれよりも前に行う契約
    ///    （`batch_limits.rs`・`incremental.rs` モジュールドキュメント参照）。
    ///
    /// 上限超過時は redb・インメモリ索引・`operation_id` 台帳のいずれも変更されない
    /// （`incremental::index_file_batch` の副作用ゼロ契約）。上限非起因の途中失敗
    /// （例: 2 ファイル目の埋め込み失敗）は文単位セマンティクスとなり、既に処理済みの
    /// 先行ファイルはそのまま索引化された状態で残る（`incremental::index_file_batch`
    /// ドキュメント参照）。
    pub fn execute_insert_sql_batch(
        &self,
        ctx: &PolicyContext,
        sqls: &[&str],
    ) -> Result<Vec<crate::sql::exec::InsertOutcome>, crate::sql::allowlist::SqlSurfaceError> {
        if sqls.is_empty() {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "INSERT batch must contain at least one statement",
            ));
        }

        // ①（バッチあたり最大ファイル数）はここで先に判定できる（`sqls.len()` は
        // 束縛前から既知）。①だけは束縛（`bind_insert_form` の `path`/`body`
        // 文字列複製を伴う）より前に切ることで、ファイル数超過時の無駄な確保・
        // 解析を避ける（coding-rust.md「不安全な設計 / DoS」対応。`batch_limits.rs`
        // 側の判定と重複するが、同じ上限値（`self.batch_limits.max_files_per_batch`）
        // に対する早期リジェクトであり結果は変わらない）。②③（本文サイズ・バッチ
        // 合計サイズ）は各文の束縛（`path`/`body` の `String` 複製を伴う）を経ない
        // と判定できないため、以下の束縛ループ内で束縛直後に逐次判定する
        // （`incremental::index_file_batch` 側の `validate_batch_shape` はその
        // 最終防衛線であり、ここでの早期判定が主防御となる）。
        if sqls.len() > self.batch_limits.max_files_per_batch {
            return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                crate::batch_limits::BatchLimitsError::TooManyFiles {
                    count: sqls.len(),
                    max: self.batch_limits.max_files_per_batch,
                }
                .to_string(),
            ));
        }

        // 束縛結果（`BoundFileInsert`）の所有権をここで保持する。後段で構築する
        // `BoundFileIndexInput`／`LedgerWrite` はこの `Vec` の要素を借用するため、
        // 束縛と `items` 構築を 2 パスへ分ける（`sql::parser::BoundFileInsert` の
        // 生存期間を `index_file_batch` 呼び出しまで維持するため）。
        let mut file_binds: Vec<crate::sql::parser::BoundFileInsert> = Vec::new();
        file_binds.try_reserve_exact(sqls.len()).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "failed to reserve batch bind buffer".to_string(),
            }
        })?;

        // ⓪（束縛前の生 SQL テキスト長に対する粗い早期リジェクト。
        // `batch_limits::validate_raw_sql_len`）を各文の束縛（`validate_insert` の
        // `lexer::tokenize`・`bind_insert_form` の `path`/`body` 文字列複製）より
        // 前に判定する（codex-review P1 指摘・PR #242 対応。②③の逐次判定は
        // decode 済みの `bound.path`/`bound.body` 長にしか作用せず、束縛処理自体
        // （構文木の構築・文字列複製）は判定前に必ず入力サイズへ比例して走って
        // しまうため、単一の巨大 SQL 文だけでファイル数上限（①）を経ずに
        // メモリ・CPU を消費させる DoS 経路が残っていた。⓪は生テキスト長のみを
        // 見る保守的な予算判定であり、正確な上限は引き続き②③（束縛後）が担う
        // ため置き換えではない。`batch_limits.rs` の `validate_raw_sql_len` ドキュ
        // メント参照）。
        //
        // ②③（1 ファイルあたり最大本文サイズ・バッチ合計最大サイズ）を束縛直後
        // ここで逐次判定する（`batch_limits.rs` の `validate_batch_shape` は
        // `index_file_batch` 側で全ファイルの束縛完了後に改めて呼ばれる最終防衛線
        // だが、束縛（`bind_insert_form` による `path`/`body` の `String` 複製）を
        // 全文について終えてからでは判定が遅く、`batch_limits.rs` モジュール
        // ドキュメントが掲げる「本文・パスの複製や追加確保を行わない」DoS 対策の
        // 意図に反する。ここでは束縛済みの `bound.path`/`bound.body` の長さのみを
        // 見て、違反を検出した時点のファイルで打ち切ることで、複製の増幅を
        // 「違反ファイルまで」に抑える）。
        let mut running_total_bytes: usize = 0;
        let mut running_raw_sql_bytes: usize = 0;
        for (index, sql) in sqls.iter().enumerate() {
            running_raw_sql_bytes = crate::batch_limits::validate_raw_sql_len(
                index,
                sql.len(),
                running_raw_sql_bytes,
                &self.batch_limits,
            )
            .map_err(|e| {
                crate::sql::allowlist::SqlSurfaceError::payload_too_large(e.to_string())
            })?;

            let stmt =
                crate::sql::allowlist::validate_insert(sql, &self.storage, self.ledger_mode)?;
            let schema = self
                .storage
                .get_table_schema(&stmt.table_name)
                .map_err(|e| match e {
                    CatalogError::TableNotFound(name) => {
                        crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                    }
                    _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "failed to load table schema".to_string(),
                    },
                })?;
            let bound = crate::sql::parser::bind_insert_form(&stmt, &schema)?;
            match bound {
                crate::sql::parser::BoundInsertForm::File(file_bound) => {
                    if file_bound.body.len() > self.batch_limits.max_file_body_bytes {
                        return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                            crate::batch_limits::BatchLimitsError::FileBodyTooLarge {
                                index: file_binds.len(),
                                len: file_bound.body.len(),
                                max: self.batch_limits.max_file_body_bytes,
                            }
                            .to_string(),
                        ));
                    }
                    let next_total_bytes = file_bound
                        .path
                        .len()
                        .checked_add(file_bound.body.len())
                        .and_then(|t| running_total_bytes.checked_add(t))
                        .ok_or_else(|| {
                            crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                                crate::batch_limits::BatchLimitsError::BatchTotalTooLarge {
                                    total: usize::MAX,
                                    max: self.batch_limits.max_batch_total_bytes,
                                }
                                .to_string(),
                            )
                        })?;
                    if next_total_bytes > self.batch_limits.max_batch_total_bytes {
                        return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                            crate::batch_limits::BatchLimitsError::BatchTotalTooLarge {
                                total: next_total_bytes,
                                max: self.batch_limits.max_batch_total_bytes,
                            }
                            .to_string(),
                        ));
                    }
                    running_total_bytes = next_total_bytes;
                    file_binds.push(file_bound);
                }
                // 行形が 1 件でも混在した場合は黙って行形として処理せず拒否する
                // （「複数ファイルのバッチ投入」という本メソッドの契約を維持する）。
                crate::sql::parser::BoundInsertForm::Row(_) => {
                    return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                        "INSERT batch requires every statement to be file-form (path/body columns)",
                    ));
                }
            }
        }

        // embedder 未構成エラーは①②③（ファイル数・本文サイズ・バッチ合計サイズ）の
        // 上限判定をすべて終えた後に返す（codex-review 指摘 P1 対応）。ここより前で
        // 返すと、上限超過バッチを embedder 未構成の EngineCore に渡した際に
        // `54000`（payload too large）ではなく embedder 未構成由来の `Internal`
        // （`XX000` 相当）が先に返ってしまい、「上限超過は常に `54000`」という
        // エラー契約（本メソッドドキュメント参照）に反する。
        let embedder = self.embedder.as_deref().ok_or_else(|| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "no embedder configured for file-form insert".to_string(),
            }
        })?;

        let mut items: Vec<crate::incremental::BatchFileIndexItem<'_>> = Vec::new();
        items.try_reserve_exact(file_binds.len()).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "failed to reserve batch item buffer".to_string(),
            }
        })?;
        for bound in &file_binds {
            let ledger_write = self
                .ledger_mode
                .resolve(bound.operation_id.as_ref())
                .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;
            let input = crate::incremental::BoundFileIndexInput {
                table: &bound.table,
                path: &bound.path,
                body: &bound.body,
                template_values: &bound.template_values,
                path_column_index: bound.path_column_index,
                body_column_index: bound.body_column_index,
                vector_column_index: bound.vector_column_index,
            };
            items.push(crate::incremental::BatchFileIndexItem {
                input,
                ledger_write,
            });
        }

        let outcomes = crate::incremental::index_file_batch(
            &self.storage,
            ctx,
            embedder,
            &self.incremental_config,
            &self.batch_limits,
            items,
        )
        .map_err(crate::sql::exec::map_batch_incremental_error)?;

        Ok(outcomes
            .into_iter()
            .map(|outcome| crate::sql::exec::InsertOutcome {
                rows_affected: outcome.rows_replaced as u64,
                incremental: Some(outcome),
            })
            .collect())
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

        // 次元・有限性検証を通過した後にのみキャッシュ照合・アリーナ構築へ進む
        // （TASK-169）。まず [`PrefilterCache::lookup`] で `(table, ctx)` に一致し
        // 世代整合が取れているスナップショットを探す。ヒットすればそのまま
        // `PrefilterSnapshot::search_with` を呼ぶ（`core.rs` モジュールドキュメント・
        // [`PrefilterCache`] のドキュメント参照）。
        if let Some(snapshot) = self.prefilter_cache.lookup(&self.storage, table, ctx) {
            return self.search_with_snapshot(table, ctx, query, k, snapshot);
        }

        // キャッシュミス: `PrefilterSnapshot::build` で新規構築する。ここでの
        // `RlsError::NotFound` は上記の早期照会と同一スナップショットではない
        // （別トランザクション）ため、直前の照会成立後にテーブルが削除された場合の
        // 理論的な競合窓のみで発生しうる。その場合も同様に存在情報を漏らさず
        // `NotFound` へ丸め込む。
        //
        // `PrefilterSnapshot::build` は内部で `VectorArena::build_filtered` へ
        // `ctx.is_visible` をそのまま述語として渡し、不可視行（他テナント行を含む）を
        // アリーナ構築時点で確保しない（codex P0/P2・Issue #137 対応の構築時フィルタを
        // 引き継ぐ。`arena.rs`・`rls.rs` のドキュメント参照）。
        let snapshot = match PrefilterSnapshot::build(&self.storage, table, ctx) {
            Ok(snapshot) => snapshot,
            Err(RlsError::NotFound) => return Err(CoreError::NotFound),
            Err(e) => return Err(map_rls_error(e)),
        };
        self.search_with_built_snapshot(table, ctx, query, k, snapshot)
    }

    fn get_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        tenant_id: &str,
        id: u64,
    ) -> Result<Row, CoreError> {
        // 行 `id` の一意性スコープはテナント内（対象ビヘイビア: TABLE-12）のため、
        // 点取得のキーは `(tenant_id, id)`。`tenant_id` は検索結果
        // （[`crate::kernel::SearchHit::tenant_id`]）等から呼び出し元が渡す行の帰属で、
        // 認可には一切使わない（認可・可視性判定は下の `is_visible` の単一照合パスのみ）。
        // 存在しない行・`ctx` から不可視な行（他テナントの `Private` 行を含む）は
        // 区別せず `NotFound` に統一するため、他テナント行の存在探査には使えない
        // （fail-closed。RLS-9・security.md P0）。
        let row = match self.storage.get_row_from_table(table, tenant_id, id) {
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
        if !ImplicitRlsHook::new(ctx).is_visible(&row.tenant_id, row.visibility) {
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
        let not_found = core.get_row(&ctx, "docs", "tenant-a", 999);
        let invisible = core.get_row(&ctx, "docs", "tenant-a", 1);
        assert!(matches!(not_found, Err(CoreError::NotFound)));
        assert!(matches!(invisible, Err(CoreError::NotFound)));
    }

    // 対象ビヘイビア: RECOVER-4。`EngineCore::update_row`/`delete_row` は
    // `crate::tenant` の書き込みガードへ委譲するだけであることの結合確認
    // （テナント境界の実質的な検証は `tests/tenant_breach.rs` 側。ここでは
    // `EngineCore` からの委譲経路そのものが `NotFound` を返すことだけを確認する）。
    #[test]
    fn update_and_delete_row_reject_other_tenant_as_not_found() {
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
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        let attacker = PolicyContext::new("tenant-a").expect("valid tenant");
        let update_input = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[0.0, 1.0],
            metadata: &[],
        };
        let op_id = OperationId::parse("op-update-delete-not-found").expect("valid operation_id");
        assert!(matches!(
            core.update_row(&attacker, "docs", 1, &update_input, Some(&op_id)),
            Err(crate::tenant::TenantWriteError::NotFound)
        ));
        assert!(matches!(
            core.delete_row(&attacker, "docs", 1, Some(&op_id)),
            Err(crate::tenant::TenantWriteError::NotFound)
        ));

        // データが不変であることを確認する（拒否が実際に永続化を止めていること）。
        let row = core
            .storage
            .get_row_from_table("docs", "tenant-b", 1)
            .expect("row still present");
        assert_eq!(row.tenant_id, "tenant-b");
    }

    // 対象ビヘイビア: RLS-1〜4（TASK-169）。同一 `(table, ctx)` での 2 回目の検索は
    // キャッシュヒットし、結果は 1 回目と同一になる。
    #[test]
    fn search_second_call_with_same_table_and_ctx_hits_the_cache() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let first = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");
        let second = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok");

        let stats = core.prefilter_cache_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(first.len(), second.len());
        assert_eq!(first[0].id, second[0].id);
    }

    // 対象ビヘイビア: RLS-1〜4（TASK-169）。キャッシュ温存中に書き込みが発生すると
    // （世代が進むと）、以後の検索は新しい行を含む結果を返す（stale なインデックスの
    // 結果を返す経路が存在しないことの確認）。
    #[test]
    fn search_reflects_a_write_committed_after_the_cache_was_populated() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let first = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");
        assert_eq!(first.len(), 1);

        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        let second = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok");
        assert_eq!(second.len(), 2);

        let stats = core.prefilter_cache_stats();
        assert!(stats.stale_evictions >= 1);
    }

    // 対象ビヘイビア: RLS-1〜4（TASK-169）。別テーブルへの書き込みでも世代（単一
    // カウンタ）が進むため、無関係なテーブルのキャッシュも失効する
    // （過剰拒否＝fail-closed 方向。過小拒否ではない）。
    #[test]
    fn search_cache_is_invalidated_by_a_write_to_a_different_table() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .create_table(&schema_for("other", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        core.search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");

        core.storage
            .insert_row_into_table(
                "other",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row into other table");

        core.search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok");

        let stats = core.prefilter_cache_stats();
        assert!(stats.stale_evictions >= 1);
        assert_eq!(stats.misses, 2);
    }

    // Issue #179: `drop_table` → 同名・同次元での再作成をまたぐと、drop 前に充填した
    // `PrefilterCache` エントリは世代不一致で破棄され、再作成後は新規行のみを返す
    // （旧行が混入する経路がないことの確認）。
    #[test]
    fn search_cache_is_invalidated_by_drop_table_and_recreate() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let first = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");
        assert_eq!(first.len(), 1);

        core.storage.drop_table("docs").expect("drop table");
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("recreate table");
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row after recreate");

        let second = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok after drop and recreate");
        let ids: Vec<u64> = second.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![2]);

        let stats = core.prefilter_cache_stats();
        assert!(stats.stale_evictions >= 1);
        assert_eq!(stats.misses, 2);
    }

    // Issue #179: drop 後に再作成しないテーブルへの検索は、既存の
    // 「存在情報を漏らさない」契約どおり `CoreError::NotFound` に丸め込まれる。
    #[test]
    fn search_returns_not_found_after_drop_without_recreate() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        core.storage.drop_table("docs").expect("drop table");

        let result = core.search(&ctx, "docs", &[1.0, 0.0], 10);
        assert!(matches!(result, Err(CoreError::NotFound)));
    }

    // 対象ビヘイビア: RLS-1（TASK-169）。異なる `PolicyContext`（別テナント）は別の
    // キャッシュエントリになり、互いの検索結果に混入しない。
    #[test]
    fn search_with_different_tenants_uses_separate_cache_entries_without_leakage() {
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
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        // `Private` は同一テナントの行のみ可視（`Public` は `policy.rs::is_visible` の
        // 契約上テナント跨ぎで可視になるため、テナント境界だけを分離要因にするには
        // `Private` を使い、`with_visibilities` で明示的に許可する必要がある）。
        let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        let ctx_b = PolicyContext::with_visibilities("tenant-b", [Visibility::Private])
            .expect("valid tenant");

        let hits_a = core
            .search(&ctx_a, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-a");
        let hits_b = core
            .search(&ctx_b, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-b");

        assert_eq!(hits_a.len(), 1);
        assert_eq!(hits_a[0].id, 1);
        assert_eq!(hits_b.len(), 1);
        assert_eq!(hits_b[0].id, 2);

        let stats = core.prefilter_cache_stats();
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.entries, 2);
    }

    // 対象ビヘイビア: TASK-169。テーブル不存在は非キャッシュ経路と同じく
    // `CoreError::NotFound` へ丸め込まれる（キャッシュ経路でも存在情報を漏らさない）。
    #[test]
    fn search_not_found_is_preserved_through_the_cache_path() {
        let dir = tempdir();
        let core = new_core(dir.path());
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        assert!(matches!(
            core.search(&ctx, "missing", &[1.0, 0.0], 10),
            Err(CoreError::NotFound)
        ));
    }

    // 対象ビヘイビア: TASK-169（security.md「不安全な設計｜無制限リソース確保
    // （DoS）」対応）。異なる `(table, ctx)` の組を上限超過分だけ検索しても、
    // エントリ数は `MAX_PREFILTER_CACHE_ENTRIES` を超えず、超過分は LRU で追い出される。
    #[test]
    fn search_cache_entries_stay_within_the_configured_capacity() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        for i in 0..(MAX_PREFILTER_CACHE_ENTRIES + 1) {
            let tenant = format!("tenant-{i}");
            let ctx = PolicyContext::new(&tenant).expect("valid tenant");
            core.search(&ctx, "docs", &[1.0, 0.0], 10)
                .expect("search ok");
        }

        let stats = core.prefilter_cache_stats();
        assert!(stats.entries <= MAX_PREFILTER_CACHE_ENTRIES);
        assert!(stats.capacity_evictions >= 1);
    }

    // 対象ビヘイビア: TASK-169（Cursor Bugbot 指摘の回帰テスト）。`PrefilterCache::insert`
    // が挿入対象スナップショット自身の世代を「現在世代」の代用にすると、並行書き込みで
    // 既に古くなったスナップショットを挿入した際、真に新しい既存エントリまで
    // 世代不一致として誤って全破棄してしまう（キャッシュが不当に空になる）。
    // `storage.current_generation()` を正として世代整合を判定していれば、後から書き込み
    // 前提で構築された（結果的に古い）スナップショットを挿入しても、既存の新しい
    // エントリは破棄されず残る。加えて（後続の Cursor Bugbot 指摘・PR #191）、
    // 挿入対象自身が現在世代と不一致（= stale）の場合はそもそもキャッシュへ反映しない
    // （型ドキュメント (0) 参照）。これにより、並行構築中の古いスナップショットの
    // 挿入が「同一キーの既存エントリを一旦取り除いてから push する」経路を経由して
    // 別スレッドが直前に挿入した現在世代の有効エントリを上書き・削除する不具合を防ぐ。
    #[test]
    fn search_insert_does_not_evict_a_newer_entry_using_a_stale_snapshots_own_generation() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 世代 G0 時点で 2 つの異なるテナント向けスナップショットを構築しておく
        // （まだキャッシュへは挿入しない）。
        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx_a).expect("build snapshot a");

        // tenant-b 側は通常の検索経路でキャッシュへ挿入し、世代 G0 のエントリとして
        // 常駐させる。
        core.search(&ctx_b, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-b");
        assert_eq!(core.prefilter_cache_stats().entries, 1);

        // 書き込みで世代を G0 → G1 へ進める（`stale_snapshot` は G0 のまま古くなる）。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // tenant-b のエントリは既に世代 G1 で再構築済み（`search` がキャッシュミスから
        // 再挿入する）とみなし、まず現在の状態を G1 に揃える。
        core.search(&ctx_b, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-b after write");
        assert_eq!(
            core.prefilter_cache_stats().entries,
            1,
            "tenant-b の再構築後エントリが 1 件残っていること"
        );

        // 古い（G0 のままの）`stale_snapshot` を挿入しても、既存の G1 エントリ
        // （tenant-b）が誤って破棄されないこと（Cursor Bugbot 指摘の再現条件）。
        // かつ、挿入対象自身が stale なため tenant-a のエントリとしても追加されない
        // こと（後続の Cursor Bugbot 指摘の修正: stale な挿入はキャッシュへ一切
        // 反映しない）。
        let inserted = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx_a, stale_snapshot);
        assert!(
            inserted.is_none(),
            "挿入対象自身が stale な場合は呼び出し元へも渡さない（Issue #280）"
        );
        let stats = core.prefilter_cache_stats();
        assert_eq!(
            stats.entries, 1,
            "stale な挿入は既存の新しいエントリを失わせてはならず、自身も追加されない"
        );
    }

    // 対象ビヘイビア: TASK-169（Cursor Bugbot 指摘の回帰テスト）。`PrefilterCache::insert`
    // は同一 `(table, ctx)` キーへの挿入で既存エントリを置換し、重複エントリを
    // 積み上げない（[`PrefilterCache::lookup`] は先頭一致のみ参照するため、重複が
    // 残ると無駄に `MAX_PREFILTER_CACHE_ENTRIES` を消費し続ける）。
    #[test]
    fn search_insert_replaces_an_existing_entry_for_the_same_key_instead_of_duplicating() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 同一 (table, ctx) キーへ複数回挿入する（両者とも現在世代 G0 と一致するため
        // `insert` に受理される。重複判定の再現には世代不一致は不要）。
        let snapshot_1 =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot 1");
        let snapshot_2 =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot 2");

        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx, snapshot_1)
            .expect("current generation の snapshot は受理される");
        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx, snapshot_2)
            .expect("current generation の snapshot は受理される");

        assert_eq!(
            core.prefilter_cache_stats().entries,
            1,
            "同一キーへの再挿入は既存エントリを置換し、重複登録してはならない"
        );
    }

    // 対象ビヘイビア: TASK-169（PR #191 codex-review 指摘の回帰テスト）。同一
    // `(table, ctx)` キーに対する並行構築時、旧世代（stale）のスナップショットの
    // 挿入が、既に挿入済みの新しい世代の有効エントリを「同一キーだから」という理由で
    // 上書き・削除してはならない。`search_with` 側の世代検証で stale 結果が漏れる
    // ことはないが、有効なキャッシュを失うと以降の検索が非キャッシュ経路へ縮退する
    // （型ドキュメント (0) 参照）。
    #[test]
    fn search_insert_does_not_overwrite_a_fresher_entry_for_the_same_key_with_a_stale_one() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 世代 G0 でスナップショットを構築しておく（並行スレッドの遅延構築を模す。
        // まだキャッシュへは挿入しない）。
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g0");

        // 書き込みで世代を G0 → G1 へ進める。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 別スレッド（を模した経路）が同一キーで G1 のスナップショットを先に
        // キャッシュへ挿入済みとする。
        let fresh_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g1");
        let fresh_arc = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx, fresh_snapshot)
            .expect("current generation の snapshot は受理される");
        assert_eq!(core.prefilter_cache_stats().entries, 1);

        // 遅れて到着した G0 の stale スナップショットを同一キーへ挿入しても、
        // 既にキャッシュされている G1 の有効エントリが上書き・削除されないこと。
        // 挿入対象自身は stale なので呼び出し元へも渡されない（Issue #280）。
        let stale_inserted =
            core.prefilter_cache
                .insert(&core.storage, "docs", &ctx, stale_snapshot);
        assert!(
            stale_inserted.is_none(),
            "stale な挿入は呼び出し元へも渡さない"
        );

        let cached = core
            .prefilter_cache
            .lookup(&core.storage, "docs", &ctx)
            .expect("同一キーの G1 エントリがキャッシュに残っていること");
        assert!(
            Arc::ptr_eq(&cached, &fresh_arc),
            "stale な挿入によって G1 の有効エントリが差し替えられてはならない"
        );
    }

    // 対象ビヘイビア: TASK-169・RLS-1〜4・Issue #280（判別テスト。`DictionaryCache::insert`
    //〔TASK-109・PR #249〕と同じ「スキャン中に世代が進んだ場合は stale な挿入を `None` で
    // 拒否する」契約を `PrefilterCache::insert` にも適用したことの確認）。世代 G0 で構築した
    // スナップショットを、書き込みで G0 → G1 へ進めた後に挿入すると `None` が返り、
    // キャッシュへも反映されないこと。対照として、G1 で構築し直したスナップショットの
    // 挿入は `Some` を返し、`lookup` で同一 `Arc` が引けること（拒否が過剰でないことの
    // 確認）。
    #[test]
    fn prefilter_cache_insert_rejects_a_stale_snapshot_when_generation_advances_during_build() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 世代 G0 でスナップショットを構築する（読み取り中に並行書き込みが割り込む
        // 状況を模す。まだキャッシュへは挿入しない）。
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g0");

        // 書き込みで世代を G0 → G1 へ進める（構築完了〜挿入の間に世代が進む競合）。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // G0 のまま stale になったスナップショットの挿入は拒否される（`None`）。
        let rejected = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx, stale_snapshot);
        assert!(
            rejected.is_none(),
            "世代不一致の挿入対象は呼び出し元へも渡さない（DictionaryCache::insert と同契約）"
        );
        assert_eq!(
            core.prefilter_cache_stats().entries,
            0,
            "stale な挿入はキャッシュへ反映されない"
        );

        // 対照: G1 で構築し直したスナップショットは受理され、`lookup` で同一 `Arc`
        // が引けること（拒否が過剰でないことの確認）。
        let fresh_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g1");
        let fresh_arc = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx, fresh_snapshot)
            .expect("current generation の snapshot は受理される");
        assert_eq!(core.prefilter_cache_stats().entries, 1);
        let cached = core
            .prefilter_cache
            .lookup(&core.storage, "docs", &ctx)
            .expect("G1 のエントリがキャッシュに残っていること");
        assert!(Arc::ptr_eq(&cached, &fresh_arc));
    }

    // 対象ビヘイビア: TASK-169・RLS-1〜4・Issue #280（受け入れ条件3: 結果側の
    // fail-closed が変わらないことの確認）。`PrefilterSnapshot::build` 完了後、
    // `PrefilterCache::insert` へ渡すまでの間に別の書き込みが世代を進め、挿入対象の
    // スナップショットが stale になったケースを、`EngineCore::search_with_built_snapshot`
    // を直接呼ぶことで決定的に再現する。`insert` が `None` を返し、`search_uncached`
    // へ 1 回だけ縮退することで、stale なスナップショットではなく最新状態で検索
    // される（クエリに最も近い、書き込み後に追加された行が結果へ含まれる）ことを
    // 確認する。
    #[test]
    fn search_falls_back_to_uncached_when_a_freshly_built_snapshot_turns_stale_before_insert() {
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
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 世代 G0（行 id=1 のみ）でスナップショットを構築する（まだ挿入しない）。
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g0");

        // クエリに最も近いベクトルを持つ行 id=2 を書き込み、世代を G0 → G1 へ進める。
        // `stale_snapshot` は行 id=2 を含まないアリーナのまま古くなる。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 構築済みの G0 スナップショットをそのまま渡す（`VectorCore::search` の
        // キャッシュミス経路が「構築 → 挿入」の間で競合に遭った状況を模す）。
        let hits = core
            .search_with_built_snapshot("docs", &ctx, &[0.0, 1.0], 10, stale_snapshot)
            .expect("search ok");

        assert!(
            hits.iter().any(|h| h.id == 2),
            "stale スナップショットではなく最新状態（行 id=2 を含む）で検索されること"
        );
        assert_eq!(
            core.prefilter_cache_stats().entries,
            0,
            "stale なスナップショットはキャッシュへ常駐しない"
        );
    }

    // 対象ビヘイビア: TASK-169（Issue #137 系の provider 検証をキャッシュ経路でも
    // 維持することの確認）。不正な結果を返す provider は、キャッシュヒット経路でも
    // `ProviderResultRejected` で拒否される。
    #[test]
    fn search_rejects_a_rogue_provider_result_even_on_a_cache_hit() {
        struct RogueProvider;
        impl SearchProvider for RogueProvider {
            fn search(&self, _input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
                // アリーナ外の id を返す不正 provider。
                Ok(vec![CandidateHit {
                    id: 999,
                    score: 1.0,
                }])
            }
        }

        let dir = tempdir();
        let core = EngineCore::with_provider(dir.path().join("db.redb"), Box::new(RogueProvider))
            .expect("open engine core with rogue provider");
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 1 回目（キャッシュミス経由）・2 回目（キャッシュヒット経由）のいずれも拒否する。
        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], 10),
            Err(CoreError::ProviderResultRejected)
        ));
        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], 10),
            Err(CoreError::ProviderResultRejected)
        ));
    }

    fn documents_schema() -> TableSchema {
        TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（security.md「不安全な設計｜無制限リソース
    // 確保（DoS）」対応。`search_cache_entries_stay_within_the_configured_capacity`
    // 〔TASK-169〕と同じ意図・手法）。異なる `(table, ctx)` の組を上限超過分だけ
    // 構築しても、`DictionaryCache` のエントリ数は `MAX_DICTIONARY_CACHE_ENTRIES` を
    // 超えず、超過分は `last_used` 最小の LRU エントリから追い出される
    // （`DictionaryCache::insert` の追い出し分岐の回帰テスト）。
    #[test]
    fn dictionary_cache_entries_stay_within_the_configured_capacity() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&documents_schema())
            .expect("create table");

        for i in 0..(MAX_DICTIONARY_CACHE_ENTRIES + 1) {
            let tenant = format!("tenant-{i}");
            let ctx = PolicyContext::new(&tenant).expect("valid tenant");
            core.dictionary_snapshot(&ctx, "documents")
                .expect("dictionary snapshot ok");
        }

        let guard = core
            .dictionary_cache
            .state
            .read()
            .expect("cache lock not poisoned");
        // 上限ちょうどまで縮退していること（`<=` では「1 件も追い出されない」実装への
        // 退行を検知できないため `==` で固定する）。
        assert_eq!(guard.entries.len(), MAX_DICTIONARY_CACHE_ENTRIES);
        // 挿入順が `last_used` 昇順と一致するため、最古（tenant-0）が LRU 追い出しの
        // 対象になり、最新（tenant-{MAX}）は生存しているはずである
        // （どのエントリが追い出されたかまで検証し、件数一致だけの弱い保証を補う）。
        assert!(
            !guard
                .entries
                .iter()
                .any(|e| e.ctx.tenant_id() == "tenant-0"),
            "oldest entry (tenant-0) must be evicted by LRU"
        );
        let newest_tenant = format!("tenant-{MAX_DICTIONARY_CACHE_ENTRIES}");
        assert!(
            guard
                .entries
                .iter()
                .any(|e| e.ctx.tenant_id() == newest_tenant),
            "newest entry must survive LRU eviction"
        );
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（`PrefilterCache::insert` の同種修正・
    // `search_insert_does_not_evict_a_newer_entry_using_a_stale_snapshots_own_generation`
    // 〔TASK-169〕と同じ意図・手法）。`DictionaryCache::insert` は挿入対象自身の
    // `built_generation` が `storage.current_generation()` と不一致（＝並行書き込みで
    // 既に古くなった）場合、既存の新しいエントリを破棄せず・自身も追加しない
    // （`DictionaryCache::insert` の世代不一致分岐の回帰テスト）。
    #[test]
    fn dictionary_cache_insert_does_not_evict_a_newer_entry_using_a_stale_generation() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&documents_schema())
            .expect("create table");

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

        // 世代 G0 時点の辞書を構築しておく（まだキャッシュへは挿入しない）。
        let stale_generation = core.storage.current_generation().expect("read generation");
        let stale_dictionary =
            crate::dictionary::DictionaryBuilder::new(core.dictionary_config.clone()).finish();
        let stale_bytes = stale_dictionary.approx_heap_bytes();

        // tenant-b 側は通常経路でキャッシュへ挿入し、世代 G0 のエントリとして常駐させる。
        core.dictionary_snapshot(&ctx_b, "documents")
            .expect("dictionary snapshot ok for tenant-b");
        assert_eq!(
            core.dictionary_cache
                .state
                .read()
                .expect("cache lock not poisoned")
                .entries
                .len(),
            1
        );

        // 書き込みで世代を G0 → G1 へ進める（`stale_dictionary`/`stale_generation` は
        // G0 のまま古くなる）。
        core.storage
            .insert_row_into_table(
                "documents",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 古い（G0 のままの）辞書を挿入しても、既存の G0 エントリ（tenant-b）が
        // 誤って破棄されないこと。かつ、挿入対象自身が stale なため tenant-a の
        // エントリとしても追加されないこと。戻り値も `None`（PR #249
        // codex-review P1 指摘対応: 呼び出し元へ書き込み未反映のスナップショットを
        // 渡さないための契約。従来は `Arc` をそのまま返しており、
        // `EngineCore::dictionary_snapshot` がこれを正常結果として扱っていた）。
        let result = core.dictionary_cache.insert(
            &core.storage,
            "documents",
            &ctx_a,
            stale_dictionary,
            stale_generation,
            stale_bytes,
        );
        assert!(
            result.is_none(),
            "insert of an already-stale dictionary must return None, not the stale Arc"
        );
        let entries = core
            .dictionary_cache
            .state
            .read()
            .expect("cache lock not poisoned");
        assert_eq!(
            entries.entries.len(),
            1,
            "stale な挿入は既存の新しいエントリを失わせてはならず、自身も追加されない"
        );
        assert_eq!(entries.entries[0].ctx, ctx_b);
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（PR #249 codex-review P1 指摘の回帰テスト）。
    // 可視行の `metadata`（スカラー列ペイロード）が `path`/`body`（非 nullable Text）
    // をデコードできないほど破損している場合、その行を黙ってスキップして内容を
    // 欠いた `Dictionary` を `truncated: false` のまま正常なキャッシュエントリと
    // して保存してはならない。`dictionary_snapshot` がエラーを返し、かつ
    // `DictionaryCache` にエントリが残らないことを確認する。
    #[test]
    fn dictionary_snapshot_fails_closed_on_corrupted_row_and_does_not_cache_it() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&documents_schema())
            .expect("create table");
        // `path`/`body` の presence タグすら読めない空のスカラーペイロード
        // （`row_codec::scan_scalar_columns` は非 nullable 列の途中で打ち切られた
        // バッファを `RowCodecError::Invalid` として拒否する）。
        core.storage
            .insert_row_into_table(
                "documents",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert corrupted row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let err = core
            .dictionary_snapshot(&ctx, "documents")
            .expect_err("corrupted visible row must fail dictionary snapshot construction");
        assert!(matches!(err, CoreError::Catalog(CatalogError::Invalid(_))));

        let guard = core
            .dictionary_cache
            .state
            .read()
            .expect("cache lock not poisoned");
        assert_eq!(
            guard.entries.len(),
            0,
            "a failed build must not leave a cache entry behind"
        );
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（PR #249 codex-review P1 指摘の回帰テスト）。
    // `with_dictionary_config` は `dictionary_config` を差し替えるだけでは足りず、
    // 既構築の `dictionary_cache` も再初期化しなければならない。`(table, ctx)` と
    // `storage.current_generation()` のみをキーにするキャッシュは設定値を含まない
    // ため、世代が変わらない限り旧設定で構築した `Arc<Dictionary>` を返し続けて
    // しまう（設定変更が効かない見えないバグ）。
    #[test]
    fn with_dictionary_config_invalidates_stale_cache_entries() {
        let dir = tempdir();
        let core = new_core(dir.path());
        let schema = documents_schema();
        core.storage.create_table(&schema).expect("create table");
        let values = vec![
            crate::row_codec::Value::Null,
            crate::row_codec::Value::Text("src/example.rs".to_string()),
            crate::row_codec::Value::Text("fn one() {}\nfn two() {}\n".to_string()),
        ];
        let metadata =
            crate::row_codec::encode_scalar_columns(&schema, &values).expect("encode metadata");
        core.storage
            .insert_row_into_table(
                "documents",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &metadata,
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 既定設定（ファイルツリーを有効）で一度構築し、キャッシュへ載せておく。
        let dict_before = core
            .dictionary_snapshot(&ctx, "documents")
            .expect("dictionary snapshot ok before config change");
        assert!(!dict_before.file_tree.paths.is_empty());
        assert_eq!(
            core.dictionary_cache
                .state
                .read()
                .expect("cache lock not poisoned")
                .entries
                .len(),
            1
        );

        // ファイルツリーを無効化する設定へ差し替える。世代は変わっていないため、
        // キャッシュを再初期化していなければ次の呼び出しが旧設定の
        // `Arc<Dictionary>` をそのまま返してしまう。
        let core = core.with_dictionary_config(crate::dictionary::DictionaryConfig {
            enable_file_tree: false,
            enable_term_index: false,
            ..crate::dictionary::DictionaryConfig::default()
        });
        assert_eq!(
            core.dictionary_cache
                .state
                .read()
                .expect("cache lock not poisoned")
                .entries
                .len(),
            0,
            "with_dictionary_config must invalidate the existing dictionary cache"
        );

        let dict_after = core
            .dictionary_snapshot(&ctx, "documents")
            .expect("dictionary snapshot ok after config change");
        assert!(
            dict_after.file_tree.paths.is_empty(),
            "the new config (file tree disabled) must take effect, not a stale cached dictionary"
        );
    }

    // codex-review P1 回帰（PR #266）: `USING PLAN` の I/O フェーズ
    // （`plan_using_plan_expansion`）はスキーマに依存せず、列インデックス解決
    // （`sql::using_plan::bind_expansion`）は I/O 完了後に取得し直した最新スキーマ
    // に対して行う。以前は I/O 前に取得した旧スキーマで列インデックスを含む
    // `BoundStatement` を確定していたため、I/O 中に同名テーブルの
    // `DROP TABLE`→再作成でレイアウトが変わると、束縛済みの列インデックスが
    // 別の列を指す状態になり得た（`execute_sql_in_session` の `Statement::Select`
    // アーム・[`EngineCore::plan_using_plan_expansion`] のドキュメント参照）。
    // ここでは `execute_sql_in_session` と同じ 2 段階（I/O → 束縛）を直接呼び出し、
    // 段階間に DDL を挟んで、列インデックスが post-I/O スキーマを反映することを
    // 固定する。
    #[test]
    fn using_plan_binds_column_indices_against_the_schema_fetched_after_io_not_before() {
        struct StubLlmClient;
        impl crate::query_planner::LlmClient for StubLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }))
            .with_query_planner(Box::new(StubLlmClient));

        // 旧レイアウト: [embedding, path, body]（body index = 2）。
        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table (old layout)");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id_old = OperationId::parse("using-plan-race-old").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("old-path".to_string()),
                crate::row_codec::Value::Text("old-body".to_string()),
            ],
            &op_id_old,
        )
        .expect("insert row (old layout)");

        let session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let stmt = crate::sql::allowlist::validate_sql(sql, &core.storage).expect("valid sql");
        let validated = match stmt {
            crate::sql::allowlist::Statement::Select(v) => v,
            other => panic!("expected Select statement, got {other:?}"),
        };
        let question = validated.using_plan().expect("USING PLAN present");

        // I/O フェーズ（スキーマに依存しない）。
        let planned = core
            .plan_using_plan_expansion(&ctx, &session, &validated, question)
            .expect("plan_using_plan_expansion should succeed");

        // I/O 完了後・束縛前に DDL が挟まる（同名テーブルの列順を入れ替えて再作成）。
        core.storage.drop_table("docs").expect("drop table");
        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                    ColumnDef::new("path", ColumnType::Text, false),
                ],
            ))
            .expect("create table (new layout)");
        let op_id_new = OperationId::parse("using-plan-race-new").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            2,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("new-body".to_string()),
                crate::row_codec::Value::Text("new-path".to_string()),
            ],
            &op_id_new,
        )
        .expect("insert row (new layout)");

        // I/O 完了後に取得し直した最新スキーマで束縛する（本テストの検証対象。
        // 呼び出し元 `execute_sql_in_session` の `Statement::Select` アームと同じ手順）。
        let (read_txn, schema) = core
            .read_txn_with_schema("docs")
            .expect("read_txn_with_schema should succeed");
        let bound = crate::sql::using_plan::bind_expansion(
            &validated,
            &schema,
            question,
            &planned.expansion,
            planned.query_vector,
            session.udfs(),
            planned.resolved_mode,
        )
        .expect("bind_expansion should succeed against the post-I/O schema");

        match bound.ranking() {
            crate::sql::parser::Ranking::Hybrid {
                text_column_index, ..
            } => assert_eq!(
                *text_column_index, 1,
                "text_column_index must reflect the post-I/O schema (body at index 1), not the \
                 pre-I/O one (index 2)"
            ),
            other => panic!("expected Ranking::Hybrid, got {other:?}"),
        }

        let result = crate::sql::exec::execute_statement_with_cache(
            &read_txn,
            core.provider.as_ref(),
            &ctx,
            &schema,
            &bound,
            &core.precision_policy,
            Some(crate::sql::sparse_cache::SparseCacheAccess {
                storage: &core.storage,
                cache: &core.sparse_index_cache,
            }),
            Some(crate::sql::arena_cache::ArenaCacheAccess {
                storage: &core.storage,
                cache: &core.sql_arena_cache,
            }),
        )
        .expect("execute_statement should succeed");
        let row = result
            .rows
            .iter()
            .find(|r| r.id == 2)
            .expect("row inserted under the new layout should be visible");
        assert_eq!(
            row.cells,
            vec![crate::sql::exec::Cell::Text("new-path".to_string())],
            "projected `path` must resolve against the post-I/O schema, not stale pre-I/O indices"
        );
    }

    // codex-review P1 回帰（PR #266）: `USING PLAN` の I/O フェーズ
    // （`plan_using_plan_expansion`）は計画開始時のテーブル世代を保持せず、I/O
    // 完了後は新しい `read_txn`・スキーマを取得するだけで、計画時との世代一致を
    // 照合していなかった。この間に対象テーブルの `DROP TABLE`→同名再作成が挟まると、
    // 旧テーブルの語彙（辞書スナップショット）による展開・再埋め込みベクトルが
    // 新テーブルへ適用され、テーブルの同一性を跨いだ不整合な結果になり得た。
    // 上記の `using_plan_binds_column_indices_against_the_schema_fetched_after_io_not_before`
    // は列インデックスが post-I/O スキーマを正しく反映することを固定するが、
    // 「世代不一致そのものを検出して拒否する」契約は別に固定する必要がある。
    // ここでは `execute_sql_in_session` を直接駆動し（`Statement::Select` アームの
    // 2 段階〔I/O → 束縛〕を経由する唯一の呼び出し元）、`LlmClient::complete`
    // コールバック（I/O フェーズの内部）から対象テーブルを `DROP TABLE`→再作成して
    // 世代を進め、fail-closed に `SqlSurfaceError::Internal` で拒否されることを
    // 検証する（修正前は `read_txn_with_schema` が新テーブルのスキーマ・
    // `read_txn` を黙って返し、クエリが成功していた）。
    #[test]
    fn execute_sql_in_session_rejects_using_plan_when_table_generation_changes_during_io() {
        // `LlmClient::complete`（I/O フェーズの内部）から対象テーブルを
        // `DROP TABLE`→再作成するには、構築中の `EngineCore` 自身（の
        // `storage` フィールド）へアクセスする必要がある。`Box<dyn LlmClient>`
        // は `'static` 境界を要求するため借用では持ち回せず、`unsafe`（生
        // ポインタ）も使わない（`tests/isa.rs::
        // unsafe_is_confined_to_isa_module_with_safety_comments` が
        // `crates/engine/src/**/*.rs` 中 `isa.rs` 以外の `unsafe` を禁止する）。
        // そこで `OnceLock<Weak<EngineCore>>` による安全な二段階初期化
        // （`EngineCore` は wire-server で実際に `Arc<EngineCore>` として
        // 共有される・`server.rs` 参照＝`Send + Sync` 済み）を使い、
        // `complete` 呼び出し時点で必ず設定済みの `Weak` を `upgrade` して
        // 同一 `EngineCore`（同一 `storage`）へアクセスする。
        struct GenerationBumpingLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for GenerationBumpingLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                core.storage.drop_table("docs").expect("drop table mid-io");
                core.storage
                    .create_table(&TableSchema::new(
                        "docs",
                        vec![
                            ColumnDef::new("embedding", ColumnType::Vector(2), false),
                            ColumnDef::new("body", ColumnType::Text, false),
                            ColumnDef::new("path", ColumnType::Text, false),
                        ],
                    ))
                    .expect("create table mid-io (new layout)");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        // 旧レイアウト: [embedding, path, body]。
        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table (old layout)");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = OperationId::parse("using-plan-gen-race").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("old-path".to_string()),
                crate::row_codec::Value::Text("old-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row (old layout)");

        // `core_cell` を `GenerationBumpingLlmClient` と外側スコープの双方で
        // `Arc::clone` して共有する（`Box<dyn LlmClient>` へ包んだ後は具体型
        // へ戻す公開経路が無いため、共有元は `with_query_planner` 呼び出し前に
        // 確保しておく）。`Arc<EngineCore>` は構築完了後にしか作れないため、
        // 二段階初期化（セル確保 → `EngineCore` 構築 → `Arc` 化 → `Weak` 登録）
        // にする。
        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(GenerationBumpingLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Err(crate::sql::allowlist::SqlSurfaceError::Internal { detail }) => {
                assert!(
                    detail.contains("generation changed"),
                    "expected a generation-mismatch rejection, got: {detail}"
                );
            }
            other => panic!(
                "expected SqlSurfaceError::Internal (generation mismatch) rejection, got: \
                 {other:?}"
            ),
        }
    }

    // codex-review P1 再指摘（PR #266）: 上記
    // `execute_sql_in_session_rejects_using_plan_when_table_generation_changes_during_io`
    // が固定した世代照合は、当初ストレージ全体の単調増加世代
    // （`crate::storage::current_generation_in_txn`）と比較していたため、対象テーブル
    // （`docs`）自身は無変化でも、I/O（LLM 呼び出し）の間に**無関係な別テーブル**へ
    // 通常の書き込みが 1 回でも完了すると `USING PLAN` が `XX000` で拒否されていた
    // （書き込みが継続する運用では恒常的に失敗し、書き込み可能な利用者が無関係
    // テナントの `USING PLAN` 検索を事実上妨害できる可用性問題）。対象テーブル固有の
    // 世代（`crate::catalog::table_generation_in_txn`）へ切り替えた後は、無関係な
    // 他テーブルへの書き込みが `docs` の世代に影響しないため、本テストは
    // `Ok` を期待する（修正前は本テストが `fail` していたことを確認済み）。
    #[test]
    fn execute_sql_in_session_using_plan_succeeds_when_an_unrelated_table_is_written_during_io() {
        // `GenerationBumpingLlmClient` と同じ二段階初期化パターン（上記テストの
        // ドキュメントコメント参照）。ここでは `docs`（対象テーブル）ではなく
        // `other_tenant_docs`（無関係な別テーブル・別テナント名義）を
        // `LlmClient::complete` から書き込む。
        struct UnrelatedTableWritingLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for UnrelatedTableWritingLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                // 対象テーブル（`docs`）とは別名の、無関係なテーブルへの通常の書き込み
                // （別テナント名義。TenantWriteError 経由の RLS 境界付き書き込み API を
                // 使い、テスト対象の書き込み経路〔tenant.rs〕を実際に通す）。
                core.storage
                    .create_table(&TableSchema::new(
                        "other_docs",
                        vec![
                            ColumnDef::new("embedding", ColumnType::Vector(2), false),
                            ColumnDef::new("path", ColumnType::Text, false),
                            ColumnDef::new("body", ColumnType::Text, false),
                        ],
                    ))
                    .expect("create unrelated table mid-io");
                let other_ctx =
                    PolicyContext::new("tenant-b").expect("valid tenant for unrelated table");
                let other_op_id =
                    OperationId::parse("unrelated-write-mid-io").expect("valid operation_id");
                crate::tenant::insert_typed_row(
                    &core.storage,
                    "other_docs",
                    &other_ctx,
                    1,
                    Visibility::Public,
                    &[
                        crate::row_codec::Value::Vector(vec![0.5, 0.5]),
                        crate::row_codec::Value::Text("other-path".to_string()),
                        crate::row_codec::Value::Text("other-body".to_string()),
                    ],
                    &other_op_id,
                )
                .expect("insert row into unrelated table mid-io");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table docs");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = OperationId::parse("using-plan-unrelated-write").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("doc-path".to_string()),
                crate::row_codec::Value::Text("doc-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row into docs");

        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(UnrelatedTableWritingLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Ok(crate::sql::SqlOutcome::Query(_)) => {}
            other => panic!(
                "expected USING PLAN to succeed despite an unrelated table being written \
                 during I/O, got: {other:?}"
            ),
        }
    }

    // Issue #285（テーブル単位世代カウンタの拒否精度の設計判断。
    // `docs/design/table-generation-rejection-granularity.md` 参照）: 同 ADR は
    // 「対象テーブルへの他テナントの書き込みは、可視性（`Public`/`Private`）を
    // 問わず拒否する」という現行の意図的な契約（テナント単位・可視性境界単位への
    // 細分化はしない設計判断）を現状維持として確定した。本テストはその契約のうち
    // `Visibility::Public` 側（要求元テナントの辞書内容に実際に影響しうる書き込み。
    // TASK-137・RLS-6, RLS-7 の可視性判定を経由）を固定する。将来 ADR の移行
    // トリガーが成立し可視性境界単位（選択肢 C）へ切り替える場合、本テストは
    // その設計変更に合わせて書き換えが必要になる。
    #[test]
    fn execute_sql_in_session_rejects_using_plan_when_other_tenant_writes_public_row_to_same_table_during_io(
    ) {
        // `GenerationBumpingLlmClient`（本モジュール上部）と同じ二段階初期化
        // パターンを使い、対象テーブル（`docs`）自身へ他テナント（`tenant-b`）
        // 名義で `Visibility::Public` 行を `LlmClient::complete`（I/O フェーズの
        // 内部）から書き込む。
        struct OtherTenantPublicWriteLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for OtherTenantPublicWriteLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                let other_ctx =
                    PolicyContext::new("tenant-b").expect("valid tenant for other-tenant write");
                let other_op_id = OperationId::parse("other-tenant-public-write-mid-io")
                    .expect("valid operation_id");
                crate::tenant::insert_typed_row(
                    &core.storage,
                    "docs",
                    &other_ctx,
                    2,
                    Visibility::Public,
                    &[
                        crate::row_codec::Value::Vector(vec![0.2, 0.3]),
                        crate::row_codec::Value::Text("other-tenant-public-path".to_string()),
                        crate::row_codec::Value::Text("other-tenant-public-body".to_string()),
                    ],
                    &other_op_id,
                )
                .expect("insert Public row into the target table mid-io");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table docs");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id =
            OperationId::parse("using-plan-other-tenant-public").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("doc-path".to_string()),
                crate::row_codec::Value::Text("doc-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row into docs");

        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(OtherTenantPublicWriteLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Err(crate::sql::allowlist::SqlSurfaceError::Internal { detail }) => {
                assert!(
                    detail.contains("generation changed"),
                    "expected a generation-mismatch rejection, got: {detail}"
                );
                assert!(
                    !detail.contains("tenant-b"),
                    "rejection detail must not leak the other tenant's identity: {detail}"
                );
            }
            other => panic!(
                "expected SqlSurfaceError::Internal (generation mismatch) rejection when \
                 another tenant writes a Public row to the same table during I/O, got: {other:?}"
            ),
        }
    }

    // Issue #285（`docs/design/table-generation-rejection-granularity.md` 参照）:
    // 上記テストの `Visibility::Private` 側。要求元テナント（`tenant-a`）からは
    // 不可視な他テナントの `Private` 行書き込みであっても、テーブル単位世代照合は
    // 拒否する（過剰検知を意図的に許容する設計判断。選択肢 C・可視性境界単位への
    // 細分化を採用しない限りこの契約は変わらない）。
    #[test]
    fn execute_sql_in_session_rejects_using_plan_when_other_tenant_writes_private_row_to_same_table_during_io(
    ) {
        struct OtherTenantPrivateWriteLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for OtherTenantPrivateWriteLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                let other_ctx =
                    PolicyContext::new("tenant-b").expect("valid tenant for other-tenant write");
                let other_op_id = OperationId::parse("other-tenant-private-write-mid-io")
                    .expect("valid operation_id");
                crate::tenant::insert_typed_row(
                    &core.storage,
                    "docs",
                    &other_ctx,
                    2,
                    Visibility::Private,
                    &[
                        crate::row_codec::Value::Vector(vec![0.2, 0.3]),
                        crate::row_codec::Value::Text("other-tenant-private-path".to_string()),
                        crate::row_codec::Value::Text("other-tenant-private-body".to_string()),
                    ],
                    &other_op_id,
                )
                .expect("insert Private row into the target table mid-io");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table docs");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id =
            OperationId::parse("using-plan-other-tenant-private").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("doc-path".to_string()),
                crate::row_codec::Value::Text("doc-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row into docs");

        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(OtherTenantPrivateWriteLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Err(crate::sql::allowlist::SqlSurfaceError::Internal { detail }) => {
                assert!(
                    detail.contains("generation changed"),
                    "expected a generation-mismatch rejection, got: {detail}"
                );
                assert!(
                    !detail.contains("tenant-b"),
                    "rejection detail must not leak the other tenant's identity: {detail}"
                );
            }
            other => panic!(
                "expected SqlSurfaceError::Internal (generation mismatch) rejection when \
                 another tenant writes a Private row to the same table during I/O (current \
                 design: over-rejection is intentional per Issue #285), got: {other:?}"
            ),
        }
    }

    // codex-review P1 回帰（PR #266）: `plan_using_plan_expansion` が
    // `Embedder::embed_batch` へ渡すテキストは、`query_planner::
    // render_reembedding_text`（固定接頭辞 `search_query: `・検索語の防御的上限
    // つき）の出力と一致しなければならない。以前は `sql::using_plan::
    // expanded_query_text`（接頭辞なし・上限なしの単純結合）をそのまま渡していた
    // ため、`USING PLAN` 経路の再埋め込みベクトルが既存の増分索引・クエリ展開
    // 受け入れ検証（TASK-114・PLAN-10）が前提とするベクトルと食い違っていた。
    #[test]
    fn plan_using_plan_expansion_embeds_render_reembedding_text_not_expanded_query_text() {
        struct StubLlmClient;
        impl crate::query_planner::LlmClient for StubLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                Ok(
                    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        // 呼び出し元が `embed_batch` へ渡した入力テキストをそのまま記録するスパイ
        // 実装（`Mutex` で束ねて `Sync` を満たす。`Embedder` は `&self` のみで
        // 呼ばれるため内部可変性が必要）。
        struct SpyEmbedder {
            dim: u32,
            seen: std::sync::Mutex<Vec<String>>,
        }
        impl crate::embedding::Embedder for SpyEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                self.seen
                    .lock()
                    .expect("spy lock not poisoned")
                    .extend(texts.iter().map(|t| t.to_string()));
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let spy = std::sync::Arc::new(SpyEmbedder {
            dim: 2,
            seen: std::sync::Mutex::new(Vec::new()),
        });

        // `EngineCore::with_embedder` は `Box<dyn Embedder>` を要求するため、
        // テスト側から観測を続けられるよう `Arc` を経由する薄いラッパーで包む
        // （スパイ本体の所有権は `spy` 側にも残す）。
        struct ArcEmbedder(std::sync::Arc<SpyEmbedder>);
        impl crate::embedding::Embedder for ArcEmbedder {
            fn dim(&self) -> u32 {
                self.0.dim()
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                self.0.embed_batch(texts)
            }
        }

        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(ArcEmbedder(spy.clone())))
            .with_query_planner(Box::new(StubLlmClient));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let session = crate::sql::mode::SessionState::default();
        let sql = "SELECT body FROM docs USING PLAN('find auth') LIMIT 5";
        let stmt = crate::sql::allowlist::validate_sql(sql, &core.storage).expect("valid sql");
        let validated = match stmt {
            crate::sql::allowlist::Statement::Select(v) => v,
            other => panic!("expected Select statement, got {other:?}"),
        };
        let question = validated.using_plan().expect("USING PLAN present");

        let planned = core
            .plan_using_plan_expansion(&ctx, &session, &validated, question)
            .expect("plan_using_plan_expansion should succeed");

        let seen = spy.seen.lock().expect("spy lock not poisoned");
        assert_eq!(
            seen.len(),
            1,
            "embed_batch must be called exactly once for USING PLAN re-embedding"
        );
        let expected = crate::query_planner::render_reembedding_text(question, &planned.expansion);
        assert_eq!(
            seen[0], expected,
            "embed_batch input must match query_planner::render_reembedding_text output \
             (fixed prefix + bounded search terms), not the unprefixed sparse query_text"
        );
        assert!(
            seen[0].starts_with(crate::query_planner::SEARCH_QUERY_PREFIX),
            "embed_batch input must carry the fixed re-embedding prefix"
        );
    }

    // 一時ディレクトリ（`TempDir` / `tempdir()`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::tempdir;
}
