# ANN 索引（HNSW/IVF）採否の設計検討

- **ステータス**: Accepted（2026-09-02 オーナー承認〔Issue #402〕。B 案
  〔条件付き opt-in 採用〕・自作 HNSW・依存追加なし。判断記録・実装ガイドは
  本書「判断記録」「実装ガイド（B 案）」節を参照。規範的な契約（決定性・
  RLS・Recall 等）の SSOT は private spec 側であり、本書は spec 起票までの
  作業メモに留まる。実装は Phase 3 タスク #404〜#413 で行う）
- **対応 Issue**: #367（判断材料整理）・#403（判断記録・実装ガイドの追記）・
  #402（Phase 3 親トラッキング）・#385（feature_bench 再測定トラッキング）
- **関連ポインタ（spec・本文は転記しない）**: CORE-9・CORE-10・CORE-12・CORE-13・
  SEARCH-1・SEARCH-3・SEARCH-7・
  `docs/spec/05-tasks.md` TASK-120・TASK-121・TASK-131・TASK-132・
  `docs/spec/06-roadmap.md` MS-2・
  `docs/spec/01-brainstorm.md`・
  `docs/spec/03-poc/real-scale-recall-reeval/README.md`（実データ規模での Recall 再評価）
- **関連ドキュメント**: `crates/engine/docs/ann-future-work.md`・
  `docs/design/core5-contrast-engine.md`・`docs/design/hybrid-recall-regression.md`・
  `docs/design/hybrid-refetch-latency.md`・`docs/design/rrf-tie-break-determinism.md`・
  `docs/design/c1-p95-dedicated-env-reverification.md`・
  `docs/design/precision-confidence-gate.md`・
  `docs/design/knn-stage-profile.md`・`docs/design/knn-two-stage-topk.md`・
  `docs/design/dot-kernel-multi-accumulator.md`・
  `docs/design/sql-arena-generation-cache.md`・`docs/design/sparse-index-cache.md`・
  `docs/design/hnsw-graph-construction.md`・`docs/design/hnsw-search.md`

## 目的・位置づけ

本ドキュメントは、ANN 索引（HNSW・IVF 系）を本リポの検索エンジンへ採用するか
どうかの**判断材料を整理する**ことを目的とする。採否そのものはオーナー判断
であり、本書はその判断に必要な比較・閾値・コストを提示するに留める。

`crates/engine/docs/ann-future-work.md`（TASK-132・CORE-10）は「ANN 併用時の
RLS 再評価の観点」の列挙に留まり、採否判断そのものに必要な方式比較・規模
閾値・依存コストの整理は行っていない。本書はその欠落を埋める位置づけであり、
同メモの内容を置き換えるものではない。

Issue #402 でオーナーが B 案（条件付き opt-in 採用）を採用したため、本書は
判断材料に加えて Phase 3 実装（#404〜#413）の**設計記録・実装ガイド**を
記す。ビヘイビア・エラー契約（`wire_code`）・決定性契約・RLS 契約・Recall
受け入れ基準といった**規範的な仕様の SSOT は private spec
（`docs/spec`）側**にあり（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)・
AGENTS.md「spec（SSOT）との整合」）、本書はそれらを本リポジトリ側で新規に
確定させるものではない。以下「実装ガイド（B 案）」節は、Phase 3 実装
（#404〜#413）が参照する**本リポジトリ実装上の初期値・整理方針**（実装・
実測に応じて改訂しうる非規範的な作業メモ）であり、対応する規範的な契約
（決定性・RLS・Recall の各契約）を spec 側へ起票・確定させることは
「spec 側への申し送り候補」節の申し送り事項とする。以下の
「ANN 方式の概観と比較」〜「選択肢比較と推奨」の判断材料本文（比較表・
仮説・推奨 A の記述）は判断時点の記録として改変せず残す。判断結果・実装
ガイドは「判断記録」節以降に追記した。

## 現状の前提（コード事実）

- 既定の検索方式は厳密最近傍（総当たり）。`search_engine.rs::SearchEngineKind`
  （`#[non_exhaustive]`）は現状 `CpuScalarBruteForce`／`ParallelBruteForce` の
  2 variant のみを持ち、`kernel.rs::SearchProvider` trait による provider 注入
  機構へ一本化されている。エンジン選択はコード上の明示指定のみで、環境変数
  による実行時上書きは設けない方針（CORE-12 踏襲）。将来の ANN 実装は
  `SearchEngineKind` へ variant を追加し `build` に分岐を 1 つ加えるだけで
  組み込める設計になっている。
