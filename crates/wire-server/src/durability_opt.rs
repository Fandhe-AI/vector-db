//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--durability`
//! opt-in CLI 引数の閉じた語彙パーサ（Issue #850）。
//!
//! `--search-engine`（Issue #656。同モジュール `search_engine_opt` 参照）と
//! 同型の「プロセス起動時にのみ明示指定する注入点」であり、
//! `engine::storage::WriteDurability`（Issue #849。書き込みトランザクション
//! durability の構築時オプション。`engine::core::EngineCore::
//! open_with_durability`／`engine::storage::Storage::open_with_durability`
//! へ渡す）へ untrusted な CLI 文字列から到達する唯一の入口を本モジュールに
//! 置く。
//!
//! `WriteDurability` は `#[non_exhaustive]` ではない（`engine::storage`
//! モジュールドキュメント「値の追加は破壊的変更として扱う」参照）。将来
//! variant が追加される場合は本モジュールの [`TOKENS`]・[`parse`]・
//! [`token_for`] の `match` を明示的に更新する契約とする（黙って既存 2 値の
//! ままにしない）。
//!
//! `WriteDurability::None` を選ぶと commit 成功応答は永続を保証しなくなる
//! （損失ウィンドウの詳細は `docs/design/ingest-write-path.md`「Issue #849
//! 追記」節参照）。本モジュールは値の受理・拒否のみを担い、非既定値を選んだ
//! 場合の起動時警告出力は `main.rs::run_server` の責務とする。

use engine::storage::WriteDurability;

/// `--durability` の CLI フラグ名（Issue #850）。
pub const FLAG: &str = "--durability";

/// `--durability` が受理する語彙（順序は `parse` の分岐・エラーメッセージの
/// 一覧順・README 記載順の単一情報源）。
pub const TOKENS: [&str; 2] = ["immediate", "none"];

/// `raw`（CLI 引数の値）を [`TOKENS`] の厳密一致でのみ受理する
/// （trim・大文字小文字の読み替えはしない。`search_engine_opt::parse` と
/// 同じ「厳密一致のみ受理」方針。曖昧な入力を黙って読み替えると、typo で
/// 意図と異なる durability が選ばれる事故を fail-closed で防げなくなる）。
pub fn parse(raw: &str) -> Result<WriteDurability, String> {
    match raw {
        "immediate" => Ok(WriteDurability::Immediate),
        "none" => Ok(WriteDurability::None),
        other => Err(format!("{FLAG} must be one of {TOKENS:?} (got {other:?})")),
    }
}

/// [`TOKENS`] のうち `d` に対応する文字列表現（診断・警告メッセージ用）。
///
/// `WriteDurability` は engine クレート定義のため本クレートでは inherent
/// メソッドを持てず、フリー関数として持つ（`search_engine_opt::
/// SearchEngineChoice::token` は自クレート定義の型なのでメソッドにできるが、
/// 本モジュールは同じ役割をフリー関数で担う）。
pub fn token_for(d: WriteDurability) -> &'static str {
    match d {
        WriteDurability::Immediate => "immediate",
        WriteDurability::None => "none",
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
    fn parse_immediate_maps_to_default() {
        assert_eq!(parse("immediate"), Ok(WriteDurability::Immediate));
        assert_eq!(WriteDurability::Immediate, WriteDurability::default());
    }

    #[test]
    fn parse_none_maps_to_none_variant() {
        assert_eq!(parse("none"), Ok(WriteDurability::None));
    }

    #[test]
    fn parse_rejects_case_variants_and_whitespace() {
        for raw in [
            "Immediate",
            "IMMEDIATE",
            " none",
            "none ",
            "None",
            "",
            "sync",
            "immediate\n",
        ] {
            assert!(
                parse(raw).is_err(),
                "expected {raw:?} to be rejected (strict match only)"
            );
        }
    }

    #[test]
    fn parse_rejects_control_character_injection() {
        // untrusted な引数に制御文字が混じっても厳密一致で弾かれ、エラー文言へ
        // そのまま埋め込まれても Debug 表記（`{:?}`）でエスケープされることを
        // 確認する（`search_engine_opt::parse` と同じ防御的姿勢）。
        let err = parse("none\0bogus").expect_err("must reject control characters");
        assert!(err.contains(FLAG));
    }

    #[test]
    fn token_round_trips_through_parse() {
        for tok in TOKENS {
            let d = parse(tok).expect("valid token");
            assert_eq!(token_for(d), tok);
        }
    }
}
