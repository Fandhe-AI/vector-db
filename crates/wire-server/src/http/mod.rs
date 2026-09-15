//! NoSQL 表層（HTTP/1.1 最小サブセットの転送路。ポインタ: `docs/spec/05-tasks.md`
//! TASK-180・`docs/spec/04-behavior/http-transport.md` HTTP-*・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-*）の入口モジュール。
//!
//! エラー契約は SQL 表層（`wire_code` ＝ [`engine::error_format::ErrorClass`]）と
//! 完全共有し、新規 `wire_code` は追加しない。本モジュール配下は `ErrorClass` を
//! HTTP 上の表現（ステータス・応答本文等）へ写像する各要素を集約する。
//!
//! - [`status`]: `ErrorClass` → HTTP ステータスの決定的射影（Issue #744・ERR-4）
//!
//! 後続 Issue（JSON エラー本文エンコーダ・要求行/ヘッダ解析・応答エンコーダ等）が
//! 本モジュール配下へ `pub mod` を追加していく。

pub mod status;
