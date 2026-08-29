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
  当初は variables を使っていたが Issue #286 で secrets へ移行）・TASK-110〜113
  （`docs/design/query-planning-recall-regression.md`。クエリ展開クライアント・
  決定的スタブ `LlmClient`・展開あり Recall 回帰の先行実装）・Issue #306
  （大規模段層 B へのクエリ展開結線）

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
- 前提: 親 Issue #301。大規模段（SEARCH-2・クエリ展開結線）は別 Issue（#306）の管轄

**層 A・層 B とも実測値・閾値・pass/fail は本ドキュメントにも public テストにも
記録しない（`.claude/rules/spec-confidentiality.md`）**。密単体・疎単体・融合後の
Recall@20 比較（受け入れ条件 1）は `crates/engine/tests/hybrid_recall.rs::measure_channel_recall20`
を使い、融合が両チャネル単体のいずれも下回らない（＝ SEARCH-3 相当の非劣化）
ことを実測値どうしの関係アサーションとしてのみ回帰トラッキングする。

### 原因調査（受け入れ条件 1・3・4）

原因調査・検討した代替方式・オーナー判断待ち事項の詳細は非公開の内部分析に
あたるため本ドキュメントには記載しない。production の融合挙動
（`hybrid.rs::accumulate_ranked`・`rrf_fuse`）・既定値（`k_const`・`pool_depth`・
重み・BM25 パラメータ）はいずれも本 Issue の範囲では変更していない。
受け入れ条件・判断事項は spec のビヘイビア定義（SEARCH-1・SEARCH-3。
`docs/spec/04-behavior/search.md`）を参照。

## Issue #310: engine 側改善（同点順位規約・境界同点グループ）

- 対応: TASK-84・TASK-103（`docs/spec/05-tasks.md`）
- 対象ビヘイビア: SEARCH-1・SEARCH-3（`docs/spec/04-behavior/search.md`）
- 前提: 親 Issue #301。先行: #306（大規模段へのクエリ展開結線）・#307（小規模段の
  engine 側原因調査。上記節）

Issue #307 の調査（同節参照）で「原因調査対象」として残していた RRF 融合の同点
順位規約を確定した。`hybrid.rs::RrfConfig` に同点順位規約 `TieRank`
（`GroupEnd`（既定。同点グループ全員へグループ末尾の順位を割り当てる
modified competition ranking）・`Positional`（従来挙動。撤回用に残置）の
2 バリアント）を追加し、`accumulate_ranked` の同点処理を `Positional` から
`GroupEnd` へ切り替えた。あわせて `hybrid_search_boosted` の密プール取得を
`fetch_k`（`pool_depth` の最大 2 倍。`checked_mul`・`MAX_POOL_DEPTH`・可視集合の
大きさで有界化）へ拡張し、`pool_depth` 境界が同点グループの途中を切る場合に
グループ全体を含める（境界が取得済み範囲内で確定できない場合は決定的・id 非依存に
そのグループを丸ごと除外する）`complete_boundary_tie_group` を追加した。

**採用規約と不採用案**: 位置順位（従来挙動。同点グループが大きいほど id の小さい
候補が根拠なく高順位を得る id 依存バイアスがある）に対し、グループ末尾順位
（`GroupEnd`）・グループ先頭順位（competition ranking）・平均順位（fractional
ranking）を層 A の固定値（下記「実測結果」）で比較し、`GroupEnd` を採用した
（先頭順位は id 非依存性は満たすが到達率が位置順位を下回る場合があり、平均順位は
非整数の順位を扱う実装コストに見合う改善幅がなかった）。BM25（疎チャネル）の
`k1`/`b` の変更は不採用（既定値からの変更は一般性に乏しい）。密プール境界の
同点グループ完全化は、`pool_depth` を跨ぐ同点グループが id 依存の切り詰めで
一部だけ取り込まれる問題を、決定的（完全に含めるか完全に除外するかの二択。
2 度目の provider 呼び出しはしない）に解消する。当初は疎チャネル（BM25 スコアは
連続値のため同点が実質発生しない）を対象外としていたが、codex-review 指摘
（PR #320）により同一語頻度・同一文書長の文書は同一スコアになりうると判明した
ため、密側と同じ `complete_boundary_tie_group`（汎用化した
`complete_boundary_tie_group_by`）を疎側（`search_within` の拡張取得結果）にも
適用する形へ改めた（`crates/engine/src/hybrid.rs`）。`rerank.rs` の RRF 型融合への
`TieRank` 適用は引き続き本 Issue の対象外（フォローアップ候補）。

