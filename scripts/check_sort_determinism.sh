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
# 検知対象（コメント・文字列リテラル・char リテラルは除外し、実コードのみを
# 走査する。詳細は scan_dir 内コメント参照）:
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
# 誤検知しない」「コメント・文字列リテラル中の一致は誤検知しない」ことを検証する
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

# `dir` 配下（存在しなければスキップ）の `*.rs` を対象に、非決定的ソート API の
# 呼び出しを検知する。マッチ行を stdout へ出力し、1 件も無ければ何も出力しない
# （呼び出し側が出力有無で成否判定する）。
#
# 完全な lexer/parser は持たない軽量チェックだが、単純な「行頭 `//` だけを除外
# して正規表現を全体適用する」実装は、ブロックコメント (`/* ... */`) や文字列
# リテラル中の `sort_unstable_by(` を実コードとして誤検知する一方、
# `sort_unstable_by /* comment */ (` のようにコメントを挟んだ有効な呼び出しは
# 検知漏れになる（codex-review P2 指摘）。そのため以下の 2 段階で処理する:
#
#   1. `mask_non_code` (perl): ファイル全体を走査し、行コメント (`//...`)・
#      ブロックコメント（`/* ... */`、ネスト対応）・文字列リテラル
#      （通常/バイト文字列と `r"..."`/`r#"...#"` 等の raw 文字列）・char
#      リテラル (`'x'`, `'\n'` 等。ライフタイム `'a` とは非貪欲マッチで区別)
#      の中身を、行・桁位置を保ったまま半角スペースへ置換する（改行はそのまま
#      保持し、行番号の対応関係を崩さない）。これにより「コメント・文字列内の
#      誤検知」と「コメントを挟んだ呼び出しの検知漏れ」の両方を同時に解消する
#      （`\s*` はマスク後の空白へそのまま一致するため、ブロックコメントを挟んだ
#      呼び出しも 1 つの正規表現でまたいで検知できる）。
#   2. マスク後のテキストに対して API 呼び出しパターンを走査する（API 名と
#      `(` の間の改行・空白を許容する `\s*`。2 回目の codex-review P1 指摘対応）に加え、
#      turbofish 付き呼び出し（`sort_unstable_by::<_>(...)` 等）の API 名と `(` の
#      間に挟まる `::<...>` も許容する（3 回目の codex-review P1 指摘対応）。turbofish
#      の中身は Perl の名前付き再帰サブパターン
#      `(?<angle><(?:->|[^<>]|(?&angle))*>)` で山括弧のネストを追跡して読み取る
#      （`[^>]*` の単純実装では `sort_unstable_by::<fn(&Vec<i32>, &Vec<i32>) ->
#      Ordering>(...)` のように型引数内に `>` を含む呼び出しで内側の `>` で
#      早期にマッチが終了し検知漏れになっていた。4 回目の codex-review P1
#      指摘対応）。関数ポインタの戻り値矢印 `->` はそれ自体に `>` を含むが
#      閉じ括弧ではないため、`->` を 1 トークンとして先に許容し、素の `>` とは
#      区別する（区別しないと `-> Ordering>` の `>`（矢印側）で山括弧の対応が
#      崩れ、直後の実在する閉じ `>` の手前で誤ってマッチが終了する）。単一行・
#      複数行いずれのネスト付き turbofish もマッチする。
#      `// sort-determinism: allow ...` 許可マーカーの判定は、マーカー自体が
#      コメント中にしか書けないため、マスク前の元テキスト（`@lines`）を参照する。
#
# fail-closed: `find`・`perl` の失敗（構文エラー・実行不能・読み取り失敗等）を
# 握り潰さず scan_dir の非ゼロ終了として呼び出し元へ伝播する（従来は
# `perl ... | sed ...` を素通しし、末尾の `sed` が成功すれば `set -u` のみでは
# パイプライン全体が成功扱いになっていた。`pipefail`（ファイル先頭で有効化）に
# 加え、本関数内では `find`/`perl` を個別にコマンド置換で実行し `$?` を明示
# チェックすることで、process substitution 経由でも失敗を確実に検知する）。
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
      # 行コメント・ブロックコメント・文字列/char リテラルの中身を、行・桁位置を
      # 保ったまま半角スペースへ置換する（改行は保持）。戻り値は元テキストと
      # 同じ長さ・同じ行数の文字列。
      sub mask_non_code {
        my ($s) = @_;
        my $out = "";
        pos($s) = 0;
        my $len = length($s);
        while (pos($s) < $len) {
          if ($s =~ /\G\/\/[^\n]*/gc) {
            $out .= (" " x length($&));
          } elsif ($s =~ /\G\/\*/gc) {
            my $depth = 1;
            $out .= "  ";
            while ($depth > 0 && pos($s) < $len) {
              if ($s =~ /\G\/\*/gc) { $depth++; $out .= "  "; }
              elsif ($s =~ /\G\*\//gc) { $depth--; $out .= "  "; }
              elsif ($s =~ /\G(\n)/gc) { $out .= "\n"; }
              elsif ($s =~ /\G./gcs) { $out .= " "; }
              else { last; }
            }
          } elsif ($s =~ /\Gb?r(#*)"/gc) {
            my $hashes = $1;
            $out .= (" " x length($&));
            my $closing = "\"" . $hashes;
            while (pos($s) < $len) {
              if ($s =~ /\G\Q$closing\E/gc) { $out .= (" " x length($closing)); last; }
              elsif ($s =~ /\G(\n)/gc) { $out .= "\n"; }
              elsif ($s =~ /\G./gcs) { $out .= " "; }
              else { last; }
            }
          } elsif ($s =~ /\Gb?"/gc) {
            $out .= (" " x length($&));
            while (pos($s) < $len) {
              if ($s =~ /\G\\(.)/gcs) {
                # エスケープシーケンス（`\n` 等）を 2 文字消費する。第 2 文字が
                # 改行そのもの（行末 `\` による Rust の行継続エスケープ）の場合は
                # 半角スペースへ潰さず改行として保持しないと、マスク後テキストの
                # 行数が元テキストとずれてしまう（`.` が `/s` 修飾子で改行にも
                # マッチするための復元漏れ。self-test fixture では未再現の実
                # ソースで顕在化したバグ）。
                $out .= " " . ($1 eq "\n" ? "\n" : " ");
              }
              elsif ($s =~ /\G"/gc) { $out .= " "; last; }
              elsif ($s =~ /\G(\n)/gc) { $out .= "\n"; }
              elsif ($s =~ /\G./gcs) { $out .= " "; }
              else { last; }
            }
          } elsif ($s =~ /\G'"'"'(?:\\.|[^'"'"'\\\n])'"'"'/gc) {
            $out .= (" " x length($&));
          } elsif ($s =~ /\G(\n)/gc) {
            $out .= "\n";
          } elsif ($s =~ /\G(.)/gcs) {
            $out .= $1;
          } else {
            last;
          }
        }
        return $out;
      }

      my $orig = $_;
      my $masked = mask_non_code($orig);
      my @lines = split /\n/, $orig, -1;
      my @masked_lines = split /\n/, $masked, -1;
      if (scalar(@lines) != scalar(@masked_lines)) {
        die "mask_non_code produced a different line count than the input (masking bug)\n";
      }

      my $joined = "";
      my @lineno_of_pos;
      for my $i (0 .. $#masked_lines) {
        my $ln = $i + 1;
        my $text = $masked_lines[$i] . "\n";
        push @lineno_of_pos, ($ln) x length($text);
        $joined .= $text;
      }
      while ($joined =~ /\b(sort_unstable_by(?:_key)?|select_nth_unstable_by(?:_key)?)\s*(?:::\s*(?<angle><(?:->|[^<>]|(?&angle))*>)\s*)?\(/gs) {
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
  # fixture 7: 検知すべきケース（API 名と `(` の間にブロックコメントを挟んだ
  # 呼び出し。テキスト走査ではコメントを挟む呼び出しを検知漏れしうるという
  # codex-review P2 指摘の再現ケース）。
  cat >"${tmp}/detect_block_comment_split.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    v.sort_unstable_by /* score-order tie-break */ (|a, b| a.cmp(b));
}
EOF
  # fixture 10: 検知すべきケース（行末 `\` による Rust の行継続エスケープを
  # 含む文字列リテラルの直後に実コードがある場合。マスク処理が改行を正しく
  # 保持できず行番号がずれるバグの再現ケース。この呼び出し自体は文字列の外の
  # 実コードなので検知しなければならない）。
  cat >"${tmp}/detect_after_string_continuation.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    let _msg = "foo \
        bar";
    v.sort_unstable_by(|a, b| a.cmp(b));
}
EOF
  # fixture 11: 検知すべきケース（turbofish 付き呼び出し・単一行。
  # `sort_unstable_by::<_>(...)` のように API 名と `(` の間に `::<...>` が
  # 挟まる形式は Rust として有効だが、`\s*\(` のみの旧パターンでは検知漏れに
  # なっていたという 3 回目の codex-review P1 指摘の再現ケース）。
  cat >"${tmp}/detect_turbofish.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    v.sort_unstable_by::<_>(|a, b| a.cmp(b));
}
EOF
  # fixture 12: 検知すべきケース（turbofish 付き呼び出し・複数引数の
  # ジェネリクスかつ複数行）。
  cat >"${tmp}/detect_turbofish_multiline.rs" <<'EOF'
