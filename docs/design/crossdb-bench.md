# 他 DB との機能別横断ベンチと GPU（RTX 3060）実機検証

- ステータス: **実施済み**（計測ツール `scripts/crossdb_bench/`・
  `crates/engine/benches/gpu_scaling_bench.rs`。production 変更は
  `crates/wire-server/src/server.rs` の `TCP_NODELAY` 設定のみ）
- 対応: PR #451 後続（hnsw_rs 撤去）・Issue #406 関連（HNSW 構築比較の対照）
- 入口: `make bench-crossdb`（README「他 DB との機能別横断ベンチ」）・
  `make bench-gpu-scaling`・`scripts/crossdb_bench/gpu/README.md`
- 関連: `docs/design/hnsw-parallel-build.md`（自作 HNSW vs usearch）・
  `docs/design/core16-f16-resident-gate.md`（GPU f16 常駐ゲート）・
  `docs/design/hybrid-rrf-latency-breakdown.md`（in-process の段別内訳）

## 目的・範囲

1. 自作 DB（本リポの `wire-server`＋`engine`。以下 self）を **wire 経由の実クライアント
   から**、PostgreSQL（pgvector）・MySQL・SQLite（sqlite-vec）・Qdrant・LanceDB と
   機能別に同一 fixture・同一クエリで比較する。
2. NVIDIA GeForce RTX 3060 で `engine::gpu_batch`（TASK-128〜130）が CPU-SIMD バッチ
   経路に対してどの規模・バッチサイズで高速化するかを実測し、FAISS（GPU）・
   Qdrant（GPU 索引構築）と対照する。

spec の受け入れ基準（閾値）は本ドキュメントでは扱わない。すべて情報提供専用の
実測であり CI には配線しない。

## 計測環境

| 項目 | 値 |
| --- | --- |
| ホスト | dev-box02（Proxmox VM・QEMU 12 vCPU・31 GB。別プロジェクトのコンテナ常駐で loadavg 約 2） |
| GPU | NVIDIA GeForce RTX 3060 12 GB（PCIe パススルー）・driver 595.71.05・CUDA 13.2・nvidia-container-toolkit 1.19.1 |
| self | `wire-server`（release ビルド・`cef02bc` 時点。loopback TCP・簡易クエリプロトコル・psycopg 3.3.5） |
| pgvector | `pgvector/pgvector:pg17`（PostgreSQL 17.11・pgvector 0.8.6。`<#>`。HNSW m=16 / ef_construction=100 / ef_search=64） |
| sqlite-vec | sqlite3 3.46.1・sqlite-vec 0.1.9（in-process・ファイルベース DB・vec0 brute-force・cosine 近似〔内積指標なし〕） |
| Qdrant | `qdrant/qdrant:latest`（gRPC・Distance.DOT・exact / HNSW） |
| LanceDB | lancedb 0.38.0（in-process・`metric="dot"`・exact / IVF_HNSW_FLAT m=16 ef_construction=100 ef=64） |
| MySQL | `mysql:9`（9.7.2 Community。`VECTOR` 型は作れるが `DISTANCE()`／`VECTOR_DISTANCE` が無く〔ERROR 1305〕、`CREATE VECTOR INDEX` は構文エラー〔HeatWave 限定〕のため KNN 系は n/a） |
| FAISS | faiss-gpu-cu12 1.14.1（Docker `bench-faiss-gpu`・python 3.12・`OMP_NUM_THREADS=12`・OpenBLAS 1 スレッド・BLAS 経路無効化） |
| Qdrant GPU | `qdrant/qdrant:gpu-nvidia-latest`（`QDRANT__GPU__INDEXING=1`。構築のみ GPU） |
| fixture | `seed_docs seed` 25,000 行・dim 128（tenant-a 23,000 行 public・tenant-b 2,000 行 private）・200 クエリ・k=10 |
| 反復 | warmup 5・50 反復（`feature_bench.rs` の既定に合わせる）・p50/p95 µs |

計測対象以外のコンテナは停止した。Docker イメージの digest は `scripts/crossdb_bench/README.md`
の手順どおり `docker inspect` で結果 JSON の meta に記録している。

### 公平性の注記

- **接続経路が異なる**: self・pgvector・MySQL は loopback TCP、sqlite-vec・LanceDB は
  in-process、Qdrant は gRPC。in-process の数値は往復コストを含まない。
- **永続化条件**: sqlite-vec は当初 `:memory:`（非永続）で他 DB（永続ストレージ）と
  投入速度の比較条件が揃っていなかったため、`--workdir` 配下のファイルベース DB へ
  変更した（codex-review P2 指摘。PRAGMA は SQLite 既定〔`journal_mode=DELETE`・
  `synchronous=FULL` 相当〕のまま）。本ドキュメントの sqlite-vec の投入系数値
  （`ingest_bulk`・`ingest_single_stmt`）はファイルベース化前（in-memory）の値であり、
  ファイルベース化後の再計測は次回実行時に行う。
- **可視性モデル**: self の wire セッションは現行契約でどのテナントからも
  `visibility = 'public'` の行のみ可視（private は所有テナント自身からも不可視）。
  他 DB は RLS を持たないため `visibility` 列を持たせ毎クエリに
  `WHERE visibility = 'public'` を付け、可視集合を同じ 23,000 行に揃えた
  （テナント列では絞らない。ポリシーエンジンの比較ではない）。
- **exact / ANN**: `exact` は索引なし全件探索、`hnsw` は各 DB の近似索引。
  self は既定エンジン（brute-force）のみ計測した（ANN opt-in は wire から選択できない）。
  Qdrant の `hnsw` は `indexing_threshold` を下げて全セグメントを索引化し、status green
  かつ `indexed_vectors_count` が全件に達したことを確認してから計測する（既定閾値では
  25,000 行・dim 128 は各セグメントが閾値未満のまま HNSW が構築されない）。逆に `exact`
  は `indexing_threshold=0` で自動構築を無効化し、検索側の `exact=True` だけでは止まらない
  バックグラウンド構築の負荷が計測へ混入しないようにする。LanceDB の
  hybrid もベクトル側は `metric="dot"` を明示している。sqlite-vec の hybrid は FTS5 の
  `MATCH` に自然文をそのまま渡すと構文エラーになるため、各語を二重引用符で囲んだ語句
  クエリへ変換している。
