//! SQL 表層 C1（純粋 Top-k）の p95 再測定ベンチ（TASK-83。ポインタ:
//! `docs/spec/05-tasks.md` TASK-83・Conditional Go 条件7）。
//!
//! `simd_bench.rs`（TASK-127）は `SearchProvider` を直接叩く provider 単体の
//! p95 であり、`EngineCore::execute_sql`（`sql::exec::execute_statement`。SQL-1〜4、
//! TASK-75）経由の C1 p95 は本ベンチが初めて計測する。SQL 表層は毎クエリ
//! `VectorArena::build_filtered_with_rows_in_txn` で候補行を redb から再デコードし
//! （`core::EngineCore::search` が使う `PrefilterCache`〔TASK-169〕は SQL 経路では
//! 使われない）、この再デコードが p95 の支配項になりうる。本ベンチは実測に加えて
//! SQL 表層 vs `EngineCore::search`（`VectorCore` trait 経由）の interleaved A/B
//! （[`harness::ab::run_ab`]）を診断情報として同時に取り、切り分け材料とする
//! （A/B の結果は合否には含めない）。**注意**: `EngineCore::search` 側は
//! `PrefilterCache`〔TASK-169〕を経由するため、テーブル内容が変化しない本ベンチの
//! 反復では初回呼び出し以降キャッシュがウォームな状態で計測される。したがって
//! `median_ratio` は「SQL 表層のパース・束縛コスト」と「キャッシュヒット時の
//! `EngineCore::search`」の比であり、両経路の候補デコード実装そのものを対称条件で
//! 比較した値ではない（SQL 表層は毎回コールドな arena 再構築を伴う）。切り分けの
//! 参考値として扱い、厳密な対称比較としては解釈しない。
//!
//! # 専有環境の宣言（Conditional Go 条件7）
//!
//! spec の計測環境前提（専有環境）を本ベンチは自動判定できないため、
//! `BENCH_DEDICATED_ENV=1` を明示的に設定した run でのみ「専有環境として宣言された」
//! ことを記録する（[`dedicated_env_attested_from_env`]）。未設定（既定）の run では
//! p95・Recall の pass/fail 自体は常に出力しつつ、条件7 の判定対象からは明示的に
//! 除外する（silent skip にしない。`simd_bench.rs` の CORE-5 opt-in と同一方針）。
//!
//! 数値基準（p95 上限・Recall 下限）は SQL-1 専用の環境変数
//! （`BENCH_SQL_C1_MAX_P95_MS`・`BENCH_SQL_C1_MIN_RECALL`）から注入する。
//! `simd_bench.rs` の `BENCH_MAX_P95_MS`・`BENCH_MIN_RECALL` は CORE-3・SEARCH-4・
//! CORE-4（`SearchProvider` 単体）の基準であり、SQL 表層全体を対象とする SQL-1 の
//! 基準とは spec 上の出所が異なるため、流用せず別 variable として分離する
//! （流用すると緩い側で false green・厳しい側で false red になる）。spec が SSOT の
//! ため本ファイルにはハードコードしない（`.claude/rules/spec-confidentiality.md`）。
//! 標準出力には実測値と pass/fail のみを記録し、注入された閾値そのものは出力しない
//! （`simd_bench.rs` と同一方針）。
//!
//! `make bench-c1`（Makefile）・`.github/workflows/bench.yml`
//! （`workflow_dispatch` 限定。GitHub ホステッド runner は共有 2 vCPU のため恒常的な
//! schedule 実行には向かない）から実行する想定。判定ロジック自体
//! （時間非依存）は `harness::accept` にあり `tests/c1_bench_accept.rs` で
//! `make ci` 側から回帰検証する。

// `harness` は独立したコンパイル単位（cargo bench バイナリ）から取り込まれる共有
// ソース。本ファイルが実際に使う項目のみで、未到達の `pub` 項目は `dead_code`
// 警告になりうるためモジュール全体を対象に許容する（`simd_bench.rs` と同一方針）。
#[allow(dead_code)]
mod harness;

use harness::ab::run_ab;
use harness::accept::{
    check_p95_within_limit, check_recall_within_limit, p95_from_samples, recall_at_k, worst_recall,
};
use harness::env_report::EnvReport;
use harness::protocol::{run, MeasurementConfig};
use harness::rng::DeterministicRng;
use harness::sql_c1::{c1_statement, vector_literal};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

use std::time::Duration;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 測定条件（`simd_bench.rs` の TASK-127 と同一値。既存ベンチがすでに公開コード
/// へ含んでいるため新規の漏えいではない）。
const ROW_COUNT: usize = 100_000;
const DIM: usize = 768;
const TOP_K: usize = 20;

/// Recall@k 判定に使うクエリ本数（`simd_bench.rs` と同一方針。worst-query 判定）。
const RECALL_QUERY_COUNT: usize = 20;

/// 単一 write トランザクションの確保量を有界化するための投入チャンクサイズ。
const SEED_BATCH_ROWS: usize = 10_000;

