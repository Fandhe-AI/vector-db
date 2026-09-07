//! x86_64 整数 i8×i8 dot カーネル本体（Issue #522・親 #520・前提 #521。
//! ポインタ: TASK-132・TASK-156・CORE-16）。
//!
//! 呼び出し文脈: `isa.rs::I8Kernel::dot_i8` の 3 箇所（AVX-512 VNNI・
//! AVX-VNNI（256bit）・AVX2 i16 widen フォールバック）からのみ `unsafe` 呼び
//! 出しされる（ADR `docs/design/simd-intrinsics-adoption.md` 決定 1: `unsafe`
//! は `isa.rs` のトークンディスパッチ箇所以外に持ち込まない。本モジュール
//! 自体は `unsafe` を一切含まない safe fn のみで構成する。`tests/isa.rs::
//! unsafe_is_confined_to_isa_module_with_safety_comments` が機械検証する）。
//!
//! 3 カーネルはいずれも `crate::sq8::Sq8QueryCodes`（`hnsw.rs::
//! PreparedI8Source` が保持）由来の `signed`／`shifted` オペランドと
//! ノード側格納コード（`i8`）から、`i32` の内積（wrapping 加算）を求める。
//! VNNI 系（u8×s8→i32 の `vpdpbusd`）は符号なしシフト済み `shifted`
//! （`u_d = qq_d + 128`）を使い、呼び出し元（`isa.rs::I8Kernel::dot_i8`）が
//! `row_sum`（`Σ code_d`）を使って `acc − 128*row_sum` へ復元する。i16 widen
//! フォールバック（`vpmaddwd`）は符号付き `signed` をそのまま使うため復元は
//! 不要（呼び出し元が `row_sum` を無視する）。
//!
//! 端数（`as_chunks` で割り切れない末尾要素）はいずれもスカラー wrapping
//! 和で処理する（整数演算のため、tail 方式の違い（Issue #528 の f32 版
//! `PADDED_TAIL`）はビット同一性に影響しない——`i32` 加算は結合律が完全に
//! 成立する）。

use std::arch::x86_64::*;

