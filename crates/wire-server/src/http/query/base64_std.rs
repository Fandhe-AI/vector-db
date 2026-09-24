//! NoSQL 表層の `BYTEA` 列 JSON 表現（B7・Issue #886）が使う標準 base64
//! （RFC 4648 §4。`=` パディング必須・正準形のみ）の encode／decode。
//!
//! `http::session::token`（base64url・パディングなし）・`auth::argon2id`
//! （standard・パディングなし）はいずれも本モジュールの仕様（standard alphabet
//! ＋パディング必須）と一致しないため、`insert`／`update`（Issue #886）の
//! `BYTEA` 値束縛専用に新設する。
//!
//! `decode_base64_std` は **受信データ経路**（`POST /v1/query` の JSON ボディ
//! 経由で untrusted な文字列が到達する）として扱い、`unwrap`／`expect`／
//! 添字アクセス（`[]`）を使わず、閉じた文字集合・厳密なパディング検査のみで
//! 判定する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
//! 復号後の長さを入力長から算出し、`max_decoded` を超える場合は確保より前に
//! 拒否する。

/// 標準 base64 の decode で検出した拒否理由。untrusted 文字列のどの位置・
/// どの文字が原因かは含めない（`http::session::token::Base64UrlError` と
/// 同じ情報最小の設計）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64StdError {
    /// 復号後の長さが呼び出し元の `max_decoded` を超える。
    TooLong,
    /// 入力長が 4 文字単位で成立しない。
    InvalidLength,
    /// 標準アルファベット（`A-Z a-z 0-9 + /`）・パディング（`=`）以外の文字が
    /// 混入していた。
    InvalidCharacter,
    /// パディングの位置・個数が不正（末尾以外の `=`・1 個を超えるパディング等）。
    InvalidPadding,
    /// 末尾グループの余剰ビットが非ゼロな非正準表現。
    NonCanonical,
}

impl std::fmt::Display for Base64StdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Base64StdError::TooLong => write!(f, "base64 input exceeds the length limit"),
            Base64StdError::InvalidLength => write!(f, "base64 input has an invalid length"),
            Base64StdError::InvalidCharacter => {
                write!(f, "base64 input contains a character outside the alphabet")
            }
            Base64StdError::InvalidPadding => write!(f, "base64 input has invalid padding"),
            Base64StdError::NonCanonical => write!(f, "base64 input is not in canonical form"),
        }
    }
}

impl std::error::Error for Base64StdError {}

/// 6bit 値（0..=63）を標準 base64 のアルファベット文字（ASCII）へ写す。
fn base64_std_char(v: u8) -> u8 {
    match v {
        0..=25 => b'A' + v,
        26..=51 => b'a' + (v - 26),
        52..=61 => b'0' + (v - 52),
        62 => b'+',
        _ => b'/',
    }
}

