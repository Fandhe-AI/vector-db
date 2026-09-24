# `ColumnType` へスカラー型を追加する基盤整備（Issue #880）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-1, TABLE-2,
  TABLE-6, TABLE-7, TABLE-13・TASK-85, TASK-86, TASK-196

## 背景・目的

`catalog::ColumnType` は `Text`／`Vector(u32)` の 2 値、カタログのテキスト形式は
`v1` のみだった。後続 Issue（#881〜#883 ほか）で `INTEGER`／`BIGINT`／`REAL`／
`DOUBLE`／`BOOLEAN` 等のスカラー型を追加する際、型ごとの処理が個別に手書きで
重複しないよう、型を 1 つ足すときに触る箇所を最小化する基盤を整える。

## 決定

### D1: `#[non_exhaustive]`・ワイルドカード腕を導入しない

`ColumnType`・`row_codec::Value` に `#[non_exhaustive]` を付けず、新設・既存の
match にもワイルドカード腕（`_ =>`）を入れない。variant を追加したとき、
コンパイラが全ディスパッチ地点を列挙することが基盤の価値になる。ワイルドカード
腕は、新型が既存分岐へ黙って流れる fail-open の温床になる。

### D2: 型タグ↔`ColumnType` の往復を関連関数 1 対へ集約

`ColumnType::catalog_fields(&self) -> (&'static str, String)` と
`ColumnType::from_catalog_fields(tag: &str, param: &str) -> Result<ColumnType>`
に集約し、`encode_schema`／`decode_schema_body` はこの 1 対だけを呼ぶ。型追加時に
カタログ側で触る箇所を 1 対に限定する。

### D5: カタログ v2 の文法

`v1` と同じ `name:tag:param:nullable` の 4 フィールドを保つ。`param` は
「パラメータなし型は `-`、それ以外は型ごとの文法」へ汎用化した。許容文字は
`[A-Za-z0-9_,]` に閉じ（`validate_catalog_param`）、encode・decode の両側で
検証する。`v2` で `TEXT`／`VECTOR` の列行バイト列は `v1` と完全に同一で、
変わるのは 1 行目のバージョン識別子のみ（`encode_schema_golden_v2_layout` で
バイト列を固定）。

### D6: `v1` は fail-closed に拒否（マイグレーションなし）

`v1` のカタログ値は未知バージョンと同じ `CatalogError::CorruptSchema` で拒否
する（`ROW_FORMAT_VERSION` と同じ方針）。SQL 表層では既存の `table_lookup_error`
を通り `XX000`／汎用メッセージに丸められるため、`wire_code` の写像・HTTP 射影は
無変更。

### D7: 未知型タグ・余剰行の確保前拒否

`decode_schema_body` は、宣言列数（`cols:`。事前に `MAX_COLUMN_COUNT` 以下と
検証済み）を超えない範囲でのみ `ColumnDef`（内部で `String` を確保する）を
構築する単一走査へ再構成した。旧実装の `lines.collect()` は残り行数が宣言列数と
無関係に無制限へ膨らむ攻撃入力（大量の短い行）に対して行数比例のアロケーションを
先に行っていたが、本実装は列ごとに「識別子検証 → `param` 文字集合検証 →
`ColumnType::from_catalog_fields`（未知タグ拒否）→ nullable 検証 →
`ColumnDef::new`（ここで初めて `String` を確保）」の順で処理し、未知の型タグ・
不正な `param` はその列の `ColumnDef` を構築する前に拒否する。宣言列数を超える
行は「末尾の空行（トレーリング改行）1 行のみ」を許容し、それ以外は余剰行として
拒否する。

### D9: `is_vector()` ヘルパ

`ColumnType::is_vector(&self) -> bool` を追加し、`catalog.rs` 内の
`matches!(ty, ColumnType::Vector(_))` を置き換えた（挙動不変・可読性向上）。
`row_codec.rs` 内の同型の `matches!` は本 Issue の時点では手つかずのまま
残置し、後続の型追加時のチェックリスト対象へ申し送る。

## スコープ縮小の判断

計画時に検討した以下の項目は、本 Issue の時点では見送り、`docs/design` の該当
ポインタへ申し送る（詳細は Issue コメント・PR 本文参照）。

