//! 境界同点グループ再取得ループ（Issue #320・`hybrid.rs::hybrid_search_boosted`）の
//! 単発クエリレイテンシへの寄与を計測するベンチ（Issue #324。ポインタ:
//! `docs/spec/04-behavior/core-engine.md` CORE-7・`docs/spec/04-behavior/
//! query-planning.md` PLAN-4, PLAN-6, PLAN-7。判定内容・数値基準は spec 側が SSOT
//! であり本ファイルには転記しない。`.claude/rules/spec-confidentiality.md`）。
//!
//! # 背景
//!
//! PR #320 は `hybrid_search_boosted` に、`pool_depth` 境界の同点グループが未確定
//! （`TieBoundary::Undetermined`）の場合に `fetch_k` を倍増して密 provider を
//! 再呼び出しするループを導入した（上限 `MAX_FETCH_K`・可視集合サイズで有界化）。
//! 既存の受け入れ基準ベンチのうち、CORE-7 ゲート（`batch_bench.rs::run_core7_gate`）は
//! `BatchEngine::batch_search`（f16 常駐・CPU-SIMD）経由で測定しており
//! `hybrid_search` を一切通らない（`docs/design/core7-dynamic-window-gate.md`）ため、
//! PR #320 の変更が CORE-7 の実測値に影響することは構造的にありえない。ティア別
//! レイテンシベンチ（`tier_latency_bench.rs`。PLAN-4/6/7）は `USING PLAN(...)` 経由で
//! `hybrid_search` を通るが常駐 Ollama 前提のため、この寄与だけを Ollama なしに
//! 分離して計測する入口が存在しなかった。本ベンチはその隙間を埋める
//! （`docs/design/hybrid-refetch-latency.md` 参照）。
//!
//! # 計測方式（in-build 比較・近似）
//!
//! 2 コミット間 worktree A/B ではなく、単一ビルド内で「再取得がほぼ発生しない
//! 通常コーパス（連続値ベクトル）」と「同点グループを誘発し再取得を複数回発生
//! させるプロトタイプクラスタコーパス」を比較する（`harness::hybrid_latency`
//! モジュールドキュメント参照）。**近似比較である**点に注意: 2 段は密ベクトルの
//! 分布そのもの（連続値 vs. プロトタイプクラスタ）が異なり、厳密には「再取得
//! ループの有無だけ」が変数ではない（疎チャネルの内容は `rng` 系列を分離して
//! 両段で共有するため揃えている。`harness::hybrid_latency::generate_corpus`
//! ドキュメント参照）。また今回の同点誘発コーパスは `reached_visible_set=0/20`
//! （`docs/design/hybrid-refetch-latency.md`「実測結果」節）であり、再取得
//! ループが可視集合サイズまで到達する最悪ケース（`tests/hybrid_recall.rs::
//! hybrid_recall_large_scale_dense_refetch_is_bounded_by_visible_set_size` が
//! 追跡する大規模 Recall フィクスチャで実際に起きる挙動）は本ベンチでは
//! 再現・測定できていない。stage 名 `*_tie_refetch` は「同点誘発による複数回
//! 再取得」を表し、可視集合到達を含意しない（可視集合到達を含意していた旧名
//! `*_max_refetch` から改称。PR #325 レビュー対応）。2 段の差分は再取得ループの
//! 寄与の**近似値**として扱う。加えて小規模・大規模の 2 スケールで測る
//! （`tests/hybrid_recall.rs` の段構成に合わせる）。
//!
//! 測定対象は `hybrid::hybrid_search`（`RrfConfig::default()`）の単発呼び出しのみで、
//! SQL パース・テーブル走査を含めない（`sql/exec.rs` の C4 経路から再取得ループの
//! 寄与だけを分離する）。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make bench-hybrid`
//! からの手動実行専用）。誤って CI 経由で実行された場合の defense-in-depth として
//! `GITHUB_ACTIONS` 環境変数が設定されていれば起動直後に fail-closed で拒否する
//! （`harness::hybrid_latency::refuse_under_github_actions`）。
//!
//! `make bench-hybrid`（Makefile）から実行する。判定ロジック自体（時間非依存）は
//! `harness::hybrid_latency` にあり `tests/hybrid_latency_accept.rs` で `make ci`
//! 側から回帰検証する。本ベンチ自体は spec 由来の pass/fail 閾値を持たない
//! 情報提供専用（計画「出力規約」節）で、実測値は常に出力する。

