//! TASK-96（対象ビヘイビア: RECOVER-5）の commit 成功境界実装。
//!
//! `redb` への commit 成功を書き込み操作の「もう後戻りできない地点」
//! （point of no return）と定め、書き込み実行中の失敗タイミングを問わず
//! 1 回の呼び出しに対する応答が常に一意（二重応答なし）であることを構造的に
//! 保証する choke point を提供する。呼び出し元は `crate::tenant`（各
//! `*_unchecked` 関数）・`crate::txn`（`WriteTxn`/`BatchWriteTxn` の commit
//! 一本化点、[`commit_write_txn_guarded`] 経由）に加え、`crate::storage`
//! （`Storage::put`/`put_batch`）・`crate::catalog`（テーブル DDL・
//! `insert_row_into_table` 系の DML）が [`commit`] を直接呼ぶ経路も含む
//! （本クレート内で実書き込みを伴う commit（`write_txn.commit()` を到達させる
//! 経路）はすべて本モジュールの choke point を経由し、`crate::storage::
//! bump_generation_and_commit` を直接呼ばない）。これらはいずれも wire 層の
//! DML 入口（`crate::core::EngineCore`）が最終的に通る経路。
//!
//! 失敗タイミングの三分類（RECOVER-5）と本モジュールの役割:
//!
//! - **(1) commit 前の失敗**: 呼び出し元が `write_txn` を commit へ渡す前に
//!   `Err` を返し、`redb::WriteTransaction` を drop（＝ abort）する契約は
//!   `crate::tenant`・`crate::catalog` 側に既存で成立している（本モジュールが
//!   新設するのは (2)(3) の保護のみ）。
//! - **(2) commit 成功後・派生状態反映の失敗**: [`PostCommitResult`] が
//!   成功応答と独立に失敗を保持し、応答は commit 成功のまま一意に確定する
//!   （反映失敗からの回復契約自体は後続タスク〔RECOVER-9 系〕のスコープで、
//!   本モジュールは「応答の一意性を壊さない」構造の提供のみを担う）。
//! - **(3) commit 成功後の panic**: [`PostCommitPanicGuard`] が commit 成功から
//!   `post_commit`（派生状態反映）完了までの区間を覆う Drop ガードで、区間内で
//!   panic が発生した場合は `std::process::abort()` してプロセスを終了させる。
//!   さらに [`ResponseBoundaryGuard`] が、commit 成功から wire 層が実際に成功
//!   応答をソケットへ書き終える（`ReadyForQuery` 送出含む）までのより広い区間を
//!   スレッドローカルフラグ経由で覆う（codex-review P1・PR #246 指摘対応。
//!   `commit_and_finish` 内で disarm した後、呼び出し元スタックを戻って wire 層が
//!   応答を組み立て・送信する区間は元々未保護だった）。unwind を継続させると
//!   呼び出し元スタック上の `catch_unwind` 等で成功応答が構築されうるため、
//!   fail-closed（成功応答を返しうる経路を構造的に遮断する）でプロセスごと
//!   止める（abort 前の ERR-1 応答送信は観測可能性側であり TASK-97・RECOVER-6
//!   のスコープ。本モジュールは安全性側のみを担う）。
//!
//!   [`ResponseBoundaryGuard`] はスレッドローカル状態に依存するため、
//!   `wire-server::server` の thread-per-connection 同期モデル（1 コネクション
//!   1 スレッドが直列にクエリを処理する）を前提とする。呼び出し元
//!   （`wire-server::simple_query::execute_and_respond`）が 1 クエリの処理開始時に
//!   1 つ生成し、応答をすべて書き終えた自然な関数末尾（正常 return / unwind の
//!   いずれでも drop される）まで所有する契約。非同期実行系（タスクがスレッドを
//!   跨ぐ）へ移行する場合はこの設計が前提を失うため見直しが必要。

use std::cell::Cell;

use crate::storage::{self, Result as StorageResult};

