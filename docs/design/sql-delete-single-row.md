# `DELETE`（単一行・id 指定）の設計判断

Issue #867・対象ビヘイビア: SQL-18（TASK-191）。関連ポインタ: RLS-9・RLS-10（他テナント
存在情報の非漏えい）・RECOVER-1〜3・RECOVER-4（対象行不存在／他テナント所有を区別
しない `NotFound`）・RECOVER-10（台帳照合による再送判定）・TABLE-12（テナント名前空間
キー）。

spec 本文は転記しない（`.claude/rules/spec-confidentiality.md` 準拠）。本ドキュメントは
本リポ側の実装判断・設計記録のみを扱う。

## 構文

```
DELETE FROM <table> WHERE id = <n> USING OPERATION_ID '<id>'
```

- `id` 疑似列の等価指定のみを受理する（`AND` 結合・`id` 以外の列に対する述語・
  `WHERE` 省略はいずれも許可リスト外・`42601`）。述語つき `DELETE`（SQL-19 系）は
  別 Issue の管轄で、本 Issue の対象外。
- `INSERT`（TASK-80・SQL-10）・`TRUNCATE`（TASK-195・SQL-22）と同じ「文末専用句
  `USING OPERATION_ID '<id>'`」規範を踏襲する。省略・明示 `NULL` はいずれも
  `LedgerMode::Ledgered`（既定）で `23502`。
- `EXPLAIN DELETE ...` は許可形状に存在しないため、先頭トークンが `EXPLAIN` の場合は
  既存の `EXPLAIN` 分岐（次トークンが `SELECT` であることを要求）へ流れて自然に
  `42601` になる（`INSERT`・`TRUNCATE` と同じ経路）。

## 実行結線

許可リスト（`sql::allowlist::ValidatedDelete`・`validate_delete`／`validate_delete_tokens`。
Issue #864）・束縛（`sql::parser::BoundDelete`・`bind_delete`。Issue #864）は前提 Issue で
実装済み。本 Issue は両者と既存の `tenant::delete_row_unchecked`（TASK-95・RECOVER-4）を
結線する:

`core.rs::execute_sql_in_session` の先頭トークン覗き見判定（`INSERT`・`TRUNCATE` と同型）
→ `sql::allowlist::validate_delete_tokens` → `sql::parser::bind_delete` →
`sql::exec::execute_delete`（新設）→ `tenant::delete_row_unchecked`。

`EngineCore::execute_delete_sql`（セッション非経由の直接エントリポイント）・
`execute_delete_form`（`execute_delete_sql`・`execute_sql_in_session` 共有の実行本体）を
`execute_truncate_sql`／`execute_truncate_form` と同じ設計で追加した。`VectorCore` trait
への昇格は行わない（`execute_insert_sql`・`execute_truncate_sql` と同じ理由）。

## 応答: 削除件数を返す（`TRUNCATE` との対比）

`DeleteOutcome { rows_affected: u64 }`（`sql::exec`）は `0` または `1` のみを取る。
`TruncateOutcome`（件数を一切返さない空構造体）とは意図的に異なる設計にした——
`DELETE` の `rows_affected` は「このセッションのテナントが所有する、この特定の
`id` に対する削除が成功したか」という自テナント内の 2 値情報のみを表し、他テナントの
行数推測には使えない（TRUNCATE の「対象テナントが何件行を持っていたか」という
定量的な推測材料になり得る件数とは性質が異なる）。wire 応答は pg 互換の
`CommandComplete` タグ `DELETE <rows_affected>`（`INSERT 0 <rows>` と同じ設計）へ
写像する（`wire-server::simple_query`）。

## 0 行成功への写像と台帳非記録（安全側の設計判断）

`tenant::delete_row_unchecked` は「対象行が不存在」と「対象行が存在するが他テナント
所有」を **区別せず** `TenantWriteError::NotFound` を返す契約（TASK-95・RECOVER-4。
`tenant.rs` モジュールドキュメント参照。既存の Rust API 契約であり本 Issue では変更
しない）。`sql::exec::execute_delete` はこの `NotFound` を **エラーとして伝播せず**
`Ok(DeleteOutcome { rows_affected: 0 })` へ写像する（`map_insert_write_error` の `_`
アーム〔`XX000`〕には渡さない）。

`NotFound` の場合、`delete_row_unchecked` 内の write トランザクションは早期 `return`
により commit されず破棄されるため、**台帳への tentative 追記・テーブル世代の進行は
いずれも発生しない**。これは `TRUNCATE`（0 件でも必ず台帳記録・世代進行が発生する
非対称設計。`truncate-table.md` 参照）とは意図的に非対称になっている。

判断根拠:

1. 他テナント保持 id・未存在 id のいずれでも、成否・件数・`wire_code`・文言・副作用が
   完全に同一になる（RLS-9・RLS-10。テナント存在情報の非漏えい）。
2. `delete_row_unchecked`（TASK-95）の既存 Rust API 契約（`recover4_*` テスト群が
   固定する「不存在と他テナント所有を区別しない」契約）を変更しない。
