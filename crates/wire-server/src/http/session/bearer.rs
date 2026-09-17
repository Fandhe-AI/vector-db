//! `Authorization: Bearer <token>` ヘッダの受信データ経路（Issue #753・
//! TASK-174・HTTP-8。関連 HTTP-5・HTTP-6。ポインタ: `docs/spec/05-tasks.md`
//! TASK-174・`docs/spec/04-behavior/http-transport.md` HTTP-8）。
//!
//! 呼び出し文脈: [`crate::http::session::close::handle`] が要求本文を読む前に
//! [`extract_bearer_token`] を呼び、`Authorization` ヘッダから
//! [`crate::http::session::token::SessionToken`] を取り出す。
//! `POST /v1/query` 前段の認証ミドルウェア（`session::middleware::
//! authenticate`。Issue #754）も本モジュールを再利用する共有 seam であり、
//! `close` モジュールへインライン化しない。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセス（`[]`）を用いず、
//! [`crate::http::headers::Headers::get_single`]・`from_utf8`・
//! [`crate::http::session::token::SessionToken::parse`] の順にフォリブルに
//! 処理する。
//!
//! ## 失敗の集約（存在オラクル非公開設計）
//!
//! ヘッダ欠落・重複・スキーム不一致・トークン長不正・base64url 不正・
//! 非正準表現のいずれも、区別可能な情報を一切外へ出さず
//! [`ErrorClass::AuthRequired`]（`28000`）へ一様に写像する。`client_message`
//! は variant によらず単一の固定文言を返す。重複 `Authorization` ヘッダを
//! `08P01`（`headers.rs` の一般則）ではなく本モジュールの `28000` として
//! 扱うのは、`Authorization` の受理判定はすべて認証エラーへ収束させ、
//! `POST /v1/query` 前段ミドルウェア（#754）とも単一の出口を共有するための
//! 意図的な差別化である。

use crate::http::headers::Headers;
use crate::http::session::token::{SessionToken, TokenError};
use engine::error_format::ErrorClass;

/// クライアントへ返す固定文言（`AuthRequired` の全 variant で共通。
/// `crate::auth::AuthFailure::MESSAGE` と同じ「対称性のため区別しない」設計）。
pub const MESSAGE: &str = "authentication required";

/// [`extract_bearer_token`] の拒否理由。いずれも [`ErrorClass::AuthRequired`]
/// （`28000`）へ写像し、`client_message()` は [`MESSAGE`] の固定文言を返す
/// （internal な理由の違いをクライアントへ一切反映しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerError {
    /// `Authorization` ヘッダが無い。
    Missing,
    /// `Authorization` ヘッダが 2 件以上あった
    /// （[`Headers::get_single`] が `Malformed` を返した）。
    Duplicate,
    /// スキームが `Bearer`（大文字小文字非区別）でない、またはスキームの
    /// 後にトークンが続かない。
    Scheme,
    /// スキーム部が可視 ASCII として UTF-8 に復号できない
    /// （[`Headers`] は値をバイト列でしか検証していないため、本関数側で
    /// 改めて `from_utf8` を試みる）。
    Encoding,
    /// トークン部の [`SessionToken::parse`] が失敗した。
    Token(TokenError),
}

impl BearerError {
    /// この失敗の分類（常に [`ErrorClass::AuthRequired`]）。
    pub const fn error_class(&self) -> ErrorClass {
        ErrorClass::AuthRequired
    }

    /// クライアントへ返す固定文言（常に [`MESSAGE`]）。
    pub const fn client_message(&self) -> &'static str {
        MESSAGE
    }
}

