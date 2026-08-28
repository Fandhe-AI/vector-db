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
    // commit 成功から本関数が応答を書き終える（`ReadyForQuery` 送出含む）までの
    // 区間全体を覆う RAII ガード（RECOVER-5 (3)・codex-review P1・PR #246 指摘対応）。
    // 区間内で engine 側の書き込み系 commit が成功すると
    // `engine::recovery::commit_boundary` 内部のスレッドローカルフラグが立ち、
    // 本関数の全 return 経路（正常 return・panic 伝播いずれも）でこのガードが
    // drop される際に、フラグが立ったまま unwind 中であればプロセスを abort する
    // （wire-server は接続 1 本につきスレッド 1 つが直列にクエリを処理する
    // thread-per-connection モデルのため、スレッドローカルでの受け渡しが成立する。
    // `engine::recovery::commit_boundary` モジュールドキュメント参照）。
    // 注意: `_response_boundary` を `let _ = ...`（無名束縛）に書き換えると即座に
    // drop され、この保護区間全体が無効化される（`ResponseBoundaryGuard` は
    // `#[must_use]`。関数末尾まで生存させるため必ずこの名前付き束縛のまま保つ）。
    let _response_boundary = engine::recovery::commit_boundary::ResponseBoundaryGuard::new();

    if sql.trim().is_empty() {
        write_all(stream, &result_encoder::encode_empty_query_response())?;
        return crate::handshake::write_ready_for_query_io(stream);
    }

    // TASK-97（対象ビヘイビア: RECOVER-6・ERR-1）: `engine.execute_sql_in_session`
    // の呼び出し区間だけを緊急応答チャネルの登録で包む。区間内で commit 成功後に
    // panic した場合、`engine::recovery::panic_hook` のフックがこの登録済み
    // バイト列を同期的に送出してから abort する（登録が無い・`try_clone`/
    // write timeout 設定が失敗した場合は登録せず、既存の接続断側〔RECOVER-5 の
    // abort バックストップ〕へ fail-closed に倒す）。応答バイト列は panic フック
    // 内でのアロケーション・整形失敗を避けるためここで事前エンコードする。
    // `WireError::internal()` の固定文言をそのまま使うことで、通常経路の内部
    // エラー応答と緊急応答の文言が構造的に一致する（別々に文字列リテラルを
    // 持たない）。
    let internal_error = engine::error_format::WireError::internal();
    let emergency_registration = build_emergency_response_bytes(&internal_error)
        .ok()
        .and_then(|response_bytes| {
            let clone = stream.try_clone().ok()?;
            clone
                .set_write_timeout(Some(crate::limits::EMERGENCY_RESPONSE_WRITE_TIMEOUT))
                .ok()?;
            Some(
                engine::recovery::panic_hook::EmergencyResponseRegistration::register(
                    response_bytes,
                    clone,
                ),
            )
        });

    // `INSERT` は engine の許可リスト（`sql::allowlist::validate_sql`）が
    // 受理しない構文のため、他の許可外構文と同じ `execute_sql_in_session` へ
    // そのまま渡す（`42601` で fail-closed に拒否される。モジュール冒頭コメント
    // 参照）。wire 層でここを分岐して `execute_insert_sql` へ振り分けることは
    // 意図的に行わない。
    let outcome = engine.execute_sql_in_session(ctx, session, sql);

    // 緊急応答チャネルの登録解除（RAII の明示 drop）。engine 呼び出しから
    // 戻った直後 = ここより後は通常応答の組み立て・送信区間であり、緊急応答は
    // 送らない（`panic_hook` モジュールドキュメント参照）。
    drop(emergency_registration);
    // `TcpStream::try_clone` は同一ソケットの複製であり、クローン側で設定した
    // write timeout（`SO_SNDTIMEO`）はソケット共有のため元の `stream` 側にも
    // 反映される。この接続の write timeout は `server.rs` が受理直後に
    // `limits::apply_read_timeout` で一度だけ `limits::READ_TIMEOUT` に設定し、
    // 他に変更箇所がない（`grep -rn set_write_timeout crates/wire-server/src`
    // で確認済み）ため、ここでの復元先は「以前の値」の近似ではなく厳密に正しい
    // 元の値である。通常応答の書き込みが緊急応答用の短いタイムアウトのまま
    // 行われないよう、この関数から戻る前に必ず復元する（設定失敗は無視する ――
    // 失敗しても以降のタイムアウトが短めに働くだけで安全側にしか倒れない）。
    let _ = stream.set_write_timeout(Some(crate::limits::READ_TIMEOUT));

    match outcome {
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

/// 緊急応答（TASK-97・RECOVER-6）の事前エンコード済みバイト列を組み立てる。
/// `state=may_be_committed` の detail フィールドを付与した ErrorResponse
/// （`'D'` フィールドの暫定運び方は `result_encoder::encode_error_response_with_detail`
/// のドキュメント参照。ERR-1 のワイヤ形式確定前の暫定実装）。
///
/// `internal_error` は呼び出し元が構築済みの `WireError::internal()` を渡す契約
/// （通常経路の内部エラー応答と同じ固定文言・`wire_code` を使い、文言を二重に
/// 持たない）。
fn build_emergency_response_bytes(
    internal_error: &engine::error_format::WireError,
) -> Result<Vec<u8>, result_encoder::EncodeError> {
    result_encoder::encode_error_response_with_detail(
        internal_error.wire_code(),
        internal_error.message(),
        "state=may_be_committed",
    )
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
