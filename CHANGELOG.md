# Changelog

`crates/engine`（`fandhe-vector-db-engine`）・`crates/wire-server`
（`fandhe-vector-db-wire-server`）の公開 API に影響する変更を記録する。
両クレートは同一バージョンで同期して公開する（`crates/wire-server/Cargo.toml`
の `engine` path 依存が完全固定バージョンで参照するため）。

## 0.2.0

### Breaking Changes

- `engine::sql::allowlist::ValidatedInsert` の公開フィールド
  `pub values: Vec<InsertLiteral>` を `pub rows: Vec<Vec<InsertLiteral>>` へ
  変更した。単一行 INSERT は `rows.len() == 1` として同じ内容を保持するため、
  移行は `stmt.values` を `stmt.rows[0]`（単一行前提のコードの場合）へ
  読み替えるか、複数行を扱う場合は `rows` を反復する形へ書き換える。
- `engine::sql::parser::BoundInsertForm` に新しい variant `RowBatch(Vec<BoundInsert>)`
  を追加した。この enum を網羅的に `match` していた利用側コードは、新 variant
  への対応を追加しないとコンパイルできない。
- いずれも SQL 表層の複数行 `VALUES (...), (...)` 構文サポート（ビヘイビア ID:
  SQL-16、TASK-190）のための変更であり、単一行 `INSERT` の外部観測可能な挙動
  （wire プロトコル応答・`wire_code`）は不変。
