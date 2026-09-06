# ADR: 広域取得モード（ソートなしのフィルタ取得）を SQL 表層へ追加する（Issue #454）

- ステータス: Implemented（契約は本リポの実装既定値。spec ビヘイビア ID は未確定
  ――下記「spec 側への申し送り」参照）
- 対応: Issue #454
- 関連ポインタ: TASK-161・TASK-162・SQL-12・SEARCH-9（取得モード `recall`／
  `precision` との関係）・RLS-8（TASK-138。全読み取り経路への RLS 一般化）・
  TABLE-12（キー/ヘッダ tenant 整合検査）。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 検証コード: `crates/engine/src/sql/allowlist.rs`（`Statement::Scan`・
  `ValidatedScan` 単体テスト）・`crates/engine/src/sql/scan.rs`（`execute_scan`
  単体テスト。nullable VECTOR・早期終了）・`crates/engine/tests/sql_scan.rs`
  （実行契約の結合テスト）・`crates/engine/tests/rls_generalized.rs`
  （`scan_limit_at_or_above_visible_total_matches_oracle_set_exactly`・
  `scan_small_limit_returns_subset_with_exact_count_and_no_leak`）・
  `crates/wire-server/tests/wire_scan.rs`（層 A）・
  `crates/wire-server/tests/extended_syntax_e2e.rs::three_clients_run_scan_where_nosort`
  （層 B）

## 背景

本 DB の設計思想「正解を含むデータ群を広く返し、丸ごと LLM へ渡す」を SQL 表層で
直接表現できる経路が無かった。既存の許可リスト（`sql/allowlist.rs::parse_select_shape`）
は行を返す `SELECT` に `ORDER BY <距離>` か `USING PLAN(...)` を必須とし、
`ORDER BY` なしのスカラーフィルタのみの行取得は `42601` で拒否していた。

他 DB 横断ベンチ（Issue #406 関連 PR。`docs/design/crossdb-bench.md`「広域取得」節）
では `scan_where_nosort_k500`（`WHERE lang = 'ja' LIMIT 500`・`id, body` 投影）が
self のみ unsupported だった。他 DB は同等のクエリを直接受理する。

既存の `USING MODE 'recall'`／`'precision'`（TASK-161・TASK-162・SQL-12・
SEARCH-9）は Top-k 固定件数／確信度ゲートであり、「ソートせず広く返す」取得の
構文ではない。

## 設計方針

### 構文・返却契約（本リポの実装既定値。spec ビヘイビア ID 確定待ち）

```text
SELECT <投影（既存許可形: *, 列名列, 式項目〔UDF 含む〕）> FROM <table>
  [WHERE <既存許可述語（等価・LIKE 前方一致・visible()・式述語）>]
  LIMIT <n>
```

| 項目 | 契約 |
| --- | --- |
| 受理条件 | `WHERE`（省略可）の直後に `LIMIT` が現れる形のみ。`ORDER BY`・`USING PLAN` は従来どおり検索 SELECT として別経路 |
| `LIMIT` | 必須。`sql::parser::validate_search_limit` を再利用し `1..=core::MAX_SEARCH_K`（10,000）。範囲外は `22000`（既存契約と同一） |
| `USING MODE`／`HINT ORDER`／`OFFSET` | 構造上受理しない（`LIMIT n` の後は文末のみ）。集計文が `USING MODE` を受理しない既存判断（「取得モードの余地を持たない」）と同じ理由 |
| セッション変数 `SET search_mode` | 広域取得は参照しない（ランキング段・確信度ゲートが存在しないため） |
| `EXPLAIN` 前置 | 従来どおり `42601`（`USING PLAN` を伴わないため） |
| 順序 | **順序保証なし**。実装は同一スナップショット内で決定的（redb 行テーブルのキー順＝`(tenant_id, id)` 昇順。他テナントの `Public` 行が可視な場合はテナント文字列順で交錯し、`id` 昇順が成り立つのは同一テナント内のみ）。グローバル `id` 順へ「修正」しない・決定性テストの前提として明記する |
| 件数 | 可視かつ `WHERE` を満たす行を先頭から最大 `n` 件。`n` 件集まった時点で走査を打ち切る（早期終了） |
| RLS | `PolicyContext::is_visible` をデコード前に無条件適用（`WHERE visible()` の有無に依存しない。RLS-8）。TABLE-12 のキー/ヘッダ tenant 整合検査を維持 |
| `score` | `ResultRow.score` は `0.0` 固定（wire では非送出） |
| エラー契約 | 既存 `SqlSurfaceError` 写像をそのまま使用（未定義テーブル `42P01`、容量超過 `54000`、破損・内部バグ `XX000`、式評価エラー `22000`/`54000`） |

### しきい値構文・大きな k の広域取得（スコープ外の判断）

