//! 宣言的メタデータフィルタ API（TASK-147・EXT-3。ポインタ:
//! `docs/spec/05-tasks.md` TASK-147・`docs/spec/04-behavior/extensions.md` EXT-3）。
//!
//! 責務境界: メタデータ列（`TEXT` 列）に対する**等価**と**前方一致**のフィルタを、
//! 任意の列名に対して宣言（[`DeclarativeFilter`]）・スキーマへ束縛（[`bind`]/
//! [`bind_all`]）・評価（[`MetadataFilter::matches`]/[`matches_all`]）する。
//!
//! 呼び出し文脈: `sql::allowlist::parse_where` が構文（`<col> = '<literal>'`・
//! `<col> LIKE '<prefix>%'`・`<col> (< | > | <= | >=) '<literal>'`）を
//! 許可リスト判定し、`sql::parser::bind_where_predicates` が本モジュールの
//! [`DeclarativeFilter`]・[`bind_all`] へ委譲してスキーマ照合済みの
//! [`MetadataFilter`] 列を得る。`sql::exec::execute_statement` の SCALAR 段
//! （RLS 事前フィルタを通過した可視行に対する事前適用・`HINT ORDER` で DISTANCE
//! 先行時の事後適用の両方）が [`matches_all`] を呼んで評価する（SQL-2 の等価条件の
//! 実装例を汎用化したもの）。
//!
//! `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較
//! （[`FilterOp::TypedCompare`]。TABLE-13・TASK-199、Issue #891）は、算術を
//! 持たない非数値型のみを対象にした宣言的経路（レーン B）として本モジュールへ
//! 追加した。INTEGER/BIGINT/REAL/DOUBLE 列を算術式・関数引数の中で使う経路
//! （レーン A）は式評価系（`sql::udf_call`・`sql::expr_program`）が担い、
//! 本モジュールの対象外のまま。
//!
//! `unwrap`/`expect`/添字アクセス `[]` を使わず `get()`・`strip_suffix`・`checked_*`
//! で untrusted なパターン文字列・列インデックスを扱う（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」）。

use crate::catalog::{ColumnType, TableSchema};
use crate::numeric::Decimal;
use crate::row_codec::{ScalarRef, MAX_TEXT_FIELD_LEN};
use crate::sql::allowlist::SqlSurfaceError;
use crate::uuid::Uuid;

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
    /// BOOLEAN 列の等価条件（TABLE-13・TASK-196、Issue #883・D-c）。`Equals`/
    /// `StartsWith` は TEXT 列限定のまま据え置き、文字列比較（`flag = 'true'`）は
    /// 受理しない（fail-closed。`bind` が列型で振り分ける）。
    BoolEquals(bool),
    /// `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較条件
    /// （TABLE-13・TASK-199、Issue #891・レーン B）。`value` は列型で解析済みの
    /// 型付きリテラルで、`bind` 時に一度だけ解析し評価時（[`MetadataFilter::matches`]）
    /// は解析し直さない。
    TypedCompare {
        op: CompareOp,
        value: TypedLiteral,
    },
    /// [`FilterOp::TypedCompare`] の未束縛（列型未確定）版。`sql::parser::
    /// bind_where_predicates` が `WherePredicate::Equality`（列型が
    /// Date/Timestamp/Numeric/Uuid/Bytea の場合。[`CompareOp::Eq`]）・
    /// `WherePredicate::Compare`（`< > <= >=`）の両方をここへ振り分ける。
    /// `bind_impl` が列型で [`CompareLiteral`] を解析し `TypedCompare` へ
    /// 確定させる。
    Compare {
        op: CompareOp,
        literal: CompareLiteral,
    },
}

/// [`FilterOp::TypedCompare`]／[`FilterOp::Compare`] の比較演算子。
/// `sql::allowlist::CompareOp`（構文層。`< > <= >=` の字句表現のみ）とは別に
/// 本モジュール（意味層）で持つ。`Eq` は構文層に対応する variant を持たず、
/// `WherePredicate::Equality`（列型が非 TEXT/ENUM の場合）からのみ到達する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    /// `ordering`（値どうしの比較結果）がこの演算子を満たすかを判定する。
    fn accepts(self, ordering: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            CompareOp::Eq => ordering == Equal,
            CompareOp::Lt => ordering == Less,
            CompareOp::Le => ordering != Greater,
            CompareOp::Gt => ordering == Greater,
            CompareOp::Ge => ordering != Less,
        }
    }
}

