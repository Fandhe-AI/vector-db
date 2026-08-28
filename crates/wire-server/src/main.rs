//! wire-server: PostgreSQL wire プロトコル v3 互換の自作実装を持つバイナリ層。
//!
//! 責務境界: クライアント接続の受け付け・wire プロトコルのパース/応答整形を担い、
//! クエリの実処理は `engine` クレート（コアロジック層）へ委譲する（TASK-73 で
//! 簡易クエリプロトコルを `engine::core::EngineCore` へ接続した）。
//!
//! CLI: `wire-server --users <path> --db <path> [--bind <addr:port>]`
//! （既定 bind: `127.0.0.1:5432`）。`--db` は必須（省略時は fail-closed で
//! 非 0 終了。匿名・揮発 DB の暗黙生成はしない。TASK-73・WIRE-1）。
//! `wire-server hash-password` サブコマンドはユーザーストア（`username:tenant_id:phc`）
//! に登録する 1 行を生成する補助コマンド（stdin からパスワードを読み、平文を
//! ログ・引数に残さない）。
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）・
//! TASK-69（対象ビヘイビア WIRE-5, WIRE-6）・TASK-70（対象ビヘイビア WIRE-7）・
//! TASK-99（対象ビヘイビア RECOVER-8。`engine::recovery::fail_fast::install` を
//! 起動時に結線し、panic を経路・スレッド問わずプロセス終了へ統一する）。
//! `--bind` は [`wire_server::bind_guard::GuardedBindAddrs::resolve`] により、TLS 未構成
//! （[`wire_server::bind_guard::TransportSecurity::Cleartext`]）の間は非ループバック
//! アドレスを起動時に fail-closed で拒否したうえで、検証済みの数値アドレスへ直接 bind
//! する（TLS 未実装のうちは平文パスワードを非ループバックへ公開しない。ホスト名の
//! 再解決による TOCTOU も作らない。TASK-67 review 是正・TASK-70 で移設）。同時接続数
//! 上限・認証前後の読み取りタイムアウトは [`wire_server::limits`] の契約値を
//! [`wire_server::server::accept_loop_with_limiter`] が適用する（TASK-69）。
//! TLS 導入（TASK-72・WIRE-9）時は [`wire_server::bind_guard::TransportSecurity`]
//! に variant を追加し、ここで渡す値を実行時の TLS 設定有無に応じて切り替える。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use wire_server::auth::{self, UserStore};
use wire_server::bind_guard::{GuardedBindAddrs, TransportSecurity};
use wire_server::limits;
use wire_server::server;

const DEFAULT_BIND: &str = "127.0.0.1:5432";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("hash-password") {
        return run_hash_password();
    }

    run_server(&args)
}

