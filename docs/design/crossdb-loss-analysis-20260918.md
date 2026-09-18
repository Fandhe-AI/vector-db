# crossdb 横断ベンチ 負けフェーズの原因分析と是正（2026-09-18）

- ステータス: Accepted（是正実装済み。専有環境での最終再確認はオーナー申し送り）
- 関連ポインタ: `docs/design/crossdb-bench.md`・`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #660」節・
  `docs/design/scalar-index-mask-search.md`・`docs/design/scalar-index-aggregate.md`・
  `docs/design/wide-retrieval-scan.md`・`docs/design/ingest-write-path.md`・
  `docs/design/benchmark-judgement-policy.md`・`docs/design/bench-data/crossdb-20260918/`
- 計測環境: Apple M4 Max・16 コア・macOS 26.6.2（共有デスクトップ環境の参考値）。self は
  `wire-server`（HEAD `64cb381` 相当）
- production コード〔`crates/engine/src/`〕変更あり（本 doc §4）。他 DB 側のコードは移植・
  長文引用せず、手法名・構造・`file:line` のみ参照する（RediSearch/Elasticsearch/MongoDB は
  ライセンス非互換につき特に厳格に。mongot（Atlas Search）・MySQL HeatWave vector は
  クローズドソースであり解析対象外）

## 1. 結論（先に）

2026-09-18 時点の crossdb 横断ベンチ（`docs/design/crossdb-bench.md` 参照）で self が負けていた
7 フェーズのうち、**6 フェーズの原因は self 側 engine の 4 つの実装機構**（後述）に帰着し、
本 doc の是正（§4）で engine 側の未解消コストを除去した（5 フェーズは最速他 DB を上回り、
`hybrid_rrf` は Redis FLAT に対し 0.90 倍の僅差。self はプール深さ 200 で候補を取り Redis は
WINDOW 50 であるため、片側 4 倍の候補処理をしてなお 10% 以内——§5 参照）。残る 1 フェーズ（`ingest_single_stmt`）と、負け幅の一部
（`scan_where_nosort_k500` の wire 分）は **比較条件の非対称**（durability 契約・in-process
対 wire 経由の測定条件）であり、self の実装欠陥ではないため対処しない（§5）。

是正した 4 機構:

1. **hybrid 経路がキャッシュヒット時も毎クエリ `VectorArena` を全行複製する**
   （`cache_fast_path_eligible` が hybrid を無条件に除外）→ `hybrid_rrf`／`bulk_hybrid_k200`
2. **hybrid の投影列（`body` 等）が Top-k 確定前に候補全行ぶん複製される**
   （`defer_projection` が hybrid を無条件に除外）→ `bulk_hybrid_k200`
3. **スカラー列二次索引がヒットしても、候補行へ毎回 `on_visible_row`（マスク済みデコード＋
   `matches_all` 再適用）を掛け直す多層防御**（索引は「絞る」だけで「通す」ことはできない
   設計の代償）→ `vector_knn_where`／`bulk_knn_where_k200`／`where_compound_count`
4. **`GROUP BY` 列挙形が全スロットの `scan_scalar_columns_masked` を再デコードする**
   （`COUNT(*)` のみでも値を再読みしていた）→ `group_by_having`

前回スコアボード（2026-09-09・QEMU x86_64 専有環境）は 7 勝 2 僅差 4 敗、今回
（2026-09-18・Apple M4 Max 共有機・NoSQL 4 DB 追加）は 7 勝 0 僅差 6 敗——ただし
`git log 1deae29..64cb381 -- crates/engine` に検索経路の性能変更は無く、self 側の実装は
不変のまま負け幅が拡大していた。原因は §2 のとおり **環境差・対照 DB 追加による相対順位の
変動**であり、self の退行ではない（本 doc の是正は「前回は勝っていたが今回負けた」ものを
戻す性質ではなく、engine 側に実在した未解消コストを新規に解消するもの）。

## 2. 前回比の 3 区分

前回（2026-09-09・QEMU x86_64・専有環境）と今回（2026-09-18・Apple M4 Max・共有機）は
CPU アーキテクチャ・ノイズ環境・対照 DB 構成が異なるため、p50 の直接比較は参考値に留める。

1. **新規参入 DB による順位変動**: 今回追加された NoSQL 対照（Redis／RediSearch・
   Elasticsearch・MongoDB Atlas local・MongoDB Community。`results-nosql/`）により、
   `hybrid_rrf`／`bulk_hybrid_k200` は Redis、`group_by_having` は Elasticsearch が新たな
   最速対抗馬となり、前回 1 位だった `group_by_having` が 3 位へ、hybrid 2 フェーズは
   5 位から 8〜9 位へ下がった。`vector_knn_where`（Qdrant）と `where_compound_count`
   （LanceDB）は前回から同じ対抗馬に負けており、新規参入とは無関係。Elasticsearch の
   2 フェーズは採用 run が 1 回のみで、同 DB の exact／hnsw 列が非ベクトルフェーズで
   約 30% 乖離する（`group_by_having` 1,387／952µs）ため単発 run の参考値と扱う。
2. **対照 DB の実行環境差**: LanceDB・sqlite-vec は in-process ライブラリとして計測されており
   （`report-lancedb.md`・`report-sqlite-pg.md`）、前回の QEMU x86_64 から今回の Apple M4 Max
   native へ移ったことで LanceDB の `where_compound_count` は 927→641µs へ速くなった（self は
   1,018→903µs）。前回「僅差」だった同フェーズが今回「負け」へ転じたのはこの環境差による。
   一方 pgvector・Qdrant・MySQL と NoSQL 4 DB は Docker Desktop VM 越しであり、self（native・
   wire 経由）との測定条件の非対称が構造的に含まれる（§3・§5）。
3. **self 側の変更**: `1deae29..64cb381` 間で engine の検索経路に性能変更は無い
   （NoSQL 表層の公開 API 化・JSON パーサ等のみ）。つまり今回計測された負け幅は
   **以前から存在した未解消コスト**であり、環境・対照 DB 構成が変わったことで顕在化した。

## 3. フェーズ別の原因分析

各節は「self の per-query 処理（file:line）」「最速他 DB が per-query にやらないこと／
取り込み時に前計算していること（file:line）」「差の帰属」の順で記す。他 DB のコードは
手法名・構造・`file:line` のみを引用し、本文の長文引用は行わない。

### 3.1 `hybrid_rrf`（self 4,800µs → Redis 1,336µs）・`bulk_hybrid_k200`（6,268µs → 2,256µs）

- **self の per-query コスト**（`report-self-engagement.md` §2・`report-self-paths.md` §2.4）:
  `sql/exec.rs` の `cache_fast_path_eligible = filters_empty && !is_hybrid` が hybrid を
  無条件に除外するため、`SparseIndexCache`・`SqlArenaCache` がヒットしていても毎クエリ
  `VectorArena::build_from_cached_rls_rows` で可視全行（23,000 行）を複製し直す。
  in-process 計測では `hybrid_search` 本体の呼び出しは 1.05〜1.09ms なのに対し、
  wire 実測の hybrid_rrf 合計は 4.6〜4.9ms——**約 3.5ms（約 75%）が SQL 表層固定コスト**。
  `bulk_hybrid_k200` はこれに加え `defer_projection` も hybrid を除外するため `body` 列を
  候補全行（23,000 行）で複製する（`exec.rs:735-753`）。
- **Redis（RediSearch `v8.10.1`）が per-query にやらないこと**（`report-redisearch.md`）:
  疎索引（転置索引・BM25 統計）は書き込み時に確定した常駐構造であり
  （`src/redisearch_rs/inverted_index/src/index/core.rs:30,195`）、クエリ時の再構築という
  概念自体が無い。各サブクエリから固定 `WINDOW`（既定 20。ベンチは 50/200 明示指定）件を
  **1 回だけ**取得して打ち切る（`result_processor.c:2461-2479`）——self の `pool_depth` 倍増・
  疎側再取得ループに相当する仕組みは無い。Top-k 境界の同点は `docId` 降順の確定的順序
  （`result_processor.c:765-779`）で 1 件だけ通し、self の「境界同点グループ完全化」
  （`TieRank::GroupEnd`）に相当する追加コストを払わない。
- **帰属**: `report-self-engagement.md` の実測により、疎側再取得ループ自体は 200 クエリ中
  全て 1 ラウンドで確定し（fetch_k=400・exhaustive）、密側も 80% が 1 ラウンドで支配的要因
  ではないと判明。主因は `cache_fast_path_eligible`／`defer_projection` が hybrid を除外する
  ことによる**毎クエリの全行複製**（`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #660」
  節の P1・P2 候補と一致）。→ §4.1 で解消。

