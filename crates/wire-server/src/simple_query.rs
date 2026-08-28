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
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;

use crate::result_encoder;

fn write_all(stream: &mut TcpStream, msg: &[u8]) -> io::Result<()> {
    stream.write_all(msg)
}

/// ErrorResponse を書いてから ReadyForQuery を書く（簡易クエリのエラーは接続を
/// 維持する。WIRE-8 の「切断」は拡張クエリプロトコル限定の既存契約であり、ここは
/// 変更しない）。`class` は `crate::handshake::write_error_response_io`（実体は
/// `crate::error_response::encode`）へそのまま渡り、severity・SQLSTATE の決定を
/// 横断写像へ一元化する（TASK-153・ERR-1・codex-review P1 指摘対応・PR #258）。
fn respond_error_and_ready(
    stream: &mut TcpStream,
    class: ErrorClass,
    message: &str,
) -> io::Result<()> {
    crate::handshake::write_error_response_io(stream, class, message)?;
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

    // TASK-97（対象ビヘイビア: RECOVER-6・ERR-1、codex-review Medium 指摘対応・
    // PR #90）: 登録はブロックスコープで「outcome を決定する区間」だけを覆う
    // ―― ブロック終端（`engine.execute_sql_in_session` の呼び出し直後）で
    // レキシカルに drop され、以降の応答書き込み（`match outcome { .. }` 側）
    // には一切及ばない。これは構造的な安全境界であり、外してはならない ――
    // 将来 commit を伴う書き込み経路が接続された場合、応答書き込みの途中
    // （例: `respond_query_result` が行を書き出している最中）で panic すると、
    // その時点で commit は既に pending 済みのため、もし登録がまだ有効なら
    // 緊急応答バイト列が「書きかけの通常応答フレームの上に」追記されてしまう
    // （[`EmergencyResponseRegistration`] のドキュメントが警告する
    // 「フレーム途中への緊急応答混入・二重応答」そのもの）。`must_use` の
    // 束縛忘れ検出を利用し、`let _ =` に書き換えて即座に drop してしまう事故を
    // 防ぐため、束縛名を `_emergency_registration` とし、`drop()` の明示呼び出し
    // には頼らずブロックの終わりに任せる（呼び出し忘れの手動 `drop` はその後に
    // コードが追加されると孤立しうるが、ブロックスコープはコードの追加位置に
    // 関わらず構造的に保たれる）。
    //
    // 以前は登録を `engine.execute_sql_in_session` の呼び出し 1 行だけに
    // 限定していたが、この区間全体（＝「outcome を決定する区間」）をブロックで
    // 括ることで、将来この区間内に書き込み系 SQL の分岐が追加されても
    // （`EngineCore::execute_sql_in_session` は現状 `SetSearchMode`・
    // `CreateFunction`・`Select`・`Aggregate`・`Explain`（TASK-78・SQL-6。
    // 検索本体を実行しない LLM 展開のみの読み取り専用経路）の読み取り専用
    // 5 分岐のみで commit を伴わない。モジュール冒頭コメント参照）、登録位置を移設せずに
    // そのまま活かせる（codex-review Medium 指摘対応）。区間内で commit 成功後
    // に panic した場合、`engine::recovery::panic_hook` のフックがこの登録済み
    // バイト列を同期的に送出してから abort する（登録が無い・`try_clone` が
    // 失敗した場合は登録せず、既存の接続断側〔RECOVER-5 の abort バック
    // ストップ〕へ fail-closed に倒す）。
    //
    // 登録（eager）と送出（emergency_send_decision による commit 成功フラグの
    // 世代一致判定）は別軸である ―― ここでの登録はブロック内で panic が
    // 起きたら常に送られることを意味しない。詳細は
    // [`build_emergency_response_bytes`] のドキュメント参照。
    //
    // 応答バイト列の内容は `WireError::internal()` の固定文言のみに依存し
    // クエリごとに変化しないため、初回呼び出し時に一度だけ構築してキャッシュ
    // する（[`cached_emergency_response_bytes`] 参照。毎クエリの
    // `WireError::internal()` 構築・エンコード・アロケーションを避ける
    // ―― codex-review Medium 指摘対応）。write timeout も登録時に固定ソケット
    // へ設定せず、`EMERGENCY_RESPONSE_WRITE_TIMEOUT` の値を `register` へ
    // そのまま渡し、panic フック内で緊急応答を書き込む直前にのみ設定する
    // （`panic_hook` モジュールドキュメント参照）。これにより、登録スコープを
    // 抜けた後に `limits::READ_TIMEOUT` へ明示的に復元する処理も不要になった
    // （以前は登録中だけ短いタイムアウトを即時設定していたため必要だった）。
    //
    // `INSERT` は engine の許可リスト（`sql::allowlist::validate_sql`）が
    // 受理しない構文のため、他の許可外構文と同じ `execute_sql_in_session` へ
    // そのまま渡す（`42601` で fail-closed に拒否される。モジュール冒頭コメント
    // 参照）。wire 層でここを分岐して `execute_insert_sql` へ振り分けることは
    // 意図的に行わない。
    let outcome = {
        let _emergency_registration =
            cached_emergency_response_bytes().and_then(|response_bytes| {
                let clone = stream.try_clone().ok()?;
                Some(
                    engine::recovery::panic_hook::EmergencyResponseRegistration::register(
                        response_bytes.clone(),
                        clone,
                        crate::limits::EMERGENCY_RESPONSE_WRITE_TIMEOUT,
                    ),
                )
            });
        engine.execute_sql_in_session(ctx, session, sql)
    };

    match outcome {
        Ok(SqlOutcome::Query(result)) => respond_query_result(stream, &result, "SELECT"),
        // TASK-78（SQL-6）: `EXPLAIN` は検索本体を実行しない別応答だが、行の
        // 形（`QUERY PLAN` 単一列・複数 `Cell::Text` 行）は通常の検索 SELECT と
        // 同じ `RowDescription`/`DataRow` エンコードを再利用できる（`ColumnMeta`/
        // `ResultRow` の汎用性による）。CommandComplete タグのみ pg 互換の
        // `EXPLAIN` に差し替える（`respond_query_result` のタグ引数化。SELECT
        // との違いはこのタグと呼び出し元の分岐のみ）。
        Ok(SqlOutcome::Explain(result)) => respond_query_result(stream, &result, "EXPLAIN"),
        Ok(SqlOutcome::SetSearchMode(_)) => match result_encoder::encode_command_complete("SET") {
            Ok(msg) => {
                write_all(stream, &msg)?;
                crate::handshake::write_ready_for_query_io(stream)
            }
            Err(_) => respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
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
                    ErrorClass::InternalError,
                    "failed to encode command complete response",
                ),
            }
        }
        Err(e) => respond_error_and_ready(stream, e.error_class(), &e.client_message()),
    }
}