- **Recall@10**: 合成 embedding は内積の同点が多いため、主指標 `recall_at_10` は
  同点許容版（`TIE_EPSILON = 1e-4`）。正解集合を境界より厳密に上位の集合
  `strict_above`（スコア > 境界スコア + ε）と境界での同点集合 `tie_boundary`
  （|スコア − 境界スコア| ≤ ε）に分け、分子を
  `|hits ∩ strict_above| + min(|hits ∩ tie_boundary|, k − |strict_above|)`、
  分母を `min(k, 可視件数)` とする（`tie_boundary` からの充当を残り枠で頭打ち
  にし、`strict_above` の欠落を `tie_boundary` の過剰一致で埋め合わせない。
  codex-review 指摘対応）。厳密一致版は `recall_at_10_strict` として併記する。

## 横断ベンチ実測（25,000 行・dim 128・k=10）

| フェーズ | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `vector_knn` | 786 / 1147 | 3551 / 4033 | 3623 / 4198 | 1971 / 2041 | 729 / 864 | 559 / 627 | 9251 / 13606 | 2407 / 2754 | n/a |
| `vector_knn_where` | 2819 / 3047 | 2159 / 2430 | 2151 / 2173 | 1842 / 1897 | 732 / 808 | 615 / 721 | 8355 / 9656 | 3882 / 5176 | n/a |
| `where_compound_count` | 3903 / 4248 | 1595 / 1620 | 1583 / 1706 | 950 / 958 | 7698 / 10959 | 7573 / 9676 | 970 / 1134 | 1020 / 1294 | 3063 / 3920 |
| `agg_count` | 3547 / 3714 | 1636 / 1700 | 1668 / 1781 | 616 / 626 | 1768 / 2282 | 1414 / 1765 | 685 / 898 | 792 / 872 | 1794 / 1977 |
| `agg_multi` | 3738 / 4079 | 1915 / 1947 | 1950 / 1998 | 1735 / 1761 | n/a | n/a | 7066 / 8810 | 7172 / 8679 | 2516 / 2580 |
| `group_by_having` | 3948 / 4495 | 2887 / 3037 | 2958 / 3118 | 2735 / 2754 | n/a | n/a | 9088 / 11634 | 9474 / 13155 | 16513 / 16710 |
| `hybrid_rrf` | 6178 / 9100 | 4764 / 5206 | 4892 / 5275 | 3508 / 4176 | n/a | n/a | 10328 / 13194 | 3820 / 4419 | n/a |
| `mode_recall` | 726 / 812 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `mode_precision` | 721 / 1096 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `udf_call` | 730 / 1149 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `rls_isolation` | 3552 / 3675 | 1623 / 1644 | 1657 / 1701 | 620 / 634 | 1761 / 2356 | 1406 / 1786 | 662 / 828 | 653 / 833 | 1792 / 1905 |
| `explain` | n/a | 113 / 123 | 122 / 166 | 4 / 4 | n/a | n/a | 1054 / 1503 | 589 / 717 | 133 / 139 |

n/a は当該 DB にその機能が無い（または wire から到達できない）ことを示し、結果 JSON には
`unsupported` と理由を fail-closed で記録している。`mode_recall`／`mode_precision`
（TASK-161）・`udf_call`（TASK-79）は self 固有機能。self の `explain` は `USING PLAN` 文
専用（TASK-78）のため通常 SELECT では n/a。

| 指標 | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `ingest_bulk` | n/a | 0.50 s（49,744 rows/s） | 0.49 s（50,599 rows/s） | 0.27 s（93,138 rows/s） | 2.00 s（12,516 rows/s） | 2.04 s（12,241 rows/s） | 0.20 s（125,093 rows/s） | 0.17 s（151,040 rows/s） | 1.61 s（15,501 rows/s） |
| `ingest_single_stmt` | 0.13 s（7,774 rows/s） | 0.64 s（1,562 rows/s） | 1.27 s（785 rows/s） | 0.09 s（11,604 rows/s） | 0.97 s（1,028 rows/s） | 1.51 s（661 rows/s） | 1.35 s（741 rows/s） | 1.46 s（685 rows/s） | 1.45 s（687 rows/s） |
| `recall_at_10` | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 0.8360 | n/a |
| `recall_at_10_strict` | 0.9995 | 0.9865 | 0.9865 | 0.9740 | 0.7570 | 0.7735 | 0.9990 | 0.6125 | n/a |

self の `ingest_bulk` は wire に COPY 相当が無く（`EngineCore::execute_insert_sql_batch`
は Rust API のみ）n/a。`ingest_single_stmt` は行形 INSERT を `USING OPERATION_ID`
付きで 1,000 行送る（commit 粒度は全 DB とも 1 文ごと〔pgvector は autocommit、MySQL・
sqlite-vec は 1 文ごとに commit、Qdrant は `wait=True`、LanceDB は 1 行ずつ `add`〕。7,473 rows/s。pgvector 1,601・Qdrant 1,081 rows/s より速く、in-process の sqlite-vec 11.7k rows/s には及ばない）。

sqlite-vec の `ingest_bulk`／`ingest_single_stmt` はファイルベース化前
（`:memory:`）の値（「公平性の注記」節参照）。再計測は次回実行時。

### 所見

- **フィルタなし KNN（exact）**: self 786 µs（p50）は Qdrant exact 729 µs と同等で、
  pgvector exact 3,551 µs・sqlite-vec 1,971 µs・LanceDB exact 9,251 µs より速い。
  Recall@10（同点許容）は self を含む exact 全構成で 1.0。