#[allow(dead_code)]
mod harness;

use harness::bench_engine::{self, BenchEngine};
use harness::env_report::EnvReport;
use harness::hybrid_latency::{
    aggregate_refetch_stats, check_ann_non_vacuous, generate_corpus, generate_query,
    parse_bounded_usize, parse_corpus_selection, parse_scale_selection,
    refuse_under_github_actions, render_ann_stage_line, render_stage_line, summarize_refetch_stats,
    AnnRoundStats, Corpus, LatencyCorpusKind, LatencyScale, Query, RefetchStats,
    RefetchTrackingProvider, MAX_CORPUS_DOCS_GUARD,
};
use harness::protocol::{run, MeasurementConfig};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hnsw::{HnswParams, ResidentPrecision, ValidatedHnswParams};
use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

// SQL 表層（hnsw opt-in）計測モード用の一時 DB ヘルパ（Issue #506）。本ファイルの
// crate root で 1 度だけ宣言する（`harness::hybrid_latency` 側は `#[path]` 経由で
// 複数バイナリから共有取り込みされるため、そちら側で `mod temp_db;` を重ねると
// 既に独自の `mod temp_db;` を持つ他バイナリと衝突し `clippy::duplicate_mod` に
// 抵触する。`harness/hybrid_latency.rs` 冒頭コメント参照）。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;

/// 小規模段。`tests/hybrid_recall.rs` の小規模フィクスチャ（400 件）は
/// `RrfConfig::default().pool_depth() * 2`（初回 `fetch_k` = 400）と偶然一致し、
/// 通常コーパスでも初回呼び出しで可視集合全体を取り切ってしまい再取得ループの
/// 有無を比較できない。本ベンチは初回 `fetch_k` を上回る規模にして
/// 「再取得の余地がある」条件を保つ。加えて `sql::hnsw_cache::MIN_INDEXED_ROWS`
/// （1,024。ここでは複製せず参照するのみ）を上回る件数にする必要がある——
/// SQL 表層（hnsw opt-in）計測モード（[`run_sql_surface_mode`]）はこの閾値
/// 未満のコーパスでは構造的に索引を構築せず全件 brute-force へ縮退するため、
/// `check_ann_non_vacuous` の `builds >= 1` 検証が常に失敗し、既定実行
/// （`BENCH_HYBRID_LATENCY_ENGINE=hnsw`・`BENCH_HYBRID_LATENCY_SCALE=all`）が
/// small ステージで fail-closed 終了してしまう（Cursor Bugbot 指摘・PR #622）。
const SMALL_NUM_DOCS: usize = 1_200;
/// 大規模段（`tests/hybrid_recall.rs` の大規模フィクスチャと同一件数。可視集合到達の
/// 判定条件をそのまま流用できるようにする）。
const LARGE_NUM_DOCS: usize = 20_000;
const VOCAB_SIZE: usize = 256;
const DIM: usize = 32;
const TOP_K: usize = 20;
/// プロトタイプクラスタコーパスのクラスタ数（少ないほど 1 クラスタあたりの文書数が
/// 増え、同点グループが大きくなる。密チャネルの同点誘発の強度パラメータであり、
/// spec 由来の数値ではないためここに定数として持つ）。
const QUANTIZE_LEVELS: usize = 5;
const NUM_QUERIES: usize = 20;
const SEED: u64 = 0x4832_4832_4832_4832;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("hybrid_latency_bench: {msg}");
    std::process::exit(1);
}

