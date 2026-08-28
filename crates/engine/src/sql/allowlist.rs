//! AST 許可リスト検証（TASK-74・SQL-8・ERR-2 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md・docs/spec/04-behavior/error-format.md）。
//!
//! 責務境界: [`lexer`](crate::sql::lexer) が返すトークン列を、明示的に許可した
//! 形状だけに一致するか再帰下降で判定する。**許可リスト**として実装するため、
//! 期待しない字句・構文は個別に検出せず、期待する位置に来ないというだけで
//! 構造的に拒否する（fail-closed。未知・未対応構文は既定で拒否側に落ちる）。
//!
//! 受理側（実在テーブルに対する検索・取得の実行）は本モジュールの管轄外で、
//! 後続タスクが [`ValidatedStatement`] を土台に実装する。本モジュールは
//! 「許可形状の構造判定を通過させる」ところまでに責務を留める。

use crate::error_format::{ClassifiedError, ErrorClass};
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::lexer::{self, Keyword, LexError, Token};
use crate::sql::plan::{self, EvaluationOrder, Stage};
use crate::sql::udf_call::{
    BinOp, Expr, MAX_CALL_ARGS, MAX_EXPR_DEPTH, MAX_EXPR_NODES, MAX_UDF_PARAMS,
};
use crate::sql::using_operation_id::OperationId;

/// エラーメッセージへ含める入力断片の長さ上限。untrusted 入力をそのまま無加工で
/// 長大にエラーへ埋め込まない（security.md「情報漏えい」対応）。値は
/// [`crate::error_format`] の単一真実源を参照し、wire 応答直前の最終切り詰め
/// （`WireError::new`）と本モジュールの構築時切り詰めが別々の上限を持たないようにする
/// （TASK-152・ERR-2）。
const MAX_ERROR_DETAIL_LEN: usize = crate::error_format::MAX_MESSAGE_LEN;

/// INSERT の列リスト・VALUES リストがそれぞれ持てる要素数の上限（SQL-10、TASK-80）。
/// 無制限 `Vec` 確保を避ける（`.claude/rules/security.md`「不安全な設計｜無制限
/// リソース確保（DoS）」対応）。`catalog::MAX_COLUMN_COUNT` と同値を採用する。
const MAX_INSERT_COLUMNS: usize = 256;

/// ORDER BY の関数呼び出し形で許可する関数名を照合する（大文字小文字を区別しない）。
/// 未知の名前は fail-closed に拒否し、識別子であれば任意の名前を関数呼び出しとして
/// 受理してしまう構造上の抜け穴を作らない。
fn is_allowed_order_by_function_name(name: &str) -> bool {
    matches!(name.to_ascii_uppercase().as_str(), "HYBRID_RRF" | "HYBRID")
}

/// WHERE の述語呼び出し形（空引数）で許可する述語名を照合する（大文字小文字を
/// 区別しない）。未知の名前は fail-closed に拒否する。
fn is_allowed_where_predicate_name(name: &str) -> bool {
    matches!(name.to_ascii_uppercase().as_str(), "VISIBLE")
}

/// 集計関数（TASK-166・SQL-13）で許可する関数名を照合する（大文字小文字を区別
/// しない）。未知の名前は fail-closed に拒否する（[`is_allowed_where_predicate_name`]
/// と同方針）。`sql::udf_call::is_reserved_function_name` から名前空間一本化の
/// ため参照される（CREATE FUNCTION での集計関数名との衝突を防ぐ。Cursor Bugbot
/// 指摘対応・PR #229）。
pub(crate) fn is_aggregate_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

/// 1 文の集計項目リストが持てる要素数の上限（TASK-166・SQL-13）。無制限 `Vec` 確保を
/// 避ける（`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」
/// 対応）。
const MAX_AGGREGATE_ITEMS: usize = 32;

/// `USING PLAN('<query>')`（TASK-77・SQL-5）に渡せる自然言語クエリ本文のバイト長
/// 上限。アロケーション（字句解析・LLM プロンプトへの組み込み）に入る前に拒否する
/// （`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」対応）。
/// [`crate::sql::parser::MAX_VECTOR_LITERAL_BYTES`] の既存前例と同じ 64 KiB を採用する
/// （意味論側の決定的切り詰めは `query_planner::MAX_QUESTION_CHARS` が別途担う）。
const MAX_USING_PLAN_LEN: usize = 64 * 1024;

fn truncate_for_error(s: &str) -> String {
    if s.len() <= MAX_ERROR_DETAIL_LEN {
        return s.to_string();
    }
    // 文字境界で安全に切り詰める（マルチバイト文字の途中で切らない）。添字直接
    // アクセスをせず `get()` で明示的に処理する（coding-rust.md 「untrusted 入力の扱い」）。
    let mut end = MAX_ERROR_DETAIL_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    match s.get(..end) {
        Some(prefix) => format!("{prefix}..."),
        None => "...".to_string(),
    }
}

/// SQL 表層のエラー型。ERR-2 参照（docs/spec/04-behavior/error-format.md）。
/// engine 全体の共通エラー型統合は他タスクの管轄のため、本モジュールローカルの
/// 型として定義する。
#[derive(Debug, Clone)]
pub enum SqlSurfaceError {
    /// 許可リスト外の構文（構文解析失敗を含む）。
    UnsupportedSyntax { detail: String },
    /// FROM に指定したテーブルがスキーマカタログに存在しない。
    UndefinedTable { name: String },
    /// カタログ照会（`TableLookup`）側の内部エラー（redb I/O 等）。受理・拒否のいずれにも
    /// 倒さず、fail-closed にエラー伝播する（`.claude/rules/security.md`
    /// 「不安全な設計」対応）。
    Internal { detail: String },
    /// 構造は許可リストを通過したが、束縛（`sql::parser::bind`、TASK-75）が値・引数を
    /// 意味論的に不正と判定した（未知の列名・列型不一致・ベクトルリテラルの不正形式・
    /// 非有限値・次元不一致、`LIMIT` 範囲外、hybrid の 2 引数形（実行不能）等。ERR-2:
    /// `22000`）。
    InvalidInput { detail: String },
    /// untrusted 入力のサイズがアロケーション前の上限を超過した（ベクトルリテラル
    /// 64 KiB 超過、候補集合の容量上限超過等。ERR-2: `54000`）。
    PayloadTooLarge { detail: String },
    /// 書き込み系 SQL 文の文末専用句 `USING OPERATION_ID '<id>'`（SQL-10、TASK-80）の
    /// 省略（空文字値を含む）。RECOVER-1 の必須化ガードの前段として、SQL 表層が
    /// 書き込みトランザクションを開始する**前**に構造検証段階で fail-closed に
    /// 拒否する（ERR-2: `23502`）。
    MissingOperationId,
    /// INSERT 先の行 `id` が**呼び出し元テナントの名前空間内で**既に使われている
    /// （`tenant::insert_typed_row` の [`crate::tenant::TenantWriteError::IdConflict`]
    /// を SQL 表層へ写像したもの。ERR-2: `23505`）。行ストアの物理キーは
    /// `(tenant_id, id)` で名前空間化されているため（TABLE-12・RLS-9）、他テナントが
    /// 同じ `id` を保持していても本 variant にはならない。`operation_id` 単位の
    /// 冪等判定（台帳による重複拒否・内容不一致検出）は [`SqlSurfaceError::DuplicateOperationId`]・
    /// [`SqlSurfaceError::OperationIdContentMismatch`] が担う（TASK-101・RECOVER-10。
    /// TASK-94・RECOVER-3 の重複拒否契約を包含する）。
    IdConflict,
    /// `operation_id` 台帳（TASK-93）に記録済みの `operation_id` へ、**内容が一致する**
    /// 書き込みが再送された（TASK-101・対象ビヘイビア: RECOVER-10。
    /// [`crate::tenant::TenantWriteError::DuplicateOperationId`] の写像。ERR-2: `23505`）。
    /// 行キー衝突（[`SqlSurfaceError::IdConflict`]）とは別の固定文言を返し、クライアント
    /// が両者を取り違えないようにする。
    DuplicateOperationId,
    /// 台帳に記録済みの `operation_id` へ、**内容が異なる**書き込みが再送された、
    /// または内容一致を証明できない旧フォーマットの台帳エントリへ再送された
    /// （TASK-101・RECOVER-10。[`crate::tenant::TenantWriteError::OperationIdContentMismatch`]
    /// の写像。ERR-3（TASK-154）: `22023`。他のいかなる分類（特に `23505`）にも写像しない
    /// ことは `tests/error_format_err3.rs` で検証する。fail-closed:
    /// commit 済み確定の根拠にしない）。
    OperationIdContentMismatch,
    /// 集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`、TASK-166・SQL-13）の数値演算が
    /// `u64`/`f64` の表現範囲を超過した（`checked_add` 失敗・`f64` 側の非有限値化）。
    /// 黙って wrap・非有限値化せず fail-closed に拒否する（`.claude/rules/coding-rust.md`
    /// 「整数演算は checked_*/saturating_* を使う」対応）。ERR-2
    /// （`docs/spec/04-behavior/error-format.md`）: `22003`。
    NumericOutOfRange { detail: String },
}

impl SqlSurfaceError {
    /// ERR-2（docs/spec/04-behavior/error-format.md）の wire_code 写像。
    /// TASK-152 で単一真実源化した [`ClassifiedError::wire_code`] へ委譲する
    /// （既存の返値は 1 つも変えない。委譲先は `error_class()` の `match` のみを
    /// 単一の判定点として持つ）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }

    /// クライアント（wire 層 `ErrorResponse`）へそのまま返してよい文言を返す。
    /// `Internal`（`wire_code() == "XX000"`）は redb I/O エラー等の内部ストレージ
    /// 詳細を保持しているため固定の一般化メッセージへ丸め、それ以外の variant は
    /// 通常の `Display` 文言（テナント越境の存在情報を含まないよう各コンストラクタ
    /// 側で既に切り詰め・一般化済み）をそのまま返す（security.md P0「private
    /// 情報の漏えい」対応。`wire-server::simple_query` はエラー応答の整形時に
    /// `to_string()` ではなく必ずこちらを使うこと）。TASK-152 で
    /// [`ClassifiedError::client_message`] へ委譲する（返値は不変）。
    pub fn client_message(&self) -> String {
        ClassifiedError::client_message(self)
    }

    /// `pub(crate)`: `sql::allowlist::Parser::parse_operation_id_clause`・
    /// `sql::using_operation_id::OperationId::parse` が文末句の省略（空文字値を
    /// 含む）を報告するために使う（SQL-10、TASK-80）。
    pub(crate) fn missing_operation_id() -> Self {
        SqlSurfaceError::MissingOperationId
    }

    /// `pub(crate)`: `catalog.rs::impl TableLookup for Storage` が `CatalogError::Invalid`
    /// を `42601` へ写像する際にも、同じ切り詰め規約を経由させるために公開する。
    pub(crate) fn unsupported(detail: impl Into<String>) -> Self {
        SqlSurfaceError::UnsupportedSyntax {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// FROM に指定されたテーブルがカタログ未存在（ERR-2: `42P01`）。テーブル名は
    /// untrusted な字句解析結果のため、`UnsupportedSyntax` と同様に長さを切り詰めて
    /// エラーへ含める（security.md「情報漏えい」対応）。
    fn undefined_table(name: impl Into<String>) -> Self {
        SqlSurfaceError::UndefinedTable {
            name: truncate_for_error(&name.into()),
        }
    }

    /// `pub(crate)`: `sql::parser::bind`（TASK-75）が束縛時の値・引数不正を報告するために
    /// 使う。他の variant と同じ切り詰め規約を経由する。
    pub(crate) fn invalid_input(detail: impl Into<String>) -> Self {
        SqlSurfaceError::InvalidInput {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `sql::parser::bind`・`sql::exec`（TASK-75）がアロケーション前の
    /// サイズ上限超過を報告するために使う。
    pub(crate) fn payload_too_large(detail: impl Into<String>) -> Self {
        SqlSurfaceError::PayloadTooLarge {
            detail: truncate_for_error(&detail.into()),
        }
    }

    /// `pub(crate)`: `sql::aggregate`（TASK-166・SQL-13）が集計の数値演算オーバー
    /// フロー（`u64` の `checked_add` 失敗・`f64` の非有限値化）を報告するために使う。
    pub(crate) fn numeric_out_of_range(detail: impl Into<String>) -> Self {
        SqlSurfaceError::NumericOutOfRange {
            detail: truncate_for_error(&detail.into()),
        }
    }
}

/// TASK-152（ERR-2）: `wire_code` 写像の単一真実源 [`ErrorClass`] へ委譲する。
/// variant → `ErrorClass` の対応は既存 `wire_code()` の返値と 1:1 で一致させ、
/// 委譲化で応答コードを変えない（`IdConflict` は行 `id` 衝突であり、原因を問わない
/// 一意制約違反の分類 [`ErrorClass::UniqueViolation`]（`23505`）へ写像する）。
impl ClassifiedError for SqlSurfaceError {
    fn error_class(&self) -> ErrorClass {
        match self {
            SqlSurfaceError::UnsupportedSyntax { .. } => ErrorClass::UnsupportedSqlSyntax,
            SqlSurfaceError::UndefinedTable { .. } => ErrorClass::TableNotFound,
            SqlSurfaceError::Internal { .. } => ErrorClass::InternalError,
            SqlSurfaceError::InvalidInput { .. } => ErrorClass::InvalidInput,
            SqlSurfaceError::PayloadTooLarge { .. } => ErrorClass::PayloadTooLarge,
            SqlSurfaceError::MissingOperationId => ErrorClass::MissingOperationId,
            SqlSurfaceError::IdConflict => ErrorClass::UniqueViolation,
            SqlSurfaceError::DuplicateOperationId => ErrorClass::UniqueViolation,
            SqlSurfaceError::NumericOutOfRange { .. } => ErrorClass::NumericOutOfRange,
            SqlSurfaceError::OperationIdContentMismatch => ErrorClass::OperationIdContentMismatch,
        }
    }

    fn client_message(&self) -> String {
        match self {
            SqlSurfaceError::Internal { .. } => "internal error".to_string(),
            other => other.to_string(),
        }
    }
}

impl std::fmt::Display for SqlSurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlSurfaceError::UnsupportedSyntax { detail } => {
                write!(f, "unsupported SQL syntax: {detail}")
            }
            SqlSurfaceError::UndefinedTable { name } => write!(f, "undefined table: {name}"),
            SqlSurfaceError::Internal { detail } => write!(f, "internal error: {detail}"),
            SqlSurfaceError::InvalidInput { detail } => write!(f, "invalid input: {detail}"),
            SqlSurfaceError::PayloadTooLarge { detail } => {
                write!(f, "payload too large: {detail}")
            }
            SqlSurfaceError::MissingOperationId => {
                write!(f, "missing USING OPERATION_ID clause")
            }
            // 所有テナント・行内容・他テナントの存在有無を一切含めない固定文言
            // （security.md P0）。同一テナント内の重複でのみ返るため、この応答自体が
            // 他テナントの行 id 存在オラクルにならない。
            SqlSurfaceError::IdConflict => {
                write!(f, "row id already exists")
            }
            // operation_id・行内容・テナントを含めない固定文言（security.md P0。
            // `crate::tenant::TenantWriteError::DuplicateOperationId`/
            // `OperationIdContentMismatch` と同じ契約。`IdConflict` の文言と区別できる
            // ことが目的）。
            SqlSurfaceError::DuplicateOperationId => {
                write!(f, "operation_id already recorded with the same content")
            }
            SqlSurfaceError::NumericOutOfRange { detail } => {
                write!(f, "numeric value out of range: {detail}")
            }
            SqlSurfaceError::OperationIdContentMismatch => {
                write!(f, "operation_id already recorded with different content")
            }
        }
    }
}

impl std::error::Error for SqlSurfaceError {}

impl From<LexError> for SqlSurfaceError {
    fn from(e: LexError) -> Self {
        SqlSurfaceError::unsupported(format!("{} (near byte {})", e.message, e.byte_offset))
    }
}

/// FROM に指定したテーブルがスキーマカタログに実在するかを確認するための抽象。
/// `catalog.rs::Storage` に対して実装し（`impl TableLookup for Storage`）、
/// allowlist の単体テストを実 `redb` ストレージ非依存で書けるようにする軽量な境界。
pub trait TableLookup {
    /// `name` が定義済みテーブルなら `Ok(true)`、未定義なら `Ok(false)`。
    /// カタログ照会自体が失敗した場合（redb I/O 等）は `Err` とし、
    /// 存在するとも存在しないとも判定しない（fail-closed）。
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError>;
}

/// ORDER BY 関数呼び出し形（`FunctionCall`, TASK-75）の 1 引数。本モジュールは
/// トークン種別（識別子／文字列リテラル）のみを構造として保持し、列名としての
/// 妥当性・リテラルの意味論的解釈は `sql::parser::bind`（TASK-75）の責務とする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionArg {
    Ident(String),
    StringLiteral(String),
}

