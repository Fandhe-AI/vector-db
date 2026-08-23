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
//! TABLE-8）・TASK-102（検索カーネルの疎検索構成要素。BM25 Okapi。対象ビヘイビア:
//! SEARCH-1, SEARCH-3。密検索との RRF 融合は TASK-103、評価ハーネス回帰テストは
//! TASK-104 の管轄）。検索カーネルの残り構成要素（密検索等）・認証・RLS ポリシー評価は
//! 後続タスクで追加する。

pub mod arena;
pub mod catalog;
pub mod row_codec;
pub mod sparse;
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