/// 緊急応答（TASK-97・RECOVER-6、対象ビヘイビア ERR-1）の事前エンコード済み
/// バイト列を組み立てる。
///
/// `crate::error_response::encode`（TASK-153・ERR-1）で通常応答と同じ `S`/`C`/`M`
/// の 3 フィールドのみの ErrorResponse を組み立てる。クライアントは commit
/// 成功後の panic を「サイレントな接続断」ではなく同期的な ErrorResponse として
/// 観測できる（RECOVER-6 が防ぐ範囲）。「commit は成功しているかもしれない」という
/// 状態情報を運ぶ `D`（detail）フィールドは、wire 形式が spec 側で未確定のため
/// 追加しない（codex-review P1 指摘対応・PR #258。`crate::error_response`
/// モジュールドキュメント参照）。
///
/// `internal_error` は呼び出し元が構築済みの `WireError::internal()` を渡す契約
/// （通常経路の内部エラー応答と同じ固定文言・`wire_code` を使い、文言を二重に
/// 持たない）。
///
/// **本関数はバイト列を組み立てるだけで、送信するかどうかには一切関与しない**
/// （codex-review P1 指摘対応・PR #258）。この事前エンコード済みバイト列は
/// [`cached_emergency_response_bytes`] を経由して `execute_and_respond` の
/// 「outcome を決定する区間」（`engine.execute_sql_in_session` 呼び出しを
/// 含むブロック）の**開始前**に登録されるが、実際に panic フックが
/// これをソケットへ書き込む（送出する）かどうかは
/// `engine::recovery::panic_hook::emergency_send_decision` が別途判定する。
/// 同関数は「このスレッドが commit 成功後・応答未確定の区間にあるか
/// （`engine::recovery::commit_boundary::active_commit_pending_generation` が
/// `Some`）」かつ「その世代が登録時に捕捉した世代と一致するか」の両方を
/// 満たす場合にのみ真を返す。`execute_sql_in_session` の 5 分岐
/// （`SetSearchMode`・`CreateFunction`・`Select`・`Aggregate`・`Explain`）はいずれも
/// `engine::recovery::commit_boundary::commit`／`commit_and_finish` を呼ばない
/// 読み取り専用経路（モジュール冒頭コメント「INSERT は wire 経由では受理
/// しない」参照）であるため、これらの区間で panic しても commit-pending 世代は
/// 立たず `emergency_send_decision` は偽となる ―― 登録は存在するが送出されない
/// （前段フック `previous_hook` へ委譲され、TASK-97 以前と同じ「接続断のみ」に
/// 倒れる）。すなわち「事前に登録される」ことと「実際に送出される」ことは別軸
/// であり、送出可否の唯一の判断材料は panic 発生時点の commit 成功フラグ
/// （世代一致）である。回帰テストは
/// `engine::recovery::panic_hook::tests::
/// try_send_emergency_response_returns_false_when_not_pending_even_if_registered`
/// を参照。
fn build_emergency_response_bytes(
    internal_error: &engine::error_format::WireError,
) -> Result<Vec<u8>, result_encoder::EncodeError> {
    crate::error_response::encode(internal_error.class(), internal_error.message())
}

