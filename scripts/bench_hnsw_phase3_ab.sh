#!/usr/bin/env bash
# Issue #507（本 Issue の担当。docs/design/hnsw-phase3-before-after.md 参照）の
# 「Phase 3（#458 ツリー）通しの前後比較」を再現するための輪番実行ドライバ。
# `docs/design/benchmark-judgement-policy.md` §3 の交互実行・per-run 生データ
# 保持規約に沿って、before/after 2 コミットのソースツリーを対象に
# `bench-hnsw-compare`・`bench-knn-profile`・`feature_bench`（`hnsw`/
# `brute_force` エンジン opt-in）の 3 対象を輪番実行し、生ログを
# `docs/design/hnsw-phase3-before-after.md` §3〜§5 が参照する命名規約
# （`<ts>-hnsw-compare-<side>-run<N>.log`・`<ts>-knn-profile-<side>-<engine>-run<N>.log`・
# `<ts>-feature-bench-<side>-<engine>-run<N>.json`）で保存する。
#
# 呼び出し元は人間の運用者（本 Issue の各対象ベンチはいずれも時間依存・spec
# 閾値を持たない情報提供専用のため CI 非配線・手動実行専用。`scripts/
# bench_dot_kernel_ab.sh` と同型の骨格を踏襲）。呼び出し先は BEFORE_DIR／
# AFTER_DIR（`git archive <commit> | tar -x -C <dir>` 等で用意した、それぞれ
# 独立した `Cargo.toml` を持つソースツリー。本 Issue では
# `docs/design/hnsw-phase3-before-after.md` §2.1 の before=4d2bd23・
# after=799a7d8 相当）に配置済みのソースを `cargo bench`／`cargo build
# --release --example` でそれぞれ独立した `CARGO_TARGET_DIR` を使いビルド・
# 実行する。production コード（`crates/engine/src/`）・既存ベンチハーネスは
# 一切変更しない。
#
# 使い方:
#   BEFORE_DIR=<path> AFTER_DIR=<path> MODE=hnsw-compare|knn-profile|feature-bench \
#     scripts/bench_hnsw_phase3_ab.sh [PAIRS]
#   PAIRS: 交互実行するペア数（既定 5・正整数のみ）。
#   OUT_DIR: 出力先（既定 docs/design/bench-data/hnsw-phase3-ab）。
#   BEFORE_DIR／AFTER_DIR は相対パスでも指定できる（本スクリプトが起動直後に
#   絶対パスへ解決する。§9 参照）。
#   MODE=hnsw-compare: BENCH_HNSW_COMPARE_ROWS／_DIM／_THREADS／_QUERIES で
#     §3 の縮小構成（rows=20,000・queries=100・thread_ladder=[12]）を上書きする
#     （子プロセス側の `hnsw_compare_bench` が読む環境変数。本スクリプトは
#     そのまま子プロセスへ継承する）。
#   MODE=knn-profile: PHASE3_KNN_PROFILE_ENGINES="hnsw brute_force"（既定）で
#     計測対象エンジンの空白区切り集合を指定する（§4 は hnsw／brute_force の
#     両方を対象にした）。
#   MODE=feature-bench: PHASE3_FEATURE_BENCH_ENGINES="hnsw"（既定。§5 は
#     hnsw opt-in の 1 arm のみを計測した。既定エンジンとの通し比較を追加する
#     場合は "hnsw brute_force" のように空白区切りで追加する）。
#
# 出力: <OUT_DIR>/<ts>-<mode>-<side>[-<engine>]-run<N>.log（feature-bench は
# .json）と <OUT_DIR>/<ts>-<mode>-loadavg.log（各 run 直前の /proc/loadavg）。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

: "${BEFORE_DIR:?BEFORE_DIR must be set to the source tree of the before commit (e.g. git archive output)}"
: "${AFTER_DIR:?AFTER_DIR must be set to the source tree of the after commit}"
: "${MODE:?MODE must be one of: hnsw-compare, knn-profile, feature-bench}"

