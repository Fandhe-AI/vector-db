# `EXPLAIN` への検索エンジン種別・ANN 適用判定の露出（Issue #411）

## ステータス

Implemented（2026-09）

## 背景

ADR [`ann-index-adoption.md`](./ann-index-adoption.md)（Issue #367／#403・B 案）の
受け入れ基準は「`EXPLAIN` へ使用エンジン種別を露出し、どの経路で結果が得られたか
追跡可能にする」ことを要求する。`sql/explain.rs`（TASK-78・SQL-6）は従来
`search_terms[i]` → `path_hint` → `kind_hint` → `mode` → `mode_source` の 6 行のみを
返し、`sql/exec.rs` が内部で判定している 4 つの ANN 適用条件（`hnsw_full_visible_
eligible`／`hnsw_subset_eligible`／`hnsw_hybrid_full_visible_eligible`／
`hnsw_hybrid_subset_eligible`。#408・#409・#410）をクライアントから確認できなかった。

## 決定

### 1. `EXPLAIN` は実行なしを維持し「静的な計画」のみを報告する

実行時の plain scan への縮退（可視カーディナリティ比・マスク分断・結果不足による
fail-closed 縮退）は RLS 可視アリーナの走査後に決まり、これを `EXPLAIN` で求めるには
アリーナ構築・索引の `prepare_*`（構築・統計加算という副作用を持つ）が必要になる。
これは「`EXPLAIN` は検索本体を実行しない」という既存契約
（`sql/explain.rs` モジュールドキュメント・`tests/sql_explain.rs::
explain_does_not_write_or_execute_search`）に反する。

そのため `EXPLAIN` は**クエリ形状とエンジン設定から決まる静的な適用判定**
（索引経路に載る形状か・plain scan か、その理由）のみを報告する。実行時の
fail-closed 縮退・hybrid 密側再取得ラウンド数（`docs/design/
hnsw-hybrid-iterative-scan.md`）は対象外とし、集計観測は
`EngineCore::hnsw_index_cache_stats()`（Rust API）に委ねる。この判定はテーブル
内容に一切依存しないため、存在情報の副次漏えいが構造的に起きない。

### 2. 適用判定の単一情報源化

`sql/hnsw_cache.rs` に `AnnPlan`（`PlainScanEngine`／`PlainScanPrecision`／
`HnswFullVisible`／`HnswSubset`）・`AnnShapeInput`・`classify_ann_plan` を追加した。
`sql/exec.rs` の 4 boolean（`is_hybrid` の真偽による組み合わせ）はこの関数の戻り値
から導出し、`sql/explain.rs` も同じ関数を呼ぶ。「並行する推測」ではなく単一実装の
共有により `EXPLAIN` の回答が executor の実際の判定と乖離しない。同値性は 32 通り
（2^5: `hnsw_enabled`・`is_hybrid`・`is_precision`・`filters_empty`・
`scalar_prefilter`）全数の判別テストで固定した。

### 3. `EXPLAIN` の `WHERE` 形状の取り方

`sql::using_plan::pre_check_bindable` の戻り値を `PreCheckShape { filters_empty }`
へ拡張した。`filters_empty` は束縛後の `metadata_filters.is_empty() &&
expr_filters.is_empty()` であり、`stmt.where_predicates().is_empty()` とは**異なる**
（`WHERE visible()` は `WherePredicate::PredicateCall` として `rls_predicate_present`
フラグのみを立て `metadata_filters`／`expr_filters` を増やさないため）。この差分を
`explain_reports_hnsw_full_visible_ann_plan_with_only_visible_predicate` で固定した。

### 4. `USING PLAN` は `HINT ORDER` を受理しない（既存の許可リスト制約）

`sql::allowlist::parse_select_shape` は `USING PLAN` 経路で `HINT ORDER(...)` を
構造上受理しない（ランキング自体を展開結果が決めるため、既存コメント参照）。
そのため `EXPLAIN`（`USING PLAN` 専用構文）の `evaluation_order` は常に
`EvaluationOrder::DEFAULT`（SCALAR 先行）であり、`ExecutionPlan::scalar_prefilter`
は `EXPLAIN` 経由では常に `true`。`classify_ann_plan` の `!scalar_prefilter` 分岐
（DISTANCE 先行時の `FullVisible` 判定）は `EXPLAIN` からは到達しないが、`sql/
exec.rs`（`ORDER BY` 経路の `HINT ORDER` を受理する）からは到達するため、関数自体
は削除しない。この制約は判別テスト
`explain_rejects_hint_order_combined_with_using_plan`（`42601`）で固定した。

### 5. 出力行の仕様（安定契約）

既存 6 行の文言・順序は不変。`mode_source` の直後へ以下を追記する（新規行は
追記のみで既定エンジン時の出力は変更前と後方互換。TASK-164 で `mode_source` を
追加した前例と同じ方針）。

