//! Ed25519 署名生成・検証（RFC 8032 §5.1。TASK-228・WIRE-9・HTTP-10
//! ポインタ。Issue #961・親 #941）。
//!
//! 呼び出し元は [`super::certificate_verify`]（`CertificateVerify` の署名
//! 対象組み立て。Issue #961）と、後続の起動経路（鍵読み込み → 公開鍵導出。
//! Issue #967）。呼び出し先は [`super::field25519`]（GF(2^255-19) の定数時間
//! フィールド算術。X25519（Issue #955）と共有する）・[`super::sha512`]
//! （鍵展開・nonce・チャレンジ値のハッシュ計算。Issue #960）・
//! [`super::pkcs8::Ed25519Seed`]（正規の鍵入力。Issue #962）。
//!
//! **表現**: 曲線点は拡張座標 `(X:Y:Z:T)`（`x=X/Z`・`y=Y/Z`・`xy=T/Z`）で
//! 保持する。加算式（RFC 8032 §5.1.4 の統一加算式。twisted Edwards
//! `a=-1` に対して完全（complete）であり、単位元・自己加算でも例外分岐が
//! 不要）・doubling 式はいずれも数値的に affine 参照実装と照合済みの
//! 公知の手法（curve25519 系実装で広く使われる add-2008-hwcd 系の式）。
//!
//! **定数時間性**: 秘密値（クランプ済みスカラー `a`・nonce `r`）が通る
//! 経路（[`scalar_mul`]・mod L のスカラー演算 [`scalar`]）は、秘密値に
//! 依存する分岐・配列添字・テーブル参照を持たない。[`scalar_mul`] は
//! 256 回固定の double-and-add-always（ビット抽出は公開のループカウンタ
//! のみを添字に使う）、mod L の還元・乗算はビット単位 Horner 法
//! （[`scalar`] モジュール）で行う。点復元（[`EdwardsPoint::decompress`]）・
//! 署名検証は公開値（自分の公開鍵・検証対象の署名）にのみ実行されるため
//! 分岐があってもよい（各関数のコメントに明記する）。
//!
//! **ゼロ化の限界**: [`SigningKey`] の `Drop`・[`SigningKey::wipe`] は
//! `unsafe`（`write_volatile` 等）を使わない best-effort であり、最適化に
//! よる消去省略が起きない保証はない（[`super::hkdf`]・[`super::sha512`]・
//! [`super::pkcs8::Ed25519Seed`] と同じ限界）。
//!
//! **対象外**: alert の実送出・接続への結線（#965・#967 の担当）。
//! クライアント証明書・クライアント側 CertificateVerify（親 #941 の対象外）。
//! 可変時間の高速検証・バッチ検証（不要）。

use super::field25519::Fe;
use super::hkdf::zeroize;
use super::pkcs8::Ed25519Seed;
use super::sha512::{digest, Sha512};
use std::fmt;

/// 圧縮点・署名・スカラーのバイト長（32 バイト）。
const POINT_LEN: usize = 32;
/// 署名のバイト長（`R || S`）。
const SIGNATURE_LEN: usize = 64;

// === 拡張座標での点演算 ================================================

/// Edwards 曲線上の点（拡張座標 `(X:Y:Z:T)`）。非公開型（[`super::field25519::Fe`]
/// を外部へ露出しないため、[`EdwardsPoint`] 自体もこのモジュール内に閉じる）。
#[derive(Clone, Copy)]
struct EdwardsPoint {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl EdwardsPoint {
    /// 単位元 `(0, 1)`。
    const IDENTITY: EdwardsPoint = EdwardsPoint {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    };

    /// 基点 B。圧縮表現は `0x58, 0x66 * 31`（[`compress`](EdwardsPoint::compress)・
    /// [`decompress`](EdwardsPoint::decompress) の往復と `[L]B == IDENTITY` を
    /// 単体テストで固定する）。x 座標を手入力で転記するリスクを避けるため、
    /// x・y 双方の座標を独立に生成した固定値として持つ（値そのものは RFC 8032
    /// 由来の公開定数）。
    fn base() -> EdwardsPoint {
        let x = Fe::from_bytes(&BASE_X_BYTES);
        let y = Fe::from_bytes(&BASE_Y_BYTES);
        EdwardsPoint {
            x,
            y,
            z: Fe::ONE,
            t: x.mul(&y),
        }
    }

    /// RFC 8032 §5.1.4 の統一加算式（`a=-1` の HWCD 式。完全公式のため
    /// 単位元・自己加算を含むあらゆる入力の組で例外分岐が不要）。
    /// affine 参照実装との数値一致は実装時に別途確認済み（curve25519 系
    /// 実装で公知の手法）。
    fn add(&self, other: &EdwardsPoint) -> EdwardsPoint {
        let a = self.y.sub(&self.x).mul(&other.y.sub(&other.x));
        let b = self.y.add(&self.x).mul(&other.y.add(&other.x));
        let c = self.t.mul(&Fe::d2()).mul(&other.t);
        let d = self.z.add(&self.z).mul(&other.z);
        let e = b.sub(&a);
        let f = d.sub(&c);
        let g = d.add(&c);
        let h = b.add(&a);
        EdwardsPoint {
            x: e.mul(&f),
            y: g.mul(&h),
            z: f.mul(&g),
            t: e.mul(&h),
        }
    }

