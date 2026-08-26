//! 宣言的メタデータフィルタ API（TASK-147・EXT-3。ポインタ:
//! `docs/spec/05-tasks.md` TASK-147・`docs/spec/04-behavior/extensions.md` EXT-3）。
//!
//! 責務境界: メタデータ列（`TEXT` 列）に対する**等価**と**前方一致**のフィルタを、
//! 任意の列名に対して宣言（[`DeclarativeFilter`]）・スキーマへ束縛（[`bind`]/
//! [`bind_all`]）・評価（[`MetadataFilter::matches`]/[`matches_all`]）する。
//!
//! 呼び出し文脈: `sql::allowlist::parse_where` が構文（`<col> = '<literal>'`・
//! `<col> LIKE '<prefix>%'`）を許可リスト判定し、`sql::parser::bind_in_session` が
//! 本モジュールの [`DeclarativeFilter`]・[`bind_all`] へ委譲してスキーマ照合済みの
//! [`MetadataFilter`] 列を得る。`sql::exec::execute_statement` の SCALAR 段
//! （RLS 事前フィルタを通過した可視行に対する事前適用・`HINT ORDER` で DISTANCE
//! 先行時の事後適用の両方）が [`matches_all`] を呼んで評価する（SQL-2 の等価条件の
//! 実装例を汎用化したもの）。
//!
//! `unwrap`/`expect`/添字アクセス `[]` を使わず `get()`・`strip_suffix`・`checked_*`
//! で untrusted なパターン文字列・列インデックスを扱う（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」）。

use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec::MAX_TEXT_FIELD_LEN;
use crate::sql::allowlist::SqlSurfaceError;

/// 1 文（`SELECT`）が持てるメタデータフィルタ件数の上限。無制限 `Vec` 確保を避ける
/// （`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」対応）。
/// `catalog::MAX_COLUMN_COUNT` と同値を採用する（1 列あたり複数フィルタを許すため
/// 列数と独立の定数だが、桁の妥当性は同じ方針に揃える）。
pub const MAX_METADATA_FILTERS: usize = 256;

/// フィルタの意味論。等価はバイト列一致、前方一致は `str::starts_with` による
/// バイト前方一致（`prefix` 自体が構築時点で valid `str` のため UTF-8 境界は安全）。
/// いずれも大文字小文字を区別する（PG の `=`/`LIKE` の既定動作に倣う。曖昧な照合は
/// 持ち込まない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterOp {
    Equals(String),
    StartsWith(String),
}

/// 未束縛の宣言的フィルタ（列名指定）。SQL 経由（`sql::parser::bind_in_session`）・
/// Rust API 直接呼び出しの両方から構築できる（汎用 API としての利用形。
/// `DeclarativeFilter::starts_with("path", "src/").bind(&schema)` のように使う）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarativeFilter {
    column: String,
    op: FilterOp,
}

impl DeclarativeFilter {
    /// 等価フィルタを宣言する。
    pub fn equals(column: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::Equals(value.into()),
        }
    }

    /// 前方一致フィルタを宣言する。`prefix` が空の場合は [`bind`](Self::bind) 時に
    /// `22000` で拒否する（無条件に真となる無意味なフィルタを黙って受理しない）。
    pub fn starts_with(column: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::StartsWith(prefix.into()),
        }
    }

    /// `schema` と照合して [`MetadataFilter`] へ束縛する。列名解決・`TEXT` 列限定
    /// （`VECTOR` 列は `22000`）・リテラル長上限（[`MAX_TEXT_FIELD_LEN`] 超は
    /// `54000`）・空 prefix 拒否（`22000`）を検証する。
    pub fn bind(&self, schema: &TableSchema) -> Result<MetadataFilter, SqlSurfaceError> {
        let column_index = schema
            .columns
            .iter()
            .position(|c| c.name == self.column)
            .ok_or_else(|| {
                SqlSurfaceError::invalid_input(format!("unknown column: {}", self.column))
            })?;
        let column = schema.columns.get(column_index).ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("unknown column: {}", self.column))
        })?;
        match column.ty {
            ColumnType::Text => {}
            ColumnType::Vector(_) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {:?} is not a TEXT column",
                    self.column
                )));
            }
        }
        let op = match &self.op {
            FilterOp::Equals(value) => {
                check_literal_len(value)?;
                FilterOp::Equals(value.clone())
            }
            FilterOp::StartsWith(prefix) => {
                if prefix.is_empty() {
                    return Err(SqlSurfaceError::invalid_input(
                        "LIKE prefix must not be empty",
                    ));
                }
                check_literal_len(prefix)?;
                FilterOp::StartsWith(prefix.clone())
            }
        };
        Ok(MetadataFilter { column_index, op })
    }
}

