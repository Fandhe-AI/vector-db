#!/usr/bin/env bash
# Issue #655（本 Issue の担当。docs/design/scalar-index-mask-search.md
# 「前後比較実測（Issue #655）」節参照）の段別プロファイル前後比較ドライバ。
#
# Issue #654（PR #664）が SCALAR 事前フィルタ付き DISTANCE の候補行探索を
# 「新規 VectorArena へ複製してから探索する」経路から「候補 id マスクを
# 直接 SearchProvider::search_subset へ渡す（複製なし）」経路へ置き換えた。
# `docs/design/filtered-distance-stage-profile.md`（Issue #653・選択率 opt-in）
# は #654 適用後（HEAD）の in-binary 対照値のみを記録しており、#654 適用前
# バイナリとの交互 A/B は「#655 の担当」として申し送られていた。本スクリプトは
# `scripts/bench_hnsw_phase3_ab.sh` と同型の骨格（独立ソースツリー・独立
# CARGO_TARGET_DIR・cargo --message-format=json からの成果物特定・交互
# N≥5 ペア・1 プロセス = 1 run・loadavg 記録・env.txt）で before（#654 適用前）/
# after（#654 適用後。#663 の選択率 opt-in を含む HEAD 相当）2 本の
# `scan_stage_profile_bench` バイナリを輪番実行する。
#
# before（#654 適用前）バイナリは選択率 opt-in（`BENCH_SCAN_PROFILE_
# SELECTIVITY`。Issue #653）を持たず `lang` 列は固定 5 値輪番
# （`LANGS[id % 5]` ≒ 20%）のため、ペア計測は before/after 共通の既定選択率
# （1/5）で行う（ペア run へは `BENCH_SCAN_PROFILE_SELECTIVITY` を渡さない
# ——`lang_for_id` の既定分母 1/5 は旧 `LANGS` 輪番とビット同一であることが
# `tests/scan_stage_profile_accept.rs::lang_for_id_default_denominator_matches_legacy_langs_rotation`
# で固定済み）。after バイナリのみで計測できる crossdb fixture 相当の
# 選択率 33%（1/3）は、ペア完了後に after-only 段として別途連続実行する
# （#653 の in-binary 対照 `I2b_candidate_arena_copy` 605.1µs vs
# `I2b_candidate_mask_build` 137.5µs と併記する位置づけ。before バイナリは
# I2b/I3 に対応する I 系列出力自体を持たないため 1/3 でのペア比較は構造的に
# 不能——I2b/I3・`index_mask_scans_delta` は #654 が導入した契約でこの
# スクリプトが再現できるものではない）。
#
# production コード（`crates/engine/src/`）・既存ベンチハーネスは一切
# 変更しない。呼び出し元は人間の運用者（本ベンチは spec 閾値を持たない
# 情報提供専用のため CI 非配線・手動実行専用）。
#
# 使い方:
#   BEFORE_DIR=<path> AFTER_DIR=<path> \
#   BEFORE_COMMIT=<sha> AFTER_COMMIT=<sha> \
#     scripts/bench_filtered_distance_ab.sh [--summarize <dir> [session_ts]]
#
#   BEFORE_DIR／AFTER_DIR: `git archive <commit> | tar -x -C <dir>` 等で
#     用意した、それぞれ独立した Cargo.toml を持つソースツリー（相対パスも
#     可・起動直後に絶対パスへ解決する）。
#   BEFORE_COMMIT／AFTER_COMMIT: env.txt 記録用の commit sha（hex 7-40 桁）。
#   AB_PAIRS（既定 5・5 未満は拒否）: 既定選択率（1/5）でのペア数。
#   AB_ROUNDS（既定 5。`BENCH_SCAN_PROFILE_ROUNDS` として子プロセスへ渡す）。
#   AB_AFTER_ONLY_SELECTIVITY（既定 1/3。空文字で after-only 段を無効化。
#     `1/<2-100>` 形式のみ受理）。
#   OUT_DIR（既定 docs/design/bench-data/filtered-distance-mask-ab）。
#   BENCH_DEDICATED_ENV は子プロセスへそのまま継承する（専有環境自己申告）。
#
# 出力: <OUT_DIR>/<ts>-scan-profile-<before|after>-sel1of5-run<N>.log
#       <OUT_DIR>/<ts>-scan-profile-after-sel1of3-run<N>.log（after-only 段）
#       <OUT_DIR>/<ts>-scan-profile-loadavg.log（各 run 直前の /proc/loadavg）
#       <OUT_DIR>/<ts>-scan-profile-env.txt
#
# --summarize <dir> [session_ts] は `scripts/bench_filtered_distance_ab_summarize.py`
# へ委譲する（複数セッション混在時は session_ts の明示指定を要求する）。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir> [session_ts]}"
  if [ -n "${3:-}" ]; then
    exec python3 "${REPO_ROOT}/scripts/bench_filtered_distance_ab_summarize.py" "${DIR}" "${3}"
  fi
  exec python3 "${REPO_ROOT}/scripts/bench_filtered_distance_ab_summarize.py" "${DIR}"
