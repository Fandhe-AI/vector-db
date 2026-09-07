#!/usr/bin/env bash
# Issue #516（f16 常駐の前後比較〔25k／100k／500k 行・dim 128／768〕と常駐
# メモリの記録）の交互計測ドライバ。`crates/engine/benches/knn_profile_bench.rs`
# の hot-only モード（`BENCH_KNN_PROFILE_HOT_ONLY=1`）・索引単体メモリモード
# （`BENCH_KNN_PROFILE_INDEX_MEMORY=1`）を、`scripts/bench_knn_visible_ratio_sweep.sh`
# と同じ「pair を外側・candidate を内側」の輪番構造で実行する
# （計測規約 `docs/design/benchmark-judgement-policy.md` §3〜§5 の
# 「交互 N≥5 ペア・per-run 生データ必須」に従う）。
#
# 計測順序（要件 A〔前後比較〕を要件 B〔常駐メモリ〕より先に完走させる。
# 索引単体メモリモードは 500k×768 点で VmHWM 約 4.7 GiB に達するため、
# 先に実行すると OOM 等でプロセスが落ちた場合に主目的の A/B（hot-only
# レイテンシ）が一切測定されずに終わる恐れがある——`docs/design/
# hnsw-f16-resident.md` の受け入れ条件 1 が本 Issue の主目的のため）:
#   1. hot-only レイテンシ（25k dim128 → 100k dim128 → 500k dim128 →
#      25k dim768 → 100k dim768。各点で pair 回、baseline(brute_force)→
#      hnsw→baseline→hnsw_f16 の輪番）
#   2. 索引単体メモリモード（5 SQL 到達可能点 × {hnsw, hnsw_f16} × 2 回）
#   3. 500k×768 の索引単体メモリのみ（SQL 表層は arena 1 GiB 上限で
#      構造的に到達不能。`docs/design/hnsw-f16-resident.md` 参照）
#
# 使い方: scripts/bench_knn_f16_resident_ab.sh [--summarize <dir>]
#   env AB_PAIRS=<N>（既定 5。5 未満は拒否）
#   env AB_POINTS="scale:dim scale:dim ..."（既定 "1:128 4:128 20:128 1:768 4:768"）
#   env AB_MEMORY_POINTS="scale:dim ..."（既定 AB_POINTS ＋ "20:768"）
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir>}"
  # per-run ログから測定行を 1 行 1 レコードの TSV へ集約する（生データは
  # ログとして保持し続ける。PR #580 の codex-review 指摘の慣行を踏襲）。
  {
    printf 'file\tkind\tfields\n'
    grep -H -E "stage\(S0_hot_sql_e2e\)|stage\(S0prime_count_star\)|^raw\(|hnsw_stats |resident_precision |index_memory |index_warm_ms" \
      "${DIR}"/*.log 2>/dev/null | \
      sed -E 's#^([^:]+):#\1\t#' | \
      awk -F'\t' '{
        rest=$2
        kind="other"
        if (rest ~ /^stage\(/) kind="stage"
        else if (rest ~ /^raw\(/) kind="raw"
        else if (rest ~ /hnsw_stats /) kind="hnsw_stats"
        else if (rest ~ /resident_precision /) kind="resident_precision"
        else if (rest ~ /index_memory /) kind="index_memory"
        else if (rest ~ /index_warm_ms=/) kind="index_warm_ms"
        printf "%s\t%s\t%s\n", $1, kind, rest
      }'
  }
  exit 0
fi

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "ERROR: refusing to run under GITHUB_ACTIONS (CI 非配線・手動専用。Issue #516)" >&2
  exit 1
fi

AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  echo "ERROR: AB_PAIRS must be an integer >= 5 (benchmark-judgement-policy.md §3), got: ${AB_PAIRS}" >&2
  exit 1
fi

# デフォルトの規模点: 25k/100k/500k × dim128、25k/100k × dim768
# （500k×768 は SQL 表層〔hot-only〕では arena 1 GiB 上限〔MAX_ARENA_TOTAL_BYTES〕
# により構造的に到達不能。`docs/design/hnsw-f16-resident.md` 参照）。
DEFAULT_POINTS="1:128 4:128 20:128 1:768 4:768"
read -r -a AB_POINTS_ARR <<<"${AB_POINTS:-${DEFAULT_POINTS}}"
# AB_MEMORY_POINTS の既定は「AB_POINTS ＋ 20:768」（AB_POINTS を上書きした
# 場合はそれに追随する。DEFAULT_POINTS 固定ではない）。
read -r -a AB_MEMORY_POINTS_ARR <<<"${AB_MEMORY_POINTS:-${AB_POINTS_ARR[*]} 20:768}"

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${REPO_ROOT}/target/bench-knn-f16-resident/${TS}"
mkdir -p "${OUT_DIR}"

{
  echo "commit=$(cd "${REPO_ROOT}" && git rev-parse HEAD)"
  echo "nproc=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
  echo "BENCH_DEDICATED_ENV=${BENCH_DEDICATED_ENV:-<unset>}"
  if [ -r /proc/cpuinfo ]; then
    echo "cpu_model=$(grep -m1 '^model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
    echo "cpu_flags=$(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
  fi
  echo "ab_pairs=${AB_PAIRS}"
  echo "ab_points=${AB_POINTS_ARR[*]}"
  echo "ab_memory_points=${AB_MEMORY_POINTS_ARR[*]}"
} >"${OUT_DIR}/env.txt"

# 以降のすべての `cargo bench` 呼び出しをリポジトリルートから実行する
# （`scripts/bench_knn_visible_ratio_sweep.sh` と同型。本スクリプトを
# 別ディレクトリ・別 CWD から絶対パスで起動しても Cargo.toml を確実に
# 見つけられるようにするため、以降の全 cargo 呼び出しより前に置く）。
cd "${REPO_ROOT}"

echo "building knn_profile_bench (release, once)"
cargo bench --bench knn_profile_bench -p engine --no-run

log_noise() {
  local log="$1"
  echo "loadavg=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo n/a)" >"${log}"
}

# --- 1. hot-only レイテンシ（交互 N≥5 ペア）---------------------------------
# `slot` はログファイル名の一意化のみに使う（同一 pair 内で brute_force を
# 2 回計測するため、`engine` だけではファイル名が衝突し `log_noise` の
# 上書き〔`>`〕で 1 回目の計測が失われる。2026-09-07 の初回計測でこの衝突が
# 発生し、`hot_*_brute_force_pair*.log` は「hnsw_f16 の直前」の 1 回分しか
# 残らなかった。前後比較の主対象は hnsw/hnsw_f16 であり brute_force は補助
# 系列のため、既存ログはそのまま「pair あたり 1 回（hnsw_f16 直前）」の値
# として扱い、本修正後の新規計測も同じ位置〔beforehnswf16〕で揃える。
# 詳細は `docs/design/hnsw-f16-resident.md`「Issue #516 追記」節参照）。
run_hot_only() {
  local scale="$1" dim="$2" engine="$3" pair="$4" slot="$5"
  local log="${OUT_DIR}/hot_scale${scale}_dim${dim}_${engine}${slot}_pair${pair}.log"
  log_noise "${log}"
  BENCH_KNN_PROFILE_HOT_ONLY=1 \
    BENCH_KNN_PROFILE_ENGINE="${engine}" \
    BENCH_KNN_PROFILE_SCALE="${scale}" \
    BENCH_KNN_PROFILE_DIM="${dim}" \
    cargo bench --bench knn_profile_bench -p engine >>"${log}" 2>&1
}

for point in "${AB_POINTS_ARR[@]}"; do
  scale="${point%%:*}"
  dim="${point##*:}"
  for pair in $(seq 1 "${AB_PAIRS}"); do
    echo "run: hot_only scale=${scale} dim=${dim} arm=brute_force(before hnsw) pair=${pair}"
    run_hot_only "${scale}" "${dim}" "brute_force" "${pair}" "_beforehnsw"
    echo "run: hot_only scale=${scale} dim=${dim} arm=hnsw pair=${pair}"
    run_hot_only "${scale}" "${dim}" "hnsw" "${pair}" ""
    echo "run: hot_only scale=${scale} dim=${dim} arm=brute_force(before hnsw_f16) pair=${pair}"
    run_hot_only "${scale}" "${dim}" "brute_force" "${pair}" "_beforehnswf16"
    echo "run: hot_only scale=${scale} dim=${dim} arm=hnsw_f16 pair=${pair}"
    run_hot_only "${scale}" "${dim}" "hnsw_f16" "${pair}" ""
  done
done

# --- 2. 索引単体メモリモード（500k×768 点で VmHWM 約 4.7 GiB。上記 1 の
# 完走後に実行する——順序の理由は本ファイル冒頭コメント参照）。-------------
for point in "${AB_MEMORY_POINTS_ARR[@]}"; do
  scale="${point%%:*}"
  dim="${point##*:}"
  for engine in hnsw hnsw_f16; do
    for rep in 1 2; do
      log="${OUT_DIR}/mem_scale${scale}_dim${dim}_${engine}_rep${rep}.log"
      echo "run: memory scale=${scale} dim=${dim} engine=${engine} rep=${rep}"
      log_noise "${log}"
      BENCH_KNN_PROFILE_INDEX_MEMORY=1 \
        BENCH_KNN_PROFILE_ENGINE="${engine}" \
        BENCH_KNN_PROFILE_SCALE="${scale}" \
        BENCH_KNN_PROFILE_DIM="${dim}" \
        cargo bench --bench knn_profile_bench -p engine >>"${log}" 2>&1
    done
  done
done

echo "done. logs in ${OUT_DIR}"
echo "summarize with: scripts/bench_knn_f16_resident_ab.sh --summarize ${OUT_DIR}"
