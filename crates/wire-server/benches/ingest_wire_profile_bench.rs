//! 単文 `INSERT` の wire 往復内訳を切り分ける実測入口（Issue #484。親 Issue
//! #483。ポインタ: TASK-82・SQL-10・TASK-92/93/101・RECOVER-1/2/5/6/10・
//! TASK-122・INDEX-4）。
//!
//! `docs/design/crossdb-bench.md` の `ingest_single_stmt`（25,000 行・dim 128・
//! wire 経由・psycopg）が通る経路——wire 簡易クエリ → `EngineCore::
//! execute_sql_in_session` → `execute_insert_sql` → `tenant::
//! insert_typed_row_unchecked`（1 文 1 write txn）——のうち、engine 内部段は
//! `crates/engine/benches/ingest_profile_bench.rs`（`BENCH_INGEST_PROFILE_MODE=
//! single`）が P0/E0/S0/I1〜I8 として実測済み。本ベンチは同一プロセス内の
//! in-process ループバックサーバーで wire 往復そのもの（S0 との差分）を
//! 切り分ける。
//!
//! # 計測段（tier）
//!
//! | tier | 内容 |
//! | --- | --- |
//! | W0 `wire_roundtrip` | `common::send_simple_query` → `read_command_complete`
//!   （`"INSERT 0 1"` を検証） → `read_ready_for_query` |
//! | S0 `sql_surface` | 同一 `Arc<EngineCore>` へ `execute_sql_in_session`
//!   （wire と同一入口。SQL 文形は W0 と同一） |
//!
//! 帰属: wire ＝ W0 − S0（ラウンド中央値の min-of-R どうし。`harness::
//! knn_wire` の統計・帯判定関数を再利用する）。
//!
//! 各ラウンドは W0 → S0 の順で交互実行し（`docs/design/
//! benchmark-judgement-policy.md` §3 の交互実行方針）、`BENCH_INGEST_WIRE_ROWS`
//! を `BENCH_INGEST_WIRE_ROUNDS` で均等分割した文数を 1 ラウンドとする
//! （先頭 20 文を warmup として統計から除外しつつ投入自体は行う。
//! `harness::protocol::MeasurementConfig` の下限検証を利用）。W0・S0 で id・
//! `operation_id` の名前空間を分離し（W0: `10,000,000+n`・S0: `20,000,000+n`）、
//! 最終行数は 2 × rows になる。
//!
//! # fail-closed 検証
//!
//! 出力前に以下をすべて検証する: W0 の全文が `CommandComplete("INSERT 0 1")`
//! を返すこと、S0 の全文が `SqlOutcome::Insert(rows_affected == 1)` を返すこと
//! （いずれも計測ループ内で都度検証し、不一致は即座に fail-closed）、計測後に
//! `EngineCore::operation_recorded`（pub・TASK-93）で W0・S0 双方のサンプル
//! `operation_id` が `LedgerLookup::Recorded` であること。すべて通過するまで
//! 測定値を一切 `println!` しない。
//!
//! accept 用に生 `redb::Database` の再オープンによる行数照合は行わない
//! （accept ループスレッドが `Arc<EngineCore>` を保持し続けるため、同一プロセス
//! 内では書き込み可能ハンドルの二重オープンができない。`operation_recorded`
//! 経由の照合で代替する）。
//!
//! # CI・出力ポリシー
//!
//! spec 由来の閾値を持たない情報提供専用のため `.github/workflows/*` へは配線
//! しない。`GITHUB_ACTIONS` 環境下では起動直後に fail-closed で拒否する。
//! `make bench-ingest-wire-profile`（Makefile）から実行する。判定ロジック自体
//! （時間非依存）は `harness::ingest_wire` にあり、
//! `tests/ingest_wire_profile_accept.rs` で `make ci` 側から回帰検証する。
//! production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
//! 無変更。

#[allow(dead_code)]
mod harness;

#[path = "../tests/common/mod.rs"]
mod common;

