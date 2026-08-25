//! TASK-72（WIRE-9）の受け入れ条件「対象ビヘイビア ID に対応するテストを追加する」に
//! 対応するテスト。TASK-72 はランタイム実装を伴わない設計ドキュメント作成タスクである
//! ため、成果物（`docs/design/tls-scram-design.md`）の存在と必須セクションの完全性を
//! 検証することでビヘイビア対応とする。TLS・SCRAM の実装本体に対するテストは、実装
//! コードが追加される後続タスク側で追加する。

use std::fs;
use std::path::PathBuf;

/// テスト対象ドキュメントの絶対パスを返す。
/// `CARGO_MANIFEST_DIR` は `crates/wire-server` を指すため、リポジトリ構造規約
/// （CLAUDE.md の設計ドキュメント置き場）に従いリポジトリルート直下の
/// `docs/design/` を参照する。
fn design_doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/tls-scram-design.md")
}

fn read_design_doc() -> String {
    let path = design_doc_path();
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "設計ドキュメントの読み込みに失敗: {}: {}",
            path.display(),
            e
        )
    })
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
    let content = read_design_doc();

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

/// 必須の見出し構造（`##` レベル）を検証する。本タスクは「導入方式確定」ではなく
/// 論点整理・調査タスクであるため、決定内容そのものではなく確定前提の論点整理
/// セクションが存在することを構造面で担保する。
#[test]
fn design_doc_has_required_heading_structure() {
    let content = read_design_doc();
    let required_headings = [
        "# ADR:",
        "## 背景",
        "## 論点",
        "## 影響",
        "## スコープ外",
        "## 参照",
    ];
    for heading in required_headings {
        assert!(
            content.lines().any(|line| line.starts_with(heading)),
            "必須の見出し '{heading}' が見つからない"
        );
    }
}

/// ステータス行が「確定」を主張していないことを検証する。設計タスクの成果物が
/// 未確定のまま「導入方式確定」等を名乗るとタスク完了の契約を満たさないため、
/// タイトル・ステータス行に「確定」の語が含まれる場合は Proposed 以外の
/// ステータス表記（Accepted 等、決定が実際に確定した状態）でなければならない。
#[test]
fn design_doc_does_not_claim_finalized_while_proposed() {
    let content = read_design_doc();
    let status_line = content
        .lines()
        .find(|line| line.starts_with("- ステータス:"))
        .expect("ステータス行が見つからない");
    let title_line = content
        .lines()
        .find(|line| line.starts_with("# ADR:"))
        .expect("タイトル行が見つからない");

    let is_proposed = status_line.contains("Proposed");
    let title_claims_finalized = title_line.contains("確定") && !title_line.contains("調査");

    assert!(
        !(is_proposed && title_claims_finalized),
        "ステータスが Proposed のままタイトルが確定を主張している: {title_line}"
    );
}