- **フィルタ付き KNN・集計・GROUP BY・hybrid**: self は 2.8〜6.1 ms で、pgvector
  （1.6〜4.8 ms）・sqlite-vec（0.6〜3.5 ms）より遅い。in-process の `feature_bench`
  では同種フェーズが 1 ms 台であり、差分の主因は wire 往復＋テキスト応答の組み立て
  と SQL 表層のフィルタ経路（`docs/design/c1-p95-dedicated-env-reverification.md`）。
  MySQL の `GROUP BY`/`HAVING` は 16.5 ms と突出して遅い。
- **投入**: 一括投入は LanceDB（125k〜151k rows/s）＞ sqlite-vec ＞ pgvector ＞ MySQL ＞ Qdrant。
- **ANN の効果**: 25,000 行では pgvector HNSW と exact の差はほぼ無く（3.6 ms 前後）、
  LanceDB HNSW は 9.3→2.4 ms と速くなる代わりに Recall@10 0.836 へ低下した。Qdrant HNSW
  （構築完了確認後）は 729→559 µs で Recall@10 1.0 を維持した。

## 広域取得（LLM へ丸ごと渡す用途）の実測

本 DB の設計思想は「正解を含むデータ群を広く返す」ことにあり、上位数件を精密に
並べ替えるより、必要な文書が漏れなく含まれる集合を安価に返すことを狙っている。
この用途を模した「広域取得」5 フェーズ（`id` と `body` の両方を返す。LLM へ
渡す本文の送出コストを含める）を全 DB に追加して計測した。

**前提の確認（現行実装の制約）**: SQL 表層の許可リスト
（`crates/engine/src/sql/allowlist.rs::parse_select_shape`）は行を返す `SELECT` に
`ORDER BY <距離>` か `USING PLAN` を必須とし、ORDER BY なしのスカラーフィルタのみの
行取得は受理しない（`scan_where_nosort_k500` は実行して拒否を捕捉し unsupported）。
したがって現状の「広く取る」は、Top-k の k を大きく取る（上限 `MAX_SEARCH_K` =
10,000）形でしか表現できず、「ソートしない」経路は SQL 表層に存在しない。
`USING MODE 'recall'` は Top-k を固定件数で返すモードであり、しきい値で件数が
可変になる構文も現状無い（`precision` は逆に少数件へ絞る側）。

| フェーズ | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `bulk_knn_k200` | 7993 / 10019（200 行） | 4455 / 5143（200 行） | 4443 / 4630（200 行） | 5280 / 11497（200 行） | 3213 / 4272（200 行） | 3097 / 3377（200 行） | 10446 / 13536（200 行） | 3865 / 4500（200 行） | n/a |
| `bulk_knn_k1000` | 11151 / 15399（1000 行） | 4989 / 5281（1000 行） | 5049 / 5143（1000 行） | 28729 / 45359（1000 行） | 12605 / 14087（1000 行） | 12761 / 17248（1000 行） | 12208 / 15042（1000 行） | 6029 / 7254（1000 行） | n/a |
| `bulk_knn_where_k200` | 4661 / 4889（200 行） | 2438 / 2473（200 行） | 2456 / 2476（200 行） | 13613 / 13865（200 行） | 3168 / 5409（200 行） | 3288 / 3682（200 行） | 9460 / 11038（200 行） | 5037 / 6306（200 行） | n/a |
| `bulk_hybrid_k200` | 9370 / 11789（200 行） | 5117 / 5531（200 行） | 5287 / 5900（200 行） | 6425 / 11701（200 行） | n/a | n/a | 11552 / 14278（200 行） | 5285 / 6522（200 行） | n/a |
| `scan_where_nosort_k500` | n/a | 999 / 1012（500 行） | 1003 / 1022（500 行） | 242 / 247（500 行） | 4696 / 8082（500 行） | 4885 / 5838（500 行） | 2395 / 3011（500 行） | 2190 / 2801（500 行） | 893 / 940（500 行） |

ANN 構成は候補幅を 3 DB とも同じ規則 max(64, k) へ引き上げて計測した（pgvector
`hnsw.ef_search`〔終了後 64 へ復帰〕、Qdrant `hnsw_ef`、LanceDB `ef`。hybrid は候補
プール 200 に対して 200）。
`bulk_hybrid_k200` の pgvector・sqlite-vec は候補プールを各 200 へ拡大（既存
`hybrid_rrf` は各 50）。Qdrant の payload には本計測から `body` を含めたため
`ingest_bulk`／`ingest_single_stmt` の値は前回計測と条件が異なる。

### self の投影コスト切り分け（wire 経由・フィルタなし・p50 / p95 µs）

`bulk_knn_k200` の self が `vector_knn`（0.7 ms）の約 11 倍になる原因を、投影列と
k を変えて切り分けた（`scratchpad` 上の ad-hoc 計測・50 反復・同一条件）。

| 投影 | k=10 | k=200 | k=1000 | k=5000 |
| --- | --- | --- | --- | --- |
| `id` | 716 / 1095 | 1423 / 2411 | 4212 / 4951 | 17331 / 18581 |
| `id, lang` | 9804 / 13191 | — | 14850 / 16643 | — |
| `id, body` | 11245 / 12562 | 12564 / 16698 | 15717 / 18068 | 30571 / 32569 |

表は連続する 2 回の ad-hoc 実行（1 回目: `id`／`id, body` × k=10〜5000、2 回目:
`id, lang` を加えた k=10／1000）を合成したもので、両実行の `id` k=10 は 716 と 761
（この幅が run-to-run のノイズ帯）。スカラー列を 1 列でも投影すると、k に依存しない
約 9〜10 ms の固定コストが乗る。
原因は `crates/engine/src/sql/exec.rs` の `on_visible_row`（RLS→SCALAR 段）が
投影列を含む場合に全可視行（25,000 行）へ `row_codec::scan_scalar_columns`
（UTF-8 検証・列複製）を適用してから Top-k を選出する構造にあり、`project_rows`
は Top-k 確定後に構築済みの列を読むだけで LIMIT はこの段のコストに効かない。
`SELECT id` だけは `SqlArenaCache` の恒等写像高速経路（`cache_fast_path_eligible`:
フィルタなし・hybrid でない・投影がスカラー列を参照しない）に乗るため全行デコードを
省略できる。Issue #350 で集計経路に導入した必要列限定デコード（`DecodeTier`）は
`sql/exec.rs` の通常 SELECT 経路には結線されていない。

