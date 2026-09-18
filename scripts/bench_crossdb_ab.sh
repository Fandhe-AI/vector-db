#!/usr/bin/env bash
# crossdb 横断ベンチ（`scripts/crossdb_bench/run_all.sh`）を N ラウンド
# 繰り返し実行し、self・対照 DB（Elasticsearch を含む全 DB）双方の
# per-run 生データを `docs/design/benchmark-judgement-policy.md` の規約
# （交互 N≥5・per-run 生データ必須）に沿って分離保存する（Issue #848）。
#
# `scripts/bench_crossdb_self_hnsw_ab.sh` 等の既存 AB ドライバは before/after
# または exact/hnsw の 2 arm を厳密に輪番させる設計だが、本スクリプトは
# `run_all.sh` が 1 回で走らせる十数 arm（self exact/hnsw/nosql・対照 DB
# 各 exact/hnsw 構成）をまとめて 1 ラウンドとみなし、そのラウンドを
# `AB_PAIRS`（既定 5）回繰り返すラウンドロビン方式を取る（cand が 1 個では
# なく DB の数だけあるため、baseline→cand→baseline→cand... という厳密な
# 2 arm 輪番を適用できない構造的な理由による意図的な逸脱）。
#
# production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
# 一切変更しない（本スクリプト・生成物はテスト・ベンチ専任）。
#
# 使い方:
#   CROSSDB_DIR=<fixture dir> CROSSDB_PYTHON=<venv python> \
#     scripts/bench_crossdb_ab.sh
#   AB_PAIRS=<N>（既定 5・5 未満は拒否）
#   BENCH_DEDICATED_ENV=1（専有環境で実施した場合にのみ設定。env.txt へ記録
#     するだけで動作は変えない）
#
# 出力: <CROSSDB_DIR>/results/round<N>/*.json・<CROSSDB_DIR>/logs/round<N>/*.log
#       docs/design/bench-data/crossdb-<ts>-ab/env.txt
#
# --summarize <dir> <ts> で Markdown 表を出力する
# （scripts/bench_crossdb_ab_summarize.py の実体へ委譲）。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  shift
  exec python3 "${REPO_ROOT}/scripts/bench_crossdb_ab_summarize.py" "$@"
fi

# GITHUB_ACTIONS 下は拒否する（本ベンチは時間依存・spec 閾値を持たない
# 情報提供専用のため CI 非配線・手動実行専用。既存 AB スクリプト群と同方針）。
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  die "this script is manual-only and must not run under GITHUB_ACTIONS"
fi

CROSSDB_DIR="${CROSSDB_DIR:?CROSSDB_DIR (fixture dir containing docs25k.redb/docs25k.jsonl/queries200.jsonl) is required}"
CROSSDB_PYTHON="${CROSSDB_PYTHON:?CROSSDB_PYTHON (venv python with requirements.txt installed) is required}"
export CROSSDB_DIR CROSSDB_PYTHON
AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  die "AB_PAIRS must be an integer >= 5 (got ${AB_PAIRS})"
fi

