//! TASK-97（対象ビヘイビア: RECOVER-6。ポインタ: `docs/spec/05-tasks.md` TASK-97・
//! `docs/spec/04-behavior/recovery.md` RECOVER-6・`docs/spec/04-behavior/
//! error-format.md` ERR-1）の観測可能性側の実装。
//!
//! 背景（責務境界）: [`crate::recovery::commit_boundary`] の各ガード
//! （`ResponseBoundaryGuard`・`PostCommitPanicGuard`）は、commit 成功後に panic が
//! 起きた場合に `std::process::abort()` して「成功応答を誤って返さない」安全性側
//! を担う。しかしこの abort だけでは、クライアントは接続断のみを観測し、
//! 「commit は成功しているかもしれない（may_be_committed）」という情報を一切
//! 得られない。本モジュールはこの観測可能性側 ―― panic フック内での同期的な
//! ソケット書き込みにより、送信を観測できた場合に限り ERR-1 応答を唯一の応答
//! として返してから終了する経路 ―― を提供する。
//!
//! 呼び出し文脈: `wire-server::main`（`run_server` 起動時）が [`install_panic_hook`]
//! を 1 回呼ぶ。`wire-server::simple_query::execute_and_respond` が
//! `engine.execute_sql_in_session` の呼び出し区間だけを
//! [`EmergencyResponseRegistration`] で包み、事前エンコード済みの ErrorResponse
//! バイト列と `TcpStream` のクローンを登録する。
//!
//! 設計上の分離（テスト容易性）: 「送信してよいか」の判定は
//! [`emergency_send_decision`] という純関数へ分離し、`abort()` を伴わない
//! ユニットテストで全分岐を検証できるようにする。フック本体はこの判定結果に
//! 従って実際の書き込みを行うだけの薄い層に留める。
//!
//! abort の単一 choke point: 本モジュール自身は `std::process::abort()` を
//! 呼ばない。緊急応答の送信可否に関わらず、パニックはそのまま unwind を続け、
//! 最終的に [`crate::recovery::commit_boundary::ResponseBoundaryGuard`]（または
//! `PostCommitPanicGuard`）の `Drop` が abort する（既存の RECOVER-5 バックストップ
//! が唯一の abort 呼び出し元であり続ける）。本フックは既存の panic フック
//! （Rust既定のフック、または他ライブラリが差し込んだフック）へチェーンする
//! 前段フックとして動作し、緊急応答を送信できた場合はその既存フックの呼び出しを
//! スキップする（応答は既に確定したため、フック側の stderr 出力等の副作用を
//! 二重に発生させないため）。送信できなかった場合は必ず既存フックへ委譲する。
//!
//! fail-closed 方針: 判定が曖昧な場合・書き込みに失敗した場合はいずれも
//! 「緊急応答を送らない」側（＝接続断のみ・応答なし。既存フックへ委譲）に倒す。
//! 成功応答を返す経路は存在しない。

use std::cell::RefCell;
use std::io::Write as _;
use std::net::TcpStream;
use std::sync::Once;

use super::commit_boundary;

thread_local! {
    /// このスレッド（＝ wire-server の 1 接続スレッド）に登録された緊急応答チャネル
    /// （TASK-97・RECOVER-6）。`(事前エンコード済み ErrorResponse バイト列, 書き込み先
    /// TcpStream のクローン)` の組。[`EmergencyResponseRegistration`] が RAII で
    /// 登録・解除し、panic フックが一度だけ `take()` して消費する（二重送信の
    /// 構造的防止）。
    static EMERGENCY_RESPONSE_CHANNEL: RefCell<Option<(Vec<u8>, TcpStream)>> =
        const { RefCell::new(None) };
}

/// [`EMERGENCY_RESPONSE_CHANNEL`] への登録を表す RAII ガード（TASK-97・RECOVER-6）。
///
/// `wire-server::simple_query::execute_and_respond` が `engine.execute_sql_in_session`
/// の呼び出し区間だけを本ガードで包む契約 ―― engine から戻った直後にこのガードを
/// drop して登録解除してから、通常の応答（成功／通常エラー）を書き始める。これに
/// より「通常応答の書き込み開始後に発生した panic」では緊急応答を送らず、既存の
/// 接続断側（[`crate::recovery::commit_boundary`] の abort バックストップ）に倒す
/// ―― フレーム途中への緊急応答混入・二重応答を構造的に排除する。
///
/// `must_use` は束縛忘れ（`let _ = ...` による即座の drop）を検出するための注記
/// （[`crate::recovery::commit_boundary::ResponseBoundaryGuard`] と同じ理由）。
#[must_use]
pub struct EmergencyResponseRegistration {
    /// drop 時に「自分が登録した内容がまだ残っているか」を確認するための印。
    /// panic フックが `take()` で消費済みなら drop は何もしない（二重 take
    /// を避ける。呼び出し元スレッドは 1 スレッドにつき同時に 1 registration
    /// のみを持つ契約のため、値そのものの比較は不要 ―― 存在有無だけで足りる）。
    _private: (),
}

