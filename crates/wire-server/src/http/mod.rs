//! NoSQL 表層（`--surface nosql`）が唯一の転送路として使う、自作 HTTP/1.1
//! 最小サブセットのモジュール群。
//!
//! 本モジュールは `AGENTS.md` P1 の「Web API はスコープ外」が指す汎用 Web API
//! フレームワークとは別物である。ここで実装するのは spec で規範化された内部転送
//! プロトコル（HTTP-1〜13）であり、pg wire v3 互換実装（[`crate::framing`] 等）と
//! 並ぶ、もう一方の表層の接続処理層に位置づく。
//!
//! モジュール構成（現時点）:
//! - [`request`]: 要求行（メソッド・ターゲット・バージョン）の解析（Issue #740・
//!   TASK-173・HTTP-2, HTTP-11）
//!
//! 後続 Issue で追加予定（本モジュールでは未実装）:
//! - ヘッダパーサ（Issue #741。[`request::parse_request_line`] が返す
//!   `consumed` オフセットから読み始める）
//! - `Content-Type`／本文長上限の検証（Issue #742）
//! - `ErrorClass` → HTTP ステータス／JSON エラー本文への写像（Issue #744, #745）
//! - 応答エンコーダ（Issue #746）
//! - 接続ハンドラ（ストリームからの有界読み取り・EOF 時の無応答クローズ判断。
//!   Issue #747）
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-1〜13。
//! PoC-15/TASK-182 は private 資産のためポインタ参照のみで、コード・所見は
//! 転記しない）。

pub mod request;
