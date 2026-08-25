//! TASK-70（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-7）。
//!
//! wire-server は cleartext password 認証（WIRE-2）を前提とし、TLS は未実装
//! （TASK-72・WIRE-9 は設計 ADR `docs/design/tls-scram-design.md` のみ・Proposed）。
//! この状態で非ループバックアドレスへ bind すると、平文パスワード・クエリ・結果が
//! loopback 外へ晒される。本モジュールは `main.rs::run_server` の唯一の bind 経路
//! （[`GuardedBindAddrs`]）として、通信路の保護状態（[`TransportSecurity`]）に応じた
//! 要件を起動時に検証し、満たさなければ fail-closed で拒否する。
//!
//! `server.rs`（accept ループ・同時接続数の有界化・I/O タイムアウト）とは責務を分離し、
//! bind 前の検証のみをここに集約する（TASK-67 review 是正の `validate_loopback_bind` /
//! `bind_loopback` を移設・拡張）。

use std::fmt;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};

/// 通信路の保護状態。TLS 実装（TASK-72・WIRE-9）導入時にこの enum へ variant を
/// 追加し、[`GuardedBindAddrs::resolve`] 内の match を exhaustiveness で更新させる
/// ことで、「TLS を足したらガードの分岐も必ず書く」ことを型で強制する。
///
/// `#[non_exhaustive]` は crate 内（本 crate 内）の match には影響しない
/// （`non_exhaustive` が抑制するのは他 crate からの exhaustive match のみ）ため、
/// 上記の内部強制目的を損なわずに付与できる。本 enum は `pub mod bind_guard` 経由で
/// crate 外にも公開されており、付けないと将来 variant 追加時に下流クレートの
/// exhaustive match を破壊しうる（PR #182 レビュー指摘）ため付与する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportSecurity {
    /// TLS 未構成（cleartext password 認証を平文 TCP で行う）。loopback 限定。
    Cleartext,
}

/// [`GuardedBindAddrs::resolve`] の拒否理由。`Display` はプログラム出力規約に
/// 従い英語で書く。
#[derive(Debug)]
pub enum BindGuardError {
    /// `bind_addr` 文字列の名前解決（`ToSocketAddrs`）自体が失敗した。
    Resolve {
        bind_addr: String,
        source: std::io::Error,
    },
    /// 名前解決は成功したが、解決結果が空だった。
    NoAddress { bind_addr: String },
    /// 解決結果に、`security` の要件を満たさないアドレスが 1 件以上含まれていた。
    NonLoopback { bind_addr: String, addr: SocketAddr },
}

impl fmt::Display for BindGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindGuardError::Resolve { bind_addr, source } => {
                write!(f, "cannot resolve bind address {bind_addr}: {source}")
            }
            BindGuardError::NoAddress { bind_addr } => {
                write!(
                    f,
                    "bind address {bind_addr} did not resolve to any socket address"
                )
            }
            BindGuardError::NonLoopback { bind_addr, addr } => {
                write!(
                    f,
                    "refusing to bind non-loopback address {addr} (from {bind_addr}): \
                     cleartext password authentication is not yet protected by TLS \
                     (TASK-72/WIRE-9); bind to a loopback address (e.g. 127.0.0.1) or \
                     place a trusted TLS terminator in front of this listener"
                )
            }
        }
    }
}

impl std::error::Error for BindGuardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BindGuardError::Resolve { source, .. } => Some(source),
            BindGuardError::NoAddress { .. } | BindGuardError::NonLoopback { .. } => None,
        }
    }
}

/// `security` の要件を満たすことが検証済みの bind 先アドレス列。
///
/// フィールドを private にすることで、この型は [`GuardedBindAddrs::resolve`] の
/// 検証を経ずには構築できない（型状態）。[`GuardedBindAddrs::bind`] は検証済みの
/// 数値 `SocketAddr` へ直接 bind し、`bind_addr`（文字列）を再度 DNS 解決しない。
/// ホスト名を検証後に再解決すると、検証時と bind 時で異なるアドレスへ解決される
/// TOCTOU（検証時 loopback・bind 時に外部アドレス）が成立しうるため（TASK-67
/// review 指摘）、本モジュールが bind の唯一の入口となる。
#[derive(Debug)]
pub struct GuardedBindAddrs {
    addrs: Vec<SocketAddr>,
}

impl GuardedBindAddrs {
    /// `bind_addr` を解決し、`security` に応じた要件を全アドレスが満たすことを
    /// 検証する。
    ///
    /// - `TransportSecurity::Cleartext`: 解決結果の全アドレスが
    ///   `IpAddr::is_loopback()` であること（IPv4 `127.0.0.0/8`・IPv6 `::1`）。
    ///   IPv4-mapped IPv6 loopback（`::ffff:127.0.0.1`）は `is_loopback()` が
    ///   `false` を返すため拒否される。これは fail-closed 側の挙動として意図的に
    ///   維持し、緩和しない。
    /// - ホスト名（`localhost` 等）は解決結果の全件が要件を満たす場合のみ許可する
    ///   （`/etc/hosts` 汚染等で 1 件でも非 loopback が混ざれば拒否する）。
    pub fn resolve(bind_addr: &str, security: TransportSecurity) -> Result<Self, BindGuardError> {
        let addrs: Vec<SocketAddr> = bind_addr
            .to_socket_addrs()
            .map_err(|e| BindGuardError::Resolve {
                bind_addr: bind_addr.to_string(),
                source: e,
            })?
            .collect();

        if addrs.is_empty() {
            return Err(BindGuardError::NoAddress {
                bind_addr: bind_addr.to_string(),
            });
        }

        for addr in &addrs {
            match security {
                TransportSecurity::Cleartext => {
                    if !addr.ip().is_loopback() {
                        return Err(BindGuardError::NonLoopback {
                            bind_addr: bind_addr.to_string(),
                            addr: *addr,
                        });
                    }
                }
            }
        }

        Ok(GuardedBindAddrs { addrs })
    }

