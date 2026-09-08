#!/bin/bash
# 他 DB 横断ベンチ（scripts/crossdb_bench/run.py）を DB ごとに直列実行する。
# `make bench-crossdb` から呼ばれる。計測対象以外のコンテナは公平性のため停止する。
#
# 必須環境変数:
#   CROSSDB_DIR    fixture ディレクトリ（docs25k.redb / docs25k.jsonl / queries200.jsonl を置く。
#                  results/ と logs/ をこの配下に作る）
#   CROSSDB_PYTHON venv の python（requirements.txt を pip install 済みのもの）
#
# 任意環境変数（Issue #466）:
#   CROSSDB_DIM    十進数字のみ（例 768）。指定時は既定ファイル名を
#                  docs25k-d${CROSSDB_DIM}.{redb,jsonl}／queries200-d${CROSSDB_DIM}.jsonl
#                  へ、結果・ログの出力先を results/d${CROSSDB_DIM}／logs/d${CROSSDB_DIM}
#                  へ切り替える（dim=128 の既存結果を上書きしないため）。
#                  CROSSDB_REDB／CROSSDB_DOCS／CROSSDB_QUERIES の明示指定はこの既定より
#                  常に優先する。未設定なら従来どおり docs25k.*／queries200.jsonl／
#                  results／logs を使う（後方互換）。
set -u
R=$(cd "$(dirname "$0")/../.." && pwd)
B=$R/scripts/crossdb_bench
S=${CROSSDB_DIR:?CROSSDB_DIR is required}
V=${CROSSDB_PYTHON:?CROSSDB_PYTHON is required}
DIM=${CROSSDB_DIM:-}
case "$DIM" in
  '') SUFFIX='' ;;
  *[!0-9]*)
    echo "CROSSDB_DIM must contain only decimal digits (got: $DIM)" >&2
    exit 1
    ;;
  *) SUFFIX="-d${DIM}" ;;
esac
ROWS_REDB=${CROSSDB_REDB:-$S/docs25k${SUFFIX}.redb}
ROWS_JSONL=${CROSSDB_DOCS:-$S/docs25k${SUFFIX}.jsonl}
QUERIES=${CROSSDB_QUERIES:-$S/queries200${SUFFIX}.jsonl}
if [ -n "$DIM" ]; then
  RESULTS_DIR="$S/results/d${DIM}"
  LOGS_DIR="$S/logs/d${DIM}"
else
  RESULTS_DIR="$S/results"
  LOGS_DIR="$S/logs"
fi
cd "$R"
mkdir -p "$RESULTS_DIR" "$LOGS_DIR"

# 失敗した DB/構成・コンテナ起動を蓄積し、後片付け後に非 0 で終了する
# （握りつぶすと結果 JSON が欠けても make bench-crossdb が成功に見え、既存の古い
# JSON を今回の結果と誤認しうる）。
FAILED=()

run() {
  echo "== $(date +%T) $2 $3"
  expect_dim_args=()
  if [ -n "$DIM" ]; then
    expect_dim_args=(--expect-dim "$DIM")
  fi
  if ! "$V" "$B/run.py" --rows-file "$1" --queries-file "$QUERIES" --docs-file "$ROWS_JSONL" \
    --out-dir "$RESULTS_DIR" --db "$2" --config "$3" "${expect_dim_args[@]+"${expect_dim_args[@]}"}" \
    > "$LOGS_DIR/$2_$3.log" 2>&1; then
    echo "FAILED $2 $3 (see $LOGS_DIR/$2_$3.log)"
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
# self の hnsw 構成（`--search-engine hnsw` opt-in。Issue #656〜#658）は
# `crossdb_plan_probe` example（`cargo build --release -p engine --example
# crossdb_plan_probe`）のビルドを追加で要求する。未ビルドなら本行は失敗として
# 記録される（`FAILED` に積まれ非 0 終了。ログは `$LOGS_DIR/self_hnsw.log`）。
run "$ROWS_REDB" self hnsw
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
echo "== $(date +%T) done: $RESULTS_DIR"