/// ASCII バイトを標準 base64 の 6bit 値へ写す。アルファベット外は `None`。
fn base64_std_sextet(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// バイト列を標準 base64（`=` パディングあり）文字列へ変換する。
pub fn encode_base64_std(bytes: &[u8]) -> String {
    let full_groups = bytes.len() / 3;
    let remainder = bytes.len() % 3;
    let out_len = full_groups
        .saturating_add(if remainder > 0 { 1 } else { 0 })
        .saturating_mul(4);
    let mut out = String::with_capacity(out_len);
    for chunk in bytes.chunks(3) {
        match chunk {
            [a, b, c] => {
                let n = ((*a as u32) << 16) | ((*b as u32) << 8) | (*c as u32);
                out.push(base64_std_char(((n >> 18) & 0x3F) as u8) as char);
                out.push(base64_std_char(((n >> 12) & 0x3F) as u8) as char);
                out.push(base64_std_char(((n >> 6) & 0x3F) as u8) as char);
                out.push(base64_std_char((n & 0x3F) as u8) as char);
            }
            [a, b] => {
                let n = ((*a as u32) << 16) | ((*b as u32) << 8);
                out.push(base64_std_char(((n >> 18) & 0x3F) as u8) as char);
                out.push(base64_std_char(((n >> 12) & 0x3F) as u8) as char);
                out.push(base64_std_char(((n >> 6) & 0x3F) as u8) as char);
                out.push('=');
            }
            [a] => {
                let n = (*a as u32) << 16;
                out.push(base64_std_char(((n >> 18) & 0x3F) as u8) as char);
                out.push(base64_std_char(((n >> 12) & 0x3F) as u8) as char);
                out.push('=');
                out.push('=');
            }
            _ => {}
        }
    }
    out
}

/// 標準 base64（`=` パディング必須・正準形のみ）文字列をバイト列へ decode する。
///
/// **受信データ経路**。検査順（優先順）:
/// 1. 復号後の長さが `max_decoded` を超える（[`Base64StdError::TooLong`]。
///    入力長から算出するため実際の確保が発生する前に判定する）
/// 2. `len % 4 != 0`（[`Base64StdError::InvalidLength`]）
/// 3. アルファベット外の文字（[`Base64StdError::InvalidCharacter`]）
/// 4. パディングの位置・個数不正（[`Base64StdError::InvalidPadding`]）
/// 5. 末尾グループの非正準表現（[`Base64StdError::NonCanonical`]）
pub fn decode_base64_std(input: &str, max_decoded: u32) -> Result<Vec<u8>, Base64StdError> {
    let bytes = input.as_bytes();

    // 入力長そのものの上限は「パディングが最大（2 個）だったとしても
    // `max_decoded` を超えうる」余裕を持たせた緩い上限に留め（確保量の
    // 大まかな上限のみを先に確定する）、実際の復号後バイト数は pad_count が
    // 判明した後に厳密に検証する（後述。`decode_base64_std` の意味論を優先し、
    // わずかに緩い早期棄却で正当な入力を誤って拒否しない）。
    let max_input_len = ((max_decoded as usize).saturating_add(2) / 3).saturating_mul(4);
    if bytes.len() > max_input_len {
        return Err(Base64StdError::TooLong);
    }
    if !bytes.len().is_multiple_of(4) {
        return Err(Base64StdError::InvalidLength);
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    // パディングは末尾グループにのみ許可し、0〜2 個までとする。パディング
    // より後ろに非パディング文字が来る形・パディングが 3 個以上ある形は
    // すべて `InvalidPadding` とする。添字アクセスは使わず `get()`／
    // イテレータのみで判定する（受信データ経路。coding-rust.md）。
    let pad_count = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    if pad_count > 2 {
        return Err(Base64StdError::InvalidPadding);
    }
    let body_len = bytes.len().saturating_sub(pad_count);
    let body = bytes
        .get(..body_len)
        .ok_or(Base64StdError::InvalidPadding)?;
    if body.contains(&b'=') {
        return Err(Base64StdError::InvalidPadding);
    }

    // 実際のパディング数が判明した時点で、復号後バイト数（`3 * (len/4) -
    // pad_count`）が `max_decoded` を超えないか厳密に検証する（確保より前。
    // 上の緩い入力長上限だけでは「パディング無しの入力」が `max_decoded` を
    // 超えて通過しうるため、ここで確定的に拒否する）。
    let decoded_len = (bytes.len() / 4)
        .saturating_mul(3)
        .saturating_sub(pad_count);
    if decoded_len > max_decoded as usize {
        return Err(Base64StdError::TooLong);
    }

    let out_capacity = decoded_len;
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(out_capacity)
        .map_err(|_| Base64StdError::TooLong)?;

    let group_count = bytes.len() / 4;
    for (group_idx, chunk) in bytes.chunks(4).enumerate() {
        let is_last_group = group_idx + 1 == group_count;
        let group_pad = if is_last_group { pad_count } else { 0 };
        match chunk {
            [a, b, c, d] => {
                let v0 = base64_std_sextet(*a).ok_or(Base64StdError::InvalidCharacter)?;
                let v1 = base64_std_sextet(*b).ok_or(Base64StdError::InvalidCharacter)?;
                match group_pad {
                    0 => {
                        let v2 = base64_std_sextet(*c).ok_or(Base64StdError::InvalidCharacter)?;
                        let v3 = base64_std_sextet(*d).ok_or(Base64StdError::InvalidCharacter)?;
                        let n = ((v0 as u32) << 18)
                            | ((v1 as u32) << 12)
                            | ((v2 as u32) << 6)
                            | (v3 as u32);
                        out.push((n >> 16) as u8);
                        out.push((n >> 8) as u8);
                        out.push(n as u8);
                    }
                    1 => {
                        let v2 = base64_std_sextet(*c).ok_or(Base64StdError::InvalidCharacter)?;
                        if v2 & 0x03 != 0 {
                            return Err(Base64StdError::NonCanonical);
                        }
                        let n = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6);
                        out.push((n >> 16) as u8);
                        out.push((n >> 8) as u8);
                    }
                    2 => {
                        if v1 & 0x0F != 0 {
                            return Err(Base64StdError::NonCanonical);
                        }
                        let n = ((v0 as u32) << 18) | ((v1 as u32) << 12);
                        out.push((n >> 16) as u8);
                    }
                    _ => return Err(Base64StdError::InvalidPadding),
                }
            }
            // `bytes.len() % 4 == 0` を上で検証済みのため、`chunks(4)` が
            // 4 未満の要素を返すことは構造上ない（防御的に拒否する）。
            _ => return Err(Base64StdError::InvalidLength),
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4648 §10 のテストベクタ（標準 base64・パディングあり）。
    const RFC4648_VECTORS: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn rfc4648_vectors_round_trip() {
        for (raw, encoded) in RFC4648_VECTORS {
            assert_eq!(encode_base64_std(raw), *encoded);
            assert_eq!(
                decode_base64_std(encoded, u32::MAX).expect("valid vector"),
                *raw
            );
        }
    }

    #[test]
    fn decode_rejects_missing_padding() {
        assert_eq!(
            decode_base64_std("3q2+7w", u32::MAX),
            Err(Base64StdError::InvalidLength)
        );
    }

    #[test]
    fn decode_rejects_excess_padding() {
        assert_eq!(
            decode_base64_std("3q2+7w=a", u32::MAX),
            Err(Base64StdError::InvalidPadding)
        );
    }

    #[test]
    fn decode_rejects_alphabet_outside_characters() {
        assert_eq!(
            decode_base64_std("!!!!", u32::MAX),
            Err(Base64StdError::InvalidCharacter)
        );
        // base64url の `-`/`_` は標準アルファベットに含まれない。
        assert_eq!(
            decode_base64_std("-_-_", u32::MAX),
            Err(Base64StdError::InvalidCharacter)
        );
    }

    #[test]
    fn decode_rejects_padding_in_the_middle() {
        assert_eq!(
            decode_base64_std("Zm=9v===", u32::MAX),
            Err(Base64StdError::InvalidPadding)
        );
    }

    #[test]
    fn decode_rejects_non_canonical_tail() {
        // 'h' の 6bit 値は 33（0b100001）。下位 2bit が非ゼロなため
        // 1 パディング（3 文字＋1 パディング）グループの余剰ビットが非ゼロ。
        assert_eq!(
            decode_base64_std("Zh==", u32::MAX),
            Err(Base64StdError::NonCanonical)
        );
    }

    #[test]
    fn decode_rejects_over_max_decoded_before_allocation() {
        // "foobar" は 6 バイトに復号される。上限を 5 バイトにすると拒否される。
        assert_eq!(
            decode_base64_std("Zm9vYmFy", 5),
            Err(Base64StdError::TooLong)
        );
        assert!(decode_base64_std("Zm9vYmFy", 6).is_ok());
    }

    #[test]
    fn round_trip_all_lengths_up_to_64_bytes() {
        for len in 0..=64usize {
            let bytes: Vec<u8> = (0..len).map(|i| ((i * 37 + 7) % 256) as u8).collect();
            let encoded = encode_base64_std(&bytes);
            let decoded = decode_base64_std(&encoded, u32::MAX).expect("round trip decode");
            assert_eq!(decoded, bytes, "len={len}");
        }
    }

    #[test]
    fn decode_rejects_length_not_multiple_of_four() {
        assert_eq!(
            decode_base64_std("A", u32::MAX),
            Err(Base64StdError::InvalidLength)
        );
        assert_eq!(
            decode_base64_std("AA", u32::MAX),
            Err(Base64StdError::InvalidLength)
        );
        assert_eq!(
            decode_base64_std("AAA", u32::MAX),
            Err(Base64StdError::InvalidLength)
        );
    }
}
