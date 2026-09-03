# `SearchEngineKind::Hnsw` の opt-in 結線（設計記録）

- ステータス: **実装済み**（`crates/engine/src/search_engine.rs`・
  `crates/engine/src/hnsw/provider.rs`・`crates/engine/src/core.rs`）
- 対応: Issue #407（`feat(engine): SearchEngineKind::Hnsw variant と build 結線・opt-in 設定`）
- 前提: Issue #404（`docs/design/hnsw-graph-construction.md`）・#405
  （`docs/design/hnsw-search.md`）・#406（`docs/design/hnsw-parallel-build.md`）
- 親: Issue #402（Phase 3: ANN 索引の opt-in 採用）・#403
  （`docs/design/ann-index-adoption.md` を Accepted 化）

## 背景・範囲

`docs/design/ann-index-adoption.md`（Issue #367・CORE-9・CORE-10・TASK-132）の
B 案（条件付き opt-in・自作 HNSW・依存追加なし）の Phase 3 分解タスクのうち、
本タスクは「エンジン選択の opt-in 経路」——`search_engine.rs::SearchEngineKind`
への `Hnsw` variant 追加・`build` 結線・`core.rs` の構築時 opt-in——を扱う。
既定エンジン（`ParallelBruteForce`）は不変で、`EngineCore::open_with_engine`／
`from_storage_with_engine` を明示的に呼んだ場合のみ選択される。

対象外（申し送り）:

- テーブル単位カタログ属性による opt-in・`wire-server` CLI へのオプション露出
  （ADR「判断確定後のスコープ外」節）
- 索引の実利用（世代整合キャッシュ・未索引差分の brute-force 併用・Top-k
  マージ）: #408
- RLS 事前フィルタとの切替: #409・#410
- `EXPLAIN` へのエンジン種別露出: #411
- Recall ゲート接続: #412

他案の検討経緯（`build` の公開性・互換ラッパーの要否・エラー型設計の代替案等）
は private spec 側（ポインタ表記）に記録する。

## 設計

### `SearchEngineKind::Hnsw(HnswParams)`

`HnswParams`（`hnsw.rs`。`Copy`・`Eq`）をそのまま variant ペイロードとする
（検証用の newtype は導入しない）。`build(kind)` は infallible のまま維持し、
不正パラメータの拒否は呼び出し境界（`build_validated`）に置く——
「parse, don't validate」を型ではなく呼び出し規約で表現する:

```rust
fn build_unchecked(kind: SearchEngineKind) -> Box<dyn SearchProvider>; // pub(crate)・infallible
pub fn build_validated(kind: SearchEngineKind)
    -> Result<Box<dyn SearchProvider>, SearchEngineError>; // Hnsw のみ validate() を通す
pub fn build(kind: SearchEngineKind) -> Box<dyn SearchProvider>; // 旧公開シグネチャの互換ラッパー
```

`build_unchecked` を直接呼ぶ経路（`default_engine`）は不正値を作れない値
（`CpuScalarBruteForce`／`ParallelBruteForce`／検証済み `HnswParams`）でのみ
呼ばれる契約とし、untrusted な値から `SearchEngineKind::Hnsw` を組み立てる
crate 内部の経路（`core.rs::EngineCore::open_with_engine`／
`from_storage_with_engine`）は必ず `build_validated` を経由する。これが HNSW
構築の正規経路であり、不正な `HnswParams` は構築時点で `SearchEngineError`
として fail-closed に拒否され、不正な状態の `EngineCore` は構築されない。

crate 外の呼び出し元向けには、旧公開シグネチャと同一の `pub fn build` を
互換ラッパーとして維持する。`build_validated` の結果が `Err`（不正な
`HnswParams`）の場合、`.expect`/`.unwrap` に頼らないだけでなく、要求した
エンジンが実際には選ばれなかったことを呼び出し元が観測できない既定
エンジンへの黙った置換（fail-open）もしない。代わりに
`search_engine::InvalidEngineProvider`（crate 内部）を返す。構築自体は
infallible な戻り値契約（`Box<dyn SearchProvider>`）を保ったまま、`search()`
が呼ばれるたびに必ず既存 variant `kernel::KernelError::WorkerPanicked` を
返す（公開・非 `#[non_exhaustive]` の `KernelError` へ新規 variant を追加する
と後方互換性を破壊するため、既存 3 variant——`DimMismatch`／
`NonFiniteQuery`／`WorkerPanicked`——のみで表現する。`WorkerPanicked` の
「検索を安全に実行できない内部状態のため、部分結果を返さず検索全体を失敗
として呼び出し元へ伝播させる」という既存の意味論が最も近いため転用する）。

