//! `sql::exec` の SCALAR 段（`WHERE` の等価・前方一致・`id` 単純比較条件）が
//! 全行走査（O(N)）の代わりに参照する、スカラー列二次索引の**構築・テーブル
//! 世代整合キャッシュ・候補削減 API**（構築・キャッシュは Issue #473、候補削減
//! への結線は Issue #474。対応 ADR: `docs/design/scalar-secondary-index.md`。
//! 親 Issue #359・#472。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-12・`docs/spec/04-behavior/rls.md`）。
//!
//! **消費経路**: 本モジュールの照会 API（[`ScalarIndex::candidates_for`]・
//! [`ScalarIndex::candidates_id_range`]・[`ScalarIndex::resolve_candidates`]）は
//! `sql::exec::execute_statement_with_cache` の SCALAR 事前フィルタから、索引
//! 対応述語（`sql::scalar_plan::classify_scalar_plan` が `PlainScan` 以外へ
//! 分類した形状。TEXT 列の等価・前方一致・`id` の単純比較とその組合せ）を持つ
//! クエリに限って消費される（詳細は `docs/design/scalar-index-prune.md` 参照）。
//! 索引は候補スロットを「絞る」ことしかできず「通す」ことはできないため、
//! 候補行にも `on_visible_row`（`matches_all`＋式述語）が引き続き適用される。
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

/// [`ScalarIndex::resolve_candidates`] の選択度切替閾値（Issue #474）。候補比
/// （交差後の候補数 ÷ 索引行数）が `numerator / denominator` を超える場合、
/// 索引経路より全走査（`O(N)`。候補列挙・交差のオーバーヘッドを持たない）が
/// 有利と判断し [`CandidateResolution::FallbackSelectivity`] へ縮退する。
/// 暫定既定値は 1/2（根拠は `docs/design/scalar-index-prune.md`「選択度切替の
/// 既定値」節。索引経路のコストは概ね `O(|hits|)`、全走査は `O(N + |hits|)`
/// のため損益分岐は `hits ≈ N` 近傍にしかなく、`sql::hnsw_cache` の ANN 先例
/// （1/10）をそのまま採ると本ユースケースの主要フェーズが索引経路に乗らず
/// 受入条件が vacuous になるため、より緩い閾値を採用する）。
const DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_NUMERATOR: u64 = 1;
const DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_DENOMINATOR: u64 = 2;

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

/// `s` の複製を、失敗しうるアロケーションとして構築する（codex-review P1
/// 対応・PR #569。`String::to_string`/`String::clone` はグローバルアロケータの
/// infallible な成長経路を辿り、失敗時は [`ScalarIndexBuildError::AllocationFailed`]
/// へ変換されずプロセスを異常終了させ得るため、`try_reserve_exact` で確保して
/// から `push_str` する明示的に fallible な経路に置き換える）。
fn try_owned_string(s: &str) -> Result<String, ScalarIndexBuildError> {
    let mut out = String::new();
    out.try_reserve_exact(s.len())
        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
    out.push_str(s);
    Ok(out)
}

/// 1 件の文字列値を索引へ追加する際の概算バイト量（[`TextColumnIndex::
/// approx_heap_bytes`] の `values`/`equality` の計上方法と揃える）。
fn approx_string_entry_bytes(s: &str) -> usize {
    s.len().saturating_add(std::mem::size_of::<String>())
}

/// 構築中の索引が確保しようとしている概算バイト量が [`MAX_SCALAR_INDEX_BYTES`]
/// を超えないことを、実際に文字列を複製する（`to_string`/`clone` で新規確保が
/// 起こる）**前**に検証する（codex-review P1 対応・PR #569。全構築完了後の
/// [`ScalarIndex::approx_heap_bytes`] 判定だけでは、長い異なる `TEXT` 値を多数
/// 含むスナップショットに対して判定前に上限超過分のメモリを確保してしまう）。
/// `running` は呼び出し元が管理する累計カウンタで、本関数が返す `Ok` を受けて
/// 呼び出し元が `additional` 分だけ加算する契約とする。
fn check_scalar_index_budget(
    running: usize,
    additional: usize,
) -> Result<(), ScalarIndexBuildError> {
    if running.saturating_add(additional) > MAX_SCALAR_INDEX_BYTES {
        return Err(ScalarIndexBuildError::TooLarge);
    }
    Ok(())
}

/// `values`/`offsets`/`slots`/`equality`（[`TextColumnIndex`] の 4 配列）を
/// 重複排除前の `pair_count` 件分だけ一括で `try_reserve_exact` する際に、
/// **実際に確保される**構造体サイズ分のバイト量（codex-review P1 対応・
/// PR #569 未解決分。`values: Vec<String>` 等の容量は `String`/`u32`/
/// `(String, u32)` の構造体サイズ分だけ確保され、その中身（文字列バイト列）は
/// 別途 [`approx_string_entry_bytes`] で行走査時に予算検証・計上済みだが、
/// 高重複 TEXT 列では重複排除後の実要素数（[`TextColumnIndex::approx_heap_bytes`]
/// が計上する対象）が `pair_count` を大幅に下回り、この**確保容量そのもの**が
/// 予算計上から漏れて `MAX_SCALAR_INDEX_BYTES` を実質バイパスし得た。呼び出し元は
/// 4 配列を確保する**前**に本関数の結果を [`check_scalar_index_budget`] で検証し、
/// 通れば `running` へ加算する契約とする）。
fn text_column_reservation_bytes(pair_count: usize) -> usize {
    let values_bytes = pair_count.saturating_mul(std::mem::size_of::<String>());
    let offsets_bytes = pair_count
        .saturating_add(1)
        .saturating_mul(std::mem::size_of::<u32>());
    let slots_bytes = pair_count.saturating_mul(std::mem::size_of::<u32>());
    // `equality: HashMap<String, u32>` は `pair_count` 件分の
    // `try_reserve(pair_count)` を行うが、実際に確保されるバケット数は
    // `pair_count` そのものではなく、hashbrown の負荷率・2 のべき乗丸めを
    // 経た `hashmap_bucket_count(pair_count)` 件（[`hashmap_reservation_bytes`]
    // 参照。codex-review P1 対応・PR #569 未解決分）。
    let equality_bytes = hashmap_reservation_bytes(pair_count);
    values_bytes
        .saturating_add(offsets_bytes)
        .saturating_add(slots_bytes)
        .saturating_add(equality_bytes)
}

