//! GF(2^255-19) 上の定数時間フィールド算術（TLS 1.3 自作実装の共有基盤）。
//!
//! [`super::x25519`]（X25519 鍵交換。Issue #955）が使う最小限の演算に加え、
//! 後続の Ed25519（Issue #961）とこのモジュールを共有する前提で
//! `pub(crate)` として `tls` モジュール内に閉じる。呼び出し元（ladder・
//! 署名検証）が渡す値はいずれも「秘密値かもしれない」中間状態であるため、
//! 本モジュールの演算はすべて **入力値に依存する分岐・配列添字・テーブル
//! 参照を持たない**（タイミングサイドチャネル対策）。唯一の例外は
//! [`Fe::from_bytes`] の固定オフセット読み出しと [`Fe::to_bytes`] の固定
//! オフセット書き込みで、これらは配列長 32 に対する**コンパイル時に範囲が
//! 確定する定数オフセット**のみを使うため、値に依存した分岐にはならない。
//!
//! 表現: 基数 2^51 の 5 limb（`[u64; 5]`）。各演算後は [`Fe::weak_reduce`]
//! により各 limb を 2^51 未満（＋わずかな繰り上がり余地）へ畳み込み、
//! 後続演算の乗算オーバーフロー（`u128` の範囲内で収まること）を保証する。
//! 乗算・逆元は curve25519 系実装で広く公知の手法（基数 2^51・5 limb 表現、
//! フェルマーの小定理による固定長加算連鎖）に基づく自作実装であり、
//! 依存クレートは追加しない。

/// 各 limb の有効ビット幅（基数 2^51）。
const LIMB_BITS: u32 = 51;
/// limb をこのビット幅へ畳み込むためのマスク。
const MASK51: u64 = (1u64 << LIMB_BITS) - 1;
/// mod p = 2^255 - 19 の還元で繰り上がりに乗じる係数（2^255 ≡ 19 mod p）。
const REDUCE19: u64 = 19;

/// GF(2^255-19) の元。基数 2^51・5 limb のリトルエンディアン表現
/// （`value = Σ limb[i] * 2^(51*i)`）。
///
/// 各演算直後の limb 上界は `weak_reduce` 呼び出し後で `< 2^51 + 2^6`
/// 程度（19 倍の繰り上がり折り込みによる余剰込み）であり、`mul`/`square`
/// が limb 同士の積を `u128` へ蓄積する際にオーバーフローしないことを
/// 保証する（各 limb < 2^57 と見ても積は < 2^114、5 項＋19 倍の合計でも
/// `u128` の範囲に収まる）。
#[derive(Clone, Copy)]
pub(crate) struct Fe([u64; 5]);

impl Fe {
    /// 加法単位元 0。
    pub(crate) const ZERO: Fe = Fe([0, 0, 0, 0, 0]);
    /// 乗法単位元 1。
    pub(crate) const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// `u64` の小さな定数（`2^51` 未満）から `Fe` を作る。X25519 の
    /// `a24 = 121665` のような固定係数を single-limb 表現として扱う。
    pub(crate) const fn from_u64(v: u64) -> Fe {
        Fe([v, 0, 0, 0, 0])
    }

    /// 32 バイトのリトルエンディアン表現から読み込む。RFC 7748 §5 の
    /// decodeUCoordinate に従い、**最上位ビット（bit 255）を無条件でマスク**
    /// し、`p` 以上の非正準値もそのまま受理する（演算過程で mod p として
    /// 扱われる）。呼び出し元（[`super::x25519::x25519`]）は untrusted な
    /// peer 公開鍵・ローカル一時鍵の双方をこの関数へ渡す。
    pub(crate) fn from_bytes(bytes: &[u8; 32]) -> Fe {
        // 各オフセットは 0/6/12/19/24 の固定定数であり、`load64_le` は
        // `off+7 <= 31` を満たす呼び出ししか行わない（コンパイル時に
        // 確定する範囲。添字は入力バイト値に依存しない）。
        let h0 = load64_le(bytes, 0) & MASK51;
        let h1 = (load64_le(bytes, 6) >> 3) & MASK51;
        let h2 = (load64_le(bytes, 12) >> 6) & MASK51;
        let h3 = (load64_le(bytes, 19) >> 1) & MASK51;
        // 8 バイト窓 bytes[24..32] の bit 255（窓内では bit 63）は
        // `>> 12` の後 bit 51 に来るため、`& MASK51` が RFC 7748 の
        // 「最上位ビットを無視する」要求をそのまま満たす。
        let h4 = (load64_le(bytes, 24) >> 12) & MASK51;
        Fe([h0, h1, h2, h3, h4])
    }

