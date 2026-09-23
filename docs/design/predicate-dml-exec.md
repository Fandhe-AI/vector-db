# 述語つき UPDATE/DELETE の実行結線（Issue #871）

- 対象 Issue: #871（親 #861「Phase 1 書き込み DML の表層公開」・ルート #860）
- 前提 Issue: #869（述語つき `UPDATE ... WHERE` の許可リスト・束縛）・#870（述語つき
  `DELETE ... WHERE` の許可リスト・束縛。`docs/design/predicate-dml-where.md`・
  `docs/design/delete-predicate-form.md`）
- 関連 ADR: `docs/design/multi-row-dml-operation-id.md`（Issue #868・RECOVER-11。
  **ステータス Proposed・オーナー承認待ち**。本実装は自動運転モードのため承認待ちを
  待たず、この ADR を現時点の最有力案＝作業前提として実行結線した）
- 関連ポインタ: `docs/spec/05-tasks.md` TASK-192・`docs/spec/04-behavior/sql-surface.md`
  SQL-19・`docs/spec/04-behavior/recovery.md` RECOVER-11（検討中）・RECOVER-9・
  RECOVER-10・`docs/spec/04-behavior/rls.md` RLS-7・RLS-9・RLS-10・TABLE-12。
  spec 本文は転記しない。
- 関連コード: `crates/engine/src/recovery/content_hash.rs`（`OpTag::UpdateWhere`／
  `DeleteWhere`・`for_update_where`／`for_delete_where`）・`crates/engine/src/tenant.rs`
  （`PredicateDmlOutcome`・`PredicateDmlError`・`DmlCandidate`・
  `delete_rows_where_unchecked`／`update_rows_where_unchecked`）・
  `crates/engine/src/sql/exec.rs`（`execute_predicate_delete`／
  `execute_predicate_update`・`map_write_error`）・`crates/engine/src/core.rs`
  （`execute_sql_in_session` の `DELETE`／`UPDATE` 分岐・
  `execute_predicate_delete_form`／`execute_predicate_update_form`）

## 1. 背景・目的

述語つき `UPDATE ... WHERE`（#869）・`DELETE ... WHERE`（#870）は許可リスト検証・
束縛まで実装済みだったが、実行結線（候補行列挙・1 トランザクション一括適用・
台帳照合・影響行数上限の実測判定）が存在せず、`EngineCore::execute_sql_in_session`
は述語形 `DELETE` を `42601` で拒否し、`UPDATE` 文自体は先頭トークン分岐を持たな
かった（`validate_sql` へフォールスルーし `42601`）。本 Issue はこの実行結線を追加
する。

## 2. 内容照合ハッシュ（RECOVER-11）

ADR §4 のレイアウトをそのまま `crates/engine/src/recovery/content_hash.rs` へ実装
した（`for_update_where`／`for_delete_where`）。ADR からの意図的な変更点は 2 点:

1. **計算位置**（ADR §5.1 は `bind_update_form`／`bind_predicate_delete` 内での計算
   を推奨するが、`BoundPredicateDelete`／`BoundPredicateUpdate` のコンストラクタが
   既に固定シグネチャを持ち、将来 NoSQL 表層（#876）が生の `WherePredicate`／
   `UdfRegistry` を経由しない別入口を持つ設計であるため）: `core.rs::EngineCore`
   （`Validated*` 形と `session.udfs()` の両方を持つ唯一の呼び出し元）が束縛の直前
   に 1 回だけ計算し、`sql::exec::execute_predicate_delete`／
   `execute_predicate_update` へ `&ContentHash` として渡す。
2. **エラー型**（ADR §4.2 は `StorageError` を示すが）: `sql::allowlist::
   SqlSurfaceError` を直接返す。WASM UDF 呼び出しの拒否（ADR §4.4.1）は
   `sql::allowlist::SqlSurfaceError` に `0A000` 相当の variant が存在しないため
   `SqlSurfaceError::unsupported`（`42601`）へ写像する（`0A000` は NoSQL 表層の op
   許可リスト専用。`grep -n '"0A000"' crates/engine/src/sql/allowlist.rs` で不在を
   確認済み）。

その他のレイアウト（`OpTag::UpdateWhere = 9`／`DeleteWhere = 10`・`SET` 割当の宣言順
直列化・`WHERE` 述語の種別タグ付き直列化・`Expr` のタグ付き前置順直列化・参照 UDF
定義セクションの推移閉包・WASM UDF 呼び出しの拒否判定順序）は ADR §4.3／§4.4／
§4.4.1 のとおり実装した。

## 3. 実行契約（ADR §6）

`tenant.rs::delete_rows_where_unchecked`／`update_rows_where_unchecked` は以下の順序
を 1 write トランザクション内で守る:

1. `begin_write_txn` → スキーマ取得（`expected_schema` 照合で並行 `ALTER TABLE` を
   検知）。
