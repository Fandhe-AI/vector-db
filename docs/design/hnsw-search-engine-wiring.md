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
- ~~`EXPLAIN` へのエンジン種別露出: #411~~ 実装済み。`docs/design/explain-search-engine-exposure.md` 参照
- Recall ゲート接続: #412

## 設計

### `SearchEngineKind::Hnsw(ValidatedHnswParams)`（型で不正値を到達不能にする）

`SearchEngineKind::Hnsw` は `HnswParams` ではなく `hnsw.rs::ValidatedHnswParams`
（private フィールドの newtype）を保持する。`ValidatedHnswParams::new(params)`
（`HnswParams::validate` を必ず経由する）以外の経路では構築できないため、
不正な `HnswParams` を保持した `SearchEngineKind::Hnsw`・
`HnswSearchProvider`・`EngineCore` はそもそも型として存在しえない。

```rust
pub fn hnsw_kind(params: HnswParams) -> Result<SearchEngineKind, SearchEngineError>; // 唯一の検証入口
pub fn build(kind: SearchEngineKind) -> Box<dyn SearchProvider>; // infallible（Err を返す必要がない）
```

untrusted な値（設定値・外部入力）から `SearchEngineKind::Hnsw` を得たい
呼び出し元は `hnsw_kind` を使う。不正な `HnswParams` はここで
`SearchEngineError` として fail-closed に拒否され、`SearchEngineKind::Hnsw`
自体が構築されない。一度 `SearchEngineKind::Hnsw` を得れば、`build`・
`core.rs::EngineCore::open_with_engine`／`from_storage_with_engine` は
Hnsw 検証を理由に失敗しない（`open_with_engine` は `Storage::open` の失敗
のみが残る）。

### `FromStr`／設定文字列パーサは追加しない

