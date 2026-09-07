//! aarch64 NEON dotprod（`vdotq_s32`。Armv8.2-A dot product 拡張。Apple M／
//! Graviton2 以降）向け整数 i8×i8 dot カーネル本体（Issue #525・親 #520・
//! 前提 #522。ポインタ: TASK-132・TASK-156・CORE-16）。
//!
//! 呼び出し文脈: `isa.rs::I8Kernel::dot_i8` の `NeonDotprod` 分岐 1 箇所からの
//! みこのモジュールの関数を `unsafe` 呼び出しする（ADR
//! `docs/design/simd-intrinsics-adoption.md` 決定 1: `unsafe` は `isa.rs` の
//! トークンディスパッチ箇所以外に持ち込まない。[`super::x86_i8`] と同じ方針で
//! 本モジュール自体は `unsafe` を一切含まない safe fn のみで構成する。
//! `tests/isa.rs::unsafe_is_confined_to_isa_module_with_safety_comments` が
//! 機械検証する）。
//!
//! `vdotq_s32` は s8×s8→i32 の 4 要素積和（`sdot v.4s, v.16b, v.16b`）であり、
//! x86_64 VNNI 系（u8×s8→i32・`vpdpbusd`）とは異なり符号付き×符号付きの積を
//! 直接計算できる。したがってクエリ側は [`super::I8QueryOperands::signed`]
//! をそのまま使い、VNNI 系が要する `acc − 128*row_sum`（`shifted` 経由の符号
//! 復元）は不要——本モジュールの唯一の公開関数 [`dot_i8_neon_dotprod`] は
//! `row_sum` を一切受け取らない（呼び出し元 `isa.rs::I8Kernel::dot_i8` の
//! `NeonDotprod` 分岐が `row_sum` を無視する。x86_64 の `Avx2Widen`
//! フォールバックと同じ立場）。
//!
//! 端数（`as_chunks::<16>()` で割り切れない末尾要素）はスカラー wrapping 和
//! で処理する（[`super::x86_i8`] と同じ意味論。整数演算のため加算順序の違い
//! はビット一致に影響しない）。水平和は `vaddvq_s32`（整数のため加算順序に
//! よる差は生じない——`neon_block4.rs` が f32 版で `vaddvq_f32` を避けた理由
//! は本カーネルには当てはまらない）。
//!
//! # レジスタ構築: `vsetq_lane_s8` 連鎖を選んだ理由
//!
//! `as_chunks::<16>()` で得た `&[i8; 16]` から `int8x16_t` を構築する際、
//! ポインタ load（`vld1q_s8`）・`transmute`・raw pointer は使わない（決定 1）。
//! 候補として (a) `vcombine_s8(vcreate_s8(lo), vcreate_s8(hi))`（2 個の `u64`
//! へパックしてから結合）と (b) `vsetq_lane_s8::<0..15>` の昇順連鎖を実測
//! 比較した——1.98.0 toolchain・`--target aarch64-unknown-linux-gnu -O
//! --emit asm` での実測で、(a) は `mov v_.d[1], v_.d[0]`（禁止パターン。
//! `scripts/check_simd_codegen.sh` の `mov v[0-9]+\.[bhsd]\[` 検査に抵触）を
//! 残す一方、(b) は単一の `ldr q`（16 要素の連続メモリロード）へ完全に畳み込
//! まれることを確認した（ADR 決定 2 の資料が想定した「`vcreate`+`vcombine`
//! が畳み込まれる」ケースは異なる呼び出しコンテキストでの実測であり、本
//! カーネルの呼び出し形〔`#[inline(never)]` 付き外側 fn から呼ばれるループ
//! 内〕では成立しなかった。詳細・asm 抜粋は
//! `docs/design/hnsw-sq8-resident.md`「Issue #525」節参照）。本モジュールは
//! 実測で確認できた (b) を採用する。
//!
//! `#[inline(never)]` を [`dot_i8_neon_dotprod`] に付ける理由:
//! `dotprod` は `aarch64-unknown-linux-gnu` の baseline 対象外の feature
//! （NEON と異なり常に有効ではない）だが、インライン化されると
//! `scripts/check_simd_codegen.sh` が関数名で対象を絞る非 vacuous 検査
//! （`sdot v.4s` の実際の出現確認）が呼び出し元へ紛れ込んだ命令列を誤検出・
//! 見落とす恐れがあるため、`dot_f16_neon_fp16`／`dot_block4_neon` と同じ方針
//! で独立シンボルとして残す。

