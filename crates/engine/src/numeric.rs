//! `NUMERIC(p, s)` 列型の内部表現・パース・整形（TABLE-13〔検討中〕・TASK-197、
//! Issue #885）。十進固定小数を `unscaled × 10^-scale`（`i128` の符号付き整数
//! `unscaled` と `u8` の `scale`）で表現する自作実装。外部クレートに依存しない
//! （dependency-policy）。
//!
//! [`catalog::ColumnType::Numeric`]（列宣言）・[`row_codec`]（行バイト表現）・
//! `sql::parser`（INSERT/UPDATE/UPSERT リテラル束縛）から呼ばれる。列の
//! `scale` は行には持たずカタログの列型のみが正とする（TABLE-13 追記参照。
//! ADR: `docs/design/column-type-extension.md`「#885 追記」節）。

use std::fmt;

/// `NUMERIC` 列が宣言できる最大精度（全体桁数）。`i128::MAX` は 10^38 未満のため、
/// 38 桁までの `unscaled` は必ず `i128` へ収まる。
pub const MAX_PRECISION: u8 = 38;

/// 10 進固定小数リテラルの解析前の入力長上限（バイト）。DoS 防止のため解析前に
/// 検査する（coding-rust.md: untrusted 入力の長さ検証）。
pub const MAX_LITERAL_LEN: usize = 1024;

/// 10^0 〜 10^38 の参照表。`p`（列精度）は untrusted 経由（カタログ decode）にも
/// なり得るため、添字アクセスは必ず [`pow10`] 経由にする。
const POW10: [i128; 39] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
    10_000_000_000_000_000_000,
    100_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000_000_000_000,
];

// 10^38 − 1 は i128::MAX（約 1.7 × 10^38）未満に収まる。境界（`MAX_PRECISION`）を
// 超える添字で `POW10` を引くコードが将来入っても panic しないよう、参照は必ず
// `pow10()` 経由にする（このアサーションは配列サイズの自己整合性のみを保証する）。
const _: () = assert!(POW10.len() == (MAX_PRECISION as usize) + 1);

/// `POW10[p]` の安全な参照。範囲外（`p > 38`）は `None`（呼び出し元は列精度の
/// 検証を経由済みのはずだが、untrusted なカタログ decode からも呼ばれるため
/// 添字アクセスを使わず fail-closed にする）。
fn pow10(p: u8) -> Option<i128> {
    POW10.get(p as usize).copied()
}

/// `NUMERIC` 列の値パース・decode で発生し得るエラー。呼び出し元
/// （`sql::parser`・`row_codec`）が `wire_code`（`22000`/`22003`）へ写像する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumericError {
    /// 文法違反（空・符号のみ・複数の `.`・指数表記・非数字・長さ超過等）。
    Malformed(String),
    /// 列の `NUMERIC(p, s)` に対して桁あふれ（丸め後の整数部が `p - s` 桁を
    /// 超える）。
    OutOfRange,
}

impl fmt::Display for NumericError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NumericError::Malformed(msg) => write!(f, "malformed numeric literal: {msg}"),
            NumericError::OutOfRange => write!(f, "numeric value out of range for column"),
        }
    }
}

impl std::error::Error for NumericError {}

/// `NUMERIC(p, s)` 列 1 個の値。`unscaled × 10^-scale` を表す（D2）。
///
/// 行バイト表現（[`crate::row_codec`]）は `scale` を持たずカタログの列型のみを
/// 正とするため、`Decimal` を独立に持ち回る場面（束縛・投影・RETURNING・wire
/// 出力）では常に列の `scale` と対にして扱う契約とする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    unscaled: i128,
    scale: u8,
}

