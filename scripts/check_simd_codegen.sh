#!/usr/bin/env bash
# Issue #467（ポインタ: TASK-156・CORE-14）の SIMD カーネル生成コード検査ガード。
#
# 動機: `crates/engine/src/isa.rs` は現状 intrinsics を使わず、
# `#[target_feature]` 付き safe fn 内の `dot_lanes::<LANES>`（`as_chunks` ＋
# `f32::mul_add`）を LLVM の自動ベクトル化に委ねている。後続 Phase（f16 常駐等）で
# 導入予定の intrinsics は「`as_chunks` で得た固定長配列から `_mm256_set_ps` 等を
# 構築し、新規 `unsafe` を持たない」方式を採る予定だが、この `set` 構築が単一の
# ロード命令（`vmovups`／`vcvtph2ps` 等）へ畳み込まれるのは LLVM の最適化挙動で
# あって言語仕様の保証ではない（判断根拠: `docs/design/chip-kernel-guidelines.md`
# §0.3・`docs/design/simd-codegen-guard.md`）。
#
# 本スクリプトは `cargo rustc --emit asm` で実際に生成されたアセンブリを走査し、
# `isa` モジュール配下の関数に「要素ごと挿入命令」（x86_64: `vinsertps`／
# `vpinsrb/w/d/q`／`vunpcklps`／`vunpckhps`。aarch64: レーン指定 `ld1 {...}[n]`・
# `ins v`・`mov v_.{b,h,s,d}[n]`）が残っていないかを機械検査する。rustc/LLVM の
# 更新やコード変更でこれらの命令が再混入した場合に CI 段階で検出することが目的
# （`scripts/check_sort_determinism.sh` と同型の、`cargo test` 経由ではない
# 軽量シェルスクリプト方式）。
#
# 検査対象を「`isa` モジュール配下の全関数」にしている理由（関数名固定にしない
# 理由）: aarch64 では `dot_neon` は baseline feature のため `SimdKernel::dot`
# 本体へインライン化され、独立シンボルとして生成されない。関数名で対象を絞ると
# aarch64 側の検査が何も対象を見つけられず vacuous pass してしまうため、
# モジュール単位（マングル名のセグメント `3isa`）で抽出したうえで、
# target 別の必須シンボル（x86_64: `dot_avx2_fma`＋`dot_avx512`／aarch64:
# `SimdKernel::dot`）が実際に対象集合へ含まれることを別途検査し、
# 抽出そのものが空振りしていないことを保証する（fail-closed）。
#
# `vinsertf128`／`vinserti128`（128→256 bit の結合）は禁止命令に含めない
# （128 bit レジスタ 2 個を 256 bit へ結合する正当な操作であり、要素ごとの
# ギャザー構築とは別物。実測で `_mm_insert_ps` 明示 intrinsic や変換後の
# `set_ps` はこの命令へ最適化されることを確認済み）。
#
# 検査パターンを弱める・無効化する環境変数上書き経路は設けない
# （`isa.rs` の CORE-12 方針・`check_sort_determinism.sh` の許可マーカー方式とも
# 異なり、本スクリプトはコード内マーカーによる例外も持たない。パターン変更は
# 本スクリプト自体の修正＝コードレビュー経由に限定する）。
#
# `--self-test` は検査ロジック自体の回帰テストモード。実ソースを使わず、
# その場で生成した fixture を `rustc -O --crate-type lib --emit asm` で
# コンパイルし、「pass すべき形（連続要素からの `set` 構築）を pass」
# 「fail すべき形（ストライドありのギャザー構築）を fail」「必須シンボル不在を
# fail」を検証する（`make simd-codegen-check`／`make simd-codegen-check-cross`・
# CI の `simd-codegen-check` ジョブ・`cross-check` ジョブから呼ばれる）。
#
# 使い方: scripts/check_simd_codegen.sh [--target <triple>] [--self-test]
#
# 環境変数: SIMD_CODEGEN_TARGET_DIR（既定 target/simd-codegen）。
# `cargo rustc --emit asm` の出力先を分離するためだけに使う（同一 deps
# ディレクトリにビルドごとに別ハッシュの `.s` が残留すると判明したため、
# 検査前に該当パターンの `.s` を削除したうえで「ちょうど 1 個」であることを
# 要求する）。

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TARGET=""
MODE="check"

while [ $# -gt 0 ]; do
  case "$1" in
    --self-test)
      MODE="self-test"
      shift
      ;;
    --target)
      if [ $# -lt 2 ]; then
        echo "ERROR: --target requires a value" >&2
        exit 1
      fi
      TARGET="$2"
      shift 2
      ;;
    *)
      echo "ERROR: unknown argument: $1" >&2
      echo "usage: $(basename "$0") [--target <triple>] [--self-test]" >&2
      exit 1
      ;;
  esac
done

if [ -z "${TARGET}" ]; then
  TARGET="$(rustc -vV | sed -n 's/^host: //p')"
  if [ -z "${TARGET}" ]; then
    echo "ERROR: failed to determine host target via 'rustc -vV'" >&2
    exit 1
  fi
