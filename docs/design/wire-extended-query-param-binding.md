# 拡張クエリプロトコル `$n` パラメータ束縛（Issue #935・WIRE-12・TASK-217）

## ステータス

Partially implemented（engine 側のみ。`crates/engine/src/` の Parse／Bind／
Describe の値束縛機構は実装済み。wire 側（`crates/wire-server/`）の
Bind／Execute／Sync（'B'／'E'／'S'）結線は #934（Bind／Execute／Sync／
Close／Flush・`ExtendedQueryState`・`PortalStore`）が本 Issue 着手時点で
`origin/main` 未マージだったため対象外のまま。#934 マージ後に別 PR で結線する）。

## 背景

`crates/wire-server/src/extended_query.rs`（#933・WIRE-11）の Parse は
`num_param_types > 0` を `0A000` で拒否し、`sql::lexer::tokenize` は `$` を
一律 `42601` で拒否していた。そのため psycopg 3（既定の拡張クエリプロトコル）・
node pg（`values` 付き `query`）・psql `\bind`・JDBC 等、パラメータ付きクエリを
送るクライアントは実行できなかった。

## 設計: トークン単位の値スロット束縛

方式は「文を解析（Parse）するタイミング」と「値を束縛（Bind）するタイミング」
を分離し、値未確定のまま構造検証を一度通し、Bind 時に実値へ 1 トークン置換して
**もう一度同じ構造検証を通す**という 2 段構成にした。得られる `ParsedSql` は
「同じ値を正しくエスケープしたリテラルで書いた SQL テキスト」を `parse_sql`
した結果と完全に同一になるため、第 2 の実行器を作らずに済む。

### 字句解析（`sql::lexer`）

- `Token::Param(u16)`（1 始まり）を追加した。既定の `tokenize` は挙動不変
  （`$` を引き続き「未対応文字」として拒否する）。新設
  `tokenize_with_params` だけが `$n` を受理する。両者は `tokenize_impl` を
  共有する。
- `$0`・先頭ゼロ（`$01`）・`$`単独・`$$`・`$1a`（数字直後の識別子継続文字）は
  いずれも `LexError`（呼び出し元で `42601`）。桁数字が `u16::MAX` を超える
  場合は飽和させる（未定義動作にしない。上限超過自体は `sql::params` が
  `54000` で拒否する）。

### 許可リスト検証の分割（`sql::allowlist`）

- `pub fn validate_sql(sql, lookup)` は字句解析後 `validate_sql_tokens(tokens,
  lookup)` へ委譲するだけの薄いラッパーへ分割した（挙動不変。既存 269 件の
  `allowlist` テストが green のまま）。

### `sql::params`（新設）

- `MAX_PARAMS = 64`。
- `validate_param_positions(tokens) -> Result<u16, SqlSurfaceError>`: 許可位置
  （下記）のみを構造的に受理し、最大の `$n` 番号（= 要求パラメータ数）を返す。
- `substitute_dummy(tokens) -> Vec<Token>`: 全 `$n` を固定ダミー `"0"` の
  `Token::StringLiteral` へ置換する（Parse 時点の構造検証専用。実行・Describe
  には使わない）。
- `substitute_values(tokens, values) -> Result<Vec<Token>, _>`: 実値で置換する
  （Bind 用）。
- `decode_bind_values(tokens, values) -> Result<Vec<String>, _>`: UTF-8 検証・
  NUL 拒否・**置換後総バイト数（`$n` の出現回数 × 対応する値の長さ、の総和）を
  値の複製より前に checked 演算で確定させ `lexer::MAX_INPUT_LEN`（1 MiB）と
  比較する**（同一 `$1` を多数回参照させて 1 回の値から巨大な置換結果を作らせる
  メモリ増幅対策）。

### `core.rs::EngineCore`

- `parse_sql` の本体を `parse_tokens(tokens: Vec<Token>)` へ抽出した（挙動
  不変。`parse_sql` は `tokenize(sql)?` してから委譲するだけ）。
- `pub fn parse_sql_prepared(&self, sql) -> Result<PreparedSql, _>`（Parse）:
  `tokenize_with_params` → `validate_param_positions` → `substitute_dummy` →
  `parse_tokens`（ダミー値束縛済みの `ParsedSql` を `PreparedSql::dummy_parsed`
  として Describe 専用に保持）。
- `pub fn bind_prepared(&self, prepared, values: &[Option<Vec<u8>>]) ->
  Result<ParsedSql, _>`（Bind）: 値件数検証 → `decode_bind_values` →
  `substitute_values` → `parse_tokens`。返る `ParsedSql` は既存の
  `execute_parsed_in_session`／`describe_parsed_in_session` へそのまま渡せる。
- `pub fn describe_prepared_in_session(&self, session, prepared) ->
  Result<Option<Vec<ColumnMeta>>, _>`（Describe(statement)）:
  `describe_parsed_in_session(session, &prepared.dummy_parsed)` を呼ぶだけ。
  投影列の形はどの `$n` 値を束縛しても変わらない（列名・型は文の構造にのみ
  依存し、リテラル値の中身には依存しない）ため、ダミー値での導出結果は
  実値束縛後と常に一致する。LLM・埋め込み I/O は呼ばない（`USING PLAN` の
  既存 Describe 契約を維持）。

## 受理するプレースホルダ位置（規範形。これ以外は `42601`）

