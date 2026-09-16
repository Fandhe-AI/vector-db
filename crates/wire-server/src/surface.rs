//! `--surface` opt-in CLI 引数の閉じた語彙パーサ（Issue #734・TASK-171／HTTP-1）。
//!
//! クエリインターフェースを SQL 表層（PostgreSQL wire プロトコル。現行）と
//! NoSQL 表層（HTTP/1.1 最小サブセット。TASK-172 以降）の 2 表層とし、
//! 起動時 CLI で排他選択する契約の入口。`search_engine_opt`（Issue #656）と
//! 同型で、untrusted な CLI 文字列から表層選択へ到達する唯一の入口を本
//! モジュールに置く。
//!
//! 既定は `sql`（未指定時。既存の SQL wire 起動経路をそのまま通す）。
//! `sql`／`nosql` 以外の値・値欠落・2 回目以降の重複指定はいずれも
//! fail-closed で拒否し、既定へ黙って読み替えない（`main.rs::run_server` の
//! 引数ループが `search_engine_opt` と同じ作法で処理する）。
//!
//! `nosql` を選択した場合、`main.rs::run_server` は本モジュールの解決結果に
//! 応じて [`crate::server::accept_loop_with_engine`]（`Sql`）／
//! [`crate::http::listener::accept_loop_with_limiter`]（`Nosql`）のいずれか
//! 1 本だけを呼ぶ（HTTP-1 の排他方針・Issue #735・#743）。両表層とも bind 自体は
//! `main.rs::run_server` が単一の `GuardedBindAddrs::resolve`／`bind` 呼び出し
//! を共有した後に分岐するため、`nosql` 選択時にも WIRE-7 の bind ガード
//! （TLS 未構成時は loopback 限定）が同じ経路で適用される（HTTP-9）。

/// `--surface` の CLI フラグ名。
pub const FLAG: &str = "--surface";

/// `--surface` が受理する語彙（順序はエラーメッセージの一覧順・README 記載順
/// の単一情報源）。
pub const TOKENS: [&str; 2] = ["sql", "nosql"];

/// `--surface <token>` の解決結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Surface {
    /// 既定（未指定時）。PostgreSQL wire プロトコル互換の SQL 表層。
    #[default]
    Sql,
    /// NoSQL 表層（HTTP/1.1 最小サブセット）。`http::listener::
    /// accept_loop_with_limiter` が受け皿（Issue #735・#743。読み取り
    /// 30 秒タイムアウト・同時接続数 64 を SQL wire と同一契約で適用する。
    /// 要求の解釈・応答生成は暫定ハンドラにとどまり、本体は Issue #747）。
    Nosql,
}

/// `raw`（CLI 引数の値）を [`TOKENS`] の厳密一致でのみ受理する（trim・大小
/// 文字の読み替えはしない。`search_engine_opt::parse` と同じ「厳密一致のみ
/// 受理」方針。曖昧な入力を黙って読み替えると、typo で意図と異なる表層が
/// 選ばれる事故を fail-closed で防げなくなる）。
pub fn parse(raw: &str) -> Result<Surface, String> {
    match raw {
        "sql" => Ok(Surface::Sql),
        "nosql" => Ok(Surface::Nosql),
        other => Err(format!("{FLAG} must be one of {TOKENS:?} (got {other:?})")),
    }
}

impl Surface {
    /// [`TOKENS`] のうち自身に対応する文字列表現（診断・テスト用）。
    pub fn token(self) -> &'static str {
        match self {
            Self::Sql => "sql",
            Self::Nosql => "nosql",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_all_tokens() {
        for tok in TOKENS {
            assert!(parse(tok).is_ok(), "expected {tok:?} to be accepted");
        }
    }

    #[test]
    fn token_round_trips_through_parse() {
        for tok in TOKENS {
            let surface = parse(tok).expect("valid token");
            assert_eq!(surface.token(), tok);
        }
    }

    #[test]
    fn parse_rejects_case_variants_whitespace_and_unknown_values() {
        for raw in [
            "SQL", "NoSQL", "Sql", " sql", "sql ", "", "http", "nosql\n", "sql\0x",
        ] {
            assert!(parse(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }

    #[test]
    fn parse_rejects_control_character_injection_without_leaking_raw_bytes() {
        // untrusted な raw に制御文字が混じっても厳密一致で弾かれ、エラー
        // 文言へそのまま埋め込まれても Debug 表記（`{:?}`）でエスケープ
        // される（`search_engine_opt::parse` の同種テストと同じ防御的姿勢）。
        let err = parse("sql\0bogus").expect_err("must reject control characters");
        assert!(err.contains(FLAG));
        assert!(err.contains("\\0") || !err.contains('\0'));
    }

    #[test]
    fn default_is_sql() {
        assert_eq!(Surface::default(), Surface::Sql);
    }
}
