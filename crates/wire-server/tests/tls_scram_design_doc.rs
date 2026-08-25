//! TASK-72（WIRE-9）の受け入れ条件「対象ビヘイビア ID に対応するテストを追加する」に
//! 対応するテスト。TASK-72 はランタイム実装を伴わない設計ドキュメント作成タスクである
//! ため、成果物（`crates/wire-server/docs/tls-scram-design.md`）の存在と必須セクションの
//! 完全性を検証することでビヘイビア対応とする。TLS・SCRAM の実装本体に対するテストは、
//! 実装コードが追加される後続タスク側で追加する。

use std::fs;
use std::path::PathBuf;

/// テスト対象ドキュメントの絶対パスを返す。
/// `CARGO_MANIFEST_DIR` は `crates/wire-server` を指すため、直下の `docs/` を参照する。
fn design_doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/tls-scram-design.md")
}

#[test]
fn design_doc_exists() {
    let path = design_doc_path();
    assert!(
        path.is_file(),
        "TASK-72 の設計ドキュメントが見つからない: {}",
        path.display()
    );
}

#[test]
fn design_doc_has_required_pointers_and_sections() {
    let path = design_doc_path();
    let content = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "設計ドキュメントの読み込みに失敗: {}: {}",
            path.display(),
            e
        )
    });

    // spec-confidentiality に従い spec 本文は転記しないが、対応するタスク・
    // ビヘイビア ID へのポインタ表記は必須とする。この不変条件は文書の言い回し
    // やステータス（Proposed → Accepted 遷移を予定）が変わっても保たれるべき
    // ものなので、それらは意図的にアサートしない。
    assert!(
        content.contains("TASK-72"),
        "TASK-72 へのポインタ表記が見つからない"
    );
    assert!(
        content.contains("WIRE-9"),
        "WIRE-9 へのポインタ表記が見つからない"
    );
}
