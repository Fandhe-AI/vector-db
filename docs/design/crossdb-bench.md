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
| sqlite-vec | sqlite3 3.46.1・sqlite-vec 0.1.9（in-process・vec0 brute-force・cosine 近似〔内積指標なし〕） |
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
- **可視性モデル**: self の wire セッションは現行契約でどのテナントからも
  `visibility = 'public'` の行のみ可視（private は所有テナント自身からも不可視）。
  他 DB は RLS を持たないため `visibility` 列を持たせ毎クエリに
  `WHERE visibility = 'public'` を付け、可視集合を同じ 23,000 行に揃えた
  （テナント列では絞らない。ポリシーエンジンの比較ではない）。
- **exact / ANN**: `exact` は索引なし全件探索、`hnsw` は各 DB の近似索引。
  self は既定エンジン（brute-force）のみ計測した（ANN opt-in は wire から選択できない）。
  Qdrant の `hnsw` は `indexing_threshold` を下げて全セグメントを索引化し、status green
  かつ `indexed_vectors_count` が全件に達したことを確認してから計測する（既定閾値では
  25,000 行・dim 128 は各セグメントが閾値未満のまま HNSW が構築されない）。LanceDB の
  hybrid もベクトル側は `metric="dot"` を明示している。sqlite-vec の hybrid は FTS5 の
  `MATCH` に自然文をそのまま渡すと構文エラーになるため、各語を二重引用符で囲んだ語句
  クエリへ変換している。
- **Recall@10**: 合成 embedding は内積の同点が多いため、主指標 `recall_at_10` は
  「真スコアが 10 位のスコア以上の全文書」を正解集合とする同点許容版
  （`TIE_EPSILON = 1e-4`）。厳密一致版は `recall_at_10_strict` として併記する。

## 横断ベンチ実測（25,000 行・dim 128・k=10）

| フェーズ | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `vector_knn` | 732 / 1147 | 3666 / 3894 | 3555 / 4144 | 1977 / 2048 | 757 / 972 | 588 / 650 | 9024 / 17139 | 2592 / 3107 | n/a |
| `vector_knn_where` | 2899 / 3047 | 2107 / 2231 | 2120 / 2294 | 1898 / 1928 | 631 / 842 | 574 / 639 | 8174 / 10800 | 4099 / 4821 | n/a |
| `where_compound_count` | 3966 / 4261 | 1606 / 1723 | 1583 / 1637 | 984 / 1015 | 5860 / 8597 | 5267 / 5640 | 967 / 1101 | 1072 / 1355 | 3021 / 3228 |
| `agg_count` | 3569 / 3807 | 1676 / 1869 | 1632 / 1804 | 643 / 665 | 1872 / 2494 | 1368 / 1677 | 684 / 866 | 732 / 896 | 1799 / 1913 |
| `agg_multi` | 3822 / 4094 | 1951 / 2038 | 1898 / 1957 | 1742 / 1786 | n/a | n/a | 7446 / 9673 | 7912 / 12148 | 2513 / 2624 |
| `group_by_having` | 4031 / 4409 | 2921 / 3105 | 2816 / 3007 | 2754 / 2786 | n/a | n/a | 8286 / 10368 | 8492 / 12623 | 16664 / 17386 |
| `hybrid_rrf` | 5932 / 8500 | 4634 / 5111 | 4744 / 5380 | 3545 / 4231 | n/a | n/a | 10308 / 12652 | 4165 / 4914 | n/a |
| `mode_recall` | 745 / 1152 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `mode_precision` | 685 / 782 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `udf_call` | 750 / 1178 | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `rls_isolation` | 3580 / 3950 | 1686 / 1887 | 1620 / 1694 | 644 / 659 | 1822 / 2587 | 1311 / 1525 | 665 / 894 | 654 / 871 | 1793 / 1914 |
| `explain` | n/a | 135 / 147 | 100 / 233 | 4 / 4 | n/a | n/a | 1079 / 1306 | 602 / 736 | 134 / 154 |

n/a は当該 DB にその機能が無い（または wire から到達できない）ことを示し、結果 JSON には
`unsupported` と理由を fail-closed で記録している。`mode_recall`／`mode_precision`
（TASK-161）・`udf_call`（TASK-79）は self 固有機能。self の `explain` は `USING PLAN` 文
専用（TASK-78）のため通常 SELECT では n/a。

| 指標 | self (wire) | pgvector exact | pgvector HNSW | sqlite-vec | Qdrant exact | Qdrant HNSW | LanceDB exact | LanceDB HNSW | MySQL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `ingest_bulk` | n/a | 0.47 s（52,741 rows/s） | 0.48 s（51,735 rows/s） | 0.27 s（92,760 rows/s） | 2.07 s（12,078 rows/s） | 2.08 s（12,021 rows/s） | 0.15 s（172,225 rows/s） | 0.15 s（162,838 rows/s） | 1.58 s（15,781 rows/s） |
| `ingest_single_stmt` | 0.13 s（7,473 rows/s） | 0.62 s（1,601 rows/s） | 1.32 s（757 rows/s） | 0.02 s（66,286 rows/s） | 0.92 s（1,081 rows/s） | 1.49 s（671 rows/s） | 1.38 s（726 rows/s） | 1.41 s（709 rows/s） | 0.14 s（7,059 rows/s） |
| `recall_at_10` | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 0.8325 | n/a |
| `recall_at_10_strict` | 0.9995 | 0.9865 | 0.9865 | 0.9740 | 0.7490 | 0.7915 | 0.9990 | 0.6060 | n/a |

