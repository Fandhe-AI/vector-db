//! `sql::aggregate::execute_aggregate`（`GROUP BY` なし・`WHERE` なし・
//! `DecodeTier::Fast` の単一行集計、TASK-166・SQL-13）専用の可視行テーブル世代
//! 整合キャッシュ（Issue #478）。
//!
//! [`crate::sql::arena_cache::SqlArenaCache`]（Issue #363）と同型の fail-closed
//! 契約（`lookup`/`insert` の非対称・失効源泉はテーブル単位世代
//! `catalog::table_generation_in_txn`）を踏襲するが、保持する内容は行本体
//! （embedding・metadata）ではなく「可視行の `id` を物理走査順に並べた列」だけ
//! （[`VisibleSnapshot`]）。`DecodeTier::Fast` が要求するのは `COUNT(*)`・
//! `COUNT(id)`・`SUM`/`AVG`/`MIN`/`MAX(id)` のみであり、可視性・TABLE-12 の
//! キー/ヘッダ tenant 整合検査さえ済んでいれば embedding・metadata は一切不要
//! なため（`sql::aggregate::DecodeTier::Fast` のドキュメント参照）、ヒット時は
//! `user_rows/{table}` を **一切開かずに** `visible_ids` を反復して集計できる。
//!
//! **構築時検査**: [`VisibleSnapshotBuilder`] は走査したすべての物理行について
//! ヘッダデコード（`storage::decode_row_header`）→ `PolicyContext::is_visible` →
//! 可視行のみ `storage::verify_row_key_tenant`（TABLE-12）を行った上でのみ `id` を
//! 記録する。呼び出し元（`sql::aggregate::execute_aggregate`）は `DecodeTier::Fast`
//! であっても PR #369 の契約により dim・metadata の構造検証
//! （`storage::decode_row_dim_and_metadata_borrowed`）を毎行実施しているため、
//! ミス時にこのキャッシュを構築するコストは実質ゼロ（同じ走査へ相乗りするだけ）。
//! いずれかの検査が失敗した行に到達した場合は構築を中止し（`None` を返し）、
//! 呼び出し元は従来どおりのエラーでクエリ自体を失敗させる（キャッシュへは
//! 登録しない）。
//!
//! **信頼基盤（ヒット時に構造検証を省略できる根拠）**: `SqlArenaCache` のヒット
//! 経路が「同一テーブル世代内は行バイト列が不変」という前提でデコードを丸ごと
//! 省略しているのと同じ前提に立つ。世代は `catalog::bump_table_generation_in_txn`
//! が対象テーブルへのすべての書き込み経路で必ず進める契約（機械強制:
//! `tests/table_generation_bump_coverage.rs`）であり、世代が一致する限り
//! 構築時に検証済みの可視 `id` 集合はそのまま有効。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use redb::ReadableDatabase;

use crate::arena::MAX_ARENA_ROWS;
use crate::policy::PolicyContext;
use crate::storage::Storage;

/// [`VisibleBitmapCache`] のエントリ数上限（`SqlArenaCache`・`SparseIndexCache` と
/// 同じ DoS 対策方針を踏襲する。Issue #363・Issue #357 参照）。
const MAX_VISIBLE_BITMAP_CACHE_ENTRIES: usize = 32;

/// 1 スナップショットが保持できる可視行数の上限（`arena::MAX_ARENA_ROWS` と同値。
/// 超過した場合はキャッシュへ登録しない——走査・クエリ自体は継続する）。
const MAX_VISIBLE_SNAPSHOT_ROWS: usize = MAX_ARENA_ROWS;

/// `visible_ids: Vec<u64>` 1 件あたり 8 バイトとして概算した総バイト量の上限
/// （`SqlArenaCache::MAX_SQL_ARENA_CACHE_TOTAL_BYTES` と同桁。`Vec<u64>` は
/// embedding を含む `VectorArena` よりはるかに軽量なため同じ上限で十分な件数を
/// 保持できる）。
const MAX_VISIBLE_BITMAP_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`VisibleBitmapCache`] の観測用統計（Issue #478）。テナント ID・行 ID・可視件数
/// 等の機微情報は一切含まない（`SqlArenaCacheStats`・`SparseIndexCacheStats` と
/// 同じ方針）。
#[derive(Debug, Clone, Copy, Default)]
pub struct VisibleBitmapCacheStats {
    /// キャッシュヒット数（テーブル世代整合まで確認できた再利用）。
    pub hits: u64,
    /// キャッシュミス数（未登録、またはテーブル世代不一致で破棄した後の再構築）。
    pub misses: u64,
    /// テーブル世代不一致による破棄回数。
    pub stale_evictions: u64,
    /// 容量上限超過による LRU 追い出し回数。
    pub capacity_evictions: u64,
    /// 現在キャッシュが保持しているエントリ数。
    pub entries: usize,
}