thread_local! {
    /// commit 成功後、wire 層の応答確定（[`ResponseBoundaryGuard`] の drop）が
    /// まだ済んでいない区間かどうかを表すスレッドローカルフラグ（RECOVER-5 (3)）。
    /// [`commit_and_finish`] が commit 成功のたびに `true` へ立て、
    /// [`ResponseBoundaryGuard`] の drop 時に読み取ってリセットする。
    static COMMIT_PENDING_RESPONSE: Cell<bool> = const { Cell::new(false) };
}

/// commit 成功から wire 層の応答確定までの区間全体を覆う RAII ガード
/// （RECOVER-5 (3)。[`PostCommitPanicGuard`] より広い区間を守る）。
///
/// `wire-server::simple_query::execute_and_respond` が 1 クエリの処理開始時に
/// 生成し、応答をすべて書き終えるまで所有する契約（モジュール冒頭ドキュメント
/// 参照）。区間内で [`commit_and_finish`] が commit に成功すると
/// [`COMMIT_PENDING_RESPONSE`] が立つ。drop 時にこのフラグを読み取ってリセットし、
/// フラグが立った状態で unwind 中（panic 伝播中）であれば
/// `std::process::abort()` する。フラグが立っていない、または unwind 中でなければ
/// （正常終了 = 応答を書き終えた、または commit 前に失敗した）何もしない。
///
/// 注意（フラグはガード区間外でも立ちうる）: [`COMMIT_PENDING_RESPONSE`] は
/// [`commit_and_finish`] が commit に成功するたびに無条件で立てるため、本ガードを
/// スタックに持たないスレッド上の commit（本モジュールのユニットテスト・将来の
/// wire 経由以外の呼び出し元等）でも立つ。その場合は次にそのスレッド上で生成された
/// 本ガードの drop で（panic していなければ）静かにリセットされるのみで実害はない
/// （fail-closed 側に倒れる誤 abort はあり得るが、保護すべき区間の取りこぼしは
/// 発生しない）。
///
/// `must_use` 属性は束縛忘れ（`let _ = ResponseBoundaryGuard::new();` 等で即座に
/// drop され保護区間が消える）をコンパイル時の warning として検出するための注記。
/// 実際に使う際は名前付きの変数（例: `let _response_boundary = ...;`）で応答確定
/// まで保持すること。
#[must_use]
pub struct ResponseBoundaryGuard {
    _private: (),
}

impl ResponseBoundaryGuard {
    /// クエリ処理の入口で呼ぶ。応答をすべて書き終えるまでこの戻り値を
    /// 名前付き変数へ束縛して生存させること（`let _ = ...` は即座に drop され
    /// 保護区間が失われるため使わない）。
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for ResponseBoundaryGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ResponseBoundaryGuard {
    fn drop(&mut self) {
        let armed = COMMIT_PENDING_RESPONSE.with(|f| f.replace(false));
        if should_abort(armed, std::thread::panicking()) {
            // 固定の英語文言のみを stderr へ出す。テナント ID・テーブル名・行データ・
            // operation_id を一切含めない（security.md P0「エラー・ログ経由で他テナントの
            // データ・存在情報を漏らさない」）。
            eprintln!(
                "fatal: panic occurred after a write transaction committed successfully; \
                 aborting the process to avoid returning a false success response"
            );
            std::process::abort();
        }
    }
}

/// commit 成功後の派生状態（索引等）反映の結果。反映が失敗しても、確定済みの
/// 成功応答（commit 自体は成功している）を `Err` へ転化させない
/// （RECOVER-5 (2)）。現状 `crate::tenant`・`crate::txn` の書き込み経路には
/// commit 後の同期的な索引反映が存在しないため、既存呼び出し元はすべて
/// [`PostCommitResult::Ok`] のみを渡す。将来 commit 後の派生状態反映を追加する
/// 際の受け皿として型を用意しておく。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostCommitResult {
    Ok,
    // `#[cfg_attr(not(test), allow(dead_code))]`: 現状 `crate::tenant`・`crate::txn` の
    // 書き込み経路は commit 後の同期的な索引反映を持たず、production 側は常に `Ok` のみを
    // 構築する。本バリアントは (2) の応答一意性（反映失敗を成功応答へ転化させない）を
    // 検証するユニットテスト（本モジュール末尾）専用の到達点であり、通常ビルドでは
    // `dead_code` lint が発火するため黙殺する（`catalog.rs::insert_row_into_table` と
    // 同じ理由・同じパターン）。
    #[cfg_attr(not(test), allow(dead_code))]
    IndexReflectionFailed,
}