「しきい値・大きな k で広く返す Top-k」のうち **しきい値構文**は spec ID 未確定の
ため本 Issue では実装しない（構文を先に固定すると spec 確定後に互換性破壊と
なるため）。大きな k は既存 `LIMIT`（上限 `MAX_SEARCH_K`＝10,000）で既に表現
できる。

### 構文の位置付け: `USING MODE` の新値ではなく独立文種

広域取得の構文は `USING MODE` の新値ではなく「`ORDER BY`／`USING PLAN` を伴わない
`SELECT ... LIMIT n`」とした。理由:

1. ベンチ `scan_where_nosort_k500` と他 DB の規範形が bare 形。
2. `SearchMode` enum（SQL-12）へ第 3 値を足すと `precision` ゲート・プランナー
   推定（PLAN-11）・`EXPLAIN` の `mode` 行と絡み spec 改訂範囲が広がる。
3. 集計（`Statement::Aggregate`）と同じ「ランキング段を持たない文種」として
   独立させれば、既存の DISTANCE 中心経路に一切触れずに済む。

### 実行本体: `VectorArena` を経由しない redb 直接走査

`sql::aggregate::execute_aggregate`（TASK-166・SQL-13）と同じ理由で
`VectorArena`（既存の検索 SELECT 実行経路）を使わない: アリーナはスキーマに
`VECTOR` 列が必須で、可視行の embedding を全件バッファへ確保するため、
`VECTOR` 列を持たないテーブルの広域取得や大規模テーブルの `id`/`TEXT` 列のみの
取得には過剰（メモリ）かつ非対応。

`sql/scan.rs::execute_scan` は `sql/aggregate.rs::execute_aggregate` の走査ループと
同一の規約（RLS 適用順序・TABLE-12 整合検査・`DecodeTier` による必要最小限
デコード）を踏襲しつつ、集計アキュムレータの代わりに投影結果セルを蓄積し、
`bound.limit` 件集まった時点で走査を打ち切る（早期終了）。デコード段階の
参照列導出（`decode_tier_for`）は投影列（`ProjectedColumn`）・`WHERE` から
独自に導出する（`sql::aggregate::ReferencedColumns` は集計項目
`BoundAggregateItem` 形状に特化しているため型を共有しない）。

### RLS・早期終了の相互作用

早期終了はテナント境界の縮約を壊さない: 不可視行は `LIMIT` のカウント対象に
一切現れない（デコード前の `ctx.is_visible` 判定でスキップされる）ため、
どの物理走査位置で打ち切っても他テナントの存在・件数の情報を漏らさない。
`tests/rls_generalized.rs` の 2 パターン検証（(a) `LIMIT` が可視総数以上ならオラクル
集合と完全一致、(b) 小さい `LIMIT` でも返却集合がオラクル集合の部分集合かつ
件数が `min(limit, |oracle|)` に一致）で固定した——(a) を欠くとオラクル比較が
早期終了により vacuous になる。

## 影響

- `crates/engine/src/sql/allowlist.rs`: `Statement::Scan(ValidatedScan)` を追加
  （**BREAKING CHANGE**: 既存の網羅的 `match` はワイルドカードアームの追加が
  必要。`Aggregate`・`Explain` 追加時と同じ運用）。`parse_select_shape` の戻り値を
  `ParsedSelect::{Search, Scan}` へ変更（モジュール内 private）。
- `crates/engine/src/sql/parser.rs`: `BoundScan`・`bind_scan` を追加。
- `crates/engine/src/sql/scan.rs`（新設）: `execute_scan`。
- `crates/engine/src/core.rs`: `execute_sql`（非セッション）・
  `execute_validated_in_session` の両方へ `Statement::Scan` アームを追加。
- `crates/wire-server/`: 追加の変更なし（`EngineCore::execute_sql_in_session` を
  経由する既存の簡易クエリ経路がそのまま `Statement::Scan` を受理する）。

## スコープ外

- spec 側ビヘイビア ID の確定（下記「spec 側への申し送り」）・確定後のポインタ
  差し替え。
- しきい値による可変件数の広域取得構文（spec ID 確定後に別タスク）。
- 検索 SELECT（`ORDER BY`）経路の投影固定コスト削減（Issue #453 の管轄。本
  Issue では `bulk_knn_*` の self 実測値は変わらない）。
- 広域取得への二次索引（Issue #359 ADR）適用・`EXPLAIN` 露出。
- `make bench-crossdb`／`make e2e-three-client` の実行環境が無い場合の再実測・
  実行はオーナー作業。

## spec 側への申し送り

spec 側でモード定義・構文・返却契約（件数上限・順序保証の有無・RLS 適用）を
ビヘイビア ID として確定する作業は本リポからは実施不可（private spec リポ
[vector-db-spec](https://github.com/Fandhe-AI/vector-db-spec) 側の作業）。本 ADR の
契約は spec ID 確定までの実装既定値として運用し、確定後にポインタを差し替える。
