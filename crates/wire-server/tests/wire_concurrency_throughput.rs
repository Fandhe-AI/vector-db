//! 接続処理モデル（Issue #482。判断記録: `docs/design/wire-connection-model.md`）
//! の同時接続数 N 別スループット手動計測ハーネス。
//!
//! `wire_limits.rs` の `wire6_production_max_connections_rejects_the_65th_connection`
//! が production 定数 `MAX_CONNECTIONS`（64）での上限ガードの正しさを固定するのに
//! 対し、本ファイルは「1 接続 1 スレッド」モデルの N（同時接続数）別スループットを
//! 実測するための手動専用ベンチである（`#[ignore]`。`tests/wire_bulk_response.rs` と
//! 同じ wire e2e パターンだが、既定検索エンジン〔`engine::search_engine::default_engine`〕
//! 経由で構築し `engine::parallel_search` のワーカー予算を経由する点が異なる。
//! `CpuScalarProvider` 単線経路ではなく、接続スレッド数 × ワーカー予算の相互作用を
//! 観測対象に含めるため）。
//!
//! `docs/design/benchmark-judgement-policy.md` の「複数規模点の同一プロセス内逐次
//! 比較は不可」規約に従い、1 プロセスの実行では 1 つの N のみを計測する
//! （`WIRE_CONCURRENCY_N` で指定）。運用者は N=1／8／64 を別プロセスとして
//! `make bench-wire-concurrency` から複数回実行し、min-of-N・median・per-run
//! 生データを手元で記録する（詳細は `docs/design/wire-connection-model.md` 参照）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use wire_server::limits::ConnectionLimiter;

use common::*;

/// `WIRE_CONCURRENCY_ROWS` 未指定時のフィクスチャ行数
/// （`docs/design/crossdb-bench.md` の `vector_knn` 相当規模）。
const DEFAULT_ROWS: u64 = 25_000;
/// `WIRE_CONCURRENCY_DIM` 未指定時の次元数。
const DEFAULT_DIM: usize = 128;
/// `WIRE_CONCURRENCY_ROUNDS` 未指定時の 1 クライアントあたり計測ラウンド数。
const DEFAULT_ROUNDS: usize = 200;
/// 統計対象に含めないウォームアップ往復回数（接続直後の cold path を除外する）。
const WARMUP_ROUNDS: usize = 5;

/// 環境変数を fail-closed にパースする（未設定・不正値は分かりやすい panic に
/// する。本ハーネスは `#[ignore]` の手動専用でありプロセス終了で構わない）。
fn env_usize(key: &str, default: Option<usize>) -> usize {
    match std::env::var(key) {
        Ok(raw) => raw
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{key} must be a positive integer, got {raw:?}")),
        Err(_) => default.unwrap_or_else(|| {
            panic!("{key} is required (e.g. WIRE_CONCURRENCY_N=8) for this manual benchmark")
        }),
    }
}

/// xorshift64* による決定的な擬似乱数生成器（依存追加なし。`dependency-policy.md`
/// 準拠）。フィクスチャのベクトル値・`body` 生成にのみ使い、暗号用途ではない。
struct Xorshift64Star(u64);

impl Xorshift64Star {
    fn new(seed: u64) -> Self {
        // 0 シードは xorshift の不動点になるため非ゼロへ補正する。
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32(&mut self) -> f32 {
        // 上位 24 bit を [0, 1) の f32 へ均等に写像する。
        ((self.next_u64() >> 40) as f32) / (1u32 << 24) as f32
    }
}

/// `docs`（`embedding VECTOR(dim)` + `body TEXT`）へ `rows` 件を決定的な擬似乱数で
/// 投入した `EngineCore` を、既定検索エンジン（`engine::search_engine::default_engine`。
/// `engine::parallel_search` のワーカー予算を経由する production 既定経路）で構築する。
fn new_default_engine_core(
    rows: u64,
    dim: usize,
) -> (Arc<EngineCore>, temp_db::CleanupGuard, Vec<f32>) {
    let path = temp_db::unique_db_path("wire-concurrency-throughput");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new(
                    "embedding",
                    ColumnType::Vector(u32::try_from(dim).expect("dim fits u32")),
                    false,
                ),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut rng = Xorshift64Star::new(0x517c_c1b7_2722_0a95);
    let mut query_vector = Vec::with_capacity(dim);
    for _ in 0..dim {
        query_vector.push(rng.next_f32());
    }

    for id in 1..=rows {
        let mut embedding = Vec::with_capacity(dim);
        for _ in 0..dim {
            embedding.push(rng.next_f32());
        }
        let body = format!("wire-concurrency-throughput-doc-{id}");
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(embedding), Value::Text(body)],
            &op_id,
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, engine::search_engine::default_engine());
    (Arc::new(core), guard, query_vector)
}

