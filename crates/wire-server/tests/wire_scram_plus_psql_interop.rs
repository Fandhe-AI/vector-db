//! libpq（psql）実クライアントでの SCRAM-SHA-256-PLUS 相互運用ゲート
//! （Issue #970・§3 ブロック C。手動専用・`#[ignore]`）。
//!
//! 本サーバーが受理する葉鍵は Ed25519 のみであり、libpq（OpenSSL バック
//! エンド）は署名アルゴリズムの単一ハッシュを決められない構成では
//! チャネルバインディングを解決できない可能性がある。`sslmode=require`・
//! `channel_binding={prefer,require,disable}` の 3 通りで実測し、
//! `docs/design/tls-channel-binding.md` の既定値決定に使う。
//!
//! CI には配線しない（psql 依存・`make ci` に含めない）。
//! `cargo test -p fandhe-vector-db-wire-server --test wire_scram_plus_psql_interop -- --ignored --nocapture`
//! で手動実行する。

#[path = "common/tls_client.rs"]
mod tls_client;

use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

const TEST_SCRAM_MOCK_KEY_SECRET: &[u8] = b"wire-scram-psql-interop-test-mock-key-secret!!";

fn write_scram_user_store_file(username: &str, password: &[u8]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wire-server-psql-interop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
    ));
    std::fs::create_dir(&dir).expect("create unique fixture dir");
    let path = dir.join("users.txt");
    let scram_salt = [7u8; wire_server::auth::scram::SALT_LEN];
    let verifier = wire_server::auth::scram::generate_verifier(
        password,
        &scram_salt,
        wire_server::auth::scram::SCRAM_ITERATIONS,
    )
    .expect("valid");
    let phc = wire_server::auth::argon2id::encode_phc(
        b"unused-in-scram-mode",
        b"0123456789abcdef",
        &wire_server::auth::argon2id::RECOMMENDED_PARAMS,
    )
    .expect("valid phc");
    let content = format!(
        "{username}:tenant-a:{phc}:{}\n",
        verifier.to_verifier_string()
    );
    std::fs::write(&path, &content).expect("write fixture");
    path
}

fn spawn_tls_scram_server(users_path: &std::path::Path) -> std::net::SocketAddr {
    let store = Arc::new(
        UserStore::load_from_file(users_path)
            .expect("valid user store")
            .require_scram(TEST_SCRAM_MOCK_KEY_SECRET)
            .expect("all records carry scram verifiers"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    let tls = tls_client::test_config();

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_tls(
            listener,
            store,
            None,
            Some(tls),
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// `channel_binding` 設定値ごとに psql の認証段階が成功するかを実測する
/// （SQL 実行そのものは engine 未接続のためエラーになってよい。判定は
/// 認証・チャネルバインディング関連のエラー文字列の有無で行う）。
fn run_psql(addr: std::net::SocketAddr, username: &str, password: &str, channel_binding: &str) {
    let conninfo = format!(
        "host=127.0.0.1 port={} user={username} dbname=irrelevant sslmode=require channel_binding={channel_binding}",
        addr.port()
    );
    let output = Command::new("psql")
        .arg(&conninfo)
        .arg("-c")
        .arg("SELECT 1")
        .env("PGPASSWORD", password)
        .output()
        .expect("failed to spawn psql (interop gate requires psql on PATH)");
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("[psql channel_binding={channel_binding}] stderr:\n{stderr}");
    let auth_failed = stderr.contains("authentication")
        || stderr.contains("channel binding")
        || stderr.contains("SCRAM")
        || stderr.contains("SSL")
        || stderr.contains("could not find digest");
    assert!(
        !auth_failed,
        "psql (channel_binding={channel_binding}) failed at the auth/TLS stage: {stderr}"
    );
}

#[test]
#[ignore]
fn wire_scram_plus_psql_interop_gate() {
    let username = "alice";
    let password = "correct horse battery staple";
    let users_path = write_scram_user_store_file(username, password.as_bytes());

    for channel_binding in ["disable", "prefer", "require"] {
        let addr = spawn_tls_scram_server(&users_path);
        // サーバー起動（`thread::spawn` の accept ループ）が listen 開始する
        // までの猶予。
        std::thread::sleep(Duration::from_millis(50));
        run_psql(addr, username, password, channel_binding);
    }
}
