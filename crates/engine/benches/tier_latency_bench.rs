//! ティア別レイテンシ受け入れ基準の実測ベンチ（TASK-116。ポインタ:
//! `docs/spec/05-tasks.md` TASK-116・対象ビヘイビア: `docs/spec/04-behavior/
//! query-planning.md` PLAN-4, PLAN-6, PLAN-7。判定内容・測定段階・数値基準は
//! spec 側が SSOT であり本ファイルには転記しない
//! （`.claude/rules/spec-confidentiality.md`）。
//!
//! 前提 TASK-115（PLAN-8）の `EngineCore::with_tiered_query_planner`／
//! `crate::tiering` および `USING PLAN('<query>')` 経由の `EngineCore::execute_sql`
//! 一意ディスパッチ（`sql::using_plan` モジュールドキュメント参照）を用いて計測する。
//!
//! wire 経由・3 クライアントでの `USING PLAN` レイテンシ検証は TASK-117（PLAN-9）の
//! 管轄でありスコープ外（本ベンチは engine クレート内で完結する）。
//!
//! # ティア routing の実証
//!
//! [`harness::tier::DIALOGUE_QUESTION`]／[`harness::tier::PRECISION_QUESTION`] の
//! 実測分類が意図したティアと一致することを本ベンチ内で確認し、不一致は
//! （閾値を満たしていても）fail とする。誤ったティアのモデルで基準判定する
//! false green/red を防ぐため。
//!
//! # 常駐 Ollama 前提・opt-in
//!
//! 常駐 Ollama への実接続が前提のため、[`harness::tier::opt_in_requested`]
//! （`BENCH_TIER` env）が明示的に要求された run でのみ実測する。未 opt-in の既定
//! run は「測定不能」を明示ログ出力して判定対象外とする（silent skip 禁止）。
//! opt-in 済みで接続・閾値 env が未設定・不正な場合は fail-closed で非ゼロ終了する。
//!
//! 数値基準（p95 上限）・接続先はいずれも env 経由で注入し、本ファイルには
//! ハードコードしない。標準出力には実測値と pass/fail のみを記録し、注入された
//! 閾値そのものは出力しない（`sql_c1_bench.rs` と同一方針）。
//!
//! `make bench-tier`（Makefile）から実行する。GitHub ホステッド runner には常駐
//! Ollama が無く、self-hosted runner の使用は組織承認済み例外の範囲外（AGENTS.md
//! 「CI・ワークフローの改変（P1）」）のため、`.github/workflows/bench.yml` に本
//! ベンチの実行経路は置かない（opt-in の有無を問わず実測を成功させられないため。
//! README「ティア別レイテンシ受け入れ基準の実測手順」参照）。実測は常駐 Ollama を
//! 持つ環境で運用者が本コマンドを直接実行する。判定ロジック自体（時間非依存）は
//! `harness::tier` にあり `tests/tier_latency_accept.rs` で `make ci` 側から回帰
//! 検証する。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::protocol::{run, MeasurementConfig};
use harness::tier::{
    build_corpus, build_ollama_client, judge, opt_in_requested, parse_host, parse_max_p95_ms,
    parse_model_name, parse_port, using_plan_statement, TierSamples, TierThresholds,
    DIALOGUE_QUESTION, PRECISION_QUESTION,
};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::HashingEmbedder;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::storage::{Storage, Visibility};
use engine::tiering::{Tier, TieringCriteria};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;

const TABLE: &str = "tier_bench_docs";
const DIM: u32 = 32;
const TOP_K: usize = 5;
const CORPUS_ROWS: usize = 64;
const TENANT_ID: &str = "bench-tenant";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// opt-in ゲートを満たさない（`BENCH_TIER` 未設定・空文字）ことを明示ログとして
/// 出力する（silent skip 禁止。合否には数えない）。
fn print_not_evaluated() {
    println!(
        "conditional_tier_latency: not evaluated in this run (BENCH_TIER opt-in not requested; set BENCH_TIER=1 with BENCH_TIER_* connection/threshold vars only on a host with a resident Ollama)"
    );
}

