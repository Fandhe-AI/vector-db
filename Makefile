# vector-db の開発タスクランナー。
#
# `make setup` 一発で開発環境（サブモジュール・rustup・lefthook）を構築し、
# `make ci` で CI（.github/workflows/ci.yml）と同等のチェックをローカル実行する。
# 実装は未着手（Cargo.toml 未追加）のため、cargo 系ターゲットは HAS_CARGO 判定で
# スキップし、workspace 作成（TASK-66）後に自動で有効化される（CI の detect 方針と
# 同一の冪等セルフヒール。deny も deny.toml + Cargo.toml が揃った時点で有効化）。
# Docker で環境非依存に開発・検証する場合は docker-* ターゲットを使う（compose.yaml 参照）。
# Fandhe-AI/rust-ai-library の Makefile と同一方針。

.DEFAULT_GOAL := help
SHELL := /bin/bash

# Cargo.toml の有無（無ければ cargo 系をスキップ。TASK-66 の workspace 作成後に有効化）
HAS_CARGO := $(wildcard Cargo.toml)
HAS_DENY := $(wildcard deny.toml)

# lint ツールの固定バージョン。CI（Fandhe-AI/actions の lint-docs reusable workflow）の
# 既定値に合わせる（CI 側が正。乖離したらこちらを追従させる）。
# EC_NPM_VERSION のみ npm ラッパーパッケージの版（CI は Go バイナリ release タグ v3.8.0 を
# 直接取得するため版番号体系が異なる。ローカル再現用の近似として npm 最新安定を固定する）。
MARKDOWNLINT_VERSION := 0.49.1
YAMLLINT_VERSION := 1.38.0
EC_NPM_VERSION := 6.1.1
COMMITLINT_VERSION := 21.2.1
COMMITLINT_CONFIG_VERSION := 21.2.0

# 導入系ツールの固定バージョン（`=x.y.z` 完全固定方針に合わせ exact 固定。
# CARGO_DENY_VERSION は Dockerfile の先行導入と値を同期させる）。
LEFTHOOK_VERSION := 2.1.10
CARGO_DENY_VERSION := 0.20.2

.PHONY: help
help: ## ターゲット一覧を表示する
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# --------------------------------------------------
# 環境構築
# --------------------------------------------------

# 依存ターゲット並記だと -j 実行時に順序が保証されず、cargo フォールバックを持つ hooks が
# rustup より先に走りうるため、再帰 make で「submodule → rustup → hooks」の順を明示する
# （rust-ai-library と同一方針）。
.PHONY: setup
setup: ## 開発環境を一括構築する（サブモジュール → rustup → lefthook の順を保証）
	$(MAKE) submodule
	$(MAKE) rustup
	$(MAKE) hooks
	@echo "setup 完了"

.PHONY: rustup
rustup: ## rustup（cargo）を未導入の場合のみ導入する
	@if ! command -v rustup >/dev/null 2>&1 && [ ! -x "$$HOME/.cargo/bin/rustup" ]; then \
		echo "rustup を導入します"; \
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable; \
	fi

# docs/spec（vector-db-spec）は private リポジトリのため、アクセス権のない環境では
# 取得に失敗する。実装コードのビルド・テストは docs/spec 抜きでも成立させる方針
# （README「開発環境構築」）のため、失敗しても setup 全体は止めない。
.PHONY: submodule
submodule: ## docs/spec サブモジュールを初期化・更新する（private・アクセス権が無ければ警告のみ）
	@git submodule update --init || \
		echo "警告: docs/spec（private）の取得に失敗しました。アクセス権のない環境では想定内です（ビルド・テストは spec 抜きで成立します）"

# lefthook（Go 製。crates.io には存在しないため cargo フォールバックは置かない）は
# brew（バージョン固定不可だが常用導線）を優先し、無ければ npm 配布版を exact 固定の
# npx ワンショットで実行する（lefthook が生成する hook スクリプトは PATH → npx の順で
# 本体を解決するため、npx 経由の導入でもコミット時にフックが機能する）。
.PHONY: hooks
hooks: ## lefthook の git hooks を導入する（未導入なら lefthook 本体も導入）
	@if command -v lefthook >/dev/null 2>&1; then \
		lefthook install; \
	elif command -v brew >/dev/null 2>&1; then \
		echo "lefthook を導入します"; \
		brew install lefthook && lefthook install; \
	elif command -v npx >/dev/null 2>&1; then \
		echo "lefthook（npx 固定版）で hooks を導入します"; \
		npx --yes lefthook@$(LEFTHOOK_VERSION) install; \
	else \
		echo "brew / npx が見つかりません。https://lefthook.dev/installation/ を参照してください" >&2; \
		exit 1; \
	fi