`Display`／`FromStr` の両方（`hnsw(m=...,ef_construction=...,ef_search=...)`
形式）を検討したが、`FromStr` の呼び出し元（wire-server CLI・`EXPLAIN`）は
いずれも本 Issue のスコープ外であり、呼び出し元が存在しない untrusted
文字列パーサを production へ追加しないという判断により `Display` のみを
実装した。`FromStr` は呼び出し元が実際にできれば追加する方針だったが、
Issue #411（実装済み）の `EXPLAIN` 露出は専用の網羅 `match`（`sql/explain.rs::
engine_token`／`ann_plan_token`）で閉じた語彙へ変換するのみで、untrusted
文字列からの逆変換（`FromStr`）を必要としなかったため、依然として未追加
のまま（`docs/design/explain-search-engine-exposure.md` 参照）。

### `SearchEngineError`

```rust
#[non_exhaustive]
pub enum SearchEngineError {
    InvalidHnswParams(crate::hnsw::HnswError),
}
```

`wire_code()`（SQLSTATE 風コード）は導入していない。本 Issue は wire／SQL
表層への露出を持たない（`hnsw_kind` の呼び出し元は `core.rs` の opt-in
経路のみ）ため、正式な `ErrorClass` 登録・`error_response.rs` への伝播・
`wire_code()` の追加は spec 側のビヘイビア ID 確定後の別タスクへ申し送る。

### `ValidatedHnswParams`（`hnsw.rs`）

```rust
pub struct ValidatedHnswParams(HnswParams); // フィールド private
impl ValidatedHnswParams {
    pub fn new(params: HnswParams) -> Result<Self, HnswError>; // 唯一の構築経路
    pub fn get(&self) -> HnswParams;
}
impl Deref for ValidatedHnswParams { type Target = HnswParams; .. }
```

### `HnswSearchProvider`（`hnsw/provider.rs`）: 全件 brute-force フォールバック

```rust
pub struct HnswSearchProvider { /* private: ValidatedHnswParams, ParallelSearchProvider */ }
impl HnswSearchProvider {
    pub fn new(params: ValidatedHnswParams) -> Self; // infallible（検証済み型のみ受け取る）
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
  OpenWithEngineError>`（新設の専用エラー型。次節参照。`kind` は型で検証済み
  のため、`Err` は `Storage::open` の失敗のみに由来する）
- `EngineCore::from_storage_with_engine(storage, kind) -> Self`（`kind` が
  型で検証済みのため infallible）
- `EngineCore::search_engine_kind(&self) -> Option<SearchEngineKind>`
- `EngineCore::open(path)` は内部で `search_engine::default_engine()`
  （infallible）を直接使い、`search_engine_kind()` は常に
  `Some(default_kind())` を返す（既定 provider・既定エンジンの動作は不変）
- `with_provider`／`from_storage`（既存 API・シグネチャ不変）は `kind` を
  受け取らないため `search_engine_kind()` は常に `None`

4 つの構築関数（`with_provider`・`open_with_engine`・`from_storage`・
`from_storage_with_engine`）が共有する 13 フィールドの struct literal を
`EngineCore::assemble(storage, provider, search_engine_kind)` へ集約した
（以前は `with_provider`／`from_storage` の 2 箇所に同一の struct literal が
重複していた）。

### `OpenWithEngineError`

`open_with_engine` の失敗は、既存 `CoreError`（`#[non_exhaustive]` を
付けない既存方針）へ variant を追加せず、独立の戻り値型
`OpenWithEngineError`（`Storage(StorageError)` /
`SearchEngine(SearchEngineError)` の 2 variant。`CoreError` への暗黙 `From`
は用意しない）として新設した。`open_with_engine` 自体が本 Issue で新設する
API のため、新規関数の戻り値型を選ぶこと自体は破壊的変更に当たらない。
`kind` は型で検証済みのため、`open_with_engine` が実際に `Err` を返すのは
`Storage::open` が失敗した場合（`Storage` variant）のみで、`SearchEngine`
variant は現状の呼び出し経路では到達しない（将来 Hnsw 以外の検証を要する
variant が `SearchEngineKind` に追加された場合に備えて型は残す）。

- `EngineCore::open`（既存 API）は `search_engine::default_engine()`
  （`build(default_kind())` と同じ infallible 経路）を直接呼び、
  `open_with_engine` へは委譲しない
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
- 不正パラメータ（`m=1`）の fail-closed 拒否を、唯一の検証入口
  `search_engine::hnsw_kind` が `Err` を返すことで
  `hnsw_407_invalid_params_rejected_fail_closed` で検証（`SearchEngineKind::
  Hnsw` 自体に不正値が到達しえないため、`open_with_engine`／
  `from_storage_with_engine` 側でのアサーションは不要になった）
- `HnswSearchProvider` 単体の入力検証・順序規約・`CpuScalarProvider` との
  bit 単位一致は `crates/engine/src/hnsw/provider.rs` の in-module テストと
  `crates/engine/tests/hnsw_provider.rs`（クレート外部公開 API のみで検証）の
  双方で固定
- CI `core-api-check`（`scripts/check_core_api.sh`）が green であることを
  確認済み

## #408 が接続した索引経路の seam（実装済み）

上記 3 点は Issue #408（`sql::hnsw_cache::HnswIndexCache`。詳細は
`docs/design/hnsw-generation-cache.md` 参照）で SQL 表層の `Ranking::Distance`
（フィルタなし）クエリに限り接続済み:

1. 索引済み集合と `SearchInput` の差分を判定する世代整合キャッシュ
   （`sql::hnsw_cache::HnswIndexCache`。`(table, ctx)` × テーブル単位世代キー）を
   `HnswSearchProvider`（本 provider）の外側、`sql::exec::
   execute_statement_with_cache` から `core.rs::EngineCore::hnsw_state` 経由で
   接続した。`HnswSearchProvider::search` 自体（本ファイル）は無変更のまま
   （常に `ParallelSearchProvider` へ委譲する全件フォールバック）——索引経路は
   `sql::hnsw_cache` が provider の**外側**から `HnswIndex::search` を直接呼ぶ形
   （下記 2.）で実現し、`SearchProvider` trait には一切触れない
2. 索引側の探索は `HnswIndex::search(query, k, self.effective_ef(k),
   scratch)` を使い、`HnswSearchScratch` は呼び出しスレッドごとに
   `thread_local!` で所有する
3. 索引側 hit と brute-force 側（未索引分）hit の Top-k マージは
   `kernel.rs::TopKSelector` と同じ順序規約（スコア `total_cmp` 降順・同点 id
   昇順）を保った `sort_by`（安定ソート）で行う（`sort_unstable` 系は
   `scripts/check_sort_determinism.sh` が禁止。CI green を確認済み）

Rust API（`VectorCore::search`）・フィルタ付きクエリ・hybrid クエリは本 seam を
経由せず、`HnswSearchProvider::search` の全件フォールバックのまま（段階化は
維持。`docs/design/hnsw-generation-cache.md`「スコープ外・申し送り」参照）。