use std::arch::aarch64::*;

/// `chunk` の 16 要素を `int8x16_t` へロードする（レーン 0..15 が
/// `chunk[0..16]` と対応）。
///
/// ポインタ load を使わず、配列をパターン分解して `vsetq_lane_s8::<0..15>`
/// を昇順に適用する（モジュール doc「レジスタ構築」参照。`neon_block4.rs::
/// load4` の f32 版・4 レーンと同じ方針を 16 レーンへ拡張したもの）。
#[target_feature(enable = "neon")]
fn load16(chunk: &[i8; 16]) -> int8x16_t {
    let [c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15] = *chunk;
    let v = vdupq_n_s8(0);
    let v = vsetq_lane_s8::<0>(c0, v);
    let v = vsetq_lane_s8::<1>(c1, v);
    let v = vsetq_lane_s8::<2>(c2, v);
    let v = vsetq_lane_s8::<3>(c3, v);
    let v = vsetq_lane_s8::<4>(c4, v);
    let v = vsetq_lane_s8::<5>(c5, v);
    let v = vsetq_lane_s8::<6>(c6, v);
    let v = vsetq_lane_s8::<7>(c7, v);
    let v = vsetq_lane_s8::<8>(c8, v);
    let v = vsetq_lane_s8::<9>(c9, v);
    let v = vsetq_lane_s8::<10>(c10, v);
    let v = vsetq_lane_s8::<11>(c11, v);
    let v = vsetq_lane_s8::<12>(c12, v);
    let v = vsetq_lane_s8::<13>(c13, v);
    let v = vsetq_lane_s8::<14>(c14, v);
    vsetq_lane_s8::<15>(c15, v)
}

/// NEON dotprod（`neon,dotprod`）向け整数 i8×i8 dot。`codes`（索引ノードの
/// 格納コード）・`signed`（`crate::sq8::Sq8QueryCodes` 由来の符号付きクエリ
/// コード。`I8QueryOperands::signed`）を chunk 16 要素ごとに [`load16`] で
/// `int8x16_t` へ構築し、`vdotq_s32`（s8×s8→i32 の非飽和 4 要素積和）で
/// 積和したうえで `vaddvq_s32` により水平和を取る。`codes`／`signed` の長さが
/// 異なる場合は短い方へ切り詰める（[`super::dot_i8_scalar`] と同じ意味論）。
///
/// `row_sum` を受け取らない理由はモジュール doc 参照（VNNI 系と異なり符号
/// 復元が不要）。
#[target_feature(enable = "neon,dotprod")]
#[inline(never)]
pub(super) fn dot_i8_neon_dotprod(codes: &[i8], signed: &[i8]) -> i32 {
    let len = codes.len().min(signed.len());
    let codes = &codes[..len];
    let signed = &signed[..len];

    let (c_chunks, c_rem) = codes.as_chunks::<16>();
    let (s_chunks, s_rem) = signed.as_chunks::<16>();

    let mut acc = vdupq_n_s32(0);
    for (cc, sc) in c_chunks.iter().zip(s_chunks.iter()) {
        let va = load16(cc);
        let vb = load16(sc);
        acc = vdotq_s32(acc, va, vb);
    }
    let lane_sum = vaddvq_s32(acc);

    let rem_sum: i32 = c_rem.iter().zip(s_rem.iter()).fold(0i32, |sum, (&c, &s)| {
        sum.wrapping_add(i32::from(c).wrapping_mul(i32::from(s)))
    });
    lane_sum.wrapping_add(rem_sum)
}