**フィクスチャ非変更**: `TEXT_KEYWORD_DROPOUT_PROB`・`VECTOR_KEYWORD_DROPOUT_PROB`・
`VECTOR_DECOY_PROB` を含む fixture パラメータ・seed・規模定数は本 Issue でも
変更していない（Issue #310 の受け入れ条件 2）。層 B の pass/fail・実測値・
閾値・導出係数は本ドキュメントにも public テストにも記録しない（受け入れ条件 3・
`.claude/rules/spec-confidentiality.md`）。

## 実測結果

（`crates/engine/tests/hybrid_recall.rs`、層 A 2/2 pass。決定的コーパスのため
再現可能。hit 数は同テストのアサーションに固定済み。Issue #310 実装後の値へ
再確定済み）

| 段 | 文書数 | QA 件数 | total_correct | ceil20 | hits20 | ceil100 | hits100 | Recall@20 | Recall@100 |
| -- | ------ | ------- | -------------- | ------ | ------ | ------- | ------- | --------- | ---------- |
| 小規模 | 400 | 60 | 202 | 202 | 182 | - | - | 0.9010 | - |
| 大規模 | 20,000 | 100 | 997 | 421 | 385 | 707 | 653 | 0.9145 | 0.9236 |

（Issue #310 実装前の値: 小規模 `hits20=171`（Recall@20=0.8465）・大規模
`hits20=328, hits100=645`（Recall@20=0.7791, Recall@100=0.9123）。`ceil20`/`ceil100`/
`total_correct`（正解集合の理論上限・総数）は fixture 非変更のため不変で、
`GroupEnd`＋境界同点グループ完全化により両段とも到達率が改善した）

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

### クエリ展開の結線（Issue #306）

大規模段の層 B（`hybrid_recall_large_scale_threshold_gate`）は、TASK-110〜113
（`docs/design/query-planning-recall-regression.md`）で確立した決定的スタブ
`LlmClient`（`query_planning_recall.rs::MockLlmClient` と同一実装。`tests/`
直下は独立 test crate で共有モジュールを持たないため `hybrid_recall.rs` へ複製）を
`EngineCore::plan_query`（TASK-110。`with_query_planner`・辞書スナップショット
〔`dictionary_snapshot` → `render_prompt_prefix`〕を経由する production のクエリ展開
結線）に通した展開ありクエリで Recall@20・Recall@100 を測定する。これにより、
SEARCH-2 が前提とする「クエリ展開あり」の測定条件に層 B の構成を揃えた。

この結線は PR #308 で 2 段階の codex-review P1 指摘対応を経て確立した:

1. 当初、層 B は QA セットを `direct` カテゴリ（コーパス語彙 `kw_XXXX` に一致する
   クエリ）のまま展開経路へ渡していたが、`MockLlmClient` はその形式の語を無変換で
   通すため、展開ありの実測値が展開なしと構造的に一致してしまい、展開結線の
   欠落・破損を検出できない問題があった（1 件目の指摘）。
   `hybrid_recall.rs::to_intent_query` でクエリテキストのみを `intent` 形
   （`syn_XXXX` 形式。コーパス語彙とは一致しない）へ書き換えたうえで展開経路に
   通すよう変更し、`MockLlmClient` の同義語写像（展開結果を検索入力へ反映する
   経路）を経由して初めて Recall が出る非恒等なゲートにした。