    /// 専用の doubling 式（`a=-1` 向け add-2008-hwcd 系。affine 参照実装との
    /// 数値一致は実装時に別途確認済み）。
    fn double(&self) -> EdwardsPoint {
        let a = self.x.square();
        let b = self.y.square();
        let c = self.z.square().add(&self.z.square());
        let d = a.neg();
        let xy = self.x.add(&self.y);
        let e = xy.square().sub(&a).sub(&b);
        let g = d.add(&b);
        let f = g.sub(&c);
        let h = d.sub(&b);
        EdwardsPoint {
            x: e.mul(&f),
            y: g.mul(&h),
            z: f.mul(&g),
            t: e.mul(&h),
        }
    }

    /// `choice`（0 または 1）に応じて `a` または `b` を定数時間で選ぶ
    /// （[`Fe::select`] を 4 座標へ適用するのみ。[`scalar_mul`] の
    /// double-and-add-always ladder が使う）。
    fn select(a: &EdwardsPoint, b: &EdwardsPoint, choice: u64) -> EdwardsPoint {
        EdwardsPoint {
            x: Fe::select(&a.x, &b.x, choice),
            y: Fe::select(&a.y, &b.y, choice),
            z: Fe::select(&a.z, &b.z, choice),
            t: Fe::select(&a.t, &b.t, choice),
        }
    }

    /// 4 座標すべてを best-effort でゼロ化する（[`Fe::zeroize`] を各座標へ
    /// 適用するのみ。[`scalar_mul`] のスクラッチ点が使う）。
    fn zeroize(&mut self) {
        self.x.zeroize();
        self.y.zeroize();
        self.z.zeroize();
        self.t.zeroize();
    }

    /// 圧縮エンコード（RFC 8032 §5.1.2）。y 座標の 32 バイトに x 座標の
    /// 偶奇を最上位ビットへ埋め込む。
    fn compress(&self) -> [u8; POINT_LEN] {
        let inv_z = self.z.invert();
        let x = self.x.mul(&inv_z);
        let y = self.y.mul(&inv_z);
        let mut out = y.to_bytes();
        // out の長さは POINT_LEN（32）固定であり末尾添字は定数。
        out[POINT_LEN - 1] |= x.is_negative() << 7;
        out
    }

