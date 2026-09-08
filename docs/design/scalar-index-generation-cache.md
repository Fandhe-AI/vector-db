# スカラー列二次索引の構築とテーブル世代整合キャッシュ

- **Issue**: #473（親 Issue #472・#359。ADR: `docs/design/scalar-secondary-index.md`）
- **対象ビヘイビア**（ポインタのみ・本文非転記）: `docs/spec/04-behavior/data-model.md`
  TABLE-12・`docs/spec/04-behavior/rls.md`
- **ステータス**: 実装済み（構築とキャッシュ）。索引を使った候補削減への結線は
  Issue #474 で実装済み（`docs/design/scalar-index-prune.md` 参照）

## 背景・目的

SQL 表層の `WHERE` スカラー条件（`Equality`・`Prefix`）は
`sql/exec.rs::on_visible_row` で RLS 可視行の全行走査＋インライン比較（O(N)）に
より評価されている（`docs/design/scan-stage-profile.md` の該当段参照）。ADR
`docs/design/scalar-secondary-index.md`（Issue #359・#472 で候補 B を採用案として
確定）は、メモリ常駐・テーブル世代整合キャッシュ方式の二次索引を提案している。

本 Issue はその第 1 段として、**索引の構築とキャッシュのみ**を実装する。索引を
使った候補削減・`ExecutionPlan` 統合・`EXPLAIN` 露出は Issue #474 が担う。

**不変条件**: 本 Issue の前後で SQL クエリの結果は完全一致する。索引は構築・
キャッシュされるだけで、いずれのクエリの応答にも一切使われない。

## データモデル（`crates/engine/src/sql/scalar_index.rs::ScalarIndex`）

構築元は `sql::arena_cache::SqlArenaSnapshot`（Issue #363。RLS 段適用済み・ctx
可視行のみを含むスナップショット）。索引のスロット番号は**このスナップショットの
スロット**（`snapshot.arena().ids()[slot]`／`snapshot.metadata()[slot]` の添字）
であり、クエリごとに異なる SCALAR 段適用後アリーナのスロットではない
（Issue #474 はスナップショット経由でこの写像を扱う）。

- `TEXT` 列ごとに `TextColumnIndex`（値の辞書 `values`〔バイト列昇順・重複
  排除〕・CSR 形式の一致スロット列 `offsets`/`slots`・等価直引き用
  `equality: HashMap<String, u32>`）を持つ。`NULL` 値はいずれの索引にも
  エントリを作らない（`declarative_filter::MetadataFilter::matches` の NULL
  常時不一致と同じ判定になることを単体テストで固定）
- `id` 昇順の順序索引（`id_index: Option<Vec<(u64, u32)>>`）。全行の `id` が
  `sql::udf_call::id_as_finite_scalar`（`id > 2^53` を拒否）を満たす場合のみ
  `Some`。1 件でも超過があれば索引全体を `None` にする（fail-closed。Issue #474
  が全走査へ縮退する契機になる）

構築は 1 回の O(N) スロット走査（`scan_scalar_columns` による borrow-only
デコード）と、列ごとの安定ソート・CSR 構築からなる。`u32::try_from`・
`try_reserve`（`try_reserve_exact` を含む）で untrusted な行数・値数に対する
無制限確保を防ぐ（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
概算バイト量が単体上限（`MAX_SCALAR_INDEX_BYTES`）を超える場合は構築失敗
（`ScalarIndexBuildError::TooLarge`）として索引なしへ縮退する。

## キャッシュ（`ScalarIndexCache`）

キー・世代源泉・fail-closed の`lookup`契約は`sql::arena_cache::SqlArenaCache`
（Issue #363）・`sql::sparse_cache::SparseIndexCache`（Issue #357）と同型:
`(table, PolicyContext)` の完全一致 × テーブル単位世代
（`catalog::table_generation_in_txn`）。呼び出し元の `read_txn` が古いだけの
可能性を考慮し、`lookup` の破棄判定は `storage` から読んだ「真に最新の」世代と
比較し**厳密に古い**場合のみ行う（`SqlArenaCache::lookup` と同じ理由）。

`ScalarIndexCache::insert` は **`SqlArenaCache::insert` とは意図的に非対称**
（`core.rs::PrefilterCache::insert`・Issue #280 と同じ契約）: 世代不一致・ロック
毒化・世代読み取り失敗のいずれも `None` を返し、キャッシュへ反映しないだけで
なく呼び出し元へも一切渡さない。本索引はまだ誰にも消費されない派生データであり、
`SqlArenaCache` のように「このクエリの応答に限って stale でも使ってよい」対象が
存在しないため（挿入失敗時は呼び出し元が単に索引なしとして扱う。fail-soft な
派生キャッシュ）。

