# NoSQL `insert` op の写像（NOSQL-6）

- Issue: #771・#772
- 対象タスク: TASK-178
- 対象ビヘイビア: NOSQL-6（関連: SQL-10・INDEX-4・RECOVER-1／2／3／10・TABLE-12・RLS-9）
- ステータス: Implemented

## 背景

`POST /v1/query` の `op: insert` を、SQL 表層の `INSERT ... USING OPERATION_ID`
（SQL-10）と同じ engine 書き込み契約へ写像する。SQL-10 の `execute_insert`
（`engine::sql::exec::execute_insert`）は 1 呼び出し = 1 行 = 1 台帳エントリの
単行 API であり、NoSQL 表層の `insert` op が要求する「`rows` 配列全体を 1 つの
`operation_id` に対応づけて書き込む」という意味論をそのままでは満たせない。

## 採用した設計

### engine 側: typed batch insert 経路の新設

- `recovery::content_hash::for_typed_insert_batch`（`pub(crate)`）: 既存
  `for_typed_insert`（単行）と同じフィールド組み立て（`push_u64(id)` →
  `push_u8(visibility)` → `push_vector` → `push_named_scalar_columns`）を、
  `for_insert_batch_encoded` と同様に件数プレフィクス付きで行ごとに連結する。
  1 つの `operation_id` が複数行を覆う操作として `OpTag::InsertBatch`
  を共有する。
- `tenant::insert_typed_rows_unchecked`（`pub(crate)`）: `insert_rows_unchecked`
  （生 `RowInput` 向け）と `insert_typed_row_unchecked`（型付き単行向け）を
  併せ持つ設計で、型付き値列の複数行を単一の write トランザクション・単一の
  台帳エントリへまとめる。バッチ内 `id` 重複はテナント名前空間内で検出する。
- `sql::exec::execute_insert_batch`（`pub`）: `bounds` が空・テーブル/`operation_id`
  混在は `22000` で拒否し、`bounds.len() == 1` は既存 `execute_insert` へ委譲
  する（単行の NoSQL insert が SQL-10 単行 INSERT と同一の台帳ハッシュ空間に
  属し、表層を跨いだ再送も同じ `(tenant, table, operation_id)` キーで判定
  される）。`operation_id` 必須化ガード（TASK-92・RECOVER-1）は関数内部で
  自己完結して適用する。
- `EngineCore::execute_bound_insert_in_session`（`pub`）: `execute_bound_scan_in_session`・
  `execute_bound_aggregate_in_session`（Issue #728）と同型の binder closure
  方式のセッション対応エントリ。判定順序は以下のとおり（fail-closed。順序が
  契約）:
  1. `operation_id` 必須化ガード（`23502`。スキーマ取得より前）
  2. 空バッチ拒否（`22000`）
  3. INDEX-4 ①（件数上限。「バッチあたり最大ファイル数」を「1 要求あたり
     最大行数」に読み替え。カタログ参照・束縛より前・`54000`）
  4. スキーマ取得 → 束縛（同一 `read_txn` 下。単一スナップショット契約）。
     束縛結果の各 `BoundInsert.operation_id` が判定 1 で検査した引数
     `operation_id` と一致することも検証する（不一致は `22000`。PR #823
     Bugbot 指摘: 6. の実書き込み（`execute_insert_batch`）は `bounds[0].operation_id` を台帳
     キーとして再解決するため、この一致検証がないと判定 1 のガードと
     実書き込みが異なる `operation_id` を使い得た）
  5. INDEX-4 ②③④（バイト量・チャンク数上限。1 行 = 1 チャンクとみなす。
     `54000`）
  6. 実書き込み（`execute_insert_batch`。独自の write トランザクション）

### wire-server 側: `http::query::insert`

- `bind_rows`／`bind_row`（純関数）: JSON `rows[*]` を `schema` の列順に
  対応する `BoundInsert` へ束縛する。`id` は `engine::json::JsonNumber`
  のうち小数点・指数部を含まない非負整数リテラル（`PosInt`。`f64` への
  丸めを経ないパース時点の分類で `u64::MAX` まで無損失に受理する）のみ
  受理し、`1.0` のような整数値に丸められる小数・指数表記・文字列形の
  `id` はいずれも拒否する（PR #823 レビュー指摘: `f64` 丸め後の値を
  `fract() == 0.0` 等で事後判定すると `2^53` 境界直上の整数が丸めで
  別の `id` へ書き込まれ得た）。
