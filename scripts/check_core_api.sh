#!/usr/bin/env bash
# TASK-125（対象ビヘイビア: CORE-1。ポインタ: docs/spec/05-tasks.md TASK-125）の
# コア API シグネチャ差分検知チェック。
#
# `crates/engine/src/core.rs` の `pub trait VectorCore` と `crates/engine/src/kernel.rs`
# の `pub trait SearchProvider`、および両シグネチャが参照する公開型
# （`PolicyContext` / `SearchInput` / `SearchHit` / `Row` / `CoreError` / `KernelError`）の
# 定義ブロックを正規化抽出し、コミット済みのゴールデンファイル
# （crates/engine/api/core_api.snapshot）と比較する。trait 本文だけでなく参照先の
# 公開型も対象に含めるのは、trait シグネチャ自体は変わらずとも `SearchInput` への
# 必須フィールド追加等で互換性が破壊されるケースを見逃さないため（PR #139 レビュー
# 対応）。加えて `crates/engine/src/lib.rs` の公開 module 宣言（`pub mod`）・
# re-export（`pub use`）も対象に含め、型定義自体は変わらずとも wire-server からの
# 到達可能性が壊れるケース（`pub mod` の非公開化・`pub use` 削除等）も検知する
# （PR #139 レビュー対応: codex-review P1 指摘）。差分があればプロトコル層
# （wire-server 等）に影響しうるコア API 変更が意図せず紛れ込んだ可能性がある
# とみなし、非 0 終了する（Makefile の core-api-check ターゲット・CI の
# core-api-check ジョブから呼ばれる。cargo を使わない軽量テキストチェックのため
# rust-ci ジョブとは独立に実行できる）。
#
# 正当な API 変更時は本スクリプトを --update 付きで再実行してスナップショットを
# 再生成し、その差分を明示的なコミットとしてレビューへ残す運用とする
# （変更を禁止するチェックではなく、変わったことを可視化するチェック）。
#
# `--self-test` は extract_item_block 自体の回帰テストモード。実ソースを使わず
# その場で用意した fixture に対して抽出結果を検証するため、
# `crates/engine/src/policy.rs` 等が「たまたま今は複数行属性・複数 impl ブロックを
# 含まない」状態でも、抽出ロジックの回帰（PR #139 レビュー対応の 2 点）を検知できる
# （make core-api-check・CI の core-api-check ジョブから呼ばれる。fixture 相当を
# 恒久的にソースへ埋め込むのは公開 API を汚すため避け、スクリプト内蔵の自己テストと
# する）。
#
# 使い方: scripts/check_core_api.sh [--update|--self-test]

set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SNAPSHOT="${REPO_ROOT}/crates/engine/api/core_api.snapshot"

MODE="check"
if [ "${1:-}" = "--update" ]; then
  MODE="update"
elif [ "${1:-}" = "--self-test" ]; then
  MODE="self-test"
elif [ "${1:-}" != "" ]; then
  echo "ERROR: unknown argument: ${1}" >&2
  echo "usage: $(basename "$0") [--update|--self-test]" >&2
  exit 1
fi