    /// 検証済みの数値アドレス列へ直接 bind する（文字列を再解決しない）。
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        // `&[SocketAddr]` も `ToSocketAddrs` を実装するが、これは単に列挙を返す
        // だけで DNS 解決は発生しない（数値アドレスなので再解決の余地がない）。
        TcpListener::bind(self.addrs.as_slice())
    }

    /// 検証済みのアドレス列を参照する（テスト・診断用）。
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    #[test]
    fn resolve_accepts_loopback_ipv4() {
        let guarded = GuardedBindAddrs::resolve("127.0.0.1:0", TransportSecurity::Cleartext)
            .expect("loopback address accepted");
        assert!(!guarded.addrs().is_empty());
        assert!(guarded.addrs().iter().all(|a| a.ip().is_loopback()));
    }

    #[test]
    fn resolve_accepts_loopback_ipv6() {
        // 解決のみを検証し bind はしない（IPv6 が無効な CI 環境への依存を避ける）。
        let guarded = GuardedBindAddrs::resolve("[::1]:0", TransportSecurity::Cleartext)
            .expect("ipv6 loopback address accepted");
        assert!(guarded.addrs().iter().all(|a| a.ip().is_loopback()));
    }

    #[test]
    fn resolve_accepts_loopback_hostname() {
        // 実行環境の名前解決結果が全件 loopback であることが前提（CI ubuntu で成立）。
        assert!(GuardedBindAddrs::resolve("localhost:0", TransportSecurity::Cleartext).is_ok());
    }

    #[test]
    fn resolve_rejects_ipv4_wildcard() {
        let result = GuardedBindAddrs::resolve("0.0.0.0:5432", TransportSecurity::Cleartext);
        let err = result.expect_err("wildcard address must be rejected");
        assert!(
            err.to_string().contains("TLS"),
            "message should explain why: {err}"
        );
    }

    #[test]
    fn resolve_rejects_ipv6_wildcard() {
        let result = GuardedBindAddrs::resolve("[::]:5432", TransportSecurity::Cleartext);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_rejects_lan_addresses() {
        assert!(
            GuardedBindAddrs::resolve("192.168.1.10:5432", TransportSecurity::Cleartext).is_err()
        );
        assert!(GuardedBindAddrs::resolve("10.0.0.1:5432", TransportSecurity::Cleartext).is_err());
    }

    /// fail-closed 明文化: IPv4-mapped IPv6 loopback は `is_loopback()` が偽を
    /// 返すため拒否される。緩和が必要な場合は spec 側課題として扱う（本タスクの
    /// スコープ外）。
    #[test]
    fn resolve_rejects_ipv4_mapped_loopback() {
        let result =
            GuardedBindAddrs::resolve("[::ffff:127.0.0.1]:5432", TransportSecurity::Cleartext);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_reports_resolve_error_for_unparsable_input() {
        let result = GuardedBindAddrs::resolve("not-a-valid-address", TransportSecurity::Cleartext);
        assert!(matches!(result, Err(BindGuardError::Resolve { .. })));
    }

    #[test]
    fn bind_binds_to_validated_addr_and_accepts_connections() {
        let guarded = GuardedBindAddrs::resolve("127.0.0.1:0", TransportSecurity::Cleartext)
            .expect("loopback resolve must succeed");
        let listener = guarded.bind().expect("loopback bind must succeed");
        let addr = listener.local_addr().expect("local addr");
        assert!(addr.ip().is_loopback());

        let client = TcpStream::connect(addr).expect("connect to bound listener");
        let (_accepted, _peer) = listener.accept().expect("listener must accept connection");
        drop(client);
    }

    /// 非 loopback アドレスは `resolve` の時点で `Err` となり、型状態上
    /// `bind` を呼べる `GuardedBindAddrs` が構築されない（bind に到達しないことの
    /// 構造的な確認）。
    #[test]
    fn non_loopback_never_reaches_bind() {
        let result = GuardedBindAddrs::resolve("0.0.0.0:0", TransportSecurity::Cleartext);
        assert!(result.is_err());
    }

    #[test]
    fn bind_guard_error_source_returns_io_error_only_for_resolve() {
        use std::error::Error;
        let resolve_err =
            GuardedBindAddrs::resolve("not-a-valid-address", TransportSecurity::Cleartext)
                .expect_err("must fail to resolve");
        assert!(resolve_err.source().is_some());

        let non_loopback_err = GuardedBindAddrs::resolve("0.0.0.0:0", TransportSecurity::Cleartext)
            .expect_err("must be rejected as non-loopback");
        assert!(non_loopback_err.source().is_none());
    }
}
