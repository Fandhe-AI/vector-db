//! `crate::catalog::table_generation_in_txn`（テーブル単位の世代照合。`USING PLAN`
//! の I/O 前後照合が対象テーブルのみを見るための土台。TASK-77・SQL-5・codex-review
//! P1 再指摘・PR #266）の「呼び忘れ検出」構造テスト。
//!
//! `bump_table_generation_in_txn` のドキュメントコメント（`catalog.rs`）が挙げる
//! 契約は「対象テーブルの `CATALOG_TABLE`／`user_rows/{table_name}` を変更する
//! すべての write commit の直前で呼ぶ」だが、この契約はコンパイラでは強制されない
//! （呼び忘れても型エラーにならない）。呼び忘れは `USING PLAN` の世代照合が対象
//! テーブルの実変更を見逃す fail-open（テナント境界の失効検出漏れと同種の重大度。
//! `.claude/rules/security.md` P0「テナント分離の検査を外す/緩める/バイパス経路を
//! 作らない」に準ずる）に直結するため、`tests/isa.rs`
//! `unsafe_is_confined_to_isa_module_with_safety_comments` と同じ「ソーステキスト
//! 走査」の手法で、`crate::recovery::commit_boundary::commit(...)` の呼び出し箇所を
//! 悉皆列挙し、各呼び出し箇所が (a) 直前の近傍行に `bump_table_generation_in_txn`
//! 呼び出しを持つか、(b) 明示的なアローリスト（`user_rows/{table}` を経由しない
//! 旧・非テーブルスコープ経路であることが確認済みの箇所）に含まれることを固定する。
//! 新たな書き込み経路（新しい commit 呼び出し）を追加した場合、本テストが
//! アローリストへの追記または `bump_table_generation_in_txn` 呼び出しの追加を
//! 強制する。

use std::path::{Path, PathBuf};

/// `commit_boundary::commit(...)` を直接呼ぶが、意図的に
/// `bump_table_generation_in_txn` を伴わない箇所（呼び出し元ファイル名・行番号）。
///
/// - `storage.rs` の `Storage::put`/`Storage::put_batch`: `storage.rs::ROWS_TABLE`
///   （旧・非テーブルスコープの単一 redb テーブル）を書く経路で、`catalog.rs`
///   の `user_rows/{table_name}` とは別テーブル。SQL 表層（`USING PLAN` を含む
///   すべてのクエリ実行）は `user_rows/{table_name}` のみを読み、`ROWS_TABLE`
///   を経由しない（`crates/engine/src/sql/exec.rs`・`arena.rs`・`rls.rs` に
///   `ROWS_TABLE` への直接依存がないことをコメントで確認済み。PR #266 レビュー
///   対応）。
/// - `recovery/panic_hook.rs`: `#[cfg(test)]` 内のテスト fixture が
///   `storage.rs::ROWS_TABLE` へ直接書き込む箇所で、上記と同じ理由で対象外。
const ALLOWLIST: &[(&str, u32)] = &[
    ("storage.rs", 550),
    ("storage.rs", 577),
    ("recovery/panic_hook.rs", 404),
];

#[test]
fn every_commit_boundary_commit_call_bumps_table_generation_or_is_allowlisted() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut rs_files = Vec::new();
    collect_rs_files(&src_dir, &mut rs_files);
    assert!(!rs_files.is_empty(), "no .rs files found under src/");

    let needles = [
        "commit_boundary::commit(",
        "commit_boundary::commit_and_finish(",
        "commit_boundary::commit_write_txn_guarded(",
    ];

    let mut checked_call_sites = 0usize;
    let mut allowlist_hits: std::collections::HashSet<(&str, u32)> =
        std::collections::HashSet::new();

    for path in &rs_files {
        let content = std::fs::read_to_string(path).expect("read source file");
        let lines: Vec<&str> = content.lines().collect();
        let rel_name = path
            .strip_prefix(&src_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        for (idx, line) in lines.iter().enumerate() {
            if !needles.iter().any(|n| line.contains(n)) {
                continue;
            }
            checked_call_sites += 1;
            let line_no = (idx + 1) as u32;

            if let Some(entry) = ALLOWLIST
                .iter()
                .find(|(name, no)| *name == rel_name.as_str() && *no == line_no)
            {
                allowlist_hits.insert(*entry);
                continue;
            }

            // アローリスト外の呼び出しは、同一関数内で近傍（直前 60 行以内）に
            // `bump_table_generation_in_txn` を伴うことを要求する（関数の長さは
            // `tenant.rs::replace_typed_rows_by_text_key` が最長で、60 行あれば
            // 同一トランザクションのスコープ内に収まる。関数境界を跨いで誤検出
            // しないよう、直前に `pub`/`pub(crate) fn` の宣言行が現れたら
            // 探索を打ち切る）。
            let start = idx.saturating_sub(60);
            let mut has_bump = false;
            for l in lines[start..idx].iter().rev() {
                if l.contains("bump_table_generation_in_txn") {
                    has_bump = true;
                    break;
                }
                if (l.contains("fn ") && (l.contains("pub fn") || l.contains("pub(crate) fn")))
                    && !l.contains("bump_table_generation_in_txn")
                {
                    break;
                }
            }
            assert!(
                has_bump,
                "{}:{} calls commit_boundary::commit* without a preceding \
                 bump_table_generation_in_txn call and is not in the ALLOWLIST; either add the \
                 bump call before commit, or add (\"{}\", {}) to ALLOWLIST with a documented \
                 reason (this file's module doc comment)",
                rel_name, line_no, rel_name, line_no
            );
        }
    }

    assert!(
        checked_call_sites >= ALLOWLIST.len(),
        "expected to find at least as many commit_boundary::commit* call sites as ALLOWLIST \
         entries (found {checked_call_sites}); ALLOWLIST may be stale"
    );
    assert_eq!(
        allowlist_hits.len(),
        ALLOWLIST.len(),
        "some ALLOWLIST entries did not match any commit_boundary::commit* call site; \
         ALLOWLIST is stale (line numbers likely shifted) and must be updated"
    );
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