[[ -f "${BEFORE_DIR}/Cargo.toml" ]] || die "BEFORE_DIR does not look like a source tree (no Cargo.toml): ${BEFORE_DIR}"
[[ -f "${AFTER_DIR}/Cargo.toml" ]] || die "AFTER_DIR does not look like a source tree (no Cargo.toml): ${AFTER_DIR}"

# BEFORE_DIR／AFTER_DIR を絶対パスへ解決する（codex-review P2 指摘）。
# 相対パスのまま `CARGO_TARGET_DIR="${dir}/target-phase3-ab"` を組み立てると、
# build_* 関数内の `cd "${dir}"` によりカレントディレクトリが変わった後に
# cargo が相対な `CARGO_TARGET_DIR` を「cd 後の」カレントディレクトリ基準で
# 解決してしまい、実際の成果物出力先（`<dir>/<dir>/target-phase3-ab`）と
# 本スクリプトが後段で探索する `${dir}/target-phase3-ab` が一致せず、
# 全 MODE でバイナリ発見に失敗する。ここで絶対パス化しておけば `cd` の
# 影響を受けない（Cursor Bugbot Low 指摘）。
BEFORE_DIR="$(cd "${BEFORE_DIR}" && pwd)"
AFTER_DIR="$(cd "${AFTER_DIR}" && pwd)"

case "${MODE}" in
  hnsw-compare|knn-profile|feature-bench) ;;
  *) die "MODE must be one of: hnsw-compare, knn-profile, feature-bench, got: ${MODE}" ;;
esac

PAIRS="${1:-5}"
if ! [[ "${PAIRS}" =~ ^[0-9]+$ ]] || [ "${PAIRS}" -lt 1 ]; then
  die "PAIRS must be a positive integer, got: ${PAIRS}"
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-${REPO_ROOT}/docs/design/bench-data/hnsw-phase3-ab}"
mkdir -p "${OUT_DIR}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"

LOADAVG_LOG="${OUT_DIR}/${TS}-${MODE}-loadavg.log"
: > "${LOADAVG_LOG}"

record_loadavg() {
  echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $1: $(cat /proc/loadavg 2>/dev/null || echo unavailable)" >> "${LOADAVG_LOG}"
}

# `cargo ... --message-format=json` の標準入力から、指定した target 名に
# 一致する compiler-artifact の実行ファイルパスを取り出す（codex-review P2
# 指摘）。mtime 最新のバイナリを探す方式では `target-phase3-ab` に別構成
# （feature フラグ違い等）の成果物が残っている場合に誤ったバイナリを選びうる
# ため、今回のビルドが実際に生成した実行ファイルを cargo 自身の出力から
# 直接特定する。同名 target のメッセージが複数出ても最後の 1 件（当該
# ビルドの最終成果物）を採用する。
locate_artifact() {
  local name="$1"
  jq -r --arg name "${name}" \
    'select(.reason == "compiler-artifact" and .target.name == $name and .executable != null) | .executable' \
    | tail -n1
}

build_hnsw_compare() {
  local dir="$1" target="$2"
  ( cd "${dir}" && CARGO_TARGET_DIR="${target}" cargo bench -p fandhe-vector-db-engine --bench hnsw_compare_bench --features contrast-bench --no-run --message-format=json ) \
    | locate_artifact hnsw_compare_bench
}

build_knn_profile() {
  local dir="$1" target="$2"
  ( cd "${dir}" && CARGO_TARGET_DIR="${target}" cargo bench -p fandhe-vector-db-engine --bench knn_profile_bench --no-run --message-format=json ) \
    | locate_artifact knn_profile_bench
}

build_feature_bench() {
  local dir="$1" target="$2"
  ( cd "${dir}" && CARGO_TARGET_DIR="${target}" cargo build --release -p fandhe-vector-db-engine --example feature_bench --message-format=json ) \
    | locate_artifact feature_bench
}

BEFORE_TARGET="${BEFORE_DIR}/target-phase3-ab"
AFTER_TARGET="${AFTER_DIR}/target-phase3-ab"

