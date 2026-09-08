//! `hybrid_rrf` 6,178µs（`docs/design/crossdb-bench.md`。25,000 行・dim 128・
//! wire 経由・psycopg・p50）の engine 内 hybrid 経路／SQL 表層／wire の内訳を
//! 切り分ける実測入口（Issue #465。ポインタ: SEARCH-1・SEARCH-3・TASK-104・
//! TASK-158）。`knn_wire_profile_bench.rs`（Issue #463）と同構成だが、
//! `body` 列・`SparseIndex` を要する hybrid 固有のコーパスのため独立したベンチ
//! として新設する（既存ベンチのコーパス〔embedding のみ〕を変更すると Issue
//! #463 の基線比較が壊れるため）。
//!
//! # 計測段（tier）
//!
//! 単一プロセス内で、単一テナント・全行 Public のコーパス（`crates/engine/
//! benches/hybrid_profile_bench.rs` と同じ単純化方針。RLS 境界の検証は
//! `hybrid_cache.rs`・`plan_rls_boost.rs` 等の既存テストが別途担う）・200
//! クエリ（ベクトル＋本文語彙由来のクエリ語）を輪番して交互計測する。
//!
//! | 段 | 内容 | 対応する区分 |
//! | --- | --- | --- |
//! | T1p `hybrid_direct_cached_index` | 事前構築 `SparseIndex` を使った公開 `hybrid_search`（engine 内 hybrid 経路の実効値） | engine 内 hybrid |
//! | T2 `sql_surface_hot` | `EngineCore::execute_sql_in_session`（wire と同じ入口。crossdb 規範形 `SELECT id … hybrid_rrf … LIMIT 10`。**ラウンドごとに新規 spawn したスレッド上で計測する**。下記「測定スレッドの揃え方」参照） | SQL 表層 |
//! | T3 `wire_roundtrip` | in-process ループバックサーバーへの簡易クエリ 1 往復（サーバの接続スレッド上で実行） | wire e2e |
//!
//! 3 区分への帰属（min-of-R を用いる）: engine 内 hybrid = T1p、
//! SQL 表層 = T2 − T1p、wire = T3 − T2。
//!
//! crossdb（`scripts/crossdb_bench/self_db.py`）は別プロセスの release
//! `wire-server` バイナリへ psycopg（Python・簡易クエリ）で接続するのに対し、
//! 本ベンチは in-process ループバックのため、プロセス間・言語間のオーバー
//! ヘッドは対象外（`docs/design/knn-wire-stage-profile.md` と同じ限界）。
//!
//! # 測定スレッドの揃え方（Issue #634）
//!
//! T2 は fixture を投入した main スレッド上ではなく、T3 のサーバ接続スレッド
//! と同じく `std::thread::scope` でラウンドごとに新規 spawn したスレッド上で
//! 計測する。main スレッド計測では T2 が T3 を一貫して上回る逆転が生じ
//! `bucket(wire)` が「逆転・未確定（n/a）」になっていた（ハーネス側の計測
//! アーティファクト）。逆転の機構は未確定（推定: fixture 投入後の main
//! スレッド固有状態）であり、本ベンチは観測事実のみを根拠に測定スレッドを
//! 揃える。全ラウンド共通の常駐ワーカースレッド＋チャネル方式は複雑さに
//! 見合わないため不採用とした（T3 の接続スレッドが全ラウンドで同一という
//! 非対称は残るが、`run` の warmup 20 反復がスレッド固有のウォーム状態を
//! 吸収する）。詳細・再計測値は
//! `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #634 追記」節参照。
//!
//! # fail-closed 検証
//!
//! 出力前に、各ラウンドの全反復（warmup 含む）について T1p・T2・T3 が同一
//! クエリに対し完全一致した Top-k id 列を返すこと・行数が `TOP_K` であること
//! を検証する。すべて通過するまで測定値を一切 `println!` しない
//! （`knn_wire_profile_bench.rs` と同一契約）。
//!
//! # CI・出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線
//! しない。`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する。
//! `make bench-hybrid-wire-profile`（Makefile）から実行する。判定ロジック
//! 自体（時間非依存）は `harness::hybrid_wire`・`harness::knn_wire` にあり、
//! `tests/hybrid_wire_profile_accept.rs` で `make ci` 側から回帰検証する。
//! production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
//! 無変更。