    /// 圧縮エンコードからの点復元（RFC 8032 §5.1.3）。**公開値専用**
    /// （検証対象の署名 `R`・公開鍵 `A`）。分岐は入力（公開値）の形状にのみ
    /// 依存し、鍵材料そのものの値に依存する定数時間性は要求しない。
    fn decompress(bytes: &[u8; POINT_LEN]) -> Result<EdwardsPoint, Ed25519Error> {
        // bytes は固定長 32 バイト配列であり、末尾添字 31 はコンパイル時に
        // 範囲が確定する定数（[`super::field25519::load64_le`] と同じ方針）。
        let sign = bytes[POINT_LEN - 1] >> 7;

        // Fe::from_bytes は bit 255 を無条件でマスクするため、符号ビットを
        // 取り出した後に読む。正準性チェックのため、符号ビットを落とした
        // 元バイト列とも比較する。
        let y = Fe::from_bytes(bytes);
        let mut without_sign = *bytes;
        without_sign[POINT_LEN - 1] &= 0x7f;
        if y.to_bytes() != without_sign {
            // y が非正準（p 以上）。RFC 8032 §5.1.3 は正準表現のみ受理する。
            return Err(Ed25519Error::InvalidPublicKey);
        }

        let y2 = y.square();
        let u = y2.sub(&Fe::ONE);
        let v = Fe::d().mul(&y2).add(&Fe::ONE);

        let v2 = v.square();
        let v3 = v2.mul(&v);
        let v7 = v2.square().mul(&v3);
        let uv7 = u.mul(&v7);
        let uv7_p58 = uv7.pow_p58();
        let mut x = u.mul(&v3).mul(&uv7_p58);

        let check = v.mul(&x.square());
        if check.equals(&u) {
            // 候補がそのまま正しい平方根。
        } else if check.equals(&u.neg()) {
            x = x.mul(&Fe::sqrt_m1());
        } else {
            return Err(Ed25519Error::InvalidPublicKey);
        }

        if x.equals(&Fe::ZERO) && sign == 1 {
            // x=0 の点は符号ビット 0 の表現しか正準ではない。
            return Err(Ed25519Error::InvalidPublicKey);
        }
        if x.is_negative() != sign {
            x = x.neg();
        }

        Ok(EdwardsPoint {
            t: x.mul(&y),
            x,
            y,
            z: Fe::ONE,
        })
    }
}

/// 基点 x 座標（RFC 8032 由来の公開定数。LE 32 バイト）。
const BASE_X_BYTES: [u8; 32] = [
    0x1a, 0xd5, 0x25, 0x8f, 0x60, 0x2d, 0x56, 0xc9, 0xb2, 0xa7, 0x25, 0x95, 0x60, 0xc7, 0x2c, 0x69,
    0x5c, 0xdc, 0xd6, 0xfd, 0x31, 0xe2, 0xa4, 0xc0, 0xfe, 0x53, 0x6e, 0xcd, 0xd3, 0x36, 0x69, 0x21,
];
/// 基点 y 座標（`4/5 mod p`。RFC 8032 由来の公開定数。LE 32 バイト）。
const BASE_Y_BYTES: [u8; 32] = [
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
];

/// 定数時間の 256 回固定 double-and-add-always ladder（`k` は秘密値
/// かもしれない。クランプ済みスカラー `a`・nonce `r` の双方に使う）。
/// ビットの抽出は公開のループカウンタ `t` だけを添字に使い、テーブル
/// 参照は行わない（[`super::x25519`] の ladder と同じ設計方針）。
fn scalar_mul(k: &[u8; POINT_LEN], p: &EdwardsPoint) -> EdwardsPoint {
    let mut q = EdwardsPoint::IDENTITY;
    // 各ラウンドの候補点（`q.add(p)`）を上書きし続ける専用スクラッチ。
    // ループ終了後に best-effort でゼロ化し、最後に残った候補点の座標が
    // スタック上に居残る期間を最小化する（[`Fe::zeroize`] のドキュメント
    // コメントが明記するとおり `unsafe` を使わないため消去省略が起きない
    // 保証はなく、あくまで多層防御）。
    let mut candidate = EdwardsPoint::IDENTITY;
    for t in (0..256usize).rev() {
        q = q.double();
        // t は 0..256 の公開なループカウンタであり、k[t>>3] の添字
        // （0..32）はコンパイル時に範囲が確定する（k の長さは POINT_LEN
        // 固定）。ビット値そのものは秘密でも、添字自体は公開情報。
        let byte = k[t >> 3];
        let bit = ((byte >> (t & 7)) & 1) as u64;
        candidate = q.add(p);
        q = EdwardsPoint::select(&q, &candidate, bit);
    }
    candidate.zeroize();
    q
}

/// 固定基点 `[k]B` を計算する（[`scalar_mul`] の特殊化）。
fn scalar_mul_base(k: &[u8; POINT_LEN]) -> EdwardsPoint {
    scalar_mul(k, &EdwardsPoint::base())
}

// === mod L のスカラー演算 ===============================================

/// mod L（Ed25519 の群位数。`L = 2^252 +
/// 27742317777372353535851937790883648493`）のスカラー演算。SHA-512
/// 出力（64 バイト）の還元・`S = r + k*a mod L` の計算を担う。r・a は
/// 秘密値になり得るため定数時間（分岐・秘密添字・テーブル参照なし）。
/// S ≥ L の判定（[`is_canonical`]）は検証専用の公開値判定であり、
/// 定数時間性は要求しない。
mod scalar {
    /// L の 4×64bit リトルエンディアン limb 表現。
    const L: [u64; 4] = [
        0x5812_631a_5cf5_d3ed,
        0x14de_f9de_a2f7_9cd6,
        0x0000_0000_0000_0000,
        0x1000_0000_0000_0000,
    ];

    fn bytes_to_limbs(b: &[u8; 32]) -> [u64; 4] {
        let mut out = [0u64; 4];
        for (i, limb) in out.iter_mut().enumerate() {
            let mut v = 0u64;
            for j in 0..8 {
                // i・j はいずれもコンパイル時に範囲が確定する固定ループの
                // カウンタ（0..4・0..8）であり、添字 i*8+j は 0..32 に収まる。
                v |= (b[i * 8 + j] as u64) << (8 * j);
            }
            *limb = v;
        }
        out
    }