/// commit 成功直後から呼び出し元への応答確定までの区間を覆う Drop ガード
/// （RECOVER-5 (3)）。
///
/// - [`Self::armed`] で commit 直後にガードを有効化する。
/// - 応答を確定できる状態（成功で return できる状態）に達したら
///   [`Self::disarm`] を呼ぶ。以降 Drop しても abort しない。
/// - `disarm` を呼ばずに drop された場合（＝ armed のまま区間内で panic して
///   unwind してきた場合）、[`should_abort`] の判定に従い
///   `std::process::abort()` する。
///
/// abort 呼び出し本体と「abort すべきかの判定」（[`should_abort`]）を分離して
/// いるのは、abort 実行はプロセスを道連れにするため in-process では検証できず、
/// 判定側だけを純粋なユニットテストで検証可能にするため。
pub(crate) struct PostCommitPanicGuard {
    armed: bool,
}

impl PostCommitPanicGuard {
    /// commit 成功直後に呼び出し、ガードを有効化した状態で返す。
    pub(crate) fn armed() -> Self {
        Self { armed: true }
    }

    /// 応答確定（成功で return できる状態）に到達した合図。以降 Drop で
    /// abort しない。
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PostCommitPanicGuard {
    fn drop(&mut self) {
        if should_abort(self.armed, std::thread::panicking()) {
            // 固定の英語文言のみを stderr へ出す。テナント ID・テーブル名・行データ・
            // operation_id を一切含めない（security.md P0「エラー・ログ経由で他テナントの
            // データ・存在情報を漏らさない」）。
            eprintln!(
                "fatal: panic occurred after a write transaction committed successfully; \
                 aborting the process to avoid returning a false success response"
            );
            std::process::abort();
        }
    }
}

/// commit 成功後の保護区間内で panic が発生したか（＝ armed のまま unwind して
/// きたか）を純粋に判定する。`armed`（[`PostCommitPanicGuard`] が disarm 済みで
/// ないか）と `panicking`（現在 unwind 中か。呼び出し元は
/// `std::thread::panicking()` を渡す）の両方が真の場合のみ abort すべきと判定する
/// （fail-closed: 判定が曖昧な場合ではなく、armed かつ panicking が確定した場合の
/// みプロセスを止める設計であり、逆に armed が誤って解除された状態を作らないことが
/// 呼び出し元〔`commit_and_finish`〕の責務となる）。
pub(crate) fn should_abort(armed: bool, panicking: bool) -> bool {
    armed && panicking
}

/// commit 成功境界の公開 choke point。`write_txn` は呼び出し元が pre-commit の
/// 検証・書き込みを終え、commit 直前まで組み立て済みのトランザクションを渡す契約
/// （呼び出し元が `Err` を返す場合はこの関数を呼ばず `write_txn` を drop する。
/// drop による abort が RECOVER-5 (1) の「commit 前失敗は副作用ゼロ」契約を担う）。
///
/// 内部で [`crate::storage::bump_generation_and_commit`] を呼んで commit する
/// （世代カウントの経路網羅契約はそのまま維持する）。commit が失敗すればここで
/// `Err` を返して終わる（まだ point of no return に達していない）。commit が
/// 成功したら [`COMMIT_PENDING_RESPONSE`]（[`ResponseBoundaryGuard`] 経由の
/// より広い区間の保護。呼び出し元が wire 層まで所有する）を立てたうえで、
/// さらに [`PostCommitPanicGuard`] を arm した状態で `post_commit` を実行し、
/// 正常に完了できたら disarm してから `(value, post_commit の結果)` を返す
/// （`post_commit` 内で panic した場合は `PostCommitPanicGuard` の Drop が abort
/// する。`post_commit` 完了後・呼び出し元が応答を確定するまでの区間は
/// `COMMIT_PENDING_RESPONSE` を経由して [`ResponseBoundaryGuard`] が引き続き
/// 保護する）。
pub(crate) fn commit_and_finish<T>(
    write_txn: redb::WriteTransaction,
    value: T,
    post_commit: impl FnOnce(&T) -> PostCommitResult,
) -> StorageResult<(T, PostCommitResult)> {
    storage::bump_generation_and_commit(write_txn)?;
    COMMIT_PENDING_RESPONSE.with(|f| f.set(true));
    let guard = PostCommitPanicGuard::armed();
    let post_commit_result = post_commit(&value);
    guard.disarm();
    Ok((value, post_commit_result))
}

/// [`commit_and_finish`] の薄いラッパ。`crate::tenant` の各 `*_unchecked`
/// 関数（commit 後の派生状態反映を持たない）から呼ばれる想定で、
/// `post_commit` は常に [`PostCommitResult::Ok`] を返す no-op とする。
pub(crate) fn commit(write_txn: redb::WriteTransaction) -> StorageResult<()> {
    commit_and_finish(write_txn, (), |()| PostCommitResult::Ok).map(|((), _)| ())
}

/// `crate::txn::WriteTxn`/`BatchWriteTxn` の commit 一本化点
/// （`crate::storage::commit_write_txn` と同じ `has_writes` 契約）から呼ばれる。
/// `has_writes == false` の場合は commit させず abort する既存契約
/// （[`crate::storage::commit_write_txn`] 参照）をそのまま踏襲する ――
/// commit していない区間には保護対象の point of no return が存在しないため、
/// ガードを arm する必要がない。`has_writes == true` の場合のみ
/// [`commit`] 経由でガード区間に載せる。
pub(crate) fn commit_write_txn_guarded(
    write_txn: redb::WriteTransaction,
    has_writes: bool,
) -> StorageResult<()> {
    if has_writes {
        commit(write_txn)
    } else {
        storage::commit_write_txn(write_txn, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    // --- should_abort の全分岐（RECOVER-5 (3) の判定純関数）---

    #[test]
    fn should_abort_true_only_when_armed_and_panicking() {
        assert!(should_abort(true, true));
        assert!(!should_abort(true, false));
        assert!(!should_abort(false, true));
        assert!(!should_abort(false, false));
    }

    // --- ガードの正常系: disarm 済みなら drop しても abort しない ---
    // （abort すればテストプロセスごと落ちるため、「abort しなかった」ことは
    // このテスト自身が最後まで完走することで示される）

    #[test]
    fn guard_disarmed_does_not_abort_on_drop() {
        let guard = PostCommitPanicGuard::armed();
        guard.disarm();
        // drop はスコープ終端で暗黙的に走る。abort されないことをテスト完走で示す。
    }

    // --- commit_and_finish: RECOVER-5 (2) 応答一意性 ---
    // 派生状態反映が失敗しても、commit 済みの成功応答は Err へ転化しない。

    #[test]
    fn commit_and_finish_keeps_success_even_if_post_commit_reflection_fails() {
        let path = unique_db_path("commit-boundary-post-commit-fail");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let write_txn = storage.db().begin_write().expect("begin_write");
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0],
            metadata: &[],
        };
        {
            let mut table = write_txn
                .open_table(crate::storage::ROWS_TABLE)
                .expect("open rows table");
            let encoded = crate::storage::encode_row(&row).expect("encode row");
            table
                .insert(("tenant-a", 1u64), encoded.as_slice())
                .expect("insert");
        }

        let (value, post_commit_result) =
            commit_and_finish(write_txn, 1u64, |_| PostCommitResult::IndexReflectionFailed)
                .expect("commit must succeed even though post_commit reports a failure");

        assert_eq!(value, 1u64);
        assert_eq!(post_commit_result, PostCommitResult::IndexReflectionFailed);

        // commit 自体は成功しているため、再オープン後も行が可視であること。
        drop(storage);
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 1u64)
            .expect("committed row must remain visible after reopen");
    }