/// `ORDER BY` 式の許可形状。TASK-74・SQL-8 参照（docs/spec/05-tasks.md）。
/// TASK-75 でリテラル値・関数引数を保持するよう拡張した（構造判定だけでなく、
/// 後続の束縛（`sql::parser::bind`）がベクトルリテラル解析・hybrid 引数解釈に使う）。
///
/// **TASK-77（SQL-5）で追加した破壊的変更（BREAKING CHANGE、codex-review P1 指摘対応、
/// PR #266）**: `UsingPlan` variant を追加した。本 enum は `#[non_exhaustive]` を
/// 付けていない公開型のため、この型に対して網羅的 `match` を書いている下流コードは
/// 本バージョンで追加された variant に対応するまでコンパイルが通らなくなる
/// （[`WherePredicate`] の `Expression`（TASK-79）・`Prefix`（TASK-147）追加時と同じ
/// 既存の破壊的変更運用に倣う）。移行方針: 既存の網羅的 `match` に `UsingPlan` の腕
/// （`USING PLAN` 文には意味を持つフィールドが無く、通常到達しない防御的経路として
/// 扱ってよい）を追加する。spec 側の定義変更は不要（TASK-77・SQL-5 のスコープ内の
/// 追加であり、`docs/spec/05-tasks.md` の対応タスクに包含される）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderByForm {
    /// 距離演算子形（`<列> <=> '<ベクトルリテラル>'`）。
    Distance { column: String, literal: String },
    /// 関数呼び出し形。引数トークン列は構造（括弧の対応・許可トークン種別のみ）を
    /// 保持し、個数・意味の解釈は `sql::parser::bind` が行う（TASK-75 時点で
    /// `hybrid_rrf`/`HYBRID` は 2 引数形・4 引数形の両方を構造上受理するが、
    /// 実行可能（束縛成功）なのは 4 引数形のみ。2 引数形は構造は受理しつつ束縛時に
    /// `SqlSurfaceError::InvalidInput`（`22000`）で拒否する。既存 2 引数形の
    /// マーシャリング・許可リスト受理そのものは変更しない）。
    FunctionCall {
        name: String,
        args: Vec<FunctionArg>,
    },
    /// `USING PLAN('<query>')`（TASK-77・SQL-5）が選ばれた文のプレースホルダ。
    /// `ORDER BY` 節自体が構文上存在しない（`USING PLAN` は `ORDER BY` と相互排他）
    /// ため意味を持つフィールドを持たず、ランキングは
    /// [`ValidatedStatement::using_plan`] から `sql::using_plan` が独立に導出する。
    /// `sql::parser::bind_ranking` はこの variant に到達すると内部エラーで拒否する
    /// （到達は `core.rs` の分岐が壊れた場合のみの防御的経路）。
    UsingPlan,
}

/// WHERE 句の許可形状。名前を照合する述語呼び出し形は、許可された名前
/// （[`is_allowed_where_predicate_name`]）のみを通過させる。
///
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Expression`
/// variant を追加した（宣言的 UDF・組み込み関数呼び出しを含む比較式
/// `<expr> <cmp> <expr>`。式の意味論検証は `sql::parser::bind_in_session` の責務）。
///
/// **TASK-147（EXT-3）で追加した破壊的変更（BREAKING CHANGE）**: `Prefix` variant
/// を追加した（`<col> LIKE '<prefix>%'` の前方一致条件。網羅的 `match` を持つ
/// 外部コードは要対応）。パターン文字列は無加工で保持し、意味論的な検証
/// （末尾 `%` のみ許可・空 prefix 拒否等）は `declarative_filter::parse_prefix_pattern`
/// （`sql::parser::bind_in_session` から呼ばれる）の責務とする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WherePredicate {
    /// 列と文字列リテラルの等価条件（TASK-75: リテラル値を保持する）。
    Equality { column: String, value: String },
    /// 前方一致条件（TASK-147・EXT-3）。`LIKE` は [`Keyword`] へ追加せず、TASK-80 と
    /// 同じ「`Token::Ident` をパーサー位置でのみ文脈照合」方式にして `like` という
    /// 列名を壊さない（[`Parser::parse_where`] 参照）。
    Prefix { column: String, pattern: String },
    /// 許可された名前の述語呼び出し形（空引数）。
    PredicateCall { name: String },
    /// 式の比較述語（TASK-79・SQL-9）。`Expr::Binary` の比較演算子（`> < >= <= =`）
    /// を頂点に持つ木のみを許可する（`parse_where` が構造的に保証する）。
    Expression(Expr),
}

/// SELECT リストの 1 項目（TASK-79・SQL-9 で式項目を追加する際の共通表現）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    /// 既存の単純な列名項目（`*` 展開・裸の列名）。
    Column(String),
    /// 関数呼び出しを頂点に持つ式項目（TASK-79・SQL-9: 宣言的 UDF・組み込み関数の
    /// 結果列位置での呼び出し）。`alias` 省略時の列名は `sql::parser` が関数名から
    /// 導出する。
    Expr { expr: Expr, alias: Option<String> },
}

/// SELECT リストの許可形状（TASK-75）。`*`・単純な列名リストに加え、TASK-79（SQL-9）
/// で式項目（少なくとも 1 項目が関数呼び出しを含む形）を [`Items`] として追加した。
/// 全項目が単純な列名の場合は従来どおり [`Columns`] のまま（後方互換）。
///
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Items` variant を
/// 追加した。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    All,
    Columns(Vec<String>),
    Items(Vec<SelectItem>),
}

/// 許可形状の構造判定を通過した SQL 文（後続タスクのパーサー・実行計画の土台）。
/// 本モジュールが保証するのはここまでの構造情報のみで、列名・リテラル値の意味論的な
/// 妥当性は検証しない（`sql::parser::bind` の責務）。
///
/// **TASK-161 で意図的に非公開化した破壊的変更（BREAKING CHANGE）**: 全フィールドを
/// `pub` から `pub(crate)` へ変更し `#[non_exhaustive]` を付与した。クレート外からの
/// 直接のフィールド参照・構造体リテラル構築は今後不可能。構築は
/// [`ValidatedStatement::new`]／[`ValidatedStatement::with_search_mode`]、読み取りは
/// [`ValidatedStatement::table_name`] 等の各アクセサーメソッドを使う（詳細は PR #188 の
/// Breaking Changes 節を参照。TASK-164 拡張点の前方互換確保とカプセル化のため）。
///
/// `#[non_exhaustive]`: TASK-161（SQL-12）で `search_mode` フィールドを追加した際、
/// 既存の構造体リテラル構築コードが必須フィールド不足でコンパイル不能になる破壊的
/// 変更となった（AGENTS.md「公開 API・エラー契約の互換性（P1）」）。今後のフィールド
/// 追加が同様の破壊を再発させないよう、外部クレートからの構造体リテラル構築を非対応
/// にする。フィールドはカプセル化のため `pub(crate)` とし（クレート外からの直読み・
/// 直書きは不可。コード内では [`ValidatedStatement::table_name`] 等のアクセサー
/// メソッドを経由する）、クレート外からの構築は [`ValidatedStatement::new`]（既存
/// フィールド相当の引数を取る）と [`ValidatedStatement::with_search_mode`]（TASK-161
/// で追加した `search_mode` を設定するビルダー的メソッド）を経由する。本構造体は
/// 通常 [`validate_sql`] の戻り値として取得するが、上記 constructor 経由でも構築
/// できる（PR #188 レビュー指摘対応: 破壊的変更の移行経路を用意しつつ、直接の
/// フィールド読み書きは許可しない）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ValidatedStatement {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub(crate) table_name: String,
    pub(crate) projection: Projection,
    pub(crate) order_by: OrderByForm,
    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub(crate) where_predicates: Vec<WherePredicate>,
    pub(crate) limit: u32,
    /// `LIMIT` 直後の文末専用句 `USING MODE '<literal>'`（TASK-161・SQL-12）の生
    /// リテラル値。省略時は `None`。値の意味論的妥当性（`recall`／`precision` の
    /// 2 値のみ有効）は本モジュールの管轄外で、`sql::mode::SearchMode::parse_literal`
    /// を経由する `sql::parser::bind_with_session` が検証する。
    pub(crate) search_mode: Option<String>,
    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。未指定時は
    /// [`EvaluationOrder::DEFAULT`]（既存 TASK-75 の固定順 RLS→SCALAR→DISTANCE）。
    pub(crate) evaluation_order: EvaluationOrder,
    /// `USING PLAN('<query>')`（TASK-77・SQL-5）で指定された自然言語クエリの生
    /// リテラル値。省略時は `None`。`ORDER BY` と相互排他（`Some` のとき
    /// `order_by` は必ず [`OrderByForm::UsingPlan`]）。値の展開（LLM クエリ
    /// プランニング、TASK-110・PLAN-1）・ハイブリッド実行形への束縛は
    /// `sql::using_plan` の管轄で、本モジュールは構文構造の受理までを担う。
    pub(crate) using_plan: Option<String>,
}

impl ValidatedStatement {
    /// クレート外から構築するための constructor（TASK-161 で `search_mode`
    /// フィールドを追加する以前の既存フィールド相当の引数を取る）。`search_mode`
    /// は未指定（`None`）で構築され、必要なら [`Self::with_search_mode`] を続けて
    /// 呼ぶ。フィールドが `pub(crate)` のため、クレート外から `ValidatedStatement`
    /// を得るにはこの constructor か [`validate_sql`] の戻り値を経由するしかない。
    pub fn new(
        table_name: String,
        projection: Projection,
        order_by: OrderByForm,
        where_predicates: Vec<WherePredicate>,
        limit: u32,
        evaluation_order: EvaluationOrder,
    ) -> Self {
        Self {
            table_name,
            projection,
            order_by,
            where_predicates,
            limit,
            search_mode: None,
            evaluation_order,
            using_plan: None,
        }
    }

    /// `search_mode`（TASK-161・SQL-12）を設定したコピーを返すビルダー的メソッド。
    /// [`Self::new`] と組み合わせて `search_mode` を含む値を外部から構築する。
    #[must_use]
    pub fn with_search_mode(mut self, search_mode: Option<String>) -> Self {
        self.search_mode = search_mode;
        self
    }

    /// `using_plan`（TASK-77・SQL-5）を設定したコピーを返すビルダー的メソッド。
    /// [`Self::new`] と組み合わせて `using_plan` を含む値を外部から構築する。
    ///
    /// **不変条件**（codex-review P1 指摘対応、PR #266）: `using_plan` に `Some`
    /// を渡した場合、`order_by` を無条件で [`OrderByForm::UsingPlan`] へ揃える。
    /// `USING PLAN` と `ORDER BY` は構文上相互排他であり、
    /// `core.rs::EngineCore::execute_sql_in_session` は `order_by` の値ではなく
    /// `using_plan()` の有無のみで束縛経路を分岐するため、この揃え込みが無いと
    /// 呼び出し元が [`Self::new`] へ渡した `order_by`（例:
    /// [`OrderByForm::Distance`]）が無言で無視され、意図せず `USING PLAN` 経路
    /// （呼び出し元が想定していないハイブリッド実行形）が実行される事故になり得た
    /// （公開 builder が矛盾した状態を構築できてしまう問題）。`None` を渡した
    /// 場合は `order_by` を変更しない（[`Self::new`] で渡された値をそのまま保つ）。
    /// 逆方向（`order_by` に [`OrderByForm::UsingPlan`] を渡しつつ `using_plan` を
    /// 設定しない）の矛盾は本メソッドだけでは防げないため、`execute_sql_in_session`
    /// 側でも分岐前に防御的に検証する（同メソッドのドキュメント参照）。
    #[must_use]
    pub fn with_using_plan(mut self, using_plan: Option<String>) -> Self {
        if using_plan.is_some() {
            self.order_by = OrderByForm::UsingPlan;
        }
        self.using_plan = using_plan;
        self
    }

    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// SELECT リストの許可形状。
    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    /// ORDER BY 句の許可形状。
    pub fn order_by(&self) -> &OrderByForm {
        &self.order_by
    }

    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// `LIMIT` 句の値。
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// `USING MODE '<literal>'`（TASK-161・SQL-12）の生リテラル値。未指定時は `None`。
    pub fn search_mode(&self) -> Option<&str> {
        self.search_mode.as_deref()
    }

    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。
    pub fn evaluation_order(&self) -> EvaluationOrder {
        self.evaluation_order
    }

    /// `USING PLAN('<query>')`（TASK-77・SQL-5）の生リテラル値。未指定時は `None`。
    pub fn using_plan(&self) -> Option<&str> {
        self.using_plan.as_deref()
    }
}

/// [`validate_sql`]（TASK-161 の公開 API）が返す statement 種別。`SELECT` 以外の
/// 文が増えても [`ValidatedStatement`] 自体は SELECT 専用の構造を保つため、
/// 統一的な enum で包む。
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `CreateFunction`
/// variant を追加した。`Aggregate` variant が `GroupByClause`（TASK-167・SQL-14）
/// 経由で `f64`（HAVING リテラル）を保持するため `Eq` は導出しない
/// （`PartialEq` のみ。`Statement` の値比較はテストでのみ使う）。
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(ValidatedStatement),
    /// `SET search_mode = '<literal>'`（TASK-161・SQL-12）。カタログ照会を必要と
    /// しないためテーブル存在確認は行わない。リテラル値の意味論的妥当性検証は
    /// `core.rs::EngineCore::execute_sql_in_session` が `SearchMode::parse_literal`
    /// で行う（本モジュールは構造の受理までを担う）。
    SetSearchMode {
        value: String,
    },
    /// `CREATE FUNCTION <name>(<param>[, <param>...]) AS <expr>`（TASK-79・SQL-9）。
    /// カタログ照会を必要としない（セッションのみに影響するため FROM テーブルの
    /// 存在確認は行わない）。パラメータ名重複・登録済み名との衝突・列参照の禁止
    /// 等の意味論的妥当性検証は `sql::udf_call::define_function`（呼び出し元
    /// `core.rs::EngineCore::execute_sql_in_session`）が行う（本モジュールは構造の
    /// 受理までを担う）。
    CreateFunction {
        name: String,
        params: Vec<String>,
        body: Expr,
    },
    /// 集計関数のみを結果列とする `GROUP BY` なし・単一行結果の `SELECT`
    /// （TASK-166・SQL-13。C6a）。`FROM` 単一テーブルのカタログ存在確認を通過済み。
    Aggregate(ValidatedAggregate),
    /// `EXPLAIN SELECT ... USING PLAN('<query>') ...`（TASK-78・SQL-6）。`USING PLAN`
    /// を伴う検索 SELECT の前置のみを受理し（`using_plan()` が必ず `Some`）、
    /// `FROM` 単一テーブルのカタログ存在確認を通過済み。`EXPLAIN` は検索本体を
    /// 実行しない（LLM クエリ展開・モード解決結果を可視化する応答を構築するのみ。
    /// `core.rs::EngineCore::execute_sql_in_session` の管轄）。`USING PLAN` を伴わない
    /// 通常 SELECT・集計・`SET`・`CREATE FUNCTION` への `EXPLAIN` 前置は許可リスト外
    /// として `42601` で拒否する。
    Explain(ValidatedStatement),
}

/// 集計関数の種別（TASK-166・SQL-13）。関数名は [`is_aggregate_function_name`] で
/// 大文字小文字を区別せず照合済み。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFunc {
    /// `name` が [`is_aggregate_function_name`] を通過済みの前提で呼ぶ。
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggregateFunc::Count),
            "SUM" => Some(AggregateFunc::Sum),
            "AVG" => Some(AggregateFunc::Avg),
            "MIN" => Some(AggregateFunc::Min),
            "MAX" => Some(AggregateFunc::Max),
            _ => None,
        }
    }

    /// `AS <alias>` を省略した場合の既定結果列名（関数名の小文字）。
    pub(crate) fn default_alias(self) -> &'static str {
        match self {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Avg => "avg",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
        }
    }
}

/// 集計関数の引数（TASK-166・SQL-13）。`Star` は `COUNT(*)` 専用（[`Parser::parse_aggregate_item`]
/// が `COUNT` 以外での出現を構造的に拒否する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateArg {
    Star,
    Expr(Expr),
}

/// SELECT リストの集計項目 1 つ（TASK-166・SQL-13）。`alias` 省略時の列名は
/// [`AggregateFunc::default_alias`] を使う（`sql::parser::bind_aggregate` の責務）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateItem {
    pub(crate) func: AggregateFunc,
    pub(crate) arg: AggregateArg,
    pub(crate) alias: Option<String>,
}

