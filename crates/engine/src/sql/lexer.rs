//! 自作 SQL トークナイザ（TASK-74・SQL-8 参照。docs/spec/05-tasks.md）。
//!
//! `sql::allowlist` の構造検証（呼び出し元）が消費する字句列を作る前段。untrusted な
//! wire 入力を直接扱うため、`unwrap`/`expect`/添字アクセスを使わず（coding-rust.md）
//! 状態機械で線形走査する。再帰を持たないため入力長に対してスタック消費が増えない。
//!
//! 未対応の記号は許可リスト外として [`LexError`] で拒否する（拒否リストではなく、
//! 既知トークンのみを許可リストとして認識する構造）。

/// トークナイザが認識する字句の種類。
///
/// キーワードは大文字小文字を区別せず ASCII 大文字へ正規化した上で、
/// 許可リストの文法が直接必要とする最小集合だけを [`Token::Keyword`] として
/// 区別する。それ以外の英字トークンはすべて [`Token::Ident`] として扱い、
/// 文法（許可リスト側）がこれらを期待しない位置に置くことで構造的に拒否させる。
///
/// `USING`・`SET`（TASK-161・SQL-12）は本レイヤでは予約語化しない。カタログ上は
/// 有効な識別子（テーブル名・列名）として従来どおり `Ident` になる語のため、
/// 字句解析の時点で無条件にキーワード化すると、その識別子が使えなくなる
/// 未告知の破壊的変更になる。構文上その語が必須の位置（`LIMIT` 直後の
/// `USING MODE ...`、statement 先頭の `SET search_mode = ...`）でのみ、
/// `allowlist` 側が `Ident` の文字列を大文字小文字を区別せず照合して
/// 文脈的にキーワードとして扱う（`allowlist.rs::parse_using_clause`・
/// `allowlist.rs::validate_sql` 参照）。`LIKE`（TASK-147・EXT-3）も同じ理由・同じ
/// 方式で `Keyword` へ含めない（`like` という列名の等価条件 `WHERE like = 'x'` を
/// 壊さないため。`allowlist.rs::Parser::parse_where` が `WHERE` 句内・`ident` の
/// 直後という位置でのみ文脈的に照合する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Keyword(Keyword),
    Ident(String),
    StringLiteral(String),
    Number(String),
    /// `(` `)` `,` `*` `=` `;` `+` `-` `/` `>` `<`（TASK-79・SQL-9 で `+ - / > <` を追加。
    /// `*` は SELECT リストの `*` と式内の乗算の両方を表す。文脈による使い分けは
    /// `allowlist::Parser` の管轄）。
    Punct(char),
    /// `<=>`（密ベクトル距離演算子）
    DistanceOp,
    /// `<=`（TASK-79・SQL-9: 式述語の比較演算子）。
    Le,
    /// `>=`（TASK-79・SQL-9: 式述語の比較演算子）。
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Select,
    From,
    Where,
    And,
    Order,
    By,
    Limit,
}

/// TASK-80・SQL-10 の `INSERT`/`INTO`/`VALUES`/`USING`/`OPERATION_ID` は
/// [`Keyword`] へ含めない（PR #189 レビュー指摘対応・P1）。この 5 語を無条件で
/// `Token::Keyword` 化すると、既存の SELECT 許可形状（`sql::allowlist`）が
/// 受理してきた同名のテーブル名・列名（`catalog::validate_identifier` は
/// これらを識別子として許可している）が `expect_ident` を通過できなくなり、
/// 引用識別子の回避策もないまま公開クエリ構文を無告知に破壊してしまう。
/// 代わりに常に [`Token::Ident`] として字句解析し、`sql::allowlist::Parser` が
/// INSERT 許可形状のパーサー位置でのみ文脈的に大文字小文字を無視して照合する
/// （`Parser::expect_contextual_keyword`）。
fn keyword_from_str(s: &str) -> Option<Keyword> {
    // 大文字小文字を区別しない ASCII 大文字比較（SQL 予約語の慣習に合わせる）。
    match s.to_ascii_uppercase().as_str() {
        "SELECT" => Some(Keyword::Select),
        "FROM" => Some(Keyword::From),
        "WHERE" => Some(Keyword::Where),
        "AND" => Some(Keyword::And),
        "ORDER" => Some(Keyword::Order),
        "BY" => Some(Keyword::By),
        "LIMIT" => Some(Keyword::Limit),
        _ => None,
    }
}