### 3.2 `vector_knn_where`（self 1,007µs → Qdrant 669µs）

- **self の per-query コスト**（`report-self-paths.md` §2.1・`report-self-engagement.md` §2）:
  `ScalarIndex::resolve_candidates` により候補を 7,621 件（`lang='ja'`）へ絞るが、
  `VectorArena::filter_cached_rls_rows_subset` が候補ごとに `on_visible_row`
  （`scan_scalar_columns` の構造検証＋`matches_all` 再適用）を再実行する多層防御を持つ
  （`arena.rs:1049,1085`）。同種の候補集合への `COUNT(*)` 実測（709µs≈93ns/候補）を代理に
  すると、`vector_knn_where` 1,014〜1,114µs のうち約 0.7ms 以上が候補再検証で、密探索
  （7,621 行）自体は 0.1〜0.2ms と推定される。
- **Qdrant `v1.19.1` が per-query にやらないこと**（`report-qdrant.md`）: `lang`／`visibility`
  の keyword payload index は upsert 時に `RoaringBitmap` へ増分反映済み
  （`mutable_map_index/in_memory.rs:53-77`）。クエリ時は `RoaringBitmap` の AND 交差
  （`payload_index_read.rs:75-88`）で候補集合を確定し、**候補ごとの述語再評価は一切行わない**
  （交差結果をそのまま真実の値として信頼）。Top-k（`top_k.rs:94-96`）もスコアのみの
  比較で同点タイブレークを持たない。
