# ADR: ハイブリッド検索 Recall 回帰ハーネス（TASK-104）

- ステータス: Proposed（本 PR のマージ後、別コミットで Accepted に更新する）
- 対応: TASK-104（`docs/spec/05-tasks.md`・`docs/spec/06-roadmap.md` 参照）
- 対象ビヘイビア: SEARCH-1, SEARCH-2（`docs/spec/04-behavior/search.md`）
- 前提: TASK-102（BM25 疎検索カーネル・`crates/engine/src/sparse.rs`）・TASK-103
  （RRF 融合・`crates/engine/src/hybrid.rs`）・TASK-105（CJK ストップワード除去）は
  いずれもマージ済み（PR #138・#142・#144）
- 関連: TASK-106（`docs/design/cjk-tokenizer-impact-ja-corpus.md`。決定的合成コーパス
  生成・固定値回帰トラッキング方式の先行実装）・TASK-127（`crates/engine/benches/
  simd_bench.rs`。spec 閾値の Actions secrets 注入パターンの先行実装。
  当初は variables を使っていたが Issue #286 で secrets へ移行）

## 背景

ハイブリッド検索カーネル（疎 BM25・RRF 融合・CJK ストップワード除去）は実装・マージ
済みだが、検索品質（Recall）の受け入れ基準を自動チェックする回帰テストが存在しな
かった。本 ADR は TASK-104 に対応し、小規模・大規模の 2 段のスケール条件付き Recall
基準を自動チェックする回帰テスト＋CI workflow を追加した設計を記録する。

**位置づけ**: TASK-106 の先例（`docs/design/cjk-tokenizer-impact-ja-corpus.md`）に
倣い、本タスクは production コード（`crates/engine/src/`）を変更せず、既存 API に
対する実測ハーネス（回帰テスト）・本レポート・CI workflow の追加に限定した。

## 検証設計

### 2 層構成（PR CI と閾値ゲートの分離。役割分担は spec 機密保持が優先）

- **層 A**（`#[test]`。常時 `cargo test` 対象）: 決定的コーパスでのヒット数を固定値
  アサーションで回帰トラッキングする（TASK-106 と同方式）。spec の数値基準を使わない
  ため public 資産に閾値を持ち込まない。`.github/workflows/ci.yml` の `cargo test`
  で PR ごとに常時実行されるため、**PR のマージ判定は層 A が担う**
