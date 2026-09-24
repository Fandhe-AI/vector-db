//! HMAC-SHA-256（RFC 2104）・HKDF-Extract/Expand（RFC 5869）・TLS 1.3 の
//! `HKDF-Expand-Label`／`Derive-Secret`（RFC 8446 §7.1）の原始操作
//! （TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #956・親 #941）。
//!
//! 対象暗号スイートは `TLS_AES_128_GCM_SHA256` のみ（親 Issue #941 の方針）の
//! ため、ハッシュ関数は SHA-256（[`engine::crypto::sha256`]）に固定する。
//! SHA-256 の実体は SCRAM-SHA-256 認証（Issue #940・WIRE-18）が engine の
//! 公開 API へ切り出し済みのものをそのまま再利用し、本モジュールが独自の
//! SHA-256 実装を持つことはない。ここでは原始操作のみを置き、TLS 1.3 の
//! 鍵スケジュール本体（Early/Handshake/Master secret の遷移・traffic
//! secret／key／iv の導出）は [`super::key_schedule`] が担う。
//!
//! 定数時間性の設計:
//! - ラウンド数・ブロック数・出力長はすべて呼び出し時点で確定する公開値
//!   （鍵長・データ長・`L`）であり、秘密値のビットに依存する分岐・ループ回数は
//!   持たない。
//! - 秘密値どうしの比較（Finished の verify_data 検証等）はここでは行わない
//!   （#964 の担当。定数時間比較はそこで別途用意する）。
//! - 秘密値の保持型 [`Secret32`] は Drop 時に best-effort でゼロ化するが、
//!   `unsafe`（`write_volatile` 等）を使わないため最適化により消去が省略され
//!   ない保証はない（[`super::x25519::SharedSecret`] と同じ限界）。

use engine::crypto::sha256::{digest, Sha256};
use std::fmt;

/// HKDF/HMAC が対象とするハッシュの出力長。SHA-256 固定のため常に 32。
pub const HASH_LEN: usize = 32;

/// SHA-256 の処理ブロック長（バイト）。HMAC の鍵パディング計算で
/// ブロック境界を意識する必要があるためここで固定する
/// （[`engine::crypto::sha256`] は公開定数を持たないため本モジュールで定義する）。
const BLOCK_LEN: usize = 64;

/// バイト列を Drop 時に best-effort でゼロ化する（`unsafe` なしのため最適化で
/// 消去が省略されない保証はない旨は呼び出し元のドキュメントに委ねる）。
/// [`Secret32`] のほか、HMAC の鍵ブロック（スタック上の一時バッファ）にも使う。
pub(crate) fn zeroize(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
    std::hint::black_box(&*buf);
}

/// HKDF・鍵スケジュールが受け渡す 32 バイトの秘密値（PRK・各段の secret）。
/// `Clone`／`Copy` は導出しない（不用意な複製を防ぐ。[`super::x25519::SharedSecret`]
/// と同じ設計判断）。`Debug` は内容を出さず、Drop で best-effort ゼロ化する。
pub struct Secret32([u8; 32]);

impl Secret32 {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Secret32(bytes)
    }

    /// 生の 32 バイト表現への参照。
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Secret32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Secret32").field(&"<redacted>").finish()
    }
}

impl Drop for Secret32 {
    fn drop(&mut self) {
        zeroize(&mut self.0);
    }
}

