# 候補集合を id マスクで直接探索し arena 複製を回避する

- **Issue**: #654（親 Issue #650。ルート #649。前提 Issue #474）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/04-behavior/data-model.md`
  TABLE-12・`docs/spec/04-behavior/rls.md`・CORE-3, CORE-4, CORE-13
- **ステータス**: 実装済み

## 背景・目的

Issue #474 は `sql::scalar_index::ScalarIndex::resolve_candidates` が絞った候補
スロットを `arena.rs::build_from_cached_rls_rows_subset` へ渡し、候補行の
embedding・id・tenant_id（`String` 複製）を**新規 `VectorArena` へ複製**してから
`SearchProvider::search` に渡していた。選択率が低い述語（crossdb fixture
`lang='ja'` で約 33%）では、候補行数 × dim ぶんの複製が毎クエリ発生する。
`SqlArenaCache`（Issue #363）の借用高速経路は SCALAR 段が恒等写像の場合
（`WHERE`・式述語・hybrid のいずれも無い）にしか効かない。

本 Issue は、キャッシュ済み `SqlArenaSnapshot` の `VectorArena` を借用したまま、
候補スロット列（狭義昇順の `Vec<u32>`）を brute-force provider へ直接渡して
探索する経路を追加し、DISTANCE 経路の arena 複製をなくす。

## 設計

### `SearchProvider::search_subset`（既定実装付き・object-safe 維持）

`kernel.rs` に部分集合探索の入力型 [`SubsetSearchInput`] と trait メソッド
`search_subset` を追加した。

```rust
pub struct SubsetSearchInput<'a> {
    pub slots: &'a [u32],   // vectors への行番号。狭義昇順・重複なし
    pub vectors: &'a [f32], // rows * dim（可視行のみ）
    pub dim: u32,
    pub query: &'a [f32],
    pub k: usize,
}

pub trait SearchProvider: Send + Sync {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError>;
    fn search_subset(&self, input: SubsetSearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        /* 既定実装: slots の行を一時バッファへ gather してから search へ委譲 */
    }
}
```

`SearchInput` を再利用せず専用型にした理由: `ids.len() * dim == vectors.len()`
という既存契約を崩さないため。本型は `ids` を持たず、返る `CandidateHit::id`
は常に `slots[i] as u64` に固定される（`sql::exec` の「provider へ渡す id は
アリーナのスロット番号」という既存契約と同型）。

既定実装（gather）があるため、trait へのメソッド追加という公開 API 変更は
既存のカスタム `SearchProvider` 実装を無変更のままコンパイル・同一結果に保つ
（`tests/kernel.rs`（本ファイル内単体テスト）
`default_search_subset_matches_overridden_implementation` で固定）。

`CpuScalarProvider::search_subset` は gather せず `vectors.get(slot*dim..)` を
直接参照する。`ParallelSearchProvider::search_subset` は既存 `search` と同型の
並列度決定・ワーカー予算調停（`acquire_execution_threads`。両メソッドが共有する
形へリファクタ）を経て、`slots` をワーカー数分に均等分割し
`search_range_by_slots`（`search_range` の「絶対行インデックス範囲」の代わりに
`slots` 配列を分割対象にした版。4 スロットブロック化＋`dot_block4` は共有）で
並列探索する。

ビット一致の根拠: スコアは行ごとに `dot`／`dot_block4`（要素ごとに `dot` と
ビット同一の契約）で計算され、`TopKSelector` はスコア降順・同点 id 昇順の全
順序選出で push 順・分割数に依存しない。候補列が狭義昇順であるため、複製経路
（連番 id）とマスク経路（元スロット id）は「同じ順序を保つ単調写像」の違いで
しかなく、同点タイブレークの相対順は不変。

### `arena.rs::filter_cached_rls_rows_subset`（複製しない候補フィルタ）

`build_from_cached_rls_rows_subset` の隣に、新規アリーナを構築せず通過した
**元スロット番号**だけを返す版を追加した。

