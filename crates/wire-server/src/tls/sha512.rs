//! 自作 SHA-512（FIPS 180-4）実装。Ed25519 署名生成・検証（Issue #961・
//! TASK-228・WIRE-9・HTTP-10 ポインタ）の下請けとして使う。秘密鍵の展開
//! （`SHA-512(seed)`）と署名計算（`SHA-512(prefix || M)`・`SHA-512(R || A || M)`）
//! がこのモジュールに依存する。
//!
//! `engine::crypto::sha256`（Issue #399 のストリーミング化方式）と同じ構成
//! （16 語ローリングメッセージスケジュール・固定長スタックバッファ・
//! `wrapping_*` 演算）を 64 ビット語向けに再構成したもの。engine クレート
//! ではなく `wire-server::tls` 配下に置くのは、TLS 1.3 自作実装（親 Issue
//! #941）の一部として Ed25519 専用に導入するため（対象暗号スイートは
//! `TLS_AES_128_GCM_SHA256` のみで、TLS のトランスクリプトハッシュ・HKDF は
//! 従来どおり SHA-256（[`super::hkdf`]）を使う。SHA-512 は署名アルゴリズム
//! 側の要求）。
//!
//! 対象外: SHA-384・SHA-512/256・SHA-512/224・HMAC-SHA-512（採用する暗号
//! スイートが要求しないため）。
//!
//! 依存追加なし（`.claude/rules/dependency-policy.md`）。`unsafe` は使わない。
//!
//! 定数時間性: 秘密値に依存する分岐・配列添字は無い。[`K`] の添字はラウンド
//! カウンタ（公開値）であり、`absorb`／`finalize` の分岐は公開情報である
//! 入力長にのみ依存する。ローテート・XOR・AND・`wrapping_add` だけで構成し、
//! テーブル参照による置換は使わない。
//!
//! ゼロ化の限界: [`Drop`] で内部状態を best-effort にゼロ化するが、
//! `write_volatile` 等の `unsafe` を使わないため、最適化により消去が
//! 省略されない保証はない（[`super::hkdf::Secret32`]・[`super::aes::Aes128`]
//! と同じ限界）。

use std::fmt;

/// ダイジェスト長（バイト）。
pub const DIGEST_LEN: usize = 64;
/// 処理ブロック長（バイト）。
pub const BLOCK_LEN: usize = 128;

const H0: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// `[u64; N]` を Drop 時に best-effort でゼロ化する（[`super::hkdf::zeroize`]
/// は `&mut [u8]` 専用のため、64 ビット語向けに別途用意する）。
fn zeroize_u64(words: &mut [u64]) {
    for w in words.iter_mut() {
        *w = 0;
    }
    std::hint::black_box(&*words);
}

/// 1 ブロック（128 バイト）ぶんの圧縮関数（FIPS 180-4 §6.4.2／RFC 6234 §6.4）。
/// メッセージスケジュールは 80 語配列ではなく 16 語のローリングバッファ
/// （`w[t & 15]`）で保持する（`engine::crypto::sha256::compress` と同じ設計。
/// Issue #399 のストリーミング化方式を 64 ビット語へ再構成したもの）。
fn compress(state: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0u64; 16];
    for (i, word) in block.as_chunks::<8>().0.iter().enumerate() {
        if let Some(slot) = w.get_mut(i) {
            *slot = u64::from_be_bytes(*word);
        }
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *state;

    for t in 0..80usize {
        let idx = t & 15;
        if t >= 16 {
            let w15 = w[(t - 15) & 15];
            let w2 = w[(t - 2) & 15];
            let s0 = w15.rotate_right(1) ^ w15.rotate_right(8) ^ (w15 >> 7);
            let s1 = w2.rotate_right(19) ^ w2.rotate_right(61) ^ (w2 >> 6);
            let prev16 = w[idx];
            w[idx] = prev16
                .wrapping_add(s0)
                .wrapping_add(w[(t - 7) & 15])
                .wrapping_add(s1);
        }

        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ ((!e) & g);
        let k = K.get(t).copied().unwrap_or(0);
        let temp1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(k)
            .wrapping_add(w[idx]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(hh);

    // w は入力バイト列由来の中間値を保持し続けるため、圧縮関数を抜ける前に
    // best-effort でゼロ化する（`absorb` のスタックコピー排除と対になる方針。
    // ゼロ化の限界はモジュール doc を参照）。
    zeroize_u64(&mut w);
}

/// ストリーミング更新型の SHA-512 状態。Ed25519（#961）の秘密鍵展開・
/// 署名計算がここから直接 [`Sha512::update`] を呼ぶ想定。
///
/// `Clone` は実装しない。Ed25519 は状態の複製を必要とせず、[`Secret32`]・
/// [`super::x25519::SharedSecret`] と同じく「不用意な複製を防ぐ」方針を
/// 揃えるため。
///
/// [`Drop`] を実装しているため [`finalize`](Sha512::finalize) はフィールドを
/// move できず、`Copy` な `state` 等を参照経由で読み出したうえで関数を抜け、
/// その時点で Drop が走って内部状態がゼロ化される（戻り値のダイジェスト
/// 自体のゼロ化は呼び出し側の責務）。
pub struct Sha512 {
    state: [u64; 8],
    /// 128 バイト未満の未処理端数（`buffered` バイトぶんのみ有効）。
    buffer: [u8; 128],
    buffered: usize,
    /// 入力バイト総数。`finalize` でビット長（`wrapping_mul(8)`）へ変換する。
    /// SHA-512 は `< 2^128` ビットのメッセージを扱うため `u128` で保持する。
    total_len: u128,
}

impl Sha512 {
    /// 初期ハッシュ値（`H0`）で状態を初期化した空のハッシャーを作る。
    pub fn new() -> Self {
        Sha512 {
            state: H0,
            buffer: [0u8; 128],
            buffered: 0,
            total_len: 0,
        }
    }

    /// `total_len` を増やさずにバイト列をブロックバッファへ吸収する（`update` と
    /// `finalize` のパディング処理が共有する内部処理）。
    fn absorb(&mut self, mut data: &[u8]) {
        if self.buffered > 0 {
            let need = BLOCK_LEN - self.buffered;
            let take = need.min(data.len());
            if let Some(slot) = self.buffer.get_mut(self.buffered..self.buffered + take) {
                if let Some(src) = data.get(..take) {
                    slot.copy_from_slice(src);
                }
            }
            self.buffered += take;
            data = data.get(take..).unwrap_or(&[]);
            if self.buffered == BLOCK_LEN {
                compress(&mut self.state, &self.buffer);
                self.buffered = 0;
            }
        }

        let (chunks, remainder) = data.as_chunks::<BLOCK_LEN>();
        for chunk in chunks {
            compress(&mut self.state, chunk);
        }

        if !remainder.is_empty() {
            if let Some(slot) = self.buffer.get_mut(..remainder.len()) {
                slot.copy_from_slice(remainder);
            }
            self.buffered = remainder.len();
        }
    }

    /// 任意長のバイト列をハッシュ計算へ順次取り込む（複数回呼び出し可）。
    pub fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u128);
        self.absorb(data);
    }

    /// FIPS 180-4 §5.1.2 のパディング（`0x80` 1 バイト → 零埋め → 16 バイト BE
    /// ビット長）をブロックバッファ経由で適用してからダイジェストを取り出す。
    pub fn finalize(mut self) -> [u8; DIGEST_LEN] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.absorb(&[0x80]);

        const ZEROS: [u8; BLOCK_LEN] = [0u8; BLOCK_LEN];
        let zero_pad = if self.buffered <= 112 {
            112 - self.buffered
        } else {
            112 + BLOCK_LEN - self.buffered
        };
        if let Some(zeros) = ZEROS.get(..zero_pad) {
            self.absorb(zeros);
        }
        self.absorb(&bit_len.to_be_bytes());

        let mut out = [0u8; DIGEST_LEN];
        for (i, word) in self.state.iter().enumerate() {
            let bytes = word.to_be_bytes();
            let start = i * 8;
            if let Some(slot) = out.get_mut(start..start + 8) {
                slot.copy_from_slice(&bytes);
            }
        }
        out
    }

    /// ラウンド鍵と同じく Drop から呼ぶゼロ化本体（ゼロ化の限界は module doc
    /// を参照。`unsafe` を使わないため最適化での省略は防げない）。
    fn wipe(&mut self) {
        zeroize_u64(&mut self.state);
        super::hkdf::zeroize(&mut self.buffer);
        std::hint::black_box(&self.buffer);
        self.buffered = 0;
        self.total_len = 0;
    }

    #[cfg(test)]
    fn state_bytes(&self) -> [u64; 8] {
        self.state
    }

    #[cfg(test)]
    fn buffer_bytes(&self) -> [u8; 128] {
        self.buffer
    }
}

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Sha512 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sha512")
            .field("state", &"<redacted>")
            .finish()
    }
}

impl Drop for Sha512 {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// 一括ハッシュのヘルパー（Ed25519〔#961〕の秘密鍵展開・署名計算・テストから
/// 使う）。戻り値のゼロ化は呼び出し側の責務。
pub fn digest(input: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hasher = Sha512::new();
    hasher.update(input);
    hasher.finalize()
}

/// 旧実装相当（バッチ全体を `Vec` へ連結してからパディングする一括処理版）の
/// 参照実装。ストリーミング版との等価性テストにのみ使う
/// （`engine::crypto::sha256::digest_reference` と同型）。
#[cfg(test)]
fn digest_reference(input: &[u8]) -> [u8; DIGEST_LEN] {
    fn pad(input: &[u8]) -> Vec<u8> {
        let bit_len = (input.len() as u128).wrapping_mul(8);
        let mut msg = input.to_vec();
        msg.push(0x80);
        while msg.len() % BLOCK_LEN != 112 {
            msg.push(0x00);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        msg
    }

    let padded = pad(input);
    let mut state = H0;
    for chunk in padded.as_chunks::<BLOCK_LEN>().0 {
        compress(&mut state, chunk);
    }

    let mut out = [0u8; DIGEST_LEN];
    for (i, word) in state.iter().enumerate() {
        let bytes = word.to_be_bytes();
        let start = i * 8;
        if let Some(slot) = out.get_mut(start..start + 8) {
            slot.copy_from_slice(&bytes);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // 以下 4 件は RFC 6234 §8.5（TEST1／TEST2_2／TEST3）・NIST FIPS 180-4
    // Examples の SHA-512 出力と一致することを固定する（公開文書からの
    // 転記。private spec 由来ではない）。

    #[test]
    fn digest_matches_fips_test_vector_empty_input() {
        let d = digest(b"");
        assert_eq!(
            hex(&d),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
    }

    #[test]
    fn digest_matches_fips_test_vector_abc() {
        let d = digest(b"abc");
        assert_eq!(
            hex(&d),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn digest_matches_rfc6234_test_vector_two_blocks() {
        let input = concat!(
            "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn",
            "hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
        );
        let d = digest(input.as_bytes());
        assert_eq!(
            hex(&d),
            "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
        );
    }

    #[test]
    fn digest_matches_nist_million_a_vector() {
        let input = vec![b'a'; 1_000_000];
        let d = digest(&input);
        assert_eq!(
            hex(&d),
            "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
        );
    }

    #[test]
    fn digest_streaming_matches_reference_for_boundary_lengths() {
        for len in [
            0usize, 1, 111, 112, 113, 127, 128, 129, 239, 240, 255, 256, 257, 1000,
        ] {
            let input: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
            assert_eq!(
                digest(&input),
                digest_reference(&input),
                "mismatch at len={len}"
            );
        }
    }

    #[test]
    fn digest_streaming_split_update_matches_one_shot_for_various_chunk_sizes() {
        let input: Vec<u8> = (0..300).map(|i| (i % 256) as u8).collect();
        let expected = digest_reference(&input);
        for chunk_size in [1usize, 3, 7, 16, 64, 111, 112, 127, 128, 129, 200] {
            let mut hasher = Sha512::new();
            for chunk in input.chunks(chunk_size) {
                hasher.update(chunk);
            }
            assert_eq!(
                hasher.finalize(),
                expected,
                "mismatch at chunk_size={chunk_size}"
            );
        }
    }

    #[test]
    fn digest_streaming_split_at_boundary_lengths_matches_one_shot() {
        // 111/112/127/128 バイトの入力を、あらゆる分割点で 2 回の `update` に
        // 分けても一括の結果と一致することを確認する（1 ブロックに収まるか
        // 2 ブロックになるかの境界を網羅する）。
        for len in [111usize, 112, 127, 128] {
            let input: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
            let expected = digest_reference(&input);
            for split in 0..=len {
                let mut hasher = Sha512::new();
                hasher.update(&input[..split]);
                hasher.update(&input[split..]);
                assert_eq!(
                    hasher.finalize(),
                    expected,
                    "mismatch at len={len}, split={split}"
                );
            }
        }
    }

    #[test]
    fn wipe_zeroes_all_state_and_buffer_bytes() {
        let mut hasher = Sha512::new();
        hasher.update(b"some secret-shaped input for wipe test");
        assert_ne!(hasher.state_bytes(), [0u64; 8]);
        assert_ne!(hasher.buffer_bytes(), [0u8; 128]);

        hasher.wipe();

        assert_eq!(hasher.state_bytes(), [0u64; 8]);
        assert_eq!(hasher.buffer_bytes(), [0u8; 128]);
        assert_eq!(hasher.buffered, 0);
        assert_eq!(hasher.total_len, 0);
    }

    #[test]
    fn debug_output_does_not_leak_internal_state() {
        let mut hasher = Sha512::new();
        hasher.update(b"secret");
        let debug_str = format!("{hasher:?}");
        assert!(debug_str.contains("redacted"));
        assert!(!debug_str.contains("secret"));
    }
}
