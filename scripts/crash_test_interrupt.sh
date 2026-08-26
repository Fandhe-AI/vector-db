#!/usr/bin/env bash
# Issue #134（TASK-142・対象ビヘイビア: PERSIST-1）の中断パス（cleanup での
# writer 停止）に対するセルフテスト。scripts/crash_test.sh を実際に起動し、
# 外側から SIGTERM を送って中断させる黒箱テスト（crash_test.sh 内部の trap
# 構造そのものを、本番と同一のまま検証する）。
#
# Makefile の crash-test-interrupt ターゲット（make ci にも含まれる）から
# 呼ばれる。crash-test ジョブそのもの（scripts/crash_test.sh 本体の反復）
# には依存させない独立ターゲットとして設計する（本テストが timing・pgrep
# に依存し flaky になり得るため、本体のクラッシュ耐性検証を巻き込まない）。
#
# 検証する 2 変種:
#   - pid: プロセスグループを介さず crash_test.sh の PID 単体へ SIGTERM を
#     送る経路（`kill <pid>`・`timeout` コマンド等での中断を模する）。
#     cleanup が writer を停止しない実装では survivor が残るため、この変種
#     こそが Issue #134 の修正を判別する
#   - pgroup: `set -m` 下でプロセスグループ全体へ SIGTERM を送る経路
#     （端末の Ctrl+C 相当）。修正前後どちらでも survivor は出ない構造の
#     ため、こちらは非回帰・ハング無しの確認に留まる

set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRASH_TEST="${REPO_ROOT}/scripts/crash_test.sh"

for cmd in pgrep ps; do
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "ERROR: required command not found: ${cmd} (install procps or equivalent)" >&2
    exit 1
  fi
done

# crash_test.sh 自身の cargo build ログ・workdir 行を拾うための一時ログ置き場。
# 自分自身の EXIT trap で必ず削除する（テストがリークしない）。
LOG_DIR="$(mktemp -d)"
trap 'rm -rf "${LOG_DIR}"' EXIT

# writer（crash_tool write）が起動しプロセスとして観測できるまでのポーリング
# 上限（100ms 単位）。初回はここで cargo build --release が走るため長めに
# 確保する（コールド runner でのビルド時間を見込む）。
WRITER_START_TIMEOUT_TENTHS=6000
# signal 送信後、crash_test.sh 本体（wait "$script_pid"）が終了コードを返す
# までの上限（秒）。writer のコミット同期は既に取れているのでビルド待ちは
# 発生しない。この上限を超えたら signal が届いていない/誤対象であるとみなし
# fail-closed で失敗させる（無反応のまま CI タイムアウトまで走らせない）。
SCRIPT_EXIT_TIMEOUT_SECONDS=30
# survivor 判定のための猶予（100ms 単位）。SIGKILL は即座に効くはずだが、
# reap 完了までの短いラグを許容する。
SURVIVOR_GRACE_TENTHS=20

# crash_test.sh に渡す反復回数。中断させる前に自然終了しない程度に大きく、
# かつ本テストが不要に長く走らないよう 1000 のような大きすぎる値は避ける。
ITERATIONS=200

fail() {
  echo "ERROR: $1" >&2
  exit 1
}

# writer（crash_tool write）が指定 PID の子として観測できるようになるまで
# 待つ。observed が非空なら writer PID を stdout へ返す。
wait_for_writer() {
  local script_pid="$1"
  local waited=0
  while [ "${waited}" -lt "${WRITER_START_TIMEOUT_TENTHS}" ]; do
    local writer_pid
    writer_pid="$(pgrep -P "${script_pid}" -f 'crash_tool write' 2>/dev/null | head -n1)"
    if [ -n "${writer_pid}" ]; then
      echo "${writer_pid}"
      return 0
    fi
    if ! kill -0 "${script_pid}" 2>/dev/null; then
      # crash_test.sh が writer 起動前に終了した（ビルド失敗など）。
      return 1
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  return 1
}

