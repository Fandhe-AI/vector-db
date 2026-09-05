#!/usr/bin/env bash
# crossdb_bench/gpu: Qdrant CPU 版・GPU 版コンテナの起動・停止。
#
# scripts/crossdb_bench/containers.sh（既存・CPU の qdrant/pgvector 用。他 agent が
# 編集中のため触らない）とはポート・コンテナ名を分離している。qdrant_gpu_build_bench.py
# が「索引構築を GPU で行う版」と「CPU 版」を同一ホストで並行比較できるように、
# CPU 版はポート 17333/17334（既定）、GPU 版は別コンテナ名で同じく 17333/17334 を使う
# （同時起動はしない前提。両方同時に立てたい場合はポートを分ける運用に変更すること）。
# ポートは環境変数 CROSSDB_QDRANT_HTTP_PORT／CROSSDB_QDRANT_GRPC_PORT で上書きでき、
# 接続側 qdrant_gpu_build_bench.py（`common.env_port`）が同じ変数名・同じ既定値
# （17333/17334）を読む。変数名は CPU 用 containers.sh と共通だが既定値は異なる
# （CPU 用は 16333/16334）ので、両ハーネスを同じ環境で使う場合は注意すること。
#
# GPU 版は `qdrant/qdrant:gpu-nvidia-latest` を `--gpus all` + `QDRANT__GPU__INDEXING=1`
# で起動する。起動ログの "Found GPU device" 行で実際に GPU が認識されたかを
# 呼び出し側（qdrant_gpu_build_bench.py）が確認する契約。
#
# `docker rm -f` で毎回冪等に扱う（既存コンテナが残っていても事故らない）。
# 計測中は対象コンテナ以外を止めること（CPU・メモリの競合を避けるため）。
set -euo pipefail

# 既定値 17333/17334 は qdrant_gpu_build_bench.py の env_port の default と一致させること。
HOST_PORT_HTTP="${CROSSDB_QDRANT_HTTP_PORT:-17333}"
HOST_PORT_GRPC="${CROSSDB_QDRANT_GRPC_PORT:-17334}"

usage() {
  echo "usage: $0 up|down <qdrant_cpu|qdrant_gpu>" >&2
  exit 1
}

wait_ready() {
  local name="$1"
  echo "${name}: waiting for readiness..."
  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:${HOST_PORT_HTTP}/readyz" >/dev/null 2>&1; then
      echo "${name}: ready"
      return 0
    fi
    sleep 1
  done
  echo "${name}: timed out waiting for readiness" >&2
  docker logs "${name}" 2>&1 | tail -n 50 >&2 || true
  exit 1
}

up_cpu() {
  docker rm -f bench-qdrant-cpu-gpucmp >/dev/null 2>&1 || true
  docker run -d --name bench-qdrant-cpu-gpucmp \
    -p 127.0.0.1:${HOST_PORT_HTTP}:6333 \
    -p 127.0.0.1:${HOST_PORT_GRPC}:6334 \
    qdrant/qdrant:latest >/dev/null
  wait_ready bench-qdrant-cpu-gpucmp
}

up_gpu() {
  docker rm -f bench-qdrant-gpu >/dev/null 2>&1 || true
  docker run -d --name bench-qdrant-gpu --gpus all \
    -e QDRANT__GPU__INDEXING=1 \
    -p 127.0.0.1:${HOST_PORT_HTTP}:6333 \
    -p 127.0.0.1:${HOST_PORT_GRPC}:6334 \
    qdrant/qdrant:gpu-nvidia-latest >/dev/null
  wait_ready bench-qdrant-gpu
  if docker logs bench-qdrant-gpu 2>&1 | grep -qi "found gpu device"; then
    echo "bench-qdrant-gpu: GPU device recognized (Found GPU device)"
  else
    echo "bench-qdrant-gpu: WARNING - 'Found GPU device' not seen in logs (fail-closed: GPU 未使用の可能性)" >&2
  fi
}

cmd="${1:-}"
db="${2:-}"
[ -n "$cmd" ] && [ -n "$db" ] || usage

case "$db" in
  qdrant_cpu)
    case "$cmd" in
      up) up_cpu ;;
      down) docker rm -f bench-qdrant-cpu-gpucmp >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  qdrant_gpu)
    case "$cmd" in
      up) up_gpu ;;
      down) docker rm -f bench-qdrant-gpu >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  *)
    usage
    ;;
esac
