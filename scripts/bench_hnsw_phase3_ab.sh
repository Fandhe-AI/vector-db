#!/usr/bin/env bash
# Issue #507: Phase 3（#458 ツリー・ルート #455。ANN／HNSW の構築並列化・
# 探索メモリ局所性・フィルタ付き探索の全施策 #489〜#503）通しの前後比較を
# 交互実行 min-of-N（既定 5 ペア）で計測するドライバ。
#
# `docs/design/benchmark-judgement-policy.md`（Issue #462 SSOT）に従い、
# ワークロードごとに before → after の順で 1 ペアずつ交互実行する
# （逐次 before×N → after×N ではなく、ペア単位で往復させることで計測環境の
# ドリフトが片側だけに偏らないようにする）。集計（min-of-N・median-of-N・
# 参照区間帯との突き合わせ）はこのスクリプトの責務外——`--summarize` は
# 各 run の該当行をそのまま列挙するだけで、判定・平均化は行わない
# （production コードへの閾値埋め込みを避ける方針。Issue #303 と同方針）。
#
# 使い方:
#   BEFORE_KNN_BIN=/path/to/before/knn_profile_bench \
#   AFTER_KNN_BIN=/path/to/after/knn_profile_bench \
#   BEFORE_COMPARE_BIN=/path/to/before/hnsw_compare_bench \
#   AFTER_COMPARE_BIN=/path/to/after/hnsw_compare_bench \
#   BEFORE_FEATURE_BIN=/path/to/before/feature_bench \
#   AFTER_FEATURE_BIN=/path/to/after/feature_bench \
#   BEFORE_COMMIT=<sha> AFTER_COMMIT=<sha> \
#   scripts/bench_hnsw_phase3_ab.sh
#
#   scripts/bench_hnsw_phase3_ab.sh --summarize target/bench-hnsw-phase3-ab/<ts>
#
# env:
#   BEFORE_KNN_BIN / AFTER_KNN_BIN          knn_profile_bench の実行ファイル
#   BEFORE_COMPARE_BIN / AFTER_COMPARE_BIN  hnsw_compare_bench の実行ファイル
#   BEFORE_FEATURE_BIN / AFTER_FEATURE_BIN  feature_bench の実行ファイル
#     （AB_WORKLOADS で選択したワークロードに対応する変数のみ必須。他は未検証）
#   BEFORE_COMMIT / AFTER_COMMIT     計測対象バイナリをビルドしたコミットの
#                                     hash（必須。benchmark-judgement-policy.md
#                                     §3 が前後双方のコミット hash 記録を要求）
#   AB_PAIRS                          ワークロードあたりの交互ペア数（既定 5。
#                                     5 未満は拒否）
#   AB_WORKLOADS                      空白区切りのワークロード集合（既定
#                                     "knn_profile hnsw_compare feature_1"。
#                                     "feature_4" は opt-in・許可リスト検証）
#   OUT_DIR                           出力先（既定
#                                     target/bench-hnsw-phase3-ab/<UTC ts>）
#
# 手動専用ベンチのドライバであり、`.github/workflows/*` から呼ばれることは
# ない（`refuse_under_github_actions` と同じ方針で GITHUB_ACTIONS 下は拒否）。
set -euo pipefail

fail() {
    echo "bench_hnsw_phase3_ab.sh: $*" >&2
    exit 1
}

if [ "${GITHUB_ACTIONS:-}" != "" ]; then
    fail "refused while running under GitHub Actions (GITHUB_ACTIONS is set); this driver is local-only"
fi

# --summarize の走査順（ワークロード → ペア番号数値昇順 → before/after）を
# ドライバ本体と共有する単一情報源（許可リストも兼ねる）。
all_workloads="knn_profile hnsw_compare feature_1 feature_4"

is_known_workload() {
    local w="$1" known
    for known in $all_workloads; do
        [ "$w" = "$known" ] && return 0
    done
    return 1
}

