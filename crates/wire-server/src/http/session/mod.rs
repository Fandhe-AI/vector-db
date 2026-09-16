//! HTTP セッション認証（NoSQL 表層。TASK-174・HTTP-4）の構成要素群。
//!
//! `POST /v1/session` での認証成功時に発行し、以後のリクエストで
//! `Authorization: Bearer <token>` として提示される不透明セッショントークンを
//! 中心に、認証フロー全体を小さな構成要素へ分割する。本モジュールが担うのは
//! [`token`] のみ（トークンの生成・base64url 表現・パース）で、以下は
//! いずれも別 Issue の担当のまま本モジュールには含まれない:
//! - セッションストア（発行済みトークン → テナント/セッション情報の対応・
//!   TTL・保持上限。Issue #751）
//! - `POST /v1/session` エンドポイント本体（認証・トークン発行。Issue #752）
//! - `Authorization: Bearer` ヘッダの解析・検証（untrusted 入力の受理点。
//!   Issue #753・#754）

pub mod token;
