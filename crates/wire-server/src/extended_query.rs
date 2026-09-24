//! 拡張クエリプロトコルの Parse（'P'）・Describe（'D' 種別 S）を受理する
//! （Issue #933・TASK-71・WIRE-11）。
//!
//! `handshake::post_auth_loop` から、engine が接続済み（`Some`）の場合にのみ
//! 呼ばれる（`engine: None` の場合は従来どおり `protocol_dispatch::
//! reject_and_close` へ流す。本モジュールドキュメント末尾参照）。接続単位の
//! [`PreparedStatementStore`]（`handshake` の接続ループが `SessionState` と並べて
//! 所有し、接続終了で破棄——接続間・テナント間で共有しない）へ Parse 済みの
//! [`engine::core::ParsedSql`] を保持し、Describe がそこから
//! [`engine::core::EngineCore::describe_parsed_in_session`] を呼んで結果列
//! メタデータのみを返す。
//!
//! **WIRE-11 確定までの暫定契約**（#934 で置換予定）: Parse／Describe の失敗は
//! ErrorResponse 送出後に本 Issue 時点では同期回復（Sync までの破棄→
//! ReadyForQuery）を持たないため、`protocol_dispatch::drain_and_close` による
//! 有界 lingering close で接続を終了する（WIRE-8 が採用していた設計をそのまま
//! 踏襲）。Bind（'B'）・Execute（'E'）・Sync（'S'）・Close（'C'）・
//! Flush（'H'）と、Describe の portal 対象（種別 'P'。portal はこの Issue の
//! 範囲では構築され得ない）はいずれも従来どおり
//! `protocol_dispatch::reject_and_close`（`0A000` + 切断）のまま。
//!
//! 応答は Sync・Flush を持たない現状の設計上、都度即時に書き出す
//! （PostgreSQL プロトコル上バックエンドは任意時点で応答してよい）。
//!
//! SQLSTATE 写像（spec に専用コードが無い箇所は既存の閉じた 16 分類
//! [`engine::error_format::ErrorClass`] へ倒す。詳細は各エラー variant の
//! ドキュメント参照）:
//! - 未定義ステートメント名への Describe・名前付きステートメントの重複 Parse
//!   → `08P01`（`ProtocolViolation`。クライアントのプロトコル逸脱）
//! - Describe の portal 対象 → `0A000`（`FeatureNotSupported`）
//! - 件数・名前長・保持バイト上限超過 →
//!   `54000`（`PayloadTooLarge`。[`crate::limits`] の各定数）
//! - body の構造不正（NUL 終端欠落・余剰バイト・負の件数・非 UTF-8・種別バイト
//!   不正）→ `08P01`
//! - パラメータ型宣言（`num_param_types > 0`）→ `0A000`（`$n` 束縛は WIRE-12・
//!   #935 の担当。本 Issue は Parse 自体を拒否する）
//! - SQL 検証失敗 → `engine::sql::allowlist::SqlSurfaceError::error_class()`
//!   （簡易クエリと同一の分類）

use std::io::{self, Write};
use std::net::TcpStream;

use engine::core::{EngineCore, ParsedSql};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::sql::mode::SessionState;

use crate::framing::{self, FrameError};
use crate::limits::{
    MAX_PREPARED_SQL_BYTES_PER_SESSION, MAX_PREPARED_STATEMENTS_PER_SESSION, MAX_STATEMENT_NAME_LEN,
};
use crate::result_encoder;

/// Parse 本文の最小長（空文字列ステートメント名 cstr・空クエリ cstr・
/// パラメータ件数 i16 の合計）。
const MIN_PARSE_BODY_LEN: usize = 4;
/// Describe 本文の最小長（種別 1 バイト・空文字列名 cstr）。
const MIN_DESCRIBE_BODY_LEN: usize = 2;

/// `post_auth_loop` へ返す、ループを継続するか（応答送出済み）接続を閉じたか
/// （エラー応答＋lingering close 済み）の合図。
pub(crate) enum LoopSignal {
    Continue,
    Closed,
}

