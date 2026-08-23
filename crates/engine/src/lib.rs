//! engine クレート: vector-db のコアロジック層。
//!
//! 責務境界: データロード・検索カーネル・認証・RLS 相当のテナント境界・redb ベースの
//! 永続化を担う（クエリの受付・応答整形など wire プロトコルの詳細は持たない）。
//! `wire-server`（バイナリクレート）から呼び出されるライブラリで、
//! ワークスペース内での相互参照は path 依存に限る
//! （.claude/rules/coding-rust.md: workspace 構成の責務境界を跨ぐ依存を作らない）。
//!
//! 対応: TASK-66（基盤・工程管理）・TASK-140/TASK-141（`redb` 永続化層。RLS フィールドの
//! スキーマ同居まで含む。ポインタ: `docs/spec/05-tasks.md`）・TASK-88（宣言済み
//! トランザクション分離レベル。対象ビヘイビア: TABLE-3）・TASK-85（スキーマカタログ・
//! `CREATE TABLE`／`ALTER TABLE ADD COLUMN`。対象ビヘイビア: TABLE-1, TABLE-4,
//! TABLE-5, TABLE-6）・TASK-86（カタログスキーマ駆動の行エンコーダー。対象ビヘイビア:
//! TABLE-7）・TASK-146（テーブル粒度次元固定・複数テーブル共存の拡張機能。対象ビヘイビア:
//! EXT-1, EXT-2）・TASK-90（2 テーブル横断トランザクション・クラッシュ耐性回帰テスト。対象
//! ビヘイビア: TABLE-10）・TASK-87（コールドスタート・ベクトルアリーナ。対象ビヘイビア:
//! TABLE-8）・TASK-124（`VectorCore` trait・`PolicyContext`・検索カーネル provider 層の
//! 製品コア。対象ビヘイビア: CORE-1, CORE-2, CORE-13）。プロトコル層（`wire-server`）は
//! `core::VectorCore` のみに依存し、認証・SQL 表層・実行バックエンド差し替え等は後続タスク
//! で拡張する。
//!
//! 性能系タスク（TASK-127・TASK-130・TASK-83 等）向けの計測プロトコル基盤は
//! `benches/harness/`（TASK-158。lib 本体外・`cargo bench`／`tests/bench_harness.rs`
//! から利用）を参照。

pub mod arena;
pub mod catalog;
pub mod core;
pub mod kernel;
pub mod policy;
pub mod row_codec;
pub mod storage;
pub mod txn;

/// engine クレートの識別子。
///
/// wire-server がリンク時にこのクレートへ到達可能であることを確認するための
/// プレースホルダ API（TASK-66 時点の雛形）。後続タスクで実際の公開 API に置き換わる。
pub const ENGINE_NAME: &str = "engine";

#[cfg(test)]
mod tests {
    use super::*;

    // workspace の雛形が成立していること（クレートがビルド・リンクできること）を
    // 確認する smoke テスト。対象ビヘイビア ID なし（基盤タスクのため）。
    #[test]
    fn engine_name_is_stable() {
        assert_eq!(ENGINE_NAME, "engine");
    }
}