fi

case "${TARGET}" in
  x86_64-*)
    ARCH_CLASS="x86_64"
    ;;
  aarch64-*)
    ARCH_CLASS="aarch64"
    ;;
  *)
    echo "ERROR: unsupported target: ${TARGET} (only x86_64-*/aarch64-* are supported)" >&2
    exit 1
    ;;
esac

TARGET_DIR="${SIMD_CODEGEN_TARGET_DIR:-${REPO_ROOT}/target/simd-codegen}"

# 対象モジュールのマングル名セグメント（例: `_ZN6engine3isa...`）に一致する
# 関数だけをラベル〜`.cfi_endproc` 単位で抽出する。第 2 引数の module_segment は
# 正規表現（perl 拡張）として扱う。
#
# ラベル行の判定は先頭の追加アンダースコア（`_`）を許容する: Mach-O（macOS。
# aarch64-apple-darwin／x86_64-apple-darwin）では ELF/COFF と異なり全シンボルへ
# リンカが追加のアンダースコアを 1 つ付与するため、legacy マングリング
# `_ZN...` は `__ZN...`、v0 マングリング `_R...` は `__R...` としてアセンブリへ
# 出力される（実体は同じシンボルであり、後続の module_segment 部分一致・
# required_segments_for の照合はどちらの形式でも変わらず動作する）。
#
# 抽出結果は「関数名<TAB>命令行...（改行区切り）」のブロックを `\x01` 区切りで
# 標準出力へ書き出す（呼び出し元がブロック単位で走査する）。
extract_functions() {
  local asm_file="$1"
  local module_segment="$2"
  # 状態機械: `waiting` → ラベル直後の行を見て関数（`.cfi_startproc` が続く）か
  # データ（`CURRENT`（`OnceLock` 静的変数）等の `.asciz`／`.zero`／`.quad` 等が
  # 続く）かを判別する。データの場合は「次に `_ZN`/`_R` ラベルが現れるまで
  # 際限なく行を溜め込む」誤りを避けるため、この時点で追跡を打ち切る
  # （`.L` から始まるローカルラベル・アンカーはブロック境界と見なさないため、
  # 判別を誤ると同一クレート内の無関係な後続シンボルまで巻き込む）。
  perl -ne '
    BEGIN { $state = "waiting"; $name = ""; @lines = (); }
    if ($state eq "after_label") {
      if (/^\s*\.cfi_startproc\s*$/) {
        $state = "in_function";
        next;
      } else {
        $state = "waiting";
        $name = "";
        @lines = ();
        # このデータ定義行自体がラベル行である可能性は構造上ないため
        # そのまま読み捨て、次行から通常探索を継続する。
      }
    }
    if (/^(_?(?:_ZN|_R)[A-Za-z0-9_.\$]*):$/) {
      $name = $1;
      @lines = ();
      $state = "after_label";
      next;
    }
    if ($state eq "in_function") {
      if (/^\s*\.cfi_endproc\s*$/) {
        if ($name =~ /\Q'"${module_segment}"'\E/) {
          print "$name\x02" . join("\x03", @lines) . "\x01";
        }
        $state = "waiting";
        $name = "";
        @lines = ();
        next;
      }
      push @lines, $_;
    }
  ' "${asm_file}"
}

# 命令行から先頭の空白・末尾改行を除いたニーモニック（先頭トークン）を返す。
# `.` で始まるディレクティブ行・ラベル行（末尾 `:`）は対象外（アセンブリ行は
# 必ず先頭にタブ／空白を伴うため、トリムしてから判定する）。
mnemonic_of() {
  local line="$1"
  local trimmed
  trimmed="$(echo "${line}" | sed -E 's/^[[:space:]]+//; s/[[:space:]]*$//')"
  case "${trimmed}" in
    ""|.*|*:)
      return 1
      ;;
  esac
  echo "${trimmed}" | sed -E 's/[[:space:]].*$//'
}

# 1 関数分の命令行（\x03 区切り）に禁止命令が含まれるかを判定する。
# 該当があれば「行番号は付けず命令名のみ」を 1 行 1 件、標準出力へ列挙し、
# 呼び出し元が非ゼロで終了する（該当なしは何も出力しない）。
scan_forbidden() {
  local arch_class="$1"
  local body="$2"
  local IFS=$'\x03'
  local found=1
  for line in ${body}; do
    local mnem
    mnem="$(mnemonic_of "${line}")" || continue
    if [ "${arch_class}" = "x86_64" ]; then
      case "${mnem}" in
        insertps|vinsertps)
          echo "${mnem}"
          found=0
          ;;
        pinsrb|vpinsrb|pinsrw|vpinsrw|pinsrd|vpinsrd|pinsrq|vpinsrq)
          echo "${mnem}"
          found=0
          ;;
        unpcklps|vunpcklps|unpckhps|vunpckhps)
          echo "${mnem}"
          found=0
          ;;
      esac
    else
      case "${mnem}" in
        ins)
          # `ins v...` のみ対象（他の `ins` 系ニーモニックは aarch64 に存在しない）。
          if echo "${line}" | grep -Eq '^\s*ins\s+v'; then
            echo "ins"
            found=0
          fi
          ;;
        mov)
          if echo "${line}" | grep -Eq '^\s*mov\s+v[0-9]+\.[bhsd]\['; then
            echo "mov(lane)"
            found=0
          fi
          ;;
        ld1)
          if echo "${line}" | grep -Eq '^\s*ld1\s*\{[^}]*\}\[[0-9]+\]'; then
            echo "ld1(lane)"
            found=0
          fi
          ;;
      esac
    fi
  done
  return "${found}"
}

