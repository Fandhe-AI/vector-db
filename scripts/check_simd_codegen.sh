#!/usr/bin/env bash
# SIMD カーネルの生成コード検査ガード（対応 Issue #467。ポインタ:
# `docs/spec/05-tasks.md` TASK-156・`docs/spec/04-behavior/core-engine.md`
# CORE-14）。
#
# 動機（詳細・基線実測は `docs/design/simd-codegen-guard.md` 参照。spec 本文は
# 転記しない）: `crates/engine/src/isa.rs` の `dot_avx2_fma`／`dot_avx512`／
# `dot_neon` は intrinsics 不使用（`#[target_feature]` fn ＋ `dot_lanes` の
# LLVM 自動ベクトル化）だが、Phase 4（ADR 参照）では `as_chunks` の固定長配列
# から `_mm256_set_ps` 等で SIMD 値を構築する方式（新規 unsafe を持たない）を
# 採る予定であり、これが「単一ロード命令へ畳み込まれる」のは LLVM の最適化
# 挙動であって言語仕様の保証ではない。toolchain（floating stable）更新で
# 退行しうるため、`cargo rustc --emit asm` で実際の生成コードを検査し、
# `vinsertps`／`vpinsr*`／`vunpck[lh]ps` 等の要素ごと挿入命令（ストライド・
# 非連続要素からの SIMD 値構築で現れる）が出現していないことを CI で固定する。
#
# 検査方式（`scripts/check_sort_determinism.sh`・`scripts/check_core_api.sh`
# と同様、fail-closed。詳細は docs/design/simd-codegen-guard.md 参照）:
#   1. カバレッジガード: `crates/engine/src/isa.rs`（存在すれば
#      `crates/engine/src/isa/*.rs` も含む）から `#[target_feature]` 直後の
#      fn 名を抽出し、本スクリプトのカーネル表と一致することを確認する
#      （新規カーネル追加時の登録漏れを検知する）。
#   2. `cargo rustc --release -p engine --lib -- --emit asm` を専用
#      `CARGO_TARGET_DIR` でビルドし、生成された単一の `.s` ファイルから
#      各カーネル関数のシンボルを mangled 名（legacy/v0 両対応）で解決する。
#   3. 関数本体（ラベル行 〜 `.Lfunc_end`）を抽出し、禁止命令（要素ごと挿入）
#      が 0 件・広幅 FMA（非 vacuous 検査）が 1 件以上であることを確認する。
#
# `--self-test` は検査ロジック自体の回帰テスト。cargo を使わず `rustc -O
# --crate-type lib --emit asm` で用意した fixture に対し、pass/fail/ERROR の
# 各ケースが期待どおりに判定されることを検証する（make simd-codegen-check・
# CI の simd-codegen-check ジョブから呼ばれる）。
#
# 使い方: scripts/check_simd_codegen.sh [--self-test]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# x86_64 で検査対象とするカーネル関数名（`isa.rs` の `#[target_feature]` fn 名と
# 一致させる。aarch64 の `dot_neon` は baseline NEON のため呼び出し元
# `SimdKernel::dot` へインライン化され独立シンボルを持たず、本ガードの対象外
# （詳細・確認結果は docs/design/simd-codegen-guard.md 参照。aarch64 向け CI
# 配線は別 ADR の判断事項）。
X86_64_KERNELS=(dot_avx2_fma dot_avx512)

MODE="check"
if [ "${1:-}" = "--self-test" ]; then
  MODE="self-test"
elif [ "${1:-}" != "" ]; then
  echo "ERROR: unknown argument: ${1}" >&2
  echo "usage: $(basename "$0") [--self-test]" >&2
  exit 1
fi

# 要素ごと挿入命令（ストライド・非連続要素から SIMD レジスタを構築する際に
# 現れる）。AT&T 構文（objdump/llvm-mc 既定）を前提に mnemonic を照合する。
FORBIDDEN_PATTERN='^\s*(vinsertps|insertps|vpinsr[bwdq]|pinsr[bwdq]|vunpck[lh]ps)\b'
# 非 vacuous 検査: 広幅（ymm/zmm）の FMA が実際に使われていることを要求する
# （検査対象の関数本体抽出そのものが壊れて空振り green になる事故を防ぐ）。
WIDE_FMA_PATTERN='^\s*vfmadd[0-9]{3}ps\b.*%(ymm|zmm)[0-9]+'

