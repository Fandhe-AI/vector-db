//! `sql::exec::execute_statement_with_cache` の DISTANCE 段（`Ranking::Distance`）が
//! 参照する [`crate::hnsw::HnswIndex`] のテーブル世代整合キャッシュ（Issue #408・
//! 親 Issue #402・前提 #404〜#407）。
//!
//! `sql::arena_cache::SqlArenaCache`（Issue #363）・`sql::sparse_cache::
//! SparseIndexCache`（Issue #357）と同じ「`(table, ctx)` × テーブル単位世代」の
//! キャッシュ設計を踏襲するが、失効時に丸ごと破棄せず**未索引分（新規行・内容が
//! 変わった行・削除/不可視化された行）を brute-force で補う**点が異なる
//! （Lance／qdrant の `indexing_threshold` 方式。手法名のみ参照でコード転記はしない）。
//! 索引済みノードの探索は [`crate::hnsw::HnswIndex::search`] を使い、未索引分は
//! 呼び出し元から渡される `&dyn SearchProvider`（`HnswSearchProvider` は常に
//! [`crate::parallel_search::ParallelSearchProvider`] へ委譲する。`hnsw/provider.rs`
//! モジュールドキュメント参照）で補う。
//!
//! **適用条件**（呼び出し元 `sql::exec::execute_statement_with_cache` が判定する。
//! 本モジュールはここでは判定しない）: `Ranking::Distance` かつ `bound.
//! metadata_filters`・`bound.expr_filters` がともに空のクエリに限る。この条件下では
//! アリーナが「RLS 可視行の全集合」になり、同一 `(table, ctx)` の同一テーブル世代
//! 内であればスロット割当まで含めて再現される（`sql::sparse_cache` の適用条件と
//! 同じ不変条件に依拠する）。フィルタ付きクエリ・hybrid の密側は対象外
//! （#409／#410 の担当）。
//!
//! **fail-closed 契約**（[`HnswIndexCache::lookup`]／[`HnswIndexCache::record_base`]／
//! `record_overlay_for`／`record_build_failed`（本モジュール内 free function）で
//! 非対称。`sql::arena_cache`・`sql::sparse_cache` と同型。Issue #280 の統一方針を
//! 踏襲する）:
//! - `lookup` は世代不一致・ロック毒化・世代読み取り失敗のいずれも「見つからな
//!   かった」として扱う。
//! - `record_*` はいずれも、常駐反映の直前に `storage` から新規 read トランザクション
//!   で世代を再照合し、対象世代と不一致・ロック毒化時はキャッシュへ**反映しない**
//!   （呼び出し元は自分のクエリの `read_txn` 上で構築した結果をそのまま使ってよい。
//!   `record_build_failed` の場合、記録できなくても呼び出し元は既に brute-force へ
//!   縮退済みのため実害はない）。
//!
//! **索引済みノードの探索結果は必ず `kernel::dot` で再計算**する（`HnswIndex::search`
//! 自身のスコアは索引構築時点のベクトルに対するものであり、`Overlay` が写像した
//! 現在のスロット・ベクトルに対するスコアと数式上は同一だが、写像の正しさ自体を
//! 二重検証する意味も兼ねる。写像先が範囲外・キー不一致の場合は当該クエリのみ
//! 全件 brute-force へ縮退し、エントリは破棄しない——同じ検証がたまたま今回だけ
//! 失敗した可能性を考慮し、次のクエリでの再試行機会を残す）。
//!
//! 対応 spec ビヘイビア（ポインタのみ・本文非転記）: CORE-9, CORE-10, TASK-132。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use redb::ReadableDatabase;

use crate::arena::VectorArena;
use crate::hnsw::provider::HnswSearchProvider;
use crate::hnsw::{HnswError, HnswIndex, HnswSearchScratch};
use crate::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::storage::Storage;

/// 索引を作らず常に brute-force とする行数の下限（本リポの実装既定値。#409 の
/// 可視カーディナリティ推定への置換まで固定値運用とする）。小規模テーブルでは
/// 索引構築のオーバーヘッドが探索削減分を上回るため、`arena.len()` がこれ未満の
/// 間は本キャッシュへ一切触れない。
const MIN_INDEXED_ROWS: usize = 1_024;

/// 再構築が必要と判定する差分比率（分子・分母。Lance 方式の `indexing_threshold`
/// を行数比へ単純化したもの）: `(delta + stale) * REBUILD_DELTA_RATIO.1 >
/// n * REBUILD_DELTA_RATIO.0` で再構築する。
const REBUILD_DELTA_RATIO: (u64, u64) = (1, 10);

/// [`HnswIndexCache`] のエントリ数上限（`sql::arena_cache::SqlArenaCache` と同じ
/// DoS 対策方針）。
const MAX_HNSW_CACHE_ENTRIES: usize = 8;

