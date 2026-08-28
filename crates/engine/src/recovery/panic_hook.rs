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
//! 「outcome を決定する区間」（`engine.execute_sql_in_session` の呼び出しを含む
//! ブロック）を [`EmergencyResponseRegistration`] で包み、事前エンコード済みの
//! ErrorResponse バイト列・`TcpStream` のクローン・緊急応答書き込み用の write
//! timeout を登録する。登録はブロック終端でレキシカルに drop され、応答の組み
//! 立て・送信区間には及ばない（[`EmergencyResponseRegistration`] のドキュメント
//! 参照 ―― 応答書き込み中の panic で緊急応答を書きかけの通常応答フレームへ
//! 追記してしまう事故を構造的に防ぐ）。
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
use std::time::Duration;

use super::commit_boundary;

/// [`EMERGENCY_RESPONSE_CHANNEL`] の要素型: `(事前エンコード済み ErrorResponse
/// バイト列, 書き込み先 TcpStream のクローン, 登録時点の ResponseBoundaryGuard
/// 世代, 緊急応答書き込み時に適用する write timeout)`。型を独立させているのは
/// 可読性のためだけでなく、clippy `type_complexity` を素直に満たすため（実質的な
/// 意味は変わらない）。
///
/// write timeout を登録時にソケットへ設定せず値のまま保持する理由（TASK-97・
/// codex-review Medium 指摘対応・PR #90）: `TcpStream::try_clone` は同一
/// ソケットの複製であり（`SO_SNDTIMEO` はソケット共有）、登録時に短い緊急応答用
/// タイムアウトをソケットへ設定してしまうと、登録解除後も元の `stream`
/// 側のタイムアウトが変わったままになる（呼び出し元が明示的に復元しない限り）。
/// 値のまま保持し、実際に緊急応答を書き込む直前（[`try_send_emergency_response`]）
/// にのみ適用することで、登録スコープを抜けた後の通常応答書き込みへ一切影響
/// させず、呼び出し元にタイムアウト復元の責務を負わせない。
type EmergencyChannelEntry = (Vec<u8>, TcpStream, Option<u64>, Duration);

thread_local! {
    /// このスレッド（＝ wire-server の 1 接続スレッド）に登録された緊急応答チャネル
    /// （TASK-97・RECOVER-6）。[`EmergencyChannelEntry`] の 3 要素目（世代）は
    /// [`EmergencyResponseRegistration::register`] が
    /// [`commit_boundary::current_response_boundary_generation`] を捕捉して保持し、
    /// panic フックが [`commit_boundary::active_commit_pending_generation`] と
    /// 突き合わせる（登録されたバイト列が指す commit と、実際に commit-pending に
    /// なった commit が同一世代であることを保証する ―― `commit_boundary` 側の
    /// `commit` は `execute_sql_in_session` 以外の経路〔`Storage::put`/`put_batch`・
    /// `catalog` の DDL〕からも直接呼ばれうるため、同一スレッド内で世代の異なる
    /// commit が pending に紛れ込む可能性を構造的に排除する）。
    /// [`EmergencyResponseRegistration`] が RAII で登録・解除し、panic フックが
    /// 一度だけ `take()` して消費する（二重送信の構造的防止）。
    static EMERGENCY_RESPONSE_CHANNEL: RefCell<Option<EmergencyChannelEntry>> =
        const { RefCell::new(None) };
}