# <asm ファイル> <mangled 名の接頭辞> <カーネル名...> を受け取り、各カーネルの
# シンボルを解決して禁止命令・非 vacuous 条件を検査する。実検査・self-test の
# 両方がこの関数を経由することで検査ロジックを単一化する。
# 戻り値: 全カーネル PASS なら 0、1 件でも FAIL/ERROR があれば非ゼロ。
check_asm_file() {
  local asm_file="$1"
  local prefix="$2"
  shift 2
  local kernels=("$@")

  if [ ! -f "${asm_file}" ]; then
    echo "ERROR: asm file not found: ${asm_file}" >&2
    return 1
  fi

  local overall_status=0
  local kernel
  for kernel in "${kernels[@]}"; do
    local name_len=${#kernel}
    # legacy mangling: _ZN...<prefix><len><name>17h<16進>E:
    # v0 mangling:      _R...<prefix><len><name>:
    local legacy_re="^_ZN[A-Za-z0-9_\$.]*${prefix}${name_len}${kernel}17h[0-9a-f]+E:\$"
    local v0_re="^_R[A-Za-z0-9_\$.]*${prefix}${name_len}${kernel}:\$"

    local label_lines
    label_lines="$(grep -nE "${legacy_re}|${v0_re}" "${asm_file}" || true)"
    local hit_count
    hit_count="$(printf '%s\n' "${label_lines}" | grep -c . || true)"

    if [ "${hit_count}" -eq 0 ]; then
      echo "ERROR: symbol for kernel '${kernel}' not found in ${asm_file} (fail-closed: cannot verify codegen)" >&2
      overall_status=1
      continue
    fi
    if [ "${hit_count}" -gt 1 ]; then
      echo "ERROR: symbol for kernel '${kernel}' matched ${hit_count} times in ${asm_file} (ambiguous; fail-closed)" >&2
      overall_status=1
      continue
    fi

    local label_lineno
    label_lineno="$(printf '%s\n' "${label_lines}" | head -n1 | cut -d: -f1)"

    # ラベル行の次行から最初の `.Lfunc_end<N>:` までを関数本体として抽出する。
    # awk 側で `exit` して早期に標準入力を閉じると、`tail` がまだ書き込み中の
    # パイプが壊れて SIGPIPE（終了コード 141）を受け取り、`pipefail` ＋
    # `set -e` の下でスクリプト全体が異常終了してしまう（マーカー以降も
    # フラグで抑制するだけにして最後まで読み切ることで回避する）。
    #
    # `.Lfunc_end` が見つからず EOF まで到達した場合、awk は単に `stop` が
    # 立たないまま最後まで出力するため、body が空になるとは限らない（別関数の
    # 本体まで跨いで抽出してしまい、その中の広幅 FMA を誤って対象関数の
    # ものとしてカウントする恐れがある。fail-closed 契約に反する）。
    # そこでマーカーを実際に検出できたかどうかを END ブロックで明示的な
    # 番兵行として出力し、body の中身とは独立に判定する。
    local raw
    raw="$(tail -n "+$((label_lineno + 1))" "${asm_file}" | awk '
      /^\.Lfunc_end[0-9]+:/ { stop = 1; found = 1 }
      !stop { print }
      END { if (found) print "__SIMD_CODEGEN_FUNC_END_FOUND__"; else print "__SIMD_CODEGEN_FUNC_END_MISSING__" }
    ')"

    local sentinel
    sentinel="$(printf '%s\n' "${raw}" | tail -n1)"
    local body
    body="$(printf '%s\n' "${raw}" | sed '$d')"

    if [ "${sentinel}" != "__SIMD_CODEGEN_FUNC_END_FOUND__" ]; then
      echo "ERROR: kernel '${kernel}': .Lfunc_end marker not found before EOF (fail-closed: cannot bound function body; refusing to count instructions that may belong to a different function)" >&2
      overall_status=1
      continue
    fi

    if [ -z "${body}" ]; then
      echo "ERROR: could not extract function body for kernel '${kernel}' (empty function body; fail-closed)" >&2
      overall_status=1
      continue
    fi

    local forbidden_hits
    forbidden_hits="$(printf '%s\n' "${body}" | grep -inE "${FORBIDDEN_PATTERN}" || true)"
    local wide_fma_hits
    wide_fma_hits="$(printf '%s\n' "${body}" | grep -inE "${WIDE_FMA_PATTERN}" || true)"

    local histogram
    histogram="$(printf '%s\n' "${body}" | grep -oE '^\s*[a-zA-Z][a-zA-Z0-9._]*' | awk '{$1=$1; print}' | sort | uniq -c | sort -rn)"

    if [ -n "${forbidden_hits}" ]; then
      echo "FAIL: kernel '${kernel}' contains element-wise insert instruction(s):" >&2
      printf '%s\n' "${forbidden_hits}" >&2
      echo "-- instruction histogram for ${kernel} --" >&2
      printf '%s\n' "${histogram}" >&2
      overall_status=1
      continue
    fi

    if [ -z "${wide_fma_hits}" ]; then
      echo "FAIL: kernel '${kernel}' has no wide (ymm/zmm) FMA instruction (non-vacuous check failed)" >&2
      echo "-- instruction histogram for ${kernel} --" >&2
      printf '%s\n' "${histogram}" >&2
      overall_status=1
      continue
    fi

    echo "PASS: kernel '${kernel}' (no element-wise insert; wide FMA present)"
    printf '%s\n' "${histogram}"
  done

  return "${overall_status}"
}

if [ "${MODE}" = "self-test" ]; then
  if ! command -v rustc >/dev/null 2>&1; then
    echo "ERROR: rustc not found; cannot run --self-test" >&2
    exit 1
  fi

  tmp="$(mktemp -d)" || {
    echo "ERROR: mktemp -d failed" >&2
    exit 1
  }
  trap 'rm -rf "${tmp}"' EXIT
  failed=0

  compile_fixture() {
    local src="$1"
    local out="$2"
    shift 2
    if ! rustc -O --crate-type lib --crate-name fixture --emit asm -o "${out}" "${src}" "$@" 2>"${tmp}/rustc.err"; then
      echo "ERROR: rustc failed to compile fixture ${src}" >&2
      cat "${tmp}/rustc.err" >&2
      failed=1
      return 1
    fi
    return 0
  }

  # pass fixture 1: unsafe を使わず、連続要素から _mm256_set_ps で SIMD 値を
  # 構築する方式（「unsafe ロードを set 構築へ置換したサンプル」の受け入れ
  # 条件に対応）。
  cat >"${tmp}/pass_set_contig.rs" <<'EOF'
use std::arch::x86_64::*;

#[target_feature(enable = "avx2,fma")]
pub fn dot_set_contig(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: `_mm256_setzero_ps` は引数を取らずレジスタをゼロ初期化するのみで
    // メモリアクセスを伴わない。呼び出しは `#[target_feature(enable =
    // "avx2,fma")]` 関数内で AVX2 が有効な前提の下にある。
    let mut acc = unsafe { _mm256_setzero_ps() };
    let (a_chunks, a_rem) = a.as_chunks::<8>();
    let (b_chunks, b_rem) = b.as_chunks::<8>();
    for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
        // SAFETY: `_mm256_set_ps` はスカラー引数からレジスタへ値を詰めるのみで
        // メモリアクセスを行わないため、`ac`/`bc` の要素数（`as_chunks::<8>`
        // が保証する固定長 8）以外の前提は不要。
        let av = unsafe {
            _mm256_set_ps(ac[7], ac[6], ac[5], ac[4], ac[3], ac[2], ac[1], ac[0])
        };
        let bv = unsafe {
            _mm256_set_ps(bc[7], bc[6], bc[5], bc[4], bc[3], bc[2], bc[1], bc[0])
        };
        // SAFETY: `_mm256_fmadd_ps` はレジスタ引数のみを取る演算命令で
        // メモリアクセスを伴わず、AVX2/FMA が有効な前提下で常に安全。
        acc = unsafe { _mm256_fmadd_ps(av, bv, acc) };
    }
    let mut buf = [0f32; 8];
    // SAFETY: `buf` は直前に確保した `[f32; 8]`（8 要素・16 バイトアライン
    // 不要な `storeu`）であり、`_mm256_storeu_ps` が書き込む 8 要素（32
    // バイト）分の有効な書き込み先である。
    unsafe { _mm256_storeu_ps(buf.as_mut_ptr(), acc) };
    let mut sum: f32 = buf.iter().sum();
    for (x, y) in a_rem.iter().zip(b_rem.iter()) {
        sum += x * y;
    }
    sum
}
EOF

  # fail fixture 1: ストライド（偶数番）要素から _mm256_set_ps を構築する
  # （要素ごと挿入命令 vinsertps が残る書き方の受け入れ条件に対応）。
  cat >"${tmp}/fail_set_strided.rs" <<'EOF'