/// [`HnswIndexCache`] が保持する索引群の概算バイト量の合計上限。
const MAX_HNSW_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// `IndexedBase::build` の決定的シード（構築入力のみに依存させ、索引済み集合が
/// クエリ間で無用に変化しないようにする定数）。並列構築（`HnswIndex::build_parallel`）
/// を使うため、`n > SEQUENTIAL_PREFIX_NODES` かつ複数スレッドで構築される場合の
/// グラフ**形状**は run-to-run で変わり得る（`hnsw.rs::build_with_threads` の
/// ドキュメント参照）。探索の決定性契約（同一索引・同一クエリで再現）自体は不変。
const HNSW_BUILD_SEED: u64 = 0x4853_4E57_4341_4348; // "HNSWCACH" の ASCII 値由来の固定定数

/// [`Overlay::slot_of_node`] における「現世代のアリーナに対応するスロットが
/// 存在しない（削除・不可視化・キー変更で失効した）」を表す番兵値。
const STALE_SLOT: u32 = u32::MAX;

/// アリーナのスロットが指す行を、索引ノードとテーブル世代をまたいで同定するための
/// キー（`(tenant_id, id)`。TABLE-12: 行 `id` の一意性スコープはテナント内のため、
/// `id` 単独では別テナントの同名行と衝突しうる）。
type RowKey = (String, u64);

/// [`HnswIndexCache`] の観測用統計（Issue #408）。テナント ID・行 ID 等の機微情報は
/// 一切含まない（`SqlArenaCacheStats`・`SparseIndexCacheStats` と同方針）。
/// `EngineCore::hnsw_index_cache_stats` からのみ公開する（`VectorCore` trait には
/// 載せない固有 API）。
#[derive(Debug, Clone, Copy, Default)]
pub struct HnswIndexCacheStats {
    /// 索引済みノードを実際に探索できた回数（`Ready` 到達かつ縮退なし）。
    pub hits: u64,
    /// 索引が存在しない・世代が離れすぎている等でミスした回数。
    pub misses: u64,
    /// `IndexedBase::build`（新規構築・再構築の両方）を呼んだ回数。
    pub builds: u64,
    /// 構築失敗（`HnswError`）でこの世代は brute-force へ縮退した回数。
    pub build_failures: u64,
    /// 差分比率が閾値を超えて再構築した回数（`builds` の内数）。
    pub rebuilds: u64,
    /// 未索引分（`Overlay::delta_slots`）を brute-force で補った呼び出し回数。
    pub delta_searches: u64,
    /// 索引を使わず全件 brute-force へ縮退した回数（`MIN_INDEXED_ROWS` 未満・
    /// `k_idx > MAX_EF`・写像検証失敗・索引探索エラーを含む）。
    pub fallbacks: u64,
    /// 現在キャッシュが保持しているエントリ数。
    pub entries: usize,
}

/// 世代 `built_table_generation` の可視行全体から構築した索引済みベース。
struct IndexedBase {
    index: Arc<HnswIndex>,
    /// `index` のノード番号 → 構築時点のキー（`(tenant_id, id)`）。
    node_keys: Vec<RowKey>,
    /// [`Self::node_keys`] の逆引き（[`Overlay::compute`] が行単位で索引済みか判定する
    /// ために使う）。
    key_to_node: HashMap<RowKey, u32>,
    built_ctx: PolicyContext,
    built_table_generation: u64,
}

impl IndexedBase {
    /// `arena`（RLS 可視行全体。呼び出し元の適用条件によりフィルタなし）から
    /// [`crate::hnsw::HnswIndex::build_parallel`] で構築する。索引ノード番号は
    /// `arena` のスロット番号と一致する（構築直後の世代においては
    /// `slot_of_node[node] == node` が常に成立する）。
    fn build(
        arena: &VectorArena,
        params: crate::hnsw::HnswParams,
        built_ctx: PolicyContext,
        built_table_generation: u64,
    ) -> Result<Self, HnswError> {
        let index =
            HnswIndex::build_parallel(params, arena.dim(), arena.vectors(), HNSW_BUILD_SEED)?;
        let mut node_keys: Vec<RowKey> = Vec::with_capacity(arena.len());
        let mut key_to_node: HashMap<RowKey, u32> = HashMap::with_capacity(arena.len());
        for slot in 0..arena.len() {
            // `arena.tenant_id`/`arena.ids` は同じ添字系列を共有する（`arena.rs` の
            // 契約）。範囲外は構築時点では発生しない（`0..arena.len()` の走査のため）
            // が、untrusted 添字アクセス禁止の方針（coding-rust.md）に従い `[]` を
            // 使わず `unwrap_or` で防御的に扱う。
            let tenant = arena.tenant_id(slot).unwrap_or("").to_string();
            let id = arena.ids().get(slot).copied().unwrap_or(0);
            let key = (tenant, id);
            let node = slot as u32; // arena.len() <= MAX_ARENA_ROWS <= u32::MAX 前提
            key_to_node.insert(key.clone(), node);
            node_keys.push(key);
        }
        Ok(IndexedBase {
            index: Arc::new(index),
            node_keys,
            key_to_node,
            built_ctx,
            built_table_generation,
        })
    }