    /// 完全に正準化した 32 バイトのリトルエンディアン表現へ変換する。
    ///
    /// 秘密値（共有秘密の X 座標）を扱いうるため、`h >= p` の判定・補正は
    /// 分岐なしのマスク演算（`(limb + 19) >> 51` の桁上げ連鎖による除算
    /// 商 `q ∈ {0, 1}` の算出）で行う。curve25519 系実装で広く使われる
    /// 公知の手法（本 crate 固有の秘密ではない）。
    pub(crate) fn to_bytes(mut self) -> [u8; 32] {
        self.weak_reduce();
        self.weak_reduce();
        let [mut h0, mut h1, mut h2, mut h3, mut h4] = self.0;

        // q = floor(h / p)。p = 2^255-19 なので h < 2*p の前提下では q は
        // 0 か 1 のいずれか。桁上げ連鎖の最終ビットがそのまま q になる。
        let mut q = (h0 + REDUCE19) >> LIMB_BITS;
        q = (h1 + q) >> LIMB_BITS;
        q = (h2 + q) >> LIMB_BITS;
        q = (h3 + q) >> LIMB_BITS;
        q = (h4 + q) >> LIMB_BITS;

        // h - p*q を計算する（p の limb 表現は [2^51-19, 2^51-1, ×3]）。
        h0 = h0.wrapping_add(REDUCE19 * q);
        let mut carry = h0 >> LIMB_BITS;
        h0 &= MASK51;
        h1 = h1.wrapping_add(carry);
        carry = h1 >> LIMB_BITS;
        h1 &= MASK51;
        h2 = h2.wrapping_add(carry);
        carry = h2 >> LIMB_BITS;
        h2 &= MASK51;
        h3 = h3.wrapping_add(carry);
        carry = h3 >> LIMB_BITS;
        h3 &= MASK51;
        h4 = h4.wrapping_add(carry);
        h4 &= MASK51;

        let mut out = [0u8; 32];
        store_limbs_le(&mut out, [h0, h1, h2, h3, h4]);
        out
    }

    /// `self + other`（mod p の弱還元。limb は < 2^51 程度へ畳み込み済み）。
    pub(crate) fn add(&self, other: &Fe) -> Fe {
        let mut out = [0u64; 5];
        for ((o, a), b) in out.iter_mut().zip(self.0.iter()).zip(other.0.iter()) {
            *o = a + b;
        }
        let mut r = Fe(out);
        r.weak_reduce();
        r
    }

    /// `self - other`（mod p）。limb ごとの下溢を防ぐため `2p` を
    /// バイアスとして先に加算してから引く（`a - b ≡ a + 2p - b`）。
    /// `2p` の limb 表現は `p = [2^51-19, 2^51-1, 2^51-1, 2^51-1, 2^51-1]`
    /// を 2 倍したもの。
    pub(crate) fn sub(&self, other: &Fe) -> Fe {
        const BIAS: [u64; 5] = [
            2 * ((1u64 << LIMB_BITS) - 19),
            2 * ((1u64 << LIMB_BITS) - 1),
            2 * ((1u64 << LIMB_BITS) - 1),
            2 * ((1u64 << LIMB_BITS) - 1),
            2 * ((1u64 << LIMB_BITS) - 1),
        ];
        let mut out = [0u64; 5];
        for (((o, a), bias), b) in out
            .iter_mut()
            .zip(self.0.iter())
            .zip(BIAS.iter())
            .zip(other.0.iter())
        {
            *o = (a + bias) - b;
        }
        let mut r = Fe(out);
        r.weak_reduce();
        r
    }