/// 1 段（コーパス規模 × 量子化有無）の計測。`corpus` に対する `queries` 件の
/// クエリを round-robin しつつ `harness::protocol::run` で p95/median を測る
/// （計測区間は `hybrid_search` 呼び出しのみで `RefetchTrackingProvider` の
/// 統計読み取りは行わない）。再取得統計（`RefetchStats`）は計測とは独立した
/// 時間非依存の 1 パスで `queries` を 1 回ずつ処理して集計する（PR #325
/// レビュー対応: 計測区間内での統計蓄積は warmup 分の混入・`Vec` 再確保による
/// p95 汚染を招くため分離した）。
fn measure_stage(stage_name: &str, corpus: &Corpus, queries: &[Query], cfg: &RrfConfig) {
    let sparse_index = corpus
        .build_sparse_index()
        .unwrap_or_else(|e| fail_closed(format!("sparse index build failed: {e}")));
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let visible_set_size = corpus.ids.len();

    // 計測回数（`measured_iterations`）は `NUM_QUERIES` の倍数にする。round-robin
    // で `queries[query_idx % queries.len()]` を選ぶため、倍数でないとクエリ 0..r-1
    // （`r = measured_iterations % NUM_QUERIES`）だけ 1 回多く計測され、p95/median が
    // 特定クエリへ偏って重み付けされる（codex-review P1 指摘・PR #325）。
    let config = MeasurementConfig::new(20, 2 * NUM_QUERIES as u32, SEED)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));

    // 計測フェーズ（`run` の内側）は p95/median を得るためだけの区間で、
    // `queries` を round-robin しつつ warmup 回・計測回（合計 `run` が
    // `workload` を呼ぶ回数）繰り返す。ここで再取得統計（`RefetchStats`）を
    // `push` すると、(1) warmup 分と計測分の呼び出しが同じ `Vec` に混在して
    // クエリ数の集計が呼び出し回数まで水増しされ（Cursor Bugbot 指摘・PR #325）、
    // (2) 事前確保した容量を超えたときの `Vec` 再確保（ヒープ確保）が計測区間の
    // 内側で発生し p95 を汚染する（codex-review P1 指摘・PR #325）という 2 つの
    // 計測汚染が起きる。再取得統計はクエリと同一コーパスに対して決定的
    // （`hybrid_search` は純粋な検索呼び出しで、同じ `(query, corpus)` なら
    // provider 呼び出し回数・`max_k_seen` は常に同じ）ため、`run` による計測とは
    // 完全に切り離した別パスで 1 クエリにつき 1 回だけ集計すれば情報は失われない。
    // 計測区間（`run` に渡す `workload`）は `provider` を経由した `hybrid_search`
    // 呼び出しのみを行い、統計の読み取り・蓄積を一切行わない。
    let mut query_idx = 0usize;

    let measurement = run(&config, || {
        let query = &queries[query_idx % queries.len()];
        query_idx += 1;
        provider.reset();
        let input = SearchInput {
            ids: &corpus.ids,
            vectors: &corpus.vectors,
            dim: corpus.dim,
            query: &query.vector,
            k: TOP_K,
        };
        hybrid_search(&provider, input, &sparse_index, &query.text, TOP_K, cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search failed: {e}")))
    })
    .unwrap_or_else(|e| fail_closed(format!("measurement protocol violation: {e}")));

    // 再取得統計は計測（`run`）とは別の、時間非依存の 1 パスで集計する（上記コメント
    // 参照）。`queries` の各要素をちょうど 1 回だけ処理するため、`summarize_refetch_stats`
    // が返す `queries` はユニーククエリ数（`queries.len()`）と一致する。
    let mut refetch_stats: Vec<RefetchStats> = Vec::with_capacity(queries.len());
    for query in queries {
        provider.reset();
        let input = SearchInput {
            ids: &corpus.ids,
            vectors: &corpus.vectors,
            dim: corpus.dim,
            query: &query.vector,
            k: TOP_K,
        };
        hybrid_search(&provider, input, &sparse_index, &query.text, TOP_K, cfg)
            .unwrap_or_else(|e| fail_closed(format!("hybrid_search failed (stats pass): {e}")));
        refetch_stats.push(aggregate_refetch_stats(
            provider.calls(),
            provider.max_k_seen(),
            visible_set_size,
        ));
    }

    let summary = summarize_refetch_stats(&refetch_stats);
    let p95 = harness::accept::p95_from_samples(&measurement.samples)
        .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
    println!(
        "{}",
        render_stage_line(
            stage_name,
            measurement.summary.median.as_micros(),
            p95.as_micros(),
            summary,
        )
    );
}

