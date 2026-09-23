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
