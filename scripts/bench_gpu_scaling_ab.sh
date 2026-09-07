#!/usr/bin/env bash
# `crates/engine/benches/gpu_scaling_bench.rs`（Issue #178 ポインタ・手動専用の
# 情報提供ベンチ）を before/after 2 バイナリで交互 min-of-N 実行するための薄い
# ドライバ。Issue #533（親 #531・#460・#455）が要求する「#532（クエリタイル化）
# の前後比較」を `docs/design/benchmark-judgement-policy.md` の交互実行・
# per-run 生データ保持・参照区間ノイズ帯規約に沿って計測するために追加した。
#
# 呼び出し元は人間の運用者（`make` 経由のターゲットは設けない。ベンチそのものが
# 手動専用・CI 非配線のため）。呼び出し先は `BEFORE_BIN`/`AFTER_BIN` として渡す
# 2 つの `gpu_scaling_bench` 実行ファイル（`cargo bench --bench gpu_scaling_bench
# -p engine --no-run --message-format=json` で得た成果物を退避したもの）。
# production コード（`crates/engine/src/`）・既存ベンチハーネスは一切変更しない。
#
# 使い方:
#   BEFORE_BIN=<path> AFTER_BIN=<path> scripts/bench_gpu_scaling_ab.sh \
#     [PAIRS] [POINTS...]
#   PAIRS: 交互実行するペア数（既定 5・正整数のみ）。
#   POINTS: "rows:dim:batch" 形式の規模点（十進数字のみ）。省略時は既定 1 点
#     20000:128:8 のみ（スモーク用）。複数指定可。
#   OUT_DIR: 出力先（既定 _/bench/gpu-scaling-ab/<UTC timestamp>）。
#
# 出力: <OUT_DIR>/<rows>-<dim>-<batch>/<pair>-<before|after>.log に stdout 全文、
# <OUT_DIR>/summary.tsv に集計用の 1 行 1 run の TSV を追記する。
# 各 run の直前に /proc/loadavg・nvidia-smi のクロック/温度を同じログへ書き、
# 計測環境のノイズ源を再現性のため残す。
#
# 実行順序（規約: 逐次実行にしない・生ログを残す・skip/unavailable を握りつぶさない）:
#   各規模点について pair=1..PAIRS の順で before → after を実行する。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

: "${BEFORE_BIN:?BEFORE_BIN must be set to the before gpu_scaling_bench binary path}"
: "${AFTER_BIN:?AFTER_BIN must be set to the after gpu_scaling_bench binary path}"

[[ -x "${BEFORE_BIN}" ]] || die "BEFORE_BIN is not an executable file: ${BEFORE_BIN}"
[[ -x "${AFTER_BIN}" ]] || die "AFTER_BIN is not an executable file: ${AFTER_BIN}"

PAIRS="${1:-5}"
if ! [[ "${PAIRS}" =~ ^[0-9]+$ ]] || [ "${PAIRS}" -lt 1 ]; then
  die "PAIRS must be a positive integer, got: ${PAIRS}"
fi
shift || true

POINTS=("$@")
if [ "${#POINTS[@]}" -eq 0 ]; then
  POINTS=("20000:128:8")
fi

for point in "${POINTS[@]}"; do
  if ! [[ "${point}" =~ ^[0-9]+:[0-9]+:[0-9]+$ ]]; then
    die "point must match rows:dim:batch (digits only), got: ${point}"
  fi
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_OUT_DIR="${REPO_ROOT}/_/bench/gpu-scaling-ab/$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${OUT_DIR:-${DEFAULT_OUT_DIR}}"
mkdir -p "${OUT_DIR}"

SUMMARY="${OUT_DIR}/summary.tsv"
if [ ! -f "${SUMMARY}" ]; then
  printf 'point\tside\tpair\tcpu_p50\tcpu_p95\tf16_p50\tf16_p95\tf32_p50\tf32_p95\tmismatch\tline\n' > "${SUMMARY}"
fi

