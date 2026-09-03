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

## 設計

### `SearchEngineKind::Hnsw(HnswParams)`

Issue の要求どおり、`HnswParams`（`hnsw.rs`。`Copy`・`Eq`）をそのまま variant
ペイロードとする（検証用の newtype は導入しない）。`build(kind)` は
infallible のまま維持し、不正パラメータの拒否は呼び出し境界
（`build_validated`）に置く——「parse, don't validate」を型ではなく
呼び出し規約で表現する:

```rust
fn build_unchecked(kind: SearchEngineKind) -> Box<dyn SearchProvider>; // pub(crate)・infallible
pub fn build_validated(kind: SearchEngineKind)
    -> Result<Box<dyn SearchProvider>, SearchEngineError>; // Hnsw のみ validate() を通す
pub fn build(kind: SearchEngineKind) -> Box<dyn SearchProvider>; // 旧公開シグネチャの互換ラッパー
```

（infallible な内部実装は初版実装の `build` から `build_unchecked` へ改称・
`pub(crate)` 化されており、旧 `pub fn build` のシグネチャは互換ラッパーの
`build` が引き継ぐ。経緯は「変更履歴」節参照）

`build_unchecked` を直接呼ぶ経路（`default_engine`）は不正値を作れない値
（`CpuScalarBruteForce`／`ParallelBruteForce`／検証済み `HnswParams`）でのみ
呼ばれる契約とし、untrusted な値から `SearchEngineKind::Hnsw` を組み立てる
crate 内部の経路（`core.rs::EngineCore::open_with_engine`／
`from_storage_with_engine`）は必ず `build_validated` を経由する。crate 外の
呼び出し元向けには、旧公開シグネチャと同一の `pub fn build` を互換ラッパーと
して維持し、`build_validated` の結果が `Err`（不正な `HnswParams`）の場合は
`.expect`/`.unwrap` に頼らず `default_kind()`（`ParallelBruteForce`）へ
fail-closed にフォールバックする（経緯は「変更履歴」節参照）。

### `FromStr`／設定文字列パーサは追加しない

計画段階では `Display`／`FromStr` の両方（`hnsw(m=...,ef_construction=...,
ef_search=...)` 形式）を検討したが、`FromStr` の呼び出し元（wire-server
CLI・`EXPLAIN`）はいずれも本 Issue のスコープ外であり、実装時点で呼び出し元が
存在しない untrusted 文字列パーサを production へ追加しないという判断により
`Display` のみを実装した。`FromStr` は呼び出し元が実際にできる #411 以降で
必要になれば追加する。

### `SearchEngineError`

```rust
#[non_exhaustive]
pub enum SearchEngineError {
    InvalidHnswParams(crate::hnsw::HnswError),
}
```

`wire_code()`（SQLSTATE 風コード `22023` を返す案）は導入していない。`22023` は
TASK-101（RECOVER-10）が既に `error_format::ErrorClass::
OperationIdContentMismatch` として ERR-2 表へ登録済みのコードであり、ERR-2 の
「分類 ⇔ `wire_code` 一意対応」契約（`wire_codes_are_pairwise_distinct` テスト）
を壊さずに本 variant 専用の新分類を `ErrorClass` へ追加することはできない。
本 Issue は wire／SQL 表層への露出を持たない（`build_validated` の呼び出し元は
`core.rs` の 2 関数のみ）ため、`ErrorClass` への正式登録・`error_response.rs`
への伝播・`wire_code()` の追加は spec 側のビヘイビア ID 確定後の別タスクへ
申し送る（経緯は「変更履歴」節参照）。

### `HnswSearchProvider`（`hnsw/provider.rs`）: 全件 brute-force フォールバック

```rust
pub struct HnswSearchProvider { /* private: HnswParams, ParallelSearchProvider */ }
impl HnswSearchProvider {
    pub fn new(params: HnswParams) -> Result<Self, crate::hnsw::HnswError>; // 自身で validate
    pub fn params(&self) -> HnswParams;
    pub fn effective_ef(&self, k: usize) -> usize; // ef_search.max(k).min(MAX_EF)。
    // k 自体はクランプしない — untrusted な k の上限保証は
    // HnswIndex::search 自身の k > MAX_EF fail-closed 検証が担う（「変更履歴」節参照）。
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
self.effective_ef(k), scratch)`）を呼ぶ際に使う契約関数として、構築時
パラメータ `ef_search` を `MAX_EF` へクランプする役割のみを本タスクで先に
固定した（`k` 自体はクランプしない。untrusted な `k` の上限保証は
`HnswIndex::search` 自身の `k > MAX_EF` fail-closed 検証が担う。詳細・
訂正経緯は「変更履歴」節「`HnswSearchProvider::effective_ef` の契約訂正」
参照）。

