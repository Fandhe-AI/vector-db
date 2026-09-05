# crossdb_bench

自作ベクトル DB（本リポジトリの `wire-server`＋`engine`。以下 "self"）と他 DB
（pgvector・sqlite-vec・Qdrant・LanceDB・MySQL）の機能別ベンチマークを行う
Python ハーネス。

private spec（`docs/spec`）の内容はここでは参照・転記しない。数値基準（warmup・
反復回数）は本ハーネス独自の実装既定値（`crates/engine/examples/feature_bench.rs`
の WARMUP=5・ITERS=50 に合わせてある）。Cargo 依存は増やしていない
（`cargo build --release -p wire-server` の既存バイナリをそのまま子プロセス
起動するだけ）。

## 前提

- Docker（`pgvector/pgvector:pg17`・`qdrant/qdrant:latest`・`mysql:9` を
  取得済みであること。`docker inspect` で digest／実バージョンを確認してから
  計測結果の meta に記録する）
- Python venv（`requirements.txt` を `pip install -r` 済み）
- `target/release/wire-server`（`cargo build --release -p wire-server` でビルド済み）
- `crates/engine/examples/seed_docs.rs`（別 agent 生成）等で作成した
  `docs25k.redb`・`docs25k.jsonl`・`queries200.jsonl`

## セットアップ

```bash
python -m venv /path/to/venv
source /path/to/venv/bin/activate
pip install -r scripts/crossdb_bench/requirements.txt

cargo build --release -p wire-server
```

## コンテナの起動・停止

```bash
scripts/crossdb_bench/containers.sh up pgvector
scripts/crossdb_bench/containers.sh up qdrant
scripts/crossdb_bench/containers.sh up mysql
scripts/crossdb_bench/containers.sh down pgvector
scripts/crossdb_bench/containers.sh down qdrant
scripts/crossdb_bench/containers.sh down mysql
```

`up mysql` は `mysql:9` イメージからコンテナ名 `bench-mysql`
（127.0.0.1:33306 既定、`MYSQL_ROOT_PASSWORD=bench`・`MYSQL_DATABASE=bench`）を
`docker rm -f` で毎回冪等に作り直し、`mysqladmin ping` で起動を待つ
（pgvector・Qdrant と同じ up/down 管理。`down mysql` も `docker rm -f` で
コンテナごと破棄する）。

sqlite-vec・LanceDB は in-process（コンテナ不要）。sqlite-vec は他 DB（永続ストレージ）
と投入速度の比較条件を揃えるため `--workdir` 配下のファイルベース DB を使う
（`:memory:` ではない。非永続だった旧実装の数値との比較は不可）。self は `run.py` が
`wire-server` を子プロセスとして起動・停止する。

**計測中は対象 DB 以外のコンテナを止めること**（公平な比較のため。CPU・メモリの
競合を避ける）。

### 環境変数（接続先ポート）

各 DB のポートは以下の環境変数で上書きできる。起動側（`containers.sh`／
`gpu/containers_gpu.sh`）と接続側（各 `*_db.py`・`gpu/qdrant_gpu_build_bench.py`。
`common.env_port`）が同じ変数名・同じ既定値を読む。Python 側は 1〜65535 の整数
以外（空文字は未設定扱い）を `ValueError` で拒否する（fail-closed）。

| 環境変数 | 既定値 | 対象 |
| -------- | ------ | ---- |
| `CROSSDB_SELF_PORT` | 15432 | self（`run.py` が起動する `wire-server` の bind ポート） |
| `CROSSDB_PG_PORT` | 15433 | pgvector（`containers.sh up pgvector`・`pgvector_db.py`） |
| `CROSSDB_QDRANT_HTTP_PORT` | 16333（gpu/ は 17333） | Qdrant HTTP（`containers.sh up qdrant`・`qdrant_db.py`。`gpu/containers_gpu.sh`・`gpu/qdrant_gpu_build_bench.py` も同じ変数名を読むが既定値は 17333） |
| `CROSSDB_QDRANT_GRPC_PORT` | 16334（gpu/ は 17334） | Qdrant gRPC（同上。gpu/ の既定値は 17334） |
| `CROSSDB_MYSQL_PORT` | 33306 | MySQL（`containers.sh up mysql`・`mysql_db.py`） |

```bash
CROSSDB_PG_PORT=25433 scripts/crossdb_bench/containers.sh up pgvector
CROSSDB_PG_PORT=25433 python scripts/crossdb_bench/run.py --db pgvector --config exact ...
```

