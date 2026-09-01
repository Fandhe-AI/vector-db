//! hybrid_rrf クエリの段別内訳プロファイルベンチ（Issue #356。親 Issue #355・
//! ポインタ: `docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3）。
//!
//! Issue #355 は「疎索引（`SparseIndex`）がクエリ毎に再構築され、これが
//! `hybrid_rrf` の主要コストと推定される」という調査結果を記録しているが、
//! 定量的な内訳切り分けは未実施だった。本ベンチは以下の段を実測し、後続の
//! Issue #357（`SparseIndex` のテーブル世代整合キャッシュ設計）が「どの段を
//! キャッシュ対象にすべきか」を判断できる分解能を提供する:
//!
//! 1. `sql_hybrid` / `sql_dense_knn`: `EngineCore::execute_sql` 経由の hybrid_rrf
//!    クエリと密 KNN のみのクエリの対照（SQL パース・束縛・テーブル走査を含む
//!    エンドツーエンドの差分の上限）
//! 2. `collect_body_strings`: 本文 String 収集（`sql/exec.rs:370-509` の
//!    `sparse_docs: Vec<(u64, String)>` 蓄積）の近似下限
//! 3. `sparse_build_total`: `SparseIndex::build` 単体
//! 4. `tokenize_only` / `tokenize_term_freq` / `tokenize_term_doc_freq`: build 内部の
//!    tokenize / term_freq 構築 / doc_freq マージの累積 3 段（複製実装。
//!    `harness::hybrid_profile` モジュールドキュメント「複製近似の限界」参照）
//!
//! # 実測値の比較可能性についての重要な注意
//!
//! `harness::hybrid_profile` モジュールドキュメント参照: Issue #355 が言及する
//! `feature_bench.rs` はこのリポジトリの履歴に存在しない。本ベンチのコーパスは
//! 新規に組み立てたものであり、**実測 ms は Issue #355 の 288ms と直接比較可能
//! ではない**。本ベンチが答える問いは「どの段が支配的か」という相対的な内訳の
//! 分解能であり、絶対値の再現ではない。
//!
//! # 出力規約
//!
//! spec 由来の pass/fail 閾値を持たない情報提供専用ベンチ（`hybrid_latency_bench.rs`
//! と同方針）。実測値は常に標準出力する（実測値の公開はオーナー判断 2026-08-29 で
//! 許可済み。spec 閾値の注入・表示は行わないため逆算リスクなし）。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make
//! bench-hybrid-profile` からの手動実行専用）。`GITHUB_ACTIONS` 環境変数が
//! 設定されていれば起動直後に fail-closed で拒否する
//! （`harness::hybrid_profile::refuse_under_github_actions`）。
//!
//! 判定ロジック自体（時間非依存）は `harness::hybrid_profile` にあり
//! `tests/hybrid_profile_accept.rs` で `make ci` 側から回帰検証する。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::hybrid_profile::{
    build_actually_succeeds, collect_body_strings, generate_corpus, generate_queries,
    refuse_under_github_actions, render_stage_line, sql_dense_statement, sql_hybrid_statement,
    tokenize_only, tokenize_term_doc_freq, tokenize_term_freq,
};
use harness::protocol::{run, MeasurementConfig};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine;
use engine::sparse::SparseIndex;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// コーパス規模（Issue #356 本文が言及する feature_bench の行数感に合わせた、
/// 本ベンチ独自の定数。spec 由来の値ではない）。
const NUM_DOCS: usize = 25_000;
const DIM: usize = 128;
const TOP_K: usize = 10;
const NUM_QUERIES: usize = 5;
const SEED: u64 = 0x3562_3562_3562_3562;