### 所見（広域取得・フィルタ＋ソート）

- **広域取得は self が「かなり速い」状態にはない（中位）。** `bulk_knn_k200` の self
  8.0 ms は Qdrant 3.1〜3.2 ms・LanceDB HNSW 3.9 ms・pgvector 4.4〜4.5 ms・sqlite-vec 5.3 ms
  より遅い。`bulk_knn_k1000` の self 11.2 ms は pgvector 5.0 ms・LanceDB HNSW 6.0 ms
  より遅く、Qdrant 12.6 ms・LanceDB exact 12.2 ms・sqlite-vec 28.7 ms よりは速い。
- **差の主因は検索本体ではなく投影の全行デコード。** `id` のみなら k=1000 でも
  4.2 ms で pgvector（5.0 ms）と同等以上であり、スカラー列投影を Top-k 確定後の
  k 行に限定できれば `id, body` k=1000 も 5 ms 前後（`id` 4.2 ms ＋ 124 KB の本文
  送出）まで下がる余地がある。この改善は production コード（`sql/exec.rs`）の変更を
  伴うため本 PR の対象外とし、Issue #453 として起票した。
- **フィルタ＋ソート（既存 DB 型の使い方）でも self は中位〜下位。** k=10 の
  `vector_knn_where` は self 2.8 ms に対し Qdrant 0.6〜0.7 ms・sqlite-vec 1.8 ms・
  pgvector 2.2 ms。k=200 の `bulk_knn_where_k200` は self 4.7 ms に対し pgvector 2.4〜2.5 ms・
  Qdrant 3.2〜3.3 ms（sqlite-vec は 13.6 ms と悪化）。フィルタ付きは `SELECT id` でも
  高速経路から外れ、全行の `scan_scalar_columns` を通るため、投影と同じ構造的コストが
  約 2 ms 分乗っている。
- **ソートなしのスカラーフィルタ取得は self に経路が無い。** 他 DB は 0.24 ms
  （sqlite-vec）〜4.9 ms（Qdrant scroll）で 500 行を返す。設計思想どおりの
  「フィルタのみで広く返す」経路は、オーナー確認（2026-09-05）を経て
  広域取得／従来型の 2 モード構想として Issue #454 に起票した（spec 側の定義が前提）。
- **ベクトル検索の本体は速い。** `SELECT id` の k=10 0.7 ms・k=1000 4.2 ms は brute-force
  ながら Qdrant exact と同等で、投影・フィルタ経路のオーバーヘッドを取り除けば広域取得
  でも上位に入る見込み。

## wire-server の 40 ms 下限と `TCP_NODELAY` 修正

初回計測では self の全フェーズが p50 42〜48 ms（1 ms 刻みで量子化）に張り付き、
psql でも `SELECT COUNT(*)` が約 45 ms だった（in-process では 1 ms 台）。原因は
`crates/wire-server/src/server.rs` の accept 直後に `TCP_NODELAY` を設定しておらず、
簡易クエリ応答が RowDescription／DataRow／CommandComplete／ReadyForQuery の 4 回の
小さな `write_all` に分かれるため Nagle アルゴリズムと受信側 delayed ACK が相互作用
していたことによる（PostgreSQL 本体・libpq も接続に `TCP_NODELAY` を設定する慣行）。

`accept_loop_inner` の `apply_read_timeout` 直後で `set_nodelay(true)` を設定し（失敗時は
同様に当該接続をスキップする fail-closed）、回帰テスト
`crates/wire-server/tests/wire_nodelay_latency.rs`（20 往復の中央値 < 20 ms）を追加した。
psql の `SELECT COUNT(*)` は 45 ms → 約 3.7 ms。

| フェーズ | 修正前 p50 | 修正後 p50 | 修正後 p95 |
| --- | --- | --- | --- |
| `vector_knn` | 41992 | 786 | 1147 |
| `vector_knn_where` | 44003 | 2819 | 3047 |
| `where_compound_count` | 45009 | 3903 | 4248 |
| `agg_count` | 44966 | 3547 | 3714 |
| `agg_multi` | 44996 | 3738 | 4079 |
| `group_by_having` | 45000 | 3948 | 4495 |
| `hybrid_rrf` | 48001 | 6178 | 9100 |
| `mode_recall` | 41993 | 726 | 812 |
| `mode_precision` | 41995 | 721 | 1096 |
| `udf_call` | 41996 | 730 | 1149 |
| `rls_isolation` | 44960 | 3552 | 3675 |

## GPU（NVIDIA GeForce RTX 3060）での高速化

### `engine::gpu_batch` vs CPU-SIMD バッチ（`make bench-gpu-scaling`）

`crates/engine/benches/gpu_scaling_bench.rs` は `GpuBatchBackend`（f16 パック常駐・
内積のみ GPU・Top-k は CPU `TopKSelector`・スコアバッファ 32 MiB 分割）・
`GpuF32ContrastBackend`（f32 常駐対照・Issue #234）・`batch_search.rs::BatchEngine`
（CPU-SIMD f16 常駐・12 スレッド）を同一コーパス・同一クエリで比較する。
`mismatch` は GPU と CPU の Top-k 結果の不一致数（全点 0）。

**（2026-09-06 追記・Issue #532）** dispatch 構造を「1 dispatch = 1 クエリ」
から「`PolicyContext` 単位にグループ化したクエリを最大 `GPU_QUERY_TILE_MAX`
本まで 1 dispatch へタイル化」する方式へ変更した（`gpu_batch.rs::
DOT_SHADER_WGSL`）。以下の実測表は変更前の数値のまま。変更後の前後比較・
数値更新は依存先 Issue #533 で実施予定。

