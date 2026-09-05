"""Qdrant の HNSW 索引構築を CPU 版／GPU 版（`QDRANT__GPU__INDEXING=1`）で比較する。

RTX 3060 上の GPU 高速化を他実装と横並びに比較するための crossdb_bench/gpu
配下スクリプトの一つ。docs/spec は参照しない。ホスト venv（qdrant-client・numpy）
で実行する前提で、対象コンテナは `containers_gpu.sh up|down qdrant_cpu|qdrant_gpu`
で個別に用意する（本スクリプト自体はコンテナを起動・停止しない）。

計測対象は「索引構築」のみで、検索（query_points）は CPU 実行のはずなので
CPU 版・GPU 版で差が無いことの確認用に付随計測する（本命の比較指標ではない）。

手順:
  1. 決定的合成データ（seed 固定・一様乱数 f32）を `rows` 件生成
  2. hnsw_config（m=16, ef_construct=100）・optimizers_config（indexing_threshold
     を行数より大きくし、投入中は索引構築を走らせない）で collection を作成
  3. batch=1,000・wait=True で upsert（この所要時間を「upsert 時間」として記録）
  4. indexing_threshold を 1 へ更新して索引構築を開始し、その時刻を起点に
     collection status が green かつ
     indexed_vectors_count == rows になるまで 0.5 秒間隔でポーリング
     （この所要時間を「索引構築時間」として記録）
  5. 決定的クエリ 200 本で query_points（hnsw_ef=64, limit=10）の p50/p95 を計測
  6. `docker logs` から GPU 認識ログ（"Found GPU device" / "Create GPU device"）
     を拾い meta へ記録（GPU 版で見つからなければ fail-closed で警告を記録する）
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from typing import Any

import numpy as np
from qdrant_client import QdrantClient, models

HOST = "127.0.0.1"
HTTP_PORT = 17333
GRPC_PORT = 17334
COLLECTION = "gpu_build_bench"
# 投入中の索引構築を止めるための閾値（KB 単位。既定グリッドの最大行数×dim×4 バイトを
# 十分上回る値）。
INDEXING_DISABLED_THRESHOLD = 10_000_000_000

DEFAULT_ROWS_GRID = (100_000, 500_000)
DEFAULT_DIM = 128
N_QUERIES = 200
SEED = 20260905

POLL_INTERVAL_SEC = 0.5
POLL_TIMEOUT_SEC = 3600  # 500k 件構築が想定より長引いても打ち切らない上限（1時間）。


def gen_data(rows: int, dim: int, seed: int) -> tuple[np.ndarray, np.ndarray]:
    rng = np.random.default_rng(seed)
    corpus = rng.uniform(-1.0, 1.0, size=(rows, dim)).astype(np.float32)
    queries = rng.uniform(-1.0, 1.0, size=(N_QUERIES, dim)).astype(np.float32)
    return corpus, queries


def percentile(sorted_values: list[float], p: float) -> float:
    if not sorted_values:
        return float("nan")
    n = len(sorted_values)
    idx = max(0, min(n - 1, int(round(p / 100.0 * (n - 1)))))
    return sorted_values[idx]


def connect() -> QdrantClient:
    return QdrantClient(host=HOST, port=HTTP_PORT, grpc_port=GRPC_PORT, prefer_grpc=True, timeout=120)


def setup_collection(client: QdrantClient, dim: int) -> None:
    if client.collection_exists(COLLECTION):
        client.delete_collection(COLLECTION)
    client.create_collection(
        collection_name=COLLECTION,
        vectors_config=models.VectorParams(size=dim, distance=models.Distance.DOT),
        hnsw_config=models.HnswConfigDiff(m=16, ef_construct=100),
        # 投入中は索引構築を走らせない（indexing_threshold を行数より十分大きく取る）。
        # 投入完了後に `enable_indexing` で閾値を下げて構築を開始し、その時点から
        # green かつ全件 indexed までを「索引構築時間」として計時する。投入中から
        # 構築を進めると、投入速度の差で CPU/GPU の構築時間比較が歪む。
        optimizers_config=models.OptimizersConfigDiff(indexing_threshold=INDEXING_DISABLED_THRESHOLD),
    )


def enable_indexing(client: QdrantClient) -> None:
    """投入完了後に indexing_threshold を下げ、索引構築（最適化）を開始させる。"""
    client.update_collection(
        collection_name=COLLECTION,
        optimizers_config=models.OptimizersConfigDiff(indexing_threshold=1),
    )


def upsert_all(client: QdrantClient, corpus: np.ndarray, batch: int = 1_000) -> dict[str, Any]:
    rows = corpus.shape[0]
    t0 = time.perf_counter()
    for i in range(0, rows, batch):
        chunk = corpus[i : i + batch]
        ids = list(range(i, i + chunk.shape[0]))
        client.upsert(
            collection_name=COLLECTION,
            points=models.Batch(ids=ids, vectors=chunk.tolist(), payloads=[{} for _ in ids]),
            wait=True,
        )
    elapsed = time.perf_counter() - t0
    return {"rows": rows, "seconds": elapsed, "rows_per_sec": rows / elapsed if elapsed > 0 else None}


def wait_indexed(client: QdrantClient, rows: int) -> dict[str, Any]:
    t0 = time.perf_counter()
    last_info = None
    while True:
        info = client.get_collection(COLLECTION)
        last_info = info
        status_ok = str(info.status) == "green"
        indexed_ok = (info.indexed_vectors_count or 0) >= rows
        if status_ok and indexed_ok:
            break
        elapsed = time.perf_counter() - t0
        if elapsed > POLL_TIMEOUT_SEC:
            raise TimeoutError(
                f"index build timed out after {elapsed:.1f}s "
                f"(status={info.status}, indexed={info.indexed_vectors_count}/{rows})"
            )
        time.sleep(POLL_INTERVAL_SEC)
    elapsed = time.perf_counter() - t0
    return {
        "seconds": elapsed,
        "final_status": str(last_info.status),
        "indexed_vectors_count": last_info.indexed_vectors_count,
    }


def measure_search(client: QdrantClient, queries: np.ndarray) -> dict[str, Any]:
    latencies_us = []
    for q in queries:
        t0 = time.perf_counter()
        client.query_points(
            collection_name=COLLECTION,
            query=q.tolist(),
            limit=10,
            search_params=models.SearchParams(hnsw_ef=64),
        )
        t1 = time.perf_counter()
        latencies_us.append((t1 - t0) * 1_000_000.0)
    arr = sorted(latencies_us)
    return {
        "n": len(arr),
        "p50_us": percentile(arr, 50),
        "p95_us": percentile(arr, 95),
        "mean_us": sum(arr) / len(arr),
    }


def gpu_log_signal(container_name: str) -> dict[str, Any]:
    """`docker logs <container_name>` から GPU 索引構築の認識ログを探す。

    見つからない場合は CLAUDE.md の fail-closed 方針に沿い「GPU 未使用」を
    明示的に記録する（黙って CPU 相当の結果を GPU 実測として扱わない）。
    """
    try:
        out = subprocess.run(
            ["docker", "logs", container_name], capture_output=True, text=True, timeout=10, check=False
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        return {"container": container_name, "checked": False, "error": str(exc)}

    logs = (out.stdout or "") + (out.stderr or "")
    lower = logs.lower()
    found_gpu = "found gpu device" in lower
    create_gpu = "create gpu device" in lower
    if found_gpu or create_gpu:
        return {
            "container": container_name,
            "checked": True,
            "gpu_used": True,
            "found_gpu_device_line": found_gpu,
            "create_gpu_device_line": create_gpu,
        }
    return {
        "container": container_name,
        "checked": True,
        "gpu_used": False,
        "warning": "GPU 未使用（'Found GPU device'/'Create GPU device' ログなし。fail-closed 記録）",
    }


def run_one(client: QdrantClient, rows: int, dim: int, container_name: str | None) -> dict[str, Any]:
    corpus, queries = gen_data(rows, dim, seed=SEED + rows + dim)

    setup_collection(client, dim)
    upsert_result = upsert_all(client, corpus)
    enable_indexing(client)
    index_result = wait_indexed(client, rows)
    search_result = measure_search(client, queries)

    entry: dict[str, Any] = {
        "rows": rows,
        "dim": dim,
        "upsert": upsert_result,
        "index_build": index_result,
        "search": search_result,
    }
    if container_name:
        entry["gpu_log_signal"] = gpu_log_signal(container_name)
    return entry


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, help="結果 JSON の出力先パス")
    parser.add_argument("--rows", type=int, nargs="*", default=list(DEFAULT_ROWS_GRID))
    parser.add_argument("--dim", type=int, default=DEFAULT_DIM)
    parser.add_argument(
        "--container-name",
        default=None,
        help="GPU 認識ログを確認する docker コンテナ名（例: bench-qdrant-gpu）。"
        "未指定なら gpu_log_signal を記録しない。",
    )
    parser.add_argument(
        "--label",
        default="unknown",
        help="結果 JSON の meta に残す構成ラベル（例: cpu / gpu）。集計時の区別用。",
    )
    args = parser.parse_args()

    client = connect()

    meta: dict[str, Any] = {
        "generated_at_unix": time.time(),
        "label": args.label,
        "host_port_http": HTTP_PORT,
        "host_port_grpc": GRPC_PORT,
        "seed": SEED,
        "n_queries": N_QUERIES,
    }
    print(f"[qdrant_gpu_build_bench] meta={meta}", file=sys.stderr)

    results = []
    for rows in args.rows:
        print(f"[qdrant_gpu_build_bench] running rows={rows} dim={args.dim} ...", file=sys.stderr)
        entry = run_one(client, rows, args.dim, args.container_name)
        print(
            f"  rows={rows}: upsert={entry['upsert']['seconds']:.1f}s "
            f"index_build={entry['index_build']['seconds']:.1f}s "
            f"search_p50={entry['search']['p50_us']:.1f}us",
            file=sys.stderr,
        )
        results.append(entry)

    out = {"meta": meta, "results": results}
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=2)
    print(f"[qdrant_gpu_build_bench] wrote {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