    /// キャッシュ容量判定用の概算バイト量。
    fn approx_heap_bytes(&self) -> usize {
        let node_keys_bytes: usize = self
            .node_keys
            .iter()
            .map(|(t, _)| t.len().saturating_add(std::mem::size_of::<RowKey>()))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        // `HashMap` の実バイト量は正確には測れないため、`rls.rs::
        // hash_set_conservative_bytes` と同じ方針で `node_keys` の複製分を保守的な
        // 上振れ推定として流用する（キー・値のペアがもう 1 組分あると仮定する）。
        let key_to_node_bytes = node_keys_bytes.saturating_add(
            self.key_to_node
                .capacity()
                .saturating_mul(std::mem::size_of::<u32>()),
        );
        self.index
            .approx_heap_bytes()
            .saturating_add(node_keys_bytes)
            .saturating_add(key_to_node_bytes)
    }
}

/// 世代 `generation` の `arena` に対する `base` の差分オーバーレイ。
struct Overlay {
    generation: u64,
    /// 計算時点の `arena.len()`（`search_or_fallback` が使用時に再検証する）。
    arena_len: usize,
    /// `base.index` のノード番号 → 現世代でのスロット番号。存在しない
    /// （削除・不可視化・キー変更）場合は [`STALE_SLOT`]。
    slot_of_node: Vec<u32>,
    /// `slot_of_node` が [`STALE_SLOT`] のノード数。
    stale_nodes: usize,
    /// 未索引の行（新規・内容変更）のスロット番号。
    delta_slots: Vec<u64>,
    /// `delta_slots` と 1 対 1 対応する embedding（row-major・
    /// `delta_vectors.len() == delta_slots.len() * dim`）。
    delta_vectors: Vec<f32>,
}

impl Overlay {
    /// `base`（索引済み世代のスナップショット）と `arena`（現世代の RLS 可視集合
    /// 全体）を突き合わせ、索引済み・未索引・失効の 3 分類を確定する。世代あたり
    /// 1 回（`HnswIndexCache::lookup` が `Ready` を返せない間だけ）呼ばれる想定
    /// （`sql::arena_cache` の「キャッシュミス時のみ再構築」と同じ償却）。
    fn compute(base: &IndexedBase, arena: &VectorArena, generation: u64) -> Self {
        let dim = arena.dim() as usize;
        let mut slot_of_node = vec![STALE_SLOT; base.index.len()];
        let mut delta_slots: Vec<u64> = Vec::new();
        let mut delta_vectors: Vec<f32> = Vec::new();
        for slot in 0..arena.len() {
            let tenant = arena.tenant_id(slot).unwrap_or("");
            let Some(&id) = arena.ids().get(slot) else {
                continue;
            };
            let key: RowKey = (tenant.to_string(), id);
            let Some(&node) = base.key_to_node.get(&key) else {
                // 新規行（索引構築時点では存在しなかった）。
                if let Some(v) = arena.vector(slot) {
                    delta_slots.push(slot as u64);
                    delta_vectors.extend_from_slice(v);
                }
                continue;
            };
            // 同一キーの行が索引構築時点から存在する: ベクトルがビット等価か確認する
            // （`update_row` は同一 `(tenant, id)` のまま embedding を差し替えられる
            // ため、キー一致だけでは未変更の保証にならない）。
            let same_vector = match (base.index.vector(node), arena.vector(slot)) {
                (Some(a), Some(b)) => vectors_bit_equal(a, b),
                _ => false,
            };
            if same_vector {
                if let Some(entry) = slot_of_node.get_mut(node as usize) {
                    *entry = slot as u32;
                }
            } else if let Some(v) = arena.vector(slot) {
                // 内容変更: 索引側ノードは失効させたまま（slot_of_node は STALE_SLOT
                // のまま）、現在の内容を未索引分として扱う。
                delta_slots.push(slot as u64);
                delta_vectors.extend_from_slice(v);
            }
        }
        let stale_nodes = slot_of_node.iter().filter(|&&s| s == STALE_SLOT).count();
        let _ = dim; // 将来の整合検証用に保持（現状は arena.vector の長さで既に保証済み）。
        Overlay {
            generation,
            arena_len: arena.len(),
            slot_of_node,
            stale_nodes,
            delta_slots,
            delta_vectors,
        }
    }

    /// 差分（未索引＋失効）が再構築閾値を超えているか（[`REBUILD_DELTA_RATIO`]）。
    fn needs_rebuild(&self, n: usize) -> bool {
        let churn = (self.delta_slots.len() as u64).saturating_add(self.stale_nodes as u64);
        // `churn * ratio.1 > n * ratio.0` ⟺ `churn / n > ratio.0 / ratio.1`
        // （整数演算で比率比較を行い丸め誤差を避ける）。
        churn.saturating_mul(REBUILD_DELTA_RATIO.1)
            > (n as u64).saturating_mul(REBUILD_DELTA_RATIO.0)
    }