- `execute`: `table`／`rows`／`operation_id` を取り出し、`operation_id` は
  欠落・`null`・空文字のいずれも `OperationId::parse("")` に正規化して
  `23502` へ収束させたうえで `EngineCore::execute_bound_insert_in_session`
  へ委譲する。第 2 の実行器は作らない。
- テナントは `SessionPrincipal::policy_context()` のみから導出し、JSON・
  ヘッダからテナント相当の値を読む経路をシグネチャ上持たない。可視性は
  engine 側が常に `Private` 固定で書き込む（`execute_insert` と同じ判断）。

### 成功応答（Issue #772）

- `execute` の戻り値を `InsertOutcome` から `InsertSuccess`（`inserted`／
  `operation_id`。`execute` 内で 1 回だけ検証した `OperationId` をそのまま
  持ち回る）へ変更し、`encode_success_body`（`{"inserted":<n>,
  "operation_id":"<escaped>"}`。キー順固定・空白なしのコンパクト形）が
  唯一の情報源として本文を組み立てる。`operation_id` はクライアント要求の
  値をそのまま echo するため、`crate::http::error_body::
  escape_json_string_into` を通す（`"`／`\` の混入がありうるため。制御
  文字は `OperationId::parse` が既に拒否済みだが多層防御として統一する）。
  `InsertOutcome::incremental` は行形では常に `None`（ファイル形専用）の
  ため本文へは含めない。
- `handle`（`scan::handle`／`aggregate::handle` と同一シグネチャ形）を
  `gate.rs` 手順 5 の `(Op::Insert, Some(engine))` アームへ結線し、`search`
  のみが引き続き暫定 `0A000`／501 を返す状態にした。
- 行 `id` のテナント内スコープ契約（TABLE-12・RLS-9）の NoSQL 表層越し
  検証を `crates/wire-server/tests/nosql6_tenant_row_id_scope.rs` として
  追加（SQL wire 版 `wire_tenant_row_id_scope.rs::rls9_wire_insert_
  response_bytes_are_identical_...`／`table12_wire_insert_duplicate_...`
  の写し）: 他テナント（tenant-b）保持 id・未存在 id への自テナント名義
  `insert` の応答（`operation_id`／`Date` をマスクした後の全バイト列）が
  完全に一致すること、同一テナント内重複は `23505` で拒否され応答本文に
  他テナント名・行 id（重複対象自身の id を含む）が現れないことを固定。
  レイテンシ分布の区別不能性検証（SQL wire 版の層 B）は対象外（§対象外
  参照）。

## spec 側への申し送り事項

- `id` の JSON 数値表現域（`JsonNumber::PosInt` による `u64::MAX` までの
  無損失受理）は本リポ独自の実装既定値。文字列形 `id` の受理可否は spec 側の
  判断事項として申し送る。
- 空 `rows` の拒否（`22000`）・`n == 1` の SQL-10 委譲による表層横断の再送
  判定・INDEX-4 の「バッチ＝ファイル数」から「1 要求＝行数」への読み替えは、
  いずれも本リポ独自の実装既定値。

## 対象外（後続 Issue の担当）

- 全契約（`23502`・`23505`／`22023`・INDEX-4 上限・SQL 経路との `wire_code`
  一致）の層 A テスト群（`nosql6_insert.rs`。#773）
- `EXPLAIN` フィールド（NOSQL-10。#765）
- (b) 他テナント保持 id・(c) 未存在 id への insert のレイテンシ分布の
  区別不能性検証（NoSQL 表層版の層 B 計測ハーネス。SQL wire 版は
  `wire_tenant_row_id_scope.rs` の Issue #738 層 B・`make
  wire-tenant-latency` を参照）
- commit 後 panic 時の緊急応答（RECOVER-6）の HTTP 表層対応（`insert` は
  HTTP 表層で初めて到達可能になる書き込み op のため、commit 境界を跨いだ
  panic の観測可能性は別途整理が必要。production では RECOVER-8 の
  panic hook が abort するため実害は限定的）