if [ "${1:-}" = "--summarize" ]; then
    dir="${2:-}"
    [ -n "$dir" ] || fail "--summarize requires a directory argument"
    [ -d "$dir" ] || fail "not a directory: $dir"
    echo "=== stage()/hnsw_stats/hnsw_compare/feature_bench json + per-run loadavg lines under $dir ==="
    found=0
    for wl in $all_workloads; do
        [ -d "$dir/$wl" ] || continue
        pair_numbers=""
        for f in "$dir/$wl"/pair*-before*.log; do
            [ -e "$f" ] || continue
            base="$(basename "$f")"
            pair_num="${base#pair}"
            pair_num="${pair_num%%-*}"
            case "$pair_num" in
                ''|*[!0-9]*) continue ;;
            esac
            pair_numbers="$pair_numbers $pair_num"
        done
        pair_numbers="$(printf '%s\n' $pair_numbers | sort -n -u)"
        for pair_num in $pair_numbers; do
            for side in before after; do
                for f in "$dir/$wl"/"pair${pair_num}-${side}"*.log; do
                    [ -e "$f" ] || continue
                    if grep -H -E '^stage\(|^knn_profile_bench: hnsw_stats|^hnsw_compare:|^# loadavg_before_run=|"phase"|^feature_bench:' "$f"; then
                        found=1
                    fi
                done
            done
        done
    done
    [ "$found" -eq 1 ] || fail "no matching log lines found under $dir"
    exit 0
fi

: "${BEFORE_COMMIT:?BEFORE_COMMIT must be set to the commit hash the before binaries were built from}"
: "${AFTER_COMMIT:?AFTER_COMMIT must be set to the commit hash the after binaries were built from}"

validate_positive_int() {
    case "$1" in
        ''|*[!0-9]*) fail "$2 must be a positive integer (got $1)" ;;
    esac
    [ "$1" -ge 1 ] || fail "$2 must be >= 1 (got $1)"
}

AB_PAIRS="${AB_PAIRS:-5}"
validate_positive_int "$AB_PAIRS" "AB_PAIRS"
[ "$AB_PAIRS" -ge 5 ] || fail "AB_PAIRS must be >= 5 per docs/design/benchmark-judgement-policy.md §3 (got $AB_PAIRS)"
[ "$AB_PAIRS" -le 50 ] || fail "AB_PAIRS must be <= 50 (got $AB_PAIRS)"

AB_WORKLOADS="${AB_WORKLOADS:-knn_profile hnsw_compare feature_1}"
for w in $AB_WORKLOADS; do
    is_known_workload "$w" || fail "unknown workload in AB_WORKLOADS: $w (allowed: $all_workloads)"
done

# 各 run 直前の同時実行プロセスのスナップショット（benchmark-judgement-policy.md
# §3「同一プロセス条件」の記録要件）。bench_hybrid_profile_ab.sh と同一実装。
record_concurrent_processes() {
    local self_pid=$$ snapshot
    if ! snapshot="$(ps -eo pid=,stat=,pcpu=,comm= 2>/dev/null)"; then
        echo "# running_processes_excluding_self=unknown (ps unavailable)"
        echo "# top_cpu_processes=unknown (ps unavailable)"
        return 0
    fi
    local running top
    running="$(printf '%s\n' "$snapshot" | awk -v self="$self_pid" '$1 != self && $4 != "ps" && $2 ~ /^R/ {n++} END {print n+0}')"
    top="$(printf '%s\n' "$snapshot" \
        | awk -v self="$self_pid" '$1 != self && $4 != "ps" {print $3, $1":"$4":"$3}' \
        | sort -k1,1 -rn \
        | awk 'NR <= 5 {printf "%s ", $2}')"
    echo "# running_processes_excluding_self=$running"
    echo "# top_cpu_processes=${top:-none}"
}

