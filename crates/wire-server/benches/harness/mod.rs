//! Issue #463（`vector_knn` 786µs の wire／SQL 表層／距離カーネル内訳の切り分け）
//! 専用ベンチハーネス。
//!
//! `crates/engine/benches/harness/{stats,protocol,accept,rng,env_report,sql_c1}.rs`
//! は `std` のみに依存する時間非依存・engine 非依存の共有ソースであり、本 crate
//! （`wire-server`）からも `#[path]` で同一ファイルを取り込む（新規クレート・
//! コピーは作らない。engine 側の `benches/harness/mod.rs` 冒頭コメントが定める
//! 「`cargo bench` バイナリと統合テストの複数コンパイル単位から取り込まれる
//! 共有ソース」という契約を、crate をまたいで踏襲する）。
//!
//! [`knn_wire`] のみが本 crate 固有の時間非依存ロジック（fail-closed な rounds
//! パース・段別内訳の帰属計算・出力整形）を持つ。
//!
//! # 呼び出し側との契約
//!
//! `crates/wire-server/benches/knn_wire_profile_bench.rs`（`fn main`。実測本体）と
//! `crates/wire-server/tests/knn_wire_profile_accept.rs`（`make ci` 対象の回帰
//! テスト。実測タイマーに依存しない [`knn_wire`] の純関数のみを検証する）の
//! 2 つの独立したコンパイル単位から本モジュールを `#[path]` で取り込む
//! （engine 側と同一パターン）。

// engine 側と同じ理由（利用側ごとに使う識別子集合が異なるため `pub use` で
// まとめない）で、各利用側はサブモジュール経由（`harness::protocol::...` 等）で
// 必要な識別子のみを import する。
#[path = "../../../engine/benches/harness/accept.rs"]
pub mod accept;
#[path = "../../../engine/benches/harness/env_report.rs"]
pub mod env_report;
pub mod hybrid_wire;
pub mod ingest_wire;
pub mod knn_wire;
#[path = "../../../engine/benches/harness/protocol.rs"]
pub mod protocol;
#[path = "../../../engine/benches/harness/rng.rs"]
pub mod rng;
#[path = "../../../engine/benches/harness/sql_c1.rs"]
pub mod sql_c1;
#[path = "../../../engine/benches/harness/stats.rs"]
pub mod stats;