# 1 関数分の命令行を集計し「ニーモニック: 件数」を頻度降順で 1 行にまとめる
# （基線サマリ表示用。何を検査しているかを人間が確認できるようにする）。
summarize_instructions() {
  local body="$1"
  local IFS=$'\x03'
  local -A counts=()
  local order=()
  for line in ${body}; do
    local mnem
    mnem="$(mnemonic_of "${line}")" || continue
    if [ -z "${counts[${mnem}]+x}" ]; then
      counts[${mnem}]=0
      order+=("${mnem}")
    fi
    counts[${mnem}]=$((counts[${mnem}] + 1))
  done
  local out=""
  for mnem in "${order[@]}"; do
    out+="${mnem}=${counts[${mnem}]} "
  done
  echo "${out}"
}

# 必須シンボル（target 別）の判定用セグメント。
#
# aarch64 側は Issue #528 で `10SimdKernel3dot`（`SimdKernel::dot`）から
# `10SimdKernel20dot_with_scalar_tail`／`10SimdKernel20dot_with_padded_tail`
# （`SimdKernel::dot_with_scalar_tail`／`dot_with_padded_tail`）へ置き換えた。
# `dot_impl<const PADDED_TAIL: bool>` を共有本体化したことで、`dot` は
# LLVM の関数マージにより独立シンボルとして現れず `dot_with_scalar_tail`
# （`PADDED_TAIL=false`）のエイリアスになるため（計測で確認済み。`.s` 上は
# ラベルではなく `.set` として出力され、本スクリプトのラベル抽出には現れない）、
# 実体を持つ 2 wrapper を必須シンボルとする。x86_64 側は `dot_avx2_fma`／
# `dot_avx512` のマングル名は `PADDED_TAIL` の値に関わらず同一関数名（トップレベル
# の型パラメータを持たない `fn` の識別子部分）を共有し、両 monomorphization
# （`PADDED_TAIL=false`／`true`）が同一セグメントを含むラベルとして現れるため
# 変更不要。
required_segments_for() {
  local arch_class="$1"
  if [ "${arch_class}" = "x86_64" ]; then
    echo "12dot_avx2_fma"
    echo "10dot_avx512"
    # Issue #510（TASK-156・CORE-14）: 4 行ブロックカーネル
    # `isa::x86_block4::dot_block4_avx2_fma`／`dot_block4_avx512`
    # （いずれも `#[target_feature]` fn のため、`dot_avx2_fma`／`dot_avx512` と
    # 同様に独立シンボルとして生成される）を必須シンボルへ追加する。
    echo "19dot_block4_avx2_fma"
    echo "17dot_block4_avx512"
    # Issue #514: f16 昇格 dot カーネル（`isa::dot_f16_f16c`）。
    echo "12dot_f16_f16c"
  else
    echo "10SimdKernel20dot_with_scalar_tail"
    echo "10SimdKernel20dot_with_padded_tail"
    # Issue #514: f16 昇格 dot カーネル（`isa::dot_f16_neon_fp16`）。
    # `#[inline(never)]` を付与しているため独立シンボルとして必ず現れる。
    echo "17dot_f16_neon_fp16"
  fi
}

# 非 vacuous 検査（決定 2・Issue #514・A1）: 「期待命令が対象関数に 1 件以上
# 出現する」ことを要求する rule 表。禁止命令検査（`scan_forbidden`）が
# 「あってはならない命令」を検査するのに対し、本関数は「無ければならない
# 命令」を検査する——f16 カーネルは `set` 構築が実際に intrinsics へ
# コンパイルされたことの証跡（`vcvtph2ps`／`fcvtl`）を要求しないと、
# ソフトウェア復号へ静かに縮退していても検査を通過してしまう
# （`--self-test` の fail fixture 参照）。
#
# 出力は「関数名セグメント<TAB>期待正規表現（拡張正規表現。命令行全体に
# 対して `grep -E` で照合）<TAB>説明」を 1 行 1 rule で返す。対象関数
# セグメントに一致する関数が 1 つも無ければ本検査は素通り
# （`required_segments_for` の必須シンボル検査が既にその欠落を検出する）。
expected_rules_for() {
  local arch_class="$1"
  if [ "${arch_class}" = "x86_64" ]; then
    printf '%s\t%s\t%s\n' \
      "dot_f16_f16c" \
      '^[[:space:]]*vcvtph2ps[[:space:]]+[^,]*\(' \
      "f16->f32 promotion (vcvtph2ps) with a memory operand (register-only form does not count)"
  else
    printf '%s\t%s\t%s\n' \
      "dot_f16_neon_fp16" \
      '^[[:space:]]*fcvtl2?[[:space:]]+v[0-9]+\.4s' \
      "f16->f32 promotion (fcvtl/fcvtl2 widening to 4s)"
  fi
}