2. `ledger::record_in_txn`（候補列挙より**先**。使用済み `operation_id` は可視集合を
   一切走査せず `23505`／`22023` へ短絡する）。
3. テナント**所有**スコープ（`(tenant, 0)..=(tenant, u64::MAX)`。`is_owner` の二重
   防御）を走査し、呼び出し元が注入した述語クロージャで候補 `id` を確定する
   （`limit + 1` 件で打ち切り）。
4. `limit` 超過なら `write_txn` を drop し `LimitExceeded` を返す（行・台帳とも
   痕跡ゼロ）。
5. 候補 `id` をすべて適用（DELETE は `remove`、UPDATE は read-merge-write。
   `tenant::upsert_typed_rows_unchecked` の `DoUpdate` 腕と同型の組み立て）。
6. 影響行数が 1 件以上のときのみ `bump_table_generation_in_txn`。
7. `commit_boundary::commit`。

`WHERE` 述語の評価は `sql::exec::execute_predicate_delete`／
`execute_predicate_update` が候補列挙クロージャとして注入し、`sql/scan.rs::
execute_scan` の走査ループと同一の意味論（`declarative_filter::matches_all` → 各
`expr_filters` を `ExprProgram::eval`。`references_embedding && dim == 0` の行は無条
件除外）を共有する（第 2 の述語評価器を作らない）。

## 4. 削除・更新スコープ

候補列挙は「RLS 可視行」ではなく「テナント**所有**（`(tenant_id, id)` キー名前空間
＋ `is_owner`）」スコープを対象とする（単一行 DELETE・`TRUNCATE` と同じ判断。
`docs/design/sql-delete-single-row.md`「削除スコープ」節参照）。

- wire 経由では RLS-11（認証主体は自テナント `Private` 行が可視）により「所有 ⊆
  可視」が成立し、両者は一致する。
- 他テナントの `Public` 行は「可視だが所有ではない」ため候補にならない
  （`SELECT` では見えるが述語つき `UPDATE`／`DELETE` の対象外）。
- engine 直呼び出しの既定 ctx（`Public` のみ）でも自テナント `Private` 行は候補に
  なる（単一行 DELETE と同じ）。
- `enumerate_dml_candidates` は `execute_scan`／`execute_aggregate` と同型の
  「テーブル全体を `.iter()` で全走査し、`ctx.is_owner` 判定は行ヘッダの
  デコード後に行う」実装である（redb の複合キー `(&str, u64)` の部分範囲
  指定を避けるための既存踏襲。TABLE-12・security.md）。他テナント行も
  `decode_row_header`／`decode_row_dim_and_metadata_borrowed`（または
  `needs_embedding` 時は `decode_row_body_into`）でデコードされたうえで
  `is_owner` 判定により除外される——「キー範囲の外にあるためデコードすら
  行わない」わけではない。不可視行の内容（embedding・metadata・述語評価
  結果）が呼び出し元・応答へ一切露出しない、という秘匿性の受け入れ条件は
  `is_owner` 判定による除外で満たされるが、走査・デコードそのものの回避は
  性能上の最適化課題であり本 Issue のスコープ外（§9「スカラー列二次索引
  による候補削減の適用」参照）。

`WHERE visible() のみ` の述語つき DELETE は #870 の既存決定（自テナント全行を候補
にする。歯止めは影響行数上限のみ）をそのまま継承し、本 Issue で再決定していない
（`docs/design/delete-predicate-form.md`「記録する判断」節参照）。述語つき UPDATE の
`WHERE visible() のみ` は #869 の既存契約どおり束縛段で `42601` のまま。

## 5. エラー優先順位

`core.rs::execute_predicate_delete_form`／`execute_predicate_update_form`:

構造検証・`operation_id` 必須化・カタログ存在確認（`sql::allowlist::
validate_delete_statement_tokens`／`validate_update_form_tokens` が呼び出し元で
既に適用済み） → スキーマ取得（`42P01`。並行 `DROP TABLE` の防御的経路） → 束縛
（`22000`。`sql/scan.rs` と同じ失敗点） → 内容照合ハッシュ計算（WASM UDF 呼び出し
の拒否・`42601`） → 実行本体（台帳照合 `23505`／`22023`・上限超過 `54000`）。

## 6. 上限 API の並立（申し送り。ADR §6 の既存申し送り）

`DELETE` 側は `DEFAULT_MAX_DML_AFFECTED_ROWS`＋`check_affected_row_count(count,
limit)`、`UPDATE` 側は `MAX_DML_AFFECTED_ROWS`＋`check_dml_affected_rows(count)`と
いう、シグネチャの異なる 2 つの上限 API が並立している（いずれも
`crates/engine/src/sql/parser.rs`。値はいずれも 1,000）。両者の統合は本 Issue の対
象外のまま。

## 7. PR #989（#865 単一行 UPDATE 実行結線）・PR #991（RETURNING）との整合ルール

