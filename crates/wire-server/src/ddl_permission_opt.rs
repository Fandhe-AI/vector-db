//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--ddl-allowed-users`
//! opt-in CLI 引数の閉じた検証パーサ（Issue #902・SQL-23・TASK-203）。
//!
//! `--durability`／`--search-engine` と同型の「プロセス起動時にのみ明示指定する
//! 注入点」であり、`engine::sql::mode::SessionState::allow_ddl`（DDL 実行権限。
//! `EngineCore::execute_parsed_in_session` の `DROP TABLE` 分岐が
//! `sql::ddl::require_ddl_permission` で参照する）へ untrusted な CLI 文字列から
//! 到達する唯一の入口を本モジュールに置く。
//!
//! 本モジュールはカンマ区切りの username 列挙をパースし重複・空要素を
//! fail-closed に拒否するところまでを担う。列挙した username が
//! `--users` で読み込むユーザーストアに実在するかの検証は
//! [`crate::auth::UserStore::with_ddl_allowed_users`] が担う（起動時に
//! 1 回だけ確定させる。未知ユーザーの指定を黙って無視しない）。
//!
//! 未指定は DDL 実行権限を持つユーザーを 0 人のまま維持する（全 DDL 文が
//! `42501` で拒否される既定。`sql::ddl::require_ddl_permission` ドキュメント
//! 参照）。

/// `--ddl-allowed-users` の CLI フラグ名（Issue #902）。
pub const FLAG: &str = "--ddl-allowed-users";

/// `raw`（`--ddl-allowed-users` の値。カンマ区切りの username 列挙）を検証する。
///
/// - 空文字列全体は拒否する（フラグを指定するなら少なくとも 1 人を挙げる
///   契約。`--ddl-allowed-users ""` で「誰も許可しない」を暗黙表現させない）。
/// - `,` で分割した各要素は前後の空白を trim しない（`durability_opt::parse` と
///   同じ「厳密一致のみ受理」方針。空白入り username を許容すると
///   `--users` ファイル側の username と黙って不一致になる事故を防げない）。
/// - 空要素（`"alice,,bob"`・先頭または末尾のカンマ）は拒否する。
/// - 重複要素は拒否する（typo によるスクリプトの二重指定で意図が読み取れない
///   構成になるのを防ぐ。`--search-engine` の重複フラグ拒否と同じ理由）。
pub fn parse(raw: &str) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Err(format!(
            "{FLAG} requires a non-empty comma-separated list of usernames"
        ));
    }
    let mut usernames: Vec<String> = Vec::new();
    for part in raw.split(',') {
        if part.is_empty() {
            return Err(format!(
                "{FLAG} must not contain empty usernames (got {raw:?})"
            ));
        }
        if usernames.iter().any(|u| u == part) {
            return Err(format!("{FLAG} lists username {part:?} more than once"));
        }
        usernames.push(part.to_string());
    }
    Ok(usernames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_single_username() {
        assert_eq!(parse("alice"), Ok(vec!["alice".to_string()]));
    }

    #[test]
    fn parse_accepts_multiple_usernames() {
        assert_eq!(
            parse("alice,bob"),
            Ok(vec!["alice".to_string(), "bob".to_string()])
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
    fn parse_does_not_trim_whitespace() {
        // 空白入り要素は許容しない設計だが、本パーサーは trim を一切行わず、
        // そのまま `Vec<String>` の一要素として通す（`--users` ファイル側との
        // 突合せは呼び出し元 `UserStore::with_ddl_allowed_users` が担う）。
        assert_eq!(parse(" alice"), Ok(vec![" alice".to_string()]));
    }
}
