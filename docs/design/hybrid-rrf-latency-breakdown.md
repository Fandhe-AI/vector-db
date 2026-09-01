# hybrid_rrf クエリ内訳プロファイル切り分け

- ステータス: Accepted（本コミットで計測ベンチ・ADR を追加。実測は本開発環境
  ——非専有・並行エージェントあり——での 1 回実測。専有環境での再実測は運用者判断）
- 対応: Issue #356（`test(engine): hybrid_rrf 288ms の内訳プロファイル切り分け`）。
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
  再実測・Issue #357 実装後の before/after 比較はオーナー／実装担当の判断で
  別途実施する
- production コード（`crates/engine/src/`）は本 Issue で無変更（テスト・ベンチ
  専任）