self の `run.py --db self` は認証ファイル `users.txt` を `--workdir`
（既定: `--rows-file` の親ディレクトリ）直下ではなく、その配下に毎回新規作成する
一意なサブディレクトリ `self_bench_auth_<pid>_*/` へ生成し、停止時に削除する
（`--workdir` 直下に既存の `users.txt` があっても上書きしない）。

## 実行

```bash
S=/path/to/scratch/crossdb  # docs25k.redb / docs25k.jsonl / queries200.jsonl のあるディレクトリ

# self（wire-server 経由）
python scripts/crossdb_bench/run.py --db self --config exact \
  --rows-file "$S/docs25k.redb" --queries-file "$S/queries200.jsonl"

# pgvector（exact / hnsw の 2 構成を別々に計測。コンテナは毎回作り直す想定）
scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"
scripts/crossdb_bench/containers.sh down pgvector
scripts/crossdb_bench/containers.sh up pgvector
python scripts/crossdb_bench/run.py --db pgvector --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# sqlite-vec（exact 固定。--config は受け付けるが無視して meta に exact と記録する。
#   ファイルベース DB を --workdir 配下の専用一時ディレクトリに作り終了時に削除する）
python scripts/crossdb_bench/run.py --db sqlite_vec --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# Qdrant
scripts/crossdb_bench/containers.sh up qdrant
python scripts/crossdb_bench/run.py --db qdrant --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# LanceDB（in-process。--workdir 配下に lancedb ディレクトリを作る）
python scripts/crossdb_bench/run.py --db lancedb --config hnsw \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"

# MySQL（KNN 系は構造的に unsupported）
scripts/crossdb_bench/containers.sh up mysql
python scripts/crossdb_bench/run.py --db mysql --config exact \
  --rows-file "$S/docs25k.jsonl" --queries-file "$S/queries200.jsonl"
```

結果は `<queries-file のディレクトリ>/results/<db>_<config>.json`
（`--out-dir` で上書き可）に書き出される。

### スモークテスト（2,000 行サブセット）

25,000 行の本計測前に、まず小規模で全フェーズが動く／`unsupported` が正しく
記録されることを確認する。

```bash
head -n 2000 "$S/docs25k.jsonl" > "$S/docs2k.jsonl"
python scripts/crossdb_bench/run.py --db pgvector --config exact \
  --rows-file "$S/docs2k.jsonl" --queries-file "$S/queries200.jsonl"
```

self はサブセット用の別 redb を用意する必要がある（`docs25k.redb` は
25,000 行固定で書き出し済みのため、2,000 行のスモークは他 DB のみで行う）。
sqlite-vec・pgvector 等のスモークでは public/private 双方を含む区間
（例: `sed -n '8501,10500p'`）を切り出すこと。`docs25k.jsonl` は
tenant-a=public・tenant-b=private が連続した区間にまとまっているため、
単純な `head -n 2000` では全行が public になり可視性フィルタの効果を
確認できない。

## フェーズ一覧

`crates/engine/examples/feature_bench.rs` の 13 フェーズに対応させたうえで、
`recall_at_10`（正解集合との突き合わせ）と広域取得 5 種を追加した 20 種類。DB がその機能を
持たない場合は `{"unsupported": true, "reason": "..."}` を記録する
（fail-closed。黙って欠落させない）。

