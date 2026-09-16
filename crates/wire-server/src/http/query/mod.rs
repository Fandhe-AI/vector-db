//! `POST /v1/query` の op 別写像の親モジュール（TASK-175。対象ビヘイビア
//! NOSQL-1〜NOSQL-10。ポインタ: `docs/spec/04-behavior/nosql.md`）。
//!
//! [`schema`] が構文解析（`engine::json::parse_json`）と意味的検証
//! （必須キー欠落・未知キー・型不一致 → `42601`）の橋渡しを担う。op 語彙の
//! 許可リストと未知 op（`0A000`）判定・各 op の実行計画への写像は後続 Issue
//! （#759 以降）がここへ追加する。

pub mod schema;