/// 集計 `SELECT` リストの 1 項目（TASK-167・SQL-14 で `AggregateItem` 単独から拡張）。
/// `GroupKey` は `GROUP BY` 句がある場合にのみ現れ、`GROUP BY` 列と同名の裸の
/// 識別子（任意で `AS <alias>`）だけを構造上受理する（`allowlist::Parser::parse_select_item`
/// ではなく [`Parser::parse_aggregate_select_item`] が列名一致を検査する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateSelectItem {
    Aggregate(AggregateItem),
    /// `column` は SELECT リストに書かれた識別子そのもの（構文解析時点では
    /// まだ `GROUP BY` 句を読んでいないため）。`GROUP BY` 句の列名と一致するかは
    /// [`parse_aggregate_shape`] が全体を読み終えた後に検査する。
    GroupKey {
        column: String,
        alias: Option<String>,
    },
}

/// `HAVING` 述語 1 つ（TASK-167・SQL-14）。左辺は SELECT リストに現れる集計項目の
/// 実効名（別名または既定名）への参照のみを許可し、右辺は数値リテラルに限定する
/// （集計関数呼び出し形の直接記述・列同士の比較・文字列リテラルはいずれも許可
/// リスト外）。意味論的な名前解決（存在確認・型検査）は
/// `sql::parser::bind_aggregate` の責務。
#[derive(Debug, Clone, PartialEq)]
pub struct HavingPredicate {
    pub(crate) item_name: String,
    pub(crate) op: BinOp,
    pub(crate) literal: f64,
}

/// `GROUP BY` 集計の `ORDER BY` 対象（TASK-167・SQL-14）。`GROUP BY` 列名、または
/// SELECT リストの集計項目の実効名のいずれかの識別子を指す（解決は
/// `sql::parser::bind_aggregate`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateOrderBy {
    pub(crate) target: String,
    pub(crate) descending: bool,
}

/// `GROUP BY <column> [HAVING ...] [ORDER BY ...] [LIMIT ...]`（TASK-167・SQL-14）の
/// 許可形状。`column` はカタログ照会前の識別子のまま保持し（`TEXT` 列限定等の
/// 意味論的検査は束縛段）、`having`/`order_by`/`limit` はいずれも省略可能。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupByClause {
    pub(crate) column: String,
    pub(crate) having: Vec<HavingPredicate>,
    pub(crate) order_by: Option<AggregateOrderBy>,
    pub(crate) limit: Option<u32>,
}

/// 許可形状の構造判定を通過した集計 `SELECT` 文（TASK-166・SQL-13。TASK-167・
/// SQL-14 で `GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT` を追加）。[`ValidatedStatement`]
/// と同様、本モジュールが保証するのはここまでの構造情報のみで、列名・式の
/// 意味論的妥当性は検証しない（`sql::parser::bind_aggregate` の責務）。
/// フィールドは `pub(crate)`（クレート外からの直読み・直書き不可。カプセル化の方針は
/// [`ValidatedStatement`] と同じ）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedAggregate {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub(crate) table_name: String,
    /// SELECT リスト項目（順序保持。1..=[`MAX_AGGREGATE_ITEMS`]）。
    pub(crate) items: Vec<AggregateSelectItem>,
    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。既存の
    /// [`ValidatedStatement::where_predicates`] と同一の許可形状を再利用する。
    pub(crate) where_predicates: Vec<WherePredicate>,
    /// `GROUP BY` 句（TASK-167・SQL-14）。`None` なら TASK-166・SQL-13 の
    /// 単一行集計（既存の受理形）のまま。
    pub(crate) group_by: Option<GroupByClause>,
}

impl ValidatedAggregate {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// SELECT リスト項目（順序保持）。
    pub fn items(&self) -> &[AggregateSelectItem] {
        &self.items
    }

    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// `GROUP BY` 句（TASK-167・SQL-14）。`None` なら `GROUP BY` なしの単一行集計。
    pub fn group_by(&self) -> Option<&GroupByClause> {
        self.group_by.as_ref()
    }
}

/// INSERT の VALUES リストの 1 リテラル（SQL-10、TASK-80）。トークン種別
/// （文字列リテラル／数値）のみを構造として保持し、列型との照合・意味論的解釈は
/// `sql::parser::bind_insert` の責務とする。
#[derive(Debug, Clone, PartialEq)]
pub enum InsertLiteral {
    String(String),
    Number(String),
}

/// 許可形状の構造判定を通過した INSERT 文（SQL-10、TASK-80）。`ValidatedStatement`
/// と同様、本モジュールが保証するのはここまでの構造情報のみで、列名・値の
/// 意味論的妥当性は検証しない（`sql::parser::bind_insert` の責務）。
///
/// 受理する形は `INSERT INTO <table> (<col>[, <col>]*) VALUES (<lit>[, <lit>]*)
/// USING OPERATION_ID '<id>' [;]` の単一行形のみ（複数行 VALUES・RETURNING・
/// 可視性ラベル指定は許可リスト外）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedInsert {
    /// INTO に指定され、カタログ存在確認を通過したテーブル名。
    pub table_name: String,
    /// 列リストの宣言順（`columns[i]` は `values[i]` に対応する。個数は
    /// `parse_insert` が既に一致を確認済み）。
    pub columns: Vec<String>,
    pub values: Vec<InsertLiteral>,
    /// 文末専用句で搬送された、検証済みの `operation_id`（SQL-10）。句の欠落・明示
    /// `NULL` はいずれも `None`（TASK-92・RECOVER-1）。`validate_insert` は
    /// `LedgerMode::Ledgered`（既定）では `None` を書き込みトランザクション開始前に
    /// `23502` で拒否するため、この構成では常に `Some` になる。
    /// `LedgerMode::CompareOnlyWithoutLedger` では `None` を許す。
    pub operation_id: Option<OperationId>,
}