# `fn_name`（1 関数）が `expected_rules_for` の対象か判定し、対象なら
# `fn_body`（命令行。\x03 区切り）に期待パターンが 1 件以上出現するかを検査
# する。対象外の関数は常に成功（0）。一致なしは非ゼロ・理由を stdout へ
# 1 行（`scan_forbidden` と同様、行番号は付けない）。
scan_expected_missing() {
  local arch_class="$1"
  local fn_name="$2"
  local fn_body="$3"

  local rules
  rules="$(expected_rules_for "${arch_class}")"
  local rule
  while IFS=$'\t' read -r seg pattern desc; do
    [ -z "${seg}" ] && continue
    case "${fn_name}" in
      *"${seg}"*) ;;
      *) continue ;;
    esac
    local IFS_OLD="${IFS}"
    IFS=$'\x03'
    local found=1
    local line
    for line in ${fn_body}; do
      if echo "${line}" | grep -Eq "${pattern}"; then
        found=0
        break
      fi
    done
    IFS="${IFS_OLD}"
    if [ "${found}" -ne 0 ]; then
      echo "${desc}"
    fi
  done <<< "${rules}"
}

# 生成済み `.s` を対象に、module_segment 配下の関数群を検査する共通本体。
# 成功時は 0・基線サマリを stdout へ出力、失敗時は非ゼロを返し理由を stderr へ
# 出力する。self-test（fixture）・実ビルドの両方から呼ばれる。
run_scan() {
  local asm_file="$1"
  local arch_class="$2"
  local module_segment="$3"
  shift 3
  local required_segments=("$@")

  if [ ! -f "${asm_file}" ]; then
    echo "ERROR: asm file not found: ${asm_file}" >&2
    return 1
  fi

  local blocks
  blocks="$(extract_functions "${asm_file}" "${module_segment}")"
  if [ -z "${blocks}" ]; then
    echo "ERROR: no functions matched module segment '${module_segment}' in ${asm_file}" >&2
    return 1
  fi

  # 必須シンボルの充足確認（vacuous pass 防止）。
  # 照合対象は各ブロックの fn_name のみに限定する（命令本文まで含めて照合すると、
  # 本来抽出すべき関数の本体抽出に失敗していても、別の関数本体に含まれる
  # 呼び出し命令のシンボル名文字列だけで必須条件を満たしてしまい、未検査の
  # カーネルを通過させ得るため。codex-review #566 P1 指摘）。
  # また `grep -q` を `set -o pipefail` 下でパイプの受け手に使うと、一致を
  # 見つけた時点で入力を読み切らずに終了し、書き手側の `echo`／ここでは
  # ヒアストリング展開が SIGPIPE で失敗しうる（`blocks` がパイプ容量を超える
  # 場合）。シンボルが実在するにもかかわらず判定全体が失敗する誤検出を避けるため、
  # パイプを介さずシェル内の文字列一致（`fn_names` へ改行区切りで蓄積し `case`
  # で判定）だけで照合する（codex-review #566 P2 指摘）。
  local fn_names=$'\n'
  local IFS_NAMES_OLD="${IFS}"
  IFS=$'\x01'
  local name_block
  for name_block in ${blocks}; do
    [ -z "${name_block}" ] && continue
    fn_names+="${name_block%%$'\x02'*}"$'\n'
  done
  IFS="${IFS_NAMES_OLD}"

  local seg
  for seg in "${required_segments[@]}"; do
    case "${fn_names}" in
      *"${seg}"*) ;;
      *)
        echo "ERROR: required symbol segment not found among scanned functions: ${seg}" >&2
        echo "  (target=${arch_class} module_segment=${module_segment})" >&2
        return 1
        ;;
    esac
  done

  local status=0
  local IFS_OLD="${IFS}"
  IFS=$'\x01'
  local block
  for block in ${blocks}; do
    [ -z "${block}" ] && continue
    local fn_name="${block%%$'\x02'*}"
    local fn_body="${block#*$'\x02'}"

    local forbidden
    forbidden="$(scan_forbidden "${arch_class}" "${fn_body}")" || true
    if [ -n "${forbidden}" ]; then
      echo "ERROR: forbidden per-element insert instruction(s) found in ${fn_name}:" >&2
      echo "${forbidden}" | while IFS= read -r item; do echo "  - ${item}"; done >&2
      status=1
      continue
    fi

    local missing
    missing="$(scan_expected_missing "${arch_class}" "${fn_name}" "${fn_body}")" || true
    if [ -n "${missing}" ]; then
      echo "${missing}" | while IFS= read -r item; do
        [ -z "${item}" ] && continue
        echo "ERROR: expected instruction missing in ${fn_name}: ${item}" >&2
      done
      status=1
      continue
    fi

    local summary
    summary="$(summarize_instructions "${fn_body}")"
    echo "ok: ${fn_name}: ${summary}"
  done
  IFS="${IFS_OLD}"

  return "${status}"
}

