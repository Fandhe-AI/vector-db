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
import re
import subprocess
import sys
import time
from typing import Any

import numpy as np
from qdrant_client import QdrantClient, models

# 親ディレクトリ（scripts/crossdb_bench）の common.py から env_port を借りる。
# 本スクリプトは単体実行（`python scripts/crossdb_bench/gpu/...py`）される前提で
# パッケージ化していないため、sys.path へ親を足してから import する。
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
from common import env_port  # noqa: E402

HOST = "127.0.0.1"
# 既定値 17333/17334 は containers_gpu.sh の `${CROSSDB_QDRANT_HTTP_PORT:-17333}`／
# `${CROSSDB_QDRANT_GRPC_PORT:-17334}` と一致させること。変数名は CPU 用
# qdrant_db.py と共通だが既定値は異なる（CPU 用は 16333/16334）。
HTTP_PORT = env_port("CROSSDB_QDRANT_HTTP_PORT", 17333)
GRPC_PORT = env_port("CROSSDB_QDRANT_GRPC_PORT", 17334)
COLLECTION = "gpu_build_bench"

# CPU 版・GPU 版のイメージタグ（containers_gpu.sh の QDRANT_VERSION と同じ値を
# 既定にする。相互に独立したタグ解決による版ずれを防ぐための固定運用——
# codex-review P2 指摘）。
QDRANT_VERSION = os.environ.get("QDRANT_VERSION", "v1.19.1")
QDRANT_IMAGE_CPU = f"qdrant/qdrant:{QDRANT_VERSION}"
QDRANT_IMAGE_GPU = f"qdrant/qdrant:{QDRANT_VERSION}-gpu-nvidia"
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


def wait_indexed(client: QdrantClient, rows: int, t0: float) -> dict[str, Any]:
    """`t0`（`enable_indexing` 送信直前の時刻）から green かつ全件 indexed までを計時する。

    開始要求の同期応答中に進んだ構築を含めるため、計時開始は呼び出し側が
    `enable_indexing` の直前に取る（codex-review 指摘）。
    """
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


def _docker_image_inspect(image: str, go_template: str) -> str:
    """`docker image inspect <image> --format <go_template>` の標準出力（改行除去）を返す。

    ローカルに未取得のイメージ・docker 未導入環境では `RuntimeError` を送出する
    （バージョン検証は fail-closed。取得できないまま検証をスキップして CPU/GPU の
    版ずれを見逃さない——codex-review P2 指摘）。
    """
    try:
        out = subprocess.run(
            ["docker", "image", "inspect", image, "--format", go_template],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError(f"docker image inspect failed for {image!r}: {exc}") from exc
    if out.returncode != 0 or not out.stdout.strip():
        raise RuntimeError(
            f"docker image inspect failed for {image!r} (rc={out.returncode}): {out.stderr.strip()}"
        )
    return out.stdout.strip()


def image_version_and_digest(image: str) -> dict[str, str]:
    """イメージタグから OCI ラベルのバージョンと RepoDigest を取得する。"""
    version = _docker_image_inspect(
        image, '{{index .Config.Labels "org.opencontainers.image.version"}}'
    )
    digest = _docker_image_inspect(image, "{{index .RepoDigests 0}}")
    return {"image": image, "version": version, "digest": digest}


def _normalize_version(v: str) -> str:
    """`v1.19.1` 形式のタグ由来バージョンと `1.19.1` 形式のサーバー応答バージョンを
    比較できるよう先頭の `v` を取り除く。"""
    return re.sub(r"^v", "", v.strip())


def verify_cpu_gpu_versions(client: QdrantClient) -> dict[str, Any]:
    """CPU 版・GPU 版イメージのバージョンが一致し、かつ現在接続中のサーバーの
    実バージョン（`client.info().version`）ともに一致することを確認する。

    どちらか一方でも取得できない・一致しない場合は `RuntimeError` で fail-closed
    にする（サーバーバージョンを記録しないまま CPU/GPU 比較結果を成功扱いに
    しない——codex-review P2 指摘）。結果 JSON の meta へ両バージョン・digest を
    記録するため、検証結果をそのまま返す。
    """
    cpu_image_info = image_version_and_digest(QDRANT_IMAGE_CPU)
    gpu_image_info = image_version_and_digest(QDRANT_IMAGE_GPU)
    cpu_version = _normalize_version(cpu_image_info["version"])
    gpu_version = _normalize_version(gpu_image_info["version"])
    if cpu_version != gpu_version:
        raise RuntimeError(
            "Qdrant CPU/GPU image version mismatch: "
            f"cpu={cpu_image_info!r} gpu={gpu_image_info!r}"
        )

    server_version = _normalize_version(client.info().version)
    if server_version != cpu_version:
        raise RuntimeError(
            "Running Qdrant server version does not match pinned CPU/GPU image version: "
            f"server={server_version!r} pinned={cpu_version!r} "
            f"(cpu_image={cpu_image_info!r}, gpu_image={gpu_image_info!r})"
        )

    return {
        "server_version": server_version,
        "cpu_image": cpu_image_info,
        "gpu_image": gpu_image_info,
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
    t_index0 = time.perf_counter()
    enable_indexing(client)
    index_result = wait_indexed(client, rows, t_index0)
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

    # 計測前に CPU/GPU 両イメージのバージョン一致・現在接続中サーバーとの一致を
    # 検証する（不一致・取得失敗は RuntimeError で fail-closed。codex-review P2
    # 指摘。containers_gpu.sh 側の QDRANT_VERSION 固定と対になる検証）。
    version_check = verify_cpu_gpu_versions(client)
    print(f"[qdrant_gpu_build_bench] version_check={version_check}", file=sys.stderr)

    meta: dict[str, Any] = {
        "generated_at_unix": time.time(),
        "label": args.label,
        "host_port_http": HTTP_PORT,
        "host_port_grpc": GRPC_PORT,
        "seed": SEED,
        "n_queries": N_QUERIES,
        "qdrant_server_version": version_check["server_version"],
        "qdrant_cpu_image": version_check["cpu_image"],
        "qdrant_gpu_image": version_check["gpu_image"],
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