```rust
pub(crate) fn filter_cached_rls_rows_subset<G>(
    expected_dim: u32,
    source_arena: &VectorArena,
    metadata: &[Vec<u8>],
    source_slots: &[u32],
    on_visible_row: G,
    max_rows: usize,
    max_bytes: usize,
) -> Result<Vec<u32>>
```

`on_visible_row` の第 1 引数（出力側の連番）は複製版と完全に同じ意味・同じ値に
なる（両関数とも `push_visible_row` と同じカウンタ規約を独立に踏襲しており、
`sql::exec` の `candidate_columns`（出力側連番で添字づけられる）契約はそのまま
満たされる）。索引は候補行を「絞る」ことしかできず「通す」ことはできないため、
`on_visible_row` を省略せず候補ごとに引き続き呼ぶ。

検証（行数/metadata 数一致・狭義昇順・重複なし・範囲内）・容量チェック
（`check_capacity`）は複製版と同じ契約を維持する。実際のバッファ確保
（`GrowableArenaBuffers`）は行わないため、返す `Vec<u32>` 自体の確保のみが
新たに発生する（候補件数に比例。索引が既に上限内へ絞っている）。

### `sql/exec.rs`: マスク経路の結線

「ヒットだが SCALAR 段に実質的な処理がある」分岐で、以下をすべて満たす場合に
限りマスク経路を採る:

- `index_candidate_slots` が `Some`（`CandidateResolution::Use`。
  `FallbackNoIndex`／`FallbackSelectivity`／同一性ガード不一致は従来どおり
  `build_from_cached_rls_rows_subset` の複製経路へ縮退する）
- `!is_hybrid`（`Ranking::Distance` のみ。hybrid の疎コーパス `DocId` は
  スロット番号に依存するため対象外）
- `!hnsw_subset_eligible`（HNSW opt-in 時の `Subset` 形状は Phase 2 へ申し送り。
  下記「対象外」参照）

マスク経路が発火すると:

1. `filter_cached_rls_rows_subset` で `kept_slots: Vec<u32>` を得る
2. `owned_arena` を作らず、高速経路（`cache_fast_path_eligible`）と同じく
   `cache_hit_snapshot = Some(snapshot)` として `snapshot.arena()` を借用する
3. `slot_ids`（DISTANCE 段・RLS 安全網へ渡す候補識別子列）は
   `mask_kept_slots.as_ref()` が `Some` なら `kept_slots` を `u64` へ写像した
   もの、`None`（従来どおり）なら `0..arena.ids().len()`
4. DISTANCE 段（非 HNSW 分岐）で `mask_kept_slots` が `Some` のとき
   `provider.search_subset(SubsetSearchInput{ slots: kept, vectors: arena.vectors(), .. })`
   を呼ぶ（`provider.search` ではない——`arena` が複製されない全行ぶんの行列で
   あるため、位置ベースの `SearchInput`（`ids[idx]` ではなく配列中の位置 `idx`
   で `vectors` の行を決める契約）へそのまま渡すと誤った行を候補にしてしまう）
5. 投影: `ScalarSource` に `EagerSubset { columns, kept_slots }` を追加した。
   `arena.vector(slot)`／`arena.ids().get(slot)` は `slot`＝元スロット番号で
   正しく引けるが（`arena` が snapshot そのものであるため）、
   `candidate_columns`（`on_visible_row` が出力側連番で積んだもの）は
   `kept_slots.binary_search(&slot)` で元スロット→出力側連番へ写像してから
   引く必要がある。`kept_slots` に無いスロットは fail-closed に拒否する
6. 統計: `scalar_access.cache.record_index_scan()`（従来どおり）に加え、新設
   `record_index_mask_scan()`（`ScalarIndexCacheStats::index_mask_scans`。
   `index_scans` の部分集合。テナント ID・値・件数を含まないカウンタ）を呼ぶ

