# SQL 表層 VectorArena のテーブル世代整合キャッシュ化

- ステータス: Accepted（本コミットで実装）
- 対応: Issue #363（`perf(engine): VectorArena のテーブル世代整合キャッシュ化（SQL 表層への拡張）`）
- 前提: TASK-169（`crates/engine/src/core.rs::PrefilterCache`。Rust API 直呼び経路
  `EngineCore::search` 用の世代整合キャッシュ）、`crates/engine/src/catalog.rs`
  のテーブル単位世代カウンタ（`table_generation_in_txn`／`bump_table_generation_in_txn`。
  `USING PLAN` の I/O 前後照合が使う源泉と同一）、Issue #285
  （`docs/design/table-generation-rejection-granularity.md`。テーブル単位世代の
  拒否精度に関する既存判断）

## 背景

SQL 表層の SELECT（`crates/engine/src/sql/exec.rs::execute_statement`）は、クエリ
のたびに `VectorArena::build_filtered_with_rows_in_txn` で対象テーブルの行を redb
から全走査・デコードし、連続 f32 バッファ（アリーナ）を再構築していた。Rust API
直呼び経路（`EngineCore::search`）には `PrefilterCache`（TASK-169）による世代整合
キャッシュが既にあるが、SQL 表層はこれを経由しない。

本 Issue は、`PrefilterCache` と同型の fail-closed 世代整合キャッシュを SQL 表層へ
導入し、同一テーブル世代内の反復 KNN で redb 再走査・デコードを避けることを目的
とする。

## 設計

### キャッシュキーと失効源泉

`SqlArenaCache`（`core.rs`）は `PrefilterCache` と同じキー（`(table,
PolicyContext)` 完全一致）・容量管理（エントリ数上限・総バイト上限・LRU 追い出し・
stale 一括破棄）を踏襲するが、失効判定の世代源泉が異なる:

| | `PrefilterCache`（既存・`EngineCore::search`） | `SqlArenaCache`（新設・SQL 表層） |
| --- | --- | --- |
| 世代源泉 | ストレージ全体世代（`Storage::current_generation`） | テーブル単位世代（`catalog::table_generation_in_txn`） |
| 失効粒度 | 任意のテーブルへの書き込みで全エントリ失効しうる | 対象テーブル自身への書き込みのみで失効 |

SQL 表層は 1 クエリが 1 テーブルのみを対象とするため、テーブル単位世代の方が
「無関係な他テーブルへの書き込みでエントリを不要に失効させない」点で有効性が
高い。両者を統一する判断（`PrefilterCache` をテーブル単位世代へ移行するか）は
本 Issue のスコープ外とする（下記「スコープ外」参照）。

`lookup` はクエリ実行そのものに使う `read_txn` の中でテーブル世代を読むため、
`PrefilterCache::lookup` のような「ロック取得と世代読み取りの前後関係」に起因する
競合を考慮する必要がない（同一 txn 内の値は以後変化しない）。

### キャッシュする対象とヒット/ミス経路

キャッシュに保持するのは **RLS 段のみを適用したアリーナ＋行 metadata の複製**
（`SqlArenaSnapshot`）。SCALAR 段（`WHERE`・式述語・hybrid 疎コーパス蓄積・投影用
スカラー列保持）はクエリごとに異なるため事前適用しない。

- **ミス**: 従来どおり redb を 1 回走査するが、`VectorArena::
  build_filtered_with_rows_in_txn_capturing` の `rls_capture` フックで RLS 通過行
  全体を同一走査の中で同時に採取する（追加の走査は発生しない）。採取は
  `SqlArenaCaptureBuilder`（`arena.rs`）が担い、行 metadata の複製バイト量が上限を
  超えた場合は以降の採取を静かに諦め（`failed()`）、`SqlArenaCache::insert` へは
  渡さない（クエリ応答自体は妨げない。fail-soft）。
