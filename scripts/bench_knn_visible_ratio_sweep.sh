#!/usr/bin/env bash
# Issue #487（可視比率〔1/2・1/4・1/10・1/20・1/50〕× 行数〔25k・100k〕での
# hnsw_subset と plain scan の損益分岐点計測）の交互計測ドライバ。
#
# `crates/engine/benches/knn_profile_bench.rs` の可視比率スイープ opt-in
# （`BENCH_KNN_PROFILE_VISIBLE_RATIO`／`BENCH_KNN_PROFILE_FULL_SCAN_RATIO`／
# `BENCH_KNN_PROFILE_SCALE`）を、scale × ratio の各組み合わせについて
# pair を外側・candidate を内側のループとして交互実行し、各 run の stdout を
# 個別ログへ保存する（計測規約 `docs/design/benchmark-judgement-policy.md`
# §3〜§5 の「交互 N≥5 ペア・per-run 生データ必須」に従う。ペアごとに
# baseline→cand1→baseline→cand2→baseline→cand3 の輪番を繰り返す構造にし、
# 特定の candidate だけを先に SWEEP_PAIRS 回連続実行してしまう時間方向の
# 交絡を避ける）。集計・表への転記は本スクリプトの責務外
# （実装者・運用者が `--summarize` で生成した一覧、または各ログを直接読んで行う）。
#
# 使い方: scripts/bench_knn_visible_ratio_sweep.sh [--summarize <dir>]
#   env SWEEP_PAIRS=<N>（既定 5。計測規約 §3 の N≥5 必須要件により 5 未満は拒否）
#   でペア数を上書きできる。
#
# candidate（brute_force を baseline として各 candidate の直前に必ず 1 回 baseline を
# 挟む輪番方式。計測規約 §3「3 候補以上の場合は baseline→cand1→baseline→cand2→…」
# に対応。baseline 単体を候補として扱わない——baseline は各候補の対照であり、
# 独立した「4 番目の候補」ではないため）:
#   - hnsw_default        : BENCH_KNN_PROFILE_ENGINE=hnsw（full_scan_ratio 既定 1/10）
#   - hnsw_force_ann       : 上記 ＋ BENCH_KNN_PROFILE_FULL_SCAN_RATIO=0/1（常に ANN 側）
#   - hnsw_force_plain      : 上記 ＋ BENCH_KNN_PROFILE_FULL_SCAN_RATIO=1/1（常に plain scan 側）
# baseline（各 candidate の直前に実行。BENCH_KNN_PROFILE_ENGINE=brute_force）
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
if ! [[ "${SWEEP_PAIRS}" =~ ^[0-9]+$ ]] || [ "${SWEEP_PAIRS}" -lt 5 ]; then
  # 計測規約（docs/design/benchmark-judgement-policy.md §3）は交互 N≥5 ペアを
  # 必須事項として定めており「推奨」ではない。5 未満は新規計測として不可。
  echo "ERROR: SWEEP_PAIRS must be an integer >= 5 (benchmark-judgement-policy.md §3), got: ${SWEEP_PAIRS}" >&2
  exit 1
fi

TS="$(date +%s)"
OUT_DIR="${REPO_ROOT}/target/bench-knn-visible-ratio/${TS}"
mkdir -p "${OUT_DIR}"

{
  echo "commit=$(cd "${REPO_ROOT}" && git rev-parse HEAD)"
  echo "nproc=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
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
# baseline（brute_force）を含まない候補一覧。各候補の直前に必ず baseline を
# 1 回実行する（輪番: baseline→cand1→baseline→cand2→baseline→cand3）。
CANDIDATES=(hnsw_default hnsw_force_ann hnsw_force_plain)

# arm ごとの env 設定を解決する（$1=arm 名。case 全分岐で
# BENCH_KNN_PROFILE_FULL_SCAN_RATIO を明示設定し、親シェルからの export 値が
# else 分岐で意図せず引き継がれる事故を防ぐ。空文字列は harness 側で
# 「未設定」＝既定 1/10 として扱われる）。
resolve_env() {
  local arm="$1"
  case "${arm}" in
    baseline) ENGINE="brute_force"; FULL_SCAN_RATIO="" ;;
    hnsw_default) ENGINE="hnsw"; FULL_SCAN_RATIO="" ;;
    hnsw_force_ann) ENGINE="hnsw"; FULL_SCAN_RATIO="0/1" ;;
    hnsw_force_plain) ENGINE="hnsw"; FULL_SCAN_RATIO="1/1" ;;
    *) echo "ERROR: unknown arm ${arm}" >&2; exit 1 ;;
  esac
}

run_one() {
  local scale="$1" ratio="$2" arm="$3" label="$4" pair="$5"
  local ratio_slug="${ratio/\//_}"
  local log="${OUT_DIR}/scale${scale}_ratio${ratio_slug}_${label}_pair${pair}.log"
  local ENGINE FULL_SCAN_RATIO
  resolve_env "${arm}"

  echo "loadavg=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo n/a)" >"${log}"
  BENCH_KNN_PROFILE_VISIBLE_RATIO="${ratio}" \
    BENCH_KNN_PROFILE_ENGINE="${ENGINE}" \
    BENCH_KNN_PROFILE_SCALE="${scale}" \
    BENCH_KNN_PROFILE_FULL_SCAN_RATIO="${FULL_SCAN_RATIO}" \
    cargo bench --bench knn_profile_bench -p engine >>"${log}" 2>&1
}

cd "${REPO_ROOT}"
for scale in "${SCALES[@]}"; do
  for ratio in "${RATIOS[@]}"; do
    # pair を外側、candidate を内側にする（計測規約 §3 の輪番
    # baseline→cand1→baseline→cand2→baseline→cand3 をペアごとに繰り返す。
    # candidate を外側にすると (baseline→候補1) を SWEEP_PAIRS 回連続
    # 実行してから候補2 に進む形になり、輪番にならず時間方向の交絡が
    # 生じる〔codex-review P1 指摘〕）。
    for pair in $(seq 1 "${SWEEP_PAIRS}"); do
      for candidate in "${CANDIDATES[@]}"; do
        echo "run: scale=${scale} ratio=${ratio} arm=baseline(for ${candidate}) pair=${pair}"
        run_one "${scale}" "${ratio}" "baseline" "baseline_for_${candidate}" "${pair}"
        echo "run: scale=${scale} ratio=${ratio} arm=${candidate} pair=${pair}"
        run_one "${scale}" "${ratio}" "${candidate}" "${candidate}" "${pair}"
      done
    done
  done
done

echo "done. logs in ${OUT_DIR}"
echo "summarize with: scripts/bench_knn_visible_ratio_sweep.sh --summarize ${OUT_DIR}"