const TABLE: &str = "docs";
const VECTOR_COLUMN: &str = "embedding";
const TEXT_COLUMN: &str = "body";
const TENANT_ID: &str = "hybrid-profile-tenant";
/// 単一 write トランザクションの確保量を有界化するための投入チャンクサイズ
/// （`sql_c1_bench.rs::SEED_BATCH_ROWS` と同一方針）。
const SEED_BATCH_ROWS: usize = 5_000;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("hybrid_profile_bench: {msg}");
    std::process::exit(1);
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "hybrid_profile_bench: measures the stage-by-stage breakdown of hybrid_rrf query \
         latency (Issue #356; corpus is freshly authored, NOT a reproduction of Issue #355's \
         feature_bench figures — see docs/design/hybrid-rrf-latency-breakdown.md). Not a \
         pass/fail gate."
    );

    // --- コーパス生成（密ベクトル・疎本文とも決定的） ---
    let corpus = generate_corpus(SEED, NUM_DOCS, DIM)
        .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));
    let queries = generate_queries(SEED, NUM_QUERIES, DIM);

    // 複製実装（tokenize/term_freq/doc_freq の累積 3 段）の構造的整合性チェック
    // （`harness::hybrid_profile::build_actually_succeeds` ドキュメント参照。
    // 複製実装に転記ミスがあり `SparseIndex::build` 自体が失敗する入力を
    // 生成してしまっている場合、ここで即座に検知して打ち切る）。
    if !build_actually_succeeds(&corpus) {
        fail_closed(
            "SparseIndex::build failed for the generated corpus (replication integrity check)",
        );
    }

    // --- SQL 段用の一時 DB へ投入 ---
    let path = unique_db_path("issue356-hybrid-profile");
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
    let mut next_id: usize = 0;
    while next_id < NUM_DOCS {
        let batch_len = SEED_BATCH_ROWS.min(NUM_DOCS - next_id);
        let mut metadata_batch: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
        for i in next_id..next_id + batch_len {
            let encoded = encode_scalar_columns(
                &schema,
                &[Value::Null, Value::Text(corpus.bodies[i].clone())],
            )
            .expect("encode scalar columns for bench seeding");
            metadata_batch.push(encoded);
        }
        let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
            .map(|i| {
                let global = next_id + i;
                let start = global * DIM;
                (
                    corpus.ids[global],
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility: Visibility::Public,
                        embedding: &corpus.vectors[start..start + DIM],
                        metadata: &metadata_batch[i],
                    },
                )
            })
            .collect();
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
            "hybrid-profile-seed-batch-{next_id}"
        ))
        .expect("valid operation_id");
        engine::tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id)
            .expect("seed batch insert");
        next_id += batch_len;
    }

    let core = EngineCore::from_storage(storage, search_engine::default_engine());

    // 可視件数の突き合わせ（単一テナント・全行 Public の単純化構成のため、SQL 段
    // 〔`sql_hybrid`/`sql_dense_knn`〕の可視集合は投入行数と一致するはずである。
    // 不一致は構成ミス〔テーブル定義・投入経路の不整合〕を示すため fail-closed に
    // 打ち切る）。
    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let count_result = core
        .execute_sql(&ctx, &count_sql)
        .unwrap_or_else(|e| fail_closed(format!("COUNT(*) query failed: {e}")));
    let visible_count = match count_result.rows.first().and_then(|row| row.cells.first()) {
        Some(engine::sql::exec::Cell::Integer(n)) => *n,
        other => fail_closed(format!("unexpected COUNT(*) result shape: {other:?}")),
    };
    if visible_count != NUM_DOCS as u64 {
        fail_closed(format!(
            "visible row count mismatch: expected {NUM_DOCS}, got {visible_count} \
             (SQL-stage corpus and direct-API-stage corpus must cover the same rows)"
        ));
    }
    println!("hybrid_profile: visible_count={visible_count} rows={NUM_DOCS} dim={DIM}");

    let config = MeasurementConfig::new(20, 30, SEED).expect("protocol minimums satisfied");

    // --- SQL レベル: sql_hybrid / sql_dense_knn ---
    let mut query_idx = 0usize;
    let sql_hybrid_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        let sql =
            sql_hybrid_statement(TABLE, VECTOR_COLUMN, TEXT_COLUMN, &q.vector, &q.text, TOP_K)
                .unwrap_or_else(|e| fail_closed(format!("sql_hybrid_statement failed: {e}")));
        core.execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| fail_closed(format!("sql_hybrid execute_sql failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sql_hybrid measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&sql_hybrid_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "sql_hybrid",
            sql_hybrid_measurement.summary.median.as_micros(),
            p95.as_micros(),
            NUM_DOCS,
        )
    );

    let mut query_idx = 0usize;
    let sql_dense_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        let sql = sql_dense_statement(TABLE, VECTOR_COLUMN, &q.vector, TOP_K)
            .unwrap_or_else(|e| fail_closed(format!("sql_dense_statement failed: {e}")));
        core.execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| fail_closed(format!("sql_dense_knn execute_sql failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sql_dense_knn measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&sql_dense_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "sql_dense_knn",
            sql_dense_measurement.summary.median.as_micros(),
            p95.as_micros(),
            NUM_DOCS,
        )
    );

    // --- コンポーネントレベル（直接 API。SQL パース・テーブル走査を含まない） ---

    let collect_measurement = run(&config, || {
        collect_body_strings(&corpus.ids, &corpus.bodies)
    })
    .unwrap_or_else(|e| fail_closed(format!("collect_body_strings measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&collect_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "collect_body_strings",
            collect_measurement.summary.median.as_micros(),
            p95.as_micros(),
            NUM_DOCS,
        )
    );

    let doc_refs = corpus.sparse_docs();
    let build_measurement = run(&config, || {
        SparseIndex::build(&doc_refs)
            .unwrap_or_else(|e| fail_closed(format!("SparseIndex::build failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sparse_build_total measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&build_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "sparse_build_total",
            build_measurement.summary.median.as_micros(),
            p95.as_micros(),
            NUM_DOCS,
        )
    );

    let tokenize_measurement = run(&config, || tokenize_only(&corpus.bodies))
        .unwrap_or_else(|e| fail_closed(format!("tokenize_only measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&tokenize_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let tokenize_check = tokenize_only(&corpus.bodies);
    println!(
        "{}",
        render_stage_line(
            "tokenize_only",
            tokenize_measurement.summary.median.as_micros(),
            p95.as_micros(),
            tokenize_check,
        )
    );

    let term_freq_measurement = run(&config, || tokenize_term_freq(&corpus.bodies))
        .unwrap_or_else(|e| fail_closed(format!("tokenize_term_freq measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&term_freq_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let term_freq_check = tokenize_term_freq(&corpus.bodies);
    println!(
        "{}",
        render_stage_line(
            "tokenize_term_freq",
            term_freq_measurement.summary.median.as_micros(),
            p95.as_micros(),
            term_freq_check,
        )
    );

    let term_doc_freq_measurement = run(&config, || tokenize_term_doc_freq(&corpus.bodies))
        .unwrap_or_else(|e| fail_closed(format!("tokenize_term_doc_freq measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&term_doc_freq_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let term_doc_freq_check = tokenize_term_doc_freq(&corpus.bodies);
    println!(
        "{}",
        render_stage_line(
            "tokenize_term_doc_freq",
            term_doc_freq_measurement.summary.median.as_micros(),
            p95.as_micros(),
            term_doc_freq_check,
        )
    );

    println!(
        "hybrid_profile: check_values tokenize_total_tokens={tokenize_check} \
         tokenize_term_freq_total_unique_terms={term_freq_check} \
         tokenize_term_doc_freq_vocab_size={term_doc_freq_check}"
    );
    println!(
        "hybrid_profile: done (see docs/design/hybrid-rrf-latency-breakdown.md for the \
         attribution table transcribed from this run's output)"
    );
}
