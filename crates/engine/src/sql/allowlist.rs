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

use crate::recovery::required_op_id::LedgerMode;
use crate::sql::lexer::{self, Keyword, LexError, Token};
use crate::sql::plan::{self, EvaluationOrder, Stage};
use crate::sql::udf_call::{
    BinOp, Expr, MAX_CALL_ARGS, MAX_EXPR_DEPTH, MAX_EXPR_NODES, MAX_UDF_PARAMS,
};
use crate::sql::using_operation_id::OperationId;

/// エラーメッセージへ含める入力断片の長さ上限。untrusted 入力をそのまま無加工で
/// 長大にエラーへ埋め込まない（security.md「情報漏えい」対応）。
const MAX_ERROR_DETAIL_LEN: usize = 200;

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
    /// 冪等判定（台帳による重複拒否・内容不一致検出）は本 variant の管轄外で、
    /// TASK-93・TASK-94・TASK-101 が後続で扱う。
    IdConflict,
}

impl SqlSurfaceError {
    /// ERR-2（docs/spec/04-behavior/error-format.md）の wire_code 写像。
    pub fn wire_code(&self) -> &'static str {
        match self {
            SqlSurfaceError::UnsupportedSyntax { .. } => "42601",
            SqlSurfaceError::UndefinedTable { .. } => "42P01",
            SqlSurfaceError::Internal { .. } => "XX000",
            SqlSurfaceError::InvalidInput { .. } => "22000",
            SqlSurfaceError::PayloadTooLarge { .. } => "54000",
            SqlSurfaceError::MissingOperationId => "23502",
            SqlSurfaceError::IdConflict => "23505",
        }
    }

    /// クライアント（wire 層 `ErrorResponse`）へそのまま返してよい文言を返す。
    /// `Internal`（`wire_code() == "XX000"`）は redb I/O エラー等の内部ストレージ
    /// 詳細を保持しているため固定の一般化メッセージへ丸め、それ以外の variant は
    /// 通常の `Display` 文言（テナント越境の存在情報を含まないよう各コンストラクタ
    /// 側で既に切り詰め・一般化済み）をそのまま返す（security.md P0「private
    /// 情報の漏えい」対応。`wire-server::simple_query` はエラー応答の整形時に
    /// `to_string()` ではなく必ずこちらを使うこと）。
    pub fn client_message(&self) -> String {
        match self {
            SqlSurfaceError::Internal { .. } => "internal error".to_string(),
            other => other.to_string(),
        }
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
}

/// WHERE 句の許可形状。名前を照合する述語呼び出し形は、許可された名前
/// （[`is_allowed_where_predicate_name`]）のみを通過させる。
///
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Expression`
/// variant を追加した（宣言的 UDF・組み込み関数呼び出しを含む比較式
/// `<expr> <cmp> <expr>`。式の意味論検証は `sql::parser::bind_in_session` の責務）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WherePredicate {
    /// 列と文字列リテラルの等価条件（TASK-75: リテラル値を保持する）。
    Equality { column: String, value: String },
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
        }
    }

    /// `search_mode`（TASK-161・SQL-12）を設定したコピーを返すビルダー的メソッド。
    /// [`Self::new`] と組み合わせて `search_mode` を含む値を外部から構築する。
    #[must_use]
    pub fn with_search_mode(mut self, search_mode: Option<String>) -> Self {
        self.search_mode = search_mode;
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
}

/// [`validate_sql`]（TASK-161 の公開 API）が返す statement 種別。`SELECT` 以外の
/// 文が増えても [`ValidatedStatement`] 自体は SELECT 専用の構造を保つため、
/// 統一的な enum で包む。
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `CreateFunction`
/// variant を追加した。
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// WHERE 句の許可形状（等価条件・述語呼び出し形・TASK-79（SQL-9）で追加した
    /// 式の比較述語 `<expr> <cmp> <expr>` の 3 種。`OR`・括弧によるネストは
    /// 引き続き許可しない）。述語呼び出し形は許可された名前
    /// （[`is_allowed_where_predicate_name`]）のみを受理し、未知の名前は拒否する。
    ///
    /// 既存 2 形との曖昧さ回避: 先頭が `ident '=' <string literal>` なら等価条件、
    /// `ident '(' ')'`（許可名のみ）なら述語呼び出し形として確定的に判定し、
    /// いずれにも一致しない場合のみ式の比較述語として再解析する（`pos` を
    /// 巻き戻してから解析し直す。式文法の `primary` は `ident '(' <args> ')'` も
    /// 受理するため、`visible()` 以外の名前の呼び出し形はここで初めて式として
    /// 解釈される）。
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
    fn parse_call_expr(&mut self, name: String, depth: usize) -> Result<Expr, SqlSurfaceError> {
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
    /// （将来 TASK-77/80 が追加する `PLAN`・`OPERATION_ID` 等）は fail-closed に拒否
    /// する（未実装の拡張点を黙って受理しない）。句を高々 1 回だけ消費するため、
    /// 2 回目以降の `USING MODE ...` は本メソッドではなく後続の
    /// [`Parser::expect_end_of_statement`] が「余剰トークン」として拒否する。
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
}

/// 許可した `SELECT` statement 形状を先頭から再帰下降で判定する（TASK-74 由来。
/// TASK-161 で `LIMIT` 直後の `USING MODE` 句判定を追加した）。
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
    match tokens.first() {
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
        other => Err(SqlSurfaceError::unsupported(format!(
            "expected SELECT, SET, or CREATE FUNCTION, got {other:?}"
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
        // `USING` 直後は `MODE` のみ許可する（`PLAN`・`OPERATION_ID` は TASK-77/80 の
        // 拡張点であり本タスクでは未実装。fail-closed に拒否する）。
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
}
