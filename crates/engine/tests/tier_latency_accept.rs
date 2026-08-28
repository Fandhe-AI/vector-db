//! `benches/harness/tier.rs`（TASK-116。対象ビヘイビア: `docs/spec/04-behavior/
//! query-planning.md` PLAN-4, PLAN-6, PLAN-7）の回帰テスト。
//!
//! `tier_latency_bench.rs` は時間依存・常駐 Ollama 前提のためこのテストからは
//! 実行しない（`tests/c1_bench_accept.rs`・`tests/bench_accept.rs` と同様、実測
//! タイマー・env に依存しない時間非依存の契約のみを `#[path]` で取り込み
//! `cargo test`（`make ci` 対象）で検証する）。
//!
//! `harness/tier.rs` 自体に `#[cfg(test)] mod tests` を置かない理由:
//! `harness/*.rs` は `#[path]` 経由で bench クレート（`tier_latency_bench.rs`。
//! `--test` フラグなしでコンパイル）とテストクレート（本ファイル）の双方に
//! 取り込まれる。bench コンパイル時は `#[test]` 項目が丸ごと除去されるため、
//! 同じ場所に `#[cfg(test)] mod tests { use super::*; ... }` を置くと
//! `use super::*;` が unused import になり `-D warnings` で失敗する
//! （`harness/ab.rs::median_ratio` のドキュメンテーションコメント参照。既存の
//! 確立パターン）。そのため `tier.rs` の単体テストは本ファイルへ集約する。
//!
//! 加えて、計測用質問（[`harness::tier::DIALOGUE_QUESTION`]／
//! [`harness::tier::PRECISION_QUESTION`]）が production の
//! `engine::tiering::classify` で意図どおり Dialogue／HighPrecision へ分類される
//! ことを固定する（PLAN-4 の「ティアを適用して実行する」前提の層 A 相当。計画
//! 「実装ステップ 4」参照）。LLM 呼び出し・実測タイマーを一切使わない決定的な
//! 回帰であり、`tier_latency_bench.rs` の routing 実証（設計方針 3）が正しい
//! 前提の上で動作していることを保証する。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;

use harness::tier::{
    build_corpus, build_ollama_client, judge, opt_in_requested, parse_host, parse_max_p95_ms,
    parse_model_name, parse_port, TierSamples, TierThresholds,
};

use std::time::Duration;

// --- opt_in_requested ---

#[test]
fn opt_in_requested_treats_unset_and_empty_as_false() {
    assert!(!opt_in_requested(None));
    assert!(!opt_in_requested(Some("")));
    assert!(!opt_in_requested(Some("   ")));
}

#[test]
fn opt_in_requested_treats_any_non_empty_value_as_true() {
    assert!(opt_in_requested(Some("1")));
    assert!(opt_in_requested(Some("true")));
    assert!(opt_in_requested(Some("0")));
}

// --- parse_max_p95_ms ---

#[test]
fn parse_max_p95_ms_accepts_positive_integer() {
    assert_eq!(
        parse_max_p95_ms("250", "VAR").unwrap(),
        Duration::from_millis(250)
    );
}

#[test]
fn parse_max_p95_ms_rejects_empty() {
    assert!(parse_max_p95_ms("", "VAR").is_err());
    assert!(parse_max_p95_ms("   ", "VAR").is_err());
}

#[test]
fn parse_max_p95_ms_rejects_zero() {
    assert!(parse_max_p95_ms("0", "VAR").is_err());
}

#[test]
fn parse_max_p95_ms_rejects_non_integer() {
    assert!(parse_max_p95_ms("1.5", "VAR").is_err());
    assert!(parse_max_p95_ms("abc", "VAR").is_err());
    assert!(parse_max_p95_ms("-5", "VAR").is_err());
}

// --- parse_port ---

#[test]
fn parse_port_accepts_valid_port() {
    assert_eq!(parse_port("11434", "VAR").unwrap(), 11434);
}

#[test]
fn parse_port_rejects_out_of_range_or_empty() {
    assert!(parse_port("70000", "VAR").is_err());
    assert!(parse_port("", "VAR").is_err());
}

// --- parse_host ---

#[test]
fn parse_host_rejects_empty() {
    assert!(parse_host("", "VAR").is_err());
    assert!(parse_host("   ", "VAR").is_err());
}

#[test]
fn parse_host_trims_and_accepts() {
    assert_eq!(parse_host(" 127.0.0.1 ", "VAR").unwrap(), "127.0.0.1");
}

// --- parse_model_name ---