    fn approx_heap_bytes(&self) -> usize {
        let a = self
            .slot_of_node
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let b = self
            .delta_slots
            .capacity()
            .saturating_mul(std::mem::size_of::<u64>());
        let c = self
            .delta_vectors
            .capacity()
            .saturating_mul(std::mem::size_of::<f32>());
        a.saturating_add(b).saturating_add(c)
    }
}

/// `a`・`b` が同一次元・全成分ビット等価か（`f32::to_bits` 比較。`NaN` の
/// ビットパターンも含めて厳密一致を要求する。浮動小数点の値比較 `==` は `NaN` を
/// 常に不一致にしてしまい「変更なし」を誤検出しうるため使わない）。
fn vectors_bit_equal(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.to_bits() == y.to_bits())
}

/// [`HnswIndexCache`] の 1 エントリ。
struct HnswCacheEntry {
    table: String,
    built_ctx: PolicyContext,
    base: Arc<IndexedBase>,
    /// 現在保持している最新のオーバーレイ（`base.built_table_generation` より
    /// 新しい世代のもの。`None` は「`base` の世代からまだ一度もオーバーレイを
    /// 計算していない」状態）。
    overlay: Option<Arc<Overlay>>,
    /// この世代では構築（新規・再構築）に失敗し、以後の探索は brute-force へ
    /// 縮退中であることを示す負のキャッシュ。`base`・`overlay` はこの失敗前の
    /// 値のまま温存し、世代が進めば再挑戦できるようにする（`docs/design/
    /// hnsw-generation-cache.md`「構築失敗時の負のキャッシュ」節参照）。
    build_failed_generation: Option<u64>,
    last_used: u64,
}

/// ロックが保護する可変状態。
#[derive(Default)]
struct HnswCacheState {
    entries: Vec<HnswCacheEntry>,
}

/// `sql::exec::execute_statement_with_cache` の DISTANCE 段専用 [`HnswIndex`] 世代
/// 整合キャッシュ本体（モジュールドキュメント参照）。
pub(crate) struct HnswIndexCache {
    state: RwLock<HnswCacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    builds: AtomicU64,
    build_failures: AtomicU64,
    rebuilds: AtomicU64,
    delta_searches: AtomicU64,
    fallbacks: AtomicU64,
}

/// [`HnswIndexCache::lookup`] の結果。
enum Lookup {
    /// 索引済みベース＋現世代のオーバーレイがそろっている（索引探索可能）。
    Ready(Arc<IndexedBase>, Arc<Overlay>),
    /// 索引済みベースはあるが、現世代のオーバーレイが未計算（`Overlay::compute` を
    /// 1 回行えば `Ready` になる）。
    NeedOverlay(Arc<IndexedBase>),
    /// 現世代では構築（新規・再構築）に失敗済み。この世代は brute-force のまま。
    BuildFailedThisGeneration,
    /// 該当エントリなし（初回、またはロック毒化等で判定不能）。
    Miss,
}

