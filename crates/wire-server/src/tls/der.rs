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
    /// primitive 必須の universal 型（constructed 必須の 5 種以外。
    /// [`universal_type_requires_constructed`] 参照）が constructed ビット
    /// 付きで現れた（BER の断片化 constructed 文字列は DER では非該当）。
    ConstructedUniversalType,
    /// universal primitive 型の値が DER の正規形・値の制約に反する
    /// （[`validate_universal_primitive`] が型ごとに判定する。時刻型は
    /// [`DerError::InvalidTime`] を別に返す）。
    InvalidPrimitiveEncoding,
    /// `UTCTime`／`GeneralizedTime` の値が DER の正規形（X.690 §11.7・
    /// §11.8）または暦として不正。
    InvalidTime,
    /// DER 正規形の検査を本実装が持たない universal 型（`REAL`（9）・
    /// `TIME`（14））または予約済みのタグ番号（15）。X.509 では使われない
    /// ため、検査せずに受理するのではなく fail-closed に拒否する。
    UnsupportedUniversalType,
    /// constructed でしか符号化できない universal 型（`EXTERNAL`（8）・
    /// `EMBEDDED PDV`（11）・`SEQUENCE`（16）・`SET`（17）・
    /// `CHARACTER STRING`（29））が primitive（constructed ビット無し）で
    /// 符号化されている（X.690 §8.18・§8.19・§8.9・§8.11・§8.21）。
    PrimitiveConstructedOnlyType,
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
            DerError::ConstructedUniversalType => {
                "DER universal type is not allowed to be constructed"
            }
            DerError::InvalidPrimitiveEncoding => "DER primitive value is not in canonical form",
            DerError::InvalidTime => "DER time value is not in canonical form",
            DerError::UnsupportedUniversalType => "DER universal type is not supported",
            DerError::PrimitiveConstructedOnlyType => {
                "DER constructed-only universal type must not be encoded as primitive"
            }
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

/// universal クラス（タグ上位 2 ビットが `00`）のうち、X.690 が常に
/// constructed での符号化を要求する型のタグ番号か（`EXTERNAL`（8。
/// §8.18）・`EMBEDDED PDV`（11。§8.19）・`SEQUENCE`（16。§8.9）・
/// `SET`（17。§8.11）・`CHARACTER STRING`（29。§8.21））。
///
/// DER ではこの 5 種は constructed が必須で primitive は不正、それ以外の
/// universal 型（`BOOLEAN`・`INTEGER`・`NULL`・`OBJECT IDENTIFIER`・
/// `BIT STRING`・`OCTET STRING`・各種文字列型・`UTCTime`／
/// `GeneralizedTime` 等）は primitive が必須で constructed は不正となる
/// （X.690 §10.2。BER が文字列型等に許す断片化 constructed 形は DER では
/// 非該当）。予約済み・未定義の universal タグ番号も fail-closed に後者
/// （primitive のみ）へ倒す。フィールドの意味は問わないため、universal
/// クラスである限りタグ番号だけで判定できる（PR #1036 codex-review P2
/// 指摘: 当初は SEQUENCE／SET のみを constructed 型として扱っていた）。
fn universal_type_requires_constructed(tag_number: u8) -> bool {
    matches!(tag_number, 0x08 | 0x0b | 0x10 | 0x11 | 0x1d)
}

/// `INTEGER`／`ENUMERATED` の値部分が DER の正規形（X.690 §8.3.2・§8.4）
/// か。値は 1 バイト以上で、先頭 9 ビットがすべて 0 またはすべて 1 に
/// なる冗長な先頭オクテットを持たないこと。
fn is_minimal_integer(value: &[u8]) -> bool {
    match value {
        [] => false,
        [first, second, ..] => {
            !((*first == 0x00 && second & 0x80 == 0) || (*first == 0xff && second & 0x80 != 0))
        }
        [_] => true,
    }
}

/// `OBJECT IDENTIFIER`／`RELATIVE-OID` の値部分（base-128 可変長サブ
/// 識別子列）が整形式か（X.690 §8.19.2・§8.20.2）。空・サブ識別子先頭の
/// `0x80`（非最小符号化）・継続ビット付きのまま終端（切り詰め）を拒否する。
pub(crate) fn is_well_formed_oid(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut at_subidentifier_start = true;
    for byte in value {
        if at_subidentifier_start && *byte == 0x80 {
            return false;
        }
        at_subidentifier_start = byte & 0x80 == 0;
    }
    at_subidentifier_start
}

