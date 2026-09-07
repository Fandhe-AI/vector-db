#!/usr/bin/env bash
# Issue #506: 再開型探索（Issue #505・`sql::hnsw_hybrid::HnswDenseProvider`）の
# 前後比較を、before（`838c53e`。#505 未マージ）／after（`4ceb6b5`。#505 マージ済み）
# の 2 バイナリで交互実行 min-of-N（既定 5 ペア）計測するドライバ。
#
# `crates/engine/benches/hybrid_latency_bench.rs`（既定モード）は
# `hybrid::hybrid_search` を直接呼ぶ in-build 比較で、#505 の実 seam
# （`sql::hnsw_hybrid::HnswDenseProvider`）を一切通らない（Issue #324 と同じ
# 構造。CORE-7 の前例）。本ドライバは `BENCH_HYBRID_LATENCY_ENGINE` を設定して
# SQL 表層（hnsw opt-in）計測モードを起動する（`docs/design/
# hnsw-hybrid-iterative-scan.md`「前後比較実測（Issue #506）」節参照）。
#
# 計測規約（`docs/design/benchmark-judgement-policy.md`。Issue #462 SSOT）に従い、
# 条件ごとに before → after の順で 1 ペアずつ、ペア数ぶん交互実行する。集計
# （min-of-N・median-of-N・参照区間帯との突き合わせ）はこのスクリプトの責務外
# ——`--summarize` は各 run の該当行をそのまま列挙するだけで、判定・平均化は
# 行わない（production コードへの閾値埋め込みを避ける方針。Issue #303 と同方針）。
#
# 条件（1 プロセス = 1 条件。Issue #313 と同方針）:
#   ref_bf_large_tie5     brute_force・large・tie_refetch（参照区間。既定経路は
#                         HnswIndexCache に一切結線されないため #505 の影響を
#                         構造的に受けない——ref band の妥当性確認用）
#   hnsw_large_uniform    hnsw・large・no_refetch（一様分布。#506 申し送り要件）
#   hnsw_large_tie5       hnsw・large・tie_refetch（既定 QUANTIZE_LEVELS=5。
#                         Issue #324 既存形状）
#   hnsw_410shape_tie2    hnsw・NUM_DOCS=4000・DIM=16・VOCAB_SIZE=64・
#                         QUANTIZE_LEVELS=2・tie_refetch（Issue #410／#505
#                         実装記録の形状。masked_short 再測定用）
#
# `hnsw_410shape_tie2` の after 側のみ `BENCH_HYBRID_LATENCY_EXPECT_RESUMED=1`
# を付与する（実測で確認済み: `hnsw_large_tie5`〔既定 QUANTIZE_LEVELS=5・
# 20,000 件〕は初回 fetch_k で可視集合全体を取り切り hybrid_rounds_max=1
# に留まるため、複数ラウンドに到達せず hybrid_resumed_rounds は構造的に
# 常に 0 になる。`no_refetch` も同様に構造的に 0 が正しい挙動のため付与
# しない。before 側は `hybrid_resumed_rounds` フィールド自体が無いため
# 付与しない）。
#
# 使い方:
#   BEFORE_BIN=/path/to/before/hybrid_latency_bench \
#   AFTER_BIN=/path/to/after/hybrid_latency_bench \
#   BEFORE_COMMIT=838c53e AFTER_COMMIT=4ceb6b5 \
#   scripts/bench_hybrid_latency_ab.sh
#
#   scripts/bench_hybrid_latency_ab.sh --summarize target/bench-hybrid-latency-ab/<ts>
#
# env:
#   BEFORE_BIN, AFTER_BIN     計測対象バイナリの絶対パス（必須。実行可能ファイル）
#   BEFORE_COMMIT, AFTER_COMMIT
#                             計測対象バイナリをビルドしたコミットの hash（必須。
#                             `docs/design/benchmark-judgement-policy.md` §3 参照）
#   AB_PAIRS                  条件あたりの交互ペア数（既定 5。5 未満は拒否）
#
# 手動専用ベンチのドライバであり、`.github/workflows/*` から呼ばれることはない
# （`refuse_under_github_actions` と同じ方針で GITHUB_ACTIONS 下は拒否）。
set -euo pipefail