| rows | dim | batch | CPU-SIMD | GPU f16 | GPU f32 | per-query CPU | per-query GPU f16 | speedup f16 (p95) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 1 | 3676 | 180 | 207 | 3676 | 180 | 20.17x |
| 20,000 | 128 | 8 | 5008 | 1448 | 1664 | 626 | 181 | 3.45x |
| 20,000 | 128 | 64 | 16537 | 11672 | 13387 | 258 | 182 | 1.39x |
| 20,000 | 128 | 256 | 57401 | 57250 | 55457 | 224 | 223 | 0.98x |
| 20,000 | 256 | 1 | 6352 | 218 | 349 | 6352 | 218 | 28.76x |
| 20,000 | 256 | 8 | 8499 | 1826 | 2795 | 1062 | 228 | 4.58x |
| 20,000 | 256 | 64 | 27668 | 15515 | 22701 | 432 | 242 | 1.67x |
| 20,000 | 256 | 256 | 92500 | 56726 | 90891 | 361 | 221 | 1.61x |
| 100,000 | 128 | 1 | 18802 | 1351 | 2338 | 18802 | 1351 | 13.77x |
| 100,000 | 128 | 8 | 25594 | 10053 | 18100 | 3199 | 1256 | 2.53x |
| 100,000 | 128 | 64 | 82954 | 80756 | 145908 | 1296 | 1261 | 0.98x |
| 100,000 | 128 | 256 | 282268 | 324617 | 587253 | 1102 | 1268 | 0.86x |
| 100,000 | 256 | 1 | 32083 | 2972 | 6318 | 32083 | 2972 | 10.32x |
| 100,000 | 256 | 8 | 42645 | 17449 | 50092 | 5330 | 2181 | 2.21x |
| 100,000 | 256 | 64 | 138425 | 138479 | 406175 | 2162 | 2163 | 0.99x |
| 100,000 | 256 | 256 | 459848 | 550063 | 1627532 | 1796 | 2148 | 0.83x |
| 500,000 | 128 | 1 | 93955 | 6637 | 9799 | 93955 | 6637 | 13.77x |
| 500,000 | 128 | 8 | 128757 | 53269 | 79643 | 16094 | 6658 | 2.39x |
| 500,000 | 128 | 64 | 413906 | 431620 | 637257 | 6467 | 6744 | 0.94x |
| 500000 | 128 | 256 | skip（rows×batch×dim が MAX_BATCH_WORK 超過） | | | | | |
| 500,000 | 256 | 1 | 187677 | 9548 | 31713 | 187677 | 9548 | 18.83x |
| 500,000 | 256 | 8 | 239812 | 76357 | 253370 | 29976 | 9544 | 3.08x |
| 500,000 | 256 | 64 | 715567 | 613496 | 2037477 | 11180 | 9585 | 1.15x |
| 500000 | 256 | 256 | skip（rows×batch×dim が MAX_BATCH_WORK 超過） | | | | | |

- **batch=1**: GPU f16 は 13.8〜28.8 倍高速（500k×256 で 187.7 ms → 9.5 ms）。
- **batch=8**: 2.2〜4.6 倍。
- **batch≥64**: 0.83〜1.67 倍で CPU-SIMD（12 コア並列）と同等〜逆転。GPU 側の
  クエリあたりコストが規模ごとにほぼ一定（20k で約 180 µs、500k×128 で約 6.6 ms）で
  バッチ化の効果が出ていない。
- f16 常駐は f32 常駐より一貫して速い（500k×256 batch 8 で 76 ms vs 253 ms）。

### FAISS（IndexFlatIP）CPU vs GPU

| rows | dim | batch | CPU | GPU f32 | GPU f16 | GPU f16 / CPU |
| --- | --- | --- | --- | --- | --- | --- |
| 20,000 | 128 | 1 | 188 | 112 | 104 | 1.8x |
| 20,000 | 128 | 8 | 416 | 129 | 119 | 3.5x |
| 20,000 | 128 | 64 | 8980 | 172 | 172 | 52.2x |
| 20,000 | 128 | 256 | 14974 | 330 | 418 | 35.8x |
| 20,000 | 256 | 1 | 475 | 148 | 121 | 3.9x |
| 20,000 | 256 | 8 | 782 | 169 | 166 | 4.7x |
| 20,000 | 256 | 64 | 15967 | 220 | 296 | 53.9x |
| 20,000 | 256 | 256 | 31780 | 512 | 662 | 48.0x |
| 100,000 | 128 | 1 | 2283 | 447 | 387 | 5.9x |
| 100,000 | 128 | 8 | 3831 | 525 | 464 | 8.3x |
| 100,000 | 128 | 64 | 37958 | 753 | 833 | 45.6x |
| 100,000 | 128 | 256 | 122907 | 1416 | 1426 | 86.2x |
| 100,000 | 256 | 1 | 5059 | 472 | 455 | 11.1x |
| 100,000 | 256 | 8 | 7567 | 664 | 596 | 12.7x |
| 100,000 | 256 | 64 | 66960 | 964 | 984 | 68.0x |
| 100,000 | 256 | 256 | 249923 | 2273 | 2292 | 109.0x |
| 500,000 | 128 | 1 | 12079 | 2095 | 1788 | 6.8x |
| 500,000 | 128 | 8 | 18727 | 2495 | 2167 | 8.6x |
| 500,000 | 128 | 64 | 176946 | 3268 | 2967 | 59.6x |
| 500,000 | 128 | 256 | 692931 | 6887 | 6782 | 102.2x |
| 500,000 | 256 | 1 | 27458 | 2910 | 2141 | 12.8x |
| 500,000 | 256 | 8 | 37695 | 3485 | 2724 | 13.8x |
| 500,000 | 256 | 64 | 359958 | 4591 | 3693 | 97.5x |
| 500,000 | 256 | 256 | 1457926 | 11188 | 10954 | 133.1x |

FAISS GPU はバッチが大きいほど有利で、500k×128 batch 64 では CPU 176.9 ms → GPU f16
3.0 ms（約 60 倍）。engine の GPU 経路が batch≥64 で頭打ちになるのと対照的であり、
engine 側は Top-k を CPU で行いスコアバッファを読み戻す構造（GPU→CPU 転送量が
rows × batch × 4 バイト）がボトルネックと推定される。GPU 上での Top-k 選出
（分割 Top-k のリダクション）は改善候補だが、本タスクでは profiling を含め対象外。

