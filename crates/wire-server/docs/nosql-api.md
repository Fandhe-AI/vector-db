# NoSQL API

`wire-server --surface nosql` が公開する 3 エンドポイント（`POST /v1/session`・
`POST /v1/session/close`・`POST /v1/query`）と、`POST /v1/query` の `op` 4 値
（`search`／`scan`／`aggregate`／`insert`）の JSON スキーマを利用者向けに整理する。

**この文書の情報源はコードとテストのみ**であり、`docs/spec`（private submodule）
の本文は転記しない。参照が必要な箇所は TASK-nn・ビヘイビア ID のポインタ表記に
限る（詳細は同 ID を持つ spec 側ファイルを、アクセス権のある人が別途参照する）。
本文中の数値・上限・応答コードは実装既定値であり、将来の変更で上書きされうる
（規範文書ではない）。

## 目次

- [概要・位置づけ](#概要位置づけ)
- [転送路の共通規則](#転送路の共通規則)
- [セッション認証](#セッション認証)
- [`POST /v1/query` 共通規則](#post-v1query-共通規則)
- [op 別スキーマ](#op-別スキーマ)
- [`filter` 配列](#filter-配列)
- [`explain`](#explain)
- [応答スキーマ](#応答スキーマ)
- [SQL ↔ NoSQL 対応表](#sql--nosql-対応表)
- [エラー応答](#エラー応答)
- [curl 例](#curl-例)
- [検証コード索引](#検証コード索引)
- [spec 側への申し送り候補](#spec-側への申し送り候補)

## 概要・位置づけ

NoSQL 表層は `wire-server --surface nosql` で SQL wire（pg wire v3 互換実装）と
排他選択する、HTTP/1.1 最小サブセット上の転送路である（TASK-171・HTTP-1・
HTTP-9）。`AGENTS.md` がスコープ外とする汎用「Web API」フレームワークとは別物で、
spec で規範化された内部転送プロトコルの実装にすぎない。

エラー契約は SQL 表層と完全共有する。NoSQL 表層は新規 `wire_code` を追加せず、
`engine::error_format::ErrorClass` を HTTP 応答（ステータス・本文）へ写像するのみ
（ERR-4）。

spec ポインタ一覧: TASK-184（基盤）・HTTP-1〜13・NOSQL-1〜11・ERR-4・SQL-1〜15・
RLS-7・RLS-9・TABLE-12。

## 転送路の共通規則

`wire-server --users <path> --db <path> --surface nosql [--bind <addr:port>]` で
起動する。`--users`・`--db` は必須（fail-closed。省略時は起動しない）。`--bind`
の既定値は `127.0.0.1:5432`。TLS 未構成時は非ループバックアドレスへの bind を
起動時に拒否する（loopback 限定）。

要求の受理条件（いずれも接続ハンドラ層で判定し、違反はすべて `08P01`）:

- メソッドは `POST` のみ、バージョンは `HTTP/1.1` のみ
- 要求行は 4 KiB 以下
- ヘッダ部は合計 8 KiB 以下・32 個以下
- `Content-Length` は必須・一意（欠落・重複・非数字は `08P01`）
- `Transfer-Encoding` は拒否
- `Content-Type` は `application/json`（パラメータなし、または
  `charset=utf-8` 1 個のみ）。不一致・欠落は `08P01`
- 本文長は 1 MiB 以下（超過は `54000`）。UTF-8 として不正な本文は `42601`
- 要求読み取りには 30 秒のタイムアウトがある

同時接続数の上限は 64（`MAX_CONNECTIONS`）。超過接続は新しいスレッドを起こさず
`503`／`53300` を返してから切断する。

応答は常に `Content-Type: application/json; charset=utf-8`・`Content-Length`・
`Connection: close`・`Date` を付け、1 要求ごとに接続を閉じる。`401` 応答のみ
`WWW-Authenticate: Bearer` を追加で付ける（RFC 9110 §11.6.1 準拠）。

検証コード: `crates/wire-server/tests/http2_framing.rs`・`http3_content_type.rs`・
`http11_limits.rs`・`http12_fail_closed.rs`・`http_limits.rs`。

## セッション認証

### `POST /v1/session`

要求本文は `user`・`password` の 2 つの必須文字列フィールドのみ（未知キーは
`42601`）。

```json
{"user": "alice", "password": "pw-alice"}
```

成功時（`200`）:

```json
{"token": "<43文字の base64url 文字列>", "expires_in": 3600}
```

- トークンは 256bit（32 バイト）の CSPRNG 出力をパディングなし base64url
  （RFC 4648 §5）で符号化した固定 43 文字
- TTL は発行時刻から 3600 秒固定（スライドしない）
- 同時有効セッション数の上限は 256（超過は `53300`）
- 資格情報の照合失敗・未知ユーザーはいずれも区別せず `28P01`（`401`）で拒否し、
  Argon2id の固定遅延・ダミー KDF による対称性を維持する（存在オラクルを与えない）

### `POST /v1/session/close`

`Authorization: Bearer <token>` ヘッダが必須。本文は空、または `{}`（それ以外の
キーを持つ本文は `42601`）。

成功時（`200`）:

```json
{"closed": true}
```

ワンタイム失効のため、二重 `close`・未知トークン・期限切れトークンはいずれも
区別せず `28000`（`401`）になる。

### `Authorization: Bearer` の扱い（`/v1/query` 共通）

`Authorization: Bearer` ヘッダの欠落・スキーム不一致（例: `Basic`）・トークン
不正・未知・期限切れ・close 済みは、すべて同一の `28000`（`401`）・固定文言へ
収束する（存在オラクルを与えない設計）。

テナント文脈はセッションに束縛された内部状態からのみ導出する。要求側が
JSON キー・ヘッダ（`tenant`／`tenant-id`／`tenantid` 相当。`x-` 接頭辞・
`_`/`-` の揺れを正規化してから判定）でテナントを自己申告しても無視されず、
`42601` で拒否される。

検証コード: `crates/wire-server/tests/http4_session.rs`・`http5_query_bearer.rs`・
`http6_auth_failure.rs`・`http8_session_close.rs`。

## `POST /v1/query` 共通規則

3 パスとも要求ターゲットのバイト厳密一致でのみ受理する（クエリ文字列付き・
末尾スラッシュ・大文字小文字違いはすべて `08P01`）。

判定順序（fail-closed。この順が契約）:

1. パス上の `tenant_id` 相当マーカー（`/v1/query?tenant_id=...`・
   `/v1/query/tenant-id/...` 等）の拒否（`42601`。認証より前）
2. `Authorization: Bearer` 認証（`28000`）
3. ヘッダの `tenant_id` 相当拒否（`42601`）
4. 本文の UTF-8／JSON 構文／`op` フィールドの形（`42601`）
5. `op` 許可リスト判定（4 値の厳密一致。語彙外は `0A000`。DDL・UDF 呼び出し・
   トランザクション制御・`UPDATE`／`DELETE` 相当を含む）
6. op 別スキーマ検証（必須キー欠落・未知キー・型不一致・`null` は `42601`）
7. op 別の意味検証・実行（`search` は `explain: true` を通常実行より先に判定）

`op` は完全一致のみで判定する（大文字小文字の読み替え・前後空白のトリムは
しない）。`"SEARCH"`・`" search"`・`"select"`・`"explain"`・`"begin"` 等はいずれも
`0A000`。

JSON 本文の構文受理規則は `engine::json`（NOSQL-8）に従う: ネスト深さ 16 まで・
文字列 1 個あたり 1 MiB まで・配列/オブジェクトの要素数 65,536 まで、オブジェクト
の重複キーは拒否（後勝ちで無警告に上書きしない）、数値リテラルは RFC 8259 準拠
（先頭ゼロ・小数部/指数部の数字欠落は拒否）。いずれの違反も `42601`。

検証コード: `crates/wire-server/tests/nosql1_endpoint_routing.rs`・
`nosql9_op_allowlist.rs`・`nosql1_op_vocabulary.rs`・`nosql8_schema_validation.rs`。

## op 別スキーマ

以下、各 op のトップレベルフィールドと `wire_code` を表にする。値の**語彙・範囲**
（`filter[].op` の語彙、`aggregates[].fn`／`having[].op` の語彙等）は別途本節の
説明文で扱う。

### `search`

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"search"` |
| `table` | ○ | string | 識別子形状（後述） |
| `limit` | ○ | number | `1..=10000`（範囲外は `42601`） |
| `vector` | △ | number[] | `plan` と排他かつどちらか必須 |
| `plan` | △ | string | `vector` と排他かつどちらか必須。LLM クエリ展開 |
| `hybrid` | △ | `{"text": string}` | `vector` とのみ併用可（`plan` と併用は `42601`）。疎側テキスト列は固定で `body` 列 |
| `mode` | △ | string | `"recall"`（既定）／`"precision"` |
| `columns` | △ | string[]（非空） | 省略時は `id`＋全実列 |
| `filter` | △ | object[] | [`filter` 配列](#filter-配列)参照 |
| `explain` | △ | bool | [`explain`](#explain)参照 |

要求例（ベクトル検索）:

```json
{"op": "search", "table": "docs", "vector": [0.1, 0.2, 0.3, 0.4], "limit": 10,
 "columns": ["id", "lang"]}
```

要求例（ハイブリッド検索）:

```json
{"op": "search", "table": "docs", "vector": [1.0, 0.0], "limit": 3,
 "columns": ["id"], "hybrid": {"text": "alpha"}}
```

要求例（クエリ展開検索）:

```json
{"op": "search", "table": "docs", "plan": "find content", "limit": 10}
```

応答例は [応答スキーマ](#応答スキーマ)を参照。

主な `wire_code`:

- `vector`／`plan` 両方指定・両方欠落・`plan`＋`hybrid` 併用・`columns: []`・
  識別子形状不正 → `42601`
- 未知テーブル → `42P01`
- 未知列・`VECTOR` 列でない列への `ORDER BY` 相当・非有限ベクトル要素等 → `22000`

### `scan`

順序保証なしの広域取得（SQL-15 の bare 形 `SELECT ... [WHERE ...] LIMIT n` と
同一実行意味論。`limit` 件到達で早期終了・取得モード非適用）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"scan"` |
| `table` | ○ | string | |
| `limit` | ○ | number | `1..=10000` |
| `filter` | △ | object[] | |
| `columns` | △ | string[]（非空） | 省略時は `id`＋全実列 |
| `explain` | △ | bool | 常に `42601`（拒否。後述） |

`vector`／`plan`／`mode`／`hybrid` はスキーマが宣言しないフィールドのため、
未知キーとして `42601` になる（`scan` への付与自体を個別に判定するロジックは
持たない）。

要求例:

```json
{"op": "scan", "table": "docs", "limit": 10, "columns": ["id", "lang"],
 "filter": [{"column": "lang", "op": "eq", "value": "ja"}]}
```

応答には `score` 列相当が一切含まれない（`ORDER BY`／`hybrid` を経由しないため
合成スコア列が構造上存在しない）。

`explain: true` は `scan`（SQL-15 の bare 形）への `EXPLAIN` 前置が拒否される
契約の写像として `42601`。

### `aggregate`

単一行集計（`GROUP BY` なし）と `GROUP BY`／`HAVING` 集計の両方を、SQL テキストを
一切組み立てずに束縛・実行する（SQL-13・SQL-14 と同一の実行計画）。

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"aggregate"` |
| `table` | ○ | string | |
| `aggregates` | ○ | object[]（`{"fn","column"}`。1〜32 要素） | `fn` は `count`／`sum`／`avg`／`min`／`max`（小文字完全一致）。`column` は列名、または `count` 専用の `"*"` |
| `filter` | △ | object[] | |
| `group_by` | △ | string[]（ちょうど 1 要素） | `TEXT` 列限定 |
| `having` | △ | object[]（`{"fn","column","op","value"}`） | `group_by` 必須。`op` は `=`／`<`／`<=`／`>`／`>=` の完全一致 |
| `explain` | △ | bool | 常に `42601`（拒否） |

要求例（単一行集計）:

```json
{"op": "aggregate", "table": "docs",
 "aggregates": [{"fn": "count", "column": "*"}, {"fn": "sum", "column": "id"}]}
```

要求例（`GROUP BY`／`HAVING`）:

```json
{"op": "aggregate", "table": "docs",
 "aggregates": [{"fn": "count", "column": "*"}],
 "group_by": ["lang"],
 "having": [{"fn": "count", "column": "*", "op": ">=", "value": 2}]}
```

主な `wire_code`:

- `aggregates` が空配列 → `group_by`／`having` の有無を問わず一律 `42601`
  （`having` の参照解決より必ず先に検査する）
- `group_by` 要素数が 1 でない・`having` のみ単独指定（`group_by` なし）・
  `fn`／`op` が語彙外・識別子形状不正 → `42601`
- グループ数上限（10,000）・グループキー累計バイト・`having` 述語数上限超過
  → `54000`
- `group_by` 列が `TEXT` 列でない・`having` が `MIN`/`MAX(<TEXT列>)` を参照・
  参照先が `aggregates` に存在しない／曖昧 → `22000`
- `VECTOR` 列の集計・`sum` オーバーフロー → `22000`／`22003`

### `insert`

| キー | 必須 | 型 | 備考 |
| --- | --- | --- | --- |
| `op` | ○ | string | `"insert"` |
| `table` | ○ | string | |
| `rows` | ○ | object[] | 各行は `id`（非負整数）＋スキーマ列名をキーとする値 |
| `operation_id` | △（実質必須） | string | 欠落・`null`・空文字はいずれも `23502` |

要求例:

```json
{"op": "insert", "table": "docs",
 "rows": [{"id": 1, "embedding": [0.1, 0.2, 0.3], "lang": "ja"}],
 "operation_id": "op-1"}
```

成功時（`200`）:

```json
{"inserted": 1, "operation_id": "op-1"}
```

- `VECTOR` 列は数値配列、nullable 列は省略または `null` 可、未知キー・次元
  不一致・型不一致は `22000`
- 同一 `operation_id` の再送: 内容が一致すれば `23505`、不一致なら `22023`
  （台帳照合。TASK-101・RECOVER-10 の再送判定を透過する）
- 同一テナント内の `id` 重複は `23505`（他テナントの同 `id` とは衝突せず、
  応答は「不在時」と同一——TABLE-12・RLS-9）
- `rows` の行数上限は既定 64（`EngineCore::execute_bound_insert_in_session` が
  `rows.len()` を INDEX-4 の件数上限相当として判定。環境変数
  `VECTOR_DB_BATCH_MAX_FILES` で上書き可能）。超過は `54000`
- `rows` にはバイト上限がない（行形は SQL 表層と同じくバイト上限を持たない設計。
  [spec 側への申し送り候補](#spec-側への申し送り候補)参照）

検証コード: `crates/wire-server/tests/nosql2_search.rs`・
`nosql2_search_binding.rs`・`nosql3_scan_mapping.rs`・`nosql3_scan_wire_parity.rs`・
`nosql4_aggregate.rs`・`nosql5_group_by.rs`・`nosql4_5_aggregate_wire_parity.rs`・
`nosql6_insert.rs`・`nosql6_tenant_row_id_scope.rs`・
`http_insert_response_boundary.rs`・`wire_insert_operation_id.rs`。

## `filter` 配列

`search`／`scan`／`aggregate` 共通で使える事前フィルタ配列。

```json
[{"column": "lang", "op": "eq", "value": "ja"}]
```

- 各要素は `column`・`op`・`value` の 3 つの必須文字列フィールドのみ
- `op` は `eq`（一致）・`prefix`（前方一致）の 2 語彙のみ（`or`・否定・範囲比較の
  構文は存在しない）。複数要素は常に AND 結合
- 要素数は 256 個まで（超過は `54000`）
- `column` にサーバー側 RLS 述語名相当（`visible`／`visible()`。大文字小文字
  非区別）を指定する経路は `42601`（RLS はサーバー側暗黙適用のみで、クライアント
  は述語を書けない）
- 列名解決・未知列／`VECTOR` 列拒否（`22000`）は `engine::declarative_filter`
  の既存契約をそのまま透過する

検証コード: `crates/wire-server/tests/nosql7_filter_mapping.rs`。

## `explain`

`op: search` かつ `plan` 指定かつ `explain: true` のときのみ、検索本体を実行せず
SQL `EXPLAIN SELECT ... USING PLAN(...)` と同一内容を返す。

要求例:

```json
{"op": "search", "table": "docs", "plan": "find content", "limit": 10,
 "explain": true}
```

応答例（`200`）:

```json
{"explain": ["<QUERY PLAN の行>", "..."]}
```

- `vector` 指定＋`explain: true` → `42601`
- `plan` 欠落＋`explain: true`（`vector`も欠落）→ `42601`
- `scan`／`aggregate` への `explain: true` → `42601`（別 op なので `search` の
  上記条件とは独立に、各 op のハンドラが拒否する）

検証コード: `crates/wire-server/tests/nosql10_explain.rs`・`wire_explain.rs`。

## 応答スキーマ

`search`（`explain` なし）／`scan`／`aggregate` 成功時は共通形:

```json
{"columns": [{"name": "id", "type": "numeric"}, {"name": "lang", "type": "text"}],
 "rows": [[1, "ja"], [2, "en"]],
 "row_count": 2}
```

- キー順固定（`columns` → `rows` → `row_count`）・空白なし
- `row_count` は常に `rows` の長さ
- `columns[].type` は SQL 表層 `RowDescription` と同じ型名写像: 疑似列 `id` は
  `"numeric"`、それ以外（実列・`VECTOR` 列・式項目）はすべて `"text"`
  （型名は wire 側との一致を優先する一方、値そのものは native JSON——配列・
  数値・真偽値・文字列——で返す。値表現を型名に合わせて文字列化してはいない）
- `Cell::Vector` は `[1,2.5]` のような JSON 数値配列として返る
- 非有限（`NaN`／`±Infinity`）な浮動小数は `null` に丸めず内部エラーとして
  fail-closed に拒否する（通常は engine 側の評価時点で `22000` になり到達しない）

`insert`・`explain: true` の応答形は前節の専用形（`{"inserted",...}`／
`{"explain":[...]}`）を参照。

検証コード: `crates/wire-server/tests/nosql11_response_schema.rs`。

## SQL ↔ NoSQL 対応表

| SQL | NoSQL |
| --- | --- |
| `SELECT id, lang FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10` | `search` + `vector` + `columns` |
| `SELECT id FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'alpha') LIMIT 3` | `search` + `vector` + `hybrid.text`（疎側列は `body` 固定） |
| `SELECT id FROM docs USING PLAN('find content') LIMIT 10` | `search` + `plan` |
| `... LIMIT n USING MODE 'recall'` | `"mode":"recall"` |
| `WHERE lang = 'ja'` | `filter` 要素 `{"op":"eq",...}` |
| `WHERE lang LIKE 'j%'` | `filter` 要素 `{"op":"prefix",...}` |
| `EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10` | `search` + `plan` + `"explain":true` |
| `SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10`（広域取得 SQL-15） | `scan` |
| `SELECT COUNT(*), SUM(id) FROM docs` | `aggregate` |
| `SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING count >= 2` | `aggregate` + `group_by` + `having` |
| `INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-1'` | `insert` + `operation_id` |

対応の無いもの（NoSQL 側に受理形が存在しない。実際の応答は語彙外 `op` として
`0A000`、または未知キーとして `42601`）:

- `HINT ORDER(...)` によるソフトブースト
- UDF 呼び出し・`CREATE FUNCTION`
- `SET`（`search_mode` 等のセッション変数設定）
- 定数のみの `SELECT`
- `ORDER BY` 形／集計／広域取得への `EXPLAIN` 前置（`USING PLAN` 付き検索
  `SELECT` への `EXPLAIN` のみ受理）
- `LIKE` の前方一致（`prefix`）以外の一致方式
- `INSERT` のファイル形（`path`／`body` 列指定の増分インデックス投入）
- `GROUP BY` への `ORDER BY`／`LIMIT` の付与

逆方向（NoSQL にあって SQL に対応形がないもの）は無い。

検証コード: `crates/wire-server/tests/nosql3_scan_wire_parity.rs`・
`nosql4_5_aggregate_wire_parity.rs`・`wire_using_plan.rs`・
`wire_insert_operation_id.rs`・`crates/engine/tests/default_preset.rs`。

## エラー応答

失敗時の本文形（緊急応答時のみ `data.state` が付く場合がある）:

```json
{"error": {"wire_code": "42601", "code": "...", "message": "..."}}
```

`wire_code` → HTTP ステータスの全射影表・ラベル一覧・表↔コード一致テストは
本文書では扱わない（別 Issue の担当。予約見出しのみ確保）。本文書の各 op 節
（[op 別スキーマ](#op-別スキーマ)・[`filter` 配列](#filter-配列)・
[`explain`](#explain)）では `wire_code` のみを示し、HTTP ステータス列は付けない。
[セッション認証](#セッション認証)・[転送路の共通規則](#転送路の共通規則)節で
触れた `401`／`503` は、`WWW-Authenticate` の付与条件・接続数上限の説明に必要な
範囲でのみ言及している。

## curl 例

```sh
# 1) セッション発行
TOKEN=$(curl -s -X POST http://127.0.0.1:5432/v1/session \
  -H 'Content-Type: application/json' \
  -d '{"user":"alice","password":"pw-alice"}' | \
  sed -n 's/.*"token":"\([^"]*\)".*/\1/p')

# 2) 検索
curl -s -X POST http://127.0.0.1:5432/v1/query \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10}'

# 3) セッション終了
curl -s -X POST http://127.0.0.1:5432/v1/session/close \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{}'
```

（`alice`／`pw-alice` は既存テストと同じダミー資格情報の一例。実運用では
`--users` で構成したストアの実クレデンシャルを使う。）

## 検証コード索引

- 転送路: `crates/wire-server/tests/http1_surface_select.rs`・
  `http2_framing.rs`・`http3_content_type.rs`・`http11_limits.rs`・
  `http12_fail_closed.rs`・`http_limits.rs`
- セッション: `http4_session.rs`・`http4_session_issue.rs`・
  `http5_query_bearer.rs`・`http6_auth_failure.rs`・`http8_session_close.rs`
- ルーティング・op 許可リスト・スキーマ: `nosql1_endpoint_routing.rs`・
  `nosql1_op_vocabulary.rs`・`nosql9_op_allowlist.rs`・`nosql8_schema_validation.rs`
- `search`: `nosql2_search.rs`・`nosql2_search_binding.rs`・`nosql10_explain.rs`・
  `wire_using_plan.rs`・`wire_explain.rs`
- `scan`: `nosql3_scan_mapping.rs`・`nosql3_scan_wire_parity.rs`
- `aggregate`: `nosql4_aggregate.rs`・`nosql5_group_by.rs`・
  `nosql4_5_aggregate_wire_parity.rs`
- `insert`: `nosql6_insert.rs`・`nosql6_tenant_row_id_scope.rs`・
  `http_insert_response_boundary.rs`・`wire_insert_operation_id.rs`
- `filter`: `nosql7_filter_mapping.rs`
- 応答形: `nosql11_response_schema.rs`
- エラー射影: `err4_http_projection.rs`

## spec 側への申し送り候補

以下は既存モジュールコメントが「spec 判断」として明示している事項の一覧で
あり、本文書執筆にあたって新たな判断は行っていない:

- `hybrid.text` の疎側テキスト列を JSON で選択可能にするか（現状は `body`
  固定）
- `/v1/session/close` 成功応答のキー名（`{"closed":true}`。実装既定値）
- `/v1/query` 配下のパス上 `tenant_id` マーカーと未知ターゲットの優先順位
  （本リポの実装判断であり spec 側での明文化は未定）
- 集計 `id` 列等の巨大整数（`u64`。2^53 超）を JSON number としてそのまま返す
  ことの是非（文字列化への変更は spec 側判断に委ねられている）
