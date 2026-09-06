//! `sql::exec` の SCALAR 段（`WHERE` の等価・前方一致条件）が将来（Issue #474）
//! 全行走査（O(N)）の代わりに参照する、スカラー列二次索引の**構築とテーブル世代
//! 整合キャッシュ**（Issue #473。対応 ADR: `docs/design/scalar-secondary-index.md`。
//! 親 Issue #359・#472。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-12・`docs/spec/04-behavior/rls.md`）。
//!
//! **本 Issue のスコープ**: 索引の構築・キャッシュのみ。索引を使った候補削減・
//! `ExecutionPlan` 統合・`EXPLAIN` 露出は Issue #474 が担う。本モジュールが
//! 提供する照会 API（[`ScalarIndex::candidates_for`] 等）は現時点で
//! `sql::exec::execute_statement_with_cache` から一切消費されない
//! （構築されるだけで応答には使われない。クエリ結果は本 Issue の前後で完全に
//! 不変）。
//!
//! **データモデル**（[`ScalarIndex`]）: 構築元は
//! [`crate::sql::arena_cache::SqlArenaSnapshot`]（RLS 段適用済み・ctx 可視行の
//! みを含むスナップショット）。索引のスロット番号は**このスナップショットの
//! スロット**（`snapshot.arena().ids()[slot]`／`snapshot.metadata()[slot]` の
//! 添字）であり、クエリごとに異なる SCALAR 段適用後アリーナのスロットではない
//! （#474 はスナップショット経由でこの写像を扱う）。`TEXT` 列ごとに値の辞書
//! （バイト列昇順）と、値ごとの一致スロット列（CSR: `offsets`/`slots`）・
//! 等価直引き用 `HashMap` を持つ。加えて全行 `id` が [`crate::sql::udf_call::
//! id_as_finite_scalar`] を満たす場合に限り、`id` 昇順の順序索引（`id_index`）を
//! 保持する（1 件でも `id > 2^53` があれば `None`。fail-closed。#474 が全走査へ
//! 縮退する契機になる）。`NULL` 値はいずれの索引にもエントリを作らない
//! （`declarative_filter::MetadataFilter::matches` の NULL 常時不一致と同じ
//! 判定になることが本モジュールの単体テストの不変条件）。
//!
//! **キャッシュ（[`ScalarIndexCache`]）**: キー・世代源泉・fail-closed 契約は
//! [`crate::sql::arena_cache::SqlArenaCache`]（Issue #363）と同型
//! （`(table, ctx)` 完全一致 × テーブル単位世代
//! `catalog::table_generation_in_txn`）。ただし [`ScalarIndexCache::insert`] は
//! **`SqlArenaCache::insert` とは意図的に非対称**で、`core.rs::PrefilterCache::
//! insert`（Issue #280）と同じく世代不一致・ロック毒化・世代読み取り失敗を
//! すべて `None` として扱い、キャッシュへ反映しないだけでなく呼び出し元へも
//! 一切渡さない（本索引はまだ誰にも消費されない派生データであり、`SqlArenaCache`
//! のように「このクエリの応答に限って stale でも使ってよい」対象が存在しない
//! ため。挿入失敗時は呼び出し元が単に索引なしとして扱う）。
//!
//! **fail-closed の適用範囲**: RLS 可視行のみから構築するため候補は構造的に
//! 可視集合の部分集合になる（TABLE-12・RLS 系ポインタ）。構築失敗・容量超過・
//! `id` 桁あふれはいずれも「索引なし」への縮退であり、クエリの成否・結果には
//! 影響しない（本モジュールは fail-soft な派生キャッシュ）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use redb::ReadableDatabase;

use crate::catalog::{ColumnType, TableSchema};
use crate::declarative_filter::{FilterOp, MetadataFilter};
use crate::policy::PolicyContext;
use crate::row_codec::scan_scalar_columns;
use crate::sql::arena_cache::SqlArenaSnapshot;
use crate::storage::Storage;

/// [`ScalarIndexCache`] のエントリ数上限（`sql::arena_cache::SqlArenaCache`・
/// `core.rs::PrefilterCache` と同じ DoS 対策方針を踏襲する）。
const MAX_SCALAR_INDEX_CACHE_ENTRIES: usize = 32;

/// [`ScalarIndexCache`] が保持する索引群の概算バイト量の合計上限
/// （`crate::arena::MAX_ARENA_TOTAL_BYTES` と同じ桁に揃える）。
const MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// 単体の [`ScalarIndex`] が超えてはならない概算バイト量上限。総量上限と同じ値を
/// 採用する（1 テーブル分の索引が総予算を単独で使い切る事態を許すが、複数
/// テーブル・複数 ctx 分を同時に常駐させない選択と両立する。`core.rs::
/// PrefilterCache::insert` の単体上限判定と同じ考え方）。
const MAX_SCALAR_INDEX_BYTES: usize = MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES;

