//! 簡易クエリプロトコル（'Q'）1 文の実行と応答整形を担う（TASK-73・WIRE-1）。
//!
//! 呼び出し文脈: `handshake::post_auth_loop` が UTF-8 検証済みの `'Q'` 本文を
//! 受け取った直後にここへ委譲する。責務境界は
//! (1) 文種別（`INSERT` かどうか）による `engine::core::EngineCore` 入口の振り分け、
//! (2) 成功／失敗結果の wire メッセージへの整形（実バイト列生成は
//! [`crate::result_encoder`] に委譲）、(3) 接続単位 `SessionState` の
//! 受け渡しのみ。SQL の構文解釈・許可リスト判定・RLS 適用はすべて engine 側
//! （`engine::sql::allowlist::validate_sql`/`validate_insert`）に委ね、ここでは
//! 一切判定しない（誤判定しても engine が `42601` 等で拒否し fail-closed が
//! 保たれる）。

use std::io::{self, Write};
use std::net::TcpStream;

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;

use crate::result_encoder;

/// SQLSTATE `XX000`（internal_error）。応答メッセージの組み立て自体が失敗した
/// 場合（フレーム長超過等）に用いる。engine が返す SQL エラーの `wire_code()`
/// とは独立した、wire 層自身の内部エラー用コード。
const SQLSTATE_INTERNAL_ERROR: &str = "XX000";

fn write_all(stream: &mut TcpStream, msg: &[u8]) -> io::Result<()> {
    stream.write_all(msg)
}

/// `sql` の先頭トークン（空白・改行を読み飛ばした後、ASCII 大文字小文字非区別）が
/// `INSERT` かどうかを判定する。振り分け専用の軽量判定であり、この判定自体が
/// 誤っていても engine 側の許可リスト検証（fail-closed）が最終的に構文を拒否する
/// ため、安全性はここに依存しない。
fn is_insert_statement(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    let head: String = trimmed.chars().take_while(|c| !c.is_whitespace()).collect();
    head.eq_ignore_ascii_case("INSERT")
}

/// ErrorResponse を書いてから ReadyForQuery を書く（簡易クエリのエラーは接続を
/// 維持する。WIRE-8 の「切断」は拡張クエリプロトコル限定の既存契約であり、ここは
/// 変更しない）。
fn respond_error_and_ready(
    stream: &mut TcpStream,
    sqlstate: &str,
    message: &str,
) -> io::Result<()> {
    crate::handshake::write_error_response_io(stream, sqlstate, message)?;
    crate::handshake::write_ready_for_query_io(stream)
}

/// 簡易クエリ 1 文を実行し、成功／失敗いずれの場合も応答（`ReadyForQuery` 込み）を
/// 書き切る。呼び出し元は UTF-8 検証済みの `sql` のみを渡すこと（バイト列のまま
/// 渡さない。UTF-8 検証は `handshake::post_auth_loop` の責務）。
///
/// SQL 本文・テナント ID はログへ出さない（security.md P0）。
pub(crate) fn execute_and_respond(
    stream: &mut TcpStream,
    engine: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    sql: &str,
) -> io::Result<()> {
    if sql.trim().is_empty() {
        write_all(stream, &result_encoder::encode_empty_query_response())?;
        return crate::handshake::write_ready_for_query_io(stream);
    }

    if is_insert_statement(sql) {
        return match engine.execute_insert_sql(ctx, sql) {
            Ok(outcome) => match result_encoder::encode_command_complete(&format!(
                "INSERT 0 {}",
                outcome.rows_affected
            )) {
                Ok(msg) => {
                    write_all(stream, &msg)?;
                    crate::handshake::write_ready_for_query_io(stream)
                }
                Err(_) => respond_error_and_ready(
                    stream,
                    SQLSTATE_INTERNAL_ERROR,
                    "failed to encode command complete response",
                ),
            },
            Err(e) => respond_error_and_ready(stream, e.wire_code(), &e.to_string()),
        };
    }

    match engine.execute_sql_in_session(ctx, session, sql) {
        Ok(SqlOutcome::Query(result)) => respond_query_result(stream, &result),
        Ok(SqlOutcome::SetSearchMode(_)) => match result_encoder::encode_command_complete("SET") {
            Ok(msg) => {
                write_all(stream, &msg)?;
                crate::handshake::write_ready_for_query_io(stream)
            }
            Err(_) => respond_error_and_ready(
                stream,
                SQLSTATE_INTERNAL_ERROR,
                "failed to encode command complete response",
            ),
        },
        Ok(SqlOutcome::CreateFunction { .. }) => {
            match result_encoder::encode_command_complete("CREATE FUNCTION") {
                Ok(msg) => {
                    write_all(stream, &msg)?;
                    crate::handshake::write_ready_for_query_io(stream)
                }
                Err(_) => respond_error_and_ready(
                    stream,
                    SQLSTATE_INTERNAL_ERROR,
                    "failed to encode command complete response",
                ),
            }
        }
        Err(e) => respond_error_and_ready(stream, e.wire_code(), &e.to_string()),
    }
}

fn respond_query_result(
    stream: &mut TcpStream,
    result: &engine::sql::exec::QueryResult,
) -> io::Result<()> {
    let row_desc = match result_encoder::encode_row_description(&result.columns) {
        Ok(msg) => msg,
        Err(_) => {
            return respond_error_and_ready(
                stream,
                SQLSTATE_INTERNAL_ERROR,
                "failed to encode row description",
            )
        }
    };
    write_all(stream, &row_desc)?;

    for row in &result.rows {
        let data_row = match result_encoder::encode_data_row(row) {
            Ok(msg) => msg,
            Err(_) => {
                return respond_error_and_ready(
                    stream,
                    SQLSTATE_INTERNAL_ERROR,
                    "failed to encode data row",
                )
            }
        };
        write_all(stream, &data_row)?;
    }

    let tag = format!("SELECT {}", result.rows.len());
    match result_encoder::encode_command_complete(&tag) {
        Ok(msg) => {
            write_all(stream, &msg)?;
            crate::handshake::write_ready_for_query_io(stream)
        }
        Err(_) => respond_error_and_ready(
            stream,
            SQLSTATE_INTERNAL_ERROR,
            "failed to encode command complete response",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::is_insert_statement;

    #[test]
    fn detects_insert_case_insensitively_with_leading_whitespace() {
        assert!(is_insert_statement("  insert into t values (1)"));
        assert!(is_insert_statement("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn does_not_detect_select_as_insert() {
        assert!(!is_insert_statement("SELECT * FROM t"));
        assert!(!is_insert_statement(""));
    }
}