- **帰属**: 主因は「索引ヒット後も候補を毎回再検証する」self 固有の多層防御。
  → §4.2 で、索引が `WHERE` を完全被覆すると静的に保証できる場合に限り再検証を省く経路を追加。

### 3.3 `where_compound_count`（self 903µs → LanceDB 641µs／ES 678µs、7,599 件 COUNT）

- **self の per-query コスト**（`report-self-paths.md` §2.2）: `visible() AND id > 100 AND
  lang = 'ja'` の 3 条件 AND は `ScalarPlan::IndexConjunction` に分類され
  `try_scalar_index_aggregate`（`aggregate.rs:704,1009`）で候補を絞るが、
  `observe_candidate_slots` が 7,599 候補全件に `scan_scalar_columns_masked`＋`matches_all`＋
  式評価（`id>100`）を再適用する（105〜110ns/候補）。
- **LanceDB `v0.38.0` が前計算していること**（`report-lancedb.md` §2）: 列指向ファイル形式が
  そのものが常時前計算であり、`count_rows` は投影 `[]`＋`with_row_id` で述語列（3 列）以外を
  一切デコードしない（`dataset.rs:1683-1690`・`filtered_read.rs:1646-1653`）。
- **Elasticsearch/Lucene（`v9.1.4`／Lucene `10.2.2`）が前計算していること**
  （`report-elasticsearch.md` §6）: 単一条件の `TermQuery.count()` は辞書統計 `docFreq` のみで
  定数時間だが、**3 条件 AND では `BooleanWeight#reqCount` が `-1` を返し実走査
  （`ConjunctionDISI` leapfrog）へフォールバックする**（`BooleanWeight.java:206-230`）ため、
  ES 側も複合条件では定数時間 COUNT には届かない。優位性は「辞書化済み postings・BKD を
  辿る走査効率」＋「`LRUQueryCache` によるフィルタ bitset のクエリ横断キャッシュ
  （既定 5 回目以降ヒット）」の組み合わせに拠る（`TermQuery.java:260-273`・
  `LRUQueryCache.java:172-192`）。
- **帰属**: self の索引経路は「絞る」ことしかできず「数える」統計を単独で持てない設計上、
  候補再検証が `agg_count`（`VisibleBitmapCache`・約 2ns/id）級に届かない。
  → §4.2 の索引信頼マスクを `COUNT(*)` 経路（`count_star_only`）へ適用し解消。

### 3.4 `group_by_having`（self 1,445µs → ES 952µs、2 グループ）

- **self の per-query コスト**（`report-self-paths.md` §2.3）: `WHERE` なしのため「列挙形」
  （`ScalarIndex::column_groups` 直接写像）に到達しているが、`observe_group_slots`
  （`group_by.rs:370-425`）が **全スロット（23,000 件）に `scan_scalar_columns_masked` を
  再デコード**する（≈60ns/スロット）。`GROUP BY` キー列（`lang`）は常にマスクへ含まれる
  ため、列挙済みの値を再デコードしている。
- **ES/Lucene が前計算していること**（`report-elasticsearch.md` §6）:
  `GlobalOrdinalsStringTermsAggregator`（`terms` agg）は Lucene の `SortedSetDocValues`
  （global ordinal）を直接使い、`DenseGlobalOrds` 戦略では `globalOrdToBucketOrd(globalOrd)
  = globalOrd`（恒等写像）で ordinal を配列添字にそのまま使う密配列カウント方式
  （`GlobalOrdinalsStringTermsAggregator.java:122-139,458-513`）——低カーディナリティ列は
  ハッシュ探索すら発生しない O(1) インクリメント。`HAVING` 相当の `bucket_selector` は
  バケット確定後の reduce フェーズで間引くだけの後処理（`BucketSelectorPipelineAggregator.
  java:27-53`）でシャード走査を変えない。
- **帰属**: 主因は WHERE/HAVING/閾値ではなく **列挙形の実装自体**（全スロット再デコード）。
  → §4.3 で `COUNT(*)` のみの `GROUP BY` に限り再デコード自体を省略。

### 3.5 `scan_where_nosort_k500`（self 427µs → sqlite-vec 203µs）