# `item_pattern`（例: `pub trait VectorCore`・`pub struct SearchInput`）に一致する行から
# 対応する `^}` 行までを 1 ブロックとして取り出す。加えて、そのブロック直前に連続する
# 外側属性行（`^#[...]`。`#[cfg(...)]`・`#[derive(...)]` 等）も抽出対象へ含める
# （PR #139 レビュー対応: 抽出開始位置を trait/struct/enum 行固定にすると、直前の
# `#[cfg(...)]` 等の変更が比較対象から漏れるため）。`///`・`//` コメント行と空行は
# 除去し、行末空白を正規化する（インデントは保持し、実質的な差分だけを比較対象に
# する）。対象ブロックが 1 件も見つからない場合は fail-closed でエラー終了する
# （名前変更・ファイル移動でチェックが空振り green になることを防ぐ）。
extract_item_block() {
  local file="$1"
  local item_pattern="$2"
  local item_label="$3"

  if [ ! -f "${file}" ]; then
    echo "ERROR: source file not found: ${file}" >&2
    return 1
  fi

  # パターン直後に識別子構成文字（英数字・アンダースコア）が続く場合は、より長い
  # 別名（例: `pub struct Row` に対する `pub struct RowInput`）への誤マッチとみなし
  # 除外する（単語境界判定）。
  #
  # attrbuf（直前の外側属性行の蓄積バッファ）は `#[...]` 行以外なら常にクリアして
  # いたが、`///` ドキュメンテーションコメント行や、外側属性とアイテム本体の間の
  # 空行を挟む配置（`#[derive(...)]\n///...\npub struct Foo`）でも先行する属性が
  # 比較前に失われてしまい、属性の変更を見逃す可能性があった。ドキュメントコメント
  # 行・空行は属性の連続とみなしてバッファを保持し、それ以外の実コード行でのみ
  # クリアする。加えて、`#[cfg_attr(\n  ...,\n)]` のように角括弧が複数行へまたがる
  # 外側属性は、`^#[` 一致の先頭行だけを蓄積すると継続行が欠落していた。角括弧の
  # 対応（`[` と `]` の出現数の差）を追跡し、開いた角括弧が閉じきるまで後続行を
  # 無条件に属性の継続とみなして蓄積する（PR #139 レビュー対応: codex-review P1
  # 指摘）。
  #
  # `found` 状態はブロック終端（`^}`）到達時に `exit` せず `found=0` に戻すのみと
  # し、ファイル全体の走査を継続する。これにより、同一パターン（例:
  # `impl PolicyContext`）が複数ブロックに分割されて出現する場合も、最初の
  # 1 ブロックで打ち切らず全ブロックを抽出対象に含める（PR #139 レビュー対応:
  # Cursor Bugbot 指摘）。
  local block
  block="$(awk -v item="${item_pattern}" '
    function bracket_delta(s,    n, i, c, d) {
      d = 0
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "[") d++
        else if (c == "]") d--
      }
      return d
    }
    found {
      print
      if ($0 ~ /^}/) { found = 0; attrbuf = "" }
      next
    }
    in_attr {
      attrbuf = attrbuf $0 "\n"
      attr_depth += bracket_delta($0)
      if (attr_depth <= 0) in_attr = 0
      next
    }
    $0 ~ ("^" item "([^A-Za-z0-9_]|$)") {
      printf "%s", attrbuf
      print
      found = 1
      next
    }
    /^#\[/ {
      attrbuf = attrbuf $0 "\n"
      attr_depth = bracket_delta($0)
      if (attr_depth > 0) in_attr = 1
      next
    }
    /^[[:space:]]*\/\// { next }
    /^[[:space:]]*$/ { next }
    { attrbuf = "" }
  ' "${file}")"

  if [ -z "${block}" ]; then
    echo "ERROR: item block not found: ${item_label} in ${file}" >&2
    return 1
  fi
  # 抽出末尾が `^}` で終わっていない場合（ファイル末尾まで到達して抜けた等）も、
  # 閉じ括弧を検出できなかった不完全な抽出として fail-closed にする。
  local last_line
  last_line="$(printf '%s\n' "${block}" | tail -n 1)"
  if [ "${last_line}" != "}" ]; then
    echo "ERROR: item block for ${item_label} in ${file} has no closing brace (extraction incomplete)" >&2
    return 1
  fi

  printf '%s\n' "${block}" | sed -e '/^[[:space:]]*\/\/\//d' -e '/^[[:space:]]*\/\//d' -e '/^[[:space:]]*$/d' -e 's/[[:space:]]*$//'
}

