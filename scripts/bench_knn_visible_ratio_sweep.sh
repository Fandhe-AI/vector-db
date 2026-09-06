#!/usr/bin/env bash
# Issue #487（可視比率〔1/2・1/4・1/10・1/20・1/50〕× 行数〔25k・100k〕での
# hnsw_subset と plain scan の損益分岐点計測）の交互計測ドライバ。
#
# `crates/engine/benches/knn_profile_bench.rs` の可視比率スイープ opt-in
# （`BENCH_KNN_PROFILE_VISIBLE_RATIO`／`BENCH_KNN_PROFILE_FULL_SCAN_RATIO`／
# `BENCH_KNN_PROFILE_SCALE`）を、scale × ratio × arm × pair の全組み合わせで
# 交互実行し、各 run の stdout を個別ログへ保存する（計測規約
# `docs/design/benchmark-judgement-policy.md` §3〜§5 の「交互 N≥5 ペア・
# per-run 生データ必須」に従う）。集計・表への転記は本スクリプトの責務外
# （実装者・運用者が `--summarize` で生成した一覧、または各ログを直接読んで行う）。
#
# 使い方: scripts/bench_knn_visible_ratio_sweep.sh [--summarize <dir>]
#   env SWEEP_PAIRS=<N>（既定 5）でペア数を上書きできる。
#
# arm（1 ペア内で輪番。計測規約の「3 候補以上の輪番方式」に対応）:
#   - brute_force        : BENCH_KNN_PROFILE_ENGINE=brute_force（対照）
#   - hnsw_default        : BENCH_KNN_PROFILE_ENGINE=hnsw（full_scan_ratio 既定 1/10）
#   - hnsw_force_ann       : 上記 ＋ BENCH_KNN_PROFILE_FULL_SCAN_RATIO=0/1（常に ANN 側）
#   - hnsw_force_plain      : 上記 ＋ BENCH_KNN_PROFILE_FULL_SCAN_RATIO=1/1（常に plain scan 側）
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir>}"
  # 各ログの `stage(S0_hot_where_subset): ... median=<N>ms` 行を
  # scale/ratio/arm/pair ごとに一覧化する（表への転記は実装者が行う。
  # awk のみで完結させ追加依存を持たない）。
  grep -H "stage(S0_hot_where_subset)" "${DIR}"/*.log | \
    sed -E 's#.*/([^/]+)\.log:stage\(S0_hot_where_subset\): rows=([0-9]+) median=([0-9.]+)ms.*#\1 rows=\2 median=\3ms#'
  exit 0
fi

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "ERROR: refusing to run under GITHUB_ACTIONS (CI 非配線・手動専用。Issue #487)" >&2
  exit 1
fi

SWEEP_PAIRS="${SWEEP_PAIRS:-5}"
if ! [[ "${SWEEP_PAIRS}" =~ ^[0-9]+$ ]] || [ "${SWEEP_PAIRS}" -lt 1 ]; then
  echo "ERROR: SWEEP_PAIRS must be a positive integer, got: ${SWEEP_PAIRS}" >&2
  exit 1
fi

TS="$(date +%s)"
OUT_DIR="${REPO_ROOT}/target/bench-knn-visible-ratio/${TS}"
mkdir -p "${OUT_DIR}"

{
  echo "commit=$(cd "${REPO_ROOT}" && git rev-parse HEAD)"
  echo "nproc=$(nproc)"
  echo "BENCH_DEDICATED_ENV=${BENCH_DEDICATED_ENV:-<unset>}"
  if [ -r /proc/cpuinfo ]; then
    echo "cpu_model=$(grep -m1 '^model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
    echo "cpu_flags=$(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
  fi
} >"${OUT_DIR}/env.txt"

echo "building knn_profile_bench (release, once)"
(cd "${REPO_ROOT}" && cargo bench --bench knn_profile_bench -p engine --no-run)

RATIOS=("1/2" "1/4" "1/10" "1/20" "1/50")
SCALES=(1 4)
ARMS=(brute_force hnsw_default hnsw_force_ann hnsw_force_plain)

run_one() {
  local scale="$1" ratio="$2" arm="$3" pair="$4"
  local ratio_slug="${ratio/\//_}"
  local log="${OUT_DIR}/scale${scale}_ratio${ratio_slug}_${arm}_pair${pair}.log"
  local engine="hnsw"
  local full_scan_ratio=""
  case "${arm}" in
    brute_force) engine="brute_force" ;;
    hnsw_default) engine="hnsw" ;;
    hnsw_force_ann) engine="hnsw"; full_scan_ratio="0/1" ;;
    hnsw_force_plain) engine="hnsw"; full_scan_ratio="1/1" ;;
    *) echo "ERROR: unknown arm ${arm}" >&2; exit 1 ;;
  esac

  echo "loadavg=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo n/a)" >"${log}"
  if [ -n "${full_scan_ratio}" ]; then
    BENCH_KNN_PROFILE_VISIBLE_RATIO="${ratio}" \
      BENCH_KNN_PROFILE_ENGINE="${engine}" \
      BENCH_KNN_PROFILE_SCALE="${scale}" \
      BENCH_KNN_PROFILE_FULL_SCAN_RATIO="${full_scan_ratio}" \
      cargo bench --bench knn_profile_bench -p engine >>"${log}" 2>&1
  else
    BENCH_KNN_PROFILE_VISIBLE_RATIO="${ratio}" \
      BENCH_KNN_PROFILE_ENGINE="${engine}" \
      BENCH_KNN_PROFILE_SCALE="${scale}" \
      cargo bench --bench knn_profile_bench -p engine >>"${log}" 2>&1
  fi
}

cd "${REPO_ROOT}"
for scale in "${SCALES[@]}"; do
  for ratio in "${RATIOS[@]}"; do
    for pair in $(seq 1 "${SWEEP_PAIRS}"); do
      for arm in "${ARMS[@]}"; do
        echo "run: scale=${scale} ratio=${ratio} arm=${arm} pair=${pair}"
        run_one "${scale}" "${ratio}" "${arm}" "${pair}"
      done
    done
  done
done

echo "done. logs in ${OUT_DIR}"
echo "summarize with: scripts/bench_knn_visible_ratio_sweep.sh --summarize ${OUT_DIR}"
