//! TASK-72（WIRE-9）・TASK-174（HTTP-10。WIRE-9 と同一方針）に対応するテスト。
//! 成果物（`docs/design/tls-scram-design.md`）の存在と必須ポインタ表記・見出し
//! 構造、および Issue #755 で追加した NoSQL 表層節の非 vacuous な内容検証を行う。

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

    // spec-confidentiality に従い spec 本文は転記しない。TASK-72・WIRE-9 への
    // ポインタ表記が保たれていることのみを検証する。
    assert!(
        content.contains("TASK-72"),
        "TASK-72 へのポインタ表記が見つからない"
    );
    assert!(
        content.contains("WIRE-9"),
        "WIRE-9 へのポインタ表記が見つからない"
    );
}

/// 必須の見出し構造（`##` レベル）を検証する。Issue #755 で NoSQL 表層
/// （HTTP-10）節を必須見出しへ追加した。
#[test]
fn design_doc_has_required_heading_structure() {
    let content = read_design_doc();
    let required_headings = [
        "# ADR:",
        "## 背景",
        "## 論点",
        "## 影響",
        "## NoSQL 表層",
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

/// NoSQL 表層節（Issue #755・TASK-174・HTTP-10）が存在し、節本文（次の `## `
/// 見出しまでの区間）に必須ポインタ・識別子の言及があることを検証する。
/// 文書全体ではなく切り出した節本文で判定することで、他節にたまたま同じ語が
/// 含まれているだけの vacuous pass を防ぐ。
#[test]
fn design_doc_has_nosql_surface_section_with_pointers() {
    let content = read_design_doc();

    // 文書全体に対する存在確認（HTTP-10・TASK-174 のポインタ表記）。
    assert!(
        content.contains("HTTP-10"),
        "HTTP-10 へのポインタ表記が見つからない"
    );
    assert!(
        content.contains("TASK-174"),
        "TASK-174 へのポインタ表記が見つからない"
    );

    // NoSQL 表層節の本文（見出し行の次から、次の `## ` 見出しの手前まで）を
    // 添字アクセスなしに `lines()` の走査で切り出す。
    let mut section_lines: Vec<&str> = Vec::new();
    let mut in_section = false;
    for line in content.lines() {
        if line.starts_with("## NoSQL 表層") {
            in_section = true;
            continue;
        }
        if in_section {
            if line.starts_with("## ") {
                break;
            }
            section_lines.push(line);
        }
    }
    assert!(
        !section_lines.is_empty(),
        "'## NoSQL 表層' 見出しが見つからない、または節本文が空"
    );
    let section_body = section_lines.join("\n");

    // 節本文内で、SQL 表層（WIRE-9）と通信路保護状態を共有する方針が
    // ポインタ表記で示されていることを検証する（spec 本文は転記しない）。
    assert!(
        section_body.contains("TransportSecurity"),
        "NoSQL 表層節に 'TransportSecurity' の言及が見つからない"
    );
    assert!(
        section_body.contains("WIRE-9"),
        "NoSQL 表層節に 'WIRE-9' へのポインタ表記が見つからない"
    );
}

/// ステータス行が「確定」を主張していないことを検証する。設計タスクの成果物が
/// 未確定のまま「導入方式確定」等を名乗るとタスク完了の契約を満たさないため、
/// タイトル・ステータス行に「確定」の語が含まれる場合は Proposed 以外の
/// ステータス表記（Accepted 等、決定が実際に確定した状態）でなければならない。
/// 「調査」等の語がタイトルに併記されていても「確定」の語自体が含まれていれば
/// 確定主張とみなす（誤って通過させないため、語による除外はしない）。
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
    let title_claims_finalized = title_line.contains("確定");

    assert!(
        !(is_proposed && title_claims_finalized),
        "ステータスが Proposed のままタイトルが確定を主張している: {title_line}"
    );
}