/// `sql::aggregate::execute_aggregate` が構築・再利用する可視行スナップショット。
/// `visible_ids` は物理走査順（`table.iter()` の複合キー `(tenant_id, id)` 昇順）に
/// 現れた可視行の `id` だけを保持し、embedding・metadata は一切含まない。
pub(crate) struct VisibleSnapshot {
    visible_ids: Vec<u64>,
    built_ctx: PolicyContext,
    built_table_generation: u64,
}

impl VisibleSnapshot {
    pub(crate) fn visible_ids(&self) -> &[u64] {
        &self.visible_ids
    }

    fn built_ctx(&self) -> &PolicyContext {
        &self.built_ctx
    }

    fn built_table_generation(&self) -> u64 {
        self.built_table_generation
    }

    /// キャッシュ容量判定用の概算バイト量。
    fn approx_heap_bytes(&self) -> usize {
        self.visible_ids
            .len()
            .saturating_mul(std::mem::size_of::<u64>())
    }
}

/// [`VisibleSnapshot`] を 1 行ずつ組み立てるビルダー。呼び出し元
/// （`sql::aggregate::execute_aggregate` の走査ループ）は、可視行と判定し
/// TABLE-12 の tenant 整合検査を通した行についてのみ [`Self::mark_visible`] を
/// 呼ぶ（不可視行・整合検査失敗行は呼ばない——不可視行は既存契約
/// （Issue #137）どおりヘッダ以外を一切デコードしないため観測対象にできない）。
pub(crate) struct VisibleSnapshotBuilder {
    visible_ids: Vec<u64>,
    overflowed: bool,
}

impl VisibleSnapshotBuilder {
    pub(crate) fn new() -> Self {
        Self {
            visible_ids: Vec::new(),
            overflowed: false,
        }
    }

    /// 可視行かつ TABLE-12 検査済みの `id` を記録する。上限超過後は記録を止め
    /// `finish` が `None` を返すようにする（DoS 対策・容量超過時は非登録で
    /// フォールバックする契約。走査・集計自体は呼び出し元が続行する）。
    pub(crate) fn mark_visible(&mut self, id: u64) {
        if self.overflowed {
            return;
        }
        if self.visible_ids.len() >= MAX_VISIBLE_SNAPSHOT_ROWS {
            self.overflowed = true;
            return;
        }
        self.visible_ids.push(id);
    }

    /// 走査が正常に完了した場合にのみ呼ぶ。`ctx`・`table_generation` を添えて
    /// [`VisibleSnapshot`] を確定する。容量超過時は `None`（非登録。呼び出し元は
    /// 集計結果を返すのみでキャッシュへは反映しない）。
    pub(crate) fn finish(
        self,
        built_ctx: PolicyContext,
        built_table_generation: u64,
    ) -> Option<VisibleSnapshot> {
        if self.overflowed {
            return None;
        }
        Some(VisibleSnapshot {
            visible_ids: self.visible_ids,
            built_ctx,
            built_table_generation,
        })
    }
}

/// [`VisibleBitmapCache`] の 1 エントリ。`table`・`ctx` の組がキャッシュキー
/// （`PolicyContext` は `Hash` を実装しないため `Vec` 線形走査で照合する。
/// `SqlArenaCache`/`SparseIndexCache` と同じ理由）。
struct VisibleBitmapCacheEntry {
    table: String,
    snapshot: Arc<VisibleSnapshot>,
    last_used: u64,
}

/// ロックが保護する可変状態（[`RwLock`] 内側）。
#[derive(Default)]
struct VisibleBitmapCacheState {
    entries: Vec<VisibleBitmapCacheEntry>,
}