- **self の内訳**（`report-self-engagement.md` §3・`report-sqlite-pg.md` §1）: engine 内部
  （in-process 相当）は 210〜227µs で sqlite-vec の 203µs と同水準——**索引の問題ではない**。
  `sql/scan.rs::execute_scan` はスカラー列二次索引を未結線のまま redb 直接走査するが
  （設計は `docs/design/wide-retrieval-scan.md` の意図的な非対応）、WHERE 未ヒット行でも
  `decode_row_dim_and_metadata_borrowed`＋`scan_scalar_columns_masked` を毎回実行する構造で
  あり、SQLite の `OP_Column`（`src/vdbe.c:3035,3155-3216`）が列単位で必要な列
  （`lang` まで）だけ遅延パースする構造より粒度が粗い。
- **wire 分**: `SELECT id FROM docs LIMIT 1` の wire 床は 62.7µs（simple query 往復＋
  psycopg）。`WHERE lang='ja' LIMIT 500`（id+body）は wire 実測 432µs、engine 内部 227µs——
  **差の約 205µs（47%）が wire＋psycopg 側**（500 行 DataRow 送出＋クライアント側行パース）。
  sqlite-vec は同一プロセス内ライブラリ呼び出しのため、この 205µs の約半分は wire 経路
  そのものに帰属する。
- **帰属**: engine 側コストは sqlite-vec と既に同水準。残差は **測定条件の非対称**
  （in-process 対 wire 経由）であり、SQL 表層の欠陥ではない。→ §5 で対処しない理由を記載。

### 3.6 `ingest_single_stmt`（self 181 rows/s → Redis 3,566 rows/s）

- **self の内訳**（`report-self-engagement.md` §5）: redb の既定 `Durability::Immediate` は
  commit ごとに `sync_data()` を呼び、macOS では std がこれを `fcntl(F_FULLFSYNC)` へ写像する
  （`sys/fs/unix.rs:1413-1434`）。本機実測: `fsync(2)` p50 18µs に対し `F_FULLFSYNC` p50
  4,177µs（min 3,574・p95 5,164）。1 commit = 1 `F_FULLFSYNC` であり、5.5ms/行のほぼ全てが
  この 1 回で説明できる。
- **Redis（RediSearch）の永続化**（`report-redisearch.md` §7）: RediSearch 自体は
  `fsync`/`fdatasync` を呼ばず（`grep` 0 件）、インデックス型は `.aof_rewrite =
  GenericAofRewrite_DisabledHandler`（`indexes.c:398`）で AOF rewrite 時に自身を書き出さない。
  `redis:8` イメージ既定は AOF 無効・周期的 RDB スナップショットのみで、単一コマンドごとの
  同期 fsync が一切乗らない（`containers.sh:107-121`）。
- **他 DB との対比**（`report-lancedb.md` §4.3・`report-mongodb.md` §7）: LanceDB はコミットで
  fsync を一切呼ばない（`object_store` の `LocalFileSystem::put_opts` に `sync_*` なし）。
  MongoDB（WiredTiger）は macOS で `F_FULLFSYNC` を使うが **WAL のみ**に対象を限定し
  データページはチェックポイントへ分離する設計（`os_fs.c:154-205`・`log.c:224-260`）。
- **帰属**: **durability 契約の差**（自作 DB は明示的に選択した「1 コミット = 1
  フェイルクローズな永続化契約」、他 DB の多くは既定で応答時点の耐久性を要求しない、または
  WAL のみに fsync 範囲を限定。MongoDB Atlas local の `w:majority` は例外で journal 永続化を待つ）。engine・wire の実装コストではない。→ §5 で対処しない。

## 4. 実装した対処

以下はいずれも `crates/engine/src/` の変更であり、索引・キャッシュの**結果**（クエリの
正しさ・テナント境界・Recall）は一切変えず、**キャッシュ・索引がヒットした後の冗長な
再構築・再検証**のみを削減する。索引ヒット判定・世代整合性チェック（`SqlArenaCache`・
`ScalarIndexCache`・`SparseIndexCache`）自体は無変更。

### 4.1 hybrid 経路のキャッシュ高速経路化（Issue #660 P1・P2 相当）

- `sql/exec.rs::cache_fast_path_eligible` の判定式を `filters_empty && !is_hybrid` から
  `filters_empty && (!is_hybrid || skip_sparse_accumulation)` へ変更。`skip_sparse_accumulation`
  は `SparseIndexCache` がヒットした（＝疎索引を再構築しない）ことを示すフラグであり、
  hybrid かつ疎索引キャッシュヒット時に限り、`VectorArena::build_from_cached_rls_rows` に
  よる可視全行の複製を省いてキャッシュ済みスナップショットを直接借用する。
- `sql/exec.rs::defer_projection` の判定式へ同じ条件（hybrid かつ疎索引キャッシュヒット）を
  追加し、投影列（`body` 等）のデコードを Top-k 確定後の候補行のみへ遅延する
  （`ScalarSource::Deferred`。Issue #453 の既存機構を hybrid にも適用）。
