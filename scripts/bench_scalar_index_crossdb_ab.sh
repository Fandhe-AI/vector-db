#!/usr/bin/env bash
# Issue #633（本 Issue の担当。docs/design/scalar-index-generation-cache.md
# 「前後比較実測（Issue #633）」節参照）の交互実行ドライバ。
#
# Issue #632（PR #638・merge commit `6ff22dc`）が `ScalarIndex::build`
# （`crates/engine/src/sql/scalar_index.rs`）へ追加した列単位の平均値長
# ゲート（`MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`。#632 当初導入時は 128・
# #644 で crossdb fixture 実測〔126.3 バイト〕を確実に上回る 64 へ
# 引き下げ済み。長文 `body` 列を索引対象から除外する）について、
# before・after・ref の 3 コミットを `git archive` で独立ソースツリーへ
# 展開・`cargo build --release -p wire-server` で個別ビルドし、
# `scripts/crossdb_bench/run.py --db self --config exact`（crossdb self
# 全フェーズ）と `scripts/crossdb_bench/hybrid_after_where.py`
# （WHERE 実行後の hybrid_rrf 単独ループ・RSS）を輪番実行する
# （計測規約 `docs/design/benchmark-judgement-policy.md` §3〜§5 の
# 「交互 N≥5 ペア・per-run 生データ必須・min-of-N＋median 併記・2 種ノイズ帯・
# 共有環境は参考値」に従う）。ハーネス（`scripts/crossdb_bench/*.py`）は
# 現行ワークツリーのものを全 arm で共通使用し、`CROSSDB_SELF_BINARY` で
# 起動するバイナリのみ差し替える（Issue #479 の方式）。
#
# 呼び出し例:
#   - Issue #633（閾値 128 導入時の no-op 確認）:
#     BEFORE_COMMIT=773a835 AFTER_COMMIT=6ff22dc REF_COMMIT=ee99db3
#   - Issue #645（閾値 128→64 見直し後の効果確認）:
#     BEFORE_COMMIT=cbe80cf AFTER_COMMIT=2f1cd80 REF_COMMIT=ee99db3
#   - Issue #655（候補 id マスク経路〔Issue #654〕の前後比較。REF_COMMIT は
#     時間短縮のため空文字で無効化した）:
#     BEFORE_COMMIT=8225baa AFTER_COMMIT=2488128 REF_COMMIT=""
#
# 3 arm（before/after/ref）比較時の輪番は `benchmark-judgement-policy.md`
# §3「baseline/cand1/baseline/cand2/… の輪番」に従い、1 ペアあたり
# before→after→before_ref→ref の順で実行する（`before_ref` は `before` と
# 同一バイナリだが ref 専用の baseline として別ラベルで記録し、ref との
# 比較が after 計測ぶん時間的に隔たった before を参照しないようにする。
# codex-review P1 指摘・Issue #633）。REF_COMMIT を無効化した場合は
# before→after のみの通常の 2 arm 交互実行になる。
#
# production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
# 一切変更しない（本スクリプト自体・生成物はテスト・ベンチ専任）。
#
# 使い方:
#   BEFORE_COMMIT=<sha> AFTER_COMMIT=<sha> CROSSDB_DIR=<dir> \
#     CROSSDB_PYTHON=<python> scripts/bench_scalar_index_crossdb_ab.sh
#   REF_COMMIT=<sha>（既定 ee99db3。空文字を指定すると ref arm を無効化する）
#   AB_PAIRS=<N>（既定 5・5 未満は拒否）
#   HYBRID_ITERS=<N>（既定 200・hybrid_after_where.py --iters に渡す）
#   WARM_WHERE=<N>（既定 50・hybrid_after_where.py --warm-where に渡す）
#   CROSSDB_SELF_PORT_BASE=<port>（既定 15437。並走ジョブとの衝突回避のため
#     既定の 15432 以外を使う）
#   OUT_DIR（既定 docs/design/bench-data/scalar-index-crossdb-ab）
#
# 出力: <OUT_DIR>/<ts>-crossdb-<arm>-run<N>/self_exact.json
#       <OUT_DIR>/<ts>-hybrid-<mode>-<arm>-run<N>.json
#       <OUT_DIR>/<ts>-loadavg.log
#       <OUT_DIR>/<ts>-env.txt
#
# --summarize <dir> [session_ts] で TSV 集約（区間別 min-of-N・median・ratio・
# reference_band）。<dir> に複数回の実行（別 ts）の生データが混在する場合は
# session_ts（`<ts>-crossdb-*`／`<ts>-hybrid-*` の接頭辞）を明示指定する
# 必要がある（Issue #633 codex-review P2 指摘。別セッションの生データが
# 混ざると before/after の min-of-N が比較不能になるため、summarize 側が
# fail-closed に拒否する）。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir> [session_ts]}"
  if [ -n "${3:-}" ]; then
    exec python3 "${REPO_ROOT}/scripts/bench_scalar_index_crossdb_ab_summarize.py" "${DIR}" "${3}"
  fi
  exec python3 "${REPO_ROOT}/scripts/bench_scalar_index_crossdb_ab_summarize.py" "${DIR}"
