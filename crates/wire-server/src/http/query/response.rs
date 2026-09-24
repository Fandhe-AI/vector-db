//! `QueryResult`（`search`／`scan`／`aggregate` 成功時。engine 側 SQL 表層の
//! 結果セット形状。ポインタ: SQL-1〜4・SQL-13〜15）→ NoSQL 表層の JSON 応答本文
//! `{"columns":[{"name":...,"type":...}],"rows":[[...],...],"row_count":n}`
//! への写像（Issue #762。対象ビヘイビア TASK-181・NOSQL-11。ポインタ:
//! `docs/spec/05-tasks.md` TASK-181・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-11）。
//!
//! 責務境界: [`crate::http::error_body`] の成功系対応物であり、同じく
//! 「本文 `String` を返すまで」に閉じる純関数を提供する。ステータス行・
//! `Connection: close`・`Content-Type`／`Content-Length`・CRLF の組み立てや
//! ソケット I/O は含まない（応答エンベロープは #746 の責務）。
//!
//! `explain: true` 時の `{"explain":["<行>", ...]}` 応答は [`encode_explain`]
//! が担う（TASK-186・NOSQL-10。Issue #765）。`encode`（`columns`／`rows`／
//! `row_count`）とは独立の関数として分離し、`search`／`scan`／`aggregate` の
//! 通常応答へ影響を与えない。
//!
//! スコープ外（隣接 Issue と二重実装しない）:
//! - `score` の送出有無・`columns` 指定に応じた投影の絞り込み（#763・#766。
//!   本モジュールは `QueryResult.columns`／`rows[].cells` をそのまま写像し、
//!   `ResultRow.score`／`id` を独自に付加しない）
//! - 接続ハンドラからの呼び出し結線（#747／#758 以降）
//!
//! `columns[].type` の型名は SQL 表層の `RowDescription`
//! （[`crate::result_encoder::encode_row_description`]）が公告する PostgreSQL
//! 型名と**同一の対応表**（[`crate::result_encoder::WireType`]）を共有する。
//! これにより `VECTOR` 列・`Computed`（式項目）列は wire 側と同じく
//! `"text"` として公告される一方、値そのものは wire の text フォーマットでは
//! なく native JSON（配列・数値・真偽値）で返す——**型名は wire 側との
//! 同一性を、値表現は JSON としてのロスレス性をそれぞれ優先する意図的な
//! 非対称**であり、値表現を型名に合わせて文字列化する「修正」はしないこと。
//!
//! 値表現（`Cell` → JSON）:
//! - `Cell::Null` → `null`
//! - `Cell::Integer(u64)` → JSON number（10進テキスト。`u64::MAX` まで桁落ち
//!   なく表現できるが、2^53 を超える値を JS 系クライアントが `JSON.parse`
//!   すると仮数部の精度により丸められうる。文字列化への変更は spec 側判断
//!   に委ねる〔TASK-185〕。表現自体は RFC 8259 上有効な JSON number）
//! - `Cell::Float(f64)` → JSON number。**非有限（NaN／±∞）は `null` へ丸め
//!   ず `Err` で fail-closed**（`"NaN"`／`"Infinity"` は妥当な JSON number
//!   ではないため）。engine は評価時に非有限を `22000` で拒否する契約だが、
//!   本モジュールは多層防御として同じ制約を持つ
//! - `Cell::Bool(bool)` → `true`／`false`（native JSON 真偽値）
//! - `Cell::Text(String)` → JSON string。[`crate::http::error_body::
//!   escape_json_string_into`] を列名と共通で再利用する
//! - `Cell::Vector(Vec<f32>)` → JSON array of number（要素は `f32::to_string()`。
//!   `crate::result_encoder::cell_to_text` の `[1,2.5]` 表現と要素の数値表記が
//!   一致する）。要素の非有限も同様に `Err`
//! - `Cell::Bytes(Vec<u8>)` → JSON string（標準 base64・パディングあり。
//!   `crate::http::query::base64_std::encode_base64_std`。wire 側の `\x` 16 進
//!   テキスト表現とは異なる——値表現は表層ごとの規約〔B7〕であり、`BYTEA` に
//!   限り型名の wire 側同一性より JSON との親和性を優先する。Issue #886）
//!
//! 出力不変条件: [`crate::http::error_body::encode`] と同じくキー順固定
//! （`columns` → `rows` → `row_count`）・空白なし・0x20 未満のバイトを含まない
//! （`Content-Length`〔#746〕算出対象を安定させる）。

