//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--ddl-principals`
//! opt-in CLI 引数の閉じた形式パーサ（SQL-23・TASK-202、Issue #899）。
//!
//! `--search-engine`／`--durability`（同モジュール群参照）と同型の「プロセス
//! 起動時にのみ明示指定する注入点」であり、`engine::sql::mode::SessionState::
//! grant_ddl`（DDL 実行権限。`sql::ddl::require_ddl_privilege` が唯一の判定点）へ
//! untrusted な CLI 文字列から到達する唯一の入口を本モジュールに置く。
//!
//! 本モジュールはカンマ区切りのユーザー名リストの**構文**のみを検証する
//! （空文字列・空要素・重複・区切りの不正は fail-closed に拒否）。個々の
//! ユーザー名が `auth::UserStore` に実在するかの検証は `main.rs` が
//! `auth::UserStore::with_ddl_principals` へ委譲する（本モジュールは
//! `UserStore` を知らない——untrusted 入力の構文検証と、ロード済みユーザー
//! ストアに対する意味検証を分離する設計。`search_engine_opt`
//! （構文検証）と `auth.rs::UserStore`（意味検証）の分離と同じ方針）。
//!
//! `--ddl-principals` 未指定のサーバーは許可主体が存在しないため、全 DDL が
//! `42501` で拒否される（fail-closed 既定。`sql::ddl` モジュールドキュメント
//! 参照）。

/// `--ddl-principals` の CLI フラグ名（Issue #899）。
pub const FLAG: &str = "--ddl-principals";

/// `raw`（`--ddl-principals` の値。カンマ区切りのユーザー名リスト）を構文検証し、
/// 宣言順を保持したユーザー名の一覧へ変換する。
///
/// 拒否する形（いずれも fail-closed。`search_engine_opt::parse` と同じ
/// 「厳密一致のみ受理・曖昧な入力を黙って読み替えない」方針）:
/// - 空文字列（`""`）
/// - 空要素（`"alice,,bob"`・先頭/末尾のカンマ `",alice"`／`"alice,"`）
/// - 前後の空白を含む要素（trim しない。typo・コピペミスを黙って許容しない）
/// - 重複したユーザー名（`"alice,alice"`）
pub fn parse(raw: &str) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Err(format!("{FLAG} must not be empty"));
    }
    let mut names: Vec<String> = Vec::new();
    for part in raw.split(',') {
        if part.is_empty() {
            return Err(format!(
                "{FLAG} must not contain empty elements (got {raw:?})"
            ));
        }
        if part.chars().any(char::is_whitespace) {
            return Err(format!(
                "{FLAG} elements must not contain whitespace (got {part:?})"
            ));
        }
        if names.iter().any(|n| n == part) {
            return Err(format!(
                "{FLAG} must not contain duplicate usernames (got {part:?} twice)"
            ));
        }
        names.push(part.to_string());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_single_and_multiple_names() {
        assert_eq!(parse("alice").unwrap(), vec!["alice".to_string()]);
        assert_eq!(
            parse("alice,bob").unwrap(),
            vec!["alice".to_string(), "bob".to_string()]
        );
    }

    #[test]
    fn parse_rejects_empty_value() {
        assert!(parse("").is_err());
    }

    #[test]
    fn parse_rejects_empty_elements() {
        for raw in [",", "alice,", ",alice", "alice,,bob"] {
            assert!(parse(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }

    #[test]
    fn parse_rejects_duplicate_usernames() {
        assert!(parse("alice,alice").is_err());
    }

    #[test]
    fn parse_rejects_whitespace_padded_elements() {
        for raw in [" alice", "alice ", "alice, bob"] {
            assert!(parse(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }

    #[test]
    fn parse_preserves_declaration_order() {
        assert_eq!(
            parse("carol,alice,bob").unwrap(),
            vec!["carol".to_string(), "alice".to_string(), "bob".to_string()]
        );
    }
}