| フェーズ | 内容 |
| -------- | ---- |
| `ingest_bulk` | 一括投入（COPY 相当。self は wire に COPY が無いため常に unsupported） |
| `ingest_single_stmt` | 単文 INSERT ループでの投入（self は行形 INSERT + `USING OPERATION_ID`） |
| `vector_knn` | フィルタなし KNN（Top-10） |
| `vector_knn_where` / `point_where` | `lang = 'ja'` フィルタ付き KNN（同形のため統合） |
| `where_compound_count` | 複合 WHERE の COUNT |
| `agg_count` | `COUNT(*)` |
| `agg_multi` | `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` |
| `group_by_having` | `GROUP BY`/`HAVING` |
| `hybrid_rrf` | ベクトル + 全文検索の RRF 融合 |
| `mode_recall` / `mode_precision` | 取得モード切替（self のみ） |
| `rls_isolation` | tenant-b 接続（self）／同一 public-only フィルタの再実行（他 DB）での `COUNT(*)`（RLS 相当の境界確認） |
| `udf_call` | 宣言的 UDF 呼び出し（self のみ） |
| `explain` | `EXPLAIN` |
| `recall_at_10` | `docs25k.jsonl` の public 行から numpy で計算した内積正解との突き合わせ（同点許容。下記「Recall@10 の算出方法」参照） |
| `bulk_knn_k200` / `bulk_knn_k1000` | 広域取得: フィルタなし Top-200／Top-1000 を `id, body` 込みで返す（LLM へ丸ごと渡す用途。ANN 構成は候補幅を max(64, k) へ引き上げて計測し `ef_search`／`hnsw_ef`／`ef` を記録） |
| `bulk_knn_where_k200` | 広域取得: `lang = 'ja'` フィルタ付き Top-200（`id, body`） |
| `bulk_hybrid_k200` | 広域取得: hybrid RRF の Top-200（`id, body`。pgvector・sqlite-vec は候補プールを 200 へ拡大し `candidate_pool` を記録） |
| `scan_where_nosort_k500` | ORDER BY なしのスカラーフィルタのみ LIMIT 500（`id, body`）。self の SQL 表層は行取得 SELECT に `ORDER BY <距離>` か `USING PLAN` を必須とするため実行して拒否を捕捉し unsupported を記録 |

## 公平性についての注記

- **接続経路の違い**: self・pgvector・MySQL は loopback TCP、sqlite-vec・
  LanceDB は in-process（sqlite-vec はファイルベース DB。`:memory:` ではない）、
  Qdrant は gRPC。経路の違い自体もレイテンシに寄与する
  ため、`meta.connection` に必ず記録し、横並び比較の際はこの違いを踏まえて
  読む。
- **exact / ANN の区別**: `--config exact` は索引なし全件探索、`--config hnsw`
  は各 DB の近似最近傍索引を使う。両者は精度・速度のトレードオフが異なるため
  同じ表で比較する際は必ず `config` を添えて読む。
- **既定設定のまま**: 各 DB は README に明記した索引パラメータ（pgvector
  m=16/ef_construction=100/ef_search=64、Qdrant 同様、LanceDB
  IVF_HNSW_FLAT m=16/ef_construction=100/ef=64）以外は既定値のまま変更していない
  （postgresql.conf のメモリ・WAL 設定、Qdrant の WAL・optimizer 設定等は
  デフォルト）。
- **fsync 等の永続化設定は未変更**: 公平性のためではなく、単に「素の Docker
  イメージ・デフォルト設定」を比較対象にする方針のため。本番運用のチューニング
  差は本ベンチの対象外。
- **距離指標**: 自作 DB の `<=>` は内積（`crates/engine/src/kernel.rs`
  「スコアは内積」参照）。pgvector は `<#>`（負の内積）、Qdrant は
  `Distance.DOT`、LanceDB は `metric="dot"` に揃えている。sqlite-vec の
  vec0 は内積相当の `distance_metric` を持たない（実機確認: `l2`/`l1`/`cosine`
  のみ受理）ため、コサイン距離で近似し `meta` に注記する。
- **RLS 相当（可視性モデル）**: 実機確認済みの自作 DB の現行契約は「**どの
  テナントの wire セッションからも `visibility = 'public'` の行のみが可視**」
  であり、private 行は所有テナント自身のセッションからも不可視（`docs25k.jsonl`
  の `visibility` フィールド: tenant-a 行 = public・tenant-b 行 = private。
  tenant-a・tenant-b いずれの wire セッションで `SELECT COUNT(*) FROM docs`
  しても public 行数〔25,000 行構成で 23,000〕に一致することを実機で確認済み）。
  他 DB は本物の RLS を持たないため、`visibility` 列（Qdrant は payload、
  LanceDB は `where` 句）を追加したうえで毎クエリに `WHERE visibility =
  'public'` を付ける素朴な模倣であり、`tenant` 列そのものでは絞り込まない。
  ポリシーエンジンとしての比較ではない。self 以外の DB にはテナント別
  セッションの概念が無いため、`rls_isolation` フェーズは同じ public-only
  フィルタを再実行して `agg_count` と同値になることの一致確認として扱う。

## Recall@10 の算出方法

正解集合は `visibility == "public"` な行のみから、200 クエリそれぞれについて
numpy で内積を計算して作る（`recall.py::build_ground_truth`）。

