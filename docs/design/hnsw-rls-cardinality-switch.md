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
3. それ以外 → `HnswIndex::search_masked(query, k, ef, Some(&visible_mask), scratch)`
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
  として、各 candidate の直前に必ず 1 回実行する（輪番:
  baseline→hnsw_default→baseline→hnsw_force_ann→baseline→hnsw_force_plain。
  `docs/design/benchmark-judgement-policy.md` §3 の「3 候補以上の場合は
  baseline→cand1→baseline→cand2→…」に対応。`scripts/
  bench_knn_visible_ratio_sweep.sh` は scale×ratio の組み合わせごとに
  pair を外側・candidate を内側のループとし、ペアごとにこの輪番を
  繰り返す〔特定の candidate だけを SWEEP_PAIRS 回連続実行してしまう
  時間方向の交絡を避けるため〕）
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
- **実行環境**: 本開発環境（共有 QEMU 環境。`docs/design/
  benchmark-judgement-policy.md` §5 の証拠力区分では「参考値」）。専有環境
  （`BENCH_DEDICATED_ENV=1`）での再実測は未実施——**本節の数値は
  `full_scan_ratio` 既定値の変更根拠にはしない**（計測プロトコル自体は
  §3 の必須事項〔交互 N≥5 ペア・輪番・per-run 生データ保持〕を満たすが、
  共有 QEMU 環境という証拠力区分の制約は別軸として残る）
- **実測方式**: `make bench-knn-visible-ratio`（`SWEEP_PAIRS=5`。既定値・
  `docs/design/benchmark-judgement-policy.md` §3 の N≥5 必須要件を満たす
  下限）で 3 candidate × 5 ratio × 2 scale の全組み合わせを baseline→
  candidate の輪番で N=5 ペアずつ実行した（計測コミット
  `06336219`）。生ログは `target/bench-knn-visible-ratio/1788722580/`
  （ローカル・gitignore 対象のため、事後の再判定に必要な per-run 生データは
  下記の各表・折りたたみ内へインライン記録する。恒久的な参照先はこの
  ドキュメント本文そのもの）

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

- 本開発環境（共有 QEMU）の実測は、計測プロトコル自体は
  `docs/design/benchmark-judgement-policy.md` §3〜§4 の必須事項（交互
  N=5 ペア・輪番・per-run 生データ保持・主統計量 min-of-N＋median 交差確認・
  両ノイズ帯併記〔全幅〕）を満たすが、§5 の証拠力区分では引き続き
  **参考値**。`full_scan_ratio` 既定値（1/10）の変更根拠にはしない
- 本フィクスチャ（均等分散マスク・一様乱数ベクトル）では `ann_masked`
  観測 arm に一度も到達しなかったため、「損益分岐点」自体を本節の実測から
  結論づけることはできない。後続実測はクラスタ寄りの可視集合構成
  （テナント単位・連続 id 範囲等）で `ann_masked` を実際に発火させたうえで
  性能比較する設計に改める必要がある
- 専有環境での再実測（計測プロトコル自体は本節で N=5 済みのため、専有
  環境での実行のみが残タスク）、および `ann_masked` を発火させる可視集合
  構成の設計は運用者・後続 Issue への申し送りとする（下記「スコープ外・
  申し送り」参照）

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
  比率×行数スイープを Issue #487 で実測（計測プロトコル自体は交互 N=5 ペア・
  輪番・per-run 生データ保持を満たす）——本フィクスチャでは `ann_masked`
  観測 arm に到達しなかったため既定値の妥当性そのものは未確定のまま。
  「可視比率 × 行数の損益分岐点実測（Issue #487）」節参照。専有環境での
  再実測・`ann_masked` を発火させる可視集合構成（クラスタ寄り）の設計は
  運用者・後続 Issue への申し送り
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
