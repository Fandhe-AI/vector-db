//! SQL 表層（`sql::exec::execute_statement`）向けの C1 クエリ文字列組み立て
//! （TASK-83。ポインタ: `docs/spec/05-tasks.md` TASK-83）。
//!
//! `sql_c1_bench.rs` から呼ばれる。`EngineCore::execute_sql` は untrusted な SQL
//! テキストを受け取る入口であり、本モジュールが生成する文字列も同じ経路を通るため
//! `.claude/rules/coding-rust.md`「untrusted 入力の扱い」に従い、定数のみで構成される
//! テーブル名・列名・k はここでも識別子検証を経てから埋め込み、ベクトルリテラルは
//! 生成時にサイズ上限・非有限値チェックを行ってから返す（未検証の外部文字列を
//! SQL へ連結する経路を作らない）。
//!
//! `std` のみに依存する（`harness/mod.rs` 冒頭コメント参照: 本モジュール群は
//! `cargo bench` バイナリと統合テストの複数コンパイル単位から `#[path]` で
//! 取り込まれる共有ソースのため、`crate::` を参照しない）。

use std::fmt::Write as _;

/// ベクトルリテラルの生バイト長上限（`sql::parser::MAX_VECTOR_LITERAL_BYTES` と
/// 同一値のつもりで手動複製した定数。当該定数は private のため独自に定義する。
/// SQL-1 の受理形状に合わせて上限超過分の追記コストを打ち切る。
/// `pub` にしているのは `tests/c1_bench_accept.rs` の
/// `max_vector_literal_bytes_matches_parser_boundary` が、この値ちょうどの境界で
/// `parser::parse_vector_literal` の受理・拒否を突き合わせてドリフトを検知するため
/// （parser 側の定数が変わればそのテストが最初に落ちる）。
pub const MAX_VECTOR_LITERAL_BYTES: usize = 64 * 1024;

/// ベンチ・回帰テストが生成した SQL 文字列の構造検証・生成失敗を表す（`harness`
/// 全体で単一のエラー型を持つ `stats::BenchError` とは責務が異なる——こちらは
/// SQL 文字列生成という untrusted 入力に隣接する処理の失敗であり、計測プロトコルの
/// 失敗ではないため独立した型にする）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlC1Error {
    /// ベクトル成分に非有限値（NaN・Inf）が含まれていた。
    NonFiniteComponent,
    /// 生成したリテラルが [`MAX_VECTOR_LITERAL_BYTES`] を超過した。
    LiteralTooLarge,
    /// テーブル名・列名が識別子として不正だった。
    InvalidIdentifier(&'static str),
}

impl std::fmt::Display for SqlC1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlC1Error::NonFiniteComponent => {
                write!(f, "vector component must be finite")
            }
            SqlC1Error::LiteralTooLarge => {
                write!(f, "vector literal exceeds {MAX_VECTOR_LITERAL_BYTES} bytes")
            }
            SqlC1Error::InvalidIdentifier(field) => {
                write!(f, "{field} is not a valid identifier")
            }
        }
    }
}

/// [`vector_literal`] の検証（非有限値の拒否・[`MAX_VECTOR_LITERAL_BYTES`] 上限）を
/// 通過したベクトルリテラルであることを型で表す newtype。
///
/// 内部の `String` は非公開で、本モジュール外からは [`vector_literal`] 経由でしか
/// 構築できない。これにより [`c1_statement`] が SQL へ単一引用符つきで連結する
/// 文字列は「検証済みである」ことがコード上で保証され、運用規約に依存しない
/// （`.claude/rules/coding-rust.md`「SQL / プラン文字列の組み立てに未検証入力を
/// 連結しない」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorLiteral(String);

impl VectorLiteral {
    /// 検証済みリテラルの本体を借用する（`sql::parser::parse_vector_literal` への
    /// 受け渡し・テストでの突き合わせに使う）。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VectorLiteral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*` に一致するかを検証する（SQL 表層の識別子受理規則と
/// 同じ形状。定数からのみ呼ばれる想定だが、定数だからといって検証を省かない
/// ——「未検証の外部文字列連結経路を作らない」という設計方針そのものを
/// テスト可能にするため）。
fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `f32` のベクトルを SQL-1 のベクトルリテラル形式（`[v1,v2,...]`）へ整形する。
///
/// `{}`（`Display`）で各成分を整形する。指数表記にはならず、`str::parse::<f32>` で
/// 元の値へ往復できる（`sql::parser::parse_vector_literal` が受理する形式と一致する
/// ことは `tests/c1_bench_accept.rs` で確認する）。非有限値（NaN・Inf）は
/// `sql::parser::parse_vector_literal` 側も拒否するため生成時点で fail-closed に
/// 拒否し、無意味な計測（常に構文エラーになるクエリの p95 を測ってしまう事態）を
/// 未然に防ぐ。バイト長が [`MAX_VECTOR_LITERAL_BYTES`] を超える場合も同様に拒否する。
/// 検証は各成分を `write!` で追記するたびに行い、上限超過を検知した時点で成長を
/// 打ち切る（一括アロケーション前の事前検証ではないが、無制限に成長し続けさせない
/// 意味で有界性は保つ。coding-rust.md「untrusted 入力の扱い」）。
pub fn vector_literal(values: &[f32]) -> Result<VectorLiteral, SqlC1Error> {
    if values.iter().any(|v| !v.is_finite()) {
        return Err(SqlC1Error::NonFiniteComponent);
    }
    let mut out = String::from("[");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // `write!` は `String` バッファへの書き込みであり失敗しない
        // （メモリ確保失敗は `alloc` の abort 契約に委ねる。他の harness コードと
        // 同じ前提）。
        let _ = write!(out, "{v}");
        if out.len() > MAX_VECTOR_LITERAL_BYTES {
            return Err(SqlC1Error::LiteralTooLarge);
        }
    }
    out.push(']');
    if out.len() > MAX_VECTOR_LITERAL_BYTES {
        return Err(SqlC1Error::LiteralTooLarge);
    }
    Ok(VectorLiteral(out))
}

/// SQL-1（純粋 Top-k）の規範形クエリ文字列を組み立てる: `SELECT id FROM <table>
/// ORDER BY <column> <=> '<literal>' LIMIT <k>`。
///
/// `table`・`column` は識別子検証を経てから埋め込む（呼び出し元がすべて定数を
/// 渡す構成であっても、未検証文字列連結の経路をコード上に残さないため）。
/// `literal` は [`VectorLiteral`]（[`vector_literal`] でしか構築できない検証済み型）
/// のみを受け付けるため、単一引用符内へ連結される文字列が未検証になる経路は
/// 型レベルで存在しない。
pub fn c1_statement(
    table: &str,
    column: &str,
    literal: &VectorLiteral,
    k: usize,
) -> Result<String, SqlC1Error> {
    if !is_valid_identifier(table) {
        return Err(SqlC1Error::InvalidIdentifier("table"));
    }
    if !is_valid_identifier(column) {
        return Err(SqlC1Error::InvalidIdentifier("column"));
    }
    Ok(format!(
        "SELECT id FROM {table} ORDER BY {column} <=> '{literal}' LIMIT {k}"
    ))
}
