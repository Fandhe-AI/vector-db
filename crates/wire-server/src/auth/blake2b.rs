//! RFC 7693 準拠の BLAKE2b-512 自作実装（`unsafe` なし）。
//!
//! `auth::argon2id` の内部ハッシュ関数 H・可変長ハッシュ関数 H' の下位プリミティブとして
//! 呼ばれる（`auth::argon2id` 以外のモジュールから直接使う想定はない）。依存追加なしで
//! Argon2id を自作するための構成要素（`.claude/rules/dependency-policy.md`）。
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`）。

/// RFC 7693 の初期化ベクタ定数（`blake2b_iv`）。
const IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// RFC 7693 §2.7 のメッセージ語順列（ラウンド 10・11 は SIGMA[0]・SIGMA[1] を再利用する
/// ため、あらかじめ 12 行に展開して保持する）。
const SIGMA: [[usize; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

const BLOCK_BYTES: usize = 128;

/// RFC 7693 §3.1 の混合関数 G（BLAKE2b の回転定数 R1..R4 = 32,24,16,63）。
#[inline]
fn mix(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// RFC 7693 §3.2 の圧縮関数 F。`t` はこれまでに処理した総バイト数（オフセットカウンタ）、
/// `is_last` は最終ブロックかどうかのフラグ。
fn compress(h: &mut [u64; 8], block: &[u8; BLOCK_BYTES], t: u128, is_last: bool) {
    let mut m = [0u64; 16];
    for (i, word) in m.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&block[i * 8..i * 8 + 8]);
        *word = u64::from_le_bytes(b);
    }

    let mut v = [0u64; 16];
    v[0..8].copy_from_slice(h);
    v[8..16].copy_from_slice(&IV);
    v[12] ^= (t & 0xFFFF_FFFF_FFFF_FFFF) as u64;
    v[13] ^= (t >> 64) as u64;
    if is_last {
        v[14] = !v[14];
    }

    for round in SIGMA.iter() {
        mix(&mut v, 0, 4, 8, 12, m[round[0]], m[round[1]]);
        mix(&mut v, 1, 5, 9, 13, m[round[2]], m[round[3]]);
        mix(&mut v, 2, 6, 10, 14, m[round[4]], m[round[5]]);
        mix(&mut v, 3, 7, 11, 15, m[round[6]], m[round[7]]);
        mix(&mut v, 0, 5, 10, 15, m[round[8]], m[round[9]]);
        mix(&mut v, 1, 6, 11, 12, m[round[10]], m[round[11]]);
        mix(&mut v, 2, 7, 8, 13, m[round[12]], m[round[13]]);
        mix(&mut v, 3, 4, 9, 14, m[round[14]], m[round[15]]);
    }

    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

/// ストリーミング型の BLAKE2b ハッシュ状態（鍵なし・出力長 `nn` バイト、1..=64）。
pub struct Blake2b {
    h: [u64; 8],
    buf: [u8; BLOCK_BYTES],
    buf_len: usize,
    total_len: u128,
    out_len: usize,
}

impl Blake2b {
    /// 鍵なし BLAKE2b の初期化（`out_len` は 1..=64 バイト）。
    ///
    /// `out_len` が範囲外の呼び出しは本クレート内部でのみ発生しうる契約違反であり、
    /// untrusted 入力からは到達しない（呼び出し元はすべて定数の出力長を渡す）ため、
    /// ここでの `assert!` は coding-rust.md の受信データ経路の禁止対象ではない。
    pub fn new(out_len: usize) -> Self {
        assert!(
            (1..=64).contains(&out_len),
            "blake2b: out_len must be in 1..=64"
        );
        let mut h = IV;
        h[0] ^= 0x0101_0000 ^ (out_len as u64);
        Self {
            h,
            buf: [0u8; BLOCK_BYTES],
            buf_len: 0,
            total_len: 0,
            out_len,
        }
    }

    /// 入力バイト列を取り込む。128 バイトブロック単位で圧縮関数へ渡し、
    /// 最終ブロックは [`Self::finalize`] まで保持する（RFC 7693 の keep-last-block 方式）。
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.buf_len == BLOCK_BYTES {
                self.total_len += BLOCK_BYTES as u128;
                let block = self.buf;
                compress(&mut self.h, &block, self.total_len, false);
                self.buf_len = 0;
            }
            let take = (BLOCK_BYTES - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
        }
    }

    /// 保持していた最終ブロックをゼロパディングして圧縮し、出力長ぶんの先頭バイトを返す。
    pub fn finalize(mut self) -> Vec<u8> {
        self.total_len += self.buf_len as u128;
        for b in self.buf[self.buf_len..].iter_mut() {
            *b = 0;
        }
        let block = self.buf;
        compress(&mut self.h, &block, self.total_len, true);

        let mut out = Vec::with_capacity(self.out_len);
        for word in self.h.iter() {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.truncate(self.out_len);
        out
    }
}

/// ワンショット呼び出しの補助関数（`update` を 1 回だけ行うケース向け）。
pub fn hash(out_len: usize, data: &[u8]) -> Vec<u8> {
    let mut h = Blake2b::new(out_len);
    h.update(data);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// RFC 7693 Appendix A: BLAKE2b-512("abc") の既知テストベクタ。
    #[test]
    fn blake2b_512_abc_matches_rfc7693_vector() {
        let digest = hash(64, b"abc");
        assert_eq!(
            hex(&digest),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d\
             17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd40099\
             23"
        );
    }

    /// 空メッセージも RFC 7693 の keep-last-block 方式（`dd=1` の空ブロック）どおり
    /// 処理できること（複数バイト境界を跨ぐ `update` 呼び出し分割の回帰確認も兼ねる）。
    #[test]
    fn blake2b_512_empty_message() {
        let digest = hash(64, b"");
        assert_eq!(
            hex(&digest),
            "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f54\
             19d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9b\
             e2ce"
        );
    }

    /// `update` を複数回・ブロック境界を跨いで分割呼び出ししても単一呼び出しと
    /// 同一結果になること（ストリーミング実装の回帰確認）。
    #[test]
    fn blake2b_512_streaming_matches_oneshot() {
        let data: Vec<u8> = (0u16..300).map(|i| (i % 251) as u8).collect();
        let oneshot = hash(64, &data);

        let mut streamed = Blake2b::new(64);
        for chunk in data.chunks(37) {
            streamed.update(chunk);
        }
        assert_eq!(streamed.finalize(), oneshot);
    }

    /// 出力長 32 バイト（`nn` がパラメータブロックに混入されるため 64 バイト出力の
    /// 単純な先頭切り詰めにはならない）でも正しく計算できること。期待値は BLAKE2b
    /// 実装系の標準ライブラリで独立に算出した既知値と一致させる
    /// （`auth::argon2id::h_prime` が短い出力長を要求するケースの前提確認）。
    #[test]
    fn blake2b_variable_length_output() {
        let short = hash(32, b"abc");
        assert_eq!(
            hex(&short),
            "bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d5231\
             9"
        );
    }
}
