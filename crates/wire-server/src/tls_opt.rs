//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る TLS opt-in CLI
//! 引数（`--tls-cert`／`--tls-key`／`--tls-mode`／`--tls-scram-channel-
//! binding`）の閉じた語彙パーサとサーバー証明書・鍵の読み込み手順
//! （Issue #967・#970・親 #941・TASK-228。対象ビヘイビア WIRE-7, WIRE-9,
//! WIRE-18, HTTP-9, HTTP-10）。
//!
//! `--durability`（[`crate::durability_opt`]）・`--search-engine`
//! （[`crate::search_engine_opt`]）と同型の「プロセス起動時にのみ明示指定
//! する注入点」であり、`crate::tls::server_handshake::TlsServerConfig`
//! （Issue #965）へ untrusted な CLI 文字列・ファイルパスから到達する
//! 唯一の入口を本モジュールに置く。
//!
//! `--tls-mode` は TLS ハンドシェイク成立後の平文接続ポリシーではなく、
//! **`SSLRequest` を経ない平文 StartupMessage** を受理するか（`allow`）
//! 拒否するか（`require`）を選ぶ（`crate::handshake::
//! handle_connection_with_tls_mode` が参照する）。証明書・鍵ファイルの
//! バイト列・長さは本モジュールの外（ログ・エラーメッセージ）へ一切出さない。
//!
//! `--tls-scram-channel-binding`（`enable`／`disable`。既定 `disable`）は
//! SCRAM-SHA-256-PLUS（`p=tls-server-end-point`）を機構リストへ提示する
//! か（`TlsServerConfig::with_scram_channel_binding`。Issue #970）を CLI
//! から切り替える唯一の入口。`--tls-cert`／`--tls-key` を指定したときのみ
//! 意味を持ち、単独指定は `--tls-mode` と同じ理由で組合せ不正として
//! fail-closed 拒否する（`main::resolve_tls_options` 参照）。本サーバーが
//! 受理する唯一の葉鍵種別（Ed25519）に対し、`enable` を選ぶと libpq の
//! `channel_binding=prefer`／`require` が接続失敗しうる（`docs/design/
//! tls-channel-binding.md` の実測結果参照）ため既定は `disable`。

use std::path::Path;
use std::sync::Arc;

use crate::tls::ed25519::SigningKey;
use crate::tls::pkcs8::{self, PrivateKeyLoadError};
use crate::tls::server_handshake::{TlsServerConfig, TlsServerConfigError};
use crate::tls::x509::{self, ServerCertificateLoadError};

/// `--tls-cert` の CLI フラグ名。
pub const CERT_FLAG: &str = "--tls-cert";
/// `--tls-key` の CLI フラグ名。
pub const KEY_FLAG: &str = "--tls-key";
/// `--tls-mode` の CLI フラグ名。
pub const MODE_FLAG: &str = "--tls-mode";

/// `--tls-mode` が受理する語彙（順序は `parse` の分岐・エラーメッセージの
/// 一覧順・README 記載順の単一情報源。`durability_opt::TOKENS` と同じ流儀）。
pub const MODE_TOKENS: [&str; 2] = ["require", "allow"];

/// `--tls-scram-channel-binding` の CLI フラグ名（Issue #970）。
pub const SCRAM_CHANNEL_BINDING_FLAG: &str = "--tls-scram-channel-binding";

/// `--tls-scram-channel-binding` が受理する語彙（`MODE_TOKENS` と同じ流儀。
/// 既定は `disable` 相当。`main::resolve_tls_options` が未指定時にこの既定を
/// 適用する）。
pub const SCRAM_CHANNEL_BINDING_TOKENS: [&str; 2] = ["enable", "disable"];

/// `SSLRequest` を経ない平文 StartupMessage の受理ポリシー。
///
/// `#[non_exhaustive]` は将来 variant を追加する際、下流クレートの
/// exhaustive match を破壊しないための予防措置（`bind_guard::
/// TransportSecurity` と同じ方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsMode {
    /// `SSLRequest` を経ない平文 StartupMessage を `08P01` で拒否する
    /// （startup パラメータを解釈する前に拒否する。WIRE-9）。
    Require,
    /// 平文 StartupMessage も受理する（TLS 未設定時の既存経路と同じ
    /// 受理判定。非ループバック bind と組み合わせると `bind_guard` が
    /// 起動時に拒否する）。
    Allow,
}

impl TlsMode {
    /// [`MODE_TOKENS`] のうち `self` に対応する文字列表現（診断メッセージ用）。
    pub fn token(self) -> &'static str {
        match self {
            TlsMode::Require => "require",
            TlsMode::Allow => "allow",
        }
    }
}