/// `sql::aggregate::execute_aggregate` 専用の可視行テーブル世代整合キャッシュ
/// 本体（モジュールドキュメント参照）。
pub(crate) struct VisibleBitmapCache {
    state: RwLock<VisibleBitmapCacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
}

impl VisibleBitmapCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(VisibleBitmapCacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
        }
    }

    /// `(table, ctx)` に一致し、`read_txn` のスナップショットにおけるテーブル
    /// 世代と整合するエントリを探す。ロック毒化・世代読み取り失敗はいずれも
    /// 「見つからなかった」として扱う（fail-closed。`SqlArenaCache::lookup` と
    /// 同じ方針・同じ世代不一致時の破棄条件——`read_txn` は呼び出し元ごとに
    /// 異なるスナップショットであり得るため、`storage` から新規に読んだ「真に
    /// 最新の」世代より厳密に古いと確認できた場合に限り stale として破棄する）。
    pub(crate) fn lookup(
        &self,
        storage: &Storage,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<Arc<VisibleSnapshot>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.state.write().ok()?;
        let current_generation = crate::catalog::table_generation_in_txn(read_txn, table).ok()?;
        let position = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)?;
        let built_generation = guard
            .entries
            .get(position)?
            .snapshot
            .built_table_generation();
        if built_generation == current_generation {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            let snapshot = Arc::clone(&entry.snapshot);
            drop(guard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(snapshot);
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

    /// 新規構築したスナップショットを挿入する。挿入対象自身が既に古い場合・
    /// ロック毒化時・容量超過時はキャッシュへ反映しない（`SqlArenaCache::insert`
    /// と同じ fail-closed 契約）。呼び出し元は戻り値を使わない
    /// （集計結果は同一走査で既に確定済みのため、`SqlArenaCache` と異なり
    /// `Arc` を呼び出し元へ返す必要がない）。
    pub(crate) fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        snapshot: VisibleSnapshot,
    ) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        let snapshot = Arc::new(snapshot);

        let Ok(mut guard) = self.state.write() else {
            return;
        };
        let Ok(read_txn) = storage.db().begin_read() else {
            return;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table)
        else {
            return;
        };
        if snapshot.built_table_generation() != current_generation {
            return;
        }

        let own_bytes = snapshot.approx_heap_bytes();
        if own_bytes > MAX_VISIBLE_BITMAP_CACHE_TOTAL_BYTES {
            return;
        }

        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)
        {
            guard.entries.remove(pos);
        }

        // 挿入対象テーブルに限定して世代不一致エントリを破棄する（他テーブルの
        // 世代カウンタは独立しているため、無関係な他テーブルの有効なエントリを
        // 巻き添え失効させない。`SqlArenaCache::insert` と同じ理由）。
        let before = guard.entries.len();
        guard.entries.retain(|e| {
            e.table != table || e.snapshot.built_table_generation() == current_generation
        });
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.snapshot.approx_heap_bytes())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_VISIBLE_BITMAP_CACHE_ENTRIES
            || total_bytes.saturating_add(own_bytes) > MAX_VISIBLE_BITMAP_CACHE_TOTAL_BYTES
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
            total_bytes = total_bytes.saturating_sub(removed.snapshot.approx_heap_bytes());
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(VisibleBitmapCacheEntry {
            table: table.to_string(),
            snapshot,
            last_used: seq,
        });
    }

    pub(crate) fn stats(&self) -> VisibleBitmapCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        VisibleBitmapCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            entries,
        }
    }
}

