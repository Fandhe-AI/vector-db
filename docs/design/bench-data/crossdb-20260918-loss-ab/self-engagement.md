# self（wire-server）負けフェーズの高速経路到達確認（実測・2026-09-18）

- 対象: crossdb 負けフェーズ 7 件（questionnaire.md）。fixture は `$S/docs25k.redb`（25,000 行・可視 23,000 行〔tenant-a public〕・`lang='ja'` 7,621 行・dim 128）。
- 環境: Apple M4 Max（16 コア・macOS 26.6.2）、`target/release/wire-server`（sha256 先頭 `f4f54896f6ca`＝`env.txt` の self 再計測バイナリ・HEAD `64cb381`）、rustc 1.98.1、redb 4.2.0。共有デスクトップ環境の参考値。
- production コード無変更。git 操作なし。作業物は全て `$S/../loss-analysis/`（`work/`・`harness/`・`wire_measure.py`）。
- **EXPLAIN は使えなかった**: `EXPLAIN` は `SELECT ... USING PLAN(...)` 文専用（`self_db.py:730-748` の実機確認コメント）で、かつ `USING PLAN` は非 nullable TEXT の `path`/`body` 列を要求するため `docs` テーブルでは束縛できない（`self_db.py:389-393`）。代わりに (a) `bench-internals` feature を有効にした scratch 側 path 依存クレート（`harness/`）で同一 SQL を engine in-process 実行し、`EngineCore::{sql_arena,sparse_index,scalar_index,visible_bitmap}_cache_stats()` の **クエリごとのカウンタ増分**で実行時経路を確定、(b) コード読解（file:line）で静的判定を裏付けた。

## 1. 計測方法

| トラック | 内容 |
| --- | --- |
| A: wire（`wire_measure.py`） | redb を `work/wire.redb` へコピー → `wire-server --users <tmp users.txt> --db work/wire.redb --bind 127.0.0.1:15499` を起動 → `$S/venv` の psycopg（simple query・`prepare_threshold=None`・`autocommit=True`）で `self_db.py` と同一 SQL・同一条件（`common.measure`: warmup 5 / iters 50、クエリは `i % 200`）で計測。終了時に `terminate()`（rc=-15 を確認）。 |
| B: in-process（`harness/`） | 別コピー `work/inproc.redb` を `EngineCore::open` → `PolicyContext::new("tenant-a")` → `execute_sql`（wire と同じ `execute_validated_in_session` 経路・キャッシュ結線は `core.rs:2407-2435`〔SELECT〕・`core.rs:2760-2773`〔集計〕）。cold 1 回 → warmup 5 → 50 回で p50/p95、その 50 回のキャッシュ統計増分を記録。hybrid は `engine::hybrid::sparse_refetch_observed`（`hybrid.rs:1785`。`bench-internals` 限定）と `search()` 呼び出しを数える `SearchProvider` ラッパ＋`hybrid_search` で再取得ラウンド数を観測。 |
| fsync | `rustc --print sysroot` 配下の std ソースと `~/.cargo/registry` の redb 4.2.0 ソースを file:line で確認し、同ボリューム上で Python `fcntl` により `F_FULLFSYNC`/`F_BARRIERFSYNC`/`fsync(2)` を各 100 回実測。 |

wire 実測は crossdb `results/self_exact.json` を ±5% 以内で再現した（hybrid_rrf 4,788 vs 4,800・vector_knn_where 1,024 vs 1,007・where_compound_count 925 vs 903・group_by_having 1,487 vs 1,445・scan 432 vs 427・agg_count 127 vs 127 µs）。

## 2. 結果サマリ表

「高速経路」＝各 Issue が想定した索引・キャッシュ経路。カウンタは in-process 50 回中の増分（`+50`＝毎クエリ発火）。p50/p95 は wire（n=50）、括弧内は in-process hot p50（n=50）。

