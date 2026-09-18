#!/usr/bin/env bash
# `crates/engine/benches/ingest_profile_bench.rs`（`BENCH_INGEST_PROFILE_MODE=single`。
# Issue #484）の durability A/B（Issue #851）。既定 `immediate`（redb
# `Durability::Immediate`。1 commit ごとに std `sync_data` → macOS では
# `F_FULLFSYNC` 相当の同期を伴う）と opt-in `none`（`WriteDurability::None`。
# Issue #849・#850）を同一バイナリ・同一 fixture で交互 N≥5 ペア実行し、
# `docs/design/benchmark-judgement-policy.md` の交互実行・per-run 生データ
# 保持規約に沿って I8（redb commit）中心の前後比較材料を残す。
#
# `scripts/bench_knn_f16_resident_ab.sh` と同型の骨格（GITHUB_ACTIONS 拒否・
# AB_PAIRS 検証・macOS/Linux 両対応の環境スナップショット・生ログ保持）を
# 踏襲する。production コード（`crates/engine/src/`）は変更しない
# （本スクリプトはベンチの起動パラメータを変えるだけ）。
#
# 使い方:
#   scripts/bench_ingest_durability_ab.sh [AB_PAIRS]
#   AB_PAIRS: 交互実行するペア数（既定 5・5 未満は拒否）。
#   env STATEMENTS: 1 arm あたりの単文数（既定 5,000。
#     `harness::ingest_profile::MIN_SINGLE_STATEMENTS`〔2,000〕以上・
#     `MAX_SINGLE_STATEMENTS`〔100,000〕以下）。既定は crossdb ベンチの
#     25,000 より小さいが、`SINGLE_WARMUP_STATEMENTS`〔1,000〕の 2 倍以上を
#     確保しつつ `immediate` arm（1 commit ごとに数 ms の同期）を N=5 ペアで
#     現実的な時間に収める値。
#   env FSYNC_PROBE_DIR: `scripts/fsync_probe.py` の対象ディレクトリ
#     （既定: OUT_DIR と同じボリューム）。
#
# 出力: <OUT_DIR>/<pair>-<arm>.log に env スナップショット・stdout 全文、
# <OUT_DIR>/fsync-probe.json に同一ボリュームでの fsync 系原始操作の参考値。
# 集計は `scripts/bench_ingest_durability_ab_summarize.py <OUT_DIR>`。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "ERROR: refusing to run under GITHUB_ACTIONS (CI 非配線・手動専用。Issue #851)" >&2
  exit 1
fi

AB_PAIRS="${1:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  echo "ERROR: AB_PAIRS must be an integer >= 5 (benchmark-judgement-policy.md §3), got: ${AB_PAIRS}" >&2
  exit 1
fi

STATEMENTS="${STATEMENTS:-5000}"
if ! [[ "${STATEMENTS}" =~ ^[0-9]+$ ]] || [ "${STATEMENTS}" -lt 2000 ] || [ "${STATEMENTS}" -gt 100000 ]; then
  echo "ERROR: STATEMENTS must be an integer in [2000, 100000], got: ${STATEMENTS}" >&2
  exit 1
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${REPO_ROOT}/target/bench-ingest-durability-ab/${TS}"
mkdir -p "${OUT_DIR}"

FSYNC_PROBE_DIR="${FSYNC_PROBE_DIR:-${OUT_DIR}}"

# macOS/Linux 両対応の CPU 情報 best-effort 収集
# （`scripts/bench_knn_f16_resident_ab.sh::collect_env_cpu_lines` と同型）。
collect_env_cpu_lines() {
  if [ -r /proc/cpuinfo ]; then
    echo "cpu_model=$(grep -m1 '^model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
    return 0
  fi
  if command -v sysctl >/dev/null 2>&1; then
    echo "cpu_brand_string=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unavailable)"
  fi
  if command -v sw_vers >/dev/null 2>&1; then
    echo "sw_vers=$(sw_vers -productVersion 2>/dev/null || echo unavailable)"
  fi
  echo "uname_m=$(uname -m 2>/dev/null || echo unavailable)"
}

log_noise() {
  local log="$1"
  if [ -r /proc/loadavg ]; then
    echo "loadavg=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo n/a)" >>"${log}"
    return 0
  fi
  if command -v sysctl >/dev/null 2>&1; then
    echo "loadavg=$(sysctl -n vm.loadavg 2>/dev/null || echo n/a)" >>"${log}"
    return 0
  fi
  echo "loadavg=n/a" >>"${log}"
}

# 同時実行プロセス数（共有機での交絡診断・`docs/design/
# benchmark-judgement-policy.md` §5「共有 QEMU 環境の証拠力区分」と同方針）。
concurrent_process_count() {
  ps ax 2>/dev/null | wc -l | tr -d ' '
}

{
  echo "commit=$(cd "${REPO_ROOT}" && git rev-parse HEAD)"
  echo "nproc=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
  echo "BENCH_DEDICATED_ENV=${BENCH_DEDICATED_ENV:-<unset>}"
  collect_env_cpu_lines
  echo "rustc_version=$(rustc --version 2>/dev/null || echo unavailable)"
  echo "ab_pairs=${AB_PAIRS}"
  echo "statements=${STATEMENTS}"
  echo "process_count_before=$(concurrent_process_count)"
} >"${OUT_DIR}/env.txt"

cd "${REPO_ROOT}"

echo "building ingest_profile_bench (release, once)"
cargo bench --bench ingest_profile_bench -p fandhe-vector-db-engine --no-run

echo "wire-server binary (unused by this driver — engine bench only; docs 側で明記)"

echo "running fsync_probe against ${FSYNC_PROBE_DIR}"
if command -v python3 >/dev/null 2>&1; then
  python3 "${REPO_ROOT}/scripts/fsync_probe.py" \
    --dir "${FSYNC_PROBE_DIR}" --iters 100 --out "${OUT_DIR}/fsync-probe.json"
else
  echo "ERROR: python3 not found; fsync_probe requires python3" >&2
  exit 1
fi

run_arm() {
  local arm="$1" pair="$2"
  local log="${OUT_DIR}/pair${pair}-${arm}.log"
  log_noise "${log}"
  BENCH_INGEST_PROFILE_MODE=single \
    BENCH_INGEST_PROFILE_STATEMENTS="${STATEMENTS}" \
    BENCH_INGEST_PROFILE_DURABILITY="${arm}" \
    cargo bench --bench ingest_profile_bench -p fandhe-vector-db-engine >>"${log}" 2>&1
}

for pair in $(seq 1 "${AB_PAIRS}"); do
  echo "run: pair=${pair} arm=immediate"
  run_arm "immediate" "${pair}"
  echo "run: pair=${pair} arm=none"
  run_arm "none" "${pair}"
done

echo "process_count_after=$(concurrent_process_count)" >>"${OUT_DIR}/env.txt"

echo "done. logs in ${OUT_DIR}"
echo "summarize with: python3 scripts/bench_ingest_durability_ab_summarize.py ${OUT_DIR}"