/// RFC 2104 の HMAC-SHA-256。`data` はマルチパートで受け取り、呼び出し側が
/// 連結用のヒープ確保をせずに複数フィールドを渡せるようにする
/// （[`super::key_schedule`] の `HkdfLabel` 組み立てがこの形を使う）。
///
/// 鍵がブロック長（64 バイト）を超える場合は先に SHA-256 で縮める
/// （RFC 2104 の規定どおり。鍵長は呼び出し時点で分かる公開値のため、
/// この分岐は秘密値に依存しない）。ipad/opad 用の鍵ブロック（スタック上の
/// 固定長配列）は使い終わったら [`zeroize`] する。
pub fn hmac_sha256(key: &[u8], data: &[&[u8]]) -> [u8; HASH_LEN] {
    let mut key_block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        let hashed = digest(key);
        if let Some(slot) = key_block.get_mut(..hashed.len()) {
            slot.copy_from_slice(&hashed);
        }
    } else if let Some(slot) = key_block.get_mut(..key.len()) {
        slot.copy_from_slice(key);
    }

    let mut ipad = [0u8; BLOCK_LEN];
    let mut opad = [0u8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        // 添字は 0..BLOCK_LEN の固定範囲のみで、`key_block`/`ipad`/`opad` は
        // いずれも同じ長さの配列のため範囲外にはならない。
        if let (Some(kb), Some(ip), Some(op)) = (key_block.get(i), ipad.get_mut(i), opad.get_mut(i))
        {
            *ip = kb ^ 0x36;
            *op = kb ^ 0x5c;
        }
    }

    let mut inner = Sha256::new();
    inner.update(&ipad);
    for part in data {
        inner.update(part);
    }
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner_digest);
    let result = outer.finalize();

    zeroize(&mut key_block);
    zeroize(&mut ipad);
    zeroize(&mut opad);

    result
}

/// RFC 5869 の HKDF-Extract。`salt` が空の場合、HMAC の鍵パディング規則により
/// 全ゼロ鍵（[`HASH_LEN`] バイト）を使ったのと同じ結果になる（RFC 5869 §2.2）。
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> Secret32 {
    Secret32::from_bytes(hmac_sha256(salt, &[ikm]))
}

/// [`hkdf_expand`]／[`hkdf_expand_label`] の失敗理由。いずれも呼び出し側の
/// 不正（出力長・ラベル長・コンテキスト長が仕様の範囲外）に起因し、外部入力
/// そのものの検証ではなく内部呼び出し契約違反のため `internal_error`（80）へ
/// 写す（alert の実送出は #965 の担当。ここでは写像のみ提供する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HkdfError {
    /// RFC 5869 の制約 `L <= 255 * HashLen` を超えた。
    OutputTooLong,
    /// RFC 8446 の `HkdfLabel.label`（`"tls13 " + label`）が 7〜255 バイトの
    /// 範囲に収まらない（`label` 自体は 1〜249 バイト）。
    InvalidLabel,
    /// RFC 8446 の `HkdfLabel.context` が 255 バイトを超えた。
    ContextTooLong,
}

impl fmt::Display for HkdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HkdfError::OutputTooLong => write!(f, "HKDF output length exceeds 255 * HashLen"),
            HkdfError::InvalidLabel => write!(f, "HKDF label length out of range"),
            HkdfError::ContextTooLong => write!(f, "HKDF context length exceeds 255 bytes"),
        }
    }
}

impl std::error::Error for HkdfError {}

impl HkdfError {
    /// TLS 1.3 alert の `AlertDescription` 値への写像。いずれも呼び出し契約
    /// 違反（内部起因）を表すため `internal_error`（80）。実送出は #965 の担当。
    pub const fn alert_description(self) -> u8 {
        80
    }
}

/// RFC 5869 の HKDF-Expand。`out.len() > 255 * HASH_LEN` は `Err` で拒否する
/// （fail-closed）。`out.is_empty()` は RFC 5869 が `L = 0` を明示的に禁止して
/// いないため `Ok(())`（何も書かない）として受理する。
///
/// T(i) 計算用の一時ブロック（前段ダイジェスト＋カウンタ）は使い終わったら
/// ゼロ化する。書き込みは `chunks_mut` で行い、添字直指定は使わない。
pub fn hkdf_expand(prk: &[u8; HASH_LEN], info: &[u8], out: &mut [u8]) -> Result<(), HkdfError> {
    const MAX_LEN: usize = 255 * HASH_LEN;
    if out.len() > MAX_LEN {
        return Err(HkdfError::OutputTooLong);
    }

    let mut prev: Option<[u8; HASH_LEN]> = None;
    let mut counter: u8 = 0;
    for chunk in out.chunks_mut(HASH_LEN) {
        counter += 1;
        let t = if let Some(p) = prev.as_ref() {
            hmac_sha256(prk, &[p.as_slice(), info, &[counter]])
        } else {
            hmac_sha256(prk, &[info, &[counter]])
        };
        chunk.copy_from_slice(&t[..chunk.len()]);
        if let Some(mut p) = prev.take() {
            zeroize(&mut p);
        }
        prev = Some(t);
    }
    if let Some(mut p) = prev.take() {
        zeroize(&mut p);
    }
    Ok(())
}

