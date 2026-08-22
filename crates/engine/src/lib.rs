//! engine クレート: vector-db のコアロジック層。
//!
//! 責務境界: データロード・検索カーネル・認証・RLS 相当のテナント境界・redb ベースの
//! 永続化を担う（クエリの受付・応答整形など wire プロトコルの詳細は持たない）。
//! `wire-server`（バイナリクレート）から呼び出されるライブラリで、
//! ワークスペース内での相互参照は path 依存に限る
//! （.claude/rules/coding-rust.md: workspace 構成の責務境界を跨ぐ依存を作らない）。
//!
//! 対応: TASK-66（基盤・工程管理）・TASK-140（`redb` 永続化層。ポインタ:
//! `docs/spec/05-tasks.md`）。検索カーネル・認証・RLS 等の実ロジックは後続タスクで追加する。

pub mod storage;

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