/// `std::collections::HashMap`（hashbrown 実装）が最低 `min_capacity` 件を
/// 保持できるよう確保する**実バケット数**の見積り（codex-review P1 対応・
/// PR #569。hashbrown v0.15.5 の `RawTableInner::fallible_with_capacity` /
/// `capacity_to_buckets`〔`raw/mod.rs`〕と同じ規則を踏襲する: 最大負荷率
/// 7/8 を満たすようバケット数を切り上げたうえで 2 のべき乗へ丸める
/// （`min_capacity < 8` は 4 または 8 に固定）。`pair_count × エントリ
/// サイズ` という単純計算では、この丸めによる追加確保
/// （例: 100 万件 → 実バケット数 2^21 ≈ 210 万）が計上から漏れ、
/// [`MAX_SCALAR_INDEX_BYTES`] の予算検査を実質バイパスし得た。
/// hashbrown の内部実装はバージョン依存で将来変わり得るため、丸め則の
/// 変化があっても過小評価側に倒れないよう本関数は意図的に保守的
/// （実バケット数以上）に倒す。
fn hashmap_bucket_count(min_capacity: usize) -> usize {
    if min_capacity == 0 {
        return 0;
    }
    if min_capacity < 8 {
        return if min_capacity < 4 { 4 } else { 8 };
    }
    let adjusted = match min_capacity.checked_mul(8) {
        Some(v) => v / 7,
        // オーバーフローする規模はそもそも `MAX_SCALAR_INDEX_BYTES` を
        // 大幅に超えるため、`usize::MAX` を返し予算検査で確実に拒否させる
        // （fail-closed）。
        None => return usize::MAX,
    };
    adjusted.next_power_of_two()
}

/// 1 バケットあたりの hashbrown 制御バイト分を含む保守的な確保バイト量
/// （codex-review P1 対応・PR #569）。hashbrown はバケット配列の直後に
/// SIMD グループ幅（実行環境依存。最大でも数十バイト程度）分の制御バイト
/// パディングを追加確保するため、その分を固定オーバーヘッドとして
/// 加算し過小評価を避ける。
const HASHBROWN_GROUP_PADDING_BYTES: usize = 32;

/// `equality: HashMap<String, u32>` が最低 `min_capacity` 件を保持できる
/// よう確保する概算バイト量（[`hashmap_bucket_count`] 参照）。
fn hashmap_reservation_bytes(min_capacity: usize) -> usize {
    let buckets = hashmap_bucket_count(min_capacity);
    if buckets == 0 {
        // `min_capacity == 0` はテーブル未確保（`HashMap::new()` 相当）で
        // 実際に確保は起こらないため、固定オーバーヘッドも計上しない。
        return 0;
    }
    // 1 バケットにつきエントリ本体（`(String, u32)`）+ 制御バイト 1。
    let per_bucket = std::mem::size_of::<(String, u32)>().saturating_add(1);
    buckets
        .saturating_mul(per_bucket)
        .saturating_add(HASHBROWN_GROUP_PADDING_BYTES)
}

/// 行走査中に `TEXT` 列ごとの作業領域 `acc: Vec<(String, u32)>`
/// （[`ScalarIndex::build`]）が確保する**タプル全体**の容量分バイト量
/// （codex-review P1 対応・PR #569 未解決分）。
///
/// 修正前は `acc` へ 1 件ずつ `try_reserve(1)` してから `push` しており、
/// 予算計上も文字列本体長＋`String` 構造体サイズ（[`approx_string_entry_bytes`]）
/// のみで、(1) タプルの `u32` 分・アラインメント詰め物と (2) `try_reserve` の
/// 内部成長戦略（要求量ちょうどではなく現容量の倍増などで余剰確保され得る）の
/// 双方が計上から漏れていた。64bit 環境で全行・全 `TEXT` 列が空文字列の場合、
/// 文字列本体バイトは 0 で予算上「ほぼ無料」に見える一方、`acc` 自体の確保
/// 容量（タプル構造体サイズ×要素数、かつ倍増成長の余剰込み）は無視できない
/// 量に達し、既存の行数・列数・スナップショット容量上限内の入力でも
/// [`MAX_SCALAR_INDEX_BYTES`] 判定より前に未検証の大容量確保が起こり得た。
///
/// 本関数は列ごとに `acc` の必要行数上限（1 行につき列あたり高々 1 値なので
/// `row_count` が上限）を**確保前に一括**でバイト量へ変換し、
/// [`check_scalar_index_budget`] で検証してから
/// `Vec::try_reserve_exact(row_count)` する契約にすることで、成長戦略の余剰
/// 確保そのものを起こさせない（倍増ではなく厳密量の 1 回確保に固定するため、
/// 見積りバイト量と実確保バイト量が一致する）。
fn per_column_accumulator_reservation_bytes(row_count: usize) -> usize {
    row_count.saturating_mul(std::mem::size_of::<(String, u32)>())
}

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
    ///
    /// `values`/`offsets`/`slots`/`equality` はいずれも構築時に
    /// `try_reserve`/`try_reserve_exact` で重複排除前の `pair_count` 件分を
    /// 一括確保するため、重複が多い列では実要素数（`len()`）が確保容量
    /// （`capacity()`）を大きく下回る。`len()` だけを計上すると実際に確保
    /// 済みのメモリを過小計上し、[`ScalarIndexCache`] の総量上限（1 GiB）に
    /// よる追い出しが実メモリ量に対して働かなくなる（codex-review P1 対応・
    /// PR #569 未解決分）。確保容量ベースで保守的に計上し、この値をキャッシュ
    /// 登録・追い出し判定にも使う契約とする。
    fn approx_heap_bytes(&self) -> usize {
        // `values: Vec<String>` は外側 Vec の確保容量（`String` 構造体サイズ
        // 分）に加え、各 `String` 自身の確保容量（中身のバイト列）も計上する。
        // 個々の `String` は `try_owned_string` で複製された時点の長さぴったり
        // に確保される想定だが、`len()` ではなく `capacity()` を使うことで
        // 標準ライブラリの確保戦略が変わっても過小計上側に倒れないようにする。
        let values_outer_bytes = self
            .values
            .capacity()
            .saturating_mul(std::mem::size_of::<String>());
        let values_inner_bytes: usize = self
            .values
            .iter()
            .map(String::capacity)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let values_bytes = values_outer_bytes.saturating_add(values_inner_bytes);
        let offsets_bytes = self
            .offsets
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let slots_bytes = self
            .slots
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        // `HashMap` のキーは `values` と同じ文字列を複製保持する（`equality`
        // が値 → 添字の直引き専用であり `values` への参照を持たないため）。
        // 各キー文字列の確保容量（中身のバイト列）に加え、テーブル本体
        // （エントリ配列＋制御バイト。使用・未使用バケット双方を含む）を
        // [`hashmap_bucket_count`] で計上する。`HashMap::capacity()` は
        // hashbrown の負荷率適用後の「保持可能要素数」であり**実バケット数
        // ではない**ため（codex-review P1 対応・PR #569 未解決分）、これを
        // `hashmap_bucket_count` へ逆算入力することで実バケット数を復元する
        // （`capacity()` は `hashmap_bucket_count` と同じ丸め則で導出される
        // ため、往復させても実バケット数と一致する）。
        let equality_inner_bytes: usize = self
            .equality
            .keys()
            .map(String::capacity)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let equality_table_bytes = hashmap_reservation_bytes(self.equality.capacity());
        let equality_bytes = equality_inner_bytes.saturating_add(equality_table_bytes);
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
    row_count: usize,
    /// `schema.columns` と同じ長さ・順序。`TEXT` 列のみ `Some`。
    columns: Vec<Option<TextColumnIndex>>,
    /// `id` 昇順（同一 `id` 内はスロット昇順）に整列した `(id, slot)`。全行が
    /// `id_as_finite_scalar` を満たす場合のみ `Some`（1 件でも `id > 2^53` が
    /// あれば `None`。モジュールドキュメント参照）。
    id_index: Option<Vec<(u64, u32)>>,
    /// [`Self::build`] 完了時に 1 回だけ計算した概算ヒープバイト量
    /// （[`Self::compute_approx_heap_bytes`] の結果）。索引は構築後不変
    /// （`columns`/`id_index` を変更する API を持たない）ため、この値も
    /// 索引の寿命を通じて不変。[`ScalarIndexCache::insert`] が
    /// 書き込みロック保持中に既存索引すべてを毎回再走査してしまう問題
    /// （codex-review P2 対応・PR #569）を解消するため、
    /// [`Self::approx_heap_bytes`] はこのフィールドを返すだけの O(1) アクセサ
    /// にする。
    approx_bytes: usize,
}