/// AVX-512 VNNI（`avx512f,avx512bw,avx512vnni`）向け整数 dot。`chunk` 64 要素
/// ごとに `_mm512_set_epi8`（`set` 構築のみ。ポインタ load/store 不使用——決定
/// 1）で `shifted`（u8）・`codes`（i8）を組み立て、`_mm512_dpbusd_epi32`
/// （非飽和 u8×s8→i32 積和）→ `_mm512_reduce_add_epi32`（水平和）で縮約する。
/// `codes`／`shifted` の長さが異なる場合は短い方へ切り詰める
/// （[`super::dot_i8_scalar`] と同じ意味論）。
#[target_feature(enable = "avx512f,avx512bw,avx512vnni")]
pub(super) fn dot_i8_avx512_vnni(codes: &[i8], shifted: &[u8]) -> i32 {
    let len = codes.len().min(shifted.len());
    let codes = &codes[..len];
    let shifted = &shifted[..len];

    let (c_chunks, c_rem) = codes.as_chunks::<64>();
    let (s_chunks, s_rem) = shifted.as_chunks::<64>();

    // `avx512f,avx512bw,avx512vnni` は本 fn の `#[target_feature]` で
    // 有効化済みのため、以下の intrinsics 呼び出しは `unsafe` ブロックを
    // 要さない safe fn 呼び出しである（`x86_block4.rs`・f16c カーネルと同じ
    // 注記）。
    let mut acc = _mm512_setzero_si512();
    for (cc, sc) in c_chunks.iter().zip(s_chunks.iter()) {
        let vc = _mm512_set_epi8(
            cc[63], cc[62], cc[61], cc[60], cc[59], cc[58], cc[57], cc[56], cc[55], cc[54], cc[53],
            cc[52], cc[51], cc[50], cc[49], cc[48], cc[47], cc[46], cc[45], cc[44], cc[43], cc[42],
            cc[41], cc[40], cc[39], cc[38], cc[37], cc[36], cc[35], cc[34], cc[33], cc[32], cc[31],
            cc[30], cc[29], cc[28], cc[27], cc[26], cc[25], cc[24], cc[23], cc[22], cc[21], cc[20],
            cc[19], cc[18], cc[17], cc[16], cc[15], cc[14], cc[13], cc[12], cc[11], cc[10], cc[9],
            cc[8], cc[7], cc[6], cc[5], cc[4], cc[3], cc[2], cc[1], cc[0],
        );
        let vs = _mm512_set_epi8(
            sc[63] as i8,
            sc[62] as i8,
            sc[61] as i8,
            sc[60] as i8,
            sc[59] as i8,
            sc[58] as i8,
            sc[57] as i8,
            sc[56] as i8,
            sc[55] as i8,
            sc[54] as i8,
            sc[53] as i8,
            sc[52] as i8,
            sc[51] as i8,
            sc[50] as i8,
            sc[49] as i8,
            sc[48] as i8,
            sc[47] as i8,
            sc[46] as i8,
            sc[45] as i8,
            sc[44] as i8,
            sc[43] as i8,
            sc[42] as i8,
            sc[41] as i8,
            sc[40] as i8,
            sc[39] as i8,
            sc[38] as i8,
            sc[37] as i8,
            sc[36] as i8,
            sc[35] as i8,
            sc[34] as i8,
            sc[33] as i8,
            sc[32] as i8,
            sc[31] as i8,
            sc[30] as i8,
            sc[29] as i8,
            sc[28] as i8,
            sc[27] as i8,
            sc[26] as i8,
            sc[25] as i8,
            sc[24] as i8,
            sc[23] as i8,
            sc[22] as i8,
            sc[21] as i8,
            sc[20] as i8,
            sc[19] as i8,
            sc[18] as i8,
            sc[17] as i8,
            sc[16] as i8,
            sc[15] as i8,
            sc[14] as i8,
            sc[13] as i8,
            sc[12] as i8,
            sc[11] as i8,
            sc[10] as i8,
            sc[9] as i8,
            sc[8] as i8,
            sc[7] as i8,
            sc[6] as i8,
            sc[5] as i8,
            sc[4] as i8,
            sc[3] as i8,
            sc[2] as i8,
            sc[1] as i8,
            sc[0] as i8,
        );
        acc = _mm512_dpbusd_epi32(acc, vs, vc);
    }
    let lane_sum = _mm512_reduce_add_epi32(acc);

    let rem_sum: i32 = c_rem.iter().zip(s_rem.iter()).fold(0i32, |sum, (&c, &s)| {
        sum.wrapping_add(i32::from(s).wrapping_mul(i32::from(c)))
    });
    lane_sum.wrapping_add(rem_sum)
}

/// AVX-VNNI（`avx2,avxvnni`。256bit・Alder Lake 以降で VEX 符号化された
/// `vpdpbusd` を使う）向け整数 dot。構造は [`dot_i8_avx512_vnni`] と同型
/// （chunk 32・`_mm256_dpbusd_avx_epi32`）で、水平和は `_mm256_castsi256_si128`／
/// `_mm256_extracti128_si256`／`_mm_add_epi32`／`_mm_shuffle_epi32`／
/// `_mm_cvtsi128_si32`（ポインタ store 不使用——決定 1・決定 9）を使う。
#[target_feature(enable = "avx2,avxvnni")]
pub(super) fn dot_i8_avx_vnni(codes: &[i8], shifted: &[u8]) -> i32 {
    let len = codes.len().min(shifted.len());
    let codes = &codes[..len];
    let shifted = &shifted[..len];

    let (c_chunks, c_rem) = codes.as_chunks::<32>();
    let (s_chunks, s_rem) = shifted.as_chunks::<32>();

    let mut acc = _mm256_setzero_si256();
    for (cc, sc) in c_chunks.iter().zip(s_chunks.iter()) {
        let vc = _mm256_set_epi8(
            cc[31], cc[30], cc[29], cc[28], cc[27], cc[26], cc[25], cc[24], cc[23], cc[22], cc[21],
            cc[20], cc[19], cc[18], cc[17], cc[16], cc[15], cc[14], cc[13], cc[12], cc[11], cc[10],
            cc[9], cc[8], cc[7], cc[6], cc[5], cc[4], cc[3], cc[2], cc[1], cc[0],
        );
        let vs = _mm256_set_epi8(
            sc[31] as i8,
            sc[30] as i8,
            sc[29] as i8,
            sc[28] as i8,
            sc[27] as i8,
            sc[26] as i8,
            sc[25] as i8,
            sc[24] as i8,
            sc[23] as i8,
            sc[22] as i8,
            sc[21] as i8,
            sc[20] as i8,
            sc[19] as i8,
            sc[18] as i8,
            sc[17] as i8,
            sc[16] as i8,
            sc[15] as i8,
            sc[14] as i8,
            sc[13] as i8,
            sc[12] as i8,
            sc[11] as i8,
            sc[10] as i8,
            sc[9] as i8,
            sc[8] as i8,
            sc[7] as i8,
            sc[6] as i8,
            sc[5] as i8,
            sc[4] as i8,
            sc[3] as i8,
            sc[2] as i8,
            sc[1] as i8,
            sc[0] as i8,
        );
        acc = _mm256_dpbusd_avx_epi32(acc, vs, vc);
    }

    let lo = _mm256_castsi256_si128(acc);
    let hi = _mm256_extracti128_si256(acc, 1);
    let sum128 = _mm_add_epi32(lo, hi);
    let shuf = _mm_shuffle_epi32(sum128, 0b01_00_11_10);
    let sums = _mm_add_epi32(sum128, shuf);
    let shuf2 = _mm_shuffle_epi32(sums, 0b00_00_00_01);
    let final_sum = _mm_add_epi32(sums, shuf2);
    let lane_sum = _mm_cvtsi128_si32(final_sum);

    let rem_sum: i32 = c_rem.iter().zip(s_rem.iter()).fold(0i32, |sum, (&c, &s)| {
        sum.wrapping_add(i32::from(s).wrapping_mul(i32::from(c)))
    });
    lane_sum.wrapping_add(rem_sum)
}