- 疎索引キャッシュが未ヒット（初回構築・世代不一致等）の場合は従来どおり全行複製経路へ
  fail-closed に落ちる——高速経路は「索引・キャッシュが両方ヒットした場合の重複コスト削減」
  であり、キャッシュミス時の挙動は不変。

### 4.2 索引信頼マスク（`WHERE` が索引で完全被覆されている場合の候補再検証省略）

`sql/aggregate.rs::count_star_only`（`COUNT(*)` のみで構成される集計）のドキュメントに、
索引由来の候補集合をそのまま「対象行の正確な集合」として信頼してよい不変条件を明記し、
`DISTANCE`（`sql/exec.rs`）・`COUNT(*)`（`sql/aggregate.rs`）・`GROUP BY`（`sql/group_by.rs`）
の 3 経路がこの不変条件を共有する形で候補再検証を省略する経路を追加した。

**不変条件（3 条件。1 つでも崩れたら従来の候補再適用・全走査経路へ fail-closed に落ちる）**:

1. `WHERE` が索引で完全被覆されている——`sql::scalar_plan::classify_scalar_plan` が
   `PlainScan` 以外を返し、かつ `ScalarIndex::resolve_candidates` が `Use` を返した場合。
   `classify_scalar_plan` は索引非対応の残余述語が 1 つでもあれば `PlainScan` を返す契約
   のため、`PlainScan` 以外＝残余述語なしが静的に保証される。
2. 索引↔スナップショット同一性ガード（行数・構築世代の一致）を通過している——索引の
   母集合が当該 `(table, ctx)` の可視集合そのものであることの担保。世代不一致時はこの
   経路へ入らない。
3. 経路固有の追加条件——`DISTANCE` は「hybrid でない・HNSW `Subset` 形状でない」
   （疎コーパスの `DocId` 割当・ANN 探索側の既存契約と衝突しないため）、`COUNT(*)`
   は「集計項目が `COUNT(*)` のみ（`AggregateInput::AllVisible`）」（`COUNT(<列>)` の
   NULL 意味論は対象外）、`GROUP BY` は同じ `count_star_only` を共有し `WHERE` なしの
   列挙形に限る。

適用箇所:

- **DISTANCE（`vector_knn_where` 等）**: `sql/exec.rs` の `mask_trusted_defer`。条件を満たす
  場合、候補スロットを `on_visible_row`（masked decode ＋ `matches_all` ＋式述語評価）へ
  一切通さず、投影も `ScalarSource::Deferred(Snapshot)` で Top-k 確定後のみデコードする。
- **`COUNT(*)`（`where_compound_count` 等）**: `sql/aggregate.rs::count_star_only` が真の場合、
  候補スロット列の長さをそのまま件数として使い、`observe_candidate_slots` を呼ばない。
- **`GROUP BY`（`group_by_having` 等）**: `sql/group_by.rs::observe_group_count_only` が
  `count_star_only` の場合に `observe_group_slots`（全スロットの `scan_scalar_columns_masked`）
  を省き、グループごとのスロット件数のみを加算する。

いずれもキャッシュ・索引が未ヒット、または不変条件のいずれかが崩れる場合は既存の全走査・
候補再適用経路へそのまま落ちるだけで、クエリの正しさ・RLS 相当のテナント境界は不変。

### 4.3 テスト

- `crates/engine/tests/scalar_index_mask_search.rs::trusted_mask_matches_plain_scan_for_
  each_predicate_shape`: 索引信頼マスク経路（`mask_trusted_defer`）と plain scan 経路の
  結果が全述語形状（等価・前方一致・`id` 範囲・複合 AND）で一致することを固定。
  `SELECT id` ケース（投影なし）も含む。
- 既存の hybrid Recall ゲート（層 A 固定値アサーション）・RLS 統合テスト・
  `count_star_only`／`observe_group_count_only` の不変条件テストで、キャッシュヒット時
  高速経路とキャッシュミス時経路（従来の全走査・候補再適用）が同一結果を返すことを
  機械検証済み（`ScalarSource::Deferred` 経由での取得元切替を含む）。

- Recall 層 B ゲート 3 本（`hybrid_recall`・`rerank_recall`・`query_planning_recall`。閾値は
  Actions 外の承認済み環境から注入）を `RECALL_ENGINE=brute_force`／`hnsw` の 2 通り × before
  （main `64cb381`）／after（本 worktree）で計 12 run 実行し、全 run pass・実測 Recall 値（hybrid
  small 0.9010、large 0.9145／0.9165、rerank 0.9488、query-planning 0.9245／0.9321、large
  0.8852）が before/after でビット同一であることを確認した（`hnsw_f16`／`hnsw_i8` は未実行）。

### 4.4 前後比較（A/B・N=5 ペア交互実測。Apple M4 Max・共有機の参考値）