/// SQL 表層（hnsw opt-in）計測モードの起動判定に使う env 変数名。
/// **未設定**の場合は本ベンチは従来どおりの in-build 比較モード（既定モード。
/// 出力はこの env の有無に関わらずバイト単位で不変）のみを実行する。
/// この env が（空文字列であっても）存在する場合のみ、Issue #506 の SQL 表層
/// 計測モード（[`run_sql_surface_mode`]）へ切り替える——`hybrid_latency_bench`
/// の実 seam は既定モードでは `hybrid::hybrid_search` の直接呼び出しであり、
/// Issue #505（`sql::hnsw_hybrid::HnswDenseProvider` の再開型探索）を一切通ら
/// ない（モジュールドキュメント「SQL 表層」節参照）。
const SQL_SURFACE_ENGINE_ENV: &str = "BENCH_HYBRID_LATENCY_ENGINE";

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );

    // `BENCH_HYBRID_LATENCY_ENGINE` の**有無**（値ではなく）だけでモードを
    // 切り替える。未設定時の出力は本節を含め一切変更しない（既定モードの
    // 後方互換性を保つ。計画「既定モードの出力は現行と完全に同一」節）。
    if std::env::var_os(SQL_SURFACE_ENGINE_ENV).is_none() {
        println!(
            "hybrid_latency_bench: measures hybrid_search_boosted's boundary tie-group refetch \
             loop (Issue #320) latency contribution via an in-build comparison (no-refetch vs \
             tie-refetch corpora), not a pass/fail gate (Issue #324; see docs/design/\
             hybrid-refetch-latency.md)"
        );
        run_in_build_mode();
    } else {
        run_sql_surface_mode();
    }
}

/// 既定モード（env 未設定）: `hybrid::hybrid_search` を `ParallelSearchProvider`
/// で直接呼ぶ in-build 比較（Issue #324。モジュール冒頭コメント参照）。
fn run_in_build_mode() {
    let cfg = RrfConfig::default();

    for (label, num_docs) in [("small", SMALL_NUM_DOCS), ("large", LARGE_NUM_DOCS)] {
        let no_refetch_corpus = generate_corpus(SEED, num_docs, VOCAB_SIZE, DIM, None)
            .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));
        let tie_refetch_corpus =
            generate_corpus(SEED, num_docs, VOCAB_SIZE, DIM, Some(QUANTIZE_LEVELS))
                .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));

        // クエリ生成はプロトタイプクラスタモードに依存しない（`generate_query`
        // ドキュメント参照: 同点誘発はコーパス側のベクトル重複のみで成立する）ため
        // 2 段で共有する。
        let queries: Vec<Query> = (0..NUM_QUERIES)
            .map(|i| generate_query(SEED.wrapping_add(i as u64), DIM, VOCAB_SIZE))
            .collect();

        measure_stage(
            &format!("{label}_no_refetch"),
            &no_refetch_corpus,
            &queries,
            &cfg,
        );
        measure_stage(
            &format!("{label}_tie_refetch"),
            &tie_refetch_corpus,
            &queries,
            &cfg,
        );
    }
}

/// env 読み取りの薄いラッパ（未設定は `None`、非 UTF-8 は fail-closed。
/// `bench_engine::read_env_var` をそのまま経由する）。
fn read_env(name: &'static str) -> Option<String> {
    bench_engine::read_env_var(name).unwrap_or_else(|e| fail_closed(e))
}

/// `generate_corpus`／`generate_query` が組み立てるベクトルリテラル・SQL 文字列。
/// 値は決定的 RNG（`harness::rng::DeterministicRng`）由来のみで外部入力を連結
/// しない（coding-rust.md「SQL / プラン文字列の組み立てに未検証入力を連結しない」。
/// `tests/fixtures/recall_engine.rs::vec_literal`／`sql_escape` と同型）。
fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// SQL 表層（hnsw opt-in）計測用の最小テーブル fixture（`docs(embedding
/// VECTOR(dim), body TEXT)`。単一テナント `tenant-a`・`Visibility::Public`
/// 固定。Issue #506）。`tests/fixtures/recall_engine.rs::SqlHybridFixture` と
/// 同じ構築パターンを複製する（bench クレートからは `tests/` 配下を `#[path]`
/// で取り込めないため。両者は独立に保守するが SQL 文組み立て・エンジン構築の
/// 契約は同一）。`harness::hybrid_latency` ではなく本ファイルへ直接置く理由は
/// 冒頭の `mod temp_db;` コメント参照。
struct SqlHybridBenchFixture {
    core: EngineCore,
    ctx: PolicyContext,
    _guard: temp_db::CleanupGuard,
}

