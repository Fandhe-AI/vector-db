#!/usr/bin/env bash
# crossdb_bench: 対照 DB（pgvector・Qdrant・MySQL）の Docker コンテナ起動・停止。
#
# ポートは scripts/crossdb_bench/README.md 記載の固定値（他コンテナと衝突しないよう
# 127.0.0.1 の非既定ポートへ bind する）。`docker rm -f` で毎回冪等に扱う
# （既存コンテナが残っていても事故らない）。
#
# 計測中は対象 DB 以外のコンテナを止めること（CLAUDE.md の指示どおり、公平な
# 計測のため他コンテナと競合させない）。
set -euo pipefail

usage() {
  echo "usage: $0 up|down <pgvector|qdrant|mysql>" >&2
  exit 1
}

cmd="${1:-}"
db="${2:-}"
[ -n "$cmd" ] && [ -n "$db" ] || usage

up_pgvector() {
  docker rm -f bench-pgvector >/dev/null 2>&1 || true
  docker run -d --name bench-pgvector \
    -e POSTGRES_PASSWORD=bench \
    -e POSTGRES_DB=bench \
    -p 127.0.0.1:15433:5432 \
    pgvector/pgvector:pg17 >/dev/null
  echo "bench-pgvector: waiting for readiness..."
  for _ in $(seq 1 60); do
    if docker exec bench-pgvector pg_isready -U postgres >/dev/null 2>&1; then
      echo "bench-pgvector: ready"
      return 0
    fi
    sleep 1
  done
  echo "bench-pgvector: timed out waiting for readiness" >&2
  exit 1
}

up_qdrant() {
  docker rm -f bench-qdrant >/dev/null 2>&1 || true
  docker run -d --name bench-qdrant \
    -p 127.0.0.1:16333:6333 \
    -p 127.0.0.1:16334:6334 \
    qdrant/qdrant:latest >/dev/null
  echo "bench-qdrant: waiting for readiness..."
  for _ in $(seq 1 60); do
    if curl -sf http://127.0.0.1:16333/readyz >/dev/null 2>&1; then
      echo "bench-qdrant: ready"
      return 0
    fi
    sleep 1
  done
  echo "bench-qdrant: timed out waiting for readiness" >&2
  exit 1
}

up_mysql() {
  docker rm -f bench-mysql >/dev/null 2>&1 || true
  docker run -d --name bench-mysql \
    -e MYSQL_ROOT_PASSWORD=bench \
    -e MYSQL_DATABASE=bench \
    -p 127.0.0.1:33306:3306 \
    mysql:9 >/dev/null
  echo "bench-mysql: waiting for readiness..."
  for _ in $(seq 1 60); do
    if docker exec bench-mysql mysqladmin ping -uroot -pbench --silent >/dev/null 2>&1; then
      echo "bench-mysql: ready"
      return 0
    fi
    sleep 1
  done
  echo "bench-mysql: timed out waiting for readiness" >&2
  exit 1
}

case "$db" in
  pgvector)
    case "$cmd" in
      up) up_pgvector ;;
      down) docker rm -f bench-pgvector >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  qdrant)
    case "$cmd" in
      up) up_qdrant ;;
      down) docker rm -f bench-qdrant >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  mysql)
    case "$cmd" in
      up) up_mysql ;;
      down) docker rm -f bench-mysql >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  *)
    usage
    ;;
esac
