# `CREATE VIEW` / `DROP VIEW`（非マテリアライズド）の設計判断

Issue #909・対象ビヘイビア: TABLE-18・SQL-23（TASK-205）。関連ポインタ:
RLS-10 (b)（複数のテーブル参照を持つ読み取り経路での暗黙適用）・ERR-6
（`42P07`／`2BP01`／`42809` の新設）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 概要

保存したクエリをビューとして登録し、`FROM` に書いて読めるようにする。
**非マテリアライズド**とし、参照のたびに定義を展開する。ビュー定義は既存の
許可リストパーサーをそのまま通したもの（許可リストの部分集合）に限って保存し、
参照時は保存済みの定義を**同じパーサーで再検証**する。第 2 の SQL パーサー・
実行器は作らない。

## 受理する構文（Phase 1）

```
CREATE VIEW <name> AS SELECT <* | 列名リスト> FROM <table | view> [WHERE <単純述語> [AND ...]]
DROP VIEW <name>
```

- body の `LIMIT`・`ORDER BY ... <=>`・`HYBRID`・`USING PLAN`・集計形・式項目
  （`Projection::Items`）・UDF 呼び出し述語（`WherePredicate::PredicateCall`／
  `Expression`）・`EXPLAIN`・`CREATE OR REPLACE`・`IF NOT EXISTS`・`TEMP`／
  `MATERIALIZED`・列別名リスト・`WITH CHECK OPTION`・末尾の余剰トークンは
  いずれも構造的に受理しない（`42601`）。使える単純述語は
  `Equality`／`Prefix`／`BoolEquality`／`BoolColumn`／`Compare`（列 対
  リテラル）のみ。
- `DROP VIEW` は単一ビュー名の指定のみを受理し、`IF EXISTS`・
  `CASCADE`／`RESTRICT`・複数名指定は `42601`。
- 拡張クエリプロトコルの `$n` を含む `CREATE VIEW`／`DROP VIEW` は Parse 時点で
  拒否する（DDL はバインドパラメータを受け付けない PostgreSQL の慣習に揃える）。

## ビューを参照するクエリ（Phase 1 のスコープ）

広域取得（SQL-15。`Statement::Scan`）のみをビュー展開の対象とする。
**集計（`Statement::Aggregate`）・ベクトル検索（`Statement::Select`）・
`EXPLAIN` はビュー展開の対象外**とし、意図的にスコープを縮小した
（実装時間の制約による判断。集計はいずれ同じ `resolve_from` を再利用できる
設計だが、追加の列スコープ検査コードが必要なため Phase 1 では見送った）。
ベクトル検索・`EXPLAIN` の対象はそもそも spec 上も対象外（ビューは
ランキング段を持てない）。これらの経路は `lookup.table_exists(view_name)` が
そのまま `false` を返すため `42601` ではなく `42P01`（未定義テーブル）に
fail-closed で落ちる——ビューの存在の有無に関わらず同一の応答になるため
情報漏えいにはならないが、PostgreSQL 的な「操作対象が違う」区別
（`42809` 相当）ではない点は既知の簡略化として記録する。

## 展開方式（検証段階での書き換え）

`sql::allowlist::validate_sql_tokens` の `Statement::Scan` 分岐にある
`lookup.table_exists(&shape.table_name)` を `sql::view::resolve_from` へ
置き換えた。

- `TableLookup` トレイトへ既定実装付きの `view_definition` メソッドを追加した
  （既定 `Ok(None)`）。既存の `TableLookup` 実装（テスト用モック等）は無変更の
  ままコンパイルできる。`catalog::Storage` のみが実データを返す。
- `resolve_from` は連鎖（ビューがビューを参照する）を内側から畳み込みながら
  実テーブルへ到達するまで辿り、`WHERE` 述語を `内側のビュー ++ 外側のビュー
  ++ クエリ自身` の順で合成し、最も外側（クエリが直接参照した）ビューが
  公開する列集合（`SELECT *` なら無制限）を決定する。
- 畳み込み後の文は通常のテーブル参照に対する `ValidatedScan` と完全に同じ形に
  なり、束縛（`sql::parser`）・実行（`sql::scan`）・RLS 適用（既存の暗黙適用
  経路）はすべて無変更のまま通る。

## RLS-10 (b) の不変条件

保存するビュー定義（`catalog::ViewDef`）は `tenant_id`／`PolicyContext`／
作成者の情報を一切含めない。展開後の文は、参照したセッションの `ctx` を使う
既存の実行経路でしか評価されないため、作成者の可視性が参照者へ引き継がれる
ことは構造的に起こらない（`table18_view.rs::
view_read_applies_referencing_session_rls_not_creator_visibility` で
3 テナント対照検証済み）。

## カタログ（redb）

- 新規テーブル `VIEWS_TABLE`（キー: ビュー名、値: `直接の参照先名 + 正規化
  body SQL` の 2 フィールド blob）。