/// 字句解析エラー。位置情報はデバッグ用途に限り、応答メッセージへは
/// `allowlist` 側が長さを切り詰めて含める（security.md「情報漏えい」対応）。
#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub byte_offset: usize,
}

/// 字句解析対象のバイト長上限。構造検証自体を線形時間で終わらせるための
/// 防御的上限（security.md「DoS」対応。値レベルの検証は本モジュールの管轄外）。
pub const MAX_INPUT_LEN: usize = 1_048_576;

/// 1 文で許容するトークン数上限。無制限 `Vec` 確保を避けるための防御的上限
/// （security.md「不安全な設計｜無制限リソース確保」対応）。
pub const MAX_TOKEN_COUNT: usize = 20_000;

/// SQL テキストをトークン列へ変換する。`unwrap`/`expect`/添字アクセスを使わず、
/// `chars()` イテレータの先読み（`Peekable`）のみで走査する。
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    if input.len() > MAX_INPUT_LEN {
        return Err(LexError {
            message: format!("input too large: {} bytes", input.len()),
            byte_offset: 0,
        });
    }

    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();

    while let Some(&(offset, c)) = chars.peek() {
        // 空白の読み飛ばしはトークンを生成しないため、上限判定より先に処理する。
        // 先に判定すると、ちょうど MAX_TOKEN_COUNT 個のトークンを生成する入力が
        // 末尾の空白 1 文字の有無だけで成否が変わってしまう（読み飛ばしのみで
        // ループが終わる場合と、空白を読む前に上限判定へ触れてしまう場合の非対称）。
        if c.is_whitespace() {
            chars.next();
            continue;
        }

        if tokens.len() >= MAX_TOKEN_COUNT {
            return Err(LexError {
                message: "too many tokens".to_string(),
                byte_offset: offset,
            });
        }

        // SQL コメントは許可リスト外として fail-closed に拒否する（緩和は後続タスク判断）。
        // TASK-79・SQL-9: `-`（減算）・`/`（除算）を式演算子として追加する際も、
        // コメント検出は演算子トークン化より必ず先に行う（順序を入れ替えると
        // `--`/`/*` を無条件に許可してしまう fail-closed の後退になる）。
        if c == '-' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '-'))) {
                return Err(LexError {
                    message: "SQL comments are not supported".to_string(),
                    byte_offset: offset,
                });
            }
            tokens.push(Token::Punct('-'));
            chars.next();
            continue;
        }
        if c == '/' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '*'))) {
                return Err(LexError {
                    message: "SQL comments are not supported".to_string(),
                    byte_offset: offset,
                });
            }
            tokens.push(Token::Punct('/'));
            chars.next();
            continue;
        }

        // 二重引用符識別子は許可リスト外（受理範囲を単純化するため）。
        if c == '"' {
            return Err(LexError {
                message: "quoted identifiers are not supported".to_string(),
                byte_offset: offset,
            });
        }

        if c == '\'' {
            let (literal, next_offset) = lex_string_literal(input, offset)?;
            tokens.push(Token::StringLiteral(literal));
            advance_to(&mut chars, next_offset);
            continue;
        }

        if c == '<' {
            // 最長一致: `<=>`（距離演算子）→ `<=`（比較演算子）→ `<`（比較演算子）の順で
            // 判定する（TASK-79・SQL-9 で `<=`・裸の `<` を式述語の比較演算子として
            // 追加。`<>` はどの分岐にも一致しないため 2 つの `Punct` トークンに分かれ、
            // 許可リスト（`allowlist::Parser`）側の文法が受理しないことで構造的に
            // 拒否される＝字句解析段階では拒否しない）。
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '='))) {
                lookahead.next();
                if matches!(lookahead.peek(), Some(&(_, '>'))) {
                    lookahead.next();
                    tokens.push(Token::DistanceOp);
                    chars = lookahead;
                    continue;
                }
                tokens.push(Token::Le);
                chars = lookahead;
                continue;
            }
            tokens.push(Token::Punct('<'));
            chars.next();
            continue;
        }

        if c == '>' {
            // `>=`（比較演算子）→ `>`（比較演算子）の最長一致（TASK-79・SQL-9）。
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '='))) {
                lookahead.next();
                tokens.push(Token::Ge);
                chars = lookahead;
                continue;
            }
            tokens.push(Token::Punct('>'));
            chars.next();
            continue;
        }

        if matches!(c, '(' | ')' | ',' | '*' | '=' | ';' | '+') {
            tokens.push(Token::Punct(c));
            chars.next();
            continue;
        }

        if c.is_ascii_digit() {
            let (number, next_offset) = lex_number(input, offset);
            tokens.push(Token::Number(number));
            advance_to(&mut chars, next_offset);
            continue;
        }

        if c.is_ascii_alphabetic() || c == '_' {
            let (word, next_offset) = lex_word(input, offset);
            match keyword_from_str(&word) {
                Some(kw) => tokens.push(Token::Keyword(kw)),
                None => tokens.push(Token::Ident(word)),
            }
            advance_to(&mut chars, next_offset);
            continue;
        }

        return Err(LexError {
            message: format!("unsupported character: {c:?}"),
            byte_offset: offset,
        });
    }

    Ok(tokens)
}

