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
//! 本モジュールはここでは判定しない）: `Ranking::Distance` のクエリに限る。
//! `bound.metadata_filters`・`bound.expr_filters` がともに空の場合はアリーナが
//! 「RLS 可視行の全集合」になり、同一 `(table, ctx)` の同一テーブル世代内であれば
//! スロット割当まで含めて再現される（`sql::sparse_cache` の適用条件と同じ不変条件
//! に依拠する）。SCALAR 事前フィルタが付く場合は [`search_subset_or_fallback`] が
//! 担う `Subset` 形状（Issue #409。per-query 写像でキャッシュには登録しない）
//! として扱う。hybrid の密側は対象外（#410 の担当）。加えて `bound.mode`
//! （`sql::mode::SearchMode::Precision`。`USING PLAN` 経由の推定を含む）が
//! `precision` のクエリも対象外（TASK-162・SEARCH-9。`precision::apply_gate` の
//! 確信度ゲートは DISTANCE 段の Top-2 マージンを厳密順位に基づいて評価する必要が
//! あり、ANN の近似近傍では真の最近傍の取りこぼしによりマージンを過大評価しうる
//! ため、常に厳密 brute-force 経路を使う）。
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
    /// `IndexedBase::build`（新規構築・再構築の両方）を呼んだ回数（成功・失敗を
    /// 問わず、`search_or_fallback` が構築を試みた時点で計上する）。
    pub builds: u64,
    /// 構築失敗（`HnswError`）でこの世代は brute-force へ縮退した回数。
    pub build_failures: u64,
    /// 差分比率が閾値を超えて再構築した回数（`builds` の内数）。
    pub rebuilds: u64,
    /// 未索引分（`Overlay::delta_slots`）を brute-force で補った呼び出し回数。
    pub delta_searches: u64,
    /// 索引を使わず全件 brute-force へ縮退した回数（`MIN_INDEXED_ROWS` 未満・
    /// 写像検証失敗・索引探索エラーを含む。`plain_scans`〔可視カーディナリティ
    /// 比が閾値未満〕・`masked_short`〔マスク付き探索の結果不足〕もこの内数）。
    pub fallbacks: u64,
    /// 可視カーディナリティ比（`visible_in_index / index.len()`）が
    /// `HnswParams::full_scan_ratio` 未満で plain scan（アリーナ全体の
    /// brute-force）を選んだ回数（Issue #409。`fallbacks` の内数）。
    pub plain_scans: u64,
    /// マスク付き探索（[`crate::hnsw::HnswIndex::search_masked`]）の結果件数が
    /// `min(k, visible_in_index)` に満たず plain scan へ縮退した回数（Issue #409。
    /// `fallbacks` の内数）。
    pub masked_short: u64,
    /// `Subset` 形状（SCALAR 事前フィルタ付き DISTANCE。Issue #409）でマスク付き
    /// 探索が縮退なしで完走した回数。`hits` とは独立に数える（`hits` は
    /// `FullVisible` 形状の `Ready` 到達かつ縮退なしの定義を変えない）。
    pub subset_searches: u64,
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
    /// `slot_of_node[node] != STALE_SLOT` を表す候補マスク（Issue #409）。
    /// `crate::hnsw::HnswIndex::search_masked` へそのまま渡し、失効ノードが
    /// 探索結果へ混入することを構造的に防ぐ（従来の `k + stale_nodes` オーバー
    /// フェッチ方式は `k_idx > MAX_EF` で全件 brute-force へ縮退していた——
    /// `docs/design/hnsw-generation-cache.md`「既知の限界」節の申し送りを
    /// 本フィールドで解消する）。
    visible_mask: crate::hnsw::NodeMask,
    /// 索引ノードのうち現アリーナのスロットへ写像できたノード数
    /// （`base.index.len() - stale_nodes` と一致）。可視カーディナリティ切替
    /// （`sql::hnsw_cache::search_with_overlay`）の分子に使う。
    visible_in_index: usize,
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
        let mut visible_mask = crate::hnsw::NodeMask::new(base.index.len());
        for (node, &slot) in slot_of_node.iter().enumerate() {
            if slot != STALE_SLOT {
                // `base.index.len() <= MAX_HNSW_NODES <= u32::MAX` 前提（`IndexedBase::build`
                // のノード番号割当と同じ不変条件）。
                if let Ok(node_u32) = u32::try_from(node) {
                    visible_mask.set(node_u32);
                }
            }
        }
        let visible_in_index = base.index.len().saturating_sub(stale_nodes);
        Overlay {
            generation,
            arena_len: arena.len(),
            slot_of_node,
            stale_nodes,
            delta_slots,
            delta_vectors,
            visible_mask,
            visible_in_index,
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
        // `NodeMask` は `u64` の語配列（1 ノード 1 ビット）。
        let d = self
            .visible_mask
            .len()
            .div_ceil(64)
            .saturating_mul(std::mem::size_of::<u64>());
        a.saturating_add(b).saturating_add(c).saturating_add(d)
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
    /// 索引済みベース。`None` は「このテーブル・ctx では一度も構築に成功して
    /// いない」状態（初回構築が失敗した直後。`build_failed_generation` の負の
    /// キャッシュを保持する場所として本エントリ自体は必要なため、ベースなしでも
    /// エントリを作る。`base` が `Some` の場合の契約は従来どおり: `build`（新規・
    /// 再構築の両方）に成功するたびに置き換わる）。
    base: Option<Arc<IndexedBase>>,
    /// 現在保持している最新のオーバーレイ（`base.built_table_generation` より
    /// 新しい世代のもの。`None` は「`base` の世代からまだ一度もオーバーレイを
    /// 計算していない」状態、または `base` 自体が `None`）。
    overlay: Option<Arc<Overlay>>,
    /// この世代では構築（新規・再構築）に失敗し、以後の探索は brute-force へ
    /// 縮退中であることを示す負のキャッシュ。`base`・`overlay` はこの失敗前の
    /// 値のまま温存し（`base` が元々 `None` なら `None` のまま）、世代が進めば
    /// 再挑戦できるようにする（`docs/design/hnsw-generation-cache.md`「構築失敗時の
    /// 負のキャッシュ」節参照）。
    build_failed_generation: Option<u64>,
    last_used: u64,
}

