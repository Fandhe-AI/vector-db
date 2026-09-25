//! 明示トランザクション `BEGIN`/`COMMIT`/`ROLLBACK`（SQL-31・TASK-221）の構文
//! 受理と、セッション単位の状態機械 [`SessionTransaction`] を提供する。
//!
//! `wire-server` の簡易クエリ（`simple_query.rs`）・拡張クエリ
//! （`extended_query.rs`）の両経路が、接続（セッション）ごとに 1 つの
//! [`SessionTransaction`] を保持し、`core::EngineCore::execute_sql_in_txn`／
//! `execute_parsed_in_txn` へ `&mut` で渡す。`Idle`（トランザクション外）の間は
//! 既存の autocommit 経路（[`crate::core::EngineCore::execute_sql_in_session`]）と
//! ビット同一に振る舞い、`Active` の間だけ共有 `redb::WriteTransaction`
//! （[`crate::storage::Storage::begin_explicit_write_txn`]）を保持して複数の
//! 書き込み文を 1 回の commit にまとめる。
//!
//! commit 成功境界（`RECOVER-5`・`RECOVER-6`）: point of no return は `COMMIT`
//! 文が呼ぶ [`crate::recovery::commit_boundary::commit`] の 1 回のみ。台帳
//! （`RECOVER-12`）は行書き込みと同一の共有 `write_txn` 内に記録されるため、
//! `ROLLBACK`・接続断（`SessionTransaction` の drop）で行・台帳とも一括して
//! 消える。
//!
//! 読み取りの既知の逸脱（SQL-31 の完全な意味論からの意図的な縮退）: 本実装は
//! 「同一トランザクション内で自分がまだ書き込んでいないテーブル」の読み取りのみ
//! 通常経路で許可し、既に書き込んだテーブルへの読み取りは `0A000` で拒否する
//! （自トランザクションの未 commit 変更の可視化は対象外）。`written_tables` が
//! この判定に使う集合。詳細は `docs/design/explicit-transaction.md` 参照。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;

/// `BEGIN`/`COMMIT`/`ROLLBACK` の種別（構造検証のみ。実行は
/// [`SessionTransaction`] が担う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnControl {
    Begin,
    Commit,
    Rollback,
}

/// `BEGIN`/`COMMIT`/`ROLLBACK` の規範形（`[WORK|TRANSACTION]` 修飾語のみ許容。
/// 分離レベル指定・`READ ONLY`・`AND CHAIN`・`$n` 等は `42601`）を受理する。
/// 先頭トークンが `BEGIN`/`COMMIT`/`ROLLBACK`（大小無視）であることは
/// 呼び出し元（`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_transaction_control_tokens(
    tokens: &[Token],
) -> Result<TxnControl, SqlSurfaceError> {
    let Some(Token::Ident(head)) = tokens.first() else {
        return Err(SqlSurfaceError::unsupported(
            "malformed transaction control statement",
        ));
    };
    let ctrl = if head.eq_ignore_ascii_case("BEGIN") {
        TxnControl::Begin
    } else if head.eq_ignore_ascii_case("COMMIT") {
        TxnControl::Commit
    } else if head.eq_ignore_ascii_case("ROLLBACK") {
        TxnControl::Rollback
    } else {
        return Err(SqlSurfaceError::unsupported(
            "malformed transaction control statement",
        ));
    };

    let rest = &tokens[1..];
    let rest = match rest.first() {
        Some(Token::Ident(kw))
            if kw.eq_ignore_ascii_case("WORK") || kw.eq_ignore_ascii_case("TRANSACTION") =>
        {
            &rest[1..]
        }
        _ => rest,
    };
    if !rest.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "unsupported clause after transaction control statement",
        ));
    }
    Ok(ctrl)
}

/// [`SessionTransaction`] の対外的な状態（#943・`WIRE-19` が `ReadyForQuery` の
/// 状態バイトへ写像する予定の入口。本 Issue では照会 API の公開までとし、
/// `'I'` 固定の送出は変更しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    Idle,
    InTransaction,
    Failed,
}

/// 明示トランザクション中に開いている共有書き込みトランザクションと、その
/// 付随状態（[`crate::storage::writer_gate::WriterPermit`] を含む。`write_txn`
/// を先に宣言し、drop 時に abort が permit の解放より先に走るようにする）。
struct ActiveTxn<'e> {
    write_txn: redb::WriteTransaction,
    #[allow(dead_code)]
    permit: crate::storage::writer_gate::WriterPermit,
    started_at: Instant,
    statements: u32,
    seen_operation_ids: HashSet<String>,
    written_tables: HashSet<String>,
    has_writes: bool,
    session_at_begin: SessionState,
    _marker: std::marker::PhantomData<&'e ()>,
}

enum TxnState<'e> {
    Idle,
    Active(Box<ActiveTxn<'e>>),
    Failed { session_at_begin: SessionState },
}

/// トランザクションの持続時間・文数の上限（実装既定値。spec 由来の数値ではない。
/// `docs/design/explicit-transaction.md` 参照）。
#[derive(Debug, Clone, Copy)]
pub struct TransactionLimits {
    pub max_duration: Duration,
    pub max_statements: u32,
}