### `core.rs::EngineCore` の opt-in 構築 API

- フィールド追加: `search_engine_kind: Option<SearchEngineKind>`
- `EngineCore::open_with_engine(path, kind) -> Result<Self, CoreError>`
- `EngineCore::from_storage_with_engine(storage, kind)
  -> Result<Self, SearchEngineError>`
- `EngineCore::search_engine_kind(&self) -> Option<SearchEngineKind>`
- `EngineCore::open(path)` は `open_with_engine(path,
  search_engine::default_kind())` へ委譲するよう変更（既定 provider・
  既定エンジンの動作は不変。`search_engine_kind()` が常に
  `Some(ParallelBruteForce)` を返すようになった点のみが観測可能な差分）
- `with_provider`／`from_storage`（既存 API・シグネチャ不変）は `kind` を
  受け取らないため `search_engine_kind()` は常に `None`

4 つの構築関数（`with_provider`・`open_with_engine`・`from_storage`・
`from_storage_with_engine`）が共有する 13 フィールドの struct literal を
`EngineCore::assemble(storage, provider, search_engine_kind)` へ集約した
（以前は `with_provider`／`from_storage` の 2 箇所に同一の struct literal が
重複していた）。

### `CoreError::InvalidSearchEngine`（破壊的変更）

`open_with_engine` の失敗（不正な `HnswParams`）を伝える先として、
`CoreError`（`#[non_exhaustive]` を付けない既存方針。`QueryPlannerUnavailable`・
`EmbedderUnavailable` 等の追加と同じ前例）へ `InvalidSearchEngine
(SearchEngineError)` variant を追加した。既存の網羅的 `match` を壊す破壊的
変更であることは `CoreError` 冒頭ドキュメントの既存注記のとおりで、
`crates/engine/api/core_api.snapshot` を `scripts/check_core_api.sh --update`
で更新しコミットに含めた。

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
- `HnswSearchProvider` 単体の入力検証・順序規約・`CpuScalarProvider` との
  bit 単位一致は `crates/engine/src/hnsw/provider.rs` の in-module テストと
  `crates/engine/tests/hnsw_provider.rs`（クレート外部公開 API のみで検証）の
  双方で固定

## 変更履歴

上記「設計」節は最終実装の状態を記す（初版実装からの差分を含めて反映済み）。
本節は初版実装（Issue #407 初回 PR）からの変更点と、その理由をレビュー対応の
記録として残す（コード例は最終実装で上書きされているため、本節のコード例は
あくまで「そのラウンド時点での変更差分」の記録であり、最終シグネチャは
「設計」節を参照する）。

### codex-review P1 対応（PR #433 初回ラウンド）

初版実装には 2 件の P1 指摘があり、いずれも対応済み。

1. **`build` の公開性**: `SearchEngineKind::Hnsw(HnswParams)` が `pub` である
   以上、`build(kind)` も `pub` のままだと外部 crate が未検証の `HnswParams`
   を直接渡して不正値を保持した provider を構築できてしまい、「`build_validated`
   を経由する限り不正値は到達しない」という契約と矛盾していた。`build` を
   `pub(crate)` へ変更し、crate 外から provider を構築する経路を
   `build_validated` の 1 本へ絞った。
2. **`HnswSearchProvider::new` の公開性**: `crate::hnsw` は公開モジュール
   （`lib.rs::pub mod hnsw`）のため、`search_engine::build_validated` を経由
   しない `HnswSearchProvider::new(params)` の直接呼び出しも外部から到達し、
   (1) と同じ問題を作れた。`new` は `pub` のまま維持しつつ（`hnsw_provider.rs`
   の既存契約テストが「外部利用者と同じ到達性」を意図的に検証しているため）、
   `Self::new(params) -> Result<Self, HnswError>` へ変更し `HnswParams::validate`
   を内部で通すようにした——検証を `search_engine.rs` 側の呼び出し規約ではなく
   provider 自身の契約として持たせ、どちらの経路から構築しても不正値を拒否する。