use harness::accept::p95_from_samples;
use harness::env_report::EnvReport;
use harness::ingest_wire::{
    parse_rounds, parse_rows, refuse_under_github_actions, rows_per_round, rows_per_sec,
};
use harness::knn_wire::{
    bucket_diff, classify_against_bands, diff_ratio_pct, median_of, min_of, reference_band,
    render_bucket_line, render_tier_round_line, render_tier_summary_line, step_ratio_pct,
};
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::vector_literal;

use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::OperationId;
use engine::search_engine;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 128;
const TENANT: &str = "tenant-a";
const TABLE: &str = "docs";
const WIRE_ID_BASE: u64 = 10_000_000;
const SQL_ID_BASE: u64 = 20_000_000;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("ingest_wire_profile_bench: {msg}");
    std::process::exit(1);
}

/// 文番号 `n`（tier 内 0 起点）から SQL 文を決定的に組み立てる。`id`・
/// `operation_id` の名前空間は呼び出し元（tier ごとの base）が分離する。
/// `body`・`literal` は決定的な数値・固定語彙からのみ構成し、外部・untrusted
/// 入力を連結しない（coding-rust.md「SQL 文字列の組み立てに未検証入力を
/// 連結しない」）。
fn op_label(op_prefix: &str, n: u64) -> String {
    format!("{op_prefix}-{n}")
}