/// AVX2 i16 widen フォールバック（VNNI 非対応 CPU 向け）。`signed`
/// （符号付き `qq_d`。VNNI 系とは異なり `shifted` の符号なしシフトを要さない
/// ——`vpmaddwd` は i16×i16→i32 の積和で `row_sum` 補正が不要）と `codes`
/// （i8）を chunk 16 要素ごとに `_mm_set_epi8` → `_mm256_cvtepi8_epi16`
/// （i8→i16 昇格）→ `_mm256_madd_epi16`（i16×i16→i32 積和・水平ペア加算）で
/// 積和し、水平和は [`dot_i8_avx_vnni`] と同じ構成で行う。
#[target_feature(enable = "avx2")]
pub(super) fn dot_i8_avx2_widen(codes: &[i8], signed: &[i8]) -> i32 {
    let len = codes.len().min(signed.len());
    let codes = &codes[..len];
    let signed = &signed[..len];

    let (c_chunks, c_rem) = codes.as_chunks::<16>();
    let (q_chunks, q_rem) = signed.as_chunks::<16>();

    let mut acc = _mm256_setzero_si256();
    for (cc, qc) in c_chunks.iter().zip(q_chunks.iter()) {
        let vc8 = _mm_set_epi8(
            cc[15], cc[14], cc[13], cc[12], cc[11], cc[10], cc[9], cc[8], cc[7], cc[6], cc[5],
            cc[4], cc[3], cc[2], cc[1], cc[0],
        );
        let vq8 = _mm_set_epi8(
            qc[15], qc[14], qc[13], qc[12], qc[11], qc[10], qc[9], qc[8], qc[7], qc[6], qc[5],
            qc[4], qc[3], qc[2], qc[1], qc[0],
        );
        let vc16 = _mm256_cvtepi8_epi16(vc8);
        let vq16 = _mm256_cvtepi8_epi16(vq8);
        let prod = _mm256_madd_epi16(vc16, vq16);
        acc = _mm256_add_epi32(acc, prod);
    }

    let lo = _mm256_castsi256_si128(acc);
    let hi = _mm256_extracti128_si256(acc, 1);
    let sum128 = _mm_add_epi32(lo, hi);
    let shuf = _mm_shuffle_epi32(sum128, 0b01_00_11_10);
    let sums = _mm_add_epi32(sum128, shuf);
    let shuf2 = _mm_shuffle_epi32(sums, 0b00_00_00_01);
    let final_sum = _mm_add_epi32(sums, shuf2);
    let lane_sum = _mm_cvtsi128_si32(final_sum);

    let rem_sum: i32 = c_rem.iter().zip(q_rem.iter()).fold(0i32, |sum, (&c, &q)| {
        sum.wrapping_add(i32::from(c).wrapping_mul(i32::from(q)))
    });
    lane_sum.wrapping_add(rem_sum)
}
