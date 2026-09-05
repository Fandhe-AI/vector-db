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

# 失敗した DB/構成・コンテナ起動を蓄積し、後片付け後に非 0 で終了する
# （握りつぶすと結果 JSON が欠けても make bench-crossdb が成功に見え、既存の古い
# JSON を今回の結果と誤認しうる）。
FAILED=()

run() {
  echo "== $(date +%T) $2 $3"
  if ! "$V" "$B/run.py" --rows-file "$1" --queries-file "$QUERIES" --docs-file "$ROWS_JSONL" \
    --out-dir "$S/results" --db "$2" --config "$3" > "$S/logs/$2_$3.log" 2>&1; then
    echo "FAILED $2 $3 (see $S/logs/$2_$3.log)"
    FAILED+=("$2/$3")
  fi
}

# コンテナ起動に失敗したら当該 DB の計測を飛ばし失敗として記録する
up() {
  if bash "$B/containers.sh" up "$1"; then return 0; fi
  echo "FAILED containers.sh up $1"
  FAILED+=("container:$1")
  return 1
}

# 対象以外のコンテナを止める（本ハーネスが起動したもののみ）
for c in pgvector qdrant mysql; do bash "$B/containers.sh" down "$c" >/dev/null 2>&1 || true; done

run "$ROWS_REDB" self exact
run "$ROWS_JSONL" sqlite_vec exact
run "$ROWS_JSONL" lancedb exact
run "$ROWS_JSONL" lancedb hnsw
if up pgvector; then
  run "$ROWS_JSONL" pgvector exact
  run "$ROWS_JSONL" pgvector hnsw
fi
bash "$B/containers.sh" down pgvector
if up qdrant; then
  run "$ROWS_JSONL" qdrant exact
  run "$ROWS_JSONL" qdrant hnsw
fi
bash "$B/containers.sh" down qdrant
if up mysql; then
  run "$ROWS_JSONL" mysql exact
fi
bash "$B/containers.sh" down mysql
if [ "${#FAILED[@]}" -gt 0 ]; then
  echo "== $(date +%T) FAILED (${#FAILED[@]}): ${FAILED[*]}"
  exit 1
fi
echo "== $(date +%T) done: $S/results"
