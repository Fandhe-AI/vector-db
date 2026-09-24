//! 拡張クエリプロトコルの `$n` パラメータ束縛（Issue #935・WIRE-12・TASK-217）。
//!
//! 責務境界: `sql::lexer::tokenize_with_params` が生成した `Token::Param` を
//! 含むトークン列に対し、(a) 許可されたプレースホルダ位置だけを構造的に受理し
//! （Parse 時点。値はまだ知らない）、(b) Bind 時点で実値を検証済みの
//! `Token::StringLiteral` へ 1 トークンとして置換する、という 2 段の
//! トークン変換だけを提供する。置換後のトークン列を `sql::allowlist::
//! validate_sql_tokens`（または `core.rs::EngineCore::parse_tokens` が委譲する
//! `validate_insert_tokens` 等）へ通すのは呼び出し元（`core.rs`）の責務であり、
//! 本モジュール自体は許可リスト検証器・実行器を一切持たない（第 2 の実行器を
//! 作らない設計。値は常に 1 個の `Token::StringLiteral` の中身としてのみ格納され、
//! SQL テキストへも字句解析器へも戻らないため、値経由のインジェクション経路は
//! 構造的に存在しない）。
//!
//! ## 受理するプレースホルダ位置（WIRE-12 の規範形。これ以外は `42601`）
//!
//! 1. `ORDER BY <vec列> <=> $n`（ベクトル位置。直前トークンが [`Token::DistanceOp`]）
//! 2. `USING PLAN($n)`（`Ident("USING") Ident("PLAN") '(' $n ')'`）
//! 3. `USING OPERATION_ID $n`（INSERT／TRUNCATE／DELETE／UPDATE 共通の文末句。
//!    `Ident("USING") Ident("OPERATION_ID") $n`）
//! 4. `WHERE <列> = $n`（トップレベル `WHERE` 節の内側、`Ident '=' $n` の形のみ。
//!    `LIKE`・式比較・BOOLEAN 列条件・`SET`／`ON CONFLICT ... SET` の同型文字列は
//!    対象外——`WHERE` 節の外側にはこのパターンを一切適用しない）
//! 5. `INSERT ... VALUES ($1, $2, ...)`（複数行含む。`VALUES` 節内の `(`/`,` と
//!    `,`/`)` に挟まれた位置のみ）
//!
//! `LIMIT $n`・`USING MODE $n`・`HINT ORDER($n, ...)`・`SET search_mode = $n`・
//! `UPDATE ... SET a = $n`・`ON CONFLICT ... DO UPDATE SET a = $n`・hybrid 関数
//! 引数（`HYBRID_RRF(col, $n, ...)`）・非等価比較（`id > $n` 等）はいずれも
//! スコープ外として `42601` で拒否する（後続 Issue へ申し送り。詳細は
//! `docs/design/wire-extended-query-param-binding.md` 参照）。
//!
//! ## 値の形式
//!
//! すべてのプレースホルダは常に [`Token::StringLiteral`] として置換する。
//! `id` 列・`BOOLEAN` 列など、生の [`Token::Number`]／`Token::Ident("true"/"false")`
//! を要求する位置（例: `INSERT ... VALUES ($1)` の `$1` が `id` 列や `BOOLEAN` 列に
//! 対応する場合）は本バージョンのスコープ外であり、束縛すると「同じ値をクォート
//! したリテラルで書いた SQL」と同一の型不一致エラー（構造検証・束縛の既存契約
//! そのまま）で拒否される——これは fail-closed な既知の制約であり誤動作ではない。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{self, Token};

/// 1 文で許容するパラメータ番号の上限（実装既定値）。wire 層（Parse の宣言型
/// 件数・Bind の値件数）もこの定数を単一情報源として参照する。
pub const MAX_PARAMS: u16 = 64;

/// `WHERE` 節（パターン 4）の境界を判定するための、句の開始を示す文脈識別子
/// （字句解析段階ではキーワード化されていない語。大文字小文字を無視して照合）。
const WHERE_CLAUSE_BOUNDARY_IDENTS: [&str; 5] = ["GROUP", "HAVING", "RETURNING", "USING", "ON"];

