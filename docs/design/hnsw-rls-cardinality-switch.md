# HNSW の RLS 事前フィルタ統合と可視カーディナリティ切替（Issue #409）

親 Issue #402（Phase 3・ANN opt-in 採用）。前提: #408（`sql::hnsw_cache::
HnswIndexCache`、`docs/design/hnsw-generation-cache.md`）。対象ビヘイビア
（ポインタのみ）: CORE-9・CORE-10・TASK-132・RLS-1〜4・RLS-8（TASK-138）・
TASK-139。ADR: `docs/design/ann-index-adoption.md`（Accepted・B 案）。本ドキュメント
が定める閾値・切替規則・マスク契約はいずれも**本リポの実装既定値（非規範）**
であり、`wire_code` の新設・`EXPLAIN` への露出は行わない（#411 の担当。実装済み。
`docs/design/explain-search-engine-exposure.md` 参照）。

## 背景・目的

Issue #408 は「`(table, PolicyContext)` × テーブル世代」単位の HNSW 索引を保持し、
SQL 表層の**フィルタなし** `Ranking::Distance` クエリのみを ANN 経路へ載せた。
残課題は 3 点あった（`hnsw-generation-cache.md`「既知の限界」「スコープ外・
申し送り」節）:

1. 失効ノード（削除・不可視化・内容変更で `STALE_SLOT` になったノード）を
   `k_idx = k + stale_nodes` で吸収する方式は、`k_idx > MAX_EF` で全件
   brute-force へ縮退し、大規模・高 churn テーブルでは ANN が実質効かない
   期間が生じる
2. フィルタ付き DISTANCE（`WHERE` の SCALAR 事前フィルタ）・Rust API
   （`VectorCore::search` → `rls.rs::PrefilterSnapshot`）は ANN 経路に載って
   いない
3. `MIN_INDEXED_ROWS`／`REBUILD_DELTA_RATIO` の固定値運用を「可視カーディナ
   リティ推定」へ置き換える申し送り

本 Issue は (a) HNSW 探索に**候補マスク**（結果へ含めてよいノード集合）を
導入し、(b) 「可視候補数 ÷ 索引ノード数」の比が閾値 `full_scan_ratio` 未満なら
plain scan（brute-force）、以上ならマスク付き ANN 探索、という切替を実装し、
(c) RLS 事前フィルタ経路（SQL 表層のフィルタなし／SCALAR 事前フィルタ付き
DISTANCE・Rust API の `PrefilterSnapshot` 経路）へ結線し、(d) テナント境界
（可視外 id の非混入）を P0 として機械検証する。

## 設計判断

### ADR との整合

Issue 本文には非可視ノードの探索経路上の通過を許容する趣旨の記述があったが、
checked-in の ADR（`ann-index-adoption.md`「実装ガイド（B 案）」節）は「事後
フィルタ不採用」「非可視ノードを探索経路として通過させる設計は不採用」を
P0 安全側の確定事項として維持している。本実装は **ADR と #408 の設計
（`(table, ctx)` 単位・ctx 可視行のみからグラフ構築）を維持**し、per-table
索引＋per-ctx マスクへは移行していない。

その帰結として、マスク外ノードとして探索経路上に現れうるのは次の 2 種のみで、
**他テナントの不可視行はグラフに構造的に存在しない**:

- (a) 索引構築後に失効した stale ノード（`Overlay::compute` が判定する
  削除・不可視化・内容変更）
- (b) SCALAR `WHERE` 事前フィルタで除外された **ctx 可視行**（RLS 上は可視。
  テナント境界とは無関係）

したがって候補マスク（`crate::hnsw::NodeMask`）は「テナント境界」ではなく
「クエリ時点の候補集合（アリーナ）と索引ノードの差」を表す装置であり、
テナント境界は従来どおり (1) 索引構築入力が ctx 可視アリーナのみ、(2) 索引
ヒットのスロット写像・キー照合・スコア再計算、(3) 呼び出し元の
`provider_result_is_valid`、(4) `RlsSafetyNet`（SQL 表層）の多層防御で維持
する。`PolicyContext::is_visible` 単一照合パスに新しい比較ロジックは追加
していない。

### 候補マスク（`crate::hnsw::NodeMask`・`HnswIndex::search_masked`）

`hnsw.rs::search_layer` は受理述語 `Option<&NodeMask>` を受け取る。初出実装
（本 Issue 初版）は候補ヒープ（探索の拡張先を決める）を全ノード対象に拡張し、
結果ヒープ（最終的な Top-k 候補）へのみ `accept` を適用していたが、これは
「非受理ノードのベクトルが探索経路（打ち切り判定・以降の隣接探索）へ影響
しない」という上記「ADR との整合」節の主張と矛盾していた（codex-review
P0 指摘・Issue #431）。是正後は `accept` が受理しないノードを候補ヒープへも
一切積まない——訪問済みマークは付けるが `self.score`（当該ノードのベクトルへ
のアクセス）自体を行わず、その隣接ノードへの探索も行わない。上位層の貪欲
降下（`HnswIndex::greedy_descend_masked`）も同様にマスクを適用する。
固定 entry point（索引全体で 1 点）自体が非受理の場合、初版は直ちに空の
結果を返していたが、これだと entry point 1 点の非受理（RLS フィルタ・行
削除で偶然除外される等）だけで次の索引再構築まで ANN 経路が無条件に無効化
され、「可視カーディナリティが `full_scan_ratio` 以上なら ANN を使う」という
本 Issue の切替設計と整合しない（codex-review P2 指摘・PR #435）。是正後は
`HnswIndex::find_alternate_entry` が受理ノードの中からレベル最大のものを
代替探索起点として選び、その起点が存在する最上層から通常どおり降下する。
受理ノードが 1 つも無い場合のみ空の結果を返す（`sql::hnsw_cache::
search_with_overlay` の `masked_short` 経由で plain scan へ縮退する）。
`None`（既存呼び出し元）はビット同一の結果を返す
（`hnsw::tests::search_masked_none_matches_search` で機械検証）。停止条件
（`results.len() >= ef && strictly_farther` の場合のみ打ち切る）は変更して
いない。

この是正により、マスク密度が低い（`stale_nodes`／WHERE 除外行が多い）クエリ
ほど探索がグラフの非受理ノードで分断されやすくなり、`masked_short` 経由の
plain scan 縮退が起きやすくなる（性能上のトレードオフであり、`docs/design/
ann-index-adoption.md`「RLS／フィルタとの相互作用と折衷案」節が定める P0
安全条件を優先した結果。filter-aware な専用探索方式は将来検討課題のまま
（Issue #410 は本節の課題には対応せず、hybrid 密側再取得ループへの結線
〔`docs/design/hnsw-hybrid-iterative-scan.md`〕を実装した）。

さらに codex-review P2 指摘対応（PR #435）で、`masked_short` の件数検査
（`min(k, visible_in_index)` 未満なら plain scan）だけでは検出できない
recall バグを是正した——マスクが複数の連結成分に分かれ、かつ探索起点側の
成分だけで結果件数を満たせてしまう場合、件数検査は通過するが、探索が
一度も訪れていない別成分により近い受理ノードを取りこぼす可能性がある。

分断検出は **`sql::hnsw_cache::Overlay::compute` が世代（マスク）が変わる
たび 1 回だけ**行う契約とし、`search_masked` 自体はクエリ毎の全域探索を
行わない。`HnswIndex::is_mask_fully_reachable`（`hnsw.rs`）が、
`search_masked` と同じ起点選択（`search_entry_for_mask`。固定 entry point
が受理されていればそれを、されていなければ `find_alternate_entry` が選ぶ
代替起点を、検査・探索の双方が共有する——分断検査が探索とは異なる起点を
使って誤判定することを避けるため）から、層 0 の隣接リストのみを辿る BFS
（非受理ノードを中継点に使わない実探索と同じ規約。ベクトルアクセス・
スコア計算は行わない）でマスクの受理ノード総数を覆えるかを判定し、結果を
`Overlay::mask_splits_graph: bool` として保持する。`Overlay::compute` は
`visible_mask` 自体の構築で既に O(N) を要するため、本判定を追加で 1 回
行っても漸近コストは変わらない（`Subset` 形状はクエリ毎に `Overlay::
compute` を呼ぶため判定もクエリ毎になるが、これはマスク構築自体がクエリ毎
に O(N) であることに起因するもので本判定固有の追加コストではない）。
`search_with_overlay` は `overlay.mask_splits_graph` が `true` なら
`search_masked` 自体を呼ばず直ちに plain scan へ縮退する（統計
`mask_splits_graph`。`masked_short` とは互いに排他）。連結性が保証できない
マスクは常に安全側（plain scan）へ倒す、という判断で、multi-entry-point
探索の実装は見送った（filter-aware な専用探索方式の検討は将来課題のまま。
Issue #410 はこの方式を実装していない）。

`is_mask_fully_reachable` が「分断なし」と判定した場合でも、
`search_masked`（`hnsw.rs`）自体の層 0 探索が到達可能性を検査した起点と
異なる起点だけを使うと recall が壊れうる（codex-review P1 指摘・PR #435
是正）。`search_masked` は上位層の貪欲降下でクエリ依存の別ノードへ移動
するため、`is_mask_fully_reachable` が検査に使った起点（`search_entry_
for_mask` の戻り値）と、降下後に層 0 探索へ渡す起点が一致するとは限ら
ない。層 0 の隣接は枝刈りで有向になり得るため、検査済み起点からは全受理
ノードへ到達できても降下後ノードからは一部の成分（別成分のより近い候補を
含む）へ到達できない場合があった。是正後は `search_masked` が層 0 探索の
初期候補集合へ検査済み起点を（降下後ノードと異なる場合）必ず含める——
検査済み起点から到達可能なことは `is_mask_fully_reachable` が保証済みの
ため、この 2 起点を初期候補にする限り「分断なし」と判定されたマスクに
ついて受理ノード全件への到達可能性が構造的に保証される。マスクが
`None` の場合（検査済み起点・降下後起点が共に固定 entry point で一致）は
従来どおり単一起点のままで、`HnswIndex::search` とのビット同一契約
（`hnsw::tests::search_masked_none_matches_search`）は変わらない。

恒等マスク（warm `FullVisible`。フィルタなし DISTANCE で可視行全体が
そのままアリーナになる形状）に対する実測（`crates/engine/src/sql/
hnsw_cache.rs::tests::
is_mask_fully_reachable_accepts_identity_mask_on_a_repaired_graph_and_ann_path_is_used`）
では、`repair_reachability`（`hnsw.rs::HnswIndex::repair_reachability`。
`build`/`build_parallel` が構築末尾で適用する）が層 0 の entry point 起点
到達性を保証しており、`mask_splits_graph` は `false`（ANN 経路が使われる）
と確認できた。

`HnswIndex::search`（既存公開 API）は `search_masked(.., None, ..)` へ委譲する
薄いラッパーへ変更した。挙動・公開シグネチャは不変。

### クエリ形状と適用条件（`sql/exec.rs`）

`execute_statement_with_cache` のアリーナは、`plan.scalar_prefilter == true`
のとき `on_visible_row` が `WHERE` を事前適用した**部分集合**、`false`
（`HINT ORDER(DISTANCE, …)`）のときは可視行**全集合**＋DISTANCE 後の
`apply_scalar_postfilter`（コード事実）。この事実に基づき適用条件を 2 形状に
分けた:

| 形状 | 条件 | アリーナ | 経路 |
| ---- | ---- | -------- | ---- |
| `FullVisible` | `Ranking::Distance` ∧ `!precision` ∧ (`filters_empty` ∨ `!plan.scalar_prefilter`) | ctx 可視全集合（#408 の不変条件と同じ） | `search_or_fallback`（世代キャッシュ済み `Overlay`） |
| `Subset` | `Ranking::Distance` ∧ `!precision` ∧ `!filters_empty` ∧ `plan.scalar_prefilter` | ctx 可視全集合の**真部分集合**（WHERE 適用後） | `search_subset_or_fallback`（per-query 写像・キャッシュ非登録） |

除外（従来どおり全件 brute-force）: `precision` モード（TASK-162・SEARCH-9。
ANN の近似近傍が確信度ゲートのマージン判定を過大評価しうるため）。hybrid の
密側は Issue #410（`sql::hnsw_hybrid::HnswDenseProvider`）で結線済み——上表と
同じ `FullVisible`／`Subset` 形状判定を流用し、再取得ループの各ラウンドで
`search_prepared` を呼ぶ。詳細は `docs/design/hnsw-hybrid-iterative-scan.md`
参照。

`FullVisible` に `!plan.scalar_prefilter` を含めるのは、DISTANCE 先行・SCALAR
事後フィルタの recall モードでは `k_eff = bound.limit`（`sql/exec.rs`
の `k_eff` 導出）で DISTANCE 段を呼び、事後フィルタは戻り値を絞るだけ
（オーバーフェッチや全体ランキングは行わない）ため、そのアリーナは
フィルタなしと同一（可視全集合）であり `Overlay` の不変条件がそのまま成立
するため。

### 切替規則（本リポ実装既定値）

`Overlay::compute` は失効判定に加え、`visible_mask: NodeMask`（`slot_of_node[node]
!= STALE_SLOT` を表す）・`visible_in_index: usize`（= `index.len() - stale_nodes`）・
`mask_splits_graph: bool`（`HnswIndex::is_mask_fully_reachable(&visible_mask)`
の否定。§前節参照）を計算する。`search_with_overlay`（`sql/hnsw_cache.rs`）は
これを使って:

1. `visible_in_index / index.len() < full_scan_ratio`
   （整数比較 `visible_in_index * denominator < index.len() * numerator`。
   `u64` の `checked_mul`。オーバーフロー時は fail-closed に plain scan）
   → plain scan（アリーナ全体の brute-force。統計 `plain_scans`）
2. `overlay.mask_splits_graph` が `true`（マスクが複数の連結成分に分かれ、
   探索起点の成分だけでは受理ノード全体を覆えないと `Overlay::compute` 時点で
   判明済み）→ `search_masked` 自体を呼ばず plain scan へ縮退（統計
   `mask_splits_graph`）
3. それ以外 → `HnswIndex::search_masked_with(query, k, ef, Some(&visible_mask),
   sparse_visited_max, scratch)`（`search_masked` は `sparse_visited_max` に
   既定値 0 を渡す薄いラッパー）。層 0 のビーム探索が使う visited 集合
   （`VisitedBitmap`／`VisitedSparse`）の選択はこの呼び出しの内部で
   `mask.count_ones() < sparse_visited_max` により決まる（Issue #497。
   探索方式そのもの——plain scan／masked ANN の切替——には影響しない診断的な
   実装選択。詳細は `docs/design/hnsw-search.md`「visited 集合の 3 実装」
   節参照）
4. マスク付き探索の結果件数が `min(k, visible_in_index)` 未満（ビーム幅内で
   可視ノードを辿り切れなかった）→ 当該クエリのみ plain scan へ縮退（統計
   `masked_short`。fail-closed 縮退で k 件充足を保証する。**`ef` の段階的拡張
   自体は本 Issue（#409）の担当ではなく追加しない**——Issue #410 の調査で
   `mask_splits_graph == false` のとき本分岐が構造的に到達不能であることが
   判明したため。詳細・証明は `docs/design/hnsw-hybrid-iterative-scan.md`
   「DISTANCE 経路の `masked_short` 到達不能性」節参照）
5. 索引ヒットのスロット写像・キー照合・`kernel::dot` 再計算・未索引分
   （`delta_slots`）の brute-force 併合は #408 と同じ

`k + stale_nodes` のオーバーフェッチ・`k_idx > MAX_EF` 縮退は撤去した
（`hnsw-generation-cache.md`「既知の限界」の申し送りを閉じる）。
`MIN_INDEXED_ROWS`／`REBUILD_DELTA_RATIO` は本 Issue では据え置く（「再構築
判定」と「探索方式判定」を分離した、という位置づけで記録する）。

`ValidatedHnswParams::full_scan_ratio`（`Ratio { numerator, denominator }`。
既定 `1/10`）は `f32` が `Copy + Eq` を満たさず `HnswParams` の derive と両立
しないため整数比で表現した（`sql/hnsw_cache.rs::REBUILD_DELTA_RATIO` と同型）。
`HnswParams` へ直接フィールド追加すると既存の外部構造体リテラルを破壊する
（codex-review P1 指摘・PR #435 是正）ため、`full_scan_ratio` は
`ValidatedHnswParams`（`HnswParams::validate` を必ず経由する private フィールド
を持つラッパー）側に置き、`ValidatedHnswParams::with_full_scan_ratio` が
`denominator >= 1`・`numerator <= denominator` を fail-closed に検査する。

### `Subset` 形状の per-query 写像（キャッシュ非登録）

`search_subset_or_fallback`（`sql/hnsw_cache.rs`）は `FullVisible` 経路
（`search_or_fallback`）と 3 点異なる（いずれも「アリーナが可視全集合では
なく WHERE 適用後の部分集合」という前提の違いから来る）:

1. `arena.len() < MIN_INDEXED_ROWS` でもエントリを evict しない（フィルタで
   絞られた行数が少ないことは「テーブル自体が小規模」を意味しない）
2. `Lookup::Miss`（索引未構築）では新規構築を試みず、常に plain scan へ縮退
   する（`base` はフィルタなしクエリが可視全集合から構築する契約——部分集合
   アリーナから `IndexedBase::build` を呼ぶと以後の `FullVisible` 経路が誤った
   base を再利用してしまう）
3. per-query の `Overlay::compute` 結果を `record_overlay_for` へ登録しない
   （`needs_rebuild` も評価しない）——WHERE で除外された行が churn に見えて
   無用な再構築を誘発したり、`FullVisible` の次クエリが参照するはずの
   キャッシュ済みオーバーレイを汚したりしないようにする

`Overlay::compute(base, arena, generation)` は「渡された `arena` のスロット
番号系列に対する索引済みノードの写像」を汎用的に計算する関数であり、可視
全集合ではなく WHERE 適用後の部分集合アリーナを渡してもそのまま正しく動く
（部分集合に含まれない索引済みノードは自然に `STALE_SLOT` として除外され、
`visible_mask` にも反映される）。この汎用性により `Subset` 専用の新しい
「差分計算」ロジックを実装する必要がなかった。

統計 `subset_searches`（`Subset` 形状でマスク付き探索が縮退なしで完走した
回数）を `hits`（`FullVisible` の `Ready` 到達かつ縮退なし）とは独立に追加した。

### Rust API（`rls.rs::PrefilterSnapshot::search_with_hnsw`）への結線

`core.rs::EngineCore::search_with_snapshot` は `hnsw_state`（`SearchEngineKind::
Hnsw` opt-in 構築時のみ `Some`）を保持する場合、`snapshot.search_with_hnsw` を
呼ぶ。契約は `search_with`（`ctx` 完全一致・`k`／`query` 検証・provider 呼び
出し前後のストレージ世代照合・`provider_result_is_valid` による Top-k 契約
検証）と同一で、探索本体だけが `sql::hnsw_cache::search_or_fallback`
（`FullVisible` 形状。アリーナ＝スナップショットの可視全集合）を経由する
点が異なる。read トランザクションはこのメソッド内で 1 つ開く（`storage.
current_generation` による事前・事後の失効照合とは独立した読み取りだが、
`search_or_fallback` 内部が世代不一致を検出すれば fail-closed に brute-force
へ縮退するだけで、事後の世代照合が最終防御として機能する）。

`SearchTimeFilter` は対象外とした: アリーナを持たず毎回ストリーミング走査し、
`PolicyContext` が呼び出しごとに動的に変わる前提の API のため、`(table, ctx)`
単位の索引キャッシュは往々にしてスラッシングするだけで、可視カーディナリ
ティ推定は多くの場合 plain scan を選ぶことになる。

既存テスト `tests/hnsw_cache.rs::rust_api_search_bypasses_cache_and_matches_default_engine_via_fallback`
は本 Issue で契約が意図的に反転した（`rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall`
へリネーム）: 「既定エンジン対照 Recall@10 ≥ 0.9・可視外テナントの id が混入
しない・`hits + plain_scans + masked_short > 0`（非 vacuous）」を検証する
（アサーション弱体化ではなく契約変更。旧テストは「ANN 経路が一切使われない
こと」自体を固定していたが、Issue #409 の目的そのものがその迂回の解消の
ため）。

## 対象ファイル

| パス | 変更内容 |
| ---- | -------- |
| `crates/engine/src/hnsw.rs` | `Ratio`・`ValidatedHnswParams::full_scan_ratio`（`with_full_scan_ratio` 経由の private フィールド）。`NodeMask`。`search_layer` の受理述語。`HnswIndex::search_masked`（`search` はこれへ委譲） |
| `crates/engine/src/search_engine.rs` | `Display` に `full_scan_ratio` を追記 |
| `crates/engine/src/sql/hnsw_cache.rs` | `Overlay::visible_mask`／`visible_in_index`／`mask_splits_graph`。`search_with_overlay` を可視カーディナリティ切替へ書き換え（`k + stale` 撤去）。`search_subset_or_fallback`（新設）。統計 `plain_scans`・`mask_splits_graph`・`masked_short`・`subset_searches` |
| `crates/engine/src/sql/exec.rs` | `hnsw_full_visible_eligible`／`hnsw_subset_eligible` の 2 条件・DISTANCE 段の形状別ディスパッチ |
| `crates/engine/src/rls.rs` | `PrefilterSnapshot::search_with_hnsw` |
| `crates/engine/src/core.rs` | `search_with_snapshot` の `hnsw_state` 分岐 |
| `crates/engine/tests/hnsw_cache.rs` | Rust API テストの契約反転・`Subset` 形状の新規テスト |

依存追加なし（`std` のみ）。`unsafe` 不使用。

## セキュリティ考慮（OWASP Top 10・security.md P0）

| 観点 | 対応 |
| ---- | ---- |
| アクセス制御の不備／テナント境界（P0） | 索引は ctx 可視アリーナのみから構築（不変）。マスクは同一 ctx 内の候補差分のみを表し、他テナント行はグラフに存在しない。索引ヒットは現世代スロットへの写像・`(tenant_id, id)` キー照合・スコア再計算を経て返し、`provider_result_is_valid`・`RlsSafetyNet` の多層防御を維持。`PolicyContext::is_visible` に新規比較ロジックを追加していない |
| 存在情報の副次漏えい | 切替判定・マスク密度・統計はいずれも ctx 自身の可視集合と自身の索引のみから導出され、他テナント行数に依存しない。`EXPLAIN` へは露出しない（#411 の担当。実装済みの `EXPLAIN` 露出自体もこれらの数値を露出しない方針を継承。`docs/design/explain-search-engine-exposure.md` 参照） |
| fail-closed のエラー契約 | HNSW 固有エラー・マスク長不一致・写像検証失敗・結果不足はいずれも当該クエリの brute-force 縮退へ吸収し呼び出し元へ伝播させない。不正 `full_scan_ratio` は `ValidatedHnswParams::with_full_scan_ratio` で拒否する |
| インジェクション | SQL 文字列を組み立てる箇所なし |
| 不安全な設計／DoS | `NodeMask` は索引ノード数分のビット（最大 `MAX_HNSW_NODES / 8` バイト程度）。整数演算は `checked_*`／`saturating_*`。`ef`・`k` の上限は既存どおり `MAX_EF` |
| 脆弱な依存 | 依存追加なし。`unsafe` なし |
| 世代整合（キャッシュ誤登録防止） | `rls.rs::PrefilterSnapshot::search_with_hnsw` は `read_txn` のテーブル世代がスナップショット構築時のテーブル世代（`built_table_generation`）と一致する場合のみ `search_or_fallback` を呼ぶ。不一致時（構築後・`read_txn` オープン前に書き込みが割り込んだ場合を含む）はキャッシュへ一切触れず brute-force へ縮退し、旧アリーナから構築した索引が新世代のエントリとして誤登録される事故を防ぐ |

## 検証

- 単体（`hnsw.rs` in-module）: `search_masked(None)` が `search` とビット同一・
  結果がマスクの部分集合・マスク長不一致の拒否・全 false マスクで空
- 単体（`sql/hnsw_cache.rs` in-module）: 既存テストを `Overlay` の新規
  フィールドへ対応させたうえで全 green（切替・写像検証の既存契約は不変）
- 結合（`crates/engine/tests/hnsw_cache.rs`）: `Subset` 形状の Recall@10 ≥ 0.9・
  WHERE を満たさない行の非混入・`subset_searches > 0`（非 vacuous）・キャッシュ
  非登録（`stats.entries` がフィルタなしクエリのベースラインから変化しない）。
  Rust API 結線の Recall@10 ≥ 0.9・可視外テナント非混入・非 vacuous
- 結合（`full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants`／
  `full_scan_ratio_plain_scan_side_matches_brute_force_and_never_leaks_across_tenants`。
  Issue #409 申し送り。受入基準 2「切替閾値の前後で結果が brute-force と同水準」
  の直接検証）: `FullVisible` 形状で可視カーディナリティ比が `full_scan_ratio`
  以上（既定 1/10・削除なし）の場合は ANN 側（`stats.hits > 0`・
  `stats.plain_scans == 0`）、比を `full_scan_ratio`（95/100 へ引き上げ）未満へ
  下げた場合（`REBUILD_DELTA_RATIO`〔1/10〕を超えない churn 幅の削除で誘発し
  再構築とは区別する）は plain scan 側（`stats.plain_scans > 0`・
  `stats.builds` 不変）を踏むこと、いずれも既定エンジン対照 Recall@10 ≥ 0.9
  （plain scan 側は ≥ 0.99）・可視外テナント行および削除済み行の非混入を固定
- 単体（`hnsw.rs` in-module。`full_scan_ratio_defaults_and_rejects_invalid_ratios`）:
  `full_scan_ratio` の既定値 1/10・分母 0／分子 > 分母の `HnswError::InvalidParams`
  拒否
- `make core-api-check`（`SearchProvider`/`VectorCore` trait 差分ゼロ）・
  `make sort-determinism-check` green

## 可視比率 × 行数の損益分岐点実測（Issue #487）

### 目的

`full_scan_ratio`（既定 1/10）の妥当性は Issue #413 の 1 点（`vector_knn_where`・
`lang='ja'`≒1/5）の実測しか持たなかった（`docs/design/hnsw-index.md` §7〜§10）。
本節は可視比率（1/2・1/4・1/10・1/20・1/50）× 行数（25k・100k）のスイープを
`crates/engine/benches/knn_profile_bench.rs` の opt-in
（`BENCH_KNN_PROFILE_VISIBLE_RATIO`／`BENCH_KNN_PROFILE_FULL_SCAN_RATIO`／
`BENCH_KNN_PROFILE_SCALE`）で実測し、`full_scan_ratio` 既定値の再調整判断へ
入力する。

### 測定条件

- **candidate**: `hnsw_default`（`full_scan_ratio` 既定 1/10）・`hnsw_force_ann`
  （`FULL_SCAN_RATIO=0/1`。閾値比較を常に ANN 側にする）・`hnsw_force_plain`
  （`FULL_SCAN_RATIO=1/1`。可視行数が索引ノード数と完全一致しない限り常に
  plain scan 側にする）の 3 つ。`brute_force`（`Subset` 系カウンタを持たない
  対照。BENCH_KNN_PROFILE_ENGINE=brute_force）は各 candidate 共通の baseline
- **実行順（要訂正・codex-review 指摘）**: 下記「実測方式」に記す計測コミット
  `06336219` 時点の `scripts/bench_knn_visible_ratio_sweep.sh` は、scale×ratio
  の組み合わせごとに pair を外側・**arm（`brute_force`・`hnsw_default`・
  `hnsw_force_ann`・`hnsw_force_plain` の 4 つ）を内側**のループとし、
  1 ペアにつき `brute_force→hnsw_default→hnsw_force_ann→hnsw_force_plain` を
  1 回ずつ実行する構成だった（各 candidate の直前に baseline を個別に挟む
  輪番ではない）。「各 candidate の直前に必ず baseline を 1 回挟む」輪番
  （baseline→hnsw_default→baseline→hnsw_force_ann→baseline→hnsw_force_plain）
  は本 PR で `docs/design/benchmark-judgement-policy.md` §3 に合わせて
  `scripts/bench_knn_visible_ratio_sweep.sh` を書き換えた**後**の構成であり、
  下記の実測値（計測コミット `06336219`）はこの新しい輪番では**採取されて
  いない**。実測値を新しい輪番で再取得するには専有環境での再実測が必要
  （下記「スコープ外・申し送り」参照）
- **warm 手順**: `Subset` 形状（SCALAR 事前フィルタ付き DISTANCE）は自身では
  索引を構築しない（`sql::hnsw_cache::prepare_subset`。既存の索引が
  `Ready`／`NeedOverlay` であればその base を再利用し per-query オーバーレイを
  計算して `Indexed` を返す。利用可能な索引が無い〔`Lookup::Miss`〕場合のみ
  `FullScan` へ縮退する。Issue #410 の既存契約）ため、同一 `EngineCore` で
  まずフィルタなしクエリを 1 回発行して `FullVisible` 形状の索引を warm し、
  `Subset` 経路が再利用できる base を用意してから WHERE クエリを計測する。
  warm 後 `builds == 0` は fail-closed（測定を中断する）
- **S0-cold 非対象の理由**: `Subset` 形状は自身では索引を新規構築しない設計
  のため、毎サンプル新規 `EngineCore` で測る S0-cold（warm 手順を経ないため
  再利用可能な base が存在しない）は「常に索引なし＝`FullScan`」を測るだけで
  可視比率スイープの目的（索引 warm 済み状態での ANN／plain scan 切替）に
  寄与しない
- **コーパスの違い**: 本スイープは `knn_profile_bench.rs` 既存の一様乱数
  ベクトル（`DeterministicRng`）を使う。`docs/design/hnsw-index.md` の
  `feature_bench`（ハッシュ埋め込み）・`vector_knn_where`（`lang='ja'` 述語）
  の数値とは直接比較できない
- **可視集合の構成**: `bucket TEXT` 列を追加し `id % denominator == 0` を
  `bucket='b0'` として WHERE 述語にする。可視集合は id 全域に均等に散らばる
  （クラスタ構造を持たない、id 空間上「最も細かく分散した」マスク形状）
- **実行環境（要訂正・codex-review 指摘）**: 本開発環境（共有 QEMU 環境。
  `docs/design/benchmark-judgement-policy.md` §5 の証拠力区分では
  「参考値」）。専有環境（`BENCH_DEDICATED_ENV=1`）での再実測は未実施——
  **本節の数値は `full_scan_ratio` 既定値の変更根拠にはしない**。なお
  計測プロトコル自体（§3〜§4 の必須事項〔交互 N≥5 ペア・輪番・per-run
  生データ保持〕）も**完全には満たしていない**——上記「実行順」「実測方式」
  「baseline 系列の不整合」の各節で訂正したとおり、計測コミット
  `06336219` 時点の輪番は §3 が定める「各 candidate の直前に baseline を
  個別に挟む」方式ではなく、参照区間（`S0_hot_sql_e2e`）の per-run 生データも
  未保存・再構成不可であり、baseline 系列自体も表間で一致しない。したがって
  共有 QEMU 環境という証拠力区分の制約に加え、計測プロトコル遵守の観点でも
  制約がある（詳細は下記「判断」参照）
- **実測方式**: `make bench-knn-visible-ratio`（`SWEEP_PAIRS=5`。既定値・
  `docs/design/benchmark-judgement-policy.md` §3 の N≥5 必須要件を満たす
  下限）で 3 candidate × 5 ratio × 2 scale の全組み合わせを、計測コミット
  `06336219` 時点の輪番（上記「実行順」参照。1 ペアにつき
  `brute_force→hnsw_default→hnsw_force_ann→hnsw_force_plain` を 1 回ずつ）で
  N=5 ペアずつ実行した。生ログは `target/bench-knn-visible-ratio/1788722580/`
  （ローカル・gitignore 対象。当時のセッション終了後に破棄されており、
  本ドキュメント作成後に取得し直すことはできない）。事後の再判定に必要な
  per-run 生データのうち、`S0_hot_where_subset`（対象クエリ）の中央値は
  下記の各表・折りたたみ内へインライン記録済みだが、**ノイズ帯算出の
  参照区間である `S0_hot_sql_e2e`（フィルタなしクエリ）の per-run 生データは
  当時記録しておらず、上記ログ破棄により事後の再構成もできない**
  （codex-review P1/P2 指摘。以下の各表「参照区間帯（全幅）」列は生ログ
  破棄前に算出済みの `(max-min)/min` 値のみを転記したものであり、算出元と
  なった個々の run 値そのものは本ドキュメントにも残っていない。この
  欠落は本節の実測を専有環境で再実施する際に是正する。「スコープ外・
  申し送り」参照）
- **baseline 系列の不整合（要訂正・codex-review 指摘）**: 上記スクリプト
  構成では 1 ペア内で `brute_force`（baseline）を 1 回だけ実行し、同じ
  ペアの `hnsw_default`・`hnsw_force_ann`・`hnsw_force_plain` の 3 候補が
  この単一の baseline 系列を共有する設計だった。しかし下記の各表の
  baseline 列（per-run 生データ）を突き合わせると、同一 scale・ratio でも
  candidate（表）ごとに異なる値になっている（例: scale=1・ratio=1/2 の
  1 本目の値が `hnsw_default` 表で 2.145、`hnsw_force_ann` 表で 2.196、
  `hnsw_force_plain` 表で 11.846）。これはスクリプトが意図する「3 候補が
  同一 baseline 系列を共有する」設計とは矛盾する実測結果であり、3 表が
  実際には同一の輪番セッションから抽出されたものではない（表ごとに
  別セッション・別実行で採取された可能性が高い）ことを示唆する。生ログは
  上記のとおり破棄済みで事後の再構成ができないため、baseline 値の出典
  （どの実行がどの表に対応するか）は本ドキュメントの記述からは追跡でき
  ない。本節の数値は既存 arm 分類の定性的傾向の把握以上には用いない
  （下記「判断」参照）という既存の制約に加え、この baseline 系列の不整合
  自体が§3の輪番方式の遵守を確認できないことの追加根拠であり、専有環境
  での再実測では、現行スクリプト（`scripts/bench_knn_visible_ratio_sweep.sh`。
  各 candidate の直前に個別の baseline を測り `baseline_for_<candidate>`
  としてログへ残す輪番方式）の設計どおり、各 candidate をその直前に測定した
  baseline と対応付け、全 candidate が同一の輪番セッション内で測定された
  ことをログ上検証可能な形で残す（baseline のログファイル名に対応する
  candidate 名を含める現行の命名規則・pair index の記録で満たされる）

### 実測結果

各表は `S0_hot_where_subset`（対象。WHERE 述語付き DISTANCE クエリ）の
min/median/max（単位 ms）・`ratio(min) = candidate min / baseline min`・
`ratio(median) = candidate median / baseline median`・両ノイズ帯
（`docs/design/benchmark-judgement-policy.md` §4: 固定 ±5% 相対帯、および
同一セッションの参照区間〔`S0_hot_sql_e2e`。フィルタなしクエリ〕の
run-to-run 全幅 `(max-min)/min`。半幅ではなく規定どおり全幅を使う）・
観測 arm（`sql::hnsw_cache::HnswIndexCacheStats` の Subset 系カウンタから
分類）を示す。**主統計量は `ratio(min)` とし `ratio(median)` は交差確認
として併記する**（`docs/design/benchmark-judgement-policy.md` §3「レイテンシ・
所要時間系の主統計量は min-of-N〔環境ノイズは加算方向のみという前提〕とし、
median を交差確認として併記する」に従う）。「判定」列は `|ratio(min) - 1.0|`
が固定 ±5% 帯・参照区間の実測帯（baseline／candidate 双方のうち大きい方）の
**両方**を超えるかどうか（min を主統計量とした判定。median は交差確認
専用であり判定には用いない）。per-run 生データ（N=5 ペアそれぞれの
`S0_hot_where_subset` 中央値。単位 ms）は各表直下の折りたたみに記録する。

#### scale=1（25,000 行）・candidate=`hnsw_default`

| ratio | baseline min/median/max（ms） | `hnsw_default` min/median/max（ms） | ratio(min) | ratio(median) | baseline 参照区間帯（全幅） | hnsw_default 参照区間帯（全幅） | 判定 | 観測 arm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1/2 | 2.023 / 2.158 / 2.285 | 5.319 / 6.278 / 7.212 | 2.63x | 2.91x | 54.7% | 4.2% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/4 | 1.205 / 1.43 / 3.084 | 3.207 / 3.978 / 8.107 | 2.66x | 2.78x | 106.9% | 40.2% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/10 | 0.887 / 0.953 / 1.483 | 1.489 / 1.514 / 1.571 | 1.68x | 1.59x | 6.9% | 3.1% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/20 | 0.652 / 0.661 / 0.686 | 0.822 / 0.843 / 0.898 | 1.26x | 1.28x | 23.8% | 3.8% | 両ノイズ帯を超過 | `plain_scan_ratio` |
| 1/50 | 0.536 / 0.549 / 0.554 | 0.615 / 0.617 / 0.636 | 1.15x | 1.12x | 43.5% | 2.6% | ノイズ帯内 | `plain_scan_ratio` |

<details><summary>per-run 生データ（N=5 ペア・単位 ms。`S0_hot_where_subset` の中央値）</summary>

- ratio=1/2: baseline=[2.145, 2.158, 2.285, 2.023, 2.229] / `hnsw_default`=[6.616, 7.212, 5.319, 6.278, 5.734]
- ratio=1/4: baseline=[1.205, 1.386, 3.084, 1.496, 1.43] / `hnsw_default`=[3.534, 4.341, 3.978, 3.207, 8.107]
- ratio=1/10: baseline=[0.953, 0.973, 0.887, 1.483, 0.946] / `hnsw_default`=[1.489, 1.514, 1.516, 1.512, 1.571]
- ratio=1/20: baseline=[0.665, 0.654, 0.661, 0.652, 0.686] / `hnsw_default`=[0.843, 0.865, 0.898, 0.822, 0.829]
- ratio=1/50: baseline=[0.54, 0.554, 0.549, 0.554, 0.536] / `hnsw_default`=[0.636, 0.616, 0.615, 0.626, 0.617]

</details>

#### scale=1（25,000 行）・candidate=`hnsw_force_ann`

| ratio | baseline min/median/max（ms） | `hnsw_force_ann` min/median/max（ms） | ratio(min) | ratio(median) | baseline 参照区間帯（全幅） | hnsw_force_ann 参照区間帯（全幅） | 判定 | 観測 arm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1/2 | 1.909 / 2.196 / 3.825 | 4.863 / 8.374 / 26.034 | 2.55x | 3.81x | 433.1% | 81.0% | ノイズ帯内 | `plain_scan_mask_split` |
| 1/4 | 1.328 / 1.519 / 1.739 | 3.604 / 4.595 / 39.415 | 2.71x | 3.03x | 338.9% | 85.3% | ノイズ帯内 | `plain_scan_mask_split` |
| 1/10 | 0.943 / 2.926 / 3.644 | 5.889 / 6.001 / 8.083 | 6.25x | 2.05x | 533.9% | 74.9% | ノイズ帯内 | `plain_scan_mask_split` |
| 1/20 | 0.654 / 0.654 / 0.674 | 0.815 / 0.818 / 0.856 | 1.25x | 1.25x | 7.4% | 2.4% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/50 | 0.536 / 0.545 / 0.585 | 0.617 / 0.62 / 0.656 | 1.15x | 1.14x | 518.9% | 78.1% | ノイズ帯内 | `plain_scan_mask_split` |

<details><summary>per-run 生データ（N=5 ペア・単位 ms。`S0_hot_where_subset` の中央値）</summary>

- ratio=1/2: baseline=[2.196, 3.825, 1.928, 2.248, 1.909] / `hnsw_force_ann`=[4.863, 8.382, 5.26, 26.034, 8.374]
- ratio=1/4: baseline=[1.559, 1.739, 1.519, 1.328, 1.432] / `hnsw_force_ann`=[7.543, 3.604, 39.415, 4.595, 3.653]
- ratio=1/10: baseline=[0.943, 2.926, 3.426, 2.91, 3.644] / `hnsw_force_ann`=[5.889, 6.081, 8.083, 6.001, 5.969]
- ratio=1/20: baseline=[0.674, 0.654, 0.654, 0.661, 0.654] / `hnsw_force_ann`=[0.856, 0.817, 0.818, 0.815, 0.824]
- ratio=1/50: baseline=[0.539, 0.536, 0.545, 0.585, 0.569] / `hnsw_force_ann`=[0.618, 0.62, 0.621, 0.617, 0.656]

</details>

#### scale=1（25,000 行）・candidate=`hnsw_force_plain`

| ratio | baseline min/median/max（ms） | `hnsw_force_plain` min/median/max（ms） | ratio(min) | ratio(median) | baseline 参照区間帯（全幅） | hnsw_force_plain 参照区間帯（全幅） | 判定 | 観測 arm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1/2 | 1.996 / 2.532 / 11.993 | 6.243 / 8.007 / 10.456 | 3.13x | 3.16x | 616.7% | 81.1% | ノイズ帯内 | `plain_scan_ratio` |
| 1/4 | 1.17 / 1.19 / 8.422 | 3.056 / 3.135 / 4.478 | 2.61x | 2.63x | 674.5% | 2.5% | ノイズ帯内 | `plain_scan_ratio` |
| 1/10 | 2 / 2.999 / 4.996 | 1.872 / 6.589 / 8.278 | 0.94x | 2.20x | 73.6% | 99.1% | ノイズ帯内 | `plain_scan_ratio` |
| 1/20 | 0.651 / 0.669 / 0.782 | 0.823 / 0.831 / 1.149 | 1.26x | 1.24x | 669.1% | 77.7% | ノイズ帯内 | `plain_scan_ratio` |
| 1/50 | 0.534 / 0.538 / 0.548 | 0.615 / 0.615 / 0.636 | 1.15x | 1.14x | 30.7% | 3.8% | ノイズ帯内 | `plain_scan_ratio` |

<details><summary>per-run 生データ（N=5 ペア・単位 ms。`S0_hot_where_subset` の中央値）</summary>

- ratio=1/2: baseline=[11.846, 2.358, 2.532, 1.996, 11.993] / `hnsw_force_plain`=[10.456, 6.243, 8.007, 7.023, 9.131]
- ratio=1/4: baseline=[1.17, 8.422, 1.176, 1.19, 1.248] / `hnsw_force_plain`=[3.387, 3.135, 4.478, 3.057, 3.056]
- ratio=1/10: baseline=[4.996, 3.001, 2, 2.999, 2.799] / `hnsw_force_plain`=[2.705, 8.278, 1.872, 6.589, 8.152]
- ratio=1/20: baseline=[0.681, 0.651, 0.782, 0.669, 0.655] / `hnsw_force_plain`=[0.867, 0.823, 1.149, 0.823, 0.831]
- ratio=1/50: baseline=[0.536, 0.538, 0.548, 0.534, 0.538] / `hnsw_force_plain`=[0.618, 0.636, 0.615, 0.615, 0.615]

</details>

#### scale=4（100,000 行）・candidate=`hnsw_default`

| ratio | baseline min/median/max（ms） | `hnsw_default` min/median/max（ms） | ratio(min) | ratio(median) | baseline 参照区間帯（全幅） | hnsw_default 参照区間帯（全幅） | 判定 | 観測 arm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1/2 | 18.108 / 18.321 / 19.35 | 36.046 / 37.779 / 41.313 | 1.99x | 2.06x | 4.5% | 0.4% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/4 | 5.954 / 6.086 / 11.577 | 17.578 / 17.604 / 21.212 | 2.95x | 2.89x | 91.4% | 48.9% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/10 | 3.839 / 3.877 / 4.428 | 9.028 / 9.353 / 9.772 | 2.35x | 2.41x | 9.5% | 2.1% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/20 | 2.782 / 2.788 / 3.661 | 4.262 / 4.269 / 4.707 | 1.53x | 1.53x | 21.6% | 0.7% | 両ノイズ帯を超過 | `plain_scan_ratio` |
| 1/50 | 2.054 / 2.067 / 2.12 | 2.479 / 2.485 / 2.538 | 1.21x | 1.20x | 5.4% | 1.0% | 両ノイズ帯を超過 | `plain_scan_ratio` |

<details><summary>per-run 生データ（N=5 ペア・単位 ms。`S0_hot_where_subset` の中央値）</summary>

- ratio=1/2: baseline=[18.321, 18.151, 18.944, 19.35, 18.108] / `hnsw_default`=[39.226, 36.046, 37.779, 41.313, 36.135]
- ratio=1/4: baseline=[5.972, 11.577, 5.954, 6.086, 8.042] / `hnsw_default`=[17.604, 21.212, 17.578, 19.342, 17.603]
- ratio=1/10: baseline=[3.877, 4.428, 3.839, 3.853, 3.893] / `hnsw_default`=[9.028, 9.307, 9.772, 9.723, 9.353]
- ratio=1/20: baseline=[2.788, 2.782, 3.661, 2.788, 2.798] / `hnsw_default`=[4.269, 4.262, 4.262, 4.338, 4.707]
- ratio=1/50: baseline=[2.12, 2.054, 2.062, 2.067, 2.067] / `hnsw_default`=[2.484, 2.479, 2.538, 2.505, 2.485]

</details>

#### scale=4（100,000 行）・candidate=`hnsw_force_ann`

| ratio | baseline min/median/max（ms） | `hnsw_force_ann` min/median/max（ms） | ratio(min) | ratio(median) | baseline 参照区間帯（全幅） | hnsw_force_ann 参照区間帯（全幅） | 判定 | 観測 arm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1/2 | 17.563 / 18.557 / 19.435 | 35.778 / 37.987 / 40.134 | 2.04x | 2.05x | 9.6% | 10.2% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/4 | 5.921 / 5.939 / 6.071 | 17.602 / 17.691 / 17.89 | 2.97x | 2.98x | 8.3% | 0.9% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/10 | 3.82 / 3.887 / 4.467 | 9.238 / 9.287 / 10.047 | 2.42x | 2.39x | 6.2% | 5.2% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/20 | 2.747 / 2.79 / 2.821 | 4.211 / 4.298 / 5.008 | 1.53x | 1.54x | 4.1% | 1.6% | 両ノイズ帯を超過 | `plain_scan_mask_split` |
| 1/50 | 2.056 / 2.076 / 2.169 | 2.484 / 2.503 / 2.543 | 1.21x | 1.21x | 11.6% | 0.8% | 両ノイズ帯を超過 | `plain_scan_mask_split` |

<details><summary>per-run 生データ（N=5 ペア・単位 ms。`S0_hot_where_subset` の中央値）</summary>

- ratio=1/2: baseline=[17.563, 18.9, 19.435, 18.083, 18.557] / `hnsw_force_ann`=[35.778, 37.987, 40.134, 36.335, 38.361]
- ratio=1/4: baseline=[6.071, 5.939, 5.921, 5.976, 5.929] / `hnsw_force_ann`=[17.797, 17.691, 17.602, 17.89, 17.635]
- ratio=1/10: baseline=[3.902, 3.853, 4.467, 3.887, 3.82] / `hnsw_force_ann`=[9.248, 10.047, 9.987, 9.287, 9.238]
- ratio=1/20: baseline=[2.79, 2.821, 2.794, 2.788, 2.747] / `hnsw_force_ann`=[4.298, 4.224, 4.211, 5.008, 4.628]
- ratio=1/50: baseline=[2.056, 2.076, 2.073, 2.169, 2.101] / `hnsw_force_ann`=[2.484, 2.503, 2.543, 2.517, 2.494]

</details>

#### scale=4（100,000 行）・candidate=`hnsw_force_plain`

| ratio | baseline min/median/max（ms） | `hnsw_force_plain` min/median/max（ms） | ratio(min) | ratio(median) | baseline 参照区間帯（全幅） | hnsw_force_plain 参照区間帯（全幅） | 判定 | 観測 arm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1/2 | 17.419 / 33.832 / 41.623 | 35.622 / 39.695 / 77.459 | 2.05x | 1.17x | 188.7% | 90.4% | ノイズ帯内 | `plain_scan_ratio` |
| 1/4 | 5.897 / 5.925 / 6.035 | 17.396 / 17.647 / 20.51 | 2.95x | 2.98x | 30.8% | 4.5% | 両ノイズ帯を超過 | `plain_scan_ratio` |
| 1/10 | 3.832 / 4.393 / 4.925 | 9.02 / 10.021 / 10.985 | 2.35x | 2.28x | 16.6% | 5.5% | 両ノイズ帯を超過 | `plain_scan_ratio` |
| 1/20 | 2.75 / 2.773 / 2.87 | 4.112 / 4.143 / 4.704 | 1.50x | 1.49x | 8.3% | 1.3% | 両ノイズ帯を超過 | `plain_scan_ratio` |
| 1/50 | 2.055 / 2.076 / 2.936 | 2.506 / 2.509 / 2.74 | 1.22x | 1.21x | 145.6% | 2.0% | ノイズ帯内 | `plain_scan_ratio` |

<details><summary>per-run 生データ（N=5 ペア・単位 ms。`S0_hot_where_subset` の中央値）</summary>

- ratio=1/2: baseline=[17.836, 41.58, 33.832, 41.623, 17.419] / `hnsw_force_plain`=[37.187, 74.287, 35.622, 77.459, 39.695]
- ratio=1/4: baseline=[5.897, 5.934, 5.925, 6.035, 5.923] / `hnsw_force_plain`=[17.396, 20.51, 17.653, 17.479, 17.647]
- ratio=1/10: baseline=[4.925, 3.855, 3.832, 4.393, 4.477] / `hnsw_force_plain`=[9.971, 10.985, 9.02, 10.484, 10.021]
- ratio=1/20: baseline=[2.87, 2.761, 2.75, 2.779, 2.773] / `hnsw_force_plain`=[4.112, 4.143, 4.125, 4.181, 4.704]
- ratio=1/50: baseline=[2.057, 2.076, 2.055, 2.616, 2.936] / `hnsw_force_plain`=[2.507, 2.687, 2.74, 2.509, 2.506]

</details>

### 中心的な所見: 本フィクスチャでは「ann_masked」観測 arm に到達しない

`full_scan_ratio` の閾値比較（`visible/index_len >= full_scan_ratio` なら
ANN）が「ANN を選ぶはず」と予測する 1/2・1/4・1/10 のいずれでも、`hnsw_default`
（全 10 測定点。scale×ratio の組み合わせ）で実際に選ばれたのは ANN 探索
（`subset_searches`）ではなく `mask_splits_graph`（マスクの受理ノードが複数の
連結成分に分かれ `search_masked` を呼ぶ前に plain scan へ縮退する経路。
Issue #409 実装）だった。`hnsw_force_ann`（閾値を無視して常に ANN 側と
判定させても）でも 1/2〜1/50 の全 10 測定点で同じく `plain_scan_mask_split`
を観測した（`search_masked` 自体を呼ぶ前に分断検査で縮退するため、
`full_scan_ratio` の値を変えても挙動が変わらない）。`hnsw_force_plain`
（`FULL_SCAN_RATIO=1/1`）は全 10 測定点で `plain_scan_ratio` を観測した
（可視行数が索引ノード数と完全一致しない限り必ず plain scan 側になる、
期待どおりの契約）。`id % N == 0` という均等分散マスクは、この一様乱数
ベクトルの HNSW グラフ構造の下では `HnswIndex::is_mask_fully_reachable` の
単一連結性検査を通過しない——つまり本スイープの可視集合構成は、
`full_scan_ratio` 閾値そのものではなく「マスク分断」が全ての高可視比率点で
先に発火する形状になっている。N=5 ペア（scale=1・4 双方、3 candidate 全て）
に拡張した本実測でもこの分類は一貫しており（上表「観測 arm」列参照）、
N=3 時点の所見（Issue #487 実装セッション）から変わらない。

これは本フィクスチャ固有の設計上の限界であり、`full_scan_ratio` 既定値の
妥当性そのものについては結論を出せない（`ann_masked` 経路の性能を一度も
観測できていないため）。「均等分散マスクは分断しやすい」こと自体は
HNSW の一般的な性質（グラフの疎な領域を均等マスクで間引くと連結性が
失われやすい）と整合するが、`docs/design/hnsw-rls-cardinality-switch.md`
の実運用シナリオ（RLS テナント境界・`WHERE` 述語）が想定する可視集合の
形状（同一テナント・同一属性値のクラスタ）とは異なる可能性が高い。
後続の実測はクラスタ寄りの可視集合構成（例: 連続 id 範囲・同一テナント）を
検討すべきという申し送りとする。

`mask_splits_graph` 経路が発火している間、`hnsw_default` は一貫して
baseline（`brute_force`）より遅い（`Overlay::compute`・分断検査・plain
scan への縮退分のオーバーヘッドが乗る。上表の主統計量「ratio(min)」列:
1/2〜1/10 で約 1.7〜3.0 倍、いずれも両ノイズ帯を超過）。`plain_scan_ratio`
（1/20・1/50）でも `hnsw_default` は baseline と同程度かやや遅い
（1.15〜1.53 倍。scale=1・1/50 のみノイズ帯内、他は両ノイズ帯を超過）。
`hnsw_force_plain`（scale=1・1/10）の 1 点のみ、主統計量 `ratio(min)`
（0.94x）では固定 ±5% 帯を超過するものの参照区間の実測帯（99.1%）以内に
収まり判定は「ノイズ帯内」——中央値ベースの `ratio(median)`（2.20x・
両ノイズ帯を超過）とは逆の判定になる。参照区間の run-to-run 幅が非常に
大きい（baseline 73.6%・candidate 99.1%）測定点であり、外れ値の影響を
受けやすい中央値では見かけ上大きな退行に見えるが、min-of-N（環境ノイズは
基本的に加算方向にしか働かないという前提）で見ると両ノイズ帯を超える
差は確認できない、という中央値・最小値の判定の乖離の実例になっている
（本表の判定はすべて主統計量 `ratio(min)` に従う。中央値ベースでの旧判定は
上記の 1 点のみ異なり、他の全測定点は両統計量で判定が一致する）。
いずれの観測 arm でも本フィクスチャでは ANN opt-in が SCALAR 事前フィルタ
付き DISTANCE を高速化する場面は確認できなかった——これは Issue #413 の
所見（`hnsw_subset` 経路は 37〜45% 悪化）と整合する。

### 判断

- 本開発環境（共有 QEMU）の実測は、`S0_hot_where_subset`（対象クエリ）の
  per-run 生データは記録済みだが、**ノイズ帯算出の参照区間
  （`S0_hot_sql_e2e`）の per-run 生データは未保存かつ生ログ破棄により
  事後の再構成も不可**（上記「実測方式」参照。codex-review 指摘）であり、
  かつ計測コミット `06336219` 時点の実行順は「上記「実行順」節で訂正した
  とおり `docs/design/benchmark-judgement-policy.md` §3 の輪番方式（各
  candidate の直前に baseline を個別に挟む）そのものでは実行されていない
  （codex-review 指摘）。したがって `docs/design/benchmark-judgement-
  policy.md` §3〜§4 の必須事項を**完全には満たしておらず**、§5 の証拠力
  区分でも従来の「参考値」よりさらに一段弱い——`full_scan_ratio` 既定値
  （1/10）の変更根拠にしないのは元より、本節の数値そのものを既存 arm
  分類（`plain_scan_mask_split`／`plain_scan_ratio`）の定性的傾向の把握
  以上には用いない。§3〜§4 を完全に満たす形での再実測は専有環境での
  再実施時に行う（下記「スコープ外・申し送り」参照）
- 本フィクスチャ（均等分散マスク・一様乱数ベクトル）では `ann_masked`
  観測 arm に一度も到達しなかったため、「損益分岐点」自体を本節の実測から
  結論づけることはできない。後続実測はクラスタ寄りの可視集合構成
  （テナント単位・連続 id 範囲等）で `ann_masked` を実際に発火させたうえで
  性能比較する設計に改める必要がある
- 専有環境での再実測（上記「判断」のとおり、本節の計測は §3〜§4 の
  輪番方式・参照区間 per-run 生データ保持・baseline 系列の一貫性のいずれも
  完全には満たしていないため、単に環境を専有環境へ切り替えるだけでは
  足りない。§3 が定める「各 candidate の直前に baseline を個別に挟む」
  輪番〔本 PR で `scripts/bench_knn_visible_ratio_sweep.sh` を書き換え
  済み〕での再実行・参照区間 `S0_hot_sql_e2e` の per-run 生データ保持・
  現行スクリプトの輪番方式（各 candidate をその直前の baseline
  `baseline_for_<candidate>` と対応付ける設計）どおり、全 candidate が
  同一の輪番セッション内で測定されたことをログ上検証可能な形で
  残すことを含めて再実測する）、および `ann_masked` を発火させる可視集合
  構成の設計は運用者・後続 Issue への申し送りとする（下記「スコープ外・
  申し送り」参照）

## Issue #488: per-query 写像コスト削減と `full_scan_ratio` 既定値の決定手順

### 前提の訂正

- Issue #487 の可視比率×行数スイープ（上節）は、全 10 測定点・全 candidate で
  `ann_masked` 観測 arm に一度も到達しなかった（均等分散マスク `id % N` は
  `HnswIndex::is_mask_fully_reachable` を通過せず常に `mask_splits_graph` →
  plain scan）。共有 QEMU 環境・計測プロトコル不完全という限定も含め、上節は
  「本節の数値は `full_scan_ratio` 既定値の変更根拠にしない」と明記している。
  したがって本 Issue では既定値を数値先決せず、`Subset` 形状の per-query
  コスト削減を主レバーとした
- Issue #413（`docs/design/hnsw-index.md` §7）の `meta.hnsw_stats` は
  `subset_searches=0`（`point_where`／`vector_knn_where` 計 112 クエリすべてが
  縮退）——25k 行では `Subset` 経路は ANN 探索を一度も完走せず、
  `Overlay::compute`（行ごとの `String` 確保・`HashMap` 引き・`node_matches`
  の dot）＋ `NodeMask` 構築＋分断検査 BFS を払ったうえで plain scan していた。
  つまり Subset 経路の退行は「フォールバックで終わる経路の純粋な
  オーバーヘッド」であり、per-query 写像コスト削減が本 Issue の主眼になる

### 実装した変更（production・`sql/hnsw_cache.rs`）

1. **早期打ち切り**（`prepare_subset`）: `overlay.visible_in_index <=
   arena.len()`（`Subset` アリーナに含まれない索引済みノードは
   `Overlay::compute` が構造的に `STALE_SLOT` として除外するため、写像を
   計算する前でも上界として使える）という不変条件を利用し、この上界がすでに
   `full_scan_ratio` 未満なら `Overlay::compute` を丸ごと省略して
   `PreparedHnswSearch::PlainScanBelowRatio`（新設 variant）を返す。判定式は
   `search_with_overlay` の可視カーディナリティ切替と共有関数
   `below_full_scan_ratio` へ集約し、3 箇所（`search_with_overlay`・
   `Overlay::compute`・`prepare_subset`）の分岐条件が将来ずれることを構造的に
   防ぐ。統計（`fallbacks`／`plain_scans`）は解決時ではなく `search_prepared`
   の探索時（hybrid 密側の複数ラウンドではラウンドごと）に加算し、`FullScan`
   と同じ計上規約を保つ
2. **分断検査（BFS）の遅延**（`Overlay::compute`）: `full_scan_ratio` を新規
   引数として受け取り、`visible_in_index` 確定後にまず比率判定を行う。比率
   未満と判明した場合は `search_with_overlay` が `mask_splits_graph` を参照
   する前に必ず plain scan を選ぶ契約（両者不変）のため、
   `HnswIndex::is_mask_fully_reachable`（|受理ノード|×次数に比例する BFS）を
   呼ばずに `mask_splits_graph = false` とする。恒等マスク（`FullVisible`
   再構築直後）の分断検査は変更していない——可視カーディナリティが常に 1/1
   のため比率判定は必ず通過し、ANN 経路が実際に使われるケースで BFS を
   省略しない

出力は両変更前後で完全に同一（`search_with_overlay` が選ぶ分岐が変わらない
ケースのみ計算を省略するため）。`crates/engine/tests/hnsw_cache.rs` の
既存 10 テスト（`filtered_distance_uses_subset_shape_and_matches_default_
engine_recall`・`full_scan_ratio_ann_side_*`・`full_scan_ratio_plain_scan_
side_*`・`hybrid_queries_use_subset_shape_and_match_default_engine_recall`
等。テナント境界・Recall 一致・非 vacuous 性を含む）はコード無変更のまま
green——`Subset` 経路が実際に `subset_searches > 0` へ到達する既存フィクス
チャ（`filtered_distance_uses_subset_shape_and_matches_default_engine_
recall`）で本変更の分岐選択が既存と一致し続けることを確認済み。
`crates/engine/tests/hnsw_hybrid_refetch.rs`・`tests/incremental_index_hnsw.
rs`・`tests/sql_explain.rs`・`crates/wire-server/tests/wire_explain.rs`
（`EXPLAIN` の `hnsw_params:`／`ann_plan:` 出力・`hnsw_index_cache_stats()`
全 0 契約）もあわせて green を確認した。`cargo clippy --all-targets
--all-features -- -D warnings`・`cargo fmt --check` も通過。

### `full_scan_ratio` 既定値そのものの決定について

`docs/design/benchmark-judgement-policy.md` が定める交互 N≥5 ペア・per-run
生データ・ノイズ帯併記での再スイープ（cluster 形状フィクスチャの追加を含む
ステップ 4〜6。当初計画参照）は、本セッションの計測予算・共有開発環境の
制約により実施できなかった。本 Issue で確定的に持ち帰れる事実は次の 2 点
のみ:

- 上記の per-query 写像コスト削減（早期打ち切り・BFS 遅延）は
  `Overlay::compute` を丸ごと省略できるケースを増やすため、`Subset` 経路が
  ANN 探索を完走しない（Issue #413・#487 いずれの実測点でも到達している）
  局面では構造的に非退行以上（プラスの改善方向）である——出力を変えずに
  計算量だけを削減しているため
- `full_scan_ratio` 既定値（1/10）自体の再調整判断は、Issue #487 で
  記録済みのとおり `ann_masked` 観測 arm に到達するフィクスチャ（クラスタ寄り
  の可視集合構成）が無いと数値的な根拠を持てない。本 Issue はその
  フィクスチャ整備（cluster 形状スイープ）・`feature_bench` 4 arm 前後比較・
  Recall ゲート同一閾値の再測定を実施しておらず、既定値は **1/10 のまま
  変更していない**（「スコープ外・申し送り」参照）。したがって
  `EXPLAIN` の `hnsw_params:` 出力・`search_engine.rs` の `Display` テスト・
  README・`hnsw-index.md` §3 のいずれも本 Issue では変更不要

## Issue #500: ACORN-1 のテナント境界契約整理とゲート条件の設計

親 Issue #499（qdrant `search_on_level_acorn` 型の低選択性フィルタ限定導入）。
前提: #488（前節）・#487（可視比率×行数スイープの所見「`ann_masked` arm
未到達」）。対象ビヘイビア（ポインタのみ）: CORE-9・CORE-10・TASK-132・
RLS-1〜4・RLS-8（TASK-138）・TASK-139。本 Issue は **docs 専任**（`crates/`
配下は無変更）。契約整理とゲート条件の設計のみを行い、実装は #501、可視比率
スイープの実測・既定値確定は #502 の担当とする。

### 目的・位置づけ

Issue #487・#488 の実測で判明したとおり、現行の「1-hop マスク付き探索」は可視比率
が下がると `HnswIndex::is_mask_fully_reachable` の分断検査に落ちて
`mask_splits_graph` → plain scan へ縮退し、`ann_masked` arm（実際に ANN
探索を完走する経路）へ一度も到達しない。`hnsw_subset` 経路が既定エンジン比
37〜45% 悪化する実測（Issue #413）を解く本命候補として、親 #499 は
ACORN-1（不適合な 1-hop ノードのリンクだけを辿り、2-hop 先の適合ノードを
候補に加える方式）を挙げている。ACORN-1 を導入するには、現行の「非受理
ノードを一切辿らない」契約とどう整合するかを先に整理する必要があり、それが
本 Issue の作業である。

### ACORN-1 の方式要約（手法名と採否のみ・コード非転記）

qdrant `search_on_level_acorn`（`lib/segment/src/index/hnsw_index/graph_layers.rs`。
Apache-2.0）は、層探索中に候補ノードの隣接を辿る際、隣接ノードが
フィルタに適合しない場合でも探索を打ち切らず、そのノードのさらに先の
隣接（2-hop 先）まで辿ってフィルタ適合ノードを候補に加える。適合しない
1-hop ノード自体はスコア計算・結果候補にはしない（`max_selectivity` 等の
閾値で低選択性フィルタに限定して有効化）。

### 現行契約の分解: I1（ベクトル非参照）と I2（リンク非参照）

origin/main 時点のコード事実（`hnsw.rs::search_layer` doc コメント・
`search_layer_in` 本体・`is_mask_fully_reachable`／`accepted_reachable_count`。
テスト `search_masked_does_not_traverse_through_a_rejected_bridge_node`・
`search_layer_prefetch_never_touches_rejected_nodes`）を精査すると、
「非受理ノードを一切参照しない」契約は実際には性質の異なる 2 つの不変条件が
束ねられている。

- **I1: ベクトル非参照（P0・不変。ACORN-1 導入後も維持する）** — 非受理
  ノードのベクトルに対するスコア計算・prefetch（#490）を一切行わず、非受理
  ノードのスコアを候補ヒープ・結果ヒープ・停止判定に一切関与させない。
  PR #431（codex-review P0 是正。survey 行 169・172・292・339 参照）が
  修正したのはまさにこの不変条件（非受理ノードのスコアが `results`
  充足・停止判定へ影響していた）であり、faiss `IDSelector` 型（不適合
  ノードにも距離計算＝ベクトルアクセスが発生する設計）を不採用とした
  ADR 側の判断もこの不変条件を根拠にしている
- **I2: リンク非参照（実装上の不変条件。本 Issue でゲート下の緩和対象）** —
  非受理ノードの隣接リストを読まず、その先を探索しない（`search_layer_in`
  が非受理ノードを visited マークのみ付けて打ち切る挙動・
  `is_mask_fully_reachable` の BFS が非受理ノードを中継点にしない挙動）。
  PR #431 の是正では I1 と同時に導入されたが、ADR（`ann-index-adoption.md`
  「実装ガイド（B 案）」節）本文が要求しているのは「非可視ノードを探索経路
  として通過させる設計は不採用」であり、この「非可視ノード」は RLS の
  意味での**他テナントの不可視行**を指す。ただし `PolicyContext::is_visible`
  （`policy.rs`）は許可された `Public` 行をテナント不一致でも可視とするため、
  「他テナント行がそもそもグラフに存在しない」は正確ではない。per-`(table, ctx)`
  索引（#409。索引は ctx 可視アリーナのみから構築）に含まれるのは常に
  「構築時点で `is_visible` を通過した行」（自テナント許可行、および他テナントの
  `Public` 許可行を含む）のみであり、**構築時点で ctx に不可視だった行の情報は
  索引に含まれない**という前提のもとで、I2 はテナント境界そのものではなく、
  その上に置かれた「探索経路の単純化」という実装上の選択だったと整理できる

ACORN-1 は **I1 を完全に維持したまま I2 のみをゲート下で緩和する**方式で
ある: 非受理ノードは「リンクを読むだけの中継点」として扱い、スコア計算・
ヒープ登録・結果への混入は一切行わない。2-hop 先の**受理**ノードのみ、
通常どおりスコア計算して候補に積む。

### テナント境界・存在情報漏えいの論点表

| 論点 | 整理 |
| ---- | ---- |
| ctx 不可視行（構築時点で不可視だった行） | 索引は `(table, ctx)` キーで ctx 可視アリーナのみから構築（#409）。索引に含まれるのは常に「構築時点で `PolicyContext::is_visible` を通過した行」（自テナント許可行、および他テナントの `Public` 許可行を含む）のみであり、構築時点で ctx に不可視だった行（許可されない可視性ラベルの行、テナント不一致の `Private` 行）は索引にそもそも含まれない。非受理ノードとして現れるのは (a) 構築後に失効した stale ノード（後述）、(b) `WHERE` で除外された ctx 可視行、の 2 種のみで、いずれも構築時点で ctx が可視性判定を通過した行に限られる |
| stale ノードのうち「不可視化」（構築後に別テナントへ再割当された行）の subcase | 索引が保持するリンクは構築時点（ctx がまだその行を可視として持っていた時点）の旧ベクトル近傍を符号化したものであり、再割当後の新ベクトル・新テナントの情報は索引に含まれない。ctx は既にその行を検索可能だった時点の情報を再利用するだけであり、再割当後のテナントへの横断経路は生じない。再構築判定（`needs_rebuild`／`REBUILD_DELTA_RATIO`、既定 1/10。`sql/hnsw_cache.rs`）は `prepare_full_visible` 経路でのみ評価され、`WHERE` 事前フィルタ付きクエリが通る `prepare_subset` は同じ base をそのまま使い続け再構築判定を行わない。そのため `Subset` 形状のクエリのみが継続する場合は比率超過後も再構築が発生せず、stale ノードの保持期間に上限はない。ただし stale ノードが保持する情報は上記のとおり 構築時点で ctx が可視だった旧ベクトルの近傍に限られ、保持期間の長さ自体が テナント境界の破れを生じさせるものではない |
| 内容変更ノード | 同様に旧ベクトルの近傍リンクを読むだけで、旧ベクトルは ctx が構築時点で参照可能だったものに限られる |
| `WHERE` 除外行（`Subset` 形状） | RLS 上は ctx にとって可視行であり、テナント境界の問題ではない（既存「ADR との整合」節の整理どおり） |
| ベクトルアクセス（I1） | 非受理ノードの `NodeSource::score`・prefetch は ACORN-1 経路でも呼ばない。非受理ノードのスコアが停止判定・ヒープへ入らないため、PR #431 是正の趣旨は完全に維持される |
| 存在情報の副次チャネル（処理量・応答時間） | ACORN-1 導入後に処理量が依存するのは ctx 自身の索引内の stale／`WHERE` 除外ノード数のみであり、他テナントの行数には依存しない。ADR が不採用とした事後フィルタ型（不可視行がグラフ上に存在し、探索がそれを辿る構造）とは前提が異なる |
| 結果への混入防止 | 結果ヒープには受理ノードのみが積まれる。呼び出し元の写像・`(tenant_id, id)` キー照合・`kernel::dot` 再計算・`provider_result_is_valid`・`RlsSafetyNet` の多層防御（#409 以降の既存契約）は不変のまま維持される |
| 既存ビット同一契約 | `accept == None`（`search`・構築経路）は ACORN 非適用時とビット同一のまま。ACORN 無効時（1-hop レジーム）も現行の探索と同一 |
| ADR 本文との関係 | 「非可視ノードを探索経路として通過させる設計は不採用」の「非可視ノード」＝構築時点で ctx にとって不可視だった行であり、per-ctx 索引にはそもそも含まれない（他テナントの `Public` 許可行は `is_visible` を通過するため対象外）。ADR 本文は無変更のまま、本節がその解釈を記録する |

### 成立可否の判断: 条件付き成立

上表のいずれの論点でも、他テナント行への横断経路・存在情報の副次漏えい・
I1（ベクトル非参照）の破壊は生じないことを確認できた。したがって
**ACORN-1 導入は条件付き成立**と判断する。条件は次の 3 点である。

1. I1（ベクトル非参照）を不変条件として維持すること（非受理ノードの
   `NodeSource::score`・prefetch 呼び出しをしない）
2. I2（リンク非参照）の緩和はゲート（後述の可視比率レジーム）下でのみ行い、
   ゲート外（既定）では現行の 1-hop 契約を維持すること
3. per-`(table, ctx)` 索引の前提（索引は ctx 可視アリーナのみから構築する
   #409 の設計）を維持すること——グローバル索引・複数テナント共有索引への
   変更は本整理の前提を崩すため対象外

### ゲート条件の設計

Issue #488 が確立した「1 つの判定式を複数箇所で共有し分岐の乖離を構造的に防ぐ」
方針（`below_full_scan_ratio` を `search_with_overlay`・`Overlay::compute`・
`prepare_subset` の 3 箇所が共有）を、**3 区分レジーム**へ拡張する設計と
する。

- 定義: `r = visible_in_index / index_len`（RLS 事前フィルタにより正確に
  既知。サンプリング推定は不要——survey 行 169 の既存整理と同じ）
- レジーム分類（設計案。`below_full_scan_ratio` を
  `traversal_regime_for(visible_in_index, index_len, params) ->
  {PlainScan, OneHop, TwoHop}` へ拡張し、`search_with_overlay`・
  `Overlay::compute`・`prepare_subset` の 3 箇所が同一情報源を共有する）:
  - `r < full_scan_ratio` → `PlainScan`（現行どおり不変）
  - `full_scan_ratio ≤ r ≤ acorn_max_visible_ratio` → `TwoHop`（ACORN-1）
  - `r > acorn_max_visible_ratio`（または ACORN 無効）→ `OneHop`（現行の
    マスク付き探索）
- 適用前提（既存条件をすべて維持）: `SearchEngineKind::Hnsw` opt-in・
  `accept.is_some()`・`precision` モード除外・`k ≤ MAX_EF`・形状は
  `FullVisible`／`Subset`（hybrid 密側 `HnswDenseProvider` は
  `search_prepared` 経由で自動的に同じ分類に従う想定）
- パラメータ設計: `ValidatedHnswParams::acorn_max_visible_ratio:
  Option<Ratio>`。`None` は無効（`TwoHop` 区間が空）。
  `with_acorn_max_visible_ratio` で `full_scan_ratio ≤ 値 ≤ 1/1`・分母 ≥ 1
  を fail-closed に検査する（`with_full_scan_ratio` と同型の検査規約）。
  `full_scan_ratio` 側を後から変更して逆転した場合も拒否する
- **既定値: #501 の実装時点では無効（`None`）で出荷する**。理由は、ACORN
  区間の現行挙動（`mask_splits_graph` → plain scan）は厳密解であり、
  ACORN-1 はこれを近似解へ置き換える設計であるため、Recall／レイテンシの
  トレードオフが #502 で未計測のまま既定 ON にすると hnsw opt-in
  利用者の結果を黙って変えてしまう。候補既定値として qdrant
  `max_selectivity`（上流ドキュメントで確認できた範囲の値のみ帰属。約
  0.4＝`4/10`）を記録し、確定は #502 の実測後に行う
- 分断検査の同期（最重要の設計制約）: `is_mask_fully_reachable`／
  `accepted_reachable_count` は**探索と同じレジームで** BFS する必要が
  ある。`TwoHop` レジームでは非受理ノードを中継点として 1 段だけ辿る BFS
  へ拡張しないと、#487 と同じく全点が `mask_splits_graph` に落ちて
  ACORN が一度も発火しない。起点共有（`search_entry_for_mask`。PR #435
  で確立した契約）は維持する。`hnsw-hybrid-iterative-scan.md`「DISTANCE
  経路の `masked_short` 到達不能性（証明）」は 2-hop レジーム下で成立が
  変わりうるため、#501 での再検証が必要（下記「申し送り」参照）
- コスト注記: `Subset` 形状では `Overlay::compute` ＋ BFS がクエリ毎に
  発生し、`TwoHop` の BFS は 1-hop 非受理ノードの隣接も走査するため
  `O((|受理| + |1-hop 非受理|)·M0) ≤ O(N·M0)` に収まる。#488 が示した
  とおり `hnsw_subset` 退行の実体は overlay＋BFS オーバーヘッドである
  ため、#502 は ANN 完走率だけでなく**総レイテンシ**を計測すべきことを
  申し送る
- 停止性・DoS: visited マークにより各ノードの隣接リスト読み取りは 1 クエリ
  1 回以下に抑えられるため、総走査は `O(N·M0)`（マスクなし最悪ケースと
  同オーダー）で構造的に有界とする。この構造的保証を主とし、診断統計
  `acorn_searches`／`acorn_expansions`（`HnswCacheStats` へ追加する設計。
  `EXPLAIN` へは非露出）で観測可能にする。追加の明示的な予算上限を設ける
  場合は「予算超過時は当該クエリを plain scan へ fail-closed 縮退する」と
  定義する
- 決定性: 非受理ノードの隣接は格納順で走査し、受理された 2-hop ノードは
  既存の `ScoredNode` 順序規約（スコア降順・id 昇順）で候補化する。同一
  索引・同一クエリ・同一マスクで再現的な結果になる設計とする
- `EXPLAIN`: `hnsw_params:` は構築時静的パラメータのみを露出する既存契約
  （#411）のため、`acorn_max_visible_ratio` も静的値のみを露出する。実行時
  のレジーム選択・可視カーディナリティは非露出のまま維持する
- 実質的な適用範囲: warm な `FullVisible`（`r=1`）・再構築閾値内の
  `FullVisible` は `OneHop` のまま変わらない。ACORN-1 が効くのは主に
  `Subset` 形状（と大量削除直後の `FullVisible`）であることを明記する

### #501（実装）・#502（実測）への申し送り

- #501 で必要な設計契約: `search_layer_in` へのレジーム引数（`TwoHop` かつ
  `accept.is_some()` のときのみ非受理ノードの `graph.neighbors` を読む）・
  `greedy_descend_masked`（上位層探索）の扱い（上位層は 1-hop のまま
  据え置くか 2-hop 適用可否を #501 で判断・記録する）・
  `is_mask_fully_reachable` のレジーム対応・`Overlay::compute` へのパラ
  メータ受け渡し・`ValidatedHnswParams` 拡張・`search_engine.rs`
  `Display` 実装の追従・診断統計の追加
- #501 のテスト契約: `search_masked_does_not_traverse_through_a_rejected_
  bridge_node` はレジーム依存になる（`OneHop` では現行どおり、`TwoHop`
  では 2-hop 先の受理ノードが結果に現れるが、橋渡し役の非受理ノード自体は
  決してスコアされないことを固定する）。
  `search_layer_prefetch_never_touches_rejected_nodes` は「非受理ノード
  自身は先読みしない・非受理ノード経由で到達した受理 2-hop ノードの
  先読みは可」へ拡張する。I1 の機械検証として、`NodeSource::score` 呼び出し
  を記録するアダプタ（既存 trait を利用）で非受理ノードへの呼び出しが 0
  件であることを固定する。既定エンジン対照 Recall@10 ≥ 0.9・可視外
  テナント非混入・非 vacuous（`acorn_searches > 0`）を受け入れ条件とする。
  新規 `unsafe` なし・依存追加なしを維持する
- #502 への申し送り: 可視比率スイープはクラスタ寄りの可視集合（連続 id・
  同一属性値）で `ann_masked`／`TwoHop` arm を実際に発火させる設計へ
  改める（#487 の申し送りを継承）。`docs/design/benchmark-judgement-
  policy.md` §3〜§4（交互 min-of-N・N≥5・参照区間 per-run 生データ・
  ノイズ帯併記）に準拠する。`RECALL_ENGINE=hnsw` の 3 ゲート同一閾値の
  確認を含める。既定値（`None` → `4/10` 候補）の確定は #502 の実測後に
  行う
- `hnsw-hybrid-iterative-scan.md`「DISTANCE 経路の `masked_short`
  到達不能性（証明）」の 2-hop レジーム下での再検証は #501 の担当とし、
  同 doc へ追記する
- ADR `ann-index-adoption.md` 本文の改訂要否: 本節の解釈で足りると判断し、
  ADR 本文の改訂は不要とする。オーナーが明文化を望む場合は別 Issue とする
  （起票はユーザー承認事項のため本 Issue では行わない）

## Issue #501: `search_layer` の 2-hop 展開（ACORN-1）実装記録

Issue #500 の設計契約（上記「ゲート条件の設計」節）に従い、`crate::hnsw::HopMode::TwoHop`（マスク付き探索限定）を実装した。

### 実装の要点

- `crate::hnsw::HopMode`（`OneHop`／`TwoHop`）・`ValidatedHnswParams::acorn_max_visible_ratio`（既定 `None`）・`with_acorn_max_visible_ratio`（`den>=1`・`num<=den`・`ratio >= full_scan_ratio` を fail-closed 検査。`with_full_scan_ratio` 側の後付け逆転も同様に拒否）を追加。両アクセサは `full_scan_ratio`／`with_full_scan_ratio` と同じ公開範囲（`pub`）にした——`ValidatedHnswParams` の他のビルダーメソッドと同様、統合テストが opt-in エンジンを組み立てるための公開 API として必要なため（`EXPLAIN` への非露出という D3 の判断とは別軸）
- 非受理ノードに出会ったときの規則を `bridge_expand`（自由関数）へ集約し、`search_layer_in`（ビーム探索の受理判定後分岐）・`HnswIndex::accepted_reachable_count`（`is_mask_fully_reachable_with` の BFS）の双方が同一実装を共有する。規則: 非受理ノード N を「1-hop 非受理として初めて訪問した」ときのみ visited を付けて `bridge_expand(N)` を呼び、N の隣接（2-hop 候補）のうち受理済みかつ未訪問のものだけを visited を付けて候補化する。2-hop 候補が非受理の場合は visited を付けない（別の 1-hop 非受理ノードから改めて中継点として使えるようにするため。ただし 3-hop 以上へは展開しない——D1 の設計どおり 1 段のみ）
- この規則により各ノードの隣接リスト読み取りはクエリ全体で高々 1 回に構造的に有界（visited マークが「展開済み」を兼ねる）となり、総走査量は `O(N・M0)` で有界（追加の明示的訪問予算上限は設けていない。§ Issue #500 の「停止性・DoS」節どおり）
- 上位層の貪欲降下（`greedy_descend_masked`）は 1-hop のまま据え置いた（D2。層 0 の起点選択にのみ関与し Recall への寄与が薄い一方リスクのみ増えるため）
- `sql::hnsw_cache::TraversalRegime`（`PlainScan`／`OneHop`／`TwoHop`）・`traversal_regime_for` を新設し、`Overlay::compute`（分断検査の hop 選択）・`search_with_overlay`（plain scan 判定・hop 選択の両方）が同一情報源を共有する形へ一本化した（Issue #488 の「単一情報源」方針をそのまま踏襲・拡張）。`search_with_overlay` 内での `below_full_scan_ratio` 再計算は撤去し `overlay.regime` を直接参照する
- `EXPLAIN` への `acorn_max_visible_ratio` の露出は D3 のとおり見送った（`full_scan_ratio` と同区分。`hnsw_params:` 行は無変更のまま据え置き）
- 診断統計 `HnswIndexCacheStats::acorn_searches`／`acorn_expansions` を追加（`TwoHop` レジームで縮退なしに完走した回数・`bridge_expand` が候補化した 2-hop ノード数の累計。`EXPLAIN` へは非露出）

### #500 の設計からの差異・確定した点

- `HnswIndex::search_masked`／`search_masked_with`・`HnswIndex::search_layer`（旧・`OneHop` 専用ラッパー）は既存シグネチャのまま `HopMode::OneHop` へ委譲する薄いラッパーとして残し、`HopMode` を受け取る新規メソッド（`search_masked_with_hop`・`search_layer_with_hop`）を追加する形にした（Issue #497 のパターンに倣う。既存呼び出し元・テストの変更を最小化し、R1 のビット同一性を「既存コードパスを一切変更しない」ことで構造的に保証するため）。`is_mask_fully_reachable`（`OneHop` 専用ラッパー）・旧 `search_layer`／`search_layer_with` は production からの呼び出しが `search_masked_with_hop`／`search_layer_with_hop` へ一本化されたことで非到達になったため `#[cfg(test)]` にした（`select_neighbors_heuristic` と同じ既存の扱い）
- `TraversalRegime::hop()` は `PlainScan` に対しても `OneHop` を返す（呼び出し元は `PlainScan` の場合その値を使わず先に plain scan 分岐へ抜ける契約のため、値自体は「使われない」）