- `row_codec::ScalarRef<'a>` の新設・`scan_scalar_columns` 系の戻り値変更
  （`Option<&'a str>` → `Option<ScalarRef<'a>>`）。`scan_scalar_columns` の
  型別セル encode／decode 集約自体は、#880 時点で既に
  `scalar_text_entry_len`／`SCALAR_TEXT_ENTRY_OVERHEAD` という形で
  `tenant::validate_set_assignments` と共有済みであり（先行 Issue で実装済み）、
  本 Issue で新たに壊す理由が薄いと判断した。#881 で `INTEGER` を追加する時点で
  実際に破壊が必要になった場合、その Issue で `!` を付けて対応する。
- `sql/parser.rs` の `(ColumnType, InsertLiteral)` 束縛 3 重複の
  `bind_literal_for_column` への集約。挙動不変のリファクタリングだが、
  #880 のスコープ（カタログ v2・型タグ往復の集約）から独立して行える
  ため、必要になった時点（#881 以降、型が増えて重複が実害になった時点）で
  別 Issue として着手する。

## 型を 1 つ追加するときのチェックリスト（#881〜#897 向け）

新しいスカラー型を `ColumnType` に追加する際、以下を確認する。

1. `catalog.rs::ColumnType` に variant を追加し、`catalog_fields`／
   `from_catalog_fields` に型タグ・`param` 文法を追加する（D2・D5）。
   `param` の文字集合は `validate_catalog_param` の許容範囲
   （`[A-Za-z0-9_,]`）に収まるよう文法を設計する。
2. `row_codec.rs` の `encode_row`／`decode_row`／`encode_scalar_columns`／
   `merge_encode_scalar_columns`／`scan_scalar_columns_validated` の
   `match column.ty` 分岐（コンパイラが列挙する）に新型のペイロード
   encode／decode を追加する。`Value` enum にも対応する variant を追加する。
3. `sql/parser.rs` の `(ColumnType, InsertLiteral)` 束縛（3 箇所。
   Issue #880 では未集約のまま）にリテラル形式の受理規則を追加する。
4. `tenant.rs::validate_set_assignments` の SET 対象列型ごとの事前検証に
   新型のフレーム長計算を追加する。
5. `recovery/content_hash.rs::push_value` のハッシュ入力タグ（現状
   Null=0／Text=1／Vector=2）に新しいタグ番号を追加する
   （既存タグの意味・並びは変更しない）。
6. wire-server 側: NoSQL 表層の JSON 束縛（`http/query/{insert,update}.rs`）・
   RowDescription の OID 写像（`result_encoder.rs`）に新型を追加する。
7. `sql::scalar_index`／`sql::scan`／`sql::exec` の `ColumnType` に対する
   網羅 match（コンパイラが列挙する）に新型の扱いを追加する。
8. 3 段階デコード tier（`sql::aggregate`／`sql::group_by` の
   `DecodeTier`）が新型を正しく分類することを確認する。

## バイト表現の不変範囲（golden で固定）

以下は Issue #880 の前後で不変（`row_codec.rs`・`catalog.rs` の既存/新設テストで
機械的に固定済み）。

- `ROW_CODEC_FORMAT_VERSION = 1`、presence タグ `0x00`／`0x01`
- `Text` = `u32 LE 長 + 本文`、`Vector` = `u32 LE 次元 + f32 LE`
- `SCALAR_TEXT_ENTRY_OVERHEAD = 5`
- `recovery/content_hash.rs::push_value` のハッシュ入力タグ（Null=0／Text=1／
  Vector=2）
- カタログ `v2` の列行バイト列（`name:tag:param:nullable`）は `v1` と同一。
  変わるのは 1 行目のバージョン識別子のみ（`encode_schema_golden_v2_layout`）

## #883 追記: BOOLEAN 列型

TABLE-13・TASK-196（Issue #883）で `ColumnType::Boolean` を追加した。上記
チェックリストに沿った実装内容は以下のとおり。

- カタログ型タグは `"boolean"`（`param` は `"-"` 固定。TEXT と同型）。
- 行バイト表現: presence タグに続く 1 バイト（`0x00`=false／`0x01`=true）。
  NULL（presence `0x00`）とは常に別のバイト列になる（受け入れ条件「NULL と
  false を区別する」の根拠）。`SCALAR_BOOL_ENTRY_LEN = 2`。