- **body は検証済み AST を正規化して描画した SQL で保存する**
  （`sql::allowlist::render_view_body`）。「描画 → 再トークン化 →
  `parse_view_body`」が元と等価な AST を復元することを round-trip テスト
  （`sql/view.rs` 内）で固定した。
- 名前空間はテーブルと共有する: `Storage::create_view` は同一 write
  トランザクションで `CATALOG_TABLE`／`VIEWS_TABLE` の両方を確認し、衝突は
  `42P07`。`Storage::create_table` にも `VIEWS_TABLE` の確認を追加した。
- 上限（実装既定値）: ビュー総数 10,000（`MAX_LIST_TABLES` と同じ）、body
  64 KiB、ネスト深さ 4（テーブル自身を深さ 0 とする）。超過は `54000`。
- すべての検査（名前衝突・参照先の存在・ネスト深さ・依存オブジェクト）は
  それぞれの操作の write トランザクション内で行う（TOCTOU 回避）。

## ネスト深さと循環

- 循環は正常な経路からは構造的に作れない: `CREATE VIEW` は参照先の存在を
  作成前に要求するため自己参照は `42P01` になり、何も永続化されない。
  他から参照されているビュー・テーブルの `DROP` は `2BP01` になるため、
  「削除して作り直す」ことで循環を組めない。`CREATE OR REPLACE VIEW`・
  `ALTER VIEW` は `42601`。
- それでもカタログ破損に備え、`resolve_from`・深さ計算のいずれも visited
  集合と上限付き反復（ループ。再帰なし）を使う。循環を検出した場合は
  `XX000`（固定文言）、上限超過は `54000`。

## 判定順序（fail-closed）

`DROP TABLE`（Issue #902）と同じ判定順序に揃えた: 構文検証（カタログ非照会）
→ `require_ddl_permission`（`42501`。テーブル・ビューの存在有無を問わず
一律拒否）→ write トランザクション内でのカタログ判定（名前衝突・参照先存在・
深さ・件数上限）→ 保存。

- `CREATE VIEW`: 名前衝突 `42P07` → 参照先不存在 `42P01` → ネスト深さ・
  件数上限 `54000` → 保存。
- `DROP VIEW`: 名前がテーブルなら `42809`、存在しなければ `42P01` →
  依存するビューがあれば `2BP01` → 削除。
- `DROP TABLE <ビュー名>`: `42809`。参照しているビューがあれば `2BP01`。
- **ビューへの書き込み**（`INSERT`／UPSERT／`UPDATE`／`DELETE`／`TRUNCATE`）は
  `42809`。`EngineCore::parse_tokens` の書き込み分岐（`validate_insert_tokens`
  等）が `UndefinedTable` を返し、かつその名前がビューとして存在する場合に
  限り `WrongObjectType` へ読み替える（`EngineCore::
  reclassify_write_to_view_error`。1 か所のみ）。副作用（台帳・世代・行）が
  一切発生しないことをテストで固定した。

## 情報漏えいの抑止

保存済み body の再検証・展開が失敗した場合（カタログ破損・非互換の将来
フォーマット変更）は `XX000` の固定文言（`invalid view definition`）にまとめ、
body のリテラル値・破損理由の詳細をクライアントへ運ばない
（`sql::view::corrupt_view_error`）。列名（`22000 unknown column`）は既存の
束縛エラーと同じ扱い（カタログ情報であり秘匿情報ではない）。

## 対象外・申し送り

- 集計形の body、LIMIT／`ORDER BY` 付きの body。
- ビューを対象にした集計（`Statement::Aggregate`）・ベクトル検索
  （`Statement::Select`）・`EXPLAIN`（上記「ビューを参照するクエリ」節参照。
  ベクトル検索・`EXPLAIN` は仕様上も対象外、集計は実装時間の制約による
  スコープ縮小）。
- 複雑な述語（`PredicateCall`／`Expression`）のビュー越し列スコープ検査
  （`sql::view::check_columns_within_view` は `column` フィールドを持つ単純
  述語のみ検査する。RLS 境界には影響しない——書き換え後の述語は基底テーブル
  へ委譲される既存の束縛・実行経路でそのまま評価されるため、ここでの検査は
  利便性のためのものであり安全性の境界ではない）。
- 更新可能ビュー。
- NoSQL 表層の `scan`／`aggregate` op でビューを対象にすると従来どおり
  `42P01`（fail-closed のまま）。NOSQL-13 の DDL op はスコープ外。
- FOREIGN KEY 由来の `2BP01`（#907）。
- `SELECT *` の展開は参照時点で動的に行う（作成時点の列集合で固定する
  PostgreSQL 互換の方式は採らない）。
- 参照時の展開は Parse 時点で確定する（拡張クエリで Parse から Execute までの
  間にビューが削除されても、Parse 時点の定義で実行される。ビュー自体は権限を
  運ばないため権限昇格にはならない）。
- PR #1041（明示トランザクション。SQL-31）とのトランザクション内 DDL 拒否
  ガードの統合はマージ順に依存する申し送り事項。