# --------------------------------------------------
# ドキュメント／設定ファイル系 lint（CI の lint-docs ジョブと同等の内容）
# --------------------------------------------------

.PHONY: lint-md
lint-md: ## markdownlint（.markdownlint.jsonc / .markdownlintignore 参照）
	npx --yes markdownlint-cli@$(MARKDOWNLINT_VERSION) --ignore-path .markdownlintignore "**/*.md"

# yamllint は Python 製のため npx で賄えない。導入済みの実体（brew / pip）を優先し、
# uvx があれば固定版のワンショット実行で代替する。いずれも無ければ fail-closed で
# 導入方法を案内して失敗する（silent skip は CI との false-green 乖離になるため行わない）。
.PHONY: lint-yaml
lint-yaml: ## yamllint（.yamllint 参照）
	@if command -v yamllint >/dev/null 2>&1; then \
		yamllint .; \
	elif command -v uvx >/dev/null 2>&1; then \
		uvx yamllint==$(YAMLLINT_VERSION) .; \
	else \
		echo "yamllint 未導入: brew install yamllint / pip install yamllint==$(YAMLLINT_VERSION) で導入してください" >&2; \
		exit 1; \
	fi

.PHONY: lint-editorconfig
lint-editorconfig: ## editorconfig-checker（.editorconfig + .editorconfig-checker.json 参照）
	npx --yes editorconfig-checker@$(EC_NPM_VERSION)

# main からの分岐点以降のコミットを CI（lint-docs の commitlint ジョブ）と同じ
# extends 構成で検証する。origin/main が未取得の環境では範囲を決められないためスキップする。
.PHONY: lint-commits
lint-commits: ## commitlint（origin/main からの分岐点以降のコミットを検証）
	@if git rev-parse --verify origin/main >/dev/null 2>&1; then \
		npx --yes -p @commitlint/cli@$(COMMITLINT_VERSION) -p @commitlint/config-conventional@$(COMMITLINT_CONFIG_VERSION) \
			commitlint --extends @commitlint/config-conventional --from "$$(git merge-base origin/main HEAD)" --to HEAD; \
	else \
		echo "skip: origin/main が未取得のため commitlint をスキップ"; \
	fi

.PHONY: lint-docs
lint-docs: lint-md lint-yaml lint-editorconfig lint-commits ## ドキュメント／設定ファイル系 lint を一括実行する

# --------------------------------------------------
# 品質チェック（Rust。Cargo.toml 追加後に有効化）
# --------------------------------------------------

.PHONY: fmt
fmt: ## cargo fmt --all で整形する
ifdef HAS_CARGO
	cargo fmt --all
else
	@echo "skip: Cargo.toml 未追加のため fmt をスキップ"
endif

.PHONY: fmt-check
fmt-check: ## cargo fmt --check（整形差分の検出）
ifdef HAS_CARGO
	cargo fmt --all --check
else
	@echo "skip: Cargo.toml 未追加のため fmt-check をスキップ"
endif

.PHONY: lint
lint: ## cargo clippy -D warnings（lint ゲート）
ifdef HAS_CARGO
	cargo clippy --workspace --all-targets --all-features -- -D warnings
else
	@echo "skip: Cargo.toml 未追加のため lint をスキップ"
endif

.PHONY: test
test: ## cargo test（workspace 全体）
ifdef HAS_CARGO
	cargo test --workspace --all-features
else
	@echo "skip: Cargo.toml 未追加のため test をスキップ"
endif

.PHONY: crash-test
crash-test: ## クラッシュ耐性回帰テスト（TASK-142・PERSIST-1。scripts/crash_test.sh を実行）
ifdef HAS_CARGO
	scripts/crash_test.sh
else
	@echo "skip: Cargo.toml 未追加のため crash-test をスキップ"
endif