    /// `self * other`（mod 2^255-19）。5×5 の limb 積を `u128` へ蓄積し、
    /// 2^255 ≡ 19（mod p）を使って高位項を折り返してから弱還元する。
    pub(crate) fn mul(&self, other: &Fe) -> Fe {
        let f = self.0;
        let g = other.0;
        let f19 = [
            0u128,
            REDUCE19 as u128 * f[1] as u128,
            REDUCE19 as u128 * f[2] as u128,
            REDUCE19 as u128 * f[3] as u128,
            REDUCE19 as u128 * f[4] as u128,
        ];

        let g0 = g[0] as u128;
        let g1 = g[1] as u128;
        let g2 = g[2] as u128;
        let g3 = g[3] as u128;
        let g4 = g[4] as u128;
        let f0 = f[0] as u128;
        let f1 = f[1] as u128;
        let f2 = f[2] as u128;
        let f3 = f[3] as u128;
        let f4 = f[4] as u128;

        let mut t0 = f0 * g0 + f19[1] * g4 + f19[2] * g3 + f19[3] * g2 + f19[4] * g1;
        let mut t1 = f0 * g1 + f1 * g0 + f19[2] * g4 + f19[3] * g3 + f19[4] * g2;
        let mut t2 = f0 * g2 + f1 * g1 + f2 * g0 + f19[3] * g4 + f19[4] * g3;
        let mut t3 = f0 * g3 + f1 * g2 + f2 * g1 + f3 * g0 + f19[4] * g4;
        let mut t4 = f0 * g4 + f1 * g3 + f2 * g2 + f3 * g1 + f4 * g0;

        // limb 桁上げ（2 回連続で行い、最終桁上げが 51 bit マスク後に
        // 十分小さいことを保証する。乗算後の中間値は最大でも数ビットの
        // 余剰しか持たないため 2 回で収束する）。
        let mask = MASK51 as u128;
        let mut c = t0 >> LIMB_BITS;
        t0 &= mask;
        t1 += c;
        c = t1 >> LIMB_BITS;
        t1 &= mask;
        t2 += c;
        c = t2 >> LIMB_BITS;
        t2 &= mask;
        t3 += c;
        c = t3 >> LIMB_BITS;
        t3 &= mask;
        t4 += c;
        c = t4 >> LIMB_BITS;
        t4 &= mask;
        t0 += c * REDUCE19 as u128;
        c = t0 >> LIMB_BITS;
        t0 &= mask;
        t1 += c;

        Fe([t0 as u64, t1 as u64, t2 as u64, t3 as u64, t4 as u64])
    }

    /// `self^2`。`mul(self, self)` に委譲する（乗算専用の平方最適化は行わず、
    /// 実装の分岐・添字面を単一の `mul` にまとめてレビュー面を絞る）。
    pub(crate) fn square(&self) -> Fe {
        self.mul(self)
    }

    /// `self` を `k` 回連続で平方する（[`Fe::invert`] の固定長加算連鎖用）。
    /// `k` は呼び出し側が渡す公開のコンパイル時定数であり、秘密値には
    /// 依存しない。
    fn pow2k(&self, k: u32) -> Fe {
        let mut r = *self;
        for _ in 0..k {
            r = r.square();
        }
        r
    }

    /// `self^(p-2) ≡ self^-1`（mod p。フェルマーの小定理）。
    ///
    /// 254 回の平方・11 回の乗算からなる固定長の加算連鎖（curve25519 系
    /// 実装で公知の手法）で計算し、`self` の値に依存する分岐・ループ回数
    /// を一切持たない。`self == 0` の場合は数学的に逆元が存在しないが、
    /// 本関数は `0^(p-2) mod p == 0` を返す（分岐なし）。呼び出し元
    /// （[`super::x25519`] の ladder 最終段）は Z 座標が 0 になり得る
    /// 入力（低次点）を別途 all-zero 判定で拒否するため、ここでの `0`
    /// 出力自体が安全性を損なうことはない。
    pub(crate) fn invert(&self) -> Fe {
        let z1 = *self;
        let z2 = z1.square();
        let z8 = z2.square().square();
        let z9 = z1.mul(&z8);
        let z11 = z2.mul(&z9);
        let z22 = z11.square();
        let z_5_0 = z9.mul(&z22);
        let z_10_5 = z_5_0.pow2k(5).mul(&z_5_0);
        let z_20_10 = z_10_5.pow2k(10).mul(&z_10_5);
        let z_40_20 = z_20_10.pow2k(20).mul(&z_20_10);
        let z_50_10 = z_40_20.pow2k(10).mul(&z_10_5);
        let z_100_50 = z_50_10.pow2k(50).mul(&z_50_10);
        let z_200_100 = z_100_50.pow2k(100).mul(&z_100_50);
        let z_250_50 = z_200_100.pow2k(50).mul(&z_50_10);
        let z_255_5 = z_250_50.pow2k(5);
        z_255_5.mul(&z11)
    }