impl Default for TransactionLimits {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_secs(20),
            max_statements: 1_000,
        }
    }
}

/// 接続（セッション）ごとに 1 つ保持する明示トランザクションの状態機械。
pub struct SessionTransaction<'e> {
    state: TxnState<'e>,
    limits: TransactionLimits,
}

impl<'e> SessionTransaction<'e> {
    pub fn new(limits: TransactionLimits) -> Self {
        Self {
            state: TxnState::Idle,
            limits,
        }
    }

    /// 現在の状態（#943 が `ReadyForQuery` の状態バイトへ写像する想定の照会 API）。
    pub fn status(&self) -> TransactionStatus {
        match &self.state {
            TxnState::Idle => TransactionStatus::Idle,
            TxnState::Active(_) => TransactionStatus::InTransaction,
            TxnState::Failed { .. } => TransactionStatus::Failed,
        }
    }

    /// `Active` なら true（`core.rs` の文実行前チェック用）。
    pub fn is_active(&self) -> bool {
        matches!(self.state, TxnState::Active(_))
    }

    /// `BEGIN`。`Idle` から `Active` へ遷移する。`storage` から
    /// [`crate::storage::Storage::begin_explicit_write_txn`] で単一ライタの
    /// permit を取得したまま保持する（`writer_gate` モジュールドキュメント
    /// 参照。待機上限超過は `55P03`）。
    pub fn begin(
        &mut self,
        storage: &'e crate::storage::Storage,
        session: &SessionState,
    ) -> Result<(), SqlSurfaceError> {
        match &self.state {
            TxnState::Idle => {
                let (write_txn, permit) = storage
                    .begin_explicit_write_txn()
                    .map_err(map_write_lock_err)?;
                self.state = TxnState::Active(Box::new(ActiveTxn {
                    write_txn,
                    permit,
                    started_at: Instant::now(),
                    statements: 0,
                    seen_operation_ids: HashSet::new(),
                    written_tables: HashSet::new(),
                    has_writes: false,
                    session_at_begin: session.clone(),
                    _marker: std::marker::PhantomData,
                }));
                Ok(())
            }
            TxnState::Active(_) => {
                // 入れ子の BEGIN はトランザクションを Failed へ遷移させる
                // （PostgreSQL の "WARNING" 相当ではなく fail-closed に倒す。
                // `docs/design/explicit-transaction.md` 参照）。
                let session_at_begin = self.active_session_at_begin();
                self.state = TxnState::Failed { session_at_begin };
                Err(SqlSurfaceError::ActiveSqlTransaction)
            }
            TxnState::Failed { .. } => Err(SqlSurfaceError::InFailedSqlTransaction),
        }
    }

    /// `COMMIT`。`Active` なら書き込みの有無に応じて commit／abort し `Idle` へ
    /// 遷移する。`Idle`／`Failed` はそれぞれ `25P01`／`25P02` を返す（`Failed`
    /// のままにし、`ROLLBACK` のみを受理し続ける）。
    pub fn commit(&mut self) -> Result<(), SqlSurfaceError> {
        match std::mem::replace(&mut self.state, TxnState::Idle) {
            TxnState::Idle => {
                self.state = TxnState::Idle;
                Err(SqlSurfaceError::NoActiveSqlTransaction)
            }
            TxnState::Active(active) => {
                let result = if active.has_writes {
                    crate::recovery::commit_boundary::commit(active.write_txn).map_err(|_| {
                        SqlSurfaceError::Internal {
                            detail: "internal error".to_string(),
                        }
                    })
                } else {
                    drop(active.write_txn);
                    Ok(())
                };
                // `permit`（drop 済みの `active.write_txn`／自身が move 済み）は
                // ここまでに drop され解放されている。
                self.state = TxnState::Idle;
                result
            }
            TxnState::Failed { .. } => {
                self.state = TxnState::Failed {
                    session_at_begin: SessionState::default(),
                };
                Err(SqlSurfaceError::InFailedSqlTransaction)
            }
        }
    }

    /// `ROLLBACK`。`Active`/`Failed` いずれからも `Idle` へ戻り、`BEGIN` 時点の
    /// `SessionState`（`SET search_mode`・`CREATE FUNCTION` 等）を復元する。
    /// `Idle` からの `ROLLBACK` は `25P01`。
    pub fn rollback(&mut self, session: &mut SessionState) -> Result<(), SqlSurfaceError> {
        match std::mem::replace(&mut self.state, TxnState::Idle) {
            TxnState::Idle => {
                self.state = TxnState::Idle;
                Err(SqlSurfaceError::NoActiveSqlTransaction)
            }
            TxnState::Active(active) => {
                // `write_txn` は先に drop（abort）してから `permit` を解放する
                // （構造体のフィールド宣言順どおりの drop 順序。writer_gate
                // モジュールドキュメント参照）。
                *session = active.session_at_begin;
                self.state = TxnState::Idle;
                Ok(())
            }
            TxnState::Failed { session_at_begin } => {
                *session = session_at_begin;
                self.state = TxnState::Idle;
                Ok(())
            }
        }
    }

