# `CREATE TABLE` の設計判断

Issue #899・対象ビヘイビア: SQL-23（TASK-85・TASK-202）。関連ポインタ:
TABLE-1（`VECTOR(N)` 列の宣言）・TABLE-2（テーブル間の次元独立）・TABLE-4（O(1)
カタログ変更）・TABLE-6（識別子・型の許可リスト）・ERR-2／ERR-4／ERR-6（`42P07`・
`42701` の分類）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
CREATE TABLE <table> (<col> <type>[, <col> <type>]*) [;]
```

- `<type>` は `TEXT` または `VECTOR ( <N> )` のみ（TABLE-13／14 の追加型は別 Issue の
  管轄。制約構文・`IF NOT EXISTS`・`USING OPERATION_ID` の付与はいずれも許可リスト外
  （構造的に受理しない・`42601`）。
- `TEXT`／`VECTOR` は `lexer::Keyword` へ追加しない（`SET`・`CREATE`・`TRUNCATE`・
  `TABLE` と同方針。statement 中の所定位置でのみ文脈的キーワードとして照合し、
  同名の列名・テーブル名として使う既存 SQL を壊さない）。
- 識別子は大文字小文字をそのまま保持する（SELECT／INSERT と同じ既存の照合方針）。
- 列は 1 件以上必須。空列リスト・末尾カンマは構造的に受理しない。

## 列の既定

- `VECTOR(N)` 列は常に `nullable = false`（TABLE-1。全フィクスチャ・
  `validate_embedding_dim` が vector の存在を前提とする既存設計と整合）。
- `TEXT` 列は PostgreSQL に倣い `nullable = true`。
- `VECTOR` 列は 0 本または 1 本（catalog の既存 `validate_schema` 規則。2 本以上の
  宣言は `catalog::CatalogError::Invalid` → `42601`）。
- 予約列名 `id`／`tenant_id`／`visibility`（ASCII 大文字小文字を無視して照合）は
  列宣言として拒否する（`42601`）。`sql::parser` がこれら 3 語を疑似列・RLS 内部列
  として扱う契約と整合させ、SQL 表層の DDL でこれらを隠蔽する列を作らせないため
  （fail-closed）。Rust API（`Storage::create_table`）の挙動は変更しない。

## 上限検証（確保より前に拒否）

| 検証 | 上限 | 違反時 |
| ---- | ---- | ------ |
| 識別子長（テーブル名・列名） | 63 バイト | `42601` |
| 列数 | 256 | `54000`（`Vec::push` の**前**に判定） |
| `VECTOR` 次元 | `1..=65536` | `42601`（`catalog::validate_schema` へ委譲） |
| 同一文内の列名重複 | — | `42701` |

列数の上限（`sql::allowlist::MAX_CREATE_TABLE_COLUMNS`）は `catalog::
MAX_COLUMN_COUNT` と同値を採用し、`MAX_INSERT_COLUMNS` と同じ「値を複製し
コメントで対応関係を明示する」既存流儀に従う（`catalog` モジュールの可視性を
変えない）。

`VECTOR` の次元は構文解析段階で `Token::Number`（文字列）を `u32` へ checked に
parse し、オーバーフロー・小数点混入は `UnsupportedSyntax`（`42601`）へ落ちる
（`u32::from_str` が自然に拒否するため専用ロジックは持たない）。範囲
（`1..=MAX_VECTOR_DIM`）検証そのものは構文解析段階では重複実装せず、
`sql::ddl::execute_create_table` が `Storage::create_table` を通じて
`catalog::validate_schema` に委譲する（`CatalogError::Invalid` → `42601` への
写像は同一分類のため二重実装しない）。

## 権限ゲート（SQL-23・TASK-202）

DDL は全テナント共有のカタログを変更するため、既定拒否（fail-closed）の実行権限
ゲートを導入した。

- `engine::sql::mode::SessionState::ddl_allowed`（既定 `false`）・`grant_ddl()`
  （wire-server の認証成功後にのみ呼ばれる想定）。
- 判定点は `engine::sql::ddl::require_ddl_privilege` の 1 箇所のみ。
- **判定順序（決定的。`core.rs::EngineCore::execute_sql_in_session`）**:
  1. 字句解析
  2. 先頭 2 トークンが `CREATE`／`TABLE`（`sql::allowlist::
     is_create_table_statement`）
  3. **権限ゲート**（未許可なら即 `42501`）
  4. 構造・上限の検証（`sql::allowlist::validate_create_table_tokens`）
  5. 書き込みトランザクション内での存在判定（`sql::ddl::execute_create_table`）
- 権限ゲートを構造検証・カタログ照会より**前**に置くことで、未許可の主体は
  構文が正しいか・テーブルが存在するかに関わらず常に同じ `DdlNotPermitted`
  （`42501`）のみを受け取り、それらの情報を一切観測できない
  （`crates/engine/tests/sql_create_table.rs::
  create_table_rejects_unauthorized_session_for_garbage_syntax`・
  `create_table_rejects_unauthorized_session_for_existing_table_name` で固定）。

`42501` は既存の `ErrorClass::ForbiddenTenantMismatch` へ写像する（`23505` が
`IdConflict`／`DuplicateOperationId` の複数原因を束ねているのと同じ運用。新規
`ErrorClass` は追加しない）。

### wire-server 側の付与経路: `--ddl-principals`

`--ddl-principals <user[,user...]>`（`wire_server::ddl_permission_opt`）で
起動時 opt-in する。

- 構文検証（カンマ区切り・空要素禁止・空白禁止・重複禁止）は
  `ddl_permission_opt::parse` が担う。
- ユーザーストアへの実在確認は `auth::UserStore::with_ddl_principals`
  （未知ユーザーを指す場合は起動失敗。fail-closed）。
- `handshake.rs` は認証成功直後・`post_auth_loop` 呼び出しより前に
  `store.is_ddl_principal(&username)` を判定し、真なら `session.grant_ddl()` を
  呼ぶ。
- **未指定のサーバーは許可主体が存在せず、全ユーザーの全 `CREATE TABLE` が
  `42501` になる**（既定 fail-closed）。
- NoSQL 表層・拡張クエリプロトコルの Parse（`EngineCore::parse_sql`／
  `ParsedSql`）は本 Issue の対象外のまま（下記「スコープ外」参照）。

## 実行本体: 既存 `Storage::create_table` への委譲

`sql::ddl::execute_create_table` は `catalog::TableSchema` を組み立て、既存の
`Storage::create_table`（TASK-85）へそのまま委譲する。第 2 の DDL 実行器は作らない。
`create_table` は単一 write トランザクション内で存在確認・挿入・
`bump_table_generation_in_txn`・commit を行うため、事前の `table_exists` 照会は
行わない（TOCTOU を避けるため。同名での並行 `CREATE` は redb の単一ライタで直列化
され、後着が `42P07` になる）。

`CatalogError` の写像:

- `TableAlreadyExists` → `SqlSurfaceError::DuplicateTable`（`42P07`）
- `Invalid`（`VECTOR` 次元範囲外・複数 `VECTOR` 列宣言等）→ `SqlSurfaceError::
  unsupported`（`42601`）
- その他（`redb` I/O 等）→ `SqlSurfaceError::Internal`（`XX000`。詳細を
  クライアントへ渡さない）

## 応答: 件数を返さない

`CreateTableOutcome`（`sql::ddl`）はフィールドを持たない空構造体
（`TruncateOutcome` と同じ設計）。wire 応答の `CommandComplete` タグは
PostgreSQL 互換の `CREATE TABLE`（件数なし）。

## `SqlOutcome::CreateTable`（BREAKING CHANGE）

`SqlOutcome` へ新 variant `CreateTable(ddl::CreateTableOutcome)` を追加した
（`Insert`／`Truncate`／`Delete`／`Update` と同じ薄いラッパー設計）。`SqlOutcome`
を網羅的にマッチする既存コード（`core.rs`・`wire-server::simple_query`・
`crates/engine/tests/describe_parity.rs`）はすべて更新済み。クレート外で
`SqlOutcome` を網羅的にマッチするコードがあれば追随が必要。

## 既知の制約

- **ファイル形 INSERT（`path`／`body` 列）は SQL で作ったテーブルでは使えない**:
  ファイル形 INSERT は `path`／`body` の non-nullable `TEXT` 列を要求するが、本
  Issue の `TEXT` 列は常に `nullable = true`。`NOT NULL` 構文（別 Issue）の実装
  まで、SQL 表層で作成したテーブルへのファイル形 INSERT は使えない。
- **`VECTOR` 列を持たないテーブルは行の INSERT ができない**: 既存の行ストア層
  （`tenant::insert_typed_row` 系）は `TableSchema::validate_embedding_dim` を
  経由し、`VECTOR` 列を持たないテーブルへの呼び出しを構造的に拒否する
  （`table has no VECTOR column`）。本 Issue はこの既存契約を変更しない
  （テーブル定義自体は 0 本の `VECTOR` 列を許すが、行の書き込みは別の制約に
  左右される）。
- **テーブル総数の上限は未設定**: `catalog::MAX_LIST_TABLES`（列挙 API 側の上限）
  とは別に、許可された DDL 主体からの `CREATE TABLE` 連打（テーブル数の枯渇）に
  対する専用の上限は本 Issue では導入しない。DDL 主体はオーナーが明示的に許可した
  信頼された運用者を想定するため優先度は低いが、起票候補として記録する
  （out-of-scope-tracking）。

## スコープ外・後続 Issue

- `ALTER TABLE ADD COLUMN`／DROP／MODIFY COLUMN・`DROP TABLE`・各種制約
  （`NOT NULL`／`DEFAULT`／`PRIMARY KEY`／`UNIQUE`／`CHECK`／`REFERENCES`）・
  `CREATE INDEX`・`VIEW`・NoSQL 表層の DDL op はいずれも別 Issue の担当（本 Issue の
  権限ゲート（`require_ddl_privilege`・`--ddl-principals`）・`DuplicateColumn`
  分類の再利用を前提とする）。
- 拡張クエリプロトコル（Parse/Describe/Execute。Issue #933〜#935）への
  `CREATE TABLE` 対応は対象外。`EngineCore::parse_sql`／`ParsedSql` はセッション
  非依存の構造検証のみを担う設計のため、セッション状態を要する DDL 権限ゲートを
  混ぜず、`execute_sql_in_session`（簡易クエリプロトコル専用）でのみ受理する。
- `EXPLAIN CREATE TABLE`・`CREATE TABLE` への `USING OPERATION_ID` 付与はいずれも
  許可形状に存在しないため構造的に `42601`。