use std::fmt::Write as _;

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};

use crate::http::error_body::escape_json_string_into;
use crate::result_encoder::column_wire_type;

/// [`encode`] の失敗を表す（`Cell::Float`／`Cell::Vector` の非有限要素が
/// JSON で表現できない場合のみ発生する）。他テナント情報・内部詳細を運ばない
/// ため `Debug` のみ持つ（`crate::result_encoder::EncodeError` と同じ方針）。
#[derive(Debug)]
pub struct ResponseEncodeError;

impl ClassifiedError for ResponseEncodeError {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::InternalError
    }

    fn client_message(&self) -> String {
        // `ErrorClass::InternalError` は `WireError::new` 側で固定文言へ
        // 差し替えられる契約（`engine::error_format` 参照）のため、ここでの
        // 文言自体は到達しても情報漏えいにならないが、内部詳細（どの列・
        // どの値が非有限だったか）は含めない。
        "internal error".to_string()
    }
}

/// 1 セルぶんの JSON 値表現を `out` へ追記する。
fn write_cell(out: &mut String, cell: &Cell) -> Result<(), ResponseEncodeError> {
    match cell {
        Cell::Null => {
            out.push_str("null");
            Ok(())
        }
        Cell::Integer(v) => {
            // `write!` への `String` 追記は infallible（アロケーション失敗
            // 以外で失敗しない）ため戻り値は捨ててよい（`error_body.rs` と
            // 同じ方針）。
            let _ = write!(out, "{v}");
            Ok(())
        }
        Cell::Float(f) => write_finite_f64(out, *f),
        Cell::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            Ok(())
        }
        Cell::Text(s) => {
            out.push('"');
            escape_json_string_into(out, s);
            out.push('"');
            Ok(())
        }
        Cell::Vector(values) => {
            out.push('[');
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_finite_f32(out, *v)?;
            }
            out.push(']');
            Ok(())
        }
        Cell::Array(array_value) => {
            use engine::row_codec::ArrayValue;
            out.push('[');
            match array_value {
                ArrayValue::Text(items) => {
                    for (i, s) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push('"');
                        escape_json_string_into(out, s);
                        out.push('"');
                    }
                }
                ArrayValue::Bool(items) => {
                    for (i, b) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push_str(if *b { "true" } else { "false" });
                    }
                }
            }
            out.push(']');
            Ok(())
        }
        Cell::Bytes(bytes) => {
            // `BYTEA` の JSON 表現は標準 base64（RFC 4648 §4・パディングあり。
            // B7・Issue #886）。base64 のアルファベットは JSON エスケープ不要。
            out.push('"');
            out.push_str(&crate::http::query::base64_std::encode_base64_std(bytes));
            out.push('"');
            Ok(())
        }
    }
}

/// 有限 `f64` を JSON number として書く。非有限（NaN／±∞）は fail-closed に
/// `Err` とする（JSON はこれらを表現できないため。モジュールドキュメント参照）。
fn write_finite_f64(out: &mut String, f: f64) -> Result<(), ResponseEncodeError> {
    if !f.is_finite() {
        return Err(ResponseEncodeError);
    }
    let _ = write!(out, "{f}");
    Ok(())
}