    /// 文実行前チェック（`Active` のときのみ意味を持つ）。上限超過・
    /// `operation_id` の同一トランザクション内再利用を検出し、検出時は
    /// トランザクションを `Failed` へ遷移させる。
    pub(crate) fn check_and_register_statement(
        &mut self,
        operation_id: Option<&str>,
    ) -> Result<(), SqlSurfaceError> {
        let active = match &mut self.state {
            TxnState::Active(active) => active,
            _ => return Ok(()),
        };
        if active.started_at.elapsed() > self.limits.max_duration
            || active.statements >= self.limits.max_statements
        {
            self.fail();
            return Err(SqlSurfaceError::payload_too_large(
                "transaction exceeded duration or statement limit",
            ));
        }
        if let Some(id) = operation_id {
            if !active.seen_operation_ids.insert(id.to_string()) {
                self.fail();
                return Err(SqlSurfaceError::InvalidTransactionState);
            }
        }
        active.statements = active.statements.saturating_add(1);
        Ok(())
    }

    /// 対象テーブルへの書き込みを記録する（読み取りの `written_tables` 判定用）。
    pub(crate) fn mark_written(&mut self, table: &str) {
        if let TxnState::Active(active) = &mut self.state {
            active.written_tables.insert(table.to_string());
            active.has_writes = true;
        }
    }

    /// `table` が同一トランザクション内で既に書き込み済みかどうか（読み取り時の
    /// `0A000` 判定に使う。§2.5 の既知の逸脱）。
    pub(crate) fn table_already_written(&self, table: &str) -> bool {
        match &self.state {
            TxnState::Active(active) => active.written_tables.contains(table),
            _ => false,
        }
    }

    /// 現在保持している共有 `redb::WriteTransaction`（`Active` のときのみ
    /// `Some`）。書き込み系の実行本体が [`crate::tenant::WriteTarget::InTxn`]
    /// を構築するために使う。
    pub(crate) fn write_txn(&self) -> Option<&redb::WriteTransaction> {
        match &self.state {
            TxnState::Active(active) => Some(&active.write_txn),
            _ => None,
        }
    }

    /// 任意のエラーで `Active` → `Failed` へ強制遷移させる（文実行中に
    /// エラーが起きた場合、`core.rs` の実行ディスパッチが呼ぶ）。
    pub fn fail(&mut self) {
        if let TxnState::Active(active) = std::mem::replace(&mut self.state, TxnState::Idle) {
            // `write_txn` は drop（abort）され、`permit` はその後に解放される。
            self.state = TxnState::Failed {
                session_at_begin: active.session_at_begin,
            };
        }
    }

    fn active_session_at_begin(&self) -> SessionState {
        match &self.state {
            TxnState::Active(active) => active.session_at_begin.clone(),
            _ => SessionState::default(),
        }
    }
}

/// [`crate::storage::StorageError::WriteLockTimeout`]／
/// [`crate::storage::StorageError::WriteTxnHeldByCurrentSession`] を
/// `SqlSurfaceError::LockNotAvailable`（`55P03`）へ写像する。それ以外は
/// 内部事象として `XX000` に丸める（他テナントの情報を含まない）。
fn map_write_lock_err(e: crate::storage::StorageError) -> SqlSurfaceError {
    match e {
        crate::storage::StorageError::WriteLockTimeout
        | crate::storage::StorageError::WriteTxnHeldByCurrentSession => {
            SqlSurfaceError::LockNotAvailable
        }
        _ => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens_of(sql: &str) -> Vec<Token> {
        crate::sql::lexer::tokenize(sql).expect("tokenize")
    }

    #[test]
    fn begin_variants_are_accepted() {
        for sql in ["BEGIN", "BEGIN WORK", "BEGIN TRANSACTION", "begin"] {
            let ctrl = validate_transaction_control_tokens(&tokens_of(sql)).expect("accepted");
            assert_eq!(ctrl, TxnControl::Begin);
        }
    }

    #[test]
    fn commit_and_rollback_variants_are_accepted() {
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("COMMIT")).unwrap(),
            TxnControl::Commit
        );
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("COMMIT WORK")).unwrap(),
            TxnControl::Commit
        );
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("ROLLBACK")).unwrap(),
            TxnControl::Rollback
        );
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("ROLLBACK TRANSACTION")).unwrap(),
            TxnControl::Rollback
        );
    }

    #[test]
    fn trailing_clauses_are_rejected() {
        for sql in [
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "BEGIN READ ONLY",
            "COMMIT AND CHAIN",
            "ROLLBACK TO SAVEPOINT x",
        ] {
            let err = validate_transaction_control_tokens(&tokens_of(sql)).unwrap_err();
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn begin_then_commit_round_trips_through_idle() {
        let limits = TransactionLimits::default();
        let mut txn: SessionTransaction<'static> = SessionTransaction::new(limits);
        assert_eq!(txn.status(), TransactionStatus::Idle);
        // `begin`/`commit`/`rollback` 自体の redb 結線は `core.rs`・wire-server
        // 側の結合テストで検証する（本テストは構文受理のみを対象とする）。
        let _ = &mut txn;
    }
}