### Qdrant HNSW 索引構築 CPU vs GPU

| rows | 構成 | upsert | index_build | search p50 / p95 µs | GPU ログ検出 |
| --- | --- | --- | --- | --- | --- |
| 100,000 | cpu | 3.0 s | 3.01 s | 1843 / 2509 | False |
| 500,000 | cpu | 9.2 s | 20.58 s | 955 / 1155 | False |
| 100,000 | gpu | 1.7 s | 5.03 s | 1090 / 1882 | True |
| 500,000 | gpu | 8.6 s | 21.07 s | 855 / 1153 | True |

投入中は索引構築を止め（`indexing_threshold` を行数より大きく設定）、投入完了後に
閾値を下げて構築を開始した時点から green・全件 indexed までを計時する。100k で GPU
5.0 s vs CPU 3.0 s、500k で 21.1 s vs 20.6 s と、この規模・この VM では GPU 索引構築
の利得は無い（GPU 使用はコンテナログの `Create GPU device`〔初期化〕で確認。
`Found GPU device` は列挙のみで証拠にしない）。探索 p50 は同水準（GPU は構築のみ）。

### 結論

GPU は単発〜小バッチの全件内積検索で明確に高速化する（10〜29 倍）。大バッチの
スループットでは現行実装は CPU-SIMD と同等にとどまり、FAISS との差が改善余地を示す。
GPU 経路は in-process API（`engine::gpu_batch`）のみで SQL／wire からは到達できない。

## 計測ツール

- `scripts/crossdb_bench/`: Python ハーネス（`run.py --db self|pgvector|sqlite_vec|qdrant|lancedb|mysql --config exact|hnsw`・`containers.sh`・`run_all.sh`）。
  依存は `requirements.txt` で `==` 固定。Cargo 依存は増やしていない。
  `CROSSDB_DIM`（`run_all.sh`。十進数字のみ）・`run.py --expect-dim` で dim 別
  fixture（`docs25k-d<dim>.*`）を計測できる（Issue #466。下記「dim=768 基線」節）。
- `scripts/crossdb_bench/gpu/`: FAISS（`Dockerfile.faiss`・`faiss_batch_bench.py`）・
  Qdrant GPU（`qdrant_gpu_build_bench.py`・`containers_gpu.sh`）。
- `crates/engine/examples/seed_docs.rs`: fixture 生成（`seed`／`export`／`queries`）。dim 引数で
  任意次元数の fixture を生成できる。
- `crates/engine/benches/gpu_scaling_bench.rs`・`benches/harness/gpu_scaling.rs`・
  `tests/gpu_scaling_accept.rs`。
- `crates/engine/examples/feature_bench.rs`・`crates/engine/benches/knn_profile_bench.rs`:
  `BENCH_FEATURE_DIM`／`BENCH_KNN_PROFILE_DIM`（Issue #466。正整数・既定 128・上限 4,096・
  fail-closed）でベクトル次元数を上書きできる（`crates/engine/benches/harness/
  bench_engine.rs::parse_dim`）。

## `vector_knn` 786µs の内訳（Issue #463）

`vector_knn`（self 786/1147µs）を wire／SQL 表層／距離カーネル・Top-k の 4 区分へ
切り分ける実測を `crates/wire-server/benches/knn_wire_profile_bench.rs`
（`make bench-knn-wire-profile`）で実施した。共有開発環境（`BENCH_DEDICATED_ENV`
未設定・参考値）での 1 回の実測（min-of-5 どうし・T3 690.3µs に対する比率。
最新の実測表・計測条件は `docs/design/knn-wire-stage-profile.md`「実測結果」節
が SSOT。当所の数値はそこからの要約であり、両者が食い違う場合は
`knn-wire-stage-profile.md` を正とする）:

| 区分 | 比率 |
| --- | --- |
| SQL 表層 | 55.4% |
| 距離カーネル | 29.6% |
| wire | 11.9%（参照区間帯内） |
| Top-k（単線条件） | 1.8%（参照区間帯内） |

SQL 表層が距離カーネル自体より支配的な区分であり、wire 区分（TCP 送受信・
フレーミング・応答エンコード）の寄与は 11.9% にとどまる。詳細な計測設計・
fail-closed 検証・実測表は `docs/design/knn-wire-stage-profile.md` 参照。

## `hybrid_rrf` 6,178µs の内訳（Issue #465）

`hybrid_rrf`（self 6178/9100µs）の engine 内 hybrid 経路／SQL 表層／wire の
3 区分と、engine 内部の B0s〜B8 段別内訳を `crates/engine/benches/
hybrid_profile_bench.rs`（`make bench-hybrid-profile`）・`crates/wire-server/
benches/hybrid_wire_profile_bench.rs`（`make bench-hybrid-wire-profile`）で
実施した。共有開発環境（`BENCH_DEDICATED_ENV` 未設定・参考値）での実測要約
（詳細・実測表・per-round 生データは `docs/design/hybrid-rrf-latency-breakdown.md`
「最新基線」節が SSOT）:

| 区分（wire 側 T1p〜T3） | 比率 |
| --- | --- |
| engine 内 hybrid（T1p） | 28.6% |
| SQL 表層（T2−T1p） | 67.1%（参照区間帯超過） |
| wire（T3−T2） | 4.3%（参照区間帯内） |

engine 内部の B0s〜B8 段別内訳では SQL 表層固定コスト（B1−B4）と疎側再取得
ループ（B5）がほぼ同水準（37〜39%）で並び最大区分であり、融合・境界同点
グループ完全化を含む残差は 19% にとどまることを確認した。Phase 6（Issue #548）
への引き継ぎ内容もあわせて同節に記録。production コード
（`crates/engine/src/`・`crates/wire-server/src/`）は無変更。

Phase 6（#546・#549）を通しで前後比較した crossdb self（wire 経由）の
`hybrid_rrf` 前後表は `docs/design/hybrid-rrf-phase6-before-after.md`
（Issue #550）に記録した（数値は同 doc 参照。本節では転記しない）。

