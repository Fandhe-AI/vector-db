//! `EXPLAIN`（TASK-78・SQL-6）応答の構築。`core.rs::EngineCore::
//! execute_sql_in_session` の `Statement::Explain` アームが LLM クエリ展開・
//! モード解決（`EngineCore::plan_query_with_mode`、TASK-164・PLAN-11）した結果
//! （[`PlannedQuery`]）と、検索エンジン種別・ANN 静的適用判定
//! （[`ExplainEngine`]、Issue #411）を受け取り、クライアントが確認できる
//! `QUERY PLAN` 単一列の [`QueryResult`] へ決定的に整形するところまでを担う。
//!
//! 責務境界: 本モジュールは純粋な整形ロジックのみを持つ（DB I/O・LLM 呼び出しは
//! 行わない。呼び出し元 `core.rs` が LLM 展開・モード解決・[`ExplainEngine`] の
//! 組み立てを完了させたうえで [`build_explain_result`] を呼ぶ）。`EXPLAIN` は
//! 検索本体（ハイブリッド実行）を実行しないため、行の `id`/`score` は実在行を
//! 持たない疑似値（`0`）とする。`engine:`／`ann_plan:` 行も実行時の縮退結果では
//! なく、クエリ形状とエンジン設定から決まる**静的判定**
//! （`sql::hnsw_cache::classify_ann_plan`）をそのまま報告する（実行時
//! fail-closed 縮退・hybrid 再取得ラウンド数は対象外。可視カーディナリティ・
//! 閾値・行数等のテナント存在情報に繋がる数値は一切含めない。security.md
//! 「テナント境界」対応）。
//!
//! 行内容は SQL-6・SQL-12（TASK-161・PLAN-11）が要求する「展開後の検索語・
//! ソフトヒント・解決済み実効モードと指定元」に、Issue #411 で「使用エンジン・
//! ANN パラメータ・適用判定」を追記したもの。決定的順序・英語表記（プログラム
//! 出力文字列は英語）で並べる。一度出した行の形式・順序は**安定契約**として
//! 今後変更しない（`sql::mode::ModeSource::as_str` のドキュメントコメントと
//! 同じ方針）。既存 6 行（`search_terms[i]`…`mode_source`）は不変、新規行は
//! `mode_source` の直後へ追記のみで既定エンジン時の出力は変更前と後方互換
//! （TASK-164 で `mode_source` を追加した前例と同じ方針）。security.md P0:
//! LLM プロンプト本文・生応答本文は含めず、厳格パース済みの構造化フィールド
//! （[`crate::query_planner::QueryExpansion`]）のみを使う。
//!
//! `docs/design/explain-search-engine-exposure.md` に露出する行・語彙・
//! 露出しない値と理由をまとめる。

use crate::query_planner::PlannedQuery;
use crate::search_engine::SearchEngineKind;
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::hnsw_cache::AnnPlan;

/// `EXPLAIN` 応答の列名（安定契約。一度出したら変えない）。
const QUERY_PLAN_COLUMN: &str = "QUERY PLAN";

/// ソフトヒント未指定時の固定表記（安定契約）。
const NONE_LABEL: &str = "(none)";

/// `search_engine_kind()` が `None`（provider を直接注入する `with_provider`／
/// `from_storage` 経由。`kind` との対応を構造的に検証できない）の場合の固定表記
/// （安定契約）。ヒント未指定の [`NONE_LABEL`] と意味が異なるため区別する。
const CUSTOM_PROVIDER_LABEL: &str = "(custom_provider)";

/// `EXPLAIN` の `engine:`／`hnsw_params:`／`ann_plan:` 行（Issue #411）を組み立てる
/// ための入力。呼び出し元 `core.rs::EngineCore::execute_sql_in_session` の
/// `Statement::Explain` アームが、実行時に executor（`sql::exec`）が使うのと同じ
/// 源泉（`EngineCore::search_engine_kind()`・`sql::hnsw_cache::classify_ann_plan`）
/// から組み立てる。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExplainEngine {
    /// [`crate::core::EngineCore::search_engine_kind`] の戻り値そのまま。
    pub(crate) kind: Option<SearchEngineKind>,
    /// [`crate::sql::hnsw_cache::classify_ann_plan`] の判定結果（静的判定）。
    pub(crate) ann_plan: AnnPlan,
}