/// `headers` から `Authorization: Bearer <token>` を取り出し
/// [`SessionToken`] へ変換する。
///
/// 手順（fail-closed。優先順）:
/// 1. [`Headers::get_single`] でヘッダ欠落・重複を判定（欠落は
///    [`BearerError::Missing`]、重複は [`BearerError::Duplicate`]）
/// 2. 値が可視 ASCII の UTF-8 として復号できることを確認
///    （[`BearerError::Encoding`]。[`Headers`] の値検証は可視 ASCII/HTAB の
///    バイト集合までのため、有効な UTF-8 の保証は本関数が担う）
/// 3. 先頭トークンを `eq_ignore_ascii_case(b"bearer")` で照合し
///    （[`BearerError::Scheme`]）、1 個以上の SP（RFC 9110 `1*SP`）を
///    スキップする
/// 4. 残り全体（前後の OWS はヘッダ側で既にトリム済み）を
///    [`SessionToken::parse`] へ渡す（[`BearerError::Token`]）
pub fn extract_bearer_token(headers: &Headers<'_>) -> Result<SessionToken, BearerError> {
    let value = headers
        .get_single(b"authorization")
        .map_err(|_| BearerError::Duplicate)?
        .ok_or(BearerError::Missing)?;

    let text = std::str::from_utf8(value).map_err(|_| BearerError::Encoding)?;

    let sp_idx = text.find(' ').ok_or(BearerError::Scheme)?;
    let scheme = text.get(..sp_idx).ok_or(BearerError::Scheme)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(BearerError::Scheme);
    }

    let after_scheme = text.get(sp_idx..).ok_or(BearerError::Scheme)?;
    let token_text = after_scheme.trim_start_matches(' ');
    if token_text.is_empty() {
        // `1*SP` の後にトークンが続かない（スキームのみ、または SP のみ）。
        return Err(BearerError::Scheme);
    }

    SessionToken::parse(token_text).map_err(BearerError::Token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::headers::{parse_headers, HeaderParse};

    // テスト専用: `Headers<'a>` の借用元をテスト関数のスコープより長生きさせる
    // ための意図的なリーク（本番コードには存在しない。単体テストのみで使う）。
    fn leak(v: Vec<u8>) -> &'static [u8] {
        Box::leak(v.into_boxed_slice())
    }

    fn headers_from(raw: &[u8]) -> Headers<'static> {
        let mut input = raw.to_vec();
        input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        match parse_headers(leak(input)).expect("header parse should succeed") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn valid_token() -> SessionToken {
        SessionToken::generate().expect("urandom available in test environment")
    }

    #[test]
    fn missing_header_rejects_with_missing() {
        let headers = headers_from(b"");
        assert_eq!(extract_bearer_token(&headers), Err(BearerError::Missing));
    }

    #[test]
    fn duplicate_header_rejects_with_duplicate() {
        let token = valid_token().encoded();
        let raw = format!("Authorization: Bearer {token}\r\nAuthorization: Bearer {token}\r\n");
        let headers = headers_from(raw.as_bytes());
        assert_eq!(extract_bearer_token(&headers), Err(BearerError::Duplicate));
    }

    #[test]
    fn non_bearer_scheme_rejects_with_scheme() {
        let raw = b"Authorization: Basic dXNlcjpwYXNz\r\n";
        let headers = headers_from(raw);
        assert_eq!(extract_bearer_token(&headers), Err(BearerError::Scheme));
    }

    #[test]
    fn bearer_without_token_rejects_with_scheme() {
        let raw = b"Authorization: Bearer\r\n";
        let headers = headers_from(raw);
        assert_eq!(extract_bearer_token(&headers), Err(BearerError::Scheme));
    }

    #[test]
    fn bearer_lowercase_scheme_is_accepted() {
        let token = valid_token();
        let raw = format!("Authorization: bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());
        assert_eq!(extract_bearer_token(&headers), Ok(token));
    }

    #[test]
    fn bearer_with_multiple_spaces_is_accepted() {
        let token = valid_token();
        let raw = format!("Authorization: Bearer  {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());
        assert_eq!(extract_bearer_token(&headers), Ok(token));
    }

    #[test]
    fn bearer_with_trailing_garbage_rejects_with_token_error() {
        let token = valid_token();
        let raw = format!("Authorization: Bearer {} extra\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());
        assert!(matches!(
            extract_bearer_token(&headers),
            Err(BearerError::Token(TokenError::InvalidLength))
        ));
    }

    #[test]
    fn token_too_short_rejects_with_token_error() {
        let raw = b"Authorization: Bearer AAAA\r\n";
        let headers = headers_from(raw);
        assert!(matches!(
            extract_bearer_token(&headers),
            Err(BearerError::Token(TokenError::InvalidLength))
        ));
    }

    #[test]
    fn token_too_long_rejects_with_token_error() {
        let token = valid_token().encoded();
        let raw = format!("Authorization: Bearer {token}A\r\n");
        let headers = headers_from(raw.as_bytes());
        assert!(matches!(
            extract_bearer_token(&headers),
            Err(BearerError::Token(TokenError::InvalidLength))
        ));
    }

    #[test]
    fn token_with_padding_rejects_with_token_error() {
        // 43 文字ちょうどだが `=` パディングを含む（正しいトークンとは異なる
        // 別文字列に差し替え、base64url アルファベット外の文字で拒否される
        // ことを確認する）。
        let bad = "=".repeat(43);
        let raw = format!("Authorization: Bearer {bad}\r\n");
        let headers = headers_from(raw.as_bytes());
        assert!(matches!(
            extract_bearer_token(&headers),
            Err(BearerError::Token(TokenError::Encoding(_)))
        ));
    }

    #[test]
    fn valid_token_round_trips() {
        let token = valid_token();
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());
        assert_eq!(extract_bearer_token(&headers), Ok(token));
    }

    #[test]
    fn all_variants_share_the_same_error_class_and_message() {
        let variants = [
            BearerError::Missing,
            BearerError::Duplicate,
            BearerError::Scheme,
            BearerError::Encoding,
            BearerError::Token(TokenError::InvalidLength),
        ];
        for variant in variants {
            assert_eq!(variant.error_class(), ErrorClass::AuthRequired);
            assert_eq!(variant.client_message(), MESSAGE);
        }
    }
}