const TABLE: &str = "documents";
const COLUMN: &str = "embedding";
const TENANT_ID: &str = "bench-tenant";

/// `BENCH_SQL_C1_MAX_P95_MS` 環境変数を読み取る（検証仕様は
/// `simd_bench.rs::max_p95_from_env` と同一だが、SQL-1 の基準は provider 単体の
/// CORE-3・SEARCH-4 とは別物のため variable 名を分ける。数値基準は spec が SSOT の
///ためここにはデフォルト値を持たない）。
fn max_p95_from_env() -> Result<Duration, String> {
    let raw = std::env::var("BENCH_SQL_C1_MAX_P95_MS")
        .map_err(|_| "BENCH_SQL_C1_MAX_P95_MS is not set (see .github/workflows/bench.yml vars)")?;
    let millis: u64 = raw.trim().parse().map_err(|_| {
        "BENCH_SQL_C1_MAX_P95_MS must be a positive integer (milliseconds)".to_string()
    })?;
    if millis == 0 {
        return Err("BENCH_SQL_C1_MAX_P95_MS must be greater than 0".to_string());
    }
    Ok(Duration::from_millis(millis))
}

/// `BENCH_SQL_C1_MIN_RECALL` 環境変数を読み取る（検証仕様は
/// `simd_bench.rs::min_recall_from_env` と同一。CORE-4 の Recall 基準と混同しない
/// よう SQL-1 専用の variable 名にする）。
fn min_recall_from_env() -> Result<f64, String> {
    let raw = std::env::var("BENCH_SQL_C1_MIN_RECALL")
        .map_err(|_| "BENCH_SQL_C1_MIN_RECALL is not set (see .github/workflows/bench.yml vars)")?;
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| "BENCH_SQL_C1_MIN_RECALL must be a floating-point number".to_string())?;
    if !(value > 0.0 && value <= 1.0) {
        return Err("BENCH_SQL_C1_MIN_RECALL must be within (0.0, 1.0]".to_string());
    }
    Ok(value)
}