- `row_codec::scan_scalar_columns` 系の戻り値を `Option<&str>` から
  `Option<ScalarRef<'a>>`（`Text`／`Bool` の 2 variant）へ型付き化した
  （上記チェックリストが「#881 以降で必要になれば」と申し送っていた対応。
  `ScalarRef::as_text()`/`as_bool()` で TEXT 専用消費側の型不一致を
  fail-closed に扱う）。
- `content_hash::push_value` のタグは `Bool = 7`（Null=0／Text=1／
  Vector=2 は不変。3〜6 は他型向けに予約）。述語 DML のハッシュ
  （`push_dml_assignments`／`push_dml_where_predicates`）にも
  `InsertLiteral::Bool`＝タグ 3、`WherePredicate::BoolEquality`＝タグ 5、
  `WherePredicate::BoolColumn`＝タグ 6 を追加（`BoolColumn` と
  `BoolEquality{value:true}` は評価結果が同じでも構文が異なるため別ハッシュ
  とする安全側の判断）。
- `WHERE` 述語は宣言的フィルタ経路（`declarative_filter::FilterOp::BoolEquals`）
  でのみ評価する（式評価器はスカラー列を参照できないため）。字句解析器は
  `true`/`false` を `Token::Ident` として出すため、`sql::allowlist::Parser::
  parse_where` が `<col> = true|false` と裸の `WHERE flag`（直後が `AND`・
  `ORDER`・`LIMIT`・文末・`USING`／`HINT`／`RETURNING`／`GROUP`／`HAVING` の
  いずれかの場合に限る）を文脈照合で受理する。`NOT`／`IS [NOT] NULL`／
  `IS TRUE`／`<>`／式中の bool 列参照は対象外（既存の拒否のまま）。
- `sql::scalar_index::ScalarIndex` は BOOLEAN 列を索引化しない
  （`per_column.push(None)`）。`sql::scalar_plan::classify_scalar_plan` は
  `FilterOp::BoolEquals` を含む述語（単独・複合いずれも）を常に `PlainScan`
  に分類する単一情報源とし、Issue #843/#844 の索引被覆多層防御
  （`mask_trusted_defer`／`count_star_only`／`observe_group_count_only`）が
  BOOLEAN 述語を誤って「索引で完全被覆済み」と信頼しないことを保証する。