3. **`wire_code()` の削除**: `SearchEngineError::wire_code()` が返していた
   `22023` は TASK-101（RECOVER-10）の `ErrorClass::OperationIdContentMismatch`
   が既に占有する値の流用であり、ERR-2 の「分類 ⇔ `wire_code` 一意対応」契約
   （`error_format.rs::wire_codes_are_pairwise_distinct`）の外側でコードが衝突
   していた。本 Issue は wire／SQL 表層への露出を持たないため、`wire_code()`・
   `INVALID_ENGINE_SPEC_WIRE_CODE` を削除し公開しないことにした。正式な
   `ErrorClass` 登録は spec 側のビヘイビア ID 確定後の別タスクへ申し送る
   （変更なし）。

対応後の公開シグネチャは「設計」節参照。

### `build` の可視性変更は正式な破壊的変更（codex-review P1 指摘・PR #433 2 巡目）

上記 1. で `build` を `pub` → `pub(crate)` へ変更した際、互換ラッパー
（旧シグネチャの `pub fn build` を残す）か正式な破壊的変更としての告知かの
どちらも行っていなかった（`main` では `build` は `pub fn` であり、`SearchEngineKind::
Hnsw` 追加以前から存在する非 Hnsw 用途の呼び出し元にも影響しうる可視性変更
だった）。

互換ラッパーは採らない: `build` は「呼び出し元が事前検証済みの値を渡す」
ことを前提にした infallible 契約であり、`pub` のまま残すと外部呼び出し元が
未検証の `HnswParams` を直接 `SearchEngineKind::Hnsw` へ詰めて渡せてしまい、
`HnswSearchProvider::new` の検証を経ない構築経路（`.expect(...)` によるパニックへ
帰結しうる経路）を公開 API として残すことになる。`.claude/rules/coding-rust.md`
の「受信データ経路での `unwrap`/`expect` 禁止」の精神に反するため、正式な
破壊的変更として次のとおり確定した:

- `search_engine.rs` モジュールドキュメントへ「破壊的変更（`build` の公開性）」
  節を追加し、変更理由と互換ラッパーを採らない判断を明記した
  （`crates/engine/src/search_engine.rs` 冒頭）
- 本 Issue（#407）が新設する opt-in API（`SearchEngineKind::Hnsw`・
  `build_validated`・`open_with_engine`／`from_storage_with_engine`）の一部として
  行う変更であり、既存 spec ビヘイビア ID に対応する変更ではない
  （`build` 自体はビヘイビア定義を持たない実装内部の構築関数のため）
- `crates/engine/api/core_api.snapshot`（`scripts/check_core_api.sh`）の追跡対象は
  `VectorCore`／`SearchProvider` trait とその参照型に限られ `search_engine::build`
  のような自由関数は対象外のため、本スナップショットの更新は不要（対象範囲は
  `scripts/check_core_api.sh` 冒頭コメント参照）

### `HnswSearchProvider::effective_ef` の契約訂正（codex-review P2 指摘・PR #433 2 巡目）

`effective_ef` のドキュメンテーションコメントは「untrusted な `k` が `MAX_EF`
を超えて索引側へ渡らないようにクランプする」と記していたが、実装は
`self.params.ef_search.max(k).min(MAX_EF)`——**`ef_search`（構築時パラメータ）
側を `MAX_EF` へクランプするだけで `k` 自体には触れない**。`#408` の契約どおり
`HnswIndex::search(query, k, self.effective_ef(k), scratch)` と呼ぶと、
`HnswIndex::search` 内部で `ef.max(k)` が計算され、`effective_ef` が返した
クランプ済み `ef` は `k` が大きければ再び `k` まで戻る。