/// [`FilterOp::Compare`] が保持する未解析リテラル。`Text` は文字列リテラル形
/// （`col > '...'`。SQL 表層の `sql::parser::bind_where_predicates` が
/// `WherePredicate::Compare`／`Equality` から構築する）。`Number` は
/// `NUMERIC` 列専用の裸数値リテラル形（`col > 1.5`）を表す
/// [`DeclarativeFilter::compare_numeric_literal`] 専用の variant で、
/// **Rust API 直接呼び出し限定**（TABLE-13・TASK-199、Issue #891・レーン B は
/// 文字列リテラル形のみを対象とするため、SQL 表層からは未結線。裸数値
/// リテラル形の SQL 構文追加はレーン A・別 Issue の対象）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareLiteral {
    Text(String),
    Number(String),
}

/// `sql::allowlist::CompareOp`（構文層）から本モジュールの意味表現へ写像する。
impl From<crate::sql::allowlist::CompareOp> for CompareOp {
    fn from(op: crate::sql::allowlist::CompareOp) -> Self {
        match op {
            crate::sql::allowlist::CompareOp::Lt => CompareOp::Lt,
            crate::sql::allowlist::CompareOp::Le => CompareOp::Le,
            crate::sql::allowlist::CompareOp::Gt => CompareOp::Gt,
            crate::sql::allowlist::CompareOp::Ge => CompareOp::Ge,
        }
    }
}