容量は `MAX_SCALAR_INDEX_CACHE_ENTRIES`（32）・`MAX_SCALAR_INDEX_CACHE_TOTAL_BYTES`
（`crate::arena::MAX_ARENA_TOTAL_BYTES` と同じ桁）を超えないよう、同一キー重複
除去 → 挿入対象テーブルに限定した世代不整合エントリの一括破棄 → LRU 追い出しの
順で管理する（`SqlArenaCache`・`PrefilterCache` と同じ手順）。

統計 `ScalarIndexCacheStats`（`hits`/`misses`/`stale_evictions`/
`capacity_evictions`/`builds`/`build_failures`/`entries`）はテナント ID・行 ID・
スカラー値等の機微情報を一切含まない。`EngineCore::scalar_index_cache_stats()`
（`VectorCore` trait には載せない固有メソッド）から観測用にのみ公開する。

## `sql::exec.rs` への結線（構築のみ）

`execute_statement_with_cache` に 10 番目の引数
`scalar_cache: Option<scalar_index::ScalarCacheAccess<'_>>` を追加した
（既存の `#[allow(clippy::too_many_arguments)]` を維持）。公開 API
`execute_statement`（`sparse_cache`/`arena_cache`/`hnsw_cache` と同じく `None`
を渡す薄いラッパー）は互換性を保つ。

適用条件（gate）: `scalar_cache.is_some() && plan.scalar_prefilter &&
!bound.metadata_filters.is_empty()`（索引対応述語 `Equality`/`Prefix` を持つ
SCALAR 事前フィルタ経路のみ。`expr_filters` の分類は Issue #474 の
`classify_scalar_plan` に委ね、本 Issue の gate には含めない）。

索引の構築材料（`SqlArenaSnapshot`）は `arena_cache`（Issue #363）経由で
スナップショットが手に入った経路（ヒット・ミスいずれも）でのみ得られる。
`arena_cache` が `None`（`execute_statement` 経由・テスト等）の場合はこの
クエリでは構築しない。手順は `scalar_cache.cache.lookup(...)` → `None` なら
`ScalarIndex::build(schema, &snapshot)` → `Ok` なら `insert`（戻り値は使わない。
`Err` は `build_failures` を計上して無視）。結果は一切消費しない（候補削減は
Issue #474）。構築・登録の失敗はクエリを失敗させない（fail-soft）。

未消費の `pub(crate)` 照会 API（`candidates_for`・`candidates_equals`・
`candidates_id_range` 等）は Issue #474 の消費者向けに用意してあるが、production
の非テストビルドでは現時点で未参照のため `#[cfg_attr(not(test), allow(dead_code))]`
を付与している（`catalog.rs::insert_row_into_table` と同じ理由・パターン）。

## テスト

- in-module（`sql/scalar_index.rs`）: プロパティ的な全走査オラクル比較（等価・
  前方一致述語）、NULL の非索引化、RLS 部分可視（他テナント Private 行の非漏えい）、
  空テーブル、`id > 2^53` を含む場合の `id_index` の `None` 化、`id` 範囲照会、
  キャッシュのヒット・世代競合時 `None` 契約・キー分離
- 結合テスト（`crates/engine/tests/scalar_index_cache.rs`）: `EngineCore::
  execute_sql` 経由（production の gated 構築経路）で、索引対応述語ありクエリの
  初回 build → 反復ヒット、`WHERE` なしクエリでの gate 不発火、対象テーブルへの
  書き込みによる失効・再構築、`(table, ctx)` キー分離をそれぞれ固定。索引が
  応答へ影響しないこと（結果が索引の有無に関わらず不変）もあわせて確認する

## スコープ外（Issue #474・#475・#476 へ）

- 索引による候補削減・`ExecutionPlan` 統合・選択度切替・`classify_scalar_plan`・
  `EXPLAIN` の `scalar_plan:` 露出
- 集計・`GROUP BY` 経路への結線
- RLS 不変の統合テストスイート・損益分岐実測・閾値確定
- `IN`／`BETWEEN` 構文（現行許可リストに無い）
- `core.rs::PrefilterCache` のテーブル単位世代への統一

## 追記（Issue #632）

### 問題

crossdb fixture（25,000 行・`lang`/`topic`/`body` の 3 `TEXT` 列）では、
`ScalarIndex::build` がスキーマ中の**全 `TEXT` 列**を無条件に索引化していた
ため、長文自由記述列 `body` まで索引化され索引が約 8MiB に達し、以降の
hybrid クエリのアロケータ状態が悪化して p50 が約 10% 劣化することが実測で
判明した（`WHERE` を伴わないクエリでは差がない。`body` を索引対象から
外した実験ビルドで回復を確認済み）。

