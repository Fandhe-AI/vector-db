//! `--search-engine` opt-in（Issue #656）をバイナリ子プロセスとして起動し、
//! CLI 引数の受理・拒否（fail-closed）を外形的に検証する結合テスト。
//!
//! `tests/wire7_bind_guard.rs`（TASK-70・WIRE-7）・
//! `tests/three_client_e2e.rs::spawn_wire_server` と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを外形的に確認する）。
//!
//! `EXPLAIN`（Issue #411 の `engine:`／`hnsw_params:` 行）はクエリプランナー
//! 注入（`--planner-endpoint`／`--planner-model`。TASK-117）が必須で、実
//! Ollama 前提の CLI からは検証できないため、R4 の wire 経由観測は
//! `tests/wire_search_engine_opt.rs`（in-process。決定的スタブ `LlmClient`）が
//! 担う。本ファイルは:
//!
//! - R1: 4 トークン（`default`／`hnsw`／`hnsw_f16`／`hnsw_i8`）いずれも
//!   `listening on` に到達すること
//! - R2: 未指定と `--search-engine default` の 2 プロセスで、同一 C1 クエリの
//!   受信バイト列（`RowDescription`〜`ReadyForQuery`）が完全一致すること
//!   （既定経路がコード上も無変更であることの実測面の裏付け）
//! - R3: 不正値・値欠落・重複指定は非 0 終了・stderr に `--search-engine` を
//!   含む説明が出ること
//!
//! を固定する。

#[path = "common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::authenticate_to_ready_for_query;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（ユーザーストア・DB ファイルの
/// 置き場）を確保し、`Drop` で確実に削除するガード（`wire7_bind_guard.rs::
/// TempUserStore` と同型。Issue #172 の一意性対策を踏襲）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-search-engine-cli-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        Self { dir }
    }

    fn users_path_str(&self) -> String {
        self.dir
            .join("users.txt")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    fn db_path_str(&self) -> String {
        self.dir
            .join("db.redb")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 空のユーザーストア（R1・R3 は認証まで到達する必要がないため）。
fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// `alice:tenant-a:<phc>` の 1 行を持つユーザーストア（R2 の認証・簡易クエリ
/// 発行に使う）。パスワードハッシュは `wire-server hash-password` サブ
/// コマンドを子プロセスとして呼び、平文をテストコード内に決め打ちしない
/// （`crates/wire-server/src/main.rs::run_hash_password` と同じ経路を通す）。
fn write_user_store_with_alice(path: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-password");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"pw-alice\n")
        .expect("write password to stdin");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success(), "hash-password must succeed");
    let phc = String::from_utf8(output.stdout)
        .expect("utf-8 phc")
        .trim()
        .to_string();
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write user store");
}

/// `docs(embedding VECTOR(2))` テーブルへ 1 行だけ投入した DB ファイルを
/// 用意する（R2 の C1 クエリが決定的な 1 行を返すようにするため）。呼び出し元
/// が子プロセスを起動する **前に** `Storage` を drop すること（redb は
/// ファイルロックを持つため、開いたまま子プロセスの `EngineCore::open` を
/// 呼ぶと衝突する）。
fn seed_single_row_db(db_path: &str) {
    let storage = Storage::open(db_path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0])],
        &OperationId::parse("seed-op").expect("valid operation_id"),
    )
    .expect("insert row");
    drop(storage);
}

/// 子プロセスの stderr を専用スレッドで読み、`listening on` を待つ
/// （`wire7_bind_guard.rs::loopback_bind_starts_listening` と同じ理由:
/// `BufReader::read_line` はデッドラインを持たないブロッキング呼び出しの
/// ため、`mpsc::Receiver::recv_timeout` で確実に打ち切れるようにする）。
fn wait_for_listening(child: &mut Child) -> bool {
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 || tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains("listening on") => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// R1: 4 トークンすべてで起動が拒否されず `listening on` に到達すること。
#[test]
fn all_four_tokens_start_listening() {
    for token in ["default", "hnsw", "hnsw_f16", "hnsw_i8"] {
        let fixture = TempFixtureDir::new(&format!("r1-{token}"));
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
        let db_path = fixture.db_path_str();

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
                "--search-engine",
                token,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let listening = wait_for_listening(&mut child);
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            listening,
            "token={token}: expected to reach listening state"
        );
    }
}

/// R3: 不正値・値欠落・重複指定はいずれも fail-closed（非 0 終了・
/// `--search-engine` を含む stderr）。
#[test]
fn invalid_or_missing_or_duplicate_search_engine_arg_is_rejected() {
    let fixture = TempFixtureDir::new("r3");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let cases: [&[&str]; 5] = [
        &["--search-engine", "bogus"],
        &["--search-engine", "HNSW"],
        // ベンチ env のトークン（`RecallEngine`）は CLI では受理しない
        // （`search_engine_opt.rs` モジュールドキュメント参照）。
        &["--search-engine", "brute_force"],
        // 直後の既知フラグ名をそのまま値として食い、閉じた語彙のいずれとも
        // 一致しないため拒否されるケース（`--search-engine` が次トークンを
        // 無条件で値とみなす仕様上、これも fail-closed で弾かれることの確認）。
        &["--search-engine", "--bind"],
        // 重複指定（D6）。
        &["--search-engine", "hnsw", "--search-engine", "hnsw_f16"],
    ];

    for extra_args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(extra_args)
            .output()
            .expect("spawn wire-server");

        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--search-engine"),
            "args={extra_args:?}: expected stderr to mention --search-engine, got: {stderr}"
        );
    }

    // 値欠落単体（末尾に `--search-engine` だけを置くケース）。
    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--search-engine",
        ])
        .output()
        .expect("spawn wire-server");
    assert!(!output.status.success(), "missing value must be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--search-engine"),
        "expected stderr to mention --search-engine, got: {stderr}"
    );
}

