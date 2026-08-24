# ADR: 日本語主体コーパスでの CJK トークナイザ影響度実測

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-106（`docs/spec/05-tasks.md`・`docs/spec/06-roadmap.md` 参照）
- 対象ビヘイビア: SEARCH-2, SEARCH-5（`docs/spec/04-behavior/search.md`）
- 前提: TASK-102（BM25 疎検索カーネル、`crates/engine/src/sparse.rs`）・
  TASK-105（CJK ストップワード除去、Issue #30 / PR #142 でマージ済み。
  `docs/design/cjk-stopword-tokenizer.md`）
- 関連: TASK-103（RRF 融合）・TASK-104（評価ハーネス回帰テスト化）

## 背景

TASK-105（対象ビヘイビア: SEARCH-5）はミニコーパス（30 件規模）での CJK ストップ
ワード除去 ON/OFF の相対比較に留まり、規模のある実測を TASK-106 のフォローアップと
して申し送っていた（`docs/design/cjk-stopword-tokenizer.md` の「測定・記録」「スコープ
外」参照）。加えて、これまでの CJK トークナイザまわりの実測は本来の対象ドメイン
（Markdown 見出し・箇条書き・雑多メモを含む日本語主体コーパス）とは異なるコーパスに
基づいており、当該ドメインでの構成差の影響度が未計測だった。本 ADR はその実測結果を
記録する。

**位置づけ**: TASK-91（`docs/design/multi-dim-table-coexistence.md`）の先例に倣い、
本タスクは production コード（`crates/engine/src/`）を変更せず、既存 API に対する
実測ハーネス（回帰テスト）・本レポートの追加に限定した。

## 検証設計

### コーパス・QA セットの生成

TASK-106（`docs/spec/05-tasks.md` 参照）は実コーパス選定を含むタスクだが、
自動運転モードのため本 PR では暫定的に**リポジトリ内で決定的に生成する合成
日本語主体コーパスによる暫定測定**として実施した（実コーパスでの再実測は
「制約・スコープ外」参照）。

- 文書数: 5,000 件。文書ごとに Zipf 近似分布（重み `1/(i+1)`）で内容語プール
  （80 語、技術・天気・料理・旅行・仕事・自然・生活・趣味・健康・教育の 10 ドメイン）
  から 3〜6 語を選び、助詞・機能語（30 語）で接続した日本語文に、Markdown 見出し
  （`##`）・箇条書き（`-`）を確率的に混在させ、約 2 割の文書に ASCII 識別子
  （`API`・`TODO` 等）を 1 個混在させた
- QA セット: 100 件。各文書から出現頻度が最も低いキーワード 2 語を選び、その
  AND 一致文書集合を正解集合とする `direct` カテゴリ相当のクエリ（内容語 2 語を
  そのまま連結した短文）
- 生成は決定的シード付き擬似乱数（xorshift64*。外部クレート不使用）によるため、
  実測値は再現可能である
- `sparse.rs` の各上限（`MAX_CORPUS_DOCS` = 100,000・`MAX_DOC_BYTES` = 1 MiB・
  `MAX_CORPUS_BYTES` = 64 MiB）に十分収まる規模で設計した

### トークナイザ変種（3 構成）

同一コーパス・同一 QA セットに対し、`engine::sparse::tokenize_with_options`
（公開 API）のみを用いた測定専用の BM25 再計算ヘルパ（`crates/engine/tests/
sparse_stopwords.rs` の `bm25_rank` パターンを踏襲。ただしクエリ 100 件 ×文書
5,000 件規模のため、コーパス統計を変種ごとに 1 度だけ構築しクエリ間で再利用する
構造に分離した）で 3 変種を比較した。

- A: 除去 ON（現行既定。`SparseIndex` が使う構成）
- B: 除去 OFF
- C: ASCII のみ（B の出力から CJK 由来トークン［ユニグラム・バイグラム］を全除去。
  CJK トークンを一切使わない構成の参照点）

### 指標

各変種の Recall@20・Recall@100 を、正解文書の総数（`total_correct`）に対する
hit 数（micro-average）として測定した。

## 実測結果

- コーパス: 5,000 文書、QA: 100 件、正解文書の総数（`total_correct`）: 2,047