fi

# GITHUB_ACTIONS 下は拒否する（本ベンチは時間依存・spec 閾値を持たない
# 情報提供専用のため CI 非配線・手動実行専用。既存 AB スクリプト群と同方針）。
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  die "this script is for manual, ad-hoc measurement only and must not run under GITHUB_ACTIONS"
fi

: "${BEFORE_COMMIT:?BEFORE_COMMIT must be set (e.g. 773a835)}"
: "${AFTER_COMMIT:?AFTER_COMMIT must be set (e.g. 6ff22dc)}"
: "${CROSSDB_DIR:?CROSSDB_DIR must be set to the directory containing docs25k.redb/docs25k.jsonl/queries200.jsonl}"
: "${CROSSDB_PYTHON:?CROSSDB_PYTHON must be set to a python3 interpreter with psycopg installed}"

REF_COMMIT="${REF_COMMIT-ee99db3}"

for v in BEFORE_COMMIT AFTER_COMMIT; do
  val="${!v}"
  [[ "${val}" =~ ^[0-9a-f]{7,40}$ ]] || die "${v} must be a hex commit sha (7-40 chars), got: ${val}"
done
if [ -n "${REF_COMMIT}" ]; then
  [[ "${REF_COMMIT}" =~ ^[0-9a-f]{7,40}$ ]] || die "REF_COMMIT must be a hex commit sha (7-40 chars) or empty, got: ${REF_COMMIT}"
fi

AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  die "AB_PAIRS must be an integer >= 5, got: ${AB_PAIRS}"
fi

HYBRID_ITERS="${HYBRID_ITERS:-200}"
if ! [[ "${HYBRID_ITERS}" =~ ^[0-9]+$ ]] || [ "${HYBRID_ITERS}" -lt 1 ] || [ "${HYBRID_ITERS}" -gt 5000 ]; then
  die "HYBRID_ITERS must be an integer within 1..5000, got: ${HYBRID_ITERS}"
fi
WARM_WHERE="${WARM_WHERE:-50}"
if ! [[ "${WARM_WHERE}" =~ ^[0-9]+$ ]] || [ "${WARM_WHERE}" -gt 5000 ]; then
  die "WARM_WHERE must be an integer within 0..5000, got: ${WARM_WHERE}"
fi
CROSSDB_SELF_PORT_BASE="${CROSSDB_SELF_PORT_BASE:-15437}"
if ! [[ "${CROSSDB_SELF_PORT_BASE}" =~ ^[0-9]+$ ]] || [ "${CROSSDB_SELF_PORT_BASE}" -lt 1 ] || [ "${CROSSDB_SELF_PORT_BASE}" -gt 65535 ]; then
  die "CROSSDB_SELF_PORT_BASE must be a valid port, got: ${CROSSDB_SELF_PORT_BASE}"
fi

CROSSDB_DIR="$(cd "${CROSSDB_DIR}" && pwd)"
[[ -f "${CROSSDB_DIR}/docs25k.redb" ]] || die "docs25k.redb not found under CROSSDB_DIR: ${CROSSDB_DIR}"
[[ -f "${CROSSDB_DIR}/docs25k.jsonl" ]] || die "docs25k.jsonl not found under CROSSDB_DIR: ${CROSSDB_DIR}"
[[ -f "${CROSSDB_DIR}/queries200.jsonl" ]] || die "queries200.jsonl not found under CROSSDB_DIR: ${CROSSDB_DIR}"
command -v "${CROSSDB_PYTHON}" >/dev/null 2>&1 || die "CROSSDB_PYTHON not executable: ${CROSSDB_PYTHON}"
command -v jq >/dev/null 2>&1 || die "jq is required"

OUT_DIR="${OUT_DIR:-${REPO_ROOT}/docs/design/bench-data/scalar-index-crossdb-ab}"
mkdir -p "${OUT_DIR}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"

SCRATCH="$(mktemp -d)"
cleanup() { rm -rf "${SCRATCH}"; }
trap cleanup EXIT

