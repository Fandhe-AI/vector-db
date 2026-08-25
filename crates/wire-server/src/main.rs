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
//! `--bind` は [`wire_server::server::bind_loopback`] により非ループバックアドレスを
//! 起動時に fail-closed で拒否したうえで、検証済みの数値アドレスへ直接 bind する
//! （TLS 未実装のうちは平文パスワードを非ループバックへ公開しない。ホスト名の
//! 再解決による TOCTOU も作らない。review 是正）。接続数上限・認証前 I/O タイムアウトは
//! [`wire_server::server::accept_loop`] が課す。本格的な接続管理・bind 方式の
//! 拡張は TASK-69・TASK-70 の管轄。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use wire_server::auth::{self, UserStore};
use wire_server::server;

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

    // `server::bind_loopback` は loopback 検証と bind を単一の入口にまとめており、
    // `bind_addr`（文字列）を別途 `TcpListener::bind` へ渡すことはしない
    // （検証時と bind 時で DNS 再解決が起きる TOCTOU を作らないため。review 指摘）。
    let listener = match server::bind_loopback(&bind_addr) {
        Ok(l) => l,
        Err(msg) => {
            eprintln!("wire-server: {msg}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("wire-server: listening on {bind_addr}");

    server::accept_loop(
        listener,
        store,
        server::MAX_CONCURRENT_CONNECTIONS,
        server::CONNECTION_IO_TIMEOUT,
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