実装開始時点（origin/main `b790abd`）で PR #989（単一行 `id` 完全一致形 UPDATE の
実行結線）・PR #991（`RETURNING`）はいずれも未マージ（OPEN）だったため、本 Issue
は当初「PR #989 未マージ」の経路（計画 §4.6-B）で実装した。その後 origin/main への
追随（PR #989・PR #991 の順にマージ済み）により、本ブランチは両 PR の定義をそのまま
再利用する形へ整合させた:

- `exec::UpdateOutcome { rows_affected: u64 }`・`SqlOutcome::Update
  (exec::UpdateOutcome)`・`simple_query.rs` の `UPDATE <n>` アームは PR #989
  （#865）が導入した定義をそのまま再利用する（本 Issue が独自に導入していた
  同名定義は PR #989 マージ時に置き換え済み）。`core.rs::execute_sql_in_session`
  の `UPDATE` 分岐は `bind_update_form` の戻り値（`BoundUpdateForm`）を
  `Single` 腕（PR #989 の `execute_update_with_schema` へ委譲）・`Predicate`
  腕（本 Issue の `execute_predicate_update_form` へ委譲）へ振り分ける。
- `SqlOutcome::Update` の追加は PR #989 で **BREAKING CHANGE** として導入済み。
  本 Issue はこの型を変更しない。
- `DELETE` は `crate::sql::allowlist::DeleteStatement`（`SingleRow`／
  `Predicate`）で振り分ける。`SingleRow` 腕はさらに `RETURNING`（PR #991・
  Issue #873）の有無で `execute_delete_returning_form`（PR #991 導入）／
  `execute_delete_form`（既存）へ分岐し、`Predicate` 腕は常に
  `execute_predicate_delete_form`（本 Issue）へ委譲する。述語形 DELETE／UPDATE
  と `RETURNING` の組合せは、構造検証段（`sql::allowlist::
  validate_delete_statement_tokens`／`validate_update_form_tokens`）が
  `RETURNING` 併用を `42601` で拒否するため、実行結線側では到達しない
  （PR #991 が導入した契約をそのまま維持）。
- `crates/wire-server/tests/wire_error_response.rs::err1_update_returns_42601_fields`
  の入力へ `USING OPERATION_ID` を付与した（`validate_update_form_tokens` が
  `operation_id` 必須化ガードを構造検証の直後に行うため、欠落時は `42601` ではなく
  `23502` になる。この変更は PR #989 で正式に取り込み済み）。

## 8. テスト

- `crates/engine/tests/sql_predicate_dml_exec.rs`: 候補列挙・応答件数の同値性
  （DELETE／UPDATE）・RLS 境界（他テナント行の非影響・非漏えい）・0 行一致の台帳
  記録と再送拒否・内容照合ハッシュ（述語順入替での `22023`）・`WHERE visible()`
  のみの DELETE・`operation_id` 欠落・`execute_sql`（セッション無し）の既存拒否・
  影響行数上限超過（`54000`・副作用ゼロ。DELETE 側
  `predicate_delete_over_limit_is_rejected_with_no_side_effects`・UPDATE 側
  `predicate_update_over_limit_is_rejected_with_no_side_effects`——§6 の上限
  API 並立を踏まえ両者を独立に固定）。
- `crates/engine/tests/predicate_dml_failure_injection.rs`: 候補列挙途中の式評価
  エラー（0 除算）が write トランザクション全体を副作用ゼロで拒否すること（RLS 可視
  列は `TEXT` を算術に使えないため、疑似列 `id` の算術で誘発）・台帳未記録（同一
  `operation_id` の再利用が可能）・drop→再オープン後も整合。
- `crates/engine/tests/sql_delete_predicate_bind.rs::
  session_executes_predicate_delete_statement`: #870 が固定していた「まだ拒否され
  る」テストを「0 件一致で成功する」へ反転。

## 9. 申し送り・スコープ外

- NoSQL `update`／`delete` op の束縛・結線（#876）・SQL/NoSQL パリティ（#877）。
- 上限 API（§6）の統合・既定値の確定（オーナー判断）。
- ADR #868 の承認・spec 側 RECOVER-11 の確定化（オーナー作業）。
- `WasmUdfBackend` への安定な定義識別子の追加（wasmtime 接続時）。
- スカラー列二次索引（`sql::scalar_index`）による候補削減の適用（本 Issue は write
  txn 内の全走査で正しさを優先。性能改善は後続）。
- `EXPLAIN UPDATE/DELETE`・`OR`／括弧付き述語・層 B の 3 クライアント e2e への追加。
- `merge_row_assignments` の `tenant::upsert_typed_rows_unchecked` との共通化
  （本 Issue は predicate UPDATE 専用の read-merge-write をインライン実装した。
  重複コードの抽出は後続の任意リファクタ）。
- `tenant.rs` 内部の `#[cfg(test)]` 失敗注入シーム（`arena.rs` の先例と同型）は
  本 Issue の時間的スコープでは追加せず、公開 API 経由の式評価エラー注入のみで
  atomicity を検証した（§8 参照）。
