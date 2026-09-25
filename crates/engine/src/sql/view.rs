//! 非マテリアライズド `VIEW` の参照時展開（TABLE-18・SQL-23・TASK-205、
//! Issue #909）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` の広域取得
//! （`Statement::Scan`）分岐が、FROM に指定された名前を [`resolve_from`] へ
//! 通すことで「テーブルへの通常参照」と「ビュー経由の参照」を単一の経路へ
//! 統一する。ビューはカタログに保存された定義（[`crate::catalog::ViewDef`]）を
//! 同じ許可リストパーサー（`sql::allowlist::parse_view_body`）で再検証し、
//! 連鎖（ビューがビューを参照する）を内側から畳み込んで最終的に 1 つの
//! 基底テーブル名＋合成済み `WHERE` 述語へ書き換える。第 2 の SQL パーサー・
//! 実行器は作らない（畳み込み後は既存の [`crate::sql::allowlist::ValidatedScan`]
//! と完全に同じ形になり、束縛・実行・RLS 適用はすべて既存経路をそのまま通る）。
//!
//! RLS-10 (b) の不変条件: ビュー定義は `tenant_id`／`PolicyContext`／作成者の
//! 情報を一切保持しない。畳み込み後の文は、参照した**セッション自身**の
//! `PolicyContext` を使う既存の実行経路でしか評価されないため、作成者の
//! 可視性が参照者へ引き継がれることは構造的に起こらない。

use super::allowlist::{parse_view_body, Projection, SqlSurfaceError, TableLookup, WherePredicate};
use crate::catalog::{ViewDef, MAX_VIEW_NESTING_DEPTH};

/// FROM に指定された名前の解決結果。
pub(crate) enum Resolved {
    /// 通常のテーブル参照。
    Table,
    /// ビュー参照（連鎖を内側から畳み込んだ結果）。
    View {
        /// 連鎖を辿った先の実テーブル名。
        base_table: String,
        /// ビュー定義由来の `WHERE` 述語（内側のビューが先頭。クエリ自身の
        /// 述語はこの後ろに追加する）。
        view_predicates: Vec<WherePredicate>,
        /// クエリ直接の参照先（最も外側のビュー）が公開する列集合。
        /// `None` は `SELECT *`（列を絞り込まない）を意味する。
        view_columns: Option<Vec<String>>,
    },
}

/// カタログ破損時の連鎖走査を打ち切る安全上限。正常経路では循環を構造的に
/// 構築できない（`CREATE VIEW` が参照先の存在を作成前に要求するため）が、
/// 破損したカタログ値に対しても無限ループにならないことを保証する。
const MAX_VIEW_RESOLVE_STEPS: usize = 10_000;

/// FROM に指定された `name` を解決する（[`crate::sql::allowlist::validate_sql_tokens`]
/// の `Statement::Scan` 分岐から呼ばれる）。テーブル・ビューのいずれにも
/// 存在しない場合は `Err(SqlSurfaceError::UndefinedTable)`。カタログに保存された
/// ビュー本文の再検証・デコードに失敗した場合（カタログ破損）は
/// `Err(SqlSurfaceError::Internal)`（固定文言。body のリテラル値を含めない。
/// security.md P0）。
pub(crate) fn resolve_from(
    lookup: &impl TableLookup,
    name: &str,
) -> Result<Resolved, SqlSurfaceError> {
    let mut current = name.to_string();
    let mut acc_predicates: Vec<WherePredicate> = Vec::new();
    let mut acc_columns: Option<Vec<String>> = None;
    let mut is_view = false;
    let mut depth: u32 = 0;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();

    for _ in 0..MAX_VIEW_RESOLVE_STEPS {
        if !visited.insert(current.clone()) {
            return Err(corrupt_view_error());
        }
        match lookup.view_definition(&current)? {
            Some(ViewDef {
                base_relation,
                body_sql,
            }) => {
                let parsed = reparse_stored_body(&body_sql)?;
                depth = depth.checked_add(1).ok_or_else(corrupt_view_error_fn)?;
                if depth > MAX_VIEW_NESTING_DEPTH {
                    return Err(SqlSurfaceError::payload_too_large(
                        "view nesting depth exceeds limit",
                    ));
                }
                if !is_view {
                    // 最も外側（クエリが直接参照した）のビューだけが、クエリへ
                    // 公開する列集合を決める。
                    acc_columns = match &parsed.projection {
                        Projection::All => None,
                        Projection::Columns(cols) => Some(cols.clone()),
                        Projection::Items(_) => None,
                    };
                }
                is_view = true;
                // 内側（より深い）のビューの述語を先頭に置く（§設計「述語合成」）。
                let mut merged = parsed.where_predicates;
                merged.extend(acc_predicates);
                acc_predicates = merged;
                current = base_relation;
            }
            None => {
                if !lookup.table_exists(&current)? {
                    return Err(SqlSurfaceError::undefined_table(name));
                }
                return if is_view {
                    Ok(Resolved::View {
                        base_table: current,
                        view_predicates: acc_predicates,
                        view_columns: acc_columns,
                    })
                } else {
                    Ok(Resolved::Table)
                };
            }
        }
    }
    Err(corrupt_view_error())
}