    fn limbs_to_bytes(limbs: &[u64; 4]) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, limb) in limbs.iter().enumerate() {
            let b = limb.to_le_bytes();
            for j in 0..8 {
                out[i * 8 + j] = b[j];
            }
        }
        out
    }

    /// `acc >= L` を判定し、真なら `acc -= L` する（分岐は使わず、ボローの
    /// マスクで条件付き減算する）。`acc` は mod-L 還元の中間値であり、
    /// `acc < 2L` の前提でのみ正しく動作する（呼び出し元が保証する）。
    fn conditional_sub_l(acc: &mut [u64; 4]) {
        let mut borrow: u128 = 0;
        let mut diff = [0u64; 4];
        for i in 0..4 {
            let sub = (acc[i] as u128)
                .wrapping_sub(L[i] as u128)
                .wrapping_sub(borrow);
            // sub は 128bit 上での結果。下位 64bit が差、上位ビットが
            // 借用の有無を表す（2 の補数表現）。
            diff[i] = sub as u64;
            borrow = (sub >> 64) & 1;
        }
        // borrow == 1 のとき acc < L だったので減算しない（元の acc を保つ）。
        // borrow == 0 のとき acc >= L だったので diff を採用する。
        let keep_diff_mask = 0u64.wrapping_sub((borrow == 0) as u64);
        for i in 0..4 {
            acc[i] = (acc[i] & !keep_diff_mask) | (diff[i] & keep_diff_mask);
        }
    }

    /// 512 ビット入力（SHA-512 出力）を mod L へ還元する（ビット単位
    /// Horner 法。上位ビットから `acc = 2*acc + bit` を計算し、都度
    /// `acc >= L` なら L を引く。512 回固定ループで秘密値に依存する
    /// 分岐・添字を持たない）。
    pub(super) fn reduce512(input: &[u8; 64]) -> [u8; 32] {
        let mut acc = [0u64; 4];
        for bit_index in (0..512usize).rev() {
            // bit_index は公開なループカウンタ（0..512）。byte_index は
            // 0..64 に収まるコンパイル時範囲確定の添字。
            let byte_index = bit_index >> 3;
            let bit = (input[byte_index] >> (bit_index & 7)) & 1;

            // acc を 1 ビット左シフトし、最下位へ新しいビットを詰める。
            let mut carry = bit as u64;
            for limb in acc.iter_mut() {
                let new_carry = *limb >> 63;
                *limb = (*limb << 1) | carry;
                carry = new_carry;
            }
            conditional_sub_l(&mut acc);
        }
        limbs_to_bytes(&acc)
    }

    /// `x + y`（両者とも `< L` の前提。和が `< 2L` に収まるため
    /// [`conditional_sub_l`] を 1 回適用するだけで `mod L` の範囲へ戻る）。
    fn add_mod_l(x: &[u64; 4], y: &[u64; 4]) -> [u64; 4] {
        let mut acc = [0u64; 4];
        let mut carry: u128 = 0;
        for i in 0..4 {
            let v = x[i] as u128 + y[i] as u128 + carry;
            acc[i] = v as u64;
            carry = v >> 64;
        }
        conditional_sub_l(&mut acc);
        acc
    }

    /// `choice`（0 または 1）に応じて `a` または `b` を定数時間で選ぶ
    /// （[`Fe::select`]・[`EdwardsPoint::select`] と同じマスク方式）。
    fn select_limbs(a: &[u64; 4], b: &[u64; 4], choice: u64) -> [u64; 4] {
        let mask = 0u64.wrapping_sub(choice);
        let mut out = [0u64; 4];
        for i in 0..4 {
            out[i] = a[i] ^ (mask & (a[i] ^ b[i]));
        }
        out
    }

    /// `S = (r + k*a) mod L`。
    ///
    /// `k`・`r` は呼び出し元（[`super::sign`]）が常に [`reduce512`] の
    /// 出力（`< L`）として渡す。`a` はクランプ済みスカラー（RFC 8032
    /// §5.1.5 のクランプにより `bit254` が常に立つため `a >= 2^254 > L`
    /// になり得、`< L` の前提を満たさない）ため、`a` は乗数の各ビットを
    /// 読むためだけに使い、mod L の演算対象にはしない。
    ///
    /// `k*a` を素朴な 4×4 limb 乗算で 512bit へ展開すると各桁の部分積の
    /// 総和が `u128` の範囲（2^128）を超えうる（実装時にオーバーフロー
    /// panic で検出）ため採らない。代わりに `a` の各ビットを最上位から
    /// 読みながら `acc = 2*acc mod L`・ビットが立っていれば
    /// `acc += k mod L` を繰り返す二進乗算（Horner 法。`k*a mod L` を
    /// 教科書的乗算より小さい中間値のみで計算する）を使う。`acc` は
    /// ループ全体を通して常に `< L` に保たれるため [`add_mod_l`] の
    /// 前提を満たし、`a` の値そのもの（秘密値）に依存する分岐は
    /// 使わない（`select_limbs` によるマスク選択のみ）。256 回固定
    /// ループで秘密値に依存する添字も持たない。
    pub(super) fn mul_add(k: &[u8; 32], a: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
        let kl = bytes_to_limbs(k);
        let al = bytes_to_limbs(a);
        let rl = bytes_to_limbs(r);

        let mut acc = [0u64; 4];
        for bit_index in (0..256usize).rev() {
            acc = add_mod_l(&acc, &acc);
            // bit_index は 0..256 の公開なループカウンタ。limb_index
            // （0..4）・limb 内シフト量（0..63）はいずれもこの範囲から
            // 導かれるコンパイル時範囲確定の添字。
            let limb_index = bit_index >> 6;
            let bit = (al[limb_index] >> (bit_index & 63)) & 1;
            let plus_k = add_mod_l(&acc, &kl);
            acc = select_limbs(&acc, &plus_k, bit);
        }
        limbs_to_bytes(&add_mod_l(&acc, &rl))
    }

    /// `S < L` の判定（RFC 8032 §5.1.7 の署名検証が要求する正準性検査）。
    /// **検証専用の公開値判定**であり、分岐があってもよい。
    pub(super) fn is_canonical(s: &[u8; 32]) -> bool {
        let limbs = bytes_to_limbs(s);
        for i in (0..4).rev() {
            if limbs[i] != L[i] {
                return limbs[i] < L[i];
            }
        }
        false // s == L は非正準（S < L を満たさない）。
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn le32(v: u128) -> [u8; 32] {
            let mut out = [0u8; 32];
            out[..16].copy_from_slice(&v.to_le_bytes());
            out
        }

        #[test]
        fn reduce512_of_zero_is_zero() {
            assert_eq!(reduce512(&[0u8; 64]), [0u8; 32]);
        }

        #[test]
        fn reduce512_of_l_is_zero() {
            let mut input = [0u8; 64];
            input[..32].copy_from_slice(&limbs_to_bytes(&L));
            assert_eq!(reduce512(&input), [0u8; 32]);
        }

        #[test]
        fn reduce512_of_l_minus_one_is_l_minus_one() {
            let mut l_minus_1 = L;
            l_minus_1[0] -= 1;
            let expected = limbs_to_bytes(&l_minus_1);
            let mut input = [0u8; 64];
            input[..32].copy_from_slice(&expected);
            assert_eq!(reduce512(&input), expected);
        }

        #[test]
        fn reduce512_of_two_l_is_zero() {
            // 2*L（limb 演算で直接計算。桁上げは 4 limb 内に収まる）。
            let mut acc = [0u64; 4];
            let mut carry = 0u128;
            for i in 0..4 {
                let v = (L[i] as u128) * 2 + carry;
                acc[i] = v as u64;
                carry = v >> 64;
            }
            let mut input = [0u8; 64];
            input[..32].copy_from_slice(&limbs_to_bytes(&acc));
            assert_eq!(reduce512(&input), [0u8; 32]);
        }

        #[test]
        fn reduce512_of_max_512_bit_value() {
            let input = [0xffu8; 64];
            // python3 で `((2**512)-1) % L` を計算した固定値（LE hex）。
            let expected_hex = "000f9c44e31106a447938568a71b0ed065bef517d273ecce3d9a307c1b419903";
            let expected_hex = &expected_hex[..64];
            let mut expected = [0u8; 32];
            for i in 0..32 {
                expected[i] =
                    u8::from_str_radix(&expected_hex[i * 2..i * 2 + 2], 16).expect("valid hex");
            }
            assert_eq!(reduce512(&input), expected);
        }

        #[test]
        fn mul_add_small_values() {
            let k = le32(2);
            let a = le32(3);
            let r = le32(4);
            assert_eq!(mul_add(&k, &a, &r), le32(10));
        }

        #[test]
        fn mul_add_l_minus_one_squared_is_one() {
            let mut l_minus_1 = L;
            l_minus_1[0] -= 1;
            let l_minus_1_bytes = limbs_to_bytes(&l_minus_1);
            let zero = [0u8; 32];
            let result = mul_add(&l_minus_1_bytes, &l_minus_1_bytes, &zero);
            assert_eq!(result, le32(1));
        }

        #[test]
        fn is_canonical_rejects_l_and_above() {
            let l_bytes = limbs_to_bytes(&L);
            assert!(!is_canonical(&l_bytes));
            let mut l_minus_1 = L;
            l_minus_1[0] -= 1;
            assert!(is_canonical(&limbs_to_bytes(&l_minus_1)));
            assert!(is_canonical(&[0u8; 32]));
        }
    }
}