/// [`write_finite_f64`] の `f32`（`Cell::Vector` の要素）版。
fn write_finite_f32(out: &mut String, f: f32) -> Result<(), ResponseEncodeError> {
    if !f.is_finite() {
        return Err(ResponseEncodeError);
    }
    let _ = write!(out, "{f}");
    Ok(())
}

/// `columns[]` の 1 要素 `{"name":"...","type":"..."}` を `out` へ追記する。
/// `name` はカタログ由来の untrusted 文字列（DDL でユーザーが指定した列名）
/// のため [`Cell::Text`] と同じエスケーパを通す。
fn write_column(out: &mut String, meta: &ColumnMeta) {
    out.push_str("{\"name\":\"");
    escape_json_string_into(out, crate::result_encoder::column_name(meta));
    out.push_str("\",\"type\":\"");
    // `pg_type_name()` は固定 ASCII（`"numeric"`／`"text"`）のためエスケープ
    // 不要だが、型写像表が将来拡張されても安全側に倒れるよう一律エスケーパを
    // 通す（`error_body.rs::wire_code_and_label_never_require_escaping` と
    // 同じ考え方）。
    escape_json_string_into(out, column_wire_type(meta).pg_type_name());
    out.push_str("\"}");
}

/// `QueryResult` → `{"columns":[...],"rows":[...],"row_count":n}`
/// （キー順固定・空白なしのコンパクト形）。
///
/// `row_count` は常に `result.rows.len()` と一致する（別経路で算出しない）。
/// 事前確保は行数 × 定数の概算ヒントに留め、無制限確保はしない
/// （`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub fn encode(result: &QueryResult) -> Result<String, ResponseEncodeError> {
    // 概算ヒント: 列 1 個あたり約 24 バイト（name+type の固定オーバーヘッド）、
    // 行 1 個あたり列数 × 8 バイト（数値・区切り文字の目安）。実データが
    // これを超えても `String` は再確保して継続するだけであり、上限として
    // 機能するものではない（無制限確保の防止は呼び出し元・engine 側の
    // `LIMIT` 契約が担う）。
    let capacity_hint = 32
        + result.columns.len().saturating_mul(24)
        + result
            .rows
            .len()
            .saturating_mul(result.columns.len().saturating_mul(8).max(8));
    let mut out = String::with_capacity(capacity_hint);

    out.push_str("{\"columns\":[");
    for (i, meta) in result.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_column(&mut out, meta);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in result.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, cell) in row.cells.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            write_cell(&mut out, cell)?;
        }
        out.push(']');
    }
    out.push_str("],\"row_count\":");
    let _ = write!(out, "{}", result.rows.len());
    out.push('}');

    Ok(out)
}