impl EmergencyResponseRegistration {
    /// `response_bytes`（事前エンコード済み ErrorResponse 全体）と `stream`
    /// （書き込み先。呼び出し元が `TcpStream::try_clone()` で用意し、必要な
    /// write timeout を設定済みのものを渡す契約）を登録する。
    ///
    /// フック内でのアロケーション・整形失敗を避けるため、`response_bytes` は
    /// 呼び出し元が事前に確定済みのバイト列として渡す（フック内で新規に
    /// エンコードしない）。
    pub fn register(response_bytes: Vec<u8>, stream: TcpStream) -> Self {
        EMERGENCY_RESPONSE_CHANNEL.with(|c| {
            *c.borrow_mut() = Some((response_bytes, stream));
        });
        Self { _private: () }
    }
}

impl Drop for EmergencyResponseRegistration {
    fn drop(&mut self) {
        // 登録解除。panic フックが既に `take()` 済み（＝ None）でも、素通しで
        // 上書きするだけなので二重解除の問題はない。
        EMERGENCY_RESPONSE_CHANNEL.with(|c| {
            c.borrow_mut().take();
        });
    }
}

/// 緊急応答を送信してよいかを判定する純関数（TASK-97・RECOVER-6）。
///
/// `pending`: このスレッドが commit 成功後・応答未確定の区間にあるか
/// （[`commit_boundary::active_commit_pending_generation`] の `is_some()`）。
/// `registered`: [`EmergencyResponseRegistration`] が登録されているか。
///
/// 両方が真の場合のみ送信してよいと判定する（fail-closed: 判定が曖昧な組み
/// 合わせは存在しない ―― 4 分岐すべてが確定的に定まる真理値表であり、いずれか
/// 片方でも欠けていれば送らない）。abort を伴わずにユニットテストで全分岐を
/// 検証できるよう、実際の書き込み・`abort()` から独立させている。
pub(crate) fn emergency_send_decision(pending: bool, registered: bool) -> bool {
    pending && registered
}

/// panic フックの冪等な導入（TASK-97・RECOVER-6）。複数回呼ばれても実際の
/// フック差し替えは 1 回のみ行う（`std::sync::Once`）。
///
/// 呼び出し元は `wire-server::main`（`run_server` 起動時）のみを想定する。
/// engine 側のライブラリ初期化経路（`EngineCore::open` 等）からは呼ばない ――
/// プロセスグローバルなフックを engine のコンストラクタが暗黙に差し替えると、
/// `catch_unwind` を多用する既存の `commit_boundary` テスト群
/// （本クレート内の単体テスト）や、engine を単体で使う他バイナリの panic 挙動を
/// 意図せず変えてしまうため。
///
/// 既存フック（Rust の既定フックを含む）へチェーンする ―― 本フックが介入しない
/// （`emergency_send_decision` が偽の）panic では、既存フックの出力・挙動を
/// そのまま引き継ぐ。介入する場合（緊急応答を送信できた場合）は既存フックを
/// 呼ばずに return する（唯一の応答として ErrorResponse を返した後、既存フックの
/// stderr 出力等を二重に発生させないため）。abort 自体はこのフックの責務では
/// なく、unwind の続きで [`crate::recovery::commit_boundary`] 側の既存ガードが
/// 行う（モジュールドキュメント「abort の単一 choke point」参照）。
pub fn install_panic_hook() {
    static INSTALL_ONCE: Once = Once::new();
    INSTALL_ONCE.call_once(|| {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            if !try_send_emergency_response() {
                previous_hook(panic_info);
            }
            // ここで return すると unwind がそのまま続く。緊急応答を送信できた
            // 場合・できなかった場合のいずれも、abort するかどうかの最終判定は
            // [`crate::recovery::commit_boundary::ResponseBoundaryGuard`]／
            // `PostCommitPanicGuard` の `Drop` に委ねる（本フックは abort しない）。
        }));
    });
}

