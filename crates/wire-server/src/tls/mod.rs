//! TLS 1.3 サーバー側自作実装（親 Issue #941・TASK-228・WIRE-9・HTTP-10
//! ポインタ）の入口。PostgreSQL wire（SSLRequest）・NoSQL 表層（HTTPS）
//! いずれのサーバー側実装も外部 TLS クレートへ依存せず自作する方針の下、
//! 各構成要素をサブモジュールとして積み上げていく。実装を 2 時間単位の
//! sub-issue（#952〜#971）へ分割して進めている。
//!
//! - `handshake.rs` の `SSLRequest` 応答（`'N'` を返す既存挙動）はビット単位で
//!   不変。TLS の実接続組み込みは #966・#968 が担当する
//! - [`aes`]: AES-128 ブロック暗号（暗号化方向のみ・定数時間。Issue #957）。
//!   GCM（#958）が CTR 鍵ストリーム生成・`H = E_K(0^128)` の計算に使う
//! - [`field25519`]（`pub(crate)`）: GF(2^255-19) 上の定数時間フィールド
//!   算術。X25519 と後続の Ed25519（Issue #961）で共有する内部基盤
//! - [`x25519`]: X25519 鍵交換（RFC 7748・定数時間。Issue #955）。TLS 1.3
//!   の鍵交換グループとして採用
//! - [`record`]: レコード層（RFC 8446 §5.1。#952）。ヘッダ検証・
//!   parse/serialize・受信バッファの組み立てを担い、鍵・暗号・状態を
//!   一切持たない（`record` のドキュメンテーションコメントを参照）
//! - [`handshake`]: ハンドシェイクメッセージ層（RFC 8446 §4。#953）。
//!   最小集合 6 種の parse/serialize と、レコード境界をまたぐ再組み立てを
//!   担う。拡張の意味解釈・状態機械・暗号は持たない（`handshake` の
//!   ドキュメンテーションコメントを参照）
//! - [`client_hello`]: `ClientHello` 拡張の意味解釈・受理判定・
//!   HelloRetryRequest 構築（RFC 8446 §4.1.2・§4.1.3・§4.2。#954）。
//!   TLS 1.3・`TLS_AES_128_GCM_SHA256`・X25519・Ed25519 のみを受理する
//!   fail-closed な純粋関数層で、状態は持たない（`client_hello` の
//!   ドキュメンテーションコメントを参照）
//! - [`hkdf`]: HMAC-SHA-256（RFC 2104）・HKDF-Extract/Expand（RFC 5869）・
//!   `HKDF-Expand-Label`／`Derive-Secret`（RFC 8446 §7.1）の原始操作
//!   （Issue #956）。ハッシュ本体は `engine::crypto::sha256`（SCRAM-SHA-256 認証・Issue #940 が切り出し済み）を再利用する
//! - [`key_schedule`]: TLS 1.3 鍵スケジュール本体（RFC 8446 §7.1。Issue #956）。
//!   Early → Handshake → Master の secret 遷移と各段の traffic secret／
//!   key／iv 導出を型状態で提供する
//!
//! alert の実送出やハンドシェイク状態機械（#965）・レコード保護／暗号化
//! （#959）・接続への結線（#966 以降）はいずれも後続 sub-issue の担当で
//! あり、本モジュールは対象外のまま。並列開発時のコンフリクトを避ける
//! ため、後続 sub-issue は `pub mod` を 1 行ずつ追加していく想定。

pub mod aes;
pub mod client_hello;
pub(crate) mod field25519;
pub mod handshake;
pub mod hkdf;
pub mod key_schedule;
pub mod record;
pub mod x25519;
