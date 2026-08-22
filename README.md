# vector-db

Rust 製のローカルファースト・vector 特化クエリ DB の実装リポジトリです。「正確なデータ 1 件のピンポイント抽出」ではなく「正解を含むデータ群を広く返す」ことを設計思想とし、LLM のコンテキストとして渡す用途に最適化します。

## 位置づけ

- **本リポジトリは public** です（rust-ai-library と同一方針）
- **仕様・ビヘイビア定義**: [vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec)（`docs/spec` に submodule 参照。**private リポジトリとして意図的に非公開を維持**する方針であり、アクセス権のない環境からは submodule を解決できません）
- Web API・MCP サーバーはいずれも別プロダクトとして本リポジトリのスコープ外です

## ステータス

実装は未着手です（ロードマップの着手判定待ち）。タスク定義は spec リポの [`05-tasks.md`](https://github.com/Fandhe-AI/vector-db-spec/blob/main/05-tasks.md)（TASK-66〜154・89 件）、マイルストーンは [`06-roadmap.md`](https://github.com/Fandhe-AI/vector-db-spec/blob/main/06-roadmap.md)（MS-1〜6）を参照してください。

## 実装方針（要点）

- **接続プロトコル**: PostgreSQL wire プロトコル v3 互換の**自作実装**（`pgwire` 等の外部ライブラリへ可能な限り依存しない）。psql・psycopg・node pg が無改造で接続可能なことを PoC-8 で実測済み
- **クエリ表層**: 標準クエリカタログ C1〜C5 を MVP とする vector 特化 SQL（C6 集計・C7 結合は拡張扱い）。LLM クエリプランニングは専用構文 `USING PLAN(...)` で SQL に露出
- **想定クレート構成**: `engine`（コアロジック: データロード・検索カーネル・認証・RLS）＋ `wire-server`（バイナリ）の workspace 構成（TASK-66）
- **永続化**: `redb` ベース（単一ライタ・スナップショット読み取り。並行書き込み検証は MS-1 の TASK-144）
- **安全性**: RLS 相当のテナント境界・fail-closed のエラー契約（SQLSTATE 風 `wire_code`）
- **依存最小方針**: 依存の追加・更新は必ずユーザー承認を経て行い、`=x.y.z` 完全固定で管理する

詳細なビヘイビア（96 件・12 領域）は spec リポの [`04-behavior/`](https://github.com/Fandhe-AI/vector-db-spec/tree/main/04-behavior) を唯一の正（SSOT）とします。

## 開発環境構築

```bash
git clone git@github.com:Fandhe-AI/vector-db.git
cd vector-db
git submodule update --init   # docs/spec（private・要アクセス権）
```

`docs/spec`（`vector-db-spec`）は private リポジトリのため、アクセス権のない環境では submodule 取得が失敗します。実装コードのビルド・テストは `docs/spec` 抜きでも成立するよう維持します。

## ライセンス

MIT OR Apache-2.0 のデュアルライセンスです（[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE)）。