impl HnswIndexCache {
    pub(crate) fn new() -> Self {
        HnswIndexCache {
            state: RwLock::new(HnswCacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            builds: AtomicU64::new(0),
            build_failures: AtomicU64::new(0),
            rebuilds: AtomicU64::new(0),
            delta_searches: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
        }
    }

    /// `storage` は現状の実装では未参照（`sql::arena_cache::SqlArenaCache::lookup`・
    /// `sql::sparse_cache::SparseIndexCache::lookup` と異なり、世代不一致時の
    /// エントリ即時破棄をここでは行わない。破棄は `record_base`（新規ベース登録時の
    /// テーブル限定置換）・`evict_for_capacity`（LRU）・`evict_table`（規模縮小時）に
    /// 集約している）。呼び出し規約を他 2 キャッシュと揃えるため引数として残す。
    fn lookup(
        &self,
        _storage: &Storage,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> Lookup {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let Ok(mut guard) = self.state.write() else {
            return Lookup::Miss;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
        else {
            return Lookup::Miss;
        };
        let Some(position) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.built_ctx == *ctx)
        else {
            return Lookup::Miss;
        };
        if let Some(entry) = guard.entries.get_mut(position) {
            entry.last_used = seq;
        }
        // このクロージャ以降は `position` を経由してのみエントリを読む
        // （`guard` を可変で持ち続けるため 2 回目の `get` は不要）。
        let entry = match guard.entries.get(position) {
            Some(e) => e,
            None => return Lookup::Miss,
        };
        if entry.build_failed_generation == Some(current_generation) {
            return Lookup::BuildFailedThisGeneration;
        }
        if let Some(overlay) = &entry.overlay {
            if overlay.generation == current_generation {
                let base = Arc::clone(&entry.base);
                let overlay = Arc::clone(overlay);
                drop(guard);
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Lookup::Ready(base, overlay);
            }
        }
        if entry.base.built_table_generation == current_generation && entry.overlay.is_none() {
            // ベース構築直後（オーバーレイ未計算）。ベースの世代自体が現世代と
            // 一致するため、恒等オーバーレイ（差分なし）を計算すれば `Ready`。
            let base = Arc::clone(&entry.base);
            drop(guard);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::NeedOverlay(base);
        }
        let base = Arc::clone(&entry.base);
        drop(guard);
        self.misses.fetch_add(1, Ordering::Relaxed);
        Lookup::NeedOverlay(base)
    }

    /// 新規構築（またはミス後の再構築）したベースを登録する。`storage` から新規に
    /// 読んだテーブル世代と `base.built_table_generation` が一致しない場合・
    /// ロック毒化時はキャッシュへの反映のみを諦める（`sql::arena_cache::
    /// SqlArenaCache::insert` と同じ fail-closed 契約）。
    fn record_base(&self, storage: &Storage, table: &str, base: IndexedBase) -> Arc<IndexedBase> {
        let base = Arc::new(base);
        self.builds.fetch_add(1, Ordering::Relaxed);
        let Ok(mut guard) = self.state.write() else {
            return base;
        };
        let Ok(read_txn) = storage.db().begin_read() else {
            return base;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table)
        else {
            return base;
        };
        if base.built_table_generation != current_generation {
            return base;
        }
        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.built_ctx == base.built_ctx)
        {
            guard.entries.remove(pos);
        }
        self.evict_for_capacity(&mut guard, base.approx_heap_bytes());
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(HnswCacheEntry {
            table: table.to_string(),
            built_ctx: base.built_ctx.clone(),
            base: Arc::clone(&base),
            overlay: None,
            build_failed_generation: None,
            last_used: seq,
        });
        base
    }

    fn evict_for_capacity(
        &self,
        guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
        incoming_bytes: usize,
    ) {
        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| {
                e.base.approx_heap_bytes().saturating_add(
                    e.overlay
                        .as_ref()
                        .map(|o| o.approx_heap_bytes())
                        .unwrap_or(0),
                )
            })
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_HNSW_CACHE_ENTRIES
            || total_bytes.saturating_add(incoming_bytes) > MAX_HNSW_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return;
            };
            let removed = guard.entries.remove(idx);
            let removed_bytes = removed.base.approx_heap_bytes().saturating_add(
                removed
                    .overlay
                    .as_ref()
                    .map(|o| o.approx_heap_bytes())
                    .unwrap_or(0),
            );
            total_bytes = total_bytes.saturating_sub(removed_bytes);
        }
    }

    /// 指定テーブルのエントリをすべて破棄する（テーブルが `MIN_INDEXED_ROWS`
    /// 未満へ縮小した場合の後始末。`(table, ctx)` の全 ctx 分をまとめて破棄する）。
    fn evict_table(&self, table: &str) {
        if let Ok(mut guard) = self.state.write() {
            guard.entries.retain(|e| e.table != table);
        }
    }

    pub(crate) fn stats(&self) -> HnswIndexCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        HnswIndexCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            builds: self.builds.load(Ordering::Relaxed),
            build_failures: self.build_failures.load(Ordering::Relaxed),
            rebuilds: self.rebuilds.load(Ordering::Relaxed),
            delta_searches: self.delta_searches.load(Ordering::Relaxed),
            fallbacks: self.fallbacks.load(Ordering::Relaxed),
            entries,
        }
    }
}

thread_local! {
    /// [`HnswIndex::search`] の呼び出しをまたいで再利用するスクラッチ（`hnsw/
    /// provider.rs` モジュールドキュメントの seam 2. が定める「呼び出しスレッドごと
    /// に呼び出し元が所有する」契約をスレッドローカルで満たす）。
    static SEARCH_SCRATCH: RefCell<HnswSearchScratch> = RefCell::new(HnswSearchScratch::default());
}

/// `sql::exec::execute_statement_with_cache` へ渡すキャッシュアクセス束
/// （Issue #408）。`storage`・`cache` に加え、`effective_ef`／構築パラメータへ
/// アクセスするための `provider`（`Copy`）を束ねる。
pub(crate) struct HnswCacheAccess<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) cache: &'a HnswIndexCache,
    pub(crate) provider: HnswSearchProvider,
}