/// `BENCH_DEDICATED_ENV` 環境変数を読み取り、Conditional Go 条件7 の判定を opt-in で
/// 有効化するかを返す。`"1"` のときのみ「専有環境として宣言された」ことを記録する
/// （未設定・それ以外の値はすべて「未宣言」。本ベンチは他プロセスとの共有有無を
/// 自動検出できないため、宣言は運用者の責任とする——モジュール冒頭コメント参照）。
fn dedicated_env_attested_from_env() -> bool {
    std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

fn main() {
    let max_p95 = match max_p95_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("sql_c1_bench: {msg}");
            std::process::exit(1);
        }
    };
    let min_recall = match min_recall_from_env() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("sql_c1_bench: {msg}");
            std::process::exit(1);
        }
    };
    let dedicated_env_attested = dedicated_env_attested_from_env();

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );

    // --- データ投入: 一時 DB へ ROW_COUNT 行を SEED_BATCH_ROWS 単位で投入する ---
    let path = unique_db_path("task83-sql-c1");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new(
                COLUMN,
                ColumnType::Vector(DIM as u32),
                false,
            )],
        ))
        .expect("create table for bench seeding");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant id");
    let mut rng = DeterministicRng::new(1);
    // 参照実装（`CpuScalarProvider`）比較用に、投入した id・平坦化済みベクトルを
    // 手元にも保持する（`EngineCore` は `Storage` を外へ出さない一方向設計のため。
    // `tests/sql_surface.rs` と同じ流儀）。
    let mut ids: Vec<u64> = Vec::with_capacity(ROW_COUNT);
    let mut flat_vectors: Vec<f32> = Vec::with_capacity(ROW_COUNT * DIM);

    let mut next_id: u64 = 0;
    while (next_id as usize) < ROW_COUNT {
        let batch_len = SEED_BATCH_ROWS.min(ROW_COUNT - next_id as usize);
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
                        // 本テーブルは VECTOR 列のみで Text 列を持たないため、
                        // `sql::exec` が期待するスカラーペイロード
                        // （`row_codec::encode_scalar_columns`）は空バイト列になる
                        // （同関数のドキュメント参照: VECTOR 列は常にスキップされ、
                        // 他に列がなければ `buf` へ何も書き込まれない）。ここへ
                        // 空でないバイト列を渡すと SQL 実行時のスカラーデコードが
                        // 失敗し `arena build failed`（`XX000`）で拒否される。
                        metadata: b"",
                    },
                )
            })
            .collect();
        engine::tenant::insert_rows(
            &storage,
            TABLE,
            &ctx,
            &rows,
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("seed batch insert");
        for (i, v) in batch_vectors.iter().enumerate() {
            ids.push(next_id + i as u64);
            flat_vectors.extend_from_slice(v);
        }
        next_id += batch_len as u64;
    }

    let core = EngineCore::from_storage(storage, search_engine::default_engine());

    let mut passed = true;

    // --- p95: SQL 表層 C1 の実行時間 ---
    let latency_query = rng.next_vector(DIM);
    let latency_literal = vector_literal(&latency_query).expect("finite query vector");
    let latency_sql = c1_statement(TABLE, COLUMN, &latency_literal, TOP_K)
        .expect("well-formed C1 statement from validated identifiers");
    let config = MeasurementConfig::new(20, 50, 1).expect("protocol minimums satisfied");
    let measurement = run(&config, || {
        core.execute_sql(&ctx, &latency_sql)
            .expect("execute_sql must succeed for well-formed synthetic C1 query")
    })
    .expect("measurement must satisfy protocol minimums");

    let p95 = p95_from_samples(&measurement.samples).expect("non-empty samples must yield a p95");
    let p95_ok = check_p95_within_limit(p95, max_p95);
    passed &= p95_ok;
    // limit（BENCH_SQL_C1_MAX_P95_MS の実測値）は意図的にログへ出力しない（`simd_bench.rs`
    // と同一方針。モジュール冒頭コメント参照）。
    println!(
        "p95_latency(sql_c1): rows={ROW_COUNT} dim={DIM} k={TOP_K} median={:?} p95={p95:?} pass={p95_ok}",
        measurement.summary.median,
    );

    // --- Recall@20: SQL 表層 C1 vs CpuScalarProvider 参照実装 ---
    let reference = CpuScalarProvider;
    let mut recalls = Vec::with_capacity(RECALL_QUERY_COUNT);
    for _ in 0..RECALL_QUERY_COUNT {
        let query = rng.next_vector(DIM);
        let literal = vector_literal(&query).expect("finite query vector");
        let sql = c1_statement(TABLE, COLUMN, &literal, TOP_K)
            .expect("well-formed C1 statement from validated identifiers");

        let expected: Vec<u64> = reference
            .search(SearchInput {
                ids: &ids,
                vectors: &flat_vectors,
                dim: DIM as u32,
                query: &query,
                k: TOP_K,
            })
            .expect("reference search must succeed for well-formed synthetic input")
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        let actual: Vec<u64> = core
            .execute_sql(&ctx, &sql)
            .expect("execute_sql must succeed for well-formed synthetic C1 query")
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect();

        recalls.push(recall_at_k(&expected, &actual).expect("non-empty reference top-k"));
    }
    let recall_min =
        worst_recall(&recalls).expect("RECALL_QUERY_COUNT queries yield a non-empty recall list");
    let recall_ok = check_recall_within_limit(recall_min, min_recall)
        .expect("min_recall validated by min_recall_from_env");
    passed &= recall_ok;
    println!(
        "topk_consistency(sql_c1_vs_scalar_exhaustive): k={TOP_K} queries={RECALL_QUERY_COUNT} recall_min={recall_min:.6} pass={recall_ok}"
    );

    // --- 診断 A/B: SQL 表層 vs EngineCore::search（VectorCore trait 経由。合否には含めない） ---
    let ab_query = rng.next_vector(DIM);
    let ab_literal = vector_literal(&ab_query).expect("finite query vector");
    let ab_sql = c1_statement(TABLE, COLUMN, &ab_literal, TOP_K)
        .expect("well-formed C1 statement from validated identifiers");
    match run_ab(
        &config,
        || -> Vec<u64> {
            core.execute_sql(&ctx, &ab_sql)
                .expect("execute_sql must succeed for well-formed synthetic C1 query")
                .rows
                .into_iter()
                .map(|row| row.id)
                .collect()
        },
        || -> Vec<u64> {
            core.search(&ctx, TABLE, &ab_query, TOP_K)
                .expect("VectorCore::search must succeed for well-formed synthetic input")
                .into_iter()
                .map(|hit| hit.id)
                .collect()
        },
    ) {
        Ok(ab) => {
            println!(
                "diagnostic_ab(sql_surface_vs_core_search): a_median={:?} b_median={:?} median_ratio={:.4} (not counted toward pass/fail)",
                ab.a.summary.median, ab.b.summary.median, ab.median_ratio
            );
        }
        Err(e) => {
            // A/B は診断情報であり合否に含めないため、算出不能（分母 0 等）でも
            // 本体の pass/fail には影響させない。ただし silent にはせず記録する。
            println!(
                "diagnostic_ab(sql_surface_vs_core_search): unavailable ({e}) (not counted toward pass/fail)"
            );
        }
    }

    // --- Conditional Go 条件7 の表示 ---
    if dedicated_env_attested {
        println!("conditional_go_7: dedicated_env=attested p95_pass={p95_ok}");
    } else {
        println!(
            "conditional_go_7: not evaluated in this run (dedicated environment not attested; set BENCH_DEDICATED_ENV=1 only when running on a host with no other processes sharing CPU/IO)"
        );
    }

    if !passed {
        eprintln!("sql_c1_bench: acceptance criteria not met (TASK-83 SQL-1 p95/Recall)");
        std::process::exit(1);
    }
}