- **層 B**（`#[ignore]`。`make recall-regression` 経由）: spec 由来の Recall 下限
  （`HYBRID_RECALL_MIN_R20_SMALL`・`HYBRID_RECALL_MIN_R20_LARGE`・
  `HYBRID_RECALL_MIN_R100_LARGE`。`.github/workflows/recall.yml` が environment
  `recall-gate` の Actions secrets から注入。Issue #286・`env:` ブロックの
  ログ印字による漏えいを防ぐため variables ではなく secrets を使う）と実測値を
  比較する閾値ゲート。
  ローカルの `make recall-regression`（`HYBRID_RECALL_REQUIRE_THRESHOLDS` を
  注入しない）では未設定（GitHub Actions では空文字列に解決される variable も
  含む）は「ゲート未設定＝明示的に対象外」を出力して成功終了し
  （`crates/engine/benches/simd_bench.rs::core5_requested_from_env` と同じ
  opt-in パターン）、設定済みで非数値・範囲外は fail-closed でテスト失敗とする。
  `recall.yml` からの実行は strict モード（下記「strict モードによる誤 green
  防止」参照）が既定で有効なため、未設定も fail-closed になる。ログには対象名と
  pass/fail のみを出力する（README「Recall 回帰ハーネスの repo secrets」参照。
  出力方針の詳細は下記「出力方針（実測値の既定非出力・Issue #303）」参照）。

  `.github/workflows/recall.yml` は `pull_request` トリガを**意図的に持たない**
  （`workflow_dispatch` + 週次 `schedule`。下記「strict モードによる誤 green
  防止」の疎通確認後、Issue #168 で `schedule` を再追加済み）。`pull_request` で起動する job は
  PR 側の untrusted なコード（Makefile・テストコード含む）を checkout して実行する
  ため、層 B を PR トリガにすると PR がコードを書き換えて `HYBRID_RECALL_MIN_*`
  （spec 由来の非公開閾値）を標準出力へ書き出すだけで public な Actions ログから
  spec の数値基準を取得できてしまう（`.claude/rules/spec-confidentiality.md` の
  P0 違反。PR #147 codex-review で一度 `pull_request` トリガを追加したが、この
  P0 指摘により巻き戻した）。この経緯から、層 B は「trusted なコードのみが走る
  非公開閾値ゲート」、層 A は「PR ごとに走る public な固定値ゲート」という役割分担を
  設計判断として採用する

  トリガを絞るだけでは不十分な点にも注意が必要である: `workflow_dispatch` は
  本来任意の ref を選んで手動起動できるため、Makefile・テストコードを書き換えた
  任意 ref を選んで実行すれば `pull_request` と同じ経路（書き換えたコードが
  `HYBRID_RECALL_MIN_*` を受け取り標準出力へ書き出す）で spec 閾値を漏えいできる
  （PR #147 codex-review 継続指摘）。そのため `recall-regression` job には
  `if: github.ref == 'refs/heads/main'` を付け、main 以外の ref を選んで
  `workflow_dispatch` を起動した場合は job ごとスキップする。`actions/checkout` も
  dispatch 側の ref 選択に依存させず `ref: main` で明示的に固定する。

  ただし `if`／`checkout ref` は workflow YAML 自体に書かれた条件であり、
  workflow_dispatch は選択した ref の YAML 定義をそのまま実行するため、write
  権限者が別ブランチでこのガードを外した `recall.yml` を push して
  `workflow_dispatch` すれば実行境界として機能しない。加えて repo レベルの
  Actions secrets はどのブランチのどの workflow からも読めるため、YAML 内の
  if 条件だけでは `HYBRID_RECALL_MIN_*` の参照自体を防げない
  （Cursor Bugbot High 指摘）。

  そのため実際の実行境界は YAML の条件式ではなく GitHub Environments の
  ブランチ保護で作る: `recall-regression` job に `environment: recall-gate`
  を指定し、`HYBRID_RECALL_MIN_*` は repo レベルではなく environment
  `recall-gate` の secrets として設定する（参照記法は repo レベル secrets と
  同じ `secrets.*` だが、job の `environment:` 指定により解決スコープが
  environment レベルへ切り替わる）。environment `recall-gate` は deployment
  branch policy で `main` のみに制限して作成する（リポジトリ管理者作業。
  README「Recall 回帰ハーネスの repo secrets」参照）。main 以外の ref から
  起動した run は environment `recall-gate` にアクセスできないため、別ブランチの
  改変 YAML から `if`／`checkout ref` を外して `workflow_dispatch` したとしても
  environment 自体にアクセスできず閾値を取得できない。`if:
  github.ref == 'refs/heads/main'`・`checkout ref: main` は environment 保護に
  対する defense-in-depth として維持する。

### strict モードによる誤 green 防止（`HYBRID_RECALL_REQUIRE_THRESHOLDS`）

層 B の opt-in 方式（未設定＝「対象外」として成功終了）は PR 実行を想定した
設計だったが、`recall.yml` からの実行（現状 `workflow_dispatch` のみ）では
別の問題を生む: environment `recall-gate` の作成漏れ・secret 名の打ち間違い・
secret の誤削除で `HYBRID_RECALL_MIN_*` が読めなくなった場合、opt-in 方式では
「一度も評価していない run」が「基準を満たした run」と同じ green になり、閾値
ゲートが実質的に機能を失っていても気付けない（codex-review P1 継続指摘）。

そこで `crates/engine/tests/hybrid_recall.rs` に `HYBRID_RECALL_REQUIRE_THRESHOLDS`
環境変数（`"1"` のときのみ true。[`strict_thresholds_required`]）による strict
モードを追加した。`recall.yml` は Run step で常にこのフラグを注入し、strict
モード下では未設定を非数値・範囲外と同様に fail-closed でテスト失敗とする
（[`resolve_gate_threshold`]）。ローカルの `make recall-regression` にはこの
フラグを注入しないため、そちらは従来どおり opt-in 挙動を維持する。