    /// `choice`（0 または 1）に応じて `a` と `b` を定数時間で入れ替える。
    /// `choice` の値に応じた分岐は行わず、`mask = 0 - choice`
    /// （`choice=1` なら全ビット 1、`choice=0` なら全ビット 0）による
    /// XOR マスクのみで実現する（[`super::x25519`] ladder の中核）。
    pub(crate) fn cswap(a: &mut Fe, b: &mut Fe, choice: u64) {
        let mask = 0u64.wrapping_sub(choice);
        for (ai, bi) in a.0.iter_mut().zip(b.0.iter_mut()) {
            let t = mask & (*ai ^ *bi);
            *ai ^= t;
            *bi ^= t;
        }
    }

    /// limb を 2^51 未満（＋わずかな繰り上がり余地）へ 1 回だけ畳み込む。
    /// `add`/`sub` の結果（各 limb が高々数ビットの余剰を持つ）を後続の
    /// `mul`/`square` の入力として安全な範囲へ戻すために使う。
    fn weak_reduce(&mut self) {
        let mut c;
        c = self.0[0] >> LIMB_BITS;
        self.0[0] &= MASK51;
        self.0[1] += c;
        c = self.0[1] >> LIMB_BITS;
        self.0[1] &= MASK51;
        self.0[2] += c;
        c = self.0[2] >> LIMB_BITS;
        self.0[2] &= MASK51;
        self.0[3] += c;
        c = self.0[3] >> LIMB_BITS;
        self.0[3] &= MASK51;
        self.0[4] += c;
        c = self.0[4] >> LIMB_BITS;
        self.0[4] &= MASK51;
        self.0[0] += c * REDUCE19;
    }
}

/// 固定オフセット `off`（`off + 7 <= 31` を満たす呼び出し元のみが使う
/// コンパイル時定数）から 8 バイトをリトルエンディアンで読む。
/// `bytes[off + k]`（`k ∈ 0..8`）の添字は呼び出し元が渡すオフセット定数
/// にのみ依存し、バイト値そのもの（untrusted）には依存しない。
fn load64_le(bytes: &[u8; 32], off: usize) -> u64 {
    (bytes[off] as u64)
        | (bytes[off + 1] as u64) << 8
        | (bytes[off + 2] as u64) << 16
        | (bytes[off + 3] as u64) << 24
        | (bytes[off + 4] as u64) << 32
        | (bytes[off + 5] as u64) << 40
        | (bytes[off + 6] as u64) << 48
        | (bytes[off + 7] as u64) << 56
}