/// [`FilterOp::TypedCompare`] が保持する、列型で解析済みの範囲比較リテラル
/// （TABLE-13・TASK-199、Issue #891）。`bind` 時に一度だけ構築し、行ごとの
/// 評価（[`MetadataFilter::matches`]）では文字列解析をやり直さない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedLiteral {
    /// `DATE` 列。1970-01-01 起点の日数。
    Date(i32),
    /// `TIMESTAMP` 列。1970-01-01 00:00:00 起点のマイクロ秒。
    Timestamp(i64),
    /// `NUMERIC` 列。列の `scale` に丸めず、リテラル自身の小数桁数で解析した
    /// 正確な値（`numeric::cmp_exact` で比較する）。
    Numeric(Decimal),
    /// `UUID` 列。
    Uuid(Uuid),
    /// `BYTEA` 列。辞書順（バイト列の `Ord`）で比較する。
    Bytes(Vec<u8>),
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

    /// BOOLEAN 列の等価フィルタを宣言する（Issue #883・D-c）。
    pub fn bool_equals(column: impl Into<String>, value: bool) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::BoolEquals(value),
        }
    }

    /// `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較フィルタを
    /// 文字列リテラル形（`col > '2024-01-01'` 等）で宣言する（TABLE-13・
    /// TASK-199、Issue #891）。列型に応じた解析は [`Self::bind`] 時に行う。
    pub fn compare(column: impl Into<String>, op: CompareOp, value: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::Compare {
                op,
                literal: CompareLiteral::Text(value.into()),
            },
        }
    }

    /// `NUMERIC` 列専用: 裸の数値リテラル形（`col > 1.5`）の範囲比較フィルタを
    /// 宣言する（TABLE-13・TASK-199、Issue #891）。`raw` は引用符なしの数値
    /// テキストで、列の `scale` に丸めずリテラル自身の小数桁数で解析する
    /// （[`crate::numeric::parse_literal_exact`]）。
    pub fn compare_numeric_literal(
        column: impl Into<String>,
        op: CompareOp,
        raw: impl Into<String>,
    ) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::Compare {
                op,
                literal: CompareLiteral::Number(raw.into()),
            },
        }
    }

    /// `schema` と照合して [`MetadataFilter`] へ束縛する。列名解決・列型検査
    /// （`Equals`/`StartsWith` は `TEXT` 列限定・`BoolEquals` は `BOOLEAN` 列限定。
    /// いずれも不一致は `22000`）・リテラル長上限（[`MAX_TEXT_FIELD_LEN`] 超は
    /// `54000`）・空 prefix 拒否（`22000`）を検証する。
    pub fn bind(&self, schema: &TableSchema) -> Result<MetadataFilter, SqlSurfaceError> {
        self.bind_impl(schema, false)
    }

    /// [`Self::bind`] の内部実装。`skip_enum_label_validation` が `true` の
    /// ときに限り、ENUM 列の等価フィルタで語彙照合（`EnumTypeDef::
    /// validate_label`）を省略する。Prepared Describe（`$n` 由来のダミー値。
    /// PR #1012・Cursor Bugbot 指摘対応）専用の縮退経路であり、公開 API
    /// [`Self::bind`]／[`bind_all`] は常に `false`（従来どおり全値検証）を渡す
    /// （`sql::parser::bind_where_predicates` の `dummy_equality_flags` 経由の
    /// み `true` になりうる。詳細は同関数のドキュメント参照）。列型検査・
    /// リテラル長上限・空 prefix 拒否など値に依存しない構造検証は
    /// `skip_enum_label_validation` の値に関係なく常に行う。
    fn bind_impl(
        &self,
        schema: &TableSchema,
        skip_enum_label_validation: bool,
    ) -> Result<MetadataFilter, SqlSurfaceError> {
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
        let op = match &self.op {
            FilterOp::Equals(value) => {
                // ENUM 列は TEXT と同じ等価述語を受理する（Issue #890 D7。
                // PostgreSQL の enum 入力と同様、語彙外のラベルは書き込み時と
                // 同じ `22P02` で拒否する。二次索引〔`sql::scalar_index`〕は
                // TEXT と同じ辞書を共有するため、この等価意味論のまま
                // 索引経由の候補削減を信頼できる）。
                match &column.ty {
                    ColumnType::Text => {}
                    ColumnType::Enum(def) => {
                        // `skip_enum_label_validation` が `true` の場合、この値は
                        // `sql::params::substitute_dummy` が生成した固定ダミー
                        // 文字列であり、実際にどのラベルが束縛されるかは Bind
                        // まで未確定（PR #1012 Cursor Bugbot 指摘: ここで通常どおり
                        // 語彙照合すると、`WHERE enum_col = $n` を含む文の Describe
                        // が実リテラルの有無に関わらず常に `22P02` になってしまう）。
                        if !skip_enum_label_validation && def.validate_label(value).is_err() {
                            return Err(SqlSurfaceError::invalid_text_representation(format!(
                                "column {:?} (enum {:?}) does not accept label {value:?}",
                                self.column,
                                def.name()
                            )));
                        }
                    }
                    // F10（Issue #882 計画）: REAL/DOUBLE 列は VECTOR 列と同じ
                    // 「TEXT 列でない」拒否腕へ合流させる（対応は #891 へ申し送り）。
                    ColumnType::Vector(_)
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Real
                    | ColumnType::Double
                    | ColumnType::Boolean
                    | ColumnType::Date
                    | ColumnType::Timestamp
                    | ColumnType::Array(_)
                    | ColumnType::Bytea
                    | ColumnType::Json
                    | ColumnType::Jsonb
                    | ColumnType::Numeric { .. }
                    | ColumnType::Uuid => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {:?} is not a TEXT column",
                            self.column
                        )));
                    }
                }
                check_literal_len(value)?;
                FilterOp::Equals(value.clone())
            }
            FilterOp::StartsWith(prefix) => {
                if !matches!(column.ty, ColumnType::Text) {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "column {:?} is not a TEXT column",
                        self.column
                    )));
                }
                if prefix.is_empty() {
                    return Err(SqlSurfaceError::invalid_input(
                        "LIKE prefix must not be empty",
                    ));
                }
                check_literal_len(prefix)?;
                FilterOp::StartsWith(prefix.clone())
            }
            FilterOp::BoolEquals(value) => {
                if !matches!(column.ty, ColumnType::Boolean) {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "column {:?} is not a BOOLEAN column",
                        self.column
                    )));
                }
                FilterOp::BoolEquals(*value)
            }
            FilterOp::Compare { op, literal } => {
                // `skip_enum_label_validation` を「型付きリテラル解析の
                // スキップ」へ一般化する（Issue #891。ENUM の語彙照合スキップ
                // と同じ理由: `sql::params::substitute_dummy` が生成する固定
                // ダミー文字列 `"0"` は DATE／TIMESTAMP／UUID／BYTEA の文法として
                // 不正なため、Describe 時点でこれを実際に解析すると
                // `WHERE date_col = $1` 等の Describe が常に失敗してしまう。
                // 実際の値検証は Bind／Execute で行われる（他の列型と同じ
                // 縮退方針）。プレースホルダ値は評価（`matches`）に到達しない
                // 契約（Describe は検索本体を実行しない）。
                let value = if skip_enum_label_validation {
                    match &column.ty {
                        ColumnType::Date => TypedLiteral::Date(0),
                        ColumnType::Timestamp => TypedLiteral::Timestamp(0),
                        ColumnType::Numeric { .. } => TypedLiteral::Numeric(
                            crate::numeric::Decimal::from_parts(0, 0).map_err(|_| {
                                // `scale=0` は `MAX_PRECISION` 以下のため理論上
                                // 到達しないが、engine ライブラリコードは panic
                                // させない契約（`.claude/rules/coding-rust.md`）
                                // のため fail-closed に `Result` で伝播する。
                                SqlSurfaceError::Internal {
                                    detail: "Decimal::from_parts(0, 0) must always succeed"
                                        .to_string(),
                                }
                            })?,
                        ),
                        ColumnType::Uuid => {
                            TypedLiteral::Uuid(crate::uuid::Uuid::from_bytes([0u8; 16]))
                        }
                        ColumnType::Bytea => TypedLiteral::Bytes(Vec::new()),
                        _ => return Err(unsupported_compare_column(&self.column)),
                    }
                } else {
                    bind_typed_compare_literal(&self.column, &column.ty, literal)?
                };
                FilterOp::TypedCompare { op: *op, value }
            }
            FilterOp::TypedCompare { .. } => {
                // `DeclarativeFilter` の公開コンストラクタ（`compare`／
                // `compare_numeric_literal`）はいずれも未束縛の `Compare` を
                // 生成し、`TypedCompare` を直接構築する経路は無い
                // （fail-closed の保険腕。`bind`/`bind_all` は常に未束縛の値を
                // 受け取る契約）。
                return Err(SqlSurfaceError::Internal {
                    detail: "DeclarativeFilter must not be constructed with an already-typed compare value".to_string(),
                });
            }
        };
        Ok(MetadataFilter { column_index, op })
    }
}