/// `BIT STRING` の値部分（先頭 1 バイトが未使用ビット数、残りが内容）が
/// DER の正規形か（X.690 §8.6.2・§11.2）。値は 1 バイト以上・未使用ビット数
/// 0〜7・内容が空なら未使用ビット数 0・未使用ビット数が非 0 なら最終
/// オクテットの下位未使用ビットがすべて 0 であること。universal の
/// `BIT STRING` は [`validate_universal_primitive`] から、IMPLICIT タグで
/// universal タグを失う `issuerUniqueID`／`subjectUniqueID` 等は
/// [`super::x509`] から、同じ本関数を呼んで検査する。
pub(crate) fn validate_bit_string(value: &[u8]) -> Result<(), DerError> {
    let (&unused_bits, content) = value
        .split_first()
        .ok_or(DerError::InvalidPrimitiveEncoding)?;
    if unused_bits > 7 {
        return Err(DerError::InvalidPrimitiveEncoding);
    }
    if unused_bits == 0 {
        return Ok(());
    }
    let last_byte = content.last().ok_or(DerError::InvalidPrimitiveEncoding)?;
    let unused_mask = (1u8 << unused_bits) - 1;
    if last_byte & unused_mask != 0 {
        return Err(DerError::InvalidPrimitiveEncoding);
    }
    Ok(())
}

/// DER の `UTCTime`／`GeneralizedTime` から取り出した暦フィールド。暦として
/// 有効な範囲（月 1〜12・月と閏年に応じた日・時 0〜23・分/秒 0〜59）は
/// 検査済み。年は `UTCTime` の 2 桁年を RFC 5280 §4.1.2.5.1 の規則
/// （`YY >= 50` は 19YY、`< 50` は 20YY）で 4 桁へ展開した値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DerTime {
    pub(crate) year: i64,
    pub(crate) month: i64,
    pub(crate) day: i64,
    pub(crate) hour: i64,
    pub(crate) minute: i64,
    pub(crate) second: i64,
    /// `GeneralizedTime` が小数秒を持っていたか（DER の正規形として
    /// 末尾 0 なし・1 桁以上であることは検査済み）。
    pub(crate) has_fraction: bool,
}