    // --- commit_write_txn_guarded: has_writes == false は commit させず abort する ---

    #[test]
    fn commit_write_txn_guarded_no_writes_does_not_bump_generation() {
        let path = unique_db_path("commit-boundary-no-writes");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let before = storage.current_generation().expect("read generation");
        let write_txn = storage.db().begin_write().expect("begin_write");
        commit_write_txn_guarded(write_txn, false).expect("no-op commit must succeed");
        let after = storage.current_generation().expect("read generation");

        assert_eq!(
            before, after,
            "no-writes commit must not advance generation"
        );
    }

    // --- RECOVER-5 (3): commit 成功後 panic のプロセス終了保証（サブプロセス検証）---
    //
    // `PostCommitPanicGuard` の abort 実行はプロセスを道連れにするため in-process では
    // 検証できない。`std::env::current_exe()`（cargo test が生成する本クレートの
    // テストバイナリ自身）を、環境変数付きでこのテスト関数だけをフィルタして
    // 再実行する自己再帰構造で子プロセス化する（`examples/crash_tool.rs` の
    // プロセス外検証パターンと同型。新規バイナリ・feature ゲート API は追加しない
    // ―― 子プロセスは本クレートの既存テストバイナリそのもの）。
    const CHILD_DB_ENV: &str = "ENGINE_COMMIT_BOUNDARY_PANIC_CHILD_DB";