impl SqlHybridBenchFixture {
    /// `corpus`（[`Corpus`]）を `docs` テーブルへ投入し、`engine` に応じて
    /// ANN opt-in（`from_storage_with_engine`）または既定エンジン
    /// （`from_storage`）でオープンする。
    fn new(corpus: &Corpus, engine: BenchEngine) -> Self {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(corpus.dim), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        );
        let dir = temp_db::unique_db_path("hybrid-latency-sql-hybrid");
        let guard = temp_db::CleanupGuard(dir.clone());
        let storage = Storage::open(&dir).expect("open storage");
        storage.create_table(&schema).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let dim = corpus.dim as usize;
        for (i, id) in corpus.ids.iter().enumerate() {
            let vector = corpus.vectors[i * dim..(i + 1) * dim].to_vec();
            let body = corpus.texts[i].clone();
            let op_id =
                OperationId::parse(&format!("hybrid-latency-seed-{id}")).expect("valid op id");
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                *id,
                Visibility::Public,
                &[Value::Vector(vector), Value::Text(body)],
                &op_id,
            )
            .unwrap_or_else(|e| panic!("insert row id={id} failed: {e}"));
        }
        let core = match engine {
            BenchEngine::Hnsw => {
                let kind = engine::search_engine::hnsw_kind(HnswParams::default())
                    .expect("valid hnsw params");
                EngineCore::from_storage_with_engine(storage, kind)
            }
            BenchEngine::HnswF16 => {
                let validated = ValidatedHnswParams::new(HnswParams::default())
                    .expect("default params validate")
                    .with_resident_precision(ResidentPrecision::F16);
                let kind = engine::search_engine::SearchEngineKind::Hnsw(validated);
                EngineCore::from_storage_with_engine(storage, kind)
            }
            BenchEngine::BruteForce => {
                EngineCore::from_storage(storage, engine::search_engine::default_engine())
            }
        };
        Self {
            core,
            ctx,
            _guard: guard,
        }
    }

    /// `SELECT id FROM docs ORDER BY HYBRID(embedding, '<vec>', body, '<text>')
    /// LIMIT k` を実行する（結果は破棄。計測対象は所要時間のみ）。
    fn run_hybrid_query(&self, query: &Query, k: usize) {
        let sql = format!(
            "SELECT id FROM docs ORDER BY HYBRID(embedding, {}, body, '{}') LIMIT {k}",
            vec_literal(&query.vector),
            sql_escape(&query.text),
        );
        self.core
            .execute_sql(&self.ctx, &sql)
            .expect("hybrid query should succeed");
    }

    /// ANN opt-in エンジンの内部統計スナップショット（[`AnnRoundStats`]）。
    fn ann_stats(&self) -> AnnRoundStats {
        let stats = self.core.hnsw_index_cache_stats();
        let debug = format!("{stats:?}");
        AnnRoundStats {
            builds: stats.builds,
            build_failures: stats.build_failures,
            hybrid_dense_searches: stats.hybrid_dense_searches,
            hybrid_rounds_max: stats.hybrid_rounds_max,
            masked_short: stats.masked_short,
            fallbacks: stats.fallbacks,
            ef_cap_fallbacks: stats.ef_cap_fallbacks,
            f16_residency_fallbacks: stats.f16_residency_fallbacks,
            // `hybrid_resumed_rounds` は Issue #505（コミット `4ceb6b5`）で
            // 追加されたフィールドで、それ以前のツリー（`838c53e`。本 Issue の
            // before バイナリ）には存在しない。Debug 文字列越しに読むことで、
            // 両ツリーでこの関数自体をコンパイル可能にする
            // （`harness::hybrid_latency::extract_counter` ドキュメンテーション
            // コメント参照）。
            hybrid_resumed_rounds: harness::hybrid_latency::extract_counter(
                &debug,
                "hybrid_resumed_rounds",
            ),
        }
    }
}