- **ヒット（通常）**: `VectorArena::build_from_cached_rls_rows` が、キャッシュ済み
  RLS-only アリーナへ SCALAR 段（`on_visible_row`）をクエリごとに再適用し、候補
  集合を再構築する。行単位の処理（容量検証・push）は `push_visible_row`
  （ミス経路の redb ループとも共有）を経由し、redb コールドパスと同一の挙動を
  保証する。
- **ヒット（高速経路）**: SCALAR 段が恒等写像であると静的に判定できる場合
  （`WHERE` なし・式述語なし・hybrid でない・投影が候補スカラー列を参照しない。
  `sql::exec` の `cache_fast_path_eligible` 判定）、行単位ループそのものを省略し、
  キャッシュ済みスナップショットの `VectorArena` を直接借用する。

### insert の非対称性

`PrefilterCache::insert` と異なり、`SqlArenaCache::insert` が `None`（＝キャッシュ
へ反映されなかった）を返しても、呼び出し元はクエリ応答自体には自分の `read_txn`
で構築済みの結果をそのまま使ってよい。fail-closed が守る対象は「stale な
**キャッシュ**を別クエリへ供すること」であり、この 1 回限りの自分自身の応答では
ないため。

## 安全性（テナント境界・fail-closed）

- **キー完全一致**: `PolicyContext` は `PartialEq`/`Eq` をテナント ID・許可可視性
  集合の値比較として実装しており、`ImplicitRlsHook::predicate` はこれ以外の入力を
  一切読まない（`policy.rs`・`rls.rs` で確認）。ctx が 1 bit でも異なれば別
  キャッシュエントリになり、他テナント・他可視性のスナップショットを供する経路を
  構造的に作らない。
- **書き込み経路の世代バンプ網羅性**: `catalog.rs` の全書き込み API
  （`create_table`・`drop_table`・`alter_table_add_column`・
  `insert_row_into_table`・`insert_rows_into_table`・`insert_typed_row`）は commit
  直前に必ず `bump_table_generation_in_txn` を呼ぶ。`tenant.rs` のテナント境界
  付き書き込みガード（`insert_row`・`insert_rows`・`insert_typed_row`）も同様。
  この網羅性は `tests/table_generation_bump_coverage.rs` が構造的（ソーステキスト
  走査）に検出する。
- **fail-closed**: 世代読み取り失敗・ロック毒化はいずれも「キャッシュ不使用」
  （ミスとして再構築）へ倒す。stale なキャッシュで応答する経路はない。

## 実測（feature_bench 前後比較）

対象リポジトリに Issue 本文が挙げた `crates/engine/examples/feature_bench.rs` は
存在しないため、SQL 表層の SELECT 経路を直接測定する既存ベンチ
`crates/engine/benches/sql_c1_bench.rs`（TASK-83・SQL-1。`EngineCore::execute_sql`
経由の C1 p95。同一クエリを WARMUP+ITERS 反復し、`EngineCore::search`
〔`PrefilterCache` 経由でウォーム〕との A/B 比較 `diagnostic_ab` を診断情報として
併記する）で代替した。

- 測定環境: linux/x86_64・12 logical CPUs・ISA `Avx2Fma`（`env:` 行参照）。
  非専有環境（`BENCH_DEDICATED_ENV` 未設定）
- 測定条件: `rows=100000`・`dim=768`・`k=20`（`sql_c1_bench.rs` 既定値）
- 比較方法: 同一プロセス内で `git stash` により変更前後を切り替え、直前に
  `uptime` の loadavg を記録して測定条件を揃えた

| | 変更前（base コミット `98cf86d`） | 変更後（本コミット） |
| --- | --- | --- |
| loadavg（測定直前） | 4.54, 4.62, 4.12 | 4.31, 4.60, 4.10 |
| p95（sql_c1、SQL 表層） | 214.16 ms | 14.94 ms |
| median（sql_c1、SQL 表層） | 206.49 ms | 12.71 ms |
| median（`EngineCore::search`、`PrefilterCache` ウォーム。A/B の `b_median`） | 9.06 ms | 9.05 ms |
| `median_ratio`（SQL 表層 / `EngineCore::search`。診断情報・合否対象外） | 23.19 | 1.32 |
| `topk_consistency` recall_min | 1.000000（pass） | 1.000000（pass） |