/// サーバースレッドを `wire_server::server::accept_loop_with_engine` 経由で起動する。
/// `common::spawn_server_with_engine` は接続上限を 16 に固定しているため、
/// production 定数 `MAX_CONNECTIONS`（64）まで N を上げる本ハーネス専用に、
/// 同じ組み立てをローカルで行う（他テストへの上限変更の波及を避ける。計画
/// ステップ 3 の申し送りどおり）。
fn spawn_server_with_max_connections(
    users_path: &std::path::Path,
    engine_core: Arc<EngineCore>,
) -> (std::net::SocketAddr, ConnectionLimiter) {
    let store =
        Arc::new(wire_server::auth::UserStore::load_from_file(users_path).expect("valid store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
    let limiter_for_server = limiter.clone();

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine_core,
            limiter_for_server,
            wire_server::limits::READ_TIMEOUT,
        );
    });

    (addr, limiter)
}

/// 全クライアントスレッド分の接続枠が解放されるまで待つ（Cursor Bugbot
/// 指摘対応: N 本のワーカースレッドが `stream` を drop した直後は、
/// accept ループ側の `ConnectionPermit` 解放〔TCP close の検出〕がまだ
/// 完了していないことがあり、その状態で新規接続を開くと accept ループが
/// まだ `MAX_CONNECTIONS` を維持していて `53300` 拒否になるレースがある。
/// ここでは実際に `limiter.active() == 0` を確認できるまでポーリングし、
/// タイムアウトした場合は明示的に panic させる（本ハーネスは `#[ignore]`
/// の手動専用でありプロセス終了で構わない）。
fn wait_for_all_permits_released(limiter: &ConnectionLimiter, timeout: std::time::Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if limiter.active() == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for all connection permits to be released (active={})",
            limiter.active()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// ベクトルを SQL リテラル（`'[v0,v1,...]'`）へ整形する。
fn format_vector_literal(v: &[f32]) -> String {
    let mut s = String::from("'[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push_str("]'");
    s
}

/// 1 回の `SELECT id FROM docs ORDER BY embedding <=> <literal> LIMIT 10` 往復を
/// 実行し、経過時間（マイクロ秒）を返す。
fn run_one_query(stream: &mut TcpStream, sql: &str) -> u128 {
    let start = Instant::now();
    send_simple_query(stream, sql);
    let _columns = read_row_description(stream);
    // LIMIT 10 ぶんの DataRow を読み切る（`crossdb-bench.md` の `vector_knn`
    // 相当の狭域取得）。
    for _ in 0..10 {
        let _ = read_data_row(stream);
    }
    let _tag = read_command_complete(stream);
    read_ready_for_query(stream);
    start.elapsed().as_micros()
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 同時接続数 N 別のスループット手動計測（Issue #482）。
///
/// `WIRE_CONCURRENCY_N`（必須。1〜`MAX_CONNECTIONS`）で同時接続数を指定する。
/// 1 プロセス = 1 規模点（`benchmark-judgement-policy.md` §5 準拠）。運用者は
/// N=1／8／64 をそれぞれ別プロセスとして複数回（推奨 5 回以上）実行し、
/// 出力される集計 QPS・p50・p95・min を手元で記録する
/// （`docs/design/wire-connection-model.md` の実測表参照）。
///
/// 併せて N=1 の同一プロセス内で「参照区間」（`SET`応答 1 往復。クエリ処理
/// そのものを含まない最小往復）を計測し、変更を含まない区間のノイズ帯の
/// 目安として出力する。
#[test]
#[ignore]
fn wire_concurrency_throughput_measurement() {
    let n = env_usize("WIRE_CONCURRENCY_N", None);
    assert!(
        (1..=wire_server::limits::MAX_CONNECTIONS).contains(&n),
        "WIRE_CONCURRENCY_N must be within 1..={}, got {n}",
        wire_server::limits::MAX_CONNECTIONS
    );
    let rows = env_usize("WIRE_CONCURRENCY_ROWS", Some(DEFAULT_ROWS as usize)) as u64;
    let dim = env_usize("WIRE_CONCURRENCY_DIM", Some(DEFAULT_DIM));
    let rounds = env_usize("WIRE_CONCURRENCY_ROUNDS", Some(DEFAULT_ROUNDS));

    let (core, _guard, query_vector) = new_default_engine_core(rows, dim);
    let users: Vec<(String, String, String)> = (0..n)
        .map(|i| {
            (
                format!("user{i}"),
                "tenant-a".to_string(),
                "correct-horse".to_string(),
            )
        })
        .collect();
    let user_refs: Vec<(&str, &str, &str)> = users
        .iter()
        .map(|(u, t, p)| (u.as_str(), t.as_str(), p.as_str()))
        .collect();
    let users_path = write_user_store_file(&user_refs);
    let (addr, limiter) = spawn_server_with_max_connections(&users_path, core);

    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> {} LIMIT 10",
        format_vector_literal(&query_vector)
    );

    // 全クライアントスレッドを揃って開始させ、接続確立の裾を計測区間から
    // 除く（`Barrier` は std のみ・依存追加なし）。ウォームアップ後にも
    // 第 2 の `Barrier` で再同期し、計測区間そのものの共通開始時刻を
    // `Instant` で揃える（codex-review P2 指摘対応: 各スレッドの往復時間
    // 合計の最大値では実時間にならず QPS を過大評価するため、共通区間の
    // 実壁時計〔開始 = 全スレッドが測定ラウンドへ入った時刻の最小値、
    // 終了 = 全スレッドが測定ラウンドを終えた時刻の最大値〕を分母にする）。
    let start_barrier = Arc::new(Barrier::new(n));
    let measure_barrier = Arc::new(Barrier::new(n));
    let total_queries = Arc::new(AtomicU64::new(0));
    let per_thread: Vec<(Vec<u128>, Instant, Instant)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let start_barrier = Arc::clone(&start_barrier);
                let measure_barrier = Arc::clone(&measure_barrier);
                let total_queries = Arc::clone(&total_queries);
                let sql = sql.clone();
                let username = users[i].0.clone();
                scope.spawn(move || {
                    let mut stream =
                        authenticate_to_ready_for_query(addr, &username, "correct-horse");
                    start_barrier.wait();
                    for _ in 0..WARMUP_ROUNDS {
                        run_one_query(&mut stream, &sql);
                    }
                    // ウォームアップ完了後に再同期してから計測区間の開始時刻を
                    // 取る。バリア解放直後の命令実行はスレッド間でごく僅かな
                    // ずれしか生まないため、各スレッドの `Instant::now()` を
                    // そのまま共通開始時刻の候補として扱える。
                    measure_barrier.wait();
                    let measure_start = Instant::now();
                    let mut samples = Vec::with_capacity(rounds);
                    for _ in 0..rounds {
                        samples.push(run_one_query(&mut stream, &sql));
                        total_queries.fetch_add(1, Ordering::Relaxed);
                    }
                    let measure_end = Instant::now();
                    (samples, measure_start, measure_end)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // 共通計測区間の実時間 = 最も遅く測定を始めたスレッドの開始時刻から
    // 最も遅く終えたスレッドの終了時刻まで。全スレッドの `total_queries` を
    // この一つの実時間で割ることで QPS の分母を実時間に一致させる。
    let wall_start_to_end_us: u128 = {
        let start = per_thread.iter().map(|(_, s, _)| *s).min();
        let end = per_thread.iter().map(|(_, _, e)| *e).max();
        match (start, end) {
            (Some(s), Some(e)) => e.saturating_duration_since(s).as_micros(),
            _ => 0,
        }
    };

    let mut all_samples: Vec<u128> = per_thread
        .into_iter()
        .flat_map(|(samples, _, _)| samples)
        .collect();
    all_samples.sort_unstable();
    let total = total_queries.load(Ordering::Relaxed);
    let qps = if wall_start_to_end_us > 0 {
        (total as f64) / (wall_start_to_end_us as f64 / 1_000_000.0)
    } else {
        0.0
    };

    // 参照区間の計測に入る前に、全クライアントスレッドが占有していた接続枠が
    // 解放済みであることを確認する（Cursor Bugbot 指摘対応。上記
    // `wait_for_all_permits_released` 参照）。N=MAX_CONNECTIONS 実行時は
    // ここで解放を待たずに新規接続を開くと accept ループがまだ枠を
    // `MAX_CONNECTIONS` 占有中とみなし `53300` 拒否になり得る。
    wait_for_all_permits_released(&limiter, Duration::from_secs(5));

    // 参照区間: N=1 の同一プロセス内での `SET`（クエリ処理を含まない最小往復）
    // を計測し、変更を含まない区間のノイズ帯の目安として出力する。
    let reference_us = {
        let mut stream = authenticate_to_ready_for_query(addr, &users[0].0, "correct-horse");
        let mut samples = Vec::with_capacity(20);
        for _ in 0..20 {
            let start = Instant::now();
            send_simple_query(&mut stream, "SET search_mode = 'recall'");
            let _ = read_command_complete(&mut stream);
            read_ready_for_query(&mut stream);
            samples.push(start.elapsed().as_micros());
        }
        samples.sort_unstable();
        samples
    };

    println!("=== wire_concurrency_throughput_measurement ===");
    println!(
        "WIRE_CONCURRENCY_N={n} rows={rows} dim={dim} rounds={rounds} (warmup={WARMUP_ROUNDS})"
    );
    println!("nproc={:?}", std::thread::available_parallelism());
    println!("total_queries={total} wall_us={wall_start_to_end_us} qps={qps:.1}");
    println!(
        "per_query_us: min={} p50={} p95={} max={}",
        all_samples.first().copied().unwrap_or(0),
        percentile(&all_samples, 0.50),
        percentile(&all_samples, 0.95),
        all_samples.last().copied().unwrap_or(0)
    );
    println!(
        "reference_band_us (SET round-trip): min={} p50={} max={}",
        reference_us.first().copied().unwrap_or(0),
        percentile(&reference_us, 0.50),
        reference_us.last().copied().unwrap_or(0)
    );
    println!(
        "note: shared/CI environment values are reference-only per docs/design/benchmark-judgement-policy.md"
    );
}