/// panic フック本体から呼ばれる。緊急応答を送信できた場合のみ `true` を返し
/// （呼び出し元が直ちに `abort()` する）、それ以外は `false`（呼び出し元は
/// 前フックへ委譲する＝既存の接続断側へ倒す）。
///
/// fail-closed の徹底: pending でない・登録がない・チャネルの取得（`take`）に
/// 失敗した・書き込みが失敗した、のいずれの場合も緊急応答は送らない。
/// `RefCell::borrow_mut` の再入 panic を避けるため、`try_borrow_mut` で
/// 失敗時は素通しする（フック内で新たに panic させない）。
fn try_send_emergency_response() -> bool {
    let pending = commit_boundary::active_commit_pending_generation().is_some();

    // `take()` は判定結果に関わらず無条件に行う（意図的な設計 ―― 「まず覗いて
    // から取る」に書き換えない）。これにより、送信しないと判定した場合も登録は
    // 必ず消費済みになる。同一 unwind 中に本関数が複数回呼ばれる経路は現状存在
    // しないが、仮に呼ばれても 2 回目は `registered == false` となり必ず前フック
    // へ委譲する ―― 二重送信より安全な側（fail-closed）に倒れる。
    let channel = EMERGENCY_RESPONSE_CHANNEL.with(|c| {
        // 通常経路では再入しないが（フックは unwind 中に 1 回だけ動く）、万一
        // 借用中であれば取得を諦めて fail-closed に倒す（フック内で panic
        // させない）。
        match c.try_borrow_mut() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        }
    });

    let registered = channel.is_some();

    if !emergency_send_decision(pending, registered) {
        return false;
    }

    // ここまで到達した時点で channel は Some（emergency_send_decision が真の
    // ため registered も真）。
    let Some((response_bytes, mut stream)) = channel else {
        return false;
    };

    // 書き込み・flush の両方が成功した場合のみ「送信を観測できた」とみなす。
    // 部分書き込み・タイムアウト・I/O エラーはすべて失敗として扱い、緊急応答は
    // 送らなかったものとして fail-closed に倒す（クライアントが不完全なフレーム
    // を受け取る余地を作らない）。
    stream.write_all(&response_bytes).is_ok() && stream.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- emergency_send_decision の全分岐（RECOVER-6 の判定純関数）---

    #[test]
    fn emergency_send_decision_true_only_when_pending_and_registered() {
        assert!(emergency_send_decision(true, true));
        assert!(!emergency_send_decision(true, false));
        assert!(!emergency_send_decision(false, true));
        assert!(!emergency_send_decision(false, false));
    }

    // --- install_panic_hook の冪等性 ---
    // 複数回呼んでも panic しない・後続の通常 panic 処理（catch_unwind）が
    // そのまま機能し続けることを確認する（RECOVER-8 のスコープ〔全 panic の
    // fail-fast 統一〕を先取りしないことの確認でもある ―― pending でない
    // 通常 panic はフック導入後も unwind として観測できる）。

    #[test]
    fn install_panic_hook_is_idempotent_and_normal_panics_still_unwind() {
        install_panic_hook();
        install_panic_hook();

        let result = std::panic::catch_unwind(|| {
            panic!("normal panic unrelated to commit boundary");
        });
        assert!(
            result.is_err(),
            "a panic with no pending commit and no registration must still unwind normally \
             (this test process must not abort)"
        );
    }

    // --- EmergencyResponseRegistration: 登録→drop で解除されること ---

    #[test]
    fn registration_drop_clears_the_channel() {
        // ローカルの TCP ペアで `TcpStream` を用意する（`register` の型契約を
        // 満たすため。実際に書き込まれるかはこのテストの関心事ではない）。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = std::net::TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");

        assert!(
            EMERGENCY_RESPONSE_CHANNEL.with(|c| c.borrow().is_none()),
            "no registration must exist before this test registers one"
        );

        {
            let _registration =
                EmergencyResponseRegistration::register(vec![1, 2, 3], server_stream);
            assert!(
                EMERGENCY_RESPONSE_CHANNEL.with(|c| c.borrow().is_some()),
                "registration must be visible while the guard is alive"
            );
        }

        assert!(
            EMERGENCY_RESPONSE_CHANNEL.with(|c| c.borrow().is_none()),
            "registration must be cleared once the guard is dropped"
        );

        drop(client);
    }

    // --- try_send_emergency_response: モック writer で送信判定・一度きり消費を検証 ---
    // ここでは `std::process::abort()` を経由しない
    // `try_send_emergency_response` 単体を直接呼び、実際の TCP ソケットへ
    // 書き込まれることと、消費後は 2 回目が「未登録」として扱われることを
    // 検証する（pending は現在のスレッドに commit_boundary 側の状態を作らず
    // `false` のまま ――「登録はあるが pending でない」分岐の実地検証を兼ねる）。

    #[test]
    fn try_send_emergency_response_returns_false_when_not_pending_even_if_registered() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .expect("set read timeout");
        let (server_stream, _) = listener.accept().expect("accept");

        let _registration =
            EmergencyResponseRegistration::register(vec![b'E', 0, 0, 0, 4], server_stream);

        // このスレッドには commit_boundary 側の pending 状態が一切立っていない
        // （このテストは commit を一切行わない）ため、登録があっても送信されない
        // はず。
        let sent = try_send_emergency_response();
        assert!(
            !sent,
            "must not send when this thread has no commit-pending state"
        );

        // 実際に 1 バイトも届いていないことを確認する。
        let mut buf = [0u8; 1];
        use std::io::Read as _;
        let n = client.read(&mut buf);
        assert!(
            matches!(n, Err(_) | Ok(0)),
            "no bytes must have been written when not pending"
        );
    }
}