`median_ratio` が 23.19 → 1.32 まで縮小し、SQL 表層の反復 KNN レイテンシが
（既に `PrefilterCache` でウォームな）`EngineCore::search` とほぼ同オーダーへ
収束した。これは Issue の受け入れ条件 1「同一世代内の反復 KNN でアリーナ
再構築が発生せず、レイテンシがスコアリング支配になること」を満たす実測的根拠
とする。

初期実装（キャッシュヒット時も毎回 `VectorArena::build_from_cached_rls_rows` で
行単位に複製・再構築する版）では median が 150 ms 程度までしか縮まらなかった。
プロファイルの見立てでは、埋め込みの memcpy（100,000 行 × 768 次元 × 4 byte ≒
307 MiB／クエリ）自体よりも、行単位ループが持つ副次コスト（`tenant_id` の
ヒープ確保 10 万回、`row_codec::scan_scalar_columns` の毎行呼び出し）が支配的
だった。`WHERE`・式述語・hybrid・投影用スカラー列参照のいずれも無い場合
（SCALAR 段が恒等写像になる場合）に限り、キャッシュ済みスナップショットの
`VectorArena` を直接借用してこのループ自体を省略する高速経路を追加したのが
上表の「変更後」の実装であり、`sql_c1_bench.rs` の規範形（`SELECT id ... ORDER
BY ... LIMIT ...`）はこの経路に該当する。

## テスト

- 単体（`arena.rs::tests`）: `SqlArenaCaptureBuilder` の metadata バイト上限
  超過時の soft-fail（`failed()`・`finish()` が `None`）・予算内での正常完了
- 結合（`tests/sql_arena_cache.rs`。9 ケース）:
  - 同一世代内の反復でヒットのみ増加し、結果がコールドキャッシュと完全一致
  - `WHERE` 句がキャッシュヒット時も正しく再適用される
  - 対象テーブルへの `INSERT` でキャッシュが失効（`stale_evictions` 増加）し、
    新しい行を反映する（fail-closed）
  - 無関係な別テーブルへの書き込みでは失効しない（テーブル単位世代の粒度）
  - `(table, ctx)` 完全一致によるテナント・可視性境界の分離（キャッシュ間の
    混線なし）
  - hybrid・`USING MODE 'precision'`・`HINT ORDER`・高速経路（`SELECT id ...`）
    の 4 経路で、コールドキャッシュとキャッシュヒットの `QueryResult`
    （`columns`／`rows`の `id`・`score`・`cells`、順序含む）が完全一致すること
    （スロット番号契約 `candidate_columns[slot]` ↔ 疎 DocId ↔ provider id の
    差分回帰検証）

## 既知の制約・申し送り（スコープ外）

- **ミス時のピークメモリ**: ミス経路は「クエリ応答用のアリーナ」と「キャッシュ
  登録用の RLS-only スナップショット（アリーナ＋metadata 複製）」を同一走査内で
  並行して構築するため、コールドクエリ 1 回あたりのピークメモリはおおむね倍増
  する（いずれも既存の `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES` で上限は掛かる。
  超過時はキャッシュ登録のみを断念しクエリ応答は継続）。
- hybrid の `SparseIndex` 構築そのもののクエリ間キャッシュ化は対象外（アリーナ・
  metadata 走査の排除まで）
- Rust API 直呼び経路（`EngineCore::search`）の `PrefilterCache` をテーブル単位
  世代へ移行する統一は対象外（挙動変更を伴うため別判断）
- 集計 SELECT（`sql::aggregate`）経路のキャッシュ化は対象外
- Issue 本文が測定対象として挙げていた `crates/engine/examples/feature_bench.rs`
  は本リポには存在しない。上記「実測」節のとおり `crates/engine/benches/
  sql_c1_bench.rs` を代替として使用した（Issue 側の記述修正が必要）
