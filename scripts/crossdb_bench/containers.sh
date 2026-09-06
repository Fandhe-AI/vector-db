#!/usr/bin/env bash
# crossdb_bench: 対照 DB（pgvector・Qdrant・MySQL）の Docker コンテナ起動・停止。
#
# ポートは 127.0.0.1 の非既定ポートへ bind する（他コンテナと衝突しないため）。
# 環境変数で上書きでき、接続側の Python（pgvector_db.py・qdrant_db.py・mysql_db.py。
# `common.env_port`）が同じ変数名・同じ既定値を読む契約（README「環境変数」参照）。
# ここで `${VAR:-default}` に書く既定値は Python 側の `env_port(name, default)` の
# default と必ず一致させること（両側で別サーバーを見てしまう事故を防ぐ）。
# `docker rm -f` で毎回冪等に扱う（既存コンテナが残っていても事故らない）。
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

# 接続側 Python の既定値（pgvector_db.py 15433・qdrant_db.py 16333/16334・
# mysql_db.py 33306）と同じ値を既定にする。
PG_PORT="${CROSSDB_PG_PORT:-15433}"
QDRANT_HTTP_PORT="${CROSSDB_QDRANT_HTTP_PORT:-16333}"
QDRANT_GRPC_PORT="${CROSSDB_QDRANT_GRPC_PORT:-16334}"
MYSQL_PORT="${CROSSDB_MYSQL_PORT:-33306}"

# コンテナ名は既定で `bench-<db>` 固定だが、同一ホストで並行して計測する
# 別セッション（他エージェント・運用者）が同名コンテナを既に使っている場合、
# 冪等化のための `docker rm -f` がその別セッションの状態を破壊してしまう
# （Issue #466。実測時に外部 `bench-qdrant` を誤って削除しかけた事故の再発
# 防止）。環境変数で別名を指定できるようにし、Docker のコンテナ名文字集合
# （英数字で始まり英数字・`_`・`.`・`-` が続く 2 文字以上。Docker Engine API
# の命名規則）に適合しない値は fail-closed で拒否する（run_all.sh の
# `CROSSDB_DIM` 十進数字検査と同じ方針）。
validate_container_name() {
  local name="$1"
  local var="$2"
  if ! [[ "$name" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]+$ ]]; then
    echo "error: ${var}=${name} is not a valid Docker container name (expected: ^[a-zA-Z0-9][a-zA-Z0-9_.-]+\$)" >&2
    exit 1
  fi
}
PG_CONTAINER="${CROSSDB_PG_CONTAINER:-bench-pgvector}"
QDRANT_CONTAINER="${CROSSDB_QDRANT_CONTAINER:-bench-qdrant}"
MYSQL_CONTAINER="${CROSSDB_MYSQL_CONTAINER:-bench-mysql}"
validate_container_name "$PG_CONTAINER" "CROSSDB_PG_CONTAINER"
validate_container_name "$QDRANT_CONTAINER" "CROSSDB_QDRANT_CONTAINER"
validate_container_name "$MYSQL_CONTAINER" "CROSSDB_MYSQL_CONTAINER"

up_pgvector() {
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$PG_CONTAINER" \
    -e POSTGRES_PASSWORD=bench \
    -e POSTGRES_DB=bench \
    -p "127.0.0.1:${PG_PORT}:5432" \
    pgvector/pgvector:pg17 >/dev/null
  echo "${PG_CONTAINER}: waiting for readiness..."
  for _ in $(seq 1 60); do
    # -h 127.0.0.1 で TCP listener を確認する（Unix socket だけを見る既定形では
    # 初期化用の一時 PostgreSQL でも成功してしまい、直後の接続が拒否されうる）。
    if docker exec "$PG_CONTAINER" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; then
      echo "${PG_CONTAINER}: ready"
      return 0
    fi
    sleep 1
  done
  echo "${PG_CONTAINER}: timed out waiting for readiness" >&2
  exit 1
}

up_qdrant() {
  docker rm -f "$QDRANT_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$QDRANT_CONTAINER" \
    -p "127.0.0.1:${QDRANT_HTTP_PORT}:6333" \
    -p "127.0.0.1:${QDRANT_GRPC_PORT}:6334" \
    qdrant/qdrant:latest >/dev/null
  echo "${QDRANT_CONTAINER}: waiting for readiness..."
  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:${QDRANT_HTTP_PORT}/readyz" >/dev/null 2>&1; then
      echo "${QDRANT_CONTAINER}: ready"
      return 0
    fi
    sleep 1
  done
  echo "${QDRANT_CONTAINER}: timed out waiting for readiness" >&2
  exit 1
}

up_mysql() {
  docker rm -f "$MYSQL_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$MYSQL_CONTAINER" \
    -e MYSQL_ROOT_PASSWORD=bench \
    -e MYSQL_DATABASE=bench \
    -p "127.0.0.1:${MYSQL_PORT}:3306" \
    mysql:9 >/dev/null
  echo "${MYSQL_CONTAINER}: waiting for readiness..."

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
    ready_count=$(docker logs "$MYSQL_CONTAINER" 2>&1 | grep -c "ready for connections" || true)
    if [ "${ready_count:-0}" -ge 2 ]; then
      break
    fi
    sleep 1
  done
  if [ "${ready_count:-0}" -lt 2 ]; then
    echo "${MYSQL_CONTAINER}: timed out waiting for 'ready for connections' x2 in logs" >&2
    exit 1
  fi

  ok_count=0
  for _ in $(seq 1 60); do
    if docker exec "$MYSQL_CONTAINER" mysqladmin ping -uroot -pbench --silent >/dev/null 2>&1; then
      ok_count=$((ok_count + 1))
      if [ "$ok_count" -ge 2 ]; then
        echo "${MYSQL_CONTAINER}: ready"
        return 0
      fi
    else
      ok_count=0
    fi
    sleep 3
  done
  echo "${MYSQL_CONTAINER}: timed out waiting for 2 consecutive successful mysqladmin ping" >&2
  exit 1
}

case "$db" in
  pgvector)
    case "$cmd" in
      up) up_pgvector ;;
      down) docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  qdrant)
    case "$cmd" in
      up) up_qdrant ;;
      down) docker rm -f "$QDRANT_CONTAINER" >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  mysql)
    case "$cmd" in
      up) up_mysql ;;
      down) docker rm -f "$MYSQL_CONTAINER" >/dev/null 2>&1 || true ;;
      *) usage ;;
    esac
    ;;
  *)
    usage
    ;;
esac