fn make_stmt_sql(id_base: u64, n: u64, dim: usize, op_prefix: &str) -> (String, String) {
    let mut rng = DeterministicRng::new(1u64.wrapping_add(n));
    let embedding = rng.next_vector(dim);
    let literal = vector_literal(&embedding).expect("finite embedding for wire ingest bench");
    let id = id_base + n;
    let op_id = op_label(op_prefix, n);
    let sql = format!(
        "INSERT INTO docs (id, embedding, body) VALUES ({id}, '{literal}', 'ingest wire bench row {n}') USING OPERATION_ID '{op_id}'"
    );
    (sql, op_id)
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    let rows_raw = std::env::var("BENCH_INGEST_WIRE_ROWS").ok();
    let rows = match parse_rows(rows_raw.as_deref()) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let rounds_raw = std::env::var("BENCH_INGEST_WIRE_ROUNDS").ok();
    let rounds = match parse_rounds(rounds_raw.as_deref()) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    let per_round = match rows_per_round(rows, rounds) {
        Ok(v) => v,
        Err(e) => fail_closed(e),
    };
    if per_round < 40 {
        // `MeasurementConfig` の下限（warmup ≥ 20・measured ≥ 20）を満たすため、
        // 1 ラウンドあたり最低 40 文を要求する（黙って warmup/measured を縮めない）。
        fail_closed(format!(
            "rows/rounds = {per_round} statements per round is below the protocol minimum 40 (increase BENCH_INGEST_WIRE_ROWS or decrease BENCH_INGEST_WIRE_ROUNDS)"
        ));
    }
    let dedicated_env = std::env::var_os("BENCH_DEDICATED_ENV").is_some();

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "ingest_wire_profile_bench: rows={rows} rounds={rounds} per_round={per_round} dim={DIM} dedicated_env={dedicated_env}"
    );
    if !dedicated_env {
        println!(
            "ingest_wire_profile_bench: BENCH_DEDICATED_ENV not set — treat results as a shared-environment reference value only (docs/design/benchmark-judgement-policy.md)"
        );
    }

    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    let db_path = unique_db_path("issue484-ingest-wire-profile");
    let _guard = CleanupGuard(db_path.clone());
    let storage = Storage::open(&db_path).expect("open storage");
    storage.create_table(&schema).expect("create table");

    let core = Arc::new(EngineCore::from_storage(
        storage,
        search_engine::default_engine(),
    ));
    let ctx = PolicyContext::new(TENANT).expect("valid tenant id");

    let users_path = common::write_user_store_file(&[("bench", TENANT, "bench")]);
    let addr = common::spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut wire_stream = common::authenticate_to_ready_for_query(addr, "bench", "bench");
    wire_stream
        .set_nodelay(true)
        .expect("set TCP_NODELAY on client socket");

    let config = MeasurementConfig::new(20, (per_round - 20) as u32, 1)
        .expect("protocol minimums satisfied");

    let mut w0_round_medians: Vec<Duration> = Vec::with_capacity(rounds as usize);
    let mut s0_round_medians: Vec<Duration> = Vec::with_capacity(rounds as usize);

    let mut w0_next = 0u64;
    let mut s0_next = 0u64;
    let mut sql_session = SessionState::default();
    // 台帳照会（fail-closed 検証）用に、各ラウンドの最初と最後の operation_id を
    // 記録しておく（全件照会は所要時間を大きく伸ばすため、代表点のみとする）。
    let mut w0_sample_ops: Vec<String> = Vec::new();
    let mut s0_sample_ops: Vec<String> = Vec::new();

    for round in 0..rounds {
        // --- W0: wire_roundtrip ------------------------------------------------
        let w0_round_start_n = w0_next;
        let measurement = run(&config, || {
            let n = w0_next;
            w0_next += 1;
            let (sql, _op_id) = make_stmt_sql(WIRE_ID_BASE, n, DIM, "ingest-wire-w0");
            common::send_simple_query(&mut wire_stream, &sql);
            let tag = common::read_command_complete(&mut wire_stream);
            common::read_ready_for_query(&mut wire_stream);
            if tag != "INSERT 0 1" {
                fail_closed(format!(
                    "W0 round={round} n={n}: unexpected CommandComplete tag {tag:?} (expected \"INSERT 0 1\")"
                ));
            }
        })
        .expect("W0 measurement must succeed");
        // `run` は `workload()` の戻り値を計測区間内で `black_box` して捨てる
        // （最適化抑止のみが目的。ここでは呼び出し前後の `w0_next` から本ラウンドで
        // 使った n の範囲を復元し、代表 operation_id を計算する）。
        w0_sample_ops.push(op_label("ingest-wire-w0", w0_round_start_n));
        w0_sample_ops.push(op_label("ingest-wire-w0", w0_next - 1));
        let round_median = measurement.summary.median;
        w0_round_medians.push(round_median);
        let round_p95 = p95_from_samples(&measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("W0 round={round}: p95: {e}")));
        println!(
            "{}",
            render_tier_round_line("W0_wire_roundtrip", round, round_median, round_p95)
        );

        // --- S0: sql_surface -----------------------------------------------------
        let s0_round_start_n = s0_next;
        let measurement = run(&config, || {
            let n = s0_next;
            s0_next += 1;
            let (sql, _op_id) = make_stmt_sql(SQL_ID_BASE, n, DIM, "ingest-wire-s0");
            let outcome = core
                .execute_sql_in_session(&ctx, &mut sql_session, &sql)
                .expect("execute_sql_in_session for S0");
            match outcome {
                SqlOutcome::Insert(o) if o.rows_affected == 1 => {}
                other => fail_closed(format!(
                    "S0 round={round} n={n}: unexpected outcome {other:?}"
                )),
            }
        })
        .expect("S0 measurement must succeed");
        s0_sample_ops.push(op_label("ingest-wire-s0", s0_round_start_n));
        s0_sample_ops.push(op_label("ingest-wire-s0", s0_next - 1));
        let round_median = measurement.summary.median;
        s0_round_medians.push(round_median);
        let round_p95 = p95_from_samples(&measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("S0 round={round}: p95: {e}")));
        println!(
            "{}",
            render_tier_round_line("S0_sql_surface", round, round_median, round_p95)
        );
    }

    // min-of-R は「ラウンド中央値どうしの最小」（`docs/design/
    // benchmark-judgement-policy.md` §3・`harness::knn_wire` の定義。単一サンプル
    // どうしの最小ではない）。
    let w0_min_of_r = min_of(&w0_round_medians).unwrap_or_else(|e| fail_closed(e));
    let w0_median_of_r = median_of(&w0_round_medians).unwrap_or_else(|e| fail_closed(e));
    let s0_min_of_r = min_of(&s0_round_medians).unwrap_or_else(|e| fail_closed(e));
    let s0_median_of_r = median_of(&s0_round_medians).unwrap_or_else(|e| fail_closed(e));
    // 参照区間帯（S0 のラウンド間中央値の run-to-run 幅。S0 は wire 帰属の
    // 分母となる「変更を含まない側」の上流区間であり、`knn_wire_profile_bench.rs`
    // が距離カーネル区分〔T1′〕から算出するのと同じ役割を担う）。
    let reference_band_pct = reference_band(&s0_round_medians)
        .map(|b| b * 100.0)
        .unwrap_or_else(|e| fail_closed(format!("reference_band: {e}")));

    // --- 整合性検証（fail-closed。すべて通過するまで測定値を出力しない） -----------
    // 計測ループ内で毎文の CommandComplete／SqlOutcome は既に検証済み。ここでは
    // 代表 operation_id が台帳へ記録されていることを追加確認する（accept ループ
    // スレッドが Arc<EngineCore> を保持し続けるため DB 再オープンでの行数照合は
    // 行えない。モジュール冒頭コメント参照）。
    for op_label in w0_sample_ops.iter().chain(s0_sample_ops.iter()) {
        let op_id = OperationId::parse(op_label).expect("valid operation id for integrity check");
        match core.operation_recorded(&ctx, TABLE, &op_id) {
            Ok(LedgerLookup::Recorded) => {}
            other => fail_closed(format!(
                "operation_recorded mismatch for {op_label:?}: expected Recorded, got {other:?}"
            )),
        }
    }
    println!(
        "integrity: operation_recorded == Recorded for {} sampled operation_id values (W0/S0)",
        w0_sample_ops.len() + s0_sample_ops.len()
    );

    // --- 出力（整合性検証をすべて通過した後） ------------------------------------
    println!(
        "{}",
        render_tier_summary_line("W0_wire_roundtrip", w0_min_of_r, w0_median_of_r)
    );
    println!(
        "{}",
        render_tier_summary_line("S0_sql_surface", s0_min_of_r, s0_median_of_r)
    );
    // `harness::knn_wire::render_reference_band_line` はラベルが
    // "kernel_distance_only" に固定されており（Issue #463 固有の対照区分名）、
    // 本ベンチの対照区分（S0 `sql_surface`）とは異なるため使わず、独自に整形する。
    println!("reference_band(S0_sql_surface): {reference_band_pct:.2}%");

    // 単文あたりの min-of-R レイテンシから換算した参考 rows/s（crossdb の
    // `ingest_single_stmt`〔psycopg・別プロセス〕と単位を揃えて並記する目的の
    // informational な値。in-process 生 TCP と別プロセス・言語間クライアントの
    // 計測器差は `docs/design/knn-wire-stage-profile.md` 参照）。
    let w0_rps = rows_per_sec(1, w0_min_of_r).unwrap_or(f64::NAN);
    println!(
        "ingest_wire_profile_bench: W0 min-of-r-based rows_per_sec={w0_rps:.1} (crossdb reference: ingest_single_stmt ~7,774 rows/s. in-process raw TCP vs psycopg/separate-process — see docs/design/knn-wire-stage-profile.md for measurement-instrument differences)"
    );

    // `ratio`（全体構成比・W0 min-of-R に対する比率）と `band`（ノイズ判定）は
    // 別の分母から算出する独立した値（`knn_wire_profile_bench.rs` と同じ分離
    // 方針。`docs/design/benchmark-judgement-policy.md` §4 が要求するノイズ判定は
    // 「対象区間自身の相対増分」であって全体構成比ではないため、`ratio` を
    // ノイズ判定へ流用しない）。
    match bucket_diff(s0_min_of_r, w0_min_of_r) {
        Some(diff) => {
            let ratio = diff_ratio_pct(diff, w0_min_of_r).unwrap_or(f64::NAN);
            let step_ratio = step_ratio_pct(s0_min_of_r, w0_min_of_r).unwrap_or(f64::NAN);
            let band = classify_against_bands(step_ratio, reference_band_pct);
            println!(
                "{}",
                render_bucket_line("wire(W0-S0)", Some(diff), Some(ratio), Some(band))
            );
        }
        None => println!("{}", render_bucket_line("wire(W0-S0)", None, None, None)),
    }

    println!("ingest_wire_profile_bench: OK");
}