/// `chars` イテレータを `byte_offset` の直前まで読み飛ばす。文字列リテラル・数値・
/// 識別子の走査を個別のバイトオフセットベース関数（`lex_string_literal` 等）で
/// 行った後、メインループの `Peekable<CharIndices>` を同じ位置まで同期させるために使う。
fn advance_to(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, byte_offset: usize) {
    while let Some(&(offset, _)) = chars.peek() {
        if offset >= byte_offset {
            break;
        }
        chars.next();
    }
}

/// `'...'` 文字列リテラルを読む。`''` を単一の `'` へのエスケープとして扱う。
/// 閉じ引用符が見つからない場合は `Err`。リテラルの内容自体の意味論的妥当性は
/// 検証しない（本レイヤは文字列として正しく閉じているかのみを構造的に見る）。
fn lex_string_literal(input: &str, start: usize) -> Result<(String, usize), LexError> {
    let bytes = input.as_bytes();
    // start は呼び出し元で確認済みの `'` の位置。
    let mut idx = start + 1;
    let mut content = String::new();
    loop {
        let Some(&b) = bytes.get(idx) else {
            return Err(LexError {
                message: "unterminated string literal".to_string(),
                byte_offset: start,
            });
        };
        if b == b'\'' {
            // 直後がもう 1 つの `'` ならエスケープ（リテラル内の `'` 1 文字）。
            if bytes.get(idx + 1) == Some(&b'\'') {
                content.push('\'');
                idx += 2;
                continue;
            }
            return Ok((content, idx + 1));
        }
        // マルチバイト文字を安全に取り出す（添字直接アクセスをせず char_indices 経由）。
        let rest = input.get(idx..).ok_or_else(|| LexError {
            message: "invalid string literal encoding".to_string(),
            byte_offset: idx,
        })?;
        let ch = rest.chars().next().ok_or_else(|| LexError {
            message: "invalid string literal encoding".to_string(),
            byte_offset: idx,
        })?;
        content.push(ch);
        idx += ch.len_utf8();
    }
}