/// 範囲比較（[`FilterOp::Compare`]）を受理しない列型へ束縛しようとした場合の
/// エラー（`22000`）。
fn unsupported_compare_column(column: &str) -> SqlSurfaceError {
    SqlSurfaceError::invalid_input(format!(
        "column {column:?} does not support range comparison (expected DATE/TIMESTAMP/NUMERIC/UUID/BYTEA)"
    ))
}

/// [`FilterOp::Compare`] の未解析リテラルを列型 `ty` へ束縛し、
/// [`TypedLiteral`] を構築する（TABLE-13・TASK-199、Issue #891）。INSERT／
/// UPDATE／UPSERT と同じ `sql::parser::bind_*_literal` 群を共有し、第 2の
/// パーサーを作らない。
fn bind_typed_compare_literal(
    column: &str,
    ty: &ColumnType,
    literal: &CompareLiteral,
) -> Result<TypedLiteral, SqlSurfaceError> {
    match (ty, literal) {
        (ColumnType::Date, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            match crate::sql::parser::bind_datetime_literal(column, ColumnType::Date, s)? {
                crate::row_codec::Value::Date(d) => Ok(TypedLiteral::Date(d)),
                _ => Err(SqlSurfaceError::Internal {
                    detail: "bind_datetime_literal returned a non-Date value for a DATE column"
                        .to_string(),
                }),
            }
        }
        (ColumnType::Timestamp, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            match crate::sql::parser::bind_datetime_literal(column, ColumnType::Timestamp, s)? {
                crate::row_codec::Value::Timestamp(t) => Ok(TypedLiteral::Timestamp(t)),
                _ => Err(SqlSurfaceError::Internal {
                    detail:
                        "bind_datetime_literal returned a non-Timestamp value for a TIMESTAMP column"
                            .to_string(),
                }),
            }
        }
        (ColumnType::Uuid, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            match crate::sql::parser::bind_uuid_literal(s, column)? {
                crate::row_codec::Value::Uuid(u) => Ok(TypedLiteral::Uuid(u)),
                _ => Err(SqlSurfaceError::Internal {
                    detail: "bind_uuid_literal returned a non-Uuid value for a UUID column"
                        .to_string(),
                }),
            }
        }
        (ColumnType::Bytea, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            match crate::sql::parser::bind_bytea_literal(s, column)? {
                crate::row_codec::Value::Bytes(b) => Ok(TypedLiteral::Bytes(b)),
                _ => Err(SqlSurfaceError::Internal {
                    detail: "bind_bytea_literal returned a non-Bytes value for a BYTEA column"
                        .to_string(),
                }),
            }
        }
        (ColumnType::Numeric { .. }, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            crate::numeric::parse_literal_exact(s)
                .map(TypedLiteral::Numeric)
                .map_err(|e| numeric_literal_error(column, e))
        }
        (ColumnType::Numeric { .. }, CompareLiteral::Number(raw)) => {
            check_literal_len(raw)?;
            crate::numeric::parse_literal_exact(raw)
                .map(TypedLiteral::Numeric)
                .map_err(|e| numeric_literal_error(column, e))
        }
        // `Number`（裸の数値リテラル）形は NUMERIC 列専用（`sql::parser::
        // bind_where_predicates` が振り分ける）。Date/Timestamp/Uuid/Bytea へ
        // 数値リテラルで比較しようとした場合、および Text/Vector/Integer/
        // BigInt/Real/Double/Boolean/Array/Json/Jsonb/Enum 列（算術のみ・
        // 等価/前方一致のみが受理形）への範囲比較はいずれも `22000`。
        _ => Err(unsupported_compare_column(column)),
    }
}

