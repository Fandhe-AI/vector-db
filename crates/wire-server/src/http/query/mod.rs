//! `POST /v1/query` の op 別写像の親モジュール（TASK-175。対象ビヘイビア
//! NOSQL-1〜NOSQL-10。ポインタ: `docs/spec/04-behavior/nosql-surface.md`）。
//!
//! [`schema`] が構文解析（`engine::json::parse_json`）と意味的検証
//! （必須キー欠落・未知キー・型不一致 → `42601`）の橋渡しを担う。[`filter`]
//! は `filter` 配列の `op` 語彙（`eq`／`prefix`）を
//! `engine::declarative_filter::DeclarativeFilter` へ写像し、`bind_all` で
//! `TableSchema` へ束縛する（Issue #761・TASK-175・NOSQL-7）。[`op`] は
//! `op` 名を閉じた語彙 4 値（`search`／`scan`／`aggregate`／`insert`）へ
//! 分類する許可リストで、語彙外は `0A000` へ写像する（Issue #759・
//! TASK-179・NOSQL-1・NOSQL-9）。各 op の実行計画への写像は後続 Issue
//! （#763・#766・#768 以降）がここへ追加する。[`response`] は
//! `search`／`scan`／`aggregate` 成功時の `engine::sql::exec::QueryResult`
//! → JSON 応答本文（`columns`／`rows`／`row_count`）への写像を担う
//! （Issue #762・NOSQL-11）。[`gate`] は認証済み要求（`crate::http::session::
//! middleware::SessionPrincipal`）に対する `POST /v1/query` の入口本体で、
//! `tenant_id` 相当ヘッダの拒否・op 許可リスト判定・本文のスキーマ検証
//! までを行い、検証を通過した要求には暫定の `0A000`／501 応答を返す
//! （Issue #754・#759。束縛・実行は #763 以降が本 seam を置き換える）。

pub mod filter;
pub mod gate;
pub mod op;
pub mod response;
pub mod schema;