`.github/workflows/bench.yml`（TASK-127）で codex-review に受理された前例
（CORE-5 未接続の間は `schedule` を有効化せず `workflow_dispatch` のみに限定し、
接続確認後に `schedule` を再度追加する）と同型の判断として、`recall.yml` も
一旦 `schedule` トリガを外し `workflow_dispatch` のみとしていた。Issue #168 の
オーナー判断により `schedule`（週次・月曜 04:00 UTC）を再度追加済みだが、
environment `recall-gate` の secrets 設定・strict モードでの手動実行による
疎通確認はリポジトリ管理者作業として別途必要（未実施のまま週次 run が走った
場合は fail-closed で red になる。false green にはならない。README「Recall
回帰ハーネスの repo secrets」参照）。

### 出力方針（実測値の既定非出力・Issue #303）

層 B ゲートは層 A と同一の決定的コーパス（同一 seed・件数）を測定するため、
実測 Recall は層 A の固定値定数（`hits20`/`ceil20` 等）から public に導出
可能である。失敗時（`pass=false`）は「非公開閾値 > 実測値」という上界が、
成功時は下界が推定できてしまうため、`recall@k=<実測値> pass=<bool>` の
併記による非公開閾値の逆算を防ぐ defense-in-depth・方針統一として、以下を
既定挙動とする（`crates/engine/benches/batch_bench.rs::
verbose_requested_from_env`・Issue #277〜#279 で確立した「既定出力は真偽値
のみ」方針の Recall テスト側への横展開）。

- **既定出力**: 層 B ゲート（`hybrid_recall_small_scale_threshold_gate`・
  `hybrid_recall_large_scale_threshold_gate`）・層 A 回帰
  （`hybrid_recall_small_scale_regression`・`hybrid_recall_large_scale_
  regression`）のいずれも、標準出力には対象名（テスト名・指標名）と
  `pass=<bool>` のみを含み、実測値の数値を含めない
  （`resolve_verbose`・`render_gate_line`）。
- **`RECALL_VERBOSE=1` opt-in**: ローカルでの実測値確認は明示的な opt-in に
  限定する。値は厳密一致 `"1"` のみ有効（`crates/engine/benches/harness/
  sql_c1.rs::resolve_verbose` と同一規約。空白付き値・他表記は無効側へ倒す）。
- **`GITHUB_ACTIONS` 下の拒否**: opt-in と `GITHUB_ACTIONS`（値を解釈せず
  存在有無のみ判定）が同時に立っている場合は、コーパス生成・測定の前に
  `panic!`（固定文言。数値を含まない）で fail-closed に拒否する
  （`resolve_verbose`）。
- **`recall.yml` への非注入**: `.github/workflows/recall.yml` は
  `RECALL_VERBOSE` を意図的に注入しない運用と、上記テスト側の拒否を
  二重化する（`bench.yml` の `BENCH_VERBOSE` と同型）。
- **転記禁止**: `RECALL_VERBOSE=1` で得た実測値そのものを本ドキュメント・
  PR 本文・Issue・コミットメッセージ等の public 資産へ転記しない
  （`.claude/rules/spec-confidentiality.md`）。

同種の出力方針は `crates/engine/tests/rerank_recall.rs`・
`crates/engine/tests/query_planning_recall.rs`・
`crates/engine/tests/precision_eval.rs` へも横展開済み（`query_planning_
recall.rs`・`precision_eval.rs` の既定出力は本 Issue 以前から実測値を含んで
おらず、`RECALL_VERBOSE` はローカル診断用の opt-in として方針統一のために
追加した。`precision_eval.rs` の `precision_eval_report`／
`precision_eval_policy_sweep`〔判断材料専用の常時実測値出力テスト〕は
`GITHUB_ACTIONS` 検出時に測定前 `panic!` で拒否する専用ガードを持つ）。

### コーパス・QA セットの生成