| フェーズ（SQL 出典 `self_db.py`） | 高速経路が使われているか | 実行時カウンタ（50 回） | 阻害要因・残るコスト（file:line） | wire p50 / p95 µs（in-process p50） |
| --- | --- | --- | --- | --- |
| hybrid_rrf（L542-543。密 400 / 疎 400 取得・pool 200・k=10） | **部分的 Yes**: `SparseIndexCache` ヒット（Yes）・`SqlArenaCache` ヒット（Yes）。**アリーナ高速経路は No（構造的に対象外）** | `sparse_hits=+50 arena_hits=+50` | `exec.rs:785` `cache_fast_path_eligible = filters_empty && !is_hybrid` により hybrid は毎クエリ可視 23,000 行を `VectorArena::build_from_cached_rls_rows`（`arena.rs:888`、行ループ `arena.rs:910-915`→`push_row` `arena.rs:351-352` で 23,000×128 f32＝11.8 MB を複製）＋`on_visible_row`（`exec.rs:644` `scan_scalar_columns` 全行）＋`exec.rs:1212` `visible_id_counts` HashMap O(N) を再実行。in-process 4,561〜4,868 µs のうち `hybrid_search` 直接呼び出しは **1,053〜1,088 µs**（疎 597〜627・密 331〜347）で、**約 3.5 ms（≈75%）が SQL 表層固定コスト**（#465/#660 の帰属と同方向、本 Mac では比率がより大きい） | 4,788 / 5,532（4,561〔run1〕・4,868〔run2〕） |
| bulk_hybrid_k200（L623-624。`SELECT id, body` k=200） | 同上 ＋ **投影遅延 No** | `sparse_hits=+50 arena_hits=+50` | 上記に加え `exec.rs:593-596` `defer_projection = … && !is_hybrid` のため `body` 列を Top-k ではなく**可視 23,000 行すべて**で `String` 複製（`exec.rs:728` 以降 `needed_column_indices` 分の複製）。wire 実測: `hybrid_rrf` id のみ 4,788 → id+body 6,386（**+1.6 ms**）、`bulk k=200` id+body 6,432 → id のみ 4,870（**−1.56 ms**）。k=10→200 の差は約 80 µs に過ぎず、**劣後幅のほぼ全てが body 全行複製** | 6,432 / 7,448（6,047〜6,109） |
| vector_knn_where（L485-486。`lang='ja'` 33%） | **Yes**: `IndexEquality` → 候補 id マスク経路（#654） | `scalar_hits=+50 index_scans=+50 index_mask_scans=+50 arena_hits=+50` | 索引は候補を 7,621 件に絞るが、`arena.rs:1049` `filter_cached_rls_rows_subset` が候補ごとに `on_visible_row`（`arena.rs:1085`）＝`scan_scalar_columns`（`exec.rs:644`）＋`matches_all`（`exec.rs:651`）を再適用。参照値: 同じ候補集合の `COUNT(*) WHERE lang='ja'`（masked decode＋matches_all のみ）が **709 µs ≈ 93 ns/候補**、`WHERE topic='gpu'`（1,888 件）の kNN は 179 µs。つまり **1,014〜1,114 µs のうち約 0.7 ms 以上が候補再検証、密探索（7,621 行）は 0.1〜0.2 ms**（推定。内訳は production 無変更では直接計測不可）。注意: 代理値の `COUNT(*)` は `scan_scalar_columns_masked`（`lang` のみ）だが、マスク経路の `on_visible_row` は **非マスクの `scan_scalar_columns`**（全列・body 含む構造検証）を呼ぶため **93 ns/候補は下限**。規模比: `topic='gpu'`（1,888 件）179 µs ≈ 95 ns/候補、`lang='ja'`（7,621 件）1,014 µs ≈ 133 ns/候補（固定費込み）で候補数にほぼ比例 | 1,024 / 1,110（1,015〔run1〕・978〜1,114〔run2〕） |
| where_compound_count（L499。`visible() AND id>100 AND lang='ja'` → 7,599） | **Yes**: `IndexConjunction`（`visible()` は filter に入らず `scalar_plan.rs:26-27`）→ `try_scalar_index_aggregate`（`aggregate.rs:1016`） | `aggregate_index_scans=+50`（fallback 0） | `aggregate.rs:1207` `observe_candidate_slots` が 7,599 候補全件に `scan_scalar_columns_masked`＋`matches_all`＋式評価（id>100）を再適用（多層防御）。実測 **801〜838 µs ≈ 105〜110 ns/候補**（`lang='ja'` 単独 709 µs との差 ≈ 100 µs が id 範囲交差＋式評価分）。参考: 同種述語の plain scan 縮退（`lang='en'` 66.9% > 1/2 で `FallbackSelectivity`）は 2,972 µs（同一述語を強制 plain scan する手段は production 無変更では無いため厳密な索引効果比ではない）。一方 `agg_count` の `VisibleBitmapCache` は 23,000 id を 48 µs（≈2 ns/id）で処理しており、索引経路の候補再検証が「agg_count 級」に届かない主因 | 925 / 957（801〜838） |
| group_by_having（L528-529。`GROUP BY lang HAVING n>1 ORDER BY n DESC LIMIT 5`） | **Yes: 列挙形に到達している**（`group_by.rs:800` `where_less=true`・COUNT(*) のみで `has_text_min_max_aggregate` 偽〔`:829`〕→ `observe_group_enumeration` `:851`） | `aggregate_index_scans=+50`（fallback 0。`HAVING` なし・`GROUP BY topic`（12 群）も同値） | 列挙形は redb 走査・RLS 判定・`GROUP BY` キー照合は省くが、**グループ内の全スロットを `observe_group_slots`（`group_by.rs:370-425`）で走査し、各スロットで `scan_scalar_columns_masked`（`:392-396`）を呼ぶ**。`GROUP BY` キー列は常にマスクへ入る（`group_by.rs:757-765` `extra_scalar_index`）ため列挙済みの値を再デコードする。`DecodeTier` も `Fast` を選ばない（`:760-773`）。実測 1,384〜1,423 µs ≈ **60 ns/スロット × 23,000**（`GROUP BY topic` 1,401 µs・`HAVING` なし 1,387 µs と不変＝グループ数非依存の O(N)）。**阻害要因は WHERE/HAVING/body 閾値ではなく列挙形の設計自体**（body は閾値 64 B で除外済み〔`scalar_index.rs:107`〕、`lang` は索引対象） | 1,487 / 1,508（1,384〜1,423） |
| scan_where_nosort_k500（L655。`SELECT id, body WHERE lang='ja' LIMIT 500`） | **該当なし**（設計どおり索引不使用。`scan.rs` に `ScalarIndex` 参照なし、`scan.rs:296-347` 早期終了付き redb 直接走査） | （カウンタ変化なし） | engine 側は 210〜227 µs（id のみ／id+body でほぼ同じ→デコード側の body コストは小）。wire 側で **id+body 432 vs id のみ 359（+73 µs）**、行数別 k=1: 54・k=50: 106・k=500: 432 µs → **wire＋psycopg 分 ≈ 205 µs（47%）**。§3 参照 | 432 / 459（227） |
| ingest_single_stmt（L775-778。1 文 1 commit × 1,000） | **該当なし**（durability 契約どおり毎 commit で `F_FULLFSYNC`） | — | §5。`F_FULLFSYNC` p50 4.2 ms ≈ 181 rows/s（5.5 ms/行）の大半 | 181 rows/s（crossdb 値） |

