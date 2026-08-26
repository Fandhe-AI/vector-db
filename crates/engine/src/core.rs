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
//! 参照）。可視性判定・provider 結果の二重防御（構築時フィルタ＋
//! `provider_result_is_valid`）はキャッシュ経路・非キャッシュ経路のいずれでも
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

    /// 新規構築したスナップショットを挿入する。単体で総量上限を超える場合はキャッシュ
    /// せず、呼び出し元は戻り値の `Arc` をその場限りで使う（型ドキュメント参照）。
    /// ロック毒化時は挿入せず `Arc` をそのまま返す（キャッシュ非搭載でも検索自体は続行
    /// できるよう fail-closed に「キャッシュを諦める」側へ倒す）。
    ///
    /// `storage` は (0) 挿入対象自身の世代整合チェックと (1) の世代不整合エントリの
    /// 一括破棄で「現在の実世代」（[`Storage::current_generation`]）を判定するために
    /// 使う。以前は `snapshot.built_generation()`（= このスナップショット自身の構築
    /// 時点の世代）を現在世代の代用にしていたが、これは挿入対象のスナップショットが
    /// 並行書き込みで既に古くなっている場合、真に新しい（現在世代と一致する）既存
    /// エントリまで「不一致」として誤って全破棄してしまう不具合があった
    /// （Cursor Bugbot 指摘）。
    /// `storage.current_generation()` の読み取りに失敗した場合は世代整合を判定できない
    /// ため、(0)(1) いずれも実行せずキャッシュへの反映をスキップする（fail-closed:
    /// 「判定できないなら書き込まない」側へ倒す。stale なエントリは [`Self::lookup`]
    /// が個別に検出して破棄する）。
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
    ) -> Arc<PrefilterSnapshot> {
        let snapshot = Arc::new(snapshot);
        self.misses.fetch_add(1, Ordering::Relaxed);

        let own_bytes = snapshot.approx_heap_bytes();
        if own_bytes > MAX_PREFILTER_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超えるスナップショットは常駐させない（DoS 対策。
            // 型ドキュメント参照）。呼び出し元へはそのまま返し、1 回の検索限りで使う。
            return snapshot;
        }

        let Ok(mut guard) = self.state.write() else {
            return snapshot;
        };

        // (0) 挿入対象自身が現在世代と一致するか確認する（対象外スレッド指摘の修正）。
        // 世代が読み取れない、または挿入対象が既に古い場合はキャッシュへ反映せず
        // その場限りの `Arc` を返す。ここでリターンすることで、後続の「同一キー破棄」
        // ステップに到達させない。すなわち並行書き込みで自身が stale になった挿入が、
        // 別スレッドが直前に挿入した現在世代の有効エントリを上書き・削除する経路を断つ
        // （型ドキュメント参照）。ロック保持中に読むため、以降のこの関数内の判定と
        // 齟齬が生じない。
        let Ok(current_generation) = storage.current_generation() else {
            return snapshot;
        };
        if snapshot.built_generation() != current_generation {
            return snapshot;
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
                return snapshot;
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
        snapshot
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
    /// `operation_id` 必須化ガード（TASK-92・対象ビヘイビア: RECOVER-1）を制御する
    /// サーバー側構成。クエリ・セッション変数から到達できる経路を持たない（差し替えは
    /// [`Self::with_ledger_mode`] のみ。`crate::recovery::required_op_id` モジュール
    /// ドキュメント参照）。
    ledger_mode: LedgerMode,
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
        Ok(Self {
            storage,
            provider,
            prefilter_cache: PrefilterCache::new(),
            precision_policy: crate::precision::PrecisionPolicy::default(),
            ledger_mode: LedgerMode::default(),
        })
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
        Self {
            storage,
            provider,
            prefilter_cache: PrefilterCache::new(),
            precision_policy: crate::precision::PrecisionPolicy::default(),
            ledger_mode: LedgerMode::default(),
        }
    }

    /// [`PrefilterCache`] の現在の統計を返す（TASK-169。テスト・運用観測用）。
    /// テナント ID・行 ID 等の機微情報は含まない（[`PrefilterCacheStats`] 参照）。
    /// `VectorCore` trait には載せない固有メソッド（`core_api.snapshot` の対象外）。
    pub fn prefilter_cache_stats(&self) -> PrefilterCacheStats {
        self.prefilter_cache.stats()
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

    /// `operation_id` 必須化ガード（TASK-92・対象ビヘイビア: RECOVER-1）を制御する
    /// [`LedgerMode`] を差し替えたビルダーを返す（[`Self::with_precision_policy`] と
    /// 同型: 所有権を消費するビルダーメソッドとし、`&mut self` セッターは公開しない）。
    /// `SessionState`・SQL 構文からはこの値へ到達できない（`crate::recovery::
    /// required_op_id` モジュールドキュメント参照）。
    pub fn with_ledger_mode(mut self, mode: LedgerMode) -> Self {
        self.ledger_mode = mode;
        self
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
            crate::sql::allowlist::Statement::Select(_) => {
                // `SELECT` と判定済みのため、[`Self::execute_sql_in_session`] が
                // `SqlOutcome::SetSearchMode`／`SqlOutcome::CreateFunction` を返すことは
                // ない（同一 `sql` を `validate_sql` で 2 度構文解析するが、副作用の
                // ない決定的なパースのため安全側に倒した単純さを優先する）。
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_sql_in_session(ctx, &mut session, sql)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. } => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as Select"
                                .to_string(),
                        })
                    }
                }
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
    pub fn execute_sql_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_sql(sql, &self.storage)?;
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
                let bound = crate::sql::parser::bind_in_session(
                    &validated,
                    &schema,
                    session.search_mode(),
                    session.udfs(),
                )?;
                let result = crate::sql::exec::execute_statement(
                    &read_txn,
                    self.provider.as_ref(),
                    ctx,
                    &schema,
                    &bound,
                    &self.precision_policy,
                )?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
        }
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
        self.ledger_mode.require(operation_id)?;
        crate::tenant::insert_row_unchecked(&self.storage, table, ctx, id, row)
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
        self.ledger_mode.require(operation_id)?;
        crate::tenant::update_row_unchecked(&self.storage, table, ctx, id, row)
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
        self.ledger_mode.require(operation_id)?;
        crate::tenant::delete_row_unchecked(&self.storage, table, ctx, id)
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
        let snapshot = self
            .prefilter_cache
            .insert(&self.storage, table, ctx, snapshot);
        self.search_with_snapshot(table, ctx, query, k, snapshot)
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
        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx_a, stale_snapshot);
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

        // 同一 (table, ctx) キーへ複数回挿入する（同一スナップショットを使い回しても
        // `insert` 自身は世代を見ないため重複判定の再現に十分）。
        let snapshot_1 =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot 1");
        let snapshot_2 =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot 2");

        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx, snapshot_1);
        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx, snapshot_2);

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
            .insert(&core.storage, "docs", &ctx, fresh_snapshot);
        assert_eq!(core.prefilter_cache_stats().entries, 1);

        // 遅れて到着した G0 の stale スナップショットを同一キーへ挿入しても、
        // 既にキャッシュされている G1 の有効エントリが上書き・削除されないこと。
        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx, stale_snapshot);

        let cached = core
            .prefilter_cache
            .lookup(&core.storage, "docs", &ctx)
            .expect("同一キーの G1 エントリがキャッシュに残っていること");
        assert!(
            Arc::ptr_eq(&cached, &fresh_arc),
            "stale な挿入によって G1 の有効エントリが差し替えられてはならない"
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

    // 一時ディレクトリ（`TempDir` / `tempdir()`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::tempdir;
}