# --- self-test 用 fixture 生成 -------------------------------------------

# 指定 crate ソースを scratch ディレクトリへ書き出し `rustc -O --crate-type lib
# --emit asm [--target <triple>]` でコンパイルする。生成された `.s` のパスを
# stdout へ返す。
compile_fixture() {
  local scratch="$1"
  local src_file="$2"
  local target="$3"

  mkdir -p "${scratch}"
  local target_flag=()
  if [ -n "${target}" ]; then
    target_flag=(--target "${target}")
  fi
  if ! rustc -O --crate-type lib --emit asm "${target_flag[@]}" \
      --out-dir "${scratch}" "${src_file}" 2>"${scratch}/rustc.log"; then
    echo "ERROR: fixture compilation failed (see ${scratch}/rustc.log)" >&2
    cat "${scratch}/rustc.log" >&2
    return 1
  fi

  local crate_name
  crate_name="$(basename "${src_file}" .rs)"
  local asm
  asm="$(find "${scratch}" -maxdepth 1 -name "${crate_name}*.s" | head -n 1)"
  if [ -z "${asm}" ]; then
    echo "ERROR: no .s output found for fixture ${src_file}" >&2
    return 1
  fi
  echo "${asm}"
}

write_x86_64_fixtures() {
  local dir="$1"

  # pass 1: 連続要素からの set 構築（禁止命令 0 を期待）。
  cat > "${dir}/fx_pass_set.rs" <<'RUST'
#![allow(internal_features)]
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<8>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<8>();
        let mut acc = _mm256_setzero_ps();
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va = _mm256_set_ps(
                ac[7], ac[6], ac[5], ac[4], ac[3], ac[2], ac[1], ac[0],
            );
            let vb = _mm256_set_ps(
                bc[7], bc[6], bc[5], bc[4], bc[3], bc[2], bc[1], bc[0],
            );
            acc = _mm256_fmadd_ps(va, vb, acc);
        }
        let lo = _mm256_castps256_ps128(acc);
        let hi = _mm256_extractf128_ps(acc, 1);
        let sum128 = _mm_add_ps(lo, hi);
        let sum64 = _mm_hadd_ps(sum128, sum128);
        let sum32 = _mm_hadd_ps(sum64, sum64);
        let mut out = [0f32; 4];
        _mm_storeu_ps(out.as_mut_ptr(), sum32);
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        out[0] + rem
    }
}
RUST

  # pass 2: 現行 dot_lanes と同形の自動ベクトル化（禁止命令 0 を期待）。
  cat > "${dir}/fx_pass_autovec.rs" <<'RUST'
pub mod isa_probe {
    #[target_feature(enable = "avx2,fma")]
    pub fn dot_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let a = &a[..len];
        let b = &b[..len];
        let mut lanes = [0f32; 8];
        let (a_chunks, a_rem) = a.as_chunks::<8>();
        let (b_chunks, b_rem) = b.as_chunks::<8>();
        for (a_chunk, b_chunk) in a_chunks.iter().zip(b_chunks.iter()) {
            for (lane, (x, y)) in lanes.iter_mut().zip(a_chunk.iter().zip(b_chunk.iter())) {
                *lane = x.mul_add(*y, *lane);
            }
        }
        let lane_sum: f32 = lanes.iter().sum();
        let rem_sum: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        lane_sum + rem_sum
    }
}
RUST

  # fail 1: ストライド 2 のギャザー構築（vinsertps を残す想定）。
  cat > "${dir}/fx_fail_stride_set.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<16>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<16>();
        let mut acc = _mm256_setzero_ps();
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va = _mm256_set_ps(
                ac[14], ac[12], ac[10], ac[8], ac[6], ac[4], ac[2], ac[0],
            );
            let vb = _mm256_set_ps(
                bc[14], bc[12], bc[10], bc[8], bc[6], bc[4], bc[2], bc[0],
            );
            acc = _mm256_fmadd_ps(va, vb, acc);
        }
        let mut out = [0f32; 8];
        _mm256_storeu_ps(out.as_mut_ptr(), acc);
        let sum: f32 = out.iter().sum();
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        sum + rem
    }
}
RUST

  # fail 2: f16 ストライド set_epi16（vpinsrw を残す想定）。
  cat > "${dir}/fx_fail_stride_f16.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma,f16c")]
    pub unsafe fn dot_avx2_fma(a: &[u16], b: &[u16]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<16>();
        let (b_chunks, _b_rem) = b[..len].as_chunks::<16>();
        let mut acc = 0f32;
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va16 = _mm_set_epi16(
                ac[14] as i16, ac[12] as i16, ac[10] as i16, ac[8] as i16,
                ac[6] as i16, ac[4] as i16, ac[2] as i16, ac[0] as i16,
            );
            let vb16 = _mm_set_epi16(
                bc[14] as i16, bc[12] as i16, bc[10] as i16, bc[8] as i16,
                bc[6] as i16, bc[4] as i16, bc[2] as i16, bc[0] as i16,
            );
            let va = _mm256_cvtph_ps(va16);
            let vb = _mm256_cvtph_ps(vb16);
            let prod = _mm256_mul_ps(va, vb);
            let mut out = [0f32; 8];
            _mm256_storeu_ps(out.as_mut_ptr(), prod);
            acc += out.iter().sum::<f32>();
        }
        let rem: f32 = a_rem.iter().sum::<u16>() as f32;
        acc + rem
    }
}
RUST

  # fail 3: 必須シンボル不在（対象モジュールにダミー関数しか無い）。
  cat > "${dir}/fx_fail_missing_symbol.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn unrelated(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }
}
RUST

  # pass (Issue #514・A1): `isa.rs::dot_f16_f16c` と同型の実装。メモリ
  # オペランド付き `vcvtph2ps` を期待する非 vacuous 検査（`expected_rules_for`）
  # が実際に pass することを確認する。
  cat > "${dir}/fx_pass_f16.rs" <<'RUST'