/// 1 文の最大トークン数を超えない前提の下で使うパーサーカーソル。
/// 再帰下降だが文法の深さは定数（statement → select_list/where/order_by の 1 階層）で、
/// 深いネストによるスタック消費は発生しない。
struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    /// 式ノード（`Expr::Number` / `Ident` / `Binary` / `Call`）の残り生成可能数。
    /// `parse_add_expr` / `parse_mul_expr` の左結合ループは `depth` を増やさず
    /// `lhs` に木を積み続けるため、`MAX_EXPR_DEPTH`（構文解析の再帰段数）だけでは
    /// "1+1+...+1" のような同一深さの連鎖入力を制限できない。ノード生成のたびに
    /// 本フィールドを課金し、AST 全体のノード数を UDF 本体と同じ [`MAX_EXPR_NODES`]
    /// で頭打ちにすることで、ノード予算枯渇後の `Box<Expr>` 再帰的 drop による
    /// スタック消費も定数に抑える（security.md「不安全な設計｜無制限リソース確保
    /// （DoS）」対応。1 文（`Parser` 1 インスタンス）につき共有）。
    expr_node_budget: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            expr_node_budget: MAX_EXPR_NODES,
        }
    }

    /// 式ノードを 1 つ生成する直前に呼び、予算を消費する。予算枯渇時は
    /// fail-closed に拒否する（[`Self::expr_node_budget`] 参照）。
    fn consume_expr_node(&mut self) -> Result<(), SqlSurfaceError> {
        self.expr_node_budget = self.expr_node_budget.checked_sub(1).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("expression exceeds the allowed node count")
        })?;
        Ok(())
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<(), SqlSurfaceError> {
        match self.advance() {
            Some(Token::Keyword(k)) if *k == kw => Ok(()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected keyword {kw:?}, got {other:?}"
            ))),
        }
    }

    /// パーサー位置に応じた文脈的キーワード照合（PR #189 レビュー指摘対応・P1）。
    /// `INSERT`/`INTO`/`VALUES`/`USING`/`OPERATION_ID` は
    /// [`lexer::Keyword`] へ含めない（`lexer.rs` の設計メモ参照）ため、
    /// [`Token::Ident`] を大文字小文字を区別せず文字列比較して INSERT 許可形状の
    /// 期待位置でのみキーワードとして扱う。同名の一般識別子（テーブル名・列名）は
    /// `expect_ident` を通る位置に置かれる限り、本メソッドの対象外として素通しする。
    fn expect_contextual_keyword(&mut self, word: &str) -> Result<(), SqlSurfaceError> {
        match self.advance() {
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word) => Ok(()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected keyword {word}, got {other:?}"
            ))),
        }
    }

    /// 次のトークンが文脈的キーワード `word` に一致するかを消費せずに判定する
    /// （`parse_operation_id_clause` が `USING` 句の有無で分岐するために使う）。
    fn peek_contextual_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
    }

    fn expect_punct(&mut self, c: char) -> Result<(), SqlSurfaceError> {
        match self.advance() {
            Some(Token::Punct(p)) if *p == c => Ok(()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected '{c}', got {other:?}"
            ))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, SqlSurfaceError> {
        match self.advance() {
            Some(Token::Ident(name)) => Ok(name.clone()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected identifier, got {other:?}"
            ))),
        }
    }

    /// 現在位置のトークンが `Token::Ident` かつ大文字小文字を区別せず `word` と
    /// 一致するかを消費せずに判定する。TASK-161（SQL-12）修正: `USING`・`SET` は
    /// 字句解析段階では予約語化せず（[`lexer`] のモジュールコメント参照）、構文上
    /// その語が必須の位置でのみ本メソッドで文脈的にキーワードとして判定する。
    /// これにより、それ以外の識別子位置（`FROM`・投影・`ORDER BY` 等）では
    /// `using`・`set` を従来どおり通常の識別子として扱える。
    fn peek_ident_matches(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case(word))
    }

    /// 現在位置が `Token::Ident` かつ大文字小文字を区別せず `word` と一致する場合のみ
    /// 消費して成功とする。TASK-161（SQL-12）修正: `SET` を statement 先頭という
    /// 文脈でのみキーワードとして判定するために使う（[`Parser::peek_ident_matches`]
    /// 参照）。
    fn expect_ident_matching(&mut self, word: &str) -> Result<(), SqlSurfaceError> {
        if !self.peek_ident_matches(word) {
            let other = self.peek();
            return Err(SqlSurfaceError::unsupported(format!(
                "expected '{word}', got {other:?}"
            )));
        }
        self.advance();
        Ok(())
    }

    fn expect_string_literal(&mut self) -> Result<String, SqlSurfaceError> {
        match self.advance() {
            Some(Token::StringLiteral(s)) => Ok(s.clone()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected string literal, got {other:?}"
            ))),
        }
    }

    fn expect_number(&mut self) -> Result<String, SqlSurfaceError> {
        match self.advance() {
            Some(Token::Number(n)) => Ok(n.clone()),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected number, got {other:?}"
            ))),
        }
    }

    /// SELECT リストの許可形状（`*`・単純な列名リスト・TASK-79（SQL-9）で追加した
    /// 式項目〔関数呼び出しを頂点に持つ式、任意で `AS <alias>`〕の混在リスト）。
    /// 全項目が単純な列名の場合は従来どおり [`Projection::Columns`]（後方互換。
    /// `AS` を列名として使う既存形を壊さないため、列名項目には `AS` を付けない）。
    fn parse_select_list(&mut self) -> Result<Projection, SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct('*'))) {
            self.advance();
            return Ok(Projection::All);
        }
        let mut items = vec![self.parse_select_item()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            items.push(self.parse_select_item()?);
        }
        if items.iter().all(|it| matches!(it, SelectItem::Column(_))) {
            let columns = items
                .into_iter()
                .map(|it| match it {
                    SelectItem::Column(name) => name,
                    SelectItem::Expr { .. } => unreachable!("filtered above"),
                })
                .collect();
            return Ok(Projection::Columns(columns));
        }
        Ok(Projection::Items(items))
    }

    /// SELECT リストの 1 項目。次のトークンが `ident '('` なら式項目（関数呼び出し。
    /// 続けて `AS <alias>` を任意で受理する）、それ以外は従来どおり裸の列名として
    /// 受理する。
    fn parse_select_item(&mut self) -> Result<SelectItem, SqlSurfaceError> {
        if let Some(Token::Ident(name)) = self.peek() {
            let name = name.clone();
            if matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('('))) {
                self.advance();
                let expr = self.parse_call_expr(name, 0)?;
                let alias = if self.peek_ident_matches("AS") {
                    self.advance();
                    Some(self.expect_ident()?)
                } else {
                    None
                };
                return Ok(SelectItem::Expr { expr, alias });
            }
        }
        Ok(SelectItem::Column(self.expect_ident()?))
    }

    /// 集計 SELECT リストの 1 項目（TASK-166・SQL-13）:
    /// `<agg_name> '(' ('*' | <expr>) ')' [AS <alias>]`。`*` は `COUNT` 専用
    /// （それ以外の関数での出現は `42601`）。空引数（`COUNT()`）・複数引数・
    /// `DISTINCT` 修飾はいずれも構造的に受理しない（`)` を期待する位置で不一致となり
    /// `42601` へ落ちる）。
    fn parse_aggregate_item(&mut self) -> Result<AggregateItem, SqlSurfaceError> {
        let name = self.expect_ident()?;
        let func = AggregateFunc::from_name(&name).ok_or_else(|| {
            SqlSurfaceError::unsupported(format!("unsupported aggregate function: {name}"))
        })?;
        self.expect_punct('(')?;
        let arg = if matches!(self.peek(), Some(Token::Punct('*'))) {
            if func != AggregateFunc::Count {
                return Err(SqlSurfaceError::unsupported(
                    "'*' is only allowed inside COUNT(*)",
                ));
            }
            self.advance();
            AggregateArg::Star
        } else {
            if matches!(self.peek(), Some(Token::Punct(')'))) {
                return Err(SqlSurfaceError::unsupported(
                    "aggregate function requires exactly one argument",
                ));
            }
            let expr = self.parse_value_expr(0)?;
            AggregateArg::Expr(expr)
        };
        self.expect_punct(')')?;
        let alias = if self.peek_ident_matches("AS") {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(AggregateItem { func, arg, alias })
    }

    /// 集計 SELECT リストの 1 項目（TASK-167・SQL-14 で拡張）。次のトークンが
    /// 集計関数名 `'('` なら従来どおり集計項目（[`Parser::parse_aggregate_item`]）、
    /// それ以外は `GROUP BY` 列と同名の裸の識別子（任意で `AS <alias>`）として
    /// [`AggregateSelectItem::GroupKey`] へ構造上受理する（列名一致・`GROUP BY` 句
    /// 自体の有無は [`parse_aggregate_shape`] が全体を読み終えてから検査する。
    /// 許可リストとして「集計項目か裸の識別子か」の 2 形にのみ絞り込み、それ以外
    /// （式・関数呼び出しの混在等）は `expect_ident` の失敗で `42601` に落ちる）。
    fn parse_aggregate_select_item(&mut self) -> Result<AggregateSelectItem, SqlSurfaceError> {
        if let Some(Token::Ident(name)) = self.peek() {
            if is_aggregate_function_name(name)
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('(')))
            {
                return Ok(AggregateSelectItem::Aggregate(self.parse_aggregate_item()?));
            }
        }
        let column = self.expect_ident()?;
        let alias = if self.peek_ident_matches("AS") {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(AggregateSelectItem::GroupKey { column, alias })
    }

    /// `GROUP BY <column>`（TASK-167・SQL-14）。`GROUP BY` は単一の裸識別子のみ
    /// 受理する（式・関数・複数列・位置番号はいずれも `expect_ident`／後続の
    /// `expect_end_of_statement` 系の失敗で `42601`）。`GROUP` は予約語化せず
    /// [`Parser::expect_contextual_keyword`] で文脈的に照合する（PR #189 の方針）。
    fn parse_group_by_clause(&mut self) -> Result<String, SqlSurfaceError> {
        self.expect_contextual_keyword("GROUP")?;
        self.expect_keyword(Keyword::By)?;
        self.expect_ident()
    }

    /// `HAVING <having_pred> [AND <having_pred>]*`（TASK-167・SQL-14）。
    /// `<having_pred> := <ident> <cmp> ['-'] <number>`。左辺は SELECT リスト集計
    /// 項目の実効名への参照のみを構造上許可し（存在確認・型検査は束縛段）、右辺は
    /// 数値リテラル限定（文字列リテラル・両辺集計・括弧・`OR` はいずれも許可リスト
    /// 外）。条件数は [`MAX_AGGREGATE_ITEMS`] で頭打ちにする（`54000`。無制限
    /// `Vec` 確保を避ける方針を HAVING 条件にも適用）。
    fn parse_having(&mut self) -> Result<Vec<HavingPredicate>, SqlSurfaceError> {
        self.expect_contextual_keyword("HAVING")?;
        let mut predicates = Vec::new();
        loop {
            if predicates.len() >= MAX_AGGREGATE_ITEMS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many HAVING predicates",
                ));
            }
            let item_name = self.expect_ident()?;
            let op = self.expect_cmp_op()?;
            let negative = matches!(self.peek(), Some(Token::Punct('-')));
            if negative {
                self.advance();
            }
            let raw = self.expect_number()?;
            let mut literal = crate::sql::udf_call::parse_number_literal(&raw)?;
            if negative {
                literal = -literal;
            }
            predicates.push(HavingPredicate {
                item_name,
                op,
                literal,
            });
            if matches!(self.peek(), Some(Token::Keyword(Keyword::And))) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(predicates)
    }

    /// 集計 `GROUP BY` の `ORDER BY <target> [ASC|DESC]`（TASK-167・SQL-14）。
    /// `<target>` は `GROUP BY` 列名または SELECT リスト集計項目の実効名のいずれか
    /// 1 つの識別子（意味論的な解決は束縛段）。`ASC`/`DESC` は予約語化せず文脈的に
    /// 照合し、省略時は昇順として扱う。
    fn parse_aggregate_order_by(&mut self) -> Result<AggregateOrderBy, SqlSurfaceError> {
        self.expect_keyword(Keyword::Order)?;
        self.expect_keyword(Keyword::By)?;
        let target = self.expect_ident()?;
        let descending = if self.peek_ident_matches("DESC") {
            self.advance();
            true
        } else if self.peek_ident_matches("ASC") {
            self.advance();
            false
        } else {
            false
        };
        Ok(AggregateOrderBy { target, descending })
    }

    /// 集計 `GROUP BY` の `LIMIT <n>`（TASK-167・SQL-14）。構文段では `u32` として
    /// 受理するのみで、範囲検査（`1..=MAX_GROUPS`）は束縛段
    /// （`sql::parser::bind_aggregate`）が行う。
    fn parse_aggregate_limit(&mut self) -> Result<u32, SqlSurfaceError> {
        self.expect_keyword(Keyword::Limit)?;
        let raw = self.expect_number()?;
        raw.parse()
            .map_err(|_| SqlSurfaceError::unsupported(format!("malformed LIMIT value: {raw}")))
    }

    /// WHERE 句の許可形状（等価条件・前方一致条件（TASK-147・EXT-3）・述語呼び出し形・
    /// TASK-79（SQL-9）で追加した式の比較述語 `<expr> <cmp> <expr>` の 4 種。
    /// `OR`・括弧によるネストは引き続き許可しない）。述語呼び出し形は許可された名前
    /// （[`is_allowed_where_predicate_name`]）のみを受理し、未知の名前は拒否する。
    ///
    /// 既存形との曖昧さ回避: 先頭が `ident '=' <string literal>` なら等価条件、
    /// `ident <文脈的 'LIKE'> <string literal>` なら前方一致条件、`ident '(' ')'`
    /// （許可名のみ）なら述語呼び出し形として確定的に判定し、いずれにも一致しない
    /// 場合のみ式の比較述語として再解析する（`pos` を巻き戻してから解析し直す。
    /// 式文法の `primary` は `ident '(' <args> ')'` も受理するため、`visible()`
    /// 以外の名前の呼び出し形はここで初めて式として解釈される）。`LIKE` は
    /// [`Keyword`] へ追加せず `Token::Ident` を本メソッド内でのみ文脈的に照合する
    /// （TASK-80 と同じ方式。`like` という列名の等価条件を壊さない）。`NOT LIKE`・
    /// `ILIKE`・`LIKE` の右辺が非リテラルの各形は、この確定判定に一致しないため
    /// 式述語フォールバックへ流れ、通常は `42601` で拒否される。
    fn parse_where(&mut self) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        let mut predicates = Vec::new();
        loop {
            let start = self.pos;
            let mut matched_legacy = false;
            if let Some(Token::Ident(name)) = self.peek().cloned() {
                if matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('=')))
                    && matches!(self.tokens.get(self.pos + 2), Some(Token::StringLiteral(_)))
                {
                    self.advance();
                    self.advance();
                    let value = self.expect_string_literal()?;
                    predicates.push(WherePredicate::Equality {
                        column: name,
                        value,
                    });
                    matched_legacy = true;
                } else if matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("LIKE"))
                    && matches!(self.tokens.get(self.pos + 2), Some(Token::StringLiteral(_)))
                {
                    self.advance();
                    self.advance();
                    let pattern = self.expect_string_literal()?;
                    predicates.push(WherePredicate::Prefix {
                        column: name,
                        pattern,
                    });
                    matched_legacy = true;
                } else if is_allowed_where_predicate_name(&name)
                    && matches!(self.tokens.get(self.pos + 1), Some(Token::Punct('(')))
                    && matches!(self.tokens.get(self.pos + 2), Some(Token::Punct(')')))
                {
                    self.advance();
                    self.advance();
                    self.advance();
                    predicates.push(WherePredicate::PredicateCall { name });
                    matched_legacy = true;
                }
            }
            if !matched_legacy {
                self.pos = start;
                let lhs = self.parse_value_expr(0)?;
                let op = self.expect_cmp_op()?;
                let rhs = self.parse_value_expr(0)?;
                predicates.push(WherePredicate::Expression(Expr::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }));
            }
            if matches!(self.peek(), Some(Token::Keyword(Keyword::And))) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(predicates)
    }

    /// 比較演算子トークン（`> < >= <= =`）を消費して [`BinOp`] へ写像する
    /// （TASK-79・SQL-9）。
    fn expect_cmp_op(&mut self) -> Result<BinOp, SqlSurfaceError> {
        match self.advance() {
            Some(Token::Punct('>')) => Ok(BinOp::Gt),
            Some(Token::Punct('<')) => Ok(BinOp::Lt),
            Some(Token::Ge) => Ok(BinOp::Ge),
            Some(Token::Le) => Ok(BinOp::Le),
            Some(Token::Punct('=')) => Ok(BinOp::Eq),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected comparison operator, got {other:?}"
            ))),
        }
    }

    /// 再帰深さ上限（[`MAX_EXPR_DEPTH`]）を検査する。構文解析自体の再帰段数を
    /// 制限することで、深いネスト入力によるスタック消費を定数に抑える
    /// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    fn check_expr_depth(&self, depth: usize) -> Result<(), SqlSurfaceError> {
        if depth > MAX_EXPR_DEPTH {
            return Err(SqlSurfaceError::payload_too_large(
                "expression nesting exceeds the allowed depth",
            ));
        }
        Ok(())
    }

    /// 式文法（`add → mul → primary`）の入口。`CREATE FUNCTION` の本体・SELECT の
    /// 式項目・WHERE 式述語の両辺・関数呼び出しの引数のいずれからも共通で使う
    /// （TASK-79・SQL-9）。
    fn parse_value_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        self.check_expr_depth(depth)?;
        self.parse_add_expr(depth)
    }

    fn parse_add_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        let mut lhs = self.parse_mul_expr(depth + 1)?;
        loop {
            let op = match self.peek() {
                Some(Token::Punct('+')) => BinOp::Add,
                Some(Token::Punct('-')) => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul_expr(depth + 1)?;
            self.consume_expr_node()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        let mut lhs = self.parse_primary_expr(depth + 1)?;
        loop {
            let op = match self.peek() {
                Some(Token::Punct('*')) => BinOp::Mul,
                Some(Token::Punct('/')) => BinOp::Div,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_primary_expr(depth + 1)?;
            self.consume_expr_node()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `primary := number | ident | ident '(' [expr {',' expr}] ')' | '(' expr ')'`。
    fn parse_primary_expr(&mut self, depth: usize) -> Result<Expr, SqlSurfaceError> {
        self.check_expr_depth(depth)?;
        match self.peek().cloned() {
            Some(Token::Number(n)) => {
                self.advance();
                self.consume_expr_node()?;
                Ok(Expr::Number(n))
            }
            Some(Token::Punct('(')) => {
                self.advance();
                let inner = self.parse_value_expr(depth + 1)?;
                self.expect_punct(')')?;
                Ok(inner)
            }
            Some(Token::Ident(name)) => {
                self.advance();
                if matches!(self.peek(), Some(Token::Punct('('))) {
                    self.parse_call_expr(name, depth)
                } else {
                    self.consume_expr_node()?;
                    Ok(Expr::Ident(name))
                }
            }
            other => Err(SqlSurfaceError::unsupported(format!(
                "unsupported expression term near {other:?}"
            ))),
        }
    }

    /// 関数呼び出し式 `<name> '(' [expr {',' expr}] ')'` を解析する。呼び出し元は
    /// 直前に `name` を消費済みで、次のトークンが `'('` である前提（`peek` 済み）。
    ///
    /// TASK-166（SQL-13）: 集計関数名（[`is_aggregate_function_name`]）はここでは
    /// 常に拒否する（`42601`）。集計関数の頂点呼び出しは
    /// [`Parser::parse_aggregate_item`] が本メソッドを経由せず直接消費するため、
    /// この経路に到達する集計名はすべて「集計項目の頂点以外」（SELECT の非集計項目・
    /// WHERE 式述語・`CREATE FUNCTION` 本体・集計引数のネスト呼び出し）での出現であり、
    /// いずれも許可形状外（`GROUP BY` を持たない集計のみを SQL-13 の受理形とする）。
    fn parse_call_expr(&mut self, name: String, depth: usize) -> Result<Expr, SqlSurfaceError> {
        if is_aggregate_function_name(&name) {
            return Err(SqlSurfaceError::unsupported(format!(
                "aggregate function {name} is only allowed as a top-level SELECT item"
            )));
        }
        self.expect_punct('(')?;
        let mut args = Vec::new();
        if !matches!(self.peek(), Some(Token::Punct(')'))) {
            args.push(self.parse_value_expr(depth + 1)?);
            while matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                if args.len() >= MAX_CALL_ARGS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many call arguments",
                    ));
                }
                args.push(self.parse_value_expr(depth + 1)?);
            }
        }
        self.expect_punct(')')?;
        self.consume_expr_node()?;
        Ok(Expr::Call { name, args })
    }

    /// ORDER BY 式の許可形状（距離演算子形または関数呼び出し形）。関数呼び出し形は
    /// 許可された名前（[`is_allowed_order_by_function_name`]）のみを受理し、
    /// 未知の名前は拒否する。
    fn parse_order_by(&mut self) -> Result<OrderByForm, SqlSurfaceError> {
        let name = self.expect_ident()?;
        match self.peek() {
            Some(Token::DistanceOp) => {
                self.advance();
                let literal = self.expect_string_literal()?;
                Ok(OrderByForm::Distance {
                    column: name,
                    literal,
                })
            }
            Some(Token::Punct('(')) => {
                if !is_allowed_order_by_function_name(&name) {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "unsupported ORDER BY function: {name}"
                    )));
                }
                self.advance();
                let args = self.parse_order_by_function_args(&name)?;
                self.expect_punct(')')?;
                Ok(OrderByForm::FunctionCall { name, args })
            }
            other => Err(SqlSurfaceError::unsupported(format!(
                "unsupported ORDER BY expression near {other:?}"
            ))),
        }
    }

    /// 許可された ORDER BY 関数ごとに、引数の個数・位置・トークン種別を明示的に
    /// 解析する。`name` は [`is_allowed_order_by_function_name`] を通過済みの
    /// 前提で呼ばれる。呼び出し元の `expect_punct(')')` が閉じ括弧を消費するため、
    /// ここでは許可した引数トークン列のみを消費し、過不足があれば
    /// （空引数・余剰引数・意味を解釈しない括弧グループを含め）その時点で拒否する
    /// （fail-closed）。
    ///
    /// `hybrid_rrf`/`HYBRID` は 2 引数形（`(<vec列>, '<query text>')`。TASK-74 で
    /// 受理済みの既存構造。マージ済み挙動を変えないため構造としては引き続き受理する）と
    /// 4 引数形（`(<vec列>, '<vec リテラル>', <text列>, '<query text>')`。TASK-75・
    /// SQL-4 の実行可能形）の両方を構造上受理する。実行可能かどうか（束縛成功）は
    /// `sql::parser::bind` が判定し、2 引数形は `SqlSurfaceError::InvalidInput`
    /// （`22000`。「実行不能」）で拒否する（advisor 方針: 既存の 2 引数形受理を壊さず
    /// 追加する）。
    fn parse_order_by_function_args(
        &mut self,
        name: &str,
    ) -> Result<Vec<FunctionArg>, SqlSurfaceError> {
        match name.to_ascii_uppercase().as_str() {
            "HYBRID_RRF" | "HYBRID" => {
                let mut args = Vec::new();
                args.push(FunctionArg::Ident(self.expect_ident()?));
                self.expect_punct(',')?;
                args.push(FunctionArg::StringLiteral(self.expect_string_literal()?));
                if matches!(self.peek(), Some(Token::Punct(','))) {
                    self.advance();
                    args.push(FunctionArg::Ident(self.expect_ident()?));
                    self.expect_punct(',')?;
                    args.push(FunctionArg::StringLiteral(self.expect_string_literal()?));
                }
                Ok(args)
            }
            other => Err(SqlSurfaceError::unsupported(format!(
                "unsupported ORDER BY function: {other}"
            ))),
        }
    }

    /// `LIMIT n` 直後の省略可能な文末専用句 `USING MODE '<literal>'`（TASK-161・
    /// SQL-12）。`USING` は字句解析段階のキーワードではなく `Ident` のため、
    /// [`Parser::peek_ident_matches`] で文脈的（この位置限定）に判定する。続かなければ
    /// 句なし（`Ok(None)`）として扱う。
    /// `USING` の直後は `MODE`（大文字小文字非区別の文脈識別子）のみ許可し、それ以外
    /// （`PLAN`・`OPERATION_ID` 等）は fail-closed に拒否する。**この位置**
    /// （`ORDER BY ... LIMIT n` の直後）に限った制約であり、`USING PLAN(...)` 自体は
    /// `ORDER BY` の代替として別の位置（`WHERE` 直後、[`Parser::
    /// parse_using_plan_clause`]）で受理する（TASK-77・SQL-5）。ここで `PLAN` を
    /// 拒否するのは、`LIMIT` 後は既に `USING PLAN` の受理位置を過ぎているため（両者
    /// 併用・非規範形 `USING PLAN 'x'`（括弧なし）はいずれもここで `42601` へ落ちる）。
    /// 句を高々 1 回だけ消費するため、2 回目以降の `USING MODE ...` は本メソッドでは
    /// なく後続の [`Parser::expect_end_of_statement`] が「余剰トークン」として拒否する。
    fn parse_using_clause(&mut self) -> Result<Option<String>, SqlSurfaceError> {
        if !self.peek_ident_matches("USING") {
            return Ok(None);
        }
        self.advance();
        let name = self.expect_ident()?;
        if !name.eq_ignore_ascii_case("MODE") {
            return Err(SqlSurfaceError::unsupported(format!(
                "unsupported USING clause: {name}"
            )));
        }
        let value = self.expect_string_literal()?;
        Ok(Some(value))
    }

    /// `ORDER BY` の代わりに置ける文末専用句 `USING PLAN('<query>')`（TASK-77・
    /// SQL-5）の構造パース。呼び出し元 [`parse_select_shape`] が `WHERE`（省略可）
    /// の直後で `USING` 識別子を先読みして本メソッドへ分岐した後に呼ぶ（`ORDER BY`
    /// 経路は必ずキーワード `ORDER` から始まるため、先読み 1 トークンで衝突なく
    /// 判定できる＝`USING PLAN` と `ORDER BY` は構文上相互排他）。
    ///
    /// 受理する規範形は `PLAN('<文字列リテラル>')`（**括弧必須**の関数呼び出し形）
    /// のみ。以下はすべて構造上の許可リスト外として `42601`（`SqlSurfaceError::
    /// unsupported`）で拒否する（fail-closed）:
    /// - `PLAN` 以外の識別子（`unsupported USING clause`）
    /// - 括弧を伴わない形（例: 非規範形の `USING PLAN 'x'`）
    /// - パラメータ形式 `USING PLAN($1)`（拡張クエリプロトコル対応後の将来形式。
    ///   `$` は字句解析段階で [`LexError`] となり、本メソッドへ到達する前に
    ///   `validate_sql` の `tokenize` 呼び出しが `42601` へ写像する）
    /// - 文字列リテラル以外の引数・複数引数
    ///
    /// 空リテラルは [`SqlSurfaceError::invalid_input`]（`22000`）、
    /// [`MAX_USING_PLAN_LEN`] 超過は [`SqlSurfaceError::payload_too_large`]
    /// （`54000`）で拒否する。
    fn parse_using_plan_clause(&mut self) -> Result<String, SqlSurfaceError> {
        self.advance(); // `USING`（呼び出し元が `peek_ident_matches("USING")` で確認済み）
        let name = self.expect_ident()?;
        if !name.eq_ignore_ascii_case("PLAN") {
            return Err(SqlSurfaceError::unsupported(format!(
                "unsupported USING clause: {name}"
            )));
        }
        self.expect_punct('(')?;
        let value = self.expect_string_literal()?;
        self.expect_punct(')')?;
        if value.is_empty() {
            return Err(SqlSurfaceError::invalid_input(
                "USING PLAN value must not be empty",
            ));
        }
        if value.len() > MAX_USING_PLAN_LEN {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "USING PLAN value length {} exceeds limit {MAX_USING_PLAN_LEN}",
                value.len()
            )));
        }
        Ok(value)
    }

    /// `LIMIT <n>` の直後・文末に 1 箇所だけ許可する `HINT ORDER(<段>, <段>, <段>)`
    /// を解析する（TASK-76・SQL-7）。`HINT` は予約語化せず、この位置でのみ文脈依存で
    /// 認識する（次のトークンが識別子 `HINT`（大文字小文字不問）かつその次が
    /// `ORDER` キーワードの場合のみ消費する）。それ以外（`hint` を列名・テーブル名等の
    /// 通常の識別子として使う既存 SQL を含む）は何も消費せず `None` を返し、
    /// `HINT ORDER` 自体が省略可能な文法として扱う（公開 API・エラー契約の互換性、
    /// AGENTS.md P1）。段名は [`plan::parse_stage_name`] で識別子トークンを閉じた
    /// [`Stage`] へ写像し、未知の名前・個数不正・重複は許可リスト外として拒否する
    /// （`Stage`/`EvaluationOrder` を経由するため、パーサーを迂回しても不完全な順序が
    /// 下流へ渡らない）。
    fn parse_hint_order(&mut self) -> Result<Option<EvaluationOrder>, SqlSurfaceError> {
        let is_hint_ident =
            matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("HINT"));
        if !is_hint_ident
            || !matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Keyword(Keyword::Order))
            )
        {
            return Ok(None);
        }
        self.advance();
        self.expect_keyword(Keyword::Order)?;
        self.expect_punct('(')?;

        let mut stages: Vec<Stage> = Vec::new();
        loop {
            let name = self.expect_ident()?;
            let stage = plan::parse_stage_name(&name).ok_or_else(|| {
                SqlSurfaceError::unsupported(format!("unsupported HINT ORDER stage: {name}"))
            })?;
            stages.push(stage);
            if matches!(self.peek(), Some(Token::Punct(','))) {
                self.advance();
                continue;
            }
            break;
        }
        self.expect_punct(')')?;

        let order = EvaluationOrder::try_from_stages(&stages).map_err(|e| {
            SqlSurfaceError::unsupported(format!("invalid HINT ORDER permutation: {e:?}"))
        })?;
        Ok(Some(order))
    }

    /// 省略可能な単一末尾セミコロンの後に余剰トークンがあれば複数 statement と
    /// みなして拒否する。
    fn expect_end_of_statement(&mut self) -> Result<(), SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct(';'))) {
            self.advance();
        }
        if self.peek().is_some() {
            return Err(SqlSurfaceError::unsupported(
                "unexpected trailing tokens after statement (multiple statements are not supported)",
            ));
        }
        Ok(())
    }

    /// VALUES リストの 1 要素（文字列リテラルまたは数値リテラルのみ。関数呼び出し・
    /// 括弧・`NULL` キーワード等は許可リスト外）。
    fn expect_literal(&mut self) -> Result<InsertLiteral, SqlSurfaceError> {
        match self.advance() {
            Some(Token::StringLiteral(s)) => Ok(InsertLiteral::String(s.clone())),
            Some(Token::Number(n)) => Ok(InsertLiteral::Number(n.clone())),
            other => Err(SqlSurfaceError::unsupported(format!(
                "expected literal value, got {other:?}"
            ))),
        }
    }

    /// `INSERT INTO <table> (<col>[, <col>]*) VALUES (<lit>[, <lit>]*)
    /// USING OPERATION_ID '<id>' [;]` の単一行形のみを受理する（SQL-10、TASK-80）。
    /// 複数行 VALUES・RETURNING・可視性ラベル指定は構造的に受理しない。
    fn parse_insert(&mut self) -> Result<ParsedInsertShape, SqlSurfaceError> {
        self.expect_contextual_keyword("INSERT")?;
        self.expect_contextual_keyword("INTO")?;
        let table_name = self.expect_ident()?;

        self.expect_punct('(')?;
        let mut columns = vec![self.expect_ident()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            if columns.len() >= MAX_INSERT_COLUMNS {
                return Err(SqlSurfaceError::unsupported("too many INSERT columns"));
            }
            columns.push(self.expect_ident()?);
        }
        self.expect_punct(')')?;

        self.expect_contextual_keyword("VALUES")?;
        self.expect_punct('(')?;
        let mut values = vec![self.expect_literal()?];
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            if values.len() >= MAX_INSERT_COLUMNS {
                return Err(SqlSurfaceError::unsupported("too many INSERT values"));
            }
            values.push(self.expect_literal()?);
        }
        self.expect_punct(')')?;

        if columns.len() != values.len() {
            return Err(SqlSurfaceError::unsupported(format!(
                "INSERT column count {} does not match value count {}",
                columns.len(),
                values.len()
            )));
        }

        // 文末専用句の構造パースのみをここで行う（省略・明示 `NULL` はいずれも
        // `None`）。必須化の判定（`23502`）は `validate_insert` が
        // `LedgerMode::require` へ委譲し、この時点でまだ FROM/INTO テーブルの
        // カタログ照会を一切行っていない＝書き込みトランザクションは絶対に
        // 開始されていない段階で行われる（TASK-92・RECOVER-1）。
        let operation_id = self.parse_operation_id_clause()?;

        Ok(ParsedInsertShape {
            table_name,
            columns,
            values,
            operation_id,
        })
    }

    /// 文末専用句 `USING OPERATION_ID '<id>'`（SQL-10、TASK-80）の構造パースのみを
    /// 行う（値の意味論的検証は [`OperationId::parse`]）。句の省略・明示
    /// `USING OPERATION_ID NULL`（大小無視。字句解析上は `Token::Ident("NULL")`）は
    /// いずれも `Ok(None)` として返し、`23502` への判定はここでは行わない
    /// （TASK-92・RECOVER-1: 必須化の可否はサーバー構成 `LedgerMode` が決める。
    /// 呼び出し元 [`validate_insert`] が `LedgerMode::require` へ委譲する）。`USING` の
    /// 後に `OPERATION_ID` キーワードが続かない形（`$n` プレースホルダ由来の字句解析
    /// 拒否を含む）・`OPERATION_ID` に文字列リテラルでも `NULL` でもない形
    /// （数値・他の識別子等）は許可リスト外として `42601` へ落ちる
    /// （`expect_contextual_keyword`/`expect_string_literal` が `UnsupportedSyntax` を
    /// 返す）。
    fn parse_operation_id_clause(&mut self) -> Result<Option<OperationId>, SqlSurfaceError> {
        if self.peek_contextual_keyword("USING") {
            self.advance();
            self.expect_contextual_keyword("OPERATION_ID")?;
            if self.peek_contextual_keyword("NULL") {
                self.advance();
                return Ok(None);
            }
            let raw = self.expect_string_literal()?;
            OperationId::parse(&raw).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// 構文木（[`ValidatedStatement`] の元）。カタログ存在確認前の中間結果。
struct ParsedShape {
    table_name: String,
    projection: Projection,
    where_predicates: Vec<WherePredicate>,
    order_by: OrderByForm,
    limit: u32,
    search_mode: Option<String>,
    evaluation_order: EvaluationOrder,
    /// `USING PLAN('<query>')`（TASK-77・SQL-5）。`Some` のとき `order_by` は必ず
    /// [`OrderByForm::UsingPlan`]。
    using_plan: Option<String>,
}

/// 許可した `SELECT` statement 形状を先頭から再帰下降で判定する（TASK-74 由来。
/// TASK-161 で `LIMIT` 直後の `USING MODE` 句判定を追加した。TASK-77・SQL-5 で
/// `WHERE`（省略可）直後の `USING PLAN(...)` 分岐を追加した）。
fn parse_select_shape(tokens: &[Token]) -> Result<ParsedShape, SqlSurfaceError> {
    let mut p = Parser::new(tokens);

    p.expect_keyword(Keyword::Select)?;
    let projection = p.parse_select_list()?;
    p.expect_keyword(Keyword::From)?;
    let table_name = p.expect_ident()?;

    let where_predicates = if matches!(p.peek(), Some(Token::Keyword(Keyword::Where))) {
        p.advance();
        p.parse_where()?
    } else {
        Vec::new()
    };

    // TASK-77・SQL-5: `USING PLAN(...)` は `ORDER BY` の代替経路（相互排他）。
    // `ORDER BY` 経路は必ずキーワード `ORDER` から始まるため、この位置で文脈的
    // 識別子 `USING` が現れるかどうかだけで両者を衝突なく判定できる。
    if p.peek_ident_matches("USING") {
        let using_plan = p.parse_using_plan_clause()?;

        p.expect_keyword(Keyword::Limit)?;
        let limit_str = p.expect_number()?;
        let limit: u32 = limit_str.parse().map_err(|_| {
            SqlSurfaceError::unsupported(format!("malformed LIMIT value: {limit_str}"))
        })?;

        // `HINT ORDER(...)` はランキング段順のヒントであり、ランキング自体を
        // `USING PLAN` の展開結果が決める本経路では意味を持たない。構造上も
        // 受理しない（`ORDER BY` 経路の既存文法を変えず、`USING PLAN` 側だけを
        // 素通しで「`USING MODE` のみ許容」に保つ）。
        let search_mode = p.parse_using_clause()?;

        p.expect_end_of_statement()?;

        return Ok(ParsedShape {
            table_name,
            projection,
            where_predicates,
            order_by: OrderByForm::UsingPlan,
            limit,
            search_mode,
            evaluation_order: EvaluationOrder::DEFAULT,
            using_plan: Some(using_plan),
        });
    }

    p.expect_keyword(Keyword::Order)?;
    p.expect_keyword(Keyword::By)?;
    let order_by = p.parse_order_by()?;

    p.expect_keyword(Keyword::Limit)?;
    let limit_str = p.expect_number()?;
    let limit: u32 = limit_str
        .parse()
        .map_err(|_| SqlSurfaceError::unsupported(format!("malformed LIMIT value: {limit_str}")))?;

    let evaluation_order = p.parse_hint_order()?.unwrap_or(EvaluationOrder::DEFAULT);
    let search_mode = p.parse_using_clause()?;

    p.expect_end_of_statement()?;

    Ok(ParsedShape {
        table_name,
        projection,
        where_predicates,
        order_by,
        limit,
        search_mode,
        evaluation_order,
        using_plan: None,
    })
}

/// 構文木（[`ValidatedAggregate`] の元）。カタログ存在確認前の中間結果
/// （TASK-166・SQL-13。TASK-167・SQL-14 で `group_by` を追加）。
struct ParsedAggregateShape {
    table_name: String,
    items: Vec<AggregateSelectItem>,
    where_predicates: Vec<WherePredicate>,
    group_by: Option<GroupByClause>,
}

/// 許可した集計 `SELECT` statement 形状を先頭から再帰下降で判定する（TASK-166・
/// SQL-13。TASK-167・SQL-14 で `GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT` を追加）。
/// `HINT ORDER`／`USING MODE` はいずれの形でも受理しない（集計結果は取得モードの
/// 余地を持たない）。呼び出し元（[`validate_sql`]）は先頭 2 トークンが集計関数名
/// `'('` であるか、`SELECT ... GROUP BY` の並びを含むかのいずれかを確認済みの
/// 前提で呼ぶ。
fn parse_aggregate_shape(tokens: &[Token]) -> Result<ParsedAggregateShape, SqlSurfaceError> {
    let mut p = Parser::new(tokens);

    p.expect_keyword(Keyword::Select)?;
    let mut items = vec![p.parse_aggregate_select_item()?];
    while matches!(p.peek(), Some(Token::Punct(','))) {
        if items.len() >= MAX_AGGREGATE_ITEMS {
            return Err(SqlSurfaceError::payload_too_large(
                "too many aggregate items",
            ));
        }
        p.advance();
        items.push(p.parse_aggregate_select_item()?);
    }
    // SELECT リストに集計項目が 1 つも無い（`GroupKey` のみ、いわゆる
    // `SELECT DISTINCT` 相当）形は許可しない（TASK-167・SQL-14 の受理形は集計
    // 結果を持つ行のみを対象とする）。
    if !items
        .iter()
        .any(|i| matches!(i, AggregateSelectItem::Aggregate(_)))
    {
        return Err(SqlSurfaceError::unsupported(
            "aggregate SELECT list requires at least one aggregate item",
        ));
    }
    p.expect_keyword(Keyword::From)?;
    let table_name = p.expect_ident()?;

    let where_predicates = if matches!(p.peek(), Some(Token::Keyword(Keyword::Where))) {
        p.advance();
        p.parse_where()?
    } else {
        Vec::new()
    };

    let has_group_by =
        matches!(p.peek(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("GROUP"));
    let group_by = if has_group_by {
        let column = p.parse_group_by_clause()?;
        // SELECT リストの `GroupKey` 項目は `GROUP BY` 列と同名でなければならない
        // （§計画 3.1）。不一致・`GROUP BY` 句を持たない `GroupKey` 項目（下の
        // `else` 分岐）はいずれも許可リスト外として `42601` に落とす。
        for item in &items {
            if let AggregateSelectItem::GroupKey { column: c, .. } = item {
                if c != &column {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "SELECT list bare identifier {c:?} does not match GROUP BY column {column:?}"
                    )));
                }
            }
        }
        let having = if matches!(p.peek(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("HAVING"))
        {
            p.parse_having()?
        } else {
            Vec::new()
        };
        let order_by = if matches!(p.peek(), Some(Token::Keyword(Keyword::Order))) {
            Some(p.parse_aggregate_order_by()?)
        } else {
            None
        };
        let limit = if matches!(p.peek(), Some(Token::Keyword(Keyword::Limit))) {
            Some(p.parse_aggregate_limit()?)
        } else {
            None
        };
        Some(GroupByClause {
            column,
            having,
            order_by,
            limit,
        })
    } else {
        // `GROUP BY` 句が無いのに SELECT リストへ裸の識別子（`GroupKey` 候補）が
        // 混在する形（例: `SELECT lang, COUNT(*) FROM t`）は許可しない。
        if items
            .iter()
            .any(|i| matches!(i, AggregateSelectItem::GroupKey { .. }))
        {
            return Err(SqlSurfaceError::unsupported(
                "bare column reference in aggregate SELECT list requires GROUP BY",
            ));
        }
        None
    };

    p.expect_end_of_statement()?;

    Ok(ParsedAggregateShape {
        table_name,
        items,
        where_predicates,
        group_by,
    })
}

/// `SET search_mode = '<literal>'`（TASK-161・SQL-12）の許可形状。規範形は
/// `=` ＋ 文字列リテラルの完全一致のみ（`TO` 形・非引用値・`RESET`/`SHOW` 等の緩和は
/// SQL-12 に規範がないため、本実装は最も厳格な形に倒す。緩和は spec 側の判断事項）。
/// 変数名 `search_mode` は大文字小文字を区別せず照合する。
fn parse_set_search_mode(tokens: &[Token]) -> Result<String, SqlSurfaceError> {
    let mut p = Parser::new(tokens);

    p.expect_ident_matching("SET")?;
    let name = p.expect_ident()?;
    if !name.eq_ignore_ascii_case("search_mode") {
        return Err(SqlSurfaceError::unsupported(format!(
            "unsupported SET variable: {name}"
        )));
    }
    p.expect_punct('=')?;
    let value = p.expect_string_literal()?;
    p.expect_end_of_statement()?;

    Ok(value)
}

/// `CREATE FUNCTION <name>(<param>[, <param>...]) AS <expr> [;]`（TASK-79・SQL-9）の
/// 許可形状。`CREATE`／`FUNCTION`／`AS` は `SET`・`USING` と同方針で予約語化せず、
/// statement 先頭・所定位置でのみ文脈的に照合する（既存の列名・テーブル名として
/// これらの語を使う SQL を破壊しない）。パラメータ数は構造検証段階でも
/// [`MAX_UDF_PARAMS`] を超えないことを確認する（意味論検証は
/// `sql::udf_call::define_function` が担うが、アロケーション前の上限検証は
/// `.claude/rules/security.md`「長さフィールドは上限検証してからアロケーションに
/// 使う」に従い構造検証段階でも行う）。
fn parse_create_function(tokens: &[Token]) -> Result<(String, Vec<String>, Expr), SqlSurfaceError> {
    let mut p = Parser::new(tokens);
    p.expect_ident_matching("CREATE")?;
    p.expect_ident_matching("FUNCTION")?;
    let name = p.expect_ident()?;
    p.expect_punct('(')?;
    let mut params = Vec::new();
    if !matches!(p.peek(), Some(Token::Punct(')'))) {
        params.push(p.expect_ident()?);
        while matches!(p.peek(), Some(Token::Punct(','))) {
            p.advance();
            if params.len() >= MAX_UDF_PARAMS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many function parameters",
                ));
            }
            params.push(p.expect_ident()?);
        }
    }
    p.expect_punct(')')?;
    p.expect_ident_matching("AS")?;
    let body = p.parse_value_expr(0)?;
    p.expect_end_of_statement()?;
    Ok((name, params, body))
}