`crates/engine/tests/hybrid_recall.rs` は決定的シード付き擬似乱数（xorshift64*。
外部クレート不使用）で合成コーパスと QA セットを生成する（TASK-106 の設計を踏襲）。

- 文書の**潜在**トピック集合（`Doc::keywords`）は Zipf 近似分布（重み `1/(i+1)`）で
  語彙プールから 3〜6 語を選ぶ。この潜在集合が正解判定（inverted index・QA の
  `correct`）の唯一の情報源であり、検索特徴量（テキスト・密ベクトル）とは独立に
  扱う（次項「疎・密チャネルの lossy view」参照。PR #147 codex-review 指摘対応:
  以前は潜在集合をそのままテキスト・密ベクトル両方へ符号化しており、正解判定と
  検索特徴量が実質同一だったため全指標が機械的に 1.0 に近づいていた）
- QA セットは各文書から出現頻度が最も低いキーワード 2 語を選び、その AND 一致
  文書集合を正解集合とする `direct` カテゴリ相当のクエリ。正規化した語ペア
  （`min`/`max`）を `BTreeSet` で追跡し、同一ペアの重複登録を除外する（重複登録は
  Recall を特定の語ペアへ偏らせるため。PR #147 codex-review 指摘対応）
- 語彙数（`vocab_size`）はコーパス規模に応じて可変にする: 固定語彙表を使うと大規模
  段で正解集合が肥大化し、Recall@k の理論上限（Σmin(k,\|correct_q\|)）に対して
  文書規模とほぼ独立な比較ができなくなるため、規模に応じた語彙数（小規模: 60 語・
  大規模: 800 語）で正解集合の絞り込み度を揃えている
- `sparse.rs` の各上限（`MAX_CORPUS_DOCS`・`MAX_DOC_BYTES`・`MAX_CORPUS_BYTES`）に
  十分収まる規模で設計した（コーパス規模は環境変数から受け取らず、テスト内定数のみで
  決める。無制限アロケーション防止）

### 疎・密チャネルの lossy view（正解判定との分離）

疎検索（BM25）は「クエリ語を両方含む文書」を自然に上位へ置くが、密ベクトルを
トピック方向のランダムベクトルの平均として素朴に合成すると、無関係なトピック間の
交差項ノイズが AND 一致の信号を上回り得ることを実測で確認した（`RrfConfig::default()`
は密・疎を等重みで融合するため、密チャネルが弱く無相関だと疎チャネルの正しい順位が
押し流される）。そのため密ベクトルは語彙数と同じ次元数の one-hot 基底ベクトルの和
として合成する（`crates/engine/tests/hybrid_recall.rs::one_hot_sum`）方針を維持する。

一方、テキスト・密ベクトルのいずれも文書の潜在トピック集合（`Doc::keywords`）を
**そのまま**符号化すると、正解判定（潜在集合の AND 一致）と検索特徴量が実質同一に
なり、Recall が構造的に理論上限（1.0）へ張り付いてしまう。これを避けるため、
両チャネルを潜在集合の非完全な観測（lossy view）として独立に生成する
（`generate_corpus` のドキュメント参照）:

- 疎チャネル（`text`）: 各潜在トピックを確率 `TEXT_KEYWORD_DROPOUT_PROB` で脱落させる
  （埋め込み・索引が潜在トピックを捉え損ねる状況を模す）
- 密チャネル（`vector`）: 各潜在トピックを確率 `VECTOR_KEYWORD_DROPOUT_PROB` で独立に
  脱落させ、確率 `VECTOR_DECOY_PROB` で無関係なトピック次元を 1 つ混入させる（decoy。
  埋め込みが無関係トピックへ誤って反応する状況を模す）

これにより「疎のみ／密のみでしか見つからない正解例」「両方脱落し、どちらからも
見つけにくい正解例」が構造的に生まれる。ドロップアウト・デコイの確率はいずれも
テストハーネスの fixture パラメータであり、spec 由来の数値基準ではない
（`.claude/rules/spec-confidentiality.md`）。脱落・混入はいずれも 0/1 の one-hot
次元への操作のみで、浮動小数点の連続ノイズは加えない——スコアが常に小さい整数値の
厳密な和になるため、`ParallelSearchProvider` の行範囲分割（スレッド数）に依存する
丸め誤差や順序依存が生じず、層 A の固定値アサーションが決定的に再現できる。
クエリ側の密ベクトル・テキストは潜在集合から直接構成し、lossy view はドキュメント
側にのみ適用する（「意図が明確なクエリ」を表す簡略化。「既知の制約」参照）。