#[test]
fn parse_model_name_rejects_empty() {
    assert!(parse_model_name("", "VAR").is_err());
    assert!(parse_model_name("   ", "VAR").is_err());
}

#[test]
fn parse_model_name_trims_and_accepts() {
    assert_eq!(parse_model_name(" llama3 ", "VAR").unwrap(), "llama3");
}

// --- build_ollama_client ---

#[test]
fn build_ollama_client_rejects_non_loopback_host() {
    assert!(build_ollama_client("203.0.113.5", 11434, "llama3").is_err());
}

#[test]
fn build_ollama_client_accepts_loopback_host() {
    assert!(build_ollama_client("127.0.0.1", 11434, "llama3").is_ok());
}

// --- build_corpus ---

#[test]
fn build_corpus_is_deterministic_for_a_fixed_seed() {
    let a = build_corpus(8, 4, 42);
    let b = build_corpus(8, 4, 42);
    assert_eq!(a.len(), 8);
    for (ra, rb) in a.iter().zip(b.iter()) {
        assert_eq!(ra.id, rb.id);
        assert_eq!(ra.path, rb.path);
        assert_eq!(ra.body, rb.body);
        assert_eq!(ra.embedding, rb.embedding);
    }
}

// --- judge ---

fn passing_samples() -> TierSamples {
    TierSamples {
        dialogue_expansion: vec![Duration::from_millis(10); 20],
        dialogue_e2e: vec![Duration::from_millis(30); 20],
        precision_expansion: vec![Duration::from_millis(50); 20],
        precision_e2e: vec![Duration::from_millis(90); 20],
        dialogue_routing_matched: true,
        precision_routing_matched: true,
    }
}

fn generous_thresholds() -> TierThresholds {
    TierThresholds {
        dialogue_expansion_max_p95: Duration::from_millis(20),
        dialogue_e2e_max_p95: Duration::from_millis(50),
        precision_expansion_max_p95: Duration::from_millis(80),
        precision_e2e_max_p95: Duration::from_millis(150),
    }
}

#[test]
fn judge_reports_pass_when_all_within_limits_and_routing_matches() {
    let judgment = judge(&passing_samples(), &generous_thresholds()).expect("non-empty samples");
    assert!(judgment.all_passed());
}

#[test]
fn judge_fails_when_routing_mismatched_even_if_latency_within_limits() {
    let mut samples = passing_samples();
    samples.dialogue_routing_matched = false;
    let judgment = judge(&samples, &generous_thresholds()).expect("non-empty samples");
    assert!(!judgment.all_passed());
    assert!(!judgment.dialogue_routing_matched);
}

#[test]
fn judge_fails_when_p95_exceeds_limit() {
    let mut samples = passing_samples();
    samples.dialogue_expansion = vec![Duration::from_millis(100); 20];
    let judgment = judge(&samples, &generous_thresholds()).expect("non-empty samples");
    assert!(!judgment.all_passed());
    assert!(!judgment.dialogue_expansion_ok);
}

#[test]
fn judge_rejects_empty_samples() {
    let mut samples = passing_samples();
    samples.dialogue_expansion = vec![];
    assert!(judge(&samples, &generous_thresholds()).is_err());
}

// --- 層A相当: 計測質問の production classify() 結果を固定 --------------------

mod routing {
    use super::harness::tier::{DIALOGUE_QUESTION, PRECISION_QUESTION};
    use engine::dictionary::{Dictionary, FileTree};
    use engine::tiering::{
        classify, NormalizedDictionaryIndex, QuestionClass, Tier, TieringCriteria,
    };
    use std::collections::{BTreeMap, BTreeSet};

    /// 空辞書（コーパス内容に依存しない routing。`harness::tier` モジュール
    /// ドキュメント「計測質問」参照）。実際の計測（`tier_latency_bench.rs`）は
    /// 実コーパスから構築した辞書スナップショットを使うが、選定した計測質問は
    /// 辞書内容に依存しないパス拡張子一致・手掛かり語一致のみで判定されるため、
    /// 空辞書でも同じ分類結果になることをこのテストで固定する。
    fn empty_dictionary() -> Dictionary {
        Dictionary {
            symbols: BTreeSet::new(),
            file_tree: FileTree {
                paths: BTreeSet::new(),
                by_extension: BTreeMap::new(),
                by_top_dir: BTreeMap::new(),
            },
            term_index: BTreeMap::new(),
            truncated: false,
        }
    }