fi

# GITHUB_ACTIONS 下は拒否する（時間依存・spec 閾値を持たない情報提供専用の
# 手動計測のため。既存 AB スクリプト群と同方針）。
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  die "this script is for manual, ad-hoc measurement only and must not run under GITHUB_ACTIONS"
fi

: "${BEFORE_DIR:?BEFORE_DIR must be set to the source tree of the before (#654 未適用) commit}"
: "${AFTER_DIR:?AFTER_DIR must be set to the source tree of the after (#654 適用後) commit}"
: "${BEFORE_COMMIT:?BEFORE_COMMIT must be set (e.g. 8225baa)}"
: "${AFTER_COMMIT:?AFTER_COMMIT must be set (e.g. 2488128 or a later no-op-equivalent HEAD)}"

[[ -f "${BEFORE_DIR}/Cargo.toml" ]] || die "BEFORE_DIR does not look like a source tree (no Cargo.toml): ${BEFORE_DIR}"
[[ -f "${AFTER_DIR}/Cargo.toml" ]] || die "AFTER_DIR does not look like a source tree (no Cargo.toml): ${AFTER_DIR}"

for v in BEFORE_COMMIT AFTER_COMMIT; do
  val="${!v}"
  [[ "${val}" =~ ^[0-9a-f]{7,40}$ ]] || die "${v} must be a hex commit sha (7-40 chars), got: ${val}"
done

# BEFORE_DIR／AFTER_DIR を絶対パスへ解決する（`bench_hnsw_phase3_ab.sh` と
# 同じ理由: 相対パスのまま `cd` 後に CARGO_TARGET_DIR を組み立てると
# 成果物の実際の出力先と本スクリプトの探索先が食い違う）。
BEFORE_DIR="$(cd "${BEFORE_DIR}" && pwd)"
AFTER_DIR="$(cd "${AFTER_DIR}" && pwd)"

AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  die "AB_PAIRS must be an integer >= 5, got: ${AB_PAIRS}"
fi

AB_ROUNDS="${AB_ROUNDS:-5}"
if ! [[ "${AB_ROUNDS}" =~ ^[0-9]+$ ]] || [ "${AB_ROUNDS}" -lt 5 ] || [ "${AB_ROUNDS}" -gt 50 ]; then
  die "AB_ROUNDS must be an integer within 5..50, got: ${AB_ROUNDS}"
fi

AB_AFTER_ONLY_SELECTIVITY="${AB_AFTER_ONLY_SELECTIVITY-1/3}"
if [ -n "${AB_AFTER_ONLY_SELECTIVITY}" ] && ! [[ "${AB_AFTER_ONLY_SELECTIVITY}" =~ ^1/([2-9]|[1-9][0-9])$ ]]; then
  die "AB_AFTER_ONLY_SELECTIVITY must be of the form 1/<2-100> or empty, got: ${AB_AFTER_ONLY_SELECTIVITY}"
fi

OUT_DIR="${OUT_DIR:-${REPO_ROOT}/docs/design/bench-data/filtered-distance-mask-ab}"
mkdir -p "${OUT_DIR}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"

LOADAVG_LOG="${OUT_DIR}/${TS}-scan-profile-loadavg.log"
: > "${LOADAVG_LOG}"
record_loadavg() {
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $1: $(cat /proc/loadavg 2>/dev/null || echo unavailable)" >> "${LOADAVG_LOG}"
}

# `cargo ... --message-format=json` の標準入力から compiler-artifact の
# 実行ファイルパスを取り出す（`bench_hnsw_phase3_ab.sh::locate_artifact` と同型）。
locate_artifact() {
  local name="$1"
  jq -r --arg name "${name}" \
    'select(.reason == "compiler-artifact" and .target.name == $name and .executable != null) | .executable' \
    | tail -n1
}