impl Decimal {
    /// `unscaled`・`scale` から検証付きで構築する（呼び出し元は列の `scale`
    /// と一致していることを保証する契約だが、この契約は本コンストラクタが
    /// 公開 API（`row_codec`・wire-server・テストから到達）であるため型
    /// レベルでは強制できない）。`-0` は正規化（`unscaled == 0` のとき符号は
    /// 常に非負）する。
    ///
    /// `scale > MAX_PRECISION` は `NumericError::OutOfRange` を返す
    /// （fail-closed）。以前の実装は範囲外 `scale` を `MAX_PRECISION` へ
    /// 暗黙に丸めていたが、`from_parts(1, 39)` が本来表す `10^-39` の値を
    /// 検証なしに `10^-38` という別の数値へ変えてしまい、以後の `Display`・
    /// ハッシュ・格納判定のすべてが誤った値を正として扱う結果になっていた
    /// （P1 指摘対応）。呼び出し元が範囲外を渡し得る限り、丸めではなく
    /// 明示的な拒否で不変条件を守る。
    pub fn from_parts(unscaled: i128, scale: u8) -> Result<Self, NumericError> {
        if scale > MAX_PRECISION {
            return Err(NumericError::OutOfRange);
        }
        Ok(Decimal {
            unscaled: if unscaled == 0 { 0 } else { unscaled },
            scale,
        })
    }

    pub fn unscaled(&self) -> i128 {
        self.unscaled
    }

    pub fn scale(&self) -> u8 {
        self.scale
    }

    /// 列の精度 `p` に対して `|unscaled| < 10^p` を満たすか（TABLE-13 の
    /// decode 側検証・`row_codec::decode_row` から呼ばれる）。`p` が範囲外
    /// （`> MAX_PRECISION`）の場合は fail-closed に `false` を返す。
    pub fn fits_precision(&self, precision: u8) -> bool {
        match pow10(precision) {
            Some(limit) => self.unscaled.unsigned_abs() < limit.unsigned_abs(),
            None => false,
        }
    }
}

impl fmt::Display for Decimal {
    /// 正規テキスト表現（D8）。符号・整数部・（`scale > 0` のときのみ）`.` と
    /// ちょうど `scale` 桁にゼロ埋めした小数部。先頭ゼロ・指数表記・trailing
    /// `-0` を持たない（wire の DataRow テキスト・HTTP JSON number 化の
    /// いずれからも共有される正規形）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = self.scale as u32;
        let divisor = match pow10(self.scale) {
            Some(d) => d,
            // scale はカタログ経由で 0..=38 に検証済みのはず。防御的に
            // 桁なし表示へ縮退する（表示は fail-closed 対象外の整形処理のため
            // panic を避けるだけで十分）。
            None => return write!(f, "{}", self.unscaled),
        };
        let sign = if self.unscaled < 0 { "-" } else { "" };
        let abs = self.unscaled.unsigned_abs();
        let int_part = abs / (divisor.unsigned_abs());
        if scale == 0 {
            return write!(f, "{sign}{int_part}");
        }
        let frac_part = abs % (divisor.unsigned_abs());
        write!(
            f,
            "{sign}{int_part}.{frac_part:0width$}",
            width = scale as usize
        )
    }
}

/// 数字列の 1 パス読み取り状態（整数部・小数部を通しで走査する）。
struct DigitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> DigitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        DigitReader { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn advance(&mut self) {
        // `checked_add` は使わず単純加算だが、`pos` は `bytes.len()`
        // （`MAX_LITERAL_LEN` 以下に検証済み）を超えて呼ばれないため
        // オーバーフローしない（`peek` が事前に `None` を返し `advance` を
        // 呼ばせない構造）。
        self.pos += 1;
    }
}