/// 格納済み `body_sql`（`sql::allowlist::render_view_body` の出力）を再トークン化・
/// 再パースする。第 2 の SQL パーサーを作らず、`CREATE VIEW` 時と完全に同じ
/// [`parse_view_body`] を経由する。失敗（カタログ破損・非互換な将来フォーマット
/// 変更）は `body_sql` の内容をエラーへ含めず、固定文言の
/// `SqlSurfaceError::Internal` へ丸める（security.md P0）。
fn reparse_stored_body(
    body_sql: &str,
) -> Result<super::allowlist::ParsedViewBody, SqlSurfaceError> {
    let tokens = super::lexer::tokenize(body_sql).map_err(|_| corrupt_view_error())?;
    parse_view_body(&tokens).map_err(|_| corrupt_view_error())
}

fn corrupt_view_error() -> SqlSurfaceError {
    // body のリテラル値・詳細な破損理由をクライアントへ運ばない固定文言
    // （security.md P0「エラー・ログ経由で他テナントのデータ・存在情報を
    // 漏らさない」対応）。
    SqlSurfaceError::Internal {
        detail: "invalid view definition".to_string(),
    }
}

fn corrupt_view_error_fn() -> SqlSurfaceError {
    corrupt_view_error()
}

/// クエリ側の投影・`WHERE` が、ビューが公開する列集合（`view_columns`）に
/// 収まっているかを検査する（`view_columns` が `None`〔ビュー自身が `*`〕の
/// 場合は検査不要——基底テーブルの束縛段がそのまま列存在を検査する）。
/// 範囲外の列参照は `SqlSurfaceError::InvalidInput`（`22000`。既存の
/// 「未知の列」束縛エラーと同じ分類）。
pub(crate) fn check_columns_within_view(
    view_columns: Option<&[String]>,
    projection: &Projection,
    where_predicates: &[WherePredicate],
) -> Result<(), SqlSurfaceError> {
    let Some(columns) = view_columns else {
        return Ok(());
    };
    match projection {
        Projection::All | Projection::Items(_) => {}
        Projection::Columns(cols) => {
            for c in cols {
                if !columns.iter().any(|vc| vc == c) {
                    return Err(SqlSurfaceError::InvalidInput {
                        detail: format!("unknown column: {c}"),
                    });
                }
            }
        }
    }
    for pred in where_predicates {
        if let Some(c) = predicate_column(pred) {
            if !columns.iter().any(|vc| vc == c) {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: format!("unknown column: {c}"),
                });
            }
        }
    }
    Ok(())
}

/// 単純な `column` フィールドを持つ述語からその列名を取り出す。`PredicateCall`・
/// `Expression`（UDF・式ベース）はビュー越しの列スコープ検査の対象外
/// （§対象外「複雑な述語のビュー越しスコープ検査」。RLS 境界には影響しない——
/// 基底テーブルへ書き換え済みの述語は RLS 判定を経由する既存経路でそのまま
/// 評価されるため、ここでの検査は「ビューが宣言した列だけに絞る」という
/// 利便性のためのものであり、安全性の境界ではない）。
fn predicate_column(pred: &WherePredicate) -> Option<&str> {
    match pred {
        WherePredicate::Equality { column, .. } => Some(column),
        WherePredicate::Prefix { column, .. } => Some(column),
        WherePredicate::BoolEquality { column, .. } => Some(column),
        WherePredicate::BoolColumn { column } => Some(column),
        WherePredicate::Compare { column, .. } => Some(column),
        WherePredicate::PredicateCall { .. } | WherePredicate::Expression(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::allowlist::render_view_body;
    use super::super::lexer;
    use super::*;

    /// render_view_body → 再トークン化 → parse_view_body が元と等価な AST を
    /// 復元することを固定する（TABLE-18・SQL-23・TASK-205、Issue #909 の
    /// round-trip 契約）。
    #[test]
    fn render_body_round_trips_through_reparse() {
        let cases = [
            "SELECT * FROM docs",
            "SELECT id, body FROM docs WHERE lang = 'ja'",
            "SELECT id FROM docs WHERE body LIKE 'foo%' AND flag",
            "SELECT id FROM docs WHERE score > '1' AND active = true",
            "SELECT id FROM docs WHERE note = 'it''s ok'",
        ];
        for sql in cases {
            let tokens = lexer::tokenize(sql).expect("tokenize");
            let parsed = parse_view_body(&tokens).expect("parse");
            let rendered = render_view_body(&parsed);
            let retokens = lexer::tokenize(&rendered).expect("retokenize");
            let reparsed = parse_view_body(&retokens).expect("reparse");
            assert_eq!(parsed, reparsed, "sql={sql} rendered={rendered}");
        }
    }
}