// === 鍵展開・署名生成・検証 =============================================

/// Ed25519 の秘密鍵（展開済み）。`Clone`／`Copy` は導出しない。`Debug` は
/// 内容を伏せ、`Drop` で `scalar`・`prefix` を best-effort ゼロ化する
/// （[`super::pkcs8::Ed25519Seed`] と同じ設計判断）。seed 自体は展開後は
/// 不要なため保持しない（ゼロ化すべき秘密バイト量を最小にする）。
pub struct SigningKey {
    /// クランプ済みスカラー `a`（RFC 8032 §5.1.5）。
    scalar: [u8; POINT_LEN],
    /// nonce 導出用の prefix（`SHA-512(seed)` の後半 32 バイト）。
    prefix: [u8; POINT_LEN],
    /// 公開鍵 `A = compress([a]B)`（秘密ではないが、便宜上ここに保持する）。
    public: [u8; POINT_LEN],
}

impl SigningKey {
    /// [`super::pkcs8::Ed25519Seed`]（PKCS#8 由来。正規の入口）から鍵を展開する。
    pub fn from_seed(seed: &Ed25519Seed) -> SigningKey {
        SigningKey::from_seed_bytes(*seed.as_bytes())
    }

    /// 32 バイトの生 seed から鍵を展開する。結合テスト・RFC 8032 §7.1 の
    /// 公開テストベクタ検証のための入口（[`super::x25519::EphemeralSecret::
    /// from_bytes`] と同じ扱い）。
    pub fn from_seed_bytes(mut seed: [u8; POINT_LEN]) -> SigningKey {
        let mut h = digest(&seed);
        zeroize(&mut seed);

        // h[0..32] をクランプする（RFC 8032 §5.1.5）。
        let mut scalar = [0u8; POINT_LEN];
        scalar.copy_from_slice(&h[..POINT_LEN]);
        scalar[0] &= 248;
        scalar[31] &= 63;
        scalar[31] |= 64;

        let mut prefix = [0u8; POINT_LEN];
        prefix.copy_from_slice(&h[POINT_LEN..]);

        let public = scalar_mul_base(&scalar).compress();

        zeroize(&mut h);

        SigningKey {
            scalar,
            prefix,
            public,
        }
    }

