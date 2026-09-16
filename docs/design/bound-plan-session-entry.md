# 束縛済み scan／aggregate 計画を実行する EngineCore のセッション対応エントリ

- Issue: #728
- 対象タスク: TASK-186（内訳「単一 `Storage` 構成での直接束縛と `EngineCore` の
  共存方式の確認・結線」）
- 対象ビヘイビア: NOSQL-3／NOSQL-4／NOSQL-5（関連 SQL-13／SQL-14／SQL-15）
- ステータス: Implemented

## 背景

`EngineCore` の SQL テキスト非経由の実行入口は SQL テキストを一切介さない値と
してのみ提供されていた（`sql::scan::execute_scan`／
`sql::aggregate::execute_aggregate`。それぞれ Issue #726・#727 で `pub` 昇格）。
しかしいずれも `&redb::ReadTransaction` を要求し、これは `Storage::db()`
（`pub(crate)`）経由でしか得られない。`EngineCore::from_storage` が `Storage`
を所有するため、engine クレート外の呼び出し元（`wire-server` の NoSQL 表層。
TASK-175・TASK-177 のポインタ）は production プロセス内で束縛済み計画を実行
できなかった（既存の公開 API テスト `sql_scan_public_api.rs`／
`sql_aggregate_public_api.rs` は `Storage` を drop して生 `redb::Database` を
再オープンする回避策を使っており、redb は同一ファイルの二重オープンを許さない
ため production では成立しない構成）。

本 Issue は、単一の `Storage` を `EngineCore` が所有したまま、SQL テキストを
経由せずに束縛済み scan／aggregate 計画を `PolicyContext`（RLS 暗黙適用）付き
で実行するセッション対応エントリを `EngineCore` に追加する。

## 採用した設計: binder closure 方式

`BoundScan`／`BoundAggregate` はスキーマ（列 index）に依存する。呼び出し元
（クレート外）は `EngineCore` からスキーマを取る公開手段を持たない。仮に別
トランザクションでスキーマを取って束縛すると、`ALTER TABLE ADD COLUMN` との
競合で旧スキーマの計画を新スナップショットへ適用する障害を再導入する（単一
スナップショット契約。Issue #56 の指摘対応の踏襲）。

そのため「束縛済み計画」は **同一 `read_txn` 内で呼ばれる binder closure の
戻り値** として受け取る:

```rust
pub fn execute_bound_scan_in_session<F>(
    &self,
    ctx: &PolicyContext,
    session: &SessionState,
    table: &str,
    bind: F,
) -> Result<QueryResult, SqlSurfaceError>
where
    F: FnOnce(&TableSchema, &UdfRegistry) -> Result<BoundScan, SqlSurfaceError>;

pub fn execute_bound_aggregate_in_session<F>(
    &self,
    ctx: &PolicyContext,
    session: &SessionState,
    table: &str,
    bind: F,
) -> Result<QueryResult, SqlSurfaceError>
where
    F: FnOnce(&TableSchema, &UdfRegistry) -> Result<BoundAggregate, SqlSurfaceError>;
```

処理順（両者共通）:

1. `table` のスキーマを同一トランザクションで取得する（`bind` はまだ呼ばない）。
   テーブル不存在は SQL 経路と同一の `UndefinedTable` へ丸め込む。
2. `bind(&schema, session.udfs())` を呼び、束縛済み計画を得る。`Err` はそのまま
   伝播する（engine 側でエラー分類を変えない）。
3. 得られた計画の対象テーブルが `table` と一致するか検証し、不一致は
   fail-closed に拒否する（他テーブル向けに束縛された計画を別スキーマで
   実行する事故への多層防御）。
4. 実行する。

設計上のポイント:

- `session` は `&SessionState`（不変借用）で足りる。scan／aggregate は
  `udfs()` しか読まず、`SearchMode`／`precision` は関与しない。
- RLS は既存の `execute_scan`／`execute_aggregate_with_cache` が `ctx` から
  暗黙適用する契約をそのまま使う（計画側の `rls_predicate_present` の有無に
  依存しない。新規のバイパス経路は作らない）。
- `bind` closure は `read_txn` が開いている間に呼ばれる。したがって呼び出し元
  の closure は純粋・軽量に保つ契約とする（I/O・LLM 呼び出し等の重い処理を
  closure 内で行わない）。

## engine 内の実行器一本化（リファクタ・挙動不変）

`execute_validated_in_session` の `Statement::Aggregate`／`Statement::Scan`
アームは、従来 `begin_read`・`get_table_schema_in_txn` を inline していた。
これは既存 `read_txn_with_schema`（`USING PLAN`／`EXPLAIN` アームが既に使って
いた private helper）と同一のエラー写像のため、両アームをこの helper へ置換
したうえで、実行本体を `run_scan_plan`／`run_aggregate_plan`（`core.rs` の
private メソッド）へ抽出した。SQL 経路は
`read_txn_with_schema` → `bind_scan`／`bind_aggregate` → `run_*_plan` に、
新エントリは `read_txn_with_schema` → closure → `run_*_plan` になり、
トランザクション・スキーマ・キャッシュ配線が 1 か所に閉じる。新エントリは
公開ラッパー `sql::aggregate::execute_aggregate`（キャッシュ非経由）と異なり、
`run_aggregate_plan` が配線する `VisibleBitmapCache`（Issue #478）・
`SqlArenaCache`（Issue #363）・`ScalarIndexCache`（Issue #473）を SQL 経路と
共有する。