### 検索経路

production API（[`SparseIndex::build`]・[`ParallelSearchProvider`]・
`hybrid::hybrid_search` ＋ `RrfConfig::default()`）のみを使用し、BM25/RRF の再実装は
行わない。`RrfConfig::default()` は spec 採用構成（等重み・pool_depth 200・
k_const 60）に一致する。

### 指標

Recall@20（小規模段）・Recall@20/Recall@100（大規模段）は、正解文書の総数
（`total_correct`）ではなく理論上限（`ceil` = Σmin(k,\|correct_q\|)）を分母とする
到達率（`hits / ceil`）として測定する。正解集合が k 件を超えるクエリが混ざると
`total_correct` を分母にした場合に理論上の最大値が 1.0 未満へ頭打ちになり
（層 A・層 B で分母の意味が揃わなくなる問題があったため）、層 A（回帰トラッキング）・
層 B（spec 閾値ゲート）とも `ceil` を分母に統一している
（`crates/engine/tests/hybrid_recall.rs::RecallResult::recall20`/`recall100`）。
「疎・密チャネルの lossy view」の導入後は `hits < ceil`（到達率 100% 未満）が通常
であり、その `hits`/`ceil`/`total_correct`/QA 件数（重複除外後）を層 A の固定値
アサーションで回帰トラッキングする。

## 小規模段ゲート未達の engine 側原因調査（Issue #307）

- 対応: TASK-103／TASK-104／TASK-84（`docs/spec/05-tasks.md`）
- 対象ビヘイビア: SEARCH-1・SEARCH-3（`docs/spec/04-behavior/search.md`）
- 前提: 親 Issue #301（`.github/workflows/recall.yml` の strict 評価で層 B の
  Recall@20 ゲートが未達）。オーナー方針として fixture パラメータ
  （`TEXT_KEYWORD_DROPOUT_PROB`・`VECTOR_KEYWORD_DROPOUT_PROB`・
  `VECTOR_DECOY_PROB` 等）を動かして通すことは禁止。本節は小規模段（SEARCH-1・
  クエリ展開の結線を前提にしない段）の engine 側調査を記録する。大規模段
  （SEARCH-2・クエリ展開結線）は別 Issue（#306）の管轄

**層 A 実測値のみ記載する（層 B の pass/fail・閾値は記載しない。
`.claude/rules/spec-confidentiality.md`）**

### チャネル別内訳（小規模段 400 件・QA 60 件・ceil20=202）

| 構成 | hits20 | Recall@20 |
| ---- | ------ | --------- |
| 密のみ（one-hot 内積 Top-20） | 151 | 0.7475 |
| 疎のみ（BM25 `search_within` Top-20） | 166 | 0.8218 |
| RRF 融合（現行） | 179 | 0.8861 |

融合は密・疎いずれの単体も下回らない（SEARCH-3 相当の非劣化は実測値の比較として
維持。一般不変条件としては追加しない。密単体・疎単体の固定値は
`crates/engine/tests/hybrid_recall.rs::measure_channel_recall20` で回帰
トラッキングする）。

### 支配要因: 同点グループと融合寄与の候補 id 依存

密チャネルは one-hot 和の内積（0/1/2 の整数スコア）のため、`pool_depth=200` の
プール内に非常に大きな同点グループが生じる（400 件コーパスで score>0 の文書は
平均 54 件程度で、残りはスコア 0 の同点）。疎チャネルも同一語構成・同一文書長では
BM25 スコアが一致するため、同様に同点グループが生じる。