/// [`EMERGENCY_RESPONSE_CHANNEL`] への登録を表す RAII ガード（TASK-97・RECOVER-6）。
///
/// `wire-server::simple_query::execute_and_respond` が「outcome を決定する区間」
/// （`engine.execute_sql_in_session` の呼び出しを含むブロック）だけを本ガードで
/// 包む契約 ―― ブロックを抜けた（＝ engine から戻った）直後にこのガードが
/// レキシカルに drop されて登録解除されてから、通常の応答（成功／通常エラー）を
/// 書き始める。これにより「通常応答の書き込み開始後に発生した panic」では緊急
/// 応答を送らず、既存の接続断側（[`crate::recovery::commit_boundary`] の abort
/// バックストップ）に倒す ―― フレーム途中への緊急応答混入・二重応答を構造的に
/// 排除する。呼び出し元がこの境界をブロックスコープで表現し、`drop()` の明示
/// 呼び出しに頼らないのは、応答書き込み開始前に必ず解除される構造そのものを
/// 保証するため（手動 `drop` は後から差し込まれたコードに取り残されうる）。
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
    /// （書き込み先。呼び出し元が `TcpStream::try_clone()` で用意したものを渡す
    /// 契約）、`write_timeout`（実際に緊急応答を書き込む直前にのみ適用する write
    /// timeout。[`EmergencyChannelEntry`] のドキュメント参照）を登録する。
    ///
    /// フック内でのアロケーション・整形失敗を避けるため、`response_bytes` は
    /// 呼び出し元が事前に確定済みのバイト列として渡す（フック内で新規に
    /// エンコードしない）。`write_timeout` の値自体もここではソケットへ設定
    /// しない（[`EmergencyChannelEntry`] 参照）。
    ///
    /// 登録時点の [`commit_boundary::current_response_boundary_generation`] を
    /// 併せて捕捉する。呼び出し元（`wire-server::simple_query::
    /// execute_and_respond`）は本関数の呼び出し前に必ず `ResponseBoundaryGuard`
    /// を生成済みの契約のため、通常経路ではここが `None` になることはない。
    /// 仮に `None`（アクティブなガードなし）で登録された場合は、
    /// [`emergency_send_decision`] が常に偽と判定するため送信されない側
    /// （fail-closed）に倒れる。
    pub fn register(response_bytes: Vec<u8>, stream: TcpStream, write_timeout: Duration) -> Self {
        let generation = commit_boundary::current_response_boundary_generation();
        EMERGENCY_RESPONSE_CHANNEL.with(|c| {
            *c.borrow_mut() = Some((response_bytes, stream, generation, write_timeout));
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
/// `pending_generation`: [`commit_boundary::active_commit_pending_generation`]
/// の返り値（このスレッドが commit 成功後・応答未確定の区間にあるなら、その
/// commit を記録した `ResponseBoundaryGuard` の世代）。
/// `registered_generation`: [`EmergencyResponseRegistration::register`] が
/// 登録時点で捕捉した世代（[`commit_boundary::current_response_boundary_generation`]）。
///
/// 両方が `Some` で、かつ同一世代の場合のみ送信してよいと判定する（fail-closed:
/// 曖昧な組み合わせは存在しない ―― pending でない・登録がない・登録はあるが
/// 世代が異なる〔登録済みバイト列が指す commit とは別の commit が pending に
/// なっている〕のいずれも送らない）。世代の一致を要求する理由は
/// [`EMERGENCY_RESPONSE_CHANNEL`] のドキュメント参照。abort を伴わずに
/// ユニットテストで全分岐を検証できるよう、実際の書き込み・`abort()` から
/// 独立させている。
pub(crate) fn emergency_send_decision(
    pending_generation: Option<u64>,
    registered_generation: Option<u64>,
) -> bool {
    pending_generation.is_some() && pending_generation == registered_generation
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
    let pending_generation = commit_boundary::active_commit_pending_generation();

    // `take()` は判定結果に関わらず無条件に行う（意図的な設計 ―― 「まず覗いて
    // から取る」に書き換えない）。これにより、送信しないと判定した場合も登録は
    // 必ず消費済みになる。同一 unwind 中に本関数が複数回呼ばれる経路は現状存在
    // しないが、仮に呼ばれても 2 回目は登録なしとなり必ず前フックへ委譲する ――
    // 二重送信より安全な側（fail-closed）に倒れる。
    let channel = EMERGENCY_RESPONSE_CHANNEL.with(|c| {
        // 通常経路では再入しないが（フックは unwind 中に 1 回だけ動く）、万一
        // 借用中であれば取得を諦めて fail-closed に倒す（フック内で panic
        // させない）。
        match c.try_borrow_mut() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        }
    });

    let registered_generation = channel
        .as_ref()
        .and_then(|(_, _, generation, _)| *generation);

    if !emergency_send_decision(pending_generation, registered_generation) {
        return false;
    }

    // ここまで到達した時点で channel は Some（emergency_send_decision が真の
    // ため registered_generation も Some ―― channel が None なら
    // registered_generation も None になり判定は偽になっている）。
    let Some((response_bytes, mut stream, _generation, write_timeout)) = channel else {
        return false;
    };

    // 緊急応答を書き込む直前にのみ write timeout を適用する（登録時には設定
    // しない理由は [`EmergencyChannelEntry`] のドキュメント参照）。プロセスは
    // この直後に unwind の続き（`commit_boundary` 側ガードの abort）で終了する
    // ため、設定を元に戻す必要はない。設定自体が失敗しても、呼び出し元
    // （`wire-server::simple_query::execute_and_respond`）が渡すクローンは
    // 接続受理直後に `server.rs` が設定した既定の読み書きタイムアウト
    // （`wire-server::limits::READ_TIMEOUT`。無期限ではなく有界値）をまだ
    // 引き継いでいるため、以降の書き込みが無期限にブロックすることはない。
    let _ = stream.set_write_timeout(Some(write_timeout));

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
    // pending・registered それぞれ「なし」「世代 X」「世代 Y（X と異なる）」の
    // 3 値を取りうるため、単純な bool 4 分岐ではなく世代の一致まで検証する。

    #[test]
    fn emergency_send_decision_true_only_when_pending_and_registered_generations_match() {
        assert!(emergency_send_decision(Some(1), Some(1)));
        assert!(!emergency_send_decision(Some(1), Some(2)));
        assert!(!emergency_send_decision(Some(1), None));
        assert!(!emergency_send_decision(None, Some(1)));
        assert!(!emergency_send_decision(None, None));
    }

    // --- install_panic_hook の冪等性 ---
    // 複数回呼んでも panic しない・後続の通常 panic 処理（catch_unwind）が
    // そのまま機能し続けることを確認する。RECOVER-8（全 panic の fail-fast
    // 統一）は別途 `engine::recovery::fail_fast::install` が明示的な opt-in
    // として提供し、本テストプロセス（engine 単体のテストバイナリ）はそれを
    // 呼ばない ―― そのため pending でない通常 panic はフック導入後も unwind
    // として観測できる（fail-fast は `wire-server::main::run_server` からのみ
    // 導入される契約。`fail_fast` モジュールドキュメント参照）。

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
            let _registration = EmergencyResponseRegistration::register(
                vec![1, 2, 3],
                server_stream,
                Duration::from_secs(5),
            );
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

    // --- 世代不一致: 登録済みチャネルの世代と実際に pending になった世代が
    // 異なる場合は送信しないこと（TASK-97・codex-review 指摘対応。
    // `commit_boundary::commit` は `execute_sql_in_session` 以外の経路からも
    // 呼ばれうるため、登録時と commit 時で世代がずれるケースを実地で再現する）。

    #[test]
    fn try_send_emergency_response_does_not_send_when_registered_generation_differs_from_pending() {
        use crate::storage::{RowInput, Storage, Visibility};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("panic-hook-generation-mismatch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = std::net::TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .expect("set read timeout");
        let (server_stream, _) = listener.accept().expect("accept");

        // 1 個目のガード配下で緊急応答を登録する（登録時の世代 = このガードの
        // 世代）。commit は伴わせず、ガードのみを drop する。`_registration` は
        // このブロックの外（テスト関数のトップレベル）で束縛し、ガード a の
        // drop 後も RAII のまま生存させる ――
        // `std::mem::forget` によるリーク（drop 自体の回避）ではなく、
        // 単に生存スコープをテスト末尾まで広げるだけなので、
        // `EMERGENCY_RESPONSE_CHANNEL` の登録解除は本テスト関数を抜ける際に
        // 通常の Drop 経由で必ず行われる（codex-review 指摘対応・PR #90）。
        let _registration;
        {
            let _guard_a = commit_boundary::ResponseBoundaryGuard::new();
            _registration = EmergencyResponseRegistration::register(
                vec![b'E', 0, 0, 0, 4],
                server_stream,
                Duration::from_secs(5),
            );

            // ガード a を drop する前に、ここまでの登録済み世代を検証する
            // （後続の commit がこの世代と一致しないことを示すための前提確認）。
            let registered_generation = EMERGENCY_RESPONSE_CHANNEL
                .with(|c| c.borrow().as_ref().and_then(|(_, _, g, _)| *g));
            assert!(
                registered_generation.is_some(),
                "registration inside an active guard must capture Some(generation)"
            );
        }
        // guard_a が drop され、次に生成するガードは新しい世代を払い出す。
        // `_registration` はまだ生存しており、チャネルへの登録も残ったまま。

        {
            let _guard_b = commit_boundary::ResponseBoundaryGuard::new();
            let write_txn = storage.db().begin_write().expect("begin_write");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
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
            commit_boundary::commit(write_txn).expect("guard_b's commit must succeed");

            // pending 世代（guard_b）は登録済みチャネルの世代（guard_a）とは
            // 異なるはずなので、送信されないこと。
            let sent = try_send_emergency_response();
            assert!(
                !sent,
                "must not send when the registered generation differs from the pending generation"
            );

            let mut buf = [0u8; 1];
            use std::io::Read as _;
            let n = client.read(&mut buf);
            assert!(
                matches!(n, Err(_) | Ok(0)),
                "no bytes must have been written on generation mismatch"
            );
        }
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

        let _registration = EmergencyResponseRegistration::register(
            vec![b'E', 0, 0, 0, 4],
            server_stream,
            Duration::from_secs(5),
        );

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