### 採用した対策

`ScalarIndex::build`（`crates/engine/src/sql/scalar_index.rs`）に列単位の
**平均値長ゲート**を追加した。`TEXT` 列ごとに、これまでに索引化した非
`NULL` 値の累積バイト量 ÷ 件数（行数ではなく実際に索引化を試みた非 `NULL`
値の件数を分母にする。NULL の多い疎な列を不当に除外しないため）が定数
`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`（暫定 128 バイト。本リポジトリの
実装既定値・spec 非関与）を超えた時点で、その列を索引対象から除外する
（`per_column[i] = None`。以降その列の値は一切複製・索引化しない）。

除外は列単位の **fail-soft な縮退**であり、`ScalarIndex::build` 自体を
失敗させる `ScalarIndexBuildError`（fail-closed。索引全体が構築されない）
とは異なる。除外された列は `ColumnType::Vector` の列と同じ「未索引」
（`columns[i] = None`）へ合流するため、`ScalarIndex::candidates_for`・
`ScalarIndex::column_groups` は既存どおり `None` を返し、呼び出し元
（`sql::exec::execute_statement_with_cache`〔Issue #474〕・
`sql::aggregate::try_scalar_index_aggregate`〔Issue #475〕・
`sql::group_by::observe_group_enumeration`〔Issue #475〕の 3 箇所）は既存の
「列が索引未対応」契約（`FallbackNoIndex`／全走査フォールバック）のまま
plain scan へ縮退する。**呼び出し側 3 箇所はいずれも無変更**である。

### 「述語参照列限定（遅延構築）」案を採らなかった理由

Issue 本文が示すもう一つの案（述語で実際に参照された列だけを索引化する
遅延・列単位構築）は、`ScalarIndex`／`ScalarIndexCache` が `(table, ctx)` ×
テーブル単位世代のみをキーにした**単一の索引インスタンス**を、SELECT の
SCALAR 事前フィルタ・集計 `WHERE`・`GROUP BY` キー列挙という異なる形状の
複数クエリで使い回す設計であるため、素直に実装すると後続の別クエリが
異なる列を参照した時点で索引の再構築・拡張・キャッシュ無効化の追加設計が
必要になりリスク・変更範囲が大きい。平均値長による列単位除外は「列が
索引対象かどうか」を構築時に一度だけ・データから静的に決定できる性質を
保てるため、既存のキャッシュ・呼び出し側を一切変更せずに済む。

### 検証

`sql/scalar_index.rs::tests` に、短い列は従来どおり索引化される回帰確認・
長文列の除外確認・除外判定が行数ではなく非 `NULL` 値件数基準であることの
確認・除外後の `approx_heap_bytes()` が除外列のコストを含まないことの確認の
4 本を追加した。`tests/scalar_index_prune.rs` に、除外列への等価・前方一致
述語が全走査と一致する結果を返しつつ `index_scans` を増やさず
`plain_scan_fallbacks` を増やすこと、短い列の索引消費が除外の影響を受けない
こと、短い列と除外列の複合述語が索引経路を一切使わないこと（`FallbackNoIndex`
の既存契約どおり、述語 1 つでも `candidates_for` が `None` を返せば全体が
縮退する）、除外列の値が RLS 判定をバイパスしないこと（他テナント private 行
の非漏えい）の 5 シナリオを追加した。`tests/scalar_index_aggregate.rs` にも、
`GROUP BY`（`WHERE` なし・列挙形）が除外列に対して `aggregate_index_scans` を
消費せず全走査と一致することを確認するテストを 1 本追加した。

### スコープ外・申し送り

crossdb fixture 相当での hybrid p50 の前後比較実測（Issue #632 本文の受け入れ
条件）は次 Issue（#633）へ申し送った。閾値
`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`（128）の最終値は本リポジトリの実装
既定値であり、前後比較実測 Issue の結果次第で再検討され得る。

## 前後比較実測（Issue #633）

### 計測条件

| arm | commit | 位置づけ | wire-server バイナリ sha256（先頭 12 桁） |
| --- | --- | --- | --- |
| before | `773a835` | Issue #638（本節の対策）マージの親（`body` 列も無条件に索引化） | `4b950db35d5c` |
| after | `6ff22dc` | Issue #638 マージコミット（列単位の平均値長ゲート適用後） | `ba67777ec994` |
| ref | `ee99db3` | 退行導入（`ScalarIndex` 自体の新設・#473・`875d38d`）より前の基準 | `becfb9f9bb26` |