拒否理由（`SearchEngineError` の `Display` 文字列）は `WorkerPanicked` が
unit variant であるため `KernelError` 経由では呼び出し元へ伝わらず、
`InvalidEngineProvider` 側にも保持しない。理由を観測したい呼び出し元は
`build_validated`（構築時にエラーを返し `SearchEngineError` の詳細を観測
できる、HNSW 構築の正規経路）を使う。

### `FromStr`／設定文字列パーサは追加しない

`Display`／`FromStr` の両方（`hnsw(m=...,ef_construction=...,ef_search=...)`
形式）を検討したが、`FromStr` の呼び出し元（wire-server CLI・`EXPLAIN`）は
いずれも本 Issue のスコープ外であり、呼び出し元が存在しない untrusted
文字列パーサを production へ追加しないという判断により `Display` のみを
実装した。`FromStr` は呼び出し元が実際にできる #411 以降で必要になれば
追加する。

### `SearchEngineError`

```rust
#[non_exhaustive]
pub enum SearchEngineError {
    InvalidHnswParams(crate::hnsw::HnswError),
}
```

`wire_code()`（SQLSTATE 風コード）は導入していない。本 Issue は wire／SQL
表層への露出を持たない（`build_validated` の呼び出し元は `core.rs` の 2 関数
のみ）ため、正式な `ErrorClass` 登録・`error_response.rs` への伝播・
`wire_code()` の追加は spec 側のビヘイビア ID 確定後の別タスクへ申し送る。

### `HnswSearchProvider`（`hnsw/provider.rs`）: 全件 brute-force フォールバック

```rust
pub struct HnswSearchProvider { /* private: HnswParams, ParallelSearchProvider */ }
impl HnswSearchProvider {
    pub fn new(params: HnswParams) -> Result<Self, crate::hnsw::HnswError>; // 自身で validate
    pub fn params(&self) -> HnswParams;
    pub fn effective_ef(&self, k: usize) -> usize; // ef_search.max(k).min(MAX_EF)。
    // k 自体はクランプしない — untrusted な k の上限保証は
    // HnswIndex::search 自身の k > MAX_EF fail-closed 検証が担う。
}
impl SearchProvider for HnswSearchProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.fallback.search(input) // ParallelSearchProvider へ委譲
    }
}
```

本 Issue 時点では `HnswIndex` を構築・保持しない。`SearchInput` は「呼び出し
ごとに集合が変わりうる可視行の縮約ビュー」（RLS フィルタ・テーブル更新の
たびに変わる）であり、`HnswIndex` は構築時点のスナップショットしか探索
できない。索引済み集合と `SearchInput` の差分を安全に判定するには世代整合
キャッシュ（`sql::sparse_cache::SparseIndexCache`〔Issue #357〕・
`sql::arena_cache::SqlArenaCache`〔Issue #363〕と同型）が要るが、これは #408
の担当。差分判定なしに索引だけを探索すると、索引構築後に追加された行・
不可視化された行を検索結果へ混入・欠落させ RLS 可視性契約を壊すため、安全側に
倒し「索引済み集合は常に空」＝全件フォールバックとして実装した。

`effective_ef` は #408 が索引探索（`HnswIndex::search(query, k,
self.effective_ef(k), scratch)`）を呼ぶ際に使う契約関数として、`ef_search.
max(k).min(MAX_EF)` を返す役割を本タスクで先に固定した。戻り値は `k` に
依存するが、`ef_search`（構築時パラメータ）側を `MAX_EF` へクランプする
だけで `k` 自体には触れない。untrusted な `k` に対する実際の上限保証は
`HnswIndex::search` 自身の `k > MAX_EF` fail-closed 検証（`ef.max(k)` を
計算する前に拒否する）が担う。

### `core.rs::EngineCore` の opt-in 構築 API

- フィールド追加: `search_engine_kind: Option<SearchEngineKind>`
- `EngineCore::open_with_engine(path, kind) -> Result<Self,
  OpenWithEngineError>`（新設の専用エラー型。次節参照）
- `EngineCore::from_storage_with_engine(storage, kind)
  -> Result<Self, SearchEngineError>`
- `EngineCore::search_engine_kind(&self) -> Option<SearchEngineKind>`
- `EngineCore::open(path)` は内部で `search_engine::default_engine()`
  （infallible）を直接使い、`search_engine_kind()` は常に
  `Some(default_kind())` を返す（既定 provider・既定エンジンの動作は不変。
  既定 kind は常に `HnswParams::validate` を要さないため失敗系が構造的に発生
  せず、`open` の戻り値型 `CoreError` は変更していない）
- `with_provider`／`from_storage`（既存 API・シグネチャ不変）は `kind` を
  受け取らないため `search_engine_kind()` は常に `None`

4 つの構築関数（`with_provider`・`open_with_engine`・`from_storage`・
`from_storage_with_engine`）が共有する 13 フィールドの struct literal を
`EngineCore::assemble(storage, provider, search_engine_kind)` へ集約した
（以前は `with_provider`／`from_storage` の 2 箇所に同一の struct literal が
重複していた）。

### `OpenWithEngineError`

`open_with_engine` の失敗（不正な `HnswParams`）は、既存 `CoreError`
（`#[non_exhaustive]` を付けない既存方針）へ variant を追加せず、独立の
戻り値型 `OpenWithEngineError`（`Storage(StorageError)` /
`SearchEngine(SearchEngineError)` の 2 variant。`CoreError` への暗黙 `From`
は用意しない）として新設した。`open_with_engine` 自体が本 Issue で新設する
API のため、新規関数の戻り値型を選ぶこと自体は破壊的変更に当たらない。

