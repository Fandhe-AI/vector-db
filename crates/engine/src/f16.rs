//! IEEE 754 binary16（f16）ビット表現の共有変換層（Issue #514・親 #513。
//! ポインタ: TASK-132・TASK-156・CORE-16）。
//!
//! `batch_search.rs::pack_f16x2`（GPU 常駐コピー用の f16 パック。CORE-16）と
//! `hnsw.rs::NodeVectors::F16`（HNSW 索引ノードの f16 常駐表現。Issue #514）が
//! 同一のビット表現・丸め規則（round-to-nearest-even・±Inf 飽和・サブノーマル
//! 対応）を共有するための単一情報源として、変換関数をここへ集約する。
//! いずれの呼び出し元も格納・クエリベクトルの dtype は f32 のまま変更せず、
//! 本モジュールは常駐表現へのエンコード層としてのみ使う
//! （`docs/design/simd-intrinsics-adoption.md` 決定 3）。
//!
//! `half` 等の外部クレートは依存最小方針（[dependency-policy](../../../.claude/rules/dependency-policy.md)）
//! により不採用（同 ADR 「不採用」節）。

/// f16 が表現できる有限値の絶対値上限（65504.0）。
pub(crate) const F16_MAX: f32 = 65504.0;

/// `value` が f16 の有限範囲（`|value| <= F16_MAX`）に収まるかどうか。
///
/// 非有限（NaN・Inf）は `false` を返す（呼び出し元は非有限値を上流
/// （`kernel::KernelError::NonFiniteQuery` 等）で既に拒否している前提だが、
/// 本関数自体は防御的に判定する）。
pub(crate) fn fits_f16(value: f32) -> bool {
    value.is_finite() && value.abs() <= F16_MAX
}

/// f32 を IEEE 754 binary16（f16）のビットパターンへ丸める。オーバーフローは
/// ±Inf へ飽和し、指数アンダーフロー域はサブノーマル f16 として正しく
/// round-to-nearest-even で丸める（`batch_search.rs` から移設。旧実装の
/// codex レビュー指摘対応履歴はそちらの履歴として残置し、ここでは挙動のみを
/// 引き継ぐ）。
pub(crate) fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    if value.is_nan() {
        // NaN は符号なしの quiet NaN パターンへ正規化する（NaN のペイロードは
        // 復元時に意味を持たないため、往復精度の対象外）。
        return sign | 0x7E00;
    }
    let abs_bits = bits & 0x7FFF_FFFF;
    if abs_bits == 0 {
        // +0.0/-0.0 は下のサブノーマル変換（暗黙ビット付与）に通すと非ゼロに
        // なってしまうため、符号だけを残して個別に返す。
        return sign;
    }
    let exp = ((abs_bits >> 23) & 0xFF) as i32 - 127 + 15;
    let mantissa = abs_bits & 0x007F_FFFF;

    if exp >= 0x1F {
        // 指数オーバーフロー: ±Inf へ飽和する。
        return sign | 0x7C00;
    }
    if exp <= 0 {
        // 指数アンダーフロー域: サブノーマル f16（もしくは丸めた結果としての 0）
        // へ変換する。入力を暗黙ビット付き 24 bit 仮数 `1.mantissa` として扱い、
        // 下位ビットの正規化丸めと同じ round-to-nearest-even ロジックを、
        // シフト量 `14 - exp`（`exp <= 0` なので 14 以上）で適用する。
        let full_mantissa = 0x0080_0000u32 | mantissa;
        let shift = (14 - exp).min(30) as u32;
        let round_bit = 1u32 << (shift - 1);
        let lower_mask = round_bit - 1;
        let mut mantissa16 = full_mantissa >> shift;
        let remainder = full_mantissa & (round_bit | lower_mask);
        if remainder > round_bit || (remainder == round_bit && (mantissa16 & 1) == 1) {
            mantissa16 += 1;
        }
        return sign | (mantissa16 as u16);
    }
    // 23 ビット仮数を 10 ビットへ round-to-nearest-even で丸める。
    let shift = 13u32;
    let round_bit = 1u32 << (shift - 1);
    let lower_mask = round_bit.wrapping_sub(1);
    let mut mantissa16 = mantissa >> shift;
    let remainder = mantissa & (round_bit | lower_mask);
    let mut exp16 = exp as u32;
    if remainder > round_bit || (remainder == round_bit && (mantissa16 & 1) == 1) {
        mantissa16 += 1;
        if mantissa16 == 0x0400 {
            // 仮数繰り上がりで指数が 1 増える。
            mantissa16 = 0;
            exp16 += 1;
        }
    }
    if exp16 >= 0x1F {
        return sign | 0x7C00;
    }
    sign | ((exp16 as u16) << 10) | (mantissa16 as u16)
}