1. `ORDER BY <vec列> <=> $n`（ベクトル位置。直前トークンが `DistanceOp`）
2. `USING PLAN($n)`
3. `USING OPERATION_ID $n`（INSERT／TRUNCATE／DELETE／UPDATE 共通の文末句）
4. `WHERE <列> = $n`（トップレベル `WHERE` 節内側の等価条件のみ。`LIKE`・式
   比較・BOOLEAN 列条件は対象外）
5. `INSERT ... VALUES ($1, $2, ...)`（複数行含む。`VALUES` 節内の行リテラル
   位置のみ）

`LIMIT $n`・`USING MODE $n`・`HINT ORDER($n, ...)`・`SET search_mode = $n`・
`UPDATE ... SET a = $n`・`ON CONFLICT ... DO UPDATE SET a = $n`・hybrid 関数
引数（`HYBRID_RRF(col, $n, ...)`）・非等価比較（`id > $n` 等）はいずれも
`sql::params::validate_param_positions` の位置判定に含めておらず、構造的に
`42601` へ落ちる（`sql::params` の単体テスト・`crates/engine/tests/
prepared_params.rs` で固定）。

`WHERE` 節内の等価条件（パターン 4）と `SET`／`ON CONFLICT ... DO UPDATE SET`
（拒否対象）は同じ `Ident '=' $n` の字面を持つため、`WHERE` キーワードの
出現位置を境界とする範囲判定でのみ区別する（`UPDATE ... SET a = $n` の `SET`
節は `WHERE` キーワードより前に現れるため対象外、`UPDATE ... SET a = 'x'
WHERE b = $n` の `WHERE` 節内の等価条件は受理——後者は述語形 UPDATE／DELETE の
`WHERE` にも同じ判定がそのまま適用される）。

## 値の形式（スコープ縮小）

すべてのプレースホルダは常に `Token::StringLiteral` として置換する。`id` 列・
`BOOLEAN` 列など、生の `Token::Number`／`Token::Ident("true"/"false")` を
要求する位置（例: `INSERT ... VALUES ($1)` の `$1` が `id` 列や `BOOLEAN` 列に
対応する場合）は本バージョンのスコープ外であり、束縛すると「同じ値をクォート
したリテラルで書いた SQL」と同一の型不一致エラー（`bind_insert` 等、既存の
構造検証・束縛契約そのまま）で拒否される——これは fail-closed な既知の制約で
あり誤動作ではない。`VECTOR`・`BYTEA` 列は `StringLiteral` 形のまま構造検証を
通り、実際のリテラル形式の妥当性（角括弧・`\x` 接頭辞等）は既存の
`bind_insert`（Bind 後、`parse_tokens` を経由した実行時）が判定する——
リテラル SQL テキストで書いた場合と完全に同じ検証経路を通る。

## NULL パラメータ値

`decode_bind_values` は `None`（SQL NULL）を一律 `22000` で拒否する（本
バージョンのスコープ外）。`USING OPERATION_ID $n` を省略と同義にしたい
クライアントは、空文字列を束縛する（`OperationId::parse("")` が既存の
`23502` へ写像する）か、SQL テキスト側で `USING OPERATION_ID NULL` を直接
書く（パラメータ化しない）。

## 検証

- `crates/engine/src/sql/lexer.rs`: `tokenize_with_params` の受理・拒否形状、
  `tokenize`（`allow_params=false`）が分割前後で不変であることを固定するテスト。
- `crates/engine/src/sql/params.rs`: 位置判定（受理 5 形・拒否 8 形以上）・
  パラメータ番号上限・ダミー/実値置換・NULL／非UTF-8／NUL バイト拒否・
  メモリ増幅対策の単体テスト。
- `crates/engine/tests/prepared_params.rs`:
  - **リテラル同値性**: 5 形それぞれで `bind_prepared` の結果が、同じ値を
    エスケープしたリテラル SQL の `parse_sql` 結果と `ParsedSql: PartialEq`
    で完全一致することを固定（束縛が「同じ値をリテラルで書いた SQL」と
    同一の判定になることの機械的な証明）。
  - **インジェクション耐性**: `' OR '1'='1`・`'); DROP TABLE ...; --`・
    改行・別の `$n` らしき文字列を含む値を束縛しても、単一の不透明な
    リテラルとして扱われ文の形が変わらないこと（実行結果・テーブルの
    存続を含む）。
  - 位置拒否（`42601`）・字句拒否（`$0`／`$01`／`$1a`）・番号上限
    （`54000`）・値検証（`22000`）・値件数不一致・RLS 暗黙適用（他テナントの
    行が混入しない）・Describe の同値性・Parse／Describe の副作用ゼロ・
    簡易クエリでの `$n` 拒否（既存契約）を固定。

## スコープ外・申し送り

- wire 側（Parse の宣言型受理・`ParameterDescription`・Bind の値保持・
  `bind_prepared` 結線）は #934 マージ後の別 PR。
- パラメータのバイナリ形式復号（WIRE-14）。
- 数値列（`id`）・`BOOLEAN` 列への `$n` 束縛（上記「値の形式」参照）。
- NULL パラメータ値の意味論的な位置別処理（`USING OPERATION_ID $n` へ NULL を
  「省略と同義」として通す等）。
- 追加のプレースホルダ位置（`LIMIT $n`・非等価 WHERE 比較・`UPDATE ... SET
  col = $n`・`ON CONFLICT ... SET`・hybrid 関数引数・`LIKE $n`）。
- 層 B（psycopg 3／node pg／psql 実クライアントでの `make e2e-three-client`
  相当）は wire 側結線後にあわせて追加する。
