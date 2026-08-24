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
- **依存最小方針**: 依存の追加・更新は必ずユーザー承認を経て行い、`=x.y.z` 完全固定で管理する

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
| `make bench-parallel` / `make recall-regression` | 時間依存・spec 閾値依存の回帰チェック（`ci` には含めない。`.github/workflows/bench.yml`・`recall.yml` から実行） |

ターゲット一覧は `make help` で確認できます。

### 回帰ベンチの repo variables（TASK-127）

`.github/workflows/bench.yml`（`workflow_dispatch` のみ。理由は後述）は `BENCH_MAX_P95_MS`（p95 レイテンシ上限・ミリ秒）と `BENCH_MIN_RECALL`（Recall@k 下限）をリポジトリの Actions variables（`vars.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。マージ後、リポジトリ管理者が以下を実行して設定してください。

```bash
gh variable set BENCH_MAX_P95_MS
gh variable set BENCH_MIN_RECALL
```

未設定のまま `workflow_dispatch` を実行すると `crates/engine/benches/parallel_bench.rs` が fail-closed で判定不能として非ゼロ終了します（デフォルト値は持ちません）。

CORE-5（対照エンジンとの中央値比較）は対照エンジンクレートの導入がユーザー承認待ちのため未接続です（TASK-127。`.claude/rules/dependency-policy.md`。Issue #35 で追跡中）。CORE-5 の判定は `BENCH_CORE5` repo variable による opt-in 方式です。

- 未設定（既定）: CORE-5 は「対象外」として標準出力へ明示され、合否判定には含まれません。CORE-3（p95 レイテンシ）・CORE-4（Recall@k）のみで合否を返します
- `gh variable set BENCH_CORE5 1` を設定: CORE-5 を判定対象に含め、未接続＝判定不能を fail-closed として扱います（非ゼロ終了）

対照エンジン接続がまだ完了していない段階での定期実行は誤検出・運用負担のリスクがあるため、`bench.yml` は schedule トリガを意図的に外し `workflow_dispatch`（手動実行）のみとしています。CORE-5 接続後、bench.yml 冒頭コメントの手順に従って schedule トリガを再度追加し、`BENCH_CORE5=1` を既定で有効化してください。

### Recall 回帰ハーネスの repo variables（TASK-104）

`.github/workflows/recall.yml`（`workflow_dispatch` ＋ 週次 `schedule`。`pull_request` トリガは意図的に持たせていません）は `crates/engine/tests/hybrid_recall.rs` の層 B（`#[ignore]` 付き閾値ゲート）を `make recall-regression` 経由で実行し、`HYBRID_RECALL_MIN_R20_SMALL`（小規模段 Recall@20 下限）・`HYBRID_RECALL_MIN_R20_LARGE`（大規模段 Recall@20 下限）・`HYBRID_RECALL_MIN_R100_LARGE`（大規模段 Recall@100 下限）を GitHub Environment `recall-gate` の Actions variables（`vars.*`）から注入します。値そのもの（spec 由来の数値基準）は本リポジトリには記載しません。各下限値は `hits@k / Σmin(k,正解集合サイズ)`（正解集合が k 件を超えるクエリがあっても頭打ちにならない、達成可能な理論上限に対する到達率）というスケールで設定してください。マージ後、リポジトリ管理者が以下を実行して設定してください（`gh api` または Settings > Environments）。

1. Environment `recall-gate` を作成し、deployment branch policy で `main` のみに制限する
2. その environment に閾値 variables を設定する:

   ```bash
   gh variable set HYBRID_RECALL_MIN_R20_SMALL --env recall-gate
   gh variable set HYBRID_RECALL_MIN_R20_LARGE --env recall-gate
   gh variable set HYBRID_RECALL_MIN_R100_LARGE --env recall-gate
   ```

**variables を設定するとゲートが有効化されます。** 未設定（GitHub Actions では空文字列に解決される repo variable も含む）のまま実行すると、`crates/engine/tests/hybrid_recall.rs` は「ゲート未設定＝明示的に対象外」を出力して成功終了します（fail-closed で塞ぐのは、設定済みの値が非数値・範囲外だった場合のみ）。

**`pull_request` トリガを持たせない理由（spec 機密保持が優先）**: `pull_request` で起動する job は PR 側の untrusted なコード（Makefile・テストコード含む）を checkout して実行するため、もし層 B を PR トリガにすると、PR がコードを書き換えて `HYBRID_RECALL_MIN_*`（spec 由来の非公開閾値）を標準出力へ書き出すだけで public な Actions ログから spec の数値基準を取得できてしまいます（`.claude/rules/spec-confidentiality.md` の P0 違反）。そのため層 B は既定ブランチの trusted なコードのみが走る `schedule`／`workflow_dispatch` に限定し、**PR のマージ判定は層 A（spec 数値を含まない public な固定値回帰。`.github/workflows/ci.yml` の `cargo test` で PR ごとに常時実行）が担う**、という役割分担にしています（`docs/design/hybrid-recall-regression.md` 参照）。決定的コーパスでの回帰トラッキング自体（層 A・固定値アサーション）は `make ci`（`cargo test`）に含まれており、こちらは repo variables 不要です。

**閾値 variables は repo レベルではなく Environment `recall-gate` に置きます**: `workflow_dispatch` は本来任意の ref を選んで起動でき、選択した ref の workflow YAML がそのまま実行されます。そのため `if: github.ref == 'refs/heads/main'`・`checkout ref: main` のような YAML 内の条件だけでは実行境界になりません——write 権限者が別ブランチでこのガードを外した `recall.yml` を push して `workflow_dispatch` すれば、そのブランチの YAML が実行されてしまうためです。加えて repo レベルの Actions variables はどのブランチのどの workflow からも参照できるため、YAML 内の条件式では閾値の参照そのものを防げません。そこで閾値は repo レベルではなく Environment `recall-gate`（deployment branch policy で `main` のみに制限）の variables として設定し、`recall-regression` job に `environment: recall-gate` を指定します。main 以外の ref から起動した run は environment `recall-gate` にアクセスできないため、別ブランチの改変 YAML から `if`／`checkout ref` を外して `workflow_dispatch` したとしても閾値を取得できません。`if: github.ref == 'refs/heads/main'`・`checkout ref: main` は environment 保護に対する defense-in-depth として維持しています。

## ライセンス

MIT OR Apache-2.0 のデュアルライセンスです（[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE)）。
