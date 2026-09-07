#!/usr/bin/env bash
# Issue #547: #546（PR #565。SparseIndex::score_by_postings のスコアアキュムレータ
# 再利用）の前後比較を、N ∈ {25000,100000} × 可視率 ∈ {1/1,1/10} の 4 条件で
# 交互実行 min-of-N（既定 5 ペア）計測するドライバ。
#
# 計測規約（`docs/design/benchmark-judgement-policy.md`。Issue #462 SSOT）に従い、
# 各条件について before → after の順で 1 ペアずつ、ペア数ぶん交互実行する
# （逐次 before×N → after×N ではなく、ペア単位で往復させることで計測環境の
# ドリフトが片側だけに偏らないようにする）。集計（min-of-N・median-of-N・
# 参照区間帯との突き合わせ）はこのスクリプトの責務外——`--summarize` は
# 各 run の該当行をそのまま列挙するだけで、判定・平均化は行わない
# （production コードへの閾値埋め込みを避ける方針。Issue #303 と同方針）。
#
# 使い方:
#   BEFORE_BIN=/path/to/before/hybrid_profile_bench \
#   AFTER_BIN=/path/to/after/hybrid_profile_bench \
#   scripts/bench_hybrid_profile_ab.sh
#
#   scripts/bench_hybrid_profile_ab.sh --summarize target/bench-hybrid-profile-ab/<ts>
#
# env:
#   BEFORE_BIN, AFTER_BIN   計測対象バイナリの絶対パス（必須。実行可能ファイル）
#   AB_PAIRS                条件あたりの交互ペア数（既定 5・正整数）
#   AB_ROUNDS               各実行に渡す BENCH_HYBRID_PROFILE_ROUNDS（既定 5）
#
# 手動専用ベンチのドライバであり、`.github/workflows/*` から呼ばれることは
# ない（`refuse_under_github_actions` と同じ方針で GITHUB_ACTIONS 下は拒否）。
set -euo pipefail

fail() {
    echo "bench_hybrid_profile_ab.sh: $*" >&2
    exit 1
}

if [ "${GITHUB_ACTIONS:-}" != "" ]; then
    fail "refused while running under GitHub Actions (GITHUB_ACTIONS is set); this driver is local-only, matching hybrid_profile_bench's own GITHUB_ACTIONS refusal"
fi

if [ "${1:-}" = "--summarize" ]; then
    dir="${2:-}"
    [ -n "$dir" ] || fail "--summarize requires a directory argument"
    [ -d "$dir" ] || fail "not a directory: $dir"
    echo "=== baseline_round_raw / baseline_summary / reference_band lines under $dir ==="
    # grep -h: ファイル名を出さず内容だけ。複数ファイルを横断して眺めるための
    # 集約専用モードであり、判定（pass/fail）はここでは行わない。
    grep -h -E 'baseline_round_raw|baseline_summary|baseline reference_band|^hybrid_profile: rows=' "$dir"/*.log 2>/dev/null \
        || fail "no matching log lines found under $dir"
    exit 0
fi

: "${BEFORE_BIN:?BEFORE_BIN must be set to the before binary absolute path}"
: "${AFTER_BIN:?AFTER_BIN must be set to the after binary absolute path}"
[ -x "$BEFORE_BIN" ] || fail "BEFORE_BIN is not an executable file: $BEFORE_BIN"
[ -x "$AFTER_BIN" ] || fail "AFTER_BIN is not an executable file: $AFTER_BIN"

# 正整数検証（coding-rust.md「untrusted 入力の扱い」。env 経由の値をそのまま
# ループ回数へ使わず、まず検証する）。
validate_positive_int() {
    case "$1" in
        ''|*[!0-9]*) fail "$2 must be a positive integer (got $1)" ;;
    esac
    [ "$1" -ge 1 ] || fail "$2 must be >= 1 (got $1)"
}

AB_PAIRS="${AB_PAIRS:-5}"
AB_ROUNDS="${AB_ROUNDS:-5}"
validate_positive_int "$AB_PAIRS" "AB_PAIRS"
validate_positive_int "$AB_ROUNDS" "AB_ROUNDS"

ts="$(date -u +%Y%m%dT%H%M%SZ)"
out_dir="target/bench-hybrid-profile-ab/${ts}"
mkdir -p "$out_dir"

{
    echo "timestamp_utc=$ts"
    echo "before_bin=$BEFORE_BIN"
    echo "after_bin=$AFTER_BIN"
    echo "ab_pairs=$AB_PAIRS"
    echo "ab_rounds=$AB_ROUNDS"
    echo "nproc=$(nproc 2>/dev/null || echo unknown)"
    echo "cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unknown)"
    echo "cpu_flags_subset=$(grep -m1 flags /proc/cpuinfo 2>/dev/null | grep -oE '\b(avx2|avx512f|fma|f16c)\b' | tr '\n' ' ' || echo unknown)"
    echo "bench_dedicated_env=${BENCH_DEDICATED_ENV:-unset}"
} > "$out_dir/env.txt"
echo "bench_hybrid_profile_ab: environment recorded at $out_dir/env.txt"

# 条件: N=25,000/100,000 × 可視率 1/1・1/10（Issue #547 の 4 条件）。
conditions="25000:1 25000:10 100000:1 100000:10"

for cond in $conditions; do
    rows="${cond%%:*}"
    denom="${cond##*:}"
    cond_label="rows${rows}_ratio1of${denom}"
    echo "bench_hybrid_profile_ab: condition $cond_label starting ($AB_PAIRS pairs, before->after each)"
    for pair in $(seq 1 "$AB_PAIRS"); do
        for side in before after; do
            bin="$BEFORE_BIN"
            [ "$side" = "after" ] && bin="$AFTER_BIN"
            log="$out_dir/${cond_label}_pair${pair}_${side}.log"
            loadavg="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo unknown)"
            {
                echo "# loadavg_before_run=$loadavg"
                echo "# condition=$cond_label pair=$pair side=$side rows=$rows denominator=$denom"
            } > "$log"
            BENCH_HYBRID_PROFILE_ROWS="$rows" \
                BENCH_HYBRID_PROFILE_VISIBLE_RATIO="1/${denom}" \
                BENCH_HYBRID_PROFILE_ROUNDS="$AB_ROUNDS" \
                "$bin" >> "$log" 2>&1
            echo "bench_hybrid_profile_ab: $cond_label pair=$pair side=$side done -> $log"
        done
    done
done

echo "bench_hybrid_profile_ab: all conditions done. Summarize with:"
echo "  scripts/bench_hybrid_profile_ab.sh --summarize $out_dir"