- RLS（テナント境界）は `rls.rs::PrefilterIndex`（構築時に `PolicyContext` を
  束縛し、可視行のみの縮約ビュー＝`VectorArena::build_filtered` を作る）と
  `SearchTimeFilter`（ポリシーが動的な場合のフォールバックで縮約ビューを保持
  しないストリーミング判定）の 2 方式を提供する。いずれも可視性判定は
  `PolicyContext::is_visible` の単一照合パスへ委譲するだけで独自の比較ロジック
  を持たない。`core.rs::PrefilterCache` が `(table, ctx)` 単位でスナップショット
  を世代整合つきでキャッシュする。
- ハイブリッド検索の候補プールは既定 `pool_depth=200`・`k_const=60`（RRF の
  ランク減衰定数）・等重み。`MAX_POOL_DEPTH=10_000`、境界同点グループ完全化の
  再取得上限 `MAX_FETCH_K = MAX_POOL_DEPTH * 4`、同点規約は `TieRank::GroupEnd`
  （既定）。密チャネルは総当たりを前提に「候補プール境界の同点グループを
  完全に取り切る」ことを決定性契約として保証している（`hybrid.rs`・
  `docs/design/rrf-tie-break-determinism.md`）。
- Recall 回帰ゲートの規模は hybrid（小規模／大規模）・query-planning（中規模／
  大規模）・rerank（大規模）のフィクスチャで段階的に用意されており、層 B は
  `recall.yml`（environment `recall-gate`）で独立評価・AND 集約される。
- ベンチマーク（CORE-3/4/5・C1）は行数 10 万・次元 768・top-k 20 の規模で
  測定しており、CORE-5 の対照エンジンは usearch の総当たり `exact_search`
  （`contrast-bench` feature 限定の optional 依存・Apache-2.0・オーナー承認
  済み〔2026-08-26〕）。usearch 本体は HNSW 実装だが、この承認は「総当たり
  対照としての利用」に限定されたものであり、ANN 方式としての採用可否を評価
  したものではない。pure Rust 対照ベンチ（`hnsw_rs`）の実測は Issue #406 追記で完了・撤去済み（2026-09-05。詳細は `docs/design/hnsw-parallel-build.md` 参照）。production ANN 採用可否の評価は依然未実施。
- 公開済みの実測値として、大規模段（20,000 件）ハイブリッド経路の p95 が
  通常コーパスで約 5.6 ms であることや、C1（100,000 行×768 次元）p95 が
  専有環境で閾値僅差の fail 状態にあることが `docs/design` に記録されている
  （個別の閾値そのものは非公開運用）。

## ANN 方式の概観と比較（公開知識の範囲）

以下は HNSW・IVF について広く公開されている一般的な設計方針の要約であり、
特定実装の内部詳細への行番号付き言及は行わない。

| 観点 | HNSW（グラフベース） | IVF（＋PQ、クラスタベース） |
| ---- | -------------------- | ---------------------------- |
| 構築コスト | 挿入ごとにグラフへ接続を張る。おおよそ N に対して準線形〜線形 | 重心（クラスタ中心）を k-means 等で学習してから転置リストへ割当 |
| 探索コスト | 対数オーダー近傍を志向（`ef` パラメータで recall と速度をトレードオフ） | 分割した nlist のうち nprobe 個のみ走査（探索範囲を絞るほど recall 低下） |
| recall 調整パラメータ | `ef`（探索時）・`M`（構築時の接続数） | `nlist`・`nprobe`・（PQ 併用時は量子化ビット数） |
| 増分挿入 | 挿入は比較的自然だが削除は tombstone 前提でグラフ全体の再構築なしに完全除去しづらい | 重心が古いデータ分布に基づくため、分布が変わると再学習（リビルド）が必要になりやすい |
| メモリ | グラフ構造（隣接リスト）分だけ追加消費 | PQ 併用時は元ベクトルを量子化し圧縮できるが非量子化なら追加消費は小さい |
| 決定性 | 探索順序がグラフ構造・`ef` に依存し、同点順位の再現性は総当たりより崩れやすい | クラスタ割当が決定的なら候補集合は再現しやすいが、境界ケース（複数クラスタにまたがる同点）は総当たりと異なりうる |