#[allow(dead_code)]
mod harness;

#[path = "../tests/common/mod.rs"]
mod common;

use harness::env_report::EnvReport;
use harness::hybrid_wire::{
    generate_body_text, generate_query_text, hybrid_statement, HybridWireProjection,
};
use harness::knn_wire::{
    bucket_diff, classify_against_bands, diff_ratio_pct, median_of, min_of,
    refuse_under_github_actions, render_bucket_line, render_reference_band_line,
    render_tier_round_line, render_tier_summary_line, step_ratio_pct, KnnWireError,
};
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::vector_literal;

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine;
use engine::sparse::SparseIndex;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 128;
const NUM_DOCS: usize = 25_000;
const TOP_K: usize = 10;
const TABLE: &str = "docs";
const VECTOR_COLUMN: &str = "embedding";
const TEXT_COLUMN: &str = "body";
const TENANT_ID: &str = "hybrid-wire-tenant";
const SEED_BATCH_ROWS: usize = 5_000;
const QUERY_POOL: usize = 200;
/// SQL 表層側 pool_depth（`sql/exec.rs::DEFAULT_HYBRID_POOL_DEPTH` の実装既定値
/// を engine 側ベンチと同じ方式で複製。`crates/engine/benches/harness/
/// hybrid_profile.rs::SQL_DEFAULT_HYBRID_POOL_DEPTH` と同値）。
const POOL_DEPTH: usize = 200;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("hybrid_wire_profile_bench: {msg}");
    std::process::exit(1);
}

fn sorted_ids(mut ids: Vec<u64>) -> Vec<u64> {
    ids.sort_unstable();
    ids
}

