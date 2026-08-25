#!/usr/bin/env bash
# TASK-84（対応 Issue #61。ポインタ: docs/spec/05-tasks.md TASK-84）の
# ソート非決定性再混入検知チェック。
#
# 動機（詳細は非公開 PoC-10 側の管轄のため本文へ転記しない。ポインタ:
# `docs/spec/03-poc/query-protocol-comparison`。判断の全体像は
# `docs/design/rrf-tie-break-determinism.md` 参照）: `sort_unstable_by`/
# `sort_unstable_by_key`/`select_nth_unstable_by(_key)`（同点要素の相対順序を
# 保証しない不安定ソート API）が `crates/engine/src`・`crates/wire-server/src`
# へ再混入していないかを、`cargo` を使わない軽量な `grep` ベースで検知する
# （`check_core_api.sh` と同様、rust-ci ジョブとは独立に実行できる）。
#
# 検知対象（行頭コメント `//`・`///`・`//!` は除外し、実コード行のみを走査）:
#   - `sort_unstable_by(` / `sort_unstable_by_key(`
#   - `select_nth_unstable_by(` / `select_nth_unstable_by_key(`
#
# `partial_cmp(...).unwrap()/.expect(...)` は当初検知対象に含める案もあったが、
# 安定ソート（`sort_by`）と組み合わせた「非有限値が来たら panic で止める」テスト
# 用途（例: ベンチマーク計測のような score ordering と無関係な数値比較）でも
# 出現するため誤検知が多く、対象から外した（判断根拠は
# `docs/design/rrf-tie-break-determinism.md` 参照）。
#
# 許可が必要な正当ケース（例: 整数 id 列など全順序を持つ値への `sort_unstable`
# 相当）は、対象行末に `// sort-determinism: allow <理由>` マーカーを付けることで
# 個別に除外できる（`check_core_api.sh` の `--update` のような自動更新機構は持たない。
# 除外は都度コードレビューでマーカーごと承認する運用とする）。
#
# `crates/engine/src` に現状 0 件であることの根拠・維持すべき不変条件は
# `docs/design/rrf-tie-break-determinism.md` を参照。
#
# `--self-test` は検知パターン自体の回帰テストモード。実ソースを使わずその場で
# 用意した fixture に対して「検知すべきケースを見逃さない」「許可マーカー付きの行は
# 誤検知しない」「コメント行・文字列中の一致は誤検知しない」ことを検証する
# （make sort-determinism-check・CI の sort-determinism-check ジョブから呼ばれる）。
#
# 使い方: scripts/check_sort_determinism.sh [--self-test]

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MODE="check"
if [ "${1:-}" = "--self-test" ]; then
  MODE="self-test"
elif [ "${1:-}" != "" ]; then
  echo "ERROR: unknown argument: ${1}" >&2
  echo "usage: $(basename "$0") [--self-test]" >&2
  exit 1
fi

# `dir` 配下（存在しなければスキップ）の `*.rs` を対象に、非決定的ソート API・
# 危険な `partial_cmp` 使用を検知する。マッチ行を stdout へ出力し、1 件も
# 無ければ何も出力しない（呼び出し側が出力有無で成否判定する）。
#
# API 名と `(` の間に改行・空白を挟んだ呼び出し（例:
# `v.sort_unstable_by\n    (|a, b| ...)`）でも検知を回避できないよう、行単位の
# `grep -E` ではなく `perl` でファイル全体を 1 つの文字列として走査する
# （2 回目の codex-review P1 指摘対応）。以下を誤検知として除外する:
#   - 行頭（先頭の空白を許容）が `//` で始まるコメント行
#   - マッチ開始行〜終了行のいずれかに `// sort-determinism: allow ...`
#     マーカーを持つ行（複数行にまたがる呼び出しでも終端側のマーカーを許容する）
#
# fail-closed: `find`・`perl` の失敗（構文エラー・実行不能・読み取り失敗等）を
# 握り潰さず scan_dir の非ゼロ終了として呼び出し元へ伝播する（2回目の
# codex-review P1 指摘対応。従来は `perl ... | sed ...` を素通しし、末尾の
# `sed` が成功すれば `set -u` のみではパイプライン全体が成功扱いになっていた。
# `pipefail`（ファイル先頭で有効化）に加え、本関数内では `find`/`perl` を
# 個別にコマンド置換で実行し `$?` を明示チェックすることで、process
# substitution 経由でも失敗を確実に検知する）。
scan_dir() {
  local dir="$1"
  if [ ! -d "${dir}" ]; then
    return 0
  fi

  local file_list
  file_list="$(find "${dir}" -type f -name '*.rs')"
  local find_status=$?
  if [ "${find_status}" -ne 0 ]; then
    echo "ERROR: find failed under ${dir} (exit ${find_status})" >&2
    return 1
  fi
  if [ -z "${file_list}" ]; then
    return 0
  fi

  local file
  while IFS= read -r file; do
    local out
    out="$(perl -0777 -ne '
      my @lines = split /\n/, $_, -1;
      # 行頭コメント行（`//`・`///`・`//!`）を除いた「実コード」だけを 1 本の
      # 文字列へ結合し、結合後の各文字位置が元のどの行番号に属するかを保持する。
      # これにより `sort_unstable_by\n    (` のように API 名と `(` の間に改行や
      # 空白を挟んだ呼び出しも 1 つの正規表現でまたいで検知できる。
      my $joined = "";
      my @lineno_of_pos;
      for my $i (0 .. $#lines) {
        my $ln = $i + 1;
        my $line = $lines[$i];
        next if $line =~ m{^\s*//};
        my $text = $line . "\n";
        push @lineno_of_pos, ($ln) x length($text);
        $joined .= $text;
      }
      while ($joined =~ /\b(sort_unstable_by(?:_key)?|select_nth_unstable_by(?:_key)?)\s*\(/gs) {
        my $start_ln = $lineno_of_pos[$-[0]];
        my $end_ln = $lineno_of_pos[$+[0] - 1] // $start_ln;
        my $allowed = 0;
        for my $l ($start_ln .. $end_ln) {
          if ($lines[$l - 1] =~ /sort-determinism: allow /) { $allowed = 1; last; }
        }
        next if $allowed;
        print "$start_ln:$lines[$start_ln - 1]\n";
      }
    ' "${file}")"
    local perl_status=$?
    if [ "${perl_status}" -ne 0 ]; then
      echo "ERROR: perl scan failed for ${file} (exit ${perl_status})" >&2
      return 1
    fi
    if [ -n "${out}" ]; then
      printf '%s\n' "${out}" | sed "s#^#${file}:#"
    fi
  done <<<"${file_list}"
}

if [ "${MODE}" = "self-test" ]; then
  tmp="$(mktemp -d)" || {
    echo "ERROR: mktemp -d failed" >&2
    exit 1
  }
  failed=0

  # fixture 1: 検知すべきケース（コメント外の sort_unstable_by 呼び出し）。
  cat >"${tmp}/detect_me.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    v.sort_unstable_by(|a, b| a.cmp(b));
}
EOF
  # fixture 2: 検知すべきケース（select_nth_unstable_by_key）。
  cat >"${tmp}/detect_select_nth.rs" <<'EOF'
fn f(v: &mut [i32], k: usize) {
    v.select_nth_unstable_by_key(k, |x| *x);
}
EOF
  # fixture 4: 検知すべきケース（API 名と `(` の間に改行を挟んだ呼び出し。
  # 行単位の `grep -E` では回避できてしまっていた 2 回目の codex-review P1
  # 指摘の再現ケース）。
  cat >"${tmp}/detect_multiline.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    v.sort_unstable_by
        (|a, b| a.cmp(b));
}
EOF
  # fixture 5: 検知すべきケース（API 名と `(` の間に空白のみを挟んだ呼び出し）。
  cat >"${tmp}/detect_whitespace.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    v.sort_unstable_by   (|a, b| a.cmp(b));
}
EOF
  # fixture 3: 検知してはならないケース（コメント行・許可マーカー・整数の
  # sort_unstable（引数なし、パターン対象外）・文字列全体としての言及）。
  cat >"${tmp}/allowed.rs" <<'EOF'
