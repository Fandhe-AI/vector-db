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
//! - [`aes_gcm`]: AES-128-GCM（NIST SP 800-38D。定数時間 GHASH・AEAD
//!   暗号化／復号・タグ検証。Issue #958）。`aes` の暗号化方向のみを使い、
//!   per-record nonce の導出・レコード保護本体は [`super::key_schedule`]・
//!   #959 の担当
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
//! - [`record_protection`]: レコード保護層（RFC 8446 §5.2〜§5.5。Issue #959）。
//!   [`record`]・[`key_schedule`]・[`aes_gcm`] をつなぎ、per-record nonce・
//!   シーケンス番号・`TLSInnerPlaintext`（内容型・パディング）・AAD の構成・
//!   handshake 鍵 → application 鍵の方向別切替（[`record_protection::
//!   Sealer`]／[`record_protection::Opener`]）を提供する
//! - [`pem`]: PEM（RFC 7468）ブロックのデコードと、鍵・証明書ファイルの
//!   上限付き読み込み（Issue #962）。定数時間 base64 デコーダを持ち、
//!   秘密鍵ブロックはちょうど 1 個・証明書チェーンは複数ブロックを順序
//!   どおりに受け付ける
//! - [`pkcs8`]: PKCS#8 v1・Ed25519（RFC 8410 §7）の最小 DER パース
//!   （Issue #962）。[`pem`] が返す DER から 32 バイトの seed を取り出し、
//!   RSA・ECDSA・v2（OneAsymmetricKey）等は起動時に明示的に拒否する。
//!   鍵導出・署名は #961 の担当
//! - [`der`]（`pub(crate)`）: X.509 向けに一般化した任意深さの DER TLV
//!   リーダー（Issue #963）。[`pkcs8::DerReader`] とは別に持つ
//! - [`x509`]: X.509 証明書の最小パース・validity 検査・葉 SPKI
//!   （Ed25519）と秘密鍵側の公開鍵の照合、`Certificate` メッセージの
//!   組み立て（Issue #963）。証明書署名の検証・extensions の意味解釈は
//!   対象外のまま（[`x509`] のドキュメンテーションコメントを参照）
//! - [`sha512`]: SHA-512（FIPS 180-4）。Ed25519 署名生成・検証（Issue #961）の
//!   秘密鍵展開・署名計算が使う（Issue #960）。トランスクリプトハッシュ・
//!   HKDF は引き続き SHA-256（[`hkdf`]）のまま
//! - [`transcript`]: transcript hash（RFC 8446 §4.4.1。Issue #964）。
//!   ハンドシェイクメッセージ列の累積 SHA-256 を、更新順序の単一情報源
//!   （[`transcript::Transcript::expected_next`]）とともに提供し、
//!   HelloRetryRequest 時の `message_hash` 置換もここで扱う
//! - [`finished`]: Finished（RFC 8446 §4.4.4。Issue #964）。
//!   `verify_data` の計算（送信側）・定数時間検証（受信側）を提供し、
//!   [`key_schedule::TrafficSecret::finished_key`]・[`transcript`] の
//!   チェックポイントをつなぐ
//!
//! alert の実送出やハンドシェイク状態機械（#965）・接続への結線
//! （#966 以降）はいずれも後続 sub-issue の担当であり、本モジュールは
//! 対象外のまま。並列開発時のコンフリクトを避けるため、後続 sub-issue は
//! `pub mod` を 1 行ずつ追加していく想定。

pub mod aes;
pub mod aes_gcm;
pub mod client_hello;
pub(crate) mod der;
pub(crate) mod field25519;
pub mod finished;
pub mod handshake;
pub mod hkdf;
pub mod key_schedule;
pub mod pem;
pub mod pkcs8;
pub mod record;
pub mod record_protection;
pub mod sha512;
pub mod transcript;
pub mod x25519;
pub mod x509;