`index_candidate_slots` は「metadata_filters か expr_filters が索引対応形状」
の場合にのみ `Some` になり得る（`classify_scalar_plan` が `scalar_prefilter`
込みで判定するため、DISTANCE 先行・SCALAR 事後フィルタ（`!plan.scalar_prefilter`）
のクエリでは常に `PlainScan` を返し `index_candidate_slots` は `None` のまま。
Issue #474 の既存契約）。したがって `!is_hybrid` を満たす限り、マスク経路は
`hnsw_full_visible_eligible`（`filters_empty` を要求するため構造的に両立しない）
とは競合しない。

## 対象外

- **hybrid（`Ranking::Hybrid`）の Subset 形状**: 疎コーパスの `DocId` がスロット
  番号に依存し `hybrid::hybrid_search` の `SearchInput` 契約を変えないと載せ
  られないため、従来の複製経路（`build_from_cached_rls_rows_subset`）を維持する
- **HNSW（`hnsw_subset_eligible`）の Subset 形状**: `sql::hnsw_cache::
  search_subset_or_fallback` は per-query の索引・オーバーレイ解決を行う別経路
  であり、Phase 2 として別 Issue へ申し送る
- **集計・`GROUP BY` 経路**（Issue #475 で別途結線済み）
- **前後比較実測**（Issue #655 の担当）

## テスト設計

`crates/engine/tests/scalar_index_mask_search.rs`（新規。`tests/scalar_index_prune.rs`
と同じ流儀: `unique_db_path`／`CleanupGuard`、実 `Storage`、
`engine::tenant::insert_typed_row`）。

- **`mask_path_avoids_arena_duplication_and_never_calls_full_copy_search`**:
  `search`／`search_subset` の呼び出しを記録する provider を注入し、hot 実行で
  `search_subset` が呼ばれ、渡された `vectors.len()` がスナップショット全行ぶん
  （`slots.len()` より真に大きい）であることを直接アサートする——「候補行だけを
  新規複製した」のでは説明できない、行列全体を借用したままマスクで絞った
  ことの機械的証拠
- **cold/hot ビット一致**（等価・前方一致・`id` 範囲・結合の 4 形状）: 複数列
  投影（`id, kind, path`）を含め `(id, cells)` の完全一致を固定する
  （`ScalarSource::EagerSubset` の写像が正しいことの直接検証。`SELECT id` のみ
  だと `EagerSubset` の値読み出しが実質検証されないため、既存の
  `tests/scalar_index_prune.rs` とは独立にこの観点を追加した）
- **`LIMIT` を候補件数未満**にした Top-k 境界検証
- **同点誘発コーパス**での cold/hot 一致
- **統計**: `index_scans`／`index_mask_scans` の非 vacuous な増分、
  `HINT ORDER(RLS, DISTANCE, SCALAR)`（DISTANCE 先行）ではマスク経路が
  一切消費されないこと
- **RLS**: 他テナント private 行が cold/hot いずれのマスク経路でも非混入
- **世代進行**: `INSERT` 後に再構築され、cold/hot 一致が保たれる

`crates/engine/src/kernel.rs`・`parallel_search.rs`・`arena.rs` の単体テストで、
既定実装（gather）とオーバーライド（直接参照）のビット一致、並列経路の
gather 済み `CpuScalarProvider::search` 対照一致（同点誘発コーパス含む）、
`filter_cached_rls_rows_subset` の戻り値が複製版 `build_from_cached_rls_rows_subset`
の出力アリーナと完全に対応することを固定した。

## 検証コマンド

```bash
cargo test -p engine --lib kernel:: parallel_search:: arena::
cargo test -p engine --test scalar_index_mask_search --test scalar_index_prune \
  --test scalar_index_cache --test hnsw_cache --test hnsw_hybrid_refetch \
  --test sql_explain --test sql_precision_mode
scripts/check_core_api.sh --update && bash scripts/check_core_api.sh
```

既存テスト（`tests/scalar_index_prune.rs` 等）は無変更のまま green（マスク経路
は既存クエリ形状でも自動的に発火するが、投影が `id` のみのクエリでは
`EagerSubset` の値読み出しパスまでは検証しないため、`tests/scalar_index_mask_search.rs`
を独立に追加した）。
