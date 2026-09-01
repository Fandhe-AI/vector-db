//! SQL パース・束縛コストの実測ベンチ（Issue #360「SQL パース・束縛結果の
//! セッション内キャッシュ検討（実測裏付け前提）」）。
//!
//! # 目的
//!
//! セッション単位の SQL 文字列 → 束縛結果キャッシュ（PostgreSQL の plan cache /
//! SQLite の prepared statement 相当）を導入する価値があるかを判断するため、
//! 「字句解析＋許可リスト構文解析＋束縛」のコストが「フル実行（走査＋実行）」に
//! 対してどの程度の比率かを実測する。Issue 本文は「パース費用は ms オーダーの
//! p50 に対して小さいと推定されるため、先に実測し、効果が薄ければ見送り判断を
//! 記録して close してよい」としており、本ベンチはその実測入口。
//!
//! `EngineCore::execute_sql_in_session` の非 `USING PLAN` Select 経路
//! （`crates/engine/src/core.rs::execute_validated_in_session`）は
//! `read_txn_with_schema` → `sql::parser::bind_in_session` の順で束縛する。
//! `read_txn_with_schema` は `pub(crate)` のためベンチ（`engine` を外部クレートとして
//! 参照する独立コンパイル単位）からは直接呼べず、代わりに公開 API
//! `Storage::get_table_schema`（内部で同じ `get_table_schema_in_txn` を独自の
//! `read_txn` 上で呼ぶ）で近似する。両者は同一スナップショット保証の有無こそ
//! 異なるが、スキーマ取得コストそのもの（redb からの `TableSchema` デコード）は
//! 同一処理であり、本ベンチが測りたい「キャッシュヒット時に省略できる総コスト」の
//! 近似として妥当とみなす。
//!
//! ベクトルリテラルのパース（`parse_vector_literal`。C1 系クエリでは 768 次元 ≒
//! 数 KiB の文字列）は `validate_sql` ではなく束縛段（`bind_ranking`）で行われるため、
//! 系列 (a) と (b) の差分がベクトルリテラルパースの寄与を表す。
//!
//! # 計測 3 系列
//!
//! - (a) `validate_sql`: 字句解析＋許可リスト構文解析＋ FROM テーブル存在確認
//! - (b) (a) ＋ スキーマ取得 ＋ `bind_in_session`（＝キャッシュで省略可能になる総コスト）
//! - (c) `EngineCore::execute_sql`（SQL 表層フル実行。走査・実行を含む）
//!
//! `EngineCore` は `Storage` を外へ出さない一方向設計（`sql_c1_bench.rs` 冒頭
//! コメント参照）のため、(a)/(b) は `Storage` を直接操作できる区間で `run` により
//! 単独計測してから `EngineCore::from_storage` へ move し、(c) も同一
//! `MeasurementConfig` の単独計測として揃える（(b)/(c) 間の interleaved A/B は
//! `EngineCore` が `storage` を再度露出しない限り組めないため、本ベンチでは
//! 単独計測 3 本の中央値比較とする）。
//!
//! # 出力規約
//!
//! 本ベンチは spec 由来の pass/fail 閾値を持たない情報提供専用（`hybrid_latency_bench.rs`
//! と同一方針）。実測値（中央値・比率）は Issue #360 の受け入れ条件そのもの
//! （実測の記録）であるため、常に出力する（verbose ガードなし。オーナー判断
//! 2026-08-29・`.claude/rules/spec-confidentiality.md`「許可される参照」）。
//!
//! `make bench-parse-bind`（Makefile）から実行する。判定ロジック自体
//! （時間非依存）は `harness::parse_bind` にあり `tests/parse_bind_bench_accept.rs`
//! で `make ci` 側から回帰検証する。CI ワークフロー（`.github/workflows/*`）へは
//! 配線しない（`bench-hybrid`・`bench-c1` と同一方針。手動実行専用）。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::parse_bind::{render_measurement_line, ParseBindMeasurement};
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::{c1_statement, vector_literal};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::search_engine;
use engine::sql::allowlist::{validate_sql, Statement};
use engine::sql::mode::SessionState;
use engine::sql::parser::bind_in_session;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 768;
const TOP_K: usize = 20;

/// 単一 write トランザクションの確保量を有界化するための投入チャンクサイズ
/// （`sql_c1_bench.rs` と同一方針）。
const SEED_BATCH_ROWS: usize = 10_000;

const TABLE: &str = "documents";
const COLUMN: &str = "embedding";
const TENANT_ID: &str = "bench-tenant";

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("sql_parse_bind_bench: {msg}");
    std::process::exit(1);
}