/// [`crate::numeric::NumericError`] を `wire_code` へ写像する（`sql::parser::
/// bind_numeric_literal` と同じ分類。エラーメッセージには列名のみを含める）。
fn numeric_literal_error(column: &str, e: crate::numeric::NumericError) -> SqlSurfaceError {
    match e {
        crate::numeric::NumericError::Malformed(detail) => {
            SqlSurfaceError::invalid_input(format!("column {column:?}: {detail}"))
        }
        crate::numeric::NumericError::OutOfRange => SqlSurfaceError::numeric_out_of_range(format!(
            "column {column:?} numeric comparison literal out of range"
        )),
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
    /// NULL は等価・前方一致・BOOLEAN 等価のいずれでも常に不一致（fail-closed。
    /// PG の三値論理での NULL 比較の既定挙動に倣う）。型不一致（`TEXT` フィルタに
    /// `Bool` 値、`BoolEquals` に `Text` 値）も `bind` が列型で事前に排除している
    /// 契約だが、念のため不一致として扱う。
    pub fn matches(&self, value: Option<ScalarRef<'_>>) -> bool {
        let Some(v) = value else {
            return false;
        };
        match &self.op {
            // `as_dictionary_text` で TEXT／ENUM の両方を等価比較する
            // （Issue #890 D7。二次索引〔`sql::scalar_index`〕と同じ辞書表現）。
            FilterOp::Equals(expected) => v.as_dictionary_text() == Some(expected.as_str()),
            FilterOp::StartsWith(prefix) => v
                .as_text()
                .map(|s| s.starts_with(prefix.as_str()))
                .unwrap_or(false),
            FilterOp::BoolEquals(expected) => v.as_bool() == Some(*expected),
            FilterOp::TypedCompare { op, value } => match (value, v) {
                (TypedLiteral::Date(expected), v) => v
                    .as_date()
                    .map(|actual| op.accepts(actual.cmp(expected)))
                    .unwrap_or(false),
                (TypedLiteral::Timestamp(expected), v) => v
                    .as_timestamp()
                    .map(|actual| op.accepts(actual.cmp(expected)))
                    .unwrap_or(false),
                (TypedLiteral::Numeric(expected), v) => v
                    .as_numeric()
                    .map(|actual| op.accepts(crate::numeric::cmp_exact(&actual, expected)))
                    .unwrap_or(false),
                (TypedLiteral::Uuid(expected), v) => v
                    .as_uuid()
                    .map(|actual| op.accepts(actual.cmp(expected)))
                    .unwrap_or(false),
                (TypedLiteral::Bytes(expected), v) => v
                    .as_bytes()
                    .map(|actual| op.accepts(actual.cmp(expected.as_slice())))
                    .unwrap_or(false),
            },
            // `bind`/`bind_all` は常に `TypedCompare` へ確定させた
            // `MetadataFilter` のみを生成する（`bind_impl` の
            // `FilterOp::TypedCompare` 保険腕参照）。未束縛の `Compare` が
            // 評価に到達することはない（fail-closed）。
            FilterOp::Compare { .. } => false,
        }
    }
}

/// `count` 件のフィルタが [`MAX_METADATA_FILTERS`] を超えないことを検証する
/// （`54000`）。[`bind_all`] の件数検査本体を切り出したもので、`Vec` 確保・
/// 要素の複製より**前**に呼べる形にする（`.claude/rules/security.md`
/// 「不安全な設計｜無制限リソース確保（DoS）」対応）。
///
/// `pub`: `wire-server::http::query::filter`（NoSQL 表層の `filter` 配列。
/// Issue #761・TASK-175・NOSQL-7）が、JSON 配列要素を [`DeclarativeFilter`]
/// へ写像する**前**（`String` 複製・`Vec` 確保より前）に同じ上限を検査する
/// ために呼ぶ。`bind_all` と別々に上限を持たない単一情報源。
pub fn check_filter_count(count: usize) -> Result<(), SqlSurfaceError> {
    if count > MAX_METADATA_FILTERS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "metadata filter count {count} exceeds limit {MAX_METADATA_FILTERS}"
        )));
    }
    Ok(())
}