/// f16 ビットパターンを f32 へ復元する（`batch_search.rs` から移設。GPU シェーダの
/// `unpack2x16float` と等価な意味論）。
pub(crate) fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = (bits & 0x8000) as u32;
    let exp = ((bits >> 10) & 0x1F) as u32;
    let mantissa = (bits & 0x03FF) as u32;

    let (out_exp, out_mantissa) = if exp == 0 {
        if mantissa == 0 {
            (0u32, 0u32)
        } else {
            // サブノーマル f16（値 = mantissa * 2^-24）を正規化 f32 へ変換する。
            let mut m = mantissa;
            let mut e = -14i32;
            while m & 0x0400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x03FF;
            let exp32 = (e + 127) as u32;
            (exp32, m << 13)
        }
    } else if exp == 0x1F {
        (0xFFu32, mantissa << 13)
    } else {
        ((exp as i32 - 15 + 127) as u32, mantissa << 13)
    };

    let out_bits = (sign << 16) | (out_exp << 23) | out_mantissa;
    f32::from_bits(out_bits)
}

/// `src` の各要素を f16 ビット表現へエンコードして `out` へ追記する
/// （`hnsw.rs::HnswIndex::freeze_from` の F16 常駐化・呼び出し元は都度
/// `out.clear()` 済みの再利用バッファを渡す想定）。
///
/// `dim` は 1 行あたりの要素数（呼び出し元の検証済み値。`checked_mul` で
/// `src.len()` との整合を確認する用途ではなく、単に `try_reserve_exact` の
/// 事前確保サイズ算出にのみ使う）。範囲外（`|x| > F16_MAX`）の成分が 1 つでも
/// あれば、どの添字かを含む [`F16EncodeError::OutOfRange`] を返し `out` は
/// 変更しない（fail-closed。呼び出し元は D6 のとおり F32 常駐へ縮退する）。
pub(crate) fn encode_rows(src: &[f32], out: &mut Vec<u16>) -> Result<(), F16EncodeError> {
    for (index, &value) in src.iter().enumerate() {
        if !fits_f16(value) {
            return Err(F16EncodeError::OutOfRange { index });
        }
    }
    out.try_reserve_exact(src.len())
        .map_err(|_| F16EncodeError::AllocationFailed)?;
    out.extend(src.iter().map(|&v| f32_to_f16_bits(v)));
    Ok(())
}

/// [`encode_rows`] の失敗要因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum F16EncodeError {
    /// `index` 番目の成分が f16 の有限範囲（`|x| <= 65504.0`）を超えている。
    OutOfRange { index: usize },
    /// 出力バッファの確保に失敗した（`try_reserve_exact` が `Err`）。
    AllocationFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_f16_boundary() {
        assert!(fits_f16(65504.0));
        assert!(fits_f16(-65504.0));
        assert!(!fits_f16(65504.1));
        assert!(!fits_f16(f32::INFINITY));
        assert!(!fits_f16(f32::NAN));
    }

    #[test]
    fn encode_rows_roundtrips_and_rejects_out_of_range() {
        let src = vec![0.0f32, 1.5, -3.25, 65504.0];
        let mut out = Vec::new();
        encode_rows(&src, &mut out).expect("in range");
        assert_eq!(out.len(), src.len());
        for (v, bits) in src.iter().zip(out.iter()) {
            assert!((f16_bits_to_f32(*bits) - v).abs() < 1e-3);
        }

        let bad = vec![0.0f32, 70000.0, 1.0];
        let mut out2 = vec![9u16];
        let err = encode_rows(&bad, &mut out2).unwrap_err();
        assert_eq!(err, F16EncodeError::OutOfRange { index: 1 });
        // 失敗時は出力を変更しない契約。
        assert_eq!(out2, vec![9u16]);
    }
}