本リポの増分反映（TASK-120。ファイル単位 `INSERT` → チャンク化 → 埋め込み →
同一パス置換書き込み）は、ANN 索引に対しては「置換対象パスの旧チャンクを
インデックスから除去し、新チャンクを挿入する」操作に相当する。HNSW は
tombstone を伴う除去、IVF は重心の陳腐化に伴う再学習が課題になりやすく、
いずれの方式でも増分反映の回帰テスト（TASK-121 系）の再設計が必要になる。

## recall 思想との緊張関係

本リポの設計思想は「正解を含むデータ群を広く返す」こと、すなわち候補プール
（既定 `pool_depth=200`）の再現率（recall）を優先する厳密最近傍検索である。
ANN は本質的に recall と探索コストのトレードオフを導入する方式であり、
密チャネルでの recall 低下は RRF 融合 → rerank → `precision` モードの確信度
ゲート（`docs/design/precision-confidence-gate.md`）まで下流へ伝播しうる。

このため、ANN を採用する場合は **SEARCH 系 Recall ゲート（hybrid 小規模・
大規模、rerank 非劣化、query-planning 大規模）を ANN 有効時も同一閾値で
通過すること**を前提条件として定義する。

また、境界同点グループの完全化（`docs/design/rrf-tie-break-determinism.md`
が定める決定性契約）は、密チャネルが総当たりであることを前提に成立して
いる。ANN 経路では近傍探索が近似であるため、この決定性契約を原理的に
保証できない（探索を打ち切った時点で「本当は同点だが探索されなかった候補」
が生じうる）。採用する場合はこの契約をどう扱うか（ANN 経路では緩めるか、
`ef`/`nprobe` を十分大きく取ることで実務上無視できる差に留めるか）を
別途決定する必要がある。

## RLS／フィルタとの相互作用と折衷案

フィルタ（RLS の可視行制約）と ANN を組み合わせる方式は一般に 3 通りが
知られている。

1. **事前フィルタ**: 可視行のみの縮約集合上に ANN 索引を作ってから検索する。
   本リポの `PrefilterIndex` はこの方式に相当する構造を既に持つが、
   `(table, ctx)` ごとに索引構築コストが発生するため、可視率・テナント数に
   比例してコストが増える。ANN 索引の構築自体が総当たりより重いため、
   クエリのたびに構築するのは非現実的であり、キャッシュ戦略
   （`PrefilterCache` 相当）の再設計が必要になる。
2. **事後フィルタ（不採用）**: フィルタなしで ANN 探索した結果を可視性で
   絞り込む。可視率が低いテナントでは十分な件数を得るために oversampling
   （余分に多く探索してから絞る）が必要になり、探索段階で不可視行の存在が
   候補集合・処理量・応答時間に影響するため、`PolicyContext::is_visible`
   による検索結果への再照合（後述）では解消されない存在情報の副次漏えい
   経路になる。これは AGENTS.md の定めるテナント境界（RLS 相当）弱体化
   禁止（P0）に抵触するため、**本リポでは不採用とする**。仮に将来
   採用を検討する場合も、副次チャネルが観測不能であることを立証できる
   具体的な設計・検証基準の提示を前提条件とする。
3. **filter-aware 探索**: フィルタを考慮しながらグラフ探索・クラスタ選択を
   行う方式（一部の ANN 実装・ベクトル DB が採用する一般的な設計方針）。

折衷案として、**可視行カーディナリティ推定に応じて plain scan（総当たり）
と ANN を切り替える方式**（Qdrant が採用しているとされる一般的な設計方針）
が知られている。本リポは `PrefilterIndex` の構築時点で可視行数が既知である
ため、この推定値を使って `SearchEngineKind` の分岐（明示指定・fail-closed）
として実装できる余地がある。可視行数が閾値未満なら総当たり、閾値以上かつ
索引が利用可能なら ANN、という分岐が候補になる。