/// `INSERT` の `VALUES` 節（パターン 5）の終端を示す文脈識別子。
const VALUES_CLAUSE_BOUNDARY_IDENTS: [&str; 2] = ["ON", "USING"];

fn ident_eq_ignore_case(token: Option<&Token>, word: &str) -> bool {
    matches!(token, Some(Token::Ident(name)) if name.eq_ignore_ascii_case(word))
}

/// `tokens` 中の `Token::Param` がすべて許可位置に収まっていることを検証し、
/// 文が要求するパラメータ数（最大の `$n` 番号。1 始まり。`$n` が 1 つも
/// 無ければ 0）を返す。
///
/// - 許可位置以外に現れた `$n` は [`SqlSurfaceError::unsupported`]（`42601`）。
/// - `$n` の番号が [`MAX_PARAMS`] を超える場合は
///   [`SqlSurfaceError::payload_too_large`]（`54000`）。
pub fn validate_param_positions(tokens: &[Token]) -> Result<u16, SqlSurfaceError> {
    let is_insert_statement = ident_eq_ignore_case(tokens.first(), "INSERT");

    // パターン 5（INSERT VALUES）の受理範囲: `VALUES` キーワード（文脈識別子。
    // 大文字小文字を無視して最初の出現のみを見る——本許可形状は `VALUES` 節を
    // 1 つしか持たない）の直後から、`ON`／`USING` のいずれかが現れる直前まで。
    let values_region = if is_insert_statement {
        tokens.iter().position(|t| match t {
            Token::Ident(name) => name.eq_ignore_ascii_case("VALUES"),
            _ => false,
        })
    } else {
        None
    };
    let values_region_end = values_region.map(|start| {
        tokens
            .iter()
            .enumerate()
            .skip(start + 1)
            .find_map(|(idx, t)| match t {
                Token::Ident(name)
                    if VALUES_CLAUSE_BOUNDARY_IDENTS
                        .iter()
                        .any(|b| name.eq_ignore_ascii_case(b)) =>
                {
                    Some(idx)
                }
                _ => None,
            })
            .unwrap_or(tokens.len())
    });

    // パターン 4（WHERE 等価）の受理範囲: トップレベル `WHERE`（最初の出現。本
    // 許可形状は `WHERE` 節を 1 つしか持たない）の直後から、後続句の境界
    // （`ORDER`／`LIMIT`／文脈識別子）の直前まで。`SET`（statement 全体）である
    // 場合は `WHERE` を含まないため自然に対象外になる。
    let where_idx = tokens
        .iter()
        .position(|t| matches!(t, Token::Keyword(crate::sql::lexer::Keyword::Where)));
    let where_region_end = where_idx.map(|start| {
        tokens
            .iter()
            .enumerate()
            .skip(start + 1)
            .find_map(|(idx, t)| match t {
                Token::Keyword(crate::sql::lexer::Keyword::Order)
                | Token::Keyword(crate::sql::lexer::Keyword::Limit) => Some(idx),
                Token::Ident(name)
                    if WHERE_CLAUSE_BOUNDARY_IDENTS
                        .iter()
                        .any(|b| name.eq_ignore_ascii_case(b)) =>
                {
                    Some(idx)
                }
                _ => None,
            })
            .unwrap_or(tokens.len())
    });

    let mut max_index: u16 = 0;
    for (i, token) in tokens.iter().enumerate() {
        let Token::Param(n) = token else { continue };
        let n = *n;
        max_index = max_index.max(n);

        let prev1 = tokens.get(i.wrapping_sub(1));
        let prev2 = i.checked_sub(2).and_then(|j| tokens.get(j));
        let prev3 = i.checked_sub(3).and_then(|j| tokens.get(j));
        let next1 = tokens.get(i + 1);

        // パターン 1: `<col> <=> $n`。
        let is_distance = matches!(prev1, Some(Token::DistanceOp));

        // パターン 2: `USING PLAN($n)`。
        let is_using_plan = ident_eq_ignore_case(prev3, "USING")
            && ident_eq_ignore_case(prev2, "PLAN")
            && matches!(prev1, Some(Token::Punct('(')))
            && matches!(next1, Some(Token::Punct(')')));

        // パターン 3: `USING OPERATION_ID $n`。
        let is_operation_id =
            ident_eq_ignore_case(prev2, "USING") && ident_eq_ignore_case(prev1, "OPERATION_ID");

        // パターン 4: `WHERE` 節内の `<ident> = $n`。
        let is_where_equality = where_idx.is_some_and(|start| {
            let end = where_region_end.unwrap_or(tokens.len());
            i > start
                && i < end
                && matches!(prev1, Some(Token::Punct('=')))
                && matches!(prev2, Some(Token::Ident(_)))
        });

        // パターン 5: `INSERT ... VALUES (...)` の行内リテラル位置。
        let is_insert_value = values_region.is_some_and(|start| {
            let end = values_region_end.unwrap_or(tokens.len());
            i > start
                && i < end
                && matches!(prev1, Some(Token::Punct('(')) | Some(Token::Punct(',')))
                && matches!(next1, Some(Token::Punct(')')) | Some(Token::Punct(',')))
        });

        if !(is_distance
            || is_using_plan
            || is_operation_id
            || is_where_equality
            || is_insert_value)
        {
            return Err(SqlSurfaceError::unsupported(format!(
                "parameter placeholder ${n} is not supported at this position"
            )));
        }
    }

    if max_index > MAX_PARAMS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "parameter number ${max_index} exceeds the allowed maximum (${MAX_PARAMS})"
        )));
    }

    Ok(max_index)
}