`git diff --stat 773a835 6ff22dc -- Cargo.lock Cargo.toml scripts/crossdb_bench
crates/wire-server` は空であり、before/after は同一ハーネス・同一依存・同一
`wire-server` ソースで、差分は `crates/engine/src/sql/scalar_index.rs`・
テスト・docs のみ（同一ビルド条件の根拠）。3 arm とも `git archive` で独立
ソースツリーへ展開し、`CARGO_TARGET_DIR` を分離して個別に `cargo build
--release -p wire-server` した。ハーネス（`scripts/crossdb_bench/*.py`・
`scripts/bench_scalar_index_crossdb_ab.sh`）は現行ワークツリー（harness
commit `6ff22dc8`）のものを全 arm 共通で使用し、`CROSSDB_SELF_BINARY` で
起動するバイナリのみを差し替えた（Issue #479 の方式）。

- 環境: 共有 QEMU（`QEMU Virtual CPU version 2.5+`・nproc=12・
  `BENCH_DEDICATED_ENV` 未設定）。`docs/design/benchmark-judgement-policy.md`
  §5 に従い**参考値・採否根拠にしない**。
  他 worktree のジョブが並走していた可能性があるため loadavg も生データと
  ともに記録した。
- ペア数: 5（交互 before→after→ref の輪番。`docs/design/
  benchmark-judgement-policy.md` §3）。
- fixture: `docs25k.redb`／`docs25k.jsonl`／`queries200.jsonl`（25,000 行・
  dim 128。`docs/design/crossdb-bench.md` と同一）。
- 対象区間: crossdb self の 4 フェーズ（`hybrid_rrf`・`bulk_hybrid_k200`・
  `vector_knn_where`・`where_compound_count`）＋参照区間（`vector_knn`・
  `mode_recall`）は `scripts/crossdb_bench/run.py --db self --config exact`
  経由。加えて「`WHERE` 実行後の `hybrid_rrf` 単独ループ・RSS」を
  `scripts/crossdb_bench/hybrid_after_where.py`（Issue #632 切り分け時の
  scratch スクリプトの tracked 版）で 3 モード（`hybrid`＝ウォームアップ
  なし対照・`warm_where_then_hybrid`＝`WHERE lang = 'ja'` を 50 本実行して
  `ScalarIndex` を構築させてから計測〔Issue #632 の再現条件〕・
  `body_predicate`＝除外候補列 `body` への前方一致 `WHERE body LIKE
  '<prefix>%'`）計測した。
- 生データ: `docs/design/bench-data/scalar-index-crossdb-ab/`（tracked）。
  再現: `make bench-scalar-index-crossdb-ab BEFORE_COMMIT=773a835
  AFTER_COMMIT=6ff22dc REF_COMMIT=ee99db3 CROSSDB_DIR=<dir>
  CROSSDB_PYTHON=<python>`。集約: `scripts/bench_scalar_index_crossdb_ab.sh
  --summarize docs/design/bench-data/scalar-index-crossdb-ab`。

### 結果（min-of-5・median 併記。単位 µs）

| 区間 | before min/median | after min/median | after/before（min比） | 判定 | ref min/median | ref/before |
| --- | --- | --- | --- | --- | --- | --- |
| `hybrid_rrf`.p50 | 6995 / 7070 | 7036 / 7084 | 1.0057 | 帯内 | 6130 / 6278 | 0.8763（ref が速い） |
| `bulk_hybrid_k200`.p50 | 9618 / 9652 | 9615 / 9657 | 0.9997 | 帯内 | 9649 / 9871 | 1.0033 |
| `vector_knn_where`.p50 | 1897 / 1989 | 1959 / 2026 | 1.0328 | 帯内 | 3037 / 3141 | 1.6012（ref が遅い＝#473 以降の高速化分） |
| `where_compound_count`.p50 | 1025 / 1046 | 1018 / 1025 | 0.9936 | 帯内 | 4344 / 4369 | 4.2405（同上） |
| 参照: `vector_knn`.p50 | 671 / 690 | 655 / 737 | 0.9767 | 帯内 | 699 / 707 | 1.0426 |
| 参照: `mode_recall`.p50 | 686 / 706 | 671 / 696 | 0.9780 | 帯内 | 705 / 738 | 1.0267 |
| hybrid ループ `warm_where_then_hybrid`.p50 | 6851 / 6886 | 6806 / 6849 | 0.9935 | 帯内 | 6210 / 6241 | **0.9065（ref が約 9% 速い）** |
| hybrid ループ `warm_where_then_hybrid`.rss_after_warm（MiB） | 63.80 / 63.92 | 63.72 / 63.86 | 0.9988 | 帯内 | 56.30 / 56.37 | **0.8825（ref が約 12% 少ない）** |
| hybrid ループ `body_predicate`.p50 | 106.9 / 107.4 | 106.6 / 107.8 | 0.9976 | 帯内 | 1735 / 1754 | 16.24（ref が大幅に遅い＝索引自体が無い） |

