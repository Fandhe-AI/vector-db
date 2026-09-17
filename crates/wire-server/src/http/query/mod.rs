//! `POST /v1/query` の op 別写像の親モジュール（TASK-175。対象ビヘイビア
//! NOSQL-1〜NOSQL-10。ポインタ: `docs/spec/04-behavior/nosql-surface.md`）。
//!
//! [`schema`] が構文解析（`engine::json::parse_json`）と意味的検証
//! （必須キー欠落・未知キー・型不一致 → `42601`）の橋渡しを担う。[`filter`]
//! は `filter` 配列の `op` 語彙（`eq`／`prefix`）を
//! `engine::declarative_filter::DeclarativeFilter` へ写像し、`bind_all` で
//! `TableSchema` へ束縛する（Issue #761・TASK-175・NOSQL-7）。[`ident`] は
//! `table`／列名等の識別子形状（SQL 表層の字句解析と同じ文字集合）を検査する
//! 共有ヘルパー（Issue #768。`search`／`scan`〔#763・#766〕からも再利用する
//! 想定）。[`op`] は `op` 名を閉じた語彙 4 値（`search`／`scan`／
//! `aggregate`／`insert`）へ分類する許可リストで、語彙外は `0A000` へ写像
//! する（Issue #759・TASK-179・NOSQL-1・NOSQL-9）。[`aggregate`] は
//! `op: aggregate`（`group_by`／`having`／`explain: true` を除く）を
//! SQL テキストを経由せずに `engine::sql::parser::BoundAggregate` へ束縛・
//! 実行する（Issue #768・TASK-177・NOSQL-4）。他 op の実行計画への写像は
//! 後続 Issue（#763・#771）がここへ追加する。[`response`] は
//! `search`／`scan`／`aggregate` 成功時の `engine::sql::exec::QueryResult`
//! → JSON 応答本文（`columns`／`rows`／`row_count`）への写像を担う
//! （Issue #762・NOSQL-11）。[`scan`] は `op: "scan"` を
//! `engine::sql::parser::BoundScan` へ束縛し `EngineCore` で実行する（Issue
//! #766・TASK-176・NOSQL-3。結線済み）。[`search`] は `op: search` の JSON
//! クエリオブジェクトを SQL 表層の `bind_in_session` と同一形の
//! `engine::sql::parser::BoundStatement` へ束縛し実行する（Issue #763・
//! #764・TASK-175・NOSQL-2。`vector`／`plan` 排他・`hybrid`／`mode`／
//! `columns` の意味論は SQL 表層の既存公開関数へ委譲する）。`plan` 指定は
//! LLM 展開が engine 内部 I/O を要するため、`EngineCore::
//! execute_bound_plan_search_in_session`（TASK-186）が SQL 表層
//! `USING PLAN` と同一の fail-closed 判定順序で実行する。[`gate`] は認証済み
//! 要求（`crate::http::session::middleware::SessionPrincipal`）に対する
//! `POST /v1/query` の入口本体で、`tenant_id` 相当ヘッダの拒否・op 許可
//! リスト判定・本文のスキーマ検証を行い、`engine` 接続済みの場合に限り
//! [`aggregate::handle`]（Issue #768）へ、`op: insert` を
//! [`insert::handle`]（Issue #772）へ、`op: search` を [`search::handle`]
//! （Issue #764）へそれぞれディスパッチする（Issue #754・#759。`engine`
//! 未接続時のみ暫定の `0A000`／501 応答が残る）。
//! [`insert`] は `insert` op を
//! `engine::sql::exec::execute_insert_batch`／
//! `EngineCore::execute_bound_insert_in_session` へ写像し、成功応答
//! `{"inserted","operation_id"}`（[`insert::encode_success_body`]）を
//! `gate.rs` の `Op::Insert` アームへ結線する
//! （Issue #771・#772・TASK-178・NOSQL-6・TABLE-12・RLS-9）。

pub mod aggregate;
pub mod filter;
pub mod gate;
pub mod ident;
pub mod insert;
pub mod op;
pub mod response;
pub mod scan;
pub mod schema;
pub mod search;