| 行 | 常時／条件 | 値の語彙（snake_case・英語・閉じた集合） |
| -- | -- | -- |
| `engine: <token>` | 常時 | `parallel_brute_force` / `cpu_scalar_brute_force` / `hnsw` / `(custom_provider)`（`search_engine_kind() == None`。`with_provider`／`from_storage` 経由） |
| `hnsw_params: m=<m>,ef_construction=<ef_c>,ef_search=<ef_s>` | `engine: hnsw` のときのみ | 構築時の静的設定値のみ（`full_scan_ratio` は含まない） |
| `ann_plan: <token>` | 常時 | `plain_scan_engine` / `plain_scan_precision` / `hnsw_full_visible` / `hnsw_subset` / `unknown_custom_provider`（`engine: (custom_provider)` のときのみ。PR #437 追記） |

`engine` の文字列化は `SearchEngineKind` の既存 `Display`（`full_scan_ratio` を
含む診断・ログ向け表現）をそのまま使わず、`sql/explain.rs` に専用の網羅 `match`
を置く。`(custom_provider)` はヒント未指定の `(none)` と意味が異なるため区別する。

**露出しない値**: `full_scan_ratio`（切替閾値）・`MIN_INDEXED_ROWS`・可視カーディ
ナリティ・行数・索引ノード数・キャッシュ状態・実行時縮退結果・hybrid 密側再取得
ラウンド数。これらはいずれもテナントの存在情報に繋がりうるため対象外とし、必要に
なれば別 Issue でオーナー判断とする。

## 実装

- `crates/engine/src/sql/hnsw_cache.rs`: `AnnPlan`／`AnnShapeInput`／
  `classify_ann_plan`（+ 32 通り真理表テスト）
- `crates/engine/src/sql/exec.rs`: 4 boolean を `classify_ann_plan` からの導出へ
  置換（ディスパッチ構造・適用条件そのものは無変更）
- `crates/engine/src/sql/using_plan.rs`: `pre_check_bindable` の戻り値を
  `PreCheckShape { filters_empty }` へ拡張
- `crates/engine/src/sql/explain.rs`: `ExplainEngine { kind, ann_plan }`・
  `build_explain_result(planned, engine)`（`engine_token`／`ann_plan_token` の
  網羅 `match`。新規行の閉じた語彙・非データ依存を機械的に固定するテストを含む）
- `crates/engine/src/core.rs`: `Statement::Explain` アームで `ExplainEngine` を
  組み立てる。`hnsw_enabled` は `self.hnsw_state.is_some()`（executor と同じ源泉）、
  `is_hybrid` は常に `true`（`USING PLAN` は常に `Ranking::Hybrid`）、`is_precision`
  は `planned.mode().mode()`、`filters_empty` は `pre_check_bindable` の戻り値、
  `scalar_prefilter` は `ExecutionPlan::from_evaluation_order(validated.
  evaluation_order())`。`hnsw_state` の `lookup`／`prepare_*` は一切呼ばない
- `crates/engine/tests/sql_explain.rs`・`crates/wire-server/tests/wire_explain.rs`:
  既定エンジン・HNSW opt-in（フィルタなし／等価フィルタ／`visible()` のみ／
  `precision` モード／`HINT ORDER` 拒否）・閉じた語彙固定・
  `hnsw_index_cache_stats()` 全 0 固定（副作用なし契約の維持）を検証

## スコープ外

- `EXPLAIN` の受理形状拡張（`USING PLAN` を伴わない `ORDER BY ... <->` DISTANCE
  クエリへの拡張）は SQL-6 の定義範囲に関わるため spec 側の課題として申し送り
- 実行時縮退結果・hybrid 密側再取得ラウンド数・キャッシュ状態の可視化
  （`EXPLAIN ANALYZE` 相当）は実行を伴うため別設計
- `full_scan_ratio` 等の閾値露出の可否はオーナー判断待ち
- wire-server CLI・テーブル単位カタログ属性でのエンジン選択露出（ADR
  「判断確定後のスコープ外」節）
- `SearchEngineError` の `ErrorClass`／`wire_code` 正式登録（spec 側ビヘイビア ID
  確定後）

## 追記（PR #437・codex-review P1 指摘対応）

`EngineCore::with_provider`／`from_storage` 経由でカスタム `SearchProvider` を
注入した場合（`search_engine_kind() == None`）、`EngineCore` は実際に ANN か
brute-force かを判別できない。従来はこの場合も `hnsw_enabled == false` を
`classify_ann_plan` へそのまま渡していたため `ann_plan: plain_scan_engine`
（厳密 brute-force と確定）を返してしまい、`engine: (custom_provider)`
（実行方式不明）と矛盾する誤表示だった。`AnnShapeInput` へ `engine_kind_unknown`
フィールドを追加し、`search_engine_kind().is_none()` の場合は新設した
`AnnPlan::UnknownCustomProvider`（`ann_plan: unknown_custom_provider`）を返す
よう分岐した。`sql::exec` 側の 4 boolean 導出（`hnsw_full_visible_eligible` 等）
は `engine_kind_unknown` を参照しない（常に `false` を渡す）ため実行時の適用
条件は無変更。

## ポインタ

SQL-6・TASK-78・CORE-9・CORE-10・CORE-12・TASK-132・SEARCH-9・PLAN-11