/// body 復号の構造不正（Issue #933）。`untrusted` な生バイト列から
/// `get()`/`try_into()` のみで復号する（`unwrap`/`expect`/添字アクセス禁止。
/// `.claude/rules/coding-rust.md` P0）。
#[derive(Debug)]
pub(crate) enum BodyError {
    /// NUL 終端の cstring を最後まで走査したが終端が見つからなかった。
    MissingNulTerminator,
    /// cstring が UTF-8 として不正。
    InvalidUtf8,
    /// パラメータ件数が負値。
    NegativeParamCount,
    /// 宣言済みの構造（cstring 2 個＋パラメータ件数）を読み終えた後に余剰バイトが
    /// 残っている、またはパラメータ件数から期待されるバイト数に満たない。
    TrailingOrTruncatedBytes,
    /// Describe の種別バイトが `'S'`/`'P'` のいずれでもない。
    InvalidDescribeKind,
}

impl BodyError {
    fn message(&self) -> &'static str {
        match self {
            BodyError::MissingNulTerminator => {
                "malformed Parse/Describe message: missing NUL terminator"
            }
            BodyError::InvalidUtf8 => "malformed Parse/Describe message: invalid UTF-8",
            BodyError::NegativeParamCount => "malformed Parse message: negative parameter count",
            BodyError::TrailingOrTruncatedBytes => {
                "malformed Parse/Describe message: trailing or truncated bytes"
            }
            BodyError::InvalidDescribeKind => "malformed Describe message: invalid target kind",
        }
    }
}

/// 復号済みの Parse メッセージ。
struct ParseMessage {
    name: String,
    query: String,
    num_param_types: usize,
}

/// body の先頭から NUL 終端 cstring を 1 個読む（`pos` を終端の次バイトへ進める）。
fn read_cstring(body: &[u8], pos: &mut usize) -> Result<String, BodyError> {
    let rest = body.get(*pos..).ok_or(BodyError::MissingNulTerminator)?;
    let nul_offset = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(BodyError::MissingNulTerminator)?;
    let bytes = rest
        .get(..nul_offset)
        .ok_or(BodyError::MissingNulTerminator)?;
    let s = std::str::from_utf8(bytes)
        .map_err(|_| BodyError::InvalidUtf8)?
        .to_string();
    *pos = pos
        .checked_add(nul_offset)
        .and_then(|p| p.checked_add(1))
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    Ok(s)
}

/// Parse（'P'）の body（PostgreSQL wire v3: 文字列名 cstr・クエリ文字列 cstr・
/// パラメータ型 OID 件数 i16・OID 列 i32×件数）を復号する。OID 列自体の値は
/// 読み捨てず件数分の存在のみ検証する（`$n` 束縛〔WIRE-12・#935〕未対応のため
/// `num_param_types > 0` は呼び出し元が `0A000` で拒否する）。
fn parse_parse_body(body: &[u8]) -> Result<ParseMessage, BodyError> {
    let mut pos = 0usize;
    let name = read_cstring(body, &mut pos)?;
    let query = read_cstring(body, &mut pos)?;
    let count_bytes = body
        .get(pos..pos.saturating_add(2))
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let count_arr: [u8; 2] = count_bytes
        .try_into()
        .map_err(|_| BodyError::TrailingOrTruncatedBytes)?;
    let count = i16::from_be_bytes(count_arr);
    if count < 0 {
        return Err(BodyError::NegativeParamCount);
    }
    let num_param_types = count as usize;
    pos = pos.saturating_add(2);
    let expected_oid_bytes = num_param_types
        .checked_mul(4)
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let remaining = body.get(pos..).ok_or(BodyError::TrailingOrTruncatedBytes)?;
    if remaining.len() != expected_oid_bytes {
        return Err(BodyError::TrailingOrTruncatedBytes);
    }
    Ok(ParseMessage {
        name,
        query,
        num_param_types,
    })
}

/// Describe（'D'）が対象とする種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DescribeTarget {
    Statement,
    Portal,
}

struct DescribeMessage {
    target: DescribeTarget,
    name: String,
}