- 集計: `COUNT(<BOOLEAN 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/`MIN`/
  `MAX` は `22000` で拒否する（`AggregateInput::BooleanColumn`）。
  `GROUP BY` キー列は引き続き TEXT 限定のまま。
- wire-server: NoSQL `update` op（`http/query/update.rs`）は JSON 真偽値の
  `SET` を受理する。`insert.rs`・RowDescription の OID 拡張・NoSQL `insert`
  op での JSON 真偽値受理は対象外（#895・#896 の担当）。
- 対象外（申し送り）: RowDescription の OID 16 公告（#895）、NoSQL JSON
  束縛の完全対応（#896）、`22P02` の新設、`NOT`/`IS [NOT] NULL`/`IS TRUE`/
  `<>`/式中の bool 列参照、SQL `CREATE TABLE` 構文での `BOOLEAN` 宣言
  （SQL-23 は未実装）、BOOLEAN 列のスカラー二次索引化。

## #888 追記: ARRAY 列型（複合型のうち配列部分）

TABLE-14・TASK-198（Issue #888）で `ColumnType::Array(ArrayType)` を追加した。
着手時点で origin/main には TEXT・BOOLEAN のみがマージ済み（INTEGER/BIGINT/
REAL/DOUBLE・DATE/TIMESTAMP は未マージ）だったため、要素型は `ArrayElemType`
（`Text`／`Bool` の 2 値）に限定した。詳細な設計判断・バイトレイアウトは
`docs/design/array-column-type.md` を参照。上記チェックリストとの対応は以下の
とおり。

- カタログ型タグは `"array"`、`param` は `"<elem_tag>,<max_len>"`（例:
  `text,64`）。要素数上限の実装既定値は `MAX_ARRAY_ELEMENTS = 1024`。
- 行バイト表現: presence タグに続き flags(1・`0x00` 固定)＋要素数(`u32 LE`)＋
  ペイロード長(`u32 LE`)＋要素列（TEXT は `u32 LE` 長＋UTF-8、BOOL は 1
  バイト）。TEXT/VECTOR/BOOLEAN の既存バイト表現は不変。
- `row_codec::ScalarRef::Array(ArrayRef<'a>)`・`Value::Array(ArrayValue)` を
  追加。`ArrayRef` は走査時に構造・UTF-8・要素数上限を検証済みの借用結果。
- `recovery::content_hash::push_value` のタグは `Array = 10`（3〜9 は他型
  向けに予約）。
- `sql::parser::parse_array_literal` が `'{v1,v2,...}'` リテラルを状態機械で
  解析する（字句解析器は変更しない）。NULL 要素（引用なしの `NULL`）は
  D-A6 により本版では受理せず `22000`。
- `WHERE`（等価・`IS NULL` を含む）・要素/パス演算子（`tags[1]`・`@>`）は
  対象外のまま `22000`／`42601` で拒否（申し送り）。
- `sql::scalar_index::ScalarIndex` は ARRAY 列を索引化しない
  （`per_column.push(None)`）。
- 集計: `COUNT(<ARRAY 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/`MIN`/
  `MAX` は `22000` で拒否する（`AggregateInput::ArrayColumn`）。
- wire-server: `result_encoder.rs` は PostgreSQL 配列テキスト形式（`{a,b}`。
  引用・エスケープ規則込み）で描画し、NoSQL `http/query/response.rs` は
  ネイティブ JSON 配列で描画する。NoSQL `insert`／`update` op は ARRAY 列への
  JSON 値を明示的に拒否する（`22000`。JSON 配列束縛は #896・NOSQL-17 へ
  申し送り）。
- 対象外（申し送り）: 配列列への等価（`=`／`IN`）・`IS NULL` 述語、要素/パス
  演算子、NoSQL の JSON 配列束縛（#896）、`22P02` の新設、SQL `CREATE TABLE`
  構文での `<型>[]` 宣言（#899）、数値・日時要素型（兄弟 PR マージ後）、NULL
  要素対応、配列列のスカラー二次索引化。

## #886 追記: BYTEA 列型

TABLE-13・TASK-197（Issue #886。関連: WIRE-13・NOSQL-17）で `ColumnType::Bytea`
を追加した。上記チェックリストに沿った実装内容は以下のとおり。

- カタログ型タグは `"bytea"`（`param` は `"-"` 固定。TEXT／BOOLEAN と同型）。
- 行バイト表現は TEXT と完全に同じ枠（presence タグ + `u32 LE` 長 + 本体）
  だが UTF-8 検証を行わない点のみ異なる。長さ上限は新設の
  `bytea::MAX_BYTEA_FIELD_LEN`（`row_codec::MAX_TEXT_FIELD_LEN` と同値。
  `const` アサーションで固定）。フレーム長計算は `scalar_text_entry_len`／
  `SCALAR_TEXT_ENTRY_OVERHEAD` を共有する。
- hex テキスト表現（SQL リテラル・wire のテキスト出力）は新設モジュール
  `engine::bytea`（`parse_hex_text`／`format_hex_text`）に集約し、engine と
  wire-server の双方から共有する。受理する形式は PostgreSQL `\x` 接頭辞形式
  の最小部分集合（`\x`／`\X` ＋ 偶数個の 16 進数字。大小文字混在可・`\x` 単体
  は空バイト列）に限り、接頭辞省略・奇数桁・非 16 進文字・PostgreSQL の
  escape 形式（`\x` 接頭辞なしの `\ooo`）はいずれも拒否する（曖昧さを避ける
  fail-closed な実装既定値）。出力は `\x` ＋ 小文字 16 進（PostgreSQL の
  `bytea_output=hex` 既定と同じ）。
- `content_hash::push_value` のタグは `Bytes = 11`（Null=0／Text=1／
  Vector=2／Bool=7 は不変。3〜6・8〜10 は他型〔INTEGER/BIGINT/REAL/DOUBLE・
  DATE/TIMESTAMP/NUMERIC。並行実装中の別 Issue〕向けに予約）。
- `sql::parser` の 4 つの束縛箇所（INSERT・UPDATE の SET・UPSERT の
  リテラル・UPSERT の `ON CONFLICT` リテラル）が共通ヘルパー
  `bind_bytea_literal` を経由し、`\x` hex リテラルのみを受理する。ファイル形
  `INSERT`（`path`/`body` 列規約専用）は BOOLEAN と同じ理由で `BYTEA` 列も
  対象外として拒否する。
- 集計: `COUNT(<BYTEA 列>)`（非 NULL 行数）のみ受理し、`SUM`/`AVG`/`MIN`/
  `MAX` は `22000` で拒否する（`AggregateInput::ByteaColumn`。`BooleanColumn`
  と同じパターン）。`GROUP BY` キー列は引き続き TEXT 限定のまま。
- `WHERE` 述語・スカラー二次索引・UDF/式評価・hybrid 本文列・`USING PLAN`・
  scoring_boost への `BYTEA` 列の露出はすべて `22000` で拒否する（`declarative_
  filter`・`sql::scalar_index`・`sql::udf_call`・`sql::using_plan`・
  `scoring_boost` の既存拒否パターンを踏襲。索引は `per_column.push(None)`）。
- wire-server: NoSQL 表層の JSON 表現は **標準 base64**（RFC 4648 §4・`=`
  パディング必須・正準形のみ）を採用し、新設 `wire-server::http::query::
  base64_std`（`encode_base64_std`／`decode_base64_std`）に厳格な codec を
  実装した。既存の `http::session::token`（base64url・パディングなし）・
  `auth::argon2id`（standard・パディングなし）はいずれも本仕様と一致しない
  ため流用しなかった。`decode_base64_std` は復号後の長さを入力長・パディング
  数から確保前に算出し、`MAX_BYTEA_FIELD_LEN` 超過を `TooLong` として拒否
  する。
  - `insert.rs`: `blob` フィールドに base64 文字列を受理し、復号結果を
    そのまま `Value::Bytes` として束縛する（`InsertLiteral` へ迂回しない）。
  - `update.rs`: 復号したバイト列を正準形（`\x` ＋ 小文字 hex）の
    `InsertLiteral::String` へ再エンコードしてから既存の hex 解析経路へ渡す
    （B9 判断。`vector_literal_text` が JSON 配列を文字列リテラルへ再エンコード
    する既存パターンと同型。engine 側の束縛経路を hex 解析の 1 本に保ち、
    NoSQL と SQL の小文字 hex リテラルのハッシュを一致させるための設計）。
    `InsertLiteral::Bytes` variant は新設しない（表層をまたぐハッシュが常に
    不一致になる・BREAKING となる enum variant が増えるため不採用）。
  - `response.rs`（`scan`／`search`／`aggregate` の JSON 応答）: `Cell::Bytes`
    を標準 base64 の JSON string として出力する。wire 側の `\x` 16 進テキスト
    表現とは意図的に異なる値表現（型名は wire 側との同一性を、値表現は
    JSON との親和性をそれぞれ優先する既存の非対称方針をそのまま踏襲）。
  - エラー分類（NOSQL-17 と一部異なる暫定判断。`22P02` 未実装〔#897・
    TASK-227〕のための申し送り）: `BYTEA` 列への非文字列 JSON（型不一致）・
    不正な base64（アルファベット外・パディング不正・非正準・長さ不正）は
    いずれも `42601`（`InsertError::InvalidBytea`／`UpdateError::
    InvalidBytea`）。SQL 表層の同種の不一致は `22000` のままであり、意図的に
    異なる（受け入れ基準 4・NOSQL-17 の「型不一致は `42601`」を優先し、
    `22P02` 導入時に再分類する）。長さ超過は表層を問わず `54000`。
- 対象外（申し送り）: RowDescription の OID 拡張（既存の `WireType::Text`
  〔OID 25〕のまま。#895）、NoSQL の既存型統一・`columns[].type`（#896）、
  `22P02` の新設（#897・TASK-227）、`WHERE` 述語・二次索引への `BYTEA` 対応
  （#891・#893）、SQL `CREATE TABLE` 構文での `BYTEA` 宣言（SQL-23 は未実装。
  宣言は Rust API の `TableSchema` 経由）。