/// 認証後、簡易クエリ 1 文を送って `RowDescription`〜`ReadyForQuery` までの
/// 生バイト列をそのまま返す（バイト列比較のため wire フレームの構造には
/// 立ち入らず、受信済みバイト列を単純連結する）。
fn run_c1_query_and_collect_bytes(addr: std::net::SocketAddr) -> Vec<u8> {
    // 認証フローそのもの（`StartupMessage`〜認証後 `ReadyForQuery` までの
    // `BackendKeyData`／`ParameterStatus*` を含む可変長メッセージ列）は
    // `tests/common/mod.rs::authenticate_to_ready_for_query`（`wire1_simple_query.rs`
    // 等が使う既存ヘルパー）にそのまま委譲する。本テストの比較対象は
    // 「簡易クエリ 1 文への応答バイト列」に絞り、認証フローの実装詳細
    // （ParameterStatus の内容等）には立ち入らない。
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    // 簡易クエリ（'Q'）を送る。
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1";
    let mut q_body = Vec::new();
    q_body.extend_from_slice(sql.as_bytes());
    q_body.push(0);
    let mut q_msg = Vec::new();
    q_msg.push(b'Q');
    q_msg.extend_from_slice(&((q_body.len() + 4) as i32).to_be_bytes());
    q_msg.extend_from_slice(&q_body);
    stream.write_all(&q_msg).expect("send simple query");

    // RowDescription('T') → DataRow('D') → CommandComplete('C') →
    // ReadyForQuery('Z') の 4 フレームを、各フレームの長さプレフィックスに
    // 従ってそのまま読み切る（内容の意味解釈はせず、受信バイト列比較用に
    // 蓄積するだけ）。
    let mut collected = Vec::new();
    for _ in 0..4 {
        let mut frame_header = [0u8; 5];
        stream
            .read_exact(&mut frame_header)
            .expect("read frame header");
        let len = i32::from_be_bytes([
            frame_header[1],
            frame_header[2],
            frame_header[3],
            frame_header[4],
        ]);
        let body_len = (len as usize).saturating_sub(4);
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).expect("read frame body");
        collected.extend_from_slice(&frame_header);
        collected.extend_from_slice(&body);
    }
    collected
}

/// R2: 未指定と `--search-engine default` は同一クエリに対し完全に同一の
/// 受信バイト列を返す（既定経路がコード上もビット同一であることの実測面の
/// 裏付け。`main.rs::run_server` が `search_engine_kind == None` の場合のみ
/// `EngineCore::open` をそのまま呼ぶ設計の外形的な確認）。
#[test]
fn unset_and_default_token_produce_identical_wire_bytes() {
    let mut outputs: Vec<Vec<u8>> = Vec::new();

    for extra_args in [Vec::<&str>::new(), vec!["--search-engine", "default"]] {
        let fixture = TempFixtureDir::new("r2");
        let users_path = fixture.users_path_str();
        write_user_store_with_alice(&users_path);
        let db_path = fixture.db_path_str();
        // 子プロセス起動前に `Storage` を必ず drop する（redb のファイル
        // ロック回避。`seed_single_row_db` 内で drop 済み）。
        seed_single_row_db(&db_path);

        let mut args = vec![
            "--users".to_string(),
            users_path,
            "--db".to_string(),
            db_path,
            "--bind".to_string(),
            "127.0.0.1:0".to_string(),
        ];
        args.extend(extra_args.iter().map(|s| s.to_string()));

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        // `listening on 127.0.0.1:<port>` から実際の bind アドレスを取得する
        // （`--bind 127.0.0.1:0` の ephemeral port 割り当て結果。
        // `docs/design/three-client-e2e-harness.md` と同じ取得手順）。
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap_or(0);
                if n == 0 || tx.send(std::mem::take(&mut line)).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut addr: Option<std::net::SocketAddr> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(line) => {
                    if let Some(idx) = line.find("listening on ") {
                        let addr_str = line[idx + "listening on ".len()..].trim();
                        addr = addr_str.parse().ok();
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let addr =
            addr.unwrap_or_else(|| panic!("did not observe listening address, args={args:?}"));

        let bytes = run_c1_query_and_collect_bytes(addr);

        let _ = child.kill();
        let _ = child.wait();

        outputs.push(bytes);
    }

    assert_eq!(
        outputs[0], outputs[1],
        "unset --search-engine and --search-engine default must produce identical wire bytes"
    );
}