## dim=768 基線（Issue #466）

### 目的

Issue #365（isa.rs dot カーネルの複数アキュムレータ化検討）の実測で dim が
採否の判別変数になることが判明したが、横断 SQL ベンチ（`feature_bench`・
`knn_profile_bench`・`scripts/crossdb_bench/`）はいずれも dim=128 の単一規模点
しか持たず、dim>=768 限定ディスパッチ等の後続施策（例: Issue #517）を横断 SQL
経路で判定する材料が無かった。本節はその計測基盤整備（`BENCH_FEATURE_DIM`／
`BENCH_KNN_PROFILE_DIM`／`CROSSDB_DIM`・`run.py --expect-dim`）と dim=768 の
1 回実測を記録する（性能改善そのものは対象外）。

### 計測条件

- fixture: `seed_docs seed docs25k-d768.redb 25000 768` → `export` →
  `queries 768 200 queries200-d768.jsonl`（tenant-a 23,000 public・tenant-b
  2,000 private・200 クエリ・k=10。dim 128 の既存 fixture と同じ生成方式）
- self コミット: `fa79572`（`origin/main`。本 Issue の production コード変更は
  無し）
- 環境: `lscpu` Model name `QEMU Virtual CPU version 2.5+`、命令セットフラグ
  `avx2 f16c fma`、`nproc` 12、`BENCH_DEDICATED_ENV` 未設定（共有 QEMU 環境。
  `docs/design/benchmark-judgement-policy.md` §5 により**参考値**。1 回実測・
  交互 N≥5 ペアなし）
- 他 DB: pgvector `pg17`（Docker イメージ `pgvector/pgvector:pg17`）・Qdrant
  `1.19.1`（Docker イメージ `qdrant/qdrant:latest`）。索引パラメータは dim=128
  の既存計測と同一（既定値のまま。上記「公平性についての注記」参照）
- Qdrant は self／pgvector とは別セッションの実測（コミット・環境は同一。
  `docs/design/benchmark-judgement-policy.md` §5 により同様に**参考値**）。
  本計測環境に本ハーネス外の `bench-qdrant`（127.0.0.1:16333/16334）コンテナが
  常駐していたため、`containers.sh` へ追加したコンテナ名・ポート上書き
  （`CROSSDB_QDRANT_CONTAINER`／`CROSSDB_QDRANT_HTTP_PORT`／`CROSSDB_QDRANT_GRPC_PORT`。
  Issue #466）を使い、別名・別ポートの `bench-qdrant-466`（127.0.0.1:26333/26334）
  として並存させ、外部コンテナは一度も停止・削除せずに計測した

### `vector_knn`・`bulk_knn_k200`（p50/p95 µs）

| 系列 | vector_knn p50 | vector_knn p95 | bulk_knn_k200 p50 | bulk_knn_k200 p95 | recall_at_10 |
| --- | --- | --- | --- | --- | --- |
| self (wire, exact) | 2222.2 | 2569.0 | 51696.3 | 54974.6 | 1.0000 |
| pgvector (exact) | 33907.3 | 34976.3 | 35131.7 | 35629.0 | 1.0000 |
| pgvector (hnsw) | 34509.7 | 35291.6 | 35097.2 | 36314.8 | 1.0000 |
| Qdrant (exact) | 2195.2 | 2816.7 | 5355.7 | 7316.6 | n/a |
| Qdrant (hnsw) | 1045.9 | 1763.3 | 3707.6 | 4143.1 | n/a |

Qdrant の `recall_at_10` が `n/a` なのは dim=128 の既存計測（本 doc 冒頭の表）と
同じ仕様で、本ハーネスの Qdrant アダプタが recall 計算を実装していないため
（self／pgvector は fixture 内の ground truth 比較で算出）。

dim=128 の同フェーズ実測値（参考。導出方法・環境が異なるため直接比較しない）は
上記「`vector_knn` 786µs の内訳」節・`docs/design/knn-wire-stage-profile.md`
参照。

### `feature_bench` 13 フェーズ（同一バイナリ・同一環境での dim=128 vs dim=768 vs dim=1536。参考値）

| フェーズ | dim=128 p50 (µs) | dim=768 p50 (µs) | 128→768 比 | dim=1536 p50 (µs) | 768→1536 比 |
| --- | --- | --- | --- | --- | --- |
| ingest | 4089 | 14693 | 3.59x | 25450 | 1.73x |
| point_where | 2810 | 4577 | 1.63x | 7163 | 1.57x |
| where_compound | 2917 | 5460 | 1.87x | 5998 | 1.10x |
| agg_count | 2545 | 5105 | 2.01x | 5445 | 1.07x |
| agg_multi | 2745 | 5246 | 1.91x | 5574 | 1.06x |
| group_by_having | 3213 | 5568 | 1.73x | 5825 | 1.05x |
| vector_knn | 8608 | 48681 | 5.66x | 86572 | 1.78x |
| vector_knn_where | 2804 | 4938 | 1.76x | 7292 | 1.48x |
| hybrid_rrf | 11741 | 70275 | 5.99x | 119294 | 1.70x |
| mode_recall | 8713 | 50080 | 5.75x | 86716 | 1.73x |
| mode_precision | 8739 | 49311 | 5.64x | 86274 | 1.75x |
| rls_isolation | 2411 | 4915 | 2.04x | 5341 | 1.09x |
| udf_call | 678 | 2338 | 3.45x | 4460 | 1.91x |

`vector_knn`・`hybrid_rrf`・`mode_recall`／`mode_precision`（いずれも距離計算・
Top-k 選出を経由する）が dim 増加に対して最も敏感（128→768 で 5.6〜6.0 倍、
768→1536 でも 1.7〜1.8 倍と他フェーズより明確に大きい）。`ingest`・`agg_*`
（embedding を主要コストとしない経路）は 128→768 で 2〜3.6 倍、768→1536 では
1.05〜1.1 倍程度まで鈍化する。比は**参考値**（同一プロセス内の単発実測。
`docs/design/benchmark-judgement-policy.md` の交互 N≥5 ペア方式ではない）。
dim=1536 は `feature_bench` の 1 回実測のみで、`knn_profile_bench`・crossdb
横断ベンチでは未実測（下記「申し送り」参照）。