/// リテラル長がアロケーション前の上限を超えないことを検証する（`54000`）。
fn check_literal_len(value: &str) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
    if len > MAX_TEXT_FIELD_LEN {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "metadata filter literal length {len} exceeds limit {MAX_TEXT_FIELD_LEN}"
        )));
    }
    Ok(())
}

/// `pattern`（`LIKE` 句の右辺リテラル）を前方一致の prefix へ変換する。
///
/// 受理する形状は「末尾がちょうど 1 つの `%` で、それ以外に `%`・`_`・`\` を
/// 含まず、prefix が非空」のみ（PG の `LIKE` 全体は実装せず前方一致だけに限定して
/// fail-closed に倒す）。以下はすべて `22000` で拒否する:
/// - 末尾に `%` が無い（`'abc'`）
/// - prefix が空（`'%'`）
/// - 中間・先頭に `%` を含む（`'a%b%'`・`'%abc'`）
/// - `_`（1 文字ワイルドカード）を含む
/// - `\`（エスケープ）を含む
pub fn parse_prefix_pattern(pattern: &str) -> Result<String, SqlSurfaceError> {
    let Some(prefix) = pattern.strip_suffix('%') else {
        return Err(SqlSurfaceError::invalid_input(
            "LIKE pattern must end with exactly one '%' (prefix match only)",
        ));
    };
    if prefix.is_empty() {
        return Err(SqlSurfaceError::invalid_input(
            "LIKE prefix must not be empty",
        ));
    }
    if prefix.contains(['%', '_', '\\']) {
        return Err(SqlSurfaceError::invalid_input(
            "LIKE pattern supports only a trailing '%' prefix match ('%', '_', '\\\\' elsewhere are not supported)",
        ));
    }
    Ok(prefix.to_string())
}

/// 束縛済みのメタデータフィルタ 1 件（列インデックス解決済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataFilter {
    column_index: usize,
    op: FilterOp,
}

impl MetadataFilter {
    /// スキーマ上の列インデックス（[`crate::row_codec::scan_scalar_columns`] が
    /// 返す `Vec` の添字と一致する）。
    pub fn column_index(&self) -> usize {
        self.column_index
    }

    /// フィルタの意味論。
    pub fn op(&self) -> &FilterOp {
        &self.op
    }

    /// `value`（対象列の値。`None` は NULL）がこのフィルタに一致するか判定する。
    /// NULL は等価・前方一致のいずれでも常に不一致（fail-closed。PG の NULL 比較の
    /// 既定挙動に倣う）。
    pub fn matches(&self, value: Option<&str>) -> bool {
        let Some(v) = value else {
            return false;
        };
        match &self.op {
            FilterOp::Equals(expected) => v == expected,
            FilterOp::StartsWith(prefix) => v.starts_with(prefix.as_str()),
        }
    }
}

