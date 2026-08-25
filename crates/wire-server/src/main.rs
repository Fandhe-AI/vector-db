//! wire-server: PostgreSQL wire プロトコル v3 互換の自作実装を持つバイナリ層。
//!
//! 責務境界: クライアント接続の受け付け・wire プロトコルのパース/応答整形を担い、
//! クエリの実処理は `engine` クレート（コアロジック層）へ委譲する（本タスク時点では
//! 未実装であり、簡易クエリには「未実装」の ErrorResponse を返す。TASK-73 以降で接続）。
//!
//! CLI: `wire-server --users <path> [--bind <addr:port>]`（既定 bind: `127.0.0.1:5432`）。
//! `wire-server hash-password` サブコマンドはユーザーストア（`username:tenant_id:phc`）
//! に登録する 1 行を生成する補助コマンド（stdin からパスワードを読み、平文を
//! ログ・引数に残さない）。
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）。
//! 非ループバック bind の拒否ガードは TASK-70 の管轄であり、本タスクでは既定値
//! （`127.0.0.1`）による運用限定でリスクを抑える。

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use wire_server::auth::{self, UserStore};

const DEFAULT_BIND: &str = "127.0.0.1:5432";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("hash-password") {
        return run_hash_password();
    }

    run_server(&args)
}

/// `wire-server --users <path> [--bind <addr:port>]`。
fn run_server(args: &[String]) -> ExitCode {
    let mut users_path: Option<PathBuf> = None;
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

    let store = match UserStore::load_from_file(&users_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wire-server: failed to load user store: {e}");
            return ExitCode::FAILURE;
        }
    };
    let store = Arc::new(store);

    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("wire-server: failed to bind {bind_addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("wire-server: listening on {bind_addr}");

    accept_loop(listener, store);
    ExitCode::SUCCESS
}

/// 接続受け付けループ本体（`main` から分離し、結合テストが同じ挙動を再利用できる
/// ようにする）。1 接続 1 スレッドで処理し、各スレッドの panic は
/// `std::thread::spawn` の join ハンドルを無視することでプロセス全体へは波及させない
/// （他接続の継続稼働を優先する。接続数制限は TASK-69 の管轄）。
fn accept_loop(listener: TcpListener, store: Arc<UserStore>) {
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wire-server: accept error: {e}");
                continue;
            }
        };
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            if let Err(e) = wire_server::handshake::handle_connection(stream, &store) {
                eprintln!("wire-server: connection error: {e}");
            }
        });
    }
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