fn env_raw(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

/// `--help`／`-h` 指定時に表示する env 一覧（変数名と用途 1 行のみ。数値・閾値・
/// 実測値は一切含めない）。README「ティア別レイテンシ受け入れ基準の実測手順」から
/// 案内される想定の実処理（案内と実挙動を一致させる。codex-review PR #269 指摘）。
/// `harness = false`（`Cargo.toml`）のため `std::env::args()` を自前で解析する
/// 必要がある。
fn print_help() {
    println!("tier_latency_bench: env vars used by this benchmark (values/thresholds are not printed; see README)");
    println!();
    println!("opt-in:");
    println!("  BENCH_TIER                                  set to opt in to running this benchmark against a resident Ollama");
    println!();
    println!("connection (required once opted in):");
    println!("  BENCH_TIER_OLLAMA_HOST                        resident Ollama host");
    println!("  BENCH_TIER_OLLAMA_PORT                        resident Ollama port");
    println!(
        "  BENCH_TIER_DIALOGUE_MODEL                     model name used for the dialogue tier"
    );
    println!(
        "  BENCH_TIER_PRECISION_MODEL                    model name used for the high-precision tier"
    );
    println!();
    println!(
        "thresholds (required once opted in; values are spec-derived and not documented here):"
    );
    println!(
        "  BENCH_TIER_DIALOGUE_MAX_EXPANSION_P95_MS      dialogue tier expansion-only p95 upper bound (ms)"
    );
    println!(
        "  BENCH_TIER_DIALOGUE_MAX_P95_MS                dialogue tier end-to-end p95 upper bound (ms)"
    );
    println!(
        "  BENCH_TIER_PRECISION_MAX_EXPANSION_P95_MS     high-precision tier expansion-only p95 upper bound (ms)"
    );
    println!(
        "  BENCH_TIER_PRECISION_MAX_P95_MS               high-precision tier end-to-end p95 upper bound (ms)"
    );
}

/// 引数に `--help`／`-h` が含まれるかを判定する。`cargo bench --bench
/// tier_latency_bench -p engine -- --help` はハーネス無効（`harness = false`）の
/// ため cargo 標準の help 傍受は効かず、本関数で明示的に処理する。
fn help_requested() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
}

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("tier_latency_bench: {msg}");
    std::process::exit(1);
}

fn required_thresholds() -> TierThresholds {
    let dialogue_expansion_max_p95 =
        match env_raw("BENCH_TIER_DIALOGUE_MAX_EXPANSION_P95_MS").as_deref() {
            Some(raw) => parse_max_p95_ms(raw, "BENCH_TIER_DIALOGUE_MAX_EXPANSION_P95_MS")
                .unwrap_or_else(|e| fail_closed(e)),
            None => fail_closed("BENCH_TIER_DIALOGUE_MAX_EXPANSION_P95_MS is not set"),
        };
    let dialogue_e2e_max_p95 = match env_raw("BENCH_TIER_DIALOGUE_MAX_P95_MS").as_deref() {
        Some(raw) => parse_max_p95_ms(raw, "BENCH_TIER_DIALOGUE_MAX_P95_MS")
            .unwrap_or_else(|e| fail_closed(e)),
        None => fail_closed("BENCH_TIER_DIALOGUE_MAX_P95_MS is not set"),
    };
    let precision_expansion_max_p95 =
        match env_raw("BENCH_TIER_PRECISION_MAX_EXPANSION_P95_MS").as_deref() {
            Some(raw) => parse_max_p95_ms(raw, "BENCH_TIER_PRECISION_MAX_EXPANSION_P95_MS")
                .unwrap_or_else(|e| fail_closed(e)),
            None => fail_closed("BENCH_TIER_PRECISION_MAX_EXPANSION_P95_MS is not set"),
        };
    let precision_e2e_max_p95 = match env_raw("BENCH_TIER_PRECISION_MAX_P95_MS").as_deref() {
        Some(raw) => parse_max_p95_ms(raw, "BENCH_TIER_PRECISION_MAX_P95_MS")
            .unwrap_or_else(|e| fail_closed(e)),
        None => fail_closed("BENCH_TIER_PRECISION_MAX_P95_MS is not set"),
    };

    TierThresholds {
        dialogue_expansion_max_p95,
        dialogue_e2e_max_p95,
        precision_expansion_max_p95,
        precision_e2e_max_p95,
    }
}

fn required_connection() -> (String, u16, String, String) {
    let host_raw = env_raw("BENCH_TIER_OLLAMA_HOST")
        .unwrap_or_else(|| fail_closed("BENCH_TIER_OLLAMA_HOST is not set"));
    let host = parse_host(&host_raw, "BENCH_TIER_OLLAMA_HOST").unwrap_or_else(|e| fail_closed(e));
    let port_raw = env_raw("BENCH_TIER_OLLAMA_PORT")
        .unwrap_or_else(|| fail_closed("BENCH_TIER_OLLAMA_PORT is not set"));
    let port = parse_port(&port_raw, "BENCH_TIER_OLLAMA_PORT").unwrap_or_else(|e| fail_closed(e));
    let dialogue_model_raw = env_raw("BENCH_TIER_DIALOGUE_MODEL")
        .unwrap_or_else(|| fail_closed("BENCH_TIER_DIALOGUE_MODEL is not set"));
    let dialogue_model = parse_model_name(&dialogue_model_raw, "BENCH_TIER_DIALOGUE_MODEL")
        .unwrap_or_else(|e| fail_closed(e));
    let precision_model_raw = env_raw("BENCH_TIER_PRECISION_MODEL")
        .unwrap_or_else(|| fail_closed("BENCH_TIER_PRECISION_MODEL is not set"));
    let precision_model = parse_model_name(&precision_model_raw, "BENCH_TIER_PRECISION_MODEL")
        .unwrap_or_else(|e| fail_closed(e));

    (host, port, dialogue_model, precision_model)
}

