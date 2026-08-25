//! 取得モード（`recall`／`precision`）の構文解決コア（TASK-161、対象ビヘイビア: SQL-12。
//! ポインタ: docs/spec/05-tasks.md TASK-161・docs/spec/04-behavior/sql-surface.md SQL-12）。
//!
//! 責務境界: クエリ単位の `USING MODE` 句・セッション変数 `SET search_mode`
//! （いずれも [`allowlist`](crate::sql::allowlist) が構文として受理し、生のリテラル
//! 文字列を返す）から得られる値を、優先順位（クエリ句 > セッション変数 > 既定）に
//! 従って決定的に解決する。解決結果（[`ResolvedMode`]）は
//! [`parser::BoundStatement`](crate::sql::parser::BoundStatement)
//! （`bind_with_session`）が保持する。
//!
//! `precision` の**実行契約**（確信度判定・空集合 fail-closed 応答）は本モジュールの
//! 管轄外（TASK-162・対象ビヘイビア SEARCH-9）。本モジュールは構文・優先順位の解決
//! までを担い、`recall`／`precision` いずれの値も対等に解決する（fail-open のような
//! 値の書き換えは行わない）。
//!
//! [`SessionState`] は wire 接続 1 本につき 1 個、呼び出し元（wire-server の接続
//! ハンドラ。TASK-73・TASK-165 の管轄）が所有する値型として設計する。`EngineCore`
//! 等の複数接続で共有される構造体には置かない（接続間でモードが混線しない構造を
//! 型で担保するため）。

use crate::sql::allowlist::SqlSurfaceError;

/// 取得モード（README「実装方針（要点）」で公開済みの `recall`〔既定・広域〕／
/// `precision`〔ピンポイント〕の 2 値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Recall,
    Precision,
}

impl SearchMode {
    /// `USING MODE`／`SET search_mode` のリテラル値を解決する（SQL-12）。
    ///
    /// 完全一致のみ受理する（大文字・前後空白・空文字はすべて拒否）。fail-closed
    /// 側に倒す方針（緩和は後から容易だが、逆〔一度緩めた受理範囲を狭める〕は
    /// 互換性破壊になるため）。不一致は [`SqlSurfaceError::InvalidInput`]
    /// （ERR-2: `22000`。構文上は文字列リテラルとして正しく受理されたが値が不正）。
    pub fn parse_literal(literal: &str) -> Result<Self, SqlSurfaceError> {
        match literal {
            "recall" => Ok(SearchMode::Recall),
            "precision" => Ok(SearchMode::Precision),
            other => Err(SqlSurfaceError::invalid_input(format!(
                "unknown search mode: {other}"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SearchMode::Recall => "recall",
            SearchMode::Precision => "precision",
        }
    }
}

/// 実効モードの指定元。TASK-164（PLAN-11）が `USING PLAN` のモード推定を追加する際の
/// 拡張点（クエリ句 > セッション変数 > プランナー推定 > 既定 の優先順位に合わせ、
/// `SessionVariable` と `Default` の間へ新しい variant を追加する想定。本タスク
/// （TASK-161）では未実装）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSource {
    QueryClause,
    SessionVariable,
    Default,
}

/// 優先順位解決を終えた実効モードと、その指定元。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedMode {
    pub mode: SearchMode,
    pub source: ModeSource,
}

/// クエリ句・セッション変数それぞれの解決済み値から実効モードを決定する
/// （副作用なし・決定的。同一入力には常に同一の結果を返す）。優先順位は
/// **クエリ句 > セッション変数 > 既定（`recall`）**（SQL-12）。
pub fn resolve_mode(query: Option<SearchMode>, session: Option<SearchMode>) -> ResolvedMode {
    if let Some(mode) = query {
        return ResolvedMode {
            mode,
            source: ModeSource::QueryClause,
        };
    }
    if let Some(mode) = session {
        return ResolvedMode {
            mode,
            source: ModeSource::SessionVariable,
        };
    }
    ResolvedMode {
        mode: SearchMode::Recall,
        source: ModeSource::Default,
    }
}

/// 接続（セッション）単位のモード状態。呼び出し元が 1 接続につき 1 個所有する
/// 値型（モジュールドキュメント参照）。意図的に `Copy` を実装しない
/// （`Copy` だと呼び出し元が値でコピーを保持したまま `set_search_mode` を
/// コピー側へ適用し、`SET` が見かけ上成功しつつ元のセッションへ反映されない
/// 事故を型で防げなくなるため。呼び出し元は `&mut SessionState` を経由して
/// 更新する契約とする）。
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    search_mode: Option<SearchMode>,
}

impl SessionState {
    pub fn search_mode(&self) -> Option<SearchMode> {
        self.search_mode
    }

    /// 検証済みの値のみを受け取る契約（呼び出し元 `core.rs::EngineCore::execute_sql_in_session`
    /// が `SearchMode::parse_literal` の検証成功後にのみ呼ぶ）。失敗した `SET` が
    /// セッションを変更しないことは、この「検証→代入」の呼び出し順序で保証する
    /// （部分更新＝黙った既定化と同種の fail-open を防ぐ。security.md 準拠）。
    pub fn set_search_mode(&mut self, mode: SearchMode) {
        self.search_mode = Some(mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_mode_defaults_to_recall_when_unset() {
        let resolved = resolve_mode(None, None);
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::Default);
    }

    #[test]
    fn resolve_mode_session_variable_wins_over_default() {
        let resolved = resolve_mode(None, Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::SessionVariable);
    }

    #[test]
    fn resolve_mode_query_clause_wins_over_session_variable() {
        let resolved = resolve_mode(Some(SearchMode::Recall), Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn resolve_mode_query_clause_alone_wins_over_default() {
        let resolved = resolve_mode(Some(SearchMode::Precision), None);
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn parse_literal_accepts_only_exact_lowercase_values() {
        assert_eq!(
            SearchMode::parse_literal("recall").expect("recall should parse"),
            SearchMode::Recall
        );
        assert_eq!(
            SearchMode::parse_literal("precision").expect("precision should parse"),
            SearchMode::Precision
        );
    }

    #[test]
    fn parse_literal_rejects_case_variants_whitespace_and_unknown_values() {
        assert!(SearchMode::parse_literal("RECALL").is_err());
        assert!(SearchMode::parse_literal("Recall").is_err());
        assert!(SearchMode::parse_literal(" recall").is_err());
        assert!(SearchMode::parse_literal("recall ").is_err());
        assert!(SearchMode::parse_literal("").is_err());
        assert!(SearchMode::parse_literal("fuzzy").is_err());
    }

    #[test]
    fn session_state_defaults_to_none_and_can_be_overwritten() {
        let mut session = SessionState::default();
        assert_eq!(session.search_mode(), None);
        session.set_search_mode(SearchMode::Precision);
        assert_eq!(session.search_mode(), Some(SearchMode::Precision));
        session.set_search_mode(SearchMode::Recall);
        assert_eq!(session.search_mode(), Some(SearchMode::Recall));
    }

    #[test]
    fn session_state_instances_are_independent() {
        // TASK-161 設計判断: セッション状態は接続単位の値型。2 つの独立したインスタンス間で
        // 一方への `set_search_mode` が他方へ波及しないことを検査する（接続間漏えいの
        // 構造的排除。security.md「テナント境界」と同種の分離思想）。
        let mut session_a = SessionState::default();
        let session_b = SessionState::default();
        session_a.set_search_mode(SearchMode::Precision);
        assert_eq!(session_a.search_mode(), Some(SearchMode::Precision));
        assert_eq!(session_b.search_mode(), None);
    }
}