    #[test]
    fn subprocess_post_commit_panic_aborts_before_returning_success() {
        if let Ok(db_path) = std::env::var(CHILD_DB_ENV) {
            // 子プロセス側: 実際に行を commit したうえで post_commit 内で panic を
            // 注入し、`PostCommitPanicGuard` の abort へ委ねる。
            let storage = Storage::open(&db_path).expect("child: open storage");
            let write_txn = storage.db().begin_write().expect("child: begin_write");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            };
            {
                let mut table = write_txn
                    .open_table(crate::storage::ROWS_TABLE)
                    .expect("child: open rows table");
                let encoded = crate::storage::encode_row(&row).expect("child: encode row");
                table
                    .insert(("tenant-a", 7u64), encoded.as_slice())
                    .expect("child: insert");
            }
            let _ = commit_and_finish(write_txn, (), |()| {
                panic!("injected post-commit panic for RECOVER-5 (3) verification")
            });
            // ここへ到達したのはガードが abort しなかった不具合。親側が判定できる
            // 固定マーカーを出してから異常終了する。
            println!("CHILD_REACHED_AFTER_PANIC_GUARD");
            std::process::exit(1);
        }

        // 親プロセス側。
        let path = unique_db_path("commit-boundary-subprocess-panic");
        let _cleanup = CleanupGuard(path.clone());
        // 子が開けるよう、DB ファイルを先に作ってから閉じる。
        drop(Storage::open(&path).expect("parent: create storage"));

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg(
                "recovery::commit_boundary::tests::\
                 subprocess_post_commit_panic_aborts_before_returning_success",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_DB_ENV, &path)
            .stdout(std::process::Stdio::piped())
            // stderr は破棄する（`Stdio::null()`）。テストは stdout の固定マーカーと
            // 終了コード・シグナルのみを検証し stderr の内容を読まないため、pipe のまま
            // 放置すると panic hook の出力（backtrace 等。`RUST_BACKTRACE` 有効時に
            // 顕著）で OS のパイプバッファが埋まり、子が `abort()` 前に write でブロック
            // して親が下記 30 秒タイムアウトで誤って失敗しうる（flaky の温床。
            // cursor[bot] レビュー指摘対応・PR #246）。親側は try_wait のみをポーリング
            // し pipe を能動的に読まない設計のため、そもそも pipe せず捨てるのが最小修正。
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child process");

