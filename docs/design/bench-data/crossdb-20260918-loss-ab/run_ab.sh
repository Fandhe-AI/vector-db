#!/bin/bash
# before/after 交互 N ペア実行（benchmark-judgement-policy.md §交互 N≥5 ペア）。
set -euo pipefail

# 同梱の run_one.sh は本スクリプトと同じディレクトリから解決する（$WORK は計測出力先）。
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BEFORE_BIN=$REPO/target/release/wire-server
AFTER_BIN=$WORKTREE/target/release/wire-server
PAIRS="${AB_PAIRS:-5}"

for i in $(seq 1 "$PAIRS"); do
  echo "=== pair $i: before ==="
  "$HERE/run_one.sh" before "$BEFORE_BIN" "$i"
  echo "=== pair $i: after ==="
  "$HERE/run_one.sh" after "$AFTER_BIN" "$i"
done
echo "AB DONE"