/// Describe（'D'）の body（種別バイト 'S'/'P'・対象名 cstr）を復号する。
fn parse_describe_body(body: &[u8]) -> Result<DescribeMessage, BodyError> {
    let kind = *body.first().ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let target = match kind {
        b'S' => DescribeTarget::Statement,
        b'P' => DescribeTarget::Portal,
        _ => return Err(BodyError::InvalidDescribeKind),
    };
    let mut pos = 1usize;
    let name = read_cstring(body, &mut pos)?;
    if pos != body.len() {
        return Err(BodyError::TrailingOrTruncatedBytes);
    }
    Ok(DescribeMessage { target, name })
}

/// 接続単位で Parse 済みステートメントを保持する（`sql_bytes` は
/// [`MAX_PREPARED_SQL_BYTES_PER_SESSION`] の累計判定用。`Empty` は空文字列の
/// クエリテキスト——PostgreSQL は空文の Parse を構文エラーにせず受理する）。
pub(crate) enum PreparedStatement {
    Empty,
    Parsed(ParsedSql),
}

/// 接続単位（`handshake` の接続ループが `SessionState` と並べて所有し、接続終了
/// で破棄）の名前付き／無名ステートメント保持（Issue #933）。無名（`""`）は
/// 件数上限にカウントせず黙って置換する（PostgreSQL の既定挙動）。
pub(crate) struct PreparedStatementStore {
    statements: std::collections::HashMap<String, (PreparedStatement, usize)>,
    total_sql_bytes: usize,
}

/// 保持上限超過・構造違反（Issue #933）。
#[derive(Debug)]
enum StoreError {
    NameTooLong,
    DuplicateName,
    TooManyStatements,
    TotalBytesExceeded,
}

impl PreparedStatementStore {
    pub(crate) fn new() -> Self {
        PreparedStatementStore {
            statements: std::collections::HashMap::new(),
            total_sql_bytes: 0,
        }
    }

    /// `name`・`sql_bytes`（Parse 対象の生クエリテキストのバイト長。保持バイト
    /// 上限の判定基準）・`statement` を保持する。上限判定はアロケーション
    /// （`HashMap::insert`）より前に行う。
    fn insert(
        &mut self,
        name: String,
        sql_bytes: usize,
        statement: PreparedStatement,
    ) -> Result<(), StoreError> {
        let is_anonymous = name.is_empty();
        if !is_anonymous {
            if name.len() > MAX_STATEMENT_NAME_LEN {
                return Err(StoreError::NameTooLong);
            }
            if self.statements.contains_key(&name) {
                return Err(StoreError::DuplicateName);
            }
            // 無名（""）エントリは件数上限にカウントしない契約（構造体コメント
            // 参照）。`statements.len()` には無名分も含まれるため、名前付き
            // （非空キー）のみを数え上げて判定する（Issue #933 codex-review 指摘）。
            let named_count = self.statements.keys().filter(|k| !k.is_empty()).count();
            if named_count >= MAX_PREPARED_STATEMENTS_PER_SESSION {
                return Err(StoreError::TooManyStatements);
            }
        }

        // 既存の同名エントリ（無名の再 Parse による黙った置換）が保持していた
        // バイト数を先に差し引いてから累計判定する。
        let existing_bytes = self.statements.get(&name).map(|(_, bytes)| *bytes);
        let base = existing_bytes
            .map(|b| self.total_sql_bytes.saturating_sub(b))
            .unwrap_or(self.total_sql_bytes);
        let new_total = base
            .checked_add(sql_bytes)
            .ok_or(StoreError::TotalBytesExceeded)?;
        if new_total > MAX_PREPARED_SQL_BYTES_PER_SESSION {
            return Err(StoreError::TotalBytesExceeded);
        }

        self.total_sql_bytes = new_total;
        self.statements.insert(name, (statement, sql_bytes));
        Ok(())
    }

    fn get(&self, name: &str) -> Option<&PreparedStatement> {
        self.statements.get(name).map(|(stmt, _)| stmt)
    }
}

/// Parse／Describe 双方が `respond_error_and_close` へ渡す分類済みエラー。
enum HandlerError {
    Body(BodyError),
    Frame(FrameError),
    Sql(engine::sql::allowlist::SqlSurfaceError),
    Store(StoreError),
    /// `$n` パラメータ型宣言（WIRE-12・#935 の担当。本 Issue では常に拒否）。
    ParamTypesUnsupported,
    /// Describe の対象が未定義のステートメント名（構造上あり得る portal 名との
    /// 衝突ではなく、Parse されていない statement 名を指した場合）。
    UnknownStatement,
    /// Describe の対象が portal（種別 'P'）。portal はこの Issue の範囲では
    /// 構築され得ないため、常に拒否する。
    PortalDescribe,
}