完全な区間別 TSV（p95・RSS 全 3 時点を含む）は上記コマンドの出力
（`docs/design/bench-data/scalar-index-crossdb-ab/` の生データから再計算
可能）。

### 判定: `hybrid_rrf` 退行は本 fixture では解消されていない

**受け入れ条件 1〜2（`hybrid_rrf` 退行の解消・ee99db3 水準への回復）は
未達成**と判断する。before→after で `hybrid_rrf`・`warm_where_then_hybrid`
とも ratio が 0.99〜1.01（固定 ±5% 帯内）にとどまり、ref（ee99db3）との
比較でも before・after いずれも ref よりなお約 9〜13% 遅い・RSS も約 12〜
13% 多いままで、before/after 間にほとんど差が無い。

原因は `docs25k.jsonl` の `body` 列の実測平均バイト長（tenant-a 可視行
23,000 件で **126.3 バイト**）が `MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`
（**128 バイト**）を約 1.7 バイト下回ることにある。`ScalarIndex::build`
（`crates/engine/src/sql/scalar_index.rs`）の除外判定は行単位の**累積**
平均（`prospective_bytes > prospective_count * 128`）であり、コーパス全体
の平均が閾値未満である以上、走査のどの時点でも累積平均が閾値を超えず
`body` 列は除外されない——つまり **Issue #632／#638 の対策は、本 issue が
問題を発見した crossdb fixture そのものに対しては no-op**である。
`warm_where_then_hybrid.rss_after_warm` が before/after でほぼ同じ
（63.80 → 63.72 MiB）であることが、除外が発火していないことの直接証跡
になっている（除外が発火していれば Issue #632 の手動実験と同様に約 56MiB
まで下がるはずだった）。

一方で `body_predicate`（`body` への前方一致述語）は before・after とも
高速（約 107µs。ref は 1735µs）だが、これは索引による除外の効果ではなく、
before の時点で既に `ScalarIndex` が `body` を索引化しているため（除外が
発火していないのだから当然）——「除外列が plain scan へ縮退する」という
受け入れ条件 3 の効果は、本実測では**観測できていない**（`body` が索引
対象から一度も外れていないため）。この経路の実際の縮退影響（除外時の
p50/p95 低下幅）を確かめるには、平均バイト長が確実に 128 を超える
（例: 200 バイト超）`body` 列を持つ fixture での再測定が必要。

### 閾値 `MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`（128）の再検討要否

**本実測は現行閾値 128 を支持しない。** 本 issue の発端となった crossdb
fixture 自体が閾値のわずか下（126.3 バイト）にあり、実装が意図した「長文
`body` 列の除外」が対象 fixture に対して発火しない。以下のいずれかの
対応が必要と考えられる（採否はオーナー判断）:

- 閾値を crossdb fixture の `body` 実測平均（126.3 バイト）を下回る値
  （例: 96〜100 バイト）へ引き下げる。
- 平均バイト長ではなく他の基準（総バイト量・最大値長・列のカーディナリ
  ティ等）で除外判定する設計へ変更する。
- 閾値はそのまま維持し、`docs25k.jsonl` 側の `body` 生成方式を見直して
  クロス DB ベンチの母集団を意図的に長文化する。

いずれも本 Issue のスコープ外（`crates/engine/src/` は本 Issue で無変更）
とし、判断材料としてこの節を残す。

### 限界・申し送り

- 共有 QEMU 環境の参考値であり、専有環境での再実測はオーナー作業として
  申し送る（`docs/design/benchmark-judgement-policy.md` §9 と同方針）。
- `ref`（ee99db3）は `Statement::Scan`（Issue #454・#562）以前のコミットの
  ため `scan_where_nosort_k500` フェーズは `unsupported` として記録される
  （ハーネスの許可リスト拒否検知により fail-closed に処理済み。失敗では
  ない）。
- `hybrid-rrf-latency-breakdown.md` 側の対応する記述の訂正・整合は
  Issue #637 の担当とする（本 doc では触れない）。
- 閾値再検討（上記）は本 Issue の受け入れ条件外のため、判断のみ記録し
  実装は行わない。実装を伴う対応はユーザー承認のうえ別 Issue で扱う。