// v.sort_unstable_by(|a, b| a.cmp(b)); コメント内は対象外
fn f(ids: &mut Vec<u64>, v: &mut Vec<i32>) {
    ids.sort_unstable(); // 引数なし sort_unstable は本チェックの対象パターン外
    v.sort_unstable_by(|a, b| a.cmp(b)); // sort-determinism: allow 全順序の整数比較のみ
}
EOF
  # fixture 6: 検知してはならないケース（改行を挟んだ呼び出しの許可マーカーが
  # 呼び出し終端側の行にある場合。マーカー検索がマッチ範囲全体を見ることの確認）。
  cat >"${tmp}/allowed_multiline.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    v.sort_unstable_by
        (|a, b| a.cmp(b)); // sort-determinism: allow 全順序の整数比較のみ
}
EOF

  local_detected="$(scan_dir "${tmp}")"
  scan_status=$?
  if [ "${scan_status}" -ne 0 ]; then
    echo "FAIL: scan_dir exited non-zero (${scan_status}) during self-test fixture scan" >&2
    rm -rf "${tmp}"
    exit 1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_me.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect sort_unstable_by in detect_me.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_select_nth.rs.*select_nth_unstable_by_key"; then
    echo "FAIL: self-test did not detect select_nth_unstable_by_key in detect_select_nth.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_multiline.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect sort_unstable_by split across lines in detect_multiline.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_whitespace.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect sort_unstable_by with whitespace before '(' in detect_whitespace.rs" >&2
    failed=1
  fi
  if printf '%s\n' "${local_detected}" | grep -q "allowed.rs\|allowed_multiline.rs"; then
    echo "FAIL: self-test false-positive on allowed*.rs (comment / allow-marker / sort_unstable without comparator should not match)" >&2
    echo "--- detected ---" >&2
    printf '%s\n' "${local_detected}" >&2
    failed=1
  fi

  rm -rf "${tmp}"
  if [ "${failed}" -ne 0 ]; then
    echo "FAIL: scripts/check_sort_determinism.sh --self-test" >&2
    exit 1
  fi
  echo "ok: scripts/check_sort_determinism.sh --self-test"
  exit 0
fi

FOUND="$(
  scan_dir "${REPO_ROOT}/crates/engine/src" &&
    scan_dir "${REPO_ROOT}/crates/wire-server/src"
)"
FOUND_STATUS=$?
if [ "${FOUND_STATUS}" -ne 0 ]; then
  echo "ERROR: scan_dir failed (exit ${FOUND_STATUS}); sort-determinism check could not be" >&2
  echo "completed. Failing closed (see scripts/check_sort_determinism.sh scan_dir)." >&2
  exit 1
fi

if [ -n "${FOUND}" ]; then
  echo "ERROR: non-deterministic sort API (re)introduced. Score-ordered Top-k must use a" >&2
  echo "stable sort with an explicit tie-break (see docs/design/rrf-tie-break-determinism.md)." >&2
  echo "If the usage is provably safe (e.g. sorting a totally-ordered integer key with no" >&2
  echo "tie-sensitive payload), mark the line with '// sort-determinism: allow <reason>'." >&2
  echo "" >&2
  printf '%s\n' "${FOUND}" >&2
  exit 1
fi

echo "ok: no non-deterministic sort API found in crates/engine/src, crates/wire-server/src"