use std::arch::x86_64::*;

#[target_feature(enable = "avx2,fma")]
pub fn dot_set_strided(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: `_mm256_setzero_ps` は引数を取らずレジスタをゼロ初期化するのみで
    // メモリアクセスを伴わない。呼び出しは `#[target_feature(enable =
    // "avx2,fma")]` 関数内で AVX2 が有効な前提の下にある。
    let mut acc = unsafe { _mm256_setzero_ps() };
    let (a_chunks, _) = a.as_chunks::<16>();
    let (b_chunks, _) = b.as_chunks::<16>();
    for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
        // SAFETY: `_mm256_set_ps` はスカラー引数からレジスタへ値を詰めるのみで
        // メモリアクセスを行わないため、`ac`/`bc` の要素数（`as_chunks::<16>`
        // が保証する固定長 16。ここではストライドで一部要素のみ参照）以外の
        // 前提は不要。
        let av = unsafe {
            _mm256_set_ps(ac[14], ac[12], ac[10], ac[8], ac[6], ac[4], ac[2], ac[0])
        };
        let bv = unsafe {
            _mm256_set_ps(bc[14], bc[12], bc[10], bc[8], bc[6], bc[4], bc[2], bc[0])
        };
        // SAFETY: `_mm256_fmadd_ps` はレジスタ引数のみを取る演算命令で
        // メモリアクセスを伴わず、AVX2/FMA が有効な前提下で常に安全。
        acc = unsafe { _mm256_fmadd_ps(av, bv, acc) };
    }
    let mut buf = [0f32; 8];
    // SAFETY: `buf` は直前に確保した `[f32; 8]`（8 要素・16 バイトアライン
    // 不要な `storeu`）であり、`_mm256_storeu_ps` が書き込む 8 要素（32
    // バイト）分の有効な書き込み先である。
    unsafe { _mm256_storeu_ps(buf.as_mut_ptr(), acc) };
    buf.iter().sum()
}
EOF

  # fail fixture 2 (non-vacuous): スカラーのみで target_feature が付いている
  # だけの関数。禁止命令は 0 件だが広幅 FMA も 0 件であり、非 vacuous 検査で
  # FAIL しなければならない。
  cat >"${tmp}/fail_no_wide_fma.rs" <<'EOF'