### 検証

- 単体テスト（`crates/engine/src/hnsw.rs`）: 橋渡しノード経由の 2-hop 到達（`search_masked_two_hop_traverses_through_a_rejected_bridge_node`）・3-hop 非到達（`search_masked_two_hop_does_not_traverse_three_hops`）・I1 機械検証（`search_masked_two_hop_never_scores_rejected_nodes`。`NodeSource::score` 呼び出しを記録するテスト専用ラッパーで非受理ノードへの呼び出しが 0 件・受理 2-hop ノードへの呼び出しが非 vacuous であることを固定）・`accept == None` でのビット同一性（`search_masked_two_hop_matches_search_when_mask_is_none`）・決定性（`search_masked_two_hop_is_deterministic`）・停止性（`search_masked_two_hop_reads_each_node_adjacency_at_most_once`。`Adjacency::neighbors` 呼び出し回数を記録するラッパーで最大 1 回であることを機械検証）・`ValidatedHnswParams` の検証規則（`acorn_max_visible_ratio_defaults_none_and_rejects_invalid_ratios`）を追加。既存の HNSW 単体テスト（103 件）は全て無変更のまま green（R1 のビット同一性の直接証拠）
- 統合テスト（`crates/engine/tests/hnsw_cache.rs`）: `acorn_two_hop_subset_shape_matches_default_engine_and_never_leaks_across_tenants`（`Subset` 形状・選択率 20% のフィクスチャで `OneHop`（既定）だと `mask_splits_graph` が 20/20 発火することを実測確認したうえで `acorn_max_visible_ratio = 1/1` を opt-in すると全 20 クエリが `subset_searches`〔縮退なしの ANN 完走〕へ転じ、既定エンジン対照 Recall@10 ≥ 0.9・tenant-b 非混入・`plain_scans == 0`・`acorn_searches`/`acorn_expansions` 非 vacuous を満たすことを固定）・`acorn_disabled_by_default_keeps_existing_behavior_unaffected`（同じフィクスチャで opt-in しない場合 `acorn_searches == 0` のまま `mask_splits_graph` が従来どおり発火することを固定。opt-in なしでは挙動が一切変わらないことの直接証拠）
- `hnsw_search.rs`・`hnsw_hybrid_refetch.rs`・`incremental_index_hnsw.rs`・`sql_explain.rs`（engine）・`wire_explain.rs`（wire-server）は無変更のまま green
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`・`cargo test --workspace --all-features` を実行（結果は本 PR の Test plan 参照）
- 新規 `unsafe` は 0（既存の禁止方針を維持）。依存追加なし（`Cargo.toml` 無変更）

`acorn_max_visible_ratio`（本節）・`full_scan_ratio`（Issue #409）は Issue #657
で `wire-server` CLI（`--hnsw-acorn-max-visible-ratio`／
`--hnsw-full-scan-ratio`）から設定可能になった（`EXPLAIN` 非露出方針は不変。
詳細は `docs/design/hnsw-search-engine-wiring.md`「CLI 探索パラメータ opt-in
（Issue #657）」節参照）。

## Issue #498 追記: visited 集合切替閾値の可視比率スイープ確認（層 2）

Issue #497（`VisitedSparse`・`sparse_visited_max` 閾値機構）の閾値既定値
確定にあたり、本節の可視比率スイープ基盤（`SWEEP_CANDIDATES=visited`
opt-in。`full_scan_ratio` を `0/1` へ固定した `hnsw_force_ann_dense`／
`hnsw_force_ann_sparse` の 2 candidate）を再利用して確認計測を実施した。
ratio=1/2（可視率 50%）では `ann_masked` が発火し
`sparse_visited_searches` の delta が期待どおり観測された一方、ratio=1/10
では上記「Issue #487: 可視比率 × 行数の損益分岐点実測」節と同じ構造的限界
（均等分散マスクによる `mask_splits_graph` 縮退）で切替自体に到達しなかっ
た。閾値既定値そのものの前後比較（層 1・dense/sparse の直接 A/B）・判断は
`docs/design/hnsw-search.md`「Issue #498」節参照。クラスタ寄り fixture の
整備は引き続き #502 の担当。production コード無変更（本節の計測は
`crates/engine/benches/`・`scripts/` のみ）。

## Issue #502: ACORN-1 の可視比率別 Recall・レイテンシ前後比較と既定値

Issue #501 が固定した ACORN-1（`acorn_max_visible_ratio` opt-in）を可視比率
横断で実測し、既定値（`None` のまま据え置くか候補値へ変更するか）を判断
するための計測・回帰テストを追加した。production コード
（`crates/engine/src/`）は無変更・テスト／ベンチ／スクリプト／docs 専任。

### 計測基盤

- `crates/engine/benches/harness/bench_engine.rs`: `parse_acorn_max_visible_ratio`
  （`BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO` 用・`parse_full_scan_ratio` と
  同型の fail-closed パーサ）・`ExpectedArm::AnnMaskedTwoHop`・
  `expected_arm_acorn`（`expected_arm` の基底判定が `AnnMasked` かつ
  `acorn_max_visible_ratio` が可視比率を含むときのみ `AnnMaskedTwoHop` へ
  格上げする。`sql::hnsw_cache::traversal_regime_for` の `TwoHop` 判定式
  〔`<=`〕を複製する予測ラベル専用関数——既存 `expected_arm` は変更せず
  後方互換を保つ）を追加
- `crates/engine/benches/knn_profile_bench.rs`: 可視比率スイープ
  （`run_visible_ratio_sweep`）専用に `BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO`
  を追加。`build_core_for_sweep` は `full_scan_ratio_override` 適用後に
  `with_acorn_max_visible_ratio` を適用する順序で `Hnsw`／`HnswF16` 両
  arm に結線（`with_acorn_max_visible_ratio` は `acorn >= full_scan_ratio`
  を検証するため、この適用順が前提）。`observed_arm_label` は
  `acorn_searches_delta > 0` を最優先で判定し `ann_masked_two_hop` を返す
  （`acorn_searches` は `subset_searches` の部分集合のため判定順が重要）。
  期待値が `AnnMaskedTwoHop` なのに `acorn_searches_delta == 0` だった場合は
  vacuous な計測として fail-closed に拒否する（`sparse_visited` の既存
  ガード・Issue #498 の方針を踏襲）。他モード（`INDEX_MEMORY`／`HOT_ONLY`）
  との併用・`ENGINE=brute_force` との併用は拒否。knob 未設定時は本 Issue
  導入前と出力・処理が完全に同一
- `scripts/bench_knn_visible_ratio_sweep.sh`: `SWEEP_CANDIDATES=acorn` で
  `hnsw_one_hop`（既定・ACORN 無効）／`hnsw_acorn_1_1`（`acorn_max_visible_
  ratio=1/1`。full_scan_ratio 以上のあらゆる比率で TwoHop）の 2 candidate
  を追加。既定比率は `1/2・1/4・1/5・1/10・1/20`（scale=1）。`4/10`
  （既定値候補）を独立 candidate としないのは、この既定比率集合では各点の
  期待 arm が 2 candidate のいずれかと完全に一致し（1/2 は 1-hop、
  1/4・1/5・1/10 は 2-hop、1/20 は plain scan——いずれも `full_scan_ratio`
  境界と `4/10` 境界を同じ 5 点で挟めているため）3 candidate 目が新規の
  測定点を生まないと判断したため。`resolve_env` の全分岐で
  `ACORN_MAX_VISIBLE_RATIO` を明示設定（親シェル export の漏れ防止）。
  `--summarize` に `arm expected=/observed=`・`acorn(delta)` 行の抽出を追加

### フィクスチャ形状のプローブ（クラスタ寄り fixture は不要と判明）

Issue #487・#498 の「均等分散マスク（`id % N`）は疎な可視集合では 1-hop の
`mask_splits_graph` に構造的に到達不能」という知見を踏まえ、`MASK_SHAPE`
knob（modulo／clustered 切替）の要否を計測前にプローブした。ところが本
Issue の `knn_profile_bench` フィクスチャ（一様乱数コーパス・dim128・
25,000 行）は、`acorn_max_visible_ratio` opt-in の有無に関わらず一貫して
`ann_masked`（1-hop）／`ann_masked_two_hop`（ACORN opt-in・可視比率が
`full_scan_ratio` 以上 `acorn_max_visible_ratio` 以下の範囲）へ到達し、
`mask_splits_graph` は一度も観測されなかった（可視比率 1/2・1/4・1/5・
1/10 のいずれでも。1/20 は `full_scan_ratio` 未満で構造的に `plain_scan_ratio`）。
均等分散マスク自体が分断を起こしにくいのではなく、Issue #487・#498 で
観測された分断は**その計測のクラスタなし・25,000 行という組み合わせ**に
固有だった可能性が高い（本 Issue のフィクスチャは同じく一様乱数だが
`id % N == 0` を `bucket` 列に事前投入する形が異なるため、単純な比較では
断定できない）。いずれにせよ本 Issue の計測目的（ACORN opt-in の効果が
観測できる測定点の確保）には十分だったため、`MASK_SHAPE` knob の追加は
見送った（modulo 一本で完結）。

一方、`tests/hnsw_acorn_recall.rs` の層 B（後述・クラスタ構造ありコーパス・
25,000 行・dim128）では逆に、可視比率 1/2・1/4・1/5・1/10 のすべてで
`mask_splits_graph` が発火し ACORN opt-in（`4/10`）でも解消しなかった。
クラスタ構造ありコーパスでは分断が可視比率だけでなく規模（1,200 行の層 A
では解消するが 25,000 行の層 B では解消しない）にも依存する可能性がある
ことを示す informational な観測として記録する（後続 Issue での深掘りに
申し送る）。

### 事前登録した判定規則

1. 各測定点（scale × ratio）: `ratio(min) = TwoHop_min / OneHop_min`
   （主統計量）。`Improved` は固定 ±5% 帯**と**参照区間（`S0_hot_sql_e2e`）
   実測帯の両方を超え、かつ `ratio < 1` の場合のみ（`docs/design/
   benchmark-judgement-policy.md` §3〜§4）
2. Recall: TwoHop の各測定点で既定エンジン（brute_force）対照
   `Recall@10 ≥ 0.9`（Issue #409／#501 と同じ絶対下限）
3. 既定値: 候補は `4/10`（qdrant `max_selectivity` 相当。上流ドキュメントで
   確認できた範囲の値のみ帰属）。採用条件は「production 到達区間
   （`r ≥ full_scan_ratio=1/10`）の全 TwoHop 点で Recall ≥ 0.9 かつ
   Improved、かつ専有環境（`BENCH_DEDICATED_ENV=1`）での実測」。本開発
   環境は共有 QEMU 環境（`docs/design/benchmark-judgement-policy.md` §5）
   のため、レイテンシの Improved 判定を Accepted 扱いにできない——既定値は
   `None` のまま据え置き、候補値・実測表を運用者へ申し送る
4. 1/20 は `r < full_scan_ratio` で ACORN 有無に関わらず `PlainScan`
   （対照点として記録するのみ）

### 専用回帰テスト（`crates/engine/tests/hnsw_acorn_recall.rs`）

`tests/hnsw_cache.rs::seed_acorn_fixture`（選択率固定 20%・単一固定点）を
可視比率 1/N（N ∈ {2,4,5,10}）横断へ一般化した。

- 層 A（常時実行・`make ci` 対象。1,200 行・dim16）:
  `acorn_4_10_regime_sweep_matches_expected_hop_mode_and_recall` は
  `acorn_max_visible_ratio=4/10` opt-in のもとで N=4,5,10（r ≤ 4/10）が
  `acorn_searches > 0`〔TwoHop〕へ、N=2（r=1/2 > 4/10）は
  `acorn_searches == 0`〔既存の 1-hop 判定のまま〕であることを固定し、
  全点で既定エンジン対照 Recall@10 ≥ 0.9・`Subset` 形状の非 vacuous 性
  （`subset_searches > 0`）・tenant-b private 行の非混入・`builds` が
  warm-up 1 回のみであることを検証する。
  `acorn_disabled_by_default_keeps_regime_sweep_unaffected` は
  `acorn_max_visible_ratio` 未設定（既定 `None`）では同じ可視比率横断で
  `acorn_searches`／`acorn_expansions` が常に 0 のまま（Issue #501 の既存
  契約を維持）であることを固定する
- 層 B（`#[ignore]`・`make hnsw-acorn-recall`。25,000 行・dim128）:
  `layer_b_25k_dim128_acorn_regime_sweep_report` は同じ可視比率横断を
  より現実的な規模で実行し Recall・カウンタを表として標準出力へ記録する
  （上記「フィクスチャ形状のプローブ」節のとおり、この規模・コーパス
  構成では `mask_splits_graph` へ縮退し ACORN opt-in の効果を観測でき
  なかった——informational な記録に留め、layer A の主張を弱めない）