/// [`ScalarIndex::build`] の失敗要因。いずれも呼び出し元（`sql::exec`）が
/// 「索引なし」へ縮退する契機として扱うのみで、クエリ自体を失敗させない
/// （モジュールドキュメント参照）。
#[derive(Debug)]
pub(crate) enum ScalarIndexBuildError {
    /// スロット番号が `u32` に収まらない（[`crate::arena::MAX_ARENA_ROWS`] は
    /// `u32::MAX` 未満のため通常到達しないが、多層防御として検査する）。
    SlotOverflow,
    /// untrusted な行 metadata のデコードに失敗した（`scan_scalar_columns` が
    /// 検出する presence タグ不正・宣言長超過・UTF-8 不正等）。
    RowDecode(crate::row_codec::RowCodecError),
    /// アロケーション失敗（`try_reserve` 系）。
    AllocationFailed,
    /// 構築結果の概算バイト量が [`MAX_SCALAR_INDEX_BYTES`] を超えた。
    TooLarge,
}

impl std::fmt::Display for ScalarIndexBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlotOverflow => write!(f, "scalar index slot count exceeds u32 range"),
            Self::RowDecode(e) => write!(f, "scalar index row decode failed: {e}"),
            Self::AllocationFailed => write!(f, "scalar index allocation failed"),
            Self::TooLarge => write!(
                f,
                "scalar index approx size exceeds limit {MAX_SCALAR_INDEX_BYTES}"
            ),
        }
    }
}

impl std::error::Error for ScalarIndexBuildError {}

/// 1 つの `TEXT` 列に対する索引（等価直引き＋前方一致範囲走査の両方を支える
/// 共有データ構造。モジュールドキュメント「データモデル」参照）。
///
/// `values`（重複排除・バイト列昇順の辞書）・`offsets`（CSR。`values[i]` の
/// 一致スロットは `slots[offsets[i]..offsets[i+1]]`。長さは `values.len() + 1`）・
/// `slots`（値ごとにスロット昇順）・`equality`（値 → `values` の添字。等価述語の
/// O(1) 直引き用）を持つ。
struct TextColumnIndex {
    values: Vec<String>,
    offsets: Vec<u32>,
    slots: Vec<u32>,
    equality: HashMap<String, u32>,
}

impl TextColumnIndex {
    /// `value_index`（`values`/`equality` の添字）に対応するスロット列（昇順）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn slots_for_value_index(&self, value_index: u32) -> Option<&[u32]> {
        let vi = usize::try_from(value_index).ok()?;
        let start = *self.offsets.get(vi)?;
        let end = *self.offsets.get(vi.checked_add(1)?)?;
        let start = usize::try_from(start).ok()?;
        let end = usize::try_from(end).ok()?;
        self.slots.get(start..end)
    }

    /// `prefix` に前方一致する全ての値のスロット列を連結して返す（値をまたぐと
    /// 昇順であることは保証しない。呼び出し元がソートする契約。モジュール
    /// ドキュメント「データモデル」・[`ScalarIndex::candidates_for`] 参照）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn prefix_slots(&self, prefix: &str) -> Vec<u32> {
        // `values` はバイト列昇順の辞書のため、`prefix` 自身が最初に現れうる
        // 位置を二分探索で求め、そこから前方一致が途切れるまで線形に辿る
        // （辞書順では前方一致する値は連続する）。
        let start = self.values.partition_point(|v| v.as_str() < prefix);
        let mut out = Vec::new();
        for (idx, value) in self.values.iter().enumerate().skip(start) {
            if !value.starts_with(prefix) {
                break;
            }
            let value_index = match u32::try_from(idx) {
                Ok(v) => v,
                Err(_) => break,
            };
            if let Some(s) = self.slots_for_value_index(value_index) {
                out.extend_from_slice(s);
            }
        }
        out
    }

    /// この列が保持する文字列実体・補助配列の概算ヒープバイト量。
    fn approx_heap_bytes(&self) -> usize {
        let values_bytes: usize = self
            .values
            .iter()
            .map(|v| v.len().saturating_add(std::mem::size_of::<String>()))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let offsets_bytes = self
            .offsets
            .len()
            .saturating_mul(std::mem::size_of::<u32>());
        let slots_bytes = self.slots.len().saturating_mul(std::mem::size_of::<u32>());
        // `HashMap` のキーは `values` と同じ文字列を複製保持する（`equality`
        // が値 → 添字の直引き専用であり `values` への参照を持たないため）。
        let equality_bytes: usize = self
            .equality
            .keys()
            .map(|k| k.len().saturating_add(std::mem::size_of::<(String, u32)>()))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        values_bytes
            .saturating_add(offsets_bytes)
            .saturating_add(slots_bytes)
            .saturating_add(equality_bytes)
    }
}

