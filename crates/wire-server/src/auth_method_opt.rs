//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--auth-method`
//! opt-in CLI 引数の閉じた語彙パーサ（Issue #940・WIRE-18・TASK-222）。
//!
//! `--durability`（`durability_opt`）・`--search-engine`（`search_engine_opt`）
//! と同型の「プロセス起動時にのみ明示指定する注入点」であり、untrusted な
//! CLI 文字列から `crate::auth::AuthMethod` へ到達する唯一の入口を本モジュール
//! に置く。認証方式はサーバー全体で 1 つに固定し、ユーザーごとには切り替えない
//! （`Authentication*` の種類自体からユーザーの存在・方式が判別できて
//! しまうのを避ける設計判断）。
//!
//! 未指定は [`crate::auth::AuthMethod::default`]（`Cleartext`）のままビット
//! 同一（既存の全テスト・全 wire 応答が本 Issue 以前と不変）。

use crate::auth::AuthMethod;

/// `--auth-method` の CLI フラグ名。
pub const FLAG: &str = "--auth-method";

/// `--scram-mock-key-file` の CLI フラグ名（Issue #940 PR #1006・P0 是正）。
/// `--auth-method scram-sha-256` を選んだときのみ必須の opt-in。値は
/// ファイルパスで、その生バイト列（32 バイト以上）を
/// [`crate::auth::UserStore::require_scram`] のモック鍵導出用秘密として渡す。
/// ユーザーストアの内容（ServerKey・行順）から独立した秘密であることが
/// 唯一の契約であり、`--auth-method cleartext`（既定）との組合せ・値欠落・
/// 短すぎるファイル・2 回目以降の指定はいずれも fail-closed（`main.rs` 側で
/// 検証する。本モジュールはフラグ名の単一情報源のみを持つ）。
pub const SCRAM_MOCK_KEY_FILE_FLAG: &str = "--scram-mock-key-file";

/// [`SCRAM_MOCK_KEY_FILE_FLAG`] が受理するファイル内容の最小バイト数
/// （`scram::KEY_LEN` と同値。十分なエントロピーを要求する）。
pub const SCRAM_MOCK_KEY_FILE_MIN_LEN: usize = crate::auth::scram::KEY_LEN;

/// `--auth-method` が受理する語彙（順序はエラーメッセージの一覧順・
/// README 記載順の単一情報源）。
pub const TOKENS: [&str; 2] = ["cleartext", "scram-sha-256"];

/// `raw`（CLI 引数の値）を [`TOKENS`] の厳密一致でのみ受理する
/// （`durability_opt::parse` と同じ「厳密一致のみ受理」方針）。
pub fn parse(raw: &str) -> Result<AuthMethod, String> {
    match raw {
        "cleartext" => Ok(AuthMethod::Cleartext),
        "scram-sha-256" => Ok(AuthMethod::ScramSha256),
        other => Err(format!("{FLAG} must be one of {TOKENS:?} (got {other:?})")),
    }
}

/// [`TOKENS`] のうち `m` に対応する文字列表現（診断メッセージ用）。
pub fn token_for(m: AuthMethod) -> &'static str {
    match m {
        AuthMethod::Cleartext => "cleartext",
        AuthMethod::ScramSha256 => "scram-sha-256",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_all_tokens() {
        for tok in TOKENS {
            assert!(parse(tok).is_ok(), "expected {tok:?} to be accepted");
        }
    }

    #[test]
    fn parse_cleartext_maps_to_default() {
        assert_eq!(parse("cleartext"), Ok(AuthMethod::Cleartext));
        assert_eq!(AuthMethod::Cleartext, AuthMethod::default());
    }

    #[test]
    fn parse_scram_sha_256_maps_to_variant() {
        assert_eq!(parse("scram-sha-256"), Ok(AuthMethod::ScramSha256));
    }

    #[test]
    fn parse_rejects_case_variants_and_whitespace() {
        for raw in [
            "Cleartext",
            "CLEARTEXT",
            " scram-sha-256",
            "scram-sha-256 ",
            "SCRAM-SHA-256",
            "",
            "scram",
            "cleartext\n",
        ] {
            assert!(
                parse(raw).is_err(),
                "expected {raw:?} to be rejected (strict match only)"
            );
        }
    }

    #[test]
    fn parse_rejects_control_character_injection() {
        let err = parse("cleartext\0bogus").expect_err("must reject control characters");
        assert!(err.contains(FLAG));
    }

    #[test]
    fn token_round_trips_through_parse() {
        for tok in TOKENS {
            let m = parse(tok).expect("valid token");
            assert_eq!(token_for(m), tok);
        }
    }
}