/// `filters` を `schema` へ一括束縛する。件数が [`MAX_METADATA_FILTERS`] を超える
/// 場合は `Vec` を確保する**前**に `54000` で拒否する。
pub fn bind_all(
    filters: &[DeclarativeFilter],
    schema: &TableSchema,
) -> Result<Vec<MetadataFilter>, SqlSurfaceError> {
    check_filter_count(filters.len())?;
    let mut bound = Vec::with_capacity(filters.len());
    for filter in filters {
        bound.push(filter.bind(schema)?);
    }
    Ok(bound)
}

/// [`bind_all`] の Prepared Describe 専用版（PR #1012 Cursor Bugbot 指摘対応。
/// Issue #935・WIRE-12・TASK-217）。`filters[i]` を束縛する際、
/// `skip_enum_label_validation[i]`（範囲外は `false` 扱い）が `true` の場合に
/// 限り ENUM 列の等価フィルタの語彙照合を省略する。呼び出し元
/// （`sql::parser::bind_where_predicates`）は、`sql::params::
/// order_by_distance_literal_is_param` と同じ設計で「そのフィルタの値が
/// `$n` に由来する固定ダミーかどうか」を並べたスライスを渡す。`filters` と
/// `skip_enum_label_validation` の対応は呼び出し元が構築順を揃えて保証する
/// 契約（本関数自身は対応関係を検証しない）。
pub(crate) fn bind_all_for_describe(
    filters: &[DeclarativeFilter],
    schema: &TableSchema,
    skip_enum_label_validation: &[bool],
) -> Result<Vec<MetadataFilter>, SqlSurfaceError> {
    check_filter_count(filters.len())?;
    let mut bound = Vec::with_capacity(filters.len());
    for (index, filter) in filters.iter().enumerate() {
        let skip = skip_enum_label_validation
            .get(index)
            .copied()
            .unwrap_or(false);
        bound.push(filter.bind_impl(schema, skip)?);
    }
    Ok(bound)
}