/// 正準化済みの 5×51bit limb を 32 バイトのリトルエンディアン表現へ
/// 詰め直す（[`Fe::to_bytes`] 専用）。
fn store_limbs_le(out: &mut [u8; 32], limbs: [u64; 5]) {
    // 255 ビット（5*51）を 8 ビット単位のバイト列へ詰め替える。ビット位置
    // `bitpos` は 0..255 の範囲を左から右へ単調に進むコンパイル時に
    // 追跡可能なオフセットであり、入力値（limb の中身）には依存しない。
    let mut acc: u128 = 0;
    let mut acc_bits: u32 = 0;
    let mut out_idx = 0usize;
    for &limb in limbs.iter() {
        acc |= (limb as u128) << acc_bits;
        acc_bits += LIMB_BITS;
        while acc_bits >= 8 && out_idx < 32 {
            out[out_idx] = (acc & 0xff) as u8;
            acc >>= 8;
            acc_bits -= 8;
            out_idx += 1;
        }
    }
    if out_idx < 32 {
        out[out_idx] = (acc & 0xff) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fe_from_hex_le(hex: &str) -> Fe {
        let bytes = hex_to_bytes32(hex);
        Fe::from_bytes(&bytes)
    }

    fn hex_to_bytes32(hex: &str) -> [u8; 32] {
        assert_eq!(hex.len(), 64, "expected 64 hex chars for 32 bytes");
        let mut out = [0u8; 32];
        for i in 0..32 {
            let byte_str = &hex[i * 2..i * 2 + 2];
            out[i] = u8::from_str_radix(byte_str, 16).expect("valid hex in test fixture");
        }
        out
    }

    fn bytes32_to_hex(bytes: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes.iter() {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    #[test]
    fn zero_and_one_roundtrip() {
        assert_eq!(Fe::ZERO.to_bytes(), [0u8; 32]);
        let mut one_bytes = [0u8; 32];
        one_bytes[0] = 1;
        assert_eq!(Fe::ONE.to_bytes(), one_bytes);
    }

    #[test]
    fn to_bytes_reduces_p_minus_one_to_canonical() {
        // p - 1 = 2^255 - 20 のリトルエンディアン表現。
        let p_minus_1_hex = "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f";
        let fe = fe_from_hex_le(p_minus_1_hex);
        assert_eq!(bytes32_to_hex(&fe.to_bytes()), p_minus_1_hex);
    }

    #[test]
    fn to_bytes_reduces_p_to_zero() {
        // p = 2^255 - 19 は from_bytes では最上位ビットが立っていない
        // ただの大きな非正準値として読み込まれ、to_bytes で 0 に還元される。
        let p_hex = "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f";
        let fe = fe_from_hex_le(p_hex);
        assert_eq!(fe.to_bytes(), [0u8; 32]);
    }

    #[test]
    fn from_bytes_masks_top_bit() {
        let mut with_top_bit = [0u8; 32];
        with_top_bit[0] = 9;
        with_top_bit[31] = 0x80;
        let mut without_top_bit = [0u8; 32];
        without_top_bit[0] = 9;
        assert_eq!(
            Fe::from_bytes(&with_top_bit).to_bytes(),
            Fe::from_bytes(&without_top_bit).to_bytes()
        );
    }

    #[test]
    fn add_sub_roundtrip() {
        let a = Fe::from_u64(12345);
        let b = Fe::from_u64(67890);
        let sum = a.add(&b);
        let back = sum.sub(&b);
        assert_eq!(back.to_bytes(), a.to_bytes());
    }

    #[test]
    fn sub_underflow_wraps_mod_p() {
        // 0 - 1 = p - 1
        let zero = Fe::ZERO;
        let one = Fe::ONE;
        let diff = zero.sub(&one);
        let p_minus_1_hex = "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f";
        assert_eq!(bytes32_to_hex(&diff.to_bytes()), p_minus_1_hex);
    }

    #[test]
    fn mul_by_one_is_identity() {
        let a = fe_from_hex_le("0900000000000000000000000000000000000000000000000000000000000000");
        let product = a.mul(&Fe::ONE);
        assert_eq!(product.to_bytes(), a.to_bytes());
    }

    #[test]
    fn invert_produces_multiplicative_inverse() {
        for seed in [9u64, 2, 12345, 999999937] {
            let a = Fe::from_u64(seed);
            let inv = a.invert();
            let product = a.mul(&inv);
            assert_eq!(product.to_bytes(), Fe::ONE.to_bytes(), "seed={seed}");
        }
    }

    #[test]
    fn cswap_swaps_only_when_choice_is_one() {
        let mut a = Fe::from_u64(1);
        let mut b = Fe::from_u64(2);
        Fe::cswap(&mut a, &mut b, 0);
        assert_eq!(a.to_bytes(), Fe::from_u64(1).to_bytes());
        assert_eq!(b.to_bytes(), Fe::from_u64(2).to_bytes());

        Fe::cswap(&mut a, &mut b, 1);
        assert_eq!(a.to_bytes(), Fe::from_u64(2).to_bytes());
        assert_eq!(b.to_bytes(), Fe::from_u64(1).to_bytes());
    }
}