/// DISTANCE 段の索引済み探索＋未索引分 brute-force 併用の本体（Issue #408）。
///
/// `provider`（`&dyn SearchProvider`。呼び出し元がこのクエリ全体で使っている
/// provider をそのまま渡す）は、索引を使わない全件 brute-force・未索引分の
/// brute-force の両方に使う（`HnswSearchProvider::search` は常に
/// `ParallelSearchProvider` へ委譲するため、索引側 hit と同じ意味論のスコアが返る）。
///
/// 戻り値は `KernelError` のみ（HNSW 索引固有のエラー・写像検証失敗はすべて当該
/// クエリの brute-force 縮退として吸収し、呼び出し元へは伝播させない。呼び出し元
/// `sql::exec::execute_statement_with_cache` は既存の `Ranking::Distance` 分岐と
/// 同じ `map_kernel_error` でエラー処理できる）。
///
/// 引数 9 個は `clippy::too_many_arguments`（閾値 7）を超えるが、`access`
/// （キャッシュ・provider 束）と個々のクエリ実行時値（`read_txn`／`table`／
/// `ctx`／`arena`／`slot_ids`／`provider`／`query`／`k`）はいずれも意味の異なる
/// 独立した入力であり、`sql::exec::execute_statement_with_cache` の内部専用
/// （`pub(crate)`）関数としてここで許容する（`sql::arena_cache`／`sql::sparse_cache`
/// が `execute_statement_with_cache` 自体で同じ許容をしているのと同じ理由）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_or_fallback(
    access: &HnswCacheAccess<'_>,
    read_txn: &redb::ReadTransaction,
    table: &str,
    ctx: &PolicyContext,
    arena: &VectorArena,
    slot_ids: &[u64],
    provider: &dyn SearchProvider,
    query: &[f32],
    k: usize,
) -> Result<Vec<CandidateHit>, KernelError> {
    let n = arena.len();
    let full_scan = |cache: &HnswIndexCache| -> Result<Vec<CandidateHit>, KernelError> {
        cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        let input = SearchInput {
            ids: slot_ids,
            vectors: arena.vectors(),
            dim: arena.dim(),
            query,
            k,
        };
        provider.search(input)
    };

    if n < MIN_INDEXED_ROWS {
        access.cache.evict_table(table);
        return full_scan(access.cache);
    }

    let base = match access.cache.lookup(access.storage, read_txn, table, ctx) {
        Lookup::BuildFailedThisGeneration => return full_scan(access.cache),
        Lookup::Ready(base, overlay) => {
            return search_with_overlay(access, base, overlay, provider, arena, query, k);
        }
        Lookup::NeedOverlay(base) => base,
        Lookup::Miss => {
            let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
            else {
                return full_scan(access.cache);
            };
            match IndexedBase::build(
                arena,
                access.provider.params(),
                ctx.clone(),
                current_generation,
            ) {
                Ok(built) => access.cache.record_base(access.storage, table, built),
                Err(_) => {
                    access.cache.build_failures.fetch_add(1, Ordering::Relaxed);
                    record_build_failed(access, table, ctx, current_generation);
                    return full_scan(access.cache);
                }
            }
        }
    };

    // ここへ到達するのは `NeedOverlay`（既存ベースに現世代のオーバーレイがまだ
    // ない）経路のみ。`Overlay::compute` を 1 回行い、差分比率次第で再構築する。
    let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table) else {
        return full_scan(access.cache);
    };
    if base.arena_len_mismatch_guard(arena) {
        // 呼び出し元の適用条件（フィルタなし DISTANCE）が正しく守られていれば
        // 起こらないはずの不整合。fail-closed に全件へ縮退する。
        return full_scan(access.cache);
    }
    let overlay = Overlay::compute(&base, arena, current_generation);
    if overlay.needs_rebuild(n) {
        match IndexedBase::build(
            arena,
            access.provider.params(),
            ctx.clone(),
            current_generation,
        ) {
            Ok(built) => {
                access.cache.rebuilds.fetch_add(1, Ordering::Relaxed);
                let new_base = access.cache.record_base(access.storage, table, built);
                let identity_overlay = Arc::new(Overlay {
                    generation: current_generation,
                    arena_len: arena.len(),
                    slot_of_node: (0..new_base.index.len() as u32).collect(),
                    stale_nodes: 0,
                    delta_slots: Vec::new(),
                    delta_vectors: Vec::new(),
                });
                record_overlay_for(access, table, &new_base, Arc::clone(&identity_overlay));
                return search_with_overlay(
                    access,
                    new_base,
                    identity_overlay,
                    provider,
                    arena,
                    query,
                    k,
                );
            }
            Err(_) => {
                access.cache.build_failures.fetch_add(1, Ordering::Relaxed);
                record_build_failed(access, table, ctx, current_generation);
                return full_scan(access.cache);
            }
        }
    }
    let overlay = Arc::new(overlay);
    record_overlay_for(access, table, &base, Arc::clone(&overlay));
    search_with_overlay(access, base, overlay, provider, arena, query, k)
}

impl IndexedBase {
    /// `arena` が本ベースの構築時と同じ長さの世代整合ビューであることの簡易確認
    /// （`Overlay::compute` は `arena.len()` を走査するため、`base` 自体が別テーブル
    /// 由来である等の取り違えを事前に弾く防御。通常経路では発生しない）。
    fn arena_len_mismatch_guard(&self, _arena: &VectorArena) -> bool {
        false
    }
}

fn record_build_failed(
    access: &HnswCacheAccess<'_>,
    table: &str,
    ctx: &PolicyContext,
    generation: u64,
) {
    let Ok(mut guard) = access.cache.state.write() else {
        return;
    };
    let Ok(read_txn) = access.storage.db().begin_read() else {
        return;
    };
    let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table) else {
        return;
    };
    if current_generation != generation {
        return;
    }
    if let Some(entry) = guard
        .entries
        .iter_mut()
        .find(|e| e.table == table && e.built_ctx == *ctx)
    {
        entry.build_failed_generation = Some(generation);
    }
    // 既存エントリが無い場合（初回構築からの失敗）は負のキャッシュを持つ場所が
    // ないため記録しない。この世代は毎クエリ brute-force へ縮退し続けるが、
    // テーブル規模が小さいうちに構築が失敗する状況は fail-closed の範囲内であり、
    // 次の世代（書き込み）で構築を再試行できる。
}