**採用しうる方式（事前フィルタ・filter-aware 探索）を採る場合も、結果は
`PolicyContext::is_visible` の単一照合パスを通す fail-closed 契約を維持
する。** ANN 経路で可視性判定を省略・緩和する設計（例: 索引構築時の可視性
判定のみに依存し検索時の再判定を省く）は不採用とする。

## 対象規模の閾値（損益分岐の考え方）

総当たり探索のコストは行数 N と次元 D の積にほぼ線形に増加する。ANN は
索引構築コストと引き換えに探索コストを対数〜nlist に対する分数オーダーへ
下げる方式であり、N が小さいうちは構築コストが探索コスト削減分を上回り
損益分岐しない。

本リポの性能ゲート・ベンチは現状 N=100,000（次元 768）を基準規模としており、
公開済みの実測（20,000 件ハイブリッド経路 p95・100,000×768 の C1 pass/fail
状況）を踏まえると、**「10 万件までは厳密検索で受け入れ基準の射程内、ANN の
損益分岐は数十万件以上」という仮説**を提示する。ただし、この数値は本リポの
ベンチ規模から類推した仮説であり、断定はできない。

確定には、`contrast_bench.rs`（CORE-5 の対照経路）に usearch の HNSW 探索
（`Index::search`）を対照として追加し、以下の実測項目で A/B 比較を行う必要
がある（実装は本 Issue のスコープ外・別 Issue 候補）。

- 行数 N ∈ {100,000, 500,000, 1,000,000} の 3 点
- 複数の可視率帯（事前フィルタ方式での縮約後行数を変えて測定）
- recall@200（候補プール既定深度に対する再現率。厳密探索との一致率）
- 索引構築時間・メモリ使用量

## 依存追加 vs 自作のコスト比較

| 選択肢 | 概要 | 主な懸念 |
| ------ | ---- | -------- |
| usearch（既存 optional 依存の HNSW） | C++ FFI・Apache-2.0。`contrast-bench` feature で bench 限定利用が承認済み〔2026-08-26〕 | production 採用は `wire-server` が C++ をリンクすることを意味し、`make check-cross`（aarch64 `cargo check`）・`deny.toml`・依存最小方針との整合を再確認する必要がある。bench 限定の既存承認は流用できず、**新規のオーナー承認が必要** |
| pure Rust クレート（`hnsw_rs`・`instant-distance` 等） | FFI 不要でクロスコンパイル影響が小さい | ライセンス・メンテナンス状況・推移的依存の規模を個別に再調査する必要がある（現時点で本リポは評価していない） |
| 自作 HNSW | 依存追加なし・実装を完全に制御できる | 実装・検証コストが最大。決定性契約・RLS 統合・削除（tombstone）処理を自前で設計する必要があり、スケジュール・品質リスクが大きい |
| 自作 IVF（k-means＋転置リスト） | HNSW より実装は軽い | 重心の再学習タイミング・増分挿入時の偏り（新規データが既存クラスタに合わない場合の recall 劣化）への対策が必要 |

いずれの選択肢も、採用時は `=x.y.z` 完全固定・ユーザー承認制
（[dependency-policy](../../.claude/rules/dependency-policy.md)）に従う。

## 選択肢比較と推奨

- **A. 見送り（現状維持）**: 厳密最近傍を維持する。現行のベンチ規模
  （10 万件）・Recall ゲート・decision 契約（境界同点グループ完全化）との
  整合が最も高く、実装・検証コストがゼロ。
- **B. 条件付き採用**: 以下をすべて満たす場合に限り、`SearchEngineKind` の
  opt-in variant として追加する。既定エンジンは変更しない。
  1. 上記「対象規模の閾値」の A/B 実測で損益分岐点が実測により確認され、
     かつ想定運用規模がそれを超える
  2. 可視行カーディナリティ推定による plain scan / ANN 切替（折衷案）が
     実装され、fail-closed（不確実なら厳密検索へ倒す）が維持される
  3. SEARCH 系 Recall ゲートを ANN 有効経路でも同一閾値で通過する
  4. 依存追加についてオーナー承認を得る
- **C. 全面採用（既定エンジンを ANN へ切り替える）**: 不推奨。recall 思想
  ・決定性契約との整合コストが最も高く、A・B で判断材料が揃ってから再検討
  すべき。