### `RecallEngine::HnswAcorn`（Recall ゲート同一閾値検証）の見送り

3 つの Recall ハーネス（`hybrid_recall.rs`・`rerank_recall.rs`・
`query_planning_recall.rs`）のゲートコーパスは単一テナント・単一
`PolicyContext` の `FullVisible` 形状（`WHERE`・部分 RLS を持たない）で
構成されている。ACORN-1 は SCALAR 事前フィルタ付き DISTANCE（`Subset`
形状。可視ノードの一部が索引外という状況）でのみ意味を持つ機構であり、
`FullVisible` 形状では非受理ノード自体が存在しないため `bridge_expand` は
構造的に一度も発火しない——`RecallEngine::HnswAcorn` を追加しても
`RecallEngine::Hnsw`（Issue #412）と構造的に同一の測定になり、ACORN 固有の
検証にはならない。#616 の申し送り「`RECALL_ENGINE=hnsw` 3 ゲートが ACORN
opt-in 経路でも同一閾値を通過することの確認」は、`RecallEngine::Hnsw`
自体が既に ACORN 無効（`acorn_max_visible_ratio=None`）で測定しており
（Issue #412・#515）、ACORN opt-in を追加してもゲートの正しさに対する
追加の保証を得られないと判断し、`RecallEngine::HnswAcorn`
の追加・`recall.yml` の `strategy.matrix.recall_engine` への `hnsw_acorn`
追加はいずれも見送った（`tests/hnsw_acorn_recall.rs` の `Subset` 形状
専用フィクスチャが ACORN 固有の Recall 検証を非 vacuous に担う）。