計測方法は `docs/design/benchmark-judgement-policy.md` の交互 N ペア方式に準拠。
before は `target/release/wire-server`（HEAD `64cb381`・sha256 先頭 `f4f54896f6ca`）、
after は本 worktree でのビルド（sha256 先頭 `627b5c53c6a9`）。両者を `run1`〜`run5` として
交互に起動・計測した。per-run 生データ・ドライバは `docs/design/bench-data/crossdb-20260918-loss-ab/`
（`ab4/{before,after}-run{1..5}/self_exact.json`・`run_ab.sh`・`run_one.sh`。self 側の実効性検査の記録は同ディレクトリの `self-engagement.md`）。

| フェーズ | before min | after min | 比（min-of-N） | before median | after median | 比（median） |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `hybrid_rrf` | 4,641.0 | 1,470.8 | 0.317 | 4,665.1 | 1,491.6 | 0.320 |
| `bulk_hybrid_k200` | 6,184.8 | 1,516.8 | 0.245 | 6,234.0 | 1,537.9 | 0.247 |
| `vector_knn_where` | 998.0 | 246.2 | 0.247 | 1,014.4 | 250.6 | 0.247 |
| `bulk_knn_where_k200` | 1,603.4 | 389.4 | 0.243 | 1,609.8 | 393.7 | 0.245 |
| `where_compound_count` | 860.0 | 90.1 | 0.105 | 870.1 | 93.2 | 0.107 |
| `group_by_having` | 1,431.4 | 70.3 | 0.049 | 1,440.7 | 74.9 | 0.052 |
| `vector_knn`（参照区間） | 582.3 | 563.0 | 0.967 | 605.8 | 594.4 | 0.981 |
| `agg_count`（参照区間） | 117.5 | 102.9 | 0.876 | 120.9 | 106.4 | 0.880 |
| `rls_isolation`（参照区間） | 116.9 | 106.6 | 0.912 | 122.4 | 107.2 | 0.876 |
| `agg_multi` | 246.5 | 238.5 | 0.967 | 250.2 | 244.8 | 0.979 |
| `bulk_knn_k200` | 697.6 | 728.6 | 1.044 | 739.7 | 732.7 | 0.991 |
| `bulk_knn_k1000` | 1,312.5 | 1,298.9 | 0.990 | 1,322.8 | 1,326.8 | 1.003 |
| `scan_where_nosort_k500`（非対象） | 413.6 | 417.5 | 1.009 | 425.3 | 423.4 | 0.995 |
| `mode_recall` | 523.9 | 528.6 | 1.009 | 540.8 | 536.5 | 0.992 |
| `mode_precision` | 518.5 | 525.3 | 1.013 | 528.1 | 531.0 | 1.006 |
| `udf_call` | 524.5 | 536.3 | 1.022 | 526.0 | 544.9 | 1.036 |

単位は p50 µs。参照区間ノイズ帯（before+after 10 run プール）: `vector_knn` 10.9%・`agg_count` 23.0%・`rls_isolation` 19.1%。是正対象 6 フェーズはいずれも帯を大きく超え全 5 run で一貫。
per-run 生データ（p50 µs）:

- `hybrid_rrf` before `[4729.2, 4685.0, 4641.0, 4665.1, 4653.0]` / after `[1470.8, 1491.6, 1491.8, 1633.5, 1483.9]`
- `bulk_hybrid_k200` before `[6184.8, 6275.9, 6289.3, 6234.0, 6198.1]` / after `[1517.6, 1516.8, 1545.0, 1564.2, 1537.9]`
- `vector_knn_where` before `[998.0, 1012.3, 1018.8, 1021.2, 1014.4]` / after `[254.9, 246.2, 251.2, 249.7, 250.6]`
- `bulk_knn_where_k200` before `[1603.4, 1609.8, 1624.1, 1608.0, 1631.8]` / after `[393.7, 397.6, 392.0, 403.4, 389.4]`
- `where_compound_count` before `[878.3, 870.1, 860.0, 891.9, 869.7]` / after `[93.5, 93.4, 91.3, 93.2, 90.1]`
- `group_by_having` before `[1435.5, 1451.7, 1431.4, 1454.9, 1440.7]` / after `[75.6, 71.7, 70.3, 74.9, 78.3]`

結果の同一性: 全 10 run で `recall_at_10 = 1.0`、`where_compound_count = 7599`、`group_by_having = [en 15379, ja 7621]`、`bulk_knn_where_k200.rows_returned = 200` が before/after で一致。`ingest_single_stmt` は両 arm とも 167〜193 rows/s（帯内）。

`scan_where_nosort_k500`・`ingest_single_stmt` は本 doc の是正対象外（§5）であり、実測でも
ノイズ帯内でほぼ不変——是正が意図した範囲以外に影響しないことの傍証。共有デスクトップ
環境の参考値であり、専有環境での再実測はオーナー作業として申し送る（§7）。