**推奨は A を既定とする。**（判断結果）Issue #402 でオーナーは B を採用した。
推奨 A からの転換理由は「判断記録」節を参照。

## 採用時の前提条件・受け入れ基準案（B を選ぶ場合）

- 既定エンジンは不変（`ParallelBruteForce` のまま）とし、ANN は明示選択のみ
- `EXPLAIN` へ使用エンジン種別を露出し、どの経路で結果が得られたか追跡可能にする
- 決定性契約（同点規約・境界同点グループ完全化 or その代替契約）を明文化する
- `PolicyContext::is_visible` への単一照合パス・fail-closed を維持する
- SEARCH 系 Recall ゲートを ANN 有効経路でも同一閾値で計測・通過する
- 増分インデックス反映の回帰テスト（TASK-121 系）を ANN 索引の増分更新
  （tombstone・重心再学習等）に対応させて拡張する

## 判断記録（オーナー記入欄）

| 項目 | 内容 |
| ---- | ---- |
| 判断（採用／見送り／条件付き採用） | 条件付き採用（B 案）。既定エンジンは `ParallelBruteForce` のまま不変とし、`SearchEngineKind` へ opt-in variant を追加する |
| 根拠 | (1) main 時点の `feature_bench` 再測定（Issue #385）で `vector_knn`／`mode_recall`／`mode_precision` が総当たりのまま規模線形（p50 数 ms オーダー）に残存することを実測確認した（実測事実。詳細は `knn-stage-profile.md`・`knn-two-stage-topk.md`・`dot-kernel-multi-accumulator.md`）。(2) 依存最小方針（[dependency-policy](../../.claude/rules/dependency-policy.md)）との整合のため方式は自作 HNSW（pure Rust・`unsafe` 不使用・依存追加なし）を選択する。(3) `usearch` を production 採用しない判断・pure Rust ANN クレート個別評価の要否を検討したが、その詳細な比較検討・却下理由は本ドキュメントの公開境界（README「実装方針の要点」）を超える非公開の内部設計判断にあたるため本書には転記しない（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）。判断の要点（自作 HNSW を選ぶこと・production では依存追加を行わないこと）のみを記録し、詳細な比較根拠は spec 側（TASK-132・CORE-10）へ起票のうえ整理する（「spec 側への申し送り候補」節） |
| 条件（条件付き採用の場合） | 「採用時の前提条件・受け入れ基準案」の 4 条件の充足方針: 条件 1（損益分岐点の A/B 事前実測）は事前実測を行わず、Issue #385 の再測定（規模線形残存）を着手根拠とし、Phase 3 完了時の前後比較（#413）で事後確認する運用へ差し替える（オーナー判断による逸脱）。条件 2（可視カーディナリティ推定による plain scan / ANN 切替・fail-closed）は #409／#410 で実装する。条件 3（SEARCH 系 Recall ゲートの同一閾値通過）は #412 で検証する。条件 4（依存追加のオーナー承認）は自作のため不要 |
| 判断日 | 2026-09-02（Issue #385／#402 の議論を経てオーナーが承認。本 ADR への転記は Issue #403） |
| 記入者 | Issue #403 の実装担当（自動運転の実装 Agent）がオーナー承認内容を本節へ転記した |

本コミットで、本節の記入とあわせてステータスを `Accepted` へ更新した
（`docs/design/precision-confidence-gate.md` の運用と同様の流儀）。

## 実装ガイド（B 案・Phase 3。規範的契約は spec 側 SSOT へのポインタに限る）

判断記録節のとおり、B 案（条件付き opt-in・自作 HNSW・依存追加なし）が
承認された。Phase 3（#404〜#413）が実装する挙動のうち、外部から観測可能な
結果・契約（不正パラメータ時の `wire_code`・決定性契約・RLS 事前フィルタ
との切替契約・増分更新〔挿入・削除双方〕時の検索結果契約）は、いずれも
**private spec 側で TASK・ビヘイビア ID を確定させることが前提**であり
（AGENTS.md「spec（SSOT）との整合」）、本書はそれらの詳細設計を本リポジトリ
側で先行して確定・記載しない。対応する Phase 3 タスク（#404〜#412）は、
「spec 側への申し送り候補」節の確定を待って着手する。着手前に spec 側で
確定すべき事項の対応表は次のとおり（詳細設計は private spec 側の該当
ビヘイビア定義に記す。本書には TASK・ビヘイビア ID のポインタのみを置く）:

| 実装項目 | 対応 Issue | spec 側で確定させる契約 |
| -------- | ---------- | ------------------------ |
| opt-in 方法・不正パラメータ拒否 | #407（実装済み。`docs/design/hnsw-search-engine-wiring.md`） | エンジン選択方式・`22023` 相当のエラー契約（`ErrorClass` への正式登録は spec 側確定後の別タスクへ申し送り） |
| 既定パラメータ・並列構築 | #404〜#406 | （非契約的な実装詳細。既存の依存最小方針・`unsafe` 原則禁止の範囲で Phase 3 実装時に確定） |
| 決定性 | #405・#410・#411 | 同点規約・境界完全化の要否 |
| RLS 事前フィルタとの切替 | #409（可視カーディナリティ切替・`FullVisible`/`Subset` 形状・Rust API 結線を実装済み。`docs/design/hnsw-rls-cardinality-switch.md`）・#410（境界再取得・hybrid 密側の残課題） | 非可視ノードの探索経路上の扱い・切替条件 |
| 増分更新（挿入・削除） | #408（実装済み。`docs/design/hnsw-generation-cache.md`）・#412（実装済み。`docs/design/ann-recall-gate-verification.md`「TASK-121 系」節） | 挿入 delta・削除時の索引再構築要否を含む検索結果契約 |
| Recall 受け入れ・継続的ゲート接続 | #412（実装済み。`docs/design/ann-recall-gate-verification.md`） | 既存 SEARCH 系ゲートとの閾値同一性・接続方式 |
| 永続化 | 初期スコープ外 | 「判断確定後のスコープ外」節参照 |

既定パラメータ・並列構築方式（`std::thread` ベース・`unsafe` 不使用）は
本リポジトリの既存方針（依存最小方針・`unsafe` 原則禁止）の範囲に収まる
非契約的な実装詳細であり、spec 確定を待たず Phase 3 実装時に確定してよい。
RLS 事前フィルタとの切替（4 列目）については、「事後フィルタ不採用」
「非可視ノードを探索経路として通過させる設計は不採用」という P0 安全側の
既存判断（本 ADR「RLS／フィルタとの相互作用と折衷案」節）のみを確定事項
として維持し、具体的な切替閾値・専用グラフ構築方式等の詳細設計は spec
確定後に定める。

## 受け入れ基準と Phase 3 タスクの対応

「採用時の前提条件・受け入れ基準案（B を選ぶ場合）」節の各項目と Phase 3
タスクの対応は以下のとおり。

| 受け入れ基準 | 対応タスク |
| ---- | ---- |
| 既定エンジン不変・ANN は明示選択のみ | #407（実装済み。`22023` の `ErrorClass` 正式登録は spec 側 TASK・ビヘイビア ID 確定後に着手） |
| `EXPLAIN` へエンジン種別露出 | #411（実装済み。`docs/design/explain-search-engine-exposure.md` 参照） |
| 決定性契約（同点規約・境界完全化 or 代替契約）の明文化 | #403（本書）・#405・#410（spec 側 TASK・ビヘイビア ID 確定後に着手） |
| `PolicyContext::is_visible` 単一照合パス・fail-closed の維持 | #409・#410（RLS 切替契約は spec 側 TASK・ビヘイビア ID 確定後に着手） |
| SEARCH 系 Recall ゲートを ANN 有効経路でも同一閾値で通過 | #412（実装済み。`recall.yml` への機械判定可能な接続を含む。「実装ガイド」節の対応表「Recall 受け入れ・継続的ゲート接続」行参照。詳細・実測は `docs/design/ann-recall-gate-verification.md` 参照） |
| TASK-121 系増分回帰の ANN 対応 | #408・#412（実装済み。`docs/design/ann-recall-gate-verification.md`「TASK-121 系」節） |
| （B 案条件 1）損益分岐点の実測 | #413（実施済み。`docs/design/hnsw-index.md` 参照。フィルタの選択性が損益分岐の主要因で、行数規模〔25k〜100k〕は副次的という所見。専有環境・広い規模ラダーでの再測定は同 doc「スコープ外・申し送り」節へ申し送り） |
| 基盤（HNSW 構築・探索・並列化） | #404・#405・#406 |

