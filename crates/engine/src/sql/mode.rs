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
///
/// **TASK-161 で意図的に付与した破壊的変更（BREAKING CHANGE）**: `#[non_exhaustive]` を
/// 付与したため、クレート外で本 enum を網羅的に `match` していたコードは
/// `_ => ...` 等のワイルドカードアーム追加が必須になる（詳細は PR #188 の
/// Breaking Changes 節を参照）。
///
/// `#[non_exhaustive]`: 将来 TASK-164 で `PlannerEstimate` 等の variant を追加した際、
/// クレート外の `match` 式が網羅性エラーでコンパイル不能になる破壊的変更を防ぐ
/// （AGENTS.md「公開 API・エラー契約の互換性（P1）」。PR #188 レビュー指摘対応:
/// `BoundStatement`／`ValidatedStatement` と同種の拡張点保護）。クレート外で本 enum を
/// `match` する場合は `_ => ...` 等のワイルドカードアームが必須になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModeSource {
    QueryClause,
    SessionVariable,
    Default,
}

/// 優先順位解決を終えた実効モードと、その指定元。
///
/// **TASK-161 で意図的に非公開化した破壊的変更（BREAKING CHANGE）**: 全フィールドを
/// `pub` から `pub(crate)` へ変更し `#[non_exhaustive]` を付与した。クレート外からの
/// 直接のフィールド参照・構造体リテラル構築は今後不可能。構築は [`ResolvedMode::new`]、
/// 読み取りは [`ResolvedMode::mode`]／[`ResolvedMode::source`] を使う（詳細は PR #188 の
/// Breaking Changes 節を参照。TASK-164 拡張点の前方互換確保とカプセル化のため）。
///
/// `#[non_exhaustive]`: 本構造体に将来フィールドを追加しても（例:
/// `ModeSource::PlannerEstimate` 導入時の付随情報）、既存の構造体リテラル構築コードが
/// 必須フィールド不足でコンパイル不能になる破壊的変更を防ぐ（PR #188 レビュー指摘
/// 対応: `BoundStatement`／`ValidatedStatement` と同種の拡張点保護）。フィールドは
/// カプセル化のため `pub(crate)` とし、クレート外からの構築は [`ResolvedMode::new`]、
/// 読み取りは [`ResolvedMode::mode`]／[`ResolvedMode::source`] を経由する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedMode {
    pub(crate) mode: SearchMode,
    pub(crate) source: ModeSource,
}

impl ResolvedMode {
    /// クレート外から解決済みモードを構築する constructor（`resolve_mode` を経ない
    /// テスト・拡張コード向け）。
    pub fn new(mode: SearchMode, source: ModeSource) -> Self {
        Self { mode, source }
    }

    /// 解決された実効モード。
    pub fn mode(&self) -> SearchMode {
        self.mode
    }

    /// 実効モードの指定元。
    pub fn source(&self) -> ModeSource {
        self.source
    }
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
    /// TASK-79（SQL-9）: `CREATE FUNCTION` で登録した宣言的 UDF のセッション単位
    /// レジストリ。`SessionState` 自体が接続（＝認証済みテナント）単位の値型であるため、
    /// UDF 定義が他接続・他テナントへ漏れる経路は構造上存在しない。永続化しない
    /// （`crate::sql::udf_call` モジュールドキュメント参照）。
    udfs: crate::sql::udf_call::UdfRegistry,
}

impl SessionState {
    pub fn search_mode(&self) -> Option<SearchMode> {
        self.search_mode
    }

    /// このセッションに登録済みの UDF レジストリ（読み取り専用）。
    pub fn udfs(&self) -> &crate::sql::udf_call::UdfRegistry {
        &self.udfs
    }

    /// `core.rs::EngineCore::execute_sql_in_session` の `CreateFunction` 分岐から
    /// 呼ばれる。登録の検証自体は `udf_call::define_function` が担い、本メソッドは
    /// セッションが保持する `&mut UdfRegistry` を貸し出すだけの薄いアクセサ。
    pub fn udfs_mut(&mut self) -> &mut crate::sql::udf_call::UdfRegistry {
        &mut self.udfs
    }

    /// TASK-149（対象ビヘイビア: EXT-5, EXT-6）: コンパイル済み WASM UDF
    /// （`crate::wasm_udf::compile` の戻り値）をこのセッションのレジストリへ登録
    /// する。検証・名前空間の衝突検査は `udf_call::define_wasm_function` が担い、
    /// 登録は宣言的 UDF と同じ「セッション（＝認証済みテナントの接続単位）に
    /// 閉じる・永続化しない」構造に従う。SQL からの登録構文（`CREATE FUNCTION
    /// ... AS WASM ...`）・wire 経由のモジュール搬送は本タスクのスコープ外で、
    /// 呼び出し元がモジュールバイト列の取得・コンパイルを担う。
    pub fn register_wasm_udf(
        &mut self,
        name: &str,
        backend: std::sync::Arc<dyn crate::wasm_udf::WasmUdfBackend>,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::udf_call::define_wasm_function(&mut self.udfs, name, backend)
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