# `crates/engine/src/lib.rs` の公開 module 宣言（`pub mod ...;`）・re-export
# （`pub use ...;`）を抽出する。extract_item_block は型定義本体のみを対象とし、
# `pub mod core` の非公開化（`mod core;` への変更）や `pub use` 削除といった
# 「到達性」の破壊的変更（wire-server から型そのものへ到達できなくなる変更）を
# 検知できなかったため追加した（PR #139 レビュー対応: codex-review P1 指摘）。
# `pub use` は複数行へ折り返される場合があるため、宣言終端（行末が `;`）まで
# 継続行とみなして蓄積する。対象宣言が 1 件も見つからない場合は fail-closed で
# エラー終了する（`extract_item_block` と同じ方針）。
extract_reachability() {
  local file="$1"
  local label="$2"

  if [ ! -f "${file}" ]; then
    echo "ERROR: source file not found: ${file}" >&2
    return 1
  fi

  local block
  block="$(awk '
    in_decl {
      print
      if ($0 ~ /;[[:space:]]*$/) { in_decl = 0 }
      next
    }
    /^pub (mod|use)[[:space:]]/ {
      print
      if ($0 ~ /;[[:space:]]*$/) { in_decl = 0 } else { in_decl = 1 }
      next
    }
  ' "${file}")"

  if [ -z "${block}" ]; then
    echo "ERROR: no public module/re-export declarations found: ${label} in ${file}" >&2
    return 1
  fi

  printf '%s\n' "${block}" | sed -e 's/[[:space:]]*$//'
}

# extract_item_block・extract_reachability の回帰テスト（PR #139 レビュー対応:
# codex-review P1「複数行の外側属性が欠落する」・Cursor Bugbot「同一パターンの複数
# impl ブロックのうち最初の 1 件で走査を打ち切る」・codex-review P1「lib.rs の
# module/re-export 到達性変更を検知できない」の 3 点を fixture で固定する）。
# `crates/engine/src` の実ソースは現時点で複数行属性・複数 inherent impl ブロックを
# 含まないため、実ソース経由の `check_core_api.sh`（引数なし）の green だけではこの
# 3 点の回帰を検知できない。一時ディレクトリに fixture を書き出し、期待する抽出結果と
# 突き合わせる。
#
# 後始末（一時ディレクトリの削除）は `trap ... RETURN` ではなく関数末尾での明示的な
# `rm -rf` で行う。bash の `RETURN` トラップはトラップコマンド自体の終了ステータスで
# 関数の戻り値を上書きしうるため（bash のバージョン・実装依存の曖昧さがある）、
# self-test の FAIL が trap 経由で exit 0 に握り潰される懸念を排除する（PR #139
# レビュー対応: Cursor Bugbot 指摘）。判定結果は `failed` 変数へ確定させた上で、
# cleanup を実行してから `return "${failed}"` する。
self_test() {
  local tmp
  tmp="$(mktemp -d)" || {
    echo "ERROR: mktemp -d failed" >&2
    return 1
  }

  local failed=0

  # fixture 1: `#[cfg_attr(...)]` が複数行へ折り返される公開 fn。
  local attr_file="${tmp}/multiline_attr.rs"
  cat >"${attr_file}" <<'EOF'
#[cfg_attr(
    feature = "foo",
    derive(Debug)
)]
pub fn hello(x: i32) -> i32 {
    x + 1
}
EOF
  local attr_expected
  attr_expected="$(cat <<'EOF'
#[cfg_attr(
feature = "foo",
derive(Debug)
)]
pub fn hello(x: i32) -> i32 {
x + 1
}
EOF
)"
  # extract_item_block はインデントを保持するが、期待値の比較は sed で行末空白と
  # インデントを正規化した上で行う（fixture のインデント量そのものは本テストの
  # 検証対象ではないため）。
  local attr_actual
  attr_actual="$(extract_item_block "${attr_file}" "pub fn hello" "self-test: multiline cfg_attr" 2>&1)"
  if [ "$(printf '%s\n' "${attr_actual}" | sed -e 's/^[[:space:]]*//')" != "${attr_expected}" ]; then
    echo "FAIL: self-test multiline cfg_attr — attribute continuation lines were not captured" >&2
    echo "--- actual ---" >&2
    printf '%s\n' "${attr_actual}" >&2
    failed=1
  fi

  # fixture 2: 同一型に対する分割された複数の inherent impl ブロック。
  local impl_file="${tmp}/multi_impl.rs"
  cat >"${impl_file}" <<'EOF'
pub struct Foo {
    pub a: i32,
}