#[target_feature(enable = "avx2,fma")]
pub fn dot_scalar_only(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        sum += x * y;
    }
    sum
}
EOF

  compile_fixture "${tmp}/pass_set_contig.rs" "${tmp}/pass_set_contig.s"
  compile_fixture "${tmp}/fail_set_strided.rs" "${tmp}/fail_set_strided.s"
  compile_fixture "${tmp}/fail_no_wide_fma.rs" "${tmp}/fail_no_wide_fma.s"
  # v0 mangling でも pass fixture を再コンパイル（シンボル解決の両形式確認）。
  compile_fixture "${tmp}/pass_set_contig.rs" "${tmp}/pass_set_contig_v0.s" -C symbol-mangling-version=v0

  if check_asm_file "${tmp}/pass_set_contig.s" 7fixture dot_set_contig >"${tmp}/out_pass.log" 2>&1; then
    :
  else
    echo "FAIL: pass_set_contig fixture expected PASS but check_asm_file failed" >&2
    cat "${tmp}/out_pass.log" >&2
    failed=1
  fi

  if check_asm_file "${tmp}/pass_set_contig_v0.s" 7fixture dot_set_contig >"${tmp}/out_pass_v0.log" 2>&1; then
    :
  else
    echo "FAIL: pass_set_contig (v0 mangling) fixture expected PASS but check_asm_file failed" >&2
    cat "${tmp}/out_pass_v0.log" >&2
    failed=1
  fi

  if check_asm_file "${tmp}/fail_set_strided.s" 7fixture dot_set_strided >"${tmp}/out_fail.log" 2>&1; then
    echo "FAIL: fail_set_strided fixture expected FAIL (element-wise insert) but check_asm_file passed" >&2
    cat "${tmp}/out_fail.log" >&2
    failed=1
  else
    if ! grep -q "vinsertps" "${tmp}/out_fail.log"; then
      echo "FAIL: fail_set_strided fixture did not report vinsertps in its failure output" >&2
      cat "${tmp}/out_fail.log" >&2
      failed=1
    fi
  fi

  if check_asm_file "${tmp}/fail_no_wide_fma.s" 7fixture dot_scalar_only >"${tmp}/out_no_fma.log" 2>&1; then
    echo "FAIL: fail_no_wide_fma fixture expected FAIL (non-vacuous check) but check_asm_file passed" >&2
    cat "${tmp}/out_no_fma.log" >&2
    failed=1
  else
    if ! grep -q "non-vacuous check failed" "${tmp}/out_no_fma.log"; then
      echo "FAIL: fail_no_wide_fma fixture did not report the non-vacuous failure reason" >&2
      cat "${tmp}/out_no_fma.log" >&2
      failed=1
    fi
  fi

  # ERROR fixture: 表に載せたカーネル名がバイナリに存在しない（fail-closed）。
  if check_asm_file "${tmp}/pass_set_contig.s" 7fixture dot_missing_symbol >"${tmp}/out_missing.log" 2>&1; then
    echo "FAIL: missing-symbol case expected ERROR but check_asm_file passed" >&2
    cat "${tmp}/out_missing.log" >&2
    failed=1
  else
    if ! grep -q "not found in" "${tmp}/out_missing.log"; then
      echo "FAIL: missing-symbol case did not report the expected ERROR reason" >&2
      cat "${tmp}/out_missing.log" >&2
      failed=1
    fi
  fi

  # ERROR fixture: `.Lfunc_end` マーカーが対象関数のシンボル解決後に一度も
  # 出現しない（EOF まで読み切っても見つからない）ケース。マーカー不在を
  # body の空/非空とは独立に検出できることを固定する（codex-review P1・
  # Cursor Bugbot 指摘: awk がマーカー未検出でも本文を返すため、別関数の
  # 命令列を対象関数のものとして誤カウントしうる fail-open だった）。
  # pass fixture の `.s` からラベル行は残しつつ `.Lfunc_end` 行以降を丸ごと
  # 除去し、「関数本体の途中で切れていて `.Lfunc_end` に到達しない」状態を
  # 再現する。
  awk '
    /^\.Lfunc_end[0-9]+:/ { exit }
    { print }
  ' "${tmp}/pass_set_contig.s" >"${tmp}/truncated_no_func_end.s"

  if check_asm_file "${tmp}/truncated_no_func_end.s" 7fixture dot_set_contig >"${tmp}/out_no_end.log" 2>&1; then
    echo "FAIL: missing-.Lfunc_end case expected ERROR but check_asm_file passed" >&2
    cat "${tmp}/out_no_end.log" >&2
    failed=1
  else
    if ! grep -q "\.Lfunc_end marker not found" "${tmp}/out_no_end.log"; then
      echo "FAIL: missing-.Lfunc_end case did not report the expected ERROR reason" >&2
      cat "${tmp}/out_no_end.log" >&2
      failed=1
    fi
  fi

  if [ "${failed}" -ne 0 ]; then
    echo "FAIL: scripts/check_simd_codegen.sh --self-test" >&2
    exit 1
  fi
  echo "ok: scripts/check_simd_codegen.sh --self-test"
  exit 0