fn f(v: &mut [(i32, i32)]) {
    v.select_nth_unstable_by_key::<_, _>(
        0,
        |x| x.0,
    );
}
EOF
  # fixture 13: 検知すべきケース（型引数内に `>` を含むネストした turbofish。
  # `<[^>]*>` の単純パターンでは型引数中の内側 `>`（`Vec<i32>` の閉じ括弧）で
  # マッチが終了し、その後に本来続くはずの `(` が見つからず検知漏れになって
  # いたという 4 回目の codex-review P1 指摘の再現ケース）。
  cat >"${tmp}/detect_turbofish_nested.rs" <<'EOF'
use std::cmp::Ordering;

fn f(v: &mut Vec<i32>, cmp: fn(&Vec<i32>, &Vec<i32>) -> Ordering) {
    let mut vv: Vec<Vec<i32>> = vec![v.clone()];
    vv.sort_unstable_by::<fn(&Vec<i32>, &Vec<i32>) -> Ordering>(cmp);
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
  # fixture 8: 検知してはならないケース（文字列リテラル中の言及。テキスト
  # 走査ではコメント・文字列リテラルを実コードと区別できないという
  # codex-review P2 指摘の再現ケース）。
  cat >"${tmp}/allowed_string_literal.rs" <<'EOF'
fn f() -> &'static str {
    "call v.sort_unstable_by(|a, b| a.cmp(b)) is forbidden by lint"
}
EOF
  # fixture 9: 検知してはならないケース（ブロックコメント中の言及。単一行・
  # 複数行の両方を確認する）。
  cat >"${tmp}/allowed_block_comment.rs" <<'EOF'