/// [`ExplainEngine::kind`] を `engine:` 行の値（閉じた語彙・snake_case）へ変換する。
/// [`SearchEngineKind`] の [`std::fmt::Display`] 実装は `full_scan_ratio` を含む
/// 診断・ログ向けの表現であり、テナント存在情報に繋がらない値のみを露出する
/// `EXPLAIN` の契約とは別に保つため、ここで専用の網羅 `match` を持つ
/// （`SearchEngineKind` は本クレート内 `#[non_exhaustive]` の影響を受けない）。
fn engine_token(kind: Option<SearchEngineKind>) -> &'static str {
    match kind {
        None => CUSTOM_PROVIDER_LABEL,
        Some(SearchEngineKind::CpuScalarBruteForce) => "cpu_scalar_brute_force",
        Some(SearchEngineKind::ParallelBruteForce) => "parallel_brute_force",
        Some(SearchEngineKind::Hnsw(_)) => "hnsw",
        // `SearchEngineKind` は `#[non_exhaustive]` だが本クレート内なので
        // 網羅チェックは効く。将来 variant が追加された場合はコンパイルエラーで
        // ここへの追記を強制する（fail-closed。未知エンジンを偽装しない）。
    }
}

/// [`AnnPlan`] を `ann_plan:` 行の値（閉じた語彙・snake_case）へ変換する。
fn ann_plan_token(plan: AnnPlan) -> &'static str {
    match plan {
        AnnPlan::PlainScanEngine => "plain_scan_engine",
        AnnPlan::PlainScanPrecision => "plain_scan_precision",
        AnnPlan::HnswFullVisible => "hnsw_full_visible",
        AnnPlan::HnswSubset => "hnsw_subset",
        // codex-review P1 指摘対応（PR #437）: `engine: (custom_provider)`
        // （`kind == None`）のときに限り到達する。実際に ANN か brute-force
        // かを `EngineCore` 側から判別できない旨を明示し、`plain_scan_engine`
        // （厳密 brute-force と確定）と区別する。
        AnnPlan::UnknownCustomProvider => "unknown_custom_provider",
    }
}

