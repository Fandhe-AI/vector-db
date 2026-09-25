//! 汎用 DER（Distinguished Encoding Rules）TLV リーダー
//! （TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #963・親 #941）。
//!
//! [`super::pkcs8::DerReader`] は PKCS#8 の固定シーケンスだけを読む
//! 最小実装だったのに対し、本モジュールは [`super::x509`] が要求する
//! X.509 の任意深さの入れ子（`SEQUENCE`／`SET`／コンテキスト依存タグ）を
//! 走査できるよう一般化したものである。単一バイトのタグ・definite 形式の
//! 長さのみを受理し、long-tag-number（`tag & 0x1f == 0x1f`）・BER の EOC
//! （`0x00`。DER では意味を持たない）・indefinite 長・非最小符号化は
//! いずれも拒否する。
//!
//! ## untrusted 入力としての扱い
//!
//! 証明書ファイルは運用者が起動時に用意するものの、内容そのものは
//! untrusted として扱う（`.claude/rules/coding-rust.md`）。添字アクセス
//! （`[]`）は使わず `split_first`／`split_at_checked` のみで進め、長さの
//! 算出は `checked_shl`／`u32` 蓄積 + `usize::try_from` でオーバーフローを
//! 起こさない。
//!
//! ## 定数時間性
//!
//! DER のタグ・長さ・入れ子構造はいずれも公開された形式情報であり秘密では
//! ないため、これらに基づく分岐は問題ない（[`super::x509`] のドキュメント
//! コメントを参照）。

use std::fmt;

/// DER 構文検証で検出した拒否理由（内容・位置・実バイトは含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DerError {
    /// 入力が途中で尽きた（TLV を最後まで読めない）。
    Truncated,
    /// 長さオクテットが indefinite 形式（`0x80`）。
    IndefiniteLength,
    /// long form の長さが最小符号化になっていない。
    NonMinimalLength,
    /// long form の長さオクテット数が本実装の対応範囲を超える、または
    /// 値が `usize` に収まらない。
    LengthTooLarge,
    /// high-tag-number 形式（`tag & 0x1f == 0x1f`）や EOC（`0x00`）など、
    /// 本実装が対応しない単一バイトタグ以外の表現。
    UnsupportedTag,
    /// 入れ子の走査深さが上限を超えた。
    NestingTooDeep,
    /// 期待した個数の TLV を読み終えた後に余りバイトがある。
    TrailingData,
    /// 期待したタグと異なるタグが現れた。
    UnexpectedTag,
}

impl fmt::Display for DerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            DerError::Truncated => "DER input is truncated",
            DerError::IndefiniteLength => "DER indefinite length is not supported",
            DerError::NonMinimalLength => "DER length encoding is not minimal",
            DerError::LengthTooLarge => "DER length is too large",
            DerError::UnsupportedTag => "DER tag form is not supported",
            DerError::NestingTooDeep => "DER nesting exceeds the depth limit",
            DerError::TrailingData => "DER input has trailing data",
            DerError::UnexpectedTag => "DER tag does not match the expected tag",
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for DerError {}

/// X.509 パース全体で許容する DER の入れ子最大深さ（本リポの実装既定値。
/// `SEQUENCE → SET → SEQUENCE` の Name 構造で 5 階層程度必要になるため
/// 十分な余裕を見込む）。
pub(crate) const MAX_DER_NESTING_DEPTH: usize = 16;

/// 走査済みの 1 個の TLV（タグ・値部分・タグ長さ込みの生バイト列）。
/// `raw` は [`super::x509`] が `tbsCertificate.signature` と外側
/// `signatureAlgorithm` の DER バイト列一致（RFC 5280 §4.1.1.2）を
/// 検査する際に使う。
#[derive(Debug)]
pub(crate) struct Tlv<'a> {
    pub(crate) tag: u8,
    pub(crate) value: &'a [u8],
    pub(crate) raw: &'a [u8],
}

/// タグ・definite 長さのみを受理する DER TLV リーダー。
pub(crate) struct DerReader<'a> {
    data: &'a [u8],
}