### 対象外・申し送り

- `hnsw_search_bench.rs` での層 1 直接 A/B（`HopMode::OneHop` vs
  `TwoHop` の総レイテンシ比較）: `HopMode`／`search_masked_with_hop` が
  `pub(crate)` のため `bench-internals` feature 限定ラッパー（production
  変更）が必要。層 2（SQL 表層経由の可視比率スイープ）で総レイテンシは
  担保済みのため見送り
- `acorn_max_visible_ratio` 既定値の変更（`None` → 候補値）: 上記「事前
  登録した判定規則」3 のとおり共有 QEMU 実測では Accepted にできない。
  専有環境での再実測と既定値変更はオーナー判断
- クラスタ構造ありコーパス・25,000 行規模での分断の深掘り（上記
  「フィクスチャ形状のプローブ」節の観測）: 後続 Issue へ申し送り
- scale=4（100k 行）規模点: 計測時間の都合で `SWEEP_SCALES=4` opt-in に
  留める（既定は scale=1）

## Issue #659: 選択率 33%（crossdb fixture）での `ann_masked` 到達実測と既定値判断

### 背景

Issue #487 の可視比率スイープ（均等分散 `id % N` マスク・一様乱数ベクトル）
は 1/2〜1/10 の全点で `mask_splits_graph`（連結性検査による plain scan 縮退）
に落ち `ann_masked` を一度も観測できなかった。Issue #658（PR）は
`lang='ja'`（crossdb fixture・可視 23,000 行中 7,621 行 ≒ 33.1%）が
`full_scan_ratio`（既定 1/10）を超えるため `mask_splits_graph` 縮退が
有力な要因と**推測**したが、`hnsw_index_cache_stats()` が wire 非露出の
ため確認できないまま本 Issue へ申し送られた。本節はその確認結果を記録する。

