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
#   env AB_PAIRS（優先）または位置引数 $1: 交互実行するペア数
#     （既定 5・5 未満は拒否）。`make bench-ingest-durability-ab AB_PAIRS=10`
#     のように env 経由で上書きできる（codex-review 指摘・Issue #857）。
#   env STATEMENTS: 1 arm あたりの単文数（既定 5,000。
#     `harness::ingest_profile::MIN_SINGLE_STATEMENTS`〔2,000〕以上・
#     `MAX_SINGLE_STATEMENTS`〔100,000〕以下）。既定は crossdb ベンチの
#     25,000 より小さいが、`SINGLE_WARMUP_STATEMENTS`〔1,000〕の 2 倍以上を
#     確保しつつ `immediate` arm（1 commit ごとに数 ms の同期）を N=5 ペアで
#     現実的な時間に収める値。
#   env FSYNC_PROBE_DIR: `scripts/fsync_probe.py` の対象ディレクトリ
#     （既定: OUT_DIR と同じボリューム）。
#
# 注意（codex-review P2 指摘・Issue #857）: `FSYNC_PROBE_DIR` の既定はリポジトリ
# 配下（`OUT_DIR`）だが、ベンチ本体の E0/S0/replica DB は `unique_db_path`
# 経由で Rust `std::env::temp_dir()`（`$TMPDIR` 優先・未設定時は `/tmp`）に
# 作られるため、`$TMPDIR` が tmpfs でリポジトリが別ディスクにある環境等では
# 「fsync プローブ計測先」と「DB 実配置先」が別ボリュームになり得る。両者が
# 一致することを前提にせず、実際の DB 配置先ボリューム情報を `env.txt` へ
# 別途記録する（`db_temp_dir`・`db_temp_dir_df` 行）ことで、前後比較時に
# ボリューム不一致の有無を確認できるようにする。
#
# 出力: <OUT_DIR>/<pair>-<arm>.log に env スナップショット・stdout 全文、
# <OUT_DIR>/fsync-probe.json に同一ボリュームでの fsync 系原始操作の参考値、
# env.txt に DB 実配置先ボリューム情報（上記）。
# 集計は `scripts/bench_ingest_durability_ab_summarize.py <OUT_DIR>`。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "ERROR: refusing to run under GITHUB_ACTIONS (CI 非配線・手動専用。Issue #851)" >&2
  exit 1
fi

# env AB_PAIRS を優先し（`make bench-ingest-durability-ab AB_PAIRS=10` 等の
# 呼び出しに対応。codex-review 指摘・Issue #857）、未設定時のみ位置引数 $1 を見る
# （後方互換）。他の A/B スクリプト（`bench_hybrid_latency_ab.sh` 等）と同じく
# env 変数を第一の入力経路とする。
AB_PAIRS="${AB_PAIRS:-${1:-5}}"
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

# `crates/engine/src/test_util/temp_db.rs::unique_db_path` が呼ぶ Rust
# `std::env::temp_dir()` と同じ解決規則（Unix: `$TMPDIR` があればそれ、
# なければ `/tmp`）で DB の実配置先ボリュームを記録する（codex-review P2
# 指摘・Issue #857。計測対象ボリューム〔OUT_DIR／FSYNC_PROBE_DIR〕と DB 実配置先
# が異なり得ることを事後確認できるようにするための参考情報。DB 配置先自体は
# 変更しない）。
DB_TEMP_DIR="${TMPDIR:-/tmp}"

{
  echo "commit=$(cd "${REPO_ROOT}" && git rev-parse HEAD)"
  echo "nproc=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
  echo "BENCH_DEDICATED_ENV=${BENCH_DEDICATED_ENV:-<unset>}"
  collect_env_cpu_lines
  echo "rustc_version=$(rustc --version 2>/dev/null || echo unavailable)"
  echo "ab_pairs=${AB_PAIRS}"
  echo "statements=${STATEMENTS}"
  echo "fsync_probe_dir=${FSYNC_PROBE_DIR}"
  echo "db_temp_dir=${DB_TEMP_DIR}"
  echo "db_temp_dir_df=$(df "${DB_TEMP_DIR}" 2>/dev/null | tail -n +2 || echo unavailable)"
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