impl HnswCacheEntry {
    /// 容量判定用の概算バイト量（`base`・`overlay` それぞれが `Some` の分のみ）。
    fn approx_heap_bytes(&self) -> usize {
        self.base
            .as_ref()
            .map(|b| b.approx_heap_bytes())
            .unwrap_or(0)
            .saturating_add(
                self.overlay
                    .as_ref()
                    .map(|o| o.approx_heap_bytes())
                    .unwrap_or(0),
            )
    }
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
    plain_scans: AtomicU64,
    masked_short: AtomicU64,
    subset_searches: AtomicU64,
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
            plain_scans: AtomicU64::new(0),
            masked_short: AtomicU64::new(0),
            subset_searches: AtomicU64::new(0),
        }
    }

    /// `storage` は現状の実装では未参照（`sql::arena_cache::SqlArenaCache::lookup`・
    /// `sql::sparse_cache::SparseIndexCache::lookup` と異なり、世代不一致時の
    /// エントリ即時破棄をここでは行わない。破棄は `record_base`（新規ベース登録時の
    /// テーブル限定置換）・`evict_for_capacity`（LRU）・`evict_entry`（規模縮小時）に
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
            // ロック毒化: エントリの有無を判定できないため fail-closed に Miss
            // 扱いとする（codex-review P2 指摘対応: 以前はこの経路が misses に
            // 計上されず、通常の未登録エントリの初回ミスと合わせて実際のミス
            // 発生回数を過小報告していた）。
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::Miss;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::Miss;
        };
        let Some(position) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.built_ctx == *ctx)
        else {
            // 対象 `(table, ctx)` のエントリが一度も登録されていない、最も一般的な
            // 初回ミス（codex-review P2 指摘対応）。
            drop(guard);
            self.misses.fetch_add(1, Ordering::Relaxed);
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
        let Some(base) = entry.base.as_ref() else {
            // 過去に構築成功したことがない（初回失敗のみを負のキャッシュとして
            // 保持しているエントリ）。現世代では失敗していない（上のチェックを
            // 通過済み）ため、新規構築を試みさせる。
            drop(guard);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::Miss;
        };
        if let Some(overlay) = &entry.overlay {
            if overlay.generation == current_generation {
                let base = Arc::clone(base);
                let overlay = Arc::clone(overlay);
                drop(guard);
                // `hits` はここでは加算しない（codex-review P2 指摘対応）。
                // `Ready` はあくまで索引済みベース＋オーバーレイが揃っている
                // ことの通知であり、その後 `search_with_overlay` が
                // `k + stale_nodes > MAX_EF`・写像検証失敗・索引探索エラー等で
                // brute-force に縮退した場合、`hits`（実際に探索できた回数）と
                // `fallbacks`（縮退した回数）の両方に同一呼び出しが二重計上され
                // てしまう。`hits` は `search_with_overlay` が縮退なしで完了した
                // 時点でのみ加算する（呼び出し元 `search_or_fallback` 参照）。
                return Lookup::Ready(base, overlay);
            }
        }
        // ベースの世代自体が現世代と一致し、かつオーバーレイ未計算の場合も
        // （恒等オーバーレイを計算すれば `Ready` になる）、世代がさらに進んで
        // オーバーレイが古い場合も、いずれも `Overlay::compute` を 1 回行う必要が
        // ある点は同じため `NeedOverlay` で統一する。
        let base = Arc::clone(base);
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
        let own_bytes = base.approx_heap_bytes();
        if own_bytes > MAX_HNSW_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超えるベースは常駐させない（`sql::sparse_cache::
            // SparseIndexCache::insert`／`sql::arena_cache::SqlArenaCache::insert`
            // と同じ DoS 対策契約。codex-review P1 指摘対応: これを省略すると
            // `evict_for_capacity` が全エントリを追い出しても空きが作れないまま
            // 呼び出し元が無条件に push しており、単一テナントの過大な索引が
            // 容量上限を超えて常駐し得た）。呼び出し元はこのクエリのスナップショット
            // から構築した `base` をそのまま使ってよい。
            //
            // 常駐させないだけでは終わらせず、`build_failed_generation` と同じ
            // 負のキャッシュへ「この世代はキャッシュ不可」と記録する（Cursor
            // Bugbot 指摘対応: 記録を省略すると次回参照が再び `Miss` となり、
            // `search_or_fallback` が同一世代内の毎クエリで `IndexedBase::build`
            // 〔HNSW グラフ全体の構築。brute-force より遥かに高価〕をやり直して
            // しまう。世代が進めばテーブル規模の変化次第で再度キャッシュ可能に
            // なりうるため、失敗記録と同じく世代限定の一時的な縮退として扱う）。
            mark_uncacheable_in_guard(self, &mut guard, table, &base.built_ctx, current_generation);
            return base;
        }
        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.built_ctx == base.built_ctx)
        {
            guard.entries.remove(pos);
        }
        self.evict_for_capacity(&mut guard, own_bytes);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(HnswCacheEntry {
            table: table.to_string(),
            built_ctx: base.built_ctx.clone(),
            base: Some(Arc::clone(&base)),
            overlay: None,
            build_failed_generation: None,
            last_used: seq,
        });
        base
    }

    /// 新規エントリ追加前に呼ぶ（`record_base`／`record_build_failed` からのみ）。
    /// エントリ数上限・総バイト量上限の両方で LRU eviction する。
    fn evict_for_capacity(
        &self,
        guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
        incoming_bytes: usize,
    ) {
        self.evict_while(guard, incoming_bytes, true);
    }

    /// 既存エントリの overlay 更新前に呼ぶ（`record_overlay_for` からのみ）。
    /// エントリ数はこの呼び出しでは増えないため、総バイト量上限のみで
    /// eviction する（Cursor Bugbot 指摘対応: `evict_for_capacity` をそのまま
    /// 使うと、overlay 更新はエントリを追加しないにもかかわらず
    /// `entries.len() >= MAX_HNSW_CACHE_ENTRIES`（キャッシュが既に満杯）の
    /// 条件だけでエビクトが発火し、build・rebuild・世代 overlay のたびに
    /// 無関係な別 `(table, ctx)` の索引エントリが巻き添えで追い出され、次回
    /// ミスして再構築を強制されていた）。
    fn evict_for_extra_bytes(
        &self,
        guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
        incoming_bytes: usize,
    ) {
        self.evict_while(guard, incoming_bytes, false);
    }

    fn evict_while(
        &self,
        guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
        incoming_bytes: usize,
        check_entry_count: bool,
    ) {
        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(HnswCacheEntry::approx_heap_bytes)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while (check_entry_count && guard.entries.len() >= MAX_HNSW_CACHE_ENTRIES)
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
            total_bytes = total_bytes.saturating_sub(removed.approx_heap_bytes());
        }
    }

    /// 指定 `(table, ctx)` のエントリのみを破棄する（当該テナントから見た
    /// `arena.len()`〔RLS 可視行数〕が `MIN_INDEXED_ROWS` 未満へ縮小した場合の
    /// 後始末）。Cursor Bugbot 指摘対応: 以前は `table` 一致のみで全 `ctx` 分を
    /// まとめて破棄しており、可視行数が少ない/0 件のテナントのクエリが同一
    /// テーブルの他テナントの構築済み索引まで巻き添えで破棄し、繰り返しフル
    /// リビルドを強制していた。`arena.len()` はテナントごとの RLS フィルタ後の
    /// 値であり、他テナントの可視行数とは独立なため `ctx` も一致条件に含める。
    ///
    /// `current_generation`（呼び出し元のクエリが見ているテーブル世代）より
    /// **新しい**世代のエントリは破棄しない（Cursor Bugbot 指摘対応: 世代チェック
    /// なしに呼んでいた旧実装では、可視行数が少ない/0 件のクエリが、並行して
    /// 別クエリが構築した「より大きい世代」の索引済みエントリまで巻き添えで
    /// 追い出し、次回の別クエリで高価な HNSW 再構築を強制し得た）。エントリの
    /// 世代は overlay があればその世代、無ければベースの構築世代、ベースも無い
    /// 〔負のキャッシュのみ〕場合は `build_failed_generation` の順に判定する。
    fn evict_entry(&self, table: &str, ctx: &PolicyContext, current_generation: u64) {
        if let Ok(mut guard) = self.state.write() {
            guard.entries.retain(|e| {
                if e.table != table || e.built_ctx != *ctx {
                    return true;
                }
                let entry_generation = e
                    .overlay
                    .as_ref()
                    .map(|o| o.generation)
                    .or_else(|| e.base.as_ref().map(|b| b.built_table_generation))
                    .or(e.build_failed_generation);
                // 世代が判定できない（全フィールド None。通常発生しない）場合は
                // 保護対象が無いとみなし破棄する（fail-closed に「破棄する」側へ
                // 倒す。破棄しても次回参照時に再構築されるだけで安全側）。
                entry_generation.is_some_and(|g| g > current_generation)
            });
        }
    }

    /// [`IndexedBase::arena_identity_mismatch_guard`] が不整合を検出した特定の
    /// `(table, ctx)` エントリのみを破棄する（Cursor Bugbot 指摘対応: 以前は
    /// ガード検出時にこのクエリを brute-force へ縮退させるだけで不整合な
    /// `IndexedBase` をキャッシュへ温存していたため、`lookup` が同一エントリへ
    /// `NeedOverlay` を返し続け、同じ `(table, ctx)` の以後の全クエリが同じ
    /// ガードへ当たり続け ANN 経路が実質的に無効化されたままになっていた。
    /// 破棄した次回の `lookup` は `Miss` となり、`search_or_fallback` が新規
    /// 構築を試みて ANN 経路へ復帰できる）。
    ///
    /// `base`（呼び出し元がガード検出直前に手にしていた不整合な `IndexedBase`）
    /// の `Arc` ポインタと一致するエントリのみを対象にする。世代チェックを
    /// 経由せず（この `base` はそもそも現在のアリーナと同定できない壊れた
    /// 状態であり保護すべき正当な理由がない）、かつ `Arc::ptr_eq` で対象を
    /// 厳密に絞ることで、並行して別スレッドが同じ `(table, ctx)` を正しい
    /// `base` へ差し替え済みだった場合にそれを巻き添えで破棄しない
    /// （テナント境界・fail-closed 契約は維持: 破棄対象は呼び出し元が既に
    /// 特定した自テーブル・自テナントのエントリ 1 件に限る）。
    fn evict_entry_if_mismatched(&self, table: &str, ctx: &PolicyContext, base: &Arc<IndexedBase>) {
        if let Ok(mut guard) = self.state.write() {
            guard.entries.retain(|e| {
                !(e.table == table
                    && e.built_ctx == *ctx
                    && e.base.as_ref().is_some_and(|b| Arc::ptr_eq(b, base)))
            });
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
            plain_scans: self.plain_scans.load(Ordering::Relaxed),
            masked_short: self.masked_short.load(Ordering::Relaxed),
            subset_searches: self.subset_searches.load(Ordering::Relaxed),
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
        // 現在のクエリが見ている世代より新しいエントリは巻き添えにしない
        // （`evict_entry` ドキュメント参照）。世代が判定できない場合は
        // eviction 自体をスキップする（fail-closed。誤って破棄する側へは
        // 倒さない——判定不能を「保護すべき新しい世代ではない」とみなして
        // 破棄すると、並行クエリが構築した索引を無条件で失わせてしまう）。
        if let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table) {
            access.cache.evict_entry(table, ctx, current_generation);
        }
        return full_scan(access.cache);
    }

    let base = match access.cache.lookup(access.storage, read_txn, table, ctx) {
        Lookup::BuildFailedThisGeneration => return full_scan(access.cache),
        Lookup::Ready(base, overlay) => {
            return search_with_overlay(
                access,
                base,
                overlay,
                provider,
                arena,
                query,
                k,
                OverlaySuccessStat::Hits,
            );
        }
        Lookup::NeedOverlay(base) => base,
        Lookup::Miss => {
            let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
            else {
                return full_scan(access.cache);
            };
            access.cache.builds.fetch_add(1, Ordering::Relaxed);
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
    if base.arena_identity_mismatch_guard(arena, ctx) {
        // 呼び出し元の適用条件（フィルタなし DISTANCE）が正しく守られていれば
        // 起こらないはずの不整合。fail-closed に全件へ縮退するだけでなく、
        // 不整合な `base` を保持するエントリ自体をキャッシュから退避する
        // （Cursor Bugbot 指摘対応「Guard disables ANN without recovery」:
        // 退避しないと `lookup` が同じ `(table, ctx)` に対し `NeedOverlay` を
        // 返し続け、以後の全クエリが同じガードに当たり続けて ANN 経路が
        // 再構築されないまま無効化されてしまう）。退避対象はこの `(table,
        // ctx)` の当該エントリのみで、他テナント・他テーブルへは波及しない。
        access.cache.evict_entry_if_mismatched(table, ctx, &base);
        return full_scan(access.cache);
    }
    let overlay = Overlay::compute(&base, arena, current_generation);
    if overlay.needs_rebuild(n) {
        access.cache.builds.fetch_add(1, Ordering::Relaxed);
        match IndexedBase::build(
            arena,
            access.provider.params(),
            ctx.clone(),
            current_generation,
        ) {
            Ok(built) => {
                access.cache.rebuilds.fetch_add(1, Ordering::Relaxed);
                let new_base = access.cache.record_base(access.storage, table, built);
                let mut identity_mask = crate::hnsw::NodeMask::new(new_base.index.len());
                for node in 0..new_base.index.len() {
                    if let Ok(node_u32) = u32::try_from(node) {
                        identity_mask.set(node_u32);
                    }
                }
                let identity_overlay = Arc::new(Overlay {
                    generation: current_generation,
                    arena_len: arena.len(),
                    slot_of_node: (0..new_base.index.len() as u32).collect(),
                    stale_nodes: 0,
                    delta_slots: Vec::new(),
                    delta_vectors: Vec::new(),
                    visible_mask: identity_mask,
                    visible_in_index: new_base.index.len(),
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
                    OverlaySuccessStat::None,
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
    search_with_overlay(
        access,
        base,
        overlay,
        provider,
        arena,
        query,
        k,
        OverlaySuccessStat::None,
    )
}

/// `Subset` 形状（Issue #409。SCALAR 事前フィルタ付き DISTANCE。`sql::exec::
/// execute_statement_with_cache` の適用条件は §モジュールドキュメント
/// 「適用条件」節・`docs/design/hnsw-rls-cardinality-switch.md` 参照）の索引済み
/// 探索＋未索引分 brute-force 併用。`FullVisible`（[`search_or_fallback`]）との
/// 違いは 3 点（いずれも「アリーナが可視全集合ではなく WHERE 適用後の部分集合」
/// という前提の違いから来る）:
///
/// 1. `arena.len() < MIN_INDEXED_ROWS` でもエントリを evict しない（フィルタで
///    絞られた行数が少ないことは「テーブル自体が小規模」を意味しない）。
/// 2. `Lookup::Miss`（索引未構築）では新規構築を試みず、常に plain scan へ縮退
///    する（`base` はフィルタなしクエリが可視全集合から構築する契約——部分集合
///    アリーナから `IndexedBase::build` を呼ぶと以後の `FullVisible` 経路が
///    誤った base を再利用してしまう）。
/// 3. per-query の `Overlay::compute` 結果を [`record_overlay_for`] へ登録しない
///    （`needs_rebuild` も評価しない）——WHERE で除外された行が churn に見えて
///    無用な再構築を誘発したり、`FullVisible` の次クエリが参照するはずの
///    キャッシュ済みオーバーレイを汚したりしないようにする。
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_subset_or_fallback(
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

    let base = match access.cache.lookup(access.storage, read_txn, table, ctx) {
        Lookup::BuildFailedThisGeneration | Lookup::Miss => return full_scan(access.cache),
        Lookup::Ready(base, _full_arena_overlay) => base,
        Lookup::NeedOverlay(base) => base,
    };

    let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table) else {
        return full_scan(access.cache);
    };
    if base.arena_identity_mismatch_guard(arena, ctx) {
        // `FullVisible` 経路（`search_or_fallback`）と異なり、ここでは巻き添えを
        // 避けるためエントリを退避しない（この不整合は通常発生しない防御的
        // 経路であり、`FullVisible` クエリが正しく参照し続けられる余地を
        // 残す。§本関数ドキュメンテーションコメント参照）。
        return full_scan(access.cache);
    }

    // per-query の写像を計算する（キャッシュへは登録しない。§本関数
    // ドキュメンテーションコメント参照）。`Overlay::compute` は「渡された
    // arena のスロット番号系列に対する索引済みノードの写像」を汎用的に計算
    // するため、可視全集合ではなく WHERE 適用後の部分集合アリーナを渡しても
    // そのまま正しく動く（部分集合に含まれない索引済みノードは自然に
    // `STALE_SLOT` として除外される）。
    let overlay = Arc::new(Overlay::compute(&base, arena, current_generation));
    search_with_overlay(
        access,
        base,
        overlay,
        provider,
        arena,
        query,
        k,
        OverlaySuccessStat::SubsetSearches,
    )
}

impl IndexedBase {
    /// `arena`・`ctx` が本ベースの構築時と取り違えられていないことの検証
    /// （`Overlay::compute` は `arena` の全スロットを本ベースのノードキー・
    /// 次元と突き合わせて走査するため、`base` 自体が別テーブル・別テナント
    /// 由来のまま渡された場合に不整合な写像を作ってしまう）。
    ///
    /// 実際に検証可能な 2 つの不変条件で判定する（`Ready`/`NeedOverlay` に
    /// 到達する経路では `lookup` が `(table, ctx)` の完全一致でエントリを
    /// 選んでいるため通常は発生しないが、fail-closed の最終防御として
    /// 冗長に確認する。security.md P0「テナント分離の検査を外す/緩める/
    /// バイパス経路を作らない」）:
    /// - `self.index.dim()` と `arena.dim()` が一致すること（別テーブル・
    ///   別次元のアリーナ取り違えを検出する）
    /// - `self.built_ctx` と `ctx` が一致すること（別テナント・別可視性の
    ///   `PolicyContext` 取り違えを検出する。`PolicyContext` の `PartialEq`
    ///   はテナント ID・許可可視性集合の完全一致を要求する）
    ///
    /// 一致していれば `false`（検証通過）、いずれか不一致なら `true`
    /// （呼び出し元は全件 brute-force へ縮退する）を返す。
    fn arena_identity_mismatch_guard(&self, arena: &VectorArena, ctx: &PolicyContext) -> bool {
        self.index.dim() != arena.dim() || self.built_ctx != *ctx
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
    mark_uncacheable_in_guard(access.cache, &mut guard, table, ctx, generation);
}

/// `(table, ctx)` の指定世代 `generation` を「この世代は brute-force へ縮退中」の
/// 負のキャッシュとして記録する（`record_build_failed`〔構築失敗〕・
/// `HnswIndexCache::record_base`〔構築成功したが単体で `MAX_HNSW_CACHE_TOTAL_BYTES`
/// を超えキャッシュ不可〕の共通処理）。既存エントリがあれば `base`／`overlay` は
/// 温存したまま `build_failed_generation` のみ更新し、無ければ `base: None` の
/// 負のキャッシュ専用エントリを新設する。世代が進めば次回参照時に再度構築を
/// 試行できる（`docs/design/hnsw-generation-cache.md`「構築失敗時の負のキャッシュ」
/// 節参照）。呼び出し元は書き込みロック `guard` を既に保持している前提。
fn mark_uncacheable_in_guard(
    cache: &HnswIndexCache,
    guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
    table: &str,
    ctx: &PolicyContext,
    generation: u64,
) {
    if let Some(entry) = guard
        .entries
        .iter_mut()
        .find(|e| e.table == table && e.built_ctx == *ctx)
    {
        entry.build_failed_generation = Some(generation);
        return;
    }
    // 既存エントリが無い場合（一度も構築成功したことがないテーブル・ctx での
    // 初回失敗、またはキャッシュ不可判定）: `base: None` の負のキャッシュ専用
    // エントリを新設する。これを省略すると、毎クエリ `IndexedBase::build`
    // （索引構築本体・決して安価ではない）を再試行し続けてしまう（同一世代内の
    // 再試行連打を防ぐという本節の目的そのものを一度も構築成功していないテーブルで
    // 満たせなくなる）。
    cache.evict_for_capacity(guard, 0);
    let seq = cache.seq.fetch_add(1, Ordering::Relaxed);
    guard.entries.push(HnswCacheEntry {
        table: table.to_string(),
        built_ctx: ctx.clone(),
        base: None,
        overlay: None,
        build_failed_generation: Some(generation),
        last_used: seq,
    });
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
    let overlay_bytes = overlay.approx_heap_bytes();
    let entry_total_bytes = base.approx_heap_bytes().saturating_add(overlay_bytes);
    if entry_total_bytes > MAX_HNSW_CACHE_TOTAL_BYTES {
        // base + overlay の合計だけで総量上限を超える場合は overlay を常駐させ
        // ない（`record_base` と同じ DoS 対策契約。codex-review P1 指摘対応:
        // これを省略すると overlay 追加後の合計容量が再判定されず、テナント・
        // テーブルごとの overlay 差分（`delta_vectors` 等）の蓄積で容量上限を
        // 超えて常駐し得た）。呼び出し元はこのクエリで計算した `overlay` を
        // その場の探索にそのまま使ってよい（`search_or_fallback` は
        // `record_overlay_for` の戻り値を見ず、呼び出し元が保持する `overlay` で
        // 続けて `search_with_overlay` を呼ぶ）。
        //
        // 常駐させないだけでは終わらせず、`record_base`〔単体超過〕・
        // `record_build_failed`〔構築失敗〕と同じ負のキャッシュへ「この世代は
        // キャッシュ不可」と記録する（Bugbot "Oversized overlay skips negative
        // cache" 指摘対応: 記録を省略すると次回参照が再び `NeedOverlay` となり、
        // `search_or_fallback` が同一世代内の毎クエリで `Overlay::compute`
        // 〔delta_slots・delta_vectors の再構築を伴う〕をやり直してしまう）。
        mark_uncacheable_in_guard(
            access.cache,
            &mut guard,
            table,
            &base.built_ctx,
            current_generation,
        );
        return;
    }
    // 対象エントリが既に overlay を保持している場合（世代進行に伴う再計算等）、
    // `HnswCacheEntry::approx_heap_bytes` はその旧 overlay 分を含んだ値を
    // 返すため、`evict_while` が合算する `total_bytes` にも旧 overlay が
    // 計上済みである。ここで新 overlay の全量をそのまま追加分として渡すと
    // 旧 overlay 分を二重計上してしまい、世代更新のたびに無関係エントリや
    // 更新対象自身を過剰に追い出し、次回クエリで高価な HNSW 再構築を強制し
    // 得る（codex-review P2 指摘対応）。旧 overlay 分を差し引いた純増分のみを
    // 渡すことで二重計上を避ける（`record_base` と異なりエントリ数はこの
    // 呼び出しで増えないため、エントリ数上限は判定条件に含めない。
    // Cursor Bugbot 指摘対応: `evict_for_capacity` をそのまま使うとキャッシュが
    // 既に満杯なだけで overlay 更新のたびに無関係な別エントリが巻き添えで
    // 追い出されていた）。このエントリ自身が LRU の対象に選ばれ追い出される
    // 可能性はあるが、その場合は下の `find` が失敗し overlay を反映しない
    // だけであり、容量上限を超えて常駐することはない（fail-closed）。
    let existing_overlay_bytes = guard
        .entries
        .iter()
        .find(|e| {
            e.table == table
                && e.built_ctx == base.built_ctx
                && e.base.as_ref().is_some_and(|b| Arc::ptr_eq(b, base))
        })
        .and_then(|e| e.overlay.as_ref())
        .map(|o| o.approx_heap_bytes())
        .unwrap_or(0);
    let extra_bytes = overlay_bytes.saturating_sub(existing_overlay_bytes);
    access.cache.evict_for_extra_bytes(&mut guard, extra_bytes);
    if let Some(entry) = guard.entries.iter_mut().find(|e| {
        e.table == table
            && e.built_ctx == base.built_ctx
            && e.base.as_ref().is_some_and(|b| Arc::ptr_eq(b, base))
    }) {
        entry.overlay = Some(overlay);
        entry.build_failed_generation = None;
    }
}

/// [`search_with_overlay`] が完了時に成功統計をどこへ加算するか（Issue #409）。
/// `NeedOverlay`（再構築直後）経路からは `None` を渡し従来どおり計上しない。
#[derive(Clone, Copy)]
enum OverlaySuccessStat {
    /// `FullVisible` 形状・`Lookup::Ready` 到達（`hits` の既存契約を変えない）。
    Hits,
    /// `Subset` 形状（SCALAR 事前フィルタ付き DISTANCE。Issue #409）。
    SubsetSearches,
    /// 再構築直後の恒等オーバーレイ等、成功統計を計上しない経路。
    None,
}

/// [`Lookup::Ready`]（または `NeedOverlay` から新規計算した直後）に共通する索引
/// 探索＋未索引分 brute-force のマージ本体（Issue #409 で「可視候補数 ÷ 索引
/// ノード数」の比による plain scan／マスク付き ANN 探索の切替へ書き換え。
/// 旧実装の `k + stale_nodes` オーバーフェッチ〔`k_idx > MAX_EF` で即座に全件
/// brute-force へ縮退〕は撤去し、`Overlay::visible_mask` を
/// `crate::hnsw::HnswIndex::search_masked` へ渡すことで失効ノードを構造的に
/// 除外する。詳細は `docs/design/hnsw-rls-cardinality-switch.md` 参照）。
///
/// `success_stat` は本関数が縮退なしで完了した場合にどの統計を加算するかを
/// 指定する（[`OverlaySuccessStat`] 参照。codex-review P2 指摘対応の延長:
/// `lookup` 側で先に加算すると、本関数がその後の分岐で縮退した場合に成功統計と
/// `fallbacks` の両方へ同一呼び出しが二重計上されてしまうため、常に本関数の
/// 末尾でのみ確定させる）。
///
/// 引数 8 個は `clippy::too_many_arguments`（閾値 7）を超えるが、`search_or_fallback`
/// と同じ方針（本ファイル内 `#[allow(clippy::too_many_arguments)]` 参照）で許容する。
#[allow(clippy::too_many_arguments)]
fn search_with_overlay(
    access: &HnswCacheAccess<'_>,
    base: Arc<IndexedBase>,
    overlay: Arc<Overlay>,
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    query: &[f32],
    k: usize,
    success_stat: OverlaySuccessStat,
) -> Result<Vec<CandidateHit>, KernelError> {
    if overlay.arena_len != arena.len() {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    // 可視カーディナリティ切替（Issue #409・`HnswParams::full_scan_ratio`）:
    // `visible_in_index / index.len() < full_scan_ratio` なら plain scan
    // （アリーナ全体の brute-force）。整数比較 `visible_in_index * den <
    // index.len() * num` で丸め誤差を避け、`checked_mul` のオーバーフロー時は
    // fail-closed に plain scan へ倒す（比較不能を「索引を信用しない」側へ）。
    let ratio = access.provider.params().full_scan_ratio;
    let index_len = base.index.len();
    let visible_in_index = overlay.visible_in_index;
    let below_ratio = match (
        (visible_in_index as u64).checked_mul(ratio.denominator as u64),
        (index_len as u64).checked_mul(ratio.numerator as u64),
    ) {
        (Some(lhs), Some(rhs)) => lhs < rhs,
        _ => true,
    };
    if index_len == 0 || below_ratio {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access.cache.plain_scans.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    let ef = access.provider.effective_ef(k);
    let index_hits = SEARCH_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        base.index
            .search_masked(query, k, ef, Some(&overlay.visible_mask), &mut scratch)
    });
    let index_hits = match index_hits {
        Ok(hits) => hits,
        Err(_) => {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        }
    };

    // マスク付き探索の結果件数が「可視ノード数と要求 k の小さい方」に満たない
    // 場合、ビーム幅内でグラフ探索が可視ノードを十分辿り切れなかったことを
    // 意味する（§`docs/design/hnsw-rls-cardinality-switch.md`「masked_short」節）。
    // `#410` の担当である `ef` 拡張再探索は行わず、fail-closed に plain scan へ
    // 縮退して k 件充足を保証する。
    let expected = k.min(visible_in_index);
    if index_hits.len() < expected {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access.cache.masked_short.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    let mut mapped: Vec<CandidateHit> = Vec::with_capacity(index_hits.len());
    for hit in index_hits {
        let node = hit.id as u32;
        let Some(&slot) = overlay.slot_of_node.get(node as usize) else {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        };
        if slot == STALE_SLOT {
            // `search_masked` はマスク受理ノードのみを結果に含むはずであり、
            // 通常この分岐には到達しない（`overlay.visible_mask` は
            // `slot_of_node[node] != STALE_SLOT` のノードのみを受理する）。
            // マスクとオーバーレイの不整合という想定外事態を fail-closed に
            // 全件 brute-force へ倒す（黙って握りつぶさない）。
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
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
    match success_stat {
        // ここへ到達するのは、上記のいずれの縮退分岐（`fallbacks` 加算）も通らず
        // 索引探索が完了した場合のみ。成功統計の契約はこの時点で初めて確定する。
        OverlaySuccessStat::Hits => {
            access.cache.hits.fetch_add(1, Ordering::Relaxed);
        }
        OverlaySuccessStat::SubsetSearches => {
            access.cache.subset_searches.fetch_add(1, Ordering::Relaxed);
        }
        OverlaySuccessStat::None => {}
    }
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

    /// 恒等マスク（全ノード受理）＋全ノード可視の `visible_in_index` を返す
    /// テスト専用ヘルパ（Issue #409 で `Overlay` に追加した 2 フィールドを
    /// 「未失効の索引をそのまま使う」テストで簡潔に埋めるため）。
    fn identity_mask_and_visible(base: &IndexedBase) -> (crate::hnsw::NodeMask, usize) {
        let mut mask = crate::hnsw::NodeMask::new(base.index.len());
        for node in 0..base.index.len() {
            if let Ok(node_u32) = u32::try_from(node) {
                mask.set(node_u32);
            }
        }
        (mask, base.index.len())
    }

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
    fn evict_entry_removes_only_the_matching_ctx_entry_for_that_table() {
        // Cursor Bugbot 指摘対応（PR #434）: 可視行数が少ない/0 件のテナントの
        // クエリで `evict_entry` が呼ばれても、同一テーブルの他テナントの
        // 構築済みエントリは温存されることを固定する回帰テスト。
        let path = unique_db_path("hnsw-cache-evict-entry");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c1 = ctx("tenant-a");
        let c2 = ctx("tenant-b");
        let mut built_generation = 0u64;

        for c in [&c1, &c2] {
            let read_txn = storage.db().begin_read().unwrap();
            let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
            built_generation = gen;
            let arena = build_arena(&read_txn, "docs", c);
            let built =
                IndexedBase::build(&arena, crate::hnsw::HnswParams::default(), c.clone(), gen)
                    .expect("build");
            cache.record_base(&storage, "docs", built);
        }
        assert_eq!(cache.stats().entries, 2);
        // 現在のクエリが見ている世代（`built_generation`）以下のエントリのみが
        // 破棄対象になる契約を固定する（Cursor Bugbot 指摘対応・PR #434）。
        cache.evict_entry("docs", &c1, built_generation);
        assert_eq!(cache.stats().entries, 1);
        cache.evict_entry("docs", &c2, built_generation);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn evict_entry_does_not_evict_a_newer_generation_entry() {
        // Cursor Bugbot 指摘対応（PR #434）: 可視行数が縮小した古い世代のクエリが
        // `evict_entry` を呼んでも、並行して別クエリが構築した「より新しい世代」
        // のエントリは巻き添えで破棄されないことを固定する回帰テスト。
        let path = unique_db_path("hnsw-cache-evict-entry-generation");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let old_generation = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let built = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            c.clone(),
            old_generation,
        )
        .expect("build");
        cache.record_base(&storage, "docs", built);
        assert_eq!(cache.stats().entries, 1);

        // このエントリより古い世代（`old_generation` 未満）を「現在のクエリの
        // 世代」として `evict_entry` を呼んでも、より新しいエントリは保護され
        // 破棄されない。
        let stale_current_generation = old_generation.saturating_sub(1);
        cache.evict_entry("docs", &c, stale_current_generation);
        assert_eq!(
            cache.stats().entries,
            1,
            "より新しい世代のエントリが誤って破棄された"
        );

        // 現在のクエリの世代がエントリの世代以上であれば、通常どおり破棄される。
        cache.evict_entry("docs", &c, old_generation);
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

    #[test]
    fn lookup_counts_miss_for_unregistered_entry() {
        // codex-review P2 指摘対応（PR #434）: 対象 `(table, ctx)` のエントリが
        // 一度も登録されていない、最も一般的な初回ミスが `misses` に計上され
        // ないまま返っていた（旧実装）ことの回帰固定。
        let path = unique_db_path("hnsw-cache-miss-count");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        assert!(matches!(
            cache.lookup(&storage, &read_txn, "docs", &c),
            Lookup::Miss
        ));
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn search_with_overlay_does_not_double_count_hits_on_fallback() {
        // codex-review P2 指摘対応（PR #434）: `Lookup::Ready` 到達の時点で
        // `hits` を先に加算していたため、その後 `search_with_overlay` が
        // 縮退（本テストでは `overlay.arena_len` を意図的に不一致にして最初の
        // 縮退分岐を発火させる）した場合、`hits` と `fallbacks` の両方に同一
        // 呼び出しが二重計上されていた（旧実装では hits == 1 になっていた）。
        let path = unique_db_path("hnsw-cache-hits-fallback");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        seed_row(&storage, "docs", 1, "tenant-a", &[1.0, 0.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 2, "tenant-a", &[0.0, 1.0, 0.0, 0.0]);
        let c = ctx("tenant-a");
        let cache = HnswIndexCache::new();

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let base = IndexedBase::build(&arena, crate::hnsw::HnswParams::default(), c.clone(), gen)
            .expect("build");
        let base = Arc::new(base);

        let (visible_mask, visible_in_index) = identity_mask_and_visible(&base);
        let overlay = Arc::new(Overlay {
            generation: gen,
            arena_len: arena.len() + 1,
            slot_of_node: (0..base.index.len() as u32).collect(),
            stale_nodes: 0,
            delta_slots: Vec::new(),
            delta_vectors: Vec::new(),
            visible_mask,
            visible_in_index,
        });

        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
        };
        let provider = crate::kernel::CpuScalarProvider;
        let query = [1.0, 0.0, 0.0, 0.0];
        let result = search_with_overlay(
            &access,
            base,
            overlay,
            &provider,
            &arena,
            &query,
            1,
            OverlaySuccessStat::Hits,
        )
        .expect("fallback search succeeds");
        assert_eq!(result.len(), 1);

        let stats = cache.stats();
        assert_eq!(stats.fallbacks, 1, "縮退は 1 回のみ計上される");
        assert_eq!(
            stats.hits, 0,
            "縮退した呼び出しは hits に計上されない（旧実装は 1 になっていた）"
        );
    }

    #[test]
    fn record_overlay_for_does_not_evict_unrelated_entries_when_cache_is_at_entry_capacity() {
        // Cursor Bugbot 指摘対応（PR #434）: overlay 更新はエントリを追加しない
        // にもかかわらず、以前は `entries.len() >= MAX_HNSW_CACHE_ENTRIES`
        // （キャッシュが既に満杯）という条件だけで build・rebuild・世代 overlay
        // のたびに無関係な別 `(table, ctx)` の索引エントリが巻き添えで追い出され
        // ていた。エントリ数がちょうど上限に達している状態で overlay 更新して
        // も、総バイト量が上限を超えない限りエントリ数が変わらないことを固定する。
        let path = unique_db_path("hnsw-cache-overlay-no-evict");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let mut bases = Vec::new();
        for i in 0..MAX_HNSW_CACHE_ENTRIES {
            let table = format!("docs_{i}");
            create_table(&storage, &table, 4);
            let read_txn = storage.db().begin_read().unwrap();
            let gen = crate::catalog::table_generation_in_txn(&read_txn, &table).unwrap();
            let arena = build_arena(&read_txn, &table, &c);
            let built =
                IndexedBase::build(&arena, crate::hnsw::HnswParams::default(), c.clone(), gen)
                    .expect("build");
            let base = cache.record_base(&storage, &table, built);
            bases.push((table, base));
        }
        assert_eq!(cache.stats().entries, MAX_HNSW_CACHE_ENTRIES);

        let (table0, base0) = &bases[0];
        let gen0 =
            crate::catalog::table_generation_in_txn(&storage.db().begin_read().unwrap(), table0)
                .unwrap();
        let (visible_mask, visible_in_index) = identity_mask_and_visible(base0);
        let overlay = Arc::new(Overlay {
            generation: gen0,
            arena_len: base0.index.len(),
            slot_of_node: (0..base0.index.len() as u32).collect(),
            stale_nodes: 0,
            delta_slots: Vec::new(),
            delta_vectors: Vec::new(),
            visible_mask,
            visible_in_index,
        });
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
        };
        record_overlay_for(&access, table0, base0, overlay);

        assert_eq!(
            cache.stats().entries,
            MAX_HNSW_CACHE_ENTRIES,
            "overlay 更新だけではエントリ数が変わらないため、他エントリを巻き添えで \
             追い出してはならない"
        );
    }

    #[test]
    fn mark_uncacheable_in_guard_causes_lookup_to_report_build_failed_this_generation() {
        // Cursor Bugbot 指摘対応（PR #434・"Uncacheable HNSW rebuilt every query"）:
        // `record_base` が単体で `MAX_HNSW_CACHE_TOTAL_BYTES` を超えるベースを
        // 拒否する経路（`mark_uncacheable_in_guard` 呼び出し）と、既存の
        // `record_build_failed`（構築失敗）は同じ負のキャッシュ機構を共有する
        // （リファクタで抽出済み）。実際に 1GiB 超のベースを構築する重量級テストの
        // 代わりに、共有機構そのものを直接検証する: 一度 `mark_uncacheable_in_guard`
        // で「この世代はキャッシュ不可」と記録すれば、以後同一世代内の `lookup` は
        // 毎回 `BuildFailedThisGeneration` を返し、`search_or_fallback` は
        // `IndexedBase::build`（HNSW グラフ全体の構築）を再試行しない
        // （呼び出し元は `full_scan` へ直行する）。
        let path = unique_db_path("hnsw-cache-mark-uncacheable");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();

        // まだエントリが無い状態からの記録（初回構築成功時点で単体超過と判明した
        // ケース、または一度も構築成功していないテーブルでの初回失敗ケースに対応）。
        {
            let mut guard = cache.state.write().unwrap();
            mark_uncacheable_in_guard(&cache, &mut guard, "docs", &c, gen);
        }
        assert_eq!(cache.stats().entries, 1);

        let read_txn2 = storage.db().begin_read().unwrap();
        match cache.lookup(&storage, &read_txn2, "docs", &c) {
            Lookup::BuildFailedThisGeneration => {}
            _ => panic!(
                "同一世代内の lookup は BuildFailedThisGeneration を返し、\
                 毎クエリのフル再構築を避けなければならない"
            ),
        }

        // 世代が進めば負のキャッシュは新しい世代には適用されず、再挑戦できる
        // （`docs/design/hnsw-generation-cache.md`「構築失敗時の負のキャッシュ」節）。
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();
        let read_txn3 = storage.db().begin_read().unwrap();
        match cache.lookup(&storage, &read_txn3, "docs", &c) {
            Lookup::Miss => {}
            _ => panic!("世代が進んだ後は再構築を試みられるよう Miss を返すべき"),
        }
    }

    #[test]
    fn arena_identity_mismatch_guard_detects_dim_and_ctx_mismatch() {
        // codex-review P2 指摘対応（PR #434「アリーナ不整合ガードが常に無効」）:
        // 旧実装は引数を無視して常に `false` を返し、呼び出し元の fail-closed
        // 分岐に決して到達しなかった。dim 不一致（別テーブル取り違え）・
        // `PolicyContext` 不一致（別テナント取り違え）のいずれも検出することを
        // 固定する。
        let path = unique_db_path("hnsw-cache-arena-identity-guard");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs4", 4);
        create_table(&storage, "docs8", 8);
        let c_a = ctx("tenant-a");
        let c_b = ctx("tenant-b");

        let read_txn = storage.db().begin_read().unwrap();
        let gen4 = crate::catalog::table_generation_in_txn(&read_txn, "docs4").unwrap();
        let arena4 = build_arena(&read_txn, "docs4", &c_a);
        let base = IndexedBase::build(
            &arena4,
            crate::hnsw::HnswParams::default(),
            c_a.clone(),
            gen4,
        )
        .expect("build");

        // dim・ctx とも本ベースの構築時と一致: 検証通過（fail-closed 分岐に入らない）。
        assert!(!base.arena_identity_mismatch_guard(&arena4, &c_a));

        // dim 不一致（別テーブル由来のアリーナ取り違え）。
        let arena8 = build_arena(&read_txn, "docs8", &c_a);
        assert!(base.arena_identity_mismatch_guard(&arena8, &c_a));

        // ctx 不一致（別テナント取り違え）。
        assert!(base.arena_identity_mismatch_guard(&arena4, &c_b));
    }

    #[test]
    fn builds_stat_counts_failed_build_attempts() {
        // codex-review P2 指摘対応（PR #434「builds が失敗した構築試行を計上
        // しない」）: 旧実装は `record_base`（構築成功後）でのみ `builds` を
        // 加算しており、`IndexedBase::build` が `HnswError` で失敗した場合は
        // `build_failures` のみ計上され `builds` は増えなかった。NaN
        // embedding を含む `MIN_INDEXED_ROWS` 件のコーパスで構築を確実に
        // 失敗させ、失敗試行も `builds` に計上されることを固定する
        // （`search_or_fallback` 自体は brute-force へ縮退し `Ok` を返す）。
        let path = unique_db_path("hnsw-cache-builds-count-failure");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");

        let embeddings: Vec<[f32; 4]> = (0..MIN_INDEXED_ROWS)
            .map(|i| {
                if i == 0 {
                    [f32::NAN, 0.0, 0.0, 0.0]
                } else {
                    [i as f32, 0.0, 0.0, 0.0]
                }
            })
            .collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-builds-count-failure")
                .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        let read_txn = storage.db().begin_read().unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        assert!(arena.len() >= MIN_INDEXED_ROWS);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();

        let cache = HnswIndexCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
        };
        let provider = crate::kernel::CpuScalarProvider;
        let query = [1.0, 0.0, 0.0, 0.0];
        let result = search_or_fallback(
            &access, &read_txn, "docs", &c, &arena, &slot_ids, &provider, &query, 1,
        );
        assert!(
            result.is_ok(),
            "NaN 混入コーパスは構築失敗後 brute-force へ縮退し、クエリ自体は成功する"
        );

        let stats = cache.stats();
        assert_eq!(
            stats.builds, 1,
            "失敗した構築試行も builds に計上されるべき（旧実装は 0 のままだった）"
        );
        assert_eq!(stats.build_failures, 1);
    }

    #[test]
    fn record_overlay_for_marks_uncacheable_when_oversized() {
        // Cursor Bugbot 指摘対応（PR #434「Oversized overlay skips negative
        // cache」）: `record_overlay_for` は base+overlay の合計が
        // `MAX_HNSW_CACHE_TOTAL_BYTES` を超える overlay を常駐させないだけで
        // 負のキャッシュへ記録していなかったため、次回 lookup が再び
        // `NeedOverlay` となり毎クエリ `Overlay::compute` を再実行してしまって
        // いた。`record_base`・`record_build_failed` と同じ負のキャッシュへ
        // 記録し、以後同一世代の lookup が `BuildFailedThisGeneration`
        // （overlay 再計算なし）を返すことを固定する。
        let path = unique_db_path("hnsw-cache-overlay-oversized");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let built = IndexedBase::build(&arena, crate::hnsw::HnswParams::default(), c.clone(), gen)
            .expect("build");
        let base = cache.record_base(&storage, "docs", built);
        assert_eq!(cache.stats().entries, 1);

        // `MAX_HNSW_CACHE_TOTAL_BYTES` を超える容量を持つ overlay を偽装する。
        // `Vec::with_capacity` は要素を書き込まない限り仮想メモリの予約に
        // 留まる（`len` は 0 のまま）ため、実メモリを消費せず `capacity()` の
        // みを大きくできる。
        let oversized_len = (MAX_HNSW_CACHE_TOTAL_BYTES / std::mem::size_of::<f32>()) + 1_000_000;
        let delta_vectors: Vec<f32> = Vec::with_capacity(oversized_len);
        let (visible_mask, visible_in_index) = identity_mask_and_visible(&base);
        let overlay = Arc::new(Overlay {
            generation: gen,
            arena_len: arena.len(),
            slot_of_node: (0..base.index.len() as u32).collect(),
            stale_nodes: 0,
            delta_slots: Vec::new(),
            delta_vectors,
            visible_mask,
            visible_in_index,
        });
        assert!(overlay.approx_heap_bytes() > MAX_HNSW_CACHE_TOTAL_BYTES);

        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
        };
        record_overlay_for(&access, "docs", &base, overlay);

        // 負のキャッシュ専用の新規エントリは追加しない（`record_base` が既に
        // 作った `(table, ctx)` のエントリへ `build_failed_generation` を
        // 上書きするだけ）。
        assert_eq!(cache.stats().entries, 1);

        let read_txn2 = storage.db().begin_read().unwrap();
        match cache.lookup(&storage, &read_txn2, "docs", &c) {
            Lookup::BuildFailedThisGeneration => {}
            _ => panic!(
                "oversized overlay を破棄した世代は BuildFailedThisGeneration を返し、\
                 毎クエリの Overlay::compute 再計算を避けなければならない"
            ),
        }
    }

    #[test]
    fn arena_identity_mismatch_guard_evicts_entry_and_recovers_ann_next_query() {
        // Cursor Bugbot 指摘対応（PR #434「Guard disables ANN without
        // recovery」）: `arena_identity_mismatch_guard` が検出した不整合な
        // `IndexedBase` を退避せずに brute-force へ縮退するだけだと、`lookup`
        // が同じエントリへ `NeedOverlay` を返し続け、以後の全クエリが同じ
        // ガードに当たり ANN 経路が再構築されないまま無効化され続ける。
        // dim 不整合の base を仕込んだ後、ガード経路を通った次のクエリでは
        // 再構築（builds 増加）され ANN 経路が復帰することを固定する。
        let path = unique_db_path("hnsw-cache-guard-recovery");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        create_table(&storage, "aux", 8);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        // `docs`（dim=4）に索引化対象の行数を投入する。
        let embeddings: Vec<[f32; 4]> = (0..MIN_INDEXED_ROWS)
            .map(|i| [i as f32, 0.0, 0.0, 0.0])
            .collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-guard-recovery")
                .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        // `aux`（dim=8）から構築した `IndexedBase` を、`docs` の現世代番号を
        // 偽って `(table="docs", ctx=c)` のエントリとして登録する。これは
        // 「呼び出し元の適用条件が正しく守られていれば起こらないはずの不整合」
        // （モジュールドキュメント参照）を意図的に再現するための細工であり、
        // `arena_identity_mismatch_guard` の fail-closed 検出そのものが本テストの
        // 対象。
        let read_txn = storage.db().begin_read().unwrap();
        let docs_gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let aux_arena = build_arena(&read_txn, "aux", &c);
        let mismatched_base = IndexedBase::build(
            &aux_arena,
            crate::hnsw::HnswParams::default(),
            c.clone(),
            docs_gen,
        )
        .expect("build mismatched base");
        cache.record_base(&storage, "docs", mismatched_base);
        assert_eq!(
            cache.stats().entries,
            1,
            "細工した不整合エントリが登録される"
        );

        let docs_arena = build_arena(&read_txn, "docs", &c);
        assert!(docs_arena.len() >= MIN_INDEXED_ROWS);
        assert_ne!(
            docs_arena.dim(),
            8,
            "docs アリーナと細工した base の次元が不整合であることが前提"
        );
        let slot_ids: Vec<u64> = (0..docs_arena.len() as u64).collect();

        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
        };
        let provider = crate::kernel::CpuScalarProvider;
        let query = [1.0, 0.0, 0.0, 0.0];

        // 1 回目: ガードが不整合を検出し brute-force へ縮退する。このとき
        // 不整合エントリはキャッシュから退避され、`builds` は増加しない
        // （新規構築はまだ試みていない）。
        let first = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &docs_arena,
            &slot_ids,
            &provider,
            &query,
            1,
        );
        assert!(first.is_ok(), "ガード経路でも brute-force で成功応答を返す");
        assert_eq!(
            cache.stats().entries,
            0,
            "不整合エントリはガード検出時にキャッシュから退避される"
        );
        assert_eq!(
            cache.stats().builds,
            0,
            "ガード検出のみでは新規構築を試みない"
        );

        // 2 回目: エントリが退避済みのため `lookup` は `Miss` を返し、正しい
        // dim（4）で新規構築される（ANN 経路の再構築発火）。
        let second = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &docs_arena,
            &slot_ids,
            &provider,
            &query,
            1,
        );
        assert!(second.is_ok());
        let stats_after_rebuild = cache.stats();
        assert_eq!(
            stats_after_rebuild.builds, 1,
            "退避後の次クエリで再構築が発火する（builds 増加）"
        );
        assert_eq!(
            stats_after_rebuild.entries, 1,
            "正しい dim の base が再登録される"
        );

        // 3 回目: 再構築済みの正しい base に対し、現世代のオーバーレイが
        // ない状態から `NeedOverlay` → `Ready` へ到達し、ガードは再発火せず
        // ANN 経路（索引探索）が復帰していることを確認する。
        let third = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &docs_arena,
            &slot_ids,
            &provider,
            &query,
            1,
        );
        assert!(third.is_ok());
        let stats_final = cache.stats();
        assert_eq!(
            stats_final.builds, 1,
            "3 回目は既存の正しい base を再利用し、再構築は発生しない"
        );
        assert!(
            stats_final.hits >= 1,
            "ANN 経路（索引探索・縮退なし）が復帰していることを固定する"
        );
    }
}
