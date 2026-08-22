#!/usr/bin/env bash
# TASK-142（対象ビヘイビア: PERSIST-1。ポインタ: docs/spec/05-tasks.md TASK-142）の
# クラッシュ耐性回帰テスト。
#
# `crates/engine/examples/crash_tool.rs`（write/verify サブコマンド）を使い、
# 書き込み進行中の engine::storage::Storage プロセスを SIGKILL で強制終了させ、
# 再オープン後の内容整合性を検証する反復を行う。プロセス内テスト
# （crates/engine/tests/persistence.rs）では表現できない「プロセス外からの
# 強制終了」を成立させるための補助スクリプトで、Makefile の crash-test
# ターゲット・CI の crash-test ジョブから呼ばれる。
#
# 使い方: scripts/crash_test.sh [反復回数（既定 10）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、
# set -e はそれらと相性が悪い（意図せずスクリプトを止めてしまう）。
# 検証結果は都度 fail-closed に明示チェックする。

ITERATIONS="${1:-10}"
if ! [[ "${ITERATIONS}" =~ ^[0-9]+$ ]] || [ "${ITERATIONS}" -lt 1 ]; then
  echo "ERROR: iterations must be a positive integer, got: ${ITERATIONS}" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/examples/crash_tool"

echo "building crash_tool (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p engine --example crash_tool); then
  echo "ERROR: failed to build crash_tool" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool binary not found at ${BIN}" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "${WORKDIR}"' EXIT
DB_PATH="${WORKDIR}/crash_test.redb"
WRITE_LOG="${WORKDIR}/write.log"

# 進捗行（COMMITTED ...）が出るまでの最大待機（100ms 単位。値 300 = 30 秒）。実際に
# 書き込みが始まっていない状態で kill しても回帰テストとして意味が無いため、
# 開始同期をここで取る（コールド runner での初回 put 遅延を見込んだ余裕）。
WRITE_START_TIMEOUT_TENTHS=300

last_rows=0

for i in $(seq 1 "${ITERATIONS}"); do
  echo "=== iteration ${i}/${ITERATIONS} ==="

  : > "${WRITE_LOG}"
  "${BIN}" write "${DB_PATH}" >> "${WRITE_LOG}" 2>&1 &
  writer_pid=$!

  # 書き込みが実際に進み始めるまでポーリングする（タイムアウト付き。fail-closed）。
  waited=0
  started=false
  while [ "${waited}" -lt "${WRITE_START_TIMEOUT_TENTHS}" ]; do
    if grep -q "^COMMITTED " "${WRITE_LOG}" 2>/dev/null; then
      started=true
      break
    fi
    if ! kill -0 "${writer_pid}" 2>/dev/null; then
      # writer が同期点に到達する前に終了した（起動失敗など）。
      break
    fi
    sleep 0.1
    waited=$((waited + 1))
  done

  if [ "${started}" != "true" ]; then
    timeout_ms=$((WRITE_START_TIMEOUT_TENTHS * 100))
    echo "ERROR: writer did not report progress within ${timeout_ms}ms" >&2
    kill -9 "${writer_pid}" 2>/dev/null
    wait "${writer_pid}" 2>/dev/null
    exit 1
  fi

  # コミット中に kill が当たる確率を上げるための短いランダム待機（10〜300ms）。
  wait_ms=$((10 + RANDOM % 291))
  sleep "0.$(printf '%03d' "${wait_ms}")"

  if ! kill -9 "${writer_pid}" 2>/dev/null; then
    echo "ERROR: kill -9 failed for writer pid ${writer_pid} at iteration ${i} (writer may have already exited on its own, e.g. a put error or safety limit); this iteration cannot verify SIGKILL crash resilience" >&2
    cat "${WRITE_LOG}" >&2
    exit 1
  fi

  wait "${writer_pid}" 2>/dev/null
  writer_status=$?
  # `wait` は SIGKILL で終了したプロセスに対して 128+9=137 を返す。
  # それ以外（writer が kill 到達前に自発終了した等）は SIGKILL を検証できて
  # いない反復であり、テストの目的（強制終了下での整合性検証）を満たさない
  # ため fail-closed で拒否する。
  if [ "${writer_status}" -ne 137 ]; then
    echo "ERROR: writer pid ${writer_pid} did not terminate via SIGKILL at iteration ${i} (wait exit status=${writer_status}, expected 137); this iteration did not exercise a real SIGKILL crash" >&2
    cat "${WRITE_LOG}" >&2
    exit 1
  fi

  verify_output="$("${BIN}" verify "${DB_PATH}")"
  echo "${verify_output}"

  if [[ "${verify_output}" != RESULT\ ok=true* ]]; then
    echo "ERROR: verify failed at iteration ${i}: ${verify_output}" >&2
    exit 1
  fi

  rows="$(echo "${verify_output}" | sed -n 's/.*rows=\([0-9]*\).*/\1/p')"
  if ! [[ "${rows}" =~ ^[0-9]+$ ]]; then
    echo "ERROR: could not parse row count from verify output: ${verify_output}" >&2
    exit 1
  fi

  # write.log の最後に完全に書き切られた `COMMITTED row=<n> rows=<total>` 行から
  # 「kill 前に確認済みだった行数（acked）」を取り出す。`$` でパターンを終端に
  # 固定しているため、kill が出力の途中で当たって行が切れた場合はマッチせず
  # 1 つ前の完全な行が採用される（acked は実際より小さくなることはあっても
  # 大きくなることはない＝false-fail しない）。
  acked="$(grep -o '^COMMITTED row=[0-9]* rows=[0-9]*$' "${WRITE_LOG}" | tail -1 | sed -n 's/.*rows=\([0-9]*\)$/\1/p')"
  if [ -n "${acked}" ]; then
    # 「started」で確認した COMMITTED はコミット成功後にのみ出力されるため、
    # 再オープン後の rows はその時点で確認済みだった acked 行数を下回ってはならない
    # （下回れば、確認済みのはずのコミットが失われたことになる＝data loss）。
    if [ "${rows}" -lt "${acked}" ]; then
      echo "ERROR: row count ${rows} is below the last acknowledged commit (rows=${acked} from write.log), data loss suspected" >&2
      exit 1
    fi
  fi

  # "started" が true な時点で write は kill 前に少なくとも 1 回 COMMITTED を
  # 出力済み（＝コミットが実際に成功した後にのみ出力される同期点を通過済み）
  # なので、rows は必ず last_rows より真に増えているはずである。`-lt` のみだと
  # 新規コミットが全て失われても rows == last_rows で見逃してしまう
  # （data loss を検出できない）ため、`-le` で「増えていない」ケースも失敗にする。
  if [ "${rows}" -le "${last_rows}" ]; then
    echo "ERROR: row count did not increase across restarts (${last_rows} -> ${rows}) despite a committed write, data loss suspected" >&2
    exit 1
  fi
  last_rows="${rows}"
done

# 空虚な成功（DB 未作成・0 行のまま検証をすり抜ける）を拒否する。
if [ "${last_rows}" -le 0 ]; then
  echo "ERROR: final row count is not positive (${last_rows})" >&2
  exit 1
fi

echo "crash_test.sh: OK (${ITERATIONS} iterations, final rows=${last_rows})"