/// `row_count` 行を投入した一時 DB を構築する（`sql_c1_bench.rs::main` のデータ投入
/// 部分を構成単位で再利用できるよう関数化したもの）。
fn seed_storage(label: &str, row_count: usize, seed: u64) -> (Storage, CleanupGuard) {
    let path = unique_db_path(&format!("issue360-parse-bind-{label}"));
    let guard = CleanupGuard(path.clone());
    let storage =
        Storage::open(&path).unwrap_or_else(|e| fail_closed(format!("open storage: {e}")));
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new(
                COLUMN,
                ColumnType::Vector(DIM as u32),
                false,
            )],
        ))
        .unwrap_or_else(|e| fail_closed(format!("create table: {e}")));

    let ctx =
        PolicyContext::new(TENANT_ID).unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")));
    let mut rng = DeterministicRng::new(seed);
    let mut next_id: u64 = 0;
    while (next_id as usize) < row_count {
        let batch_len = SEED_BATCH_ROWS.min(row_count - next_id as usize);
        let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
        for _ in 0..batch_len {
            batch_vectors.push(rng.next_vector(DIM));
        }
        let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
            .map(|i| {
                let id = next_id + i as u64;
                (
                    id,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility: Visibility::Public,
                        embedding: &batch_vectors[i],
                        // VECTOR 列のみのテーブルでは空バイト列が期待される
                        // （`sql_c1_bench.rs` 同箇所のコメント参照）。
                        metadata: b"",
                    },
                )
            })
            .collect();
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
            "seed-batch-{label}-{next_id}"
        ))
        .unwrap_or_else(|e| fail_closed(format!("operation_id: {e}")));
        engine::tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id)
            .unwrap_or_else(|e| fail_closed(format!("seed batch insert: {e}")));
        next_id += batch_len as u64;
    }
    (storage, guard)
}

/// 1 構成（`row_count` 行）の測定を行い、レポート行を出力する。
fn measure_config(label: &str, row_count: usize) {
    let (storage, _guard) = seed_storage(label, row_count, 1);
    let mut rng = DeterministicRng::new(2);
    let query = rng.next_vector(DIM);
    let literal =
        vector_literal(&query).unwrap_or_else(|e| fail_closed(format!("vector literal: {e}")));
    let sql = c1_statement(TABLE, COLUMN, &literal, TOP_K)
        .unwrap_or_else(|e| fail_closed(format!("c1 statement: {e}")));

    let config = MeasurementConfig::new(20, 50, 1)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));

    // --- (a) validate_sql 単体 ---
    let measurement_a = run(&config, || {
        validate_sql(&sql, &storage).unwrap_or_else(|e| fail_closed(format!("validate_sql: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("measurement (a) protocol violation: {e}")));

    // --- (b) validate_sql ＋ スキーマ取得 ＋ bind_in_session ---
    let session = SessionState::default();
    let measurement_b = run(&config, || {
        let stmt = validate_sql(&sql, &storage)
            .unwrap_or_else(|e| fail_closed(format!("validate_sql (b): {e}")));
        let validated = match stmt {
            Statement::Select(validated) => validated,
            other => fail_closed(format!(
                "unexpected statement variant for C1 query: {other:?}"
            )),
        };
        let schema = storage
            .get_table_schema(TABLE)
            .unwrap_or_else(|e| fail_closed(format!("get_table_schema: {e}")));
        bind_in_session(&validated, &schema, session.search_mode(), session.udfs())
            .unwrap_or_else(|e| fail_closed(format!("bind_in_session: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("measurement (b) protocol violation: {e}")));

    // (c) はフル実行のため EngineCore が storage の所有権を握る。(a)/(b) の計測が
    // 完了してから storage を EngineCore へ move する（同一 DB に対する測定を
    // 直列化することで、複数の Storage ハンドルによる redb 側の同時アクセス制約を
    // 回避する）。
    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let ctx =
        PolicyContext::new(TENANT_ID).unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")));

    // --- (c) EngineCore::execute_sql フル実行 ---
    //
    // `EngineCore` は `Storage` を外へ出さない一方向設計（`sql_c1_bench.rs`
    // 冒頭コメント参照）のため、(a)/(b) のような `storage` 直接操作による
    // interleaved A/B（`run_ab`）は組めない。(a)/(b) は同一 `storage` 上で
    // 直列計測済みのため、(c) も同一構成の単独計測（`run`）で揃え、3 系列とも
    // 同一 `MeasurementConfig` の単独計測として比較する。
    let measurement_c = run(&config, || {
        core.execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| fail_closed(format!("execute_sql (c): {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("measurement (c) protocol violation: {e}")));

    let report = ParseBindMeasurement {
        validate_median: measurement_a.summary.median,
        parse_and_bind_median: measurement_b.summary.median,
        full_execution_median: measurement_c.summary.median,
    };
    println!("{}", render_measurement_line(label, &report));
}

fn main() {
    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "sql_parse_bind_bench: measures validate_sql / (validate_sql + bind) / full \
         execute_sql cost ratios to inform the Issue #360 session-scoped bind cache \
         go/no-go decision (informational only; no spec-derived pass/fail threshold; see \
         docs/design/sql-parse-bind-cache.md)"
    );

    // 現実的構成（sql_c1_bench.rs と同一値: 10,000 行）と、実行コストが小さく
    // パース比率が最大化する対照の小規模構成（1,000 行）を測る。
    measure_config("small_1k_rows", 1_000);
    measure_config("realistic_10k_rows", 10_000);
}