- `EngineCore::open`（既存 API）は `search_engine::default_engine()`
  （`build_unchecked(default_kind())` と同じ infallible 経路）を直接呼び、
  `open_with_engine` へは委譲しない。既定 kind は常に検証を要さないため、
  `open` の戻り値型 `Result<Self, CoreError>` は変更していない
- `EngineCore::open_with_engine` は `build_validated(kind)` を
  `Storage::open`（ファイルオープン・ロック取得を伴う）より先に呼ぶ。
  不正な `HnswParams` の場合にストレージ側の副作用を発生させない
  fail-closed の順序とするため（`SearchEngineKind` は `Copy` のため所有権の
  問題は生じない）。`from_storage_with_engine` は呼び出し元が既に開いた
  `Storage` の所有権を受け取る設計のためこの順序制約は適用されない
- `crates/engine/api/core_api.snapshot`（`scripts/check_core_api.sh`）の
  追跡対象は `VectorCore`／`SearchProvider` trait とその参照型に限られ、
  `search_engine::build` のような自由関数・`CoreError` 以外の新設エラー型は
  対象外のため、本スナップショットへの追加は不要

## フォールバック等価性・受け入れ条件の検証

- (a) 既定エンジン不変: `EngineCore::open` の `search_engine_kind()` が
  `Some(SearchEngineKind::ParallelBruteForce)` であることを
  `crates/engine/tests/search_engine.rs::hnsw_407_default_engine_kind_is_unchanged`
  で固定
- (b) opt-in 選択: `from_storage_with_engine`／`open_with_engine` で
  `Hnsw` を明示指定すると `search_engine_kind()` がその値を返し、同一入力
  （同点タイブレークを含む）で既定エンジンと Top-k が完全一致すること
  （全件フォールバック契約）を
  `hnsw_407_opt_in_engine_selected_and_matches_default_via_fallback` で検証
- 不正パラメータ（`m=1`）の fail-closed 拒否を
  `hnsw_407_invalid_params_rejected_fail_closed` で検証
- `build` 互換ラッパーが不正な `HnswParams` に対してパニックせず
  fail-closed provider を返し、`search()` が `KernelError::WorkerPanicked`
  を返すことを
  `build_compat_wrapper_returns_fail_closed_provider_on_invalid_hnsw_params_without_panicking`
  で固定
- `HnswSearchProvider` 単体の入力検証・順序規約・`CpuScalarProvider` との
  bit 単位一致は `crates/engine/src/hnsw/provider.rs` の in-module テストと
  `crates/engine/tests/hnsw_provider.rs`（クレート外部公開 API のみで検証）の
  双方で固定
- CI `core-api-check`（`scripts/check_core_api.sh`）が green であることを
  確認済み

## #408 が接続する索引経路の seam（本タスクでは実装しない）

1. 索引済み集合と `SearchInput` の差分を判定する世代整合キャッシュは、
   provider の外側（`core.rs`／`sql` 側のテーブル世代整合機構）が持つ
2. 索引側の探索は `HnswIndex::search(query, k, self.effective_ef(k),
   scratch)` を使い、`HnswSearchScratch` は呼び出しスレッドごとに呼び出し元が
   所有する
3. 索引側 hit と brute-force 側（未索引分）hit の Top-k マージは
   `kernel.rs::TopKSelector` と同じ順序規約（スコア `total_cmp` 降順・同点 id
   昇順）を保った安定マージで行う（`sort_unstable` 系は
   `scripts/check_sort_determinism.sh` が禁止）