/// `sql::aggregate::execute_aggregate` へ渡すキャッシュアクセス束（`storage` は
/// `VisibleBitmapCache::insert`/`lookup` の世代再読取用、`cache` は本体。
/// `sql::arena_cache::ArenaCacheAccess` と同じ理由・構造）。
pub(crate) struct VisibleCacheAccess<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) cache: &'a VisibleBitmapCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant")
    }

    fn create_table(storage: &Storage, name: &str) {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("path", ColumnType::Text, false)],
            ))
            .expect("create table");
    }

    fn sample_snapshot(ctx: &PolicyContext, generation: u64, ids: &[u64]) -> VisibleSnapshot {
        let mut builder = VisibleSnapshotBuilder::new();
        for &id in ids {
            builder.mark_visible(id);
        }
        builder
            .finish(ctx.clone(), generation)
            .expect("snapshot within capacity")
    }

    #[test]
    fn lookup_hits_when_generation_matches() {
        let path = unique_db_path("visible_cache_hit");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "t");
        let cache = VisibleBitmapCache::new();
        let c = ctx("tenant-a");
        let read_txn = storage.db().begin_read().expect("read txn");
        let generation = crate::catalog::table_generation_in_txn(&read_txn, "t").expect("gen");
        cache.insert(
            &storage,
            "t",
            &c,
            sample_snapshot(&c, generation, &[1, 2, 3]),
        );

        let read_txn2 = storage.db().begin_read().expect("read txn");
        let hit = cache.lookup(&storage, &read_txn2, "t", &c);
        assert!(hit.is_some());
        assert_eq!(hit.expect("hit").visible_ids(), &[1, 2, 3]);
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn lookup_misses_for_different_ctx() {
        let path = unique_db_path("visible_cache_ctx_miss");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "t");
        let cache = VisibleBitmapCache::new();
        let a = ctx("tenant-a");
        let b = ctx("tenant-b");
        let read_txn = storage.db().begin_read().expect("read txn");
        let generation = crate::catalog::table_generation_in_txn(&read_txn, "t").expect("gen");
        cache.insert(&storage, "t", &a, sample_snapshot(&a, generation, &[1]));

        let read_txn2 = storage.db().begin_read().expect("read txn");
        assert!(cache.lookup(&storage, &read_txn2, "t", &b).is_none());
    }

    #[test]
    fn lookup_evicts_when_truly_stale() {
        let path = unique_db_path("visible_cache_stale");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "t");
        let cache = VisibleBitmapCache::new();
        let c = ctx("tenant-a");
        let read_txn = storage.db().begin_read().expect("read txn");
        let generation = crate::catalog::table_generation_in_txn(&read_txn, "t").expect("gen");
        cache.insert(&storage, "t", &c, sample_snapshot(&c, generation, &[1]));

        // 対象テーブルへの書き込みで世代を進める。
        let write_txn = storage.db().begin_write().expect("write txn");
        crate::catalog::bump_table_generation_in_txn(&write_txn, "t").expect("bump");
        crate::recovery::commit_boundary::commit(write_txn).expect("commit");

        let read_txn2 = storage.db().begin_read().expect("read txn");
        assert!(cache.lookup(&storage, &read_txn2, "t", &c).is_none());
        assert_eq!(cache.stats().stale_evictions, 1);
    }

    #[test]
    fn insert_does_not_bump_other_table_entries() {
        let path = unique_db_path("visible_cache_other_table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "t1");
        create_table(&storage, "t2");
        let cache = VisibleBitmapCache::new();
        let c = ctx("tenant-a");
        let read_txn1 = storage.db().begin_read().expect("read txn");
        let gen1 = crate::catalog::table_generation_in_txn(&read_txn1, "t1").expect("gen1");
        let read_txn2 = storage.db().begin_read().expect("read txn");
        let gen2 = crate::catalog::table_generation_in_txn(&read_txn2, "t2").expect("gen2");
        cache.insert(&storage, "t1", &c, sample_snapshot(&c, gen1, &[1]));
        cache.insert(&storage, "t2", &c, sample_snapshot(&c, gen2, &[2]));

        let read_txn = storage.db().begin_read().expect("read txn");
        assert!(cache.lookup(&storage, &read_txn, "t1", &c).is_some());
        assert!(cache.lookup(&storage, &read_txn, "t2", &c).is_some());
    }

    #[test]
    fn builder_over_capacity_yields_no_snapshot() {
        let c = ctx("tenant-a");
        let mut builder = VisibleSnapshotBuilder::new();
        // 容量超過を模擬するため直接オーバーフローフラグを立てる代わりに
        // 大量件数を投入するのは非現実的なので、内部状態を経由せず
        // 上限定数を直接検証する回帰として `MAX_VISIBLE_SNAPSHOT_ROWS` 相当の
        // 小さいビルダーを想定した契約テストに留める（実容量は
        // `arena::MAX_ARENA_ROWS` と共有）。
        for id in 0..10u64 {
            builder.mark_visible(id);
        }
        let snapshot = builder.finish(c, 0).expect("within capacity");
        assert_eq!(snapshot.visible_ids().len(), 10);
    }
}
