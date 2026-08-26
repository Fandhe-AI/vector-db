//! 簡易クエリプロトコル（'Q'）1 文の実行と応答整形を担う（TASK-73・WIRE-1）。
//!
//! 呼び出し文脈: `handshake::post_auth_loop` が UTF-8 検証済みの `'Q'` 本文を
//! 受け取った直後にここへ委譲する。責務境界は
//! (1) `engine::core::EngineCore::execute_sql_in_session` 呼び出し、
//! (2) 成功／失敗結果の wire メッセージへの整形（実バイト列生成は
//! [`crate::result_encoder`] に委譲）、(3) 接続単位 `SessionState` の
//! 受け渡しのみ。SQL の構文解釈・許可リスト判定・RLS 適用はすべて engine 側
//! （`engine::sql::allowlist::validate_sql`）に委ねる。
//!
//! `INSERT` は wire 経由では受理しない。engine 側の `INSERT`
//! （`sql::exec::execute_insert`）は行を常に `Visibility::Private` で書き込む
//! 固定仕様（TASK-80・SQL-10）である一方、wire 認証経由の `PolicyContext`
//! （`auth::verify` → `PolicyContext::new`）は `Public` のみを許可可視性とする
//! 最小権限の既定を維持している（`wire1_three_tenant_visibility_public_shared_private_hidden`
//! が、認証したテナント自身の `Private` 行も含めて wire 越しには不可視である
//! ことを回帰確認済み。codex-review P1・PR #210 指摘の検討過程で確認）。
//! `INSERT` を wire 側で受理してしまうと、書き込んだ本人にも二度と wire
//! 経由では読めない行を生成することになるため、`engine::sql::allowlist::
//! validate_sql` の許可リストに INSERT 文が含まれないことを利用し、他の
//! 許可外構文と同じ `42601`（fail-closed）で拒否させる。wire 経由での書き込み
//! 系 SQL 対応は、`Private` 行が wire 越しに読める経路の設計が定まってから
//! 改めて着手する。

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

    // `INSERT` は engine の許可リスト（`sql::allowlist::validate_sql`）が
    // 受理しない構文のため、他の許可外構文と同じ `execute_sql_in_session` へ
    // そのまま渡す（`42601` で fail-closed に拒否される。モジュール冒頭コメント
    // 参照）。wire 層でここを分岐して `execute_insert_sql` へ振り分けることは
    // 意図的に行わない。
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
        Err(e) => respond_error_and_ready(stream, e.wire_code(), &e.client_message()),
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