build_scan_stage_profile() {
  local dir="$1" target="$2"
  ( cd "${dir}" && CARGO_TARGET_DIR="${target}" cargo bench -p engine --bench scan_stage_profile_bench --no-run --message-format=json ) \
    | locate_artifact scan_stage_profile_bench
}

BEFORE_TARGET="${BEFORE_DIR}/target-fd-ab"
AFTER_TARGET="${AFTER_DIR}/target-fd-ab"

echo "building before (${BEFORE_COMMIT})..." >&2
BEFORE_BIN="$(build_scan_stage_profile "${BEFORE_DIR}" "${BEFORE_TARGET}")"
echo "building after (${AFTER_COMMIT})..." >&2
AFTER_BIN="$(build_scan_stage_profile "${AFTER_DIR}" "${AFTER_TARGET}")"
[[ -x "${BEFORE_BIN}" ]] || die "could not locate before scan_stage_profile_bench binary under ${BEFORE_TARGET}"
[[ -x "${AFTER_BIN}" ]] || die "could not locate after scan_stage_profile_bench binary under ${AFTER_TARGET}"

ENV_FILE="${OUT_DIR}/${TS}-scan-profile-env.txt"
{
  echo "timestamp_utc=${TS}"
  echo "harness_commit=$(git -C "${REPO_ROOT}" rev-parse HEAD)"
  echo "before_commit=${BEFORE_COMMIT}"
  echo "before_binary_sha256=$(sha256sum "${BEFORE_BIN}" | awk '{print $1}')"
  echo "after_commit=${AFTER_COMMIT}"
  echo "after_binary_sha256=$(sha256sum "${AFTER_BIN}" | awk '{print $1}')"
  echo "ab_pairs=${AB_PAIRS}"
  echo "ab_rounds=${AB_ROUNDS}"
  echo "ab_after_only_selectivity=${AB_AFTER_ONLY_SELECTIVITY:-(disabled)}"
  echo "nproc=$(nproc)"
  echo "cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | sed 's/^[^:]*: //' || echo unavailable)"
  echo "bench_dedicated_env=${BENCH_DEDICATED_ENV:-}"
  echo "github_actions=${GITHUB_ACTIONS:-}"
  echo "note=shared QEMU environment; treat as reference-only, not a pass/fail basis (docs/design/benchmark-judgement-policy.md §5)"
} > "${ENV_FILE}"

# --- ペア計測（既定選択率 1/5。before/after 共通・selectivity opt-in を渡さない）。
for pair in $(seq 1 "${AB_PAIRS}"); do
  for side in before after; do
    bin="${BEFORE_BIN}"; [[ "${side}" == after ]] && bin="${AFTER_BIN}"
    log="${OUT_DIR}/${TS}-scan-profile-${side}-sel1of5-run${pair}.log"
    [[ -e "${log}" ]] && die "raw log already exists (refusing to overwrite): ${log}"
    record_loadavg "scan-profile/${side}/sel1of5/run${pair}"
    BENCH_SCAN_PROFILE_ROUNDS="${AB_ROUNDS}" "${bin}" --bench > "${log}" 2>&1
  done
done

# --- after-only 段（crossdb fixture 相当の選択率 33%。I 系列・
# index_mask_scans_delta を含む #654 適用後専用の内訳を、#653 の
# in-binary 対照値と併記する位置づけで記録する）。
if [ -n "${AB_AFTER_ONLY_SELECTIVITY}" ]; then
  for pair in $(seq 1 "${AB_PAIRS}"); do
    log="${OUT_DIR}/${TS}-scan-profile-after-sel1of3-run${pair}.log"
    [[ -e "${log}" ]] && die "raw log already exists (refusing to overwrite): ${log}"
    record_loadavg "scan-profile/after/sel1of3/run${pair}"
    BENCH_SCAN_PROFILE_ROUNDS="${AB_ROUNDS}" BENCH_SCAN_PROFILE_SELECTIVITY="${AB_AFTER_ONLY_SELECTIVITY}" \
      "${AFTER_BIN}" --bench > "${log}" 2>&1
  done
fi

echo "done: results under ${OUT_DIR} (timestamp prefix ${TS})"
