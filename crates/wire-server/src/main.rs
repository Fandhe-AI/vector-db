//! wire-server: PostgreSQL wire プロトコル v3 互換の自作実装を持つバイナリ層。
//!
//! 責務境界: クライアント接続の受け付け・wire プロトコルのパース/応答整形を担い、
//! クエリの実処理は `engine` クレート（コアロジック層）へ委譲する（TASK-73 で
//! 簡易クエリプロトコルを `engine::core::EngineCore` へ接続した）。
//!
//! CLI: `wire-server --users <path> --db <path> [--bind <addr:port>]
//! [--planner-endpoint <host:port> --planner-model <name>]
//! [--embedder-hashing-dim <N>]`
//! （既定 bind: `127.0.0.1:5432`）。`--db` は必須（省略時は fail-closed で
//! 非 0 終了。匿名・揮発 DB の暗黙生成はしない。TASK-73・WIRE-1）。
//! `wire-server hash-password` サブコマンドはユーザーストア（`username:tenant_id:phc`）
//! に登録する 1 行を生成する補助コマンド（stdin からパスワードを読み、平文を
//! ログ・引数に残さない）。
//!
//! `--planner-endpoint`／`--planner-model`（TASK-117・PLAN-9）: 両方指定時のみ
//! `engine::query_planner::OllamaClient` を構築して `EngineCore::with_query_planner`
//! へ注入する（未指定が既定＝`CoreError::QueryPlannerUnavailable` で `USING PLAN`
//! を fail-closed 拒否する現行契約を維持。片方だけの指定は起動時エラー）。
//! `--embedder-hashing-dim`（同 TASK-117）は `engine::embedding::HashingEmbedder`
//! （決定的・ネットワーク不要な**検証用参照実装**であり意味的埋め込みではない。
//! 同モジュールドキュメント参照）を opt-in 注入する。いずれも wire 経由での
//! `USING PLAN` 受け入れ検証（PLAN-9 確定化）のための注入点であり、実運用の
//! 埋め込み/プランナー接続先は別途の構成を要する。
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
    let mut planner_endpoint: Option<String> = None;
    let mut planner_model: Option<String> = None;
    let mut embedder_hashing_dim: Option<String> = None;

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
            "--planner-endpoint" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --planner-endpoint requires a host:port argument");
                    return ExitCode::FAILURE;
                };
                planner_endpoint = Some(v.clone());
                i += 2;
            }
            "--planner-model" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --planner-model requires a name argument");
                    return ExitCode::FAILURE;
                };
                planner_model = Some(v.clone());
                i += 2;
            }
            "--embedder-hashing-dim" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("wire-server: --embedder-hashing-dim requires a numeric argument");
                    return ExitCode::FAILURE;
                };
                embedder_hashing_dim = Some(v.clone());
                i += 2;
            }
            other => {
                eprintln!("wire-server: unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    // TASK-117（PLAN-9）: `--planner-endpoint`／`--planner-model` は両方揃って
    // 初めて `OllamaClient` を構築できる契約（片方のみは操作ミスの検出漏れを防ぐ
    // ため fail-closed で起動を拒否する。未接続の既定＝`QueryPlannerUnavailable`
    // 拒否を静かに維持したまま片方だけ設定漏れした状態を作らせない）。
    let query_planner = match (planner_endpoint.as_deref(), planner_model.as_deref()) {
        (None, None) => None,
        (Some(_), None) => {
            eprintln!("wire-server: --planner-endpoint requires --planner-model to also be set");
            return ExitCode::FAILURE;
        }
        (None, Some(_)) => {
            eprintln!("wire-server: --planner-model requires --planner-endpoint to also be set");
            return ExitCode::FAILURE;
        }
        (Some(endpoint), Some(model)) => match build_query_planner(endpoint, model) {
            Ok(client) => Some(client),
            Err(e) => {
                eprintln!("wire-server: invalid --planner-endpoint/--planner-model: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let embedder = match embedder_hashing_dim.as_deref() {
        None => None,
        Some(raw_dim) => match build_hashing_embedder(raw_dim) {
            Ok(embedder) => Some(embedder),
            Err(e) => {
                eprintln!("wire-server: invalid --embedder-hashing-dim: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

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
    let mut core = match engine::core::EngineCore::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wire-server: failed to open database at {db_path:?}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // TASK-117（PLAN-9）: opt-in 注入。未指定（既定）では `query_planner`/
    // `embedder` とも未設定のままとなり、`USING PLAN` は従来どおり
    // `CoreError::QueryPlannerUnavailable`/`EmbedderUnavailable`
    // （wire 応答は `XX000`・固定の一般化メッセージ。`SqlSurfaceError::Internal`
    // 経由）で fail-closed 拒否される。
    if let Some(embedder) = embedder {
        core = core.with_embedder(embedder);
    }
    if let Some(query_planner) = query_planner {
        core = core.with_query_planner(query_planner);
    }
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

/// `--planner-endpoint <host:port>`／`--planner-model <name>` から
/// `engine::query_planner::OllamaClient` を構築する（TASK-117・PLAN-9）。
///
/// `host:port` は最後の `:` で分割する（IPv6 リテラルの角括弧表記は本 CLI では
/// 受理しない。検証用途の loopback 接続のみを想定するため、IPv4/ホスト名の
/// 単純な `host:port` 表記に限定して曖昧さを避ける）。ホストの loopback 検証
/// 自体は `OllamaConfig::with_host` が担う（IP リテラルは構築時点で非 loopback
/// を拒否。ホスト名は接続直前の名前解決結果検証へ委譲。同メソッドドキュメント
/// 参照）。モデル名は空文字・制御文字混入・過大な長さを CLI 引数の時点で
/// fail-closed に拒否する（プロンプトへ連結される値のため、明らかに不正な値を
/// 早期に弾く防御的措置。実際の SSRF 対策の主体は `OllamaConfig` 側）。
fn build_query_planner(
    endpoint: &str,
    model: &str,
) -> Result<Box<dyn engine::query_planner::LlmClient>, String> {
    let Some(colon_idx) = endpoint.rfind(':') else {
        return Err(format!("expected host:port, got {endpoint:?}"));
    };
    let (host, port_str) = endpoint.split_at(colon_idx);
    let port_str = &port_str[1..];
    if host.is_empty() {
        return Err(format!("expected host:port, got {endpoint:?}"));
    }
    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("invalid port in {endpoint:?}"))?;

    if model.is_empty() {
        return Err("model name must not be empty".to_string());
    }
    if model.len() > 256 {
        return Err("model name exceeds 256 bytes".to_string());
    }
    if model.contains(['\0', '\n', '\r']) {
        return Err("model name must not contain control characters".to_string());
    }

    let config = engine::query_planner::OllamaConfig::new(model)
        .with_host(host)
        .map_err(|e| format!("{e:?}"))?
        .with_port(port);
    Ok(Box::new(engine::query_planner::OllamaClient::new(config)))
}

/// `--embedder-hashing-dim <N>` から `engine::embedding::HashingEmbedder`
/// （検証用の決定的参照実装。意味的埋め込みではない。同モジュールドキュメント
/// 参照）を構築する（TASK-117・PLAN-9）。`u32` へのパース失敗・`HashingEmbedder::
/// new` の範囲外拒否をそのまま呼び出し元へ fail-closed で伝える。
fn build_hashing_embedder(raw_dim: &str) -> Result<Box<dyn engine::embedding::Embedder>, String> {
    let dim: u32 = raw_dim
        .parse()
        .map_err(|_| format!("invalid dimension {raw_dim:?}"))?;
    let embedder = engine::embedding::HashingEmbedder::new(dim).map_err(|e| format!("{e:?}"))?;
    Ok(Box::new(embedder))
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
    use super::*;

    // workspace の雛形が成立していること（wire-server から engine への path 依存が
    // リンクできること）を確認する smoke テスト。対象ビヘイビア ID なし。
    #[test]
    fn engine_is_linked() {
        assert_eq!(engine::ENGINE_NAME, "engine");
    }

    // TASK-117（PLAN-9）: `--planner-endpoint`／`--planner-model`／
    // `--embedder-hashing-dim` の引数パース単体テスト。wire 経由の実行契約
    // （fail-closed 応答の中身）は `tests/wire_using_plan.rs` が担う。
    //
    // `build_query_planner`/`build_hashing_embedder` の `Ok` 側（`Box<dyn
    // LlmClient>`/`Box<dyn Embedder>`）は `Debug` を実装しないため
    // `unwrap_err()` が使えない。`Result::err()` で `Option<String>` へ変換して
    // から展開する（`Err` の中身＝`String` のみを見る、両関数共通のヘルパー）。
    fn expect_err<T>(result: Result<T, String>) -> String {
        result.err().expect("expected an error")
    }

    #[test]
    fn build_query_planner_accepts_loopback_host_and_port() {
        build_query_planner("127.0.0.1:11434", "dummy-model").expect("valid endpoint/model");
    }

    #[test]
    fn build_query_planner_rejects_missing_colon() {
        let err = expect_err(build_query_planner("127.0.0.1", "dummy-model"));
        assert!(err.contains("host:port"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_empty_host() {
        let err = expect_err(build_query_planner(":11434", "dummy-model"));
        assert!(err.contains("host:port"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_non_numeric_port() {
        let err = expect_err(build_query_planner("127.0.0.1:not-a-port", "dummy-model"));
        assert!(err.contains("port"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_non_loopback_ip_host() {
        // `OllamaConfig::with_host` が非 loopback IP リテラルを構築時点で拒否する
        // 既存契約（TASK-72 未実装のうちは平文接続を loopback へ限定する）を、
        // CLI 経由でも維持できていることを確認する。
        let err = expect_err(build_query_planner("10.0.0.5:11434", "dummy-model"));
        assert!(!err.is_empty());
    }

    #[test]
    fn build_query_planner_rejects_empty_model_name() {
        let err = expect_err(build_query_planner("127.0.0.1:11434", ""));
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_model_name_with_control_characters() {
        let err = expect_err(build_query_planner("127.0.0.1:11434", "bad\nmodel"));
        assert!(err.contains("control"), "unexpected error: {err}");
    }

    #[test]
    fn build_query_planner_rejects_overlong_model_name() {
        let long_name = "a".repeat(257);
        let err = expect_err(build_query_planner("127.0.0.1:11434", &long_name));
        assert!(err.contains("256"), "unexpected error: {err}");
    }

    #[test]
    fn build_hashing_embedder_accepts_valid_dim() {
        build_hashing_embedder("16").expect("valid dim");
    }

    #[test]
    fn build_hashing_embedder_rejects_zero_dim() {
        expect_err(build_hashing_embedder("0"));
    }

    #[test]
    fn build_hashing_embedder_rejects_non_numeric_dim() {
        let err = expect_err(build_hashing_embedder("not-a-number"));
        assert!(err.contains("invalid dimension"), "unexpected error: {err}");
    }

    #[test]
    fn build_hashing_embedder_rejects_dim_exceeding_max() {
        // `MAX_EMBEDDER_DIM`（= `storage::MAX_EMBEDDING_DIM`）を超える値は
        // `HashingEmbedder::new` が `Result::Err` で拒否する契約
        // （`embedding.rs` モジュールドキュメント参照）。上限値そのものは
        // engine 側の実装既定であり本テストでは転記せず、`u32::MAX` という
        // どの上限設定でも確実に超過する値で契約を確認する。
        expect_err(build_hashing_embedder(&u32::MAX.to_string()));
    }
}