3. 0 行 DELETE の再送は台帳非記録のため、同一 `operation_id` を再送しても常に同じ
   0 行成功へ収束する（冪等）——`23505` にならない（`TRUNCATE` の 0 件時とは異なる
   選択だが、`DELETE` は「対象が無ければ何もしない」操作として自然な冪等性を保つ）。

**#865（UPDATE 実行結線）への申し送り**: 同じ判断（0 件時の台帳非記録）を UPDATE 側
でも揃えるかどうかは #865 側の実装判断として委ねる。`update_row_unchecked` も同じ
「不存在／他テナント所有を区別しない `NotFound`」契約を持つため、対称的な設計に
揃えることが自然だと考えられる。

## 台帳照合の優先順位（RECOVER-4・RECOVER-10）

`delete_row_unchecked` は台帳照合（内容一致／不一致判定）を所有権判定
（`owns_existing`）より **前** に行う（TASK-101・RECOVER-10。`tenant.rs` ドキュメント
参照）。`content_hash::for_delete(id)` は削除対象 `id` のみを内容とするため、

- 使用済み `operation_id` を **同一 `id`** へ再送 → 内容一致 → `23505`
  （対象行は既に削除済みで `NotFound` になり得る状態だが、台帳照合が先に働くため
  重複commitとして検出される）
- 使用済み `operation_id` を **異なる `id`**（他テナント保持 id・未存在 id を含む）へ
  再送 → 内容不一致 → `22023`（`NotFound` ではない）

のいずれかへ確定的に収束し、未使用の `operation_id` に対してのみ所有権判定
（`NotFound`〔0 行成功へ写像〕）へ到達する。`crates/engine/tests/sql_delete_single_row.rs::
delete_resending_used_operation_id_hits_ledger_before_ownership_check` が固定する。

## 削除スコープ: 「所有（`is_owner`）」（`TRUNCATE` と同じ、可視性とは独立）

`delete_row_unchecked` の判定は `(tenant_id, id)` キー（TABLE-12）＋ `is_owner` の
二重防御であり、RLS 可視性（`is_visible`）ではない。`TRUNCATE` と同じ「テナント所有」
スコープをそのまま採用する（他テナントの `Public` 行は可視でも所有でないため削除
不能 → 0 行成功）。

wire 経由では RLS-11（TASK-195）により自テナント行は常に可視のため、可視集合との
差は engine 直呼び出しの既定 ctx（`Public` のみ）で自テナント `Private` 行を削除する
場合にのみ現れる（この場合も `is_owner` は可視性ラベルを問わず自テナント判定のため
削除は成功する）。production コードはこの点について変更しない。

## RECOVER-10: 台帳キー空間は INSERT と共有

`DELETE` の台帳キーは `(tenant, table, operation_id)` で `INSERT`・`TRUNCATE` と同一
空間を共有する。`crates/engine/tests/sql_delete_single_row.rs::
delete_shares_ledger_key_space_with_insert` が以下を固定する:

- `INSERT` で使用した `operation_id` を同一内容ではない `DELETE` へ再利用 → `22023`
- `DELETE` で使用した `operation_id` を同文再送 → `23505`
- `DELETE` で使用した `operation_id` を異なる内容の `INSERT` へ再利用 → `22023`

## キャッシュ失効

`DELETE`（1 行成功時）は `bump_table_generation_in_txn` を経由するため、既存の
テーブル単位世代整合キャッシュ（`SqlArenaCache`・`VisibleBitmapCache`・
`SparseIndexCache`・`HnswIndexCache`・`ScalarIndexCache` 等）はすべて自然に失効する。
`sql_delete_single_row.rs::delete_invalidates_arena_and_visible_bitmap_caches`・
`delete_invalidates_hnsw_index_cache_and_excludes_deleted_row` が固定する（後者は
`MIN_INDEXED_ROWS`〔1,024〕未満のフィクスチャのため常に brute-force `fallbacks`
経路を通ることを踏まえ、索引キャッシュ経路そのものが DELETE 後も非 vacuous に
動作し続けることを確認する）。0 行 DELETE（対象不存在）はテーブル世代を進めない
ため、この場合はキャッシュも失効しない（意図した契約であり、対象行が無い以上
キャッシュ内容自体は正しいまま）。

## NoSQL 表層は対象外

`op` 許可リスト（`search`／`scan`／`aggregate`／`insert` の閉じた語彙。NOSQL-1・
NOSQL-9）に `delete` は含まれない。NoSQL 側への `delete` op 追加は別 Issue
（#875・#876）の担当。

## 対象外（申し送り）

- NoSQL `delete` op・SQL/NoSQL パリティ検証（#875・#876・#877）
- `RETURNING` 句（#873）
- 述語つき `DELETE ... WHERE <非 id 述語>`（#870・#871）
- 複数行 `operation_id` 設計（#868）
- 層 B の 3 クライアント e2e（`extended_syntax_e2e.rs`）への `DELETE` 追加
- `fault_injection.rs` の DELETE 版 post-commit panic 注入（`TRUNCATE` と同じ扱いで
  対象外のまま）
