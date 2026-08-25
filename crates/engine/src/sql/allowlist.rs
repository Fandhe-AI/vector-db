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

use crate::sql::lexer::{self, Keyword, LexError, Token};

/// エラーメッセージへ含める入力断片の長さ上限。untrusted 入力をそのまま無加工で
/// 長大にエラーへ埋め込まない（security.md「情報漏えい」対応）。
const MAX_ERROR_DETAIL_LEN: usize = 200;

/// `parse_arg_list` が許容する丸括弧の最大ネスト深さ。字句解析側に入力長・ネスト
/// 段数の上限がないため、`expect_value` を深さごとに保持するスタック（`Vec<bool>`）
/// は攻撃者が選ぶ入れ子段数に比例してヒープを消費しうる。このモジュールは
/// ネストした関数呼び出しの意味を一切解釈しない設計であり、正当な入力が
/// この深さを超えることはないため、上限超過は無制限リソース確保を防ぐため
/// fail-closed に拒否する（coding-rust.md「untrusted 入力の扱い」・security.md
/// 「不安全な設計」対応）。
const MAX_ARG_NESTING_DEPTH: usize = 32;

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
}

impl SqlSurfaceError {
    /// ERR-2（docs/spec/04-behavior/error-format.md）の wire_code 写像。
    pub fn wire_code(&self) -> &'static str {
        match self {
            SqlSurfaceError::UnsupportedSyntax { .. } => "42601",
            SqlSurfaceError::UndefinedTable { .. } => "42P01",
            SqlSurfaceError::Internal { .. } => "XX000",
        }
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
}

impl std::fmt::Display for SqlSurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlSurfaceError::UnsupportedSyntax { detail } => {
                write!(f, "unsupported SQL syntax: {detail}")
            }
            SqlSurfaceError::UndefinedTable { name } => write!(f, "undefined table: {name}"),
            SqlSurfaceError::Internal { detail } => write!(f, "internal error: {detail}"),
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

/// `ORDER BY` 式の許可形状。TASK-74・SQL-8 参照（docs/spec/05-tasks.md）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderByForm {
    /// 距離演算子形。
    Distance { column: String },
    /// 関数呼び出し形。引数は本モジュールでは構造（括弧の対応・許可トークンのみ）
    /// しか見ず、意味は後続タスクが解釈する。
    FunctionCall { name: String },
}

/// WHERE 句の許可形状。名前を照合する述語呼び出し形は、許可された名前
/// （[`is_allowed_where_predicate_name`]）のみを通過させる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WherePredicate {
    /// 列と文字列リテラルの等価条件。
    Equality { column: String },
    /// 許可された名前の述語呼び出し形（空引数）。
    PredicateCall { name: String },
}

/// 許可形状の構造判定を通過した SQL 文（後続タスクのパーサー・実行計画の土台）。
/// 本モジュールが保証するのはここまでの構造情報のみで、列名・リテラル値の意味論的な
/// 妥当性は検証しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedStatement {
    /// FROM に指定され、カタログ存在確認を通過したテーブル名。
    pub table_name: String,
    pub order_by: OrderByForm,
    /// WHERE 句に含まれる述語（AND 結合順）。空なら WHERE 句なし。
    pub where_predicates: Vec<WherePredicate>,
    pub limit: u32,
}