LOADAVG_LOG="${OUT_DIR}/${TS}-loadavg.log"
: > "${LOADAVG_LOG}"
record_loadavg() {
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $1: $(cat /proc/loadavg 2>/dev/null || echo unavailable)" >> "${LOADAVG_LOG}"
}

# `cargo ... --message-format=json` の標準入力から wire-server バイナリの
# パスを取り出す（`bench_hnsw_phase3_ab.sh::locate_artifact` と同型）。
locate_artifact() {
  jq -r --arg name "wire-server" \
    'select(.reason == "compiler-artifact" and .target.name == $name and .executable != null) | .executable' \
    | tail -n1
}

# arm（before/after/ref）1 つぶんのソースツリーを用意し wire-server を
# ビルドしてバイナリパス・sha256 を返す（env.txt へ記録するため）。
prepare_arm() {
  local commit="$1" wt="${SCRATCH}/wt-$2" target="${SCRATCH}/target-$2"
  mkdir -p "${wt}"
  git -C "${REPO_ROOT}" archive "${commit}" | tar -x -C "${wt}"
  local bin
  bin="$(cd "${wt}" && CARGO_TARGET_DIR="${target}" cargo build --release -p wire-server --message-format=json | locate_artifact)"
  [[ -x "${bin}" ]] || die "could not locate wire-server binary for commit ${commit} under ${target}"
  echo "${bin}"
}

echo "building before (${BEFORE_COMMIT})..." >&2
BEFORE_BIN="$(prepare_arm "${BEFORE_COMMIT}" before)"
echo "building after (${AFTER_COMMIT})..." >&2
AFTER_BIN="$(prepare_arm "${AFTER_COMMIT}" after)"
REF_BIN=""
if [ -n "${REF_COMMIT}" ]; then
  echo "building ref (${REF_COMMIT})..." >&2
  REF_BIN="$(prepare_arm "${REF_COMMIT}" ref)"
fi

ENV_FILE="${OUT_DIR}/${TS}-env.txt"
{
  echo "timestamp_utc=${TS}"
  echo "harness_commit=$(git -C "${REPO_ROOT}" rev-parse HEAD)"
  echo "before_commit=${BEFORE_COMMIT}"
  echo "before_binary_sha256=$(sha256sum "${BEFORE_BIN}" | awk '{print $1}')"
  echo "after_commit=${AFTER_COMMIT}"
  echo "after_binary_sha256=$(sha256sum "${AFTER_BIN}" | awk '{print $1}')"
  if [ -n "${REF_COMMIT}" ]; then
    echo "ref_commit=${REF_COMMIT}"
    echo "ref_binary_sha256=$(sha256sum "${REF_BIN}" | awk '{print $1}')"
  else
    echo "ref_commit=(disabled)"
  fi
  echo "ab_pairs=${AB_PAIRS}"
  echo "hybrid_iters=${HYBRID_ITERS}"
  echo "warm_where=${WARM_WHERE}"
  echo "self_port=${CROSSDB_SELF_PORT_BASE}"
  echo "nproc=$(nproc)"
  echo "cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | sed 's/^[^:]*: //' || echo unavailable)"
  echo "bench_dedicated_env=${BENCH_DEDICATED_ENV:-}"
  echo "github_actions=${GITHUB_ACTIONS:-}"
  echo "note=shared QEMU environment; treat as reference-only, not a pass/fail basis (docs/design/benchmark-judgement-policy.md §5)"
} > "${ENV_FILE}"

# 候補（before と比較する対象）の一覧と、候補ごとの直近 baseline arm 名
# （`benchmark-judgement-policy.md` §3「baseline/cand1/baseline/cand2/…
# の輪番」codex-review P1 指摘）。3 arm 目（ref）を比較に混ぜる際、
# before→after→ref の順で before を 1 回しか取らないと ref との比較が
# after 計測ぶん時間的に隔たった before を参照することになり、交互実行が
# 前提とする時間方向の対称性が崩れる。そのため候補ごとに専用の baseline
# 計測（before は after 用、before_ref は ref 用）を用意し、実行順は
# 1 ペアあたり before→after→before_ref→ref（baseline/cand1/baseline/cand2）
# とする。`before_ref` は `before` と同一バイナリ（`BEFORE_BIN`）を使う
# 別ラベルの出力（summarize 側で ref 専用の baseline 系列として扱う）。
CANDIDATES=(after)
[ -n "${REF_COMMIT}" ] && CANDIDATES+=(ref)