/// [`crate::sql::arena_cache::SqlArenaSnapshot`] から構築するスカラー列二次索引
/// 本体（モジュールドキュメント「データモデル」参照）。
pub(crate) struct ScalarIndex {
    built_ctx: PolicyContext,
    built_table_generation: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    row_count: usize,
    /// `schema.columns` と同じ長さ・順序。`TEXT` 列のみ `Some`。
    columns: Vec<Option<TextColumnIndex>>,
    /// `id` 昇順（同一 `id` 内はスロット昇順）に整列した `(id, slot)`。全行が
    /// `id_as_finite_scalar` を満たす場合のみ `Some`（1 件でも `id > 2^53` が
    /// あれば `None`。モジュールドキュメント参照）。
    id_index: Option<Vec<(u64, u32)>>,
}

/// `TextColumnIndex` 構築時の並び替えキー: バイト列昇順、同値はスロット昇順。
/// スロット（`u32`。同一列内で重複しない）が全順序の明示的タイブレークを
/// 提供するため、この比較関数のもとで同値ペアは存在しない（決定的ソート）。
fn cmp_value_then_slot(a: &(String, u32), b: &(String, u32)) -> std::cmp::Ordering {
    a.0.as_bytes().cmp(b.0.as_bytes()).then(a.1.cmp(&b.1))
}

