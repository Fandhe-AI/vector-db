//! `engine::error_format::ErrorClass` → PostgreSQL wire `ErrorResponse`（'E'）バイト列の
//! 横断写像（TASK-153、対象ビヘイビア: ERR-1。ポインタ: `docs/spec/05-tasks.md` TASK-153・
//! `docs/spec/04-behavior/error-format.md`）。
//!
//! 責務境界: engine 側の `ErrorClass`（`wire_code`・SSOT は `engine::error_format`）を
//! 入力に取り、wire プロトコルの `ErrorResponse` フィールド構成（severity/SQLSTATE/
//! message、および `RECOVER-5` (3) 該当時限定の detail）へ整形するのが本モジュールの
//! 唯一の責務。ソケットへの書き込みは行わず、`Vec<u8>` を返す純関数のみで構成する
//! （`crate::result_encoder` と同じ方針。呼び出し元が I/O を担う）。
//!
//! [`encode`] は通常エラー応答（`S`/`C`/`M` の 3 フィールド）を組み立てる。
//! [`encode_may_be_committed`] は `RECOVER-5` (3)（commit 後 panic）該当時**限定**で
//! `D`（detail）フィールドに `state=may_be_committed` 相当の情報を追加で運ぶ版であり、
//! 呼び出し元は `crate::simple_query` の緊急応答チャネル（`engine::recovery::
//! panic_hook::EmergencyResponseRegistration`）に限定して使うこと（通常エラー経路から
//! 呼ばない。呼び出し面を分けることで誤って通常応答へ `may_be_committed` を混入させる
//! ことを構造的に防ぐ）。
//!
//! フレーム長は [`crate::result_encoder::frame_len`]（`checked` 方式）を再利用し、
//! `as i32` によるオーバーフローを起こさない（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」）。メッセージへの NUL バイト混入はフィールド区切り
//! （NUL 終端）を破壊しフレーム構造を壊すため、[`encode`]・[`encode_may_be_committed`]
//! はいずれも NUL を含む `message` を fail-closed に拒否する（本モジュールの `message`
//! 引数は固定英語文言のみを渡す契約だが、防御的に検証する）。

use engine::error_format::ErrorClass;

use crate::result_encoder::{frame_len, EncodeError};

/// `D`（detail）フィールドに載せる固定文言。`RECOVER-5` (3) 該当時に限り、commit が
/// 成功していたかもしれない（応答未達のまま該当操作が確定した可能性がある）ことを
/// クライアントへ伝える。値の形式（`key=value` の単純テキスト）は TASK-153 が
/// spec の「検討中確定化」として決定したワイヤ形式であり、spec 本文の逐語引用ではない
/// （`.claude/rules/spec-confidentiality.md`）。
const MAY_BE_COMMITTED_DETAIL: &str = "state=may_be_committed";

/// severity フィールド（`S`）の固定値。PostgreSQL wire v3 が規定する語彙のうち
/// 本リポは `ERROR` のみを送出する（`WARNING`/`NOTICE` 等の非エラー応答は簡易
/// クエリプロトコルの他メッセージ種別で表現し、本モジュールの対象外）。
const SEVERITY_ERROR: &str = "ERROR";

/// `message` に NUL バイトが含まれないか検証する（fail-closed）。フィールドは
/// NUL 終端のため、混入するとフレーム構造そのものが壊れる（後続フィールドの
/// 消失・意図しない終端）。
fn reject_embedded_nul(message: &str) -> Result<(), EncodeError> {
    if message.as_bytes().contains(&0) {
        return Err(EncodeError);
    }
    Ok(())
}

/// `ErrorResponse`（'E'）本体のうち `S`/`C`/`M` の 3 フィールドを書き込む。
/// [`encode`]・[`encode_may_be_committed`] の共通部分（末尾のフィールド終端・
/// メッセージ長算出は呼び出し元がそれぞれ行う）。
fn push_s_c_m_fields(body: &mut Vec<u8>, class: ErrorClass, message: &str) {
    body.push(b'S');
    body.extend_from_slice(SEVERITY_ERROR.as_bytes());
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(class.wire_code().as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message.as_bytes());
    body.push(0);
}

/// `body`（フィールド終端込みで組み立て済み）を `ErrorResponse`（'E'）フレームへ
/// 包む。フレーム長は [`frame_len`] の `checked` 方式に従う。
fn wrap_frame(body: Vec<u8>) -> Result<Vec<u8>, EncodeError> {
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'E');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// 通常エラー応答。`S`=`ERROR`・`C`=`class.wire_code()`・`M`=`message` の 3
/// フィールドのみを含む（他テナント・存在情報は含めない契約。`.claude/rules/
/// security.md` P0）。
pub fn encode(class: ErrorClass, message: &str) -> Result<Vec<u8>, EncodeError> {
    reject_embedded_nul(message)?;
    let mut body = Vec::new();
    push_s_c_m_fields(&mut body, class, message);
    body.push(0); // フィールド終端
    wrap_frame(body)
}

