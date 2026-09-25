# 明示トランザクション BEGIN / COMMIT / ROLLBACK（Issue #942）

- ステータス: **Accepted（実装既定値）**
- 対応: Issue #942・TASK-221
- ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-31・
  `docs/spec/04-behavior/persistence.md` RECOVER-12・
  `docs/spec/04-behavior/error-format.md` ERR-6
- 関連ポインタ: RECOVER-5・RECOVER-6・RECOVER-8・TASK-96・TASK-97・TASK-99・
  TABLE-3・SQL-18（0 行 DELETE は台帳非記録）・WIRE-16（複数文実行。
  `wire-multi-statement.md`）・WIRE-19（`ReadyForQuery` 状態バイト。#943 が担当）

## 背景・目的

各書き込み文（`INSERT`／`UPDATE`／`DELETE`／`TRUNCATE`／UPSERT）は、`tenant.rs`
の `*_unchecked` 関数の中で `Storage::begin_write_txn()` → 書き込み →
`commit_boundary::commit` まで自己完結していた（autocommit）。`BEGIN` などは
許可リストに無いため `42601` で拒否され、複数文メッセージ（Issue #938・
`wire-multi-statement.md`）では書き込み文は最後の 1 文に限られていた。

本 Issue は、簡易クエリ・拡張クエリの両経路で `BEGIN`〜`COMMIT`／`ROLLBACK`
を受理し、複数の書き込み文を 1 つの redb 書き込みトランザクションにまとめて
原子的に commit／破棄できるようにする。

## 設計の要点（判断記録）

### 1. `BEGIN` の時点で redb の write txn を取得して保持する

`redb::WriteTransaction`（`=4.2.0`）はライフタイム引数を持たず、全フィールドが
`Arc`／`Mutex`／`Atomic` なので `Send`。`Drop` は abort 相当（panicking 中を
除く）。単一ライタ（TABLE-3）なので、`BEGIN` で write txn を取得すれば
`COMMIT`／`ROLLBACK` までの間に他者は commit できない。台帳
（`recovery::ledger::record_in_txn`）も同じ write txn 内で書かれるため、
「台帳エントリが `COMMIT` と同じトランザクションで永続化され、`ROLLBACK`・
接続断で消える」（RECOVER-12）は追加コードなしで成立する。

代償: 読み取り専用のトランザクションでも単一ライタを占有する。既知のトレード
オフとして記録する。

### 2. writer gate（待機上限付き。ロック待ちで `55P03`）

`crates/engine/src/storage/writer_gate.rs`。`Mutex<GateState>` と `Condvar` で
構成し、`GateState` は `{ held_by: Option<ThreadId> }`。

- **autocommit 経路**（`Storage::begin_write_txn`）: ゲートを待機上限つきで
  取得し、`redb` のライタを得た直後にゲートを手放す（占有時間は redb 呼び出し
  1 回分のみ）。超過時は `StorageError::WriteLockTimeout`（`55P03`）。
- **明示トランザクション経路**（`Storage::begin_explicit_write_txn`）:
  ゲートを取得したまま `WriterPermit`（RAII）としてトランザクションと同じ
  寿命で保持する。
- **デッドロックが起きない理由**: ゲートは 1 段しかなく、redb 自身のロックへ
  待機付きで到達できるのはゲートを取得できたスレッドだけ。明示トランザクション
  保持中の autocommit はゲートの段で待たされる（上限で打ち切り）ため redb の
  `begin_write` に届かない。
- **自己デッドロックの防御**: 明示トランザクションを保持しているスレッド自身が
  再度ゲートへ到達した場合は待たずに即座に `WriteTxnHeldByCurrentSession`
  エラーを返す（`55P03` へ写像。書き込み経路の配線漏れに対する多層防御）。

### 3. `tenant.rs` の書き込み本体を `WriteTarget` で共有する

`pub(crate) enum WriteTarget<'a> { Autocommit(&'a Storage), InTxn(&'a redb::WriteTransaction) }`
と `WriteTarget::with_txn`（`f: impl FnOnce(&WriteTransaction) -> Result<T, E>`
を受け取り、`Autocommit` は `begin_write_txn` → `f` → `commit`、`InTxn` は
`f` の結果をそのまま返す）を導入した。対象は次の 4 関数のみ:

- `insert_row_unchecked`（Rust API の行形 INSERT）
- `insert_rows_unchecked`（同・バッチ）
- `insert_typed_row_unchecked`（SQL 表層の単一行 typed INSERT。
  `sql::exec::execute_insert_with_schema_in` 経由）
- `truncate_table_unchecked`（`TRUNCATE TABLE`。
  `sql::exec::execute_truncate_in` 経由）

