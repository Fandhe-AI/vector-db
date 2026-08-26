# vector-db

Rust 製のローカルファースト・vector 特化クエリ DB の実装リポジトリです。「正解を含むデータ群を広く返す」広域検索（`recall` モード・既定）を設計思想の中心（差別化ポイント）とし、LLM のコンテキストとして渡す用途に最適化します。「正確なデータ 1 件のピンポイント抽出」（`precision` モード）への切り替えも提供します。

## 位置づけ

- **本リポジトリは public** です（rust-ai-library と同一方針）
- **仕様・ビヘイビア定義**: [vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec)（`docs/spec` に submodule 参照。**private リポジトリとして意図的に非公開を維持**する方針であり、アクセス権のない環境からは submodule を解決できません）
- Web API・MCP サーバーはいずれも別プロダクトとして本リポジトリのスコープ外です

## ステータス

実装は未着手です（ロードマップの着手判定待ち）。タスク定義は spec リポの [`05-tasks.md`](https://github.com/Fandhe-AI/vector-db-spec/blob/main/05-tasks.md)（TASK-66〜165・100 件）、マイルストーンは [`06-roadmap.md`](https://github.com/Fandhe-AI/vector-db-spec/blob/main/06-roadmap.md)（MS-1〜6）を参照してください。

## 実装方針（要点）

- **接続プロトコル**: PostgreSQL wire プロトコル v3 互換の**自作実装**（`pgwire` 等の外部ライブラリへ可能な限り依存しない）。psql・psycopg・node pg が無改造で接続可能なことを PoC-8 で実測済み
- **クエリ表層**: 標準クエリカタログ C1〜C5 を MVP とする vector 特化 SQL（C6 集計・C7 結合は拡張扱い）。LLM クエリプランニングは専用構文 `USING PLAN(...)` で SQL に露出
- **検索モード**: `recall`（広域・既定）／`precision`（ピンポイント抽出）の切り替えを提供。切替手段・実行契約の詳細は spec のビヘイビア定義（SQL-12・SEARCH-9/10・PLAN-11・TASK-161〜165）を参照
- **クレート構成**: `engine`（コアロジック: データロード・検索カーネル・認証・RLS）＋ `wire-server`（バイナリ）の workspace 構成（TASK-66 で雛形を構築済み。各機能の実装は後続タスク）
- **永続化**: `redb` ベース（単一ライタ・スナップショット読み取り。並行書き込み検証は MS-1 の TASK-144）
- **安全性**: RLS 相当のテナント境界・fail-closed のエラー契約（SQLSTATE 風 `wire_code`）
- **検索結果順序**: スコア順 Top-k・RRF 融合結果はいずれもスコア降順・同点は id 昇順で決定的（判断根拠は [`docs/design/rrf-tie-break-determinism.md`](docs/design/rrf-tie-break-determinism.md)）。ただし複数テナントを 1 バッチで扱うバッチ検索経路（`batch_search.rs`）では、同点タイブレークは常駐行列の行スロット昇順であり、行を `(tenant_id, id)` キー順（`Storage` の行キー順）で常駐行列へ渡すという事前条件のもとで `(tenant_id, id)` 昇順になる（単一テナント内では従来どおり id 昇順。CPU 経路・GPU 経路とも同一）
- **依存最小方針**: 依存の追加・更新は必ずユーザー承認を経て行い、`=x.y.z` 完全固定で管理する
- **バッチ検索の GPU 経路**: 一括インデクシング専用のバッチ検索（TASK-128〜130）は `wgpu`（=30.0.1・依存追加はオーナー承認済み〔2026-08-26〕）による実 GPU バックエンドを持ち、初期化失敗・実行時エラー時は CPU-SIMD 経路へ fail-closed に縮退する（詳細: [`docs/design/gpu-batch-wgpu-enablement.md`](docs/design/gpu-batch-wgpu-enablement.md)）。単発クエリ経路は引き続き CPU-SIMD のみ

詳細なビヘイビア（106 件・12 領域）は spec リポの [`04-behavior/`](https://github.com/Fandhe-AI/vector-db-spec/tree/main/04-behavior) を唯一の正（SSOT）とします。

## 開発環境構築

```bash
git clone git@github.com:Fandhe-AI/vector-db.git
cd vector-db
make setup   # サブモジュール → rustup → lefthook（git hooks）を一括構築
```

`docs/spec`（`vector-db-spec`）は private リポジトリのため、アクセス権のない環境では submodule 取得が失敗します（`make setup` は警告のみで継続します）。実装コードのビルド・テストは `docs/spec` 抜きでも成立するよう維持します。

### タスクランナー（Makefile）

| コマンド | 内容 |
| -------- | ---- |
| `make setup` | 開発環境の一括構築（submodule → rustup → lefthook） |
| `make ci` | CI（`.github/workflows/ci.yml`）と同等のチェックをローカル一括実行 |
| `make lint-docs` | ドキュメント／設定ファイル系 lint（markdownlint・yamllint・editorconfig-checker・commitlint） |
| `make fmt` / `make fmt-check` / `make lint` / `make test` / `make deny` | Rust 系チェック（workspace 追加により有効化済み） |
| `make docker-build` / `make docker-shell` / `make docker-ci` | Docker による環境非依存の開発・検証（`compose.yaml` 参照） |
| `make bench-simd` / `make bench-c1` / `make recall-regression` / `make precision-regression` | 時間依存・spec 閾値依存の回帰チェック（`ci` には含めない。`.github/workflows/bench.yml`・`recall.yml` から実行。`precision-regression` は目標値未確定のため `recall.yml` へ未接続。詳細は下記「`precision` 評価ハーネス」参照） |
| `make precision-report` | TASK-163 の判断材料レポート・パラメータ感度スイープ（実測値を標準出力へ出すため**ローカル専用**。CI・GitHub Actions からは実行しない） |
| `make e2e-three-client` | TASK-73（WIRE-1）／TASK-165（SQL-12・SEARCH-9）／TASK-168（SQL-13・SQL-14）実 `psql`／`psycopg`／`pg` クライアント統合テスト（`ci` には含めない opt-in。要 `psql`・`python3`+`psycopg`・`node`+`pg`。`PSQL_BIN`/`PYTHON_BIN`/`NODE_BIN` で上書き可） |

ターゲット一覧は `make help` で確認できます。

### wire-server の起動（TASK-73）

```bash
cargo run -p wire-server -- --users <ユーザーストアのパス> --db <redb ファイルのパス> [--bind 127.0.0.1:5432]
```

`--users`・`--db` はいずれも必須です（省略時は匿名ログイン・匿名 DB を暗黙生成せず
fail-closed で起動を拒否します）。`--bind` 省略時は `127.0.0.1:5432`。psql・
psycopg・node pg から無改造で cleartext password 認証つき接続できます
（詳細: `docs/design/three-client-e2e-harness.md`）。

### 回帰ベンチの repo variables（TASK-127）

`.github/workflows/bench.yml`（`workflow_dispatch` + 週次 `schedule`。毎週月曜 03:00 UTC）は `BENCH_MAX_P95_MS`（p95 レイテンシ上限・ミリ秒）・`BENCH_MIN_RECALL`（Recall@k 下限）・`BENCH_BATCH_MAX_DEGRADATION_PCT`（バッチ経路の劣化率上限・TASK-130）・`BENCH_MAX_CONTRAST_RATIO`（対照エンジンに対する p95 レイテンシ比率〔被検/対照〕の上限・TASK-127 CORE-5）をリポジトリの Actions variables（`vars.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。マージ後、リポジトリ管理者が以下を実行して設定してください。

```bash
gh variable set BENCH_MAX_P95_MS
gh variable set BENCH_MIN_RECALL
gh variable set BENCH_BATCH_MAX_DEGRADATION_PCT
gh variable set BENCH_MAX_CONTRAST_RATIO
```

形式は以下のとおりです（値は上記のとおり本リポジトリには記載しません）。

| variable | 形式 |
| -------- | ---- |
| `BENCH_MAX_P95_MS` | 正の整数（単位: ms） |
| `BENCH_MIN_RECALL` | `(0.0, 1.0]` の浮動小数点 |
| `BENCH_BATCH_MAX_DEGRADATION_PCT` | 0 以上の有限浮動小数点 |
| `BENCH_MAX_CONTRAST_RATIO` | 0 より大きい有限浮動小数点 |

未設定のまま実行すると `crates/engine/benches/simd_bench.rs`／`batch_bench.rs`／`contrast_bench.rs` が fail-closed で判定不能として非ゼロ終了します（デフォルト値は持ちません）。

CORE-5（対照エンジンとの p95 レイテンシ比較。ポインタ: `docs/spec/04-behavior/core-engine.md` CORE-5）は usearch の総当たり `exact_search`（`contrast-bench` feature 限定の optional 依存。`crates/engine/Cargo.toml`）を対照エンジンとして接続済みです（TASK-127・Issue #176。クレート採用と公開境界はオーナー承認済み〔2026-08-26〕）。`contrast_bench.rs` が被検（`ParallelSearchProvider`）と対照エンジンを同一データ・同一クエリで interleaved A/B 実行し、両者の p95 レイテンシ比率（被検/対照）が `BENCH_MAX_CONTRAST_RATIO` 以下であることを判定します。CORE-3/CORE-4（`simd_bench.rs`）とは独立した bench-contrast ジョブとして既定ゲート実行され、`BENCH_MAX_CONTRAST_RATIO` 未設定・不正値は fail-closed で非ゼロ終了します（旧 `BENCH_CORE5` repo variable による opt-in 方式は撤去済み）。閾値の具体値は spec が SSOT のため本リポジトリには記載せず、bench の標準出力にも出しません。`contrast-bench` feature は `make lint`／`make test`（lefthook pre-push 含む）が `--all-features` で実行するため、`make bench-contrast` に限らずこれらのローカル実行・CI でも usearch の C++ ビルドが走ります。C++17 コンパイラが必要です（GitHub ホステッド `ubuntu-latest` には同梱済み。ローカルに C++17 コンパイラがない環境では `make lint`／`make test`／`make ci` が失敗します）。

同様に CORE-6（GPU vs CPU-SIMD）・CORE-16（f16 常駐 vs f32 常駐）は Issue #178 で追跡中です。実 GPU バックエンド（`gpu_batch.rs`）に加え、CORE-6 は `benches/batch_bench.rs` の A/B 実測ゲート（GPU 経路 vs CPU-SIMD 経路）へ配線済みです。GitHub ホステッド runner に GPU が無いこと・閾値が spec SSOT であることから `BENCH_CORE6` repo variable による opt-in 方式を維持します（未設定＝既定で対象外。opt-in 時は短縮率下限 `BENCH_CORE6_MIN_IMPROVEMENT_PCT` も必要で、未設定なら fail-closed）。CORE-16 は GPU 側の f32 常駐対照経路が未実装のため引き続き測定不能で（Issue #234 で追跡）、`BENCH_CORE16` を opt-in するとその理由とともに `pass=false` を報告します。`schedule` トリガ（週次）は #168 で再追加済みです。variables 未設定のまま週次 run が実行された場合は fail-closed で red になります（false green にはなりません）。GitHub ホステッド runner には GPU が無いため、CORE-6/16 の実測には GPU 搭載ホストでの手動実行が必要です。

### C1 p95 専有環境再測定（TASK-83）

`make bench-c1`（`crates/engine/benches/sql_c1_bench.rs`）は SQL 表層（`EngineCore::execute_sql`）経由の C1（純粋 Top-k）p95 を測定します。閾値は SQL-1 専用の `BENCH_SQL_C1_MAX_P95_MS`（正の整数・ms）・`BENCH_SQL_C1_MIN_RECALL`（`(0.0, 1.0]` の浮動小数点）から注入します。上記 TASK-127 の `BENCH_MAX_P95_MS`／`BENCH_MIN_RECALL` は `SearchProvider` 単体（CORE-3・SEARCH-4・CORE-4）の基準であり SQL-1 とは spec 上の出所が異なるため、流用せず別 variable として分離しています（流用すると緩い側で false green・厳しい側で false red になります）。値そのものは本リポジトリには記載しません。

```bash
gh variable set BENCH_SQL_C1_MAX_P95_MS
gh variable set BENCH_SQL_C1_MIN_RECALL
```

未設定のまま実行すると `sql_c1_bench.rs` が fail-closed で判定不能として非ゼロ終了します（デフォルト値は持ちません）。`.github/workflows/bench.yml` の `bench-c1` ジョブは `workflow_dispatch` 限定で、`bench-simd`／`bench-batch` と異なり週次 `schedule` には含めません（GitHub ホステッド runner が専有環境ではないため。詳細は `docs/design/c1-p95-dedicated-env-reverification.md` 参照）。

`BENCH_DEDICATED_ENV=1` は Conditional Go 条件7（専有環境での p95 再測定）の判定を有効化する opt-in フラグです。他プロセスと CPU/IO を共有しない専有環境で実行する場合にのみ設定してください（自動検出はできないため運用者の明示宣言に限ります）。未設定（既定）の場合、p95・Recall の pass/fail 自体は出力されますが、条件7 の判定対象からは明示的に除外されます。

```bash
BENCH_SQL_C1_MAX_P95_MS=<spec 値> BENCH_SQL_C1_MIN_RECALL=<spec 値> BENCH_DEDICATED_ENV=1 make bench-c1
```

### Recall 回帰ハーネスの repo variables（TASK-104）

`.github/workflows/recall.yml`（`workflow_dispatch` + 週次 `schedule`。毎週月曜 04:00 UTC。`pull_request` トリガは意図的に持たせていません）は `crates/engine/tests/hybrid_recall.rs` の層 B（`#[ignore]` 付き閾値ゲート）を `make recall-regression` 経由で実行し、`HYBRID_RECALL_MIN_R20_SMALL`（小規模段 Recall@20 下限）・`HYBRID_RECALL_MIN_R20_LARGE`（大規模段 Recall@20 下限）・`HYBRID_RECALL_MIN_R100_LARGE`（大規模段 Recall@100 下限）を GitHub Environment `recall-gate` の Actions variables（`vars.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。各下限値は `hits@k / Σmin(k,正解集合サイズ)`（正解集合が k 件を超えるクエリがあっても頭打ちにならない、達成可能な理論上限に対する到達率）というスケールで設定してください。マージ後、リポジトリ管理者が以下を実行して設定してください（`gh api` または Settings > Environments）。

> [!WARNING]
> **workflow を一度でも実行する前に、必ず deployment branch policy（`main` のみ）付きで Environment `recall-gate` を作成してください。** 未作成のまま `recall-regression` job（`environment: recall-gate` を指定）が走ると、GitHub は branch policy なしの environment を自動作成してしまい、`main` 以外の ref からもアクセスできる状態になります。これは本 workflow が `environment` 指定でブランチ保護（実行境界）を作っている前提を崩し、`HYBRID_RECALL_MIN_*`（spec 由来の非公開閾値）が任意 ref から漏えいしうる状態に戻ってしまいます。**本リポジトリでは Environment `recall-gate` は作成済みです**（下記手順どおり branch policy `main` 付き）。

1. Environment `recall-gate` を作成し、deployment branch policy で `main` のみに制限する（上記警告参照。**workflow の初回実行より前に行うこと**）
2. その environment に閾値 variables を設定する:

   ```bash
   gh variable set HYBRID_RECALL_MIN_R20_SMALL --env recall-gate
   gh variable set HYBRID_RECALL_MIN_R20_LARGE --env recall-gate
   gh variable set HYBRID_RECALL_MIN_R100_LARGE --env recall-gate
   ```

3. `RERANK_RECALL_MIN_R20_LARGE`／`RERANK_RECALL_MIN_R20_IMPROVEMENT`（下記 TASK-108 参照）も同じ Environment `recall-gate` に設定する（strict モードは 5 変数すべてを必須とするため）
4. `gh workflow run recall.yml --ref main` で **main を ref に指定して** 本 workflow を手動実行し、`gh run watch` で `recall-regression` job が **skip ではなく実際に実行され**、strict モード（下記）のもとで 5 変数すべてが正しく評価されて green になることを確認する（main 以外の ref を指定すると job が skip されて green に見えるため注意）
5. 疎通確認が済めば、`schedule` トリガ（週次・#168 で再追加済み）により以降は自動実行されます

**variables を設定するとゲートが有効化されます。** ローカルの `make recall-regression`（`HYBRID_RECALL_REQUIRE_THRESHOLDS` を注入しない）で未設定（GitHub Actions では空文字列に解決される repo variable も含む）のまま実行すると、`crates/engine/tests/hybrid_recall.rs` は「ゲート未設定＝明示的に対象外」を出力して成功終了します（fail-closed で塞ぐのは、設定済みの値が非数値・範囲外だった場合のみ）。

**`recall.yml` は strict モードで実行されます**: `recall.yml` は Run step で `HYBRID_RECALL_REQUIRE_THRESHOLDS=1` を常に注入します。この strict モードでは `HYBRID_RECALL_MIN_*` の未設定（environment 作成漏れ・variable 名の誤り・variable の誤削除を含む）も非数値・範囲外と同様に fail-closed でテスト失敗とします。strict モードなしだと「一度も評価していない run」が「基準を満たした run」と同じ green になってしまうため（`crates/engine/tests/hybrid_recall.rs::resolve_gate_threshold` 参照。PR #147 codex-review P1 継続指摘対応）。**`schedule`（週次・#168 で再追加済み）が無人実行で正しく評価されるよう、マージ後は必ず `workflow_dispatch` で strict モードのもとで疎通確認してください**（手順 4 参照）。

**`pull_request` トリガを持たせない理由（spec 機密保持が優先）**: `pull_request` で起動する job は PR 側の untrusted なコード（Makefile・テストコード含む）を checkout して実行するため、もし層 B を PR トリガにすると、PR がコードを書き換えて `HYBRID_RECALL_MIN_*`（spec 由来の非公開閾値）を標準出力へ書き出すだけで public な Actions ログから spec の数値基準を取得できてしまいます（`.claude/rules/spec-confidentiality.md` の P0 違反）。そのため層 B は既定ブランチの trusted なコードのみが走る `workflow_dispatch`・`schedule`（週次。#168 で再追加済み）に限定し、**PR のマージ判定は層 A（spec 数値を含まない public な固定値回帰。`.github/workflows/ci.yml` の `cargo test` で PR ごとに常時実行）が担う**、という役割分担にしています（`docs/design/hybrid-recall-regression.md` 参照）。決定的コーパスでの回帰トラッキング自体（層 A・固定値アサーション）は `make ci`（`cargo test`）に含まれており、こちらは repo variables 不要です。

**閾値 variables は repo レベルではなく Environment `recall-gate` に置きます**: `workflow_dispatch` は本来任意の ref を選んで起動でき、選択した ref の workflow YAML がそのまま実行されます。そのため `if: github.ref == 'refs/heads/main'`・`checkout ref: main` のような YAML 内の条件だけでは実行境界になりません——write 権限者が別ブランチでこのガードを外した `recall.yml` を push して `workflow_dispatch` すれば、そのブランチの YAML が実行されてしまうためです。加えて repo レベルの Actions variables はどのブランチのどの workflow からも参照できるため、YAML 内の条件式では閾値の参照そのものを防げません。そこで閾値は repo レベルではなく Environment `recall-gate`（deployment branch policy で `main` のみに制限）の variables として設定し、`recall-regression` job に `environment: recall-gate` を指定します。main 以外の ref から起動した run は environment `recall-gate` にアクセスできないため、別ブランチの改変 YAML から `if`／`checkout ref` を外して `workflow_dispatch` したとしても閾値を取得できません。`if: github.ref == 'refs/heads/main'`・`checkout ref: main` は environment 保護に対する defense-in-depth として維持しています。

### リランキング効果測定 Recall 閾値ゲートの repo variables（TASK-108）

`.github/workflows/recall.yml` の同一 `recall-regression` job は、上記に続けて `crates/engine/tests/rerank_recall.rs` の層 B（`#[ignore]` 付き閾値ゲート）も `make rerank-regression` 経由で実行し、`RERANK_RECALL_MIN_R20_LARGE`（リランキング後の最終 Recall@20 の絶対下限）・`RERANK_RECALL_MIN_R20_IMPROVEMENT`（baseline＝リランキングなしからの改善幅の下限）を同じ Environment `recall-gate` の Actions variables（`vars.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。設計・実測経緯は `docs/design/rerank-recall-regression.md` を参照してください。Environment `recall-gate` は上記手順ですでに作成済みのため、追加で行うのは variables の設定のみです。`recall.yml` は `workflow_dispatch` / `schedule` の両方で strict モード（`RERANK_RECALL_REQUIRE_THRESHOLDS=1`）で実行されるため、上記 5 変数がすべて揃っていない場合は fail-closed でテスト失敗になります。

```bash
gh variable set RERANK_RECALL_MIN_R20_LARGE --env recall-gate
gh variable set RERANK_RECALL_MIN_R20_IMPROVEMENT --env recall-gate
```

挙動（opt-in・strict モード・`pull_request` 非対応の理由）は上記「Recall 回帰ハーネスの repo variables」と同一です。ローカルの `make rerank-regression`（`RERANK_RECALL_REQUIRE_THRESHOLDS` を注入しない）で未設定のまま実行すると「ゲート未設定＝明示的に対象外」を出力して成功終了し、`recall.yml` からの実行（`RERANK_RECALL_REQUIRE_THRESHOLDS=1` を常時注入）では未設定も fail-closed でテスト失敗とします。

### `precision` 評価ハーネス（TASK-163）

`crates/engine/tests/precision_eval.rs` は `precision` モード（TASK-162）の
SEARCH-10 の評価指標を、決定的合成コーパス（正解不在クエリを含む）上で実測する
評価ハーネスです。設計判断の記録は `docs/design/precision-eval-regression.md`
を参照してください（指標の定義・実測値・パラメータ感度は spec 側で管理します）。

- 層 A（`cargo test -p engine --test precision_eval`。`make ci` 対象）: 決定的コーパス
  上で評価を通しで実行し、構造不変条件と測定の決定性のみを検査します（指標の実測値は
  アサートも出力もしません。品質の回帰判定は層 B が担います）。
- 層 B（`make precision-regression`）: 閾値ゲートのみを実行し、指標名と pass/fail
  だけを出力します（閾値の数値も実測値も出力しません）。`PRECISION_EVAL_MIN_TOP1_ACC`・
  `PRECISION_EVAL_MIN_MRR10`・`PRECISION_EVAL_MAX_FALSE_RETURN` 環境変数
  と比較して判定します。未設定なら評価は実行しつつ判定をスキップし「ゲート未設定＝
  明示的に対象外」として成功終了、`PRECISION_EVAL_REQUIRE_THRESHOLDS=1`（strict
  モード）では未設定も fail-closed でテスト失敗とします。非数値・範囲外は常に
  fail-closed です。
- 判断材料レポート・感度スイープ（`make precision-report`。**ローカル専用**）:
  hybrid・dense 双方の指標（`precision_eval_report`）と `PrecisionPolicy` の閾値を
  差し替えたパラメータ感度スイープ（`precision_eval_policy_sweep`。hybrid 系列・
  dense 系列）を出力します。実測値を標準出力へ出すため、public runner で動く CI・
  `recall.yml` からは実行しません（`.claude/rules/spec-confidentiality.md`）。
- **`.github/workflows/recall.yml` への接続は行っていません**: TASK-163 のスコープは
  実測・判断材料の提示までであり目標値の確定は含まないため、上記の
  `PRECISION_EVAL_*` 環境変数は Environment `recall-gate` にまだ設定していません。
  目標値が確定したのち、`RERANK_RECALL_MIN_*` 等と同様に `recall-gate` の Actions
  variables として設定し、`recall.yml` の `recall-regression` job に
  `PRECISION_EVAL_REQUIRE_THRESHOLDS=1` 付きの step を追加してください。

## ライセンス

MIT OR Apache-2.0 のデュアルライセンスです（[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE)）。
