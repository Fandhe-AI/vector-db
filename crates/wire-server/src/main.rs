//! wire-server: PostgreSQL wire プロトコル v3 互換の自作実装を持つバイナリ層。
//!
//! 責務境界: クライアント接続の受け付け・wire プロトコルのパース/応答整形を担い、
//! クエリの実処理は `engine` クレート（コアロジック層）へ委譲する。
//! TASK-66（基盤・工程管理）時点では TCP リスナー・認証は未実装であり、
//! ネットワーク待ち受けは行わない stub（fail-closed 設計を伴う実装は後続タスク）。
//!
//! 対応: TASK-66（ポインタ: `docs/spec/05-tasks.md`）。

/// stub のエントリポイント。
///
/// engine クレートのプレースホルダ API を参照し、workspace のクレート間リンクが
/// コンパイル時に成立することを検証する。未実装である旨を英語メッセージで
/// stderr へ出力して非 0 終了する（プログラム出力文字列は英語の規約に従う）。
fn main() {
    eprintln!(
        "wire-server: not yet implemented (engine={})",
        engine::ENGINE_NAME
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    // workspace の雛形が成立していること（wire-server から engine への path 依存が
    // リンクできること）を確認する smoke テスト。対象ビヘイビア ID なし。
    #[test]
    fn engine_is_linked() {
        assert_eq!(engine::ENGINE_NAME, "engine");
    }
}