    #[test]
    fn dialogue_question_classifies_as_direct_dialogue_tier() {
        let dict = empty_dictionary();
        let index = NormalizedDictionaryIndex::build(&dict);
        let criteria = TieringCriteria::default();
        let result = classify(DIALOGUE_QUESTION, &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
    }

    #[test]
    fn precision_question_classifies_as_abstraction_high_precision_tier() {
        let dict = empty_dictionary();
        let index = NormalizedDictionaryIndex::build(&dict);
        let criteria = TieringCriteria::default();
        let result = classify(PRECISION_QUESTION, &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
    }
}

// --- e2e: `using_plan_statement` の出力が `EngineCore::execute_sql` に受理される
//     ことを固定する（時間非依存・スタブ `LlmClient`）。実測（`tier_latency_bench.rs`）
//     が実行するのと同一の文字列生成関数を経由するため、この e2e が pass すれば
//     実測側の SQL 構築も許可リスト・スキーマ束縛を通ることが保証される
//     （`tests/c1_bench_accept.rs::e2e` と同一パターン）。`seeded_core` は
//     `tier_latency_bench.rs` が実際に使う `EngineCore::with_tiered_query_planner`
//     （`PlannerBinding::Tiered`）で構築し、ティア構成特有の分岐
//     （`TieredPlanner::select` の呼び出し）もこの e2e で通す。

mod e2e {
    use super::harness::tier::{
        build_corpus, using_plan_statement, DIALOGUE_QUESTION, PRECISION_QUESTION,
    };
    use engine::catalog::{ColumnDef, ColumnType, TableSchema};
    use engine::core::EngineCore;
    use engine::embedding::HashingEmbedder;
    use engine::kernel::CpuScalarProvider;
    use engine::policy::PolicyContext;
    use engine::query_planner::{LlmClient, PlanError};
    use engine::recovery::required_op_id::OperationId;
    use engine::row_codec::Value;
    use engine::storage::{Storage, Visibility};
    use engine::tiering::TieringCriteria;

    use super::temp_db::{unique_db_path, CleanupGuard};

    const TABLE: &str = "tier_bench_docs";
    const DIM: u32 = 4;
    const TOP_K: usize = 3;
    const TENANT_ID: &str = "bench-tenant";

    /// `tier_latency_bench.rs::schema` と同一の列構成（`embedding VECTOR`・
    /// `path TEXT`・`body TEXT`）。両ファイルが将来ドリフトした場合に検知できる
    /// よう、列名・型はここで独立に再定義する（`c1_bench_accept.rs::e2e` と
    /// 同じ流儀。テストが production の実際のスキーマではなく想定スキーマを
    /// 固定化するだけの無意味な自己参照テストにならないようにする）。
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

    /// 固定の展開結果を返すスタブ（`tests/sql_using_plan.rs::StubLlmClient` と
    /// 同一パターン。実 Ollama 疎通は対象外）。
    struct StubLlmClient;

    impl LlmClient for StubLlmClient {
        fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
            Ok(
                r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#
                    .to_string(),
            )
        }
    }

    fn seeded_core(path: &std::path::Path) -> (EngineCore, PolicyContext) {
        let storage = Storage::open(path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let ctx = PolicyContext::new(TENANT_ID).expect("valid tenant id");

        for row in build_corpus(4, DIM as usize, 1) {
            let op_id = OperationId::parse(&format!("tier-e2e-seed-{}", row.id))
                .expect("valid operation_id");
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

        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
            .with_tiered_query_planner(
                Box::new(StubLlmClient),
                Box::new(StubLlmClient),
                TieringCriteria::default(),
            );
        (core, ctx)
    }

    #[test]
    fn dialogue_using_plan_statement_is_accepted_by_execute_sql() {
        let path = unique_db_path("tier-latency-accept-e2e-dialogue");
        let _guard = CleanupGuard(path.clone());
        let (core, ctx) = seeded_core(&path);

        let sql = using_plan_statement(TABLE, DIALOGUE_QUESTION, TOP_K);
        core.execute_sql(&ctx, &sql)
            .expect("USING PLAN dispatch must accept the dialogue-tier measurement statement");
    }

    #[test]
    fn precision_using_plan_statement_is_accepted_by_execute_sql() {
        let path = unique_db_path("tier-latency-accept-e2e-precision");
        let _guard = CleanupGuard(path.clone());
        let (core, ctx) = seeded_core(&path);

        let sql = using_plan_statement(TABLE, PRECISION_QUESTION, TOP_K);
        core.execute_sql(&ctx, &sql)
            .expect("USING PLAN dispatch must accept the precision-tier measurement statement");
    }
}