ts="$(date -u +%Y%m%dT%H%M%SZ)"
out_dir="${OUT_DIR:-target/bench-hnsw-phase3-ab/${ts}}"
case "$out_dir" in
    /*|*..*) fail "OUT_DIR must be a relative path without .. segments (got $out_dir)" ;;
esac
mkdir -p "$out_dir"

{
    echo "timestamp_utc=$ts"
    echo "before_commit=$BEFORE_COMMIT"
    echo "after_commit=$AFTER_COMMIT"
    echo "ab_pairs=$AB_PAIRS"
    echo "ab_workloads=$AB_WORKLOADS"
    echo "nproc=$(nproc 2>/dev/null || echo unknown)"
    echo "cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unknown)"
    echo "cpu_flags_subset=$(grep -m1 flags /proc/cpuinfo 2>/dev/null | grep -oE '\b(avx2|avx512f|fma|f16c)\b' | tr '\n' ' ' || echo unknown)"
    echo "bench_dedicated_env=${BENCH_DEDICATED_ENV:-unset}"
} > "$out_dir/env.txt"
echo "bench_hnsw_phase3_ab: environment recorded at $out_dir/env.txt"

run_pair() {
    local wl="$1" pair="$2" side="$3" bin="$4"
    shift 4
    mkdir -p "$out_dir/$wl"
    [ -x "$bin" ] || fail "$wl/$side binary is not executable: $bin"
    local log="$out_dir/$wl/pair${pair}-${side}.log"
    local loadavg
    loadavg="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo unknown)"
    {
        echo "# loadavg_before_run=$loadavg"
        echo "# workload=$wl pair=$pair side=$side"
        record_concurrent_processes
    } > "$log"
    "$bin" >> "$log" 2>&1 || echo "# exit_status=$? (non-zero; see above)" >> "$log"
    echo "bench_hnsw_phase3_ab: $wl pair=$pair side=$side done -> $log"
}

for wl in $AB_WORKLOADS; do
    echo "bench_hnsw_phase3_ab: workload $wl starting ($AB_PAIRS pairs, before->after each)"
    case "$wl" in
        knn_profile)
            : "${BEFORE_KNN_BIN:?BEFORE_KNN_BIN must be set for workload knn_profile}"
            : "${AFTER_KNN_BIN:?AFTER_KNN_BIN must be set for workload knn_profile}"
            for pair in $(seq 1 "$AB_PAIRS"); do
                for eng in brute_force hnsw; do
                    BENCH_KNN_PROFILE_ENGINE="$eng" run_pair "knn_profile" "$pair" "before-${eng}" "$BEFORE_KNN_BIN"
                    BENCH_KNN_PROFILE_ENGINE="$eng" run_pair "knn_profile" "$pair" "after-${eng}" "$AFTER_KNN_BIN"
                done
            done
            ;;
        hnsw_compare)
            : "${BEFORE_COMPARE_BIN:?BEFORE_COMPARE_BIN must be set for workload hnsw_compare}"
            : "${AFTER_COMPARE_BIN:?AFTER_COMPARE_BIN must be set for workload hnsw_compare}"
            for pair in $(seq 1 "$AB_PAIRS"); do
                BENCH_HNSW_COMPARE_THREADS="${BENCH_HNSW_COMPARE_THREADS:-12}" run_pair "hnsw_compare" "$pair" "before" "$BEFORE_COMPARE_BIN"
                BENCH_HNSW_COMPARE_THREADS="${BENCH_HNSW_COMPARE_THREADS:-12}" run_pair "hnsw_compare" "$pair" "after" "$AFTER_COMPARE_BIN"
            done
            ;;
        feature_1)
            : "${BEFORE_FEATURE_BIN:?BEFORE_FEATURE_BIN must be set for workload feature_1}"
            : "${AFTER_FEATURE_BIN:?AFTER_FEATURE_BIN must be set for workload feature_1}"
            for pair in $(seq 1 "$AB_PAIRS"); do
                run_pair "feature_1" "$pair" "before-default" "$BEFORE_FEATURE_BIN"
                BENCH_FEATURE_ENGINE=hnsw run_pair "feature_1" "$pair" "before-hnsw" "$BEFORE_FEATURE_BIN"
                run_pair "feature_1" "$pair" "after-default" "$AFTER_FEATURE_BIN"
                BENCH_FEATURE_ENGINE=hnsw run_pair "feature_1" "$pair" "after-hnsw" "$AFTER_FEATURE_BIN"
            done
            ;;
        feature_4)
            : "${BEFORE_FEATURE_BIN:?BEFORE_FEATURE_BIN must be set for workload feature_4}"
            : "${AFTER_FEATURE_BIN:?AFTER_FEATURE_BIN must be set for workload feature_4}"
            for pair in $(seq 1 "$AB_PAIRS"); do
                BENCH_FEATURE_SCALE=4 run_pair "feature_4" "$pair" "before-default" "$BEFORE_FEATURE_BIN"
                BENCH_FEATURE_SCALE=4 BENCH_FEATURE_ENGINE=hnsw run_pair "feature_4" "$pair" "before-hnsw" "$BEFORE_FEATURE_BIN"
                BENCH_FEATURE_SCALE=4 run_pair "feature_4" "$pair" "after-default" "$AFTER_FEATURE_BIN"
                BENCH_FEATURE_SCALE=4 BENCH_FEATURE_ENGINE=hnsw run_pair "feature_4" "$pair" "after-hnsw" "$AFTER_FEATURE_BIN"
            done
            ;;
        *)
            fail "unhandled workload: $wl"
            ;;
    esac
done

echo "bench_hnsw_phase3_ab: all workloads done. Summarize with:"
echo "  scripts/bench_hnsw_phase3_ab.sh --summarize $out_dir"