| 変種 | Recall@20 | Recall@100 |
| ---- | --------- | ---------- |
| A: 除去 ON | 0.4558（933/2047） | 0.7963（1630/2047） |
| B: 除去 OFF | 0.4568（935/2047） | 0.7987（1635/2047） |
| C: ASCII のみ | 0.0000（0/2047） | 0.0000（0/2047） |

（C の 0.0000 はクエリが CJK 語彙のみで構成されるための構造的な自明値であり、
劣化度の実測ではない。詳細は「考察」参照）

（`crates/engine/tests/cjk_tokenizer_impact.rs::cjk_tokenizer_impact_on_ja_corpus`、
1/1 pass。上表の hit 数は同テストのアサーションに固定済みで、決定的コーパスのため
再現可能）

## 考察

- **除去 ON と除去 OFF の差は僅少**（Recall@20 で 0.1pt、Recall@100 で 0.24pt、
  除去 OFF がわずかに上回る）。本合成コーパスの QA は内容語 2 語の AND 一致による
  `direct` クエリのみで構成しており、ストップワード（助詞等）由来のノイズマッチが
  ランキング上位に与える影響がミニコーパス実測（TASK-105）ほど大きく現れなかった
  と考えられる。除去 ON/OFF いずれもコーパスの内容語トークン化自体は同一であり、
  差はストップワード由来のスコア加算の有無にとどまるため、この結果は妥当である
- **ASCII のみ構成は Recall 0**。ただしこれは劣化度の実測値ではなく、**構造的な
  自明値**である。本 QA セットのクエリは `CONTENT_VOCAB`（純 CJK 語彙）のみから
  構成されるため、ASCII のみ構成ではクエリ自体が空トークン集合になり、ランキング
  関数が恒等的に空を返す（`(score > 0.0).then_some(...)` により全文書がフィルタ
  される）。この結果自体は「CJK トークンを使わない構成が日本語主体コーパスで
  劣化する」ことの実測ではなく、CJK 語彙のみのクエリを ASCII のみでトークン化
  すれば当然そうなる、という設計上自明な帰結である
- 以上より、本実測で意味のある比較は **除去 ON と除去 OFF の差**（僅少）のみで
  あり、CJK トークナイザ自体の必要性についての定量的な実測とはならなかった
  （Proposed）。CJK トークンの必要性を実測で裏付けるには、ASCII 語彙を含む
  クエリでの構成比較など QA 設計の見直しが要る。本 QA セットは `direct`
  カテゴリのみであり、助詞・機能語自体を問うクエリや、内容語 1 語のみの曖昧な
  クエリでの挙動は範囲外である

## 制約・スコープ外

1. **合成コーパスであり、実コーパスでの人間によるデータセット選定（spec 上の
   人間担当分）は未了**。本実測は暫定値であり、実コーパスでの再実測要否をオーナー
   に確認する必要がある
2. **BM25 疎検索単体の実測**。RRF 融合（TASK-103）・評価ハーネス回帰テスト化
   （TASK-104）は未実装のため、ハイブリッド構成込みの数値はそれらの後続タスク完了
   後に別途実測する
3. **QA セットは `direct` カテゴリ（内容語 2 語の AND 一致）のみ**。曖昧クエリ・
   1 語クエリ・助詞を含むクエリでの挙動は範囲外
4. **語彙は 80 語・10 ドメインの固定プールから合成**。実文書に現れる語彙の多様性・
   共起パターンとは異なりうる
5. `docs/spec` submodule の変更（spec リポ側の作業。SEARCH-2/5 の spec ステータス
   引き上げは本リポからは行わない）

## 参照

- `docs/spec/05-tasks.md`（TASK-106・TASK-105・TASK-102・TASK-103・TASK-104）
- `docs/spec/04-behavior/search.md`（SEARCH-2, SEARCH-5）
- `crates/engine/src/sparse.rs`（`tokenize_with_options`・`SparseIndex`・
  `MAX_CORPUS_*` 上限定数）
- `crates/engine/tests/cjk_tokenizer_impact.rs`（本 ADR の実測ハーネス・回帰テスト）
- `crates/engine/tests/sparse_stopwords.rs`（TASK-105、ミニコーパスでの相対比較の
  前例・`bm25_rank` パターン）
- `docs/design/cjk-stopword-tokenizer.md`（TASK-105、同様の ADR 形式の前例）
- `docs/design/multi-dim-table-coexistence.md`（production コード無変更・実測
  ハーネス限定という位置づけの前例）