use std::arch::x86_64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "avx2,fma,f16c")]
    pub fn dot_f16_f16c(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, _a_rem) = a[..len].as_chunks::<8>();
        let (b_chunks, _b_rem) = b[..len].as_chunks::<8>();
        let mut acc = _mm256_setzero_ps();
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let va16 = _mm_set_epi16(
                ac[7] as i16, ac[6] as i16, ac[5] as i16, ac[4] as i16,
                ac[3] as i16, ac[2] as i16, ac[1] as i16, ac[0] as i16,
            );
            let va = _mm256_cvtph_ps(va16);
            let vb = _mm256_set_ps(bc[7], bc[6], bc[5], bc[4], bc[3], bc[2], bc[1], bc[0]);
            acc = _mm256_fmadd_ps(va, vb, acc);
        }
        let lo = _mm256_castps256_ps128(acc);
        let hi = _mm256_extractf128_ps(acc, 1);
        let sum128 = _mm_add_ps(lo, hi);
        let shuf = _mm_shuffle_ps(sum128, sum128, 0b01_00_11_10);
        let sums = _mm_add_ps(sum128, shuf);
        let shuf2 = _mm_shuffle_ps(sums, sums, 0b00_00_00_01);
        let final_sum = _mm_add_ss(sums, shuf2);
        _mm_cvtss_f32(final_sum)
    }
}
RUST

  # fail (Issue #514・A1): 関数名は `dot_f16_f16c` だが実体はソフトウェア復号
  # のみ（`vcvtph2ps` を一切使わない）。非 vacuous 検査が「命令が存在しない
  # 縮退」を実際に検出できることを確認する（禁止命令検査は素通りしてしまう
  # ケースへの対照）。
  cat > "${dir}/fx_fail_f16_missing_instruction.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn dot_f16_f16c(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let mut acc = 0f32;
        for (&bits, &y) in a[..len].iter().zip(b[..len].iter()) {
            let sign = ((bits & 0x8000) as u32) << 16;
            let exp = ((bits >> 10) & 0x1F) as u32;
            let mantissa = (bits & 0x03FF) as u32;
            let bits32 = if exp == 0 {
                sign
            } else {
                sign | (((exp as i32 - 15 + 127) as u32) << 23) | (mantissa << 13)
            };
            acc += f32::from_bits(bits32) * y;
        }
        acc
    }
}
RUST
}

