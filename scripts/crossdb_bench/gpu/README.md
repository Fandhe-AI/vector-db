# crossdb_bench/gpu

RTX 3060（driver 595.71.05・CUDA 13.2）上での GPU 高速化を他実装（FAISS・Qdrant）と
横並びに比較するためのスクリプト群。`scripts/crossdb_bench/`（CPU 系機能別ベンチ。
他 agent が編集中のため本ディレクトリからは触らない）とは独立に動く。

private spec（`docs/spec`）の内容は参照しない。Cargo 依存は増やしていない
（本ディレクトリは Python／Docker のみで完結する）。

## 前提

- Docker（`docker run --gpus all` 可能。NVIDIA Container Toolkit 導入済み）
- イメージ
  - `bench-faiss-gpu`（`Dockerfile.faiss` からビルド。python:3.12-slim +
    `faiss-gpu-cu12==1.14.1.post1` + numpy）
  - `qdrant/qdrant:v1.19.1`（CPU 版）
  - `qdrant/qdrant:v1.19.1-gpu-nvidia`（GPU 版・索引構築のみ GPU 利用）

    CPU 版・GPU 版は `latest`／`gpu-nvidia-latest` のように独立にタグが動くと
    異なる Qdrant バージョンを比較してしまう事故が起きるため、同一リリース
    番号へ固定している（`containers_gpu.sh` の `QDRANT_VERSION` 変数・環境変数
    `QDRANT_VERSION` で上書き可。両者を変える場合は必ず両方揃えて更新すること。
    codex-review P2 指摘）。`qdrant_gpu_build_bench.py` は計測前に両イメージの
    バージョンラベル・接続中サーバーの実バージョン（`client.info().version`）が
    一致することを確認し、不一致・未取得（イメージ未 pull 等）は
    `RuntimeError` で fail-closed にする。結果 JSON の `meta.qdrant_server_version`・
    `meta.qdrant_cpu_image`・`meta.qdrant_gpu_image`（バージョン・digest）に
    実測値を記録する。
- ホスト venv（`qdrant-client`・numpy。`scripts/crossdb_bench/requirements.txt` の
  範囲で足りる。ホストの Python 3.14 には faiss-gpu の wheel が無いため FAISS 側は
  必ずコンテナ内で実行する）

```bash
docker build -t bench-faiss-gpu -f scripts/crossdb_bench/gpu/Dockerfile.faiss \
    scripts/crossdb_bench/gpu
# CPU/GPU 両方のバージョン検証（qdrant_gpu_build_bench.py）に必要なため、
# 実行前に両方のイメージを pull しておくこと。
docker pull qdrant/qdrant:v1.19.1
docker pull qdrant/qdrant:v1.19.1-gpu-nvidia
```

## 1. FAISS（CPU / GPU f32 / GPU f16）

`faiss_batch_bench.py` はコンテナ内で実行する（プロセス内で CPU・GPU f32・GPU f16 の
3 経路をすべて計測する）。

```bash
S=/path/to/scratch  # results/ を作る作業ディレクトリ
mkdir -p "$S/results"

docker run --rm --gpus all \
    -v "$S":/work \
    -v "$(pwd)/scripts/crossdb_bench/gpu":/scripts:ro \
    bench-faiss-gpu \
    python /scripts/faiss_batch_bench.py --out /work/results/faiss.json
```

規模格子は既定で `rows ∈ {20000, 100000, 500000}` × `dim ∈ {128, 256}` ×
`batch ∈ {1, 8, 64, 256}`・`k=10`（`crates/engine/benches/gpu_scaling_bench.rs`
と対応する規模。本ハーネス独自の既定値）。`--rows`／`--dims` で規模を上書きできる
（スモーク実行用）。

各構成 warmup 5 回・計測 20 回の `search` 所要時間（マイクロ秒）から
min/p50/p95/mean と 1 クエリあたり p50（`p50_us / batch`）を出す。GPU 側は
`add`（転送・索引構築）を計測区間の外に置き、`search`（GPU でも呼び出し元スレッドに
対して同期完了する）だけを計測する。

CPU 経路の OpenMP スレッド数は既定 12（`--omp-threads` で上書き可、
`faiss.omp_set_num_threads` を明示呼び出し）。結果 JSON の `meta.omp_num_threads`・
`meta.openblas_num_threads_env`・`meta.cpu_blas_threshold`・`meta.cpu_condition`
に実際の設定値を記録する。

`GpuIndexFlatConfig.useFloat16` が無いビルドでは GPU f16 の結果を文字列 `"N/A"`
として記録する。

### CPU 経路の BLAS/OMP スレッド競合

初回実測（rows=20000, dim=128, `OMP_NUM_THREADS=12` のみ）で batch=64/256 の
CPU 検索 p50 が 115ms 超まで悪化する事象を確認した。FAISS の CPU 検索は
`nq >= faiss.cvar.distance_compute_blas_threshold`（既定 20）で BLAS(sgemm) 経路へ
切り替わり、slim イメージの OpenBLAS スレッドが既定でコア数いっぱいに張られて
OMP スレッドと競合することが原因と推定し、3 条件（+ 組み合わせで計 4 条件）を
比較した（rows=20000, dim=128, k=10, cpu_p50_us）:

| 条件 | batch=1 | batch=8 | batch=64 | batch=256 |
| --- | ---: | ---: | ---: | ---: |
| A: 現状（`OPENBLAS_NUM_THREADS` 未指定・`blas_threshold`=20 既定） | 210.6 | 371.3 | 115971.1 | 115150.4 |
| B: `OPENBLAS_NUM_THREADS=1` のみ | 195.3 | 369.1 | 115976.6 | 116936.0 |
| C: `blas_threshold=1<<30`（BLAS 経路無効化）のみ | 192.1 | 467.6 | 8987.2 | 14970.7 |
| D: B + C の組み合わせ（採用） | 195.7 | 369.1 | 8988.0 | 16969.4 |