## 却下した設計案

| 案 | 内容 | 却下理由 |
| --- | --- | --- |
| A | wire-server が `sql::exec::execute_statement`／`execute_scan`／`execute_aggregate` を直接呼ぶ | `&redb::ReadTransaction` を得る公開手段が無い（`Storage::db()` は `pub(crate)`・`EngineCore` が `Storage` を所有・redb は同一ファイルの二重オープン不可）。得られたとしてもキャッシュ非経由で SQL 経路と性能・挙動が乖離し、txn・スキーマ・キャッシュ配線を wire-server 側へ複製する＝第 2 の実行器になる。責務境界（プロトコル処理は wire-server、コアロジックは engine）にも反する |
| B | `EngineCore` から `Storage`／`db()`／`begin_read` を露出 | A と同じ複製問題に加え、永続化ハンドルのカプセル化を破り `core-api-check` の到達性契約を広げる |
| C | 束縛済み計画を値渡しで受ける `execute_bound_*(ctx, session, &BoundScan)` ＋ スキーマ公開アクセサ | スキーマ取得と実行が別トランザクションになり単一スナップショット契約（Issue #56）を破る。安全にするには列 index／名／型の再検証や世代照合が必要で API 面積が膨らむ。将来必要になれば本エントリの上に薄く足せる |

## スコープ外

- `BoundAggregate::new`（SQL テキスト非経由の直接構築 constructor）:
  `AggregateInput`／`ExprProgram`／`ProjectionColumn`／`BoundGroupBy` の公開設計
  が必要。TASK-177（aggregate 写像）へ申し送り。aggregate エントリが SQL
  テキスト非経由で外部から使えるのは TASK-177 以降になる。
- search（`BoundStatement`）向けの同型エントリ: 同じ `read_txn` の壁に当たるが
  TASK-186 の対象外。TASK-175 へ申し送り。
- `execute_insert`／`build_explain_result` の公開: Issue #730。

## テスト

`crates/engine/tests/core_bound_plan_entry.rs`（単一 `Storage` 構成。生 redb
再オープンを使わない）:

- RLS 暗黙適用（他テナントの `Private` 行が非漏えい）
- 未定義テーブルは binder 呼び出し前に拒否（binder が呼ばれないことを固定）
- binder のエラーがそのまま伝播する
- 束縛済み計画の対象テーブルが要求テーブルと不一致なら拒否
- 集計エントリが SQL テキスト経由（`execute_sql_in_session`）と `Cell` レベル
  で一致し、`VisibleBitmapCache` を共有する（2 回目の呼び出しでキャッシュヒット
  することを確認）
- `GROUP BY` 付き集計で他テナント専有のグループ値が非漏えい
- binder closure がセッションに登録済みの UDF レジストリを観測できる

既存回帰（無変更で green を確認）: `sql_scan.rs`・`sql_scan_public_api.rs`・
`sql_aggregate.rs`・`sql_aggregate_public_api.rs`・`sql_group_by.rs`・
`scalar_index_aggregate.rs`・`sql_surface.rs`・wire-server `wire_scan.rs`
（7 件）・`wire_aggregate.rs`（7 件）。

`crates/engine/tests/bound_plan_public_api.rs`（Issue #729。単一 `Storage` 上で
SQL 経由の実行と束縛済み計画経由の実行が混在しても結果が一致し、テナント境界
（RLS-7・RLS-8）を破らないことを固定）:

- scan: `SELECT` の複数形（`WHERE`・`SELECT *` を含む）で `id` ソート後の行が
  両経路で完全一致（`LIMIT` が可視行数を下回る場合は SQL-15 の順序保証なし
  契約に合わせ件数・可視範囲のみ比較）
- 集計（`GROUP BY` なし）: `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`・UDF（`vec_norm`）
  経由の式項目が固定オラクルと両経路で一致
- `GROUP BY`／`HAVING`／`ORDER BY ... LIMIT`: 順序込みで両経路が固定オラクルと
  一致
- 書き込み（`insert_row`）を挟んだ世代進行の前後で両経路が同時に新しい世代を
  反映し、`VisibleBitmapCache` が実際にヒットした状態（非 vacuous）であることを
  確認
- 一方のテナントを束縛経路、他方を SQL 経路で交互実行（往復）しても
  `Visibility::Private` 行が他テナントへ非漏えい（`Visibility::Public` は
  テナント非依存の全体公開契約どおり両テナントから見える）
- `Arc<EngineCore>` を複数スレッドで共有し SQL 経路・束縛済み経路を交互実行
  しても単一スレッド参照結果と一致
- 同一の拒否入力（`SUM(embedding)`・`HAVING` での `GROUP BY` キー列比較）に
  対する `wire_code` が両経路で一致

再実測した wire 回帰の pass 件数（本 Issue 時点で無変更）: `wire_scan.rs`
7 件・`wire_aggregate.rs` 7 件、いずれも pass。