**スコープの意図的な縮小**: 複数行 `VALUES`・ファイル形 INSERT・`UPSERT`・
`UPDATE`（単一行・述語形）・`DELETE`（単一行・述語形）・COPY は本 Issue の
明示トランザクション対応に含めない（`0A000` で拒否）。これらは既存の autocommit
専用書き込み関数（`insert_typed_rows_unchecked`・`upsert_typed_rows_unchecked`・
`update_row_unchecked`・`update_row_columns_unchecked`・`delete_row_impl`・
`delete_rows_where_unchecked`・`update_rows_where_unchecked`）を `WriteTarget`
対応へ分割していない。理由は実装コストと検証範囲を Issue #942 の予算内に
収めるためであり、対応拡大は別 Issue の対象とする。

**空バッチの挙動差**: `insert_rows_unchecked` の空バッチ（`rows.is_empty()`）は
旧実装では `write_txn` を drop（abort）していたが、`WriteTarget::with_txn`
経由では `Ok(())` を返した後にラッパーが `commit` を呼ぶ（autocommit 経路）。
行を 1 件も書かず世代も進めないため観測可能な挙動は変わらないが、実際の
redb commit 呼び出しが 1 回増える（cosmetic な差異として記録する）。

### 4. 状態機械（`crates/engine/src/sql/transaction.rs`）

`SessionTransaction<'e>`（`enum TxnState<'e> { Idle, Active(Box<ActiveTxn<'e>>),
Failed { session_at_begin: SessionState } }`）。`ActiveTxn` は
`write_txn: redb::WriteTransaction`・`permit: WriterPermit`・`started_at`・
`statements`・`seen_operation_ids`・`written_tables`・`has_writes`・
`session_at_begin` を保持する。

- **`BEGIN`**: `Idle` → `Active`（`Storage::begin_explicit_write_txn` 経由）。
  `Active` 中の再 `BEGIN` は `25001`（`ActiveSqlTransaction`）で `Failed` へ
  遷移。`Failed` 中は `25P02`。
- **`COMMIT`**: `Idle` は `25P01`（`NoActiveSqlTransaction`）。`Active` は
  `has_writes` なら `commit_boundary::commit`、無ければ `drop`（abort）して
  `Idle` へ。`Failed` は `25P02` のまま（`ROLLBACK` のみ受理し続ける）。
- **`ROLLBACK`**: `Active`／`Failed` いずれからも `Idle` へ戻り、`BEGIN` 時点の
  `SessionState`（`SET search_mode`・`CREATE FUNCTION` 等）を復元する
  （PostgreSQL の挙動に準拠する実装判断。spec は沈黙）。`Idle` からの
  `ROLLBACK` は `25P01`。
- **通常の文**: `Idle` は既存 autocommit（`execute_parsed_in_session`）を
  そのまま通す（挙動は不変）。`Failed` は実行せず `25P02`。`Active` は
  上限検査 → `operation_id` 再利用検査 → 種別判定 → 実行、の順。
- **`Active` 中のどの文でエラーが起きても**、その場で `write_txn` を abort
  し permit を解放して `Failed`（ロックは保持しない）へ遷移し、元のエラーを
  そのまま返す。Failed 状態から `COMMIT` はできないため、失敗した文が残した
  部分書き込みが永続化されることはない。**redb の savepoint は不要**。
  構文・許可リスト検証のエラー（`execute_sql_in_txn` の `parse_sql` 失敗）、
  簡易クエリの複数文分割・位置検証のエラー、拡張クエリプロトコルの
  Parse／Bind／Describe／Execute のエラー応答も同じく `Failed` へ遷移させる
  （PostgreSQL と同じく、エラーの種類を問わない。PR #1041 レビュー指摘）。
  `SessionTransaction::fail` は `Active` 以外では状態を変えない（冪等）ため、
  wire 層はエラー応答のたびに状態を問わず呼べる。
- **同一トランザクション内での `operation_id` の再利用**: 台帳照合より前に
  `seen_operation_ids` と照合し、一致すれば `25000`
  （`InvalidTransactionState`）で `Failed` へ遷移する。台帳は自トランザクションの
  未 commit エントリを見て `23505` を返してしまうため、それより先に検査する。
  テーブルを問わず同じ ID を拒否する（保守的な側に倒す）。

### 5. 上限（実装既定値。spec 由来の数値ではない）

- `max_duration`（`BEGIN` からの経過時間の上限）: **20 秒**
- `max_statements`（トランザクション内の文数の上限）: **1,000**
- 超過時は `54000`（`PayloadTooLarge`）で `Failed` へ遷移する。
- `lock_wait`（他セッションが writer gate を待つ上限）: **30 秒**
  （`Storage::DEFAULT_WRITE_LOCK_WAIT`。既存 `READ_TIMEOUT` と同じ値）。
  超過時は `55P03`。

### 6. トランザクション内の読み取り（既知の逸脱）

読み取り経路（`sql::exec::execute_statement`・`scan::execute_scan`・
`aggregate::execute_aggregate`、各キャッシュ）は具体型の `&redb::ReadTransaction`
に深く依存しており、「自トランザクションの未 commit 変更を読む」機能を完全に
実装するのは本 Issue の予算を超えると判断した。