.PHONY: crash-test-interrupt
crash-test-interrupt: ## crash_test.sh の中断パス（SIGTERM 単体・プロセスグループ）のセルフテスト（Issue #134・TASK-142。scripts/crash_test_interrupt.sh を実行）
ifdef HAS_CARGO
	scripts/crash_test_interrupt.sh
else
	@echo "skip: Cargo.toml 未追加のため crash-test-interrupt をスキップ"
endif

.PHONY: crash-test-cross-table
crash-test-cross-table: ## 2 テーブル横断トランザクション・クラッシュ耐性回帰テスト（TASK-90・TABLE-10。scripts/crash_test_cross_table.sh を実行）
ifdef HAS_CARGO
	scripts/crash_test_cross_table.sh
else
	@echo "skip: Cargo.toml 未追加のため crash-test-cross-table をスキップ"
endif

.PHONY: core-api-check
core-api-check: ## コア API（VectorCore/SearchProvider）シグネチャ差分検知（TASK-125・CORE-1。cargo 不要のテキスト比較）
	scripts/check_core_api.sh --self-test
	scripts/check_core_api.sh

.PHONY: sort-determinism-check
sort-determinism-check: ## RRF 融合等のソート非決定性 API 再混入検知（TASK-84・Issue #61。cargo 不要のテキスト比較）
	scripts/check_sort_determinism.sh --self-test
	scripts/check_sort_determinism.sh

.PHONY: check-cross
check-cross: ## TASK-156（CORE-14）aarch64 クロスコンパイル確認。cargo check のみ（リンクしないためクロスリンカ不要）。手元に target 未導入でも make ci を壊さないよう独立ターゲットとする（bench-* と同方針）。`contrast-bench` feature を付けないため usearch（TASK-127 CORE-5・Issue #176。C++ ビルドを伴う）は本コマンドの対象に含まれない
ifdef HAS_CARGO
	cargo check -p engine --all-targets --target aarch64-unknown-linux-gnu
else
	@echo "skip: Cargo.toml 未追加のため check-cross をスキップ"
endif

.PHONY: e2e-three-client
e2e-three-client: ## TASK-73（WIRE-1）/TASK-82（SQL-5〜7,9,10）/TASK-165（SQL-12・SEARCH-9）/TASK-168（SQL-13・SQL-14）psql/psycopg/pg 実クライアント統合テスト（opt-in・`ci` には含めない。要 psql・python3+psycopg・node+pg。PSQL_BIN/PYTHON_BIN/NODE_BIN で上書き可）
ifdef HAS_CARGO
	cargo test -p wire-server --test three_client_e2e -- --ignored
	cargo test -p wire-server --test extended_syntax_e2e -- --ignored
else
	@echo "skip: Cargo.toml 未追加のため e2e-three-client をスキップ"
endif

.PHONY: deny
deny: ## cargo deny check advisories bans licenses sources（依存監査。cargo-deny 未導入なら自動導入）
ifneq ($(and $(HAS_CARGO),$(HAS_DENY)),)
	@export PATH="$$HOME/.cargo/bin:$$PATH"; \
	command -v cargo-deny >/dev/null 2>&1 || { \
		echo "cargo-deny を導入します"; \
		cargo install cargo-deny@$(CARGO_DENY_VERSION) --locked; \
	}; \
	cargo deny --locked check advisories bans licenses sources
else
	@echo "skip: Cargo.toml または deny.toml 未追加のため deny をスキップ"
endif

.PHONY: ci
ci: lint-docs fmt-check lint test crash-test crash-test-interrupt crash-test-cross-table core-api-check sort-determinism-check deny ## CI（ci.yml）と同等のチェックを一括実行する

# --------------------------------------------------
# 性能・Recall 受け入れ基準の回帰ベンチ（TASK-127。crates/engine/benches/simd_bench.rs）
# --------------------------------------------------

.PHONY: bench-simd
bench-simd: ## TASK-127 の性能・Recall 受け入れ基準回帰ベンチを実行する（時間依存のため ci には含めない。.github/workflows/bench.yml から週次 schedule / workflow_dispatch で実行）
ifdef HAS_CARGO
	cargo bench --bench simd_bench -p engine
else
	@echo "skip: Cargo.toml 未追加のため bench-simd をスキップ"
endif