/// [`Token::Param`] をすべて `value_for` が返す文字列の [`Token::StringLiteral`]
/// へ置換したトークン列を返す。`value_for` は 1 始まりのパラメータ番号を受け取る。
fn substitute_with<E>(
    tokens: &[Token],
    mut value_for: impl FnMut(u16) -> Result<String, E>,
) -> Result<Vec<Token>, E> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token {
            Token::Param(n) => out.push(Token::StringLiteral(value_for(*n)?)),
            other => out.push(other.clone()),
        }
    }
    Ok(out)
}

/// Parse 時点の構造検証専用: すべての `$n` を、位置に依存しない固定ダミー値へ
/// 置換する。ダミーは非空・非制御文字（[`crate::sql::using_operation_id::
/// OperationId::parse`]・[`crate::sql::allowlist::validate_using_plan_question`]
/// のいずれも満たす）であり、それ以外の位置（ベクトルリテラル・WHERE 等価・
/// INSERT VALUES）では値の意味論的妥当性が Bind まで遅延されるため内容を問わない。
/// 得られたトークン列を実行・Describe に使ってはならない（値未確定のダミーの
/// ため、呼び出し元は構造検証の結果だけを見て破棄する）。
pub fn substitute_dummy(tokens: &[Token]) -> Vec<Token> {
    substitute_with(tokens, |_| {
        Ok::<_, std::convert::Infallible>("0".to_string())
    })
    .unwrap_or_default()
}

/// Bind 時点: `values`（`$1` から順に 1 始まりで対応する実値）で全 `$n` を
/// 置換する。呼び出し元は事前に `values.len()` が [`validate_param_positions`]
/// の返す `param_count` と一致することを確認していること（本関数は添字
/// アクセスに `checked_sub`／`get` を使うが、この事前条件自体は検査しない）。
pub fn substitute_values(
    tokens: &[Token],
    values: &[String],
) -> Result<Vec<Token>, SqlSurfaceError> {
    substitute_with(tokens, |n| {
        let idx = usize::from(n).checked_sub(1).ok_or_else(|| {
            SqlSurfaceError::unsupported("parameter placeholder $0 is not valid".to_string())
        })?;
        values.get(idx).cloned().ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("missing bind value for parameter ${n}"))
        })
    })
}

