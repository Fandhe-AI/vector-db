//! 簡易クエリプロトコルの応答メッセージ（`RowDescription`/`DataRow`/
//! `CommandComplete`/`EmptyQueryResponse`）のバイト列生成を担う。
//!
//! 責務境界: 本モジュールは純関数のみを持ち、I/O を一切行わない
//! （`Vec<u8>` を返すのみ）。呼び出し元の [`crate::simple_query`] が
//! `TcpStream` への書き込みを担当する。`engine::sql::exec::{ColumnMeta, Cell,
//! ResultRow}` から wire v3 の text フォーマットへの写像がここに閉じる
//! （TASK-73・WIRE-1）。
//!
//! 型写像（すべて format code 0 = text）:
//! - `ColumnMeta::Id` → `numeric`（OID 1700, typlen -1）。engine の行 ID は
//!   `u64` 全域（`u64::MAX` を含む）を有効値とするため、符号付き 64bit の
//!   `int8`（OID 20）では `i64::MAX` を超える正当な ID を表現できない
//!   （PR #210 レビュー指摘）。10進テキスト表現は `int8`/`numeric` のどちらでも
//!   同一であり、`numeric` として公告することで値域制限なく全 `u64` 値を
//!   そのまま送出できる
//! - `ColumnMeta::Scalar{ty: Text}` → `text`（OID 25, typlen -1）
//! - `ColumnMeta::Scalar{ty: Vector(_)}` → `text`（OID 25。値は `[v1,v2,...]` 形式）
//! - `ColumnMeta::Computed{..}` → `text`（OID 25。実行時型のため text 固定）
//!
//! サイズ安全: フレーム長は `i32::try_from`/`checked_add` で算出し、超過は
//! `Err(EncodeError::FrameTooLarge)` とする（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」。応答側だが同じ規律を踏襲し `as i32` によるオーバー
//! フローを避ける）。

use engine::sql::exec::{Cell, ColumnMeta, ResultRow};

/// 応答メッセージ組み立て時のフレーム長超過エラー。呼び出し元
/// （[`crate::simple_query`]）はこれを内部エラー（`XX000`）として扱い、panic
/// させない。
#[derive(Debug)]
pub struct EncodeError;

/// `ColumnMeta` 1 個の PostgreSQL 型 OID・typlen を返す（本モジュール先頭の
/// 型写像表を参照）。
fn column_type_oid_and_len(meta: &ColumnMeta) -> (i32, i16) {
    match meta {
        ColumnMeta::Id => (1700, -1), // numeric（u64 全域を表現するため int8 ではなく numeric）
        ColumnMeta::Scalar { .. } => (25, -1), // text（Vector も text 表現で返す）
        ColumnMeta::Computed { .. } => (25, -1), // text
    }
}

fn column_name(meta: &ColumnMeta) -> &str {
    match meta {
        ColumnMeta::Id => "id",
        ColumnMeta::Scalar { name, .. } => name.as_str(),
        ColumnMeta::Computed { name } => name.as_str(),
    }
}

/// メッセージ長フィールド（自身の 4 バイトを含む i32）を `checked` に計算する。
/// `crate::error_response`（TASK-153・ERR-1）も同じ `checked` 方式を踏襲するため
/// crate 内に限り公開する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub(crate) fn frame_len(body_len: usize) -> Result<i32, EncodeError> {
    let total = body_len.checked_add(4).ok_or(EncodeError)?;
    i32::try_from(total).map_err(|_| EncodeError)
}

