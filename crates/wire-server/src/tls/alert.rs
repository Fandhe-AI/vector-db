//! TLS 1.3 alert メッセージ（RFC 8446 §6）の parse／serialize と、
//! 受信 alert の分類（TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #965・
//! 親 #941）。
//!
//! 責務は「alert レコードの 2 バイト本体（level・description）の
//! parse／serialize」と「受信した alert が正常終了（`close_notify`／
//! `user_canceled`）かエラーかの分類」のみに限定する。alert の実送出・
//! ハンドシェイク状態機械への統合は [`super::server_handshake`] が担う。
//!
//! RFC 8446 §5.1 は alert メッセージの分割・結合を禁止しており、
//! 1 レコードの fragment は必ず 2 バイトちょうどでなければならない。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを用いず `get()`
//! で処理する（`.claude/rules/coding-rust.md` P0）。`unsafe` は使わない。

use super::record::AlertDescription;

/// alert の `AlertLevel`（RFC 8446 §6）。TLS 1.3 は仕様上すべての alert を
/// fatal として扱うことを要求するが、ワイヤ上の値そのものは 1（warning）・
/// 2（fatal）のいずれかでなければ構造違反として拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertLevel {
    Warning,
    Fatal,
}

impl AlertLevel {
    fn as_u8(self) -> u8 {
        match self {
            AlertLevel::Warning => 1,
            AlertLevel::Fatal => 2,
        }
    }

    fn try_from(value: u8) -> Option<Self> {
        match value {
            1 => Some(AlertLevel::Warning),
            2 => Some(AlertLevel::Fatal),
            _ => None,
        }
    }
}

/// parse 済みの alert メッセージ。`description` は受信した生の u8 を
/// そのまま保持する（未知の値を失わないため。[`AlertDescription`] の
/// 閉じた語彙へは変換しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alert {
    pub level: AlertLevel,
    pub description: u8,
}

impl Alert {
    /// alert レコードの fragment（ちょうど 2 バイト）を parse する。
    /// 長さ違反・level 値の違反はいずれも `decode_error`
    /// （RFC 8446 §5.1・§6）。
    pub fn parse(fragment: &[u8]) -> Result<Self, AlertDescription> {
        let bytes: &[u8; 2] = fragment
            .try_into()
            .map_err(|_| AlertDescription::DecodeError)?;
        let [level_byte, description] = *bytes;
        let level = AlertLevel::try_from(level_byte).ok_or(AlertDescription::DecodeError)?;
        Ok(Alert { level, description })
    }

    /// 2 バイトへ直列化する。
    pub fn to_bytes(self) -> [u8; 2] {
        [self.level.as_u8(), self.description]
    }

    /// fatal alert（RFC 8446 §6.2 の定型コード）を組み立てる。
    pub fn encode_fatal(description: AlertDescription) -> [u8; 2] {
        Alert {
            level: AlertLevel::Fatal,
            description: description.as_u8(),
        }
        .to_bytes()
    }

    /// `close_notify`（RFC 8446 §6.1）を組み立てる。
    pub fn close_notify() -> [u8; 2] {
        Alert {
            level: AlertLevel::Warning,
            description: AlertDescription::CloseNotify.as_u8(),
        }
        .to_bytes()
    }
}

/// 受信した alert の分類（RFC 8446 §6.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceivedAlert {
    /// `close_notify`／`user_canceled`。正常終了として扱う。
    Closed,
    /// 上記以外（未知の description を含む）。エラー alert として扱う。
    Fatal(u8),
}

/// 受信した alert を分類する（level は分類に用いない。RFC 8446 は
/// すべての alert を fatal として扱うことを要求するため、warning として
/// 届いた `close_notify` も正常終了として扱う契約）。
pub fn classify_received(alert: Alert) -> ReceivedAlert {
    match alert.description {
        d if d == AlertDescription::CloseNotify.as_u8() => ReceivedAlert::Closed,
        d if d == AlertDescription::UserCanceled.as_u8() => ReceivedAlert::Closed,
        other => ReceivedAlert::Fatal(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_byte_fatal_alert() {
        let alert = Alert::parse(&[2, 10]).expect("valid alert");
        assert_eq!(alert.level, AlertLevel::Fatal);
        assert_eq!(alert.description, 10);
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            Alert::parse(&[2]).expect_err("1 byte must be rejected"),
            AlertDescription::DecodeError
        );
        assert_eq!(
            Alert::parse(&[2, 10, 0]).expect_err("3 bytes must be rejected"),
            AlertDescription::DecodeError
        );
    }

    #[test]
    fn rejects_invalid_level() {
        assert_eq!(
            Alert::parse(&[0, 10]).expect_err("level 0 must be rejected"),
            AlertDescription::DecodeError
        );
        assert_eq!(
            Alert::parse(&[3, 10]).expect_err("level 3 must be rejected"),
            AlertDescription::DecodeError
        );
    }

    #[test]
    fn classifies_close_notify_and_user_canceled_as_closed() {
        let close = Alert::parse(&Alert::close_notify()).expect("valid alert");
        assert_eq!(classify_received(close), ReceivedAlert::Closed);

        let user_canceled = Alert {
            level: AlertLevel::Warning,
            description: AlertDescription::UserCanceled.as_u8(),
        };
        assert_eq!(classify_received(user_canceled), ReceivedAlert::Closed);
    }

    #[test]
    fn classifies_unknown_description_as_fatal() {
        let alert = Alert {
            level: AlertLevel::Fatal,
            description: 255,
        };
        assert_eq!(classify_received(alert), ReceivedAlert::Fatal(255));
    }

    #[test]
    fn encode_fatal_round_trips() {
        let bytes = Alert::encode_fatal(AlertDescription::HandshakeFailure);
        let parsed = Alert::parse(&bytes).expect("valid alert");
        assert_eq!(parsed.level, AlertLevel::Fatal);
        assert_eq!(
            parsed.description,
            AlertDescription::HandshakeFailure.as_u8()
        );
        assert_eq!(classify_received(parsed), ReceivedAlert::Fatal(40));
    }
}