/// SQL 文をトークン化し、許可リスト形式で構造検証する（TASK-161 の公開 API。
/// TASK-74 の `validate_statement` を `SELECT`／`SET search_mode`／
/// `CREATE FUNCTION`（TASK-79・SQL-9）の 3 statement 種別へ拡張したもの）。先頭
/// トークンで statement 種別を判定し、`SELECT` のみ `lookup` を通じて FROM テーブルの
/// カタログ存在確認まで行う（`SET`・`CREATE FUNCTION` はカタログ照会を要しない）。
///
/// 検証順序（決定的。同一入力には常に同一の [`SqlSurfaceError`] を返す）:
/// 1. 字句解析（入力長・トークン数上限を含む。失敗は [`SqlSurfaceError::UnsupportedSyntax`]）
/// 2. 構造の許可リスト判定（失敗は `UnsupportedSyntax`）
/// 3. `SELECT` の場合のみ、FROM 単一テーブルのカタログ存在確認
///    （不存在は [`SqlSurfaceError::UndefinedTable`]）
pub fn validate_sql(sql: &str, lookup: &impl TableLookup) -> Result<Statement, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    // `SET`・`CREATE` は字句解析段階のキーワードではなく `Ident` のため
    // （TASK-161・SQL-12 修正と同方針）、statement 先頭という文脈でのみ大文字小文字を
    // 区別せず判定する。
    let is_set_statement =
        matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("SET"));
    let is_create_function_statement =
        matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("CREATE"));
    // `EXPLAIN` も `SET`・`CREATE` と同方針（字句解析段階のキーワードにせず、
    // statement 先頭という文脈でのみ大文字小文字を区別せず判定する。TASK-78・SQL-6）。
    let is_explain_statement =
        matches!(tokens.first(), Some(Token::Ident(name)) if name.eq_ignore_ascii_case("EXPLAIN"));
    // TASK-166（SQL-13）: `SELECT` の直後（2 番目・3 番目のトークン）が
    // 集計関数名 `'('` なら集計 SELECT 形状（[`parse_aggregate_shape`]）へ、それ
    // 以外は既存の検索 SELECT 形状（[`parse_select_shape`]）へ分岐する。バック
    // トラックせず先読みだけで確定させる（`Parser::pos` の巻き戻しに依存しない）。
    // TASK-167（SQL-14）: トークン列中に文脈キーワード `GROUP` → `BY` の並びが
    // あれば、集計項目が SELECT リストの先頭に来ない形（`SELECT <col>, <agg>(...)
    // FROM t GROUP BY <col>`）も集計 SELECT 形状へ振り分ける。`GROUP`/`BY` は
    // どちらも他の文脈で通常の識別子・既存の `ORDER BY` の一部として現れうるが、
    // 「`Ident("GROUP")` の直後に `Keyword::By`」という並びは既存の許可形状には
    // 存在しないため、フォールス・ポジティブなく集計形状の目印として使える。
    let contains_group_by = tokens.windows(2).any(|w| {
        matches!(&w[0], Token::Ident(name) if name.eq_ignore_ascii_case("GROUP"))
            && matches!(w[1], Token::Keyword(Keyword::By))
    });
    let is_aggregate_select = matches!(tokens.first(), Some(Token::Keyword(Keyword::Select)))
        && ((matches!(tokens.get(1), Some(Token::Ident(name)) if is_aggregate_function_name(name))
            && matches!(tokens.get(2), Some(Token::Punct('('))))
            || contains_group_by);
    match tokens.first() {
        Some(Token::Keyword(Keyword::Select)) if is_aggregate_select => {
            let shape = parse_aggregate_shape(&tokens)?;
            let exists = lookup.table_exists(&shape.table_name)?;
            if !exists {
                return Err(SqlSurfaceError::undefined_table(shape.table_name));
            }
            Ok(Statement::Aggregate(ValidatedAggregate {
                table_name: shape.table_name,
                items: shape.items,
                where_predicates: shape.where_predicates,
                group_by: shape.group_by,
            }))
        }
        Some(Token::Keyword(Keyword::Select)) => {
            let shape = parse_select_shape(&tokens)?;
            let exists = lookup.table_exists(&shape.table_name)?;
            if !exists {
                return Err(SqlSurfaceError::undefined_table(shape.table_name));
            }
            Ok(Statement::Select(ValidatedStatement {
                table_name: shape.table_name,
                projection: shape.projection,
                order_by: shape.order_by,
                where_predicates: shape.where_predicates,
                limit: shape.limit,
                search_mode: shape.search_mode,
                evaluation_order: shape.evaluation_order,
                using_plan: shape.using_plan,
            }))
        }
        _ if is_set_statement => {
            let value = parse_set_search_mode(&tokens)?;
            Ok(Statement::SetSearchMode { value })
        }
        _ if is_create_function_statement => {
            let (name, params, body) = parse_create_function(&tokens)?;
            Ok(Statement::CreateFunction { name, params, body })
        }
        // TASK-78（SQL-6）: `EXPLAIN` は「`USING PLAN` を伴う検索 SELECT」の前置
        // のみを受理する（fail-closed。将来の拡張は別タスクの管轄）。先頭の
        // `EXPLAIN` トークンを消費した残りを既存の検索 SELECT 形状パーサー
        // （[`parse_select_shape`]）へそのまま渡し、`USING PLAN` を含まない形
        // （通常 SELECT・`ORDER BY` 経路）は `shape.using_plan` が `None` になる
        // ことを利用して一律 `42601` へ落とす（`SET`・`CREATE FUNCTION` への
        // 前置は残り先頭が `SELECT` キーワードでないため、同じ `42601` へ自然に
        // 落ちる）。
        //
        // 集計 SELECT（TASK-166・SQL-13／TASK-167・SQL-14）は非 EXPLAIN 経路では
        // `is_aggregate_select` の先読みで `parse_aggregate_shape` へ振り分けられ
        // `parse_select_shape` には到達しないが、この分岐は残りトークンを無条件に
        // `parse_select_shape` へ渡すため、同じ先読みを適用しないと内側の
        // `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` が集計ではなく UDF 呼び出しの検索射影
        // として誤って受理されうる（Issue #267 Bugbot 指摘）。`EXPLAIN` に集計
        // SELECT の対応契約は無い（`ValidatedAggregate` に `using_plan` は無く
        // `USING PLAN` と両立しない）ため、`is_aggregate_select` と同じ先読みを
        // 残りトークンに適用し、集計形状に見える場合は fail-closed で拒否する。
        _ if is_explain_statement => {
            let rest = &tokens[1..];
            if !matches!(rest.first(), Some(Token::Keyword(Keyword::Select))) {
                return Err(SqlSurfaceError::unsupported(
                    "EXPLAIN requires a SELECT ... USING PLAN(...) statement",
                ));
            }
            let rest_contains_group_by = rest.windows(2).any(|w| {
                matches!(&w[0], Token::Ident(name) if name.eq_ignore_ascii_case("GROUP"))
                    && matches!(w[1], Token::Keyword(Keyword::By))
            });
            let rest_is_aggregate_select = (matches!(rest.get(1), Some(Token::Ident(name)) if is_aggregate_function_name(name))
                && matches!(rest.get(2), Some(Token::Punct('('))))
                || rest_contains_group_by;
            if rest_is_aggregate_select {
                return Err(SqlSurfaceError::unsupported(
                    "EXPLAIN is not supported for aggregate SELECT statements",
                ));
            }
            let shape = parse_select_shape(rest)?;
            if shape.using_plan.is_none() {
                return Err(SqlSurfaceError::unsupported(
                    "EXPLAIN is only supported for SELECT ... USING PLAN(...) statements",
                ));
            }
            let exists = lookup.table_exists(&shape.table_name)?;
            if !exists {
                return Err(SqlSurfaceError::undefined_table(shape.table_name));
            }
            Ok(Statement::Explain(ValidatedStatement {
                table_name: shape.table_name,
                projection: shape.projection,
                order_by: shape.order_by,
                where_predicates: shape.where_predicates,
                limit: shape.limit,
                search_mode: shape.search_mode,
                evaluation_order: shape.evaluation_order,
                using_plan: shape.using_plan,
            }))
        }
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected SELECT, SET, CREATE FUNCTION, or EXPLAIN, got {other:?}"
        ))),
    }
}