/// `NUMERIC(precision, scale)` 列向けにリテラル文字列を解析し、束縛値
/// `Decimal` を返す（D4・D5）。
///
/// 受理する文法: `[+-]?(digits)?(\.digits?)?`（整数部・小数部の少なくとも
/// 一方に 1 桁以上必要）。空白・指数表記（`e`/`E`）・`NaN`/`Infinity`・複数の
/// `.` は拒否する。丸めは half away from zero（D4）。丸め後の整数部が
/// `precision - scale` 桁を超える場合は `OutOfRange`（`22003`）。
///
/// 1 パス・アロケーションなしで走査し、`unwrap`/`expect`/添字アクセスを
/// 使わない（coding-rust.md）。
pub fn parse_for_column(text: &str, precision: u8, scale: u8) -> Result<Decimal, NumericError> {
    if text.len() > MAX_LITERAL_LEN {
        return Err(NumericError::Malformed("literal too long".to_string()));
    }
    if !text.is_ascii() {
        return Err(NumericError::Malformed("literal must be ASCII".to_string()));
    }
    let bytes = text.as_bytes();
    let mut reader = DigitReader::new(bytes);

    let negative = match reader.peek() {
        Some(b'+') => {
            reader.advance();
            false
        }
        Some(b'-') => {
            reader.advance();
            true
        }
        _ => false,
    };

    // 整数部: 有効桁（先頭の連続する `0` は桁数に数えない。`NUMERIC(38,38)` へ
    // `0.999...9` を通すために必要）が `precision - scale` を超えた時点で
    // 打ち切って `OutOfRange` にする（丸め前に整数部だけで既に超過が確定する
    // ケースの早期終了。丸めによる繰り上がりは別途 fits_precision で再検査する）。
    let int_limit = precision.saturating_sub(scale);
    let mut int_value: i128 = 0;
    let mut int_digits: u32 = 0;
    let mut saw_int_digit = false;
    let mut leading_zero = true;
    while let Some(c) = reader.peek() {
        if c.is_ascii_digit() {
            saw_int_digit = true;
            let digit = i128::from(c - b'0');
            if leading_zero && digit == 0 {
                // 先頭ゼロは有効桁に数えない（`007` の `int_digits` は 1 の
                // まま）。
            } else {
                leading_zero = false;
                int_digits += 1;
                if int_digits > int_limit as u32 {
                    return Err(NumericError::OutOfRange);
                }
            }
            int_value = int_value
                .checked_mul(10)
                .and_then(|v| v.checked_add(digit))
                .ok_or(NumericError::OutOfRange)?;
            reader.advance();
        } else {
            break;
        }
    }

    let mut saw_frac_digit = false;
    let mut frac_value: i128 = 0;
    let mut round_up = false;
    if reader.peek() == Some(b'.') {
        reader.advance();
        let mut frac_digits: u32 = 0;
        while let Some(c) = reader.peek() {
            if c.is_ascii_digit() {
                saw_frac_digit = true;
                let digit = c - b'0';
                if frac_digits < scale as u32 {
                    frac_value = frac_value
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(i128::from(digit)))
                        .ok_or(NumericError::OutOfRange)?;
                } else if frac_digits == scale as u32 {
                    // scale+1 桁目だけを丸め方向の判定に使う（half away from
                    // zero）。それ以降は数字かどうかの妥当性検査だけ行う。
                    round_up = digit >= 5;
                }
                frac_digits += 1;
                reader.advance();
            } else {
                break;
            }
        }
        // 列の scale より入力桁が少ない場合はゼロ埋め相当（既に frac_value は
        // 0 埋め済みの値になっている必要がある）。
        if frac_digits < scale as u32 {
            let missing = scale as u32 - frac_digits;
            let mul = pow10(missing.min(u32::from(MAX_PRECISION)) as u8).ok_or(
                NumericError::Malformed("scale exceeds supported precision".to_string()),
            )?;
            frac_value = frac_value
                .checked_mul(mul)
                .ok_or(NumericError::OutOfRange)?;
        }
    }

    if !saw_int_digit && !saw_frac_digit {
        return Err(NumericError::Malformed("literal has no digits".to_string()));
    }
    // 未消費のバイトが残っていれば文法違反（空白・指数表記・複数の `.` 等）。
    if reader.peek().is_some() {
        return Err(NumericError::Malformed(
            "literal contains unsupported trailing characters".to_string(),
        ));
    }

    let scale_mul = pow10(scale).ok_or(NumericError::OutOfRange)?;
    let mut unscaled = int_value
        .checked_mul(scale_mul)
        .and_then(|v| v.checked_add(frac_value))
        .ok_or(NumericError::OutOfRange)?;
    if round_up {
        unscaled = unscaled.checked_add(1).ok_or(NumericError::OutOfRange)?;
    }
    if negative && unscaled != 0 {
        unscaled = -unscaled;
    }

    // `scale` は呼び出し元（カタログ経由の列定義）で `0..=MAX_PRECISION` に
    // 検証済みのはずだが、`from_parts` の fail-closed 契約に合わせ `?` で
    // 伝播する（本関数はもともと `Result` を返すため追加コストはない）。
    let value = Decimal::from_parts(unscaled, scale)?;
    // 丸め後の繰り上がりで桁あふれし得るため、最終値で改めて検査する
    // （D4: `999.995` → 丸め後 `1000.00` は `22003`）。
    if !value.fits_precision(precision) {
        return Err(NumericError::OutOfRange);
    }
    Ok(value)
}