fn main() {
    // `harness::knn_wire::refuse_under_github_actions` の Display は
    // "knn_wire_profile_bench" 名を固定で埋め込むため、本ベンチ固有の名前で
    // 上書きする（BENCH_HYBRID_WIRE_ROUNDS の扱いと同じ理由）。
    if refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()).is_err() {
        fail_closed(
            "hybrid_wire_profile_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is \
             set); this bench is not wired into any workflow and must be run locally via \
             `make bench-hybrid-wire-profile`",
        );
    }
    let rounds_raw = std::env::var("BENCH_HYBRID_WIRE_ROUNDS").ok();
    let rounds = match harness::knn_wire::parse_rounds(rounds_raw.as_deref()) {
        Ok(v) => v,
        Err(KnnWireError::InvalidRounds(reason)) => {
            fail_closed(format!("invalid BENCH_HYBRID_WIRE_ROUNDS: {reason}"))
        }
        Err(e) => fail_closed(format!("BENCH_HYBRID_WIRE_ROUNDS validation failed: {e}")),
    };
    let dedicated_env = std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "hybrid_wire_profile_bench: rows={NUM_DOCS} dim={DIM} top_k={TOP_K} rounds={rounds} \
         dedicated_env={dedicated_env}"
    );
    if !dedicated_env {
        println!(
            "hybrid_wire_profile_bench: BENCH_DEDICATED_ENV 未設定のため共有環境の参考値として \
             扱う（docs/design/benchmark-judgement-policy.md §5）。採否根拠にしない。"
        );
    }

    // --- データ投入（単一テナント・全行 Public。hybrid_profile_bench.rs と
    // 同じ単純化方針）---
    let path = unique_db_path("issue465-hybrid-wire-profile");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new(VECTOR_COLUMN, ColumnType::Vector(DIM as u32), false),
            ColumnDef::new(TEXT_COLUMN, ColumnType::Text, false),
        ],
    );
    storage
        .create_table(&schema)
        .expect("create table for bench seeding");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant id");
    let mut rng = DeterministicRng::new(1);
    let mut bodies: Vec<String> = Vec::with_capacity(NUM_DOCS);
    let mut next_id: usize = 0;
    while next_id < NUM_DOCS {
        let batch_len = SEED_BATCH_ROWS.min(NUM_DOCS - next_id);
        let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
        let mut batch_bodies: Vec<String> = Vec::with_capacity(batch_len);
        for _ in 0..batch_len {
            batch_vectors.push(rng.next_vector(DIM));
            batch_bodies.push(generate_body_text(&mut rng));
        }
        let mut metadata_batch: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
        for body in &batch_bodies {
            let encoded = encode_scalar_columns(&schema, &[Value::Null, Value::Text(body.clone())])
                .expect("encode scalar columns for bench seeding");
            metadata_batch.push(encoded);
        }
        let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
            .map(|i| {
                let id = (next_id + i) as u64;
                (
                    id,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility: Visibility::Public,
                        embedding: &batch_vectors[i],
                        metadata: &metadata_batch[i],
                    },
                )
            })
            .collect();
        let op_id =
            OperationId::parse(&format!("hybrid-wire-seed-{next_id}")).expect("valid operation_id");
        engine::tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id)
            .expect("seed batch insert");
        bodies.extend(batch_bodies);
        next_id += batch_len;
    }
    if bodies.len() != NUM_DOCS {
        fail_closed(format!(
            "seeded row count mismatch: expected {NUM_DOCS}, got {}",
            bodies.len()
        ));
    }

    // 事前構築 SparseIndex（T1p が使う「キャッシュヒット相当」の疎索引）。
    let doc_refs: Vec<(u64, &str)> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| (i as u64, b.as_str()))
        .collect();
    let sparse_index = SparseIndex::build(&doc_refs)
        .unwrap_or_else(|e| fail_closed(format!("SparseIndex::build failed: {e}")));
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, POOL_DEPTH)
        .unwrap_or_else(|e| fail_closed(format!("RrfConfig::new failed: {e:?}")));

    // 200 クエリ（ベクトル＋本文語彙由来のクエリ語）を事前構築する。
    let vectors: Vec<Vec<f32>> = (0..QUERY_POOL).map(|_| rng.next_vector(DIM)).collect();
    let query_texts: Vec<String> = (0..QUERY_POOL)
        .map(|_| generate_query_text(&mut rng))
        .collect();
    let sqls: Vec<String> = vectors
        .iter()
        .zip(&query_texts)
        .map(|(v, text)| {
            let literal = vector_literal(v).expect("finite query vector");
            hybrid_statement(
                TABLE,
                VECTOR_COLUMN,
                TEXT_COLUMN,
                &literal,
                text,
                TOP_K,
                HybridWireProjection::Id,
            )
            .expect("well-formed hybrid statement from validated identifiers")
        })
        .collect();

    let ids: Vec<u64> = (0..NUM_DOCS as u64).collect();

    // T1p 用の可視 VectorArena（`storage` を `EngineCore::from_storage` へ move
    // する前に構築する。`knn_wire_profile_bench.rs` と同一パターン）。
    let arena =
        engine::arena::VectorArena::build_filtered(&storage, TABLE, |tenant, visibility| {
            ctx.is_visible(tenant, visibility)
        })
        .expect("arena build must succeed for well-formed synthetic corpus");
    if arena.len() != NUM_DOCS {
        fail_closed(format!(
            "arena row count mismatch: expected {NUM_DOCS} visible rows, got {}",
            arena.len()
        ));
    }

    let core = Arc::new(EngineCore::from_storage(
        storage,
        search_engine::default_engine(),
    ));

    let users_path = common::write_user_store_file(&[("bench", TENANT_ID, "bench")]);
    let addr = common::spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut wire_stream = common::authenticate_to_ready_for_query(addr, "bench", "bench");
    wire_stream
        .set_nodelay(true)
        .expect("set TCP_NODELAY on client socket");

    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    let parallel_provider = ParallelSearchProvider;

    // --- ウォームアップ（SqlArenaCache／SparseIndexCache をヒット状態にする）---
    let mut warm_session = SessionState::default();
    for i in 0..config.warmup_iterations() as usize {
        let sql = &sqls[i % QUERY_POOL];
        match core
            .execute_sql_in_session(&ctx, &mut warm_session, sql)
            .expect("warmup sql_surface query must succeed")
        {
            SqlOutcome::Query(_) => {}
            other => fail_closed(format!("unexpected warmup outcome: {other:?}")),
        }
    }
    for i in 0..config.warmup_iterations() as usize {
        let sql = &sqls[i % QUERY_POOL];
        common::send_simple_query(&mut wire_stream, sql);
        let _ = common::read_row_description(&mut wire_stream);
        for _ in 0..TOP_K {
            let _ = common::read_data_row(&mut wire_stream);
        }
        let _ = common::read_command_complete(&mut wire_stream);
        common::read_ready_for_query(&mut wire_stream);
    }

    let mut t1p_medians = Vec::with_capacity(rounds as usize);
    let mut t2_medians = Vec::with_capacity(rounds as usize);
    let mut t3_medians = Vec::with_capacity(rounds as usize);
    let mut round_lines: Vec<String> = Vec::new();

    let iterations_per_stage =
        config.warmup_iterations() as usize + config.measured_iterations() as usize;
    // ラウンド間の環境ノイズ帯判定（T1p の中央値ばらつき）にクエリ内容差が
    // 混入しないよう、全ラウンドで同一のクエリ部分集合（先頭 iterations_per_stage 件）
    // を測定する（codex-review P2 指摘・PR #556）。
    let round_start: usize = 0;

    for round in 1..=rounds {
        // T1p: engine 内 hybrid 経路（直接 API・事前構築 SparseIndex）。
        let mut t1p_cursor = round_start;
        let mut t1p_ids_all: Vec<Vec<u64>> = Vec::with_capacity(iterations_per_stage);
        let t1p = run(&config, || {
            let idx = t1p_cursor % QUERY_POOL;
            t1p_cursor += 1;
            let input = SearchInput {
                ids: &ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &vectors[idx],
                k: TOP_K,
            };
            let hits = hybrid_search(
                &parallel_provider,
                input,
                &sparse_index,
                &query_texts[idx],
                TOP_K,
                &cfg,
            )
            .expect("hybrid_search must succeed for well-formed synthetic input");
            t1p_ids_all.push(sorted_ids(hits.iter().map(|h| h.id).collect()));
            black_box(hits)
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "hybrid_direct_cached_index",
            round,
            t1p.summary.median,
            harness::accept::p95_from_samples(&t1p.samples).expect("non-empty sample set"),
        ));
        t1p_medians.push(t1p.summary.median);

        // T2: SQL 表層（wire と同じ入口。crossdb 規範形 SELECT id）。
        //
        // fixture を投入した main スレッドではなく、T3 のサーバ接続スレッドと
        // 同じく `std::thread::scope` でラウンドごとに新規 spawn したスレッド
        // 上で計測する（Issue #634）。main スレッド計測では T2 が T3 を
        // 一貫して上回る逆転が生じ `bucket(wire)` が n/a になっていた
        // （原因の機構は未確定〔推定: fixture 投入後の main スレッド固有状態〕
        // であり、本ベンチは観測事実のみを根拠に測定スレッドを揃える。全
        // ラウンド共通の常駐ワーカースレッド＋チャネル方式は複雑さに見合わ
        // ないため不採用）。`core`（`Arc<EngineCore>`）・`ctx`・`sqls` は
        // 借用のみで `Send + Sync`。`hot_session`・`t2_cursor`・
        // `t2_ids_all` はスレッド内で生成し戻り値で持ち帰る。
        let (t2, t2_ids_all): (harness::protocol::Measurement, Vec<Vec<u64>>) =
            std::thread::scope(|scope| {
                let handle = std::thread::Builder::new()
                    .name("sql_surface_hot".to_string())
                    .spawn_scoped(scope, || {
                        let mut t2_cursor = round_start;
                        let mut t2_ids_all: Vec<Vec<u64>> =
                            Vec::with_capacity(iterations_per_stage);
                        let mut hot_session = SessionState::default();
                        let t2 = run(&config, || {
                            let idx = t2_cursor % QUERY_POOL;
                            t2_cursor += 1;
                            let sql = &sqls[idx];
                            let outcome = core
                                .execute_sql_in_session(&ctx, &mut hot_session, sql)
                                .expect(
                                    "sql_surface_hot query must succeed for well-formed synthetic \
                                     input",
                                );
                            match outcome {
                                SqlOutcome::Query(result) => {
                                    t2_ids_all.push(sorted_ids(
                                        result.rows.iter().map(|r| r.id).collect(),
                                    ));
                                    black_box(result)
                                }
                                other => fail_closed(format!(
                                    "unexpected sql_surface_hot outcome: {other:?}"
                                )),
                            }
                        })
                        .expect("measurement must satisfy protocol minimums");
                        (t2, t2_ids_all)
                    });
                match handle {
                    Ok(h) => h
                        .join()
                        .unwrap_or_else(|_| fail_closed("sql_surface_hot thread panicked")),
                    Err(e) => fail_closed(format!("spawn sql_surface_hot thread: {e}")),
                }
            });
        round_lines.push(render_tier_round_line(
            "sql_surface_hot",
            round,
            t2.summary.median,
            harness::accept::p95_from_samples(&t2.samples).expect("non-empty sample set"),
        ));
        t2_medians.push(t2.summary.median);

        // T3: wire e2e（in-process ループバックサーバーへの簡易クエリ往復）。
        let mut t3_cursor = round_start;
        let mut t3_ids_all: Vec<Vec<u64>> = Vec::with_capacity(iterations_per_stage);
        let t3 = run(&config, || {
            let idx = t3_cursor % QUERY_POOL;
            t3_cursor += 1;
            let sql = &sqls[idx];
            let start = Instant::now();
            common::send_simple_query(&mut wire_stream, sql);
            let cols = common::read_row_description(&mut wire_stream);
            if cols.is_empty() {
                fail_closed("wire_roundtrip: RowDescription reported zero columns");
            }
            let mut row_ids = Vec::with_capacity(TOP_K);
            for _ in 0..TOP_K {
                let row = common::read_data_row(&mut wire_stream);
                let id_text = row
                    .first()
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| fail_closed("wire_roundtrip: id cell was NULL"));
                let id: u64 = id_text.parse().unwrap_or_else(|_| {
                    fail_closed(format!(
                        "wire_roundtrip: id cell not a valid u64: {id_text:?}"
                    ))
                });
                row_ids.push(id);
            }
            let _ = common::read_command_complete(&mut wire_stream);
            common::read_ready_for_query(&mut wire_stream);
            t3_ids_all.push(sorted_ids(row_ids));
            let _ = start.elapsed();
        })
        .expect("measurement must satisfy protocol minimums");
        round_lines.push(render_tier_round_line(
            "wire_roundtrip",
            round,
            t3.summary.median,
            harness::accept::p95_from_samples(&t3.samples).expect("non-empty sample set"),
        ));
        t3_medians.push(t3.summary.median);

        // --- fail-closed 整合性検証（本ラウンドの全反復）---
        if t1p_ids_all.len() != iterations_per_stage
            || t2_ids_all.len() != iterations_per_stage
            || t3_ids_all.len() != iterations_per_stage
        {
            fail_closed(format!(
                "iteration count mismatch: expected {iterations_per_stage} per stage, got \
                 hybrid_direct_cached_index={} sql_surface_hot={} wire_roundtrip={}",
                t1p_ids_all.len(),
                t2_ids_all.len(),
                t3_ids_all.len()
            ));
        }
        for iter_idx in 0..iterations_per_stage {
            let query_idx = (round_start + iter_idx) % QUERY_POOL;
            let t1p_ids = &t1p_ids_all[iter_idx];
            let t2_ids = &t2_ids_all[iter_idx];
            let t3_ids = &t3_ids_all[iter_idx];
            for (label, row_ids) in [
                ("hybrid_direct_cached_index", t1p_ids),
                ("sql_surface_hot", t2_ids),
                ("wire_roundtrip", t3_ids),
            ] {
                if row_ids.len() != TOP_K {
                    fail_closed(format!(
                        "{label}: expected {TOP_K} result rows, got {} (idx={query_idx})",
                        row_ids.len()
                    ));
                }
                for id in row_ids {
                    if *id as usize >= NUM_DOCS {
                        fail_closed(format!(
                            "{label}: returned out-of-range id {id} (idx={query_idx})"
                        ));
                    }
                }
            }
            if t1p_ids != t2_ids {
                fail_closed(format!(
                    "id set mismatch for identical query (idx={query_idx}): \
                     hybrid_direct_cached_index={t1p_ids:?} sql_surface_hot={t2_ids:?}"
                ));
            }
            if t2_ids != t3_ids {
                fail_closed(format!(
                    "id set mismatch for identical query (idx={query_idx}): \
                     sql_surface_hot={t2_ids:?} wire_roundtrip={t3_ids:?}"
                ));
            }
        }
    }

    // --- 集約統計（min-of-R/median-of-R） ---
    let summarize = |label: &str, medians: &[Duration]| -> (Duration, Duration) {
        let min = min_of(medians).unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
        let median = median_of(medians).unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
        (min, median)
    };
    let (t1p_min, t1p_med) = summarize("hybrid_direct_cached_index", &t1p_medians);
    let (t2_min, t2_med) = summarize("sql_surface_hot", &t2_medians);
    let (t3_min, t3_med) = summarize("wire_roundtrip", &t3_medians);

    // 参照区間帯は T1p（engine 内 hybrid・スレッドスケジューリングに依存する
    // ため厳密には B0s ほど厳格な参照ではないが、本ベンチは T1p 自体を分解する
    // 段を持たないため T1p の複数ラウンド中央値の振れをそのまま参照帯として
    // 使う。共有環境の参考値であることは dedicated_env で明示する）。
    let reference_band_pct = harness::knn_wire::reference_band(&t1p_medians)
        .map(|b| b * 100.0)
        .unwrap_or_else(|e| fail_closed(format!("reference_band: {e}")));

    // --- ここまで全検証通過。以降で初めて測定値を出力する ---
    for line in &round_lines {
        println!("{line}");
    }
    println!(
        "{}",
        render_tier_summary_line("hybrid_direct_cached_index", t1p_min, t1p_med)
    );
    println!(
        "{}",
        render_tier_summary_line("sql_surface_hot", t2_min, t2_med)
    );
    println!(
        "{}",
        render_tier_summary_line("wire_roundtrip", t3_min, t3_med)
    );
    println!("{}", render_reference_band_line(reference_band_pct));

    let render_bucket = |label: &str, from: Duration, to: Duration| match bucket_diff(from, to) {
        Some(diff) => {
            let ratio = diff_ratio_pct(diff, t3_min)
                .unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
            let band = if from.is_zero() {
                None
            } else {
                let step_ratio = step_ratio_pct(from, to)
                    .unwrap_or_else(|e| fail_closed(format!("{label}: {e}")));
                Some(classify_against_bands(step_ratio, reference_band_pct))
            };
            println!(
                "{}",
                render_bucket_line(label, Some(diff), Some(ratio), band)
            );
        }
        None => println!("{}", render_bucket_line(label, None, None, None)),
    };

    println!("--- hybrid_rrf breakdown (min-of-{rounds}, median): engine/sql/wire ---");
    render_bucket("engine_hybrid", Duration::ZERO, t1p_min);
    render_bucket("sql_surface", t1p_min, t2_min);
    render_bucket("wire", t2_min, t3_min);

    let _ = std::io::Write::write_all(&mut wire_stream, &[b'X', 0, 0, 0, 4]);
}