impl<'a> DerReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        DerReader { data }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(crate) fn expect_end(&self) -> Result<(), DerError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(DerError::TrailingData)
        }
    }

    /// 単一バイトのタグを読む。high-tag-number 形式（下位 5 ビットが
    /// すべて 1）は本実装が対応しないため拒否し、`0x00`（BER の EOC。DER
    /// では意味を持たない）も有効なタグとして扱わず拒否する。
    fn read_tag(&mut self) -> Result<u8, DerError> {
        let (tag, rest) = self.data.split_first().ok_or(DerError::Truncated)?;
        if *tag & 0x1f == 0x1f || *tag == 0x00 {
            return Err(DerError::UnsupportedTag);
        }
        self.data = rest;
        Ok(*tag)
    }

    /// definite 形式の長さオクテットを読み、値の長さを返す。
    /// - `0x80`（indefinite）は拒否する
    /// - long form は 1〜4 オクテットまでとし、最小符号化（1 バイトで
    ///   表現できる値を long form で書いていない）を要求する
    /// - 値は `u32` で蓄積してから `usize` へ変換するため、シフトによる
    ///   ビット落ち（`checked_shl` は「シフト量」しか検査しない）を避ける
    /// - 残りバイト数を超える長さは拒否する
    fn read_length(&mut self) -> Result<usize, DerError> {
        let (first, rest) = self.data.split_first().ok_or(DerError::Truncated)?;
        if *first & 0x80 == 0 {
            self.data = rest;
            return Ok(*first as usize);
        }
        let octet_count = (*first & 0x7f) as usize;
        if octet_count == 0 {
            // 0x80: indefinite length。
            return Err(DerError::IndefiniteLength);
        }
        if octet_count > 4 {
            return Err(DerError::LengthTooLarge);
        }
        let (len_bytes, after_len) = rest
            .split_at_checked(octet_count)
            .ok_or(DerError::Truncated)?;
        let (first_len_byte, _) = len_bytes.split_first().ok_or(DerError::Truncated)?;
        if *first_len_byte == 0 {
            // 先頭オクテットが 0x00 の long form は非最小符号化。
            return Err(DerError::NonMinimalLength);
        }
        let value_u32 = len_bytes
            .iter()
            .try_fold(0u32, |acc, b| acc.checked_shl(8).map(|v| v | (*b as u32)))
            .ok_or(DerError::LengthTooLarge)?;
        if octet_count == 1 && value_u32 < 0x80 {
            // 1 バイトで表現できる値を long form で書いた非最小符号化。
            return Err(DerError::NonMinimalLength);
        }
        let value = usize::try_from(value_u32).map_err(|_| DerError::LengthTooLarge)?;
        self.data = after_len;
        Ok(value)
    }

    /// タグ・長さ・値を 1 組読み、内部カーソルを進める。
    pub(crate) fn read_any(&mut self) -> Result<Tlv<'a>, DerError> {
        let start = self.data;
        let tag = self.read_tag()?;
        let len = self.read_length()?;
        let (value, remaining) = self.data.split_at_checked(len).ok_or(DerError::Truncated)?;
        self.data = remaining;
        // raw（タグ+長さ+値）は「消費したバイト数」を start の先頭から
        // 切り出して求める。remaining は start の末尾側の部分列であり
        // （read_tag／read_length／split_at_checked がいずれも同一バッファの
        // 先頭を削るだけのため）、start.len() - remaining.len() が
        // 消費バイト数と一致する。
        let consumed = start.len() - remaining.len();
        let (raw, _) = start
            .split_at_checked(consumed)
            .ok_or(DerError::Truncated)?;
        Ok(Tlv { tag, value, raw })
    }

    /// `expected_tag` の TLV を読み、値部分を返す。タグが異なれば
    /// カーソルを進めず `UnexpectedTag` を返す。
    pub(crate) fn read_expected(&mut self, expected_tag: u8) -> Result<&'a [u8], DerError> {
        let (tag, _) = self.data.split_first().ok_or(DerError::Truncated)?;
        if *tag != expected_tag {
            return Err(DerError::UnexpectedTag);
        }
        let tlv = self.read_any()?;
        Ok(tlv.value)
    }

    /// 次のタグが `expected_tag` なら読み進めて `Some(value)` を返し、
    /// そうでなければカーソルを進めずに `None` を返す。
    pub(crate) fn read_optional(&mut self, expected_tag: u8) -> Option<&'a [u8]> {
        let (tag, _) = self.data.split_first()?;
        if *tag != expected_tag {
            return None;
        }
        // read_any はここまでの検査（タグ一致）が済んでいるため通常は
        // 成功するが、長さが不正なら構造検証（validate_structure）で
        // 別途拒否されている前提のため、ここでの失敗はカーソルを進めず
        // None として扱い、呼び出し元の後続読み取りで検出させる。
        let before = self.data;
        match self.read_any() {
            Ok(tlv) => Some(tlv.value),
            Err(_) => {
                self.data = before;
                None
            }
        }
    }
}