/// [`build_emergency_response_bytes`] の結果をプロセス生存期間でキャッシュする
/// （TASK-97・RECOVER-6、codex-review Medium 指摘対応・PR #90）。
///
/// `WireError::internal()` の固定文言のみに依存し、クエリごとに内容が変わらない
/// ため、初回呼び出し時に一度だけ構築する。以降の呼び出しは `OnceLock` の読み取り
/// のみで、`WireError::internal()` の構築・エンコード・アロケーションを毎クエリ
/// 発生させない。エンコード自体が失敗した場合（通常発生しない想定）は `None` を
/// キャッシュし、以降も緊急応答チャネルへ登録しない側（fail-closed）に倒れる。
fn cached_emergency_response_bytes() -> Option<&'static Vec<u8>> {
    static CACHE: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let internal_error = engine::error_format::WireError::internal();
            build_emergency_response_bytes(&internal_error).ok()
        })
        .as_ref()
}

/// 検索 SELECT（`command_tag` = `"SELECT"`）・`EXPLAIN`（`command_tag` =
/// `"EXPLAIN"`。TASK-78・SQL-6）いずれの応答整形にも使う共通経路。行の
/// `RowDescription`/`DataRow` エンコードは両者で共通（`ColumnMeta`/`ResultRow`
/// の汎用性による）。`EXPLAIN` の CommandComplete タグは pg 互換で行数を
/// 付けない（`"EXPLAIN"` 固定。検索 SELECT は既存どおり `"SELECT <行数>"`）。
fn respond_query_result(
    stream: &mut TcpStream,
    result: &engine::sql::exec::QueryResult,
    command_tag: &str,
) -> io::Result<()> {
    let row_desc = match result_encoder::encode_row_description(&result.columns) {
        Ok(msg) => msg,
        Err(_) => {
            return respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
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
                    ErrorClass::InternalError,
                    "failed to encode data row",
                )
            }
        };
        write_all(stream, &data_row)?;
    }

    let tag = if command_tag == "EXPLAIN" {
        command_tag.to_string()
    } else {
        format!("{command_tag} {}", result.rows.len())
    };
    match result_encoder::encode_command_complete(&tag) {
        Ok(msg) => {
            write_all(stream, &msg)?;
            crate::handshake::write_ready_for_query_io(stream)
        }
        Err(_) => respond_error_and_ready(
            stream,
            ErrorClass::InternalError,
            "failed to encode command complete response",
        ),
    }
}