/// `wire-server --users <path> --db <path> [--bind <addr:port>]`。
fn run_server(args: &[String]) -> ExitCode {
    // TASK-97（対象ビヘイビア: RECOVER-6・ERR-1）: commit 成功境界を跨いだ panic の
    // 観測可能性側（緊急応答の送出）を有効化する。プロセス全体で 1 回だけ導入し
    // （`engine::recovery::panic_hook::install_panic_hook` は `Once` で冪等）、
    // 起動処理の他のどの失敗経路よりも前に呼ぶことで、後続の初期化中に commit を
    // 伴う処理が万一走っても保護対象から漏れないようにする。engine のライブラリ
    // 初期化（`EngineCore::open` 等）からは呼ばない契約（`panic_hook` モジュール
    // ドキュメント参照。engine 単体のテスト・他バイナリの panic 挙動を変えない）。
    engine::recovery::panic_hook::install_panic_hook();
    // TASK-99（対象ビヘイビア: RECOVER-8）: 内部エラーの 2 系統統一のうち panic 側
    // ―― 経路・スレッドを問わない fail-fast ―― を有効化する。`panic_hook` の
    // **直後**に呼ぶ契約（`engine::recovery::fail_fast` モジュールドキュメント
    // 「導入順序」参照）: `std::panic::set_hook` は 1 プロセスに 1 フックしか
    // 保持できないため、`fail_fast::install` は自分がフックへ差し替わる際に
    // 捕捉した直前のフック（＝ここまでに導入済みの `panic_hook`）を必ず先に
    // 呼んでから abort する。この順序を逆にする（`fail_fast` を先に呼ぶ）と
    // `panic_hook` が緊急応答を送る前段が失われ、TASK-97・RECOVER-6 の緊急応答が
    // 退行する。
    engine::recovery::fail_fast::install();

    let mut users_path: Option<PathBuf> = None;
    let mut db_path: Option<PathBuf> = None;
    let mut bind_addr = DEFAULT_BIND.to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--users" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --users requires a path argument");
                    return ExitCode::FAILURE;
                };
                users_path = Some(PathBuf::from(v));
                i += 2;
            }
            "--db" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --db requires a path argument");
                    return ExitCode::FAILURE;
                };
                db_path = Some(PathBuf::from(v));
                i += 2;
            }
            "--bind" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --bind requires an address argument");
                    return ExitCode::FAILURE;
                };
                bind_addr = v.clone();
                i += 2;
            }
            other => {
                eprintln!("wire-server: unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let Some(users_path) = users_path else {
        eprintln!("wire-server: --users <path> is required (fail-closed: no anonymous login)");
        return ExitCode::FAILURE;
    };
    let Some(db_path) = db_path else {
        eprintln!(
            "wire-server: --db <path> is required (fail-closed: no implicit anonymous/volatile database)"
        );
        return ExitCode::FAILURE;
    };

    // TLS（TASK-72・WIRE-9）は未実装のため常に `Cleartext` を渡す。bind の
    // loopback 検証をユーザーストア読込より前に行うことで、ユーザーストアの
    // 内容に関わらず bind 先が拒否対象であれば即座に終了できる（fail-closed を
    // 早期に確定させる）。
    let guarded = match GuardedBindAddrs::resolve(&bind_addr, TransportSecurity::Cleartext) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("wire-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    let store = match UserStore::load_from_file(&users_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wire-server: failed to load user store: {e}");
            return ExitCode::FAILURE;
        }
    };
    let store = Arc::new(store);

    // engine（永続化 + SQL 表層）を起動する。ユーザーストア読込に続けて bind 前に
    // 開くことで、DB を開けない状態のまま listen してしまう経路を避ける
    // （fail-closed。TASK-73・WIRE-1）。
    let core = match engine::core::EngineCore::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wire-server: failed to open database at {db_path:?}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let core = Arc::new(core);

    // `guarded.bind()` は検証済みの数値アドレスへ直接 bind し、`bind_addr`
    // （文字列）を別途 `TcpListener::bind` へ渡すことはしない（検証時と bind 時で
    // DNS 再解決が起きる TOCTOU を作らないため。TASK-67 review 指摘）。
    let listener = match guarded.bind() {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "wire-server: failed to bind {bind_addr} ({:?}): {e}",
                guarded.addrs()
            );
            return ExitCode::FAILURE;
        }
    };
    // 実際に bind されたアドレスを出す（`--bind 127.0.0.1:0` の ephemeral port
    // 割り当て結果を E2E テストハーネスがこの行から取得する前提。TASK-73）。
    match listener.local_addr() {
        Ok(addr) => eprintln!("wire-server: listening on {addr}"),
        Err(_) => eprintln!("wire-server: listening on {bind_addr}"),
    }

    server::accept_loop_with_engine(
        listener,
        store,
        core,
        limits::ConnectionLimiter::new(limits::MAX_CONNECTIONS),
        limits::READ_TIMEOUT,
    );
    ExitCode::SUCCESS
}

/// `hash-password` サブコマンド: stdin からパスワードを 1 行読み、新規 salt を
/// 生成して PHC 文字列を stdout へ出力する。パスワードを引数・ログに残さない。
fn run_hash_password() -> ExitCode {
    let mut password = String::new();
    if let Err(e) = std::io::stdin().read_line(&mut password) {
        eprintln!("wire-server: failed to read password from stdin: {e}");
        return ExitCode::FAILURE;
    }
    let password = password.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        eprintln!("wire-server: empty password is not allowed");
        return ExitCode::FAILURE;
    }

    let salt = match auth::generate_salt() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wire-server: failed to read salt from CSPRNG: {e}");
            return ExitCode::FAILURE;
        }
    };

    match auth::argon2id::encode_phc(password.as_bytes(), &salt, &auth::DEFAULT_PARAMS) {
        Ok(phc) => {
            println!("{phc}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wire-server: failed to compute password hash: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    // workspace の雛形が成立していること（wire-server から engine への path 依存が
    // リンクできること）を確認する smoke テスト。対象ビヘイビア ID なし。
    #[test]
    fn engine_is_linked() {
        assert_eq!(engine::ENGINE_NAME, "engine");
    }
}
