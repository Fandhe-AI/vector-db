//! wire-server: PostgreSQL wire プロトコル v3 互換の自作実装ライブラリ層。
//!
//! `src/main.rs`（バイナリ）と `tests/`（結合テスト）の双方から利用できるよう、
//! TASK-66 の stub から lib+bin 構成へ再編した（バイナリのみでは結合テストが
//! 内部モジュールへアクセスできないため）。責務境界はクライアント接続の受け付け・
//! wire プロトコルのパース/応答整形であり、クエリの実処理は `engine` クレート
//! （コアロジック層）へ委譲する方針を維持する。
//!
//! モジュール構成:
//! - [`auth`]: ユーザーストア・Argon2id 照合・`PolicyContext` へのテナント導出（WIRE-2, WIRE-3）
//! - [`handshake`]: TCP 接続ごとのメッセージ読み書き・StartupMessage・認証フロー（WIRE-1）
//! - [`server`]: bind アドレスの loopback 検証・接続受け付けループ（WIRE-5, WIRE-6 の
//!   契約適用は [`limits`] に委譲）
//! - [`limits`]: 読み取りタイムアウト・共有接続数リミッター（TASK-69・WIRE-5, WIRE-6）
//!
//! 対応: TASK-67（対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）・TASK-69
//! （対象ビヘイビア WIRE-5, WIRE-6）（ポインタ: `docs/spec/05-tasks.md`）。

pub mod auth;
pub mod handshake;
pub mod limits;
pub mod server;