impl HandlerError {
    fn error_class(&self) -> ErrorClass {
        match self {
            HandlerError::Body(_) => ErrorClass::ProtocolViolation,
            HandlerError::Frame(e) => e.error_class().unwrap_or(ErrorClass::ProtocolViolation),
            HandlerError::Sql(e) => e.error_class(),
            HandlerError::Store(StoreError::DuplicateName) => ErrorClass::ProtocolViolation,
            HandlerError::Store(_) => ErrorClass::PayloadTooLarge,
            HandlerError::ParamTypesUnsupported => ErrorClass::FeatureNotSupported,
            HandlerError::UnknownStatement => ErrorClass::ProtocolViolation,
            HandlerError::PortalDescribe => ErrorClass::FeatureNotSupported,
        }
    }

    fn message(&self) -> String {
        match self {
            HandlerError::Body(e) => e.message().to_string(),
            HandlerError::Frame(e) => e.client_message().to_string(),
            HandlerError::Sql(e) => e.client_message(),
            HandlerError::Store(StoreError::NameTooLong) => {
                "statement name exceeds the maximum length".to_string()
            }
            HandlerError::Store(StoreError::DuplicateName) => {
                "a prepared statement with this name already exists".to_string()
            }
            HandlerError::Store(StoreError::TooManyStatements) => {
                "too many prepared statements on this connection".to_string()
            }
            HandlerError::Store(StoreError::TotalBytesExceeded) => {
                "total prepared statement text exceeds the per-connection limit".to_string()
            }
            HandlerError::ParamTypesUnsupported => {
                "parameter type declarations in Parse are not supported on this connection"
                    .to_string()
            }
            HandlerError::UnknownStatement => "no such prepared statement".to_string(),
            HandlerError::PortalDescribe => {
                "Describe for a portal is not supported on this connection".to_string()
            }
        }
    }
}

impl From<BodyError> for HandlerError {
    fn from(e: BodyError) -> Self {
        HandlerError::Body(e)
    }
}

/// ErrorResponse を送出し、Sync による同期回復を持たない本 Issue の暫定契約
/// （モジュールドキュメント参照）どおり、有界 lingering close で接続を終える。
fn respond_error_and_close(stream: &mut TcpStream, err: &HandlerError) -> io::Result<()> {
    eprintln!(
        "wire-server: extended query rejecting message ({})",
        err.error_class().wire_code()
    );
    let class = err.error_class();
    let body =
        result_encoder::encode_error_response(class.wire_code(), &err.message()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "failed to encode ErrorResponse")
        })?;
    stream.write_all(&body)?;
    stream.flush()?;
    crate::protocol_dispatch::drain_and_close(
        stream,
        crate::protocol_dispatch::LINGER_DRAIN_TIMEOUT,
        crate::protocol_dispatch::LINGER_DRAIN_MAX_BYTES,
    );
    Ok(())
}

/// Parse（'P'）を処理する（`engine` が接続済みの場合のみ呼ばれる。`handshake::
/// post_auth_loop` 参照）。SQL は簡易クエリと同一の許可リスト
/// （[`EngineCore::parse_sql`]）で検証してから保持する（検証失敗なら保持しない）。
pub(crate) fn handle_parse(
    stream: &mut TcpStream,
    engine: &EngineCore,
    store: &mut PreparedStatementStore,
) -> io::Result<LoopSignal> {
    let body = match framing::read_length_prefixed_body(
        stream,
        MIN_PARSE_BODY_LEN,
        framing::MAX_MESSAGE_LEN,
    ) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    match handle_parse_body(engine, store, &body) {
        Ok(()) => {
            stream.write_all(&result_encoder::encode_parse_complete())?;
            stream.flush()?;
            Ok(LoopSignal::Continue)
        }
        Err(err) => {
            respond_error_and_close(stream, &err)?;
            Ok(LoopSignal::Closed)
        }
    }
}

