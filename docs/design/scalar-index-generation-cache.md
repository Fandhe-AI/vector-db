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
`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`（現行値 64 バイト。#632 では暫定
128 を採用したが #633 実測を受け #644 で 64 へ引き下げ済み。「閾値の見直し
（Issue #644）」節参照。本リポジトリの実装既定値・spec 非関与）を超えた
時点で、その列を索引対象から除外する
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
`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`（当時 128）の最終値は本リポジトリの
実装既定値であり、前後比較実測 Issue の結果次第で再検討され得る（実際に
Issue #633 の実測を受け Issue #644 で 64 へ引き下げ済み。「閾値の見直し
（Issue #644）」節参照）。

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
- ペア数: 5（交互実行。`docs/design/benchmark-judgement-policy.md` §3
  「baseline/cand1/baseline/cand2/… の輪番」）。本節の生データ取得時点の
  `bench_scalar_index_crossdb_ab.sh` は before→after→ref の順で before を
  1 回しか計測しておらず、ref との比較が after 計測ぶん時間的に隔たった
  before を参照する構成だった（codex-review P1 指摘・Issue #633）。指摘を
  受けて同スクリプトは候補（after／ref）ごとに専用の直近 baseline 計測
  （ref 用は `before_ref`）を挟む before→after→before_ref→ref の輪番へ
  修正済みだが、本節の生データ自体は修正前に取得したものであり
  再計測はしていない（`bench_scalar_index_crossdb_ab_summarize.py` は
  この旧形式データを再集計する際 `before_ref` 不在を検出して `before`
  へ自動フォールバックする——出力される値は本節の表と同一）。
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
  --summarize docs/design/bench-data/scalar-index-crossdb-ab 20260908T064132Z`
  （本 doc に別セッションの生データ〔Issue #645〕が同一ディレクトリへ
  追加された後は `session_ts` 明示指定が必須になったため付記した）。

### 結果（min-of-5・median 併記。単位 µs）

判定は `docs/design/benchmark-judgement-policy.md` §4 の 2 種ノイズ帯
（固定 ±5% と参照区間実測帯を**両方**超えて初めて `regressed`/`improved`。
片方のみは「帯内」）に従う（codex-review P1 指摘・Issue #633）。
`reference_band` は候補（after／ref）ごとの baseline と candidate それぞれの
`vector_knn.p50` run-to-run 値列を連結した幅（`(max-min)/min`。
`scripts/bench_scalar_index_crossdb_ab_summarize.py::reference_band`）から
算出した実測ノイズ帯で、after は **16.48%**（`min=655.15 max=763.14`。
baseline=`before`・candidate=`after`）、ref は **8.53%**（`min=670.78
max=727.97`。baseline=`before`〔フォールバック〕・candidate=`ref`）——
固定 ±5% 帯より大きく広い（after）／狭い（ref）ため、下表の「判定」は
固定帯だけで機械的に出した値ではなく、この実測帯もあわせて適用したもの
（算出根拠は上記コマンドの stderr 出力・完全な TSV に記録。生データは
tracked のため誰でも再計算できる）。

| 区間 | before min/median | after min/median | after/before（min比） | after 判定（固定帯＋参照帯 16.48%） | ref min/median | ref/before | ref 判定（固定帯＋参照帯 8.53%） |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `hybrid_rrf`.p50 | 6995 / 7070 | 7036 / 7084 | 1.0057 | 帯内 | 6130 / 6278 | 0.8763 | improved（ref が速い） |
| `bulk_hybrid_k200`.p50 | 9618 / 9652 | 9615 / 9657 | 0.9997 | 帯内 | 9649 / 9871 | 1.0033 | 帯内 |
| `vector_knn_where`.p50 | 1897 / 1989 | 1959 / 2026 | 1.0328 | 帯内 | 3037 / 3141 | 1.6012 | regressed（ref が遅い＝#473 以降の高速化分） |
| `where_compound_count`.p50 | 1025 / 1046 | 1018 / 1025 | 0.9936 | 帯内 | 4344 / 4369 | 4.2405 | regressed（同上） |
| 参照: `vector_knn`.p50 | 671 / 690 | 655 / 737 | 0.9767 | 帯内 | 699 / 707 | 1.0426 | 帯内 |
| 参照: `mode_recall`.p50 | 686 / 706 | 671 / 696 | 0.9780 | 帯内 | 705 / 738 | 1.0267 | 帯内 |
| hybrid ループ `warm_where_then_hybrid`.p50 | 6851 / 6886 | 6806 / 6849 | 0.9935 | 帯内 | 6210 / 6241 | 0.9065 | improved（ref が約 9% 速い） |
| hybrid ループ `warm_where_then_hybrid`.rss_after_warm（MiB） | 63.80 / 63.92 | 63.72 / 63.86 | 0.9988 | 帯内 | 56.30 / 56.37 | 0.8825 | improved（ref が約 12% 少ない） |
| hybrid ループ `body_predicate`.p50 | 106.9 / 107.4 | 106.6 / 107.8 | 0.9976 | 帯内 | 1735 / 1754 | 16.24 | regressed（ref が大幅に遅い＝索引自体が無い） |

本 issue の受け入れ判定に使う before→after 比較（4 対象区間・参照区間とも）
は、固定帯・参照帯（16.48%）のいずれの基準でも「帯内」で変わらない
（実測ノイズ帯を導入しても判定結果自体は変化しない。ref との比較のみ
参照帯〔8.53%〕の下で新たに `regressed`/`improved` に分類される行がある
——ただしこれらは元々 before/after 比較ではなく「ref との差」の解説として
記録していた値であり、本 issue の受け入れ判定〔下記「判定」節〕には使わない）。

完全な区間別 TSV（p95・RSS 全 3 時点・`*_ref_band`／`*_class` 列を含む）は
上記コマンドの出力（`docs/design/bench-data/scalar-index-crossdb-ab/` の
生データから再計算
可能）。

### 判定: `hybrid_rrf` 退行は本 fixture では解消されていない

**受け入れ条件 1〜2（`hybrid_rrf` 退行の解消・ee99db3 水準への回復）は
未達成**と判断する。before→after で `hybrid_rrf`・`warm_where_then_hybrid`
とも ratio が 0.99〜1.01（固定 ±5% 帯内）にとどまり、ref（ee99db3）との
比較でも before・after いずれも ref よりなお約 9〜13% 遅い・RSS も約 12〜
13% 多いままで、before/after 間にほとんど差が無い。

`ScalarIndex::build`（`crates/engine/src/sql/scalar_index.rs`）の除外
判定は行単位の**累積**平均（`prospective_bytes > prospective_count *
128`）であり、これは全体平均とは別の量である——コーパス全体の平均が
閾値未満でも、走査順の途中区間だけが閾値を超えていれば累積平均は一時的に
閾値を超えて除外が発火しうる（例: 200 バイト行→0 バイト行の順なら全体
平均は 100 だが 1 行目の時点で既に閾値超過）。そのため `docs25k.jsonl` の
`body` 列の実測**全体**平均バイト長（tenant-a 可視行 23,000 件で
**126.3 バイト**。`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`＝**128 バイト**を
約 1.7 バイト下回る）だけでは「除外が一度も発火しない」ことの根拠には
ならない（codex-review P2 指摘・Issue #633）。

除外が発火していないことの実際の根拠は、`warm_where_then_hybrid.
rss_after_warm` が before/after でほぼ同じ（63.80 → 63.72 MiB）である
という直接の実測証跡である（除外が発火していれば Issue #632 の手動実験
と同様に約 56MiB まで下がるはずだった）——**Issue #632／#638 の対策は、
本 issue が問題を発見した crossdb fixture そのものに対しては no-op**
であるとこの RSS 実測から判断する。全体平均が閾値のわずか下にあるという
事実は、除外が発火しない**蓋然性が高い**ことの補助的な状況証拠に留め、
本節の no-op 判断そのものは RSS 実測のみを根拠とする（走査順における
累積平均の実際の推移・索引構築後の列別除外状態を直接ダンプする手段は
本 issue のスコープ〔`crates/engine/src/` 無変更〕では用意していない）。

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

### 閾値の見直し（Issue #644）

上記 3 案のうち、オーナー承認済み（2026-09-08）で 1 案目（閾値の引き下げ）
を採用し、`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN` を **128 → 64** バイトへ
引き下げた（`crates/engine/src/sql/scalar_index.rs`）。判断基準（平均値長。
非 `NULL` 値の累積バイト量 ÷ 件数の**行単位累積平均**）そのものは変更せず、
2〜3 案目（総バイト量・最大値長・カーディナリティ等の別基準への変更、
fixture 側の `body` 生成方式見直し）は採らなかった。

- **採用値の根拠**: #633 実測で crossdb fixture `body` 列の全体平均が
  126.3 バイトと判明し、128 では実質的にゲートが発火しない（RSS 実測
  不変）ことが確定した。64 であれば crossdb `body` 相当（100 バイト超）は
  確実に上回り、判定が行単位の累積平均であることを踏まえても実質的に
  1 行目の時点で除外が発火する見込みである。一方 `lang`（2 バイト程度）・
  `topic`（十数バイト）のような短い分類値列は 64 を大きく下回るため
  引き続き索引対象に残る。
- **境界テストの追加**: `sql/scalar_index.rs::tests` に、crossdb fixture
  相当（全行 126 バイト固定）での除外確認・短い分類値列相当（全行 32
  バイト固定）での索引維持確認、および定数相対の境界値テスト（ちょうど
  閾値バイトは索引対象に残る／閾値 + 1 バイトは除外される。厳密な `>`
  比較であることの固定）の計 4 本を追加した。既存 4 本（Issue #632）は
  定数相対・短い固定値のため 64 化に伴う修正は不要だった。
- **既存テストへの影響**: `tests/scalar_index_prune.rs`・
  `scalar_index_aggregate.rs`・`scalar_index_cache.rs` はいずれも閾値
  非依存の安全マージン（`repeat(1000)` 以上・`3 MiB` 等）で TEXT 値を
  構成しており、64 への引き下げ後も無変更のまま green であることを確認
  した。65〜128 バイトの TEXT 値で索引化を前提にする既存テストは無い。
- **`classify_scalar_plan`／`resolve_candidates` の契約**: 除外列を含む
  述語が plain scan へ縮退する契約（Issue #632・#474）はそのまま不変。
  クエリ結果・`EXPLAIN` の `scalar_plan:` 出力（静的分類）も本変更の前後で
  不変。
- **申し送り**: crossdb fixture での RSS 低下・`hybrid_rrf` p50 回復の
  前後比較実測は本 Issue のスコープ外であり、後続 Issue（実測手段は
  `make bench-scalar-index-crossdb-ab`）の担当とする。64 でも不十分と
  判明した場合の基準再検討（総バイト量・最大値長・カーディナリティ等）も
  同様に別 Issue の担当とする（実測結果は「前後比較実測（Issue #645）」節
  参照。除外発火の非 vacuous 証跡は確認できたため基準再検討は現時点で
  不要と判断）。

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

## 前後比較実測（Issue #645）

Issue #644（`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN` 128→64）適用後の
crossdb fixture 上での効果を事後確認する。

### 計測条件

| arm | commit | 位置づけ | wire-server バイナリ sha256（先頭 12 桁） |
| --- | --- | --- | --- |
| before | `cbe80cf` | Issue #646（本節の対策）マージの親（閾値 128） | `ba67777ec994` |
| after | `2f1cd80` | Issue #646 マージコミット（閾値 64 適用後） | `8e3085ecd225` |
| ref | `ee99db3` | 退行導入（`ScalarIndex` 自体の新設・#473・`875d38d`）より前の基準 | `becfb9f9bb26` |

`git diff --stat cbe80cf 2f1cd80 -- Cargo.lock Cargo.toml scripts/crossdb_bench
crates/wire-server scripts/bench_scalar_index_crossdb_ab*` は空であり、
before/after 間の差分は `CLAUDE.md`・`crates/engine/src/sql/scalar_index.rs`・
`docs/design/scalar-index-generation-cache.md` のみ（同一ビルド条件の根拠。
`before` の wire-server バイナリ sha256 が Issue #633 節の `after`〔`6ff22dc`〕と
一致するのは、その間の全コミットが wire-server バイナリへ影響しない
テスト・docs 変更のみだったため）。ハーネス（harness commit `2f1cd80`）は
Issue #633 と同一の `scripts/bench_scalar_index_crossdb_ab.sh`・
`scripts/crossdb_bench/*.py` を使用し、3 arm を `git archive` で独立ソース
ツリーへ展開・`CARGO_TARGET_DIR` を分離して個別に `cargo build --release
-p wire-server` した。輪番は before→after→before_ref→ref（Issue #633
codex-review P1 指摘対応後の方式。候補〔after／ref〕ごとに専用の直近
baseline を挟む）。

- 環境: 共有 QEMU（`QEMU Virtual CPU version 2.5+`・nproc=12・
  `BENCH_DEDICATED_ENV` 未設定）。`docs/design/benchmark-judgement-policy.md`
  §5 に従い**参考値・採否根拠にしない**。計測開始時の loadavg は 5.44
  （他ジョブ並走の可能性）と Issue #633 節よりやや高めだったため、
  この点も踏まえて below のノイズ帯実測を優先する。
- ペア数: 5（交互実行）。
- fixture: `docs25k.redb`／`docs25k.jsonl`／`queries200.jsonl`（25,000 行・
  dim 128。Issue #633 と同一）。
- 対象区間: Issue #633 と同一（crossdb self 4 フェーズ・参照区間 2 本・
  hybrid ループ 3 モード・RSS 3 時点）。
- 生データ: `docs/design/bench-data/scalar-index-crossdb-ab/`
  （`20260908T102007Z-*`。tracked）。再現: `make
  bench-scalar-index-crossdb-ab BEFORE_COMMIT=cbe80cf AFTER_COMMIT=2f1cd80
  REF_COMMIT=ee99db3 CROSSDB_DIR=<dir> CROSSDB_PYTHON=<python>`。集約:
  `scripts/bench_scalar_index_crossdb_ab.sh --summarize
  docs/design/bench-data/scalar-index-crossdb-ab 20260908T102007Z`
  （本 doc に Issue #633 のセッションと混在するため `session_ts` 明示が
  必須）。

### 結果（min-of-5・median 併記。単位 µs、RSS は MiB）

判定は Issue #633 節と同じ 2 種ノイズ帯（固定 ±5% と参照区間実測帯を
**両方**超えて初めて `regressed`/`improved`）に従う。実測ノイズ帯
（`vector_knn.p50` run-to-run 幅）は after が **16.18%**（before+after
プール）、ref が **8.64%**（before_ref+ref プール）——固定 ±5% 帯より
いずれも広い。

| 区間 | before min/median | after min/median | after/before（min比） | after 判定 | ref min/median | ref/before_ref | ref 判定 | after/ref（min比・参考） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `hybrid_rrf`.p50 | 6869 / 7043 | 6608 / 6731 | 0.9620 | 帯内（固定帯超えず） | 6335 / 6347 | 0.9129 | improved | 1.0431 |
| `bulk_hybrid_k200`.p50 | 9517 / 9654 | 9210 / 9279 | 0.9678 | 帯内 | 9752 / 9808 | 1.0297 | 帯内 | 0.9444 |
| `vector_knn_where`.p50 | 1919 / 1959 | 1976 / 2002 | 1.0294 | 帯内 | 2989 / 3069 | 1.5709 | regressed（ref が遅い＝#473 以降の高速化分） | 0.6611 |
| `where_compound_count`.p50 | 1011 / 1024 | 1027 / 1030 | 1.0161 | 帯内 | 4331 / 4373 | 4.2089 | regressed（同上） | 0.2371 |
| 参照: `vector_knn`.p50 | 671 / 679 | 673 / 691 | 1.0028 | 帯内 | 695 / 725 | 1.0218 | 帯内 | — |
| 参照: `mode_recall`.p50 | 676 / 708 | 696 / 703 | 1.0298 | 帯内 | 691 / 715 | 1.0289 | 帯内 | — |
| hybrid ループ `warm_where_then_hybrid`.p50 | 6850 / 6884 | 6177 / 6267 | **0.9017**（約 9.8% 改善） | 帯内（参照帯 16.18% を超えず） | 6172 / 6216 | 0.9074 | improved | **1.0008**（ref とほぼ同水準） |
| hybrid ループ `warm_where_then_hybrid`.rss_after_warm | 63.73 / 63.91 | **55.58 / 55.67**（約 12.8% 減） | 0.8720 | 帯内（固定帯超えるが判定は参照帯優先で帯内） | 56.32 / 56.35 | 0.8834 | improved | 1.0129 |
| hybrid ループ `body_predicate`.p50 | 107.3 / 108.2 | **1765.9 / 1774.3**（約 16.5 倍増） | **16.4536** | **regressed**（両帯超え） | 1747.1 / 1754.4 | 16.4493 | regressed（同水準） | 1.0108（ref とほぼ同一） |
| hybrid ループ `body_predicate`.rss_after_warm | 58.91 / 59.00 | 51.04 / 51.19 | 0.8663 | 帯内 | 47.79 / 47.88 | 0.8090 | improved | 1.0680 |

完全な区間別 TSV（p95・RSS 全 3 時点・`*_ref_band`／`*_class` 列を含む）は
上記コマンドの出力（生データから再計算可能）。

### 除外発火の非 vacuous 証跡（受け入れ条件 b）

**`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN` の 128→64 引き下げにより、
crossdb fixture の `body` 列で除外が確実に発火するようになったことを
2 系統の直接証跡で確認した**（Issue #633 節が要求していた「本 fixture で
除外が発火することの直接実測」が揃った）:

1. **`warm_where_then_hybrid.rss_after_warm`**: before 63.73 MiB
   （閾値 128・除外未発火。Issue #633 節の記録と一致）→ after **55.58 MiB**
   （閾値 64・除外発火）。ref（`ee99db3`＝`ScalarIndex` 自体が存在しない
   基準）は 56.32 MiB で after とほぼ同水準（比 1.0129）——`body` を
   索引化しない状態（ref）と、索引化はするが `body` だけ除外する状態
   （after）が同じ RSS 水準に収束しており、#632 手動実験が想定した
   「約 56 MiB」という見込み値とも整合する。
2. **`body_predicate.p50`（除外列 `body` への前方一致述語）**: before
   107.3µs（`body` が索引対象＝索引経由の高速応答）→ after **1765.9µs**
   （約 16.5 倍に悪化）。ref は 1747.1µs で after とほぼ同一（比 1.0108）
   ——除外された `body` への前方一致述語が `classify_scalar_plan`／
   `resolve_candidates`（Issue #632・#474）の契約どおり plain scan へ
   縮退し、`ScalarIndex` 自体が存在しない ref と同水準の応答時間になった
   ことを直接示す。

両証跡とも Issue #633 節が「観測できていない」としていた効果（除外発火に
よる RSS 低下・plain scan 縮退）を、この 2 回目の実測で確認できたことに
なる。

### plain scan 縮退の影響（受け入れ条件 c）

上記 2. のとおり、`body` への前方一致述語（`body_predicate` モード）は
索引除外後に **約 16.5 倍**遅くなる（107.3µs → 1765.9µs、min-of-5・
固定 ±5% 帯・参照帯 16.18% のいずれも超える `regressed` 判定）。これは
「除外列を含む述語は索引を使わず plain scan へ縮退する」という
Issue #632・#474 で確定済みの契約どおりの構造的な結果であり、`ScalarIndex`
自体が存在しない ref（1747.1µs）とほぼ同一の応答時間（比 1.0108）に
収束していることから、実装上の異常ではなく契約が意図どおり働いている
証跡と判断する。

一方で `body` 以外の列（`lang`・`topic` 等の短い分類値列。閾値 64 を
下回るため引き続き索引対象）を対象にした等価・前方一致述語には本節の
悪化は及ばない——`vector_knn_where`・`where_compound_count`（いずれも
`lang`／`topic` 相当の短い列への述語と推測される既存 crossdb フェーズ）は
before→after で `regressed` 判定に至っていない（帯内）。

`body` への前方一致検索の性能を維持したい場合は索引対象へ戻す（閾値を
上げる）以外に手段が無く、これは長文列の索引構築コスト（RSS・
`hybrid_rrf` 退行）とのトレードオフである。**採否（`body` 列への前方一致
検索を優先するか、`hybrid_rrf` の RSS・レイテンシを優先するか）は
オーナー判断とし、本 Issue では判断のみ記録し閾値の再変更は行わない。**

### 判定

**受け入れ条件 (a)（`hybrid_rrf` p50 が固定・参照帯を超えて改善し
ref 比 ±5% 帯内へ収束）は部分的達成**:

- `hybrid_rrf`.p50: before→after で 3.80% 改善（0.9620）——固定 ±5% 帯
  以内にとどまり Issue #633 節の判定基準では「帯内」（両帯超えを要求する
  厳密な `regressed`/`improved` 判定には至らない）。ただし after/ref 比は
  **1.0431**（ref より 4.3% 遅いのみ）まで縮まっており、Issue #633 節の
  「before・after いずれも ref よりなお約 9〜13% 遅い」という状態からは
  明確に改善している。
- `warm_where_then_hybrid`.p50（`WHERE` 実行後に `ScalarIndex` を構築
  させてから計測する Issue #632 の再現条件）: before→after で **9.83%
  改善**（0.9017）——固定 ±5% 帯は超えるが参照帯 16.18% には届かず
  「帯内」判定にとどまる。一方 after/ref 比は **1.0008**——ほぼ完全に
  ref 水準へ収束している。
- 両指標とも「両ノイズ帯を超える」という Issue #633 節の厳密な判定基準
  では `regressed`/`improved` の確定判定に至らないが、これは主に参照帯
  自体が広い（16.18%／8.64%）ことによるものであり、after/ref 比が
  1.00〜1.04 まで収束したという事実（Issue #633 節時点は before/after が
  ほぼ無風だった）は方向として明確な改善を示す。

**受け入れ条件 (b)（RSS 低下による除外発火確認）は達成**——上記「除外
発火の非 vacuous 証跡」節のとおり、`rss_after_warm` の低下（63.73→55.58
MiB）と `body_predicate.p50` の plain scan 相当への悪化（107.3→1765.9µs）
の 2 系統で確認済み。

**受け入れ条件 (c)（plain scan 縮退の実測値記録）は達成**——上記「plain
scan 縮退の影響」節のとおり記録済み。

**親 #630 の close 可否**: 「WHERE 実行後の hybrid_rrf 約 10% 退行」の
是正という親課題の目的に対し、`warm_where_then_hybrid`.p50（Issue #632 の
再現条件そのもの）が before→after で約 9.8% 改善し after/ref 比 1.0008
（ref とほぼ同水準）まで収束したことは、目的達成の直接証跡と判断する。
`hybrid_rrf`（`WHERE` を経由しない単発 hybrid クエリ）側の改善幅が
相対的に小さいのは、除外閾値の変更が効くのは「`WHERE` 述語評価時に
`ScalarIndex` が長文 `body` 列まで索引化してしまう」経路（`warm_where_
then_hybrid`）に限られ、`WHERE` を経由しない `hybrid_rrf` 自体は元々
`ScalarIndex` の構築コストを踏まない設計（フィルタなし hybrid クエリは
`sql/sparse_cache.rs` 等の別キャッシュ経路）であるためと考えられる——
両指標の挙動差自体が「除外閾値の効果は `ScalarIndex` 構築を伴う経路に
限定される」という設計の妥当性を裏付ける。共有 QEMU 環境の参考値である
ことを踏まえると、**専有環境での再実測により両ノイズ帯を明確に超える
確証が得られることを条件に、親 #630 は close 可能**と判断する（本
Issue 単独では「方向は確認済みだが共有環境のノイズ帯の広さにより
統計的な確定判定には至らない」という限定付きの達成に留める）。

### 限界・申し送り（Issue #645）

- 共有 QEMU 環境の参考値であり、専有環境での再実測はオーナー作業として
  申し送る（`docs/design/benchmark-judgement-policy.md` §9 と同方針。
  親 #630 の確定 close 判断もこの再実測を条件とする）。
- 参照帯（16.18%／8.64%）が本節の主要な改善幅（3.8%〜9.8%）より広く、
  「両ノイズ帯超え」を要求する厳密な判定では `regressed`/`improved` の
  確定に至らない指標がある。after/ref 比の収束（1.00〜1.04）を補助的な
  傍証として判定に用いた（詳細は上記「判定」節）。
- `body` 列への前方一致検索が plain scan へ縮退することによる約 16.5 倍の
  レイテンシ悪化は契約どおりの構造的結果であり、本 Issue のスコープでは
  対応しない（採否はオーナー判断。上記「plain scan 縮退の影響」節）。
- `ref`（ee99db3）は Issue #633 節と同じ理由で `scan_where_nosort_k500`
  フェーズが `unsupported` として記録される（fail-closed 処理済み。
  失敗ではない）。
- 閾値 64 でも不十分と判明した場合の基準再検討（総バイト量・最大値長・
  カーディナリティ等）は、本節の実測（除外発火の非 vacuous 証跡を確認
  済み）を踏まえると現時点では不要と判断する。今後 crossdb fixture の
  `body` 生成方式が変わり平均長が再び 64 バイト未満になった場合は
  再検討が必要になりうる。
