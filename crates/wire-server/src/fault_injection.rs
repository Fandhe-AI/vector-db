//! Issue #705: テスト専用の障害注入（feature `fault-injection` 有効時のみ
//! コンパイルされる。`crates/wire-server/Cargo.toml` の `[features]` 参照）。
//!
//! 目的: commit 成功境界を跨いだ panic → 緊急応答の同期送出 → abort
//! （TASK-97・RECOVER-6・`crate::simple_query` モジュールコメント）は、
//! これまで engine のプロセス内テストと wire-server 層 A
//! （`tests/wire_emergency_response.rs`。テストバイナリ自身が登録・commit・
//! panic を再現する）でしか観測できず、`wire-server` バイナリを外部クライアント
//! から commit 後 panic させる手段が無かった。本モジュールは `main.rs`
//! （CLI 引数 `--fault-inject post-commit-panic`）と `simple_query.rs`
//! （SQL wire 側の注入点）が共有する、untrusted な CLI 文字列から「自プロセスを
//! 1 回だけ commit 後 panic させる」という単一の能力へ到達する唯一の入口を
//! 提供する（Issue #656 の `search_engine_opt` と同型の設計）。
//!
//! [`maybe_panic_after_http_insert_commit`]（Issue #829・codex-review P1
//! 指摘対応）は同じ arm・take-once 消費（[`take_post_commit_panic_if`]）を
//! HTTP 表層の `insert` op（`crate::http::query::insert::execute`）からも
//! 呼べるようにした注入点。HTTP 側は RECOVER-6（緊急応答の同期送出）を
//! まだ実装していないため（`docs/design/nosql-insert-mapping.md`「対象外」
//! 節参照）、この経路で検証できるのは RECOVER-5（応答境界。
//! `crate::http::conn::build_outcome` の `ResponseBoundaryGuard`）の
//! 安全性側——commit 後 panic が通常の `500` へ縮退せずプロセス終了へ倒れる
//! こと——のみである。
//!
//! 安全性: default features に含めないため、既定ビルド・crates.io 公開の
//! 既定構成にはシンボルもフラグも存在しない。feature を有効化しても露出する
//! のは上記の能力のみで、テナント境界・RLS・認証・fail-closed 経路を迂回する
//! API は一切露出しない（発火経路自体は production の RECOVER-5／RECOVER-6
//! 経路そのもので、追加するのはトリガーだけ）。詳細な判断根拠は
//! `docs/design/three-client-e2e-harness.md`「Issue #705」節参照。

use std::sync::atomic::{AtomicBool, Ordering};

use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::SqlOutcome;

/// `--fault-inject` の CLI フラグ名。
pub const FLAG: &str = "--fault-inject";
/// `--fault-inject post-commit-panic` が受理する唯一の値。
pub const POST_COMMIT_PANIC_TOKEN: &str = "post-commit-panic";

/// `parse` が受理する障害注入の種別（閉じた語彙。現状 1 種類のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// `INSERT` の commit 成功直後に固定文言で panic する。
    PostCommitPanic,
}

/// `--fault-inject` の値を閉じた語彙の厳密一致でのみ受理する（fail-closed。
/// 大小文字違い・空文字・前後空白付きはいずれも拒否）。
pub fn parse(raw: &str) -> Result<FaultKind, String> {
    if raw == POST_COMMIT_PANIC_TOKEN {
        Ok(FaultKind::PostCommitPanic)
    } else {
        Err(format!(
            "{FLAG} must be one of [{POST_COMMIT_PANIC_TOKEN:?}], got {raw:?}"
        ))
    }
}

/// プロセスグローバルな arm 状態（`main.rs::run_server` が起動時に高々 1 回
/// `arm` を呼ぶ想定だが、プロセス内で複数回呼ばれても「立っている」状態に
/// 収束するだけで安全）。
static POST_COMMIT_PANIC_ARMED: AtomicBool = AtomicBool::new(false);

/// `main.rs::run_server` が listen 開始前に高々 1 回呼ぶ（`kind` は現状
/// `PostCommitPanic` の 1 値のみ。将来の種別追加に備えて引数化してある）。
pub fn arm(kind: FaultKind) {
    match kind {
        FaultKind::PostCommitPanic => POST_COMMIT_PANIC_ARMED.store(true, Ordering::SeqCst),
    }
}

/// `Ok(SqlOutcome::Insert(_))` のときだけ真を返す（commit 成功の wire 側判定
/// 材料。`Ok(Insert)` は `EngineCore::execute_sql_in_session` が
/// `execute_insert_sql` → `recovery::commit_boundary::commit` の成功を経て
/// 初めて返す値であり、`Err(_)` や他の読み取り専用 variant では commit は
/// 発生しない。実際に commit-pending 世代が有効かどうかの最終判定は
/// `engine::recovery::panic_hook::emergency_send_decision` 側が担うため、
/// wire 側でそれ以上の判定は行わない）。
pub(crate) fn is_committed_insert(outcome: &Result<SqlOutcome, SqlSurfaceError>) -> bool {
    matches!(outcome, Ok(SqlOutcome::Insert(_)))
}