baseline_for_candidate() {
  case "$1" in
    after) echo "before" ;;
    ref) echo "before_ref" ;;
    *) die "unknown candidate: $1" ;;
  esac
}

bin_for_arm() {
  case "$1" in
    before | before_ref) echo "${BEFORE_BIN}" ;;
    after) echo "${AFTER_BIN}" ;;
    ref) echo "${REF_BIN}" ;;
    *) die "unknown arm: $1" ;;
  esac
}

run_crossdb_self() {
  local arm="$1" pair="$2"
  local out_run_dir="${OUT_DIR}/${TS}-crossdb-${arm}-run${pair}"
  [[ -e "${out_run_dir}" ]] && die "output dir already exists (refusing to overwrite): ${out_run_dir}"
  mkdir -p "${out_run_dir}"
  local workdir="${SCRATCH}/work-${arm}-${pair}"
  mkdir -p "${workdir}"
  record_loadavg "crossdb-self/${arm}/run${pair}"
  CROSSDB_SELF_BINARY="$(bin_for_arm "${arm}")" \
  CROSSDB_SELF_PORT="${CROSSDB_SELF_PORT_BASE}" \
    "${CROSSDB_PYTHON}" "${REPO_ROOT}/scripts/crossdb_bench/run.py" \
      --db self --config exact \
      --rows-file "${CROSSDB_DIR}/docs25k.redb" \
      --queries-file "${CROSSDB_DIR}/queries200.jsonl" \
      --docs-file "${CROSSDB_DIR}/docs25k.jsonl" \
      --out-dir "${out_run_dir}" \
      --workdir "${workdir}" \
      > "${out_run_dir}/stdout.log" 2>&1
  # `common.py::write_result` は末尾改行を付けずに書き出すため、tracked
  # 生データとして保存する前に editorconfig（`insert_final_newline`）へ
  # 合わせる（本ドライバ固有の後処理。`common.py` 自体は変更しない）。
  if [[ -s "${out_run_dir}/self_exact.json" ]] && [[ "$(tail -c1 "${out_run_dir}/self_exact.json")" != "" ]]; then
    printf '\n' >> "${out_run_dir}/self_exact.json"
  fi
}

run_hybrid_after_where() {
  local arm="$1" pair="$2" mode="$3"
  local out_file="${OUT_DIR}/${TS}-hybrid-${mode}-${arm}-run${pair}.json"
  [[ -e "${out_file}" ]] && die "raw output already exists (refusing to overwrite): ${out_file}"
  local workdir="${SCRATCH}/hybrid-${arm}-${pair}-${mode}"
  mkdir -p "${workdir}"
  record_loadavg "hybrid-${mode}/${arm}/run${pair}"
  CROSSDB_SELF_BINARY="$(bin_for_arm "${arm}")" \
  CROSSDB_SELF_PORT="${CROSSDB_SELF_PORT_BASE}" \
    "${CROSSDB_PYTHON}" "${REPO_ROOT}/scripts/crossdb_bench/hybrid_after_where.py" \
      --rows-file "${CROSSDB_DIR}/docs25k.redb" \
      --queries-file "${CROSSDB_DIR}/queries200.jsonl" \
      --docs-file "${CROSSDB_DIR}/docs25k.jsonl" \
      --mode "${mode}" \
      --iters "${HYBRID_ITERS}" \
      --warm-where "${WARM_WHERE}" \
      --workdir "${workdir}" \
      --out "${out_file}"
}

run_arm_full() {
  local arm="$1" pair="$2"
  echo "== pair ${pair} / arm ${arm} ==" >&2
  run_crossdb_self "${arm}" "${pair}"
  run_hybrid_after_where "${arm}" "${pair}" hybrid
  run_hybrid_after_where "${arm}" "${pair}" warm_where_then_hybrid
  run_hybrid_after_where "${arm}" "${pair}" body_predicate
}

# baseline/cand1/baseline/cand2/… の輪番（`benchmark-judgement-policy.md`
# §3。codex-review P1 指摘）。候補ごとに専用の baseline 計測を直前に
# 挟むことで、各候補が自身専用の baseline と時間的に隣接した対で比較
# される（REF_COMMIT 無効時は before→after のみで実質 2 arm の交互実行）。
for pair in $(seq 1 "${AB_PAIRS}"); do
  for candidate in "${CANDIDATES[@]}"; do
    baseline="$(baseline_for_candidate "${candidate}")"
    run_arm_full "${baseline}" "${pair}"
    run_arm_full "${candidate}" "${pair}"
  done
done

echo "done: results under ${OUT_DIR} (timestamp prefix ${TS})"