# 1 行の `gpu_scaling: ...` 出力（正常計測行のみ）から TSV フィールドを
# 取り出す。skip/not measurable/gpu unavailable 行は集計対象外のまま
# ログにのみ残し、summary.tsv には別途 status 行として記録する。
extract_field() {
  local line="$1" key="$2"
  # 例: "cpu_simd_p50=1234us" -> 1234
  # 注意: `grep -oE '[0-9]+'` を素朴に重ねると "cpu_simd_p50" 自体に含まれる
  # "50" まで数値として拾ってしまう（例: p50/p95 系のキー名と衝突する）。
  # 必ず `key=` の直後の数値だけを sed で取り出す。
  local matched
  matched="$(echo "${line}" | grep -oE "${key}=[0-9]+us" | head -1 || true)"
  [ -n "${matched}" ] || { echo ""; return; }
  echo "${matched}" | sed -E "s/^${key}=([0-9]+)us\$/\\1/"
}

run_one() {
  local bin="$1" side="$2" pair="$3" rows="$4" dim="$5" batch="$6" point="$7"
  local point_dir="${OUT_DIR}/${point//:/-}"
  mkdir -p "${point_dir}"
  local log="${point_dir}/${pair}-${side}.log"

  {
    echo "# pre-run environment snapshot"
    echo "loadavg: $(cat /proc/loadavg 2>/dev/null || echo unavailable)"
    if command -v nvidia-smi >/dev/null 2>&1; then
      echo "nvidia-smi clocks.sm,temperature.gpu:"
      nvidia-smi --query-gpu=clocks.sm,temperature.gpu --format=csv,noheader 2>/dev/null || echo unavailable
    else
      echo "nvidia-smi: not found"
    fi
    echo "# gpu_scaling_bench output"
  } > "${log}"

  local status=0
  BENCH_GPU_SCALING_ROWS="${rows}" \
    BENCH_GPU_SCALING_DIMS="${dim}" \
    BENCH_GPU_SCALING_BATCH="${batch}" \
    "${bin}" >> "${log}" 2>&1 || status=$?

  local result_line
  result_line="$(grep -E '^gpu_scaling: rows=' "${log}" | tail -1 || true)"

  if [ -n "${result_line}" ]; then
    local cpu_p50 cpu_p95 f16_p50 f16_p95 f32_p50 f32_p95 mismatch
    cpu_p50="$(extract_field "${result_line}" cpu_simd_p50)"
    cpu_p95="$(extract_field "${result_line}" cpu_simd_p95)"
    f16_p50="$(extract_field "${result_line}" gpu_f16_p50)"
    f16_p95="$(extract_field "${result_line}" gpu_f16_p95)"
    f32_p50="$(extract_field "${result_line}" gpu_f32_p50)"
    f32_p95="$(extract_field "${result_line}" gpu_f32_p95)"
    mismatch="$(echo "${result_line}" | grep -oE 'mismatch=[0-9]+' | grep -oE '[0-9]+' || true)"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "${point}" "${side}" "${pair}" "${cpu_p50}" "${cpu_p95}" "${f16_p50}" "${f16_p95}" \
      "${f32_p50}" "${f32_p95}" "${mismatch}" "measured" >> "${SUMMARY}"
  else
    local status_line
    status_line="$(grep -E '^gpu_scaling: (skip|not measurable|gpu unavailable)' "${log}" | tail -1 || echo "exit=${status}")"
    printf '%s\t%s\t%s\t\t\t\t\t\t\t\t%s\n' \
      "${point}" "${side}" "${pair}" "${status_line//$'\t'/ }" >> "${SUMMARY}"
  fi
}

for point in "${POINTS[@]}"; do
  IFS=':' read -r rows dim batch <<< "${point}"
  for pair in $(seq 1 "${PAIRS}"); do
    run_one "${BEFORE_BIN}" before "${pair}" "${rows}" "${dim}" "${batch}" "${point}"
    run_one "${AFTER_BIN}" after "${pair}" "${rows}" "${dim}" "${batch}" "${point}"
  done
done

echo "done: results under ${OUT_DIR}"