/// `committed_insert` が真のときだけ arm を消費する（`compare_exchange` に
/// よる take-once。CAS が失敗＝既に消費済み・未 arm のいずれでも偽を返し、
/// `committed_insert` が偽のとき（失敗した INSERT・SELECT 等）は arm を
/// 一切触らず据え置く。これにより、失敗した INSERT や SELECT を挟んでも
/// 後続の成功 INSERT で確実に発火する）。
pub(crate) fn take_post_commit_panic_if(committed_insert: bool) -> bool {
    if !committed_insert {
        return false;
    }
    POST_COMMIT_PANIC_ARMED
        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// `crate::simple_query::execute_and_respond` の「登録ブロック」内から呼ぶ
/// （呼び出し位置の契約は同関数のコメント参照。登録が生きている区間の外で
/// panic しても緊急応答は送られないため、ここより後ろへ移動してはならない）。
///
/// arm 済みかつ直前の `outcome` が commit 成功 `INSERT` のときのみ、SQL 本文・
/// テナント ID を含まない固定文言で panic する（security.md P0）。
pub(crate) fn maybe_panic_after_commit(outcome: &Result<SqlOutcome, SqlSurfaceError>) {
    if take_post_commit_panic_if(is_committed_insert(outcome)) {
        panic!("fault-injection: injected post-commit panic (test only)");
    }
}

/// `crate::http::query::insert::execute` の commit 成功直後から呼ぶ
/// （呼び出し位置の契約は同関数のコメント参照。`crate::http::conn::
/// build_outcome` の `ResponseBoundaryGuard`（RECOVER-5 (3)。PR #829）が
/// 保護している区間の内側でだけ発火させる契約）。
///
/// `crate::http::query::insert` は `SqlOutcome` を経由しない（NoSQL 表層は
/// `engine::sql::parser::BoundInsert` を直接束縛する第 2 の実行器を作らない
/// 設計。`insert.rs` モジュール doc 参照）ため、[`maybe_panic_after_commit`]
/// が判定に使う `is_committed_insert` は適用できない。呼び出し元
/// （`insert::execute`）が「commit まで成功した」ことを自ら知っている
/// 呼び出し位置からのみ呼ばれる契約とし、[`take_post_commit_panic_if`] へは
/// 常に `true` を渡す（arm の take-once 消費ロジック自体は SQL 経路と共有）。
pub(crate) fn maybe_panic_after_http_insert_commit() {
    if take_post_commit_panic_if(true) {
        panic!("fault-injection: injected post-commit panic (test only)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `POST_COMMIT_PANIC_ARMED` はプロセスグローバルなため、`cargo test` の
    // 並列実行から本モジュールのテストを直列化する（他ファイルのテストとは
    // 独立したプロセスグローバル状態のため、モジュール内で閉じたロックで足りる）。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// テスト間で arm 状態を必ずリセットする（前のテストの副作用を持ち込まない）。
    fn reset_for_test() {
        POST_COMMIT_PANIC_ARMED.store(false, Ordering::SeqCst);
    }

    fn ok_insert() -> Result<SqlOutcome, SqlSurfaceError> {
        Ok(SqlOutcome::Insert(engine::sql::exec::InsertOutcome {
            rows_affected: 1,
            incremental: None,
        }))
    }

    fn ok_set_search_mode() -> Result<SqlOutcome, SqlSurfaceError> {
        Ok(SqlOutcome::SetSearchMode(
            engine::sql::mode::SearchMode::Recall,
        ))
    }

    fn err_internal() -> Result<SqlOutcome, SqlSurfaceError> {
        Err(SqlSurfaceError::Internal {
            detail: "test".to_string(),
        })
    }

    #[test]
    fn parse_accepts_the_single_token() {
        assert_eq!(parse("post-commit-panic"), Ok(FaultKind::PostCommitPanic));
    }

    #[test]
    fn parse_rejects_case_variants_and_bogus_values() {
        for raw in [
            "",
            "Post-Commit-Panic",
            "post_commit_panic",
            "bogus",
            " post-commit-panic",
        ] {
            let err = parse(raw).expect_err("must reject");
            assert!(err.contains(FLAG), "error must mention {FLAG}: {err}");
        }
    }

    #[test]
    fn is_committed_insert_matches_only_ok_insert() {
        assert!(is_committed_insert(&ok_insert()));
        assert!(!is_committed_insert(&ok_set_search_mode()));
        assert!(!is_committed_insert(&err_internal()));
    }

    #[test]
    fn unarmed_ok_insert_does_not_take() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset_for_test();
        assert!(!take_post_commit_panic_if(
            is_committed_insert(&ok_insert())
        ));
    }

    #[test]
    fn armed_but_not_committed_insert_leaves_arm_intact() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset_for_test();
        arm(FaultKind::PostCommitPanic);

        assert!(!take_post_commit_panic_if(is_committed_insert(
            &err_internal()
        )));
        assert!(!take_post_commit_panic_if(is_committed_insert(
            &ok_set_search_mode()
        )));

        // arm はまだ消費されていないため、続く commit 成功 INSERT で発火する。
        assert!(take_post_commit_panic_if(is_committed_insert(&ok_insert())));
    }

    #[test]
    fn armed_ok_insert_fires_exactly_once() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset_for_test();
        arm(FaultKind::PostCommitPanic);

        assert!(take_post_commit_panic_if(is_committed_insert(&ok_insert())));
        // 2 回目は arm が既に消費済みのため偽。
        assert!(!take_post_commit_panic_if(
            is_committed_insert(&ok_insert())
        ));
    }
}