    /// 公開鍵 `A`。[`super::x509::ServerCertificateChain::from_der_chain`]
    /// の `expected_leaf_public_key` へ渡す値（起動経路の接続は #967）。
    pub fn public_key(&self) -> [u8; POINT_LEN] {
        self.public
    }

    /// `sign(M) = R || S`（RFC 8032 §5.1.6）。
    ///
    /// 1. `r = SHA-512(prefix || M) mod L`
    /// 2. `R = compress([r]B)`
    /// 3. `k = SHA-512(R || A || M) mod L`
    /// 4. `S = (r + k*a) mod L`
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        let mut hasher = Sha512::new();
        hasher.update(&self.prefix);
        hasher.update(message);
        let mut r_hash = hasher.finalize();
        let mut r = scalar::reduce512(&r_hash);
        zeroize(&mut r_hash);

        let r_point = scalar_mul_base(&r);
        let r_compressed = r_point.compress();

        let mut hasher = Sha512::new();
        hasher.update(&r_compressed);
        hasher.update(&self.public);
        hasher.update(message);
        let mut k_hash = hasher.finalize();
        let k = scalar::reduce512(&k_hash);
        zeroize(&mut k_hash);

        let s = scalar::mul_add(&k, &self.scalar, &r);
        zeroize(&mut r);

        let mut out = [0u8; SIGNATURE_LEN];
        out[..POINT_LEN].copy_from_slice(&r_compressed);
        out[POINT_LEN..].copy_from_slice(&s);
        out
    }

    /// 内部の秘密バイト（`scalar`・`prefix`）を best-effort でゼロ化する
    /// （テストからも呼べる形で [`Drop`] と同じ処理を公開する）。
    fn wipe(&mut self) {
        zeroize(&mut self.scalar);
        zeroize(&mut self.prefix);
    }

    #[cfg(test)]
    pub(crate) fn scalar_for_test(&self) -> &[u8; POINT_LEN] {
        &self.scalar
    }

    #[cfg(test)]
    pub(crate) fn prefix_for_test(&self) -> &[u8; POINT_LEN] {
        &self.prefix
    }
}

impl Drop for SigningKey {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("scalar", &"<redacted>")
            .field("prefix", &"<redacted>")
            .field("public", &"<redacted>")
            .finish()
    }
}

/// 署名検証（RFC 8032 §5.1.7）。自己整合性テスト用（サーバーはクライアント
/// 証明書を扱わないため、production 経路での検証呼び出し元は無い）。
/// すべて公開値（`public_key`・`message`・`signature`）を扱うため分岐が
/// あってもよい。
///
/// `[S]B == R + [k]A`（cofactor を掛けない判定式。RFC 8032 が許容し、
/// §7.1 のベクタも通る）を、圧縮 → デコードの往復無しに拡張座標の
/// 交差乗算（`X1*Z2 == X2*Z1 && Y1*Z2 == Y2*Z1`）で判定する。
pub fn verify(
    public_key: &[u8; POINT_LEN],
    message: &[u8],
    signature: &[u8],
) -> Result<(), Ed25519Error> {
    if signature.len() != SIGNATURE_LEN {
        return Err(Ed25519Error::InvalidSignatureLength);
    }
    // signature.len() == SIGNATURE_LEN（64）を確認済みなので
    // get(..32)/get(32..) は必ず Some を返すが、untrusted 入力（署名は
    // ネットワーク越しの検証対象になり得る値）のため unwrap ではなく
    // ok_or で明示的に扱う。
    let r_bytes: &[u8; POINT_LEN] = signature
        .get(..POINT_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or(Ed25519Error::InvalidSignatureLength)?;
    let s_bytes: &[u8; POINT_LEN] = signature
        .get(POINT_LEN..)
        .and_then(|s| s.try_into().ok())
        .ok_or(Ed25519Error::InvalidSignatureLength)?;

    if !scalar::is_canonical(s_bytes) {
        return Err(Ed25519Error::NonCanonicalScalar);
    }

    let a = EdwardsPoint::decompress(public_key).map_err(|_| Ed25519Error::InvalidPublicKey)?;
    let r = EdwardsPoint::decompress(r_bytes).map_err(|_| Ed25519Error::InvalidSignaturePoint)?;

    let mut hasher = Sha512::new();
    hasher.update(r_bytes);
    hasher.update(public_key);
    hasher.update(message);
    let k_hash = hasher.finalize();
    let k = scalar::reduce512(&k_hash);

    let sb = scalar_mul(s_bytes, &EdwardsPoint::base());
    let ka = scalar_mul(&k, &a);
    let rka = r.add(&ka);

    // 交差乗算による射影座標の等価性判定（affine 座標へ変換する逆元計算を
    // 避ける）。
    let lhs_x = sb.x.mul(&rka.z);
    let rhs_x = rka.x.mul(&sb.z);
    let lhs_y = sb.y.mul(&rka.z);
    let rhs_y = rka.y.mul(&sb.z);

    if lhs_x.equals(&rhs_x) && lhs_y.equals(&rhs_y) {
        Ok(())
    } else {
        Err(Ed25519Error::VerificationFailed)
    }
}

/// Ed25519 の鍵展開・署名・検証で起こり得る失敗。受信バイト列は一切
/// 保持しない（ログ・エラー経由の漏えい防止。`Display` も固定文字列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ed25519Error {
    /// 署名長が 64 バイトでない。
    InvalidSignatureLength,
    /// 署名の `S` が mod L で非正準（`S >= L`。RFC 8032 §5.1.7）。
    NonCanonicalScalar,
    /// 公開鍵の圧縮エンコードが不正（非正準・平方根が存在しない・
    /// x=0 かつ符号ビットが 1 のいずれか）。
    InvalidPublicKey,
    /// 署名の `R` の圧縮エンコードが不正（`InvalidPublicKey` と同種の
    /// 判定を R に対して行った結果）。
    InvalidSignaturePoint,
    /// `[S]B == R + [k]A` が成立しない（署名が無効）。
    VerificationFailed,
}