/// `SELECT` 文のみを受理する後方互換 API（TASK-74・TASK-75 が既に依存している
/// シグネチャを維持する）。[`validate_sql`]（TASK-161）へ委譲し、`SELECT` 以外
/// （`SET search_mode` 等）は「このエントリポイントでは受理しない statement 形」
/// として `42601` で拒否する（`SET` のリテラル値自体が妥当でも、それを保持する
/// セッションを持たないこのエントリポイントでは意味を持たないため。黙った
/// no-op にはしない）。
pub fn validate_statement(
    sql: &str,
    lookup: &impl TableLookup,
) -> Result<ValidatedStatement, SqlSurfaceError> {
    match validate_sql(sql, lookup)? {
        Statement::Select(stmt) => Ok(stmt),
        Statement::SetSearchMode { .. } => Err(SqlSurfaceError::unsupported(
            "SET is not a query statement (use a session-aware entry point)",
        )),
        Statement::CreateFunction { .. } => Err(SqlSurfaceError::unsupported(
            "CREATE FUNCTION is not a query statement (use a session-aware entry point)",
        )),
        // TASK-166（SQL-13）: 集計 SELECT は `ValidatedStatement`（検索 SELECT 専用の
        // 形）を持たないため、このエントリポイントでは受理しない（`SET`・
        // `CREATE FUNCTION` と同じ「このエントリポイントでは非対応」の一律 `42601`）。
        Statement::Aggregate(_) => Err(SqlSurfaceError::unsupported(
            "aggregate SELECT is not a search query statement (use a session-aware entry point)",
        )),
        // TASK-78（SQL-6）: `EXPLAIN` は `ValidatedStatement` を包んで返すものの、
        // 「検索本体を実行しない」という別の実行契約を持つため、`SET`・
        // `CREATE FUNCTION`・`Aggregate` と同じくこのセッションなしエントリ
        // ポイントでは受理しない（一律 `42601`）。
        Statement::Explain(_) => Err(SqlSurfaceError::unsupported(
            "EXPLAIN is not a search query statement (use a session-aware entry point)",
        )),
    }
}

/// 構文木（[`ValidatedInsert`] の元）。カタログ存在確認前の中間結果（SQL-10、TASK-80）。
struct ParsedInsertShape {
    table_name: String,
    columns: Vec<String>,
    values: Vec<InsertLiteral>,
    operation_id: Option<OperationId>,
}

