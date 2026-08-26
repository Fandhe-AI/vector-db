# CLAUDE.md

## Overview

Rust 製のローカルファースト・vector 特化クエリ DB の実装リポジトリ。「正解を含むデータ群を広く返す」ことを設計思想とし、LLM のコンテキストとして渡す用途に最適化する。

- **本リポは public**。仕様・ビヘイビア定義の SSOT は private リポ [vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec)（`docs/spec` submodule）。**spec 本文を public 資産へ転記しない**（[spec-confidentiality](.claude/rules/spec-confidentiality.md)）
- 接続プロトコル: PostgreSQL wire プロトコル v3 互換の自作実装（外部プロトコルライブラリ非依存）
- クレート構成: `engine`（コアロジック）＋ `wire-server`（lib+bin）の workspace（`crates/`）
- 永続化: `redb` ベース / 安全性: RLS 相当のテナント境界・fail-closed のエラー契約（`wire_code`）
- 依存は最小・`=x.y.z` 完全固定・ユーザー承認制（[dependency-policy](.claude/rules/dependency-policy.md)）
- ステータス: workspace 雛形構築済み（TASK-66）。wire プロトコル層は実装済み（TASK-67・TASK-68・TASK-69・TASK-70・TASK-71）。SQL 表層は許可リスト検証（TASK-74）・束縛と実行計画（TASK-75）・取得モード切替構文（TASK-161）・`precision` モードの実行契約（TASK-162）・宣言的 UDF 呼び出し（TASK-79）・宣言的フィルタ API（TASK-147）まで実装済み。簡易クエリプロトコルを SQL 表層へ接続（TASK-73・WIRE-1）。`precision` モードの評価ハーネス実装済み（TASK-163。目標値の確定はユーザー判断待ち・`.github/workflows/recall.yml` 未接続）。モード切替・`precision` 契約の wire 経由 3 クライアント検証実装済み（TASK-165・SQL-12・SEARCH-9）。RLS 暗黙適用の全読み取り経路一般化検証を実施済み（TASK-138）。障害回復の `operation_id` 必須化ガード実装済み（TASK-92）。集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`、TASK-166・SQL-13）・`GROUP BY`/`HAVING` 集計（TASK-167・SQL-14）まで実装済み。集計クエリの wire 経由 3 クライアント検証実装済み（TASK-168・SQL-13・SQL-14）。バッチ検索の実 GPU バックエンド（`wgpu`。**依存追加はオーナー承認待ち**）を `gpu_batch.rs` として接続済み（TASK-128〜130・Issue #178。ベンチは CORE-6 のみ実測経路へ配線済み・CORE-16 は測定不能を明示）。他は未実装（タスクは spec リポの `05-tasks.md`（TASK-66〜165）、マイルストーンは `06-roadmap.md`（MS-1〜6）参照）

## Repository Structure

```text
vector-db/
├── CLAUDE.md / AGENTS.md          # Claude 運用方針 / レビュー観点集（codex-review の基準）
├── README.md                      # 概要・実装方針（要点）・開発環境構築
├── Makefile                       # タスクランナー（make setup / make ci / docker-*）
├── lefthook.yml                   # git hooks（rustfmt・secrets-guard・Conventional Commits・clippy/test）
├── Dockerfile / compose.yaml      # 環境非依存の開発コンテナ（make docker-ci）
├── deny.toml                      # cargo-deny 設定（make deny で有効化済み）
├── rust-toolchain.toml            # stable + rustfmt/clippy（単一真実源）
├── commitlint.config.mjs          # Conventional Commits 検証設定
├── skills-lock.json               # 導入スキルのロックファイル
├── docs/
│   ├── design/                    # 設計ドキュメント（ADR 形式・public）
│   └── spec/                      # vector-db-spec submodule（private・要アクセス権）
├── .github/workflows/
│   ├── ci.yml                     # lint-docs + rust-ci（fmt/clippy/test/cargo-deny）+ crash-test + crash-test-interrupt + crash-test-cross-table + core-api-check + sort-determinism-check + cross-check（aarch64 クロスコンパイル確認）の CI
│   ├── bench.yml                  # TASK-127 性能・Recall 受け入れ基準（CORE-5 は Issue #176 まで opt-in）+ TASK-130 バッチ高速化受け入れ基準（CORE-6 は GPU 搭載環境向けの opt-in・CORE-16 は測定不能）の回帰ベンチ（workflow_dispatch + 週次 schedule）+ TASK-83 SQL 表層 C1 p95 専有環境再測定（Conditional Go 条件7・workflow_dispatch 限定）
│   ├── recall.yml                 # TASK-104 ハイブリッド検索 Recall 回帰の層 B 閾値ゲート（workflow_dispatch + 週次 schedule。environment recall-gate + strict モードで閾値未評価runの誤green化を防止。pull_request 非対応＝spec 閾値の非公開ログ漏えい防止。PR ゲートは層 A が担う）
│   └── codex-review.yml           # PR 自動レビュー wrapper
├── .claude/
│   ├── agents/                    # カテゴリ別 subagent 定義
│   ├── rules/                     # 運用ルール
│   ├── skills/                    # npx skills add 導入スキル
│   ├── workflows/                 # implement-issue-tree.js (相対 symlink)
│   └── settings.json              # SessionStart / PostToolUse hooks
├── scripts/                       # 補助スクリプト（crash_test.sh・crash_test_interrupt.sh・crash_test_cross_table.sh・check_sort_determinism.sh 等。make 経由で実行）
├── Cargo.toml                     # workspace 定義（members: crates/engine, crates/wire-server）
└── crates/                        # engine（lib）/ wire-server（lib+bin）workspace
```

## 委譲方針（必読）

main セッションはオーケストレーションに徹し、調査・実装・レビューは subagent へ委譲してコンテキスト消費を抑える。詳細は [delegation](.claude/rules/delegation.md)（調査）・[delegation-impl](.claude/rules/delegation-impl.md)（実装）を参照。

### パスベース切り替え表

| 対象 | 調査 | 作成・編集 |
| ---- | ---- | ---------- |
| `crates/engine/` | explorer | engine-builder |
| `crates/wire-server/` | explorer | wire-builder |
| `docs/spec/`（private） | explorer（ポインタ表記） | 変更しない（spec リポ側で管理） |
| 外部仕様（pg wire v3・redb 等） | reference-researcher | — |
| テスト・lint | test-runner / linter | — |
| ドキュメント | explorer | docs-writer |

### model 配分表

| 用途 | model |
| ---- | ----- |
| 複雑な横断判断・アーキテクチャ設計 | opus または fable（fable は特に大規模設計・横断判断の最上位 tier） |
| 調査・生成・実装・レビュー | sonnet |
| 機械的集計・lint・ドキュメント更新 | haiku |

## Sub-agents

| カテゴリ | subagent_type | model | 役割 |
| -------- | ------------- | ----- | ---- |
| research | explorer | sonnet | コードベース・spec 横断調査（spec はポインタ表記） |
| research | reference-researcher | sonnet | 外部仕様・依存候補クレートの調査 |
| implement | engine-builder | sonnet | engine クレート（検索カーネル・認証・RLS・永続化）実装 |
| implement | wire-builder | sonnet | wire-server クレート（pg wire v3 自作実装）実装 |
| testing | test-runner | sonnet | cargo test / clippy 実行と失敗解析 |
| quality | reviewer | sonnet | AGENTS.md P0/P1/P2 観点のレビュー |
| quality | security-auditor | sonnet | テナント境界・wire 入力・spec 漏えい・OWASP 監査 |
| quality | linter | haiku | rustfmt / clippy / markdownlint 等の機械的確認 |
| docs | docs-writer | haiku | README・CLAUDE.md・ドキュメント更新 |

## Rules

| ファイル | 内容 |
| -------- | ---- |
| [delegation.md](.claude/rules/delegation.md) | 調査フェーズの委譲原則・パスベース切り替え |
| [delegation-impl.md](.claude/rules/delegation-impl.md) | 実装フェーズの委譲マッピング・標準フロー |
| [coding-rust.md](.claude/rules/coding-rust.md) | Rust 規約（untrusted 入力・fail-closed・unsafe 原則禁止） |
| [security.md](.claude/rules/security.md) | OWASP Top 10・秘密情報混入防止・テナント境界 |
| [japanese-style.md](.claude/rules/japanese-style.md) | 日本語出力スタイル |
| [conventional-commits.md](.claude/rules/conventional-commits.md) | Conventional Commits 詳細規約（type/scope 一覧） |
| [code-comment-style.md](.claude/rules/code-comment-style.md) | コメント規約（役割・責務・呼び出し文脈の埋め込み） |
| [out-of-scope-tracking.md](.claude/rules/out-of-scope-tracking.md) | スコープ外事項の Issue 追跡フロー |
| [spec-confidentiality.md](.claude/rules/spec-confidentiality.md) | **リポ固有・P0**: private spec のポインタ表記運用 |
| [dependency-policy.md](.claude/rules/dependency-policy.md) | **リポ固有**: 依存最小・`=x.y.z` 固定・ユーザー承認制 |

## Current Skills

`npx skills add`（Fandhe-AI/agent-cli-skills ほか）で導入済み。ロックは `skills-lock.json`。

- **ワークフロー系**: create-commit / create-pr / create-issue / create-issue-tree / create-plan / implement-issue / implement-issue-tree / implement-review / implement-review-pr / update-issue-tree / update-docs / comment-code
- **メンテ系**: init-claude / update-claude / contribute-skill / sync-skills-lock / setup-repo-guards
- **リファレンス系**: rust / github-docs / commitlint / lefthook / editorconfig / nvidia-cuda / amd-rocm / apple-silicon

## Conventions

- **環境構築・検証**: `make setup`（submodule → rustup → lefthook）で構築し、push 前に `make ci`（CI と同等のチェック）をローカル実行する。cargo 系ターゲットは workspace 追加（TASK-66）により有効化済み
- **日本語**: やりとり・報告・コミット説明文・コード内コメントは日本語（プログラム出力文字列は英語）
- **Conventional Commits**: commitlint で検証。`--no-verify` 禁止
- **セキュリティレビュー**: PR 作成前に OWASP Top 10＋AGENTS.md P0 観点（spec 漏えい・テナント境界・wire 入力検証）を確認
- **ユーザー承認フロー**: 依存の追加・更新 / Issue 起票 / 既存ファイル上書き / implement-issue の実装開始（計画承認後）は必ずユーザー承認を経る
- **spec ポインタ運用**: `docs/spec` の内容は TASK-nn・ビヘイビア ID・パスのポインタ表記でのみ参照する

## hooks（settings.json）

- **SessionStart**: 日本語・委譲・Conventional Commits・`--no-verify` 禁止・spec 漏えい注意・依存承認制のリマインダーを表示
- **PostToolUse**（Edit|Write）: `*.rs` 編集後に rustfmt で自動整形。edition は workspace の正である `Cargo.toml` から取得し（lefthook.yml の rustfmt-check と同一方針）、Cargo.toml / jq / rustfmt 未導入時は何もしない。整形失敗は隠さず hook のエラーとして報告される