impl Foo {
    pub fn a(&self) -> i32 {
        self.a
    }
}

pub struct Bar;

impl Foo {
    pub fn b(&self) -> i32 {
        42
    }
}
EOF
  local impl_actual
  impl_actual="$(extract_item_block "${impl_file}" "impl Foo" "self-test: multiple inherent impl blocks" 2>&1)"
  local impl_block_count
  impl_block_count="$(printf '%s\n' "${impl_actual}" | grep -c '^impl Foo {')"
  if [ "${impl_block_count}" != "2" ]; then
    echo "FAIL: self-test multiple inherent impl blocks — expected 2 impl blocks, got ${impl_block_count}" >&2
    echo "--- actual ---" >&2
    printf '%s\n' "${impl_actual}" >&2
    failed=1
  fi

  # fixture 3: lib.rs 相当のファイルで公開 module 宣言・複数行 `pub use` re-export を
  # 正しく抽出できること、および module の非公開化（`pub mod` -> `mod`）や re-export
  # 削除といった到達性の破壊的変更が抽出結果へ反映される（見逃されない）ことを検証する。
  local lib_before="${tmp}/lib_before.rs"
  cat >"${lib_before}" <<'EOF'
pub mod core;
pub mod kernel;
pub use core::{
    VectorCore,
    CoreError,
};
EOF
  local lib_before_actual
  lib_before_actual="$(extract_reachability "${lib_before}" "self-test: reachability before" 2>&1)"
  local lib_before_expected
  lib_before_expected="$(cat <<'EOF'
pub mod core;
pub mod kernel;
pub use core::{
    VectorCore,
    CoreError,
};
EOF
)"
  if [ "${lib_before_actual}" != "${lib_before_expected}" ]; then
    echo "FAIL: self-test reachability — pub mod/multi-line pub use were not captured as expected" >&2
    echo "--- actual ---" >&2
    printf '%s\n' "${lib_before_actual}" >&2
    failed=1
  fi

  local lib_after="${tmp}/lib_after.rs"
  cat >"${lib_after}" <<'EOF'
mod core;
pub mod kernel;
EOF
  local lib_after_actual
  lib_after_actual="$(extract_reachability "${lib_after}" "self-test: reachability after" 2>&1)"
  if [ "${lib_after_actual}" = "${lib_before_actual}" ]; then
    echo "FAIL: self-test reachability — module de-publicization / re-export removal was not detected as a diff" >&2
    failed=1
  fi
  if printf '%s\n' "${lib_after_actual}" | grep -q '^pub mod core;$'; then
    echo "FAIL: self-test reachability — private \`mod core;\` was misdetected as \`pub mod core;\`" >&2
    failed=1
  fi

  # 一時ディレクトリの後始末（trap RETURN は使わない。上記コメント参照）。
  rm -rf "${tmp}"

  if [ "${failed}" -eq 0 ]; then
    echo "ok: extract_item_block / extract_reachability self-test passed"
  fi
  return "${failed}"
}

if [ "${MODE}" = "self-test" ]; then
  self_test
  exit $?
fi

CORE_FILE="${REPO_ROOT}/crates/engine/src/core.rs"
KERNEL_FILE="${REPO_ROOT}/crates/engine/src/kernel.rs"
POLICY_FILE="${REPO_ROOT}/crates/engine/src/policy.rs"
STORAGE_FILE="${REPO_ROOT}/crates/engine/src/storage.rs"
CATALOG_FILE="${REPO_ROOT}/crates/engine/src/catalog.rs"
ARENA_FILE="${REPO_ROOT}/crates/engine/src/arena.rs"
LIB_FILE="${REPO_ROOT}/crates/engine/src/lib.rs"

# lib.rs の到達性（公開 module 宣言・re-export）をスナップショットへ含める
# （codex-review P1 指摘: `pub mod` の非公開化・`pub use` 削除は型定義そのものは
# 変わらなくても wire-server からの到達可能性を破壊するため、extract_item_block とは
# 独立に検知する）。
LIB_REACHABILITY_BLOCK="$(extract_reachability "${LIB_FILE}" "crates/engine/src/lib.rs :: reachability (pub mod / pub use)")" || exit 1