/// `TextColumnIndex` 構築時の並び替えキー: バイト列昇順、同値はスロット昇順。
/// スロット（`u32`。同一列内で重複しない）が全順序の明示的タイブレークを
/// 提供するため、この比較関数のもとで同値ペアは存在しない（決定的ソート）。
fn cmp_value_then_slot(a: &(String, u32), b: &(String, u32)) -> std::cmp::Ordering {
    a.0.as_bytes().cmp(b.0.as_bytes()).then(a.1.cmp(&b.1))
}

/// [`ScalarIndex::resolve_candidates`] の結果（Issue #474: `sql::exec` が
/// 候補削減を行うか全走査へ縮退するかの唯一の分岐点）。
pub(crate) enum CandidateResolution {
    /// 交差済み候補スロット（昇順）を使う。空 `Vec` は「一致 0 件」（全走査と
    /// 同じ結果になる。呼び出し元は空アリーナを構築するだけでよい）。
    Use(Vec<u32>),
    /// 索引対応述語のうち 1 つ以上が「列未索引」または `id_index` が `None`
    /// （`id > 2^53` を含む世代）で判定不能だったため、索引を一切使わず
    /// 全走査へ縮退する。
    FallbackNoIndex,
    /// 交差後の候補比が選択度切替閾値
    /// （[`DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_NUMERATOR`]/
    /// [`DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_DENOMINATOR`]）を超えた
    /// （索引経路より全走査が有利と判断）ため縮退する。
    FallbackSelectivity,
}

