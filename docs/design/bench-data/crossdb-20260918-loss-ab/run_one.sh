#!/bin/bash
# crossdb self（wire 経由）1 run 実行ドライバ。
# 使い方: run_one.sh <arm 名（before|after）> <wire-server バイナリ絶対パス> <run 番号>
set -euo pipefail

ARM="$1"
BIN="$2"
RUN="$3"

W=$WORKTREE
S=$FIXTURE_DIR
L=$WORK

OUT="$L/ab4/out/${ARM}-run${RUN}"
WORK="$L/ab4/work"
mkdir -p "$OUT" "$WORK"

export CROSSDB_SELF_BINARY="$BIN"
cd "$W"
"$S/venv/bin/python" scripts/crossdb_bench/run.py \
  --db self --config exact \
  --rows-file "$S/docs25k.redb" \
  --queries-file "$S/queries200.jsonl" \
  --out-dir "$OUT" \
  --workdir "$WORK"