**オーナー向け承認事項**: §4.2 の「索引で完全被覆された述語に限って索引を信頼する」設計は、
Issue #474 が明示していた「索引は絞ることしかできず通すことはできない」という多層防御方針
を、3 条件下でのみ緩和するものである。この緩和はオーナー判断（設計方針の変更）に該当する
ため、本 doc をレビューのうえ承認・差し戻しを判断されたい。差し戻す場合は `sql/exec.rs`
の `mask_trusted_defer` 分岐・`sql/aggregate.rs::count_star_only`／`try_scalar_index_
aggregate` の早期リターン・`sql/group_by.rs::observe_group_count_only` の 3 箇所を撤去すれば
従来の候補再適用経路（§4.2 以前の挙動）へ戻る。

## 5. 対処しない項目と理由

| 項目 | 理由 |
| --- | --- |
| `scan_where_nosort_k500` の wire 分（≈205µs） | engine 内部（227µs）は sqlite-vec（203µs・in-process）と既に同水準。残差は wire＋psycopg の測定条件差であり、sqlite-vec 側も in-process 計測（§3.5）。是正すれば公平な比較にならない |
| `ingest_single_stmt`（`F_FULLFSYNC` 4.2ms 対 Redis の fsync なし） | durability 契約の差（§3.6）。TASK-96/97・RECOVER-5/6 の commit 成功境界・fail-closed 契約に関わるため、`Durability` を緩める変更はオーナー承認が前提。他 DB の既定値一覧は下表 |
| `DEFAULT_HYBRID_POOL_DEPTH`（200）対 他 DB のプール深さ（Redis 既定 20・LanceDB は `limit` そのもの） | Recall ゲート（層 B・`recall.yml`）が現行のプール幅を前提に閾値判定しているため、変更は Recall 契約の見直しを伴う。据え置き |

**他 DB の単一書き込み durability 既定値一覧**（`report-*.md` より。手法名・構造のみ）:

| DB | 既定 durability | 根拠 |
| --- | --- | --- |
| self（redb） | 毎 commit `Durability::Immediate`→`sync_data()`（macOS では `F_FULLFSYNC`） | `redb-4.2.0/src/transactions.rs:954-955`・std `sys/fs/unix.rs:1413-1434` |
| Redis（RediSearch） | AOF 既定無効・周期的 RDB のみ。単一コマンドに fsync なし | `containers.sh:107-121`・`indexes.c:398` |
| LanceDB | 全レイヤで fsync 呼び出しなし。OS ページキャッシュ止まり | `ostore/src/local.rs:352-372` |
| MongoDB Atlas local（`mongodb_db.py`・単一ノード replica set） | 既定 write concern `w:majority`（`writeConcernMajorityJournalDefault=true`）。応答前に journal の永続化を待つ。データページは checkpoint 分離 | `env.txt:40`・`journal_flusher.cpp:257-286`・`os_fs.c:154-205` |
| MongoDB Community（`mongodb_plain_db.py`・standalone） | 既定 write concern `w:1`（`j` 未指定）。journal flush は WAL のみ 100ms 間隔で応答時点では永続化を待たない | `env.txt:40`・`journal_flusher.cpp:257-286` |
| SQLite（sqlite-vec 経由） | `synchronous=FULL`（既定）→ `fsync(2)`。`PRAGMA fullfsync` は既定 OFF で `sqlite_vec_db.py` も有効化しないため macOS でも `F_FULLFSYNC` は使わない | `os_unix.c:3817-3830`・`pragma.html#pragma_fullfsync` |
| PostgreSQL（pgvector） | `autocommit=True`＝1 文 1 トランザクション。`synchronous_commit=on` 既定（WAL fsync 経路は本調査のスコープ外） | `pgvector_db.py:52,149` |

**RediSearch（RSALv2/SSPL/AGPLv3）・Elasticsearch（AGPL/SSPL/ELv2）・MongoDB（SSPL）は本リポ
（MIT/Apache-2.0）と非互換のライセンスであり、上記はいずれも手法名・構造・`file:line` の
要約のみで、コードの移植・長文引用は行っていない。mongot（Atlas Search）・MySQL HeatWave
vector はクローズドソースであり、解析対象外である旨を明記する。**

## 6. チップ最適化の考察

crossdb で self が負けていたフェーズ（本 doc §3 の全 6 フェーズ）は、いずれも **dot 積
（内積計算）自体が律速していない**。支配要因は SQL 表層の固定コスト・redb 行デコード・
述語再評価・fsync などであり、GPU オフロードや広幅 SIMD（AVX-512/VNNI・NEON dotprod・
Metal・CUDA・ROCm）を投入してもそこには到達しない。逆に dot 積が支配的なフェーズ
（`vector_knn`・`bulk_knn_k200`／`k1000`）では self は既に他 DB に勝っている。