/// 1 文の最大トークン数を超えない前提の下で使うパーサーカーソル。
/// 再帰下降だが文法の深さは定数（statement → select_list/where/order_by の 1 階層）で、
/// 深いネストによるスタック消費は発生しない（`arg_list` の丸括弧対応のみ非再帰ループで数える）。
struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
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

    /// SELECT リストの許可形状（`*` または単純な列名リストのみ）。
    fn parse_select_list(&mut self) -> Result<(), SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct('*'))) {
            self.advance();
            return Ok(());
        }
        self.expect_ident()?;
        while matches!(self.peek(), Some(Token::Punct(','))) {
            self.advance();
            self.expect_ident()?;
        }
        Ok(())
    }

    /// WHERE 句の許可形状（等価条件・述語呼び出し形の 2 種のみ。`OR`・括弧による
    /// ネスト・比較演算子の拡張は許可しない）。述語呼び出し形は許可された名前
    /// （[`is_allowed_where_predicate_name`]）のみを受理し、未知の名前は拒否する。
    fn parse_where(&mut self) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        let mut predicates = Vec::new();
        loop {
            let name = self.expect_ident()?;
            match self.peek() {
                Some(Token::Punct('=')) => {
                    self.advance();
                    self.expect_string_literal()?;
                    predicates.push(WherePredicate::Equality { column: name });
                }
                Some(Token::Punct('(')) => {
                    if !is_allowed_where_predicate_name(&name) {
                        return Err(SqlSurfaceError::unsupported(format!(
                            "unsupported WHERE predicate: {name}"
                        )));
                    }
                    self.advance();
                    self.expect_punct(')')?;
                    predicates.push(WherePredicate::PredicateCall { name });
                }
                other => {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "unsupported WHERE predicate form near {other:?}"
                    )))
                }
            }
            if matches!(self.peek(), Some(Token::Keyword(Keyword::And))) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(predicates)
    }

    /// ORDER BY 式の許可形状（距離演算子形または関数呼び出し形）。関数呼び出し形は
    /// 許可された名前（[`is_allowed_order_by_function_name`]）のみを受理し、
    /// 未知の名前は拒否する。
    fn parse_order_by(&mut self) -> Result<OrderByForm, SqlSurfaceError> {
        let name = self.expect_ident()?;
        match self.peek() {
            Some(Token::DistanceOp) => {
                self.advance();
                self.expect_string_literal()?;
                Ok(OrderByForm::Distance { column: name })
            }
            Some(Token::Punct('(')) => {
                if !is_allowed_order_by_function_name(&name) {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "unsupported ORDER BY function: {name}"
                    )));
                }
                self.advance();
                self.parse_arg_list()?;
                self.expect_punct(')')?;
                Ok(OrderByForm::FunctionCall { name })
            }
            other => Err(SqlSurfaceError::unsupported(format!(
                "unsupported ORDER BY expression near {other:?}"
            ))),
        }
    }

    /// 関数呼び出し形 ORDER BY 式の引数部分。意味は解釈せず、括弧の対応が取れた
    /// 許可トークン（識別子・文字列・数値・カンマ・入れ子丸括弧）のみで構成されて
    /// いるかを構造的に検証する（fail-closed。キーワード・`;` 等の混入は拒否）。
    /// カンマ区切りの値リスト構造を厳密に要求し、区切りなしの連続トークン・
    /// 空要素列・先頭/末尾カンマは拒否する。値/区切りの期待状態はネストの深さ
    /// ごとに独立したスタックで追跡し、丸括弧の深さに関係なく同じ規律を適用する
    /// （入れ子丸括弧の内側だけ規律が緩む非対称を作らない）。
    fn parse_arg_list(&mut self) -> Result<(), SqlSurfaceError> {
        if matches!(self.peek(), Some(Token::Punct(')'))) {
            return Ok(());
        }
        // 深さごとの値/区切り期待状態スタック。先頭が呼び出し直下（depth 0）で、
        // `(` を消費するたびにフレームを push する。untrusted 入力由来のトークン列
        // を扱うため、取得は常に `Option` 経路で行い空スタックによる panic を防ぐ
        // （coding-rust.md: 受信データ経路での unwrap/expect/添字アクセス禁止）。
        let mut expect_value: Vec<bool> = vec![true];
        loop {
            match self.advance() {
                Some(Token::Ident(_)) | Some(Token::StringLiteral(_)) | Some(Token::Number(_)) => {
                    let cur = expect_value.last_mut().ok_or_else(|| {
                        SqlSurfaceError::unsupported("internal argument list depth underflow")
                    })?;
                    if !*cur {
                        return Err(SqlSurfaceError::unsupported(
                            "expected ',' or ')' between argument list values",
                        ));
                    }
                    *cur = false;
                }
                Some(Token::Punct('(')) => {
                    let cur = expect_value.last_mut().ok_or_else(|| {
                        SqlSurfaceError::unsupported("internal argument list depth underflow")
                    })?;
                    if !*cur {
                        // 識別子や値の直後に区切りカンマなしで `(` が続く形
                        // （`foo(...)` 等）。関数呼び出しのネストは解釈しないため拒否。
                        return Err(SqlSurfaceError::unsupported(
                            "expected ',' or ')' between argument list values",
                        ));
                    }
                    // 入れ子丸括弧グループ全体を現在の深さの 1 値として扱う。
                    *cur = false;
                    if expect_value.len() >= MAX_ARG_NESTING_DEPTH {
                        return Err(SqlSurfaceError::unsupported(
                            "argument list nesting depth exceeds the allowed maximum",
                        ));
                    }
                    expect_value.push(true);
                }
                Some(Token::Punct(')')) => {
                    if expect_value.len() <= 1 {
                        let cur = expect_value.last().copied().unwrap_or(true);
                        if cur {
                            return Err(SqlSurfaceError::unsupported(
                                "unexpected ')' in argument list: missing value",
                            ));
                        }
                        // 呼び出し元の `expect_punct(')')` が消費すべき閉じ括弧に達した。
                        // `advance()` 直後で `pos >= 1` が保証されるため `saturating_sub`
                        // は防御的措置（coding-rust.md「整数演算は checked/saturating を使う」）。
                        self.pos = self.pos.saturating_sub(1);
                        return Ok(());
                    }
                    // 入れ子フレームを閉じる。空グループ（`()`）は値として認めず拒否する。
                    let closed_frame_expects_value = expect_value.pop().ok_or_else(|| {
                        SqlSurfaceError::unsupported("internal argument list depth underflow")
                    })?;
                    if closed_frame_expects_value {
                        return Err(SqlSurfaceError::unsupported(
                            "unexpected ')' in argument list: missing value",
                        ));
                    }
                }
                Some(Token::Punct(',')) => {
                    let cur = expect_value.last_mut().ok_or_else(|| {
                        SqlSurfaceError::unsupported("internal argument list depth underflow")
                    })?;
                    if *cur {
                        return Err(SqlSurfaceError::unsupported(
                            "unexpected ',' in argument list: missing value",
                        ));
                    }
                    *cur = true;
                }
                other => {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "unsupported token in argument list: {other:?}"
                    )))
                }
            }
        }
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
}

