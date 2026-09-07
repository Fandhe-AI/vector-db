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
//! Issue #389（転置索引・doc_len／doc_ids 配列の追加）は受け入れ条件として
//! メモリ増分の実測・記録を求めるため、以下を追加する:
//!
//! 8. `memory stage=sparse_index_resident`: `SparseIndex` を保持したままの
//!    VmRSS 増分（`/proc/self/status`。読めない環境では `unavailable`）と
//!    `approx_heap_bytes()` の実測値
//!
//! Issue #392 は疎側再取得ループの再スコアリング回避（`SparseIndex::
//! score_within`／`SparseScored::top`）の効果を実測するため、以下を追加する:
//!
//! 9. `score_within_once`: `SparseIndex::score_within` 単体（クエリ 1 回分の
//!    スコア計算のみ。Top-k 選出を含まない）
//! 10. `sparse_refetch_loop`: `engine::hybrid::sparse_refetch_observed`
//!     （`hybrid_search_boosted` の疎側再取得ループ本体そのもの。Issue #387
//!     PR #416 で production 経路と共有化済み）を round-robin クエリで
//!     直接計測した、疎側再取得ループの実測累積コスト。既存の `search_within_
//!     fetch_k=<k>`＋`sparse_refetch_summary`（複数ラウンドの medianを合算
//!     した推定値）を実測で補完する
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
    bucket_diff, collect_body_strings, dense_refetch_schedule, expected_visible_count, fetch_cap,
    generate_corpus, generate_queries, initial_fetch_k, refetch_schedule_matches_observed_calls,
    refuse_under_github_actions, render_baseline_bucket_line, render_dense_refetch_line,
    render_sparse_refetch_line, render_sparse_refetch_summary_line, render_stage_line,
    replica_matches_real, resolve_rows_from_env, resolve_visible_ratio_denominator_from_env,
    select_visible_ids, sparse_refetch_schedule, sql_dense_statement,
    sql_dense_statement_with_projection, sql_hybrid_statement,
    sql_hybrid_statement_with_projection, summarize_sparse_refetch, tokenize_only,
    tokenize_term_doc_freq, tokenize_term_freq, HybridProjection, ProfileSparseIndex,
    SQL_DEFAULT_HYBRID_POOL_DEPTH,
};
use harness::protocol::{run, run_bounded_retain, MeasurementConfig};
use harness::scan_stage_profile::{
    classify_against_bands, median_of, min_of, parse_rounds, reference_band, step_ratio_pct,
    ScanStageError,
};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hybrid::{hybrid_search, rrf_fuse, sparse_refetch_observed, RrfConfig};
use engine::kernel::{CandidateHit, CpuScalarProvider, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine;
use engine::sparse::{ScoredDoc, SparseIndex};
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// コーパス規模の既定値（Issue #356 本文が言及する feature_bench の行数感に
/// 合わせた、本ベンチ独自の値。spec 由来の値ではない）。Issue #547 で
/// `BENCH_HYBRID_PROFILE_ROWS`（既定 25,000・後方互換）による opt-in 可変化。
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

    // --- 行数・可視率の opt-in（Issue #547。#546〔PR #565〕のスコアアキュムレータ
    // 再利用が「索引 N ≫ 可視集合」条件で効くかを検証するための注入点。
    // `harness::hybrid_profile` モジュールドキュメント「可視率の意味」参照） ---
    let num_docs = resolve_rows_from_env().unwrap_or_else(|e| fail_closed(e.to_string()));
    let visible_ratio_denominator =
        resolve_visible_ratio_denominator_from_env().unwrap_or_else(|e| fail_closed(e.to_string()));
    let expected_visible = expected_visible_count(num_docs, visible_ratio_denominator)
        .unwrap_or_else(|e| fail_closed(e.to_string()));

    // --- コーパス生成（密ベクトル・疎本文とも決定的） ---
    let corpus = generate_corpus(SEED, num_docs, DIM)
        .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));
    let queries = generate_queries(SEED, NUM_QUERIES, DIM);

    let doc_refs = corpus.sparse_docs();

    // 可視行 id 集合（SQL 段は RLS の正規経路で、直接 API 段は明示的な部分集合
    // として、同じ規則〔`doc_id % denominator == 0`〕をどちらも使う。索引は
    // 常に全件〔`doc_refs`／`SparseIndex::build`〕から構築し、可視率が縮小
    // させるのは「クエリが渡す候補集合」のみである点が Issue #547 の測定条件）。
    let visible_row_ids: BTreeSet<u64> = select_visible_ids(num_docs, visible_ratio_denominator);
    // 直接 API 段（B0s/B0/B4/B5/B8）が `SearchInput`／`search_within` へ渡す
    // 可視部分集合。`corpus.ids`（0 始まり連番）と同じ並び順を保つため、
    // フィルタだけで id・vector・body が引き続き位置対応する。
    let visible_ids: Vec<u64> = corpus
        .ids
        .iter()
        .copied()
        .filter(|id| visible_row_ids.contains(id))
        .collect();
    let visible_vectors: Vec<f32> = corpus
        .ids
        .iter()
        .enumerate()
        .filter(|(_, id)| visible_row_ids.contains(id))
        .flat_map(|(i, _)| corpus.vectors[i * DIM..(i + 1) * DIM].iter().copied())
        .collect();
    let visible_bodies: Vec<String> = corpus
        .ids
        .iter()
        .enumerate()
        .filter(|(_, id)| visible_row_ids.contains(id))
        .map(|(i, _)| corpus.bodies[i].clone())
        .collect();
    if visible_ids.len() != expected_visible {
        fail_closed(format!(
            "select_visible_ids produced {} ids, expected {expected_visible} \
             (rows={num_docs} denominator={visible_ratio_denominator})",
            visible_ids.len()
        ));
    }

    // --- Issue #389: SparseIndex 常駐時の常駐メモリ（RSS）増分 -----------------
    // プロセス内でこれが最初かつ唯一の `SparseIndex::build` 呼び出しになるよう、
    // 複製実装（tokenize/term_freq/doc_freq の累積 3 段）の構造的整合性チェック
    // （`harness::hybrid_profile::build_actually_succeeds` ドキュメント参照）を
    // この 1 回の構築で兼務させる（codex-review 指摘・Cursor Bugbot 指摘・PR #424:
    // 整合性チェック用に別途 `SparseIndex::build` を呼んで破棄すると、その確保・
    // 解放でアロケータ／ページがウォームになり、続く RSS 計測が「未ウォーム状態
    // からの増分」にならず過小評価しうる。整合性チェックの目的〔複製実装の転記
    // ミス検出〕は「同一入力で `SparseIndex::build` 自体が成功するか」の確認に
    // 尽きるため、ここで構築したインデックスの `is_ok()` をそのままその判定に
    // 使い、計測対象としても保持し続ければ二重構築を避けられる）。また
    // `core.execute_sql` も未呼び出し、つまり `sql/sparse_cache.rs::
    // SparseIndexCache`（Issue #357）経由の構築機会もまだ無い時点で計測する
    // （そのため本計測は SQL 実行（テーブル作成・投入・COUNT(*)・hybrid/dense
    // いずれのクエリも含む）より前、コーパス生成直後に置く）。
    // `SparseIndex` を保持したまま前後の VmRSS を比較する（保持しなければ
    // drop されて増分を観測できない）。`approx_heap_bytes()` はテスト・
    // ベンチ以外の一般利用側（`sql/sparse_cache.rs`）が実際に参照する
    // 概算値であり、RSS 実測と並記することで概算の妥当性を突き合わせられる。
    let vm_rss_kb_before = harness::proc_stats::read_vm_rss_kb();
    let resident_index = SparseIndex::build(&doc_refs).unwrap_or_else(|e| {
        fail_closed(format!(
            "SparseIndex::build failed for the generated corpus \
             (replication integrity check via memory-measurement build): {e}"
        ))
    });
    let approx_heap_bytes = resident_index.approx_heap_bytes();
    let vm_rss_kb_after = harness::proc_stats::read_vm_rss_kb();
    let vm_hwm_kb = harness::proc_stats::read_vm_hwm_kb();
    println!(
        "{}",
        harness::hybrid_profile::render_memory_line(
            approx_heap_bytes,
            vm_rss_kb_before,
            vm_rss_kb_after,
            vm_hwm_kb,
        )
    );
    // `resident_index` は RSS 差分計測の対象そのものであり、以降の段では参照
    // しないため、計測直後に明示的に drop してよい（メモリ計測意図の明確化）。
    drop(resident_index);

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
    while next_id < num_docs {
        let batch_len = SEED_BATCH_ROWS.min(num_docs - next_id);
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
                let id = corpus.ids[global];
                // 可視率 100%（既定・`visible_ratio_denominator == 1`）では従来
                // どおり全行 Public。< 100% の条件は、RLS の正規経路（テナント内
                // `Visibility::Private` 行は `PolicyContext::new` の Public-only
                // ctx から不可視）で可視率を実現する（別テナント方式より
                // 「可視率」の意味に忠実。Issue #547 計画参照）。
                let visibility = if visible_row_ids.contains(&id) {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                (
                    id,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility,
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

    // 可視件数の突き合わせ（RLS 経由の `COUNT(*)` は可視行のみを数えるため、
    // 可視率 100% では投入行数と一致し、< 100% では `expected_visible_count`
    // と一致するはずである。不一致は構成ミス〔テーブル定義・投入経路・
    // 可視率規則の不整合〕を示すため fail-closed に打ち切る）。
    let count_sql = format!("SELECT COUNT(*) FROM {TABLE}");
    let count_result = core
        .execute_sql(&ctx, &count_sql)
        .unwrap_or_else(|e| fail_closed(format!("COUNT(*) query failed: {e}")));
    let visible_count = match count_result.rows.first().and_then(|row| row.cells.first()) {
        Some(engine::sql::exec::Cell::Integer(n)) => *n,
        other => fail_closed(format!("unexpected COUNT(*) result shape: {other:?}")),
    };
    if visible_count != expected_visible as u64 {
        fail_closed(format!(
            "visible row count mismatch: expected {expected_visible}, got {visible_count} \
             (rows={num_docs} denominator={visible_ratio_denominator})"
        ));
    }
    println!(
        "hybrid_profile: rows={num_docs} visible_ratio=1/{visible_ratio_denominator} \
         visible_count={visible_count} dim={DIM}"
    );

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
            num_docs,
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
            num_docs,
        )
    );

    // --- コンポーネントレベル（直接 API。SQL パース・テーブル走査を含まない） ---

    // `sql/exec.rs::on_visible_row` は可視行の本文のみ蓄積するため、本複製も
    // 可視部分集合（`visible_ids`/`visible_bodies`）だけを対象にする（Issue #547。
    // `harness::hybrid_profile` モジュールドキュメント「可視率の意味」参照）。
    let collect_measurement = run(&config, || {
        collect_body_strings(&visible_ids, &visible_bodies)
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
            visible_ids.len(),
        )
    );

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
            num_docs,
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
    let visible: BTreeSet<u64> = visible_row_ids.clone();
    let pool_depth = SQL_DEFAULT_HYBRID_POOL_DEPTH;
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, pool_depth)
        .unwrap_or_else(|e| fail_closed(format!("RrfConfig::new failed: {e:?}")));

    // 疎側再取得スケジュールの収集（Issue #387 PR #416 codex-review P1 指摘
    // 対応）: `sparse_refetch_schedule` は production の疎側再取得ループ実装
    // （`hybrid.rs::sparse_refetch_loop`）をテスト・ベンチ向け公開フック
    // `engine::hybrid::sparse_refetch_observed` 経由でそのまま呼び出すため、
    // `schedule.fetch_ks` は予測・複製ではなく実際に発火した `fetch_k` の列
    // そのものである（`hybrid_search_boosted` が内部で呼ぶのと同一の
    // `sparse_index`・`query_text`・`visible_ids`・`cfg.pool_depth()` を渡して
    // 同じ決定的コードパスを実行するため、別途の呼び出し回数突き合わせ・終端
    // 安定性検証は不要。以前はここで境界同点判定〔`boundary_tie_decision`〕を
    // ベンチ側で複製・予測しており、複製の追随漏れリスクを終端固定点検証で
    // 間接的に補っていたが、実観測へ切り替えたことでその補償自体が不要になった）。
    //
    // 忠実性検証（fail-closed）: `search_within` 内部の subset/df/score 3 区間
    // 分解（後段の `search_within_subset_only`/`_subset_df`/`_replica_full`）に
    // 使う複製 `ProfileSparseIndex` の出力が、上記スケジュール上で実際に呼ばれる
    // 各 `fetch_k` について実 `SparseIndex::search_within` の出力と数値一致する
    // ことを起動時に確認する（`harness::hybrid_profile` モジュールドキュメント
    // 「複製固有の限界」参照）。
    let mut sparse_schedules = Vec::with_capacity(queries.len());
    for q in &queries {
        let schedule = sparse_refetch_schedule(&sparse_index, &q.text, &visible, pool_depth)
            .unwrap_or_else(|e| fail_closed(format!("sparse_refetch_schedule failed: {e}")));
        for &fetch_k in &schedule.fetch_ks {
            replica_matches_real(&sparse_index, &replica, &q.text, fetch_k, &visible)
                .unwrap_or_else(|e| fail_closed(format!("replica fidelity check failed: {e}")));
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
            &visible_ids,
            &visible_vectors,
            corpus.dim,
            &q.vector,
            pool_depth,
        )
        .unwrap_or_else(|e| fail_closed(format!("dense_refetch_schedule failed: {e}")));
        provider.reset();
        let input = SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
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
        "hybrid_profile: fidelity checks passed (sparse_refetch_schedule now calls production's \
         shared sparse_refetch_loop via engine::hybrid::sparse_refetch_observed, so its \
         fetch_ks are an actual observation of hybrid_search_boosted's sparse refetch calls, \
         not a prediction — no separate call-count cross-check is needed for it; replica \
         search_within matches real API for every fetch_k on that schedule (used by the \
         subset/df/score breakdown below); dense refetch schedule predictions still match \
         observed hybrid_search calls via RefetchTrackingProvider)"
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
            ids: &visible_ids,
            vectors: &visible_vectors,
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
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        hybrid_search(&provider, input, &sparse_index, &q.text, TOP_K, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search (stats pass) failed: {e}")));
        dense_stats.push(harness::hybrid_latency::aggregate_refetch_stats(
            provider.calls(),
            provider.max_k_seen(),
            visible_ids.len(),
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

    // --- score_within_once: `SparseIndex::score_within` 単体（Issue #392）。
    // クエリ 1 回分のスコア計算のみを round-robin クエリで実測する
    // （Top-k 選出〔`SparseScored::top`〕を含まない）。
    let mut query_idx = 0usize;
    let score_within_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        sparse_index
            .score_within(&q.text, &visible)
            .unwrap_or_else(|e| fail_closed(format!("score_within failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("score_within_once measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&score_within_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let check = sparse_index
        .score_within(&queries[0].text, &visible)
        .map(|scored| scored.len())
        .unwrap_or(0);
    println!(
        "{}",
        render_stage_line(
            "score_within_once",
            score_within_measurement.summary.median.as_micros(),
            p95.as_micros(),
            check,
        )
    );

    // --- sparse_refetch_loop: 疎側再取得ループ本体（`hybrid_search_boosted` が
    // 実際に呼ぶコードパスそのもの。Issue #392）の実測累積コスト。Issue #387
    // PR #416 で production 経路とテスト・ベンチ向け公開フック
    // `sparse_refetch_observed` が同一実装を共有する構成になったため、下記の
    // `search_within_fetch_k=<k>`＋`sparse_refetch_summary`（複数ラウンドの
    // median 合算による推定値）とは異なり本区間は実測値そのものである。
    let mut query_idx = 0usize;
    let sparse_refetch_loop_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        sparse_refetch_observed(&sparse_index, &q.text, &visible, &cfg)
            .unwrap_or_else(|e| fail_closed(format!("sparse_refetch_observed failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("sparse_refetch_loop measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&sparse_refetch_loop_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    let check = sparse_refetch_observed(&sparse_index, &queries[0].text, &visible, &cfg)
        .map(|(hits, _limit, _fetch_ks)| hits.len())
        .unwrap_or(0);
    println!(
        "{}",
        render_stage_line(
            "sparse_refetch_loop",
            sparse_refetch_loop_measurement.summary.median.as_micros(),
            p95.as_micros(),
            check,
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

    // --- search_within 内部 3 区間（複製実装）---
    // codex-review P2 指摘対応（PR #416）: `subset_only`／`subset_df` は
    // `fetch_k` を受け取らず、可視集合サイズのみに依存するため k に対して
    // 不変である。以前は initial_k/final_k の k ループ内で再測定しラベルに
    // `k=<k>` を付けていたため、反復測定の揺らぎを fetch_k による差である
    // かのように誤読させていた。ここでは k ループの外で 1 回だけ測定し、
    // ラベルからも `k=` を外して k 非依存であることを明示する。k が実際に
    // 効く `search_within_replica_full` のみ initial_k/final_k の 2 点で
    // 計測するループに残す。
    let mut query_idx = 0usize;
    let subset_only_measurement = run(&config, || {
        let q = &queries[query_idx % queries.len()];
        query_idx += 1;
        replica.subset_only(&q.text, &visible)
    })
    .unwrap_or_else(|e| fail_closed(format!("search_within_subset_only measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&subset_only_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "search_within_subset_only (k-independent)",
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
    .unwrap_or_else(|e| fail_closed(format!("search_within_subset_df measurement failed: {e}")));
    let p95 = harness::accept::p95_from_samples(&subset_df_measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            "search_within_subset_df (k-independent)",
            subset_df_measurement.summary.median.as_micros(),
            p95.as_micros(),
            visible.len(),
        )
    );

    let cap = fetch_cap(visible.len());
    let initial_k = initial_fetch_k(pool_depth, cap);
    let final_k = union_fetch_ks.last().copied().unwrap_or(initial_k);
    for &fetch_k in &[initial_k, final_k] {
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

    // =========================================================================
    // Issue #465: hybrid_rrf 最新基線のラウンド計測（Issue #392 適用後）
    // =========================================================================
    //
    // 上記の単一パス段（Issue #356〜#392）はいずれも 1 回分の warmup+計測のみで
    // あり、共有計測環境のノイズを考慮した交互複数ラウンド計測になっていない。
    // 本節は `docs/design/benchmark-judgement-policy.md` の計測規約（交互
    // N≥5・min-of-R と median-of-R の併記・参照区間帯併記）に沿って B0s〜B8 の
    // 段を `BENCH_HYBRID_PROFILE_ROUNDS` ラウンド交互計測し、帰属表（SQL 表層・
    // 投影・密・疎・残差の内訳）を出力する。既存の単一パス段は無変更のまま
    // 残す（Issue #356〜#392 節との比較可能性維持）。
    //
    // 参照区間には `CpuScalarProvider::search`（単線・決定的）を使う。
    // `ParallelSearchProvider`（B0）はスレッドスケジューリングに依存し
    // ラウンド間で振れやすいため参照区間には使わない
    // （`docs/design/knn-wire-stage-profile.md` T1′ と同じ理由）。
    let rounds_env = std::env::var("BENCH_HYBRID_PROFILE_ROUNDS").ok();
    let rounds = match parse_rounds(rounds_env.as_deref()) {
        Ok(r) => r,
        Err(ScanStageError::InvalidRounds(reason)) => {
            fail_closed(format!("invalid BENCH_HYBRID_PROFILE_ROUNDS: {reason}"))
        }
        Err(e) => fail_closed(format!(
            "BENCH_HYBRID_PROFILE_ROUNDS validation failed: {e}"
        )),
    };
    println!(
        "hybrid_profile: baseline round measurement (Issue #465) starting — rounds={rounds} \
         (set BENCH_HYBRID_PROFILE_ROUNDS=5..50 to override; set BENCH_DEDICATED_ENV=1 to \
         self-report a dedicated measurement environment)"
    );
    // 値の中身を確認せず存在のみで判定すると "0" や空文字でも専有環境扱いになる
    // ため、他ベンチ（sql_c1_bench.rs・*_wire_profile_bench.rs）と同じ厳密一致
    // 契約（trim 後 "1" のときのみ true）に揃える（codex-review P2 指摘・PR #556）。
    let dedicated_env = std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    if !dedicated_env {
        println!(
            "hybrid_profile: BENCH_DEDICATED_ENV not set — the following baseline numbers are \
             shared-environment reference values only, not a basis for accept/reject decisions \
             (docs/design/benchmark-judgement-policy.md)"
        );
    }

    let dense_fetch_k = initial_fetch_k(pool_depth, fetch_cap(visible.len()));
    let mut b0s_round_medians = Vec::with_capacity(rounds as usize);
    let mut b0_round_medians = Vec::with_capacity(rounds as usize);
    let mut b1_round_medians = Vec::with_capacity(rounds as usize);
    let mut b2_round_medians = Vec::with_capacity(rounds as usize);
    let mut b3_round_medians = Vec::with_capacity(rounds as usize);
    let mut b4_round_medians = Vec::with_capacity(rounds as usize);
    let mut b5_round_medians = Vec::with_capacity(rounds as usize);
    let mut b7_round_medians = Vec::with_capacity(rounds as usize);
    let mut b8_round_medians = Vec::with_capacity(rounds as usize);

    // Issue #549 参考値: B7_fuse_lower_bound（`rrf_fuse` 単体の下限近似）。密・疎
    // それぞれの Top-`pool_depth` 候補を計測外（ラウンドループの前）で事前に捕捉し、
    // 計測区間には `hybrid::rrf_fuse` の呼び出しのみを含める。境界同点グループ完全化
    // （Issue #310・#320）の再取得コストは含まない「融合コアだけの処理時間」の
    // 下限近似であり、B4（`hybrid_search`。再取得込みの実効値）と対にして参照する
    // （申し送り: 前後比較・採否は #550／#547 の担当。本ベンチは参考値の記録のみ）。
    let b7_precomputed: Vec<(Vec<CandidateHit>, Vec<ScoredDoc>)> = queries
        .iter()
        .map(|q| {
            // 可視部分集合（`visible_ids`/`visible_vectors`）を使う。B4
            // （`hybrid_search_cached_index`）と同じ候補集合を密側へ渡さないと、
            // B7 が可視率縮小時に B4 より広い母集団から Top-`pool_depth` を
            // 拾ってしまい、対比対象として不整合になる（codex-review 指摘）。
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: pool_depth,
            };
            let dense = ParallelSearchProvider
                .search(input)
                .unwrap_or_else(|e| fail_closed(format!("B7 precompute dense search failed: {e}")));
            let mut sparse = sparse_refetch_observed(&sparse_index, &q.text, &visible, &cfg)
                .unwrap_or_else(|e| {
                    fail_closed(format!("B7 precompute sparse_refetch_observed failed: {e}"))
                })
                .0;
            sparse.truncate(pool_depth);
            (dense, sparse)
        })
        .collect();

    // fail-closed 整合性検証: B1（SQL hybrid・SELECT id）が返す id 集合が
    // B4（直接 API・hybrid_search）と一致することを、ラウンドループの前に
    // 1 回（queries[0]）だけ確認する（計測区間には混ぜない）。SQL 表層と
    // 直接 API 呼び出しが同じ融合結果を返すことの構造的な裏付けであり、
    // 不一致は投影・束縛経路の不整合を示すため fail-closed に打ち切る。
    {
        let q = &queries[0];
        let sql = sql_hybrid_statement_with_projection(
            TABLE,
            VECTOR_COLUMN,
            TEXT_COLUMN,
            &q.vector,
            &q.text,
            TOP_K,
            HybridProjection::Id,
        )
        .unwrap_or_else(|e| fail_closed(format!("B1 fidelity statement build failed: {e}")));
        let result = core
            .execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| fail_closed(format!("B1 fidelity execute_sql failed: {e}")));
        let mut b1_ids: Vec<u64> = result.rows.iter().map(|row| row.id).collect();
        b1_ids.sort_unstable();
        let input = SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: corpus.dim,
            query: &q.vector,
            k: TOP_K,
        };
        let hits = hybrid_search(
            &ParallelSearchProvider,
            input,
            &sparse_index,
            &q.text,
            TOP_K,
            &cfg,
        )
        .unwrap_or_else(|e| fail_closed(format!("B4 fidelity hybrid_search failed: {e}")));
        let mut b4_ids: Vec<u64> = hits.iter().map(|hit| hit.id).collect();
        b4_ids.sort_unstable();
        if b1_ids != b4_ids {
            fail_closed(format!(
                "B1(sql_hybrid_select_id)/B4(hybrid_search_cached_index) id set mismatch: \
                 sql={b1_ids:?} direct={b4_ids:?}"
            ));
        }
        // 可視件数が TOP_K 未満の設定（`BENCH_HYBRID_PROFILE_VISIBLE_RATIO` で
        // 小規模コーパス×高い分母を指定した場合。公開されている入力範囲内）では
        // 返る行数が可視件数で頭打ちになるのが正当であり、常に TOP_K ちょうどを
        // 要求すると到達可能な入力で必ず fail-closed してしまう（codex-review
        // 指摘）。期待値は `TOP_K` と可視件数の小さい方とする。
        let expected_hits = TOP_K.min(visible.len());
        if b1_ids.len() != expected_hits {
            fail_closed(format!(
                "B1/B4 fidelity: expected {expected_hits} ids, got {}",
                b1_ids.len()
            ));
        }
    }

    for round in 0..rounds {
        println!("hybrid_profile: baseline round {}/{rounds}", round + 1);

        // B0s_dense_scalar_ref: 参照区間（単線・決定的）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: dense_fetch_k,
            };
            CpuScalarProvider
                .search(input)
                .unwrap_or_else(|e| fail_closed(format!("B0s dense_scalar_ref failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B0s measurement failed: {e}")));
        b0s_round_medians.push(m.summary.median);

        // B0_dense_provider_pool: 密側の実効値（production 既定 provider）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: dense_fetch_k,
            };
            ParallelSearchProvider
                .search(input)
                .unwrap_or_else(|e| fail_closed(format!("B0 dense_provider_pool failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B0 measurement failed: {e}")));
        b0_round_medians.push(m.summary.median);

        // B1_sql_hybrid_select_id: crossdb 規範形（SQL 表層 e2e の上限）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let sql = sql_hybrid_statement_with_projection(
                TABLE,
                VECTOR_COLUMN,
                TEXT_COLUMN,
                &q.vector,
                &q.text,
                TOP_K,
                HybridProjection::Id,
            )
            .unwrap_or_else(|e| fail_closed(format!("B1 statement build failed: {e}")));
            core.execute_sql(&ctx, &sql)
                .unwrap_or_else(|e| fail_closed(format!("B1 execute_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B1 measurement failed: {e}")));
        b1_round_medians.push(m.summary.median);

        // B2_sql_hybrid_select_star: 既存段と同じ投影（本文複製あり）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let sql =
                sql_hybrid_statement(TABLE, VECTOR_COLUMN, TEXT_COLUMN, &q.vector, &q.text, TOP_K)
                    .unwrap_or_else(|e| fail_closed(format!("B2 statement build failed: {e}")));
            core.execute_sql(&ctx, &sql)
                .unwrap_or_else(|e| fail_closed(format!("B2 execute_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B2 measurement failed: {e}")));
        b2_round_medians.push(m.summary.median);

        // B3_sql_dense_select_id: fast path 対照（informational）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let sql = sql_dense_statement_with_projection(
                TABLE,
                VECTOR_COLUMN,
                &q.vector,
                TOP_K,
                HybridProjection::Id,
            )
            .unwrap_or_else(|e| fail_closed(format!("B3 statement build failed: {e}")));
            core.execute_sql(&ctx, &sql)
                .unwrap_or_else(|e| fail_closed(format!("B3 execute_sql failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B3 measurement failed: {e}")));
        b3_round_medians.push(m.summary.median);

        // B4_hybrid_search_cached_index: engine 内 hybrid 経路（直接 API）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            let input = SearchInput {
                ids: &visible_ids,
                vectors: &visible_vectors,
                dim: corpus.dim,
                query: &q.vector,
                k: TOP_K,
            };
            hybrid_search(
                &ParallelSearchProvider,
                input,
                &sparse_index,
                &q.text,
                TOP_K,
                &cfg,
            )
            .unwrap_or_else(|e| fail_closed(format!("B4 hybrid_search failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B4 measurement failed: {e}")));
        b4_round_medians.push(m.summary.median);

        // B5_sparse_refetch_loop: 疎側再取得ループ本体（既存フック共用）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let q = &queries[query_idx % queries.len()];
            query_idx += 1;
            sparse_refetch_observed(&sparse_index, &q.text, &visible, &cfg)
                .unwrap_or_else(|e| fail_closed(format!("B5 sparse_refetch_observed failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B5 measurement failed: {e}")));
        b5_round_medians.push(m.summary.median);

        // B7_fuse_lower_bound: `rrf_fuse` 単体（参考値。密・疎の取得コストは含まない
        // 下限近似。Issue #549）。
        let mut query_idx = 0usize;
        let m = run(&config, || {
            let (dense, sparse) = &b7_precomputed[query_idx % b7_precomputed.len()];
            query_idx += 1;
            rrf_fuse(dense, sparse, &cfg)
                .unwrap_or_else(|e| fail_closed(format!("B7 rrf_fuse failed: {e}")))
        })
        .unwrap_or_else(|e| fail_closed(format!("B7 measurement failed: {e}")));
        b7_round_medians.push(m.summary.median);

        // B8_visible_set_build: 残差内訳（可視集合 BTreeSet 構築。std 操作のみ）。
        let m = run(&config, || {
            visible_ids.iter().copied().collect::<BTreeSet<u64>>()
        })
        .unwrap_or_else(|e| fail_closed(format!("B8 measurement failed: {e}")));
        b8_round_medians.push(m.summary.median);
    }

    // --- per-round 生データ（計測規約 §3: per-run 生データ必須） ---
    for (round, (((((((b0s, b0), b1), b2), b3), b4), b5), b8)) in b0s_round_medians
        .iter()
        .zip(&b0_round_medians)
        .zip(&b1_round_medians)
        .zip(&b2_round_medians)
        .zip(&b3_round_medians)
        .zip(&b4_round_medians)
        .zip(&b5_round_medians)
        .zip(&b8_round_medians)
        .enumerate()
    {
        println!(
            "hybrid_profile: baseline_round_raw round={} B0s={}us B0={}us B1={}us B2={}us \
             B3={}us B4={}us B5={}us B8={}us",
            round + 1,
            b0s.as_micros(),
            b0.as_micros(),
            b1.as_micros(),
            b2.as_micros(),
            b3.as_micros(),
            b4.as_micros(),
            b5.as_micros(),
            b8.as_micros(),
        );
    }
    // B7 は Issue #549 で追加した参考値のため、上記の 8 段タプルとは別に単独ループで
    // per-round 生データを出力する（既存タプル連結を組み替えて破壊するリスクを避ける）。
    for (round, b7) in b7_round_medians.iter().enumerate() {
        println!(
            "hybrid_profile: baseline_round_raw_b7 round={} B7={}us",
            round + 1,
            b7.as_micros(),
        );
    }

    let ref_band_ratio = reference_band(&b0s_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("reference_band computation failed: {e}")));
    let ref_band_pct = ref_band_ratio * 100.0;
    println!(
        "hybrid_profile: baseline reference_band(B0s)={ref_band_pct:.2}% \
         (dedicated_env={dedicated_env})"
    );

    let min_b0s = min_of(&b0s_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B0s) failed: {e}")));
    let med_b0s = median_of(&b0s_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B0s) failed: {e}")));
    let min_b0 = min_of(&b0_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B0) failed: {e}")));
    let med_b0 = median_of(&b0_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B0) failed: {e}")));
    let min_b1 = min_of(&b1_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B1) failed: {e}")));
    let med_b1 = median_of(&b1_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B1) failed: {e}")));
    let min_b2 = min_of(&b2_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B2) failed: {e}")));
    let med_b2 = median_of(&b2_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B2) failed: {e}")));
    let min_b3 = min_of(&b3_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B3) failed: {e}")));
    let med_b3 = median_of(&b3_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B3) failed: {e}")));
    let min_b4 = min_of(&b4_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B4) failed: {e}")));
    let med_b4 = median_of(&b4_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B4) failed: {e}")));
    let min_b5 = min_of(&b5_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B5) failed: {e}")));
    let med_b5 = median_of(&b5_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B5) failed: {e}")));
    let min_b7 = min_of(&b7_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B7) failed: {e}")));
    let med_b7 = median_of(&b7_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B7) failed: {e}")));
    let min_b8 = min_of(&b8_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("min_of(B8) failed: {e}")));
    let med_b8 = median_of(&b8_round_medians)
        .unwrap_or_else(|e| fail_closed(format!("median_of(B8) failed: {e}")));

    println!(
        "hybrid_profile: baseline_summary_b7 B7(min={}us,median={}us)",
        min_b7.as_micros(),
        med_b7.as_micros(),
    );

    println!(
        "hybrid_profile: baseline_summary B0s(min={}us,median={}us) B0(min={}us,median={}us) \
         B1(min={}us,median={}us) B2(min={}us,median={}us) B3(min={}us,median={}us) \
         B4(min={}us,median={}us) B5(min={}us,median={}us) B8(min={}us,median={}us)",
        min_b0s.as_micros(),
        med_b0s.as_micros(),
        min_b0.as_micros(),
        med_b0.as_micros(),
        min_b1.as_micros(),
        med_b1.as_micros(),
        min_b2.as_micros(),
        med_b2.as_micros(),
        min_b3.as_micros(),
        med_b3.as_micros(),
        min_b4.as_micros(),
        med_b4.as_micros(),
        min_b5.as_micros(),
        med_b5.as_micros(),
        min_b8.as_micros(),
        med_b8.as_micros(),
    );

    // --- 帰属表（min-of-R 基準。ratio_of_b1 は表示専用で B1 min に対する構成比を
    // 示すが、band 判定はこれと分離し docs/design/benchmark-judgement-policy.md
    // §4 の「before を分母とする」規約どおり、当該区間自身の before/after
    // （`step_ratio_pct(before, after)`）を用いる。比較元となる before/after の
    // 対を持たない単独区分（dense/sparse/residual 系・wire 側等）は band を
    // n/a とする ---
    let sql_surface_diff = bucket_diff(min_b4, min_b1); // B1 - B4
    let projection_diff = bucket_diff(min_b1, min_b2); // B2 - B1
    let residual_diff = {
        // B4 - B0 - B5（飽和差分の連鎖。どこかで逆転したら以降は None）
        bucket_diff(min_b0, min_b4).and_then(|d| bucket_diff(min_b5, d))
    };
    let residual_minus_visible_diff = residual_diff.and_then(|d| bucket_diff(min_b8, d));

    let b1_us = min_b1.as_micros().max(1) as f64;
    let render_bucket =
        |label: &str,
         diff: Option<std::time::Duration>,
         band_basis: Option<(std::time::Duration, std::time::Duration)>| {
            let diff_us = diff.map(|d| d.as_micros());
            let ratio_pct = diff_us.map(|us| (us as f64 / b1_us) * 100.0);
            let band = match band_basis {
                Some((before, after)) => match step_ratio_pct(before, after) {
                    Ok(pct) => classify_against_bands(pct, ref_band_pct).to_string(),
                    Err(_) => "n/a".to_string(),
                },
                None => "n/a".to_string(),
            };
            println!(
                "{}",
                render_baseline_bucket_line(label, diff_us, ratio_pct, &band)
            );
        };
    render_bucket(
        "sql_surface(B1-B4)",
        sql_surface_diff,
        Some((min_b4, min_b1)),
    );
    render_bucket("projection(B2-B1)", projection_diff, Some((min_b1, min_b2)));
    render_bucket("dense(B0)", Some(min_b0), None);
    render_bucket("sparse(B5)", Some(min_b5), None);
    render_bucket(
        "fuse_lower_bound(B7, informational, Issue #549)",
        Some(min_b7),
        None,
    );
    render_bucket("residual(B4-B0-B5)", residual_diff, None);
    render_bucket(
        "residual_minus_visible_set_build(B4-B0-B5-B8)",
        residual_minus_visible_diff,
        None,
    );
    render_bucket("visible_set_build(B8)", Some(min_b8), None);
    render_bucket(
        "dense_fast_path_contrast(B3, informational)",
        Some(min_b3),
        None,
    );

    println!(
        "hybrid_profile: baseline round measurement (Issue #465) done — see \
         docs/design/hybrid-rrf-latency-breakdown.md \"最新基線\" section for the transcribed \
         attribution table and top-2-stage identification handed to Issue #548"
    );

    println!(
        "hybrid_profile: done (see docs/design/hybrid-rrf-latency-breakdown.md for the \
         attribution table transcribed from this run's output)"
    );
}