/// `raw`（CLI 引数の値）を [`MODE_TOKENS`] の厳密一致でのみ受理する
/// （trim・大文字小文字の読み替えはしない。`durability_opt::parse`・
/// `search_engine_opt::parse` と同じ「厳密一致のみ受理」方針）。
pub fn parse(raw: &str) -> Result<TlsMode, String> {
    match raw {
        "require" => Ok(TlsMode::Require),
        "allow" => Ok(TlsMode::Allow),
        other => Err(format!(
            "{MODE_FLAG} must be one of {MODE_TOKENS:?} (got {other:?})"
        )),
    }
}

/// `raw`（CLI 引数の値）を [`SCRAM_CHANNEL_BINDING_TOKENS`] の厳密一致でのみ
/// 受理し、`TlsServerConfig::with_scram_channel_binding` へ渡す `bool` へ
/// 変換する（`parse` と同じ「厳密一致のみ受理」方針）。
pub fn parse_scram_channel_binding(raw: &str) -> Result<bool, String> {
    match raw {
        "enable" => Ok(true),
        "disable" => Ok(false),
        other => Err(format!(
            "{SCRAM_CHANNEL_BINDING_FLAG} must be one of {SCRAM_CHANNEL_BINDING_TOKENS:?} (got {other:?})"
        )),
    }
}

/// [`load_server_config`] の失敗理由。`Display` は各内部エラー型の内容
/// 非依存な `Display` へ委譲し、鍵・証明書の内容・長さは含めない
/// （呼び出し元がフラグ名を前置してエラーメッセージを組み立てる）。
#[derive(Debug)]
pub enum TlsConfigLoadError {
    Key(PrivateKeyLoadError),
    Clock,
    Certificate(ServerCertificateLoadError),
    Mismatch(TlsServerConfigError),
}