補助実測（in-process hot p50）: `vector_knn` 448〜458 µs（wire 535）、`agg_count` 48 µs（wire 127）、`SELECT COUNT(*) WHERE lang='ja' AND topic='gpu'`（611 件）72 µs、`WHERE id > 100` 単独 → `FallbackSelectivity`（22,900/23,000 > 1/2、`scalar_index.rs:974-989`）で plain scan 3,297 µs。

## 3. wire 床と scan の wire 分

| 文 | wire p50 / p95 / min µs | 解釈 |
| --- | --- | --- |
| `SELECT id FROM docs LIMIT 1`（in-process 2.5 µs。RowDescription+DataRow+CommandComplete+ReadyForQuery） | 62.7 / 79.5 / 51.7 | **行返却文の wire 床 ≈ 60 µs（ループバック＋pg wire 往復＋psycopg）** |
| `SELECT id, body FROM docs LIMIT 1` | 60.3 / 62.6 / 53.0 | body 1 行では差なし |
| `SET search_mode = 'recall'`（テーブル非参照・CommandComplete のみ） | 60.2 / 65.3 / 55.8 | 非行返却文の下限（参考） |
| `SELECT id FROM docs LIMIT 500`（WHERE なし） | 189.8 / 199.9 | 500 行応答の下限 |
| `SELECT id, body FROM docs LIMIT 500`（WHERE なし） | 264.1 / 281.9 | 500 行×body 転送＋psycopg 文字列化 ≈ +74 µs |
| `WHERE lang='ja' LIMIT 1 / 50 / 500`（id+body） | 54.0 / 106.0 / 432.4 | ≈ 0.76 µs/行（engine 走査＋wire＋クライアント込み） |
| `WHERE lang='ja' LIMIT 500`（id のみ） | 358.9 / 372.6 | body なしで −73 µs |

