//! `POST /v1/query` の受理語彙（閉じた 6 値）を許可リストとして表現する
//! モジュール（Issue #759・TASK-179。`update`／`delete` 追加は Issue #875・
//! NOSQL-12。対象ビヘイビア NOSQL-1・NOSQL-9・NOSQL-12。ポインタ:
//! `docs/spec/05-tasks.md` TASK-179・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-1・NOSQL-9・NOSQL-12）。
//!
//! 責務境界: [`super::schema::extract_op`] が取り出した `op` 文字列を、
//! [`Op::parse`] により **完全一致（大文字小文字・trim の読み替えなし）**
//! でこの 6 値のいずれかへ分類する。これ以外の値（DDL・UDF 呼び出し・
//! トランザクション制御・表記揺れをすべて含む）は [`UnsupportedOp`] として
//! `ErrorClass::FeatureNotSupported`（`0A000`）へ fail-closed に写像する。
//! この判定は SQL 表層の許可リスト検証（`engine::sql::allowlist`・SQL-8）
//! と同じ設計原則（許可リスト方式。「既知の未対応名」を列挙する拒否リストに
//! しない）に従う。
//!
//! `update`／`delete` は語彙へ加わり許可リストを通過し、`where`（単一行・
//! `id` 完全一致形）は Issue #876（TASK-186・NOSQL-6・NOSQL-12）で束縛・
//! 実行結線済み。`filter`（述語形）は実行器未接続（Issue #871 の担当）の
//! ため [`super::gate::handle`] の当該アーム内部で `0A000`／501 のまま
//! 拒否する（実行器なしで成功を偽装しない）。
//!
//! [`super::schema::schema_for`] はこのモジュールの表引き（[`Op::schema`]）
//! へ委譲し、`op` 名 → [`super::schema::ObjectSchema`] の対応表を単一情報源
//! として保つ。

use engine::error_format::{ClassifiedError, ErrorClass};

use super::schema::ObjectSchema;

/// `POST /v1/query` の `op` が取りうる閉じた語彙。
///
/// この 6 値以外を表す variant を追加しない（許可リストの意味が崩れる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    Scan,
    Aggregate,
    Insert,
    Update,
    Delete,
}

impl Op {
    /// 全 variant（宣言順）。[`super::schema::OP_SCHEMAS`] と名前列が
    /// 順序込みで一致することをテストで固定する。
    pub const ALL: [Op; 6] = [
        Op::Search,
        Op::Scan,
        Op::Aggregate,
        Op::Insert,
        Op::Update,
        Op::Delete,
    ];

    /// `raw` を語彙へ分類する。完全一致のみ（大文字小文字の読み替え・
    /// 前後空白のトリムを行わない）。語彙外は `None`。
    pub fn parse(raw: &str) -> Option<Op> {
        match raw {
            "search" => Some(Op::Search),
            "scan" => Some(Op::Scan),
            "aggregate" => Some(Op::Aggregate),
            "insert" => Some(Op::Insert),
            "update" => Some(Op::Update),
            "delete" => Some(Op::Delete),
            _ => None,
        }
    }

    /// JSON 上の正準名（`op` フィールドの受理値そのもの）。
    pub fn name(self) -> &'static str {
        match self {
            Op::Search => "search",
            Op::Scan => "scan",
            Op::Aggregate => "aggregate",
            Op::Insert => "insert",
            Op::Update => "update",
            Op::Delete => "delete",
        }
    }

    /// 対応する [`ObjectSchema`]（`super::schema::OP_SCHEMAS` と同一の表を
    /// 参照。表引きの単一情報源は本モジュール）。
    pub fn schema(self) -> &'static ObjectSchema {
        match self {
            Op::Search => &super::schema::SEARCH_SCHEMA,
            Op::Scan => &super::schema::SCAN_SCHEMA,
            Op::Aggregate => &super::schema::AGGREGATE_SCHEMA,
            Op::Insert => &super::schema::INSERT_SCHEMA,
            Op::Update => &super::schema::UPDATE_SCHEMA,
            Op::Delete => &super::schema::DELETE_SCHEMA,
        }
    }
}

/// 語彙外の `op` を表す分類済みエラー。
///
/// untrusted な `op` 文字列は一切保持しない（固定文言のみを返す）。
/// [`super::schema::SchemaError::UnknownKey`] が理由と同じ:
/// JSON 文字列はバックスラッシュ NUL エスケープ経由で NUL を含み得る
/// ため、untrusted 値をエラー文言へ埋め込むと `error_response::encode` の
/// NUL 拒否で `XX000` へ縮退しうる。`.claude/rules/security.md`
/// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」方針にも
/// 沿う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedOp;

/// 語彙外 `op` に返す固定文言。
pub const UNSUPPORTED_OP_MESSAGE: &str = "unsupported op";

impl ClassifiedError for UnsupportedOp {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::FeatureNotSupported
    }

    fn client_message(&self) -> String {
        UNSUPPORTED_OP_MESSAGE.to_string()
    }
}

/// `raw`（[`super::schema::extract_op`] が返した `op` 文字列）を [`Op`] へ
/// 分類する入口。語彙外は `Err(UnsupportedOp)`。
pub fn classify_op(raw: &str) -> Result<Op, UnsupportedOp> {
    Op::parse(raw).ok_or(UnsupportedOp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::query::schema::OP_SCHEMAS;

    #[test]
    fn parse_accepts_exact_six_values() {
        assert_eq!(Op::parse("search"), Some(Op::Search));
        assert_eq!(Op::parse("scan"), Some(Op::Scan));
        assert_eq!(Op::parse("aggregate"), Some(Op::Aggregate));
        assert_eq!(Op::parse("insert"), Some(Op::Insert));
        assert_eq!(Op::parse("update"), Some(Op::Update));
        assert_eq!(Op::parse("delete"), Some(Op::Delete));
    }

    #[test]
    fn parse_rejects_vocabulary_outside_six_values() {
        let negatives = [
            "",
            " search",
            "search ",
            "SEARCH",
            "Search",
            "select",
            "explain",
            "create_table",
            "alter_table",
            "drop_table",
            "call",
            "udf",
            "begin",
            "commit",
            "rollback",
            "set",
            "UPDATE",
            " update",
            "update ",
            "DELETE",
            " delete",
            "delete ",
            "upsert",
            "truncate",
            "merge",
            "patch",
        ];
        for raw in negatives {
            assert_eq!(Op::parse(raw), None, "unexpected accept for {raw:?}");
        }
    }

    #[test]
    fn classify_op_maps_to_unsupported_op_error() {
        let err = classify_op("begin").unwrap_err();
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
        assert_eq!(err.client_message(), UNSUPPORTED_OP_MESSAGE);
    }

    #[test]
    fn unsupported_op_message_is_always_the_fixed_literal() {
        for raw in ["begin", "create_table", "SEARCH", ""] {
            let err = classify_op(raw).unwrap_err();
            assert_eq!(err.client_message(), "unsupported op");
        }
    }

    #[test]
    fn all_matches_op_schemas_names_in_order() {
        let all_names: Vec<&str> = Op::ALL.iter().map(|op| op.name()).collect();
        let schema_names: Vec<&str> = OP_SCHEMAS.iter().map(|(name, _)| *name).collect();
        assert_eq!(all_names, schema_names);
    }

    #[test]
    fn schema_matches_op_schemas_table_by_pointer() {
        for (name, schema) in OP_SCHEMAS.iter() {
            let op = Op::parse(name).expect("OP_SCHEMAS name must be in vocabulary");
            assert!(std::ptr::eq(op.schema(), *schema));
        }
    }
}