fail() {
    echo "bench_hybrid_latency_ab.sh: $*" >&2
    exit 1
}

if [ "${GITHUB_ACTIONS:-}" != "" ]; then
    fail "refused while running under GitHub Actions (GITHUB_ACTIONS is set); this driver is local-only, matching hybrid_latency_bench's own GITHUB_ACTIONS refusal"
fi

# 条件ラベル一覧（実行順の単一情報源。--summarize もこの順で走査する）。
conditions="ref_bf_large_tie5 hnsw_large_uniform hnsw_large_tie5 hnsw_410shape_tie2"

# 条件ラベル → env 変数群（`NAME=VALUE` を空白区切りで並べたもの）。
condition_env() {
    case "$1" in
        ref_bf_large_tie5)
            echo "BENCH_HYBRID_LATENCY_ENGINE=brute_force BENCH_HYBRID_LATENCY_SCALE=large BENCH_HYBRID_LATENCY_CORPUS=tie_refetch"
            ;;
        hnsw_large_uniform)
            echo "BENCH_HYBRID_LATENCY_ENGINE=hnsw BENCH_HYBRID_LATENCY_SCALE=large BENCH_HYBRID_LATENCY_CORPUS=no_refetch"
            ;;
        hnsw_large_tie5)
            echo "BENCH_HYBRID_LATENCY_ENGINE=hnsw BENCH_HYBRID_LATENCY_SCALE=large BENCH_HYBRID_LATENCY_CORPUS=tie_refetch"
            ;;
        hnsw_410shape_tie2)
            echo "BENCH_HYBRID_LATENCY_ENGINE=hnsw BENCH_HYBRID_LATENCY_SCALE=large BENCH_HYBRID_LATENCY_CORPUS=tie_refetch BENCH_HYBRID_LATENCY_NUM_DOCS=4000 BENCH_HYBRID_LATENCY_DIM=16 BENCH_HYBRID_LATENCY_VOCAB_SIZE=64 BENCH_HYBRID_LATENCY_QUANTIZE_LEVELS=2"
            ;;
        *)
            fail "unknown condition: $1"
            ;;
    esac
}

# after 側のみ EXPECT_RESUMED=1 を付ける条件。実測で確認済み: `hnsw_large_tie5`
# （20,000 件・dim=32・vocab=256・QUANTIZE_LEVELS=5）は密側初回 fetch_k（400）で
# 可視集合全体を取り切り hybrid_rounds_max=1（複数ラウンドに到達しない）ため
# hybrid_resumed_rounds は構造的に常に 0 になる——`hnsw_410shape_tie2`
# （Issue #410 fixture 形状。4,000 件・dim=16・vocab=64・QUANTIZE_LEVELS=2）
# のみが実測で複数ラウンド（hybrid_rounds_max=4）・再開型経路の発火
# （hybrid_resumed_rounds>0）を安定して示す。`hnsw_large_uniform` は
# no_refetch のため構造的に resumed=0 が正しく、`ref_bf_large_tie5` は
# brute_force のため ANN 統計自体を持たない。
condition_expects_resumed() {
    case "$1" in
        hnsw_410shape_tie2) return 0 ;;
        *) return 1 ;;
    esac
}

if [ "${1:-}" = "--summarize" ]; then
    dir="${2:-}"
    [ -n "$dir" ] || fail "--summarize requires a directory argument"
    [ -d "$dir" ] || fail "not a directory: $dir"
    echo "=== hybrid_latency stage lines / env / per-run load & process lines under $dir ==="
    found=0
    for cond in $conditions; do
        pair_numbers=""
        for f in "$dir/${cond}_pair"*_before.log; do
            [ -e "$f" ] || continue
            base="$(basename "$f")"
            pair_num="${base#"${cond}"_pair}"
            pair_num="${pair_num%_before.log}"
            case "$pair_num" in
                ''|*[!0-9]*) continue ;;
            esac
            pair_numbers="$pair_numbers $pair_num"
        done
        pair_numbers="$(printf '%s\n' $pair_numbers | sort -n -u)"
        for pair_num in $pair_numbers; do
            for side in before after; do
                pair_file="$dir/${cond}_pair${pair_num}_${side}.log"
                [ -e "$pair_file" ] || continue
                if grep -H -E '^hybrid_latency: stage=|^# loadavg_before_run=|^# running_processes_excluding_self=|^# top_cpu_processes=|^env: ' "$pair_file"; then
                    found=1
                fi
            done
        done
    done
    [ "$found" -eq 1 ] || fail "no matching log lines found under $dir"
    exit 0