決定的な集計フェーズでの wire−in-process 差: agg_count +79・vector_knn +78・group_by_having +103・where_compound_count +124 µs。hybrid/vector_knn_where は in-process 側の run 間ばらつき（±100 µs）が wire 差と同程度で分離不能。**scan_where_nosort_k500 427 µs のうち wire＋クライアント分 ≈ 205 µs（in-process 227 µs との差。id のみ 149 µs）**——他フェーズより大きいのは 500 行 × (id+126 B body) の DataRow 送出（`simple_query.rs:322-329` `ResponseBuffer` 1 回 `write_all`）と psycopg 側の行パースが行数比例で乗るため。sqlite-vec（203 µs・同一プロセス内ライブラリ）との差の**約半分は wire 経路そのもの**であり、engine 走査（227 µs）だけで既に同水準。

## 4. hybrid の再取得ループ発火回数（本 fixture・200 クエリ／crossdb が使う先頭 55 クエリ）

`RrfConfig::new(60.0, 1.0, 1.0, pool_depth)`・`pool_depth = max(k, 200)`（`exec.rs:57`・`:1458`）、初回 `fetch_k = pool_depth×2 = 400`（`hybrid.rs:1560-1565`〔密〕・`:1711-1716`〔疎〕）。可視 23,000 文書を jsonl の id 昇順スロット 0..N で再構成。SQL 表層の実スロット順は redb 物理順（`SqlArenaSnapshot`）であり、本 harness では未検証（推定・§7）。ラウンド数はスコア境界のみで決まり DocId 付番には依存しない。

| 側 | k=10（hybrid_rrf） | k=200（bulk_hybrid_k200） |
| --- | --- | --- |
| 疎側 `sparse_refetch_loop` ラウンド数 | **全 200 クエリで 1 ラウンド**（fetch_k=400、BM25 正スコア件数 200〜288 < 400 で exhaustive 確定） | 同（pool_depth 同じため同一） |
| 密側ラウンド数（`search()` 呼び出し回数） | 1 回: 159 / 2 回: 38 / 3 回: 2 / 5 回: 1（先頭 55: 47 / 7 / 0 / 1） | 同一分布 |
| 密側最終 fetch_k | 400: 159・800: 38・1,600: 2・6,400: 1 | 同 |
| 時間（55 クエリ p50） | 疎ループ 597〜618 µs・密 1 回（k=400）322〜331 µs・`hybrid_search` 合計 1,053〜1,082 µs | 疎 627〜640・密 322〜347・合計 1,079〜1,088 µs |

所見: 境界同点グループ完全化（`complete_boundary_tie_group`）は**密側で約 20% のクエリに発火**（fixture の embedding が離散値 {0, ±0.2, ±0.4…} で dot が同点になりやすいため）し、1 件は 6,400 件まで再取得している。ただし密探索 1 回が 0.33 ms のため p50 への寄与は小さく、hybrid 全体の内訳は **SQL 表層 3.5 ms ≫ 疎 0.6 ms ＞ 密 0.33 ms** で、勝敗差（Redis 1.3 ms）を説明するのは再取得ではなく §2 のアリーナ再構築＋（bulk では）body 全行複製である。

## 5. fsync 機構（file:line）

