//! TLS 1.3 サーバー側自作実装（親 Issue #941・TASK-228・WIRE-9・HTTP-10
//! ポインタ）の入口。PostgreSQL wire（SSLRequest）・NoSQL 表層（HTTPS）
//! いずれのサーバー側実装も外部 TLS クレートへ依存せず自作する方針の下、
//! 各構成要素をサブモジュールとして積み上げていく。実装を 2 時間単位の
//! sub-issue（#952〜#971）へ分割して進めている。
//!
//! - `handshake.rs` の `SSLRequest` 応答（`'N'` を返す既存挙動）はビット単位で
//!   不変。TLS の実接続組み込みは #966・#968 が担当する
//! - [`field25519`]（`pub(crate)`）: GF(2^255-19) 上の定数時間フィールド
//!   算術。X25519 と後続の Ed25519（Issue #961）で共有する内部基盤
//! - [`x25519`]: X25519 鍵交換（RFC 7748・定数時間。Issue #955）。TLS 1.3
//!   の鍵交換グループとして採用
//! - [`record`]: レコード層（RFC 8446 §5.1。#952）。ヘッダ検証・
//!   parse/serialize・受信バッファの組み立てを担い、鍵・暗号・状態を
//!   一切持たない（`record` のドキュメンテーションコメントを参照）
//! - [`hkdf`]: HMAC-SHA-256（RFC 2104）・HKDF-Extract/Expand（RFC 5869）・
//!   `HKDF-Expand-Label`／`Derive-Secret`（RFC 8446 §7.1）の原始操作
//!   （Issue #956）。ハッシュ本体は `engine::sha256` を再利用する
//! - [`key_schedule`]: TLS 1.3 鍵スケジュール本体（RFC 8446 §7.1。Issue #956）。
//!   Early → Handshake → Master の secret 遷移と各段の traffic secret／
//!   key／iv 導出を型状態で提供する
//!
//! key_share 拡張の解析（#954）・alert 型やハンドシェイク状態機械（#965）・
//! レコード保護／暗号化（#959）・接続への結線（#966 以降）はいずれも
//! 後続 sub-issue の担当であり、本モジュールは対象外のまま。並列開発時の
//! コンフリクトを避けるため、後続 sub-issue は `pub mod` を 1 行ずつ
//! 追加していく想定。

pub(crate) mod field25519;
pub mod hkdf;
pub mod key_schedule;
pub mod record;
pub mod x25519;
