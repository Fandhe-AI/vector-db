#!/bin/bash
# before/after 交互 N ペア実行（benchmark-judgement-policy.md §交互 N≥5 ペア）。
set -euo pipefail

L=$WORK
BEFORE_BIN=$REPO/target/release/wire-server
AFTER_BIN=$WORKTREE/target/release/wire-server
PAIRS="${AB_PAIRS:-5}"

for i in $(seq 1 "$PAIRS"); do
  echo "=== pair $i: before ==="
  "$L/ab4/run_one.sh" before "$BEFORE_BIN" "$i"
  echo "=== pair $i: after ==="
  "$L/ab4/run_one.sh" after "$AFTER_BIN" "$i"
done
echo "AB DONE"