/// `filters` を `schema` へ一括束縛する。件数が [`MAX_METADATA_FILTERS`] を超える
/// 場合は `Vec` を確保する**前**に `54000` で拒否する。
pub fn bind_all(
    filters: &[DeclarativeFilter],
    schema: &TableSchema,
) -> Result<Vec<MetadataFilter>, SqlSurfaceError> {
    if filters.len() > MAX_METADATA_FILTERS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "metadata filter count {} exceeds limit {MAX_METADATA_FILTERS}",
            filters.len()
        )));
    }
    let mut bound = Vec::with_capacity(filters.len());
    for filter in filters {
        bound.push(filter.bind(schema)?);
    }
    Ok(bound)
}

/// `scanned`（`row_codec::scan_scalar_columns` が返す列値。添字は列インデックス）に
/// 対して `filters` を全件 AND 評価する。範囲外インデックスは不一致として扱う
/// （fail-closed。`scanned` は投影・フィルタが必要とする列だけを保持する構造の
/// ため、束縛時に検証済みの列インデックスでも呼び出し元の保持方針次第では
/// 範囲外になり得る）。
pub fn matches_all(filters: &[MetadataFilter], scanned: &[Option<&str>]) -> bool {
    filters.iter().all(|f| {
        let value = scanned.get(f.column_index).copied().flatten();
        f.matches(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("kind", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        )
    }

    #[test]
    fn equality_matches_and_mismatches() {
        let f = DeclarativeFilter::equals("kind", "code")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some("code")));
        assert!(!f.matches(Some("docs")));
    }

    #[test]
    fn prefix_matches_and_mismatches() {
        let f = DeclarativeFilter::starts_with("path", "src/")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some("src/lib.rs")));
        assert!(!f.matches(Some("lib.rs")));
    }

    #[test]
    fn null_never_matches() {
        let eq = DeclarativeFilter::equals("tag", "x")
            .bind(&schema())
            .unwrap();
        let pre = DeclarativeFilter::starts_with("tag", "x")
            .bind(&schema())
            .unwrap();
        assert!(!eq.matches(None));
        assert!(!pre.matches(None));
    }

    #[test]
    fn empty_prefix_is_rejected() {
        let err = DeclarativeFilter::starts_with("path", "")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_prefix_pattern_accepts_trailing_percent_only() {
        assert_eq!(parse_prefix_pattern("src/%").unwrap(), "src/");
    }

    #[test]
    fn parse_prefix_pattern_rejects_missing_trailing_percent() {
        assert_eq!(
            parse_prefix_pattern("abc").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_empty_prefix() {
        assert_eq!(parse_prefix_pattern("%").unwrap_err().wire_code(), "22000");
    }

    #[test]
    fn parse_prefix_pattern_rejects_middle_percent() {
        assert_eq!(
            parse_prefix_pattern("a%b%").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_underscore() {
        assert_eq!(
            parse_prefix_pattern("a_%").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_backslash() {
        assert_eq!(
            parse_prefix_pattern("a\\%").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_leading_percent_only_form() {
        assert_eq!(
            parse_prefix_pattern("%abc").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn bind_rejects_vector_column() {
        let err = DeclarativeFilter::equals("embedding", "x")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_unknown_column() {
        let err = DeclarativeFilter::equals("nope", "x")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_all_rejects_over_limit_count_before_allocating() {
        let filters: Vec<DeclarativeFilter> = (0..=MAX_METADATA_FILTERS)
            .map(|i| DeclarativeFilter::equals("kind", i.to_string()))
            .collect();
        let err = bind_all(&filters, &schema()).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn multibyte_prefix_is_boundary_safe() {
        let f = DeclarativeFilter::starts_with("path", "日本語/")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some("日本語/doc.md")));
        assert!(!f.matches(Some("語/doc.md")));
    }

    #[test]
    fn matches_all_out_of_range_index_is_mismatch() {
        // 束縛済みフィルタの列インデックスが `scanned` の長さを超える異常系
        // （呼び出し元の保持方針の齟齬）でも fail-closed に不一致とする。
        let f = DeclarativeFilter::equals("kind", "code")
            .bind(&schema())
            .unwrap();
        assert!(!matches_all(&[f], &[]));
    }
}