fi

: "${BEFORE_BIN:?BEFORE_BIN must be set to the before binary absolute path}"
: "${AFTER_BIN:?AFTER_BIN must be set to the after binary absolute path}"
: "${BEFORE_COMMIT:?BEFORE_COMMIT must be set to the commit hash the before binary was built from (docs/design/benchmark-judgement-policy.md §3)}"
: "${AFTER_COMMIT:?AFTER_COMMIT must be set to the commit hash the after binary was built from (docs/design/benchmark-judgement-policy.md §3)}"
[ -x "$BEFORE_BIN" ] || fail "BEFORE_BIN is not an executable file: $BEFORE_BIN"
[ -x "$AFTER_BIN" ] || fail "AFTER_BIN is not an executable file: $AFTER_BIN"

validate_positive_int() {
    case "$1" in
        ''|*[!0-9]*) fail "$2 must be a positive integer (got $1)" ;;
    esac
    [ "$1" -ge 1 ] || fail "$2 must be >= 1 (got $1)"
}

AB_PAIRS="${AB_PAIRS:-5}"
validate_positive_int "$AB_PAIRS" "AB_PAIRS"
[ "$AB_PAIRS" -ge 5 ] || fail "AB_PAIRS must be >= 5 per docs/design/benchmark-judgement-policy.md §3 (got $AB_PAIRS)"

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
out_dir="target/bench-hybrid-latency-ab/${ts}"
mkdir -p "$out_dir"

{
    echo "timestamp_utc=$ts"
    echo "before_bin=$BEFORE_BIN"
    echo "after_bin=$AFTER_BIN"
    echo "before_commit=$BEFORE_COMMIT"
    echo "after_commit=$AFTER_COMMIT"
    echo "ab_pairs=$AB_PAIRS"
    echo "nproc=$(nproc 2>/dev/null || echo unknown)"
    echo "cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unknown)"
    echo "cpu_flags_subset=$(grep -m1 flags /proc/cpuinfo 2>/dev/null | grep -oE '\b(avx2|avx512f|fma|f16c)\b' | tr '\n' ' ' || echo unknown)"
    echo "bench_dedicated_env=${BENCH_DEDICATED_ENV:-unset}"
} > "$out_dir/env.txt"
echo "bench_hybrid_latency_ab: environment recorded at $out_dir/env.txt"

for cond in $conditions; do
    env_pairs="$(condition_env "$cond")"
    expect_resumed_after=0
    if condition_expects_resumed "$cond"; then
        expect_resumed_after=1
    fi
    echo "bench_hybrid_latency_ab: condition $cond starting ($AB_PAIRS pairs, before->after each)"
    for pair in $(seq 1 "$AB_PAIRS"); do
        for side in before after; do
            bin="$BEFORE_BIN"
            [ "$side" = "after" ] && bin="$AFTER_BIN"
            log="$out_dir/${cond}_pair${pair}_${side}.log"
            loadavg="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo unknown)"
            {
                echo "# loadavg_before_run=$loadavg"
                echo "# condition=$cond pair=$pair side=$side"
                record_concurrent_processes
            } > "$log"
            expect_resumed=0
            [ "$side" = "after" ] && expect_resumed="$expect_resumed_after"
            # shellcheck disable=SC2086
            env $env_pairs BENCH_HYBRID_LATENCY_EXPECT_RESUMED="$expect_resumed" \
                "$bin" >> "$log" 2>&1
            echo "bench_hybrid_latency_ab: $cond pair=$pair side=$side done -> $log"
        done
    done
done

echo "bench_hybrid_latency_ab: all conditions done. Summarize with:"
echo "  scripts/bench_hybrid_latency_ab.sh --summarize $out_dir"