fn handle_parse_body(
    engine: &EngineCore,
    store: &mut PreparedStatementStore,
    body: &[u8],
) -> Result<(), HandlerError> {
    let msg = parse_parse_body(body)?;
    if msg.num_param_types > 0 {
        return Err(HandlerError::ParamTypesUnsupported);
    }

    let statement = if msg.query.trim().is_empty() {
        PreparedStatement::Empty
    } else {
        let parsed = engine.parse_sql(&msg.query).map_err(HandlerError::Sql)?;
        PreparedStatement::Parsed(parsed)
    };

    store
        .insert(msg.name, msg.query.len(), statement)
        .map_err(HandlerError::Store)
}

/// Describe（'D'）を処理する（`engine` が接続済みの場合のみ呼ばれる）。
/// `ParameterDescription`（常に 0 件。`$n` 束縛は #935 の担当）と
/// `RowDescription`（結果列なしは `NoData`）を返す。
pub(crate) fn handle_describe(
    stream: &mut TcpStream,
    engine: &EngineCore,
    session: &SessionState,
    store: &PreparedStatementStore,
) -> io::Result<LoopSignal> {
    let body = match framing::read_length_prefixed_body(
        stream,
        MIN_DESCRIBE_BODY_LEN,
        framing::MAX_MESSAGE_LEN,
    ) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    match handle_describe_body(engine, session, store, &body) {
        Ok(columns) => {
            stream.write_all(&result_encoder::encode_parameter_description(&[]).map_err(
                |_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "failed to encode ParameterDescription",
                    )
                },
            )?)?;
            match columns {
                Some(columns) => {
                    let row_description = result_encoder::encode_row_description(&columns)
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "failed to encode RowDescription",
                            )
                        })?;
                    stream.write_all(&row_description)?;
                }
                None => {
                    stream.write_all(&result_encoder::encode_no_data())?;
                }
            }
            stream.flush()?;
            Ok(LoopSignal::Continue)
        }
        Err(err) => {
            respond_error_and_close(stream, &err)?;
            Ok(LoopSignal::Closed)
        }
    }
}

fn handle_describe_body(
    engine: &EngineCore,
    session: &SessionState,
    store: &PreparedStatementStore,
    body: &[u8],
) -> Result<Option<Vec<engine::sql::exec::ColumnMeta>>, HandlerError> {
    let msg = parse_describe_body(body)?;
    if msg.target == DescribeTarget::Portal {
        return Err(HandlerError::PortalDescribe);
    }
    let statement = store.get(&msg.name).ok_or(HandlerError::UnknownStatement)?;
    match statement {
        PreparedStatement::Empty => Ok(None),
        PreparedStatement::Parsed(parsed) => engine
            .describe_parsed_in_session(session, parsed)
            .map_err(HandlerError::Sql),
    }
}

