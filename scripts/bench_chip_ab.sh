#!/usr/bin/env bash
# `crates/engine/benches/chip_bench.rs`（Issue #469・手動専用の情報提供
# ベンチ）を before/after 2 状態ディレクトリ（`git archive` で書き出した
# 独立ワークツリー。ブランチ HEAD 参照は使わない）で交互 min-of-N 実行する
# ための薄いドライバ。Issue #530（親 #459・ルート #455）が要求する
# 「Phase 4（チップ最適カーネル群）着手前 SHA と適用後 SHA の前後比較」を
# `docs/design/benchmark-judgement-policy.md` の交互実行・per-run 生データ
# 保持規約に沿って行うために追加した。`scripts/bench_dot_kernel_ab.sh` と
# 同型の骨格（入力検証・生ログ保持・summary.tsv 集計は別スクリプトへ分離）
# を踏襲する。
#
# 呼び出し元は人間の運用者（`make bench-chip-ab`）。呼び出し先は
# `BEFORE_DIR`/`AFTER_DIR`（それぞれ workspace ルート相当のディレクトリ。
# `crates/engine` を含み `cargo bench --bench chip_bench -p engine` が
# 起動できる状態を前提とする）配下の `chip_bench` バイナリを `cargo bench`
# 経由で呼ぶ。production コード（`crates/engine/src/`）・`chip_bench.rs`・
# `harness/chip.rs` は一切変更しない（変更すると before/after で計測器が
# 変わってしまうため。Issue #519 の判断を踏襲）。
#
# 使い方:
#   BEFORE_DIR=<path> AFTER_DIR=<path> \
#     [AB_PAIRS=5] [BENCH_CHIP_WORKLOADS=dot_kernel,knn_profile] \
#     scripts/bench_chip_ab.sh
#   AB_PAIRS: 交互実行するペア数（既定 5・5 未満は拒否。
#     benchmark-judgement-policy.md §3 の N≥5 規約。他の bench-*-ab スクリプト
#     群と同じ env var 命名規約）。
#   OUT_DIR: 出力先（既定 _/bench/chip-ab/<UTC timestamp>）。
#     `.gitignore` 対象（`_/`）のため、記録を残す場合は
#     `--summarize <dir>` の出力を明示的に
#     `docs/design/bench-data/phase4-chip-ab/<UTC ts>-*.tsv` へコピーする
#     （benchmark-judgement-policy.md §3）。
#
# 出力: <OUT_DIR>/pair<N>-<before|after>/ 配下に各状態の `chip_bench`
# （`BENCH_CHIP_ROUNDS=1` 固定・1 回のドライバ呼び出し = 1 ラウンド分の
# `summary.json`）を書く。集計は `--summarize <dir>` で
# `bench_chip_ab_summarize.py` へ委譲する。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  dir="${2:?usage: $0 --summarize <dir>}"
  exec python3 "${REPO_ROOT}/scripts/bench_chip_ab_summarize.py" "${dir}"
fi

# 共有 CI 環境での閾値評価誤爆・非公開閾値ログの混入防止
# （既存 bench-*-ab スクリプト群と同一方針）。
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  die "refusing to run under GITHUB_ACTIONS (manual-only benchmark; see benchmark-judgement-policy.md)"
fi

: "${BEFORE_DIR:?BEFORE_DIR must be set to the before state directory (git archive checkout)}"
: "${AFTER_DIR:?AFTER_DIR must be set to the after state directory (git archive checkout)}"

for label_dir in "BEFORE_DIR=${BEFORE_DIR}" "AFTER_DIR=${AFTER_DIR}"; do
  dir="${label_dir#*=}"
  [[ -d "${dir}" ]] || die "not a directory: ${dir}"
  [[ -f "${dir}/Cargo.toml" ]] || die "missing Cargo.toml (not a workspace root): ${dir}"
  [[ -d "${dir}/crates/engine" ]] || die "missing crates/engine: ${dir}"
done

PAIRS="${AB_PAIRS:-5}"
if ! [[ "${PAIRS}" =~ ^[0-9]+$ ]] || [ "${PAIRS}" -lt 5 ]; then
  die "AB_PAIRS must be an integer >= 5 (benchmark-judgement-policy.md \$3 N>=5), got: ${PAIRS}"
fi

DEFAULT_OUT_DIR="${REPO_ROOT}/_/bench/chip-ab/$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${OUT_DIR:-${DEFAULT_OUT_DIR}}"
mkdir -p "${OUT_DIR}"

CARGO_BIN="${CARGO:-cargo}"

run_one() {
  local state_dir="$1" side="$2" pair="$3"
  local run_out="${OUT_DIR}/pair${pair}-${side}"

  if [ -e "${run_out}" ]; then
    die "output directory already exists (refusing to overwrite): ${run_out}. Use a fresh OUT_DIR."
  fi
  mkdir -p "${run_out}"

  echo "# pre-run environment snapshot" > "${run_out}/pre-run.txt"
  echo "loadavg: $(cat /proc/loadavg 2>/dev/null || echo unavailable)" >> "${run_out}/pre-run.txt"
  echo "side=${side} pair=${pair} state_dir=${state_dir}" >> "${run_out}/pre-run.txt"

  local status=0
  (
    cd "${state_dir}"
    BENCH_CHIP_ROUNDS=1 \
    BENCH_CHIP_OUT_DIR="${run_out}" \
    BENCH_CHIP_WORKLOADS="${BENCH_CHIP_WORKLOADS:-}" \
    "${CARGO_BIN}" bench --bench chip_bench -p engine
  ) > "${run_out}/driver.log" 2>&1 || status=$?

  if [ "${status}" -ne 0 ]; then
    die "chip_bench failed for side=${side} pair=${pair} (exit=${status}); see ${run_out}/driver.log"
  fi
}

for pair in $(seq 1 "${PAIRS}"); do
  run_one "${BEFORE_DIR}" before "${pair}"
  run_one "${AFTER_DIR}" after "${pair}"
done

echo "done: results under ${OUT_DIR}"
echo "summarize with: scripts/bench_chip_ab.sh --summarize ${OUT_DIR}"
