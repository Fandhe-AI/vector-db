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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Keyword(Keyword),
    Ident(String),
    StringLiteral(String),
    Number(String),
    /// `(` `)` `,` `*` `=` `;`
    Punct(char),
    /// `<=>`（密ベクトル距離演算子）
    DistanceOp,
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
        if c == '-' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '-'))) {
                return Err(LexError {
                    message: "SQL comments are not supported".to_string(),
                    byte_offset: offset,
                });
            }
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
            // `<=>` のみを認識する。それ以外（`<`・`<=`・`<>`）は許可リスト外。
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
            }
            return Err(LexError {
                message: "unsupported operator starting with '<'".to_string(),
                byte_offset: offset,
            });
        }

        if matches!(c, '(' | ')' | ',' | '*' | '=' | ';') {
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
fn lex_number(input: &str, start: usize) -> (String, usize) {
    let Some(rest) = input.get(start..) else {
        return (String::new(), start);
    };
    let mut end = 0usize;
    for c in rest.chars() {
        if c.is_ascii_digit() {
            end += c.len_utf8();
        } else {
            break;
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
    fn rejects_lone_less_than() {
        assert!(tokenize("a < b").is_err());
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