/// INSERT 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を通じて
/// INTO テーブルがカタログに実在するかを確認する（SQL-10、TASK-80 の公開 API）。
/// `validate_statement`（SELECT 専用、TASK-74）とは独立したエントリポイントとする
/// （SELECT 文に `USING OPERATION_ID` を付けた入力は `validate_statement` 側の
/// `expect_end_of_statement` が余剰トークンとして `42601` で拒否するため、
/// SELECT/INSERT を誤って混同受理する経路は構造的に存在しない）。
///
/// 検証順序は決定的（同一入力には常に同一の [`SqlSurfaceError`] を返す）。
/// `operation_id` 必須化ガード（`mode.require`。TASK-92・RECOVER-1）を含む段階構成の
/// 詳細は `recovery::required_op_id` モジュールドキュメント参照。
pub fn validate_insert(
    sql: &str,
    lookup: &impl TableLookup,
    mode: LedgerMode,
) -> Result<ValidatedInsert, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    let mut p = Parser::new(&tokens);
    let shape = p.parse_insert()?;
    p.expect_end_of_statement()?;

    mode.require(shape.operation_id.as_ref())?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(ValidatedInsert {
        table_name: shape.table_name,
        columns: shape.columns,
        values: shape.values,
        operation_id: shape.operation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// storage 非依存の単体テスト用フェイク（`Storage` を必要としないための軽量抽象）。
    struct FakeCatalog {
        tables: HashSet<&'static str>,
    }

    impl TableLookup for FakeCatalog {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(self.tables.contains(name))
        }
    }

    struct FailingCatalog;
    impl TableLookup for FailingCatalog {
        fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
            Err(SqlSurfaceError::Internal {
                detail: "simulated backend failure".to_string(),
            })
        }
    }

    // codex-review P0・PR #210 指摘の再発防止: `Internal`（`wire_code() ==
    // "XX000"`）の `client_message()` は redb I/O エラー等の内部詳細
    // （`detail`）を一切含まない固定文言へ丸めること。`wire-server::simple_query`
    // は `to_string()` ではなく必ずこちらを使う契約（security.md P0）。
    #[test]
    fn internal_error_client_message_does_not_leak_detail() {
        let err = SqlSurfaceError::Internal {
            detail: "redb I/O error: disk quota exceeded at /var/lib/vector-db/data.redb"
                .to_string(),
        };
        assert_eq!(err.wire_code(), "XX000");
        assert_eq!(err.client_message(), "internal error");
        assert!(!err.client_message().contains("redb"));
        assert!(!err.client_message().contains("disk quota"));
    }

    // 対照確認: `Internal` 以外の variant は通常の `Display` 文言をそのまま
    // `client_message()` として返す（各コンストラクタで既に切り詰め・一般化
    // 済みのため、丸め不要）。
    #[test]
    fn non_internal_error_client_message_matches_display() {
        let err = SqlSurfaceError::UndefinedTable {
            name: "ghost_table".to_string(),
        };
        assert_eq!(err.client_message(), err.to_string());
        assert!(err.client_message().contains("ghost_table"));
    }

    fn catalog_with(tables: &[&'static str]) -> FakeCatalog {
        FakeCatalog {
            tables: tables.iter().copied().collect(),
        }
    }

    // --- 受理系（許可した SQL 表層の構造判定通過） -------------------------

    #[test]
    fn accepts_basic_select_with_order_by_distance() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2]' LIMIT 20",
            &lookup,
        )
        .expect("basic shape should be accepted");
        assert_eq!(stmt.table_name, "documents");
        assert!(stmt.where_predicates.is_empty());
        assert_eq!(stmt.limit, 20);
        assert_eq!(
            stmt.order_by,
            OrderByForm::Distance {
                column: "embedding".to_string(),
                literal: "[0.1,0.2]".to_string(),
            }
        );
    }

    #[test]
    fn accepts_select_with_where_equality() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("WHERE equality shape should be accepted");
        assert_eq!(
            stmt.where_predicates,
            vec![WherePredicate::Equality {
                column: "lang".to_string(),
                value: "ja".to_string(),
            }]
        );
    }

    #[test]
    fn accepts_select_with_where_predicate_call() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE visible() ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("WHERE predicate-call shape should be accepted");
        assert_eq!(
            stmt.where_predicates,
            vec![WherePredicate::PredicateCall {
                name: "visible".to_string()
            }]
        );
    }

    #[test]
    fn accepts_select_with_combined_where_predicates() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE lang = 'ja' AND visible() ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("combined WHERE predicates should be accepted");
        assert_eq!(
            stmt.where_predicates,
            vec![
                WherePredicate::Equality {
                    column: "lang".to_string(),
                    value: "ja".to_string(),
                },
                WherePredicate::PredicateCall {
                    name: "visible".to_string()
                },
            ]
        );
    }

    #[test]
    fn accepts_select_with_order_by_function_call() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("function-call shape should be accepted");
        assert_eq!(
            stmt.order_by,
            OrderByForm::FunctionCall {
                name: "hybrid_rrf".to_string(),
                args: vec![
                    FunctionArg::Ident("embedding".to_string()),
                    FunctionArg::StringLiteral("query text".to_string()),
                ],
            }
        );
    }

    #[test]
    fn accepts_select_with_order_by_function_call_alternate_allowed_name() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY HYBRID(embedding, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("alternate allowed function name should be accepted");
        assert_eq!(
            stmt.order_by,
            OrderByForm::FunctionCall {
                name: "HYBRID".to_string(),
                args: vec![
                    FunctionArg::Ident("embedding".to_string()),
                    FunctionArg::StringLiteral("query text".to_string()),
                ],
            }
        );
    }

    // TASK-75・SQL-4: 4 引数形（`<vec列>, '<vec リテラル>', <text列>, '<query text>'`）を
    // 構造として受理する（既存 2 引数形の受理は変更しない。実行可能性の判定は
    // `sql::parser::bind` の管轄）。
    #[test]
    fn accepts_select_with_order_by_function_call_four_args() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, '[0.1,0.2]', body, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("4-arg function-call shape should be accepted");
        assert_eq!(
            stmt.order_by,
            OrderByForm::FunctionCall {
                name: "hybrid_rrf".to_string(),
                args: vec![
                    FunctionArg::Ident("embedding".to_string()),
                    FunctionArg::StringLiteral("[0.1,0.2]".to_string()),
                    FunctionArg::Ident("body".to_string()),
                    FunctionArg::StringLiteral("query text".to_string()),
                ],
            }
        );
    }

    #[test]
    fn accepts_select_with_order_by_function_call_four_args_alternate_name() {
        let lookup = catalog_with(&["documents"]);
        validate_statement(
            "SELECT * FROM documents ORDER BY HYBRID(embedding, '[0.1,0.2]', body, 'query text') LIMIT 20",
            &lookup,
        )
        .expect("4-arg HYBRID shape should be accepted");
    }

    #[test]
    fn rejects_order_by_function_call_with_unknown_name() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY attacker_controlled(embedding) LIMIT 5",
        );
    }

    #[test]
    fn rejects_where_predicate_call_with_unknown_name() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE unknown() ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    // 許可された ORDER BY 関数の引数形状回帰テスト。
    #[test]
    fn rejects_order_by_function_call_with_empty_args() {
        assert_rejected_as_syntax_error("SELECT * FROM documents ORDER BY HYBRID() LIMIT 5");
    }

    #[test]
    fn rejects_order_by_function_call_with_single_arg() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding) LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_too_many_args() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'q', 'extra') LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_missing_comma_between_args() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding 'q') LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_trailing_comma() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'q',) LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_wrong_second_arg_type() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 123) LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_nested_paren_group_as_arg() {
        // 括弧グループは意味を持たないため、第 1 引数の位置に来ても拒否する。
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf((embedding), 'q') LIMIT 5",
        );
    }

    #[test]
    fn rejects_order_by_function_call_with_nested_call_as_arg() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, foo('q')) LIMIT 5",
        );
    }

    #[test]
    fn accepts_explicit_column_list() {
        let lookup = catalog_with(&["documents"]);
        validate_statement(
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("explicit column list should be accepted");
    }

    #[test]
    fn accepts_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5;",
            &lookup,
        )
        .expect("single trailing semicolon should be accepted");
    }

    // --- 拒否系（SQL-8 列挙） ------------------------------------------------

    fn assert_rejected_as_syntax_error(sql: &str) {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(sql, &lookup).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601", "sql={sql:?} err={err:?}");
    }

    #[test]
    fn rejects_cte() {
        assert_rejected_as_syntax_error(
            "WITH x AS (SELECT * FROM documents) SELECT * FROM x ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_distinct() {
        assert_rejected_as_syntax_error(
            "SELECT DISTINCT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_group_by() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents GROUP BY lang ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_having() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents HAVING lang = 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_join() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents JOIN other ON documents.id = other.id ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_multiple_from_tables() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents, other ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_offset() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 OFFSET 10",
        );
    }

    #[test]
    fn rejects_multiple_order_by_expressions() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]', lang LIMIT 5",
        );
    }

    #[test]
    fn rejects_unsupported_where_condition() {
        // 単一等価・単一 RLS 呼び出し以外（比較演算子・OR 等）は許可リスト外。
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents WHERE lang != 'ja' ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_long_left_associative_arithmetic_chain_by_node_budget() {
        // `parse_add_expr`/`parse_mul_expr` の左結合ループは `depth` を増やさずに
        // `lhs` へ木を積み続けるため、"1+1+...+1" のような同一深さの連鎖入力は
        // `MAX_EXPR_DEPTH`（構文解析の再帰段数チェック）をすり抜けうる。ノード数
        // 予算（`Parser::expr_node_budget`、[`MAX_EXPR_NODES`] 共有）がこの形の
        // 入力も頭打ちにすることを確認する（`54000`。ノード予算エラー後の
        // `Box<Expr>` 再帰的 drop によるスタック消費を定数に抑える対応）。
        let chain: String = "1+".repeat(600) + "1";
        let sql = format!(
            "SELECT * FROM documents WHERE {chain} > 0 ORDER BY embedding <=> '[0.1]' LIMIT 5"
        );
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(&sql, &lookup).unwrap_err();
        assert_eq!(err.wire_code(), "54000", "err={err:?}");
    }

    #[test]
    fn rejects_multiple_statements() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5; SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_non_select_statement() {
        assert_rejected_as_syntax_error("INSERT INTO documents (embedding) VALUES ('[0.1]')");
    }

    #[test]
    fn rejects_dollar_parameter_placeholder_in_order_by() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> $1 LIMIT 5",
        );
    }

    #[test]
    fn rejects_lowercase_distinct_case_insensitively() {
        assert_rejected_as_syntax_error(
            "SELECT distinct * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
        );
    }

    #[test]
    fn rejects_semicolon_inside_argument_list_injection_attempt() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding; DROP) LIMIT 5",
        );
    }

    // --- HINT ORDER（TASK-76・SQL-7） ----------------------------------------

    #[test]
    fn accepts_all_six_hint_order_permutations() {
        let lookup = catalog_with(&["documents"]);
        for perm in [
            "RLS, SCALAR, DISTANCE",
            "RLS, DISTANCE, SCALAR",
            "SCALAR, RLS, DISTANCE",
            "SCALAR, DISTANCE, RLS",
            "DISTANCE, RLS, SCALAR",
            "DISTANCE, SCALAR, RLS",
        ] {
            let sql = format!(
                "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER({perm})"
            );
            validate_statement(&sql, &lookup)
                .unwrap_or_else(|e| panic!("perm={perm:?} must be accepted, got {e:?}"));
        }
    }

    #[test]
    fn accepts_lowercase_hint_order_stage_names_and_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(rls, scalar, distance);",
            &lookup,
        )
        .expect("lowercase stage names should be accepted");
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn accepts_hint_as_an_ordinary_column_name() {
        // `HINT` は LIMIT 直後の所定位置でのみ文脈依存で認識するため、通常の列名
        // としての `hint` は引き続き受理する（後方互換性、AGENTS.md P1）。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT hint FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("hint should be usable as an ordinary column name");
        assert_eq!(
            stmt.projection,
            Projection::Columns(vec!["hint".to_string()])
        );
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn accepts_hint_as_a_where_equality_column_name() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE hint = 'x' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("hint should be usable as an ordinary WHERE column name");
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn no_hint_order_defaults_to_rls_scalar_distance() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("must be accepted");
        assert_eq!(stmt.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn hint_order_populates_evaluation_order_field() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(DISTANCE, SCALAR, RLS)",
            &lookup,
        )
        .expect("must be accepted");
        assert_eq!(
            stmt.evaluation_order.stages(),
            [Stage::Distance, Stage::Scalar, Stage::Rls]
        );
    }

    #[test]
    fn rejects_hint_order_with_two_stages() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR)",
        );
    }

    #[test]
    fn rejects_hint_order_with_four_stages() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, DISTANCE, RLS)",
        );
    }

    #[test]
    fn rejects_hint_order_with_duplicate_stage() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, RLS, SCALAR)",
        );
    }

    #[test]
    fn rejects_hint_order_with_unknown_stage_name() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, ATTACKER)",
        );
    }

    #[test]
    fn rejects_hint_order_with_empty_parens() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER()",
        );
    }

    #[test]
    fn rejects_hint_alone_without_order() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT",
        );
    }

    #[test]
    fn rejects_hint_order_without_parens() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER RLS, SCALAR, DISTANCE",
        );
    }

    #[test]
    fn rejects_hint_order_before_limit() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' HINT ORDER(RLS, SCALAR, DISTANCE) LIMIT 5",
        );
    }

    #[test]
    fn rejects_hint_order_specified_twice() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, DISTANCE) HINT ORDER(RLS, SCALAR, DISTANCE)",
        );
    }

    #[test]
    fn rejects_trailing_tokens_after_hint_order() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER(RLS, SCALAR, DISTANCE) extra",
        );
    }

    #[test]
    fn rejects_dollar_parameter_in_hint_order() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 HINT ORDER($1, SCALAR, DISTANCE)",
        );
    }

    // `hint` を列名として使う既存 SQL の後方互換性は
    // `accepts_hint_as_an_ordinary_column_name` / `accepts_hint_as_a_where_equality_column_name`
    // で検証する（codex-review P1 指摘・AGENTS.md「公開 API・エラー契約の互換性」
    // 対応。`HINT` は LIMIT 直後の所定位置でのみ文脈依存で認識し、予約語化はしない）。

    // --- 未知テーブル（42P01） -----------------------------------------------

    #[test]
    fn rejects_undefined_table() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM nope ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn structurally_invalid_and_undefined_table_is_classified_as_syntax_error() {
        // 構造違反とテーブル不存在の両方に該当する入力は、検証順序（構造判定が先）に
        // より常に 42601 として決定的に分類される（GROUP BY は許可リスト外）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM nope GROUP BY x ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    // --- カタログ照会失敗（XX000）・fail-closed ------------------------------

    #[test]
    fn catalog_backend_failure_is_not_treated_as_acceptance() {
        let err = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &FailingCatalog,
        )
        .expect_err("backend failure must not fall open into acceptance");
        assert_eq!(err.wire_code(), "XX000");
    }

    // --- 決定性 ---------------------------------------------------------------

    #[test]
    fn same_input_yields_same_classification_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "SELECT * FROM documents GROUP BY x ORDER BY embedding <=> '[0.1]' LIMIT 5";
        let first = validate_statement(sql, &lookup).unwrap_err().wire_code();
        let second = validate_statement(sql, &lookup).unwrap_err().wire_code();
        assert_eq!(first, second);
    }

    // --- 頑健性（panic せず Err を返す） ---------------------------------------

    #[test]
    fn does_not_panic_on_empty_input() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement("", &lookup).is_err());
    }

    #[test]
    fn does_not_panic_on_only_whitespace() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement("   \n\t  ", &lookup).is_err());
    }

    #[test]
    fn does_not_panic_on_unterminated_quotes_and_nested_quotes() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents WHERE lang = 'ja ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup
        )
        .is_err());
        assert!(validate_statement(
            "SELECT * FROM documents WHERE lang = '''' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup
        )
        .is_ok());
    }

    #[test]
    fn does_not_panic_on_huge_input() {
        let lookup = catalog_with(&["documents"]);
        let huge = format!(
            "SELECT * FROM documents WHERE lang = '{}' ORDER BY embedding <=> '[0.1]' LIMIT 5",
            "x".repeat(2_000_000)
        );
        assert!(validate_statement(&huge, &lookup).is_err());
    }

    // --- validate_insert（SQL-10、TASK-80） -----------------------------------

    #[test]
    fn accepts_insert_with_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2]') USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect("basic INSERT shape should be accepted");
        assert_eq!(stmt.table_name, "documents");
        assert_eq!(
            stmt.columns,
            vec!["id".to_string(), "embedding".to_string()]
        );
        assert_eq!(
            stmt.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
    }

    #[test]
    fn accepts_insert_with_trailing_semicolon() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 'op-0001';",
            &lookup,
            LedgerMode::Ledgered,
        )
        .is_ok());
    }

    #[test]
    fn rejects_insert_missing_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1)",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("missing clause must be rejected");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_with_explicit_null_operation_id() {
        // 明示 `NULL` は句の欠落と同様に扱う（TASK-92・RECOVER-1）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("explicit NULL must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_with_explicit_null_operation_id_case_insensitive() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID null",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("lowercase null must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn explicit_null_operation_id_does_not_reach_catalog_lookup() {
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1) USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn rejects_insert_with_empty_operation_id_value() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID ''",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("empty value must be rejected as missing");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_operation_id_dollar_placeholder() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID $1",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("$n placeholder must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_operation_id_non_string_value() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 123",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("non-string value must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_with_duplicate_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 'a' USING OPERATION_ID 'b'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("duplicate clause must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn compare_only_without_ledger_accepts_missing_operation_id_clause() {
        // サーバー構成のみが必須化の可否を決める（TASK-92・RECOVER-1）:
        // `CompareOnlyWithoutLedger` では句の省略を許す。
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id) VALUES (1)",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        assert_eq!(stmt.operation_id, None);
    }

    #[test]
    fn compare_only_without_ledger_accepts_explicit_null() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID NULL",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect("compare-only mode must not require operation_id");
        assert_eq!(stmt.operation_id, None);
    }

    #[test]
    fn compare_only_without_ledger_still_validates_control_characters() {
        // 値検証（制御文字混入は `22000`）はサーバー構成に依存しない
        // （`LedgerMode` は必須化の可否のみを制御し、値の意味論的妥当性検証
        // 〔`OperationId::parse`〕を迂回させない）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id) VALUES (1) USING OPERATION_ID 'op-\u{0007}'",
            &lookup,
            LedgerMode::CompareOnlyWithoutLedger,
        )
        .expect_err("control character must still be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_select_with_using_operation_id_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING OPERATION_ID 'op-0001'",
            &lookup,
        )
        .expect_err("USING OPERATION_ID on a SELECT must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_column_value_count_mismatch() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO documents (id, embedding) VALUES (1) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("column/value count mismatch must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_insert_into_undefined_table() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1) USING OPERATION_ID 'op-0001'",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn missing_operation_id_is_classified_before_undefined_table() {
        // 構造違反（句省略）とテーブル不存在の両方に該当する入力は、検証順序
        // （構造判定が先）により常に 23502 として決定的に分類される
        // （SQL-10 の要件: 省略は書き込みトランザクション開始前に拒否）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1)",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn insert_structural_validation_never_queries_catalog_when_operation_id_is_missing() {
        // 23502 が「行数不変」のような弱い代理指標ではなく、カタログ照会（＝write
        // txn 開始の前段）そのものに到達していないことを直接確認する
        // （advisor 指摘対応: catalog lookup が一度も呼ばれていないことをフラグで検証）。
        struct FlaggingCatalog {
            called: std::cell::Cell<bool>,
        }
        impl TableLookup for FlaggingCatalog {
            fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
                self.called.set(true);
                Ok(true)
            }
        }
        let lookup = FlaggingCatalog {
            called: std::cell::Cell::new(false),
        };
        let err = validate_insert(
            "INSERT INTO nope (id) VALUES (1)",
            &lookup,
            LedgerMode::Ledgered,
        )
        .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502");
        assert!(
            !lookup.called.get(),
            "catalog lookup must not be reached before the operation_id clause is validated"
        );
    }

    #[test]
    fn rejects_insert_exceeding_max_columns() {
        let lookup = catalog_with(&["documents"]);
        let cols: Vec<String> = (0..MAX_INSERT_COLUMNS + 1)
            .map(|i| format!("c{i}"))
            .collect();
        let vals: Vec<String> = (0..MAX_INSERT_COLUMNS + 1).map(|i| i.to_string()).collect();
        let sql = format!(
            "INSERT INTO documents ({}) VALUES ({}) USING OPERATION_ID 'op-0001'",
            cols.join(", "),
            vals.join(", ")
        );
        let err =
            validate_insert(&sql, &lookup, LedgerMode::Ledgered).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn same_insert_input_yields_same_classification_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "INSERT INTO documents (id) VALUES (1)";
        let first = validate_insert(sql, &lookup, LedgerMode::Ledgered)
            .unwrap_err()
            .wire_code();
        let second = validate_insert(sql, &lookup, LedgerMode::Ledgered)
            .unwrap_err()
            .wire_code();
        assert_eq!(first, second);
    }

    // --- TASK-161（SQL-12: `USING MODE`／`SET search_mode`）------------------------

    #[test]
    fn accepts_using_mode_clause_after_limit() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 'precision'",
            &lookup,
        )
        .expect("USING MODE clause should be accepted");
        assert_eq!(stmt.search_mode.as_deref(), Some("precision"));
    }

    #[test]
    fn using_mode_clause_is_case_insensitive_on_mode_keyword_only() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 using mode 'recall'",
            &lookup,
        )
        .expect("USING/mode keywords should be case-insensitive");
        // リテラル値自体は完全一致判定（`sql::mode::SearchMode::parse_literal`）の管轄で、
        // 本モジュールは構造のみを見る。ここでは構造受理のみ検査する。
        assert_eq!(stmt.search_mode.as_deref(), Some("recall"));
    }

    #[test]
    fn select_without_using_mode_has_no_search_mode() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("plain SELECT should still be accepted");
        assert_eq!(stmt.search_mode, None);
    }

    #[test]
    fn rejects_using_mode_with_identifier_instead_of_literal() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE recall",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_with_number_literal() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 123",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_dollar_parameter_form() {
        // SQL-12: `USING MODE $n` は MVP では構文エラーで拒否する（拡張クエリプロトコル
        // 対応後の将来形式。`$` はレキサー側で既に許可リスト外として拒否される）。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE $1",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_clause_with_unsupported_word() {
        // `LIMIT` 直後の `USING` は `MODE` のみ許可する。`USING PLAN(...)`（TASK-77・
        // SQL-5）は `ORDER BY` の代替として別の位置でのみ受理するため、`ORDER BY`
        // 併用かつ非規範形（括弧なし）のこの入力はここで `42601` へ落ちる。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING PLAN 'x'",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_duplicate_using_mode_clause() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 'recall' USING MODE 'precision'",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_clause_before_limit() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' USING MODE 'recall' LIMIT 5",
            &lookup
        )
        .is_err());
    }

    #[test]
    fn rejects_using_mode_on_statement_that_is_not_select() {
        // 書き込み系文（`INSERT` 等）は本モジュールが `SELECT`／`SET` 以外を一切
        // 構文として認識しないため、`USING MODE` の有無に関わらず先頭キーワードの
        // 時点で拒否される（SQL-8 の許可リスト検証への統合。SQL-12 の R6）。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement(
            "INSERT INTO documents VALUES (1) USING MODE 'recall'",
            &lookup
        )
        .is_err());
    }

    // --- TASK-77（SQL-5: `USING PLAN(...)`）----------------------------------------

    #[test]
    fn accepts_using_plan_normative_form() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents USING PLAN('find the auth handler') LIMIT 5",
            &lookup,
        )
        .expect("normative USING PLAN form should be accepted");
        assert_eq!(stmt.using_plan(), Some("find the auth handler"));
        assert!(matches!(stmt.order_by(), OrderByForm::UsingPlan));
    }

    #[test]
    fn with_using_plan_forces_order_by_to_using_plan_variant() {
        // codex-review P1 指摘対応（PR #266）: 公開 builder が矛盾した
        // `ValidatedStatement`（`using_plan` は `Some` なのに `order_by` は
        // `OrderByForm::Distance` のまま）を構築できてしまうと、
        // `core.rs::EngineCore::execute_sql_in_session` は `using_plan()` の有無
        // だけで分岐するため、呼び出し元が意図した `order_by` が無言で無視され
        // 意図しない `USING PLAN` 経路が実行される事故になり得た。`with_using_plan`
        // が `order_by` を自動的に揃えることを固定する。
        let stmt = ValidatedStatement::new(
            "documents".to_string(),
            Projection::All,
            OrderByForm::Distance {
                column: "embedding".to_string(),
                literal: "[0.1]".to_string(),
            },
            Vec::new(),
            5,
            EvaluationOrder::DEFAULT,
        )
        .with_using_plan(Some("find auth".to_string()));
        assert!(matches!(stmt.order_by(), OrderByForm::UsingPlan));
        assert_eq!(stmt.using_plan(), Some("find auth"));
    }

    #[test]
    fn accepts_using_plan_with_where_and_using_mode() {
        let lookup = catalog_with(&["documents"]);
        let stmt = validate_statement(
            "SELECT * FROM documents WHERE visible() USING PLAN('q') LIMIT 5 USING MODE 'precision'",
            &lookup,
        )
        .expect("USING PLAN with WHERE/USING MODE should be accepted");
        assert_eq!(stmt.using_plan(), Some("q"));
        assert_eq!(stmt.search_mode(), Some("precision"));
    }

    #[test]
    fn rejects_using_plan_parameter_form() {
        // `USING PLAN($1)` は拡張クエリプロトコル対応後の将来形式。`$` は字句解析
        // 段階で拒否される（TASK-77・SQL-5）。
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN($1) LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_using_plan_without_parens() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN 'q' LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_using_plan_together_with_order_by() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' USING PLAN('q') LIMIT 5",
            &lookup,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_duplicate_using_plan_clause() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement(
            "SELECT * FROM documents USING PLAN('q') USING PLAN('q') LIMIT 5",
            &lookup,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_using_plan_empty_literal() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN('') LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_using_plan_oversized_literal() {
        let lookup = catalog_with(&["documents"]);
        let huge = format!(
            "SELECT * FROM documents USING PLAN('{}') LIMIT 5",
            "x".repeat(MAX_USING_PLAN_LEN + 1)
        );
        let err = validate_statement(&huge, &lookup).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_using_plan_non_string_argument() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT * FROM documents USING PLAN(1) LIMIT 5", &lookup)
            .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_set_search_mode_statement() {
        let lookup = catalog_with(&["documents"]);
        match validate_sql("SET search_mode = 'precision'", &lookup)
            .expect("SET search_mode should be accepted")
        {
            Statement::SetSearchMode { value } => assert_eq!(value, "precision"),
            other => panic!("expected SetSearchMode, got {other:?}"),
        }
    }

    #[test]
    fn set_search_mode_variable_name_is_case_insensitive() {
        let lookup = catalog_with(&["documents"]);
        match validate_sql("SET SEARCH_MODE = 'recall'", &lookup)
            .expect("variable name should be case-insensitive")
        {
            Statement::SetSearchMode { value } => assert_eq!(value, "recall"),
            other => panic!("expected SetSearchMode, got {other:?}"),
        }
    }

    #[test]
    fn rejects_set_of_unsupported_variable() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SET other_variable = 'x'", &lookup).is_err());
    }

    #[test]
    fn rejects_set_search_mode_to_form() {
        // 規範形は `=`。`TO` 形は SQL-12 に規範がないため受理しない。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SET search_mode TO 'recall'", &lookup).is_err());
    }

    #[test]
    fn rejects_set_search_mode_unquoted_value() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SET search_mode = recall", &lookup).is_err());
    }

    #[test]
    fn rejects_reset_search_mode() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("RESET search_mode", &lookup).is_err());
    }

    #[test]
    fn rejects_show_search_mode() {
        let lookup = catalog_with(&["documents"]);
        assert!(validate_sql("SHOW search_mode", &lookup).is_err());
    }

    #[test]
    fn rejects_set_with_trailing_using_mode_clause() {
        let lookup = catalog_with(&["documents"]);
        assert!(
            validate_sql("SET search_mode = 'recall' USING MODE 'precision'", &lookup).is_err()
        );
    }

    #[test]
    fn validate_statement_rejects_set_search_mode_as_query() {
        // `validate_statement`（後方互換 API）はセッションを持たないため `SET` を
        // 拒否する（R: 黙った no-op にしない）。
        let lookup = catalog_with(&["documents"]);
        assert!(validate_statement("SET search_mode = 'recall'", &lookup).is_err());
    }

    #[test]
    fn using_mode_and_set_search_mode_are_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1]' LIMIT 5 USING MODE 'precision'";
        let first = validate_statement(sql, &lookup).expect("should be accepted");
        let second = validate_statement(sql, &lookup).expect("should be accepted");
        assert_eq!(first, second);

        // 失敗系（未知の SET 変数）も同一入力に対し同一 `wire_code` を返すことを確認する。
        let err_a = validate_sql("SET other_variable = 'x'", &lookup)
            .expect_err("unsupported variable should be rejected")
            .wire_code();
        let err_b = validate_sql("SET other_variable = 'x'", &lookup)
            .expect_err("unsupported variable should be rejected")
            .wire_code();
        assert_eq!(err_a, err_b);
    }

    #[test]
    fn using_and_set_remain_usable_as_table_and_column_identifiers() {
        // codex-review P1 の回帰テスト: `USING`／`SET` を字句解析段階で無条件に
        // キーワード化すると、カタログ上有効な識別子（`[A-Za-z_][A-Za-z0-9_]*`）
        // である `using`／`set` というテーブル名・列名が `FROM`・投影・`ORDER BY`
        // などの識別子位置で使えなくなる未告知の破壊的変更になる。`USING`／`SET` が
        // 構文上必須の位置（`LIMIT` 直後・statement 先頭）以外では、従来どおり
        // `Ident` として通ることを確認する。
        let lookup = catalog_with(&["using", "set"]);

        // テーブル名としての `using`／`set`（FROM 句の識別子位置）。
        let stmt = validate_statement(
            "SELECT * FROM using ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("table named `using` should remain a valid identifier");
        assert_eq!(stmt.table_name, "using");
        let stmt = validate_statement(
            "SELECT * FROM set ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("table named `set` should remain a valid identifier");
        assert_eq!(stmt.table_name, "set");

        // 投影リストの列名としての `using`／`set`。
        validate_statement(
            "SELECT using, set FROM using ORDER BY embedding <=> '[0.1]' LIMIT 5",
            &lookup,
        )
        .expect("columns named `using`/`set` should remain valid identifiers");
    }

    // --- 集計 SELECT（TASK-166・SQL-13） ------------------------------------

    fn expect_aggregate(sql: &str, lookup: &impl TableLookup) -> ValidatedAggregate {
        match validate_sql(sql, lookup).expect("expected the aggregate shape to be accepted") {
            Statement::Aggregate(agg) => agg,
            other => panic!("expected Statement::Aggregate, got {other:?}"),
        }
    }

    /// [`AggregateSelectItem::Aggregate`] であることを前提に中身を取り出す
    /// （TASK-167・SQL-14 で `items()` の要素型が `AggregateSelectItem` へ変わった
    /// ことに伴うテストヘルパ）。
    fn expect_agg_item(item: &AggregateSelectItem) -> &AggregateItem {
        match item {
            AggregateSelectItem::Aggregate(item) => item,
            AggregateSelectItem::GroupKey { .. } => {
                panic!("expected an aggregate item, got a GroupKey item")
            }
        }
    }

    #[test]
    fn accepts_count_star() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate("SELECT COUNT(*) FROM documents", &lookup);
        assert_eq!(agg.table_name(), "documents");
        assert_eq!(agg.items().len(), 1);
        let item = expect_agg_item(&agg.items()[0]);
        assert_eq!(item.func, AggregateFunc::Count);
        assert_eq!(item.arg, AggregateArg::Star);
        assert_eq!(item.alias, None);
    }

    #[test]
    fn accepts_count_star_case_insensitive_function_name() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate("SELECT count(*) FROM documents", &lookup);
        assert_eq!(expect_agg_item(&agg.items()[0]).func, AggregateFunc::Count);
    }

    #[test]
    fn accepts_multiple_aggregate_items_with_alias_and_where() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate(
            "SELECT COUNT(lang), SUM(id) AS total, MIN(lang), MAX(id), AVG(id) FROM documents WHERE visible() AND lang = 'en'",
            &lookup,
        );
        assert_eq!(agg.items().len(), 5);
        let second = expect_agg_item(&agg.items()[1]);
        assert_eq!(second.func, AggregateFunc::Sum);
        assert_eq!(second.alias.as_deref(), Some("total"));
        assert_eq!(agg.where_predicates().len(), 2);
    }

    #[test]
    fn accepts_trailing_semicolon_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        expect_aggregate("SELECT COUNT(*) FROM documents;", &lookup);
    }

    #[test]
    fn accepts_group_by_with_having_order_by_and_limit() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate(
            "SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING n > 1 ORDER BY n DESC LIMIT 10",
            &lookup,
        );
        let group_by = agg.group_by().expect("GROUP BY clause must be accepted");
        assert_eq!(group_by.column, "lang");
        assert_eq!(group_by.having.len(), 1);
        assert_eq!(group_by.having[0].item_name, "n");
        assert_eq!(group_by.having[0].literal, 1.0);
        let order_by = group_by.order_by.as_ref().expect("ORDER BY must be parsed");
        assert_eq!(order_by.target, "n");
        assert!(order_by.descending);
        assert_eq!(group_by.limit, Some(10));
    }

    #[test]
    fn rejects_group_key_not_in_select_list_alone() {
        // 集計項目を 1 つも持たない SELECT リスト（`DISTINCT` 相当）は許可しない。
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT lang FROM documents GROUP BY lang", &lookup)
            .expect_err("aggregate-less GROUP BY must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_bare_column_in_select_list_without_group_by() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT lang, COUNT(*) FROM documents", &lookup)
            .expect_err("bare column reference without GROUP BY must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_select_list_bare_identifier_mismatching_group_by_column() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT id, COUNT(*) FROM documents GROUP BY lang", &lookup)
            .expect_err("mismatching bare identifier must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_multiple_group_by_columns() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT lang, COUNT(*) FROM documents GROUP BY lang, id",
            &lookup,
        )
        .expect_err("multi-column GROUP BY must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_having_without_group_by() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents HAVING COUNT(*) > 1",
            &lookup,
        )
        .expect_err("HAVING without GROUP BY must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_having_with_string_literal_rhs() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING n > 'x'",
            &lookup,
        )
        .expect_err("HAVING right-hand side must be numeric");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn accepts_having_with_negative_literal() {
        let lookup = catalog_with(&["documents"]);
        let agg = expect_aggregate(
            "SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING n > -1",
            &lookup,
        );
        let group_by = agg.group_by().expect("GROUP BY clause must be accepted");
        assert_eq!(group_by.having[0].literal, -1.0);
    }

    #[test]
    fn rejects_group_by_over_max_aggregate_items_worth_of_having_predicates() {
        let lookup = catalog_with(&["documents"]);
        let having: String = std::iter::repeat_n("n > 1", MAX_AGGREGATE_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql =
            format!("SELECT lang, COUNT(*) AS n FROM documents GROUP BY lang HAVING {having}");
        let err = validate_sql(&sql, &lookup).expect_err("HAVING predicate count must be bounded");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_order_by_and_limit_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents ORDER BY id LIMIT 10",
            &lookup,
        )
        .expect_err("ORDER BY/LIMIT must be rejected on an aggregate SELECT");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_hint_order_and_using_mode_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents HINT ORDER(rls, scalar, distance)",
            &lookup,
        )
        .expect_err("HINT ORDER must be rejected on an aggregate SELECT");
        assert_eq!(err.wire_code(), "42601");
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents USING MODE 'recall'",
            &lookup,
        )
        .expect_err("USING MODE must be rejected on an aggregate SELECT");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_mixed_aggregate_and_non_aggregate_items_either_order() {
        let lookup = catalog_with(&["documents"]);
        assert_eq!(
            validate_sql("SELECT id, COUNT(*) FROM documents", &lookup)
                .expect_err("mixing non-aggregate then aggregate must be rejected")
                .wire_code(),
            "42601"
        );
        assert_eq!(
            validate_sql("SELECT COUNT(*), id FROM documents", &lookup)
                .expect_err("mixing aggregate then non-aggregate must be rejected")
                .wire_code(),
            "42601"
        );
    }

    #[test]
    fn rejects_aggregate_call_inside_where_expression() {
        let lookup = catalog_with(&["documents"]);
        // `WHERE COUNT(id) > 1` は先頭トークンが集計形の判定に一致しない
        // （`SELECT` の直後は `WHERE` ではない）ため通常 SELECT として構文解析
        // され、`parse_call_expr` が集計名を拒否して `42601` になる。
        let err = validate_sql(
            "SELECT id FROM documents WHERE COUNT(id) > 1 ORDER BY id <=> '[0.1]' LIMIT 1",
            &lookup,
        )
        .expect_err("aggregate call in WHERE must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_nested_aggregate_call() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT(SUM(id)) FROM documents", &lookup)
            .expect_err("nested aggregate call must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_star_outside_count() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT SUM(*) FROM documents", &lookup)
            .expect_err("'*' outside COUNT must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_empty_argument_list() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT() FROM documents", &lookup)
            .expect_err("COUNT() must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_distinct_modifier() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT(DISTINCT lang) FROM documents", &lookup)
            .expect_err("COUNT(DISTINCT ...) must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_aggregate_call_in_create_function_body() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("CREATE FUNCTION f(x) AS COUNT(x)", &lookup)
            .expect_err("aggregate call in CREATE FUNCTION body must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_multiple_aggregate_statements() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql(
            "SELECT COUNT(*) FROM documents; SELECT COUNT(*) FROM documents",
            &lookup,
        )
        .expect_err("multiple statements must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_undefined_table_on_aggregate_select() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_sql("SELECT COUNT(*) FROM ghost", &lookup)
            .expect_err("undefined table must be rejected");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn rejects_too_many_aggregate_items() {
        let lookup = catalog_with(&["documents"]);
        let items = std::iter::repeat_n("COUNT(*)", MAX_AGGREGATE_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {items} FROM documents");
        let err = validate_sql(&sql, &lookup)
            .expect_err("exceeding MAX_AGGREGATE_ITEMS must be rejected");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn accepts_exactly_max_aggregate_items() {
        let lookup = catalog_with(&["documents"]);
        let items = std::iter::repeat_n("COUNT(*)", MAX_AGGREGATE_ITEMS)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {items} FROM documents");
        let agg = expect_aggregate(&sql, &lookup);
        assert_eq!(agg.items().len(), MAX_AGGREGATE_ITEMS);
    }

    #[test]
    fn validate_statement_rejects_aggregate_select_as_search_query() {
        let lookup = catalog_with(&["documents"]);
        let err = validate_statement("SELECT COUNT(*) FROM documents", &lookup)
            .expect_err("aggregate SELECT must be rejected by the SELECT-only entry point");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn aggregate_classification_is_deterministic_across_repeated_calls() {
        let lookup = catalog_with(&["documents"]);
        let sql = "SELECT COUNT(*) FROM documents WHERE lang = 'en'";
        let first = validate_sql(sql, &lookup).expect("first call should succeed");
        let second = validate_sql(sql, &lookup).expect("second call should succeed");
        assert_eq!(first, second);
    }
}
