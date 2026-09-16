//! HTTP セッション認証（NoSQL 表層。TASK-174・HTTP-4・HTTP-5・HTTP-8）の構成要素群。
//!
//! `POST /v1/session` での認証成功時に発行し、以後のリクエストで
//! `Authorization: Bearer <token>` として提示される不透明セッショントークンを
//! 中心に、認証フロー全体を小さな構成要素へ分割する。本モジュールが担うのは
//! [`token`]（トークンの生成・base64url 表現・パース）・[`store`]
//! （発行済みトークン → `PolicyContext` の対応・TTL・同時有効数上限を持つ
//! メモリ内ストア。Issue #751）・[`issue`]（`POST /v1/session` の発行
//! パイプライン本体。本文検証→`crate::auth::verify`→`store::SessionStore::issue`。
//! Issue #752・HTTP-6）・[`bearer`]（`Authorization: Bearer` ヘッダの受信データ
//! 経路。[`store::SessionStore`] のトークン検証に必要な `SessionToken` への
//! 変換。Issue #753・HTTP-8）・[`close`]（`POST /v1/session/close` のワンタイム
//! 失効パイプライン本体。`bearer::extract_bearer_token`→本文検証→
//! `store::SessionStore::close`。Issue #753・HTTP-8）で、以下は別 Issue の担当の
//! まま本モジュールには含まれない:
//! - `POST /v1/query` 前段の `Authorization: Bearer` ミドルウェア適用・
//!   `tenant_id` 拒否（[`bearer`] を再利用する。Issue #754）

pub mod bearer;
pub mod close;
pub mod issue;
pub mod store;
pub mod token;