### `knn_profile_bench`（S0〜S5' 段別。dim=768・既定エンジン）

| 段 | median | ns/row |
| --- | --- | --- |
| S4_arena_build | 42.077ms | 1683.1 |
| S5_search_parallel | 1.687ms | 67.5 |
| S5_search_scalar | 2.828ms | 113.1 |
| S5prime_distance_only | 2.594ms | 103.8 |
| S0_cold_sql_e2e | 124.178ms | 4967.1 |
| S0_hot_sql_e2e | 2.278ms | 91.1 |
| S0prime_count_star | 2.966ms | 118.7 |
| S1_redb_scan | 2.014ms | 80.6 |
| S2_header_decode | 2.164ms | 86.6 |
| S3_f32_decode | 11.491ms | 459.6 |

dim=768 では `S4_arena_build`（embedding 込み全件デコード＋arena 構築）が
ns/row 1683.1 と、dim=128 実測（`docs/design/hnsw-index.md` 参照）に比べ
明確に支配的な段になる。`S1`／`S2`（走査・ヘッダデコードのみ）は dim 非依存の
ため ns/row がほぼ変わらない。consistency checks（S1..S3 行数一致・S0 結果
件数・S0' COUNT(*)・S3 vs VectorArena cross-check）はすべて green。

### 再現手順

```bash
cargo build --release -p engine --example seed_docs -p wire-server

S=<CROSSDB_DIR>
cargo run --release -p engine --example seed_docs -- seed "$S/docs25k-d768.redb" 25000 768
cargo run --release -p engine --example seed_docs -- export "$S/docs25k-d768.redb" "$S/docs25k-d768.jsonl"
cargo run --release -p engine --example seed_docs -- queries 768 200 "$S/queries200-d768.jsonl"

python scripts/crossdb_bench/run.py --db self --config exact \
  --rows-file "$S/docs25k-d768.redb" --queries-file "$S/queries200-d768.jsonl" \
  --out-dir "$S/results/d768" --expect-dim 768

scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config exact \
  --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl" \
  --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down pgvector
scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config hnsw \
  --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl" \
  --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down pgvector

# Qdrant は本ハーネス外の常駐コンテナ（既定名 bench-qdrant）と衝突しないよう、
# コンテナ名・ポートを上書きして計測する（Issue #466）。
export CROSSDB_QDRANT_CONTAINER=bench-qdrant-466
export CROSSDB_QDRANT_HTTP_PORT=26333 CROSSDB_QDRANT_GRPC_PORT=26334
scripts/crossdb_bench/containers.sh up qdrant
python scripts/crossdb_bench/run.py --db qdrant --config exact   --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl"   --docs-file "$S/docs25k-d768.jsonl" --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down qdrant
scripts/crossdb_bench/containers.sh up qdrant
python scripts/crossdb_bench/run.py --db qdrant --config hnsw   --rows-file "$S/docs25k-d768.jsonl" --queries-file "$S/queries200-d768.jsonl"   --docs-file "$S/docs25k-d768.jsonl" --out-dir "$S/results/d768" --expect-dim 768
scripts/crossdb_bench/containers.sh down qdrant
unset CROSSDB_QDRANT_CONTAINER CROSSDB_QDRANT_HTTP_PORT CROSSDB_QDRANT_GRPC_PORT

BENCH_FEATURE_DIM=768 cargo run --release -p engine --example feature_bench
BENCH_KNN_PROFILE_DIM=768 make bench-knn-profile
```

per-run 生データは
`$CROSSDB_DIR/results/d768/{self_exact,pgvector_exact,pgvector_hnsw,qdrant_exact,qdrant_hnsw}.json`
（self／pgvector は `run_all.sh` 経由なら `CROSSDB_DIM=768 make bench-crossdb ...`。
Qdrant は上記の別セッション実測のため `run_all.sh` には含まれない）。

### 申し送り

- **dim=1536 は `feature_bench` のみ参考実測**: `BENCH_FEATURE_DIM`／
  `BENCH_KNN_PROFILE_DIM`／`CROSSDB_DIM` はいずれも dim=1536 を受理できるが
  （上限 4,096）、上記「`feature_bench` 13 フェーズ」節の 1 回実測のみで、
  `knn_profile_bench`・crossdb 横断ベンチ（self／pgvector／Qdrant）は
  dim=768 までで完了させた。
- `scan_stage_profile_bench`／`knn_wire_profile_bench`／`hybrid_wire_profile_bench`・
  `batch_bench.rs`・`gpu/faiss_batch_bench.py` の `DIM_GRID` は dim 展開の対象外
  （本 Issue のスコープ外）。
- 専有環境（`BENCH_DEDICATED_ENV=1`）での再実測はオーナー作業として引き続き
  未実施（`docs/design/benchmark-judgement-policy.md` §9 と同方針）。

## 申し送り

- SQL 表層のフィルタ付き経路・集計の wire 越し 2.7〜6 ms は、in-process との差分を
  wire 側（応答組み立て・テキスト化）と SQL 側で切り分ける profiling が未実施
  （`vector_knn`〔フィルタなし〕は上記「`vector_knn` 786µs の内訳」節で実施済み）。
- スカラー列投影時の全行デコード（`sql/exec.rs::on_visible_row` の `scan_scalar_columns`）
  を Top-k 確定後の k 行へ遅延させる改善は Issue #453（上記「self の投影コスト切り分け」節）。
  ORDER BY なしのフィルタのみ行取得（広域取得モード）の SQL 表層追加は Issue #454。
- engine GPU 経路の大バッチ頭打ち（Top-k の GPU 化・転送量削減）は別タスク。
- 本計測は共有 VM（loadavg 約 2）での単発実測であり、専有環境での再測定は未実施。