## 参照した外部実装（手法名・ライセンスのみ・コード転記なし）

自作 HNSW の設計にあたり、以下の手法・実装を参照した。ソースコード・
コメントの転記は行わず、公開されている手法名・設計方針・ライセンスのみを
記す。

- **HNSW**（Malkov & Yashunin の論文）: 層構造・`ef` 探索・近傍選択
  ヒューリスティックの基本設計
- **qdrant**（Apache-2.0）: HNSW の既定パラメータ・`full_scan_threshold` に
  よる plain scan 切替・初期点の単一スレッド構築という設計方針
- **pgvector**（PostgreSQL License）: 並列ビルドのロック粒度・
  `iterative_scan` 型の境界再取得という設計方針
- **usearch**（Apache-2.0）: 既定パラメータの参考・CORE-5 の bench 対照
  エンジンとしての既存利用（production は非採用のまま）
- **Lance**（Apache-2.0）: 未索引分 brute-force 併用＋再構築という設計方針

各実装の既定値等を数値として引用する場合は実装時に上流ドキュメントで
確認し、確認できない値は本リポの採用値としてのみ記載する（出典の数値を
誤って帰属させない）。

## 判断確定後のスコープ外（申し送り）

- HNSW 索引の永続化（「実装ガイド」の対応表「永続化」行参照）
- テーブル単位カタログ属性による opt-in・`wire-server` CLI オプション露出
- pure Rust ANN クレート（`hnsw_rs`・`instant-distance` 等）の個別評価は Issue #406 追記で実測・撤去済み（2026-09-05。詳細は `docs/design/hnsw-parallel-build.md` 参照）。production 採用可否の評価は依然未実施
- README「実装方針（要点）」の opt-in 手順・公開境界拡張（#413 が担当。実施済み。README「ANN（HNSW）opt-in 手順と前後比較（Issue #413）」節参照）
- CLAUDE.md の `- ステータス:` 行が 2 本重複し内容が一部乖離している件の
  統合（本 Issue のスコープ外。両行を同一内容へ更新するに留める）

## spec 側への申し送り候補（ポインタのみ）

- **Phase 3 着手前に必須（「実装ガイド」節の対応表に掲げたブロック対象。
  #407・#405・#410・#411・#409・#408・#412 は下記確定まで着手しない）**:
  「実装ガイド」対応表の (1) 不正パラメータ拒否時の `22023` 相当
  `wire_code` 契約、(2) 決定性契約（同点規約・境界完全化の要否）、
  (3) RLS 事前フィルタとの切替契約（非可視ノードの探索経路上の扱い・
  切替条件）、(4) 増分更新〔挿入・削除双方〕時の検索結果契約——の
  いずれも外部から観測可能な振る舞いを決めるため、それぞれに対応する
  新規 TASK・ビヘイビア ID を spec 側で起票・確定する
- CORE-9 の状態確定（本ドキュメントの判断結果を踏まえた更新要否）
- TASK-132 の完了判定（本ドキュメントとの関係整理）
- 採用時に必要となる新規 TASK・ビヘイビア ID の要否
- 自作 HNSW 採用・`usearch` 非採用（production）の判断根拠の詳細整理
  （TASK-132・CORE-10 の完了判定と合わせて spec 側で扱う）

本リポでは spec（`docs/spec` submodule）を変更しない。上記は spec リポ側の
課題としてオーナーへ報告する。

## スコープ外

- ANN の実装・`SearchEngineKind` への variant 追加は Phase 3 タスク
  #404〜#413 で実装する。本 Issue（#403）は判断記録・実装ガイドの記述のみ
- 依存追加（usearch の production 採用・pure Rust クレート導入）
- `contrast_bench.rs` への HNSW 対照経路追加とその実測
- HNSW 索引の永続化・テーブル単位カタログ属性による opt-in・
  `wire-server` CLI オプション露出・pure Rust ANN クレートの個別評価
- README「実装方針（要点）」の公開境界拡張（#413 で扱う）

いずれも Phase 3 タスク（#404〜#413）または判断確定後の別 Issue で扱う。