case "${MODE}" in
  hnsw-compare)
    BEFORE_BIN="$(build_hnsw_compare "${BEFORE_DIR}" "${BEFORE_TARGET}")"
    AFTER_BIN="$(build_hnsw_compare "${AFTER_DIR}" "${AFTER_TARGET}")"
    [[ -x "${BEFORE_BIN}" ]] || die "could not locate before hnsw_compare_bench binary under ${BEFORE_TARGET}"
    [[ -x "${AFTER_BIN}" ]] || die "could not locate after hnsw_compare_bench binary under ${AFTER_TARGET}"
    for pair in $(seq 1 "${PAIRS}"); do
      for side in before after; do
        bin="${BEFORE_BIN}"; [[ "${side}" == after ]] && bin="${AFTER_BIN}"
        log="${OUT_DIR}/${TS}-hnsw-compare-${side}-run${pair}.log"
        [[ -e "${log}" ]] && die "raw log already exists (refusing to overwrite): ${log}"
        record_loadavg "hnsw-compare/${side}/run${pair}"
        "${bin}" --bench > "${log}" 2>&1
      done
    done
    ;;
  knn-profile)
    BEFORE_BIN="$(build_knn_profile "${BEFORE_DIR}" "${BEFORE_TARGET}")"
    AFTER_BIN="$(build_knn_profile "${AFTER_DIR}" "${AFTER_TARGET}")"
    [[ -x "${BEFORE_BIN}" ]] || die "could not locate before knn_profile_bench binary under ${BEFORE_TARGET}"
    [[ -x "${AFTER_BIN}" ]] || die "could not locate after knn_profile_bench binary under ${AFTER_TARGET}"
    ENGINES="${PHASE3_KNN_PROFILE_ENGINES:-hnsw brute_force}"
    # side を engine の内側にネストすると before-hnsw → before-brute_force →
    # after-hnsw の順になり、各エンジンの before/after が隣接しない
    # （Cursor Bugbot Medium 指摘。§9 が主張する before→after の対比・
    # コミット済みセッション順と不一致）。engine を外側にして
    # 「エンジンごとに before→after を隣接させる」順へ変更する。
    for pair in $(seq 1 "${PAIRS}"); do
      for engine in ${ENGINES}; do
        for side in before after; do
          bin="${BEFORE_BIN}"; [[ "${side}" == after ]] && bin="${AFTER_BIN}"
          log="${OUT_DIR}/${TS}-knn-profile-${side}-${engine}-run${pair}.log"
          [[ -e "${log}" ]] && die "raw log already exists (refusing to overwrite): ${log}"
          record_loadavg "knn-profile/${side}/${engine}/run${pair}"
          BENCH_KNN_PROFILE_ENGINE="${engine}" "${bin}" --bench > "${log}" 2>&1
        done
      done
    done
    ;;
  feature-bench)
    BEFORE_BIN="$(build_feature_bench "${BEFORE_DIR}" "${BEFORE_TARGET}")"
    AFTER_BIN="$(build_feature_bench "${AFTER_DIR}" "${AFTER_TARGET}")"
    [[ -x "${BEFORE_BIN}" ]] || die "could not locate before feature_bench binary under ${BEFORE_TARGET}"
    [[ -x "${AFTER_BIN}" ]] || die "could not locate after feature_bench binary under ${AFTER_TARGET}"
    ENGINES="${PHASE3_FEATURE_BENCH_ENGINES:-hnsw}"
    # knn-profile と同じ理由（Cursor Bugbot Medium 指摘）で engine を外側にする。
    for pair in $(seq 1 "${PAIRS}"); do
      for engine in ${ENGINES}; do
        for side in before after; do
          bin="${BEFORE_BIN}"; [[ "${side}" == after ]] && bin="${AFTER_BIN}"
          log="${OUT_DIR}/${TS}-feature-bench-${side}-${engine}-run${pair}.json"
          [[ -e "${log}" ]] && die "raw log already exists (refusing to overwrite): ${log}"
          record_loadavg "feature-bench/${side}/${engine}/run${pair}"
          BENCH_FEATURE_ENGINE="${engine}" "${bin}" > "${log}" 2>&1
        done
      done
    done
    ;;
esac

echo "done: results under ${OUT_DIR} (timestamp prefix ${TS})"