fn main() {
    if help_requested() {
        print_help();
        return;
    }

    if !opt_in_requested(env_raw("BENCH_TIER").as_deref()) {
        print_not_evaluated();
        return;
    }

    let thresholds = required_thresholds();
    let (host, port, dialogue_model, precision_model) = required_connection();

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );

    let dialogue_client =
        build_ollama_client(&host, port, &dialogue_model).unwrap_or_else(|e| fail_closed(e));
    let precision_client =
        build_ollama_client(&host, port, &precision_model).unwrap_or_else(|e| fail_closed(e));

    let path = temp_db::unique_db_path("task116-tier-latency");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage for bench seeding");
    storage.create_table(&schema()).expect("create table");

    let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant id");
    for row in build_corpus(CORPUS_ROWS, DIM as usize, 1) {
        let op_id =
            OperationId::parse(&format!("tier-bench-seed-{}", row.id)).expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            row.id,
            Visibility::Public,
            &[
                Value::Vector(row.embedding),
                Value::Text(row.path),
                Value::Text(row.body),
            ],
            &op_id,
        )
        .expect("seed row insert");
    }

    let core = EngineCore::from_storage(storage, search_engine::default_engine())
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_tiered_query_planner(
            Box::new(dialogue_client),
            Box::new(precision_client),
            TieringCriteria::default(),
        );

    let config = MeasurementConfig::new(20, 30, 1).expect("protocol minimums satisfied");

    // --- ティア別クエリ展開の追加処理時間（PLAN-4） ---
    let mut dialogue_routing_matched = true;
    let dialogue_expansion = run(&config, || {
        let (_, classification) = core
            .plan_query_with_classification(&ctx, TABLE, DIALOGUE_QUESTION)
            .expect("plan_query_with_classification must succeed against a resident Ollama");
        if classification.map(|c| c.tier) != Some(Tier::Dialogue) {
            dialogue_routing_matched = false;
        }
    })
    .expect("measurement must satisfy protocol minimums");

    let mut precision_routing_matched = true;
    let precision_expansion = run(&config, || {
        let (_, classification) = core
            .plan_query_with_classification(&ctx, TABLE, PRECISION_QUESTION)
            .expect("plan_query_with_classification must succeed against a resident Ollama");
        if classification.map(|c| c.tier) != Some(Tier::HighPrecision) {
            precision_routing_matched = false;
        }
    })
    .expect("measurement must satisfy protocol minimums");

    // --- ティア別展開込みエンドツーエンド（PLAN-6/7・`USING PLAN` 一意ディスパッチ） ---
    let dialogue_sql = using_plan_statement(TABLE, DIALOGUE_QUESTION, TOP_K);
    let dialogue_e2e = run(&config, || {
        core.execute_sql(&ctx, &dialogue_sql)
            .expect("USING PLAN dispatch must succeed against a resident Ollama")
    })
    .expect("measurement must satisfy protocol minimums");

    let precision_sql = using_plan_statement(TABLE, PRECISION_QUESTION, TOP_K);
    let precision_e2e = run(&config, || {
        core.execute_sql(&ctx, &precision_sql)
            .expect("USING PLAN dispatch must succeed against a resident Ollama")
    })
    .expect("measurement must satisfy protocol minimums");

    let samples = TierSamples {
        dialogue_expansion: dialogue_expansion.samples,
        dialogue_e2e: dialogue_e2e.samples,
        precision_expansion: precision_expansion.samples,
        precision_e2e: precision_e2e.samples,
        dialogue_routing_matched,
        precision_routing_matched,
    };

    let judgment = judge(&samples, &thresholds).expect("non-empty measurement samples");

    // limit（BENCH_TIER_*_MAX_*_P95_MS の実測値）は意図的にログへ出力しない
    // （`sql_c1_bench.rs` と同一方針。モジュール冒頭コメント参照）。
    println!(
        "p95_latency(tier_dialogue_expansion): p95={:?} pass={} routing_matched={}",
        judgment.dialogue_expansion_p95,
        judgment.dialogue_expansion_ok,
        judgment.dialogue_routing_matched
    );
    println!(
        "p95_latency(tier_dialogue_e2e): p95={:?} pass={}",
        judgment.dialogue_e2e_p95, judgment.dialogue_e2e_ok
    );
    println!(
        "p95_latency(tier_precision_expansion): p95={:?} pass={} routing_matched={}",
        judgment.precision_expansion_p95,
        judgment.precision_expansion_ok,
        judgment.precision_routing_matched
    );
    println!(
        "p95_latency(tier_precision_e2e): p95={:?} pass={}",
        judgment.precision_e2e_p95, judgment.precision_e2e_ok
    );

    if !judgment.all_passed() {
        eprintln!("tier_latency_bench: acceptance criteria not met (TASK-116 PLAN-4/6/7)");
        std::process::exit(1);
    }
}
