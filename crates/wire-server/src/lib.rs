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
//! - [`framing`]: メッセージフレーミングの長さ検証・fail-closed エラー分類（WIRE-4, WIRE-10）
//! - [`handshake`]: TCP 接続ごとのメッセージ読み書き・StartupMessage・認証フロー（WIRE-1）
//! - [`bind_guard`]: bind アドレスの通信路保護要件検証（TLS 未構成時は loopback 限定。
//!   TASK-70・WIRE-7）。`main.rs::run_server` の唯一の bind 経路
//! - [`server`]: 接続受け付けループ・同時接続数の有界化・I/O タイムアウト
//!   （review 是正。本格的な接続管理は TASK-69）
//! - [`protocol_dispatch`][]: 認証後メッセージの型バイト分類と、拡張クエリ
//!   プロトコル等の未対応メッセージへの fail-closed 拒否応答＋切断（TASK-71・WIRE-8）
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）、
//! TASK-68（対象ビヘイビア WIRE-4, WIRE-10）、TASK-70（対象ビヘイビア WIRE-7）、
//! TASK-71（対象ビヘイビア WIRE-8）。

pub mod auth;
pub mod bind_guard;
pub mod framing;
pub mod handshake;
pub mod protocol_dispatch;
pub mod server;