/// [`PlannedQuery`]（LLM 展開結果＋解決済み実効モード）と [`ExplainEngine`]
/// （使用エンジン・ANN 静的判定、Issue #411）から `EXPLAIN` の [`QueryResult`]
/// を決定的に構築する（副作用なし。同一入力には常に同一の行を返す）。
/// 行順序: `search_terms[i]`（展開結果の件数分）→ `path_hint` → `kind_hint` →
/// `mode` → `mode_source` → `engine` → （`engine: hnsw` のときのみ）
/// `hnsw_params` → `ann_plan`。
pub(crate) fn build_explain_result(planned: &PlannedQuery, engine: &ExplainEngine) -> QueryResult {
    let expansion = planned.expansion();
    let resolved = planned.mode();

    let mut lines: Vec<String> = Vec::with_capacity(expansion.search_terms.len() + 7);
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
    lines.push(format!("engine: {}", engine_token(engine.kind)));
    if let Some(SearchEngineKind::Hnsw(params)) = engine.kind {
        // `ValidatedHnswParams::get()` は検証済み `m`／`ef_construction`／
        // `ef_search` のみを返す（構築時の静的設定値。`full_scan_ratio` や
        // 実行時の可視カーディナリティ・索引ノード数はここでは露出しない）。
        let p = params.get();
        lines.push(format!(
            "hnsw_params: m={},ef_construction={},ef_search={}",
            p.m, p.ef_construction, p.ef_search
        ));
    }
    lines.push(format!("ann_plan: {}", ann_plan_token(engine.ann_plan)));

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
    use crate::hnsw::{HnswParams, ValidatedHnswParams};
    use crate::query_planner::QueryExpansion;
    use crate::sql::mode::{ModeSource, ResolvedMode, SearchMode};

    fn cell_text(result: &QueryResult, row: usize) -> &str {
        match &result.rows[row].cells[0] {
            Cell::Text(s) => s.as_str(),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    /// 既定エンジン（`ParallelBruteForce`・`ann_plan: plain_scan_engine`）を
    /// 表す `ExplainEngine`（多くのテストで共通に使う）。
    fn default_engine() -> ExplainEngine {
        ExplainEngine {
            kind: Some(SearchEngineKind::ParallelBruteForce),
            ann_plan: AnnPlan::PlainScanEngine,
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

        let result = build_explain_result(&planned, &default_engine());

        assert_eq!(result.columns.len(), 1);
        assert_eq!(
            result.columns[0],
            ColumnMeta::Computed {
                name: QUERY_PLAN_COLUMN.to_string()
            }
        );
        // 既存 6 行（不変・後方互換）+ Issue #411 の `engine`／`ann_plan` 2 行
        // （既定エンジンでは `hnsw_params` 行は出ない）。
        assert_eq!(result.rows.len(), 8);
        assert_eq!(cell_text(&result, 0), "search_terms[0]: alpha");
        assert_eq!(cell_text(&result, 1), "search_terms[1]: beta");
        assert_eq!(cell_text(&result, 2), "path_hint: src/lib.rs");
        assert_eq!(cell_text(&result, 3), "kind_hint: fn");
        assert_eq!(cell_text(&result, 4), "mode: precision");
        assert_eq!(cell_text(&result, 5), "mode_source: query_clause");
        assert_eq!(cell_text(&result, 6), "engine: parallel_brute_force");
        assert_eq!(cell_text(&result, 7), "ann_plan: plain_scan_engine");
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

        let result = build_explain_result(&planned, &default_engine());

        // 検索語 0 件のため行は path_hint/kind_hint/mode/mode_source/engine/
        // ann_plan の 6 行。
        assert_eq!(result.rows.len(), 6);
        assert_eq!(cell_text(&result, 0), "path_hint: (none)");
        assert_eq!(cell_text(&result, 1), "kind_hint: (none)");
        assert_eq!(cell_text(&result, 2), "mode: recall");
        assert_eq!(cell_text(&result, 3), "mode_source: default");
        assert_eq!(cell_text(&result, 4), "engine: parallel_brute_force");
        assert_eq!(cell_text(&result, 5), "ann_plan: plain_scan_engine");
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
            let result = build_explain_result(&planned, &default_engine());
            // `mode_source` は末尾から 3 番目（末尾 2 行が `engine`／`ann_plan`）。
            let mode_source_row = result.rows.len() - 3;
            assert_eq!(
                cell_text(&result, mode_source_row),
                format!("mode_source: {expected_source}")
            );
        }
    }

    #[test]
    fn build_explain_result_uses_custom_provider_label_when_kind_absent() {
        let planned = PlannedQuery::new(
            QueryExpansion::default(),
            ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
        );
        // codex-review P1 指摘対応（PR #437）: `kind == None` の実運用ペアリングは
        // `AnnPlan::UnknownCustomProvider`（`core.rs` の `EXPLAIN` アームが
        // `engine_kind_unknown: self.search_engine_kind().is_none()` を渡すことで
        // 到達する）。
        let engine = ExplainEngine {
            kind: None,
            ann_plan: AnnPlan::UnknownCustomProvider,
        };

        let result = build_explain_result(&planned, &engine);

        let last = result.rows.len() - 1;
        assert_eq!(cell_text(&result, last - 1), "engine: (custom_provider)");
        assert_eq!(
            cell_text(&result, last),
            "ann_plan: unknown_custom_provider"
        );
    }

    #[test]
    fn build_explain_result_reports_hnsw_params_only_for_hnsw_engine() {
        let planned = PlannedQuery::new(
            QueryExpansion::default(),
            ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
        );
        let hnsw_params = ValidatedHnswParams::new(HnswParams::default())
            .expect("既定 HnswParams は常に検証を通過する");
        let engine = ExplainEngine {
            kind: Some(SearchEngineKind::Hnsw(hnsw_params)),
            ann_plan: AnnPlan::HnswFullVisible,
        };

        let result = build_explain_result(&planned, &engine);

        // path_hint/kind_hint/mode/mode_source/engine/hnsw_params/ann_plan の 7 行
        // （`hnsw_params` が挟まる分、既定エンジンより 1 行多い）。
        assert_eq!(result.rows.len(), 7);
        assert_eq!(cell_text(&result, 4), "engine: hnsw");
        assert_eq!(
            cell_text(&result, 5),
            "hnsw_params: m=16,ef_construction=100,ef_search=64"
        );
        assert_eq!(cell_text(&result, 6), "ann_plan: hnsw_full_visible");
    }

    #[test]
    fn build_explain_result_reports_all_five_ann_plan_tokens() {
        for (plan, expected) in [
            (AnnPlan::PlainScanEngine, "plain_scan_engine"),
            (AnnPlan::PlainScanPrecision, "plain_scan_precision"),
            (AnnPlan::HnswFullVisible, "hnsw_full_visible"),
            (AnnPlan::HnswSubset, "hnsw_subset"),
            (AnnPlan::UnknownCustomProvider, "unknown_custom_provider"),
        ] {
            let planned = PlannedQuery::new(
                QueryExpansion::default(),
                ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
            );
            let engine = ExplainEngine {
                kind: Some(SearchEngineKind::ParallelBruteForce),
                ann_plan: plan,
            };
            let result = build_explain_result(&planned, &engine);
            let last = result.rows.len() - 1;
            assert_eq!(cell_text(&result, last), format!("ann_plan: {expected}"));
        }
    }

    /// Issue #411 の要件 3（テナント存在情報に繋がる数値の非露出）を
    /// 機械的に固定する: 新規 3 行（`engine`／`hnsw_params`／`ann_plan`）の値が
    /// いずれも閉じた語彙集合の要素であり、可視カーディナリティ・行数・
    /// 索引ノード数等のデータ由来の数値を含まないことを検証する。
    #[test]
    fn build_explain_result_new_rows_use_closed_vocabulary_only() {
        const ENGINE_TOKENS: &[&str] = &[
            "cpu_scalar_brute_force",
            "parallel_brute_force",
            "hnsw",
            "(custom_provider)",
        ];
        const ANN_PLAN_TOKENS: &[&str] = &[
            "plain_scan_engine",
            "plain_scan_precision",
            "hnsw_full_visible",
            "hnsw_subset",
            "unknown_custom_provider",
        ];

        let hnsw_params = ValidatedHnswParams::new(HnswParams::default())
            .expect("既定 HnswParams は常に検証を通過する");
        for (kind, ann_plan) in [
            (
                Some(SearchEngineKind::CpuScalarBruteForce),
                AnnPlan::PlainScanEngine,
            ),
            (
                Some(SearchEngineKind::ParallelBruteForce),
                AnnPlan::PlainScanEngine,
            ),
            (
                Some(SearchEngineKind::Hnsw(hnsw_params)),
                AnnPlan::HnswFullVisible,
            ),
            (
                Some(SearchEngineKind::Hnsw(hnsw_params)),
                AnnPlan::HnswSubset,
            ),
            (
                Some(SearchEngineKind::Hnsw(hnsw_params)),
                AnnPlan::PlainScanPrecision,
            ),
            // codex-review P1 指摘対応（PR #437）: `kind == None`（`with_provider`／
            // `from_storage` 経由）の実運用ペアリングは `AnnPlan::
            // UnknownCustomProvider`（`core.rs` の `EXPLAIN` アームが
            // `engine_kind_unknown: self.search_engine_kind().is_none()` を渡す
            // ことで到達する）。
            (None, AnnPlan::UnknownCustomProvider),
        ] {
            let planned = PlannedQuery::new(
                QueryExpansion::default(),
                ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
            );
            let engine = ExplainEngine { kind, ann_plan };
            let result = build_explain_result(&planned, &engine);

            let engine_line = format!("engine: {}", engine_token(kind));
            assert!(
                ENGINE_TOKENS
                    .iter()
                    .any(|t| engine_line == format!("engine: {t}")),
                "unexpected engine token: {engine_line}"
            );
            let ann_plan_line = format!("ann_plan: {}", ann_plan_token(ann_plan));
            assert!(
                ANN_PLAN_TOKENS
                    .iter()
                    .any(|t| ann_plan_line == format!("ann_plan: {t}")),
                "unexpected ann_plan token: {ann_plan_line}"
            );
            if let Some(SearchEngineKind::Hnsw(params)) = kind {
                let p = params.get();
                let expected = format!(
                    "hnsw_params: m={},ef_construction={},ef_search={}",
                    p.m, p.ef_construction, p.ef_search
                );
                assert!(
                    result
                        .rows
                        .iter()
                        .any(|row| matches!(&row.cells[0], Cell::Text(s) if *s == expected)),
                    "hnsw_params row missing or mismatched for {result:?}"
                );
            }
        }
    }
}