fi

# --- 実検査モード ---

HOST_ARCH="$(uname -m)"
if [ "${HOST_ARCH}" != "x86_64" ]; then
  echo "ERROR: simd-codegen-check currently targets x86_64 hosts only (host: ${HOST_ARCH})." >&2
  echo "aarch64 (dot_neon) is inlined into the caller under baseline NEON and is not" >&2
  echo "covered by this guard yet (see docs/design/simd-codegen-guard.md)." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: cargo not found" >&2
  exit 1
fi
if ! command -v perl >/dev/null 2>&1; then
  echo "ERROR: perl not found" >&2
  exit 1
fi

# カバレッジガード: isa.rs（と isa/*.rs があれば）から #[target_feature] 直後の
# fn 名を抽出し、上記カーネル表と一致することを確認する。新規カーネル追加時の
# 登録漏れを検知するための fail-closed チェック。
ISA_SOURCES=("${REPO_ROOT}/crates/engine/src/isa.rs")
if [ -d "${REPO_ROOT}/crates/engine/src/isa" ]; then
  while IFS= read -r f; do
    ISA_SOURCES+=("${f}")
  done < <(find "${REPO_ROOT}/crates/engine/src/isa" -type f -name '*.rs')
fi

# `#[target_feature(...)]` と `fn` の間に他の属性（`#[inline(never)]` 等）や
# `///`/`//` コメント行が挟まっていても抽出できるよう、その間を
# `(?:属性行 | 行コメント)*` の反復として許容する（codex-review P1 指摘:
# 直後の行に固定した旧正規表現では、間に別属性・コメントを挟む新規カーネルが
# 登録漏れ検査を素通りしてしまっていた）。
ACTUAL_KERNELS="$(perl -0777 -ne '
  while (/#\[target_feature\([^)]*\)\]\s*(?:(?:#!?\[[^\]]*\]|\/\/[^\n]*)\s*)*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?fn\s+([A-Za-z0-9_]+)/g) {
    print "$1\n";
  }