/// `explain: true` の `search` 要求が返す `EXPLAIN` 応答本文
/// `{"explain":["<行>", ...]}`（TASK-186・NOSQL-10。Issue #765）への写像。
///
/// `result` は [`crate::core::EngineCore::explain_bound_plan_in_session`]（SQL
/// `EXPLAIN SELECT ... USING PLAN(...)` の `Statement::Explain` アームと同一の
/// 私的ヘルパーを共有する。`crate::core::EngineCore` モジュールドキュメント参照）
/// の戻り値をそのまま渡す想定で、列が `Computed { name: "QUERY PLAN" }` 1 本・
/// 各行が [`Cell::Text`] 1 個であることを検証してから写像する。この形状
/// （`sql::explain::build_explain_result` の固定契約）から逸脱する場合は
/// best-effort に描画せず `Err`（`ErrorClass::InternalError`）で fail-closed に
/// 拒否する。
pub fn encode_explain(result: &QueryResult) -> Result<String, ResponseEncodeError> {
    if result.columns.len() != 1
        || result.columns[0]
            != (ColumnMeta::Computed {
                name: "QUERY PLAN".to_string(),
            })
    {
        return Err(ResponseEncodeError);
    }

    let capacity_hint = 16
        + result
            .rows
            .len()
            .saturating_mul(48 /* 行文字列の目安長 + 区切り文字 */);
    let mut out = String::with_capacity(capacity_hint);
    out.push_str("{\"explain\":[");
    for (i, row) in result.rows.iter().enumerate() {
        if row.cells.len() != 1 {
            return Err(ResponseEncodeError);
        }
        let Cell::Text(text) = &row.cells[0] else {
            return Err(ResponseEncodeError);
        };
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        escape_json_string_into(&mut out, text);
        out.push('"');
    }
    out.push_str("]}");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::ColumnType;
    use engine::json::{parse_json, JsonValue};
    use engine::sql::exec::ResultRow;
    use std::collections::BTreeMap;

    fn row(cells: Vec<Cell>) -> ResultRow {
        ResultRow {
            id: 1,
            score: 0.0,
            cells,
        }
    }

    /// トップレベルを `{"columns": Array, "rows": Array, "row_count": Number}`
    /// として構造的に解析するテスト専用ヘルパー。`unwrap`/`expect` はテスト
    /// コードでは許容する（`error_body.rs` の tests と同じ方針）。
    fn parse_top(body: &str) -> BTreeMap<String, JsonValue> {
        let parsed = parse_json(body).expect("valid JSON");
        let JsonValue::Object(top) = parsed else {
            panic!("top level must be an object");
        };
        top
    }

    fn all_column_kinds() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: ColumnType::Text,
            },
            ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: ColumnType::Vector(2),
            },
            ColumnMeta::Computed {
                name: "n".to_string(),
            },
        ]
    }

    #[test]
    fn round_trips_all_column_and_cell_kinds() {
        let result = QueryResult {
            columns: all_column_kinds(),
            rows: vec![row(vec![
                Cell::Integer(42),
                Cell::Text("ja".to_string()),
                Cell::Vector(vec![1.0, 2.5]),
                Cell::Float(3.5),
            ])],
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);

        let JsonValue::Array(columns) = &top["columns"] else {
            panic!("columns must be an array");
        };
        assert_eq!(columns.len(), 4);
        let names: Vec<&str> = columns
            .iter()
            .map(|c| {
                let JsonValue::Object(obj) = c else {
                    panic!("column must be an object");
                };
                let JsonValue::String(name) = &obj["name"] else {
                    panic!("name must be a string");
                };
                name.as_str()
            })
            .collect();
        assert_eq!(names, ["id", "lang", "embedding", "n"]);

        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        assert_eq!(rows.len(), 1);
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        assert_eq!(cells.len(), 4);
        assert!(matches!(&cells[0], JsonValue::Number(n) if n.as_f64() == 42.0));
        assert!(matches!(&cells[1], JsonValue::String(s) if s == "ja"));
        let JsonValue::Array(vec_cell) = &cells[2] else {
            panic!("vector cell must be an array");
        };
        assert_eq!(vec_cell.len(), 2);
        assert!(matches!(&cells[3], JsonValue::Number(n) if n.as_f64() == 3.5));

        assert!(matches!(&top["row_count"], JsonValue::Number(n) if n.as_f64() == 1.0));
    }

    /// 型名一致テーブルテスト: 各 `ColumnMeta` について `encode_row_description`
    /// のバイト列から OID を読み取り、`WireType::from_oid` 相当の固定表
    /// （テスト側で独立に持つ）と JSON `type` が一致することを固定する
    /// （PR 計画 5.1「型名一致テーブルテスト」）。
    #[test]
    fn column_type_matches_row_description_oid_for_every_column_kind() {
        let expected: [(&str, i32); 4] = [("id", 1700), ("lang", 25), ("embedding", 25), ("n", 25)];
        let columns = all_column_kinds();
        assert_eq!(
            expected.len(),
            columns.len(),
            "期待表の長さが ColumnMeta 種別数と一致すること"
        );

        let row_description =
            crate::result_encoder::encode_row_description(&columns).expect("encode");
        let result = QueryResult {
            columns,
            rows: Vec::new(),
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(json_columns) = &top["columns"] else {
            panic!("columns must be an array");
        };

        // RowDescription body: 'T' + length(4) + field_count(2) から各フィールド
        // が続く。各フィールドは name(NUL 終端) + table_oid(4) + attnum(2) +
        // type_oid(4) + typlen(2) + typmod(4) + format(2)。
        let mut cursor = 1 + 4 + 2; // 'T' + length + field_count
        for (i, (name, expected_oid)) in expected.iter().enumerate() {
            let name_end = row_description[cursor..]
                .iter()
                .position(|&b| b == 0)
                .expect("NUL terminator");
            let actual_name = std::str::from_utf8(&row_description[cursor..cursor + name_end])
                .expect("utf8 name");
            assert_eq!(&actual_name, name);
            cursor += name_end + 1; // NUL を含めて読み飛ばす
            cursor += 4 + 2; // table_oid + attnum
            let oid_bytes: [u8; 4] = row_description[cursor..cursor + 4]
                .try_into()
                .expect("4 bytes");
            let oid = i32::from_be_bytes(oid_bytes);
            assert_eq!(oid, *expected_oid, "column={name}");
            cursor += 4; // type_oid
            cursor += 2 + 4 + 2; // typlen + typmod + format

            let expected_type_name = crate::result_encoder::WireType::from_oid(oid)
                .expect("known oid")
                .pg_type_name();
            let JsonValue::Object(col_obj) = &json_columns[i] else {
                panic!("column must be an object");
            };
            let JsonValue::String(actual_type) = &col_obj["type"] else {
                panic!("type must be a string");
            };
            assert_eq!(actual_type, expected_type_name, "column={name}");
        }
    }

    #[test]
    fn row_count_equals_rows_len_for_empty_single_and_multiple_rows() {
        for n in [0usize, 1, 3] {
            let result = QueryResult {
                columns: vec![ColumnMeta::Id],
                rows: (0..n as u64)
                    .map(|id| ResultRow {
                        id,
                        score: 0.0,
                        cells: vec![Cell::Integer(id)],
                    })
                    .collect(),
            };
            let body = encode(&result).expect("encode");
            let top = parse_top(&body);
            assert!(
                matches!(&top["row_count"], JsonValue::Number(v) if v.as_f64() == n as f64),
                "n={n}"
            );
            let JsonValue::Array(rows) = &top["rows"] else {
                panic!("rows must be an array");
            };
            assert_eq!(rows.len(), n);
        }
    }

    #[test]
    fn zero_column_result_has_empty_columns_and_empty_row_arrays() {
        let result = QueryResult {
            columns: Vec::new(),
            rows: vec![row(Vec::new())],
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(columns) = &top["columns"] else {
            panic!("columns must be an array");
        };
        assert!(columns.is_empty());
        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        assert_eq!(rows.len(), 1);
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        assert!(cells.is_empty());
    }

    #[test]
    fn golden_string_is_fixed() {
        let result = QueryResult {
            columns: vec![
                ColumnMeta::Id,
                ColumnMeta::Scalar {
                    name: "lang".to_string(),
                    ty: ColumnType::Text,
                },
            ],
            rows: vec![row(vec![Cell::Integer(1), Cell::Text("ja".to_string())])],
        };
        let body = encode(&result).expect("encode");
        assert_eq!(
            body,
            "{\"columns\":[{\"name\":\"id\",\"type\":\"numeric\"},\
{\"name\":\"lang\",\"type\":\"text\"}],\"rows\":[[1,\"ja\"]],\"row_count\":1}"
        );
    }

    #[test]
    fn encode_is_deterministic() {
        let result = QueryResult {
            columns: all_column_kinds(),
            rows: vec![row(vec![
                Cell::Null,
                Cell::Text("x".to_string()),
                Cell::Vector(vec![1.0]),
                Cell::Bool(true),
            ])],
        };
        assert_eq!(encode(&result).unwrap(), encode(&result).unwrap());
    }

    /// U+0000〜U+001F 全走査＋`"`／`\` を含む Text セルと列名の両方を
    /// エスケープし、出力に 0x20 未満のバイトが無いこと・往復一致を確認する。
    #[test]
    fn escapes_control_chars_in_text_cell_and_column_name() {
        let mut message = String::new();
        for cp in 0u32..=0x1F {
            let ch = char::from_u32(cp).expect("valid control char codepoint");
            message.push(ch);
        }
        message.push('"');
        message.push('\\');

        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: message.clone(),
                ty: ColumnType::Text,
            }],
            rows: vec![row(vec![Cell::Text(message.clone())])],
        };
        let body = encode(&result).expect("encode");
        assert!(
            body.bytes().all(|b| b >= 0x20),
            "body must not contain raw bytes below 0x20: {body:?}"
        );

        let top = parse_top(&body);
        let JsonValue::Array(columns) = &top["columns"] else {
            panic!("columns must be an array");
        };
        let JsonValue::Object(col_obj) = &columns[0] else {
            panic!("column must be an object");
        };
        assert_eq!(col_obj["name"], JsonValue::String(message.clone()));

        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        assert_eq!(cells[0], JsonValue::String(message));
    }

    #[test]
    fn non_finite_float_cell_is_rejected() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let result = QueryResult {
                columns: vec![ColumnMeta::Computed {
                    name: "n".to_string(),
                }],
                rows: vec![row(vec![Cell::Float(f)])],
            };
            let err = encode(&result).expect_err("non-finite float must be rejected");
            assert_eq!(
                ClassifiedError::error_class(&err),
                ErrorClass::InternalError
            );
        }
    }

    #[test]
    fn non_finite_vector_element_is_rejected() {
        for f in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let result = QueryResult {
                columns: vec![ColumnMeta::Scalar {
                    name: "embedding".to_string(),
                    ty: ColumnType::Vector(1),
                }],
                rows: vec![row(vec![Cell::Vector(vec![f])])],
            };
            let err = encode(&result).expect_err("non-finite vector element must be rejected");
            assert_eq!(
                ClassifiedError::error_class(&err),
                ErrorClass::InternalError
            );
        }
    }

    #[test]
    fn u64_max_integer_cell_has_no_precision_loss_in_decimal_text() {
        // `engine::json::JsonNumber::PosInt` は `u64::MAX` を無損失に表現できる
        // ため（Issue #823 レビュー指摘対応）`parse_json` 経由の往復でも精度は
        // 失われないが、本テストは `encode` が生成する 10 進テキスト自体が
        // `u64::MAX` と一致することを検証する（JSON パーサの実装詳細に
        // 依存しない、応答文字列そのものに対する直接検証）。
        let result = QueryResult {
            columns: vec![ColumnMeta::Id],
            rows: vec![row(vec![Cell::Integer(u64::MAX)])],
        };
        let body = encode(&result).expect("encode");
        assert!(body.contains(",\"row_count\":1}"));
        assert!(body.contains(&u64::MAX.to_string()));
    }

    #[test]
    fn vector_cell_element_text_matches_result_encoder_bracketed_form() {
        let values = vec![1.0f32, 2.5, -3.0];
        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: ColumnType::Vector(3),
            }],
            rows: vec![row(vec![Cell::Vector(values.clone())])],
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        let JsonValue::Array(vec_cell) = &cells[0] else {
            panic!("vector cell must be an array");
        };
        let numbers: Vec<f64> = vec_cell
            .iter()
            .map(|v| match v {
                JsonValue::Number(n) => n.as_f64(),
                other => panic!("expected number, got {other:?}"),
            })
            .collect();
        let expected: Vec<f64> = values.iter().map(|f| *f as f64).collect();
        assert_eq!(numbers, expected);
    }
}