### 事前登録した判定規則（実測前に本節へ記載）

- arm 分類は `crates/engine/benches/knn_profile_bench.rs::observed_arm_label`
  と同一の優先順位（acorn > subset > plain_scan_ratio > mask_splits_graph
  > masked_short > none）。全カウンタ 0 は vacuous として fail。
- `ann_masked` / `ann_masked_two_hop` に到達した arm は、同 arm のフィルタ
  なし Recall@10 に対し `filtered >= unfiltered - 0.02` であること
  （フィルタ付き ANN がフィルタなし ANN より悪化しないことの検証）。
- 縮退（plain scan）した arm は既定エンジンと厳密一致（Recall 1.0）で
  あること。
- 既定値（`full_scan_ratio`／`acorn_max_visible_ratio`）の変更提案は、
  (i) 決定的成果（arm 到達・Recall）で優位が示され、かつ (ii) 専有環境
  （`BENCH_DEDICATED_ENV=1`）でレイテンシの Improved が両ノイズ帯を
  超えた場合のみ Accepted にする。本環境（共有 QEMU）では (ii) を満たせ
  ないため、実測後も既定値は据え置く。

### 計測基盤

`crates/engine/tests/hnsw_crossdb_selectivity.rs`（層 A は `make ci`
対象・fixture 不要。層 B は `#[ignore]`・`make hnsw-crossdb-selectivity
CROSSDB_DIR=<dir>`・release 専用）。crossdb fixture の実体
（`docs25k.redb`・`queries200.jsonl`。`seed_docs seed <db> 25000 128`
形式）を対象に、5 arm（`default`＝既定 `full_scan_ratio=1/10`・
`acorn_4_10`＝`acorn_max_visible_ratio=4/10`・`acorn_1_1`＝同 `1/1`・
`full_scan_2_5`＝`full_scan_ratio=2/5`〔fixture 選択率 33.1% より高い
閾値で `PlainScanBelowRatio` 早期打ち切りを狙う対照〕・`force_plain`＝
`full_scan_ratio=1/1`〔常に plain scan の対照〕）× 2 k（10・200）で
`hnsw_index_cache_stats()` の増分から到達方式を分類し、`lang='ja'`
フィルタ付き／なし DISTANCE の既定エンジン対照 Recall@10 を記録する。
カウンタの before/after 窓は**フィルタ付き 200 クエリのみ**に限定する
（フィルタなしクエリは `FullVisible` 経路〔可視比率 1.0〕で別途
`traversal_regime_for` を通り `acorn_searches` 等を独立に加算しうるため、
同一窓に混ぜるとフィルタ付き経路の到達方式判定が汚染される。実装時に
一度この汚染を作り込み〔`acorn_1_1` が誤って `ann_masked_two_hop` と
分類された〕、レビューで検出して分離した。詳細はテストのコメント参照）。