WIRE_SERVER_BIN="${CROSSDB_SELF_BINARY:-${REPO_ROOT}/target/release/wire-server}"
case "${WIRE_SERVER_BIN}" in
  /*) : ;;
  *) WIRE_SERVER_BIN="$(pwd)/${WIRE_SERVER_BIN}" ;;
esac
[ -x "${WIRE_SERVER_BIN}" ] || die "wire-server binary not found: ${WIRE_SERVER_BIN} (run: cargo build --release -p fandhe-vector-db-wire-server)"

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DOC_DIR="${REPO_ROOT}/docs/design/bench-data/crossdb-${TS}-ab"
mkdir -p "${OUT_DOC_DIR}"

# CPU モデル・コア数・loadavg は macOS（`sysctl`）／Linux（`/proc/*`）の両対応
# で取得する（本 Issue の計測環境が macOS のため。既存 AB ドライバ群は
# Linux 専有の `/proc/loadavg` 等を前提にしており macOS では unavailable に
# なる）。
cpu_model() {
  if command -v sysctl >/dev/null 2>&1 && sysctl -n machdep.cpu.brand_string 2>/dev/null; then
    return 0
  fi
  grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unavailable
}
cpu_count() {
  sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo unavailable
}
loadavg_now() {
  if command -v sysctl >/dev/null 2>&1 && sysctl -n vm.loadavg 2>/dev/null; then
    return 0
  fi
  cat /proc/loadavg 2>/dev/null || echo unavailable
}
concurrent_processes_snapshot() {
  local self_pid=$$ snapshot
  if ! snapshot="$(ps -eo pid=,stat=,comm= 2>/dev/null)"; then
    echo "unknown (ps unavailable)"
    return 0
  fi
  printf '%s\n' "${snapshot}" \
    | awk -v self="${self_pid}" '$1 != self && $3 != "ps" && $2 ~ /^R/ {n++} END {print n+0}'
}
fs_type_of() {
  # macOS の `df` には `-T`（ファイルシステム種別列）が無い。`df` が解決した
  # デバイス（シンボリックリンク・bind mount を辿った後の実マウント点）を
  # 手掛かりに `mount` 出力（`<device> on <path> (<fstype>, ...)`）から拾う。
  # Linux では `df -T` の 2 列目（filesystem type）をそのまま使う。
  if command -v sysctl >/dev/null 2>&1; then
    local dev
    dev="$(df "$1" 2>/dev/null | tail -n1 | awk '{print $1}')"
    [ -n "${dev}" ] || { echo unavailable; return 0; }
    mount 2>/dev/null | awk -v d="${dev}" '$1 == d {print; exit}' \
      | sed -n 's/.*(\([a-zA-Z0-9_]*\),.*/\1/p'
    return 0
  fi
  df -T "$1" 2>/dev/null | tail -n1 | awk '{print $2}' || echo unavailable
}
docker_image_digest() {
  # $1 = image reference（例 pgvector/pgvector:pg17）。`latest`／メジャータグは
  # 計測ごとに digest を記録する既定方針（README 参照）。
  docker image inspect --format '{{index .RepoDigests 0}}' "$1" 2>/dev/null || echo "unavailable($1)"
}

{
  echo "# crossdb 再計測（Issue #848）環境記録"
  echo "timestamp_utc: ${TS}"
  echo "wire_server_sha256: $(shasum -a 256 "${WIRE_SERVER_BIN}" 2>/dev/null | awk '{print $1}')"
  echo "git_rev: $(git -C "${REPO_ROOT}" rev-parse HEAD)"
  echo "ab_pairs: ${AB_PAIRS}"
  echo "cpu_model: $(cpu_model)"
  echo "nproc: $(cpu_count)"
  echo "loadavg_before: $(loadavg_now)"
  echo "concurrent_running_processes_excluding_self: $(concurrent_processes_snapshot)"
  echo "bench_dedicated_env: ${BENCH_DEDICATED_ENV:-<unset>}"
  echo "docker_version: $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unavailable)"
  for img in pgvector/pgvector:pg17 qdrant/qdrant:latest mysql:9 \
    mongodb/mongodb-atlas-local:latest mongo:8 redis:8 \
    docker.elastic.co/elasticsearch/elasticsearch:9.1.4; do
    echo "docker_image[${img}]: $(docker_image_digest "${img}")"
  done
  echo "fs_type_crossdb_dir: $(fs_type_of "${CROSSDB_DIR}")"
} >"${OUT_DOC_DIR}/env.txt"

echo "writing per-round results under ${CROSSDB_DIR}/results/round<N>/ (logs: ${CROSSDB_DIR}/logs/round<N>/)"

FAILED_ROUNDS=()
for n in $(seq 1 "${AB_PAIRS}"); do
  echo "=== round ${n}/${AB_PAIRS} $(date -u +%H:%M:%SZ) loadavg=$(loadavg_now) ==="
  {
    echo "round_${n}_loadavg_start: $(loadavg_now)"
  } >>"${OUT_DOC_DIR}/env.txt"
  if ! CROSSDB_RUN_TAG="round${n}" bash "${REPO_ROOT}/scripts/crossdb_bench/run_all.sh"; then
    echo "round ${n}: run_all.sh reported failures (see logs/round${n}/*.log for FAILED entries)" >&2
    FAILED_ROUNDS+=("${n}")
  fi
  {
    echo "round_${n}_loadavg_end: $(loadavg_now)"
  } >>"${OUT_DOC_DIR}/env.txt"
done

if [ "${#FAILED_ROUNDS[@]}" -gt 0 ]; then
  echo "WARNING: rounds with at least one FAILED arm: ${FAILED_ROUNDS[*]} (see logs/round<N>/*.log). generated JSON for successful arms is still usable; do not silently drop missing arms." >&2
fi

echo "done: ${AB_PAIRS} rounds written under ${CROSSDB_DIR}/results/round*/"
echo "env record: ${OUT_DOC_DIR}/env.txt"
echo "summarize with: $0 --summarize ${CROSSDB_DIR} ${AB_PAIRS}"
