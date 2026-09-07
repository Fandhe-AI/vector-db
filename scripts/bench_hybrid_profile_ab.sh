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
#   BEFORE_BIN, AFTER_BIN     計測対象バイナリの絶対パス（必須。実行可能ファイル）
#   BEFORE_COMMIT, AFTER_COMMIT
#                             計測対象バイナリをビルドしたコミットの hash（必須。
#                             `docs/design/benchmark-judgement-policy.md` §3 が
#                             before/after 双方のコミット hash の記録を要求する。
#                             バイナリのパスだけでは退避先が更新されると比較対象を
#                             追跡できないため、呼び出し側にビルド時点の hash を
#                             明示させる）
#   AB_PAIRS                  条件あたりの交互ペア数（既定 5。`docs/design/
#                             benchmark-judgement-policy.md` §3 の下限 5 ペア
#                             未満は拒否）
#   AB_ROUNDS                 各実行に渡す BENCH_HYBRID_PROFILE_ROUNDS（既定 5）
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

# 条件: N=25,000/100,000 × 可視率 1/1・1/10（Issue #547 の 4 条件）。
# --summarize のファイル走査順を実行順（この配列の並び）と一致させるため、
# ドライバ本体・--summarize の双方がこの変数を単一情報源として使う。
conditions="25000:1 25000:10 100000:1 100000:10"

if [ "${1:-}" = "--summarize" ]; then
    dir="${2:-}"
    [ -n "$dir" ] || fail "--summarize requires a directory argument"
    [ -d "$dir" ] || fail "not a directory: $dir"
    echo "=== baseline_round_raw / baseline_summary / reference_band / per-run load & process lines under $dir ==="
    found=0
    # 条件→ペア番号（数値昇順）→before/after の実行順を明示的に辿る（shell
    # glob の辞書順 `*.log` 展開だと "ratio1of10" が "ratio1of1" より前に来る、
    # また pair 番号も文字列順で "pair10" が "pair2" より前に来て実行順と逆転し、
    # before/after・ペア番号の対応を取り違えやすい。codex-review 指摘）。
    # ペア番号はディレクトリ実体から抽出し `sort -n` で数値昇順に整列してから、
    # 同一ペア内は before → after の順（実行順と一致）で列挙する。
    # grep -H: ファイル名を出す。ファイル名に条件・ペア・side が埋め込まれて
    # いるため、before/after とペア番号の対応を summary 出力だけで追える。
    for cond in $conditions; do
        rows="${cond%%:*}"
        denom="${cond##*:}"
        cond_label="rows${rows}_ratio1of${denom}"
        pair_numbers=""
        for f in "$dir/${cond_label}"_pair*_before.log; do
            [ -e "$f" ] || continue
            base="$(basename "$f")"
            pair_num="${base#"${cond_label}"_pair}"
            pair_num="${pair_num%_before.log}"
            case "$pair_num" in
                ''|*[!0-9]*) continue ;;
            esac
            pair_numbers="$pair_numbers $pair_num"
        done
        pair_numbers="$(printf '%s\n' $pair_numbers | sort -n -u)"
        for pair_num in $pair_numbers; do
            for side in before after; do
                pair_file="$dir/${cond_label}_pair${pair_num}_${side}.log"
                [ -e "$pair_file" ] || continue
                if grep -H -E 'baseline_round_raw|baseline_summary|baseline reference_band|^hybrid_profile: rows=|^# loadavg_before_run=|^# running_processes_excluding_self=|^# top_cpu_processes=' "$pair_file"; then
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
# `docs/design/benchmark-judgement-policy.md` §3: 新規計測は N >= 5 ペアを
# 必須とする（3 ペア〔#401〕・4 ペア〔#366〕はいずれも下限未満で不可）。
[ "$AB_PAIRS" -ge 5 ] || fail "AB_PAIRS must be >= 5 per docs/design/benchmark-judgement-policy.md §3 (got $AB_PAIRS)"
# hybrid_profile_bench 自身（harness::scan_stage_profile::parse_rounds）が
# BENCH_HYBRID_PROFILE_ROUNDS を 5..=50 でしか受理しないため、範囲外の値を
# ここで早期に拒否する（Bugbot 指摘。従来は正整数チェックのみで 1〜4 でも
# スクリプトは起動してしまい、実行時に各 run が個別に fail-closed していた）。
if [ "$AB_ROUNDS" -lt 5 ] || [ "$AB_ROUNDS" -gt 50 ]; then
    fail "AB_ROUNDS must be in 5..=50 (hybrid_profile_bench's own BENCH_HYBRID_PROFILE_ROUNDS bound; got $AB_ROUNDS)"
fi

# 各 run 直前の同時実行プロセスのスナップショットを `# ` 接頭辞の行として出力する。
# `running_processes_excluding_self`: 状態 R（running）のプロセス数から自シェル・ps を
# 除いた値。0 なら計測時点で他に CPU を使っているプロセスは無い。
# `top_cpu_processes`: CPU 使用率上位 5 件の「pid:comm:%cpu」（自シェル・ps を除く）。
# ps が失敗した場合は両方 unknown を出力し、記録が欠けたことを明示する。
record_concurrent_processes() {
    local self_pid=$$ snapshot
    if ! snapshot="$(ps -eo pid=,stat=,pcpu=,comm= 2>/dev/null)"; then
        echo "# running_processes_excluding_self=unknown (ps unavailable)"
        echo "# top_cpu_processes=unknown (ps unavailable)"
        return 0
    fi
    local running top
    running="$(printf '%s\n' "$snapshot" | awk -v self="$self_pid" '$1 != self && $4 != "ps" && $2 ~ /^R/ {n++} END {print n+0}')"
    top="$(printf '%s\n' "$snapshot" | awk -v self="$self_pid" '$1 != self && $4 != "ps" {print $1":"$4":"$3}' | sort -t: -k3 -rn | head -5 | tr '\n' ' ')"
    echo "# running_processes_excluding_self=$running"
    echo "# top_cpu_processes=${top:-none}"
}

ts="$(date -u +%Y%m%dT%H%M%SZ)"
out_dir="target/bench-hybrid-profile-ab/${ts}"
mkdir -p "$out_dir"

{
    echo "timestamp_utc=$ts"
    echo "before_bin=$BEFORE_BIN"
    echo "after_bin=$AFTER_BIN"
    echo "before_commit=$BEFORE_COMMIT"
    echo "after_commit=$AFTER_COMMIT"
    echo "ab_pairs=$AB_PAIRS"
    echo "ab_rounds=$AB_ROUNDS"
    echo "nproc=$(nproc 2>/dev/null || echo unknown)"
    echo "cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unknown)"
    echo "cpu_flags_subset=$(grep -m1 flags /proc/cpuinfo 2>/dev/null | grep -oE '\b(avx2|avx512f|fma|f16c)\b' | tr '\n' ' ' || echo unknown)"
    echo "bench_dedicated_env=${BENCH_DEDICATED_ENV:-unset}"
} > "$out_dir/env.txt"
echo "bench_hybrid_profile_ab: environment recorded at $out_dir/env.txt"

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
                # 各 run 直前の同時実行プロセスの有無（`docs/design/benchmark-judgement-policy.md`
                # §3「同一プロセス条件（同時実行プロセス等）」の記録要件）。自プロセス
                # （このシェルと ps 自身）を除いた running 状態のプロセス数と、CPU 使用率
                # 上位のプロセス名を残す。ps が使えない環境では推測せず unknown と明示する
                record_concurrent_processes
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