' "${ISA_SOURCES[@]}" | sort -u)"

EXPECTED_KERNELS="$(printf '%s\n' "${X86_64_KERNELS[@]}" dot_neon | sort -u)"

MISSING_FROM_TABLE="$(comm -23 <(printf '%s\n' "${ACTUAL_KERNELS}") <(printf '%s\n' "${EXPECTED_KERNELS}"))"
if [ -n "${MISSING_FROM_TABLE}" ]; then
  echo "ERROR: #[target_feature] function(s) found in isa.rs but not registered in this" >&2
  echo "script's kernel table (coverage guard; fail-closed):" >&2
  printf '%s\n' "${MISSING_FROM_TABLE}" >&2
  exit 1
fi

# 通常ビルドの fingerprint を汚さないよう専用 target dir を使う。
SIMD_TARGET_DIR="${REPO_ROOT}/target/simd-codegen"
DEPS_DIR="${SIMD_TARGET_DIR}/release/deps"
rm -f "${DEPS_DIR}"/engine-*.s 2>/dev/null || true

# `--emit asm` は cargo のフィンガープリントに含まれない追加出力のため、直前の
# ビルドとフィンガープリントが一致すると cargo は rustc を再実行せず `.s` が
# 生成されない（fail-closed で上の `.s` 個数チェックに落ちるだけで済むが、
# 意図せぬ空振り ERROR を避けるため、`engine` パッケージ分だけ明示的に
# クリーンしてから毎回フルビルドする）。
(
  cd "${REPO_ROOT}"
  CARGO_TARGET_DIR="${SIMD_TARGET_DIR}" cargo clean --release -p engine
  CARGO_TARGET_DIR="${SIMD_TARGET_DIR}" cargo rustc --release -p engine --lib -- --emit asm
)

ASM_FILES=("${DEPS_DIR}"/engine-*.s)
if [ ! -e "${ASM_FILES[0]:-}" ]; then
  echo "ERROR: no engine-*.s produced under ${DEPS_DIR} (fail-closed)" >&2
  exit 1
fi
if [ "${#ASM_FILES[@]}" -ne 1 ]; then
  echo "ERROR: expected exactly one engine-*.s under ${DEPS_DIR}, found ${#ASM_FILES[@]}:" >&2
  printf '%s\n' "${ASM_FILES[@]}" >&2
  exit 1
fi

check_asm_file "${ASM_FILES[0]}" 6engine3isa "${X86_64_KERNELS[@]}"