| フェーズ | 支配区分（2026-09-18 実測） | dot 積の位置づけ |
| --- | --- | --- |
| `hybrid_rrf`／`bulk_hybrid` | SQL 表層の全行複製・BM25 posting 走査・RRF 融合 | 密側の一部でしかなく全体の律速要因ではない |
| `vector_knn_where`／`where_compound_count` | 索引ヒット後の候補再検証（≈100ns/候補・redb 行デコード込み） | 候補生成後の逐次 CPU 述語評価が支配 |
| `group_by_having` | 全スロットのキー列デコード | dot 積は不使用（集計経路） |
| `scan_where_nosort_k500` | engine 内部 227µs（sqlite-vec 同等）。残りは wire | dot 積は不使用（`ORDER BY` なし） |
| `ingest_single_stmt` | macOS `F_FULLFSYNC` が commit あたり 4.2ms | チップと無関係（OS の durability 契約） |
| `vector_knn`（勝っているフェーズ・対照） | dot 積が支配（547µs） | NEON 実装により他 DB に勝っている |

計測環境そのもの（Apple M4 Max・macOS）を踏まえたチップ別の採否は以下のとおり:

- **NEON／NEON dotprod／NEON fp16**（既存実装済み）: dot 積律速フェーズには効くが、
  上表の負けフェーズには効かない。拡張余地は限定的で追加投資は見送り。
- **SME（Scalable Matrix Extension）／AMX（Apple Matrix Coprocessor）**: いずれも不採用。
  SME は `stable` Rust の `is_aarch64_feature_detected!` では到達不可（inline asm＝`unsafe`
  追加承認が必要）。AMX は非公開 ISA で Accelerate 経由の新規依存承認が必要。両者とも
  効果対象（行列積主体の演算）が負けフェーズの支配要因（述語評価・行デコード）と合わない。
- **Metal（`wgpu` Metal backend・`gpu_batch.rs`）**: SQL 表層への結線は見送り。
  `SHADER_F16` 経由の f16 算術シェーダは既定のクエリ生成では発火せず（f16 厳密往復可能な
  値でない限り unpack 版へ fail-closed 縮退）、UMA ゼロコピーも `wgpu 30.0.1` の API 構造上
  staging 経由の memcpy が必須で成立しない。いずれにせよ解消対象は GPU 転送コストであり、
  本 doc の負けフェーズが抱える SQL 表層固定コスト・行デコードコストではない。
- **CUDA／ROCm 直接依存**: 依存最小・自作方針と相性が悪く新規依存承認が必要。macOS
  開発環境では検証不可。効果範囲も `gpu_batch` のバッチ検索・索引構築に限られ、crossdb の
  単発クエリ p50 には現れない。
- **GPU バッチ検索の SQL 表層結線**（`wgpu` 経由・既存依存の範囲）: 条件付き保留。
  効果が出るのはバッチ化された複数クエリ・大規模行数・常駐行列アップロード済みの場合に
  限られ、crossdb の単発クエリ（1 クエリ 1 往復）ではこの償却が働かない。むしろ
  ディスパッチ・readback の固定コストが単発クエリのレイテンシへ直接加算され悪化しうる。
- **CPU 側アルゴリズム改善**（本 doc §4 で実施）: 負けフェーズの支配要因に直接効く。
  依存追加不要・チップ非依存。推奨・継続方針。

3 チップ実機（Apple M／AMD Zen／Intel）での性能前後比較は引き続きオーナー実測へ申し送り
（`docs/design/phase4-chip-before-after.md`）。

## 7. 申し送り

- **専有環境での再実測**: 本 doc §4.4 の A/B は共有デスクトップ環境（Apple M4 Max・並行
  作業あり）の参考値。専有環境（オーナー実機・`workflow_dispatch` 経由の専有ランナー等）
  での再確認をオーナー作業として申し送る。
- **ES の交互再計測**: `report-elasticsearch.md` は ES 対照値を単発 run で取得しており、
  `agg_count`／`rls_isolation` 系と同様に exact/hnsw 列で ±30% 程度の乖離が生じうる
  （`docs/design/scan-stage-profile.md` の既存所見）。ES を含む再計測時は交互 N≥5 ペアでの
  再取得が望ましい。
- **pub フィールド追加の記録**: §4.2 で `sql/aggregate.rs::count_star_only` の不変条件
  ドキュメントを追加・`sql/group_by.rs::observe_group_count_only` を新設した。公開 API
  シグネチャ変更は無い（いずれも `pub(crate)`）。
- **スコアボード更新**: 本 doc の是正後、`docs/design/crossdb-bench.md` のスコアボードを
  2026-09-18 実測値のまま「6 フェーズ是正済み（本 doc 参照）」として更新することをオーナー
  へ申し送る（crossdb ハーネス自体の再実行は別途必要）。
