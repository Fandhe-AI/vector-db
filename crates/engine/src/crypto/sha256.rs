//! 自作 SHA-256（FIPS 180-4）実装。
//!
//! 元は `recovery::content_hash`（TASK-101・RECOVER-10）の private 実装
//! だったものを、`wire-server` の SCRAM-SHA-256 認証（Issue #940・WIRE-18・
//! TASK-222）が HMAC-SHA-256／PBKDF2 の下請けとして必要としたため、
//! engine クレートの公開 API（`engine::crypto::sha256`）へ移設した
//! （Issue #399 のストリーミング化・ローリングメッセージスケジュールは
//! そのまま。出力バイト列・`recovery::content_hash` の既存 `ContentHash`
//! 値はすべて不変であることを移設前後の等価性テストで確認する）。
//!
//! 依存追加なし（`.claude/rules/dependency-policy.md`）。`unsafe` は使わず、
//! 固定サイズ配列・`wrapping_*` 演算（FIPS 180-4 が定める mod 2^32 加算その
//! もの。未定義動作にはならない）で構成する。

pub(crate) const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 1 ブロック（64 バイト）ぶんの圧縮関数（FIPS 180-4 6.2.2 節）。メッセージ
/// スケジュールは 64 語配列ではなく 16 語のローリングバッファ（`w[t & 15]`）で
/// 保持する（Issue #399）。
pub(crate) fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
        if let Some(slot) = w.get_mut(i) {
            *slot = u32::from_be_bytes(*word);
        }
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *state;

    for t in 0..64usize {
        let idx = t & 15;
        if t >= 16 {
            let w15 = w[(t - 15) & 15];
            let w2 = w[(t - 2) & 15];
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            let prev16 = w[idx];
            w[idx] = prev16
                .wrapping_add(s0)
                .wrapping_add(w[(t - 7) & 15])
                .wrapping_add(s1);
        }

        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let k = K.get(t).copied().unwrap_or(0);
        let temp1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(k)
            .wrapping_add(w[idx]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
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
}

/// ストリーミング更新型の SHA-256 状態（Issue #399）。`recovery::content_hash`
/// の `HashInputBuilder` が各 `push_*` から [`Sha256::update`] を直接呼ぶことで、
/// 入力全体を一度 `Vec` へ連結してからパディングのため再度複製する、という
/// 2 回の全量コピーを避ける設計。固定長スタックバッファ（64 バイト）のみを使い、
/// 入力長に比例したヒープ確保は行わない。
///
/// `wire-server` の HMAC-SHA-256（Issue #940・WIRE-18）は ipad/opad 適用後の
/// 状態を `Clone` して使い回すため `Clone` を実装する。
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    /// 64 バイト未満の未処理端数（`buffered` バイトぶんのみ有効）。
    buffer: [u8; 64],
    buffered: usize,
    /// 入力バイト総数。`finalize` でビット長（`wrapping_mul(8)`）へ変換する。
    total_len: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            state: H0,
            buffer: [0u8; 64],
            buffered: 0,
            total_len: 0,
        }
    }

    /// `total_len` を増やさずにバイト列をブロックバッファへ吸収する（`update` と
    /// `finalize` のパディング処理が共有する内部処理）。
    fn absorb(&mut self, mut data: &[u8]) {
        if self.buffered > 0 {
            let need = 64 - self.buffered;
            let take = need.min(data.len());
            if let Some(slot) = self.buffer.get_mut(self.buffered..self.buffered + take) {
                slot.copy_from_slice(&data[..take]);
            }
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                compress(&mut self.state, &block);
                self.buffered = 0;
            }
        }

        let (chunks, remainder) = data.as_chunks::<64>();
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

    pub fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        self.absorb(data);
    }

    /// FIPS 180-4 5.1.1 節のパディング（`0x80` 1 バイト → 零埋め → 8 バイト BE
    /// ビット長）をブロックバッファ経由で適用してからダイジェストを取り出す。
    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.absorb(&[0x80]);

        const ZEROS: [u8; 64] = [0u8; 64];
        let zero_pad = if self.buffered <= 56 {
            56 - self.buffered
        } else {
            56 + 64 - self.buffered
        };
        if let Some(zeros) = ZEROS.get(..zero_pad) {
            self.absorb(zeros);
        }
        self.absorb(&bit_len.to_be_bytes());

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            let bytes = word.to_be_bytes();
            let start = i * 4;
            if let Some(slot) = out.get_mut(start..start + 4) {
                slot.copy_from_slice(&bytes);
            }
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// 一括ハッシュのヘルパー（`wire-server` の HMAC-SHA-256／PBKDF2・
/// `recovery::content_hash` のテストから使う）。
pub fn digest(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize()
}

/// 旧実装（バッチ全体を `Vec` へ連結してからパディングする一括処理版）の
/// 参照実装。ストリーミング版との等価性テストにのみ使う。
#[cfg(test)]
pub(crate) fn digest_reference(input: &[u8]) -> [u8; 32] {
    fn pad(input: &[u8]) -> Vec<u8> {
        let bit_len = (input.len() as u64).wrapping_mul(8);
        let mut msg = input.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0x00);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        msg
    }

    let padded = pad(input);
    let mut state = H0;
    for chunk in padded.as_chunks::<64>().0 {
        compress(&mut state, chunk);
    }

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        let bytes = word.to_be_bytes();
        let start = i * 4;
        if let Some(slot) = out.get_mut(start..start + 4) {
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

    #[test]
    fn digest_matches_fips_test_vector_abc() {
        let d = digest(b"abc");
        assert_eq!(
            hex(&d),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn digest_matches_known_digest_for_empty_input() {
        let d = digest(b"");
        assert_eq!(
            hex(&d),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn digest_matches_fips_test_vector_two_blocks() {
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        let d = digest(input);
        assert_eq!(
            hex(&d),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn digest_matches_nist_million_a_vector() {
        let input = vec![b'a'; 1_000_000];
        let d = digest(&input);
        assert_eq!(
            hex(&d),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn digest_streaming_matches_reference_for_boundary_lengths() {
        for len in [0usize, 1, 55, 56, 57, 63, 64, 65, 127, 128, 129, 1000] {
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
        for chunk_size in [1usize, 3, 7, 16, 64, 65, 200] {
            let mut hasher = Sha256::new();
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
}