/// `RowDescription`（'T'）を組み立てる。フィールド数は `i16` に収まる必要がある
/// （カタログの列数上限で有界だが、念のため `try_from` で検証する）。
pub fn encode_row_description(columns: &[ColumnMeta]) -> Result<Vec<u8>, EncodeError> {
    let field_count = i16::try_from(columns.len()).map_err(|_| EncodeError)?;
    let mut body = Vec::new();
    body.extend_from_slice(&field_count.to_be_bytes());
    for meta in columns {
        let name = column_name(meta);
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table OID
        body.extend_from_slice(&0i16.to_be_bytes()); // attr number
        let (type_oid, type_len) = column_type_oid_and_len(meta);
        body.extend_from_slice(&type_oid.to_be_bytes());
        body.extend_from_slice(&type_len.to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        body.extend_from_slice(&0i16.to_be_bytes()); // format code (text)
    }
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'T');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `Cell` の text フォーマット表現。`Null` は `None`（`DataRow` の -1 長へ写像）。
///
/// `Cell::Integer` は `u64` 全域（`u64::MAX` を含む）を保持しうる。本モジュール
/// は `id` 列を `numeric`（OID 1700, 型写像表参照）として公告しており、
/// `numeric` の text 表現は符号無し10進整数をそのまま送出してよい（`int8` の
/// ような signed 64bit 制約が無い）ため、`i64` への変換は行わず値域制限なく
/// `to_string()` する（PR #210 レビュー指摘: 旧実装は `i64::try_from` で
/// `i64::MAX` 超の正当な ID を `EncodeError`/`XX000` にしていた）。
fn cell_to_text(cell: &Cell) -> Result<Option<String>, EncodeError> {
    match cell {
        Cell::Null => Ok(None),
        Cell::Integer(v) => Ok(Some(v.to_string())),
        Cell::Text(s) => Ok(Some(s.clone())),
        Cell::Vector(v) => {
            let joined = v
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Ok(Some(format!("[{joined}]")))
        }
        Cell::Float(f) => Ok(Some(f.to_string())),
        Cell::Bool(b) => Ok(Some(if *b { "t".to_string() } else { "f".to_string() })),
    }
}

/// `DataRow`（'D'）を 1 行ぶん組み立てる。`row.cells` は呼び出し元の
/// `QueryResult::columns` と同じ順序・同じ長さであることを engine 側が保証する
/// （`sql::exec::execute_statement` の投影順。ポインタ: TASK-75・SQL-1〜4）。
pub fn encode_data_row(row: &ResultRow) -> Result<Vec<u8>, EncodeError> {
    let field_count = i16::try_from(row.cells.len()).map_err(|_| EncodeError)?;
    let mut body = Vec::new();
    body.extend_from_slice(&field_count.to_be_bytes());
    for cell in &row.cells {
        match cell_to_text(cell)? {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(text) => {
                let bytes = text.as_bytes();
                let len = i32::try_from(bytes.len()).map_err(|_| EncodeError)?;
                body.extend_from_slice(&len.to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'D');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `CommandComplete`（'C'）。`tag` は `SELECT n` / `INSERT 0 1` / `SET` /
/// `CREATE FUNCTION` 等、簡易クエリプロトコルが規定するコマンドタグ文字列。
pub fn encode_command_complete(tag: &str) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::with_capacity(tag.len() + 1);
    body.extend_from_slice(tag.as_bytes());
    body.push(0);
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'C');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `EmptyQueryResponse`（'I'）。body なし・長さ固定（4）。
pub fn encode_empty_query_response() -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(b'I');
    msg.extend_from_slice(&4i32.to_be_bytes());
    msg
}

/// `ErrorResponse`（'E'）を `S`/`C`/`M` の 3 フィールドのみで組み立てる
/// （`severity`/`code`/`message`。他テナント・存在情報は含めない）。
///
/// 呼び出し文脈: `crate::simple_query::execute_and_respond` が「outcome を
/// 決定する区間」の緊急応答チャネル（`engine::recovery::panic_hook::
/// EmergencyResponseRegistration`）へ登録するバイト列を、通常の実行経路
/// （panic フックの外）で事前に組み立てるために使う（panic フック内での
/// エンコードによるアロケーション・整形失敗を避けるため。`panic_hook`
/// モジュールドキュメント参照）。`handshake.rs` の `write_error_response`
/// （ソケットへ直接書き込む版）とはフィールド構成は同じだが、こちらはバイト列を
/// 返すだけで書き込みを行わない別実装（呼び出し文脈が異なるため統合しない）。
///
/// フレーム長の算出は既存 encoder（[`encode_command_complete`] 等）と同じ
/// `checked` 方式（[`frame_len`]）を使い、`as i32` によるオーバーフローを
/// 起こさない（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」。
/// 本関数の入力自体は untrusted ではないが、同じ規律を踏襲する）。
pub fn encode_error_response(sqlstate: &str, message: &str) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"ERROR");
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(sqlstate.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message.as_bytes());
    body.push(0);
    body.push(0); // フィールド終端
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'E');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

// NOTE（TASK-153・対象ビヘイビア ERR-1 で確定化。旧 NOTE は codex-review P1・
// PR #253 指摘対応で「D フィールドは spec 側確定待ちのため送出しない」としていた
// が、TASK-153 のタスク定義自体が `RECOVER-5` (3) 該当時の `state=may_be_committed`
// 条件付き付与の実装を完了条件に含むため、`crate::error_response::
// encode_may_be_committed` として D（detail）フィールド付き encoder を正式実装
// した。`simple_query::build_emergency_response_bytes` は本関数
// （`encode_error_response`）ではなくそちらを呼ぶ。本関数は 3 フィールド
// （`S`/`C`/`M`）のみの通常エラー応答用として引き続き使う。

#[cfg(test)]
mod tests {
    use super::*;

    /// テストのみが使うバイト列アサーションヘルパー（`.claude/rules/
    /// coding-rust.md` の添字アクセス禁止は untrusted 受信入力経路が対象だが、
    /// `[]` の使用箇所をレビュー時に P0 判定と混同されないよう `get()` で統一する）。
    fn byte_at(msg: &[u8], idx: usize) -> u8 {
        *msg.get(idx).expect("message too short")
    }

    fn i32_at(msg: &[u8], idx: usize) -> i32 {
        let bytes: [u8; 4] = msg
            .get(idx..idx + 4)
            .expect("message too short")
            .try_into()
            .expect("slice is exactly 4 bytes");
        i32::from_be_bytes(bytes)
    }

    fn i16_at(msg: &[u8], idx: usize) -> i16 {
        let bytes: [u8; 2] = msg
            .get(idx..idx + 2)
            .expect("message too short")
            .try_into()
            .expect("slice is exactly 2 bytes");
        i16::from_be_bytes(bytes)
    }

    fn slice_at(msg: &[u8], idx: usize, len: usize) -> &[u8] {
        msg.get(idx..idx + len).expect("message too short")
    }

    #[test]
    fn row_description_encodes_id_and_text_columns() {
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
        ];
        let msg = encode_row_description(&columns).expect("encode");
        assert_eq!(byte_at(&msg, 0), b'T');
        // フィールド数は body 先頭の i16（type/len/OID ヘッダ直後の先頭 4 バイトが
        // 'T' + length のため、body はインデックス 5 から始まる）。
        assert_eq!(i16_at(&msg, 5), 2);
    }

    #[test]
    fn data_row_null_cell_has_length_negative_one() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        let msg = encode_data_row(&row).expect("encode");
        // 'D' + length(4) + field_count(2) + cell length(4)
        assert_eq!(i32_at(&msg, 7), -1);
    }

    #[test]
    fn data_row_vector_cell_is_bracketed_text() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Vector(vec![1.0, 2.5])],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, "[1,2.5]");
    }

    #[test]
    fn data_row_bool_cell_encodes_t_or_f() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Bool(true), Cell::Bool(false)],
        };
        let msg = encode_data_row(&row).expect("encode");
        // 先頭 field: 'D' + length(4) + field_count(2) = idx 7 から length(4) + 't'
        let first_len = i32_at(&msg, 7) as usize;
        assert_eq!(first_len, 1);
        assert_eq!(slice_at(&msg, 11, first_len), b"t");
    }

    #[test]
    fn data_row_integer_cell_within_i64_range_encodes_as_decimal() {
        let row = ResultRow {
            id: i64::MAX as u64,
            score: 0.0,
            cells: vec![Cell::Integer(i64::MAX as u64)],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, i64::MAX.to_string());
    }

    #[test]
    fn data_row_integer_cell_beyond_i64_max_encodes_as_decimal() {
        // `id` 列は `numeric`（OID 1700）として公告するため（型写像表参照）、
        // `i64::MAX` を超える正当な `u64` 行 ID も値域制限なくそのまま10進
        // テキストで送出できる（PR #210 レビュー指摘: 旧実装は int8 前提で
        // `EncodeError`/`XX000` にしていた）。
        let row = ResultRow {
            id: u64::MAX,
            score: 0.0,
            cells: vec![Cell::Integer(u64::MAX)],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, u64::MAX.to_string());
    }

    #[test]
    fn command_complete_contains_tag() {
        let msg = encode_command_complete("SELECT 3").expect("encode");
        assert_eq!(byte_at(&msg, 0), b'C');
        assert!(msg.ends_with(b"SELECT 3\0"));
    }

    #[test]
    fn empty_query_response_has_fixed_length_and_no_body() {
        let msg = encode_empty_query_response();
        assert_eq!(msg, vec![b'I', 0, 0, 0, 4]);
    }

    // --- encode_error_response（TASK-97・RECOVER-6、codex-review P1・PR #253 指摘対応）---

    #[test]
    fn error_response_contains_s_c_m_fields_and_terminator_only() {
        let msg = encode_error_response("XX000", "internal error").expect("encode");
        assert_eq!(byte_at(&msg, 0), b'E');

        // body は 'E' + length(4 バイト) の直後（インデックス 5）から始まる。
        let body = slice_at(&msg, 5, msg.len() - 5);
        assert_eq!(body.first().copied(), Some(b'S'));
        assert!(body.windows(6).any(|w| w == b"ERROR\0"));
        assert!(body.windows(6).any(|w| w == b"XX000\0"));
        assert!(body.windows(15).any(|w| w == b"internal error\0"));
        // `D`（detail）フィールドは含まない（ERR-1 のワイヤ形式が spec 側で
        // 未確定のため。上記 NOTE 参照）。
        assert!(!body.contains(&b'D'));
        assert_eq!(
            body.last().copied(),
            Some(0),
            "field terminator (double NUL)"
        );

        // 長さフィールドは i32 で self を含む total_len。
        let declared_len = i32_at(&msg, 1) as usize;
        assert_eq!(
            declared_len,
            msg.len() - 1,
            "length field excludes only the leading 'E' type byte"
        );
    }
}