impl ScalarIndex {
    /// `schema`（対象テーブルのスキーマ）と `snapshot`（RLS 段適用済みスナップ
    /// ショット）から索引を構築する。`snapshot` の各スロットを 1 回だけ走査し、
    /// untrusted な行 metadata のデコード検証は [`scan_scalar_columns`] が
    /// 一切弱めずに行う（`.claude/rules/coding-rust.md`「untrusted 入力の
    /// 扱い」）。
    pub(crate) fn build(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
    ) -> Result<Self, ScalarIndexBuildError> {
        let row_count = snapshot.arena().len();
        let column_count = schema.columns.len();

        // 列ごとに (value, slot) を蓄積する作業領域（`TEXT` 列のみ `Some`）。
        let mut per_column: Vec<Option<Vec<(String, u32)>>> = Vec::new();
        per_column
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        for column in &schema.columns {
            match column.ty {
                ColumnType::Text => per_column.push(Some(Vec::new())),
                ColumnType::Vector(_) => per_column.push(None),
            }
        }

        for slot in 0..row_count {
            let slot_u32 = u32::try_from(slot).map_err(|_| ScalarIndexBuildError::SlotOverflow)?;
            let metadata = snapshot
                .metadata()
                .get(slot)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let scanned =
                scan_scalar_columns(schema, metadata).map_err(ScalarIndexBuildError::RowDecode)?;
            for (col_index, value) in scanned.into_iter().enumerate() {
                let Some(v) = value else { continue };
                if let Some(Some(acc)) = per_column.get_mut(col_index) {
                    acc.try_reserve(1)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    acc.push((v.to_string(), slot_u32));
                }
            }
        }

        let mut columns: Vec<Option<TextColumnIndex>> = Vec::new();
        columns
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        for entries in per_column {
            match entries {
                None => columns.push(None),
                Some(mut pairs) => {
                    pairs.sort_unstable_by(cmp_value_then_slot); // sort-determinism: allow キーは (バイト列, スロット昇順) のタプルでスロットが全順序の明示的タイブレーク
                    let mut values: Vec<String> = Vec::new();
                    let mut offsets: Vec<u32> = Vec::new();
                    let mut slots: Vec<u32> = Vec::new();
                    let mut equality: HashMap<String, u32> = HashMap::new();
                    offsets.push(0);
                    let mut iter = pairs.into_iter().peekable();
                    while let Some((value, slot)) = iter.next() {
                        slots
                            .try_reserve(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        slots.push(slot);
                        while iter
                            .peek()
                            .is_some_and(|(next_value, _)| next_value == &value)
                        {
                            // untrusted な行 metadata 由来の値列に対する走査のため
                            // `unwrap`/`expect` を使わない（`.claude/rules/
                            // coding-rust.md`）。直前の `is_some_and` で `Some` を
                            // 確認済みだが、`if let` で明示的に分岐しコード上も
                            // 添字アクセス相当を避ける。
                            let Some((_, next_slot)) = iter.next() else {
                                break;
                            };
                            slots
                                .try_reserve(1)
                                .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                            slots.push(next_slot);
                        }
                        let value_index = u32::try_from(values.len())
                            .map_err(|_| ScalarIndexBuildError::SlotOverflow)?;
                        equality
                            .try_reserve(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        equality.insert(value.clone(), value_index);
                        values
                            .try_reserve_exact(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        values.push(value);
                        let slots_len = u32::try_from(slots.len())
                            .map_err(|_| ScalarIndexBuildError::SlotOverflow)?;
                        offsets
                            .try_reserve_exact(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        offsets.push(slots_len);
                    }
                    columns.push(Some(TextColumnIndex {
                        values,
                        offsets,
                        slots,
                        equality,
                    }));
                }
            }
        }

        // `id` 順序索引: 1 件でも `id_as_finite_scalar` に失敗する行があれば
        // 索引全体を `None` にする（fail-closed。モジュールドキュメント参照）。
        let mut id_index: Option<Vec<(u64, u32)>> = None;
        'id_index: {
            let mut pairs: Vec<(u64, u32)> = Vec::new();
            if pairs.try_reserve_exact(row_count).is_err() {
                break 'id_index;
            }
            for (slot, &id) in snapshot.arena().ids().iter().enumerate() {
                if crate::sql::udf_call::id_as_finite_scalar(id).is_err() {
                    break 'id_index;
                }
                let Ok(slot_u32) = u32::try_from(slot) else {
                    break 'id_index;
                };
                pairs.push((id, slot_u32));
            }
            pairs.sort_unstable();
            id_index = Some(pairs);
        }

        let built = Self {
            built_ctx: snapshot.built_ctx_for_index().clone(),
            built_table_generation: snapshot.built_table_generation_for_index(),
            row_count,
            columns,
            id_index,
        };
        if built.approx_heap_bytes() > MAX_SCALAR_INDEX_BYTES {
            return Err(ScalarIndexBuildError::TooLarge);
        }
        Ok(built)
    }

    /// 等価述語向け直引き（列が `TEXT` でない・未知の列は `None`。値が辞書に
    /// 無い場合は `None`。存在有無を区別しない照会は [`Self::candidates_for`]
    /// を使う）。
    #[cfg(test)]
    fn candidates_equals(&self, column_index: usize, value: &str) -> Option<&[u32]> {
        let column = self.columns.get(column_index)?.as_ref()?;
        let value_index = *column.equality.get(value)?;
        column.slots_for_value_index(value_index)
    }

    /// [`MetadataFilter`] を評価し、一致スロットの**昇順** `Vec<u32>` を返す。
    /// 列が `TEXT` でない・未知の列は `None`。一致 0 件（列は索引済みだが値が
    /// 存在しない）は `Some(vec![])` を返す（`None` と区別する）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn candidates_for(&self, filter: &MetadataFilter) -> Option<Vec<u32>> {
        let column = self.columns.get(filter.column_index())?.as_ref()?;
        let mut result = match filter.op() {
            FilterOp::Equals(value) => column
                .equality
                .get(value)
                .and_then(|&vi| column.slots_for_value_index(vi))
                .map(|s| s.to_vec())
                .unwrap_or_default(),
            FilterOp::StartsWith(prefix) => column.prefix_slots(prefix),
        };
        result.sort_unstable();
        Some(result)
    }

    /// `id` に対する範囲述語（単純比較）向け照会。`id_index` が `None`
    /// （`id > 2^53` を含む行がある）の場合は `None`（呼び出し元が全走査へ
    /// 縮退する契機。モジュールドキュメント参照）。戻り値は昇順 `Vec<u32>`。
    #[cfg(test)]
    fn candidates_id_range(
        &self,
        lower: std::ops::Bound<u64>,
        upper: std::ops::Bound<u64>,
    ) -> Option<Vec<u32>> {
        use std::ops::Bound;
        let idx = self.id_index.as_ref()?;
        let in_lower = |id: u64| match lower {
            Bound::Included(l) => id >= l,
            Bound::Excluded(l) => id > l,
            Bound::Unbounded => true,
        };
        let in_upper = |id: u64| match upper {
            Bound::Included(u) => id <= u,
            Bound::Excluded(u) => id < u,
            Bound::Unbounded => true,
        };
        let mut out: Vec<u32> = idx
            .iter()
            .filter(|(id, _)| in_lower(*id) && in_upper(*id))
            .map(|(_, slot)| *slot)
            .collect();
        out.sort_unstable();
        Some(out)
    }

    /// 索引全体（`TEXT` 列の辞書・CSR・`equality`・`id_index`）の概算ヒープ
    /// バイト量（容量判定用）。
    fn approx_heap_bytes(&self) -> usize {
        let columns_bytes: usize = self
            .columns
            .iter()
            .filter_map(|c| c.as_ref())
            .map(TextColumnIndex::approx_heap_bytes)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let id_index_bytes = self
            .id_index
            .as_ref()
            .map(|v| v.len().saturating_mul(std::mem::size_of::<(u64, u32)>()))
            .unwrap_or(0);
        columns_bytes.saturating_add(id_index_bytes)
    }

    #[cfg(test)]
    fn row_count(&self) -> usize {
        self.row_count
    }

    fn built_table_generation(&self) -> u64 {
        self.built_table_generation
    }
}

/// [`ScalarIndexCache`] の観測用統計。テナント ID・行 ID・値等の機微情報は一切
/// 含まない（`core.rs::PrefilterCacheStats` と同じ方針）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScalarIndexCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stale_evictions: u64,
    pub capacity_evictions: u64,
    pub builds: u64,
    pub build_failures: u64,
    pub entries: usize,
}

struct ScalarIndexCacheEntry {
    table: String,
    index: Arc<ScalarIndex>,
    last_used: u64,
}

#[derive(Default)]
struct ScalarIndexCacheState {
    entries: Vec<ScalarIndexCacheEntry>,
}

/// `(table, ctx)` × テーブル単位世代でキャッシュする [`ScalarIndex`] キャッシュ
/// 本体（モジュールドキュメント「キャッシュ」参照）。
pub(crate) struct ScalarIndexCache {
    state: RwLock<ScalarIndexCacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
    builds: AtomicU64,
    build_failures: AtomicU64,
}

impl ScalarIndexCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(ScalarIndexCacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
            builds: AtomicU64::new(0),
            build_failures: AtomicU64::new(0),
        }
    }

    /// `(table, ctx)` に一致し、`read_txn` のスナップショットにおけるテーブル
    /// 世代と整合するエントリを探す。契約は
    /// [`crate::sql::arena_cache::SqlArenaCache::lookup`] と同一（`read_txn` が
    /// 古いだけの可能性があるため、破棄は `storage` から読んだ真の最新世代より
    /// 厳密に古いと確認できた場合のみ行う）。
    pub(crate) fn lookup(
        &self,
        storage: &Storage,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<Arc<ScalarIndex>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.state.write().ok()?;
        let current_generation = crate::catalog::table_generation_in_txn(read_txn, table).ok()?;
        let position = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.index.built_ctx == *ctx)?;
        let built_generation = guard.entries.get(position)?.index.built_table_generation();
        if built_generation == current_generation {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            let index = Arc::clone(&entry.index);
            drop(guard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(index);
        }
        let Ok(true_current_generation) = storage.table_generation(table) else {
            return None;
        };
        if built_generation < true_current_generation {
            guard.entries.remove(position);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
        }
        None
    }

    /// 新規構築した索引を挿入する。**`SqlArenaCache::insert` とは意図的に非対称**
    /// （モジュールドキュメント「キャッシュ」参照）: 世代不一致・ロック毒化・
    /// 世代読み取り失敗のいずれも `None` を返し、キャッシュへ反映しないだけで
    /// なく呼び出し元へも一切渡さない（`core.rs::PrefilterCache::insert`・
    /// Issue #280 と同じ契約）。
    pub(crate) fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        index: ScalarIndex,
    ) -> Option<Arc<ScalarIndex>> {
        let index = Arc::new(index);
        self.builds.fetch_add(1, Ordering::Relaxed);

        let mut guard = self.state.write().ok()?;
        let Ok(read_txn) = storage.db().begin_read() else {
            return None;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table)
        else {
            return None;
        };
        if index.built_table_generation() != current_generation {
            return None;
        }

        let own_bytes = index.approx_heap_bytes();

        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.index.built_ctx == *ctx)
        {
            guard.entries.remove(pos);
        }

        let before = guard.entries.len();
        guard
            .entries
            .retain(|e| e.table != table || e.index.built_table_generation() == current_generation);
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        if own_bytes > MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超える索引は常駐させないが、世代整合済みなので
            // 呼び出し元へは `Some` で返す（この 1 回のクエリ限りで使ってよい。
            // `SqlArenaCache::insert` と同じ「単体超過時の縮退」方針）。
            return Some(index);
        }

        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.index.approx_heap_bytes())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_SCALAR_INDEX_CACHE_ENTRIES
            || total_bytes.saturating_add(own_bytes) > MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return Some(index);
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.index.approx_heap_bytes());
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(ScalarIndexCacheEntry {
            table: table.to_string(),
            index: Arc::clone(&index),
            last_used: seq,
        });
        Some(index)
    }

    pub(crate) fn stats(&self) -> ScalarIndexCacheStats {
        self.misses.fetch_add(0, Ordering::Relaxed); // no-op（misses は現状 insert 起点で数えない。将来 lookup 側で加算する余地を残す）
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        ScalarIndexCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            builds: self.builds.load(Ordering::Relaxed),
            build_failures: self.build_failures.load(Ordering::Relaxed),
            entries,
        }
    }

    /// [`ScalarIndex::build`] が失敗したことを観測用統計へ計上する
    /// （`sql::exec::execute_statement_with_cache` の gated 構築が呼ぶ。
    /// モジュールドキュメント「fail-closed の適用範囲」参照）。
    pub(crate) fn record_build_failure(&self) {
        self.build_failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// `sql::exec::execute_statement_with_cache` へ渡すキャッシュアクセス束
/// （`sql::arena_cache::ArenaCacheAccess`・`sql::sparse_cache::SparseCacheAccess`
/// と同じ理由・構造）。
pub(crate) struct ScalarCacheAccess<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) cache: &'a ScalarIndexCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::VectorArena;
    use crate::catalog::ColumnDef;
    use crate::recovery::required_op_id::OperationId;
    use crate::row_codec::Value;
    use crate::storage::Visibility;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use std::ops::Bound;

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant")
    }

    fn op_id(label: &str) -> OperationId {
        OperationId::parse(label).expect("valid operation id")
    }

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("kind", ColumnType::Text, true),
                ColumnDef::new("path", ColumnType::Text, true),
            ],
        )
    }

    fn create_table(storage: &Storage) {
        storage.create_table(&schema()).expect("create table");
    }

    fn insert(
        storage: &Storage,
        ctx: &PolicyContext,
        id: u64,
        kind: Option<&str>,
        path: Option<&str>,
        visibility: Visibility,
    ) {
        crate::tenant::insert_typed_row(
            storage,
            "docs",
            ctx,
            id,
            visibility,
            &[
                Value::Vector(vec![0.0, 0.0]),
                kind.map(|k| Value::Text(k.to_string()))
                    .unwrap_or(Value::Null),
                path.map(|p| Value::Text(p.to_string()))
                    .unwrap_or(Value::Null),
            ],
            &op_id(&format!("seed-{id}")),
        )
        .expect("insert row");
    }

    /// テストのみに公開するアクセサ（`SqlArenaSnapshot` は crate 内部型のため、
    /// 索引側から世代・ctx を取り出す薄い橋渡し）。
    fn snapshot_from(
        storage: &Storage,
        ctx: &PolicyContext,
    ) -> (crate::sql::arena_cache::SqlArenaSnapshot, TableSchema) {
        let read_txn = storage.db().begin_read().expect("begin read");
        let schema = crate::catalog::get_table_schema_in_txn(&read_txn, "docs").expect("schema");
        let expected_dim = schema.vector_dim().expect("vector dim");
        let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
            expected_dim,
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        );
        let hook = crate::rls::ImplicitRlsHook::new(ctx);
        let mut rls_capture = |id: u64,
                               tenant_id: &str,
                               visibility,
                               embedding: &[f32],
                               metadata: &[u8],
                               response_arena_bytes_in_use: usize|
         -> std::result::Result<(), crate::arena::ArenaError> {
            capture.push(
                id,
                tenant_id,
                visibility,
                embedding,
                metadata,
                response_arena_bytes_in_use,
            );
            Ok(())
        };
        let _built: VectorArena =
            crate::arena::VectorArena::build_filtered_with_rows_in_txn_capturing(
                &read_txn,
                "docs",
                hook.predicate(),
                |_, _, _, _| Ok(true),
                &mut rls_capture,
            )
            .expect("build arena");
        let table_generation =
            crate::catalog::table_generation_in_txn(&read_txn, "docs").expect("table generation");
        let (cache_arena, cache_metadata) = capture.finish("docs").expect("capture snapshot");
        (
            crate::sql::arena_cache::SqlArenaSnapshot::new(
                cache_arena,
                cache_metadata,
                ctx.clone(),
                table_generation,
            ),
            schema,
        )
    }

    #[test]
    fn build_indexes_equality_and_prefix_matching_matches_all_oracle() {
        let path = unique_db_path("scalar-index-basic");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(
            &storage,
            &ctx_a,
            1,
            Some("alpha"),
            Some("src/a.rs"),
            Visibility::Public,
        );
        insert(
            &storage,
            &ctx_a,
            2,
            Some("beta"),
            Some("src/b.rs"),
            Visibility::Public,
        );
        insert(&storage, &ctx_a, 3, Some("alpha"), None, Visibility::Public);
        insert(
            &storage,
            &ctx_a,
            4,
            None,
            Some("src/ab.rs"),
            Visibility::Public,
        );

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert_eq!(index.row_count(), 4);

        let kind_col = 1;
        let path_col = 2;

        let eq_alpha = index
            .candidates_equals(kind_col, "alpha")
            .expect("alpha indexed");
        assert_eq!(eq_alpha.len(), 2);

        let filters = declarative_filter_all(&schema);
        for filter in &filters {
            let expected = oracle_matches(&schema, &snapshot, filter);
            let actual = index.candidates_for(filter).expect("indexed column");
            assert_eq!(
                actual, expected,
                "filter {filter:?} must match full-scan oracle"
            );
        }

        let prefix_filter = MetadataFilter_starts_with(&schema, "path", "src/a");
        let expected = oracle_matches(&schema, &snapshot, &prefix_filter);
        let actual = index
            .candidates_for(&prefix_filter)
            .expect("indexed column");
        assert_eq!(actual, expected);

        // 存在しない値・NULL 列は空集合。
        let missing = index.candidates_for(&MetadataFilter_equals(&schema, "kind", "gamma"));
        assert_eq!(missing, Some(Vec::new()));
        let _ = path_col;
    }

    #[test]
    fn null_values_never_appear_in_any_index_entry() {
        let path = unique_db_path("scalar-index-null");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, None, None, Visibility::Public);
        insert(&storage, &ctx_a, 2, Some("x"), None, Visibility::Public);

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let eq_x = index.candidates_for(&MetadataFilter_equals(&schema, "kind", "x"));
        assert_eq!(eq_x, Some(vec![1]));
    }

    #[test]
    fn rls_partial_visibility_excludes_private_rows_from_dictionary() {
        let path = unique_db_path("scalar-index-rls");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        // `ctx_b` は自テナントの Private 行も見える構成にする（`PolicyContext::new`
        // は Public のみ可視の既定コンストラクタのため、Private 行を検証するには
        // `with_visibilities` で明示的に許可する必要がある）。
        let ctx_b =
            PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
                .expect("valid tenant with visibilities");
        insert(
            &storage,
            &ctx_a,
            1,
            Some("A-PRIVATE-1"),
            None,
            Visibility::Private,
        );
        insert(
            &storage,
            &ctx_b,
            2,
            Some("B-PRIVATE-1"),
            None,
            Visibility::Private,
        );
        insert(
            &storage,
            &ctx_b,
            3,
            Some("B-PUBLIC-1"),
            None,
            Visibility::Public,
        );

        let (snapshot, schema) = snapshot_from(&storage, &ctx_b);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        // ctx_b（自テナント Private+Public 可視）は自身の private を含み他テナントの
        // private は見えない契約（RLS-1〜4 相当）。A の private 行が索引の辞書へ
        // 混入しないことを確認する。
        assert_eq!(
            index.candidates_for(&MetadataFilter_equals(&schema, "kind", "A-PRIVATE-1")),
            Some(Vec::new()),
            "other tenant's private row must not leak into this ctx's dictionary"
        );
        assert!(index
            .candidates_for(&MetadataFilter_equals(&schema, "kind", "B-PRIVATE-1"))
            .map(|v| !v.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn empty_table_builds_empty_index() {
        let path = unique_db_path("scalar-index-empty");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert_eq!(index.row_count(), 0);
        assert_eq!(
            index.candidates_for(&MetadataFilter_equals(&schema, "kind", "anything")),
            Some(Vec::new())
        );
    }

    #[test]
    fn id_index_is_none_when_any_id_exceeds_exact_f64_range() {
        let path = unique_db_path("scalar-index-id-overflow");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);
        insert(
            &storage,
            &ctx_a,
            (1u64 << 53) + 1,
            Some("y"),
            None,
            Visibility::Public,
        );
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert!(index
            .candidates_id_range(Bound::Unbounded, Bound::Unbounded)
            .is_none());
    }

    #[test]
    fn id_index_supports_range_queries_when_all_ids_in_range() {
        let path = unique_db_path("scalar-index-id-range");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        for id in [1u64, 5, 10, 20] {
            insert(&storage, &ctx_a, id, Some("x"), None, Visibility::Public);
        }
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let ids_in_range = |lower, upper| {
            let slots = index.candidates_id_range(lower, upper).expect("id index");
            let mut ids: Vec<u64> = slots
                .iter()
                .map(|&s| snapshot.arena().ids()[s as usize])
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(
            ids_in_range(Bound::Included(5), Bound::Included(10)),
            vec![5, 10]
        );
        assert_eq!(
            ids_in_range(Bound::Excluded(5), Bound::Unbounded),
            vec![10, 20]
        );
    }

    // ---------- キャッシュ契約テスト（`arena_cache.rs`・`core.rs::PrefilterCache`
    // のテストを雛形に。世代整合・非対称 insert 契約を固定する） ----------

    #[test]
    fn cache_hits_same_generation_and_evicts_on_write() {
        let path = unique_db_path("scalar-index-cache-basic");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let inserted = cache
            .insert(&storage, "docs", &ctx_a, index)
            .expect("insert must succeed on fresh generation");
        assert_eq!(cache.stats().entries, 1);

        let read_txn = storage.db().begin_read().expect("begin read");
        let hit = cache
            .lookup(&storage, &read_txn, "docs", &ctx_a)
            .expect("lookup must hit same generation");
        assert!(Arc::ptr_eq(&hit, &inserted));
        assert_eq!(cache.stats().hits, 1);

        // 書き込みで世代を進めるとミスになり、stale eviction が発生する。
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);
        let read_txn2 = storage.db().begin_read().expect("begin read");
        let miss = cache.lookup(&storage, &read_txn2, "docs", &ctx_a);
        assert!(miss.is_none());
        assert_eq!(cache.stats().stale_evictions, 1);
    }

    #[test]
    fn insert_returns_none_on_generation_conflict() {
        let path = unique_db_path("scalar-index-cache-conflict");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();
        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        // 構築後、挿入前に別の書き込みで世代を進める（並行書き込みを模す）。
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);

        let result = cache.insert(&storage, "docs", &ctx_a, index);
        assert!(
            result.is_none(),
            "stale insert must be rejected (None), not returned to caller"
        );
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn cache_key_separates_by_ctx() {
        let path = unique_db_path("scalar-index-cache-ctx");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        let ctx_b = ctx("tenant-b");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);

        let cache = ScalarIndexCache::new();
        let (snapshot_a, schema) = snapshot_from(&storage, &ctx_a);
        let index_a = ScalarIndex::build(&schema, &snapshot_a).expect("build index a");
        cache
            .insert(&storage, "docs", &ctx_a, index_a)
            .expect("insert a");

        let read_txn = storage.db().begin_read().expect("begin read");
        assert!(cache.lookup(&storage, &read_txn, "docs", &ctx_b).is_none());
        assert_eq!(cache.stats().entries, 1);
    }

    // ---------- 補助（テスト専用のフィルタ構築ヘルパ） ----------

    #[allow(non_snake_case)]
    fn MetadataFilter_equals(schema: &TableSchema, column: &str, value: &str) -> MetadataFilter {
        crate::declarative_filter::DeclarativeFilter::equals(column, value)
            .bind(schema)
            .expect("bind equals filter")
    }

    #[allow(non_snake_case)]
    fn MetadataFilter_starts_with(
        schema: &TableSchema,
        column: &str,
        prefix: &str,
    ) -> MetadataFilter {
        crate::declarative_filter::DeclarativeFilter::starts_with(column, prefix)
            .bind(schema)
            .expect("bind starts_with filter")
    }

    fn declarative_filter_all(schema: &TableSchema) -> Vec<MetadataFilter> {
        vec![
            MetadataFilter_equals(schema, "kind", "alpha"),
            MetadataFilter_equals(schema, "kind", "beta"),
            MetadataFilter_starts_with(schema, "path", "src/"),
        ]
    }

    /// 全スロットを `scan_scalar_columns` ＋ `MetadataFilter::matches` で走査する
    /// オラクル（索引と完全一致することを検証する対照実装）。
    fn oracle_matches(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
        filter: &MetadataFilter,
    ) -> Vec<u32> {
        let mut out = Vec::new();
        for slot in 0..snapshot.arena().len() {
            let metadata = snapshot
                .metadata()
                .get(slot)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let scanned = scan_scalar_columns(schema, metadata).expect("decode row");
            let value = scanned.get(filter.column_index()).copied().flatten();
            if filter.matches(value) {
                out.push(slot as u32);
            }
        }
        out
    }
}