### arm 表（実測。フィルタ付き専用窓へ分離後に再現確認）

共有 QEMU 環境・fixture: `docs/design/bench-data/crossdb-selectivity-659/arm-report-20260909T013000Z.log`。

| arm | k | ja_visible/index | subset | acorn | plain_ratio | mask_split | 分類 | recall filtered | recall unfiltered |
| --- | - | ----------------- | ------ | ----- | ------------ | ---------- | ---- | ---------------- | ------------------- |
| default | 10 | 7621/23000 | 0 | 0 | 0 | 200 | `plain_scan_mask_split` | 1.0000 | 0.8670 |
| default | 200 | 7621/23000 | 0 | 0 | 0 | 200 | `plain_scan_mask_split` | 1.0000 | 0.8515〜0.8516 |
| acorn_4_10 | 10 | 7621/23000 | 0 | 0 | 0 | 200 | `plain_scan_mask_split` | 1.0000 | 0.8670 |
| acorn_4_10 | 200 | 7621/23000 | 0 | 0 | 0 | 200 | `plain_scan_mask_split` | 1.0000 | 0.8515 |
| acorn_1_1 | 10 | 7621/23000 | 0 | 0 | 0 | 200 | `plain_scan_mask_split` | 1.0000 | 0.8670 |
| acorn_1_1 | 200 | 7621/23000 | 0 | 0 | 0 | 200 | `plain_scan_mask_split` | 1.0000 | 0.8515 |
| full_scan_2_5 | 10 | 7621/23000 | 0 | 0 | 200 | 0 | `plain_scan_ratio` | 1.0000 | 0.8665〜0.8670 |
| full_scan_2_5 | 200 | 7621/23000 | 0 | 0 | 200 | 0 | `plain_scan_ratio` | 1.0000 | 0.8515 |
| force_plain | 10 | 7621/23000 | 0 | 0 | 200 | 0 | `plain_scan_ratio` | 1.0000 | 0.8665〜0.8670 |
| force_plain | 200 | 7621/23000 | 0 | 0 | 200 | 0 | `plain_scan_ratio` | 1.0000 | 0.8515〜0.8516 |

