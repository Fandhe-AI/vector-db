//! `EXPLAIN`（TASK-78・SQL-6）応答の構築。`core.rs::EngineCore::
//! execute_sql_in_session` の `Statement::Explain` アームが LLM クエリ展開・
//! モード解決（`EngineCore::plan_query_with_mode`、TASK-164・PLAN-11）した結果
//! （[`PlannedQuery`]）を受け取り、クライアントが確認できる `QUERY PLAN` 単一列の
//! [`QueryResult`] へ決定的に整形するところまでを担う。
//!
//! 責務境界: 本モジュールは純粋な整形ロジックのみを持つ（DB I/O・LLM 呼び出しは
//! 行わない。呼び出し元 `core.rs` が LLM 展開・モード解決を完了させたうえで
//! [`build_explain_result`] を呼ぶ）。`EXPLAIN` は検索本体（ハイブリッド実行）を
//! 実行しないため、行の `id`/`score` は実在行を持たない疑似値（`0`）とする。
//!
//! 行内容は SQL-6・SQL-12（TASK-161・PLAN-11）が要求する「展開後の検索語・
//! ソフトヒント・解決済み実効モードと指定元」を、決定的順序・英語表記
//! （プログラム出力文字列は英語）で並べる。一度出した行の形式・順序は
//! **安定契約**として今後変更しない（`sql::mode::ModeSource::as_str` のドキュメント
//! コメントと同じ方針）。security.md P0: LLM プロンプト本文・生応答本文は含めず、
//! 厳格パース済みの構造化フィールド（[`crate::query_planner::QueryExpansion`]）
//! のみを使う。

use crate::query_planner::PlannedQuery;
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};

/// `EXPLAIN` 応答の列名（安定契約。一度出したら変えない）。
const QUERY_PLAN_COLUMN: &str = "QUERY PLAN";

/// ソフトヒント未指定時の固定表記（安定契約）。
const NONE_LABEL: &str = "(none)";

/// [`PlannedQuery`]（LLM 展開結果＋解決済み実効モード）から `EXPLAIN` の
/// [`QueryResult`] を決定的に構築する（副作用なし。同一入力には常に同一の行を
/// 返す）。行順序: `search_terms[i]`（展開結果の件数分）→ `path_hint` →
/// `kind_hint` → `mode` → `mode_source`。
pub(crate) fn build_explain_result(planned: &PlannedQuery) -> QueryResult {
    let expansion = planned.expansion();
    let resolved = planned.mode();

    let mut lines: Vec<String> = Vec::with_capacity(expansion.search_terms.len() + 4);
    for (i, term) in expansion.search_terms.iter().enumerate() {
        lines.push(format!("search_terms[{i}]: {term}"));
    }
    lines.push(format!(
        "path_hint: {}",
        expansion.path_hint.as_deref().unwrap_or(NONE_LABEL)
    ));
    lines.push(format!(
        "kind_hint: {}",
        expansion.kind_hint.as_deref().unwrap_or(NONE_LABEL)
    ));
    lines.push(format!("mode: {}", resolved.mode().as_str()));
    lines.push(format!("mode_source: {}", resolved.source().as_str()));

    let rows = lines
        .into_iter()
        .map(|text| ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Text(text)],
        })
        .collect();

    QueryResult {
        columns: vec![ColumnMeta::Computed {
            name: QUERY_PLAN_COLUMN.to_string(),
        }],
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_planner::QueryExpansion;
    use crate::sql::mode::{ModeSource, ResolvedMode, SearchMode};

    fn cell_text(result: &QueryResult, row: usize) -> &str {
        match &result.rows[row].cells[0] {
            Cell::Text(s) => s.as_str(),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    #[test]
    fn build_explain_result_orders_search_terms_then_hints_then_mode() {
        let expansion = QueryExpansion {
            search_terms: vec!["alpha".to_string(), "beta".to_string()],
            path_hint: Some("src/lib.rs".to_string()),
            kind_hint: Some("fn".to_string()),
            ..QueryExpansion::default()
        };
        let planned = PlannedQuery::new(
            expansion,
            ResolvedMode::new(SearchMode::Precision, ModeSource::QueryClause),
        );

        let result = build_explain_result(&planned);

        assert_eq!(result.columns.len(), 1);
        assert_eq!(
            result.columns[0],
            ColumnMeta::Computed {
                name: QUERY_PLAN_COLUMN.to_string()
            }
        );
        assert_eq!(result.rows.len(), 6);
        assert_eq!(cell_text(&result, 0), "search_terms[0]: alpha");
        assert_eq!(cell_text(&result, 1), "search_terms[1]: beta");
        assert_eq!(cell_text(&result, 2), "path_hint: src/lib.rs");
        assert_eq!(cell_text(&result, 3), "kind_hint: fn");
        assert_eq!(cell_text(&result, 4), "mode: precision");
        assert_eq!(cell_text(&result, 5), "mode_source: query_clause");
    }

    #[test]
    fn build_explain_result_uses_none_label_for_absent_hints() {
        let expansion = QueryExpansion {
            search_terms: Vec::new(),
            path_hint: None,
            kind_hint: None,
            ..QueryExpansion::default()
        };
        let planned = PlannedQuery::new(
            expansion,
            ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
        );

        let result = build_explain_result(&planned);

        // 検索語 0 件のため行は path_hint/kind_hint/mode/mode_source の 4 行。
        assert_eq!(result.rows.len(), 4);
        assert_eq!(cell_text(&result, 0), "path_hint: (none)");
        assert_eq!(cell_text(&result, 1), "kind_hint: (none)");
        assert_eq!(cell_text(&result, 2), "mode: recall");
        assert_eq!(cell_text(&result, 3), "mode_source: default");
    }

    #[test]
    fn build_explain_result_reports_all_four_mode_sources() {
        for (mode, source, expected_source) in [
            (SearchMode::Recall, ModeSource::QueryClause, "query_clause"),
            (
                SearchMode::Precision,
                ModeSource::SessionVariable,
                "session_variable",
            ),
            (
                SearchMode::Recall,
                ModeSource::PlannerEstimate,
                "planner_estimate",
            ),
            (SearchMode::Recall, ModeSource::Default, "default"),
        ] {
            let planned =
                PlannedQuery::new(QueryExpansion::default(), ResolvedMode::new(mode, source));
            let result = build_explain_result(&planned);
            let last = result.rows.len() - 1;
            assert_eq!(
                cell_text(&result, last),
                format!("mode_source: {expected_source}")
            );
        }
    }
}