        // CI ハング防止のタイムアウト（`wait_timeout` 等の追加依存を使わず、
        // `try_wait` をポーリングして上限超過時は kill する）。
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("subprocess did not terminate within {timeout:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        use std::io::Read as _;
        let mut stdout_buf = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout_buf);
        }

        assert!(
            !stdout_buf.contains("CHILD_REACHED_AFTER_PANIC_GUARD"),
            "guard failed to abort before returning a response: stdout={stdout_buf}"
        );
        assert!(
            !status.success(),
            "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            // `std::process::abort()` は Unix では SIGABRT（== 6）で終了する契約。
            // libc への新規依存を避け、数値を直接比較する。
            assert_eq!(
                status.signal(),
                Some(6),
                "child must be terminated by SIGABRT (std::process::abort); \
                 status={status:?} stdout={stdout_buf}"
            );
        }

        // commit 自体は成功しているため、再オープン後も行が可視であること
        // （(3) は commit 後の panic であり、副作用〔commit〕自体は残る ――
        // 保証対象は「commit しないこと」ではなく「成功応答を返さないこと」）。
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 7u64)
            .expect("committed row must remain visible after subprocess abort");
    }

    // --- RECOVER-5 (3) 拡張区間: commit_and_finish が正常 return（disarm 済み）した
    // 後、`ResponseBoundaryGuard` がまだ生存している区間（＝ wire 層が応答を組み立て・
    // 送信する区間の模擬）で panic しても abort すること。
    // codex-review P1・PR #246 指摘（`commit_and_finish` の disarm 直後から応答確定
    // までが未保護だった点）の回帰テスト。
    const CHILD_DB_ENV_RESPONSE_BOUNDARY: &str =
        "ENGINE_COMMIT_BOUNDARY_RESPONSE_BOUNDARY_PANIC_CHILD_DB";

    #[test]
    fn subprocess_panic_after_commit_and_finish_returns_still_aborts_within_response_boundary() {
        if let Ok(db_path) = std::env::var(CHILD_DB_ENV_RESPONSE_BOUNDARY) {
            // 子プロセス側: wire 層（`simple_query::execute_and_respond`）が
            // クエリ処理の入口で `ResponseBoundaryGuard` を生成してから
            // `commit_and_finish` を呼び、正常 return を受け取った後（＝ 応答の
            // 組み立て・送信中を模擬する区間）で panic する、という呼び出し順を
            // 直接再現する。
            let storage = Storage::open(&db_path).expect("child: open storage");
            let write_txn = storage.db().begin_write().expect("child: begin_write");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            };
            {
                let mut table = write_txn
                    .open_table(crate::storage::ROWS_TABLE)
                    .expect("child: open rows table");
                let encoded = crate::storage::encode_row(&row).expect("child: encode row");
                table
                    .insert(("tenant-a", 9u64), encoded.as_slice())
                    .expect("child: insert");
            }

            let _response_boundary = ResponseBoundaryGuard::new();
            let (_, post_commit_result) =
                commit_and_finish(write_txn, (), |()| PostCommitResult::Ok)
                    .expect("child: commit_and_finish must succeed");
            assert_eq!(post_commit_result, PostCommitResult::Ok);

            // `commit_and_finish` は既に disarm 済みで正常 return している
            // （旧実装ならここから先は無保護）。`_response_boundary` はまだ
            // 生存中のため、ここで panic しても abort するはずである。
            panic!("injected panic in the wire-layer response-assembly window (post commit_and_finish)");
        }

        // 親プロセス側。
        let path = unique_db_path("commit-boundary-subprocess-response-boundary-panic");
        let _cleanup = CleanupGuard(path.clone());
        drop(Storage::open(&path).expect("parent: create storage"));

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg(
                "recovery::commit_boundary::tests::\
                 subprocess_panic_after_commit_and_finish_returns_still_aborts_within_response_boundary",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_DB_ENV_RESPONSE_BOUNDARY, &path)
            .stdout(std::process::Stdio::piped())
            // 上のテスト同様、stderr は pipe せず破棄する（パイプバッファ詰まり回避）。
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child process");

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("subprocess did not terminate within {timeout:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        use std::io::Read as _;
        let mut stdout_buf = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout_buf);
        }

        assert!(
            !status.success(),
            "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(
                status.signal(),
                Some(6),
                "child must be terminated by SIGABRT (std::process::abort) even though the \
                 panic occurred after commit_and_finish already returned; \
                 status={status:?} stdout={stdout_buf}"
            );
        }

        // commit 自体は成功しているため、再オープン後も行が可視であること。
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 9u64)
            .expect("committed row must remain visible after subprocess abort");
    }
}