write_aarch64_fixtures() {
  local dir="$1"

  # pass: 連続要素での vsetq_lane_f32 構築（ldr q へ畳み込まれる想定）。
  cat > "${dir}/fx_pass_lane_seq.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "neon")]
    pub unsafe fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<4>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<4>();
        let mut acc = vdupq_n_f32(0.0);
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let mut va = vdupq_n_f32(0.0);
            va = vsetq_lane_f32(ac[0], va, 0);
            va = vsetq_lane_f32(ac[1], va, 1);
            va = vsetq_lane_f32(ac[2], va, 2);
            va = vsetq_lane_f32(ac[3], va, 3);
            let mut vb = vdupq_n_f32(0.0);
            vb = vsetq_lane_f32(bc[0], vb, 0);
            vb = vsetq_lane_f32(bc[1], vb, 1);
            vb = vsetq_lane_f32(bc[2], vb, 2);
            vb = vsetq_lane_f32(bc[3], vb, 3);
            acc = vfmaq_f32(acc, va, vb);
        }
        let sum = vaddvq_f32(acc);
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        sum + rem
    }
}
RUST

  # fail: ストライド 2 のレーン構築（レーン指定 ld1 が残る想定）。
  cat > "${dir}/fx_fail_lane_stride.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "neon")]
    pub unsafe fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, a_rem) = a[..len].as_chunks::<8>();
        let (b_chunks, b_rem) = b[..len].as_chunks::<8>();
        let mut acc = vdupq_n_f32(0.0);
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let mut va = vdupq_n_f32(0.0);
            va = vsetq_lane_f32(ac[0], va, 0);
            va = vsetq_lane_f32(ac[2], va, 1);
            va = vsetq_lane_f32(ac[4], va, 2);
            va = vsetq_lane_f32(ac[6], va, 3);
            let mut vb = vdupq_n_f32(0.0);
            vb = vsetq_lane_f32(bc[0], vb, 0);
            vb = vsetq_lane_f32(bc[2], vb, 1);
            vb = vsetq_lane_f32(bc[4], vb, 2);
            vb = vsetq_lane_f32(bc[6], vb, 3);
            acc = vfmaq_f32(acc, va, vb);
        }
        let sum = vaddvq_f32(acc);
        let rem: f32 = a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum();
        sum + rem
    }
}
RUST

  # pass (Issue #514・A1): `isa.rs::dot_f16_neon_fp16` と同型の実装。
  # `fcvtl` へ畳み込まれる f16 昇格を要求する非 vacuous 検査が実際に pass
  # することを確認する。
  cat > "${dir}/fx_pass_f16.rs" <<'RUST'
use std::arch::aarch64::*;

pub mod isa_probe {
    use super::*;
    #[target_feature(enable = "neon,fp16")]
    pub fn dot_f16_neon_fp16(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let (a_chunks, _a_rem) = a[..len].as_chunks::<4>();
        let (b_chunks, _b_rem) = b[..len].as_chunks::<4>();
        let mut acc = vdupq_n_f32(0.0);
        for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
            let mut vh_u16 = vdup_n_u16(0);
            vh_u16 = vset_lane_u16(ac[0], vh_u16, 0);
            vh_u16 = vset_lane_u16(ac[1], vh_u16, 1);
            vh_u16 = vset_lane_u16(ac[2], vh_u16, 2);
            vh_u16 = vset_lane_u16(ac[3], vh_u16, 3);
            let va = vcvt_f32_f16(vreinterpret_f16_u16(vh_u16));

            let mut vb = vdupq_n_f32(0.0);
            vb = vsetq_lane_f32(bc[0], vb, 0);
            vb = vsetq_lane_f32(bc[1], vb, 1);
            vb = vsetq_lane_f32(bc[2], vb, 2);
            vb = vsetq_lane_f32(bc[3], vb, 3);
            acc = vfmaq_f32(acc, va, vb);
        }
        vaddvq_f32(acc)
    }
}
RUST

  # fail (Issue #514・A1): 関数名は `dot_f16_neon_fp16` だが実体はソフトウェア
  # 復号のみ（`fcvtl` を一切使わない）。
  cat > "${dir}/fx_fail_f16_missing_instruction.rs" <<'RUST'
pub mod isa_probe {
    #[inline(never)]
    pub fn dot_f16_neon_fp16(a: &[u16], b: &[f32]) -> f32 {
        let len = a.len().min(b.len());
        let mut acc = 0f32;
        for (&bits, &y) in a[..len].iter().zip(b[..len].iter()) {
            let sign = ((bits & 0x8000) as u32) << 16;
            let exp = ((bits >> 10) & 0x1F) as u32;
            let mantissa = (bits & 0x03FF) as u32;
            let bits32 = if exp == 0 {
                sign
            } else {
                sign | (((exp as i32 - 15 + 127) as u32) << 23) | (mantissa << 13)
            };
            acc += f32::from_bits(bits32) * y;
        }
        acc
    }
}
RUST
}

