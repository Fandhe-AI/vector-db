//! NoSQL 表層（`--surface nosql`）が唯一の転送路として使う、自作 HTTP/1.1
//! 最小サブセットのモジュール群。
//!
//! 本モジュールは `AGENTS.md` P1 の「Web API はスコープ外」が指す汎用 Web API
//! フレームワークとは別物である。ここで実装するのは spec で規範化された内部転送
//! プロトコル（HTTP-1〜13）であり、pg wire v3 互換実装（[`crate::framing`] 等）と
//! 並ぶ、もう一方の表層の接続処理層に位置づく。
//!
//! エラー契約は SQL 表層（`wire_code` ＝ [`engine::error_format::ErrorClass`]）と
//! 完全共有し、新規 `wire_code` は追加しない。本モジュール配下は `ErrorClass` を
//! HTTP 上の表現（ステータス・応答本文等）へ写像する各要素、要求側のパース処理、
//! および `POST /v1/query` の JSON クエリオブジェクト検証を集約する。
//!
//! モジュール構成（現時点）:
//! - [`request`]: 要求行（メソッド・ターゲット・バージョン）の解析（Issue #740・
//!   TASK-173・HTTP-2, HTTP-11）
//! - [`status`]: `ErrorClass` → HTTP ステータスの決定的射影（Issue #744・ERR-4）
//! - [`error_body`]: `ErrorClass` → JSON エラー本文（Issue #745・ERR-4・ERR-5）
//! - [`listener`]: `--surface nosql` の accept ループ stub（要求を読まず接続
//!   を即クローズ。Issue #735・TASK-171／HTTP-1・HTTP-9）。`main.rs::run_server`
//!   が SQL wire の [`crate::server::accept_loop_with_engine`] と排他選択で
//!   呼ぶ唯一の呼び出し元
//! - [`query`]: `POST /v1/query` の op 別写像の親モジュール（TASK-175。
//!   `query::schema` が JSON クエリオブジェクトの意味的検証（必須キー欠落・
//!   未知キー・型不一致 → `42601`）を担う（Issue #760）。`query::response` は
//!   `search`／`scan`／`aggregate` 成功時の `QueryResult` → JSON 応答本文
//!   （`columns`／`rows`／`row_count`、`crate::result_encoder` と同じ型写像。
//!   Issue #762・NOSQL-11）を担う
//!
//! 後続 Issue で追加予定（本モジュールでは未実装）:
//! - ヘッダパーサ（Issue #741。[`request::parse_request_line`] が返す
//!   `consumed` オフセットから読み始める）
//! - `Content-Type`／本文長上限の検証（Issue #742）
//! - 応答エンベロープ（ステータス行・`Connection: close`・`Content-Type`／
//!   `Content-Length`・CRLF の組み立て。Issue #746）
//! - `explain: true` 時の `{"explain":[...]}` 応答（#765）
//! - 接続ハンドラ本体（[`listener::accept_loop_stub`] を置き換える有界読み取り・
//!   EOF 時の無応答クローズ判断・panic 非伝播。Issue #747）
//! - op 語彙の許可リストと未知 op（`0A000`）判定・各 op の実行計画への写像
//!   （Issue #759 以降）
//!
//! 対応: TASK-173〜TASK-175（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア
//! HTTP-1〜13・NOSQL-1〜NOSQL-10。PoC-15/TASK-182 は private 資産のため
//! ポインタ参照のみで、コード・所見は転記しない）。

pub mod error_body;
pub mod listener;
pub mod query;
pub mod request;
pub mod status;
