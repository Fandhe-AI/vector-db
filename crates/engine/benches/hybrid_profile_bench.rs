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
//! Issue #387 は、キャッシュヒット後（Issue #357）になお残る `search_within`
//! 単体コストと疎側再取得ループの寄与を切り分けるため、以下を追加する:
//!
//! 5. `hybrid_search_cached_index`: 事前構築済み `SparseIndex`（キャッシュヒット
//!    相当）を使った `hybrid::hybrid_search` の直接呼び出し。密・疎双方の再取得
//!    発火回数（`sparse_refetch`／`provider_calls_max`）を併記する
//! 6. `search_within_fetch_k=<k>`: `SparseIndex::search_within` 単体を、疎側再取得
//!    スケジュール上で実際に呼ばれる `fetch_k` ごとに実測する
//! 7. `search_within_subset_only` / `search_within_subset_df` /
//!    `search_within_replica_full`: `search_within` 内部の可視 subset 構築／df 再
//!    計算パス／スコアリングパスの累積 3 区間（複製実装。起動時に実 API の出力と
//!    数値一致するかを fail-closed 検証してから使う）
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

use std::collections::BTreeSet;

use harness::env_report::EnvReport;
use harness::hybrid_latency::RefetchTrackingProvider;
use harness::hybrid_profile::{
    build_actually_succeeds, collect_body_strings, dense_refetch_schedule, fetch_cap,
    generate_corpus, generate_queries, initial_fetch_k, refetch_schedule_matches_observed_calls,
    refuse_under_github_actions, render_dense_refetch_line, render_sparse_refetch_line,
    render_sparse_refetch_summary_line, render_stage_line, replica_matches_real,
    sparse_refetch_schedule, sql_dense_statement, sql_hybrid_statement, summarize_sparse_refetch,
    tokenize_only, tokenize_term_doc_freq, tokenize_term_freq,
    verify_sparse_schedule_terminal_is_stable, ProfileSparseIndex, SQL_DEFAULT_HYBRID_POOL_DEPTH,
};
use harness::protocol::{run, run_bounded_retain, MeasurementConfig};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
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

    // --- Issue #387: キャッシュヒット後（Issue #357）になお残る search_within ---
    // --- 単体コスト・疎側再取得ループの寄与 -----------------------------------

    let sparse_index = SparseIndex::build(&doc_refs)
        .unwrap_or_else(|e| fail_closed(format!("SparseIndex::build (Issue #387) failed: {e}")));
    let replica = ProfileSparseIndex::build(&doc_refs)
        .unwrap_or_else(|e| fail_closed(format!("ProfileSparseIndex::build failed: {e}")));
    let visible: BTreeSet<u64> = corpus.ids.iter().copied().collect();
    let pool_depth = SQL_DEFAULT_HYBRID_POOL_DEPTH;
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, pool_depth)
        .unwrap_or_else(|e| fail_closed(format!("RrfConfig::new failed: {e:?}")));

    // 忠実性検証（fail-closed）: 疎側再取得スケジュール上で実際に呼ばれる各
    // fetch_k について、複製（ProfileSparseIndex）の出力が実 SparseIndex の
    // 出力と数値一致することを起動時に確認する（`harness::hybrid_profile`
    // モジュールドキュメント「複製固有の限界」参照）。
    let mut sparse_schedules = Vec::with_capacity(queries.len());
    for q in &queries {
        let schedule = sparse_refetch_schedule(&sparse_index, &q.text, &visible, pool_depth)
            .unwrap_or_else(|e| fail_closed(format!("sparse_refetch_schedule failed: {e}")));
        for &fetch_k in &schedule.fetch_ks {
            replica_matches_real(&sparse_index, &replica, &q.text, fetch_k, &visible)
                .unwrap_or_else(|e| fail_closed(format!("replica fidelity check failed: {e}")));
        }
        // 終端固定点検証（codex-review P1 指摘対応。PR #416）: `schedule` の終端
        // 判定（`boundary_tie_decision` 複製）が実 `search_within` 出力でも
        // 安定していることを、予測終端の 1 段先まで実際に呼んで確認する
        // （`verify_sparse_schedule_terminal_is_stable` ドキュメント参照。
        // 疎側呼び出し回数そのものの外部観測ではなく終端判定の間接検証にとどまる
        // 限界は同ドキュメントに明記）。
        let stable = verify_sparse_schedule_terminal_is_stable(
            &sparse_index,
            &q.text,
            &visible,
            &schedule,
            pool_depth,
        )
        .unwrap_or_else(|e| {
            fail_closed(format!(
                "sparse schedule terminal stability check failed: {e}"
            ))
        });
        if !stable {
            fail_closed(
                "sparse refetch schedule terminal fetch_k is not a stable fixed point against \
                 real search_within output one round further (boundary_tie_decision replica may \
                 have diverged from what the real index data supports)",
            );
        }
        sparse_schedules.push(schedule);
    }

    // 密側の忠実性検証: 複製予測（dense_refetch_schedule）の呼び出し回数と、
    // 実 hybrid_search 呼び出し時に RefetchTrackingProvider が観測した回数を
    // 突き合わせる。
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    for (idx, q) in queries.iter().enumerate() {
        let predicted = dense_refetch_schedule(
            &provider,
            &corpus.ids,
            &corpus.vectors,
            corpus.dim,
            &q.vector,
            pool_depth,
        )
        .unwrap_or_else(|e| fail_closed(format!("dense_refetch_schedule failed: {e}")));
        provider.reset();
        let input = SearchInput {
            ids: &corpus.ids,
            vectors: &corpus.vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(&provider, input, &sparse_index, &q.text, TOP_K, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search (fidelity pass) failed: {e}")));
        refetch_schedule_matches_observed_calls(idx, &predicted, provider.calls())
            .unwrap_or_else(|e| fail_closed(format!("dense refetch fidelity check failed: {e}")));
    }
    println!(
        "hybrid_profile: fidelity checks passed (replica search_within matches real API for \
         every fetch_k on the sparse schedule; each sparse schedule's terminal fetch_k was \
         verified as a stable fixed point one round further via a real search_within call \
         [verify_sparse_schedule_terminal_is_stable]; dense refetch schedule predictions match \
         observed hybrid_search calls via RefetchTrackingProvider — sparse still has no \
         equivalent internal-call-count observation hook [production diagnostic hook rejected, \
         see docs/design/hybrid-rrf-latency-breakdown.md 限界・申し送り], so \
         sparse_refetch_schedule's boundary_tie_decision-driven loop remains a reproduction \
         cross-checked for output fidelity and terminal stability, not a call-count \
         cross-check against real hybrid_search)"
    );

    // --- hybrid_search_cached_index: 事前構築済み SparseIndex（キャッシュヒット
    // 相当）を使った直接 API 呼び出し。計測区間（timed pass）は素の
    // `ParallelSearchProvider` を使う（`RefetchTrackingProvider` は呼び出しの
    // たびに atomic な呼び出し回数・最大 k 更新を行うため、計測区間へ混ぜると
    // p95/median にその分のオーバーヘッドが混入する。codex-review 指摘・
    // Issue #387 PR #416）。呼び出し回数・最大 k の統計は別パス（stats pass。
    // `hybrid_latency_bench.rs::measure_stage` と同じ「計測区間内では統計蓄積を
    // 行わない」方針）で `RefetchTrackingProvider` を使って集計する。
    let timed_provider = ParallelSearchProvider;
    let mut query_idx = 0usize;
    let hybrid_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        let input = SearchInput {
            ids: &corpus.ids,
            vectors: &corpus.vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(&timed_provider, input, &sparse_index, &q.text, TOP_K, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search (timed) failed: {e}")))
    })
    .unwrap_or_else(|e| {
        fail_closed(format!(
            "hybrid_search_cached_index measurement failed: {e}"
        ))
    });
    let p95 = harness::accept::p95_from_samples(&hybrid_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));

    let mut dense_stats = Vec::with_capacity(queries.len());
    for q in &queries {
        provider.reset();
        let input = SearchInput {
            ids: &corpus.ids,
            vectors: &corpus.vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(&provider, input, &sparse_index, &q.text, TOP_K, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search (stats pass) failed: {e}")));
        dense_stats.push(harness::hybrid_latency::aggregate_refetch_stats(
            provider.calls(),
            provider.max_k_seen(),
            corpus.ids.len(),
        ));
    }
    let dense_summary = harness::hybrid_latency::summarize_refetch_stats(&dense_stats);
    println!(
        "{}",
        render_dense_refetch_line(
            "hybrid_search_cached_index",
            hybrid_measurement.summary.median.as_micros(),
            p95.as_micros(),
            &dense_summary,
        )
    );

    // --- 疎側再取得スケジュールの出力 ---
    for (idx, schedule) in sparse_schedules.iter().enumerate() {
        println!("{}", render_sparse_refetch_line(idx, schedule));
    }
    let sparse_summary = summarize_sparse_refetch(&sparse_schedules);

    // --- search_within_fetch_k=<k>: 疎側再取得スケジュール上で実際に呼ばれる
    // 各 fetch_k について、実 search_within 単体を round-robin クエリで実測する。
    let mut union_fetch_ks: Vec<usize> = sparse_schedules
        .iter()
        .flat_map(|s| s.fetch_ks.iter().copied())
        .collect();
    union_fetch_ks.sort_unstable();
    union_fetch_ks.dedup();

    let mut median_by_fetch_k: std::collections::BTreeMap<usize, u128> =
        std::collections::BTreeMap::new();
    for &fetch_k in &union_fetch_ks {
        let mut query_idx = 0usize;
        let measurement = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            sparse_index
                .search_within(&q.text, fetch_k, &visible)
                .unwrap_or_else(|e| {
                    fail_closed(format!("search_within(fetch_k={fetch_k}) failed: {e}"))
                })
        })
        .unwrap_or_else(|e| {
            fail_closed(format!(
                "search_within_fetch_k={fetch_k} measurement failed: {e}"
            ))
        });
        let p95 = harness::accept::p95_from_samples(&measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
        let check = sparse_index
            .search_within(&queries[0].text, fetch_k, &visible)
            .map(|hits| hits.len())
            .unwrap_or(0);
        median_by_fetch_k.insert(fetch_k, measurement.summary.median.as_micros());
        println!(
            "{}",
            render_stage_line(
                &format!("search_within_fetch_k={fetch_k}"),
                measurement.summary.median.as_micros(),
                p95.as_micros(),
                check,
            )
        );
    }

    // 推定累積時間（codex-review P1 指摘対応。PR #416）: `median_by_fetch_k[k]`
    // は各 fetch_k を全クエリで round-robin 測定した**全クエリ混合集団**の実測
    // 中央値であり、特定クエリの実測値ではない。以下はその混合集団中央値を、
    // 最も再取得回数が多いクエリの実スケジュールに沿って合算した**推定値**
    // （クエリ別の真の累積コストの実測ではない。以前は「最悪ケース」＝
    // クエリ別実測であるかのように扱っていたが、実体は全クエリ混合中央値に
    // よる推定である。詳細は `render_sparse_refetch_summary_line` ドキュメント
    // 参照）。
    let estimated_worst_cumulative_mixed_median_us: u128 = sparse_schedules
        .iter()
        .map(|s| {
            s.fetch_ks
                .iter()
                .map(|k| median_by_fetch_k.get(k).copied().unwrap_or(0))
                .sum::<u128>()
        })
        .max()
        .unwrap_or(0);
    println!(
        "{}",
        render_sparse_refetch_summary_line(
            &sparse_summary,
            estimated_worst_cumulative_mixed_median_us
        )
    );

    // --- search_within 内部 3 区間（複製実装。初期 fetch_k と最終 fetch_k の
    // 2 点で計測する）---
    let cap = fetch_cap(visible.len());
    let initial_k = initial_fetch_k(pool_depth, cap);
    let final_k = union_fetch_ks.last().copied().unwrap_or(initial_k);
    for &fetch_k in &[initial_k, final_k] {
        let mut query_idx = 0usize;
        let subset_only_measurement = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            replica.subset_only(&q.text, &visible)
        })
        .unwrap_or_else(|e| {
            fail_closed(format!("search_within_subset_only measurement failed: {e}"))
        });
        let p95 = harness::accept::p95_from_samples(&subset_only_measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
        println!(
            "{}",
            render_stage_line(
                &format!("search_within_subset_only k={fetch_k}"),
                subset_only_measurement.summary.median.as_micros(),
                p95.as_micros(),
                visible.len(),
            )
        );

        let mut query_idx = 0usize;
        let subset_df_measurement = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            replica.subset_df(&q.text, &visible)
        })
        .unwrap_or_else(|e| {
            fail_closed(format!("search_within_subset_df measurement failed: {e}"))
        });
        let p95 = harness::accept::p95_from_samples(&subset_df_measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
        println!(
            "{}",
            render_stage_line(
                &format!("search_within_subset_df k={fetch_k}"),
                subset_df_measurement.summary.median.as_micros(),
                p95.as_micros(),
                visible.len(),
            )
        );

        let mut query_idx = 0usize;
        let (replica_full_measurement, _retained) = run_bounded_retain(&config, 0, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            replica.search_within_replica(&q.text, fetch_k, &visible)
        })
        .unwrap_or_else(|e| {
            fail_closed(format!(
                "search_within_replica_full measurement failed: {e}"
            ))
        });
        let p95 = harness::accept::p95_from_samples(&replica_full_measurement.samples)
            .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
        let check = replica
            .search_within_replica(&queries[0].text, fetch_k, &visible)
            .len();
        println!(
            "{}",
            render_stage_line(
                &format!("search_within_replica_full k={fetch_k}"),
                replica_full_measurement.summary.median.as_micros(),
                p95.as_micros(),
                check,
            )
        );
    }

    println!(
        "hybrid_profile: done (see docs/design/hybrid-rrf-latency-breakdown.md for the \
         attribution table transcribed from this run's output)"
    );
}