/// 2 本の昇順スライスの交差（両方に存在する要素のみ）を昇順で返す
/// （2 本指マージ。両リストとも重複要素を持たない契約——`candidates_for`／
/// `candidates_id_range` はいずれもスロット重複のない集合を返す）。
/// アロケーション失敗（`try_reserve`）は `None`（呼び出し元は索引なしへ
/// 縮退する）。
fn intersect_sorted(a: &[u32], b: &[u32]) -> Option<Vec<u32>> {
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve(a.len().min(b.len())).ok()?;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    Some(out)
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

        // 構築完了まで確保した（概算）文字列バイト量の累計。値の複製
        // （`to_string`/`clone`）を行う**前**に [`check_scalar_index_budget`] で
        // 検証してから加算する（codex-review P1 対応・PR #569。モジュール
        // ドキュメント「fail-closed の適用範囲」参照）。
        let mut approx_bytes: usize = 0;

        // 列ごとに (value, slot) を蓄積する作業領域（`TEXT` 列のみ `Some`）。
        let mut per_column: Vec<Option<Vec<(String, u32)>>> = Vec::new();
        per_column
            .try_reserve_exact(column_count)
            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
        for column in &schema.columns {
            match column.ty {
                ColumnType::Text => {
                    // `acc` は 1 行につき列あたり高々 1 値しか追加されないため
                    // `row_count` が確保上限になる。倍増などの成長戦略による
                    // 余剰確保を避けるため、行走査を始める前に必要量ちょうどを
                    // 一括で `try_reserve_exact` し、その容量分（タプル全体の
                    // サイズ）を確保前にバイト予算へ計上する（codex-review P1
                    // 対応・PR #569 未解決分。`per_column_accumulator_reservation_bytes`
                    // 参照）。
                    let reservation_bytes = per_column_accumulator_reservation_bytes(row_count);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    let mut acc: Vec<(String, u32)> = Vec::new();
                    acc.try_reserve_exact(row_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    per_column.push(Some(acc));
                }
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
                    // タプル・`Vec` の確保容量分は上記の事前一括確保
                    // （`per_column_accumulator_reservation_bytes`）で
                    // 既に予算計上済みのため、ここでは文字列本体（ヒープ）の
                    // バイト量のみを追加計上する（二重計上を避ける）。
                    let additional = v.len();
                    check_scalar_index_budget(approx_bytes, additional)?;
                    let owned = try_owned_string(v)?;
                    approx_bytes = approx_bytes.saturating_add(additional);
                    // `acc` は列ごとに `row_count` ちょうどの容量を
                    // 事前に厳密確保済みで、1 行につき列あたり高々 1 回しか
                    // push されないため、この push が容量を超えて再確保
                    // （＝未計上の追加確保）を起こすことはない。
                    acc.push((owned, slot_u32));
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
                                                                 // 重複排除後の値種類数は `pairs.len()`（行の重複値込みの総数）を
                                                                 // 上回らないため、この上限で `values`/`offsets`/`equality` を
                                                                 // 事前に一括確保し、高カーディナリティ列で値ごとに
                                                                 // `try_reserve_exact(1)` を呼ぶ再確保コストを避ける
                                                                 // （codex-review P2 対応・PR #569）。`slots` は値と無関係に
                                                                 // 行数分（`pairs.len()`）で確定するため同様に一括確保する。
                    let pair_count = pairs.len();
                    // 上記 4 配列の確保容量（重複排除前の pair_count 件分）は
                    // 実メモリとして確保されるため、確保前にバイト予算へ計上して
                    // 検証する（codex-review P1 対応・PR #569 未解決分。
                    // `text_column_reservation_bytes` 参照）。
                    let reservation_bytes = text_column_reservation_bytes(pair_count);
                    check_scalar_index_budget(approx_bytes, reservation_bytes)?;
                    approx_bytes = approx_bytes.saturating_add(reservation_bytes);
                    let mut values: Vec<String> = Vec::new();
                    values
                        .try_reserve_exact(pair_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    let mut offsets: Vec<u32> = Vec::new();
                    offsets
                        .try_reserve_exact(pair_count.saturating_add(1))
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    let mut slots: Vec<u32> = Vec::new();
                    slots
                        .try_reserve_exact(pair_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                    let mut equality: HashMap<String, u32> = HashMap::new();
                    equality
                        .try_reserve(pair_count)
                        .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
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
                        // `equality` は `values` と同じ文字列を複製保持する
                        // （[`TextColumnIndex::approx_heap_bytes`] の
                        // `equality_bytes` 計上と対応）ため、複製前に同じ予算
                        // 検証を経る（codex-review P1 対応・PR #569）。
                        let additional = approx_string_entry_bytes(&value);
                        check_scalar_index_budget(approx_bytes, additional)?;
                        let equality_key = try_owned_string(&value)?;
                        approx_bytes = approx_bytes.saturating_add(additional);
                        equality
                            .try_reserve(1)
                            .map_err(|_| ScalarIndexBuildError::AllocationFailed)?;
                        equality.insert(equality_key, value_index);
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

        let mut built = Self {
            built_ctx: snapshot.built_ctx_for_index().clone(),
            built_table_generation: snapshot.built_table_generation_for_index(),
            row_count,
            columns,
            id_index,
            // 直後に `compute_approx_heap_bytes` の結果で確定させるまでの
            // 仮値。この構造体は `build` の外へ `0` のまま漏れ出さない。
            approx_bytes: 0,
        };
        let computed_bytes = built.compute_approx_heap_bytes();
        if computed_bytes > MAX_SCALAR_INDEX_BYTES {
            return Err(ScalarIndexBuildError::TooLarge);
        }
        built.approx_bytes = computed_bytes;
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

    /// `column_index` 列（`TEXT` 列限定）の値ごとのグループを、値のバイト列
    /// 昇順（[`Self::build`] の `TextColumnIndex.values` と同じ順序）で列挙する
    /// （Issue #475: `sql::group_by` の `WHERE` なし `GROUP BY` 列挙形が使う）。
    /// 列が `TEXT` でない・未知の列は `None`。各グループのスロット列は昇順。
    /// [`Self::build`] は該当列の非 `NULL` 値を**すべて**索引化するか（成功）、
    /// 予算超過等で索引全体の構築を諦めるか（`Err`。この場合キャッシュに
    /// エントリ自体が存在しない）のいずれかであり、一部の値だけを欠落させたまま
    /// 索引を返すことはないため（モジュールドキュメント「データモデル」・
    /// [`Self::build`] 参照）、ここで列挙される値は当該列の可視行が実際に
    /// 持つ非 `NULL` 値の**全体**である（一部スキップによる取りこぼしを呼び
    /// 出し元が心配する必要はない）。
    pub(crate) fn column_groups(
        &self,
        column_index: usize,
    ) -> Option<impl Iterator<Item = (&str, &[u32])> + '_> {
        let column = self.columns.get(column_index)?.as_ref()?;
        Some((0..column.values.len()).filter_map(move |i| {
            let value = column.values.get(i)?.as_str();
            let value_index = u32::try_from(i).ok()?;
            let slots = column.slots_for_value_index(value_index)?;
            Some((value, slots))
        }))
    }

    /// `column_index` 列（`TEXT` 列限定）が `NULL`（＝索引のどの値エントリにも
    /// 現れない）である可視行のスロットを昇順で返す（Issue #475:
    /// `sql::group_by` の `WHERE` なし `GROUP BY` 列挙形が NULL グループを
    /// 補完するために使う）。列が `TEXT` でない・未知の列は `None`。
    ///
    /// `row_count`（索引構築時の全スロット数）長のビットマップで索引済み全値の
    /// スロットを被覆し、被覆されなかったスロットを NULL とみなす。
    /// [`Self::build`] のドキュメントどおり「索引化された値の集合」は当該列の
    /// 非 `NULL` 値の全体であるため、この差分計算は正確に NULL 行と一致する
    /// （`declarative_filter::MetadataFilter::matches` の NULL 常時不一致判定と
    /// 同じ意味論。モジュールドキュメント参照）。
    pub(crate) fn slots_without_value(&self, column_index: usize) -> Option<Vec<u32>> {
        let column = self.columns.get(column_index)?.as_ref()?;
        let mut covered: Vec<bool> = Vec::new();
        covered.try_reserve_exact(self.row_count).ok()?;
        covered.resize(self.row_count, false);
        for &slot in &column.slots {
            if let Some(flag) = covered.get_mut(slot as usize) {
                *flag = true;
            }
        }
        let mut out: Vec<u32> = Vec::new();
        for (idx, &is_covered) in covered.iter().enumerate() {
            if !is_covered {
                let slot = u32::try_from(idx).ok()?;
                out.try_reserve(1).ok()?;
                out.push(slot);
            }
        }
        Some(out)
    }

    /// `id` に対する範囲述語（単純比較）向け照会。`id_index` が `None`
    /// （`id > 2^53` を含む行がある）の場合は `None`（呼び出し元が全走査へ
    /// 縮退する契機。モジュールドキュメント参照）。戻り値は昇順 `Vec<u32>`。
    ///
    /// `id_index` は `id` 昇順（同一 `id` 内はスロット昇順）に整列済みのため、
    /// 範囲の両端を [`slice::partition_point`]（二分探索）で求め、対象区間
    /// だけを複製する（Issue #474。旧実装は全件の線形走査＋フィルタだった）。
    /// 区間内は `id` 昇順だがスロット昇順とは限らない（異なる `id` 間で挿入
    /// 順が保たれるだけ）ため、返す前に `sort_unstable` で候補列の共通契約
    /// （スロット昇順）へ揃える。
    pub(crate) fn candidates_id_range(
        &self,
        lower: std::ops::Bound<u64>,
        upper: std::ops::Bound<u64>,
    ) -> Option<Vec<u32>> {
        use std::ops::Bound;
        let idx = self.id_index.as_ref()?;
        let start = match lower {
            Bound::Unbounded => 0,
            Bound::Included(l) => idx.partition_point(|(id, _)| *id < l),
            Bound::Excluded(l) => idx.partition_point(|(id, _)| *id <= l),
        };
        let end = match upper {
            Bound::Unbounded => idx.len(),
            Bound::Included(u) => idx.partition_point(|(id, _)| *id <= u),
            Bound::Excluded(u) => idx.partition_point(|(id, _)| *id < u),
        };
        if start >= end {
            return Some(Vec::new());
        }
        let mut out: Vec<u32> = Vec::new();
        out.try_reserve_exact(end - start).ok()?;
        out.extend(idx[start..end].iter().map(|(_, slot)| *slot));
        out.sort_unstable();
        Some(out)
    }

    /// `metadata_filters`（`TEXT` 列の等価・前方一致）と `id_preds`（`id` の
    /// 単純比較。`sql::scalar_plan::classify_scalar_plan` が
    /// `ScalarPlan::PlainScan` 以外へ分類した述語のみを渡す契約）から候補
    /// スロット集合を導出する（Issue #474）。各述語の候補を個別に取得し
    /// （`None` は「判定不能」＝列未索引または `id_index` が `None`）、複数
    /// 述語は昇順ベクトルの交差（2 本指マージ）で結合する。交差はスロットを
    /// 「絞る」ことしかできず「通す」ことはできないため、呼び出し元
    /// （`sql::exec`）が候補行にも引き続き `on_visible_row`（`matches_all`＋
    /// 式述語）を適用する契約と組み合わさって fail-closed が成立する（本
    /// メソッド自体が正しさの唯一の防御ではない）。
    pub(crate) fn resolve_candidates(
        &self,
        metadata_filters: &[MetadataFilter],
        id_preds: &[crate::sql::scalar_plan::IdPredicate],
    ) -> CandidateResolution {
        if metadata_filters.is_empty() && id_preds.is_empty() {
            // `classify_scalar_plan` が `PlainScan` 以外を返す限り到達しない
            // 呼び出し規約違反だが、防御的に fail-closed へ倒す。
            return CandidateResolution::FallbackNoIndex;
        }
        // 述語ごとの候補列を全件 `Vec<Vec<u32>>` に集めてから交差する実装は、
        // 交差前の累計保持量に上限が無かった（codex-review P1 指摘・PR #601）。
        // 許可リスト上限の述語数（最大 256 件・`sql::allowlist` 参照）それぞれが
        // 索引済み行の大半に一致する入力（同一 TEXT 値の等価条件を 256 個
        // 並べる等）では、選択度切替（縮退判定）が交差**後**にしか働かないため、
        // 交差前に 256 本の候補列（各最大 `row_count` 件）を同時保持し
        // `MAX_ARENA_ROWS`（約 100 万行）規模ではコピーだけで約 1 GiB を
        // 追加確保しうる。交差は「述語を追加するほど結果が単調非増加になる」
        // （2 本指マージの結果は常に両オペランド以下の長さ）性質を持つため、
        // 述語ごとの候補列を生成するたびにその場で累積へ交差し、次の述語へ
        // 進む前に前の候補列を破棄する（保持するのは累積候補列と直近生成した
        // 1 本のみ）。これにより述語数に依存せず、保持量は個々の候補列の
        // 最大サイズ（`row_count` に比例）で頭打ちになる。
        //
        // 交差前に最小の候補列から処理する最適化（Issue #474 時点の実装）は
        // 全列の長さを事前に知る必要があり本対応と両立しないため撤去した。
        // 累積が空集合になった時点で以降の述語を評価せず打ち切る（交差は
        // 単調非増加のため以降の交差結果も必ず空集合）ことで、代わりに早期
        // 打ち切りによる実用上の性能劣化を抑える。
        let mut accumulated: Option<Vec<u32>> = None;
        for filter in metadata_filters {
            if accumulated.as_deref().is_some_and(<[u32]>::is_empty) {
                break;
            }
            let slots = match self.candidates_for(filter) {
                Some(slots) => slots,
                None => return CandidateResolution::FallbackNoIndex,
            };
            accumulated = Some(match accumulated {
                None => slots,
                Some(acc) => match intersect_sorted(&acc, &slots) {
                    Some(v) => v,
                    None => return CandidateResolution::FallbackNoIndex,
                },
            });
        }
        for pred in id_preds {
            if accumulated.as_deref().is_some_and(<[u32]>::is_empty) {
                break;
            }
            let Some((lower, upper)) = crate::sql::scalar_plan::id_bounds(pred) else {
                return CandidateResolution::FallbackNoIndex;
            };
            let slots = match self.candidates_id_range(lower, upper) {
                Some(slots) => slots,
                None => return CandidateResolution::FallbackNoIndex,
            };
            accumulated = Some(match accumulated {
                None => slots,
                Some(acc) => match intersect_sorted(&acc, &slots) {
                    Some(v) => v,
                    None => return CandidateResolution::FallbackNoIndex,
                },
            });
        }
        // 上の 2 ループは冒頭の空チェックにより少なくとも 1 回は候補列を
        // 生成するため、ここで `None` のままということはない。
        let intersected = accumulated.unwrap_or_default();
        let hits = intersected.len() as u64;
        let row_count = self.row_count as u64;
        // 選択度切替（`sql::hnsw_cache` の `full_scan_ratio` と同型の
        // `checked_mul` による整数比較。丸め誤差を避けるため両辺を分母倍する）。
        let exceeds = match hits.checked_mul(DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_DENOMINATOR) {
            Some(lhs) => {
                match row_count.checked_mul(DEFAULT_SCALAR_INDEX_FULL_SCAN_RATIO_NUMERATOR) {
                    Some(rhs) => lhs > rhs,
                    // `row_count` は `MAX_ARENA_ROWS`（100 万行程度）で頭打ちの
                    // ため現実的には到達しないが、桁あふれた場合は判定不能
                    // として選択度超過側（保守的に全走査）へ倒す。
                    None => true,
                }
            }
            None => true,
        };
        if exceeds {
            return CandidateResolution::FallbackSelectivity;
        }
        CandidateResolution::Use(intersected)
    }

    /// 索引全体（`TEXT` 列の辞書・CSR・`equality`・`id_index`）の概算ヒープ
    /// バイト量を実走査で計算する（`O(索引サイズ)`）。[`Self::build`] 完了時に
    /// 1 回だけ呼び、結果を `approx_bytes` フィールドへ確定させる。索引は
    /// 構築後不変のためこの計算はここでしか行わない
    /// （[`Self::approx_heap_bytes`] のドキュメント参照。codex-review P2
    /// 対応・PR #569）。
    fn compute_approx_heap_bytes(&self) -> usize {
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

    /// 概算ヒープバイト量（容量判定用）を返す O(1) アクセサ。
    /// [`Self::build`] 完了時に確定した `approx_bytes` フィールドをそのまま
    /// 返すだけで索引を再走査しない（[`ScalarIndexCache::insert`] が書き込み
    /// ロック保持中に呼んでも既存索引の再走査コストを引き起こさない。
    /// codex-review P2 対応・PR #569）。
    fn approx_heap_bytes(&self) -> usize {
        self.approx_bytes
    }

    /// 索引構築時のスナップショット行数（Issue #474: `sql::exec` の索引↔
    /// スナップショット同一性ガードが `snapshot.arena().len()` と突き合わせる）。
    pub(crate) fn row_count(&self) -> usize {
        self.row_count
    }

    /// 索引構築時のテーブル世代（Issue #474: `sql::exec` の索引↔スナップショット
    /// 同一性ガードが `snapshot.built_table_generation_for_index()` と突き合わせる）。
    pub(crate) fn built_table_generation(&self) -> u64 {
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
    /// Issue #474: `sql::exec` が索引経路（`ScalarIndex::resolve_candidates`
    /// の `CandidateResolution::Use`）を実際に消費してクエリを実行した回数。
    pub index_scans: u64,
    /// Issue #474: `sql::exec` が索引対応述語を持つクエリで全走査へ縮退した
    /// 回数（`FallbackNoIndex`／`FallbackSelectivity`／同一性ガード不一致
    /// いずれも含む）。
    pub plain_scan_fallbacks: u64,
    /// Issue #475: `sql::aggregate`／`sql::group_by` が索引経路（候補走査・
    /// `GROUP BY` キー列挙形）を実際に消費して集計クエリを実行した回数
    /// （`index_scans`/`plain_scan_fallbacks` と別枠。SELECT 経路の消費と
    /// 区別する）。
    pub aggregate_index_scans: u64,
    /// Issue #475: 集計・`GROUP BY` クエリが索引対応述語・形状を持ちながら
    /// 全走査へ縮退した回数（`FallbackNoIndex`／`FallbackSelectivity`／
    /// 同一性ガード不一致／構築断念／NULL 補完不能のいずれも含む）。
    pub aggregate_plain_scan_fallbacks: u64,
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
    index_scans: AtomicU64,
    plain_scan_fallbacks: AtomicU64,
    aggregate_index_scans: AtomicU64,
    aggregate_plain_scan_fallbacks: AtomicU64,
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
            index_scans: AtomicU64::new(0),
            plain_scan_fallbacks: AtomicU64::new(0),
            aggregate_index_scans: AtomicU64::new(0),
            aggregate_plain_scan_fallbacks: AtomicU64::new(0),
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
        // 以下、ヒットで早期 `return` する 1 経路を除き `None` に到達する分岐
        // （ロック毒化・世代読み取り失敗・未登録・世代不一致のいずれも）で
        // `self.misses` を明示的に加算する（codex-review P2 対応・PR #569。
        // 以前は `stats()` 側の `fetch_add(0)` のみで実質常にゼロだった）。
        let Ok(mut guard) = self.state.write() else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Some(position) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.index.built_ctx == *ctx)
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Some(built_generation) = guard
            .entries
            .get(position)
            .map(|e| e.index.built_table_generation())
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if built_generation == current_generation {
            let Some(entry) = guard.entries.get_mut(position) else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            entry.last_used = seq;
            let index = Arc::clone(&entry.index);
            drop(guard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(index);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
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
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        ScalarIndexCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            builds: self.builds.load(Ordering::Relaxed),
            build_failures: self.build_failures.load(Ordering::Relaxed),
            entries,
            index_scans: self.index_scans.load(Ordering::Relaxed),
            plain_scan_fallbacks: self.plain_scan_fallbacks.load(Ordering::Relaxed),
            aggregate_index_scans: self.aggregate_index_scans.load(Ordering::Relaxed),
            aggregate_plain_scan_fallbacks: self
                .aggregate_plain_scan_fallbacks
                .load(Ordering::Relaxed),
        }
    }

    /// [`ScalarIndex::build`] が失敗したことを観測用統計へ計上する
    /// （`sql::exec::execute_statement_with_cache` の gated 構築が呼ぶ。
    /// モジュールドキュメント「fail-closed の適用範囲」参照）。
    pub(crate) fn record_build_failure(&self) {
        self.build_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #474: `sql::exec` が索引経路（`CandidateResolution::Use`）を
    /// 実際に消費してクエリを実行したことを観測用統計へ計上する。
    pub(crate) fn record_index_scan(&self) {
        self.index_scans.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #474: `sql::exec` が索引対応述語を持つクエリで全走査へ縮退した
    /// ことを観測用統計へ計上する（`FallbackNoIndex`／`FallbackSelectivity`／
    /// 索引↔スナップショット同一性ガード不一致のいずれも呼ぶ）。
    pub(crate) fn record_plain_scan_fallback(&self) {
        self.plain_scan_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #475: `sql::aggregate`／`sql::group_by` が索引経路（候補走査・
    /// `GROUP BY` キー列挙形）を実際に消費して集計クエリを実行したことを
    /// 観測用統計へ計上する。
    pub(crate) fn record_aggregate_index_scan(&self) {
        self.aggregate_index_scans.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue #475: 集計・`GROUP BY` クエリが索引対応述語・形状を持ちながら
    /// 全走査へ縮退したことを観測用統計へ計上する。
    pub(crate) fn record_aggregate_plain_scan_fallback(&self) {
        self.aggregate_plain_scan_fallbacks
            .fetch_add(1, Ordering::Relaxed);
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

    // ---------- 容量検証ヘルパーの単体テスト（codex-review P1 対応・PR #569:
    // 「確保前に容量を検証する」契約を、1 GiB 規模のデータを実際に確保せず
    // 直接固定する） ----------

    #[test]
    fn check_scalar_index_budget_rejects_before_exceeding_limit() {
        // 上限ちょうどまでは許可し、1 byte でも超えたら拒否する（境界値）。
        assert!(check_scalar_index_budget(0, MAX_SCALAR_INDEX_BYTES).is_ok());
        assert!(matches!(
            check_scalar_index_budget(0, MAX_SCALAR_INDEX_BYTES + 1),
            Err(ScalarIndexBuildError::TooLarge)
        ));
        assert!(matches!(
            check_scalar_index_budget(MAX_SCALAR_INDEX_BYTES, 1),
            Err(ScalarIndexBuildError::TooLarge)
        ));
        // `saturating_add` によるオーバーフロー耐性（巨大な累計値でもパニックしない）。
        assert!(matches!(
            check_scalar_index_budget(usize::MAX, 1),
            Err(ScalarIndexBuildError::TooLarge)
        ));
    }

    #[test]
    fn text_column_reservation_bytes_scales_with_pair_count_regardless_of_dedup() {
        // 重複排除前の pair_count のみに依存する（実際に何種類の値が
        // 含まれるかは無関係）。高重複列でも確保容量分の予算計上を
        // バイパスできないことを固定する（codex-review P1 対応・PR #569
        // 未解決分）。
        // pair_count=0 でも offsets は 1 件（先頭の 0）分だけ確保される。
        assert_eq!(text_column_reservation_bytes(0), std::mem::size_of::<u32>());
        let per_entry = std::mem::size_of::<String>()
            + std::mem::size_of::<u32>() // offsets（pair_count + 1 分だが定数項は無視）
            + std::mem::size_of::<u32>()
            + std::mem::size_of::<(String, u32)>();
        let pair_count = 1_000;
        let bytes = text_column_reservation_bytes(pair_count);
        // offsets の +1 分だけ厳密な等式ではなく下限として確認する。
        assert!(bytes >= pair_count.saturating_mul(per_entry));
    }

    #[test]
    fn text_column_reservation_bytes_budget_rejects_high_duplication_column() {
        // 1 GiB 上限に対し、重複排除後は 1 件しか実データが残らない列でも、
        // 確保容量（pair_count 件分）だけで上限を超えるケースを検証する。
        // 一致し得ない大きさの pair_count（例: 全行が同一値の高重複 TEXT 列）を
        // 想定し、確保**前**の予算検証でバイパスされないことを固定する。
        let huge_pair_count = MAX_SCALAR_INDEX_BYTES; // 明らかに 1 GiB を超える確保容量になる件数
        let reservation_bytes = text_column_reservation_bytes(huge_pair_count);
        assert!(reservation_bytes > MAX_SCALAR_INDEX_BYTES);
        assert!(matches!(
            check_scalar_index_budget(0, reservation_bytes),
            Err(ScalarIndexBuildError::TooLarge)
        ));
    }

    #[test]
    fn hashmap_bucket_count_matches_hashbrown_rounding_for_known_capacities() {
        // hashbrown の負荷率 7/8・2 のべき乗丸めの既知の境界値を固定する
        // （codex-review P1 対応・PR #569）。100 万件は指摘で挙げられた
        // 具体例（1,000,000 * 8 / 7 ≈ 1,142,857 → 次のべき乗 2^21）。
        assert_eq!(hashmap_bucket_count(0), 0);
        assert_eq!(hashmap_bucket_count(1), 4);
        assert_eq!(hashmap_bucket_count(4), 8);
        assert_eq!(hashmap_bucket_count(7), 8);
        assert_eq!(hashmap_bucket_count(8), 16);
        assert_eq!(hashmap_bucket_count(1_000_000), 1 << 21);
    }

    #[test]
    fn hashmap_reservation_bytes_exceeds_naive_pair_count_times_entry_size() {
        // 修正前の単純計算（pair_count × エントリサイズ）を常に上回ることを
        // 固定する（codex-review P1 対応・PR #569。バケット数丸め・制御バイト
        // 分が計上されない旧実装への回帰防止）。
        let pair_count = 1_000_000usize;
        let naive = pair_count.saturating_mul(std::mem::size_of::<(String, u32)>());
        let actual = hashmap_reservation_bytes(pair_count);
        assert!(
            actual > naive,
            "actual={actual} naive={naive} (バケット数丸め・制御バイト分が計上されているはず)"
        );
    }

    #[test]
    fn text_column_reservation_bytes_rejects_one_million_rows_all_empty_string_scenario() {
        // codex-review P1 指摘（PR #569・threadId: PRRT_kwDOUAKASM6fu0DC）の
        // 再現ケース: 64bit 環境で 100 万行・全 TEXT 列が空文字列の場合、
        // 文字列本体バイトはほぼ 0 で「予算上ほぼ無料」に見えるが、
        // `equality: HashMap<String, u32>` の実バケット確保（2^21 件、
        // 制御バイト込み）だけで 1 列あたり優に前提の百バイト単位を超える。
        // 旧実装（pair_count × size_of::<(String, u32)>() の単純計算）では
        // この超過分が計上されず 1 GiB 予算をバイパスし得たため、
        // 新しい見積りが hashbrown の丸めを踏まえて十分大きいことを固定する。
        let pair_count = 1_000_000usize;
        let reservation_bytes = text_column_reservation_bytes(pair_count);
        // 2^21 バケット × (エントリサイズ + 制御バイト 1) 以上であること
        // （[`hashmap_bucket_count`] の既知境界値と対応）。
        let expected_floor = (1usize << 21) * (std::mem::size_of::<(String, u32)>() + 1);
        assert!(
            reservation_bytes >= expected_floor,
            "reservation_bytes={reservation_bytes} expected_floor={expected_floor}"
        );
    }

    #[test]
    fn build_handles_empty_string_text_values_via_preallocated_accumulator() {
        // codex-review P1 指摘（PR #569 未解決分）の是正後も、空文字列 `TEXT`
        // 値を含む通常規模の入力で正しく構築できることを固定する
        // （`acc` の事前一括確保が実データの push を壊していないことの確認）。
        let path = unique_db_path("scalar-index-empty-values");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some(""), Some(""), Visibility::Public);
        insert(&storage, &ctx_a, 2, Some(""), None, Visibility::Public);
        insert(&storage, &ctx_a, 3, Some("x"), None, Visibility::Public);

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        assert_eq!(index.row_count(), 3);
        let empty_matches = index.candidates_for(&MetadataFilter_equals(&schema, "kind", ""));
        let mut empty_slots = empty_matches.expect("index available");
        empty_slots.sort_unstable();
        assert_eq!(empty_slots, vec![0, 1]);
        assert_eq!(
            index.candidates_for(&MetadataFilter_equals(&schema, "kind", "x")),
            Some(vec![2])
        );
    }

    #[test]
    fn per_column_accumulator_reservation_bytes_scales_with_row_count() {
        // acc: Vec<(String, u32)> の確保上限は列あたり高々 row_count 件
        // （1 行につき列あたり高々 1 値）。タプル全体のサイズ（アラインメント
        // 詰め物込み）で計上されることを固定する（codex-review P1 対応・
        // PR #569 未解決分）。
        assert_eq!(per_column_accumulator_reservation_bytes(0), 0);
        let row_count = 1_000;
        assert_eq!(
            per_column_accumulator_reservation_bytes(row_count),
            row_count.saturating_mul(std::mem::size_of::<(String, u32)>())
        );
    }

    #[test]
    fn per_column_accumulator_reservation_bytes_budget_rejects_before_row_scan() {
        // 100 万行 × 全列空文字列のような「文字列本体はほぼ無料だが acc 自体の
        // 確保容量が大きい」入力で、確保**前**の予算検証がバイパスされない
        // ことを固定する（codex-review P1 指摘: scalar_index.rs:343）。
        let huge_row_count = MAX_SCALAR_INDEX_BYTES; // 明らかに上限を超える確保容量になる行数
        let reservation_bytes = per_column_accumulator_reservation_bytes(huge_row_count);
        assert!(reservation_bytes > MAX_SCALAR_INDEX_BYTES);
        assert!(matches!(
            check_scalar_index_budget(0, reservation_bytes),
            Err(ScalarIndexBuildError::TooLarge)
        ));
    }

    #[test]
    fn text_column_index_approx_heap_bytes_counts_reserved_capacity_not_len() {
        // codex-review P1 対応（PR #569）: `values`/`offsets`/`slots`/`equality`
        // は重複排除前の pair_count 件分を一括確保するため、重複が多い列では
        // `len()` が `capacity()` を大きく下回る。この回帰テストは `len()` を
        // 使う実装（旧実装）だと小さく計上されるが `capacity()` ベースの実装
        // では確保容量分がきちんと計上されることを固定する。
        let pair_count = 1_000usize;
        let mut values: Vec<String> = Vec::with_capacity(pair_count);
        values.push("dup".to_string());
        let mut offsets: Vec<u32> = Vec::with_capacity(pair_count.saturating_add(1));
        offsets.push(0);
        offsets.push(1);
        let mut slots: Vec<u32> = Vec::with_capacity(pair_count);
        slots.push(0);
        let mut equality: HashMap<String, u32> = HashMap::with_capacity(pair_count);
        equality.insert("dup".to_string(), 0);
        let index = TextColumnIndex {
            values,
            offsets,
            slots,
            equality,
        };

        // `len()` ベースで計上した場合の下限（旧実装相当）。
        let len_based_lower_bound = "dup".len()
            + std::mem::size_of::<String>()
            + 2 * std::mem::size_of::<u32>()
            + std::mem::size_of::<u32>()
            + ("dup".len() + std::mem::size_of::<(String, u32)>());

        let actual = index.approx_heap_bytes();
        assert!(
            actual > len_based_lower_bound,
            "capacity ベースの計上は len ベースの計上より大きいはず: actual={actual} len_based_lower_bound={len_based_lower_bound}"
        );
        // 確保容量（pair_count 件分）に見合う規模まで計上されていることを
        // 大まかに確認する（下限は values/offsets/slots の capacity 分のみ。
        // equality の未使用バケット分は HashMap の実装依存のため含めない）。
        let capacity_floor = pair_count.saturating_mul(std::mem::size_of::<u32>()) * 2;
        assert!(
            actual >= capacity_floor,
            "actual={actual} capacity_floor={capacity_floor}"
        );
    }

    #[test]
    fn try_owned_string_copies_content_without_infallible_allocation_path() {
        let owned = try_owned_string("scalar-index").expect("small string must succeed");
        assert_eq!(owned, "scalar-index");
        assert_eq!(owned.capacity(), "scalar-index".len());
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

        // insert 前の初回 lookup は未登録によるミス（codex-review P2 対応・
        // PR #569: `misses` が実際のミス経路で加算されることの回帰）。
        let read_txn0 = storage.db().begin_read().expect("begin read");
        let before_insert = cache.lookup(&storage, &read_txn0, "docs", &ctx_a);
        assert!(before_insert.is_none());
        assert_eq!(cache.stats().misses, 1);

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
        // ヒットでは加算されないため、直前のミス 1 件のまま変化しない。
        assert_eq!(cache.stats().misses, 1);

        // 書き込みで世代を進めるとミスになり、stale eviction が発生する。
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);
        let read_txn2 = storage.db().begin_read().expect("begin read");
        let miss = cache.lookup(&storage, &read_txn2, "docs", &ctx_a);
        assert!(miss.is_none());
        assert_eq!(cache.stats().stale_evictions, 1);
        assert_eq!(cache.stats().misses, 2);
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

    /// codex-review P2 対応（PR #569）の固定テスト: `approx_heap_bytes()` は
    /// [`ScalarIndex::build`] 完了時に確定した `approx_bytes` フィールドを
    /// 返すだけの O(1) アクセサであり、実走査（`compute_approx_heap_bytes`）
    /// と常に一致する。`insert` が書き込みロック保持中に既存索引すべてを
    /// 再走査しなくなったことを、両者の値が同一であり続けることで間接的に
    /// 固定する（`insert` が誤った値を積み上げていればここで乖離する）。
    #[test]
    fn approx_heap_bytes_matches_build_time_computed_value_and_is_not_recomputed() {
        let path = unique_db_path("scalar-index-approx-bytes-cached");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let ctx_a = ctx("tenant-a");
        insert(&storage, &ctx_a, 1, Some("x"), None, Visibility::Public);
        insert(&storage, &ctx_a, 2, Some("y"), None, Visibility::Public);

        let (snapshot, schema) = snapshot_from(&storage, &ctx_a);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let cached = index.approx_heap_bytes();
        let recomputed = index.compute_approx_heap_bytes();
        assert_eq!(
            cached, recomputed,
            "approx_heap_bytes() must equal a fresh full traversal"
        );

        let cache = ScalarIndexCache::new();
        let inserted = cache
            .insert(&storage, "docs", &ctx_a, index)
            .expect("insert must succeed on fresh generation");
        // insert 後もキャッシュ登録済みインスタンスの値は build 時点のまま
        // （insert 経路が値を書き換えたり再走査結果へ差し替えたりしない）。
        assert_eq!(inserted.approx_heap_bytes(), cached);
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

    // Issue #475: `column_groups`／`slots_without_value`（`sql::group_by` の
    // WHERE なし GROUP BY 列挙形が使う API）の単体テスト。

    #[test]
    fn column_groups_enumerates_values_in_byte_order_with_correct_slots() {
        let path = unique_db_path("scalar-index-column-groups");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        // わざと非バイト順で挿入し、列挙がバイト列昇順であることを固定する。
        insert(&storage, &c, 1, Some("zulu"), None, Visibility::Public);
        insert(&storage, &c, 2, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 3, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 4, None, None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");

        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        let groups: Vec<(String, Vec<u32>)> = index
            .column_groups(kind_col)
            .expect("text column")
            .map(|(v, slots)| (v.to_string(), slots.to_vec()))
            .collect();
        let values: Vec<&str> = groups.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(values, vec!["alpha", "zulu"]);
        for (_, slots) in &groups {
            let mut sorted = slots.clone();
            sorted.sort_unstable();
            assert_eq!(slots, &sorted, "slots must be ascending");
        }
        let alpha_slots = &groups[0].1;
        assert_eq!(alpha_slots.len(), 2);

        // id=4 (kind=NULL) はどの値グループにも現れない。
        for (_, slots) in &groups {
            for &slot in slots {
                let arena_id = snapshot.arena().ids()[slot as usize];
                assert_ne!(arena_id, 4);
            }
        }
    }

    #[test]
    fn slots_without_value_returns_null_rows_only() {
        let path = unique_db_path("scalar-index-slots-without-value");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        insert(&storage, &c, 1, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 2, None, None, Visibility::Public);
        insert(&storage, &c, 3, None, None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        let null_slots = index
            .slots_without_value(kind_col)
            .expect("text column supports null slots");
        let null_ids: Vec<u64> = null_slots
            .iter()
            .map(|&slot| snapshot.arena().ids()[slot as usize])
            .collect();
        let mut sorted_ids = null_ids.clone();
        sorted_ids.sort_unstable();
        assert_eq!(sorted_ids, vec![2, 3]);
        // 昇順契約。
        let mut sorted_slots = null_slots.clone();
        sorted_slots.sort_unstable();
        assert_eq!(null_slots, sorted_slots);
    }

    #[test]
    fn slots_without_value_empty_when_no_nulls() {
        let path = unique_db_path("scalar-index-slots-without-value-empty");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        insert(&storage, &c, 1, Some("alpha"), None, Visibility::Public);
        insert(&storage, &c, 2, Some("beta"), None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let kind_col = schema
            .columns
            .iter()
            .position(|col| col.name == "kind")
            .expect("kind column");
        assert_eq!(
            index.slots_without_value(kind_col).expect("text column"),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn column_groups_and_slots_without_value_none_for_vector_column() {
        let path = unique_db_path("scalar-index-column-groups-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage);
        let c = ctx("tenant-a");
        insert(&storage, &c, 1, Some("alpha"), None, Visibility::Public);
        let (snapshot, schema) = snapshot_from(&storage, &c);
        let index = ScalarIndex::build(&schema, &snapshot).expect("build index");
        let embedding_col = schema
            .columns
            .iter()
            .position(|col| col.name == "embedding")
            .expect("embedding column");
        assert!(index.column_groups(embedding_col).is_none());
        assert!(index.slots_without_value(embedding_col).is_none());
    }
}
