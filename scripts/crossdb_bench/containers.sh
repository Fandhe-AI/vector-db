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

  # 二段判定（実機確認済みの事象への対処）: MySQL の公式イメージは初期化時、
  # 「初期化スクリプト実行用の一時サーバーを起動 → 実行 → 一時サーバーを停止
  # → 本番サーバーを起動」という手順を踏み、`docker logs` に
  # "ready for connections" が計 2 回出力される（1 回目が一時サーバー、
  # 2 回目が port 3306 で listen する本番サーバー）。1 回目だけを見て
  # ready と判定すると、直後の接続が一時サーバー停止のタイミングと競合し
  # `2013 Lost connection ... reading initial communication packet` で
  # 失敗する（25k 本計測 `mysql exact` で実機確認）。そのため 1 段目で
  # ログに 2 回出るまで待ち、2 段目で `mysqladmin ping` が 3 秒間隔で
  # 2 回連続成功するまで待つ（起動直後の瞬間的な接続断の再発防止）。
  ready_count=0
  for _ in $(seq 1 90); do
    ready_count=$(docker logs bench-mysql 2>&1 | grep -c "ready for connections" || true)
    if [ "${ready_count:-0}" -ge 2 ]; then
      break
    fi
    sleep 1
  done
  if [ "${ready_count:-0}" -lt 2 ]; then
    echo "bench-mysql: timed out waiting for 'ready for connections' x2 in logs" >&2
    exit 1
  fi

  ok_count=0
  for _ in $(seq 1 60); do
    if docker exec bench-mysql mysqladmin ping -uroot -pbench --silent >/dev/null 2>&1; then
      ok_count=$((ok_count + 1))
      if [ "$ok_count" -ge 2 ]; then
        echo "bench-mysql: ready"
        return 0
      fi
    else
      ok_count=0
    fi
    sleep 3
  done
  echo "bench-mysql: timed out waiting for 2 consecutive successful mysqladmin ping" >&2
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