fn f(v: &mut Vec<i32>) {
    /* do not call v.sort_unstable_by(|a, b| a.cmp(b)) here */
    /* multi-line block comment mentioning
       v.sort_unstable_by(|a, b| a.cmp(b))
       across several lines */
    v.sort_by(|a, b| a.cmp(b));
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
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_block_comment_split.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect sort_unstable_by split by a block comment in detect_block_comment_split.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_after_string_continuation.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect sort_unstable_by after a string literal with a line-continuation escape in detect_after_string_continuation.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_turbofish.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect turbofish-qualified sort_unstable_by::<_>(...) in detect_turbofish.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_turbofish_multiline.rs.*select_nth_unstable_by_key"; then
    echo "FAIL: self-test did not detect multiline turbofish-qualified select_nth_unstable_by_key::<_, _>(...) in detect_turbofish_multiline.rs" >&2
    failed=1
  fi
  if ! printf '%s\n' "${local_detected}" | grep -q "detect_turbofish_nested.rs.*sort_unstable_by"; then
    echo "FAIL: self-test did not detect nested-turbofish-qualified sort_unstable_by::<fn(&Vec<i32>, &Vec<i32>) -> Ordering>(...) in detect_turbofish_nested.rs" >&2
    failed=1
  fi
  if printf '%s\n' "${local_detected}" | grep -q "allowed.rs\|allowed_multiline.rs\|allowed_string_literal.rs\|allowed_block_comment.rs"; then
    echo "FAIL: self-test false-positive on allowed*.rs (comment / allow-marker / string literal / sort_unstable without comparator should not match)" >&2
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