/// RFC 8446 §7.1 の `HkdfLabel` 構造体を組み立てる:
///
/// ```text
/// struct {
///     uint16 length = Length;
///     opaque label<7..255> = "tls13 " + Label;
///     opaque context<0..255> = Context;
/// } HkdfLabel;
/// ```
///
/// 固定長スタックバッファ（最大 2 + 1 + 255 + 1 + 255 = 514 バイト）へ、長さ
/// 検証を終えてからのみ書き込む（ヒープ確保をしない）。戻り値は書き込んだ
/// 有効長。
fn build_hkdf_label(
    out_len: u16,
    label: &[u8],
    context: &[u8],
    buf: &mut [u8; 514],
) -> Result<usize, HkdfError> {
    // "tls13 " (6 バイト) + label は 7..=255 バイト → label 自体は 1..=249 バイト。
    const PREFIX: &[u8] = b"tls13 ";
    let full_label_len = PREFIX.len() + label.len();
    if label.is_empty() || full_label_len > 255 {
        return Err(HkdfError::InvalidLabel);
    }
    if context.len() > 255 {
        return Err(HkdfError::ContextTooLong);
    }

    let mut pos = 0usize;
    let length_bytes = out_len.to_be_bytes();
    if let Some(slot) = buf.get_mut(pos..pos + 2) {
        slot.copy_from_slice(&length_bytes);
    }
    pos += 2;

    let full_label_len_u8 = u8::try_from(full_label_len).map_err(|_| HkdfError::InvalidLabel)?;
    if let Some(slot) = buf.get_mut(pos..pos + 1) {
        slot.copy_from_slice(&[full_label_len_u8]);
    }
    pos += 1;
    if let Some(slot) = buf.get_mut(pos..pos + PREFIX.len()) {
        slot.copy_from_slice(PREFIX);
    }
    pos += PREFIX.len();
    if let Some(slot) = buf.get_mut(pos..pos + label.len()) {
        slot.copy_from_slice(label);
    }
    pos += label.len();

    let context_len_u8 = u8::try_from(context.len()).map_err(|_| HkdfError::ContextTooLong)?;
    if let Some(slot) = buf.get_mut(pos..pos + 1) {
        slot.copy_from_slice(&[context_len_u8]);
    }
    pos += 1;
    if let Some(slot) = buf.get_mut(pos..pos + context.len()) {
        slot.copy_from_slice(context);
    }
    pos += context.len();

    Ok(pos)
}

/// RFC 8446 §7.1 の `HKDF-Expand-Label(Secret, Label, Context, Length)`。
pub fn hkdf_expand_label(
    secret: &[u8; HASH_LEN],
    label: &[u8],
    context: &[u8],
    out: &mut [u8],
) -> Result<(), HkdfError> {
    let out_len = u16::try_from(out.len()).map_err(|_| HkdfError::OutputTooLong)?;
    let mut buf = [0u8; 514];
    let len = build_hkdf_label(out_len, label, context, &mut buf)?;
    let info = buf.get(..len).ok_or(HkdfError::InvalidLabel)?;
    let result = hkdf_expand(secret, info, out);
    zeroize(&mut buf);
    result
}

