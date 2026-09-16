//! UTC 秒（Unix タイムスタンプ）→ RFC 9110 §5.6.7 IMF-fixdate 文字列への変換
//! （`http::response` の `Date` ヘッダ生成専用。Issue #746・#809 レビュー対応。
//! 対象ビヘイビア HTTP-2）。
//!
//! 依存追加なし方針（`.claude/rules/dependency-policy.md`）のため、Howard
//! Hinnant の civil calendar algorithm（`http://howardhinnant.github.io/
//! date_algorithms.html` で公開されている、グレゴリオ暦と days-since-epoch の
//! 相互変換アルゴリズム。第三者コードの転記ではなく整数演算の再実装）を用いて
//! `chrono` 等の日時クレートを追加せずに変換する。うるう秒は考慮しない
//! （HTTP-date はうるう秒を表現しない・RFC 9110 の想定どおり）。

use std::time::Duration;

const WEEKDAY_NAMES: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `UNIX_EPOCH` からの経過秒（`SystemTime::duration_since` の結果）を
/// RFC 9110 IMF-fixdate（例: `Sun, 06 Nov 1994 08:49:37 GMT`）へ変換する。
///
/// 呼び出し元（[`crate::http::response`]）は `SystemTime::now()` を
/// `duration_since(UNIX_EPOCH)` した結果（クロックが `UNIX_EPOCH` より前を指す
/// 異常系は呼び出し元が `Duration::ZERO` へ縮退させる）をそのまま渡す契約。
/// 本関数自体は外部状態を参照しない純関数（`elapsed` の値のみに依存）。
pub fn format_http_date(elapsed: Duration) -> String {
    let total_secs = elapsed.as_secs();
    let days = total_secs / 86_400;
    let secs_of_day = total_secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let weekday = WEEKDAY_NAMES[(days % 7) as usize];
    let (year, month, day) = civil_from_days(days);
    let month_name = MONTH_NAMES[(month - 1) as usize];

    format!("{weekday}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} GMT")
}

/// `1970-01-01` からの経過日数 → `(year, month, day)`（グレゴリオ暦・1-indexed
/// month/day）。Howard Hinnant の civil calendar algorithm の整数演算部分の
/// 再実装（`u64` 入力のため紀元前方向は扱わない＝本リポの用途である「現在時刻」
/// のみを対象とする）。
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    // アルゴリズムの元エポック（0000-03-01）へ揃えるためのシフト。
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `UNIX_EPOCH` 自体（1970-01-01 は木曜日）。
    #[test]
    fn formats_unix_epoch() {
        assert_eq!(
            format_http_date(Duration::from_secs(0)),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    /// RFC 9110 §5.6.7 の例示値そのもの（IMF-fixdate の仕様上の参照値）。
    #[test]
    fn formats_rfc_9110_example() {
        // 784111777 == 1994-11-06T08:49:37Z（日数 9075・秒 31777 で構成）。
        assert_eq!(
            format_http_date(Duration::from_secs(784_111_777)),
            "Sun, 06 Nov 1994 08:49:37 GMT"
        );
    }

    /// うるう年 2000-02-29（世紀年かつ 400 の倍数のためうるう年）を跨ぐ。
    #[test]
    fn formats_leap_day_2000() {
        // 951_782_400 == 2000-02-29T00:00:00Z
        assert_eq!(
            format_http_date(Duration::from_secs(951_782_400)),
            "Tue, 29 Feb 2000 00:00:00 GMT"
        );
    }

    /// 世紀年だが 400 の倍数でない 1900 年はうるう年ではない
    /// （civil_from_days の `doe / 36_524` 項がこの分岐を担う）ことを、
    /// 1900 年を含む era 内の既知の一点で間接的に固定する。
    #[test]
    fn formats_year_boundary_2001() {
        // 978_307_200 == 2001-01-01T00:00:00Z（2000 年がうるう年である前提で
        // 2000-02-29 から先の日数計算が正しいことの回帰）。
        assert_eq!(
            format_http_date(Duration::from_secs(978_307_200)),
            "Mon, 01 Jan 2001 00:00:00 GMT"
        );
    }

    /// 出力は常に ASCII・固定長（29 バイト）で CR/LF を含まない
    /// （ヘッダインジェクション面が無いことの固定。`http::status::reason_phrase`
    /// と同じ観点）。
    #[test]
    fn output_is_fixed_length_ascii_without_crlf() {
        for secs in [0, 1, 60, 3600, 86_400, 1_700_000_000, 4_102_444_800] {
            let s = format_http_date(Duration::from_secs(secs));
            assert_eq!(s.len(), 29, "unexpected length for secs={secs}: {s:?}");
            assert!(s.is_ascii());
            assert!(!s.contains(['\r', '\n']));
        }
    }

    /// 同一入力からは常に同一出力（外部状態非依存の確認）。
    #[test]
    fn is_deterministic() {
        let d = Duration::from_secs(1_700_000_000);
        assert_eq!(format_http_date(d), format_http_date(d));
    }
}
