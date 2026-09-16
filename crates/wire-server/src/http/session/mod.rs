//! HTTP セッション認証（NoSQL 表層。TASK-174・HTTP-4・HTTP-5）の構成要素群。
//!
//! `POST /v1/session` での認証成功時に発行し、以後のリクエストで
//! `Authorization: Bearer <token>` として提示される不透明セッショントークンを
//! 中心に、認証フロー全体を小さな構成要素へ分割する。本モジュールが担うのは
//! [`token`]（トークンの生成・base64url 表現・パース）・[`store`]
//! （発行済みトークン → `PolicyContext` の対応・TTL・同時有効数上限を持つ
//! メモリ内ストア。Issue #751）・[`issue`]（`POST /v1/session` の発行
//! パイプライン本体。本文検証→`crate::auth::verify`→`store::SessionStore::issue`。
//! Issue #752・HTTP-6）で、以下はいずれも別 Issue の担当のまま本モジュールには
//! 含まれない:
//! - `POST /v1/session/close`（Issue #753）
//! - `Authorization: Bearer` ヘッダの解析・検証（untrusted 入力の受理点。
//!   Issue #754）

pub mod issue;
pub mod store;
pub mod token;