self の `ingest_bulk` は wire に COPY 相当が無く（`EngineCore::execute_insert_sql_batch`
は Rust API のみ）n/a。`ingest_single_stmt` は行形 INSERT を `USING OPERATION_ID`
付きで 1,000 行送る（7,474 rows/s。pgvector 1,601・Qdrant 1,234 rows/s より速く、in-process の sqlite-vec 67k rows/s には及ばない）。

### 所見

- **フィルタなし KNN（exact）**: self 732 µs（p50）は Qdrant exact 757 µs と同等で、
  pgvector exact 3,666 µs・sqlite-vec 1,977 µs・LanceDB exact 9,024 µs より速い。
  Recall@10（同点許容）は self を含む exact 全構成で 1.0。
- **フィルタ付き KNN・集計・GROUP BY・hybrid**: self は 2.9〜5.9 ms で、pgvector
  （1.6〜4.7 ms）・sqlite-vec（0.6〜3.5 ms）より遅い。in-process の `feature_bench`
  では同種フェーズが 1 ms 台であり、差分の主因は wire 往復＋テキスト応答の組み立て
  と SQL 表層のフィルタ経路（`docs/design/c1-p95-dedicated-env-reverification.md`）。
  MySQL の `GROUP BY`/`HAVING` は 16.7 ms と突出して遅い。
- **投入**: 一括投入は LanceDB（172k rows/s）＞ sqlite-vec ＞ pgvector ＞ MySQL ＞ Qdrant。
- **ANN の効果**: 25,000 行では pgvector HNSW と exact の差はほぼ無く（3.6 ms 前後）、
  LanceDB HNSW は 9.0→2.6 ms と速くなる代わりに Recall@10 0.833 へ低下した。Qdrant HNSW
  （構築完了確認後）は 757→588 µs で Recall@10 1.0 を維持した。

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
| `vector_knn` | 41992 | 732 | 1147 |
| `vector_knn_where` | 44003 | 2899 | 3047 |
| `where_compound_count` | 45009 | 3966 | 4261 |
| `agg_count` | 44966 | 3569 | 3807 |
| `agg_multi` | 44996 | 3822 | 4094 |
| `group_by_having` | 45000 | 4031 | 4409 |
| `hybrid_rrf` | 48001 | 5932 | 8500 |
| `mode_recall` | 41993 | 745 | 1152 |
| `mode_precision` | 41995 | 685 | 782 |
| `udf_call` | 41996 | 750 | 1178 |
| `rls_isolation` | 44960 | 3580 | 3950 |

## GPU（NVIDIA GeForce RTX 3060）での高速化

### `engine::gpu_batch` vs CPU-SIMD バッチ（`make bench-gpu-scaling`）

`crates/engine/benches/gpu_scaling_bench.rs` は `GpuBatchBackend`（f16 パック常駐・
内積のみ GPU・Top-k は CPU `TopKSelector`・スコアバッファ 32 MiB 分割）・
`GpuF32ContrastBackend`（f32 常駐対照・Issue #234）・`batch_search.rs::BatchEngine`
（CPU-SIMD f16 常駐・12 スレッド）を同一コーパス・同一クエリで比較する。
`mismatch` は GPU と CPU の Top-k 結果の不一致数（全点 0）。

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
の利得は無い（GPU 使用はコンテナログの `Found GPU device`／`Create GPU device` で
確認）。探索 p50 は同水準（GPU は構築のみ）。

### 結論

GPU は単発〜小バッチの全件内積検索で明確に高速化する（10〜29 倍）。大バッチの
スループットでは現行実装は CPU-SIMD と同等にとどまり、FAISS との差が改善余地を示す。
GPU 経路は in-process API（`engine::gpu_batch`）のみで SQL／wire からは到達できない。

## 計測ツール

- `scripts/crossdb_bench/`: Python ハーネス（`run.py --db self|pgvector|sqlite_vec|qdrant|lancedb|mysql --config exact|hnsw`・`containers.sh`・`run_all.sh`）。
  依存は `requirements.txt` で `==` 固定。Cargo 依存は増やしていない。
- `scripts/crossdb_bench/gpu/`: FAISS（`Dockerfile.faiss`・`faiss_batch_bench.py`）・
  Qdrant GPU（`qdrant_gpu_build_bench.py`・`containers_gpu.sh`）。
- `crates/engine/examples/seed_docs.rs`: fixture 生成（`seed`／`export`／`queries`）。
- `crates/engine/benches/gpu_scaling_bench.rs`・`benches/harness/gpu_scaling.rs`・
  `tests/gpu_scaling_accept.rs`。

## 申し送り

- SQL 表層のフィルタ付き経路・集計の wire 越し 2.7〜6 ms は、in-process との差分を
  wire 側（応答組み立て・テキスト化）と SQL 側で切り分ける profiling が未実施。
- engine GPU 経路の大バッチ頭打ち（Top-k の GPU 化・転送量削減）は別タスク。
- 本計測は共有 VM（loadavg 約 2）での単発実測であり、専有環境での再測定は未実施。