# 抽出対象: (ソースファイル, awk 抽出パターン, スナップショット上のラベル)。
# trait 本体（VectorCore・SearchProvider）に加え、両シグネチャが直接・推移的に参照する
# 公開型・公開コンストラクタを含める（PR #139 レビュー対応: codex-review P1 指摘。
# `CoreError` の variant payload である `StorageError`/`CatalogError`/`ArenaError`/
# `PolicyError`、`Row` の公開フィールド型である `Visibility`、`PolicyContext` の公開
# コンストラクタ（`impl PolicyContext` ブロック丸ごと）まで対象を広げないと、
# プロトコル層が実際に依存するこれらの API を破壊的に変更しても検知できないため）。
ITEMS=(
  "${CORE_FILE}|pub trait VectorCore|crates/engine/src/core.rs :: VectorCore"
  "${KERNEL_FILE}|pub trait SearchProvider|crates/engine/src/kernel.rs :: SearchProvider"
  "${CORE_FILE}|pub enum CoreError|crates/engine/src/core.rs :: CoreError"
  "${KERNEL_FILE}|pub struct SearchHit|crates/engine/src/kernel.rs :: SearchHit"
  "${KERNEL_FILE}|pub enum KernelError|crates/engine/src/kernel.rs :: KernelError"
  "${KERNEL_FILE}|pub struct SearchInput|crates/engine/src/kernel.rs :: SearchInput"
  "${POLICY_FILE}|pub struct PolicyContext|crates/engine/src/policy.rs :: PolicyContext"
  "${POLICY_FILE}|impl PolicyContext|crates/engine/src/policy.rs :: PolicyContext (公開コンストラクタ・メソッド)"
  "${POLICY_FILE}|pub enum PolicyError|crates/engine/src/policy.rs :: PolicyError"
  "${STORAGE_FILE}|pub struct Row|crates/engine/src/storage.rs :: Row"
  "${STORAGE_FILE}|pub enum Visibility|crates/engine/src/storage.rs :: Visibility"
  "${STORAGE_FILE}|pub enum StorageError|crates/engine/src/storage.rs :: StorageError"
  "${CATALOG_FILE}|pub enum CatalogError|crates/engine/src/catalog.rs :: CatalogError"
  "${ARENA_FILE}|pub enum ArenaError|crates/engine/src/arena.rs :: ArenaError"
)

GENERATED_PARTS=()
GENERATED_PARTS+=("# source: crates/engine/src/lib.rs :: reachability (pub mod / pub use)"$'\n'"${LIB_REACHABILITY_BLOCK}")
for entry in "${ITEMS[@]}"; do
  IFS='|' read -r file pattern label <<<"${entry}"
  block="$(extract_item_block "${file}" "${pattern}" "${label}")" || exit 1
  GENERATED_PARTS+=("# source: ${label}"$'\n'"${block}")
done

GENERATED="$(
  {
    echo "# generated by scripts/check_core_api.sh --update (TASK-125). Do not hand-edit."
    for part in "${GENERATED_PARTS[@]}"; do
      echo
      printf '%s\n' "${part}"
    done
  }
)"

if [ "${MODE}" = "update" ]; then
  mkdir -p "$(dirname "${SNAPSHOT}")"
  printf '%s\n' "${GENERATED}" >"${SNAPSHOT}"
  echo "updated: ${SNAPSHOT}"
  exit 0
fi

if [ ! -f "${SNAPSHOT}" ]; then
  echo "ERROR: snapshot not found: ${SNAPSHOT}" >&2
  echo "run: scripts/check_core_api.sh --update" >&2
  exit 1
fi

if ! diff -u "${SNAPSHOT}" <(printf '%s\n' "${GENERATED}"); then
  echo "" >&2
  echo "ERROR: core API signature changed; if intentional, run scripts/check_core_api.sh --update and commit the snapshot" >&2
  exit 1
fi

echo "ok: core API signature matches ${SNAPSHOT}"