ただし `HnswIndex::search` は `ef.max(k)` を計算する**前**に `k > MAX_EF` を
fail-closed で拒否する（`hnsw.rs::HnswIndex::search` 既存実装）ため、
untrusted な `k` に対する実際の上限保証はこの拒否が担っており、現状の呼び出し
経路（#408 未実装のため本 Issue 時点では到達しない）でも資源消費上の実害はない。
ただし `effective_ef` のドキュメンテーションコメントが実装しない契約を主張して
いた点は誤りのため、次のとおり訂正した:

- `effective_ef` は「`ef_search` を `MAX_EF` へクランプするだけで `k` には触れない」
  ことを明記
- untrusted な `k` の上限保証は `HnswIndex::search` 自身の `k > MAX_EF` 検証が担う
  ことを明記し、`effective_ef` 側の記述と重複しない形で「設計」節・
  `hnsw/provider.rs` モジュールドキュメントへ反映
- production コードの挙動（クランプ計算式）自体は変更していない
  （ドキュメンテーションコメントのみの訂正）

### `build` の公開互換ラッパーへの再変更（codex-review P1 指摘・PR #433 3 巡目）

2 巡目で確定した「正式な破壊的変更（互換ラッパーを採らない）」判断は、
AGENTS.md「公開 API・エラー契約の互換性（P1）」——公開 API の破壊的変更は
spec 側の対応する定義変更と対にする規約——を満たしていなかった（`build` は
`docs/spec` のビヘイビア ID に対応しない実装内部の構築関数であり、対にすべき
spec 側変更が存在しない）ことを指摘され、破壊的変更を伴わない方式へ再度
変更した。

- infallible な内部実装（旧 `build`）を `build_unchecked`（`pub(crate)`）へ
  改称し、`SearchEngineError` を経由しない不正値の到達を型でなく命名で
  明示する
- 旧 `pub fn build(SearchEngineKind) -> Box<dyn SearchProvider>` と同一
  シグネチャの `build` を公開互換ラッパーとして維持し、外部 crate の既存
  呼び出しをコンパイル可能に保つ
- `build`（互換ラッパー）は内部で `build_validated` を呼び、拒否された
  `HnswParams`（`m=0` 等）に対しては `.expect`/`.unwrap` によるパニックへ
  頼らず `default_kind()`（`ParallelBruteForce`）へ fail-closed に
  フォールバックする——2 巡目で懸念していた「未検証の到達がパニックへ
  帰結する」経路を、パニックではなく安全な既定エンジンへの縮退として解消した
- `crates/engine/src/search_engine.rs` の
  `build_compat_wrapper_falls_back_on_invalid_hnsw_params_without_panicking`・
  `build_compat_wrapper_accepts_valid_hnsw_params` で、旧シグネチャでの
  呼び出し可能性（コンパイル可能性）とフォールバック挙動を固定した

2 巡目の変更履歴（直前の節）は判断の推移の記録として残し、削除・書き換えは
行わない。

### `open_with_engine` の検証順序訂正と `effective_ef` 記述の再訂正（codex-review P2 指摘・PR #433 4 巡目）

- `EngineCore::open_with_engine` が `Storage::open`（ファイルオープン・ロック
  取得、場合により空 DB ファイル作成を伴う）の**後**に `build_validated(kind)`
  を実行しており、不正な `HnswParams` でも先にストレージ側の副作用が発生して
  いた（fail-closed 方針との不整合）。検証順序を入れ替え、`build_validated`
  を `Storage::open` より前に呼ぶよう修正した（`SearchEngineKind` は `Copy`
  のため所有権の問題は生じない）。`from_storage_with_engine` は呼び出し元が
  既に開いた `Storage` の所有権を受け取る設計のためこの問題は無く、変更対象
  外
- 「設計」節（`### HnswSearchProvider`）内の `effective_ef` 説明が、直前の
  「`effective_ef` の契約訂正」節（本節の直前）での訂正後も「`k` を `MAX_EF`
  でクランプする」という誤った記述のまま残っていた。`ef_search`（構築時
  パラメータ）を `MAX_EF` へクランプするだけで `k` 自体はクランプしない旨・
  `k` の上限保証は `HnswIndex::search` 自身の `k > MAX_EF` fail-closed 検証が
  担う旨を明記し、97 行目・236〜253 行目の記述と整合させた
- production コードの変更は `open_with_engine` の検証順序入れ替えのみ
  （`effective_ef` のクランプ計算式自体は変更していない）

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