fn two_digits(pair: &[u8]) -> Option<i64> {
    match pair {
        [a, b] if a.is_ascii_digit() && b.is_ascii_digit() => {
            Some(i64::from(a - b'0') * 10 + i64::from(b - b'0'))
        }
        _ => None,
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// `YYYY`（展開済み）と `MMDDHHMMSS` の 10 桁から暦として有効な
/// [`DerTime`] を組み立てる。うるう秒（60 秒）は fail-closed に受理しない。
fn build_der_time(year: i64, rest: &[u8], has_fraction: bool) -> Result<DerTime, DerError> {
    let field = |start: usize| {
        rest.get(start..start + 2)
            .and_then(two_digits)
            .ok_or(DerError::InvalidTime)
    };
    let time = DerTime {
        year,
        month: field(0)?,
        day: field(2)?,
        hour: field(4)?,
        minute: field(6)?,
        second: field(8)?,
        has_fraction,
    };
    if !(1..=12).contains(&time.month)
        || time.day < 1
        || time.day > days_in_month(time.year, time.month)
        || time.hour > 23
        || time.minute > 59
        || time.second > 59
    {
        return Err(DerError::InvalidTime);
    }
    Ok(time)
}

/// `UTCTime` の値部分を DER の正規形（X.690 §11.8: `YYMMDDHHMMSSZ` の
/// 13 バイト固定。秒は省略不可・終端は `Z`・タイムゾーンオフセット不可）と
/// して読み、暦として有効な [`DerTime`] を返す。
pub(crate) fn parse_utc_time(value: &[u8]) -> Result<DerTime, DerError> {
    let (body, terminator) = value.split_at_checked(12).ok_or(DerError::InvalidTime)?;
    if terminator != b"Z" {
        return Err(DerError::InvalidTime);
    }
    let two_digit_year = body
        .get(0..2)
        .and_then(two_digits)
        .ok_or(DerError::InvalidTime)?;
    let year = if two_digit_year >= 50 {
        1900 + two_digit_year
    } else {
        2000 + two_digit_year
    };
    let rest = body.get(2..).ok_or(DerError::InvalidTime)?;
    build_der_time(year, rest, false)
}

/// `GeneralizedTime` の値部分を DER の正規形（X.690 §11.7:
/// `YYYYMMDDHHMMSS[.f+]Z`。秒は省略不可・小数秒の区切りは `.` で 1 桁以上・
/// 末尾に `0` を置かない・終端は `Z`・タイムゾーンオフセット不可）として
/// 読み、暦として有効な [`DerTime`] を返す。小数秒の有無は
/// [`DerTime::has_fraction`] で呼び出し元に伝える（RFC 5280 の validity は
/// 小数秒を禁じるため [`super::x509`] がそこで拒否する）。
pub(crate) fn parse_generalized_time(value: &[u8]) -> Result<DerTime, DerError> {
    let (body, terminator) = value
        .split_last()
        .ok_or(DerError::InvalidTime)
        .map(|(last, body)| (body, *last))?;
    if terminator != b'Z' {
        return Err(DerError::InvalidTime);
    }
    let (date_time, fraction) = body.split_at_checked(14).ok_or(DerError::InvalidTime)?;
    let has_fraction = match fraction.split_first() {
        None => false,
        Some((b'.', digits)) => {
            let last = digits.last().ok_or(DerError::InvalidTime)?;
            if !digits.iter().all(u8::is_ascii_digit) || *last == b'0' {
                return Err(DerError::InvalidTime);
            }
            true
        }
        Some(_) => return Err(DerError::InvalidTime),
    };
    let year_digits = date_time.get(0..4).ok_or(DerError::InvalidTime)?;
    let high = year_digits.get(0..2).and_then(two_digits);
    let low = year_digits.get(2..4).and_then(two_digits);
    let year = match (high, low) {
        (Some(high), Some(low)) => high * 100 + low,
        _ => return Err(DerError::InvalidTime),
    };
    let rest = date_time.get(4..).ok_or(DerError::InvalidTime)?;
    build_der_time(year, rest, has_fraction)
}

/// `PrintableString` の文字集合（X.680 §41.4: 英大小文字・数字・空白・
/// `' ( ) + , - . / : = ?`）。
fn is_printable_string_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b" '()+,-./:=?".contains(&byte)
}

/// universal クラスの primitive な値を型ごとの DER の正規形・値の制約で
/// 検査する（[`validate_structure`] が全階層で呼ぶ唯一の型別検査）。
///
/// - `BOOLEAN`（1）: 長さ 1 で `0x00`／`0xFF`
/// - `INTEGER`（2）・`ENUMERATED`（10）: 空でなく最小符号化
/// - `BIT STRING`（3）: [`validate_bit_string`]
/// - `NULL`（5）: 長さ 0
/// - `OBJECT IDENTIFIER`（6）・`RELATIVE-OID`（13）: 空でなく各サブ識別子が
///   最小符号化で切り詰められていない
/// - `UTF8String`（12）: 正しい UTF-8
/// - `NumericString`（18）: 数字と空白
/// - `PrintableString`（19）: [`is_printable_string_char`] の文字集合
/// - `IA5String`（22）: 7 ビット（`0x00`〜`0x7F`）
/// - `UTCTime`（23）・`GeneralizedTime`（24）: [`parse_utc_time`]／
///   [`parse_generalized_time`]（違反は [`DerError::InvalidTime`]）
/// - `VisibleString`（26）: `0x20`〜`0x7E`
/// - `UniversalString`（28）: 長さが 4 の倍数
/// - `BMPString`（30）: 長さが 2 の倍数
/// - `REAL`（9）・`TIME`（14）・予約（15）: 検査を持たないため
///   [`DerError::UnsupportedUniversalType`] で拒否
///
/// 意図的に制約を課さない型: `OCTET STRING`（4。任意のオクテット列）と、
/// ISO 2022 のエスケープシーケンスで文字集合を切り替えるため本実装では
/// 文字集合を検証できない `ObjectDescriptor`（7）・`TeletexString`（20）・
/// `VideotexString`（21）・`GraphicString`（25）・`GeneralString`（27）。
/// constructed 必須の型（8・11・16・17・29）はここへ来る前に
/// [`DerError::PrimitiveConstructedOnlyType`] で拒否される。
fn validate_universal_primitive(tag_number: u8, value: &[u8]) -> Result<(), DerError> {
    let canonical = match tag_number {
        0x01 => matches!(value, [0x00] | [0xff]),
        0x02 | 0x0a => is_minimal_integer(value),
        0x03 => return validate_bit_string(value),
        0x05 => value.is_empty(),
        0x06 | 0x0d => is_well_formed_oid(value),
        0x09 | 0x0e | 0x0f => return Err(DerError::UnsupportedUniversalType),
        0x0c => std::str::from_utf8(value).is_ok(),
        0x12 => value.iter().all(|b| b.is_ascii_digit() || *b == b' '),
        0x13 => value.iter().all(|b| is_printable_string_char(*b)),
        0x16 => value.is_ascii(),
        0x17 => return parse_utc_time(value).map(|_| ()),
        0x18 => return parse_generalized_time(value).map(|_| ()),
        0x1a => value.iter().all(|b| (0x20..=0x7e).contains(b)),
        0x1c => value.len().is_multiple_of(4),
        0x1e => value.len().is_multiple_of(2),
        _ => true,
    };
    if canonical {
        Ok(())
    } else {
        Err(DerError::InvalidPrimitiveEncoding)
    }
}

/// 入力全体がちょうど 1 個のトップレベル TLV であり、かつ constructed な
/// TLV の値部分が入れ子の TLV 列として整形式であることを、深さ上限
/// `max_depth` 付きで検証する。あわせて、universal 型ごとの
/// primitive/constructed の別（[`universal_type_requires_constructed`]。
/// 違反は `ConstructedUniversalType`／`PrimitiveConstructedOnlyType`）と、
/// universal primitive 型ごとの DER 正規形・値の制約
/// （[`validate_universal_primitive`]）を、フィールドの意味を問わず入れ子の
/// 深さ全体にわたって検証する（PR #1036 codex-review P1 指摘: 当初は
/// `BIT STRING` 等の primitive 値を検査せず、AlgorithmIdentifier の
/// parameters のように意味を解釈しない値の中の非正規形を通していた）。
/// context-specific 等 universal 以外のクラスの primitive 値は、IMPLICIT
/// タグで元の型が分からないため呼び出し元のフィールド検査に委ねる。
///
/// [`super::x509`] が本体パースの前段として呼び、構文が壊れた入力を
/// フィールドごとの意味解釈に渡さないようにする。issuer/subject の
/// `Name`（`RDNSequence`）や `extensions`（値部分は本モジュールの対象外
/// のまま意味解釈しない）の中に現れる文字列型・`NULL`・`BOOLEAN` も、
/// これらが constructed 化されている・非正規形であるといった DER 違反は
/// ここで一律に拒否する。
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
    let is_constructed = tag & 0x20 != 0;
    let is_universal_class = tag & 0xc0 == 0x00;
    let tag_number = tag & 0x1f;

    if is_universal_class {
        if is_constructed {
            if !universal_type_requires_constructed(tag_number) {
                return Err(DerError::ConstructedUniversalType);
            }
        } else {
            // 逆方向の制約: constructed 必須の型（SEQUENCE・SET・EXTERNAL・
            // EMBEDDED PDV・CHARACTER STRING）が primitive のまま現れて
            // いれば DER 違反として拒否する（例えば issuer/subject の RDN を
            // 表す SET を primitive の 0x11 に置き換えた不正 DER。
            // PR #1036 codex-review P1 指摘）。
            if universal_type_requires_constructed(tag_number) {
                return Err(DerError::PrimitiveConstructedOnlyType);
            }
            validate_universal_primitive(tag_number, value)?;
        }
    }

    // constructed ビット（0x20）が立っていない primitive な値は
    // 中身を解釈しない（OCTET STRING の中に BIT STRING 相当のデータが
    // あっても、それは呼び出し元がフィールドとして別途パースする）。
    if !is_constructed {
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

    #[test]
    fn validate_structure_rejects_constructed_utf8_string() {
        // UTF8String（0x0c）を constructed（0x2c）で符号化した BER 断片化
        // 形式は DER では不正。SEQUENCE の子要素として埋め込む。
        let der = [0x30, 0x04, 0x2c, 0x02, 0x0c, 0x00];
        let err = validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::ConstructedUniversalType);
    }

    #[test]
    fn validate_structure_rejects_constructed_octet_string() {
        // OCTET STRING（0x04）を constructed（0x24）で符号化。
        let der = [0x30, 0x02, 0x24, 0x00];
        let err = validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::ConstructedUniversalType);
    }

    #[test]
    fn validate_structure_accepts_constructed_sequence_and_set() {
        // SEQUENCE（0x30）・SET（0x31）はいずれも universal クラスだが
        // constructed が正しい表現であり拒否されない。
        let der = [0x31, 0x03, 0x02, 0x01, 0x00];
        validate_structure(&der, MAX_DER_NESTING_DEPTH).expect("SET must remain constructed-ok");
    }

    #[test]
    fn validate_structure_rejects_non_empty_null() {
        // NULL（0x05）は値が常に空でなければならない。
        let der = [0x30, 0x02, 0x05, 0x00];
        validate_structure(&der, MAX_DER_NESTING_DEPTH).expect("empty NULL is valid");
        let der_nonempty = [0x30, 0x03, 0x05, 0x01, 0x00];
        let err = validate_structure(&der_nonempty, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::InvalidPrimitiveEncoding);
    }

    #[test]
    fn validate_structure_rejects_non_canonical_boolean() {
        // BOOLEAN（0x01）は DER では 1 バイト・`0x00`／`0xFF` のみが正規形。
        let der_true = [0x30, 0x03, 0x01, 0x01, 0xff];
        validate_structure(&der_true, MAX_DER_NESTING_DEPTH).expect("0xFF is canonical TRUE");
        let der_false = [0x30, 0x03, 0x01, 0x01, 0x00];
        validate_structure(&der_false, MAX_DER_NESTING_DEPTH).expect("0x00 is canonical FALSE");
        // BER では TRUE を非ゼロの任意バイトで表現できるが DER では不正。
        let der_non_canonical_true = [0x30, 0x03, 0x01, 0x01, 0x01];
        let err = validate_structure(&der_non_canonical_true, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::InvalidPrimitiveEncoding);
        // 2 バイト以上の BOOLEAN も不正。
        let der_wrong_len = [0x30, 0x04, 0x01, 0x02, 0x00, 0x00];
        let err = validate_structure(&der_wrong_len, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::InvalidPrimitiveEncoding);
    }

    #[test]
    fn validate_structure_rejects_primitive_sequence() {
        // SEQUENCE（0x30）を primitive（0x10。constructed ビット無し）で
        // 符号化した不正 DER。
        let der = [0x30, 0x02, 0x10, 0x00];
        let err = validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::PrimitiveConstructedOnlyType);
    }

    #[test]
    fn validate_structure_rejects_primitive_set() {
        // SET（0x31）を primitive（0x11）で符号化した不正 DER
        // （issuer/subject 内 RDN の SET を primitive に置き換える攻撃を
        // 想定。PR #1036 codex-review P1 指摘）。
        let der = [0x30, 0x02, 0x11, 0x00];
        let err = validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::PrimitiveConstructedOnlyType);
    }

    // X.690 が constructed を必須とする universal 型（EXTERNAL・EMBEDDED PDV・
    // CHARACTER STRING）は constructed で受理し primitive で拒否する
    // （PR #1036 codex-review P2 指摘の回帰）。
    #[test]
    fn validate_structure_accepts_constructed_only_types_and_rejects_their_primitive_form() {
        for tag_number in [0x08u8, 0x0b, 0x1d] {
            let constructed = [0x30, 0x05, 0x20 | tag_number, 0x03, 0x02, 0x01, 0x00];
            validate_structure(&constructed, MAX_DER_NESTING_DEPTH)
                .unwrap_or_else(|e| panic!("constructed tag {tag_number:#x} rejected: {e:?}"));
            let primitive = [0x30, 0x02, tag_number, 0x00];
            assert_eq!(
                validate_structure(&primitive, MAX_DER_NESTING_DEPTH).unwrap_err(),
                DerError::PrimitiveConstructedOnlyType,
                "primitive tag {tag_number:#x}"
            );
        }
        // constructed 必須型の内側にも DER 制約が及ぶ（非正規 BOOLEAN）。
        let nested_invalid = [0x30, 0x05, 0x28, 0x03, 0x01, 0x01, 0x01];
        assert_eq!(
            validate_structure(&nested_invalid, MAX_DER_NESTING_DEPTH).unwrap_err(),
            DerError::InvalidPrimitiveEncoding
        );
    }

    #[test]
    fn validate_structure_rejects_constructed_primitive_only_types() {
        // 文字列型・BIT STRING・OCTET STRING・INTEGER 等と、予約済みの
        // universal タグ番号（0x0e・0x0f）は constructed を拒否する。
        for tag_number in [
            0x01u8, 0x02, 0x03, 0x04, 0x06, 0x0c, 0x0e, 0x0f, 0x13, 0x16, 0x17,
        ] {
            let der = [0x30, 0x02, 0x20 | tag_number, 0x00];
            assert_eq!(
                validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err(),
                DerError::ConstructedUniversalType,
                "constructed tag {tag_number:#x}"
            );
        }
    }

    // INTEGER／ENUMERATED の最小符号化・OID／RELATIVE-OID の整形式性を、
    // フィールドの意味を問わず全階層で検査する。
    #[test]
    fn validate_structure_checks_integer_and_oid_canonical_form() {
        for ok in [
            &[0x30, 0x03, 0x02, 0x01, 0x00][..],
            &[0x30, 0x04, 0x02, 0x02, 0x00, 0x80],
            &[0x30, 0x04, 0x02, 0x02, 0xff, 0x7f],
            &[0x30, 0x03, 0x0a, 0x01, 0x05],
            &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70],
            &[0x30, 0x04, 0x0d, 0x02, 0x81, 0x00],
        ] {
            validate_structure(ok, MAX_DER_NESTING_DEPTH)
                .unwrap_or_else(|e| panic!("{ok:02x?} rejected: {e:?}"));
        }
        for bad in [
            &[0x30, 0x02, 0x02, 0x00][..],
            &[0x30, 0x04, 0x02, 0x02, 0x00, 0x7f],
            &[0x30, 0x04, 0x02, 0x02, 0xff, 0x80],
            &[0x30, 0x04, 0x0a, 0x02, 0x00, 0x01],
            &[0x30, 0x02, 0x06, 0x00],
            &[0x30, 0x04, 0x06, 0x02, 0x80, 0x01],
            &[0x30, 0x04, 0x06, 0x02, 0x2b, 0x81],
            &[0x30, 0x03, 0x0d, 0x01, 0x80],
        ] {
            assert_eq!(
                validate_structure(bad, MAX_DER_NESTING_DEPTH).unwrap_err(),
                DerError::InvalidPrimitiveEncoding,
                "{bad:02x?}"
            );
        }
    }

    /// テスト専用: `SEQUENCE { <tag> <value> }` を組み立てて
    /// `validate_structure` にかける（値は 127 バイト未満に限る）。
    fn check_primitive(tag: u8, value: &[u8]) -> Result<(), DerError> {
        let inner_len = u8::try_from(value.len()).expect("short test value");
        let mut der = vec![0x30, inner_len + 2, tag, inner_len];
        der.extend_from_slice(value);
        validate_structure(&der, MAX_DER_NESTING_DEPTH)
    }

    fn assert_primitive_cases(tag: u8, accepted: &[&[u8]], rejected: &[&[u8]], error: DerError) {
        for value in accepted {
            assert_eq!(
                check_primitive(tag, value),
                Ok(()),
                "tag {tag:#04x} value {value:02x?} must be accepted"
            );
        }
        for value in rejected {
            assert_eq!(
                check_primitive(tag, value),
                Err(error),
                "tag {tag:#04x} value {value:02x?} must be rejected"
            );
        }
    }

    // BIT STRING（PR #1036 codex-review P1 指摘の回帰: 意味を解釈しない値の
    // 中の BIT STRING も DER の正規形で検査する）。
    #[test]
    fn primitive_bit_string_follows_der_canonical_form() {
        assert_primitive_cases(
            0x03,
            &[&[0x00], &[0x00, 0xff], &[0x03, 0xf8], &[0x07, 0x80]],
            &[&[], &[0x08, 0x00], &[0x03, 0xf9], &[0x01], &[0x05]],
            DerError::InvalidPrimitiveEncoding,
        );
    }

    #[test]
    fn primitive_utc_time_follows_der_canonical_form() {
        assert_primitive_cases(
            0x17,
            &[b"160801121924Z", b"240229000000Z", b"500101000000Z"],
            &[
                b"1608011219Z",
                b"160801121924",
                b"160801121924+0900",
                b"160801121924z",
                b"161301121924Z",
                b"230229000000Z",
                b"160801241924Z",
                b"160801121960Z",
                b"16080112192.Z",
            ],
            DerError::InvalidTime,
        );
    }

    #[test]
    fn primitive_generalized_time_follows_der_canonical_form() {
        assert_primitive_cases(
            0x18,
            &[
                b"20501231235959Z",
                b"20501231235959.5Z",
                b"20501231235959.123Z",
                b"20000229000000Z",
            ],
            &[
                b"205012312359Z",
                b"20501231235959",
                b"20501231235959+0000",
                b"20501231235959.50Z",
                b"20501231235959.0Z",
                b"20501231235959.Z",
                b"20501231235959,5Z",
                b"20501231235959.5aZ",
                b"21000229000000Z",
                b"20501232235959Z",
            ],
            DerError::InvalidTime,
        );
    }

    #[test]
    fn primitive_string_types_follow_their_character_constraints() {
        let invalid = DerError::InvalidPrimitiveEncoding;
        // UTF8String
        assert_primitive_cases(
            0x0c,
            &[b"", b"abc", "\u{e9}\u{3042}".as_bytes()],
            &[&[0xc3], &[0xff], &[0xed, 0xa0, 0x80]],
            invalid,
        );
        // NumericString
        assert_primitive_cases(0x12, &[b"0123 456"], &[b"12a", b"1-2"], invalid);
        // PrintableString
        assert_primitive_cases(
            0x13,
            &[b"Ab 09'()+,-./:=?"],
            &[b"a*b", b"a@b", b"a_b", b"a&b", &[0x41, 0xc3, 0xa9]],
            invalid,
        );
        // IA5String
        assert_primitive_cases(
            0x16,
            &[b"user@example.com", &[0x00, 0x7f]],
            &[&[0x80]],
            invalid,
        );
        // VisibleString
        assert_primitive_cases(0x1a, &[b" a~"], &[&[0x7f], &[0x0a]], invalid);
        // UniversalString（4 の倍数）・BMPString（2 の倍数）
        assert_primitive_cases(0x1c, &[&[0, 0, 0, 0x41]], &[&[0, 0, 0x41]], invalid);
        assert_primitive_cases(0x1e, &[&[0, 0x41]], &[&[0x41], &[0, 0x41, 0]], invalid);
    }

    #[test]
    fn unconstrained_primitive_types_accept_arbitrary_octets() {
        // OCTET STRING と、ISO 2022 のエスケープで文字集合を切り替えるため
        // 文字集合を検証しない型（ObjectDescriptor・Teletex・Videotex・
        // Graphic・GeneralString）は任意のオクテット列を受理する。
        for tag in [0x04u8, 0x07, 0x14, 0x15, 0x19, 0x1b] {
            assert_eq!(
                check_primitive(tag, &[0xff, 0x00, 0x1b]),
                Ok(()),
                "{tag:#04x}"
            );
        }
    }

    #[test]
    fn unsupported_primitive_types_are_rejected() {
        // REAL・TIME・予約済み（15）は正規形検査を持たないため fail-closed。
        for tag in [0x09u8, 0x0e, 0x0f] {
            assert_eq!(
                check_primitive(tag, &[]),
                Err(DerError::UnsupportedUniversalType),
                "{tag:#04x}"
            );
        }
    }

    #[test]
    fn context_specific_primitive_values_are_left_to_field_checks() {
        // IMPLICIT タグで universal タグを失った値（例: [1] の BIT STRING）は
        // 型が分からないため構造検証では検査せず、x509 のフィールド検査に委ねる。
        assert_eq!(check_primitive(0x81, &[0x08, 0x00]), Ok(()));
    }

    #[test]
    fn validate_structure_checks_universal_constraints_inside_nested_context_tags() {
        // context-specific constructed タグ（[0] EXPLICIT 相当）の内側に
        // 現れる universal 型にも同じ検査が及ぶことを確認する
        // （issuer/subject Name・extensions のような、意味解釈しないまま
        // 構造検証だけを通すフィールドの内側を想定）。
        let der = [0x30, 0x06, 0xa0, 0x04, 0x2c, 0x02, 0x0c, 0x00];
        let err = validate_structure(&der, MAX_DER_NESTING_DEPTH).unwrap_err();
        assert_eq!(err, DerError::ConstructedUniversalType);
    }
}
