# UNIQUE 制約（TABLE-16・TASK-204）

- **Issue**: #905（`feat(engine): UNIQUE 制約`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/05-tasks.md`
  TASK-204・`docs/spec/04-behavior/data-model.md` TABLE-16・TABLE-12・
  `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
  `docs/spec/04-behavior/error-format.md` ERR-2・ERR-4・ERR-6・
  `docs/spec/04-behavior/recovery.md` RECOVER-7・RECOVER-12
- **ステータス**: 実装済み（本ドキュメントが範囲を確定する。カタログ永続化・
  単一検査点・主要な書き込みプリミティブへの結線・`CREATE TABLE` 構文・
  `Storage::alter_table_add_unique_constraint`・層 A テスト）

## 背景・目的

行 `id` の暗黙一意性（TABLE-12）と `nullable` フラグに加え、任意の列（単一列・
複数列）の値の組に一意性を課す UNIQUE 制約を導入する。受入基準:

1. 宣言済み列に対する重複する INSERT・UPDATE・UPSERT を `23505` で拒否する
2. 一意性のスコープはテナント内に閉じる（他テナントの同値は違反にならない）
3. 既存行に重複がある状態での制約追加を fail-closed に拒否する（副作用ゼロ）
4. 応答（成否・`wire_code`・文言）から他テナントの値の存在を推測できない
5. RLS 暗黙適用・fail-closed 維持、`wire_code` 契約との整合、untrusted 入力経路
   での `unwrap`/`expect`/添字アクセス禁止、依存追加なし

## 設計判断

### D1. 検査方式: 書き込みトランザクション内のテナント範囲走査

検査は write txn 内で自テナントの物理キー範囲 `(tenant_id, 0)..=(tenant_id,
u64::MAX)`（TABLE-12）を走査し、制約列のみを `row_codec::scan_scalar_columns`
でデコードして行う（`crates/engine/src/tenant/unique_check.rs`）。可視性
（`PolicyContext` の可視集合）では絞らない——`Public`／`Private` を問わずテナント
所有の全行が母集合になる。他テナントの範囲は構造的に一切読まない。

候補（今回書き込む行）側のキー集合のみをメモリに保持し、既存行はストリーミング
で照合する（O(候補数) メモリ）。制約を持たないテーブル（`schema.
unique_constraints().is_empty()`）は走査自体を一切行わない（既存ベンチ経路に
影響なし）。

**既知の制約**: 制約付きテーブルへの書き込みは 1 文あたり O(自テナント行数) の
走査を伴う（`MAX_SCANNED_ROWS` のような上限は課さない——課すと大規模テナントの
制約付きテーブルが書き込み不能になるため）。永続一意索引（redb 二次テーブルに
よる O(log n) 判定）は不採用のまま、将来検討事項として記録する。

### D2. 単一の検査点

`unique_check::check_in_txn`（テーブルを自前で開く版）・`check_against_table`
（呼び出し元が既に行テーブルを開いている場合——`upsert_typed_rows_unchecked`
の read-merge-write ループ。同一 write トランザクション内で同じ動的テーブルを
2 回 `open_table` すると redb がエラーを返すため）の 2 form を提供し、以下の
書き込みプリミティブ（`crates/engine/src/tenant.rs`）が台帳照合の**直後**・行
書き込みの**直前**に呼ぶ:

- `insert_row_unchecked`（生 `RowInput`）
- `insert_rows_unchecked`（生 `RowInput` バッチ）
- `insert_typed_row_unchecked`（型付き単一行）
- `insert_typed_rows_unchecked`（型付きバッチ。COPY・NoSQL `insert` op・SQL
  複数行 INSERT の共有入口）
- `upsert_typed_rows_unchecked`（`DO UPDATE` の merge 後の行・新規挿入行の
  双方。`DO NOTHING` 分岐は書き込みが発生しないため対象外）
- `update_row_unchecked`（全行置換 UPDATE）
- `update_row_columns_unchecked`（単一行・列指定 UPDATE。SET 対象列が制約列と
  交差しない場合も含め常に検査する——多層防御として省略しない）
- `update_rows_where_unchecked`（述語つき UPDATE。候補ごとに検査）

`replace_typed_rows_by_text_key`（増分インデックス反映・TASK-120）は本 Issue の
スコープ外とし、UNIQUE 制約を持つテーブルへのファイル形 INSERT を fail-closed
に拒否する（サイレントバイパスにしない。§スコープ外参照）。

**判定順序（規範）**: 台帳照合・追記（`ledger::record_in_txn`）→ 一意性検査 →
行書き込み → 世代 bump → commit。違反時は commit 前に `Err` を返し write txn を
drop する（台帳エントリを含め副作用ゼロ）。同一 `operation_id`・同一内容の再送は
台帳照合が一意性検査より先に走るため、値そのものが UNIQUE 制約と衝突していても
常に台帳由来の判定が優先される。

### D3. 等価性・NULL・対象型

NULL は衝突しない（NULLS DISTINCT）。複数列制約はいずれかの列が NULL の行は
検査対象外。

キーは `ScalarRef` からの正規バイト列（型タグ 1 バイト + 型別の固定長／長さ前置
バイト列。`catalog::push_unique_key_component`）で比較する。対象型
（`catalog::is_unique_constraint_eligible`）: `TEXT`・`INTEGER`・`BIGINT`・
`BOOLEAN`・`DATE`・`TIMESTAMP`・`BYTEA`・`NUMERIC`（列固定 `scale` の下での
`unscaled` 比較）・`UUID`・`ENUM`。

対象外: `VECTOR`（検索専用列）・`REAL`／`DOUBLE PRECISION`（`row_codec::Value`
の encode 側は非有限値・`-0.0` を正規化するが、この不変条件は encode 経路の
検証でありコンストラクタレベルでは強制されないため、ビット比較キーの安全性を
構造的に保証できず fail-closed に対象外とした——計画段階では「NaN・`-0.0` の
曖昧さ」を理由に別の根拠で対象外としていたが、実装時に `Value` の doc を確認し
「encode 側の検証止まりで Rust API 経由の構築を禁止しない」点を採用理由として
確定した）・`JSON`／`JSONB`／配列型（要素単位の等価性を定義しない）。

### D4. カタログ永続化（v4）

`TableSchema` に非公開フィールド `unique_constraints: Vec<UniqueConstraint>`
（`pub fn unique_constraints(&self) -> &[UniqueConstraint]`・`pub(crate) fn
with_unique_constraints`）を追加。`UniqueConstraint::columns()` は宣言順の列名
リスト。

カタログ形式: **UNIQUE 制約を 1 つ以上持つスキーマのみ新バージョン `v4`**
（列の物理配置部分は v3 と同じ 5 フィールド形式を再利用し、末尾に `uniq:<n>` 行
＋ `n` 個の `U:<col>[,<col>]*` 行を追記）で書く。制約なしスキーマは従来どおり
v2／v3（バイト列不変）。v2/v3/v4 は互いに排他な正規形（v3 は墓標 1 件以上必須・
v4 は制約 1 件以上必須。制約を持つ v4 は墓標の有無を問わない）。

`decode_schema_body`・軽量パーサー `catalog_value_references_enum_type`（`DROP
TYPE` の依存判定が使う。v4 の `uniq:` セクションを読み飛ばさないと「宣言列数を
超える残り行は拒否」判定が誤発火し、UNIQUE 制約を持つ全テーブルで `DROP TYPE`
の依存判定が壊れるため対応が必須だった）の両方を v4 対応済み。

`validate_schema`（`validate_unique_constraints`）が検証: 参照列の存在（生存
列）・対象型・制約内列重複・同一列集合の制約重複（宣言順のまま比較する実装
既定値。列順が異なる制約は別制約として許容する）・上限（`MAX_UNIQUE_
CONSTRAINTS` = 32 制約／テーブル・`MAX_UNIQUE_CONSTRAINT_COLUMNS` = 32 列／
制約）。

### D5. DDL 経路

**SQL `CREATE TABLE`**（`sql/allowlist.rs::Parser::parse_create_table`）: 列制約
`<col> TEXT UNIQUE` と表制約 `UNIQUE (<col>[, <col>]*)` を受理する。表制約は
「識別子 `UNIQUE` の直後が `(`」で判定し（`is_table_unique_constraint_start`）、
列名としての `unique` と区別する。列参照の解決（未宣言列・`VECTOR` 列参照）は
全列が出揃った後にまとめて行う。現行の `CREATE TABLE` パーサーは `TEXT`／
`VECTOR(N)` の 2 型のみを受理するため、UNIQUE 対象は実質 `TEXT` 列のみになる
（`is_unique_constraint_eligible` が定める広い対象型は主に Rust API・将来の型
拡張向け）。

**制約追加（受入基準 3）**: `Storage::alter_table_add_unique_constraint(table,
&[&str])`（Rust API 専用。SQL `ALTER TABLE ... ADD UNIQUE` は対象外——後述）。
単一 write txn 内でテーブル全行をテナントごとに独立して走査し重複を検出、1 件
でもあれば `CatalogError::UniqueConstraintViolation` で拒否（副作用ゼロ・カタログ
不変・世代不変）。走査は物理キー順（`(tenant_id, id)`）でテナント境界ごとに
検査用キー集合をリセットする。

`alter_table_drop_column`: 制約に含まれる列の削除は
`CatalogError::DependentObjectsStillExist`（`2BP01`）で fail-closed に拒否する
（暗黙 cascade しない）。

### D6. エラー契約

`TenantWriteError::UniqueViolation`・`SqlSurfaceError::UniqueViolation`
（固定文言 `duplicate key value violates unique constraint`。値・列名・テナント
を含めない）を追加し、いずれも既存の `ErrorClass::UniqueViolation`（`23505`／
`UNIQUE_VIOLATION`）へ写像する。

**計画からの変更点**: 当初案は「台帳由来の `23505` と行制約由来の `23505` を
`ErrorClass` レベルで label 分離する」ことを検討していたが、実装時に
`error_format.rs` の `wire_codes_are_pairwise_distinct` テスト（`ErrorClass::
ALL` の全 `wire_code` が pairwise に異なることを機械的に固定する不変条件）と
衝突することが判明した。PostgreSQL 自身も行制約由来・値制約由来いずれの一意性
違反も同一 SQLSTATE `23505` で返すため、この 1 wire_code=1 label という既存
アーキテクチャの制約は仕様として妥当と判断し、`IdConflict`／
`DuplicateOperationId` が既に同一 `ErrorClass::UniqueViolation` を共有している
のと同じ扱いに揃えた。判定順序（D2）により、台帳由来の重複は引き続き
`TenantWriteError::DuplicateOperationId`（固定文言で区別可能）として観測され、
UNIQUE 制約由来の重複と Rust の型レベルでは区別できるが、wire_code／HTTP
`code` レベルでは両者とも `23505`／`UNIQUE_VIOLATION` のまま。

## スコープ外・申し送り

- SQL `ALTER TABLE ... ADD [CONSTRAINT] UNIQUE` / `DROP CONSTRAINT` と制約名
  （`ALTER TABLE` の SQL 骨格〔#900〕に依存）
- 永続一意索引（redb 二次テーブル）による O(log n) 判定
- `replace_typed_rows_by_text_key`（増分インデックス反映。TASK-120）への結線。
  UNIQUE 制約を持つテーブルへのファイル形 INSERT は明示的に fail-closed 拒否
  （サイレントバイパスにしない）
- COPY（`sql/copy.rs`）は `insert_typed_rows_unchecked` を経由するため本 Issue
  の範囲内で検査対象（追加実装は不要）
- `UPSERT` の `ON CONFLICT` 対象列（`id` 固定）への UNIQUE 列拡張
- `REAL`／`DOUBLE PRECISION`／`JSON`／`JSONB`／配列型への UNIQUE 拡張
- NoSQL 表層の DDL op（`create table` 相当）・wire-server 経由の専用結合テスト
  （`wire_unique_constraint.rs` 相当。engine 層 A テスト
  〔`crates/engine/tests/unique_constraint.rs`〕が SQL 表層〔`EngineCore::
  execute_sql_in_session`〕経由で検証済みのため、NoSQL 表層は `execute_bound_
  insert_in_session`／`execute_bound_update_in_session` が同一の書き込み
  プリミティブを共有する既存の設計により同じ契約を継承する）
- `PRIMARY KEY`（#903）による本検査点の再利用

## 検証

- `crates/engine/tests/unique_constraint.rs`: `CREATE TABLE` の列制約・表制約
  構文、単一列・複合列・NULL 許容、テナントスコープ（可視・不可視を問わない
  母集合を含む）、バッチ内重複・副作用ゼロ、台帳優先、UPDATE の自己比較除外、
  UPSERT の `DO UPDATE`／新規挿入分岐、`Storage::alter_table_add_unique_
  constraint` の拒否・成功・事後強制、`alter_table_drop_column` の依存検査
- `crates/engine/src/catalog.rs` 単体テスト: v2/v3 の既存ゴールデンテスト（バイト
  列不変）は無変更のまま green
- `crates/engine/tests/table_generation_bump_coverage.rs`: `catalog.rs` への
  行追加に伴う ALLOWLIST の行番号追随（`Storage::create_enum_type`／
  `drop_enum_type` の commit 呼び出し。挙動・判定ロジックは無変更）