fn io_error_from_frame(e: FrameError) -> io::Error {
    match e {
        FrameError::Io(io_err) => io_err,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_parse_body_decodes_name_query_and_zero_params() {
        let mut body = Vec::new();
        body.extend_from_slice(b"stmt1\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&0i16.to_be_bytes());

        let msg = parse_parse_body(&body).expect("valid Parse body");
        assert_eq!(msg.name, "stmt1");
        assert_eq!(msg.query, "SELECT 1");
        assert_eq!(msg.num_param_types, 0);
    }

    #[test]
    fn parse_parse_body_decodes_anonymous_statement() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&0i16.to_be_bytes());

        let msg = parse_parse_body(&body).expect("valid Parse body");
        assert_eq!(msg.name, "");
        assert_eq!(msg.query, "SELECT 1");
    }

    #[test]
    fn parse_parse_body_rejects_missing_nul_terminator() {
        let body = b"stmt1".to_vec();
        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::MissingNulTerminator)
        ));
    }

    #[test]
    fn parse_parse_body_rejects_negative_param_count() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&(-1i16).to_be_bytes());

        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::NegativeParamCount)
        ));
    }

    #[test]
    fn parse_parse_body_rejects_trailing_bytes() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0xff); // 余剰バイト

        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn parse_parse_body_rejects_truncated_param_oids() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&1i16.to_be_bytes()); // 1 件宣言だが OID 本体なし

        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn parse_describe_body_decodes_statement_target() {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"stmt1\0");

        let msg = parse_describe_body(&body).expect("valid Describe body");
        assert_eq!(msg.target, DescribeTarget::Statement);
        assert_eq!(msg.name, "stmt1");
    }

    #[test]
    fn parse_describe_body_decodes_portal_target() {
        let mut body = Vec::new();
        body.push(b'P');
        body.extend_from_slice(b"\0");

        let msg = parse_describe_body(&body).expect("valid Describe body");
        assert_eq!(msg.target, DescribeTarget::Portal);
    }

    #[test]
    fn parse_describe_body_rejects_invalid_kind() {
        let mut body = Vec::new();
        body.push(b'X');
        body.extend_from_slice(b"\0");

        assert!(matches!(
            parse_describe_body(&body),
            Err(BodyError::InvalidDescribeKind)
        ));
    }

    #[test]
    fn parse_describe_body_rejects_trailing_bytes() {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"stmt1\0");
        body.push(0xff);

        assert!(matches!(
            parse_describe_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn store_rejects_name_too_long() {
        let mut store = PreparedStatementStore::new();
        let long_name = "a".repeat(MAX_STATEMENT_NAME_LEN + 1);
        let result = store.insert(long_name, 10, PreparedStatement::Empty);
        assert!(matches!(result, Err(StoreError::NameTooLong)));
    }

    #[test]
    fn store_rejects_duplicate_named_statement() {
        let mut store = PreparedStatementStore::new();
        store
            .insert("stmt1".to_string(), 10, PreparedStatement::Empty)
            .expect("first insert succeeds");
        let result = store.insert("stmt1".to_string(), 10, PreparedStatement::Empty);
        assert!(matches!(result, Err(StoreError::DuplicateName)));
    }

    #[test]
    fn store_allows_repeated_anonymous_inserts_without_counting_toward_limit() {
        let mut store = PreparedStatementStore::new();
        for _ in 0..(MAX_PREPARED_STATEMENTS_PER_SESSION * 2) {
            store
                .insert(String::new(), 10, PreparedStatement::Empty)
                .expect("anonymous insert always succeeds within byte budget");
        }
        assert_eq!(store.statements.len(), 1);
    }

    #[test]
    fn store_rejects_too_many_named_statements() {
        let mut store = PreparedStatementStore::new();
        for i in 0..MAX_PREPARED_STATEMENTS_PER_SESSION {
            store
                .insert(format!("stmt{i}"), 10, PreparedStatement::Empty)
                .expect("insert within limit succeeds");
        }
        let result = store.insert(
            format!("stmt{MAX_PREPARED_STATEMENTS_PER_SESSION}"),
            10,
            PreparedStatement::Empty,
        );
        assert!(matches!(result, Err(StoreError::TooManyStatements)));
    }

    /// 無名 statement が先に存在していても、名前付き statement は規定の
    /// `MAX_PREPARED_STATEMENTS_PER_SESSION` 件をすべて保持できる（codex-review
    /// ・Cursor Bugbot 指摘・Issue #933: 無名エントリを件数上限へ誤って算入する
    /// 境界バグの回帰）。
    #[test]
    fn store_allows_full_named_quota_alongside_anonymous_statement() {
        let mut store = PreparedStatementStore::new();
        store
            .insert(String::new(), 10, PreparedStatement::Empty)
            .expect("anonymous insert succeeds");
        for i in 0..MAX_PREPARED_STATEMENTS_PER_SESSION {
            store
                .insert(format!("stmt{i}"), 10, PreparedStatement::Empty)
                .expect("named insert within limit succeeds despite anonymous entry present");
        }
        let result = store.insert(
            format!("stmt{MAX_PREPARED_STATEMENTS_PER_SESSION}"),
            10,
            PreparedStatement::Empty,
        );
        assert!(matches!(result, Err(StoreError::TooManyStatements)));
    }

    #[test]
    fn store_rejects_total_bytes_exceeded() {
        let mut store = PreparedStatementStore::new();
        let result = store.insert(
            "big".to_string(),
            MAX_PREPARED_SQL_BYTES_PER_SESSION + 1,
            PreparedStatement::Empty,
        );
        assert!(matches!(result, Err(StoreError::TotalBytesExceeded)));
    }
}