判定規則に照らした結果:

- **A1**（`default` の到達方式）: `ann_masked` ではなく
  `mask_splits_graph` に 200 クエリ全件（k=10・200 いずれも）が到達する
  ことを確認した。Issue #658 の推測（`mask_splits_graph` 縮退が有力な
  要因）を**カウンタで確定**した——`subset_searches` は常に 0、
  `mask_splits_graph` が常に 200（非 vacuous）。
- **A2**（ACORN-1 opt-in の効果）: `acorn_4_10`・`acorn_1_1` いずれも
  `default` と**同じ** `mask_splits_graph` 縮退のまま（`subset_searches`・
  `acorn_searches` は常に 0）。両閾値とも regime 判定式
  （`visible_in_index * denom <= index_len * numerator`。`sql/
  hnsw_cache.rs::traversal_regime_for`）上は `TraversalRegime::TwoHop`
  に解決されるはずだが（`0.3313 <= 0.4` と `0.3313 <= 1.0` はいずれも
  真）、2-hop 中継を試みても連結性検査
  `is_mask_fully_reachable_with(mask, HopMode::TwoHop)` が依然として
  失敗し、plain scan へ縮退する。**この fixture・このグラフでは
  ACORN-1（`acorn_max_visible_ratio`。閾値を 4/10 まで・1/1〔常に
  TwoHop〕まで引き上げても）が `mask_splits_graph` を一切解消しない**
  ことを確認した（Issue #502 の「クラスタ構造ありコーパス・25,000 行
  規模では ACORN 4/10 でも分断が解消しない」という既存所見と整合する
  結果）。