`hybrid.rs::rrf_fuse`（内部の `accumulate_ranked`）は、[`CandidateHit`]/[`ScoredDoc`]
が定める順位契約（スコア降順・同点は候補 id 昇順）に従ってソート済みの列を受け取り、
1-based の順位に応じて `weight / (k_const + rank)` を加算して RRF スコアを計算する。
従来の実装は同点グループ内の各要素にも列に現れる位置（＝同点内では id 昇順）を
そのまま個別の順位として割り当てていたため、同点グループ内での融合寄与が
「候補 id の並び」という、RRF が本来意味を持たせたい「意味的な近さ」とは無関係な
鍵に依存していた。同点グループが競合する近傍順位を押しのけて正解を圏外へ追い出す
経路が、小規模段の Recall@20 の主要な劣化要因だった。

### 採用した改善: 同点グループへの平均順位割り当て

`accumulate_ranked` を「同点グループ（スコアが `f64::total_cmp` で等しい連続区間）
には区間の平均 1-based 順位を与える」方式へ変更した
（`crates/engine/src/hybrid.rs`）。

- 同点でない入力（実埋め込みの連続値スコア）では現行と完全同一の結果になる
  （区間長が常に 1 のため）
- 順位は `[1, len(items)]`（`len(items) <= pool_depth`）の部分区間に留まるため、
  ソフトブースト（TASK-111・PLAN-1）の `soft_boost_confirm_cap`／
  `soft_boost_loose_upper_bound` の上下限導出は不変
- 出力側のタイブレーク（スコア降順・id 昇順の安定ソート。
  `docs/design/rrf-tie-break-determinism.md`）とは独立な変更であり、その不変条件は
  維持したまま「同点グループの融合寄与が候補 id の並びに依存しない」性質を追加で
  得る（同点グループが `pool_depth` 内に収まる場合。詳細は
  `docs/design/rrf-tie-break-determinism.md` 参照）
- `RrfConfig` の公開 API・既定値（`k_const`・重み・`pool_depth`）は変更していない

### 不採用とした案

- **`k_const`／`pool_depth`／重み・BM25 パラメータの既定値変更**: いずれも
  `RrfConfig::default()` が表す spec 採用構成（等重み・pool 200・k 60）の範囲。
  診断のための掃引は行ったが、既定値の変更はオーナー判断事項のため本 Issue では
  採用しない
- **競争順位方式（同点グループ全体に区間先頭の順位を一律付与。いわゆる 1224 方式）**:
  小規模段では改善するが、大規模段の診断掃引では劣化した
- **平均逆数（mean-of-reciprocals。`(1/n) * Σ w/(k_const+r)` を同点グループの
  各要素へ一律付与）**: 平均順位方式のもう一つの標準的な代替だが、追加の掃引・
  再検証が必要なため本 Issue では見送り、平均順位方式（13 構成の掃引で検証済み）を
  採用する
- **密プールのスコア 0 除外**: 内積 0 は one-hot 合成コーパス固有の性質であり、
  一般の埋め込みでは「不一致」を意味しない

### 到達不能な正解（fixture の構造的難易度）

小規模段の正解文書 202 件のうち、密ベクトルがクエリ 2 語の両方を保持するのは
130 件、テキストが両方を保持するのは 138 件、**どちらの観測にも両語が残っていない
のは 22 件（10.9%）**。この 22 件は密・疎いずれの AND 信号でも到達不能であり、
「疎・密チャネルの lossy view」（本ドキュメント既存節）が課す構造的な難易度に
起因する。RRF 融合パイプライン側の改善では解消できない。

### オーナー報告事項

- RRF の同点時の順位規約（平均順位）は spec 採用構成（等重み・pool 200・k 60）の
  範囲外の実装判断である
- 改善後も到達不能な正解（小規模段 22/202）は fixture の lossy view 由来であり、
  層 B が依然 red の場合はコーパス生成条件の spec 側確定が必要になる可能性がある

## 実測結果

（`crates/engine/tests/hybrid_recall.rs`、層 A 2/2 pass。決定的コーパスのため
再現可能。hit 数は同テストのアサーションに固定済み。Issue #307 の RRF 同点順位
変更〔上記節参照〕により小規模段 171→179・大規模段 328→365／645→663 へ更新）