/// Bind 値の UTF-8 検証・NUL 拒否・置換後総バイト長の上限判定
/// （[`crate::sql::lexer::MAX_INPUT_LEN`]）を行い、`Token::StringLiteral` へ
/// 格納できる文字列へ変換する。`tokens` は `$n` の出現回数を数えるために使う
/// （同一 `$n` を大量に参照させて 1 回の値から巨大な置換結果を作らせる
/// メモリ増幅を、値の複製（[`String::to_string`]）より前に防ぐ）。
///
/// P0（codex-review・Issue #935 PR #1012 指摘）: `values` は
/// [`validate_param_positions`] が返す `param_count`（文中で参照される
/// 最大の `$n` 番号）と同数だけ渡ってくる契約のため、文が実際に参照して
/// いない `$n`（例: `$64` だけを使う文に対する `$1`〜`$63`）に巨大な値が
/// 含まれ得る。この関数はまず UTF-8／NUL 検証を借用 `&str` のまま行い
/// （複製しない）、置換後総バイト数（参照回数込み）を checked 演算で
/// 確定させたうえで、実際に参照されている位置の値だけを複製する。未参照の
/// 値は（検証は受けるが）一切複製されないため、その内容量はメモリ増幅
/// 攻撃の入力にならない。
///
/// - `NULL`（`values` の要素が `None`）は本バージョンのスコープ外として
///   [`SqlSurfaceError::invalid_input`]（`22000`）で拒否する（未参照の
///   位置も含め、渡された全値に対して検証する）。
/// - 非 UTF-8・NUL 文字混入も同じ `22000`。
/// - 置換後総バイト数超過は [`SqlSurfaceError::payload_too_large`]（`54000`）。
pub fn decode_bind_values(
    tokens: &[Token],
    values: &[Option<Vec<u8>>],
) -> Result<Vec<String>, SqlSurfaceError> {
    // 借用のまま UTF-8 検証・NUL 拒否を行う（値の複製より前）。
    let mut borrowed: Vec<&str> = Vec::with_capacity(values.len());
    for raw in values {
        let bytes = raw.as_ref().ok_or_else(|| {
            SqlSurfaceError::invalid_input("NULL parameter values are not supported".to_string())
        })?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| SqlSurfaceError::invalid_input("parameter value is not valid UTF-8"))?;
        if text.contains('\0') {
            return Err(SqlSurfaceError::invalid_input(
                "parameter value must not contain a NUL byte",
            ));
        }
        borrowed.push(text);
    }

    // 総置換バイト数（$n の出現回数 × 対応する値の長さ、の総和）を、値の
    // 複製より前に借用済みスライスの長さだけを見て checked 演算で確定
    // させる。未参照の $n（`tokens` に一切現れない番号）の値はここでも
    // 一切参照されないため、集計・複製いずれのコストにも計上されない。
    let mut total: usize = 0;
    for token in tokens {
        let Token::Param(n) = token else { continue };
        let idx = usize::from(*n).checked_sub(1);
        let len = idx
            .and_then(|i| borrowed.get(i))
            .map(|s| s.len())
            .unwrap_or(0);
        total = total.checked_add(len).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("substituted parameter payload size overflowed")
        })?;
    }
    if total > lexer::MAX_INPUT_LEN {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "substituted parameter payload ({total} bytes) exceeds the allowed limit ({} bytes)",
            lexer::MAX_INPUT_LEN
        )));
    }

    // サイズ検証を通過した後、実際に参照されている位置の値だけを複製する。
    // 未参照の位置は空文字列のまま残す（`substitute_values` は `tokens` の
    // `Token::Param` からしか添字アクセスしないため参照されず、複製コストも
    // メモリ上の保持コストも発生しない）。
    let mut decoded: Vec<String> = vec![String::new(); values.len()];
    for token in tokens {
        let Token::Param(n) = token else { continue };
        let Some(idx) = usize::from(*n).checked_sub(1) else {
            continue;
        };
        if let Some(slot) = decoded.get_mut(idx) {
            if slot.is_empty() {
                if let Some(text) = borrowed.get(idx) {
                    *slot = (*text).to_string();
                }
            }
        }
    }

    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize_with_params;

    fn positions(sql: &str) -> Result<u16, SqlSurfaceError> {
        let tokens = tokenize_with_params(sql).expect("tokenize_with_params should succeed");
        validate_param_positions(&tokens)
    }

    #[test]
    fn accepts_vector_distance_position() {
        assert_eq!(
            positions("SELECT * FROM documents ORDER BY embedding <=> $1 LIMIT 5").unwrap(),
            1
        );
    }

    #[test]
    fn accepts_using_plan_position() {
        assert_eq!(
            positions("SELECT * FROM documents USING PLAN($1) LIMIT 5").unwrap(),
            1
        );
    }

    #[test]
    fn accepts_using_operation_id_position() {
        assert_eq!(
            positions("INSERT INTO documents (id, body) VALUES (1, 'x') USING OPERATION_ID $1")
                .unwrap(),
            1
        );
    }

    #[test]
    fn accepts_where_equality_position() {
        assert_eq!(
            positions("SELECT * FROM documents WHERE lang = $1 LIMIT 5").unwrap(),
            1
        );
    }

    #[test]
    fn accepts_insert_values_position() {
        assert_eq!(
            positions("INSERT INTO documents (id, body) VALUES ($1, $2) USING OPERATION_ID 'op-1'")
                .unwrap(),
            2
        );
    }

    #[test]
    fn accepts_multi_row_insert_values_positions() {
        assert_eq!(
            positions(
                "INSERT INTO documents (id, body) VALUES ($1, $2), ($3, $4) USING OPERATION_ID 'op-1'"
            )
            .unwrap(),
            4
        );
    }

    #[test]
    fn rejects_limit_position() {
        assert!(
            positions("SELECT * FROM documents ORDER BY embedding <=> '[0]' LIMIT $1").is_err()
        );
    }

    #[test]
    fn rejects_using_mode_position() {
        let err =
            positions("SELECT * FROM documents ORDER BY embedding <=> '[0]' LIMIT 5 USING MODE $1")
                .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_hint_order_position() {
        assert!(positions(
            "SELECT * FROM documents ORDER BY embedding <=> '[0]' LIMIT 5 HINT ORDER($1)"
        )
        .is_err());
    }

    #[test]
    fn rejects_set_search_mode_position() {
        assert!(positions("SET search_mode = $1").is_err());
    }

    #[test]
    fn rejects_update_set_position() {
        assert!(positions("UPDATE documents SET lang = $1 WHERE id = 1").is_err());
    }

    #[test]
    fn accepts_where_equality_for_update_predicate_form() {
        // `UPDATE ... SET ... WHERE <col> = $n` は SET 側を拒否しつつ WHERE 側の
        // 等価条件は受理する（両者が同じ `Ident '=' $n` の字面を持つため、
        // WHERE 節の範囲判定でのみ区別する）。
        assert_eq!(
            positions("UPDATE documents SET lang = 'ja' WHERE id_tag = $1").unwrap(),
            1
        );
    }

    #[test]
    fn rejects_on_conflict_do_update_set_position() {
        assert!(positions(
            "INSERT INTO documents (id, body) VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET body = $1 USING OPERATION_ID 'op-1'"
        )
        .is_err());
    }

    #[test]
    fn rejects_hybrid_function_argument_position() {
        assert!(positions(
            "SELECT * FROM documents ORDER BY HYBRID_RRF(embedding, $1, body, 'q') LIMIT 5"
        )
        .is_err());
    }

    #[test]
    fn rejects_non_equality_where_comparison_position() {
        assert!(positions("SELECT * FROM documents WHERE id > $1 LIMIT 5").is_err());
    }

    #[test]
    fn rejects_parameter_number_over_max() {
        let sql = format!(
            "SELECT * FROM documents WHERE lang = ${}",
            (MAX_PARAMS as u32) + 1
        );
        let err = positions(&sql).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn accepts_parameter_number_at_max() {
        let sql = format!("SELECT * FROM documents WHERE lang = ${MAX_PARAMS}");
        assert!(positions(&sql).is_ok());
    }

    #[test]
    fn no_params_yields_zero_count() {
        assert_eq!(positions("SELECT * FROM documents LIMIT 5").unwrap(), 0);
    }

    #[test]
    fn substitute_dummy_replaces_every_param_with_string_literal() {
        let tokens = tokenize_with_params("embedding <=> $1").expect("tokenize should succeed");
        let dummy = substitute_dummy(&tokens);
        assert_eq!(
            dummy,
            vec![
                Token::Ident("embedding".to_string()),
                Token::DistanceOp,
                Token::StringLiteral("0".to_string()),
            ]
        );
    }

    #[test]
    fn substitute_values_maps_each_param_to_its_value() {
        let tokens =
            tokenize_with_params("WHERE lang = $1 AND lang = $2").expect("tokenize should succeed");
        let substituted =
            substitute_values(&tokens, &["ja".to_string(), "en".to_string()]).unwrap();
        assert_eq!(
            substituted,
            vec![
                Token::Keyword(crate::sql::lexer::Keyword::Where),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("ja".to_string()),
                Token::Keyword(crate::sql::lexer::Keyword::And),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("en".to_string()),
            ]
        );
    }

    #[test]
    fn substitute_values_repeated_param_reuses_same_value() {
        let tokens =
            tokenize_with_params("WHERE lang = $1 AND lang = $1").expect("tokenize should succeed");
        let substituted = substitute_values(&tokens, &["ja".to_string()]).unwrap();
        assert_eq!(
            substituted,
            vec![
                Token::Keyword(crate::sql::lexer::Keyword::Where),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("ja".to_string()),
                Token::Keyword(crate::sql::lexer::Keyword::And),
                Token::Ident("lang".to_string()),
                Token::Punct('='),
                Token::StringLiteral("ja".to_string()),
            ]
        );
    }

    #[test]
    fn decode_bind_values_rejects_null() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[None]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn decode_bind_values_rejects_non_utf8() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[Some(vec![0xff, 0xfe])]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn decode_bind_values_rejects_embedded_nul() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[Some(b"a\0b".to_vec())]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn decode_bind_values_accepts_valid_utf8() {
        let tokens = tokenize_with_params("WHERE lang = $1").expect("tokenize should succeed");
        let decoded = decode_bind_values(&tokens, &[Some("日本語".as_bytes().to_vec())]).unwrap();
        assert_eq!(decoded, vec!["日本語".to_string()]);
    }

    #[test]
    fn decode_bind_values_rejects_amplified_total_payload_before_cloning_values() {
        // 同一 `$1` を大量に参照させて、1 回の値から巨大な置換結果を作らせる
        // メモリ増幅を、値の複製より前に検出する。`tokenize_with_params` の
        // `MAX_TOKEN_COUNT`（20,000）に収まる範囲（1 参照 = 4 トークン）で、
        // 出現回数 × 値の長さが `lexer::MAX_INPUT_LEN` を超えるようにする。
        let mut sql = "WHERE lang = $1".to_string();
        for _ in 0..4_999 {
            sql.push_str(" AND lang = $1");
        }
        let tokens = tokenize_with_params(&sql).expect("tokenize should succeed");
        let big_value = "x".repeat(300);
        let err = decode_bind_values(&tokens, &[Some(big_value.into_bytes())]).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn decode_bind_values_ignores_unreferenced_values_in_size_and_content() {
        // P0（codex-review・Issue #935 PR #1012 指摘）の回帰: 文が `$2` しか
        // 参照しない場合、`$1` に渡された巨大な値はサイズ集計にも複製にも
        // 一切影響しない（複製前の借用スライスの長さだけで検証するため）。
        // 修正前は全 `values` を検証前に無条件で `to_string()` 複製していた
        // ため、この巨大な未参照値がメモリ増幅攻撃の入力になり得た。
        let tokens = tokenize_with_params("WHERE lang = $2").expect("tokenize should succeed");
        let huge_unreferenced = "x".repeat(lexer::MAX_INPUT_LEN * 4);
        let decoded = decode_bind_values(
            &tokens,
            &[Some(huge_unreferenced.into_bytes()), Some(b"ja".to_vec())],
        )
        .expect("unreferenced oversized value must not affect size validation");

        // 参照されている `$2` の値は正しく複製される。
        assert_eq!(decoded[1], "ja");
        // 参照されていない `$1` の値は複製されない（プレースホルダのまま）。
        assert_eq!(decoded[0], "");
    }

    #[test]
    fn decode_bind_values_still_rejects_null_at_unreferenced_position() {
        // 未参照の位置であっても NULL・非 UTF-8・NUL 混入の検証は全値に対して
        // 行う（fail-closed。参照有無で入力検証の強度を変えない）。
        let tokens = tokenize_with_params("WHERE lang = $2").expect("tokenize should succeed");
        let err = decode_bind_values(&tokens, &[None, Some(b"ja".to_vec())]).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }
}