/// RFC 8446 §7.1 の `Derive-Secret(Secret, Label, Messages) =
/// HKDF-Expand-Label(Secret, Label, Hash(Messages), Hash.length)`。
///
/// `Messages` を蓄積するトランスクリプトの管理は #964（Finished 計算）の
/// 担当のため、ここでは呼び出し側が既に計算済みの transcript hash
/// （`&[u8; HASH_LEN]`）を受け取る形に留める。
pub fn derive_secret(
    secret: &[u8; HASH_LEN],
    label: &[u8],
    transcript_hash: &[u8; HASH_LEN],
) -> Result<Secret32, HkdfError> {
    let mut out = [0u8; HASH_LEN];
    hkdf_expand_label(secret, label, transcript_hash, &mut out)?;
    Ok(Secret32::from_bytes(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    // RFC 4231 §4.2 Test Case 1（HMAC-SHA-256。20 バイト鍵）。
    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_1() {
        let key = hex_decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let data = hex_decode("4869205468657265");
        let mac = hmac_sha256(&key, &[&data]);
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    // RFC 4231 §4.3 Test Case 2（鍵が出力長より短い。"Jefe"）。
    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_2() {
        let key = b"Jefe";
        let data = hex_decode("7768617420646f2079612077616e7420666f72206e6f7468696e673f");
        let mac = hmac_sha256(key, &[&data]);
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    // RFC 4231 §4.8 Test Case 7（鍵・データとも SHA-256 のブロック長 64 バイトを
    // 超える。鍵は 131 バイトで、HMAC 内部で先に SHA-256 圧縮される経路を通す）。
    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_7_long_key() {
        let key = hex_decode(&"aa".repeat(131));
        let data = hex_decode(
            "54686973206973206120746573742075\
             73696e672061206c6172676572207468\
             616e20626c6f636b2d73697a65206b65\
             7920616e642061206c61726765722074\
             68616e20626c6f636b2d73697a652064\
             6174612e20546865206b6579206e6565\
             647320746f2062652068617368656420\
             6265666f7265206265696e6720757365\
             642062792074686520484d414320616c\
             676f726974686d2e",
        );
        let mac = hmac_sha256(&key, &[&data]);
        assert_eq!(
            hex(&mac),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    // 分割 data を渡した HMAC が一括版と一致する（`HashInputBuilder` 相当の
    // マルチパート呼び出しを HMAC 内部でも検証する）。
    #[test]
    fn hmac_sha256_multipart_data_matches_single_part() {
        let key = b"key";
        let combined = hmac_sha256(key, &[b"hello world"]);
        let split = hmac_sha256(key, &[b"hello", b" ", b"world"]);
        assert_eq!(combined, split);
    }

    // RFC 5869 Appendix A.1（SHA-256・基本ケース）。
    #[test]
    fn hkdf_matches_rfc5869_test_case_1() {
        let ikm = hex_decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = hex_decode("000102030405060708090a0b0c");
        let info = hex_decode("f0f1f2f3f4f5f6f7f8f9");
        let prk = hkdf_extract(&salt, &ikm);
        assert_eq!(
            hex(prk.as_bytes()),
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"
        );
        let mut okm = [0u8; 42];
        hkdf_expand(prk.as_bytes(), &info, &mut okm).expect("valid params");
        assert_eq!(
            hex(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    // RFC 5869 Appendix A.2（長い IKM/salt/info・長い出力）。
    #[test]
    fn hkdf_matches_rfc5869_test_case_2() {
        let ikm: Vec<u8> = (0x00..=0x4f).collect();
        let salt: Vec<u8> = (0x60..=0xaf).collect();
        let info: Vec<u8> = (0xb0..=0xff).collect();
        let prk = hkdf_extract(&salt, &ikm);
        assert_eq!(
            hex(prk.as_bytes()),
            "06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244"
        );
        let mut okm = [0u8; 82];
        hkdf_expand(prk.as_bytes(), &info, &mut okm).expect("valid params");
        assert_eq!(
            hex(&okm),
            "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c\
             59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71\
             cc30c58179ec3e87c14c01d5c1f3434f1d87"
        );
    }

    // RFC 5869 Appendix A.3（salt/info とも空長）。
    #[test]
    fn hkdf_matches_rfc5869_test_case_3_empty_salt_and_info() {
        let ikm = hex_decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let prk = hkdf_extract(&[], &ikm);
        assert_eq!(
            hex(prk.as_bytes()),
            "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04"
        );
        let mut okm = [0u8; 42];
        hkdf_expand(prk.as_bytes(), &[], &mut okm).expect("valid params");
        assert_eq!(
            hex(&okm),
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
        );
    }

    // 空 salt と 32 バイトの 0 salt が同じ PRK を生む（HMAC の鍵パディング規則
    // により等価。RFC 8446 §7.1 の "0" 表記の根拠）。
    #[test]
    fn hkdf_extract_empty_salt_equals_all_zero_salt() {
        let ikm = b"some ikm material";
        let prk_empty = hkdf_extract(&[], ikm);
        let prk_zero = hkdf_extract(&[0u8; HASH_LEN], ikm);
        assert_eq!(prk_empty.as_bytes(), prk_zero.as_bytes());
    }

    // 出力長の境界: L = 255 * HashLen は受理、+1 は拒否。
    #[test]
    fn hkdf_expand_accepts_max_length_and_rejects_one_more() {
        let prk = [0x11u8; HASH_LEN];
        let mut ok_buf = vec![0u8; 255 * HASH_LEN];
        assert!(hkdf_expand(&prk, b"info", &mut ok_buf).is_ok());

        let mut too_long = vec![0u8; 255 * HASH_LEN + 1];
        assert_eq!(
            hkdf_expand(&prk, b"info", &mut too_long),
            Err(HkdfError::OutputTooLong)
        );
    }

    // L = 0 は空書き込みとして受理する（RFC 5869 は明示的に禁止していない）。
    #[test]
    fn hkdf_expand_accepts_zero_length_output() {
        let prk = [0x22u8; HASH_LEN];
        let mut out: [u8; 0] = [];
        assert!(hkdf_expand(&prk, b"info", &mut out).is_ok());
    }

    // hkdf_expand_label のラベル境界: 空ラベルは拒否、1 バイトラベルは受理、
    // 249 バイトラベル（"tls13 " 込みで 255 バイト）は受理、250 バイトは拒否。
    #[test]
    fn hkdf_expand_label_rejects_empty_and_too_long_label() {
        let secret = [0x33u8; HASH_LEN];
        let mut out = [0u8; 32];

        assert_eq!(
            hkdf_expand_label(&secret, b"", b"", &mut out),
            Err(HkdfError::InvalidLabel)
        );

        let label_1 = vec![b'a'; 1];
        assert!(hkdf_expand_label(&secret, &label_1, b"", &mut out).is_ok());

        let label_249 = vec![b'a'; 249];
        assert!(hkdf_expand_label(&secret, &label_249, b"", &mut out).is_ok());

        let label_250 = vec![b'a'; 250];
        assert_eq!(
            hkdf_expand_label(&secret, &label_250, b"", &mut out),
            Err(HkdfError::InvalidLabel)
        );
    }

    // hkdf_expand_label のコンテキスト境界: 255 バイトは受理、256 バイトは拒否。
    #[test]
    fn hkdf_expand_label_rejects_context_too_long() {
        let secret = [0x44u8; HASH_LEN];
        let mut out = [0u8; 32];
        let label = b"test";

        let ctx_255 = vec![0u8; 255];
        assert!(hkdf_expand_label(&secret, label, &ctx_255, &mut out).is_ok());

        let ctx_256 = vec![0u8; 256];
        assert_eq!(
            hkdf_expand_label(&secret, label, &ctx_256, &mut out),
            Err(HkdfError::ContextTooLong)
        );
    }

    // hkdf_expand_label の出力長超過は拒否する。
    #[test]
    fn hkdf_expand_label_rejects_output_too_long() {
        let secret = [0x55u8; HASH_LEN];
        let mut out = vec![0u8; 255 * HASH_LEN + 1];
        assert_eq!(
            hkdf_expand_label(&secret, b"test", b"", &mut out),
            Err(HkdfError::OutputTooLong)
        );
    }

    // zeroize がバッファを全バイト 0 にする。
    #[test]
    fn zeroize_clears_all_bytes() {
        let mut buf = [0xffu8; 16];
        zeroize(&mut buf);
        assert_eq!(buf, [0u8; 16]);
    }

    // Secret32 の Debug 出力に鍵バイトの 16 進表記が含まれない。
    #[test]
    fn secret32_debug_does_not_expose_bytes() {
        let secret = Secret32::from_bytes([0xabu8; 32]);
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("ab"));
    }
}