self_test() {
  local scratch
  scratch="$(mktemp -d)"
  trap 'rm -rf "${scratch}"' RETURN

  local overall=0

  if [ "${ARCH_CLASS}" = "x86_64" ]; then
    write_x86_64_fixtures "${scratch}"

    local asm
    asm="$(compile_fixture "${scratch}/pass_set" "${scratch}/fx_pass_set.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null; then
        echo "self-test ok: pass_set (element-wise set from contiguous chunk)"
      else
        echo "self-test FAILED: expected pass_set to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/pass_autovec" "${scratch}/fx_pass_autovec.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null; then
        echo "self-test ok: pass_autovec (dot_lanes-equivalent auto-vectorization)"
      else
        echo "self-test FAILED: expected pass_autovec to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_stride_set" "${scratch}/fx_fail_stride_set.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_stride_set to be rejected (vinsertps)" >&2
        overall=1
      else
        echo "self-test ok: fail_stride_set correctly rejected"
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_stride_f16" "${scratch}/fx_fail_stride_f16.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_stride_f16 to be rejected (vpinsrw)" >&2
        overall=1
      else
        echo "self-test ok: fail_stride_f16 correctly rejected"
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_missing_symbol" "${scratch}/fx_fail_missing_symbol.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_avx2_fma" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_missing_symbol to be rejected (required symbol missing)" >&2
        overall=1
      else
        echo "self-test ok: fail_missing_symbol correctly rejected"
      fi
    fi

    # Issue #514・A1: 期待命令の非 vacuous 検査（`expected_rules_for`）。
    asm="$(compile_fixture "${scratch}/pass_f16" "${scratch}/fx_pass_f16.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_f16_f16c" >/dev/null; then
        echo "self-test ok: pass_f16 (vcvtph2ps memory-operand promotion present)"
      else
        echo "self-test FAILED: expected pass_f16 to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_f16_missing_instruction" "${scratch}/fx_fail_f16_missing_instruction.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" x86_64 "isa_probe" "12dot_f16_f16c" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_f16_missing_instruction to be rejected (no vcvtph2ps)" >&2
        overall=1
      else
        echo "self-test ok: fail_f16_missing_instruction correctly rejected"
      fi
    fi
  else
    write_aarch64_fixtures "${scratch}"

    local asm
    asm="$(compile_fixture "${scratch}/pass_lane_seq" "${scratch}/fx_pass_lane_seq.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "8dot_neon" >/dev/null; then
        echo "self-test ok: pass_lane_seq (sequential vsetq_lane_f32 folds to ldr q)"
      else
        echo "self-test FAILED: expected pass_lane_seq to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_lane_stride" "${scratch}/fx_fail_lane_stride.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "8dot_neon" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_lane_stride to be rejected (lane ld1/ins)" >&2
        overall=1
      else
        echo "self-test ok: fail_lane_stride correctly rejected"
      fi
    fi

    # Issue #514・A1: 期待命令の非 vacuous 検査（`expected_rules_for`）。
    asm="$(compile_fixture "${scratch}/pass_f16" "${scratch}/fx_pass_f16.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "17dot_f16_neon_fp16" >/dev/null; then
        echo "self-test ok: pass_f16 (fcvtl widening promotion present)"
      else
        echo "self-test FAILED: expected pass_f16 to pass" >&2
        overall=1
      fi
    fi

    asm="$(compile_fixture "${scratch}/fail_f16_missing_instruction" "${scratch}/fx_fail_f16_missing_instruction.rs" "${TARGET}")" || { overall=1; asm=""; }
    if [ -n "${asm}" ]; then
      if run_scan "${asm}" aarch64 "isa_probe" "17dot_f16_neon_fp16" >/dev/null 2>&1; then
        echo "self-test FAILED: expected fail_f16_missing_instruction to be rejected (no fcvtl)" >&2
        overall=1
      else
        echo "self-test ok: fail_f16_missing_instruction correctly rejected"
      fi
    fi
  fi

  if [ "${overall}" -ne 0 ]; then
    echo "self-test: FAILED" >&2
    return 1
  fi
  echo "self-test: ok"
  return 0
}

real_check() {
  local build_target_dir="${TARGET_DIR}"
  local deps_dir="${build_target_dir}/release/deps"
  local cargo_target_flag=()
  local host
  host="$(rustc -vV | sed -n 's/^host: //p')"
  if [ "${TARGET}" != "${host}" ]; then
    cargo_target_flag=(--target "${TARGET}")
    deps_dir="${build_target_dir}/${TARGET}/release/deps"
  fi

  mkdir -p "${build_target_dir}"
  rm -f "${deps_dir}"/engine-*.s

  # `.s` を削除しても、cargo のフィンガープリントが「最新」と判断すれば
  # rustc は再起動されず `.s` が再生成されない（F2）。`cargo clean -p engine`
  # で engine crate のビルド成果物（依存クレートは残す）だけを無効化し、
  # 毎回確実に `--emit asm` が再実行されるようにする。
  CARGO_TARGET_DIR="${build_target_dir}" cargo clean -p engine --release \
    "${cargo_target_flag[@]}" >/dev/null 2>&1 || true

  if ! CARGO_TARGET_DIR="${build_target_dir}" cargo rustc -p engine --release --lib \
      "${cargo_target_flag[@]}" -- --emit asm; then
    echo "ERROR: cargo rustc --emit asm failed" >&2
    return 1
  fi

  local matches
  matches="$(find "${deps_dir}" -maxdepth 1 -name 'engine-*.s' 2>/dev/null)"
  local count
  count="$(echo "${matches}" | grep -c . || true)"
  if [ -z "${matches}" ] || [ "${count}" -ne 1 ]; then
    echo "ERROR: expected exactly one engine-*.s under ${deps_dir}, found ${count}" >&2
    return 1
  fi

  local required=()
  while IFS= read -r seg; do
    required+=("${seg}")
  done < <(required_segments_for "${ARCH_CLASS}")

  run_scan "${matches}" "${ARCH_CLASS}" "3isa" "${required[@]}"
}

if [ "${MODE}" = "self-test" ]; then
  self_test
  exit $?
else
  real_check
  exit $?
fi