| 層 | 参照 | 内容 |
| --- | --- | --- |
| std（Apple） | `~/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/std/src/sys/fs/unix.rs:1413-1424`（`fsync`）・`:1427-1434`（`datasync`） | `#[cfg(target_vendor = "apple")]` で **`sync_all`・`sync_data` とも `libc::fcntl(fd, libc::F_FULLFSYNC)`**。std 内に `F_BARRIERFSYNC` は出現しない（`grep -rn F_BARRIERFSYNC library/std/src` 0 件） |
| redb 4.2.0 commit | `transactions.rs:955-956`（既定 `two_phase_commit: false`・`quick_repair: false`）→ `:1707` `durable_commit`（既定 `Durability::Immediate` `:954`）→ `:1986` `mem.commit(.., two_phase=false, ..)` → `page_manager.rs:1033` `commit`: `:1061` `write_header`（`:865-869` キャッシュ書き込み）→ `:1074` `write_header` → **`:1075` `self.storage.flush()`**（two_phase 時のみ `:1065` でもう 1 回） | `flush` は `cached_file.rs:497-500`（`flush_write_buffer` `:414` で書き戻し → `self.file.sync_data()`）→ `file_backend/optimized.rs:96-97` `File::sync_data` → 上記 std → **commit 1 回につき `F_FULLFSYNC` 1 回**。engine は `storage.rs:520` `redb::Database::create(path)` で builder 未使用、`set_durability`/`set_two_phase_commit` の呼び出しなし（`grep` 0 件。two-phase は redb 内部の compaction/repair 経路 `db.rs:869` のみ） |
| 実測（同ボリューム `/System/Volumes/Data`・4 KiB 書き込み後・n=100） | `fsync(2)` p50 18 µs / `F_BARRIERFSYNC` p50 350 µs（min 160・p95 771）/ **`F_FULLFSYNC` p50 4,177 µs（min 3,574・p95 5,164）** | crossdb の 181 rows/s = 5.5 ms/文 は F_FULLFSYNC 1 回（4.2 ms）＋engine/wire（`ingest-stage-profile.md` §484: commit 以外の段は数十 µs）と整合。Redis 3,566 rows/s との差は **fsync 契約の差**であり engine 経路の問題ではない |

## 6. 結論（負けフェーズ別の一言）

1. **hybrid_rrf / bulk_hybrid_k200**: 疎索引・アリーナのキャッシュはヒットしているが、hybrid は `cache_fast_path_eligible`（`exec.rs:785`）と `defer_projection`（`exec.rs:593-596`）の両方から構造的に除外されており、毎クエリ 23,000 行のアリーナ再構築（≈3.5 ms）と bulk では body 全行複製（≈1.6 ms）を払っている。再取得ループは疎 1 ラウンド・密 80% が 1 ラウンドで主因ではない。#660 の P1/P2 候補がそのまま該当。
2. **vector_knn_where / where_compound_count**: 索引経路（マスク経路・`try_scalar_index_aggregate`）に毎回到達しているが、候補 7.6k 件への `scan_scalar_columns(_masked)`＋`matches_all` 再検証が ≈93〜110 ns/候補で支配的（0.7〜0.8 ms）。索引は「絞る」だけで通さない設計（`exec.rs:903-916` コメント）の代償。
3. **group_by_having**: 列挙形には乗っている。agg_count 級（48 µs）に届かないのは列挙形が全スロットの metadata を再デコードする実装（`group_by.rs:370-425`）のためで、WHERE/HAVING/閾値は無関係。
4. **scan_where_nosort_k500**: engine 227 µs は sqlite-vec 203 µs と同水準。差はほぼ wire＋psycopg（≈205 µs、床 56〜60 µs＋行数比例分）。
5. **ingest_single_stmt**: 1 commit＝1 `F_FULLFSYNC`（4.2 ms）で説明できる。

## 7. 限界・注記

- in-process 値は run 間で ±100 µs 程度振れる（hybrid_rrf 4,561/4,868、vector_knn_where 978〜1,114）。wire 分の切り分けは決定的フェーズ（agg/集計）の差 78〜124 µs と床 56〜63 µs を根拠とする。
- vector_knn_where の内訳（候補再検証 vs 密探索）は production 無変更では直接計測できないため、同一候補集合の `COUNT(*)` を代理とした推定である。
- §4 の DocId は jsonl の id 順スロット。SQL 表層の実スロット順（redb 物理順）と一致するかは未検証の推定。同点タイブレーク順のみが影響を受け、ラウンド数（スコア境界で決まる）には影響しない。
- リポの working tree には本セッション開始前から `scripts/crossdb_bench/*` の変更・未追跡ファイルがあり、本作業とは無関係（本作業はリポ内を一切変更していない）。
- 生ログ: `work/wire_measure.log`・`work/wire_results.json`・`work/harness_run1.log`・`work/harness_run2.log`。ハーネス: `harness/src/main.rs`（path 依存・`bench-internals`・リポ外ビルド `harness/target`）。
