#!/bin/bash
# 他 DB 横断ベンチ（scripts/crossdb_bench/run.py）を DB ごとに直列実行する。
# `make bench-crossdb` から呼ばれる。計測対象以外のコンテナは公平性のため停止する。
#
# 必須環境変数:
#   CROSSDB_DIR    fixture ディレクトリ（docs25k.redb / docs25k.jsonl / queries200.jsonl を置く。
#                  results/ と logs/ をこの配下に作る）
#   CROSSDB_PYTHON venv の python（requirements.txt を pip install 済みのもの）
set -u
R=$(cd "$(dirname "$0")/../.." && pwd)
B=$R/scripts/crossdb_bench
S=${CROSSDB_DIR:?CROSSDB_DIR is required}
V=${CROSSDB_PYTHON:?CROSSDB_PYTHON is required}
ROWS_REDB=${CROSSDB_REDB:-$S/docs25k.redb}
ROWS_JSONL=${CROSSDB_DOCS:-$S/docs25k.jsonl}
QUERIES=${CROSSDB_QUERIES:-$S/queries200.jsonl}
cd "$R"
mkdir -p "$S/results" "$S/logs"

run() {
  echo "== $(date +%T) $2 $3"
  "$V" "$B/run.py" --rows-file "$1" --queries-file "$QUERIES" --docs-file "$ROWS_JSONL" \
    --out-dir "$S/results" --db "$2" --config "$3" > "$S/logs/$2_$3.log" 2>&1 || echo "FAILED $2 $3"
}

# 対象以外のコンテナを止める（本ハーネスが起動したもののみ）
for c in pgvector qdrant mysql; do bash "$B/containers.sh" down "$c" >/dev/null 2>&1 || true; done

run "$ROWS_REDB" self exact
run "$ROWS_JSONL" sqlite_vec exact
run "$ROWS_JSONL" lancedb exact
run "$ROWS_JSONL" lancedb hnsw
bash "$B/containers.sh" up pgvector
run "$ROWS_JSONL" pgvector exact
run "$ROWS_JSONL" pgvector hnsw
bash "$B/containers.sh" down pgvector
bash "$B/containers.sh" up qdrant
run "$ROWS_JSONL" qdrant exact
run "$ROWS_JSONL" qdrant hnsw
bash "$B/containers.sh" down qdrant
bash "$B/containers.sh" up mysql
run "$ROWS_JSONL" mysql exact
bash "$B/containers.sh" down mysql
echo "== $(date +%T) done: $S/results"
