# HNSW の RLS 事前フィルタ統合と可視カーディナリティ切替（Issue #409）

親 Issue #402（Phase 3・ANN opt-in 採用）。前提: #408（`sql::hnsw_cache::
HnswIndexCache`、`docs/design/hnsw-generation-cache.md`）。対象ビヘイビア
（ポインタのみ）: CORE-9・CORE-10・TASK-132・RLS-1〜4・RLS-8（TASK-138）・
TASK-139。ADR: `docs/design/ann-index-adoption.md`（Accepted・B 案）。本ドキュメント
が定める閾値・切替規則・マスク契約はいずれも**本リポの実装既定値（非規範）**
であり、`wire_code` の新設・`EXPLAIN` への露出は行わない（#411 の担当）。

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
安全条件を優先した結果。filter-aware な専用探索方式は #410 以降の検討課題）。

さらに codex-review P2 指摘対応（PR #435）で、`masked_short` の件数検査
（`min(k, visible_in_index)` 未満なら plain scan）だけでは検出できない
recall バグを是正した——マスクが複数の連結成分に分かれ、かつ探索起点側の
成分だけで結果件数を満たせてしまう場合、件数検査は通過するが、探索が
一度も訪れていない別成分により近い受理ノードを取りこぼす可能性がある。
`HnswIndex::search_masked` は、実探索が層 0 で使う起点から辿った先に、
[`HnswIndex::accepted_reachable_at_least`]（層 0 の隣接リストのみを辿る
BFS。非受理ノードを中継点に使わない実探索と同じ規約。ベクトルアクセス・
スコア計算は行わない）でマスクの受理ノード総数を覆えるかを検証し、覆え
なければ結果件数に関わらず空集合を返す（呼び出し元の `masked_short` 経由
plain scan 縮退へ委ねる）。連結性が保証できないマスクは常に安全側
（plain scan）へ倒す、という判断で、multi-entry-point 探索の実装は見送った
（filter-aware な専用探索方式の検討は引き続き #410 の担当）。

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

除外（従来どおり全件 brute-force）: hybrid の密側（#410 の担当）・`precision`
モード（TASK-162・SEARCH-9。ANN の近似近傍が確信度ゲートのマージン判定を
過大評価しうるため）。

`FullVisible` に `!plan.scalar_prefilter` を含めるのは、DISTANCE 先行・SCALAR
事後フィルタの recall モードでは `k_eff = bound.limit`（`sql/exec.rs`
の `k_eff` 導出）で DISTANCE 段を呼び、事後フィルタは戻り値を絞るだけ
（オーバーフェッチや全体ランキングは行わない）ため、そのアリーナは
フィルタなしと同一（可視全集合）であり `Overlay` の不変条件がそのまま成立
するため。

### 切替規則（本リポ実装既定値）

`Overlay::compute` は失効判定に加え、`visible_mask: NodeMask`（`slot_of_node[node]
!= STALE_SLOT` を表す）・`visible_in_index: usize`（= `index.len() - stale_nodes`）
を計算する。`search_with_overlay`（`sql/hnsw_cache.rs`）はこれを使って:

1. `visible_in_index / index.len() < full_scan_ratio`
   （整数比較 `visible_in_index * denominator < index.len() * numerator`。
   `u64` の `checked_mul`。オーバーフロー時は fail-closed に plain scan）
   → plain scan（アリーナ全体の brute-force。統計 `plain_scans`）
2. それ以外 → `HnswIndex::search_masked(query, k, ef, Some(&visible_mask), scratch)`
3. マスク付き探索の結果件数が `min(k, visible_in_index)` 未満（ビーム幅内で
   可視ノードを辿り切れなかった）→ 当該クエリのみ plain scan へ縮退（統計
   `masked_short`。**`ef` 拡張による再探索は #410 の担当**。本 Issue は
   fail-closed 縮退で k 件充足を保証する）
4. 索引ヒットのスロット写像・キー照合・`kernel::dot` 再計算・未索引分
   （`delta_slots`）の brute-force 併合は #408 と同じ

`k + stale_nodes` のオーバーフェッチ・`k_idx > MAX_EF` 縮退は撤去した
（`hnsw-generation-cache.md`「既知の限界」の申し送りを閉じる）。
`MIN_INDEXED_ROWS`／`REBUILD_DELTA_RATIO` は本 Issue では据え置く（「再構築
判定」と「探索方式判定」を分離した、という位置づけで記録する）。

`HnswParams::full_scan_ratio`（`Ratio { numerator, denominator }`。既定
`1/10`）は `f32` が `Copy + Eq` を満たさず `HnswParams` の derive と両立しない
ため整数比で表現した（`sql/hnsw_cache.rs::REBUILD_DELTA_RATIO` と同型）。
`HnswParams::validate` が `denominator >= 1`・`numerator <= denominator` を
fail-closed に検査する。

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
| `crates/engine/src/hnsw.rs` | `Ratio`・`HnswParams::full_scan_ratio`・`validate` 拡張。`NodeMask`。`search_layer` の受理述語。`HnswIndex::search_masked`（`search` はこれへ委譲） |
| `crates/engine/src/search_engine.rs` | `Display` に `full_scan_ratio` を追記 |
| `crates/engine/src/sql/hnsw_cache.rs` | `Overlay::visible_mask`／`visible_in_index`。`search_with_overlay` を可視カーディナリティ切替へ書き換え（`k + stale` 撤去）。`search_subset_or_fallback`（新設）。統計 `plain_scans`・`masked_short`・`subset_searches` |
| `crates/engine/src/sql/exec.rs` | `hnsw_full_visible_eligible`／`hnsw_subset_eligible` の 2 条件・DISTANCE 段の形状別ディスパッチ |
| `crates/engine/src/rls.rs` | `PrefilterSnapshot::search_with_hnsw` |
| `crates/engine/src/core.rs` | `search_with_snapshot` の `hnsw_state` 分岐 |
| `crates/engine/tests/hnsw_cache.rs` | Rust API テストの契約反転・`Subset` 形状の新規テスト |

依存追加なし（`std` のみ）。`unsafe` 不使用。

## セキュリティ考慮（OWASP Top 10・security.md P0）

| 観点 | 対応 |
| ---- | ---- |
| アクセス制御の不備／テナント境界（P0） | 索引は ctx 可視アリーナのみから構築（不変）。マスクは同一 ctx 内の候補差分のみを表し、他テナント行はグラフに存在しない。索引ヒットは現世代スロットへの写像・`(tenant_id, id)` キー照合・スコア再計算を経て返し、`provider_result_is_valid`・`RlsSafetyNet` の多層防御を維持。`PolicyContext::is_visible` に新規比較ロジックを追加していない |
| 存在情報の副次漏えい | 切替判定・マスク密度・統計はいずれも ctx 自身の可視集合と自身の索引のみから導出され、他テナント行数に依存しない。`EXPLAIN` へは露出しない（#411 の担当） |
| fail-closed のエラー契約 | HNSW 固有エラー・マスク長不一致・写像検証失敗・結果不足はいずれも当該クエリの brute-force 縮退へ吸収し呼び出し元へ伝播させない。不正 `full_scan_ratio` は `hnsw_kind`（唯一の検証入口）で拒否する |
| インジェクション | SQL 文字列を組み立てる箇所なし |
| 不安全な設計／DoS | `NodeMask` は索引ノード数分のビット（最大 `MAX_HNSW_NODES / 8` バイト程度）。整数演算は `checked_*`／`saturating_*`。`ef`・`k` の上限は既存どおり `MAX_EF` |
| 脆弱な依存 | 依存追加なし。`unsafe` なし |

## 検証

- 単体（`hnsw.rs` in-module）: `search_masked(None)` が `search` とビット同一・
  結果がマスクの部分集合・マスク長不一致の拒否・全 false マスクで空
- 単体（`sql/hnsw_cache.rs` in-module）: 既存テストを `Overlay` の新規
  フィールドへ対応させたうえで全 green（切替・写像検証の既存契約は不変）
- 結合（`crates/engine/tests/hnsw_cache.rs`）: `Subset` 形状の Recall@10 ≥ 0.9・
  WHERE を満たさない行の非混入・`subset_searches > 0`（非 vacuous）・キャッシュ
  非登録（`stats.entries` がフィルタなしクエリのベースラインから変化しない）。
  Rust API 結線の Recall@10 ≥ 0.9・可視外テナント非混入・非 vacuous
- `make core-api-check`（`SearchProvider`/`VectorCore` trait 差分ゼロ）・
  `make sort-determinism-check` green

## スコープ外・申し送り

- 不足時の `ef` 倍増再探索（iterative scan）・hybrid 密側の ANN 化と
  `complete_boundary_tie_group` の相互作用: #410
- `EXPLAIN` へのエンジン種別・縮退有無の露出: #411
- Recall 3 ゲートの ANN 同一閾値検証・TASK-121 系増分回帰: #412
- `full_scan_ratio` 既定値（1/10）の実測による再調整・前後比較: #413
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