impl Ed25519Error {
    /// alert の実送出は #965 の担当。[`super::finished::FinishedError::
    /// alert_description`] と同じ流儀で定型コードへ写像する。長さ不正は
    /// `decode_error`、検証・エンコードの失敗は RFC 8446 §4.4.3 に合わせて
    /// `decrypt_error` へ写像する
    /// （[`super::certificate_verify::CertificateVerifyError`] が本判定を
    /// 再利用する）。
    pub fn alert_description(&self) -> super::record::AlertDescription {
        match self {
            Ed25519Error::InvalidSignatureLength => super::record::AlertDescription::DecodeError,
            Ed25519Error::NonCanonicalScalar
            | Ed25519Error::InvalidPublicKey
            | Ed25519Error::InvalidSignaturePoint
            | Ed25519Error::VerificationFailed => super::record::AlertDescription::DecryptError,
        }
    }
}

impl fmt::Display for Ed25519Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Ed25519Error::InvalidSignatureLength => "Ed25519 signature must be 64 bytes",
            Ed25519Error::NonCanonicalScalar => "Ed25519 signature S is not canonical mod L",
            Ed25519Error::InvalidPublicKey => "Ed25519 public key point is invalid",
            Ed25519Error::InvalidSignaturePoint => "Ed25519 signature R point is invalid",
            Ed25519Error::VerificationFailed => "Ed25519 signature verification failed",
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for Ed25519Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(s: &str) -> [u8; 32] {
        assert_eq!(s.len(), 64);
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex in fixture");
        }
        out
    }

    fn hex_vec(s: &str) -> Vec<u8> {
        assert_eq!(s.len() % 2, 0);
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex in fixture"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // RFC 8032 §7.1 の公開テストベクタ（IETF 公開文書。独立実装の
    // python 参照実装でも事前に一致を確認済み）。
    struct Vector {
        sk: &'static str,
        pk: &'static str,
        msg: &'static str,
        sig: &'static str,
    }

    const TEST_1: Vector = Vector {
        sk: "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        pk: "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        msg: "",
        sig: "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    };
    const TEST_2: Vector = Vector {
        sk: "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        pk: "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        msg: "72",
        sig: "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    };
    const TEST_3: Vector = Vector {
        sk: "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        pk: "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        msg: "af82",
        sig: "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    };
    // メッセージは SHA-512("abc")（RFC 8032 §7.1 TEST SHA(abc)）。
    const TEST_SHA_ABC: Vector = Vector {
        sk: "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
        pk: "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
        msg: "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        sig: "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
    };

    fn check_vector(v: &Vector) {
        let key = SigningKey::from_seed_bytes(hex32(v.sk));
        assert_eq!(hex(&key.public_key()), v.pk, "public key mismatch");

        let msg = hex_vec(v.msg);
        let sig = key.sign(&msg);
        assert_eq!(hex(&sig), v.sig, "signature mismatch");

        assert!(verify(&key.public_key(), &msg, &sig).is_ok());
    }

    #[test]
    fn rfc8032_test_vector_1_empty_message() {
        check_vector(&TEST_1);
    }

    #[test]
    fn rfc8032_test_vector_2() {
        check_vector(&TEST_2);
    }

    #[test]
    fn rfc8032_test_vector_3() {
        check_vector(&TEST_3);
    }

    #[test]
    fn rfc8032_test_vector_sha_abc() {
        check_vector(&TEST_SHA_ABC);
    }

    #[test]
    fn signing_is_deterministic() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = b"same message";
        let sig1 = key.sign(msg);
        let sig2 = key.sign(msg);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn bit_flip_in_signature_r_is_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = hex_vec(TEST_1.msg);
        let mut sig = key.sign(&msg);
        sig[0] ^= 0x01;
        // R の 1 ビット反転は、非正準エンコード化（InvalidSignaturePoint）と
        // 平方根が存在しなくなる場合の両方があり得るため、失敗すること
        // だけを確認する（具体的な失敗種別までは固定しない）。
        assert!(verify(&key.public_key(), &msg, &sig).is_err());
    }

    #[test]
    fn bit_flip_in_signature_s_is_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = hex_vec(TEST_1.msg);
        let mut sig = key.sign(&msg);
        sig[63] ^= 0x01;
        assert!(verify(&key.public_key(), &msg, &sig).is_err());
    }

    #[test]
    fn bit_flip_in_message_is_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_2.sk));
        let mut msg = hex_vec(TEST_2.msg);
        let sig = key.sign(&msg);
        msg[0] ^= 0x01;
        assert!(verify(&key.public_key(), &msg, &sig).is_err());
    }

    #[test]
    fn verification_with_wrong_public_key_is_rejected() {
        let key1 = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let key2 = SigningKey::from_seed_bytes(hex32(TEST_2.sk));
        let msg = hex_vec(TEST_1.msg);
        let sig = key1.sign(&msg);
        assert!(verify(&key2.public_key(), &msg, &sig).is_err());
    }

    #[test]
    fn signature_length_0_63_65_are_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = hex_vec(TEST_1.msg);
        assert_eq!(
            verify(&key.public_key(), &msg, &[]),
            Err(Ed25519Error::InvalidSignatureLength)
        );
        assert_eq!(
            verify(&key.public_key(), &msg, &[0u8; 63]),
            Err(Ed25519Error::InvalidSignatureLength)
        );
        assert_eq!(
            verify(&key.public_key(), &msg, &[0u8; 65]),
            Err(Ed25519Error::InvalidSignatureLength)
        );
    }

    #[test]
    fn signature_with_s_greater_or_equal_l_is_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = hex_vec(TEST_1.msg);
        let mut sig = key.sign(&msg);
        // S を L（scalar::L の LE バイト列）へ書き換える。
        let l_bytes: [u8; 32] =
            hex32("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010");
        sig[32..].copy_from_slice(&l_bytes);
        assert_eq!(
            verify(&key.public_key(), &msg, &sig),
            Err(Ed25519Error::NonCanonicalScalar)
        );
    }

    #[test]
    fn non_canonical_r_encoding_is_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = hex_vec(TEST_1.msg);
        let mut sig = key.sign(&msg);
        // R を p（field25519 のテストが使うのと同じ非正準値）へ書き換える。
        let p_bytes: [u8; 32] =
            hex32("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f");
        sig[..32].copy_from_slice(&p_bytes);
        assert_eq!(
            verify(&key.public_key(), &msg, &sig),
            Err(Ed25519Error::InvalidSignaturePoint)
        );
    }

    #[test]
    fn public_key_with_no_square_root_is_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = hex_vec(TEST_1.msg);
        let sig = key.sign(&msg);
        // y=2（多倍長整数計算で事前に「u/v が平方非剰余になる」ことを
        // 確認済みの値。すなわちこの y に対応する x は存在しない）。
        let mut bad = [0u8; 32];
        bad[0] = 2;
        assert!(verify(&bad, &msg, &sig).is_err());
    }

    #[test]
    fn public_key_with_x_zero_and_sign_bit_set_is_rejected() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let msg = hex_vec(TEST_1.msg);
        let sig = key.sign(&msg);
        // y=1（x=0 の点）に符号ビット 1 を立てた非正準表現。
        let mut bad = [0u8; 32];
        bad[0] = 1;
        bad[31] = 0x80;
        assert!(verify(&bad, &msg, &sig).is_err());
    }

    #[test]
    fn signing_key_debug_does_not_expose_secret_bytes() {
        let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        let debug = format!("{key:?}");
        assert!(!debug.contains(TEST_1.sk));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn drop_zeroizes_scalar_and_prefix() {
        // wipe() を直接呼び、Drop と同じ経路がゼロ化することを確認する
        // （Drop 自体はスコープ終了時に暗黙に呼ばれるため、ここでは
        // 同じ処理を行う wipe() を明示的に呼ぶ形でテストする）。
        let mut key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
        key.wipe();
        assert_eq!(key.scalar_for_test(), &[0u8; 32]);
        assert_eq!(key.prefix_for_test(), &[0u8; 32]);
    }

    #[test]
    fn base_point_round_trips_through_compress_and_decompress() {
        let base = EdwardsPoint::base();
        let compressed = base.compress();
        let decoded = EdwardsPoint::decompress(&compressed).expect("base point must decompress");
        assert_eq!(decoded.compress(), compressed);
    }

    #[test]
    fn identity_compresses_to_expected_bytes() {
        let mut expected = [0u8; 32];
        expected[0] = 1;
        assert_eq!(EdwardsPoint::IDENTITY.compress(), expected);
    }

    #[test]
    fn scalar_l_times_base_is_identity() {
        // scalar::L の LE バイト列（field25519 の p のテストと同様、
        // python の多倍長整数から算出した固定値）。
        let l_bytes: [u8; 32] =
            hex32("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010");
        let result = scalar_mul_base(&l_bytes);
        assert_eq!(result.compress(), EdwardsPoint::IDENTITY.compress());
    }
}