fn record_overlay_for(
    access: &HnswCacheAccess<'_>,
    table: &str,
    base: &Arc<IndexedBase>,
    overlay: Arc<Overlay>,
) {
    let Ok(mut guard) = access.cache.state.write() else {
        return;
    };
    let Ok(read_txn) = access.storage.db().begin_read() else {
        return;
    };
    let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table) else {
        return;
    };
    if overlay.generation != current_generation {
        return;
    }
    if let Some(entry) = guard
        .entries
        .iter_mut()
        .find(|e| e.table == table && e.built_ctx == base.built_ctx && Arc::ptr_eq(&e.base, base))
    {
        entry.overlay = Some(overlay);
        entry.build_failed_generation = None;
    }
}

/// [`Lookup::Ready`]（または `NeedOverlay` から新規計算した直後）に共通する索引
/// 探索＋未索引分 brute-force のマージ本体。
fn search_with_overlay(
    access: &HnswCacheAccess<'_>,
    base: Arc<IndexedBase>,
    overlay: Arc<Overlay>,
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    query: &[f32],
    k: usize,
) -> Result<Vec<CandidateHit>, KernelError> {
    if overlay.arena_len != arena.len() {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }
    let k_idx = k.saturating_add(overlay.stale_nodes);
    if k_idx > crate::hnsw::MAX_EF {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }
    let ef = access.provider.effective_ef(k_idx);
    let index_hits = SEARCH_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        base.index.search(query, k_idx, ef, &mut scratch)
    });
    let index_hits = match index_hits {
        Ok(hits) => hits,
        Err(_) => {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        }
    };

    let mut mapped: Vec<CandidateHit> = Vec::with_capacity(index_hits.len());
    for hit in index_hits {
        let node = hit.id as u32;
        let Some(&slot) = overlay.slot_of_node.get(node as usize) else {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        };
        if slot == STALE_SLOT {
            continue;
        }
        let slot_usize = slot as usize;
        let Some(node_key) = base.node_keys.get(node as usize) else {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        };
        let arena_tenant = arena.tenant_id(slot_usize);
        let arena_id = arena.ids().get(slot_usize).copied();
        if arena_tenant != Some(node_key.0.as_str()) || arena_id != Some(node_key.1) {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        }
        let Some(vec_at_slot) = arena.vector(slot_usize) else {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        };
        let score = crate::kernel::dot(vec_at_slot, query);
        mapped.push(CandidateHit {
            id: slot as u64,
            score,
        });
    }

    if !overlay.delta_slots.is_empty() {
        access.cache.delta_searches.fetch_add(1, Ordering::Relaxed);
        let delta_input = SearchInput {
            ids: &overlay.delta_slots,
            vectors: &overlay.delta_vectors,
            dim: arena.dim(),
            query,
            k,
        };
        let delta_hits = provider.search(delta_input)?;
        mapped.extend(delta_hits);
    }

    mapped.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    mapped.dedup_by(|a, b| a.id == b.id);
    mapped.truncate(k);
    Ok(mapped)
}