# --------------------------------------------------
# CORE-5 対照エンジン接続の回帰ベンチ（TASK-127・Issue #176。crates/engine/benches/contrast_bench.rs）
# --------------------------------------------------

.PHONY: bench-contrast
bench-contrast: ## TASK-127 CORE-5（対照エンジンに対する p95 レイテンシ比率）の回帰ベンチを実行する（`contrast-bench` feature 限定・C++17 コンパイラが必要。時間依存のため ci には含めない。.github/workflows/bench.yml から週次 schedule / workflow_dispatch で実行）
ifdef HAS_CARGO
	cargo bench --bench contrast_bench -p engine --features contrast-bench
else
	@echo "skip: Cargo.toml 未追加のため bench-contrast をスキップ"
endif

# --------------------------------------------------
# バッチ高速化の受け入れ基準検証（TASK-130。crates/engine/benches/batch_bench.rs）
# --------------------------------------------------

.PHONY: bench-batch
bench-batch: ## TASK-130 のバッチ高速化受け入れ基準回帰ベンチを実行する（時間依存のため ci には含めない。.github/workflows/bench.yml から週次 schedule / workflow_dispatch で実行）
ifdef HAS_CARGO
	cargo bench --bench batch_bench -p engine
else
	@echo "skip: Cargo.toml 未追加のため bench-batch をスキップ"
endif

# --------------------------------------------------
# C1（純粋 Top-k）SQL 表層 p95 専有環境再測定（TASK-83。crates/engine/benches/sql_c1_bench.rs）
# --------------------------------------------------

.PHONY: bench-c1
bench-c1: ## TASK-83（Conditional Go 条件7）の SQL 表層 C1 p95 再測定ベンチを実行する（時間依存のため ci には含めない。.github/workflows/bench.yml から workflow_dispatch のみで実行。schedule 化はしない）
ifdef HAS_CARGO
	cargo bench --bench sql_c1_bench -p engine
else
	@echo "skip: Cargo.toml 未追加のため bench-c1 をスキップ"
endif

# --------------------------------------------------
# ティア別レイテンシ受け入れ基準の検証（TASK-116。crates/engine/benches/tier_latency_bench.rs）
# --------------------------------------------------

.PHONY: bench-tier
bench-tier: ## TASK-116（PLAN-4/6/7）のティア別レイテンシ受け入れ基準ベンチを実行する（時間依存・常駐 Ollama 前提のため ci には含めない。CI 経路は存在せず README「ティア別レイテンシ受け入れ基準の実測手順」記載の Actions 外の承認済み計測環境で運用者が直接実行する）
ifdef HAS_CARGO
	cargo bench --bench tier_latency_bench -p engine
else
	@echo "skip: Cargo.toml 未追加のため bench-tier をスキップ"
endif

# --------------------------------------------------
# ハイブリッド検索 Recall 閾値ゲート（TASK-104。crates/engine/tests/hybrid_recall.rs 層 B）
# --------------------------------------------------

.PHONY: recall-regression
recall-regression: ## TASK-104 のハイブリッド検索 Recall 閾値ゲート（層 B）を実行する（spec 閾値の環境変数注入が必要。ci には含めない。.github/workflows/recall.yml から実行。標準出力は対象名と pass/fail のみ。実測値は RECALL_VERBOSE=1〔GitHub Actions 外限定〕。Issue #303）
ifdef HAS_CARGO
	cargo test --release -p engine --test hybrid_recall -- --ignored --nocapture
else
	@echo "skip: Cargo.toml 未追加のため recall-regression をスキップ"
endif

# --------------------------------------------------
# リランキング効果測定 Recall 閾値ゲート（TASK-108。crates/engine/tests/rerank_recall.rs 層 B）
# --------------------------------------------------

.PHONY: rerank-regression
rerank-regression: ## TASK-108 のリランキング効果測定 Recall 閾値ゲート（層 B）を実行する（spec 閾値の環境変数注入が必要。ci には含めない。.github/workflows/recall.yml から実行。標準出力は対象名と pass/fail のみ。実測値は RECALL_VERBOSE=1〔GitHub Actions 外限定〕。Issue #303）
ifdef HAS_CARGO
	cargo test --release -p engine --test rerank_recall -- --ignored --nocapture
else
	@echo "skip: Cargo.toml 未追加のため rerank-regression をスキップ"
endif