/// SQL 表層（hnsw opt-in）計測モード（Issue #506）。`BENCH_HYBRID_LATENCY_ENGINE`
/// が設定されている場合にのみ [`main`] から呼ばれる。Issue #505 の実 seam
/// （`sql::hnsw_hybrid::HnswDenseProvider`）へ到達できる唯一の production API
/// である `EngineCore::from_storage_with_engine` ＋ `ORDER BY HYBRID(...)`
/// （[`SqlHybridBenchFixture`]）を計測する。
fn run_sql_surface_mode() {
    let engine = bench_engine::parse_engine(read_env(SQL_SURFACE_ENGINE_ENV).as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let scales = parse_scale_selection(read_env("BENCH_HYBRID_LATENCY_SCALE").as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let corpora = parse_corpus_selection(read_env("BENCH_HYBRID_LATENCY_CORPUS").as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let dim = parse_bounded_usize(
        read_env("BENCH_HYBRID_LATENCY_DIM").as_deref(),
        DIM,
        1,
        4_096,
        "BENCH_HYBRID_LATENCY_DIM",
    )
    .unwrap_or_else(|e| fail_closed(e));
    let vocab_size = parse_bounded_usize(
        read_env("BENCH_HYBRID_LATENCY_VOCAB_SIZE").as_deref(),
        VOCAB_SIZE,
        1,
        1_000_000,
        "BENCH_HYBRID_LATENCY_VOCAB_SIZE",
    )
    .unwrap_or_else(|e| fail_closed(e));
    let quantize_levels = parse_bounded_usize(
        read_env("BENCH_HYBRID_LATENCY_QUANTIZE_LEVELS").as_deref(),
        QUANTIZE_LEVELS,
        2,
        100_000,
        "BENCH_HYBRID_LATENCY_QUANTIZE_LEVELS",
    )
    .unwrap_or_else(|e| fail_closed(e));
    // `tie_refetch` 条件の after 側計測にのみ渡す想定（`no_refetch` へ渡すと
    // 構造的に失敗する。`check_ann_non_vacuous` ドキュメンテーションコメント
    // 参照）。有効・無効は呼び出し側（`scripts/bench_hybrid_latency_ab.sh`）が
    // 条件ごとに選ぶ。
    let expect_resumed =
        bench_engine::parse_flag(read_env("BENCH_HYBRID_LATENCY_EXPECT_RESUMED").as_deref())
            .unwrap_or_else(|e| fail_closed(e));

    println!(
        "hybrid_latency_bench: SQL-surface (hnsw opt-in) mode — measures sql::hnsw_hybrid::\
         HnswDenseProvider (Issue #505's resumable refetch path) via EngineCore::\
         from_storage_with_engine + ORDER BY HYBRID(...), the only production seam that reaches \
         it (Issue #412 design). Not a pass/fail gate (Issue #506; see docs/design/\
         hnsw-hybrid-iterative-scan.md)."
    );

    for scale in scales {
        let (label, default_num_docs) = match scale {
            LatencyScale::Small => ("small", SMALL_NUM_DOCS),
            LatencyScale::Large => ("large", LARGE_NUM_DOCS),
        };
        let num_docs = parse_bounded_usize(
            read_env("BENCH_HYBRID_LATENCY_NUM_DOCS").as_deref(),
            default_num_docs,
            1,
            MAX_CORPUS_DOCS_GUARD,
            "BENCH_HYBRID_LATENCY_NUM_DOCS",
        )
        .unwrap_or_else(|e| fail_closed(e));

        for &corpus_kind in &corpora {
            let (corpus_label, quantize) = match corpus_kind {
                LatencyCorpusKind::NoRefetch => ("no_refetch", None),
                LatencyCorpusKind::TieRefetch => ("tie_refetch", Some(quantize_levels)),
            };
            let corpus = generate_corpus(SEED, num_docs, vocab_size, dim, quantize)
                .unwrap_or_else(|e| fail_closed(format!("corpus generation failed: {e}")));
            let queries: Vec<Query> = (0..NUM_QUERIES)
                .map(|i| generate_query(SEED.wrapping_add(i as u64), dim, vocab_size))
                .collect();

            let fixture = SqlHybridBenchFixture::new(&corpus, engine);
            let mut query_idx = 0usize;
            let config = MeasurementConfig::new(20, 2 * NUM_QUERIES as u32, SEED)
                .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));
            let measurement = run(&config, || {
                let query = &queries[query_idx % queries.len()];
                query_idx += 1;
                fixture.run_hybrid_query(query, TOP_K);
            })
            .unwrap_or_else(|e| fail_closed(format!("measurement protocol violation: {e}")));

            let p95 = harness::accept::p95_from_samples(&measurement.samples)
                .unwrap_or_else(|e| fail_closed(format!("p95 computation failed: {e}")));
            let stats = fixture.ann_stats();
            let stage = format!("sql_{label}_{corpus_label}");
            println!(
                "{}",
                render_ann_stage_line(
                    &stage,
                    engine,
                    measurement.summary.median.as_micros(),
                    p95.as_micros(),
                    stats,
                )
            );

            if engine != BenchEngine::BruteForce {
                let expect_resumed_here =
                    expect_resumed && matches!(corpus_kind, LatencyCorpusKind::TieRefetch);
                if let Err(e) = check_ann_non_vacuous(stats, expect_resumed_here) {
                    fail_closed(format!("stage={stage}: {e}"));
                }
            }
        }
    }
}