/// `AVG(NUMERIC(p,s))`（TABLE-13・SQL-13、Issue #892・D6）向けの長除算。
/// `sum`（`from_scale` 桁の unscaled 累積値）を `count`（非 NULL 行数、`> 0`）で
/// 割り、結果を `to_scale`（`>= from_scale`）桁の unscaled 整数として返す。
///
/// `sum * 10^(to_scale - from_scale)` を素直に計算すると `to_scale` が大きい
/// 場合に中間値が `i128` を超えて桁あふれし得るため、1 桁ずつ商を確定する
/// 長除算方式を取る（各ステップの剰余は常に `count` 未満 = `u64::MAX` 以下の
/// ため、`remainder * 10` は `i128`/`u128` の範囲内に収まり桁あふれしない）。
/// 最後の剰余で丸め方向を判定する（half away from zero。
/// [`parse_for_column`] の丸め規約と同じ）。
///
/// `count == 0` または `to_scale < from_scale` は呼び出し元
/// （`sql::aggregate::Accumulator::finish`）の不変条件違反であり、
/// [`NumericError::Malformed`] を返す（到達しない想定の防御）。
pub(crate) fn avg_unscaled(
    sum: i128,
    count: u64,
    from_scale: u8,
    to_scale: u8,
) -> Result<i128, NumericError> {
    if count == 0 {
        return Err(NumericError::Malformed(
            "AVG divisor must not be zero".to_string(),
        ));
    }
    if to_scale < from_scale {
        return Err(NumericError::Malformed(
            "AVG result scale must not be smaller than the input scale".to_string(),
        ));
    }

    let negative = sum < 0;
    let divisor: u128 = u128::from(count);
    let mut remainder: u128 = sum.unsigned_abs();
    let mut quotient: u128 = remainder / divisor;
    remainder %= divisor;

    let extra_scale = u32::from(to_scale - from_scale);
    for _ in 0..extra_scale {
        // `remainder < divisor <= u64::MAX` なので `remainder * 10` は
        // `u128` に必ず収まる（桁あふれしない）。
        remainder *= 10;
        let digit = remainder / divisor;
        remainder %= divisor;
        quotient = quotient
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or(NumericError::OutOfRange)?;
    }

    // 同じ理由（`remainder < divisor <= u64::MAX`）で `remainder * 2` も
    // 桁あふれしない。half away from zero: 剰余が除数の半分以上なら切り上げる。
    if remainder * 2 >= divisor {
        quotient = quotient.checked_add(1).ok_or(NumericError::OutOfRange)?;
    }

    let unscaled = i128::try_from(quotient).map_err(|_| NumericError::OutOfRange)?;
    Ok(if negative && unscaled != 0 {
        -unscaled
    } else {
        unscaled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(unscaled: i128, scale: u8) -> Decimal {
        Decimal::from_parts(unscaled, scale).expect("test scale must be within MAX_PRECISION")
    }

    #[test]
    fn parses_basic_values() {
        assert_eq!(parse_for_column("1.50", 5, 2), Ok(d(150, 2)));
        assert_eq!(parse_for_column("-1.50", 5, 2), Ok(d(-150, 2)));
        assert_eq!(parse_for_column("+1.50", 5, 2), Ok(d(150, 2)));
        assert_eq!(parse_for_column("0", 5, 2), Ok(d(0, 2)));
        assert_eq!(parse_for_column("0.0", 5, 2), Ok(d(0, 2)));
    }

    #[test]
    fn accepts_leading_and_trailing_dot_forms() {
        assert_eq!(parse_for_column(".5", 5, 2), Ok(d(50, 2)));
        assert_eq!(parse_for_column("5.", 5, 2), Ok(d(500, 2)));
    }

    #[test]
    fn pads_short_fraction_with_zeros() {
        assert_eq!(parse_for_column("1.5", 5, 2), Ok(d(150, 2)));
        assert_eq!(parse_for_column("1", 5, 2), Ok(d(100, 2)));
    }

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(parse_for_column("1.005", 5, 2), Ok(d(101, 2)));
        assert_eq!(parse_for_column("-1.005", 5, 2), Ok(d(-101, 2)));
        assert_eq!(parse_for_column("1.004", 5, 2), Ok(d(100, 2)));
        assert_eq!(parse_for_column("1.999", 5, 2), Ok(d(200, 2)));
    }

    #[test]
    fn rounding_carry_can_overflow() {
        // NUMERIC(5,2) の最大は 999.99。丸め後 1000.00 になる入力は桁あふれ。
        assert_eq!(
            parse_for_column("999.995", 5, 2),
            Err(NumericError::OutOfRange)
        );
    }

    #[test]
    fn rejects_malformed_literals() {
        for bad in [
            "", "+", "-", "abc", "1.2.3", " 1", "1 ", "1e3", "1E3", "NaN", "Infinity", ".",
        ] {
            assert!(
                matches!(
                    parse_for_column(bad, 10, 2),
                    Err(NumericError::Malformed(_))
                ),
                "expected Malformed for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_overlong_literal() {
        let too_long = "1".repeat(MAX_LITERAL_LEN + 1);
        assert_eq!(
            parse_for_column(&too_long, 38, 0),
            Err(NumericError::Malformed("literal too long".to_string()))
        );
    }

    #[test]
    fn boundary_precision_38_0() {
        let max = "9".repeat(38);
        let parsed = parse_for_column(&max, 38, 0).expect("max value must fit");
        assert_eq!(
            parsed.unscaled(),
            99_999_999_999_999_999_999_999_999_999_999_999_999i128
        );
        let neg = format!("-{max}");
        let parsed_neg = parse_for_column(&neg, 38, 0).expect("min value must fit");
        assert_eq!(
            parsed_neg.unscaled(),
            -99_999_999_999_999_999_999_999_999_999_999_999_999i128
        );

        let overflow = "1".to_string() + &"0".repeat(38);
        assert_eq!(
            parse_for_column(&overflow, 38, 0),
            Err(NumericError::OutOfRange)
        );
    }

    #[test]
    fn boundary_precision_38_38() {
        let frac = "9".repeat(38);
        let text = format!("0.{frac}");
        let parsed = parse_for_column(&text, 38, 38).expect("scale 38 boundary must fit");
        assert_eq!(parsed.scale(), 38);
    }

    #[test]
    fn boundary_precision_1_0() {
        assert_eq!(parse_for_column("9", 1, 0), Ok(d(9, 0)));
        assert_eq!(parse_for_column("10", 1, 0), Err(NumericError::OutOfRange));
        assert_eq!(parse_for_column("-10", 1, 0), Err(NumericError::OutOfRange));
    }

    #[test]
    fn negative_zero_normalizes() {
        let v = parse_for_column("-0.00", 5, 2).expect("negative zero literal must parse");
        assert_eq!(v.unscaled(), 0);
        assert_eq!(v.to_string(), "0.00");
    }

    #[test]
    fn display_formats_canonical_text() {
        assert_eq!(d(150, 2).to_string(), "1.50");
        assert_eq!(d(-150, 2).to_string(), "-1.50");
        assert_eq!(d(-50, 2).to_string(), "-0.50");
        assert_eq!(d(-1, 2).to_string(), "-0.01");
        assert_eq!(d(-100, 2).to_string(), "-1.00");
        assert_eq!(d(0, 2).to_string(), "0.00");
        assert_eq!(d(5, 0).to_string(), "5");
    }

    #[test]
    fn fits_precision_boundary_is_strict() {
        // NUMERIC(2,0) は -99..=99。100 は収まらない。
        assert!(d(99, 0).fits_precision(2));
        assert!(!d(100, 0).fits_precision(2));
        assert!(!d(-100, 0).fits_precision(2));
    }

    #[test]
    fn from_parts_rejects_out_of_range_scale() {
        // 呼び出し元がカタログ検証（`0..=MAX_PRECISION`）を経由せず
        // `MAX_PRECISION` 超過の `scale` を渡した場合、以前の実装は
        // `MAX_PRECISION` へ暗黙に丸めていたが、これは `from_parts(1, 39)`
        // が本来表す `10^-39` を検証なしに別の数値（`10^-38`）へ変えてしまう
        // 問題があった（P1 指摘対応）。丸めず明示的に拒否することを固定する。
        assert_eq!(Decimal::from_parts(1, 39), Err(NumericError::OutOfRange));
        assert_eq!(
            Decimal::from_parts(5, u8::MAX),
            Err(NumericError::OutOfRange)
        );
        // 境界値（`MAX_PRECISION` ちょうど）は受理される。
        assert!(Decimal::from_parts(1, MAX_PRECISION).is_ok());
    }

    // --- avg_unscaled（D6・Issue #892） -------------------------------------

    #[test]
    fn avg_unscaled_exact_division_needs_no_rounding() {
        // 1.00 + 2.00 = 3.00（sum=300, scale=2）を 2 件で割ると 1.50。
        assert_eq!(avg_unscaled(300, 2, 2, 4), Ok(15000));
    }

    #[test]
    fn avg_unscaled_rounds_half_away_from_zero() {
        // 0.01 / 3 = 0.003333... を scale 4 まで求めると 0.0033（切り捨て側）。
        assert_eq!(avg_unscaled(1, 3, 2, 4), Ok(33));
        // 0.05 / 2 = 0.025 を scale 2 で丸めると 0.03（4 捨 5 入ではなく
        // 5 は必ず切り上げる half away from zero）。
        assert_eq!(avg_unscaled(5, 2, 2, 2), Ok(3));
    }

    #[test]
    fn avg_unscaled_rounds_negative_values_away_from_zero() {
        // -0.05 / 2 = -0.025 → 絶対値が大きい方（-0.03）へ丸める。
        assert_eq!(avg_unscaled(-5, 2, 2, 2), Ok(-3));
    }

    #[test]
    fn avg_unscaled_same_scale_is_plain_division_with_rounding() {
        assert_eq!(avg_unscaled(10, 4, 0, 0), Ok(3));
        assert_eq!(avg_unscaled(10, 3, 0, 0), Ok(3));
    }

    #[test]
    fn avg_unscaled_rejects_zero_count() {
        assert!(matches!(
            avg_unscaled(1, 0, 2, 4),
            Err(NumericError::Malformed(_))
        ));
    }

    #[test]
    fn avg_unscaled_rejects_shrinking_scale() {
        assert!(matches!(
            avg_unscaled(1, 1, 4, 2),
            Err(NumericError::Malformed(_))
        ));
    }

    #[test]
    fn avg_unscaled_large_sum_does_not_overflow_intermediate_math() {
        // sum が大きく `sum * 10^(to_scale-from_scale)` を素直に計算すると
        // i128 を超える組み合わせでも、1 桁ずつ商を確定する長除算（各ステップの
        // 剰余は常に `count` 未満）のため、最終結果が 38 桁以内に収まる限り
        // オーバーフローしない（D6 のコメント参照）。
        let large_sum = 12_345_678_901_234_567_890i128; // 20 桁
        assert!(avg_unscaled(large_sum, 7, 0, 16).is_ok());
    }

    #[test]
    fn avg_unscaled_overflow_is_rejected() {
        // 除算結果が 38 桁を超える場合、桁を積み上げる過程で `checked_mul`/
        // `checked_add` が失敗し `OutOfRange` になる。
        let near_max = 99_999_999_999_999_999_999_999_999_999_999_999_999i128;
        assert_eq!(
            avg_unscaled(near_max, 1, 0, 30),
            Err(NumericError::OutOfRange)
        );
    }
}