# writer_pid が既に不在（プロセス不在＝survivor 無し）であることを検証する。
# 生存していれば後始末として SIGKILL してから失敗させる（テスト自身が
# survivor をリークしないため）。
assert_no_survivor() {
  local writer_pid="$1"
  local waited=0
  while [ "${waited}" -lt "${SURVIVOR_GRACE_TENTHS}" ]; do
    if ! kill -0 "${writer_pid}" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  if kill -0 "${writer_pid}" 2>/dev/null; then
    # PID が生存していても、それが本当に writer（crash_tool write）とは限らない。
    # writer が SIGKILL で停止・reap された直後の SURVIVOR_GRACE_TENTHS
    # （約 2 秒）のうちに OS が同じ PID を無関係なプロセスへ再利用しうるため、
    # /proc 経由（Linux。無ければ ps）で cmdline を照合して同一性を確かめる。
    local cmdline=""
    if [ -r "/proc/${writer_pid}/cmdline" ]; then
      cmdline="$(tr '\0' ' ' < "/proc/${writer_pid}/cmdline" 2>/dev/null)"
    else
      cmdline="$(ps -o command= -p "${writer_pid}" 2>/dev/null)"
    fi
    if [[ "${cmdline}" != *crash_tool* ]]; then
      # PID 再利用で別プロセスを掴んだ場合。writer 自体は停止済みであり
      # survivor は無いので成功扱いとする（無関係なプロセスを kill せず、
      # かつ Issue #134 の regression と誤判定して赤 CI にもしない）。
      return 0
    fi
    # cmdline が crash_tool を含む＝writer が実際に生き残っている。後始末に
    # SIGKILL してから regression として失敗させる（kill と判定を同一条件に揃える）。
    kill -9 "${writer_pid}" 2>/dev/null
    return 1
  fi
  return 0
}

# 変種 1 つを実行する。target は "pid"（単体）または "pgroup"（プロセス
# グループ全体）。
run_variant() {
  local name="$1"
  local target="$2"
  local log="${LOG_DIR}/${name}.log"

  echo "--- variant: ${name} ---"

  # set -m はこの関数の実行中のみジョブ制御を有効化する。crash_test.sh を
  # このまま `&` で起動すれば `$!` は crash_test.sh を解釈する bash 自身の
  # PID であり、set -m 下でその子はプロセスグループリーダーになる
  # （pgid == script_pid）。余分なサブシェル層を挟むと `$!` がサブシェルの
  # PID になり pid/pgroup どちらの対象も取り違えるため、直接 `&` する。
  set -m
  "${CRASH_TEST}" "${ITERATIONS}" > "${log}" 2>&1 &
  local script_pid=$!
  set +m

  local writer_pid
  if ! writer_pid="$(wait_for_writer "${script_pid}")"; then
    kill -9 -- "-${script_pid}" 2>/dev/null
    kill -9 "${script_pid}" 2>/dev/null
    wait "${script_pid}" 2>/dev/null
    fail "[${name}] writer did not start within $((WRITER_START_TIMEOUT_TENTHS * 100))ms"
  fi

  case "${target}" in
    pid)
      kill -TERM "${script_pid}" 2>/dev/null
      ;;
    pgroup)
      kill -TERM -- "-${script_pid}" 2>/dev/null
      ;;
    *)
      fail "[${name}] unknown target: ${target}"
      ;;
  esac

  # `wait` をバックグラウンドで待ちつつ、ハード上限を超えたら強制後始末する
  # （signal が届かない/誤対象だった場合に CI のジョブタイムアウトまで
  # 無反応のまま走らせない。fail-closed）。
  local waited=0
  local exited=false
  local script_status=""
  while [ "${waited}" -lt "$((SCRIPT_EXIT_TIMEOUT_SECONDS * 10))" ]; do
    if ! kill -0 "${script_pid}" 2>/dev/null; then
      exited=true
      break
    fi
    sleep 0.1
    waited=$((waited + 1))
  done

  if [ "${exited}" != "true" ]; then
    kill -9 -- "-${script_pid}" 2>/dev/null
    kill -9 "${script_pid}" 2>/dev/null
    wait "${script_pid}" 2>/dev/null
    assert_no_survivor "${writer_pid}" || true
    fail "[${name}] crash_test.sh did not exit within ${SCRIPT_EXIT_TIMEOUT_SECONDS}s after signal (pid=${script_pid})"
  fi

  wait "${script_pid}" 2>/dev/null
  script_status=$?
  if [ "${script_status}" -ne 130 ]; then
    # 期待外の終了コード（trap 未設置による 143 など）でも、writer が生き残って
    # いれば MAX_ROWS まで自走し続ける。タイムアウト経路と同様に後始末してから
    # 失敗させる（テスト自身が survivor をリークしない）。
    assert_no_survivor "${writer_pid}" || true
    fail "[${name}] crash_test.sh exit status=${script_status}, expected 130"
  fi

  if ! assert_no_survivor "${writer_pid}"; then
    fail "[${name}] writer pid ${writer_pid} survived cleanup (Issue #134 regression)"
  fi

  local workdir
  workdir="$(sed -n 's/^workdir: //p' "${log}" | head -n1)"
  if [ -z "${workdir}" ]; then
    fail "[${name}] could not find workdir line in crash_test.sh output"
  fi
  if [ -d "${workdir}" ]; then
    fail "[${name}] workdir ${workdir} was not removed by cleanup"
  fi

  echo "variant ${name}: OK (script exit=130, no survivor, workdir removed)"
}

run_variant "pid" pid
run_variant "pgroup" pgroup

echo "crash_test_interrupt.sh: OK"
