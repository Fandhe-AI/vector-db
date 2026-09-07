# 可視ビットマップの世代整合キャッシュ（VisibleBitmapCache）

- ステータス: Accepted（本 Issue で実装。スコープを Fast tier 集計へ限定）
- 対応: Issue #478（`feat(engine): 可視ビットマップの世代整合キャッシュ
  （VisibleBitmapCache）を実装し集計・DISTANCE 経路へ結線する`）
- 前提: Issue #464（`docs/design/scan-stage-profile.md`。`agg_count`／
  `rls_isolation` の段別プロファイル実測）、Issue #363
  （`sql/arena_cache.rs::SqlArenaCache`。同型の fail-closed 契約を確立）、
  Issue #357（`sql/sparse_cache.rs::SparseIndexCache`。同じ契約の先行例）、
  Issue #350（`sql/aggregate.rs::DecodeTier`。3 段階デコードの土台）

## 背景

Issue #464 の実測（`docs/design/scan-stage-profile.md`）で、`agg_count`／
`rls_isolation`（`SELECT COUNT(*)`）は毎クエリ `user_rows/{table}` を全行
走査し、行ごとにヘッダデコード → RLS 可視判定 → TABLE-12 のキー/ヘッダ
tenant 整合検査 → dim/metadata 構造検証（`DecodeTier::Fast` も PR #369 の
契約により毎行実施）を繰り返すことが支配的コストと判明した。`SqlArenaCache`
（Issue #363）は SELECT（DISTANCE/hybrid）経路専用で集計経路を経由しない。

## 設計

### スナップショット（`VisibleSnapshot`）

`(table, ctx, table_generation)` ごとに、`user_rows/{table}` を物理走査
（`(tenant_id, id)` キー昇順）したときに可視かつ TABLE-12 検査を通過した行の
`id` だけを列挙順に保持する（`visible_ids: Vec<u64>`）。embedding・metadata は
一切保持しない。

### 適用範囲（スコープ判断）

Issue 本文が要求した「集計・DISTANCE 経路への結線」のうち、本実装は
**`sql::aggregate::execute_aggregate` の `GROUP BY` なし・`WHERE` なしの
`DecodeTier::Fast`（`COUNT(*)`・`COUNT(id)`・`SUM`/`AVG`/`MIN`/`MAX(id)`）に
限定**した。理由:

- `DecodeTier::Fast` の入力（`AllVisible`・`IdU64`）は dim・metadata を一切
  参照しないため、ヒット時は `user_rows/{table}` を **一切開かずに**
  `visible_ids` を反復するだけで集計できる（A1〜A5 全段の省略）。これが
  Issue #464 で最劣後と特定された `agg_count`／`rls_isolation` への直接効果。
- `DimAndScalar`／`Embedding` tier・`GROUP BY`（`sql::group_by`）は行ごとに
  dim・metadata・embedding を実際に読む必要があり、ビットマップで省略できる
  のはヘッダデコード＋RLS 判定＋TABLE-12 検査のみ（Issue 本文の設計メモにも
  「効果は小さい」と明記）。
- SELECT（DISTANCE/hybrid）経路は既に `SqlArenaCache`（Issue #363）が
  同種の全段省略を担っており、本キャッシュを二重に結線する追加効果が薄い。

上記の非対象経路（`DimAndScalar`/`Embedding` tier・`GROUP BY`・SELECT
DISTANCE 経路）への拡張、および `sparse.rs::VisibleBitmap`（BM25 の
`doc_idx` 添字空間・母数確定と一体の構造）との表現共有は、変更面を実装
コストに見合う効果のある範囲へ絞るため、本 Issue のスコープ外とした
（後続 Issue で拡張を検討）。

### 信頼基盤・fail-closed 契約

`SqlArenaCache`・`SparseIndexCache` と同型: `lookup`/`insert` の非対称
（`lookup` はロック毒化・世代読取失敗を「見つからない」として扱い、`read_txn`
視点の不一致は `storage` から再読取した真の最新世代より厳密に古いと確認
できた場合のみ破棄。`insert` は挿入対象自身が既に古い場合・容量超過時は
反映しない）。失効源泉はテーブル単位世代（`catalog::table_generation_in_txn`）。

構築（ミス時）は、`DecodeTier::Fast` が PR #369 の契約により既に毎行
dim・metadata の構造検証（`decode_row_dim_and_metadata_borrowed`）と
TABLE-12 検査を実施済みの走査に相乗りするだけなので、追加コストは実質
ゼロ。ヒット時に構造検証を再実行しない根拠は `SqlArenaCache` のヒット経路
と同じ「同一テーブル世代内は行バイト列が不変」という前提
（`catalog::bump_table_generation_in_txn` が対象テーブルへの全書き込み経路で
必ず世代を進める契約。`tests/table_generation_bump_coverage.rs` が機械強制）。

## 検証

対照 DB 方式による非漏えいの追加検証・前後比較実測は Issue #479・
`docs/design/visible-bitmap-cache-verification.md` を参照。

`crates/engine/tests/sql_visible_cache.rs`:

1. 受け入れ条件 (a): 同一テーブル世代内の 2 回目以降は `misses` を増やさず
   `hits` のみ増加し、`COUNT(*)`/`COUNT(id)`/`SUM(id)`/`MIN(id)`/`MAX(id)`
   いずれも cold/hot で結果が完全一致する。
2. 対象テーブルへの `INSERT` は次回 lookup 時に失効し（`stale_evictions`
   増加）、新しい行数を反映する。
3. `(table, PolicyContext)` キーの非漏えい: tenant-a の Private 行を hot に
   した後の tenant-b（Public のみ可視）の `COUNT(*)` は自身の可視件数の
   ままで、tenant-a の非公開行の存在・件数が混入しない（RLS-7・RLS-8）。

既存の `tests/rls_generalized.rs`（TASK-138）・`tests/sparse_cache_recall.rs`・
`tests/sql_aggregate.rs`・`tests/sql_group_by.rs`・`tests/sql_arena_cache.rs`
は無変更のまま green（受け入れ条件 (b)）。

### 前後比較実測（受け入れ条件 (a) 補足）

共有計測環境での 1 回実測（`make bench-scan-stage-profile`。
`BENCH_SCAN_PROFILE_ROUNDS` 既定 5・`BENCH_SCAN_PROFILE_SCALE` 既定 1＝
25,000 行）で、e2e の `agg_count`／`rls_isolation`（`EngineCore::execute_sql`
経由の `SELECT COUNT(*)`。ラウンドを跨いで同一 `EngineCore` を使い回すため
2 回目以降は本キャッシュのヒット経路を測る）を導入前（`origin/main`）・
導入後（本ブランチ）で比較した。

| 測定点 | 導入前 median | 導入後 median |
| ------ | -------------: | -------------: |
| `agg_count`（A0a, ctx=tenant-a） | 1.598ms | 0.050ms |
| `rls_isolation`（A0b, ctx=tenant-b） | 1.625ms | 0.050ms |

両測定点とも約 32 倍高速化しており、Issue #464 で最劣後と特定された
全行走査（A1〜A5 相当）がヒット時に完全に省略されていることを裏づける。
共有環境での単発実測（交互 min-of-N ではない）につき、専有環境での正式な
交互比較実測はオーナー／運用者作業として引き続き申し送る。

## スコープ外（申し送り）

- `DimAndScalar`／`Embedding` tier・`GROUP BY` 集計への拡張
- SELECT（DISTANCE/hybrid）経路への結線（`SqlArenaCache` が既に担う範囲との
  重複度の評価を含む）
- `sparse.rs::VisibleBitmap` との表現共有（添字空間・責務が異なるため不採用）
- 専有環境での交互 min-of-N 実測（上記は共有環境での単発実測。手動計測は
  オーナー／運用者作業）