# --------------------------------------------------
# クエリ展開の受け入れ基準 Recall 閾値ゲート（TASK-112・TASK-113。crates/engine/tests/query_planning_recall.rs 層 B）
# --------------------------------------------------

.PHONY: query-planning-regression
query-planning-regression: ## TASK-112・TASK-113 のクエリ展開受け入れ基準（intent 改善幅・direct 維持・劣化展開時の intent 改善幅・大規模段 direct 絶対下限）Recall 閾値ゲート（層 B。--ignored 一括実行のため大規模段ゲートも対象に含む）を実行する（spec 閾値の環境変数注入が必要。ci には含めない。.github/workflows/recall.yml から実行。標準出力は対象名と pass/fail のみ。実測値は RECALL_VERBOSE=1〔GitHub Actions 外限定〕。Issue #303）
ifdef HAS_CARGO
	cargo test --release -p engine --test query_planning_recall -- --ignored --nocapture
else
	@echo "skip: Cargo.toml 未追加のため query-planning-regression をスキップ"
endif

# --------------------------------------------------
# precision モード評価基準ゲート（TASK-163。crates/engine/tests/precision_eval.rs 層 B）
# --------------------------------------------------

# precision-regression は将来 recall.yml（public runner）へ接続しうるため、閾値との
# pass/fail 判定のみを実行し実測値は出力しない。実測値を出力する
# precision_eval_report / precision_eval_policy_sweep は public Actions のログへ
# 非公開値を出さないようローカル専用の precision-report 側へ分離する
# （.claude/rules/spec-confidentiality.md。PR #212 codex-review P0）。
.PHONY: precision-regression
precision-regression: ## TASK-163 の precision モード評価基準（Top-1 Accuracy・MRR@10・誤返却率）閾値ゲートのみを実行する（pass/fail のみ出力・実測値は出さない。RECALL_VERBOSE=1〔GitHub Actions 外限定〕opt-in 時のみ実測値を追加出力。目標値未確定のため .github/workflows/recall.yml には未接続。ci には含めない。Issue #303）
ifdef HAS_CARGO
	cargo test --release -p engine --test precision_eval -- --ignored --nocapture --exact precision_eval_threshold_gate
else
	@echo "skip: Cargo.toml 未追加のため precision-regression をスキップ"
endif

.PHONY: precision-report
precision-report: ## TASK-163 の判断材料レポート（hybrid/dense の指標）とパラメータ感度スイープを実行する（実測値を標準出力へ出すためローカル専用。CI・GitHub Actions からは実行しない。GITHUB_ACTIONS 下ではテスト側が fail-closed で拒否する。Issue #303）
ifdef HAS_CARGO
	cargo test --release -p engine --test precision_eval -- --ignored --nocapture --exact precision_eval_report
	cargo test --release -p engine --test precision_eval -- --ignored --nocapture --exact precision_eval_policy_sweep
else
	@echo "skip: Cargo.toml 未追加のため precision-report をスキップ"
endif

# --------------------------------------------------
# 評価スクリプト（scripts/eval。TASK-118）
# --------------------------------------------------

# python3 は開発コンテナ（Dockerfile）に含まれず、CI（ci.yml）にも組み込まれていない
# 独立ターゲットのため、未導入時は cargo 系（HAS_CARGO）と違い silent skip にしない
# （false-green 回避。yamllint と同一方針）。`ci` には含めない（docker-ci を壊さないため）。
.PHONY: test-eval
test-eval: ## scripts/eval のユニットテストを実行する（TASK-118。python3 が必要）
	@if command -v python3 >/dev/null 2>&1; then \
		python3 -m unittest discover scripts/eval/tests; \
	else \
		echo "python3 未導入: scripts/eval のテストには python3 が必要です" >&2; \
		exit 1; \
	fi

# --------------------------------------------------
# Docker（環境非依存の開発・検証。詳細は compose.yaml / Dockerfile 参照）
# --------------------------------------------------

.PHONY: docker-build
docker-build: ## 開発コンテナイメージをビルドする
	docker compose build

.PHONY: docker-shell
docker-shell: ## 開発コンテナのシェルに入る
	docker compose run --rm dev

.PHONY: docker-ci
docker-ci: ## コンテナ内で make ci を実行する（環境非依存の検証）
	docker compose run --rm dev make ci