合成 embedding は疎で単純な値（0・±0.2・±0.4 等）の組み合わせが多く、内積が
完全に一致する文書グループができやすい。そのため「厳密な上位 10 件」を
決定的な順序（例: id 昇順）で切ると、その境界がどの 10 件を含むかは同点内の
順序規約に依存してしまい、exact 構成（索引なし全件探索）で比較しても DB 間
で完全一致しない——同じ意味で「正しい」10 件の組み合わせが複数存在するため。

そこで `recall_at_10`（主指標）は正解集合を「真の内積スコアが 10 位のスコア
以上の全文書」（同点を全て含む）へ広げ、DB の返却 10 件のうちこの集合に
含まれる件数 / 10 を返す（`recall_at_k_tie_tolerant`）。同点判定には
`TIE_EPSILON = 1e-4` の許容誤差を設けている——数学的には同一の内積値でも、
DB ごとに float32 の総和順序が異なると計算結果が最終桁でずれるため（実機
確認: pgvector と numpy で同一の意味的な同点グループのうち 1 件だけ
`0.4850712716579437` と `0.48507124185562134` のように 1e-7 台で食い違う
ケースを確認）。この許容誤差の導入により、self・pgvector(exact)・sqlite-vec
いずれも exact 構成で `recall_at_10 == 1.0` になることをスモークテストで
確認済み。

従来の厳密一致版（同点は決定的だが恣意的な順序で 10 件に切ったうえで完全一致
を要求する）は `recall_at_10_strict` として引き続き記録する。exact 構成でも
同点境界の順序差により 1.0 未満になり得る（sqlite-vec は距離指標がコサイン
近似のため、同点構造自体が自作 DB〔内積〕と異なりさらに低くなり得る）。

## 既知の unsupported 項目（スモークテストで確認済み）

- **MySQL**: `vector_knn`・`vector_knn_where`・`point_where`・`hybrid_rrf`。
  `VECTOR(128)` 列自体は作成でき `STRING_TO_VECTOR()`/`VECTOR_TO_STRING()` で
  入出力できるが、KNN 用の `DISTANCE()` 関数が存在しない
  （実機確認: `ERROR 1305 (42000): FUNCTION bench.DISTANCE does not exist`）。
  `CREATE VECTOR INDEX ...` も構文エラー
  （実機確認: `ERROR 1064 (42000)`）。
- **sqlite-vec**: `distance_metric=dot`/`ip`/`inner_product` はいずれも
  vec0 constructor error（実機確認）のため `cosine` で近似。vec0 の
  auxiliary 列（`+col`）は KNN の WHERE 句に使えない
  （実機確認: `An illegal WHERE constraint was provided on a vec0 auxiliary
  column in a KNN query.`）ため、`visibility` 列は `partition key` として
  宣言する（`tenant`・`lang` は素の列）。
- **Qdrant**: `agg_multi`・`group_by_having`（SUM/AVG/MIN/MAX・GROUP BY 相当の
  API が無い）・`hybrid_rrf`（疎ベクトルを自前生成すると自作 DB の BM25 系
  hybrid と非等価）・`mode_recall`/`mode_precision`・`udf_call`・`explain`。
- **pgvector・LanceDB**: `mode_recall`/`mode_precision`・`udf_call`
  （取得モード切替・宣言的 UDF 呼び出しの概念自体が無い）。
- **self**: `ingest_bulk`（wire プロトコルに COPY 相当が無く、SQL 表層は
  単文 INSERT のみ受理する。`EngineCore::execute_insert_sql_batch` は
  Rust API であり wire 未露出）。

## ファイル構成

| ファイル | 役割 |
| -------- | ---- |
| `common.py` | フィクスチャ読み込み・レイテンシ統計・meta 構築・計測ループの共通部品 |
| `recall.py` | numpy による内積正解集合の計算（同点許容込み）・Recall@10 突き合わせ |
| `self_db.py` | self（wire-server 経由）の全フェーズ実装 |
| `pgvector_db.py` | pgvector の全フェーズ実装 |
| `sqlite_vec_db.py` | sqlite-vec の全フェーズ実装 |
| `qdrant_db.py` | Qdrant の全フェーズ実装 |
| `lancedb_db.py` | LanceDB の全フェーズ実装 |
| `mysql_db.py` | MySQL の全フェーズ実装 |
| `run.py` | CLI エントリポイント（フィクスチャ読み込み → 各 db モジュール呼び出し → recall 計算 → JSON 書き出し） |
| `containers.sh` | pgvector・Qdrant・MySQL コンテナの起動・停止（`docker rm -f` で毎回冪等に作り直す） |