/// `RECOVER-5` (3)（commit 後 panic）該当時**限定**の緊急応答。[`encode`] の 3
/// フィールドに加え、`D`（detail）フィールドで [`MAY_BE_COMMITTED_DETAIL`] を運ぶ。
///
/// 呼び出し元は `crate::simple_query` の緊急応答チャネルに限定すること。通常の
/// エラー応答経路（`crate::handshake::write_error_response` 等）からは呼ばない
/// （呼び出し面の分離により、`may_be_committed` が通常応答へ誤って混入すること
/// を構造的に防ぐ）。
pub fn encode_may_be_committed(class: ErrorClass, message: &str) -> Result<Vec<u8>, EncodeError> {
    reject_embedded_nul(message)?;
    let mut body = Vec::new();
    push_s_c_m_fields(&mut body, class, message);
    body.push(b'D');
    body.extend_from_slice(MAY_BE_COMMITTED_DETAIL.as_bytes());
    body.push(0);
    body.push(0); // フィールド終端
    wrap_frame(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// body 中の 1 フィールド（タグ 1 バイト＋NUL 終端文字列）を機械的に抽出する
    /// テスト専用ヘルパー。`.claude/rules/coding-rust.md` の添字アクセス禁止は
    /// untrusted 受信入力経路が対象のため、テストコードでは可読性を優先し
    /// `iter`/`position` ベースで実装する（`unwrap`/`expect` は許容）。
    fn find_field(body: &[u8], tag: u8) -> Option<String> {
        let mut idx = 0;
        while idx < body.len() {
            let this_tag = *body.get(idx)?;
            if this_tag == 0 {
                return None; // フィールド終端に到達
            }
            let value_start = idx + 1;
            let nul_offset = body.get(value_start..)?.iter().position(|&b| b == 0)?;
            let value_end = value_start + nul_offset;
            if this_tag == tag {
                let bytes = body.get(value_start..value_end)?;
                return std::str::from_utf8(bytes).ok().map(str::to_string);
            }
            idx = value_end + 1;
        }
        None
    }

    fn body_of(msg: &[u8]) -> &[u8] {
        // 'E'（1 バイト）+ length（4 バイト）の直後が body。
        msg.get(5..).expect("message too short")
    }

    /// ERR-1: `ErrorClass::ALL` 全件を [`encode`] し、`E` タグ・フレーム長・
    /// `S`=`ERROR`・`C`=各 `wire_code`・`M` 非空・終端 0 を機械的に検証する
    /// （分類追加時も `ErrorClass::ALL` 経由で自動的に網羅が追随する）。
    #[test]
    fn encode_covers_all_error_classes_with_s_c_m_fields() {
        for class in ErrorClass::ALL {
            let msg = encode(class, "test message").expect("encode");
            assert_eq!(msg.first().copied(), Some(b'E'));

            let declared_len = i32::from_be_bytes(
                msg.get(1..5)
                    .expect("length field")
                    .try_into()
                    .expect("4 bytes"),
            ) as usize;
            assert_eq!(
                declared_len,
                msg.len() - 1,
                "length field excludes only the leading 'E' type byte (class={class:?})"
            );

            let body = body_of(&msg);
            assert_eq!(
                find_field(body, b'S').as_deref(),
                Some("ERROR"),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'C').as_deref(),
                Some(class.wire_code()),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'M').as_deref(),
                Some("test message"),
                "class={class:?}"
            );
            // 通常応答は D フィールドを含まない。
            assert!(!body.contains(&b'D'), "class={class:?}");
            assert_eq!(body.last().copied(), Some(0), "field terminator");
        }
    }

    /// [`encode_may_be_committed`] は全分類で `D` フィールドを追加で運ぶ。
    #[test]
    fn encode_may_be_committed_covers_all_error_classes_with_d_field() {
        for class in ErrorClass::ALL {
            let msg = encode_may_be_committed(class, "internal error").expect("encode");
            let body = body_of(&msg);
            assert_eq!(
                find_field(body, b'S').as_deref(),
                Some("ERROR"),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'C').as_deref(),
                Some(class.wire_code()),
                "class={class:?}"
            );
            assert_eq!(
                find_field(body, b'D').as_deref(),
                Some(MAY_BE_COMMITTED_DETAIL),
                "class={class:?}"
            );
            assert_eq!(body.last().copied(), Some(0), "field terminator");
        }
    }

    #[test]
    fn encode_rejects_message_with_embedded_nul() {
        let result = encode(ErrorClass::InternalError, "bad\0message");
        assert!(result.is_err(), "embedded NUL must be rejected fail-closed");
    }

    #[test]
    fn encode_may_be_committed_rejects_message_with_embedded_nul() {
        let result = encode_may_be_committed(ErrorClass::InternalError, "bad\0message");
        assert!(result.is_err(), "embedded NUL must be rejected fail-closed");
    }

    #[test]
    fn encode_and_encode_may_be_committed_differ_only_by_detail_field() {
        let plain = encode(ErrorClass::InternalError, "internal error").expect("encode");
        let committed =
            encode_may_be_committed(ErrorClass::InternalError, "internal error").expect("encode");
        assert!(committed.len() > plain.len());
        assert!(!body_of(&plain).contains(&b'D'));
        assert!(body_of(&committed).contains(&b'D'));
    }
}