impl std::fmt::Display for TlsConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsConfigLoadError::Key(e) => write!(f, "{e}"),
            TlsConfigLoadError::Clock => {
                write!(f, "failed to read the current time for validity checks")
            }
            TlsConfigLoadError::Certificate(e) => write!(f, "{e}"),
            TlsConfigLoadError::Mismatch(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TlsConfigLoadError {}

/// `cert`（証明書チェーン PEM）・`key`（Ed25519 PKCS#8 秘密鍵 PEM）から
/// [`TlsServerConfig`] を構築する（Issue #967）。`PLUS` 提示は既定 `false`
/// （非提示）のまま構築する後方互換の薄いラッパーで、実体は
/// [`load_server_config_with_options`] に委譲する（codex-review PR #1087
/// P1 是正: 公開関数へ必須引数を追加すると既存呼び出し元を破壊するため、
/// 2 引数のシグネチャ自体は変えない）。
///
/// 手順（鍵を先に読むのは、葉証明書の公開鍵照合に公開鍵が要るため）:
/// 1. `key` から Ed25519 seed を読み、署名鍵を導出する。
/// 2. 現在時刻（エポック秒）を取得する。
/// 3. `cert` を検証付きで読む（署名鍵の公開鍵との一致・有効期限を含む）。
/// 4. `TlsServerConfig::new` で組み立てる（公開鍵の再照合は定数時間）。
///
/// `Ed25519Seed`／`SigningKey` は複製せず、鍵のバイト列・長さをエラー
/// メッセージへ出さない（`Ed25519Seed` の `Drop` ゼロ化を活かす）。
pub fn load_server_config(cert: &Path, key: &Path) -> Result<TlsServerConfig, TlsConfigLoadError> {
    load_server_config_with_options(cert, key, false)
}

/// [`load_server_config`] に `--tls-scram-channel-binding`（Issue #970）の
/// 解決済み値を追加で渡せる入口。`main.rs` はこちらを呼ぶ
/// （`handle_connection_with_options`・`accept_loop_with_tls_mode` 等、
/// 既存関数を「オプション追加時は `_with_options` を新設し、元の関数は
/// 既定値で委譲する後方互換ラッパーへ変える」流儀に合わせる）。
pub fn load_server_config_with_options(
    cert: &Path,
    key: &Path,
    scram_channel_binding: bool,
) -> Result<TlsServerConfig, TlsConfigLoadError> {
    let seed = pkcs8::load_ed25519_private_key_file(key).map_err(TlsConfigLoadError::Key)?;
    let signing_key = SigningKey::from_seed(&seed);
    let public_key = signing_key.public_key();

    let now = x509::current_unix_secs().map_err(|_| TlsConfigLoadError::Clock)?;
    let chain = x509::load_server_certificate_chain_file(cert, &public_key, now)
        .map_err(TlsConfigLoadError::Certificate)?;

    TlsServerConfig::new(chain, signing_key)
        .map(|cfg| cfg.with_scram_channel_binding(scram_channel_binding))
        .map_err(TlsConfigLoadError::Mismatch)
}

/// [`load_server_config`] の結果を `Arc` へ包むヘルパー（`server::
/// accept_loop_with_tls_mode` が要求する型と一致させる）。既定 `false`
/// のまま構築する後方互換ラッパー（実体は
/// [`load_server_config_arc_with_options`]）。
pub fn load_server_config_arc(
    cert: &Path,
    key: &Path,
) -> Result<Arc<TlsServerConfig>, TlsConfigLoadError> {
    load_server_config_arc_with_options(cert, key, false)
}

/// [`load_server_config_arc`] に `--tls-scram-channel-binding` の解決済み
/// 値を追加で渡せる入口（`main.rs` が呼ぶ）。
pub fn load_server_config_arc_with_options(
    cert: &Path,
    key: &Path,
    scram_channel_binding: bool,
) -> Result<Arc<TlsServerConfig>, TlsConfigLoadError> {
    load_server_config_with_options(cert, key, scram_channel_binding).map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_all_tokens() {
        for tok in MODE_TOKENS {
            assert!(parse(tok).is_ok(), "expected {tok:?} to be accepted");
        }
    }

    #[test]
    fn parse_require_and_allow_round_trip() {
        assert_eq!(parse("require"), Ok(TlsMode::Require));
        assert_eq!(parse("allow"), Ok(TlsMode::Allow));
    }

    #[test]
    fn parse_rejects_case_variants_and_whitespace() {
        for raw in [
            "Require",
            "REQUIRE",
            " allow",
            "allow ",
            "Allow",
            "",
            "prefer",
            "require\n",
        ] {
            assert!(
                parse(raw).is_err(),
                "expected {raw:?} to be rejected (strict match only)"
            );
        }
    }

    #[test]
    fn parse_rejects_control_character_injection() {
        // untrusted な引数に制御文字が混じっても厳密一致で弾かれ、エラー文言へ
        // そのまま埋め込まれても Debug 表記（`{:?}`）でエスケープされることを
        // 確認する（`durability_opt::parse` と同じ防御的姿勢）。
        let err = parse("require\0bogus").expect_err("must reject control characters");
        assert!(err.contains(MODE_FLAG));
    }

    #[test]
    fn token_round_trips_through_parse() {
        for tok in MODE_TOKENS {
            let mode = parse(tok).expect("valid token");
            assert_eq!(mode.token(), tok);
        }
    }

    #[test]
    fn load_server_config_reports_missing_key_file() {
        // `TlsServerConfig`（`Ok` 側）は鍵・証明書を保持するため `Debug` を
        // 導出しない（内容を誤ってログへ出さないための設計判断）。
        // `expect_err` は `Ok` 側に `Debug` を要求するため使わず、`match` で
        // 直接判定する。2 引数版の呼び出しが壊れないことも兼ねて確認する
        // （codex-review PR #1087 P1 是正: 破壊的シグネチャ変更の回帰確認）。
        match load_server_config(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        ) {
            Err(TlsConfigLoadError::Key(_)) => {}
            Err(other) => panic!("expected Key error, got a different error: {other}"),
            Ok(_) => panic!("missing files must fail"),
        }
    }

    #[test]
    fn load_server_config_with_options_reports_missing_key_file() {
        // `_with_options` 入口も同じ失敗を返すことを確認する（薄いラッパー
        // であることの回帰確認）。
        match load_server_config_with_options(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
            true,
        ) {
            Err(TlsConfigLoadError::Key(_)) => {}
            Err(other) => panic!("expected Key error, got a different error: {other}"),
            Ok(_) => panic!("missing files must fail"),
        }
    }

    #[test]
    fn parse_scram_channel_binding_accepts_all_tokens() {
        for tok in SCRAM_CHANNEL_BINDING_TOKENS {
            assert!(
                parse_scram_channel_binding(tok).is_ok(),
                "expected {tok:?} to be accepted"
            );
        }
    }

    #[test]
    fn parse_scram_channel_binding_enable_and_disable_round_trip() {
        assert_eq!(parse_scram_channel_binding("enable"), Ok(true));
        assert_eq!(parse_scram_channel_binding("disable"), Ok(false));
    }

    #[test]
    fn parse_scram_channel_binding_rejects_case_variants_and_whitespace() {
        for raw in [
            "Enable", "ENABLE", " disable", "disable ", "Disable", "", "true", "enable\n",
        ] {
            assert!(
                parse_scram_channel_binding(raw).is_err(),
                "expected {raw:?} to be rejected (strict match only)"
            );
        }
    }

    #[test]
    fn parse_scram_channel_binding_rejects_control_character_injection() {
        let err = parse_scram_channel_binding("enable\0bogus")
            .expect_err("must reject control characters");
        assert!(err.contains(SCRAM_CHANNEL_BINDING_FLAG));
    }
}