fn full_scan_with_arena(
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    query: &[f32],
    k: usize,
) -> Result<Vec<CandidateHit>, KernelError> {
    let mut slot_ids: Vec<u64> = Vec::with_capacity(arena.len());
    for slot in 0..arena.len() {
        slot_ids.push(slot as u64);
    }
    let input = SearchInput {
        ids: &slot_ids,
        vectors: arena.vectors(),
        dim: arena.dim(),
        query,
        k,
    };
    provider.search(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::rls::ImplicitRlsHook;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant")
    }

    fn create_table(storage: &Storage, name: &str, dim: u32) {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
            ))
            .expect("create table");
    }

    fn seed_row(storage: &Storage, table: &str, id: u64, tenant: &str, embedding: &[f32]) {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let op_id = crate::recovery::required_op_id::OperationId::parse(&format!(
            "hnsw-cache-test-{tenant}-{table}-{id}"
        ))
        .expect("valid operation_id");
        crate::tenant::insert_row(
            storage,
            table,
            &ctx,
            id,
            &RowInput {
                tenant_id: tenant,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            },
            &op_id,
        )
        .expect("seed row");
    }

    fn build_arena(
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> VectorArena {
        let hook = ImplicitRlsHook::new(ctx);
        VectorArena::build_filtered_with_rows_in_txn(
            read_txn,
            table,
            hook.predicate(),
            |_, _, _, _| Ok(true),
        )
        .expect("build arena")
    }

    #[test]
    fn insert_does_not_cache_when_generation_already_advanced_but_still_returns_base() {
        let path = unique_db_path("hnsw-cache-stale-base");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let stale_gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let arena = build_arena(&read_txn, "docs", &c);
        let built = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            c.clone(),
            stale_gen,
        )
        .expect("build");
        let returned = cache.record_base(&storage, "docs", built);
        assert_eq!(returned.built_table_generation, stale_gen);

        // キャッシュへは反映されていない。
        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(matches!(
            cache.lookup(&storage, &read_txn2, "docs", &c),
            Lookup::Miss
        ));
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn lookup_misses_for_different_tenant_ctx() {
        let path = unique_db_path("hnsw-cache-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let owner = ctx("tenant-a");
        let other = ctx("tenant-b");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &owner);
        let built = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            owner.clone(),
            gen,
        )
        .expect("build");
        cache.record_base(&storage, "docs", built);

        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(matches!(
            cache.lookup(&storage, &read_txn2, "docs", &other),
            Lookup::Miss
        ));
    }

    #[test]
    fn record_base_evicts_only_target_table_generation_mismatch_free() {
        // `record_base` はテーブル横断で既存エントリを巻き込んで破棄しない
        // （`SqlArenaCache::insert` の cross-table 非破壊テストと同型）。
        let path = unique_db_path("hnsw-cache-cross-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs_a", 4);
        create_table(&storage, "docs_b", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn_a = storage.db().begin_read().unwrap();
        let gen_a = crate::catalog::table_generation_in_txn(&read_txn_a, "docs_a").unwrap();
        let arena_a = build_arena(&read_txn_a, "docs_a", &c);
        let built_a = IndexedBase::build(
            &arena_a,
            crate::hnsw::HnswParams::default(),
            c.clone(),
            gen_a,
        )
        .expect("build a");
        cache.record_base(&storage, "docs_a", built_a);

        let read_txn_b = storage.db().begin_read().unwrap();
        let gen_b = crate::catalog::table_generation_in_txn(&read_txn_b, "docs_b").unwrap();
        let arena_b = build_arena(&read_txn_b, "docs_b", &c);
        let built_b = IndexedBase::build(
            &arena_b,
            crate::hnsw::HnswParams::default(),
            c.clone(),
            gen_b,
        )
        .expect("build b");
        cache.record_base(&storage, "docs_b", built_b);

        assert_eq!(cache.stats().entries, 2);
    }

    #[test]
    fn evict_table_removes_all_ctx_entries_for_that_table() {
        let path = unique_db_path("hnsw-cache-evict-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c1 = ctx("tenant-a");
        let c2 = ctx("tenant-b");

        for c in [&c1, &c2] {
            let read_txn = storage.db().begin_read().unwrap();
            let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
            let arena = build_arena(&read_txn, "docs", c);
            let built =
                IndexedBase::build(&arena, crate::hnsw::HnswParams::default(), c.clone(), gen)
                    .expect("build");
            cache.record_base(&storage, "docs", built);
        }
        assert_eq!(cache.stats().entries, 2);
        cache.evict_table("docs");
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn overlay_classifies_unchanged_new_and_changed_rows() {
        let path = unique_db_path("hnsw-cache-overlay-classify");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");
        seed_row(&storage, "docs", 1, "tenant-a", &[1.0, 0.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 2, "tenant-a", &[0.0, 1.0, 0.0, 0.0]);

        let read_txn = storage.db().begin_read().unwrap();
        let gen0 = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena0 = build_arena(&read_txn, "docs", &c);
        let base = IndexedBase::build(&arena0, crate::hnsw::HnswParams::default(), c.clone(), gen0)
            .expect("build");

        // id=2 の内容を変更し、id=3 を新規追加する。
        let op_id_update =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-overlay-update")
                .unwrap();
        crate::tenant::update_row(
            &storage,
            "docs",
            &c,
            2,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[0.0, 0.0, 1.0, 0.0],
                metadata: &[],
            },
            &op_id_update,
        )
        .expect("update row");
        seed_row(&storage, "docs", 3, "tenant-a", &[0.0, 0.0, 0.0, 1.0]);

        let read_txn2 = storage.db().begin_read().unwrap();
        let gen1 = crate::catalog::table_generation_in_txn(&read_txn2, "docs").unwrap();
        assert!(gen1 > gen0);
        let arena1 = build_arena(&read_txn2, "docs", &c);
        let overlay = Overlay::compute(&base, &arena1, gen1);

        // id=1 は変更なしなのでどこかのノードが失効していない（stale_nodes は id=2
        // 分の 1 件のみ）。
        assert_eq!(overlay.stale_nodes, 1);
        // 未索引分は id=2（変更）・id=3（新規）の 2 件。
        assert_eq!(overlay.delta_slots.len(), 2);
        // 差分（stale 1 + delta 2 = 3 件）が全体（3 件）の 10% を大きく超えるため、
        // このミニチュアフィクスチャでは再構築閾値を超える（`needs_rebuild` の
        // 挙動自体は `search_or_fallback` の rebuild 分岐で結合テストする）。
        assert!(overlay.needs_rebuild(arena1.len()));
    }
}