**採用案（fail-closed）**: トランザクション内で**まだ書き込んでいないテーブル**
を読む文（`SELECT`／`Aggregate`／`Scan`／`EXPLAIN`）は、従来の経路（BEGIN 時点
のスナップショットと厳密に一致する。単一ライタにより保証される）で実行する。
**同じトランザクションで既に書き込んだテーブル**（`written_tables`）を読む文は
`0A000` を返し `Failed` へ遷移する（黙って古い結果を返すことはしない）。
書き込みの結果を読み戻したい場合は `RETURNING` を使う（本 Issue のスコープ外。
書き込み系文自体が明示トランザクション内で `RETURNING` を受理していない）。

これは SQL-31 の要件からの**既知の逸脱**である。

## `#943` との分担

`ReadyForQuery` の状態バイト（`'I'`／`'T'`／`'E'`。WIRE-19）は `#943` が担当
する。本 Issue では engine 側で `SessionTransaction::status() ->
TransactionStatus` の照会 API を公開するまでとし、wire-server の `'I'` 固定の
送出は変更しない。

## wire-server への結線

- `handshake.rs::post_auth_loop` が接続単位の `Option<SessionTransaction<'e>>`
  を `session` と並べて保持する（`engine.map(|e| e.new_session_transaction())`）。
  未 commit のまま接続が切れた場合、共有 `write_txn` は commit されずに abort
  され、`WriterPermit` も解放される。
- `simple_query.rs::execute_and_respond`／`run_statement` は
  `engine.execute_sql_in_session` の代わりに `engine.execute_sql_in_txn` を
  呼ぶ（`txn` が `Idle` の間はビット同一の挙動）。
- `statement_splitter::check_write_placement(stmts, initially_in_txn)` が
  `TransactionControl` 遷移を模擬し、`BEGIN` を含む複数文メッセージでは書き込み
  文の位置制約を緩和する（詳細は `wire-multi-statement.md` 参照）。
- 明示トランザクション中の `COPY` は `0A000` で拒否し、トランザクションを
  `Failed` へ遷移させる（`crate::copy::run` へは委譲しない）。`Failed` 中の
  `COPY` も autocommit として実行せず `25P02` で拒否する（`Idle` のときだけ
  `crate::copy::run` へ委譲する。PR #1041 レビュー指摘）。

## 検証

- `crates/engine/tests/sql31_transaction.rs`: BEGIN/INSERT/INSERT/COMMIT の
  可視性・ROLLBACK の完全巻き戻し・入れ子 BEGIN の `25001`・トランザクション外
  COMMIT/ROLLBACK の `25P01`・同一トランザクション内 `operation_id` 再利用の
  `25000`・対応外文の `0A000`・文数上限の `54000`・TRUNCATE と INSERT の原子的
  commit・接続断相当（drop）での完全ロールバックを固定。
- `crates/wire-server/tests/`: 既存の複数文・COPY・エラー射影テストが
  `check_write_placement` のシグネチャ変更・`SqlOutcome` の新 variant 追加後も
  無変更のまま green（回帰なし）。
- `crates/wire-server/tests/err4_http_projection.rs`・
  `crates/wire-server/docs/nosql-api.md`（`nosql_api_doc.rs`）: 新規 5 分類
  （`55P03`／`25000`／`25001`／`25P01`／`25P02`）を NoSQL 表層からの到達不能
  分類として追加し、production の応答エンコーダ経由で射影のみを固定する
  （NoSQL 表層の `op` 許可リストにトランザクション制御が無いため）。

## 対象外・申し送り

- `ReadyForQuery` の状態バイト `I`／`T`／`E`（WIRE-19）→ `#943`。
- トランザクション内での自トランザクションの未 commit 変更の可視化
  （上記「既知の逸脱」）。
- 複数行 INSERT・ファイル形 INSERT・UPSERT・`UPDATE`・`DELETE`・COPY の
  明示トランザクション対応（現状 `0A000`）。
- `DUPLICATE_OPERATION_ID` と `UNIQUE_VIOLATION` の `code` ラベルを wire 上で
  区別すること → TASK-227（ERR-6 の横断事項）。
- 暗黙トランザクション（WIRE-16）による複数文の書き込み位置制約の完全撤廃
  （`BEGIN` を含まないメッセージは引き続き「書き込みは最後の 1 文のみ」）。
- savepoint、分離レベルの指定、`START TRANSACTION`／`END`／`ABORT` などの別名。
- NoSQL 表層のトランザクション（設計上 `0A000`。`op` 許可リストに追加しない）。
- 先頭文が 0 行 DELETE の場合に RECOVER-12 の再送判定が成立しない制約
  （SQL-18 の既存契約〔0 行 DELETE は台帳非記録〕との相互作用）。
- 上限の既定値（20 秒・1,000 件・30 秒）の確定 → オーナー判断。