- **A4**（Recall 非劣化）: `ann_masked`／`ann_masked_two_hop` に到達した
  arm は 0 件だった（上記 A2 のとおり全 hnsw arm が plain scan・
  `plain_scan_mask_split` または `plain_scan_ratio` へ縮退）ため、
  「filtered >= unfiltered - 0.02」の判定規則は本実測では一度も
  行使されなかった。全 arm の filtered recall は既定エンジンと厳密
  一致（1.0000）——ただしこれは ANN 探索の精度検証ではなく、plain scan
  縮退が既定エンジン（brute-force）と等価な経路を通ることの自明な
  帰結である点に注意（trivial pass。ANN 経路の Recall 非劣化検証には
  ならない）。

### wire A/B（A3）

`make bench-crossdb-self-hnsw-ab` 相当・self（wire 経由）exact/hnsw 交互 N=5 ペア。HEAD `a1e7b0e`。生データ: `docs/design/bench-data/crossdb-selectivity-659/`。

S1（`default`。`CROSSDB_SELF_HNSW_ARGS` 未指定）:

| phase | exact min/median (µs) | hnsw min/median (µs) | ratio min/median |
| --- | --- | --- | --- |
| vector_knn | 669.4 / 674.7 | 425.7 / 437.8 | 0.636 / 0.649 |
| vector_knn_where | 1270.0 / 1295.9 | 4083.7 / 4248.2 | **3.215 / 3.278** |
| hybrid_rrf | 6579.1 / 6690.9 | 5860.6 / 6180.0 | 0.891 / 0.924 |
| bulk_knn_k200 | 852.5 / 911.6 | 602.4 / 620.4 | 0.707 / 0.681 |
| bulk_knn_k1000 | 1648.0 / 1651.4 | 1364.8 / 1376.5 | 0.828 / 0.834 |
| bulk_knn_where_k200 | 2189.2 / 2209.3 | 5673.7 / 5719.9 | **2.592 / 2.589** |
| bulk_hybrid_k200 | 9123.7 / 9288.4 | 8377.7 / 8831.3 | 0.918 / 0.951 |

S3（`full_scan_2_5`。`--hnsw-full-scan-ratio 2/5`〔fixture 選択率
33.1% を上回る閾値で `PlainScanBelowRatio` 早期打ち切りを狙う〕）:

| phase | exact min/median (µs) | hnsw min/median (µs) | ratio min/median |
| --- | --- | --- | --- |
| vector_knn | 652.3 / 701.5 | 432.4 / 449.8 | 0.663 / 0.641 |
| vector_knn_where | 1265.5 / 1292.4 | 1991.8 / 2027.8 | **1.574 / 1.569** |
| hybrid_rrf | 6478.3 / 6554.3 | 5878.4 / 5987.8 | 0.907 / 0.914 |
| bulk_knn_k200 | 845.3 / 896.6 | 602.2 / 615.4 | 0.712 / 0.686 |
| bulk_knn_k1000 | 1614.5 / 1697.3 | 1380.3 / 1388.6 | 0.855 / 0.818 |
| bulk_knn_where_k200 | 2176.7 / 2251.3 | 3228.7 / 3264.8 | **1.483 / 1.450** |
| bulk_hybrid_k200 | 9094.1 / 9272.0 | 8644.3 / 8749.8 | 0.951 / 0.944 |

所見: `full_scan_ratio` を fixture 選択率（33.1%）より高い `2/5`
（40%）へ引き上げると、`vector_knn_where`（3.22x→1.57x）・
`bulk_knn_where_k200`（2.59x→1.48x）とも hnsw 側の劣後幅が明確に縮小
する——arm 表で確認した「`PlainScanBelowRatio` は `Overlay::compute`・
BFS 連結性検査を経由せず即座に plain scan する」設計（Issue #488）が
`mask_splits_graph` 経路（`Overlay::compute` を経由してから BFS で
失敗する、より高コストな plain scan 縮退）よりレイテンシ面で有利という
仮説を支持する参考値。ただし依然として exact 側（1.0x）には及ばず、
共有 QEMU 環境のため両ノイズ帯を超える確定判定はできない（S2＝
`acorn_4_10` の wire 実測は、in-process ハーネスで `default`／`acorn_1_1`
と同一の `mask_splits_graph` 縮退が確認できたため予測結果が自明であり、
時間予算の優先順位（S1 > S2 > S3。計画§3.3）に従い本 Issue では省略
した）。HEAD には Issue #664（非 HNSW の Subset 形状のみ id マスク経路化）
が含まれるため、Issue #658 時点の比率（2.19x／1.81x）とは前提条件が異なり
単純比較できない（exact 側だけ #664 で速くなった非対称が残っている
ため、hnsw 側の絶対値は #658 とほぼ同水準でも比率は悪化して見える）。

### 判断

- **既定値（`full_scan_ratio=1/10`・`acorn_max_visible_ratio=None`）は
  据え置く**。共有 QEMU 環境では専有環境実測（判定規則の条件 (ii)）を
  満たせないため Accepted にできない。
- 候補値の一次評価（決定的成果のみ。判定規則の条件 (i)）:
  - `full_scan_ratio` を `2/5`（fixture 選択率 33.1% 超）へ引き上げる
    候補は、Recall 非劣化（force_plain と同じ `plain_scan_ratio`
    縮退・既定エンジン厳密一致）を満たしたうえで、wire A/B（参考値）
    でも改善方向が一貫した。専有環境で条件 (ii) を満たせば Accepted に
    できる候補として最有力。
  - `acorn_max_visible_ratio`（`4/10`・`1/1` いずれも）は、regime 判定式
    上は TwoHop に解決されるにもかかわらず本 fixture の `mask_splits_graph`
    を一切解消しなかった（上記 A2）。決定的優位性が無いため、この
    fixture を根拠には推奨できない——`full_scan_ratio` 引き上げの方が
    本 fixture には効果があるという逆の結論になった（Issue #502 が
    候補として残した `4/10` は本 fixture では採用理由にならない）。
- **オーナーへの質問**: `full_scan_ratio` の既定値を `1/10` から `2/5`
  相当（本 fixture の選択率 33% 前後をカバーする水準）へ引き上げる
  方向性を専有環境実測（`BENCH_DEDICATED_ENV=1`・`make
  bench-crossdb-self-hnsw-ab CROSSDB_SELF_HNSW_ARGS="--hnsw-full-scan-ratio
  2/5"`）で検証してよいか。

## スコープ外・申し送り

- ~~不足時の `ef` 倍増再探索（iterative scan）・hybrid 密側の ANN 化と
  `complete_boundary_tie_group` の相互作用~~ Issue #410 で対応済み——DISTANCE
  経路の `ef` 倍増再探索は構造的に到達不能と判明したため追加せず、hybrid
  密側再取得ループの `fetch_k` 倍増自体が iterative scan として機能する形で
  結線した（`sql::hnsw_hybrid::HnswDenseProvider`）。詳細は
  `docs/design/hnsw-hybrid-iterative-scan.md` 参照
- ~~`EXPLAIN` へのエンジン種別・縮退有無の露出: #411~~ 実装済み（縮退有無は静的判定のみ・実行時縮退は非露出）。`docs/design/explain-search-engine-exposure.md` 参照
- ~~Recall 3 ゲートの ANN 同一閾値検証・TASK-121 系増分回帰: #412~~
  実装済み。`docs/design/ann-recall-gate-verification.md` 参照
- ~~`full_scan_ratio` 既定値（1/10）の実測による再調整・前後比較: #413~~
  比率×行数スイープを Issue #487 で実測（`S0_hot_where_subset` 対象クエリの
  per-run 生データ保持は満たすが、`docs/design/benchmark-judgement-
  policy.md` §3〜§4 を完全には満たさない——参照区間 `S0_hot_sql_e2e` の
  per-run 生データ未保存〔生ログ破棄により再構成不可〕、および計測コミット
  `06336219` 時点の実行順が §3 の輪番方式〔各 candidate の直前に baseline
  を個別に挟む〕とは異なる回転方式だった点を codex-review 指摘で確認・
  本文へ記録済み。詳細は「実行順（要訂正・codex-review 指摘）」「実測方式」
  各節参照）——本フィクスチャでは `ann_masked` 観測 arm に到達しなかった
  ため既定値の妥当性そのものは未確定のまま。「可視比率 × 行数の損益分岐点
  実測（Issue #487）」節参照。専有環境での再実測（§3〜§4 を完全に満たす
  形での参照区間 per-run 生データ保持・現行の輪番方式〔各 candidate の
  直前に baseline を個別に挟む〕での再測定を含む）・`ann_masked` を発火
  させる可視集合構成（クラスタ寄り）の設計は運用者・後続 Issue への申し送り
- `SearchTimeFilter` 経路の ANN 化（設計上対象外）
- `Subset` 形状で base 未構築時の非同期／バックグラウンド構築（現状は plain
  scan 縮退）
- `REBUILD_DELTA_RATIO`／`MIN_INDEXED_ROWS` の再設計（本 Issue は「再構築
  判定」と「探索方式判定」の分離までに留める）
- `IndexedBase` のキー表インターン化（`RowKey = (String, u64)` の per-query
  アロケーション回避）。`Subset` 経路は per-query に `Overlay::compute` を
  呼ぶため、フィルタ選択率が非常に低いクエリが多発する場合はテナント文字列の
  複製コストが積み上がりうる。性能のみに関わる最適化であり本 Issue の
  正当性・安全性には影響しないため、実測してから要否を判断する後続課題として
  申し送る
- `tests/rls_generalized.rs`／`tests/plan_rls_boost.rs` への HNSW エンジン
  variant 追加（TASK-138・TASK-139 の ANN 経路検証）。`tests/hnsw_cache.rs`
  の `r4_tenant_isolation_never_leaks_across_ctx`・新規 Rust API／`Subset`
  テストがテナント境界・非 vacuous 性の主要な検証を担っているが、既存 2
  ファイルへの HNSW variant 追加は本 Issue のスコープでは見送った
- spec 側: RLS 事前フィルタとの切替契約（非可視ノードの探索経路上の扱い・
  切替条件）の TASK／ビヘイビア ID 起票は引き続き ADR「spec 側への申し送り
  候補」のとおりオーナーへ報告
- ACORN-1（不適合 1-hop ノードのリンクのみを 2-hop 中継点として参照する
  方式）の実装（`search_layer_in`・`is_mask_fully_reachable` のレジーム
  対応・`ValidatedHnswParams` 拡張・統計追加）: #500 で契約整理・ゲート
  条件設計まで実施済み（「Issue #500」節参照）。実装は #501
- ~~ACORN-1 有効時の可視比率スイープ実測・既定値（`acorn_max_visible_ratio`）
  の確定: #502~~ 実測・回帰テストを実装済み（「Issue #502」節参照）。既定値
  は共有 QEMU 環境の制約により `None` のまま据え置き、候補値 `4/10` と
  専有環境での再実測・確定を運用者へ申し送り
- ~~crossdb fixture（選択率 33%）での `ann_masked` 到達実測・既定値判断:
  #659~~ 実測を実施済み（「Issue #659」節参照）。`default` は
  `mask_splits_graph` へ到達すること（Issue #658 の推測を確定）・
  ACORN-1（`acorn_max_visible_ratio=4/10`／`1/1` いずれも）は本 fixture の
  分断を解消しないこと・`full_scan_ratio=2/5` 候補は wire A/B 参考値で
  改善方向が一貫することを確認。既定値は共有 QEMU 環境の制約により据え置き、
  専有環境実測を後続 Issue へ申し送り
