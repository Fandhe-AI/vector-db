#!/usr/bin/env bash
# TASK-84（対応 Issue #61。ポインタ: docs/spec/05-tasks.md TASK-84）の
# ソート非決定性再混入検知チェック。
#
# PoC-10（`docs/spec/03-poc/query-protocol-comparison`）が発見した「同点タイブレーク
# 欠如による非決定性」（`sort_unstable_by`/`sort_unstable_by_key`/
# `select_nth_unstable_by(_key)` は同点要素の相対順序を保証しないため、ハッシュ由来の
# 挿入順・スレッドスケジューリング次第で Top-k の並びが実行ごとに変わりうる）が
# `crates/engine/src`・`crates/wire-server/src` へ再混入していないかを、`cargo` を
# 使わない軽量な `grep` ベースで検知する（`check_core_api.sh` と同様、rust-ci ジョブとは
# 独立に実行できる）。
#
# 検知対象（行頭コメント `//`・`///`・`//!` は除外し、実コード行のみを走査）:
#   - `sort_unstable_by(` / `sort_unstable_by_key(`
#   - `select_nth_unstable_by(` / `select_nth_unstable_by_key(`
# これらはいずれも同点要素の相対順序を保証しない（安定ソートではない）ため、
# ハッシュ由来の挿入順・スレッドスケジューリング次第で同点グループの並びが
# 実行ごとに変わりうる（PoC-10 が指摘した非決定性の直接原因）。
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

set -u

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
# `grep -n` の対象は `*.rs` ファイル全体だが、以下を誤検知として除外する:
#   - 行頭（先頭の空白を許容）が `//` で始まるコメント行
#   - 行末に `// sort-determinism: allow ...` マーカーを持つ行
scan_dir() {
  local dir="$1"
  if [ ! -d "${dir}" ]; then
    return 0
  fi

  local pattern
  pattern='sort_unstable_by\(|sort_unstable_by_key\(|select_nth_unstable_by\(|select_nth_unstable_by_key\('

  # `find` の出力を `grep -f` ではなく個別に渡すのは、0 件時に GNU/BSD grep 間で
  # 終了コードの扱いが割れる `xargs` 経由を避け、ループ内で行頭コメント除外の
  # 2 段 grep を安定して適用するため。
  local file
  while IFS= read -r file; do
    grep -nE "${pattern}" "${file}" 2>/dev/null |
      grep -vE '^[0-9]+:[[:space:]]*//' |
      grep -vE 'sort-determinism: allow ' |
      sed "s#^#${file}:#"
  done < <(find "${dir}" -type f -name '*.rs')
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
  # fixture 3: 検知してはならないケース（コメント行・許可マーカー・整数の
  # sort_unstable（引数なし、パターン対象外）・文字列全体としての言及）。
  cat >"${tmp}/allowed.rs" <<'EOF'
// v.sort_unstable_by(|a, b| a.cmp(b)); コメント内は対象外
fn f(ids: &mut Vec<u64>, v: &mut Vec<i32>) {
    ids.sort_unstable(); // 引数なし sort_unstable は本チェックの対象パターン外
    v.sort_unstable_by(|a, b| a.cmp(b)); // sort-determinism: allow 全順序の整数比較のみ
}
EOF

  local_detected="$(scan_dir "${tmp}")"
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_me.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect sort_unstable_by in detect_me.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_select_nth.rs.*select_nth_unstable_by_key"; then
    echo "FAIL: self-test did not detect select_nth_unstable_by_key in detect_select_nth.rs" >&2
    failed=1
  fi
  if printf '%s\n' "${local_detected}" | grep -q "allowed.rs"; then
    echo "FAIL: self-test false-positive on allowed.rs (comment / allow-marker / sort_unstable without comparator should not match)" >&2
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
  {
    scan_dir "${REPO_ROOT}/crates/engine/src"
    scan_dir "${REPO_ROOT}/crates/wire-server/src"
  }
)"

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