/// `scanned`（`row_codec::scan_scalar_columns` が返す列値。添字は列インデックス）に
/// 対して `filters` を全件 AND 評価する。範囲外インデックスは不一致として扱う
/// （fail-closed。`scanned` は投影・フィルタが必要とする列だけを保持する構造の
/// ため、束縛時に検証済みの列インデックスでも呼び出し元の保持方針次第では
/// 範囲外になり得る）。
pub fn matches_all(filters: &[MetadataFilter], scanned: &[Option<ScalarRef<'_>>]) -> bool {
    filters.iter().all(|f| {
        // 型不一致（`TEXT` フィルタに `Bool`／`Real`／`Double` 値、`BoolEquals` に
        // `Text` 値等）は `bind` が列型で事前に排除している契約だが、
        // `MetadataFilter::matches` 側で防御的に不一致（fail-closed）へ落とす
        // （F10: TEXT 系フィルタに対する REAL/DOUBLE も同様に「値なし」と同じ
        // 扱いになる）。
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

    /// TABLE-13・TASK-199、Issue #891: レーン B（範囲比較）が対象とする
    /// 5 型を 1 列ずつ持つスキーマ。
    fn typed_compare_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("day", ColumnType::Date, true),
                ColumnDef::new("at", ColumnType::Timestamp, true),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("ext_id", ColumnType::Uuid, true),
                ColumnDef::new("blob", ColumnType::Bytea, true),
            ],
        )
    }

    #[test]
    fn equality_matches_and_mismatches() {
        let f = DeclarativeFilter::equals("kind", "code")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some(ScalarRef::Text("code"))));
        assert!(!f.matches(Some(ScalarRef::Text("docs"))));
    }

    #[test]
    fn prefix_matches_and_mismatches() {
        let f = DeclarativeFilter::starts_with("path", "src/")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some(ScalarRef::Text("src/lib.rs"))));
        assert!(!f.matches(Some(ScalarRef::Text("lib.rs"))));
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

    // PR #1012 Cursor Bugbot 指摘の回帰: `bind_all_for_describe` は
    // `skip_enum_label_validation[i]` が `true` の位置に限り ENUM 列の等価
    // フィルタの語彙照合を省略し、それ以外（範囲外含む）は従来どおり
    // `bind`（`bind_impl(.., false)`）と同一の検証を行う。
    #[test]
    fn bind_all_for_describe_skips_enum_validation_only_at_flagged_positions() {
        use crate::storage::Storage;
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("declarative-filter-bind-all-for-describe");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let mood = storage
            .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
            .expect("create enum type");
        let schema_with_enum = TableSchema::new(
            "docs",
            vec![ColumnDef::new("mood", ColumnType::Enum(mood), true)],
        );

        // flags[0] = true（ダミー値扱い）: 語彙外ラベルでも束縛が成功する。
        let filters = [DeclarativeFilter::equals("mood", "not-a-real-mood")];
        let bound = bind_all_for_describe(&filters, &schema_with_enum, &[true])
            .expect("skip_enum_label_validation=true must accept an out-of-vocabulary label");
        assert_eq!(bound.len(), 1);

        // flags[0] = false（実値扱い）: 従来どおり `22P02` で拒否される。
        let err = bind_all_for_describe(&filters, &schema_with_enum, &[false])
            .expect_err("skip_enum_label_validation=false must reject the same invalid label");
        assert_eq!(err.wire_code(), "22P02");

        // flags が短い（対応する要素が無い）場合は `false` 扱い（安全側）。
        let err_default = bind_all_for_describe(&filters, &schema_with_enum, &[])
            .expect_err("missing flag entries must default to full validation");
        assert_eq!(err_default.wire_code(), "22P02");

        // 妥当なラベルは `skip_enum_label_validation` の値に関係なく常に成功する。
        let valid_filters = [DeclarativeFilter::equals("mood", "happy")];
        assert!(bind_all_for_describe(&valid_filters, &schema_with_enum, &[true]).is_ok());
        assert!(bind_all_for_describe(&valid_filters, &schema_with_enum, &[false]).is_ok());
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
    fn check_filter_count_accepts_at_limit_and_rejects_over_limit() {
        assert!(check_filter_count(MAX_METADATA_FILTERS).is_ok());
        assert_eq!(
            check_filter_count(MAX_METADATA_FILTERS + 1)
                .unwrap_err()
                .wire_code(),
            "54000"
        );
    }

    #[test]
    fn multibyte_prefix_is_boundary_safe() {
        let f = DeclarativeFilter::starts_with("path", "日本語/")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some(ScalarRef::Text("日本語/doc.md"))));
        assert!(!f.matches(Some(ScalarRef::Text("語/doc.md"))));
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

    // --- TABLE-13・TASK-199、Issue #891: レーン B（範囲比較）------------------

    #[test]
    fn typed_compare_date_equality_and_range() {
        let schema = typed_compare_schema();
        let eq = DeclarativeFilter::compare("day", CompareOp::Eq, "2024-01-01")
            .bind(&schema)
            .expect("bind DATE equality");
        assert!(eq.matches(Some(ScalarRef::Date(19723))));
        assert!(!eq.matches(Some(ScalarRef::Date(19724))));
        assert!(!eq.matches(None));

        let gt = DeclarativeFilter::compare("day", CompareOp::Gt, "2024-01-01")
            .bind(&schema)
            .expect("bind DATE range");
        assert!(gt.matches(Some(ScalarRef::Date(19724))));
        assert!(!gt.matches(Some(ScalarRef::Date(19723))));
    }

    #[test]
    fn typed_compare_timestamp_range() {
        let schema = typed_compare_schema();
        let le = DeclarativeFilter::compare("at", CompareOp::Le, "1970-01-01 00:00:01")
            .bind(&schema)
            .expect("bind TIMESTAMP range");
        assert!(le.matches(Some(ScalarRef::Timestamp(1_000_000))));
        assert!(le.matches(Some(ScalarRef::Timestamp(0))));
        assert!(!le.matches(Some(ScalarRef::Timestamp(1_000_001))));
    }

    #[test]
    fn typed_compare_numeric_does_not_round_to_column_scale() {
        // 列は NUMERIC(10, 2) だが、リテラル自身の scale（3 桁）をそのまま
        // 保持して正確に比較する（列の scale へ丸めると `1.005` が `1.01` に
        // 化けてしまい範囲比較の意味が変わる）。
        let schema = typed_compare_schema();
        let gt = DeclarativeFilter::compare("price", CompareOp::Gt, "1.005")
            .bind(&schema)
            .expect("bind NUMERIC range without rounding to column scale");
        let just_below = Decimal::from_parts(1004, 3).expect("1.004");
        let equal = Decimal::from_parts(1005, 3).expect("1.005");
        let just_above = Decimal::from_parts(1006, 3).expect("1.006");
        assert!(!gt.matches(Some(ScalarRef::Numeric(just_below))));
        assert!(!gt.matches(Some(ScalarRef::Numeric(equal))));
        assert!(gt.matches(Some(ScalarRef::Numeric(just_above))));

        // 列 scale（2 桁）で丸めた `1.01` と比較した場合、`1.005` は境界上に
        // なるが、丸めない正確な比較では `1.005 < 1.01` の関係を保つ。
        let rounded_to_column_scale = Decimal::from_parts(101, 2).expect("1.01");
        assert!(gt.matches(Some(ScalarRef::Numeric(rounded_to_column_scale))));
    }

    #[test]
    fn typed_compare_numeric_bare_number_literal() {
        let schema = typed_compare_schema();
        let ge = DeclarativeFilter::compare_numeric_literal("price", CompareOp::Ge, "2.5")
            .bind(&schema)
            .expect("bind NUMERIC bare-number-literal range");
        assert!(ge.matches(Some(ScalarRef::Numeric(
            Decimal::from_parts(250, 2).expect("2.50")
        ))));
        assert!(!ge.matches(Some(ScalarRef::Numeric(
            Decimal::from_parts(249, 2).expect("2.49")
        ))));
    }

    #[test]
    fn typed_compare_uuid_range_uses_byte_order() {
        let schema = typed_compare_schema();
        let gt = DeclarativeFilter::compare(
            "ext_id",
            CompareOp::Gt,
            "00000000-0000-0000-0000-000000000000",
        )
        .bind(&schema)
        .expect("bind UUID range");
        assert!(
            gt.matches(Some(ScalarRef::Uuid(crate::uuid::Uuid::from_bytes(
                [0xff; 16]
            ))))
        );
        assert!(
            !gt.matches(Some(ScalarRef::Uuid(crate::uuid::Uuid::from_bytes(
                [0x00; 16]
            ))))
        );
    }

    #[test]
    fn typed_compare_bytea_range_uses_dictionary_order() {
        let schema = typed_compare_schema();
        let lt = DeclarativeFilter::compare("blob", CompareOp::Lt, "\\xff")
            .bind(&schema)
            .expect("bind BYTEA range");
        assert!(lt.matches(Some(ScalarRef::Bytes(&[0xde, 0xad]))));
        assert!(!lt.matches(Some(ScalarRef::Bytes(&[0xff]))));
    }

    #[test]
    fn typed_compare_rejects_type_mismatched_column() {
        // TEXT 列（`path` 相当。ここでは `typed_compare_schema` に無いので
        // `schema()` の `path` を使う）への範囲比較は非対応列として `22000`。
        let err = DeclarativeFilter::compare("path", CompareOp::Gt, "x")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn typed_compare_rejects_malformed_literal_per_column_type() {
        let schema = typed_compare_schema();
        assert_eq!(
            DeclarativeFilter::compare("day", CompareOp::Eq, "not-a-date")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        assert_eq!(
            DeclarativeFilter::compare("ext_id", CompareOp::Eq, "not-a-uuid")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22P02"
        );
        assert_eq!(
            DeclarativeFilter::compare("blob", CompareOp::Eq, "not-hex")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
    }

    #[test]
    fn typed_compare_describe_dummy_skip_accepts_placeholder_without_parsing() {
        // Prepared Describe（`$n` 由来のダミー文字列 `"0"`）専用の縮退経路。
        // `"0"` は DATE/UUID/BYTEA の文法として不正だが、
        // `skip_enum_label_validation = true` の位置では実際には解析せず
        // プレースホルダ値へ縮退するため束縛は成功する（ENUM の語彙照合
        // スキップと同じ一般化。詳細は `bind_impl` のドキュメント参照）。
        let schema = typed_compare_schema();
        for column in ["day", "at", "price", "ext_id", "blob"] {
            let filters = [DeclarativeFilter::compare(column, CompareOp::Eq, "0")];
            let bound = bind_all_for_describe(&filters, &schema, &[true])
                .unwrap_or_else(|e| panic!("describe dummy skip for {column:?} failed: {e:?}"));
            assert_eq!(bound.len(), 1);

            // flags[0] = false（実値扱い）: ダミー文字列 `"0"` は
            // DATE/TIMESTAMP/UUID/BYTEA の文法としては不正なため束縛が
            // 失敗する。NUMERIC だけは `"0"` 自体が正当な数値リテラルの
            // ため、実値扱いでも成功する（ダミー値かどうかで結果が
            // 変わらない自明なケース）。
            let result = bind_all_for_describe(&filters, &schema, &[false]);
            if column == "price" {
                assert!(
                    result.is_ok(),
                    "NUMERIC column accepts \"0\" as a real literal too"
                );
            } else {
                let err = result.unwrap_err();
                assert!(
                    err.wire_code() == "22000" || err.wire_code() == "22P02",
                    "column {column:?}: unexpected wire_code {:?}",
                    err.wire_code()
                );
            }
        }
    }
}