| 段 | 文書数 | QA 件数 | total_correct | ceil20 | hits20 | ceil100 | hits100 | Recall@20 | Recall@100 |
| -- | ------ | ------- | -------------- | ------ | ------ | ------- | ------- | --------- | ---------- |
| 小規模 | 400 | 60 | 202 | 202 | 179 | - | - | 0.8861 | - |
| 大規模 | 20,000 | 100 | 997 | 421 | 365 | 707 | 663 | 0.8670 | 0.9378 |

疎・密チャネルの lossy view（ドロップアウト・デコイ）により、いずれの段も
Recall@k が 1.0 未満の現実的な値になっている。QA 件数はいずれも重複除外前の
クエリ候補数（小規模 60・大規模 100）と一致しており、本コーパス規模では語ペアの
重複は発生していない（コーパス規模・語彙数が変わると重複除外により QA 件数が
`num_queries` を下回る可能性がある。`generate_qa_set` のドキュメント参照）。

大規模段のデバッグビルド実行時間はローカル実測で約 4.5 秒であり、PR CI
（`cargo test`）に含めても許容範囲と判断し、層 A の両テストとも `#[ignore]` に
していない。ただし `ubuntu-latest`（GitHub Actions ランナー）での実測はまだ行って
おらず、CI 実測後に実行時間が悪化していれば `#[ignore]` 側へ移し
`.github/workflows/recall.yml` 専用にする判断を再検討する
（`crates/engine/tests/hybrid_recall.rs::hybrid_recall_large_scale_regression` の
ドキュメンテーションコメントと同方針）。なお `.github/workflows/recall.yml` は
`pull_request` トリガを持たない（「2 層構成」参照。spec 機密保持が優先）ため、
PR ごとの実行コストは層 A（layer A の `cargo test` 分のみ）に限られる。

## 既知の制約・スコープ外

- **合成コーパスによる暫定測定**: 実コーパスでの評価は未了（TASK-106 と同種の制約）
- **密ベクトルの簡略化**: 「疎・密チャネルの lossy view」で述べた通り、one-hot
  AND 信号＋構造的ドロップアウト／デコイは埋め込みモデルの類似度分布（連続値・
  高次元の相関構造）を忠実には模倣しないため、実際の埋め込み品質の回帰検出には
  使えない。あくまで RRF 融合パイプライン自体（密・疎の統合・pool_depth・k_const の
  挙動）が、正解判定と検索特徴量が分離された非自明な入力に対しても機能することの
  回帰検出が目的
- **クエリ側は lossy view を適用しない**: ドキュメント側のみ非完全な観測にし、
  クエリは潜在集合から直接構成する簡略化のため、クエリ自体の embedding 品質劣化は
  対象外
- **fixture パラメータの経験的選定**: `TEXT_KEYWORD_DROPOUT_PROB`・
  `VECTOR_KEYWORD_DROPOUT_PROB`・`VECTOR_DECOY_PROB` は Recall を 1.0 未満の
  非退化な範囲に収めるために実験的に選んだ値であり、spec 由来の受け入れ基準では
  ない（層 B の閾値のみが spec 由来。「2 層構成」参照）
- **クエリ展開との統合測定**: SEARCH-2 の前提にはクエリ展開（PLAN-5 系、TASK-109
  以降）が含まれるが未実装のため、本ハーネスはハイブリッド検索単体（クエリ展開なし）
  の測定に留める
- Actions secrets（`HYBRID_RECALL_MIN_*`）の実値設定はマージ後のリポジトリ管理者
  作業（README「Recall 回帰ハーネスの repo secrets」参照。secret ↔ spec
  ポインタの対応表・設定手順は `docs/design/ci-gate-variables.md` に集約した。
  当初は Actions variables を使っていたが、`env:` ブロックのログ印字による
  漏えいを防ぐため secrets へ移行した（Issue #286）

[`SparseIndex::build`]: ../../crates/engine/src/sparse.rs
[`ParallelSearchProvider`]: ../../crates/engine/src/parallel_search.rs
[`strict_thresholds_required`]: ../../crates/engine/tests/hybrid_recall.rs
[`resolve_gate_threshold`]: ../../crates/engine/tests/hybrid_recall.rs