`OPENBLAS_NUM_THREADS=1` 単独（B）では改善せず、`distance_compute_blas_threshold`
を上げて BLAS 経路自体を無効化する（C）ことで batch>=64 が劇的に改善する一方、
C 単独では batch=8 がやや悪化する（371us→468us）。B と C を組み合わせた D が
batch=1/8 を A 相当に保ったまま batch=64/256 も 9-17ms 程度に抑えられる最速条件
だったため、これを既定として採用した:

- `Dockerfile.faiss` に `ENV OMP_NUM_THREADS=12`・`ENV OPENBLAS_NUM_THREADS=1` を
  焼き込み（`docker run -e ...` で上書き可能）
- `faiss_batch_bench.py --blas-threshold` の既定値を `1<<30`
  （`FAISS_BLAS_THRESHOLD` 環境変数でも上書き可）

FAISS 既定の BLAS 経路をそのまま計測したい場合は `--blas-threshold 20` を渡す。
結果 JSON の `meta.cpu_condition`（`"blas_disabled"` / `"blas_default_threshold_20"` /
それ以外の任意値は `"blas_custom_threshold_<n>"`）でどの条件が使われたかを確認できる
（既定 20 ちょうどの場合だけ既定ラベルになり、他の値は既定条件と区別される）。

## 2. Qdrant（索引構築 CPU vs GPU）

コンテナは `containers_gpu.sh` で個別に用意する（CPU 版・GPU 版は同じポート
17333/17334〔既定〕を使うため、同時に両方は起動しない）。ポートは環境変数
`CROSSDB_QDRANT_HTTP_PORT`／`CROSSDB_QDRANT_GRPC_PORT` で上書きでき、起動側
（`containers_gpu.sh`）と接続側（`qdrant_gpu_build_bench.py`）が同じ既定値
（17333/17334）を読む。変数名は `../containers.sh`・`../qdrant_db.py` と共通だが
そちらの既定値は 16333/16334 である点に注意。

```bash
# CPU 版
scripts/crossdb_bench/gpu/containers_gpu.sh up qdrant_cpu
python scripts/crossdb_bench/gpu/qdrant_gpu_build_bench.py \
    --out "$S/results/qdrant_cpu.json" --label cpu
scripts/crossdb_bench/gpu/containers_gpu.sh down qdrant_cpu

# GPU 版（QDRANT__GPU__INDEXING=1・--gpus all）
scripts/crossdb_bench/gpu/containers_gpu.sh up qdrant_gpu
python scripts/crossdb_bench/gpu/qdrant_gpu_build_bench.py \
    --out "$S/results/qdrant_gpu.json" --label gpu --container-name bench-qdrant-gpu
scripts/crossdb_bench/gpu/containers_gpu.sh down qdrant_gpu
```

`containers_gpu.sh up qdrant_gpu` は起動直後に `docker logs` を確認し、
`Create GPU device` 行が無ければ標準エラーへ警告を出す（fail-closed。`Found GPU device`
は列挙のみで CPU エミュレータも含むため初期化の証拠にせず、GPU が実際には
使われていないのに GPU 実測として扱わない）。
`qdrant_gpu_build_bench.py --container-name` 側でも同じログを結果 JSON の
`results[].gpu_log_signal` へ記録する。

規模は既定 `rows ∈ {100000, 500000}`・`dim=128`（`--rows`／`--dim` で上書き可）。
手順は upsert（batch 1,000・wait=True。所要時間を記録）→ collection status が
green かつ `indexed_vectors_count == rows` になるまで 0.5 秒間隔でポーリング
（この所要時間を索引構築時間として記録）→ 決定的クエリ 200 本で
`query_points`（hnsw_ef=64・limit=10）の p50/p95 を計測、の順。検索は
CPU 実行のはずなので CPU 版・GPU 版で `search` の値に有意差が無いことを
確認する用途（本命の比較指標は索引構築時間）。

## 公平性に関する注記

- FAISS はプロセス内 API（Python プロセスから直接呼ぶ）で転送・IPC のオーバーヘッドが
  無い。Rust 側（`crates/engine/benches/gpu_scaling_bench.rs`、wgpu/Vulkan バックエンド
  常駐 f16）・Qdrant（gRPC 経由の別プロセス）とは呼び出し経路の性質が異なるため、
  レイテンシの絶対値は「同じ土俵」ではなく参考値として扱うこと。
- Qdrant の GPU 利用は**索引構築のみ**（`QDRANT__GPU__INDEXING=1`）。検索
  （`query_points`）は CPU 実行であり、FAISS の GPU search・Rust 側の GPU 常駐検索とは
  比較対象が異なる。
- Rust 側は f16 常駐（`docs/design/core16-f16-resident-gate.md` 参照。ポインタ表記）を
  比較しており、本ディレクトリの FAISS f16 経路（`useFloat16` cloner オプション）とは
  実装が異なる（比較目的は「同じ GPU 上でどの実装がどの程度の速度を出せるか」の
  横並び把握であり、アルゴリズム・精度の同一性を保証するものではない）。
- CPU 経路はいずれも共有ホスト上での計測であり、他プロセスとの競合を避けるため
  計測中は対象コンテナ以外を止めること。

## 出力

各スクリプトは `--out` で指定した JSON ファイルへ `{"meta": {...}, "results": [...]}`
形式で書き出す。フル規模（rows=500000 等）の実行は本 agent では行わない
（スモーク実行のみ確認済み）。
