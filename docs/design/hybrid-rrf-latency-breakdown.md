# hybrid_rrf クエリ内訳プロファイル切り分け

- ステータス: Accepted（本コミットで計測ベンチ・ADR を追加。実測は本開発環境
  ——非専有・並行エージェントあり——での 1 回実測。専有環境での再実測は運用者判断）
- 対応: Issue #356（`test(engine): hybrid_rrf 288ms の内訳プロファイル切り分け`）・
  Issue #387（`search_within` の段別プロファイル・疎側再取得発火回数の追加）。
  親: Issue #355（`SparseIndex` のクエリ毎再構築の排除）
- 前提: `docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3（判定内容・数値基準は
  spec 側が SSOT。本 ADR は spec 由来の pass/fail 閾値を持たない情報提供専用の
  実測記録）

## 背景

Issue #355 は「hybrid 検索（`crates/engine/src/sql/exec.rs` の `Ranking::Hybrid`
分岐）はクエリ毎に可視行の本文を String 複製で収集し、`SparseIndex::build`
（`crates/engine/src/sparse.rs`）をゼロから実行している」という調査結果と、
「feature_bench（25,000 行）実測で hybrid_rrf p50 288.6ms、密 KNN 単体 11.7ms の
約 25 倍」という実測値を記録している。しかし `SparseIndex::build` が実際に
支配的なのか、本文 String 収集や境界同点グループ再取得ループ（Issue #310/#320）
の寄与がどの程度なのかは定量的に切り分けられていなかった。本 Issue はその内訳を
実測し、後続の Issue #357（`SparseIndex` のテーブル世代整合キャッシュ設計）が
「どの段をキャッシュ対象にすべきか」を判断できる分解能を提供する。

## 実測値の比較可能性についての重要な注意

Issue #355 が言及する `crates/engine/examples/feature_bench.rs` は、本ブランチの
分岐元コミット（`f749af6`）時点のリポジトリ履歴に**一度も存在しない**
（`git log --all --diff-filter=A -- 'crates/engine/examples/*'` で確認済み。全
ブランチ・全リモート追跡ブランチを対象にしても該当コミットはなく、GitHub の
コード検索でも `feature_bench` を含むファイルはヒットしなかった）。おそらく
Issue #355 の実測は一時的なローカルスクリプトで行われ、コミットされなかった。

そのため本 Issue のベンチ（`crates/engine/benches/hybrid_profile_bench.rs`・
`crates/engine/benches/harness/hybrid_profile.rs`）は feature_bench の複製では
なく、Issue #356 本文の記述（行数・次元の規模感）にのみ合わせて新規に組み立てた
コーパスを使う。単純化点は以下のとおり:

- 単一テナント・全行 `Visibility::Public`（Issue #355 の tenant-a/tenant-b 2 テナント
  構成は再現しない。本 Issue が切り分けたいのは RLS 境界ではなく hybrid_rrf 内部の
  段別コストのため）
- 疎チャネル本文は 40 語の合成語彙を 1 文書あたり 30 語回転させて生成（feature_bench
  の語彙生成方式そのものは不明なため、非自明な tokenize/BM25 統計コストを持つ
  合成文書として設計）

**したがって本 ADR の実測 ms は Issue #355 の 288ms と直接比較可能ではない**
（コーパス内容・実行環境のいずれも異なる）。本 Issue が実際に必要とするのは
絶対値の再現ではなく「どの段が支配的か」という相対的な内訳の分解能であり、
その目的には十分に応えられる。なお偶然にも本実測の `sql_hybrid` 中央値
（286.1ms、下記「実測結果」参照）は Issue #355 の記録値（288.6ms）に近い値と
なったが、これはコーパス規模（25,000 行）を Issue #356 の記述に合わせたことに
よる規模感の一致であり、意図した再現ではないことに注意する。

## スコープ（計画レビューによる絞り込み）

境界同点グループ再取得ループの寄与は Issue #324（`crates/engine/benches/harness/
hybrid_latency.rs`・`docs/design/hybrid-refetch-latency.md`）で既に測定済みで、
CORE-7 は `hybrid_search` を通らない測定経路のため構造的に不変であることが
確認されている。本 Issue では再取得ループの統計収集を再実装せず、以下 4 点の
分解に絞る:

1. `sql_hybrid` 対 `sql_dense_knn`（SQL 表層エンドツーエンドの対照）
2. 本文 String 収集（`collect_body_strings`）
3. `SparseIndex::build` 全体（`sparse_build_total`）
4. build 内部の tokenize / term_freq 構築 / doc_freq マージの累積 3 段

## 測定設計

`crates/engine/benches/hybrid_profile_bench.rs`（実測本体）・`crates/engine/
benches/harness/hybrid_profile.rs`（時間非依存ヘルパ。決定的コーパス生成・build
内部 3 段の複製・SQL 文字列組み立て）・`crates/engine/tests/hybrid_profile_accept.rs`
（`harness::hybrid_profile` の回帰テスト。`make ci` 対象）で構成する。

- コーパス: 25,000 文書・128 次元・単一テナント・全行 Public（上記「実測値の
  比較可能性」参照）
- 計測プロトコル: `harness::protocol::run`（warmup 20 回・計測 30 回）。SQL 段
  （`sql_hybrid`/`sql_dense_knn`）はクエリ 5 種を round-robin
- `sql_dense_knn` は「密チャネルのみの SQL 対照」であり、`sql/exec.rs` の
  `on_visible_row`（370-509 行）を読むと、`is_hybrid == false` の間は本文
  String 収集（`sparse_docs.push`）自体が実行されない（`scan_scalar_columns`
  による構造検証は hybrid・dense 双方で共通に走るが、`Text` 列の `String` 確保は
  hybrid 側のみ）。したがって `sql_hybrid − sql_dense_knn` の差は「本文 String
  収集＋`SparseIndex::build`＋RRF 融合＋（該当すれば）再取得ループ」の合算に
  ほぼ等しく、`collect_body_strings`・`sparse_build_total` を差し引いた残差を
  「融合＋再取得＋帰属できない差分」として扱う（厳密な対称比較ではない近似）
- build 内部 3 段（`tokenize_only`・`tokenize_term_freq`・`tokenize_term_doc_freq`）
  は `sparse.rs::with_params` のロジックを計測用に手動転記した複製実装で、
  上限検証（`MAX_DOC_BYTES` 等）は行わない。複製の妥当性は「同一コーパスに対し
  実際の `SparseIndex::build` が成功するか」という構造的整合性チェック
  （`build_actually_succeeds`）のみで担保する——`SparseIndex` の内部フィールド
  （`doc_freq`・`docs`）が private のため、複製実装の語彙数と実際の内部状態を
  数値比較する手段が公開 API には存在しない（下記「複製近似の限界」参照）

## 実測結果

本開発環境（Linux/x86_64、論理コア数 12、Avx2Fma。**非専有**——他エージェントが
並行実行中——のため下記数値は参考値として扱う）での 1 回実測
（`make bench-hybrid-profile`。中央値・p95、単位 ms）:

| stage | median | p95 | 備考 |
| ----- | -----: | --: | ---- |
| `sql_hybrid` | 286.1 | 303.0 | SQL 表層 hybrid_rrf エンドツーエンド |
| `sql_dense_knn` | 9.0 | 9.6 | SQL 表層・密 KNN のみ（対照） |
| `collect_body_strings` | 0.84 | 0.86 | 本文 25,000 件の `String` clone 収集 |
| `sparse_build_total` | 176.4 | 181.2 | `SparseIndex::build` 単体 |
| `tokenize_only` | 20.3 | 20.7 | tokenize 段（累積） |
| `tokenize_term_freq` | 85.2 | 86.2 | tokenize + term_freq 構築（累積） |
| `tokenize_term_doc_freq` | 150.9 | 153.7 | tokenize + term_freq + doc_freq マージ（累積） |

検算値（決定的コーパスに対する固定値）: `tokenize_only` の総トークン数
800,000（25,000 文書 × 30 語 + 各文書の `doc-{id}` トークン 1 個）、
`tokenize_term_doc_freq` のコーパス全体語彙数 25,041。

### 帰属分析

`sql_hybrid − sql_dense_knn` = 286.1 − 9.0 = **277.1ms**（hybrid 経路が dense 経路
に対して上乗せするコストの上限）。この差分に対する内訳:

| 内訳 | ms | 対 277.1ms 比率 |
| ---- | -: | --------------: |
| 本文 String 収集（`collect_body_strings`） | 0.8 | 0.3% |
| `SparseIndex::build`（`sparse_build_total`） | 176.4 | **63.7%** |
| 残差（融合・再取得ループ・帰属できない差分） | 99.9 | 36.0% |

`SparseIndex::build` が単独で差分の 6 割以上を占め、Issue #355 のコードリーディング
上の推定（build がクエリ毎再構築のコストの主要因）を定量的に裏付ける。残差
36% は融合（`hybrid::rrf_fuse`）・（本コーパスでは連続値ベクトルのため通常は
発生しない）再取得ループ・測定誤差の合算だが、単独の内訳としては
`SparseIndex::build` を下回る。

### build 内部 3 段の内訳（累積差分から算出）

| 段 | 累積 median | 段単独の限界寄与 |
| -- | ----------: | ---------------: |
| tokenize | 20.3ms | 20.3ms |
| term_freq 構築 | 85.2ms | 85.2 − 20.3 = 64.9ms |
| doc_freq マージ | 150.9ms | 150.9 − 85.2 = 65.7ms |
| （`DocEntry`/`id_index` 構築＋上限検証） | 176.4ms（`sparse_build_total`） | 176.4 − 150.9 = 25.5ms |

tokenize・term_freq 構築・doc_freq マージがほぼ均等に build 全体の 3 割前後
（tokenize 11.5%・term_freq 36.8%・doc_freq 37.2%・残り 14.5% が `DocEntry`/
`id_index` 構築と上限検証）を占め、単一のホットスポットではなく `BTreeMap<String,
_>` を用いた文字列キー処理全体（tokenize が生成する `String` の clone を term_freq・
doc_freq の双方が繰り返す構造）がコストの主要因であることが読み取れる。

### 複製近似の限界

`tokenize_term_doc_freq`（複製実装の doc_freq マージ後、`DocEntry`/`id_index`
構築前）は 150.9ms、`sparse_build_total`（実際の `SparseIndex::build`）は
176.4ms で、比率は 150.9 / 176.4 ≈ **0.855**。残り 14.5%（25.5ms）は
`DocEntry`/`id_index` の構築コスト・4 段の上限検証（`MAX_DOC_BYTES` 等）の
オーバーヘッドとして妥当な範囲に収まっており、複製実装が build の主要コスト
（tokenize・term_freq・doc_freq）を過小評価も過大評価もしていないことを示す
（計画時に設定した許容乖離の目安 15% 以内）。

## Issue #357（キャッシュ設計）への示唆

- `SparseIndex::build` がクエリ毎再構築コストの支配項（対 hybrid 上乗せ分の
  64%）であることが定量的に裏付けられた。Issue #357 が「テーブル世代整合の
  `SparseIndex` キャッシュ」を設計する際、キャッシュ対象は `SparseIndex::build`
  全体（本文 String 収集は build の入力であり、キャッシュヒット時は入力生成
  自体が不要になるため合わせて省略される）で妥当
- build 内部の tokenize / term_freq / doc_freq はいずれも同程度の寄与（各 3 割
  前後）であり、内部の特定段だけを個別最適化するより、build 全体をテーブル
  世代（`catalog.rs::bump_table_generation_in_txn` 等が進める世代カウンタ）に
  紐付けてキャッシュし、書き込みが無い限り再構築を省略する設計（本リポ既存の
  `PrefilterCache`/`DictionaryCache` と同じ世代整合・fail-closed 契約。Issue
  #280/#285 参照）が最も効果が大きいと考えられる
- 残差 36%（融合・再取得ループ等）はキャッシュでは削減できないため、
  `SparseIndex` キャッシュ導入後の hybrid_rrf レイテンシは dense KNN 対照
  （本実測で 9.0ms）まで下がるのではなく、本文収集＋build を除いた残差
  （本実測で概算 100ms 程度）に漸近すると見込まれる。この残差の縮小は
  Issue #357 のスコープ外（別途 Issue #320/#324 の枠組みで扱う）

## 再現方法

```
make bench-hybrid-profile
```

`.github/workflows/*` へは配線しない（手動実行専用）。`GITHUB_ACTIONS` 環境変数
が設定された実行環境下では起動直後に fail-closed で拒否する
（`harness::hybrid_profile::refuse_under_github_actions`）。判定ロジック自体
（時間非依存）は `crates/engine/tests/hybrid_profile_accept.rs` で `make ci`
側から回帰検証する。

## 申し送り

- 本実測は非専有環境（並行エージェントあり）での 1 回実測であり、専有環境での
  再実測はオーナー／実装担当の判断で別途実施する
- Issue #357（疎索引キャッシュ）実装後の `feature_bench` `hybrid_rrf` フェーズの
  before/after 比較は Issue #358 で実施済み（`docs/design/
  sparse-index-cache-verification.md` 参照。`hybrid_profile_bench` 自体の
  before/after 再実測は同 Issue で時間予算の都合により見送り、申し送り事項）
- production コード（`crates/engine/src/`）は本 Issue で無変更（テスト・ベンチ
  専任）

## Issue #387: `search_within` 段別・再取得発火回数の基線

Issue #357（`SparseIndex` のテーブル世代整合キャッシュ）導入後、キャッシュ
ヒット時になお残るコストが `crates/engine/src/sparse.rs::search_within`
単体（可視 subset 構築／df 再計算パス／スコアリングパスの 3 区間）と、それを
繰り返し呼ぶ `hybrid.rs::hybrid_search_boosted` の疎側再取得ループ
（Issue #320）のどちらにどれだけ帰属するかは未計測だった。本節はその内訳と、疎側の
再取得発火回数・密側の発火回数を実測した基線を記録する（Issue #358 の
`feature_bench` `hybrid_rrf` hot p50 約 147ms・本 ADR 前節の残差 36%
〔約 100ms〕の主な内訳仮説を検証する位置づけ）。

### 測定設計

`crates/engine/src/sparse.rs::SparseIndex`（`docs`・`id_index`・`k1`・`b` は
private フィールド）・`crates/engine/src/hybrid.rs` の境界同点判定
（`resolve_boundary_tie_group`/`complete_boundary_tie_group_by`。いずれも
`pub(crate)`/private）はいずれもベンチ・統合テストから直接呼べないため、
`harness/hybrid_profile.rs` に以下を追加した:

- `ProfileSparseIndex`: `search_within` の 3 区間（`subset_only`／`subset_df`／
  `search_within_replica`）を個別に呼び分けられる複製索引。`replica_matches_real`
  が実 `SparseIndex::search_within` の出力（`doc_id` 列・スコア）と数値一致する
  ことをベンチ起動時に fail-closed で検証してから使う（Issue #356 の build
  複製と異なり、本複製は公開 API 経由で出力を直接比較できる）
- `boundary_tie_decision`: 境界同点判定の**判定結果のみ**（列の切り詰めは
  行わない）を複製し、`sparse_refetch_schedule`／`dense_refetch_schedule`
  （鏡像定数 `MAX_POOL_DEPTH_MIRROR`/`MAX_FETCH_K_MIRROR` を使い、初期
  `fetch_k` から倍増しつつ実 `search_within`/`provider.search` を呼ぶ）が
  疎側・密側それぞれの再取得スケジュールを予測する。密側予測はベンチ起動時に
  `RefetchTrackingProvider`（既存 Issue #324 ハーネス）が観測する実際の呼び出し
  回数と突き合わせて fail-closed 検証する

複製固有の限界: `MAX_POOL_DEPTH_MIRROR`/`MAX_FETCH_K_MIRROR` は `hybrid.rs`
側の値をこのファイルへ手動転記したものであり、コード上の同期は強制されない
（ドリフトは `max_pool_depth_mirror_matches_rrf_config_bounds` アクセプトテストが
`RrfConfig::new` の受理境界との突き合わせで検知する）。境界同点判定の複製は
判定結果（`Resolved`/`Undetermined`）のみを再現し、`hybrid_search_boosted` が
行う「境界の同点グループ全体を対称に除外する」列操作（`exclude_undetermined_
boundary_group`）自体は複製しない（本測定が必要とするのはスケジュール
——何回・どの `fetch_k` で呼ばれるか——であり、最終的にどの候補が採用されるかの
複製は不要なため）。

計測条件は Issue #356 と同一コーパス（25,000 行・dim 128・単一テナント・全行
`Visibility::Public`）・同一クエリ集合（5 件）・同一プロトコル
（`MeasurementConfig::new(20, 30, SEED)`）。`pool_depth` は SQL 表層の既定
（`sql/exec.rs::DEFAULT_HYBRID_POOL_DEPTH` の鏡像 `SQL_DEFAULT_HYBRID_POOL_DEPTH`
= 200）を使い、`SparseIndex`・`ProfileSparseIndex` は事前に 1 回だけ構築して
以降の全段で使い回す（`sql/sparse_cache.rs::SparseIndexCache` のキャッシュ
ヒット経路と同型の前提）。

### 実測結果（本開発環境・非専有・並行エージェントあり・1 回実測）

忠実性検証（複製 ↔ 実 API・密側スケジュール予測 ↔ 実測呼び出し回数）はいずれも
通過（`hybrid_profile: fidelity checks passed ...`）。

| 段 | median | p95 | 備考 |
| -- | -----: | --: | ---- |
| `hybrid_search_cached_index`（キャッシュヒット相当・`hybrid_search` 直接呼び出し） | 115.7ms | 136.1ms | `provider_calls_max=1`（密側は 1 回で確定） |
| `search_within_fetch_k=400`〜`25000`（各 fetch_k 単体） | 17.2〜18.8ms | 18.3〜21.7ms | `fetch_k` によらずほぼ一定 |
| `search_within_subset_only`（区間 1） | 0.78〜0.79ms | 0.81ms | k=400/25000 でほぼ不変（可視集合サイズのみに依存） |
| `search_within_subset_df`（区間 1+2） | 9.66〜10.01ms | 10.76〜10.95ms | 区間 2 単独の寄与 ≈ 8.9〜9.2ms |
| `search_within_replica_full`（区間 1+2+3） | 15.25〜16.76ms | 17.09〜17.89ms | 区間 3 単独の寄与 ≈ 5.6〜6.8ms |

疎側再取得ループの実測（5 クエリ）:

```
sparse_refetch query=0 calls=7 fetch_ks=400,800,1600,3200,6400,12800,25000 reached_cap=true
sparse_refetch query=1 calls=7 fetch_ks=400,800,1600,3200,6400,12800,25000 reached_cap=true
sparse_refetch query=2 calls=7 fetch_ks=400,800,1600,3200,6400,12800,25000 reached_cap=true
sparse_refetch query=3 calls=6 fetch_ks=400,800,1600,3200,6400,12800       reached_cap=false
sparse_refetch query=4 calls=6 fetch_ks=400,800,1600,3200,6400,12800       reached_cap=false
sparse_refetch_summary queries=5 calls_max=7 calls_total=33 reached_cap_count=3 max_fetch_k=25000
```

密側は `provider_calls_max=1`（初回 `fetch_k=400` で境界確定）で再取得が
発生していない。疎側は 5 クエリ全件で 6〜7 回発火し、うち 3 クエリは可視集合
全体（25,000 件）まで倍増し続ける exhaustive 到達（本ベンチの合成コーパスは
文書ごとに語彙を等間隔で回転させるだけの構成のため BM25 スコアが大量に同点に
なりやすく、Issue #356 前提調査で想定した「最悪ケースに近い同点誘発」が
実際に生じている）。

### 帰属分析

- `search_within` 単体は `fetch_k` によらずほぼ一定（17〜19ms）であり、これは
  区間 1（subset 構築・0.8ms・全体の 4〜5%）・区間 2（df 再計算パス・8.9〜
  9.2ms・全体の約 50%）・区間 3（スコアリング・5.6〜6.8ms・全体の 35〜40%）の
  合算で説明できる。**df 再計算パス（区間 2）が単独最大の寄与**であり、
  「クエリ語ごとに可視 subset 全件を線形走査して df を数え直す」処理
  （`sparse.rs::search_within` 832〜842 行の複製）が `fetch_k` に依存しない
  固定コスト（可視集合サイズにのみ依存）として支配的である
- 疎側再取得ループの累積コストは `sparse_refetch_summary` の
  `estimated_cumulative_mixed_median_us=125613`（`search_within_fetch_k=<k>`
  段——各 `fetch_k` を全クエリで round-robin 測定した**全クエリ混合集団**の
  実測中央値であり、クエリ別の実測値ではない——を、最も再取得回数が多い
  クエリの実スケジュールに沿って合算した**推定値**。以前は
  `cumulative_median_us`・「実測値」「最悪ケース」と表記していたが、実体は
  全クエリ混合中央値による推定である。codex-review P1 指摘対応・PR #416）で、
  `hybrid_search_cached_index` の実測 median（115.7ms）と近い値になった
  （`search_within` 自体が `fetch_k` に依存せずほぼ一定のため、再取得回数の
  多寡がほぼ線形にコストへ跳ね返る。ただし上記のとおり全クエリ混合中央値に
  よる推定であり、クエリ別の真の累積コストとの乖離は未検証）
- 本 ADR 前節（Issue #356）が「残差 36%（約 100ms）」としていた帰属不能分は、
  本実測により**大部分が疎側再取得ループ（`search_within` を 6〜7 回繰り返す
  こと）で説明できる**ことが確認された。`search_within` 1 回あたりのコストの
  半分（区間 2・df 再計算パス）は `fetch_k` を増やしても変わらないため、
  再取得回数そのものを減らす（同点誘発を弱める・境界判定を改善する）よりも、
  df 再計算パスを転置索引（`term -> doc_id` の逆引き構造）で置き換え、
  クエリ語ごとに可視 subset を全走査せず該当 posting のみを辿る方式へ変える
  方が、再取得 1 回あたりのコストを直接削減でき効果が大きいと考えられる

### 転置索引化（Phase 1 後続）への示唆

親 Issue 系譜（#355 → #356 → #357 → #358 → 本 Issue）が指す「転置索引化」を
実施する場合、本実測が示す優先順位は以下のとおり:

1. **df 再計算パス（区間 2）**: 転置索引があれば「クエリ語ごとに `term -> df`
   を可視 subset に限定して再計算する」処理は、posting リストの長さを
   可視集合でフィルタしてカウントするだけになり、可視 subset 全件の線形走査
   （現状 O(|subset| × |query_terms|)）を避けられる
2. **スコアリングパス（区間 3）**: 同様に posting リスト経由で「その語を含む
   文書」だけを走査すればよくなり、subset 全件を舐めて `term_freq.get` する
   現状より削減余地がある（ただし本実測では区間 3 は区間 2 より小さい寄与
   〔35〜40% 対 50%〕であり、優先度は区間 2 より低い）
3. 疎側再取得ループの発火回数自体（本ベンチの同点誘発が強い合成コーパスでは
   6〜7 回）は転置索引化では直接減らない（境界同点判定のロジックは変わらない
   ため）。ただし 1 回あたりのコストが下がれば累積コストは比例して下がる

### 限界・申し送り

- 本実測は非専有環境（並行エージェントあり）での 1 回実測であり、専有環境
  での再実測はオーナー／実装担当の判断で別途実施する
- 本ベンチの合成コーパス（40 語の語彙を等間隔で回転させるだけの構成）は
  BM25 スコアが同点になりやすく、疎側再取得の発火回数（6〜7 回・うち過半が
  exhaustive 到達）は実コーパスでの発火回数の上限に近い可能性がある。実
  コーパスでの疎側再取得発火回数は本実測より少ない可能性がある点に注意
- production 側フック案（`hybrid_search_with_diagnostics` のような診断 API を
  `hybrid.rs` へ追加する案）は、本実測でベンチ側複製＋忠実性検証のみで要件を
  満たせたため不採用（`crates/engine/src/` は本 Issue で無変更）。
  `SparseIndex::search_within` は `hybrid.rs::hybrid_search_boosted` から
  具象型 `&SparseIndex` へ直接呼ばれる構造のため、密側の
  `RefetchTrackingProvider`（`&dyn SearchProvider` を介した外部観測）と
  同型の呼び出し回数観測フックは存在しない、という制約自体は変わらない
  （codex-review P1 指摘・PR #416。`sparse_refetch_schedule` の疎側発火回数は
  依然としてベンチ側複製〔`boundary_tie_decision`〕の予測値であり、
  production の実呼び出し列そのものと突き合わせた call-count 一致検証では
  ない）。代わりに `verify_sparse_schedule_terminal_is_stable`
  （`harness/hybrid_profile.rs`）を追加し、各クエリのスケジュール終端が
  予測 `fetch_k` の 1 段先まで実際に呼んでも上位プレフィックスが変化しない
  固定点であることを実 API で確認する間接検証を起動時 fail-closed に行う
  （呼び出し回数の外部観測ではなく終端判定の正しさの間接検証にとどまる限界
  は同関数のドキュメント参照）
- 転置索引化そのもの（設計・実装）は本 Issue のスコープ外。本節の帰属分析・
  優先順位は次段の設計判断の入力として記録するに留める