/// `input.get(start..)` が `None` を返す（`start` が文字境界でない・範囲外）ことは
/// 呼び出し元がメインループで先読みした ASCII 文字境界の直後からしか呼ばないため
/// 通常あり得ないが、untrusted 入力経路では添字直接アクセス（`input[start..]`）を
/// 使わず `get()` で明示的に処理する（coding-rust.md）。
/// TASK-79・SQL-9: 整数に加え `<digits>.<digits>` の小数リテラルを 1 トークンとして
/// 認識する。先頭 `.`（`.5`）・末尾 `.`（`1.`）・2 個目以降の `.`（`1..2`）は本関数の
/// 対象外（呼び出し元のメインループは整数部までしか消費しないため、残った `.` は
/// 「未対応文字」として `tokenize` が fail-closed に拒否する）。指数表記は非対応。
fn lex_number(input: &str, start: usize) -> (String, usize) {
    let Some(rest) = input.get(start..) else {
        return (String::new(), start);
    };
    let mut chars = rest.char_indices().peekable();
    let mut end = 0usize;
    while let Some(&(_, c)) = chars.peek() {
        if c.is_ascii_digit() {
            end += c.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    // 小数部は「`.` の直後に少なくとも 1 桁の数字が続く」場合のみ消費する
    // （1 文字先読みで確定させ、`1.` のような末尾 `.` を誤って飲み込まない）。
    if let Some(&(_, '.')) = chars.peek() {
        let mut lookahead = chars.clone();
        lookahead.next();
        if matches!(lookahead.peek(), Some(&(_, d)) if d.is_ascii_digit()) {
            end += '.'.len_utf8();
            chars.next();
            while let Some(&(_, c)) = chars.peek() {
                if c.is_ascii_digit() {
                    end += c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
        }
    }
    let word = rest.get(..end).unwrap_or_default().to_string();
    (word, start + end)
}

fn lex_word(input: &str, start: usize) -> (String, usize) {
    let Some(rest) = input.get(start..) else {
        return (String::new(), start);
    };
    let mut end = 0usize;
    for c in rest.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    let word = rest.get(..end).unwrap_or_default().to_string();
    (word, start + end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_simple_select() {
        let tokens = tokenize("SELECT * FROM docs LIMIT 10").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Keyword(Keyword::Select),
                Token::Punct('*'),
                Token::Keyword(Keyword::From),
                Token::Ident("docs".to_string()),
                Token::Keyword(Keyword::Limit),
                Token::Number("10".to_string()),
            ]
        );
    }

    #[test]
    fn insert_operation_id_words_lex_as_plain_idents() {
        // PR #189 レビュー指摘対応（P1）: INSERT 許可形状・USING OPERATION_ID
        // 文末句が使う 5 語は Token::Keyword 化せず、常に Token::Ident として
        // 字句解析されることを固定する（文脈的キーワード化は
        // `sql::allowlist::Parser` 側の責務）。
        let tokens =
            tokenize("INSERT INTO t VALUES USING OPERATION_ID").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("INSERT".to_string()),
                Token::Ident("INTO".to_string()),
                Token::Ident("t".to_string()),
                Token::Ident("VALUES".to_string()),
                Token::Ident("USING".to_string()),
                Token::Ident("OPERATION_ID".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_using_and_set_as_plain_idents() {
        // TASK-161（SQL-12）: `USING`／`SET` は字句解析の時点ではキーワード化しない
        // （`allowlist` 側が LIMIT 直後／statement 先頭という文脈でのみキーワードとして
        // 扱う。カタログ上有効な識別子としての `using`／`set` を字句解析段階で
        // 破壊的に奪わないための設計）。
        let tokens = tokenize("using MODE 'recall'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("using".to_string()),
                Token::Ident("MODE".to_string()),
                Token::StringLiteral("recall".to_string()),
            ]
        );
        let tokens = tokenize("SET search_mode = 'precision'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("SET".to_string()),
                Token::Ident("search_mode".to_string()),
                Token::Punct('='),
                Token::StringLiteral("precision".to_string()),
            ]
        );
    }

    #[test]
    fn insert_operation_id_words_remain_usable_as_ordinary_identifiers() {
        // PR #189 レビュー指摘対応（P1）: `catalog::validate_identifier` が許可する
        // 同名のテーブル名・列名（例: `values`）が、SELECT 許可形状の
        // `expect_ident` 位置で引き続き受理できることを固定する
        // （`SELECT values FROM documents` が構文破壊しない回帰テスト）。
        let tokens = tokenize("SELECT values FROM documents").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Keyword(Keyword::Select),
                Token::Ident("values".to_string()),
                Token::Keyword(Keyword::From),
                Token::Ident("documents".to_string()),
            ]
        );
    }

    #[test]
    fn using_and_set_remain_valid_identifiers_outside_their_keyword_positions() {
        // P1 修正の回帰: `using`／`set` はテーブル名・列名としての `Ident` 位置
        // （`FROM`・投影・`ORDER BY` 等）で従来どおり使用できる。
        let tokens = tokenize("SELECT using FROM set").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Keyword(Keyword::Select),
                Token::Ident("using".to_string()),
                Token::Keyword(Keyword::From),
                Token::Ident("set".to_string()),
            ]
        );
    }

    #[test]
    fn keyword_matching_is_case_insensitive() {
        let tokens = tokenize("select * from docs limit 1").expect("tokenize should succeed");
        assert_eq!(tokens[0], Token::Keyword(Keyword::Select));
        assert_eq!(tokens[2], Token::Keyword(Keyword::From));
        assert_eq!(tokens[4], Token::Keyword(Keyword::Limit));
    }

    #[test]
    fn hint_is_a_context_dependent_ident_not_a_reserved_keyword() {
        // HINT ORDER(...) は LIMIT 直後の所定位置でのみ allowlist 側が文脈依存で
        // 認識する語であり、字句解析の時点では常に通常の識別子として扱う
        // （後方互換性: `hint` を列名・テーブル名として使う既存 SQL を拒否しない）。
        let tokens = tokenize("HINT ORDER(RLS)").expect("tokenize should succeed");
        assert_eq!(tokens[0], Token::Ident("HINT".to_string()));
        assert_eq!(tokens[1], Token::Keyword(Keyword::Order));

        let tokens = tokenize("hint order(rls)").expect("tokenize should succeed");
        assert_eq!(tokens[0], Token::Ident("hint".to_string()));
    }

    #[test]
    fn tokenizes_distance_operator() {
        let tokens = tokenize("embedding <=> 'x'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("embedding".to_string()),
                Token::DistanceOp,
                Token::StringLiteral("x".to_string()),
            ]
        );
    }

    #[test]
    fn string_literal_handles_escaped_quote() {
        let tokens = tokenize("'it''s'").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::StringLiteral("it's".to_string())]);
    }

    #[test]
    fn rejects_unterminated_string_literal() {
        assert!(tokenize("'abc").is_err());
    }

    #[test]
    fn rejects_line_comment() {
        assert!(tokenize("SELECT * FROM docs -- comment").is_err());
    }

    #[test]
    fn rejects_block_comment() {
        assert!(tokenize("SELECT /* c */ * FROM docs").is_err());
    }

    #[test]
    fn rejects_quoted_identifier() {
        assert!(tokenize("SELECT * FROM \"docs\"").is_err());
    }

    #[test]
    fn rejects_dollar_parameter_placeholder() {
        assert!(tokenize("embedding <=> $1").is_err());
    }

    #[test]
    fn accepts_lone_less_than_as_comparison_operator() {
        // TASK-79・SQL-9: 式述語の比較演算子として単独の `<` を受理するようになった
        // （旧 `rejects_lone_less_than` の置き換え。構文上その位置を受理するかどうかは
        // `allowlist::Parser` の管轄で、本テストは字句解析段階の受理のみを確認する）。
        let tokens = tokenize("a < b").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("a".to_string()),
                Token::Punct('<'),
                Token::Ident("b".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_arithmetic_and_comparison_operators() {
        // TASK-79・SQL-9: `+ - / > < >= <=` を式演算子として追加。`<=>`（距離演算子）
        // との最長一致・`*`（乗算と SELECT * の両方に使う既存トークン）を確認する。
        let tokens = tokenize("1.5 + 2 - 3 * 4 / 5 > 6 >= 7 < 8 <= 9 <=> 10")
            .expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Number("1.5".to_string()),
                Token::Punct('+'),
                Token::Number("2".to_string()),
                Token::Punct('-'),
                Token::Number("3".to_string()),
                Token::Punct('*'),
                Token::Number("4".to_string()),
                Token::Punct('/'),
                Token::Number("5".to_string()),
                Token::Punct('>'),
                Token::Number("6".to_string()),
                Token::Ge,
                Token::Number("7".to_string()),
                Token::Punct('<'),
                Token::Number("8".to_string()),
                Token::Le,
                Token::Number("9".to_string()),
                Token::DistanceOp,
                Token::Number("10".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_decimal_number_literal() {
        let tokens = tokenize("2.0").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::Number("2.0".to_string())]);
    }

    #[test]
    fn rejects_trailing_dot_number_literal() {
        // `1.` は整数部 `1` のみを 1 トークンとして消費し、残った `.` が
        // 「未対応文字」として拒否される（小数部は「`.` の直後に数字」の場合のみ
        // 消費するため）。
        assert!(tokenize("1. ").is_err());
    }

    #[test]
    fn rejects_leading_dot_number_literal() {
        assert!(tokenize(".5").is_err());
    }

    #[test]
    fn rejects_double_dot_number_literal() {
        assert!(tokenize("1..2").is_err());
    }

    #[test]
    fn rejects_input_exceeding_max_len() {
        let huge = "a".repeat(MAX_INPUT_LEN + 1);
        assert!(tokenize(&huge).is_err());
    }

    #[test]
    fn accepts_exactly_max_token_count_without_trailing_whitespace() {
        let input = "*".repeat(MAX_TOKEN_COUNT);
        let tokens = tokenize(&input).expect("exactly MAX_TOKEN_COUNT tokens should be accepted");
        assert_eq!(tokens.len(), MAX_TOKEN_COUNT);
    }

    #[test]
    fn accepts_exactly_max_token_count_with_trailing_whitespace() {
        // 上限判定は空白の読み飛ばしより後に行うため、ちょうど MAX_TOKEN_COUNT 個の
        // トークンを生成する入力は末尾空白の有無に関係なく同じ結果になる。
        let input = format!("{} ", "*".repeat(MAX_TOKEN_COUNT));
        let tokens = tokenize(&input).expect("trailing whitespace must not affect the boundary");
        assert_eq!(tokens.len(), MAX_TOKEN_COUNT);
    }

    #[test]
    fn rejects_more_than_max_token_count() {
        let input = "*".repeat(MAX_TOKEN_COUNT + 1);
        assert!(tokenize(&input).is_err());
    }

    #[test]
    fn handles_multibyte_characters_in_string_literal_without_panicking() {
        let tokens = tokenize("'日本語データ'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![Token::StringLiteral("日本語データ".to_string())]
        );
    }

    #[test]
    fn does_not_panic_on_truncated_utf8_like_input() {
        // 不正なバイト列を意図的に含む入力でも panic せず Err を返すことを確認する
        // （String は常に有効な UTF-8 のため、ここでは実用上あり得るケースとして
        // 閉じられない文字列リテラル中に非 ASCII 文字を混在させる形で検証する）。
        let input = "'\u{e9}\u{e9}";
        assert!(tokenize(input).is_err());
    }
}