2. しかし (1) の時点では、展開経路自体（`hybrid_recall.rs::
   expand_and_reconstruct_with`）が `query_planner::render_full_prompt` →
   `LlmClient::complete` → `query_planner::parse_expansion` を固定接頭辞 `""` で
   直接呼ぶ手組みのままだったため、`EngineCore::plan_query`・`with_query_planner`・
   辞書スナップショットという production の結線自体が欠落・破損しても検出できない
   （2 件目の指摘・本件）。展開経路を `hybrid_recall.rs::PlannerFixture`（`path`/
   `body` 各 1 行のみの最小テーブルを持つ `EngineCore`。検索対象コーパスとは無関係で
   `plan_query` を呼ぶためだけの器）経由の `EngineCore::plan_query` へ置き換え、
   辞書スナップショットに含めたセンチネル値（`hybrid_recall.rs::
   DICTIONARY_WIRING_SENTINEL_PATH`）が展開時のプロンプトに現れない場合は
   `MockLlmClient::complete` が `Err(PlanError::InvalidResponse)` を返して展開自体を
   失敗させるよう変更した。

これにより、(1) の同義語写像と (2) の `plan_query`/`with_query_planner`/辞書
スナップショット結線のいずれが壊れても Recall 崩壊・ゲート失敗する二重の非恒等
ゲートになった。正解集合（`correct`）は変更しないため、既存の
`HYBRID_RECALL_MIN_R20_LARGE`/`HYBRID_RECALL_MIN_R100_LARGE`（Actions secrets 由来）
の較正・意味は変わらない（結線が壊れていなければ実測値は従来の `direct` 経路と
一致する）。再埋め込み（`plan_and_embed_query`）・`USING PLAN` SQL 表層は対象外の
まま（密チャネルの再構成は既存の `one_hot_sum` 契約を維持）。層 A
（`hybrid_recall_large_scale_regression`）は従来どおり `direct` 形クエリのままの
展開あり経路（`QuerySource::Expanded`。同じく `PlannerFixture` 経由）を測定対象と
し、展開なしとの hits/ceil 完全一致をパススルー回帰ガードとして維持する——この
等式が崩れた場合は展開パーサ・スタブ・再構成経路のいずれかの回帰を意味する（層 B
と異なり QA を書き換えないため、この層 A の等式・変更対象は無変更）。小規模段の
層 B・層 A の主測定は引き続き展開なし（`QuerySource::Baseline`）で行う。fixture
パラメータ・seed・規模定数（`LARGE_*`/`SMALL_*`）は変更していない。

層 B の pass/fail・実測値は本ドキュメントに記載しない（「実測値の既定非出力
（Issue #303）」節と同方針）。TASK-113（PLAN-3）が定義する `intent` カテゴリ
（言い換え語彙の QA セット）自体の大規模測定は、`to_intent_query` による疑似
`intent` 形クエリ（正解集合・fixture は `direct` カテゴリ由来のまま）とは別物であり
本結線でも対象外のまま（下記「既知の制約・スコープ外」参照）。

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
- **クエリ展開との統合測定**: 大規模段の層 B は Issue #306（PR #308 で `to_intent_query`
  による非恒等化・`PlannerFixture` 経由の `EngineCore::plan_query` 結線まで対応済み）
  で決定的スタブ `LlmClient` による展開ありの経路へ結線済み（上記「クエリ展開の結線
  （Issue #306）」節参照）。ただし QA の正解集合・
  fixture は `direct` カテゴリ由来のままで、TASK-113（PLAN-3）が定義する `intent`
  カテゴリ（言い換え語彙の QA セット）自体での大規模測定は本ハーネスのスコープ外の
  まま（フォローアップ候補。Issue 起票はユーザー判断待ち）
- Actions secrets（`HYBRID_RECALL_MIN_*`）の実値設定はマージ後のリポジトリ管理者
  作業（README「Recall 回帰ハーネスの repo secrets」参照。secret ↔ spec
  ポインタの対応表・設定手順は `docs/design/ci-gate-variables.md` に集約した。
  当初は Actions variables を使っていたが、`env:` ブロックのログ印字による
  漏えいを防ぐため secrets へ移行した（Issue #286）

[`SparseIndex::build`]: ../../crates/engine/src/sparse.rs
[`ParallelSearchProvider`]: ../../crates/engine/src/parallel_search.rs
[`strict_thresholds_required`]: ../../crates/engine/tests/hybrid_recall.rs
[`resolve_gate_threshold`]: ../../crates/engine/tests/hybrid_recall.rs
