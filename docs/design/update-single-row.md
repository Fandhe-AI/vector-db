# `UPDATE`（単一行・id 指定）の設計判断

Issue #865・対象ビヘイビア: SQL-17（TASK-191。実行結線）。関連ポインタ:
RECOVER-1／2／4／10（`operation_id` 必須化・台帳照合による再送判定）・
TABLE-12（テナント名前空間キー）・RLS-7／9／10／11（RLS 暗黙適用・他テナント
存在情報の非漏えい・read-your-writes）・ERR-1／ERR-2／ERR-4（`wire_code` 契約）。
許可リスト検証・束縛（`sql::allowlist::ValidatedUpdate`・`sql::parser::BoundUpdate`）
は Issue #864 で実装済み。本 Issue はその後段（書き込み経路への結線・
`operation_id` 契約の適用）を扱う。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
UPDATE <table> SET <col> = <lit>[, <col> = <lit>]* WHERE id = <n>
USING OPERATION_ID '<id>'
```

単一行・`id` 等価指定形のみを受理する（述語形 `WHERE`・複数テーブル・サブクエリ・
`RETURNING` は許可リスト外）。`id`／`tenant_id`／`visibility` を SET 対象にすることは
許可形状の時点で `42601` として拒否する（疑似列・RLS 内部列であり、クライアントが
行の所有者・可視性を書き換える経路を作らないため）。`EXPLAIN UPDATE ...` は許可形状に
存在しないため、先頭トークンが `EXPLAIN` の場合は既存の `EXPLAIN` 分岐（次トークンが
`SELECT` であることを要求）へ流れて自然に `42601` になる（`INSERT`／`TRUNCATE` と
同じ経路）。

## 判断 A: 列指定の書き込み入口を新設する（第 2 の書き込み経路ではない）

`tenant::update_row_unchecked`（既存の Rust API。全行置換 `RowInput` を要求）へ
直結せず、同一の書き込みプリミティブ（`validate_identifier`・
`require_table_schema_write`・`content_hash`・`ledger::record_in_txn`・
`user_rows_table_def`・`decode_row_for_key`・`encode_row`・
`bump_table_generation_in_txn`・`recovery::commit_boundary::commit`）だけで組み立てた
`tenant::update_row_columns_unchecked`（`pub(crate)`）を新設した。

`BoundUpdate.assignments` は宣言順を保持した**部分更新**の表現（列インデックス・
値のペア列）であり、SET で指定されなかった列は一切現れない。対して
`update_row_unchecked` が要求する `RowInput` は全列を埋めた全行置換形であるため、
そのまま渡せない。

read（対象行の既存 `metadata`／`embedding`）→ merge（SET 対象列だけを上書き）→
encode → write を**単一の write トランザクション内**で行う設計は必須である。
別 read トランザクションで先に読んでから `update_row_unchecked` へ渡す 2 段構成に
すると、同一行への並行 UPDATE（列が互いに素）で read スナップショットと write の
間に他セッションの commit が挟まり lost update が起きる
（`crates/engine/tests/sql_update_single_row.rs::sequential_disjoint_column_updates_both_persist`
が両方の変更が残ることを固定する）。

前提: 対象行の既存 `metadata` は `row_codec::encode_scalar_columns` が書いた正規
レイアウトであることを要求する。SQL 表層の `INSERT`・型付き挿入 API はすべてこの
経路を通るため本番では常に満たされるが、旧フォーマットの raw metadata（全行置換版
`RowInput` を直接構築する非 SQL 呼び出し元専用の Rust API）が書いた行を対象にした
場合は `decode_scalar_columns` が構造不整合を検出し、格納済みデータの破損・
実装不整合として `CatalogError::CorruptSchema`（`sql::exec::map_write_error`
経由で `XX000`）で fail-closed に拒否する（クライアント入力エラー `22000` には
丸めない。codex-review P1 指摘・PR #989）。

`EngineCore::update_row`（全行置換の既存 Rust API）・`tenant::update_row`（同）は
無変更のまま維持する。

## 判断 B: 0 行更新でも台帳記録・世代進行・commit は必ず行う

対象行が「他テナントの行」「未存在 id」「所有者ではあるが RLS 可視集合外
（`PolicyContext::is_visible` が false）」のいずれであっても、行書き込みだけを
スキップし、台帳記録（内容照合ハッシュの計算・記録）・テーブル世代進行・
commit は 1 行更新の場合とまったく同じ手順で行う（`truncate_table_unchecked` と
同じ非対称設計）。

`update_row_unchecked`（全行置換 API）の既存契約は `NotFound` を早期 return
（write トランザクションを commit せず drop）する形だが、列指定の新経路では意図的に
これと異なる契約を採用した。理由: 0 行更新の早期 return を許すと、(a) fsync の
有無でレイテンシが大きく変わり応答時間から対象行の有無が推測できてしまう
（RLS-10）、(b) 台帳が残らないため「0 行更新の再送」が重複コミット（`23505`）として
検知できず、RECOVER-7 系の再送ベース回復確認と整合しない。

`crates/engine/tests/sql_update_single_row.rs::zero_row_update_still_records_the_ledger_entry`
が、0 行更新後の同一 `operation_id` 再送が `23505` になることで台帳記録の非
vacuous 性を固定する。`zero_row_update_is_identical_across_not_found_reasons`・
`zero_row_update_when_owner_but_rls_excludes_visibility` が、対象不在の理由に
関わらず応答（`Ok(rows_affected: 0)`）が同一であることを固定する。

対象行の特定は「テナント名前空間キー（TABLE-12）の物理 lookup ∩ `is_owner` ∩
`is_visible`」の積で判定する（`update_row_unchecked` の `is_owner` 単独判定より
厳格。実装判断: 読み取り経路の RLS 暗黙適用〔RLS-7・RLS-10〕と判定を揃え、可視集合
外の行は 0 行更新扱いにする）。書き込む行の `tenant_id` はサーバー側の `ctx` から、
`visibility` は既存行の値を維持する（クライアントは両者を SET 対象にできない。
許可形状の `42601` と多層防御）。

## 判断 C: 内容照合ハッシュはクライアント入力のみから計算し、専用 `OpTag` を使う

既存 `content_hash::for_update_encoded(id, encoded_row)`（全行置換 API 用）は
マージ後の全行をハッシュするため DB 状態に依存する。列指定 UPDATE は台帳記録を
所有権判定より**前**（`update_row_unchecked` と同じ順序契約。RECOVER-10）に行う
必要があり、この時点ではまだ行を読んでいないため、DB 状態非依存のハッシュ入力が
必須になる。

新設 `content_hash::for_update_columns(id, columns: &[(&str, &Value)])` は、`id` と
SET 句の（列名, 値）ペアを**宣言順のまま**（呼び出し元は並べ替えない）連結する。
列の位置ではなく名前で連結する理由（`ALTER TABLE ADD COLUMN` 耐性）は
`push_named_scalar_columns` と共有する。`VECTOR` 列の SET も `Value::Vector` として
同じ経路に入る。`OpTag::UpdateColumns`（既存 `OpTag::Update` とは別 variant）を
専用に割り当て、ドメイン分離する。

帰結（`crates/engine/tests/sql_update_single_row.rs` で固定）:

- 同一文の再送（0 行更新後を含む）→ `23505`
- SET 値の違い・`id` の違い・SET 句の列宣言順の違い → `22023`
- 同一 `operation_id` を INSERT で使った後に UPDATE で再利用 → `22023`
  （`OpTag::Insert`／`OpTag::UpdateColumns` の分離により実質的な内容一致でも
  必ず内容不一致として拒否される）

## エラー写像: `map_write_error` の操作名パラメータ化

`execute_insert`／`execute_insert_batch`／`execute_truncate` が共有していた
`map_insert_write_error`（`TenantWriteError` → `SqlSurfaceError`）を、呼び出し元の
操作名を追加パラメータとして受け取る `map_write_error(e, op)` へ切り出した。
`wire_code` 自体は不変だが、`CatalogError::Invalid`／`StorageError::Codec` アームの
detail 文言（`"{op} rejected: invalid row"`）に操作名を埋め込む。UPDATE の SET
列値の不正・スキーマ不一致を「insert が拒否された」という誤った文言でクライアントへ
返さないための変更（`client_message()` はこの detail をそのままクライアントへ
含める）。`execute_update` は新設の `op = "update"` 呼び出しとして本経路に乗る
（対象行の既存 metadata デコード失敗は `CorruptSchema` として別経路の catch-all
`XX000` へ分類され、この detail 文言は付与されない）。

あわせて `execute_delete`（Issue #983 で `map_insert_write_error` を暫定使用して
いた既存箇所）も `map_write_error(e, "delete")` へ切り替え、DELETE 失敗時に誤って
「insert が拒否された／失敗した」と返していた既存の不整合を本 Issue で解消した
（UPDATE 追加のついでに DELETE 側の呼び出しも `op` パラメータ化本体へ揃えた形。
`wire_code` 自体は不変）。

`execute_truncate` は本 Issue のスコープ外のため `map_insert_write_error`
（固定文言 `"insert"`）の呼び出しを変更していない。TRUNCATE 失敗時の detail が
引き続き `"insert rejected"`／`"insert failed"` になる既存の不整合は本 PR でも
未解消のまま残る（新規のリグレッションではなく現状維持。是正は別 Issue の担当）。

`map_insert_write_error` は `map_write_error(e, "insert")` の薄いラッパーとして
残し、`execute_insert`／`execute_insert_batch`／`execute_truncate` の既存呼び出しは
無変更のまま維持する。

## wire 応答: `CommandComplete` タグ

`UpdateOutcome { rows_affected: u64 }`（0 または 1）を pg 互換の `CommandComplete`
タグ `UPDATE <n>` へ整形する。`INSERT <oid> <rows>` と異なり OID フィールドを
持たない（PostgreSQL の `UPDATE` タグ規範に準拠）。

## `SqlOutcome::Update` の追加（BREAKING CHANGE）

`sql::SqlOutcome` は `#[non_exhaustive]` でないため、`Update` variant の追加は
破壊的変更として扱う（`Truncate` 追加時〔TASK-195〕と同じ扱い）。網羅 match の
更新箇所は `core.rs::execute_sql`（`Select`／`Aggregate`／`Scan` の 3 アーム）・
`wire-server::simple_query`。

## スコープ外（本 Issue で対処しないもの）

- NoSQL 表層 `update` op（`op: update` は引き続き `0A000`）
- 述語つき `UPDATE ... WHERE`（複数行の条件付き更新）・複数行内容照合の設計
- `RETURNING`・UPSERT
- `DELETE`（単一行）の実行結線（`update_row_columns_unchecked` の「0 行同一化」
  パターンを再利用できる設計）
- `fault_injection.rs::is_committed_insert` の UPDATE 対応
- `EngineCore::update_row`（全行置換 Rust API）の意味論変更
- 0 行／1 行経路のレイテンシ分布実測: 台帳記録・世代進行・commit（fsync）まで
  1 行経路と完全に同一の手順を踏む「構造上の同一性」（判断 B）により、`wire`
  応答からのタイミング差は生じない設計だが、実測による定量的確認は行っていない
  （`docs/design/wire-tenant-row-id-scope.md`〔Issue #738〕のハーネスを UPDATE
  向けに拡張すれば計測可能）