/// 構文木（[`ValidatedStatement`] の元）。カタログ存在確認前の中間結果。
struct ParsedShape {
    table_name: String,
    where_predicates: Vec<WherePredicate>,
    order_by: OrderByForm,
    limit: u32,
}

/// 許可した statement 形状を先頭から再帰下降で判定する。
fn parse_statement(tokens: &[Token]) -> Result<ParsedShape, SqlSurfaceError> {
    let mut p = Parser::new(tokens);

    p.expect_keyword(Keyword::Select)?;
    p.parse_select_list()?;
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

    p.expect_end_of_statement()?;

    Ok(ParsedShape {
        table_name,
        where_predicates,
        order_by,
        limit,
    })
}

/// SQL 文をトークン化し、許可リスト形式で構造検証してから、`lookup` を通じて
/// FROM テーブルがカタログに実在するかを確認する（TASK-74 の公開 API）。
///
/// 検証順序（決定的。同一入力には常に同一の [`SqlSurfaceError`] を返す）:
/// 1. 字句解析（入力長・トークン数上限を含む。失敗は [`SqlSurfaceError::UnsupportedSyntax`]）
/// 2. 構造の許可リスト判定（失敗は `UnsupportedSyntax`）
/// 3. FROM 単一テーブルのカタログ存在確認（不存在は [`SqlSurfaceError::UndefinedTable`]）
pub fn validate_statement(
    sql: &str,
    lookup: &impl TableLookup,
) -> Result<ValidatedStatement, SqlSurfaceError> {
    let tokens = lexer::tokenize(sql)?;
    let shape = parse_statement(&tokens)?;

    let exists = lookup.table_exists(&shape.table_name)?;
    if !exists {
        return Err(SqlSurfaceError::undefined_table(shape.table_name));
    }

    Ok(ValidatedStatement {
        table_name: shape.table_name,
        order_by: shape.order_by,
        where_predicates: shape.where_predicates,
        limit: shape.limit,
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
                column: "embedding".to_string()
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
                column: "lang".to_string()
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
                    column: "lang".to_string()
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
                name: "hybrid_rrf".to_string()
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
                name: "HYBRID".to_string()
            }
        );
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

    // parse_arg_list の区切り構造回帰テスト（Issue #55 レビュー指摘）。
    #[test]
    fn rejects_hybrid_args_without_comma_separator() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding 'q') LIMIT 5",
        );
    }

    #[test]
    fn rejects_hybrid_args_all_commas_no_values() {
        assert_rejected_as_syntax_error("SELECT * FROM documents ORDER BY hybrid_rrf(,,,) LIMIT 5");
    }

    #[test]
    fn rejects_hybrid_args_trailing_comma() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding,) LIMIT 5",
        );
    }

    #[test]
    fn rejects_hybrid_args_leading_comma() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(,embedding) LIMIT 5",
        );
    }

    #[test]
    fn accepts_hybrid_args_with_nested_paren_group() {
        // 入れ子丸括弧そのもの（identifier に隣接しない、括弧単独のグループ）は
        // depth 0 の 1 値として引き続き許可される。identifier 直後に区切りなしで
        // `(` が続く形（`foo(...)`）は区切りカンマなしの連結として拒否側に倒す
        // （このモジュールは関数呼び出しのネストを解釈しない）。
        let lookup = catalog_with(&["documents"]);
        validate_statement(
            "SELECT * FROM documents ORDER BY hybrid_rrf((embedding), 'q') LIMIT 5",
            &lookup,
        )
        .expect("standalone nested paren group argument should still be accepted");
    }

    #[test]
    fn rejects_hybrid_args_identifier_immediately_followed_by_paren() {
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, foo('q')) LIMIT 5",
        );
    }

    #[test]
    fn rejects_hybrid_args_identifier_followed_by_paren_wrapped_in_nested_group() {
        // Issue #55 レビュー指摘の再現ケース: 識別子直後の `(` を拒否する契約は、
        // 呼び出し直下（depth 0）だけでなく入れ子丸括弧の内側（depth >= 1）でも
        // 同様に働かなければならない。1 段カッコで包むだけでバイパスできてはならない。
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf((foo('q'))) LIMIT 5",
        );
    }

    #[test]
    fn rejects_hybrid_args_without_comma_separator_inside_nested_group() {
        // 同上: 区切りカンマなしの連続トークン（`a b c`）も入れ子丸括弧の内側で
        // 拒否されなければならない（depth 0 の `hybrid_rrf(a b c)` と対称であること）。
        assert_rejected_as_syntax_error(
            "SELECT * FROM documents ORDER BY hybrid_rrf((a b c)) LIMIT 5",
        );
    }

    #[test]
    fn rejects_hybrid_args_empty_nested_paren_group() {
        // 入れ子丸括弧の空グループ（`()`）は値として認めない。
        assert_rejected_as_syntax_error("SELECT * FROM documents ORDER BY hybrid_rrf(()) LIMIT 5");
    }

    #[test]
    fn rejects_hybrid_args_nesting_depth_beyond_maximum() {
        // untrusted 入力による無制限の入れ子でスタックが際限なく伸びないよう、
        // ネスト深さに上限を設けて fail-closed に拒否する。
        let opens = "(".repeat(MAX_ARG_NESTING_DEPTH + 1);
        let closes = ")".repeat(MAX_ARG_NESTING_DEPTH + 1);
        let sql = format!(
            "SELECT * FROM documents ORDER BY hybrid_rrf({opens}embedding{closes}) LIMIT 5"
        );
        assert_rejected_as_syntax_error(&sql);
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
}