/// 入力全体がちょうど 1 個のトップレベル TLV であり、かつ constructed な
/// TLV の値部分が入れ子の TLV 列として整形式であることを、深さ上限
/// `max_depth` 付きで検証する。primitive な値（OCTET STRING・BIT STRING・
/// INTEGER 等）の中身には潜らない。
///
/// [`super::x509`] が本体パースの前段として呼び、構文が壊れた入力を
/// フィールドごとの意味解釈に渡さないようにする。
pub(crate) fn validate_structure(der: &[u8], max_depth: usize) -> Result<(), DerError> {
    let mut reader = DerReader::new(der);
    let top = reader.read_any()?;
    reader.expect_end()?;
    validate_tlv_contents(top.tag, top.value, max_depth, 0)
}

fn validate_tlv_contents(
    tag: u8,
    value: &[u8],
    max_depth: usize,
    depth: usize,
) -> Result<(), DerError> {
    // constructed ビット（0x20）が立っていない primitive な値は
    // 中身を解釈しない（OCTET STRING の中に BIT STRING 相当のデータが
    // あっても、それは呼び出し元がフィールドとして別途パースする）。
    if tag & 0x20 == 0 {
        return Ok(());
    }
    if depth >= max_depth {
        return Err(DerError::NestingTooDeep);
    }
    let mut reader = DerReader::new(value);
    while !reader.is_empty() {
        let child = reader.read_any()?;
        validate_tlv_contents(child.tag, child.value, max_depth, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_short_form_length() {
        let der = [0x30, 0x02, 0x01, 0x02];
        let mut r = DerReader::new(&der);
        let tlv = r.read_any().expect("valid TLV");
        assert_eq!(tlv.tag, 0x30);
        assert_eq!(tlv.value, &[0x01, 0x02]);
        assert!(r.is_empty());
    }

    #[test]
    fn accepts_long_form_length_1_to_4_octets() {
        // 1 オクテット long form: 0x81 0x80 → 長さ 128。
        let mut der = vec![0x04, 0x81, 0x80];
        der.extend_from_slice(&[0u8; 128]);
        let mut r = DerReader::new(&der);
        let tlv = r.read_any().expect("valid 1-octet long form");
        assert_eq!(tlv.value.len(), 128);
        assert!(r.is_empty());

        // 2 オクテット long form: 0x82 0x01 0x00 → 長さ 256。
        let mut der2 = vec![0x04, 0x82, 0x01, 0x00];
        der2.extend_from_slice(&[0u8; 256]);
        let tlv2 = DerReader::new(&der2)
            .read_any()
            .expect("valid 2-octet long form");
        assert_eq!(tlv2.value.len(), 256);

        // 3 オクテット long form: 0x83 0x01 0x00 0x00 → 長さ 65536。
        let mut der3 = vec![0x04, 0x83, 0x01, 0x00, 0x00];
        der3.extend_from_slice(&[0u8; 65536]);
        let tlv3 = DerReader::new(&der3)
            .read_any()
            .expect("valid 3-octet long form");
        assert_eq!(tlv3.value.len(), 65536);

        // 4 オクテット long form: 0x84 0x01 0x00 0x00 0x00 → 長さ 16777216
        // は確保が重いため、値本体は用意せず read_length のみを検証する
        // （残りバイト不足で Truncated になることを確認する）。
        let der4 = [0x04u8, 0x84, 0x01, 0x00, 0x00, 0x00];
        let err = DerReader::new(&der4).read_any().unwrap_err();
        assert_eq!(err, DerError::Truncated);
    }

    #[test]
    fn rejects_indefinite_length() {
        let der = [0x30, 0x80];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::IndefiniteLength);
    }

    #[test]
    fn rejects_non_minimal_length_1_octet() {
        // 0x10（16）は 1 バイトで表現できるのに long form（0x81 0x10）。
        let der = [0x30, 0x81, 0x10];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::NonMinimalLength);
    }

    #[test]
    fn rejects_non_minimal_length_2_octet() {
        // 0x82 0x00 0x80 は 1 オクテット（0x81 0x80）で書けるはずの値。
        let der = [0x30, 0x82, 0x00, 0x80];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::NonMinimalLength);
    }

    #[test]
    fn rejects_zero_leading_length_octet() {
        let der = [0x30, 0x82, 0x00, 0x00];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::NonMinimalLength);
    }

    #[test]
    fn rejects_length_octet_count_over_4() {
        let der = [0x30u8, 0x85, 0x01, 0x00, 0x00, 0x00, 0x00];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::LengthTooLarge);
    }

    #[test]
    fn rejects_length_exceeding_remaining_bytes() {
        let der = [0x30, 0x10, 0x02, 0x01, 0x00];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::Truncated);
    }

    #[test]
    fn rejects_high_tag_number_form() {
        let der = [0x1f, 0x01, 0x00];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::UnsupportedTag);
    }

    #[test]
    fn rejects_eoc_tag() {
        // BER の EOC（0x00 0x00）は DER では意味を持たないタグであり、
        // read_tag の時点で拒否する。
        let der = [0x00, 0x00];
        let err = DerReader::new(&der).read_any().unwrap_err();
        assert_eq!(err, DerError::UnsupportedTag);
    }

    #[test]
    fn rejects_trailing_data_after_top_level_tlv() {
        let der = [0x02, 0x01, 0x00, 0x00];
        let err = validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::TrailingData);
    }

    #[test]
    fn validate_structure_accepts_nested_sequences_within_depth() {
        // ちょうど深さ 16 の SEQUENCE 入れ子（最内は INTEGER）。
        let mut der = vec![0x02, 0x01, 0x00];
        for _ in 0..16 {
            let mut wrapped = vec![0x30, der.len() as u8];
            wrapped.extend_from_slice(&der);
            der = wrapped;
        }
        validate_structure(&der, MAX_DER_NESTING_DEPTH).expect("depth 16 must be accepted");
    }

    #[test]
    fn validate_structure_rejects_nesting_beyond_depth_limit() {
        // 深さ 17（上限 16 を 1 段超える）の SEQUENCE 入れ子爆弾。
        let mut der = vec![0x02, 0x01, 0x00];
        for _ in 0..17 {
            let mut wrapped = vec![0x30, der.len() as u8];
            wrapped.extend_from_slice(&der);
            der = wrapped;
        }
        let err = validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::NestingTooDeep);
    }

    #[test]
    fn validate_structure_does_not_descend_into_primitive_values() {
        // OCTET STRING（primitive）の値の中に、それ自体では壊れた DER
        // （長さが残りを超える）が入っていても、primitive の中身には
        // 潜らないため構造検証は通る。
        let der = [0x04, 0x03, 0x30, 0x10, 0x00];
        validate_structure(&der, MAX_DER_NESTING_DEPTH).expect("primitive contents are opaque");
    }

    #[test]
    fn read_optional_returns_none_without_advancing_on_tag_mismatch() {
        let der = [0x02, 0x01, 0x00];
        let mut r = DerReader::new(&der);
        assert!(r.read_optional(0xa0).is_none());
        // カーソルが進んでいないため、続けて INTEGER を読める。
        let value = r.read_expected(0x02).expect("still readable");
        assert_eq!(value, &[0x00]);
    }

    #[test]
    fn read_expected_rejects_tag_mismatch() {
        let der = [0x02, 0x01, 0x00];
        let err = DerReader::new(&der).read_expected(0x30).unwrap_err();
        assert_eq!(err, DerError::UnexpectedTag);
    }
}
