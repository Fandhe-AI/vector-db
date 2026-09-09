# ADR: HNSW Subset 形状の Overlay／BFS 世代整合キャッシュ（Issue #678）

- ステータス: Proposed（採否は Issue #677（案 A 適用後の前後比較実測）の結果を
  条件としてオーナー判断。本 ADR 自体は案 B（本キャッシュ）の設計検討・実装
  タスク分解案の記録に留め、Accepted 化は別コミットで行う）
- 対応: Issue #678（親 #673「Phase 1: HNSW Subset 縮退経路の候補 id マスク経路
  への合流」→ ルート #672。兄弟 #676「perf: 縮退時の id マスク経路委譲」・
  #677「test: 前後比較」）
- 関連ポインタ: `docs/spec/04-behavior/search.md`（CORE-9・CORE-10）・
  `docs/spec/05-tasks.md`（TASK-132）。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/sql/hnsw_cache.rs`（`IndexedBase`・`Overlay`・
  `HnswIndexCache`・`prepare_subset`・`search_subset_or_fallback`・
  `search_with_overlay`・`record_overlay_for`・`traversal_regime_for`・
  `below_full_scan_ratio`）・`crates/engine/src/sql/exec.rs`（`hnsw_subset_eligible`・
  `classify_ann_plan` 分岐）・`crates/engine/src/sql/scalar_plan.rs`
  （`classify_scalar_plan`）・`crates/engine/src/sql/scalar_index.rs`
  （`ScalarIndex`・世代整合キャッシュの参考実装）・
  `crates/engine/src/sql/arena_cache.rs`（`SqlArenaCache`・世代整合キャッシュの
  参考実装）・`crates/engine/src/hnsw.rs`（`HnswIndex::is_mask_fully_reachable_with`・
  `NodeMask`・`HopMode`）・`crates/engine/src/sql/hnsw_hybrid.rs`
  （`HnswDenseProvider`）
- 関連 doc: `docs/design/hnsw-generation-cache.md`（Issue #408。`FullVisible`
  世代整合キャッシュの原設計）・`docs/design/hnsw-rls-cardinality-switch.md`
  （Issue #409・#659。`Subset` per-query 写像の非登録理由・`RowKey` インターン化
  の申し送り）・`docs/design/scalar-index-mask-search.md`（Issue #654・#676 の
  候補 id マスク経路）・`docs/design/scalar-index-generation-cache.md`
  （Issue #473。`ScalarIndexCache` は `(table, ctx)` × テーブル世代でキャッシュ
  し述語はキーに含まない――`MetadataFilter`／`id_bounds` の述語表現・
  正規化ロジックの先例として参照する。述語キー付きキャッシュ自体の先例
  ではない）・
  `docs/design/benchmark-judgement-policy.md`（Issue #462。前後比較実測の
  計測規約）・`docs/design/ann-index-adoption.md`（Issue #367・#403。HNSW
  採否 ADR 本体・「事後フィルタ不採用」の判断）
- 検証コード: なし（docs 専任。production コード〔`crates/engine/src/`〕は
  本 Issue の範囲では無変更）

## 1. 概要・現状の機構（コード事実）

HEAD `997cf00` 時点の実装事実を以下に整理する。行番号は当該コミット時点の
参考値であり、以降の変更で前後する可能性がある。

| 段階 | 場所 | 内容 |
| --- | --- | --- |
| 適用判定 | `sql/exec.rs`（`hnsw_subset_eligible`。`classify_ann_plan` の結果が `AnnPlan::HnswSubset` かつ `!is_hybrid`） | ANN opt-in・非 hybrid・非 precision・SCALAR 事前フィルタありの DISTANCE クエリを `Subset` 形状に分類する |
| 索引解決 | `sql/hnsw_cache.rs::search_subset_or_fallback` → `prepare_subset` | `HnswIndexCache::lookup` を呼び、`Lookup::Ready(base, overlay)` の `overlay`（`FullVisible` 形状用）を**捨てて** `base`（`IndexedBase`）のみ使う |
| 早期縮退 | `prepare_subset` 内 `below_full_scan_ratio(arena.len(), base.index.len(), ratio)` | 可視候補数が索引ノード数の `full_scan_ratio` 未満なら `Overlay::compute` を呼ばずに `PreparedHnswSearch::PlainScanBelowRatio` を返す（Issue #488） |
| per-query 写像・マスク構築 | `Overlay::compute`（`hnsw_cache.rs`） | 部分集合アリーナの全スロットについて `(tenant.to_string(), id)` の `RowKey` を生成 → `base.key_to_node: HashMap<RowKey, u32>` 引き → `node_matches` によるベクトル一致確認 → `visible_mask: NodeMask` 構築 |
| BFS 連結性判定 | `Overlay::compute` 内 `traversal_regime_for` → `HnswIndex::is_mask_fully_reachable_with`（`hnsw.rs`。層 0 BFS） | `OneHop`／`TwoHop` regime のとき `mask_splits_graph` を判定する |
| キャッシュ登録 | `Overlay::compute` の戻り値 | `record_overlay_for` へ**登録しない**（後述 §5） |
| 探索・縮退 | `search_with_overlay` | `regime == PlainScan` または `mask_splits_graph` なら `full_scan_with_arena`（部分集合アリーナの全件 brute-force）へ縮退する |
| hybrid 密側 | `sql/hnsw_hybrid.rs::HnswDenseProvider` | 同じ `prepare_subset` をクエリあたり 1 回呼ぶ（`k` 非依存のため複数ラウンドで再利用） |

特筆すべき 3 点:

1. `Lookup::Ready` は `FullVisible` 用 overlay を保持しているが `Subset` 経路は
   それを一切参照しない（`base` の `IndexedBase` のみ使う）
2. `Subset` の `Overlay::compute` 結果はクエリ終了後に破棄され、次クエリで
   再計算される（世代が変わらなくても）
3. hybrid 密側も同一の `prepare_subset` を経由するため、本キャッシュが効けば
   両経路（DISTANCE 直・hybrid 密側）が同時に恩恵を受ける

## 2. 案 A（#676）適用後のコスト分解

Issue #676（縮退時の id マスク経路委譲）は `hnsw_subset_eligible` 経路で
`exec.rs` が行っていた**部分集合アリーナの複製**（`build_from_cached_rls_rows_subset`）
を、#654 の候補 id マスク経路（`filter_cached_rls_rows_subset` ＋
`SearchProvider::search_subset`）へ合流させ、arena 借用のまま探索対象を絞る。

これにより per-query コストは 2 系統に整理される:

- **(a) 写像・マスク構築**: `Overlay::compute` の `RowKey` 生成（テナント
  文字列の `String` 複製を含む）・`key_to_node` の `HashMap` 引き・
  `node_matches` によるベクトル一致確認・`NodeMask` 構築
- **(b) BFS 連結性判定**: `is_mask_fully_reachable_with`（受理ノード数 ×
  平均次数に比例する層 0 BFS）

**#676 が除去するのは arena 複製のみであり、(a)(b) はいずれも #676 適用後も
per-query のまま残る。** 本 ADR が対象とするのはこの残余コスト（(a)(b)）であり、
arena 複製ではない。

## 3. 案 B の分割: B-1（写像の導出）と B-2（BFS 判定のキャッシュ）

案 B は性質の異なる 2 つの最適化に分割できる。

### 3.1 B-1（述語非依存・キー不要）

(a) の写像構築コストは、以下の構造的事実が成立する場合、**述語に依存しない
情報だけから O(1)〜O(|候補|) で導出できる可能性がある**（実装前に検証すべき
検査項目として提示する。以下は結論ではなく仮説）:

- `IndexedBase::build`（`hnsw_cache.rs`）は `SqlArenaCache` のスナップショット
  （ctx 可視全集合）から構築され、構築世代では
  `slot_of_node[node] == node` が成立する（`Overlay` の
  `slot_of_node` フィールドのドキュメンテーションコメント参照）
- `FullVisible` 形状の overlay（世代あたり 1 回・`record_overlay_for` 登録済み）
  が持つ `slot_of_node: Vec<u32>`（索引ノード → 現行アリーナスロット）を反転
  した `node_of_slot: Vec<u32>`（スナップショットスロット → ノード。世代あたり
  1 回の導出で済む）があれば、#676 適用後の Subset 探索がスナップショット
  arena（借用）上の候補マスクで動作するという前提のもとでは、`Subset` の
  `visible_mask` は「候補スロット集合 ∩ 写像可能ノード」という整数演算の
  組み合わせで導出でき、`RowKey` の文字列複製・`HashMap` 引きが不要になる

成立条件（検証タスクとして §10 に列挙）:

- (i) #676 適用後、Subset 経路の探索対象が `IndexedBase` 構築時と同一の
  スナップショット arena（借用）であること（複製された別 arena ではないこと）
- (ii) 現世代の `FullVisible` overlay が `Lookup::Ready` で得られること
  （`NeedOverlay` の場合は `FullVisible` 経路と同じ手順で 1 回計算・登録して
  よい——可視全集合に対する計算であり、特定の述語に依存しないため
  `needs_rebuild` を汚染しない）

B-1 が成立する場合、`RowKey`（`(String, u64)`）インターン化（Issue #659 の
申し送り）は **`Subset` 経路については moot になる**（後述 §8）。

### 3.2 B-2（述語依存）

(b) の BFS 連結性判定（`mask_splits_graph`／`regime`。必要なら `visible_mask`
本体も）を、WHERE 述語をキーの一部とする独立キャッシュとして保持する。
B-1 が成立しない場合でも B-2 単独で BFS コストは償却できる。

## 4. キー設計（B-2）

### 4.1 構成要素

`(table, PolicyContext 完全一致, テーブル世代, 述語正規化キー)`。

- テーブル世代は `catalog::table_generation_in_txn(read_txn)` を用いる
  （`Storage::table_generation` ではない——`ScalarIndex` の世代テストが固定する
  区別と同じ注意点。`docs/design/scalar-index-generation-cache.md` 参照）
- `k`（LIMIT）には依存しない。hybrid 密側の複数ラウンド（`dense_fetch_k`
  倍増）で同一キャッシュを共有できる
- `full_scan_ratio`／`acorn_max_visible_ratio`／`sparse_visited_max`
  （Issue #657 の CLI opt-in）は `ValidatedHnswParams` としてエンジン構築時に
  静的確定し、プロセス内で不変であるためキーに含めない。この前提は
  プロセス起動後にこれらのパラメータを動的に変更する経路が存在しないことに
  依存する（現状そのような経路はない）

### 4.2 述語正規化

`hnsw_subset_eligible` は `classify_scalar_plan != PlainScan` を要求しない
（`exec.rs` の適用条件は SCALAR 事前フィルタの有無のみを見る）。したがって
`Subset` 経路は `ScalarIndex`（Issue #473〜#475）が候補削減に使う索引対応述語
（`Equality`／`Prefix`／`id` 範囲）だけでなく、索引非対応の述語（`Builtin`・
`WasmCall`・`VectorRef`・`f64` リテラルを含む `BoundExpr` のカスタム
`PartialEq`）も対象に含む。

正規化キーが well-defined に定義できるのは索引対応形状
（`metadata_filters` を `column_index` 昇順に整列した上での
`(column_index, FilterOp, 値)` の並び、`IdPredicate` の上下限）のみである。

2 方式を比較する:

| 方式 | 内容 | 長所 | 短所 |
| --- | --- | --- | --- |
| K1 | B-2 の対象を `classify_scalar_plan != PlainScan`（索引対応形状）に限定し、`PlainScan` 形状（索引非対応述語を含む）は従来どおり per-query 計算のまま | `ScalarIndex` が候補削減で既に扱う述語表現（`metadata_filters`／`id_bounds`。`ScalarIndexCache` 自体は述語をキーに含まないため、共有できるのはこの述語表現とその正規化ロジックのみ）を土台にでき、well-defined 性が保証される――ただし述語列を並び替え・直列化してキャッシュキー化する処理自体は B-2 で新規に設計する必要がある | 索引非対応述語（`Builtin`／`WasmCall` 等）を伴うクエリはキャッシュされない・キー化の実装コストは `ScalarIndex` に既存キャッシュキーがある前提より大きい |
| K2 | 候補スロット集合そのもののフィンガープリント（O(\|候補\|) のハッシュ）をキーにする | 述語の形状を問わず適用できる | フィンガープリントの衝突可能性（fail-closed に「別候補」として扱う定義が必要）・O(\|候補\|) の計算コストが (a) の一部を再導入する（B-1 が成立すれば (a) 自体が軽量になるため許容範囲かは要実測）・候補集合が 1 クエリごとに変わりやすい場合（`id` 範囲述語の可変下限等）ヒット率が構造的に低い |

**推奨: K1。** 理由は (1) `ScalarIndex`（Issue #473）が持つ述語表現
（`metadata_filters`／`id_bounds`）を土台にでき、K2 のような候補集合
フィンガープリント方式よりキー設計コストを抑えられる（ただし
`ScalarIndexCache` 自体に述語キー付きキャッシュの実装は無く、述語列を
正規化してキャッシュキー化する処理は B-2 で新規に設計する必要がある）、
(2) K2 のフィンガープリント計算
コストは B-1 が成立しない場合 (a) を代替する程度の重さになり得え、B-2 単独の
狙い（BFS コストの償却）に対して割に合わない可能性が高い、(3) 索引非対応述語
（`Builtin`／`WasmCall`）は crossdb fixture の主要経路（`lang='ja'` 等価述語）
ではなく実運用頻度が低いと見込まれる。ただし K1 の適用範囲外（`PlainScan`
形状）にどれだけのクエリが分類されるかは実測していないため、この推奨は
Proposed の範囲での暫定判断とする。

キー長上限: 正規化キーは `ScalarIndex` の述語表現をそのまま使う場合、
述語数の上限（`MAX_METADATA_FILTERS` 等、既存の allowlist 制約）に従う。
新規の上限は設けず既存の SQL 表層の述語数制約を継承する。

### 4.3 容量・evict

- エントリ数上限: `MAX_HNSW_CACHE_ENTRIES`（現行 8）・
  `MAX_SCALAR_INDEX_CACHE_ENTRIES`（現行 32）を参考に、B-2 は
  `(table, ctx)` あたり複数の述語キーを持ちうるため後者に近い規模
  （候補値 16〜32 エントリ／`(table, ctx)`）を軸に検討する
- 総バイト上限: `NodeMask` は 索引ノード数 N に対し概ね N/8 バイト（現行
  `MAX_HNSW_NODES` = 1,000,000 で 1 述語あたり約 125 KB）。`mask_splits_graph`
  の bool のみをキャッシュする場合は 1 述語あたり数バイトで済むため、
  「`visible_mask` 本体を保持するか、判定結果（bool）のみ保持するか」で
  容量設計が大きく変わる。判定結果のみのキャッシュはメモリコストが小さい
  一方、B-2 が (a)（写像構築）まで償却しない設計になる——B-1 が成立するなら
  問題ないが、成立しない場合は判定結果のみのキャッシュでは (a) が残る
- evict 方式: LRU。既存キャッシュ（`HnswIndexCache`・`SqlArenaCache`・
  `SparseIndexCache`・`ScalarIndexCache`）と同様の方式に揃える
- 世代進行時の一括失効: テーブル世代が進行したら該当 `(table, ctx)` 配下の
  全述語キーを一括で無効化する（個別述語ごとの再照合は行わない）

### 4.4 `IndexedBase` 個体識別によるキー失効（世代一致だけでは不十分な理由）

§4.1 の実測構成要素（テーブル世代・述語正規化キー）だけでは、**同一世代内での
索引再構築**を区別できない。`IndexedBase::build` は世代が変わらない場面でも
再度呼ばれうる（並列構築はワークスティール方式であり同一データ・同一 seed でも
挿入順序が確定しないためグラフ形状が実行ごとに異なり得る〔`hnsw-parallel-build.md`
参照〕・`HnswIndexCache` の LRU eviction 後の再構築等）。B-2 のキーがテーブル
世代のみに依存する場合、旧い `IndexedBase` インスタンスに対して計算した
`mask_splits_graph`／`visible_mask`（あるいは B-1 が導出する `node_of_slot`）を、
世代が同一というだけで新しい（形状の異なりうる）`IndexedBase` インスタンスへ
誤って適用し、本来必要な `full_scan_with_arena` への縮退を省略して Recall を
損なう経路になりうる。

既存コード（`hnsw_cache.rs::record_overlay_for`）は同種の問題を
`Arc::ptr_eq(b, base)` によるインスタンス識別で解決している（`FullVisible`
overlay を書き込む際、対象 `HnswCacheEntry.base` が呼び出し元の保持する
`Arc<IndexedBase>` と同一インスタンスかを世代とは独立に照合し、不一致なら
書き込みを行わない）。B-2（および B-1 の `node_of_slot` 導出元である
`FullVisible` overlay 参照）は同型の識別を用いる:

- B-2 のキャッシュエントリは、計算対象となった `Arc<IndexedBase>` への参照
  （`Weak<IndexedBase>` または強参照のいずれかは実装検討事項とする）を保持し、
  再利用時に現在解決された `base` と `Arc::ptr_eq` で再照合する。世代が一致して
  いてもインスタンス不一致なら fail-closed（キャッシュ非使用・per-query 再計算）
  へ倒す
- B-1 の `node_of_slot` 導出は `FullVisible` overlay の `base` が、`Subset`
  クエリが今まさに使っている `base` と `Arc::ptr_eq` で同一であることを前提と
  する（世代一致のみでは §3.1 (i) の前提「同一のスナップショット arena」を
  保証しない）

この識別条件は §9 の fail-closed 契約・§10 の実装タスク（B-1 検証・B-2 独立
キャッシュ層）に明示的に含める。

## 5. `FullVisible` 側 `HnswCacheEntry` に同居させない根拠（非登録理由の再評価）

`docs/design/hnsw-rls-cardinality-switch.md` が挙げた「`Subset` 形状の
per-query 写像（キャッシュ非登録）」の理由を、コード事実で再評価する。

- `HnswCacheEntry.overlay` は単一スロットであり、`record_overlay_for` は
  既存の overlay を無条件に上書きする（複数の述語ごとの overlay を同時に
  持てない）
- `HnswCacheEntry` の `needs_rebuild` 判定は索引ノードの新規・内容変更・
  削除を検出する目的のものであり、`Subset` 述語による候補外ノードを
  「stale」と誤認識すると無用な再構築を誘発しうる（`FullVisible` の
  overlay 更新契約と混線する）
- `approx_heap_bytes`・`evict_for_extra_bytes` は「1 エントリ = 1 overlay」を
  前提にサイズ計算・evict 判定を行っており、複数の述語キー付き overlay を
  同一エントリに持たせるには構造変更が必要

これらの理由は今も有効であり、B-2 は `HnswCacheEntry` とは**独立したキャッシュ
層**（例: `(table, ctx)` エントリ配下のサブマップ、または完全に別構造体
`HnswSubsetOverlayCache`）として設計する。`record_overlay_for` と同じ
世代再照合の fail-closed 契約（世代不一致は無条件でキャッシュ非使用扱い）・
`arena_len`／`built_table_generation` の同一性ガード（`ScalarIndexCache` と
同型）を持つ。

B-1（写像導出）は `FullVisible` overlay を**読むだけ**で書き込まない
（`FullVisible` 側の状態機械には一切影響しない）。

## 6. 案 A／B／C の比較と相互作用

| 案 | 内容 | 効く範囲 | 効かない範囲 |
| --- | --- | --- | --- |
| A（#676） | 縮退時の id マスク経路委譲（arena 複製の除去） | `Subset` 経路全般の arena 複製コスト | Overlay 構築・BFS の per-query コスト |
| B（本 ADR） | B-1 写像導出／B-2 BFS 判定キャッシュ | 可視比率 ≥ `full_scan_ratio` で `mask_splits_graph` 判定または ANN 実行に至るクエリ | 可視比率 < `full_scan_ratio`（`PlainScanBelowRatio` で既に早期打ち切り済み） |
| C（#659 申し送り） | `full_scan_ratio` 既定値の引き上げ（2/5 候補） | 選択率が 40% 未満のクエリを `PlainScanBelowRatio` で即座に縮退させる（`below_full_scan_ratio` は厳密な `<` 比較のため、ちょうど 40% は含まない） | それ以上の可視比率、または ANN 実行そのものを狙いたい場合 |

`full_scan_ratio` を仮に 2/5 へ引き上げると、選択率が 40% 未満のクエリは
`Overlay::compute` 自体に到達しなくなり、案 B は何も追加の効果を持たない
（B の対象範囲が縮小する）。

**推奨する着手順序: A → C（オーナー判断・専有環境実測を経て確定）→
B（#677 の前後比較実測で、C 適用後もなお残余コストが両ノイズ帯
〔`benchmark-judgement-policy.md` §4〕を超えて有意に残る場合に限り着手）。**
本 ADR は Proposed のまま、採否を #677 の実測結果に条件付ける。

## 7. 交絡の注記と受け入れ条件

`scripts/crossdb_bench/self_db.py`（フィルタ付き全フェーズ）は固定
`lang = 'ja'` の等価述語を反復しており、`tests/hnsw_crossdb_selectivity.rs`
（Issue #659）も同一 fixture を継承する。**述語キー付きキャッシュ
（B-2）はこの fixture 上ではヒット率 100% になり、ベンチ値を過大に改善して
見せる**（実運用でクエリごとに異なる述語が使われる場合の効果とは乖離する）。

受け入れには以下を必須とする:

1. **述語を回転させるワークロード**での実測（複数の `lang`／タグ値・複数の
   `id` 範囲を反復し、キャッシュのヒット率が現実的な分布になる条件）
2. **cold（初回・ミス）／hot（再照会・ヒット）を分離した報告**（単一の
   平均値ではなく両方を明示する）
3. `docs/design/benchmark-judgement-policy.md` §3〜§4（交互 N≥5・
   per-run 生データ・min-of-N＋median 併記・固定 ±5% と参照区間実測の
   2 種ノイズ帯）および §7.1 チェックリストの遵守
4. キャッシュ統計（新設カウンタ案: `subset_overlay_hits`／
   `subset_overlay_misses`）の**非 vacuous 確認**（`hnsw_index_cache_stats()`
   と同型の公開契約を想定。実測が「常に miss」になっていないことを機械的に
   確認する）

## 8. `RowKey=(String,u64)` インターン化との関係

Issue #659 の申し送りである `RowKey` インターン化は、B-1 が成立する場合
`Subset` 経路については不要になる（B-1 は `RowKey` 生成自体を経由しない
整数演算で `visible_mask` を導出するため）。

一方、`FullVisible` 側の `Overlay::compute`（世代あたり 1 回。`IndexedBase::build`
自身の `key_to_node` 構築を含む）と、#676 の候補 id → `NodeMask` 構築
（id マスク経路。詳細は `scalar-index-mask-search.md` 参照）には引き続き
`RowKey` 相当の文字列キー引きが残るため、インターン化はこれらに対する
**独立した後続課題**として `hnsw-rls-cardinality-switch.md` の申し送りを
維持する（本 ADR では対象外）。

## 9. セキュリティ考慮（OWASP Top 10・security.md P0）

- **アクセス制御／テナント境界（P0）**: B-1／B-2 いずれもキーは
  `(table, PolicyContext 完全一致)` 配下に閉じ、他 ctx のマスク・BFS
  判定結果を参照する経路を作らない。索引は ctx 可視アリーナのみから構築
  する `docs/design/ann-index-adoption.md`「事後フィルタ不採用」の契約を
  維持する。探索結果は従来どおりスロット写像＋`(tenant_id, id)` 照合＋
  `kernel::dot` 再計算＋`provider_result_is_valid` の多層防御を通す（本
  ADR はこの多層防御を一切バイパスしない）
- **不安全な設計／DoS**: エントリ数・総バイト・述語キー長の上限を必ず
  設ける。`visible_mask` 本体をキャッシュする場合は `NodeMask` の
  N/8 バイト／述語という見積りを容量設計の基礎とする。LRU evict は
  既存キャッシュ同様テナント間干渉の経路になりうるため、B-2 が
  干渉面を広げないこと（既存の `MAX_*` 予算と同等以下）を実装条件とする
- **fail-closed**: 世代不一致・ロック毒化・キー衝突（K2 採用時）・
  同一性ガード不一致（§4.4。`IndexedBase` インスタンスの `Arc::ptr_eq`
  不一致——世代が一致していても同一世代内の再構築で異なるインスタンスに
  なっている場合を含む）はすべて「キャッシュ非使用（per-query 計算または
  plain scan への縮退）」側へ倒す。fail-open な分岐を設計上持たない
- **情報漏えい**: `EXPLAIN` へヒット率・可視カーディナリティ等の
  実行時値を露出しない契約（Issue #411）を維持する。統計カウンタは
  テナント ID・行 ID を一切含めない集計値のみとする
- **インジェクション**: 述語正規化キーは束縛済み構造
  （`MetadataFilter`・`IdPredicate`）から生成し、SQL 文字列そのものを
  キーにしない
- **spec 漏えい（P0）**: 本 ADR・関連コミット・PR 本文は CORE-9・CORE-10・
  TASK-132 の ID ポインタのみを用い、spec 本文・非公開の内部判断を転記
  しない
- **秘密情報**: 実測ログ・fixture パスに資格情報を含めない

## 10. 実装タスク分解案（起票はオーナー承認後）

1. **B-1 検証**: `prepare_subset` が `FullVisible` overlay を再利用できる
   条件（§3.1 (i)(ii)）を in-module テストで固定し、`node_of_slot` の
   導出方法（`slot_of_node` の反転）を実装する。§4.4 の `Arc::ptr_eq`
   同一性照合（`FullVisible` overlay の `base` と `Subset` クエリの `base`
   が同一インスタンスであること）をこの検証に含める
2. **B-2 独立キャッシュ層**: キー正規化（K1 方式）・容量上限・LRU evict・
   統計カウンタ・世代再照合の fail-closed 契約を実装する。§4.4 の
   `IndexedBase` インスタンス識別（`Arc::ptr_eq`。既存 `record_overlay_for`
   と同型）をキャッシュエントリの失効条件に含め、世代一致のみで
   同一世代内の再構築（並列構築の形状差異・LRU eviction 後の再構築等）を
   見逃さないことを in-module テストで固定する
3. **`EXPLAIN` 非露出方針の確認**: `ann_plan: hnsw_subset` の出力契約が
   不変であること、実行時のキャッシュヒット率・可視カーディナリティが
   露出しないことを回帰テストで固定する
4. **述語回転ワークロードのベンチ整備**: §7 の条件（cold/hot 分離・N≥5
   交互計測）を満たすベンチハーネスを新設し、前後比較を実施する
5. **hybrid Subset への適用**: `sql::hnsw_hybrid::HnswDenseProvider` は
   疎側 `DocId` がスロット番号に依存する構造のため、B-1／B-2 の適用は
   DISTANCE 直接経路での検証後に別途検討する（本 ADR の対象外）

## 11. スコープ外・申し送り

- hybrid Subset（`sql::hnsw_hybrid`）の arena 複製経路そのものの見直し
  （#676 の対象範囲）
- `full_scan_ratio` 既定値の確定（Issue #659。専有環境実測待ち）
- `REBUILD_DELTA_RATIO`／`MIN_INDEXED_ROWS` の再設計
- ACORN-1（`acorn_max_visible_ratio`）既定値の確定

## 判断記録（オーナー記入欄）

- 採否（Accepted / Rejected / 保留）:
- 判断日・根拠:
