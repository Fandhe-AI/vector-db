//! 自作 SHA-256（FIPS 180-4）実装。安全 Rust のみ・`unsafe` 不使用・依存追加なし。
//!
//! 元は `recovery::content_hash`（TASK-101・RECOVER-10）が台帳の内容照合ハッシュ
//! 専用に非公開実装していたもの（Issue #399 でストリーミング化）を、wire-server の
//! TLS 1.3 鍵スケジュール（Issue #956・WIRE-9・HTTP-10・TASK-228 ポインタ）が
//! HMAC-SHA-256／HKDF の下敷きとして再利用できるよう本モジュールへ切り出し、
//! 公開 API 化した（Issue #956）。`recovery::content_hash` は本モジュールの
//! [`Sha256`]／[`digest`] を呼ぶだけの薄い利用側になり、ハッシュ入力バイト列の
//! レイアウト・出力値はこの切り出しの前後で完全に不変（既存 FIPS／NIST テスト
//! ベクタ・境界長網羅テストをそのまま本モジュールへ移設して機械検証する）。
//!
//! 依存追加が承認制のため（`.claude/rules/dependency-policy.md`）、外部クレートに
//! 頼らず標準ライブラリのみで実装する。固定サイズ配列・`wrapping_*` 演算
//! （FIPS 180-4 が定める mod 2^32 加算そのもの。未定義動作にはならない）のみで
//! 構成し、`unsafe` は使わない。

/// SHA-256 ダイジェストのバイト長。
pub const DIGEST_LEN: usize = 32;

/// SHA-256 の処理ブロック長（バイト）。HMAC のパディング計算等、呼び出し側が
/// ブロック境界を意識する必要がある箇所（`tls::hkdf` の HMAC 鍵パディング）向けに
/// 公開する。
pub const BLOCK_LEN: usize = 64;

const H0: [u32; 8] = [
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
/// 保持する。`t >= 16` のラウンドでは、更新前の `w[t & 15]` が
/// （16 引くごとに同じスロットへ戻ってくるため）ちょうど `w[t - 16]` を保持して
/// いることを利用し、そのスロットへ新しい `w[t]` を上書きしてから同じラウンドの
/// 圧縮に使う（Issue #399 由来の最適化。切り出しの前後で挙動は不変）。
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
        // `as_chunks::<4>()` は固定長 4 バイト配列を返すため `from_be_bytes` は
        // 失敗しない。添字直接アクセスの代わりに `get_mut` で明示的に処理する
        // （coding-rust.md「untrusted 入力の扱い」と同じ規律を内部処理にも適用する）。
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
            // 上書き前の w[idx] は w[t - 16]（ローリングバッファでは同一スロット
            // を 16 ラウンドごとに再利用する）。
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

/// ストリーミング更新型の SHA-256 状態（Issue #399）。呼び出し側（`recovery::content_hash`
/// の `HashInputBuilder`、`tls::hkdf` の HMAC 実装）は各フィールド・各ブロックを
/// `update` へ直接渡すことで、入力全体を一度 `Vec` へ連結してからパディングのため
/// さらに複製する、という 2 回の全量コピーを避けられる。固定長スタックバッファ
/// （64 バイト）のみを使い、入力長に比例したヒープ確保は行わない。
///
/// `Clone` は #964（TLS 1.3 Finished の transcript hash 計算）が、途中経過の状態から
/// 分岐して複数のダイジェストを取り出す用途を見込んで導出する（本 Issue 自体は
/// その用途を実装しない）。
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    /// 64 バイト未満の未処理端数（`buffered` バイトぶんのみ有効）。
    buffer: [u8; 64],
    buffered: usize,
    /// 入力バイト総数。`finalize` でビット長（`wrapping_mul(8)`）へ変換する
    /// （既存の `pad()` と同じ契約。実運用上の入力が `u64::MAX / 8` バイトへ
    /// 到達することはない）。
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Sha256 {
    /// HMAC の鍵由来の内部状態を誤ってログ等へ露出させないよう、内容を出さない
    /// （`tls::secret::Secret32` 等、本リポの秘密値型の `Debug` 秘匿方針に合わせる）。
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sha256").finish_non_exhaustive()
    }
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
    pub fn finalize(mut self) -> [u8; DIGEST_LEN] {
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

        let mut out = [0u8; DIGEST_LEN];
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

impl Drop for Sha256 {
    /// best-effort のゼロ化（`unsafe`／`write_volatile` を使わないため、最適化で
    /// 消去が省略されない保証はない。`tls::hkdf::zeroize` と同じ限界を持つ）。
    /// HMAC の鍵由来の内部状態がハッシャーの寿命を超えて残る面を減らす。
    fn drop(&mut self) {
        self.state = [0u32; 8];
        for b in self.buffer.iter_mut() {
            *b = 0;
        }
        core::hint::black_box(&self.state);
        core::hint::black_box(&self.buffer);
    }
}

/// 一括ハッシュのヘルパー。`Sha256::new().update(data).finalize()` の糖衣構文。
pub fn digest(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()
}

/// [`Sha256`] の参照実装（Issue #399 以前の一括処理版。バッチ全体を `Vec` へ
/// 連結してからパディングする旧実装をそのまま残す）。production からは呼ばれず、
/// ストリーミング版との等価性テスト（本モジュール・`recovery::content_hash`）
/// にのみ使うため `#[cfg(test)] pub(crate)`。
#[cfg(test)]
pub(crate) fn reference_digest(input: &[u8]) -> [u8; DIGEST_LEN] {
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

    let mut out = [0u8; DIGEST_LEN];
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

    // FIPS 180-4 附属の公開テストベクタ（SHA-256("abc")）。
    #[test]
    fn sha256_matches_fips_test_vector_abc() {
        let d = digest(b"abc");
        assert_eq!(
            hex(&d),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // 空文字列の既知ダイジェスト（NIST 公開値）。
    #[test]
    fn sha256_matches_known_digest_for_empty_input() {
        let d = digest(b"");
        assert_eq!(
            hex(&d),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // FIPS 180-4 の複数ブロックにまたがるテストベクタ
    // （"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"）。
    #[test]
    fn sha256_matches_fips_test_vector_two_blocks() {
        let input = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        let d = digest(input);
        assert_eq!(
            hex(&d),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    // Issue #399 追加: FIPS 180-4 附属の 896 bit（4 ブロックにまたがる）テストベクタ。
    #[test]
    fn sha256_matches_fips_test_vector_four_blocks() {
        let input = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        let d = digest(input);
        assert_eq!(
            hex(&d),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    // Issue #399 追加: NIST 公開の 1,000,000 × 'a' 反復テストベクタ。ストリーミング
    // 版の分割 `update`（`absorb` のブロック境界処理）を長大入力で検証する。
    #[test]
    fn sha256_matches_nist_million_a_vector() {
        let input = vec![b'a'; 1_000_000];
        let d = digest(&input);
        assert_eq!(
            hex(&d),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    // Issue #399: ストリーミング版と参照実装（一括処理版）が境界長 0..=200 バイト
    // で完全一致することを機械検証する（55/56/63/64/65/119/120 バイト等の
    // パディング分岐を網羅する）。決定的 LCG で生成した入力を使う。
    #[test]
    fn sha256_streaming_matches_reference_for_boundary_lengths() {
        let mut state: u64 = 0x2545F4914F6CDD1D;
        let mut next_byte = || {
            // xorshift* 相当の決定的 LCG（暗号強度は不要。境界長網羅の入力生成専用）。
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        };
        for len in 0..=200usize {
            let input: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            assert_eq!(
                digest(&input),
                reference_digest(&input),
                "mismatch at len={len}"
            );
        }
        for &len in &[4096usize, 65_537] {
            let input: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            assert_eq!(
                digest(&input),
                reference_digest(&input),
                "mismatch at len={len}"
            );
        }
    }

    // Issue #399: 同一入力を異なる粒度（1・3・63・64・65・100 バイト刻み）で
    // 分割 `update` した結果が、一括 `update` と一致することを検証する
    // （`Sha256::absorb` のバッファ境界処理のピン留め）。
    #[test]
    fn sha256_streaming_split_update_matches_one_shot_for_various_chunk_sizes() {
        let input: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let expected = reference_digest(&input);

        for chunk_size in [1usize, 3, 63, 64, 65, 100] {
            let mut hasher = Sha256::new();
            for chunk in input.chunks(chunk_size) {
                hasher.update(chunk);
            }
            let d = hasher.finalize();
            assert_eq!(d, expected, "mismatch at chunk_size={chunk_size}");
        }
    }

    // `Debug` が内部状態（ハッシュ対象の断片が残り得るバッファ）を出力しない。
    #[test]
    fn debug_does_not_expose_internal_state() {
        let mut h = Sha256::new();
        h.update(b"secret-looking-input");
        let rendered = format!("{h:?}");
        assert!(!rendered.contains("secret-looking-input"));
    }
}
