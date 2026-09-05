//! 自作 HNSW（`engine::hnsw::HnswIndex`）と外部フレームワーク usearch（承認済み
//! optional 依存 `usearch =2.26.1`）・hnsw_rs（承認済み optional 依存
//! `hnsw_rs =0.3.4`。2026-09-05 承認。いずれも `contrast-bench` feature 限定）の
//! 構築時間（スレッド数ラダー）・Recall@10・探索レイテンシを同一条件で比較する
//! 手動専用ベンチ（Issue #402 系 ADR `docs/design/ann-index-adoption.md` の
//! 実測補強。`docs/spec/05-tasks.md` TASK-132 のポインタ）。
//!
//! # コーパス正規化は 3 エンジン共通
//!
//! hnsw_rs 側は `anndists::dist::distances::DistDot`（`simdeez_f` feature 経由
//! では `dist::disteez::distance_dot_f32_simdeez`）が単位ベクトルを前提に
//! `assert!(dot <= 1.000002)` する（`harness::hnsw_compare::hnsw_rs_adapter` の
//! モジュールコメント参照）ため単位ノルムが必須だが、本ベンチはベンチ冒頭で
//! [`harness::hnsw_compare::l2_normalize_corpus`] を 1 回だけ呼び出し、
//! self（`kernel::dot`）・usearch（`MetricKind::IP`）・hnsw_rs（`DistDot`）
//! の 3 エンジンすべてへ同じ正規化済みコーパス・クエリを渡す。正規化後は
//! 内積の最大化とコサイン類似度の最大化が一致するため、3 エンジンの
//! Recall@10・構築時間はすべて同一入力に基づく数値として単純比較できる
//! （出力ヘッダの `corpus=l2_normalized` 参照。PR #445 時点〔hnsw_rs だけ
//! 正規化・self/usearch は無正規化という非対称条件〕の実測値とは比較条件が
//! 異なるため、当時の数値と本ベンチの数値を直接比較しない）。ゼロベクトル
//! （正規化不能な行）が生成された場合は fail-closed でベンチを中断する
//! （`std::process::exit(1)`）。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には本ベンチの実行経路を置かない（`make
//! bench-hnsw-compare` からの手動実行専用。`hnsw_build_bench.rs`・
//! `hnsw_parallel_build_bench.rs` と同一方針の defense-in-depth 拒否）。
//!
//! `contrast_bench.rs`（TASK-127 CORE-5）と同様に対照エンジン（usearch。C++ FFI
//! を含む `contrast-bench` feature）へ依存するため、`Cargo.toml` の `[[bench]]
//! hnsw_compare_bench` は `required-features = ["contrast-bench"]` を持ち、
//! feature 無効時は本ターゲット自体がスキップされる（他ベンチの failure domain
//! 分離を壊さない）。
//!
//! # 測定条件
//!
//! - `rows`（既定 [`harness::hnsw_compare::DEFAULT_ROWS`]・
//!   `BENCH_HNSW_COMPARE_ROWS` で上書き可）・`dim`（既定
//!   [`harness::hnsw_compare::DEFAULT_DIM`]・`BENCH_HNSW_COMPARE_DIM` で
//!   `{64, 128}` のみ上書き可）のコーパスを
//!   `harness::hnsw_build::generate_corpus` で決定的に生成する。
//! - クエリは別シードで同じ生成器から作る（既定件数
//!   [`harness::hnsw_compare::DEFAULT_QUERIES`]・
//!   `BENCH_HNSW_COMPARE_QUERIES` で上書き可）。
//! - スレッド数ラダー（既定 `[1, 2, 4, 8, .., available_parallelism]`・
//!   `BENCH_HNSW_COMPARE_THREADS` で上書き可）ごとに、
//!   engine 側 [`engine::hnsw::HnswIndex::build_with_threads`] と usearch 側
//!   （[`harness::hnsw_compare::usearch_adapter::build_usearch_index_parallel`]。
//!   `Index::new` → `reserve_capacity_and_threads` → 並列 `add` までを 1 回分の
//!   計測に含める。engine 側が毎回の呼び出しで `Arc::from` によるスナップショット
//!   コピー等の初期化コストを含めて計測しているのと対称にするため）の構築
//!   時間中央値を [`harness::protocol::run`]（warmup/計測下限 20/20 は変更しない）
//!   で計測する。
//! - パラメータ等価: engine 側 `HnswParams::default()`（m=16,
//!   ef_construction=100, ef_search=64。Issue #403 の本リポ採用既定値）と
//!   usearch 側（connectivity=16, expansion_add=100, expansion_search=64,
//!   metric=IP, quantization=F32, multi=false）の対応は
//!   `harness::hnsw_compare::usearch_adapter::usearch_index_options` の
//!   ドキュメンテーションコメント参照。
//! - Recall@10: ラダー最大スレッド数で構築した各索引について、全クエリで
//!   Top-10 を取り、同じ正規化済みコーパス・クエリに対する単一の
//!   brute-force（`engine::kernel::CpuScalarProvider`。内積最大）対照で
//!   Recall@10 平均を出す（`harness::accept::recall_at_k` は id の集合演算
//!   のみで判定するため、同点近傍の内部順序入れ替わりは Recall を過小評価
//!   しない。連続一様分布の合成ベクトルでは Top-k 境界での完全な同点は
//!   ほぼ発生しないため、本ベンチでは同点の特別扱いをしない）。3 エンジン
//!   とも同じ brute-force 対照・同じ正規化済み入力を使うため Recall@10 の
//!   数値をそのまま比較できる。engine 側は threads=1（逐次 `build` と同一
//!   グラフ）でも Recall@10 を出し、並列構築で Recall が落ちていないことを
//!   あわせて記録する。
//! - 探索レイテンシ（参考値・合否閾値なし）: 同じクエリ集合を巡回して 1
//!   クエリあたりの中央値 µs を両者で出す（engine 側は
//!   `HnswSearchScratch` を再利用、usearch 側は `search(query, 10)`）。
//!
//! 本ベンチは spec 由来の pass/fail 閾値を持たない情報提供専用ベンチのため、
//! 実測値をそのまま標準出力へ出す
//! （`.claude/rules/spec-confidentiality.md`: 数値基準・実測値はオーナー
//! 判断〔2026-08-29〕により公開可）。

#[allow(dead_code)]
mod harness;

use harness::env_report::EnvReport;
use harness::hnsw_build::generate_corpus;
use harness::hnsw_compare::hnsw_rs_adapter::{
    build_hnsw_rs_index, hnsw_rs_search_topk, max_layer_for,
    EF_CONSTRUCTION as HNSW_RS_EF_CONSTRUCTION, MAX_NB_CONNECTION as HNSW_RS_MAX_NB_CONNECTION,
};
use harness::hnsw_compare::usearch_adapter::{
    build_usearch_index_parallel, usearch_index_options, usearch_search_topk,
};
use harness::hnsw_compare::{
    average_recall_at_k, l2_normalize_corpus, ratio_self_over_hnsw_rs, ratio_self_over_usearch,
    refuse_under_github_actions, render_build_line, render_header_line, render_hnsw_rs_params_line,
    render_latency_line, render_ratio_line, render_ratio_self_over_hnsw_rs_line,
    render_recall_line, render_self_params_line, render_usearch_params_line, resolve_dim,
    resolve_queries, resolve_rows, resolve_thread_ladder, speedup, EF_SEARCH, TOP_K,
};
use harness::protocol::{run, run_bounded_retain, MeasurementConfig};

use engine::hnsw::{HnswIndex, HnswParams, HnswSearchScratch};
use engine::isa;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// engine 側 1 スレッド数点の構築時間中央値を計測する。
///
/// `protocol::run` は戻り値（構築済み索引）を計測区間の内側で drop する契約
/// （`protocol.rs::run` モジュールコメント）のため、索引解放コストが
/// `build_median` へ混入する（codex-review P2 指摘・PR #445）。索引の drop を
/// 計測区間の外側へ追い出す [`run_bounded_retain`]（`retain_capacity=1`。
/// 直前 1 件だけを保持し、次の反復で新しい索引と入れ替える際に古い索引を
/// 計測区間の外側で drop する）を使う。
fn measure_self_build(
    corpus: &[f32],
    dim: usize,
    params: HnswParams,
    threads: usize,
) -> Result<std::time::Duration, String> {
    let config = MeasurementConfig::new(20, 20, 0xC0FF_EE00 ^ threads as u64)
        .map_err(|e| format!("threads={threads}: {e}"))?;
    let (measurement, _retained) = run_bounded_retain(&config, 1, || {
        HnswIndex::build_with_threads(params, dim as u32, corpus, 1, threads)
            .expect("self build should succeed on well-formed corpus")
    })
    .map_err(|e| format!("threads={threads}: {e}"))?;
    Ok(measurement.summary.median)
}

/// usearch 側 1 スレッド数点の構築時間中央値を計測する。
///
/// [`measure_self_build`] と同じ理由で [`run_bounded_retain`] を使い、
/// usearch 側索引の解放コストも計測区間の外側へ追い出す。
fn measure_usearch_build(
    rows: usize,
    dim: usize,
    corpus: &[f32],
    threads: usize,
) -> Result<std::time::Duration, String> {
    let config = MeasurementConfig::new(20, 20, 0xBEEF_0000 ^ threads as u64)
        .map_err(|e| format!("threads={threads}: {e}"))?;
    let (measurement, _retained) = run_bounded_retain(&config, 1, || {
        build_usearch_index_parallel(rows, dim, corpus, threads)
            .expect("usearch parallel build should succeed on well-formed corpus")
    })
    .map_err(|e| format!("threads={threads}: {e}"))?;
    Ok(measurement.summary.median)
}

/// hnsw_rs 側 1 スレッド数点の構築時間中央値を計測する。`normalized_corpus`
/// は呼び出し元が [`l2_normalize_corpus`] 済みのものを渡す契約（`hnsw_rs_adapter`
/// モジュールコメント「DistDot の単位ベクトル前提」参照）。
///
/// [`measure_self_build`] と同じ理由で [`run_bounded_retain`] を使い、
/// hnsw_rs 側索引の解放コストも計測区間の外側へ追い出す。
fn measure_hnsw_rs_build(
    rows: usize,
    dim: usize,
    normalized_corpus: &[f32],
    threads: usize,
) -> Result<std::time::Duration, String> {
    let config = MeasurementConfig::new(20, 20, 0xFEED_0000 ^ threads as u64)
        .map_err(|e| format!("threads={threads}: {e}"))?;
    let (measurement, _retained) = run_bounded_retain(&config, 1, || {
        build_hnsw_rs_index(rows, dim, normalized_corpus, threads)
    })
    .map_err(|e| format!("threads={threads}: {e}"))?;
    Ok(measurement.summary.median)
}

/// 探索レイテンシ計測の回数: クエリ数の整数倍で protocol 下限（20）以上の
/// 最小値。`queries` 本を `qi % len` で巡回するため、この回数なら本計測中に
/// 全クエリが同じ回数ずつ評価される。
fn latency_iterations_for(query_count: usize) -> u32 {
    const MIN_ITERATIONS: usize = 20;
    let n = query_count.max(1);
    let multiples = MIN_ITERATIONS.div_ceil(n).max(1);
    u32::try_from(n.saturating_mul(multiples)).unwrap_or(u32::MAX)
}

fn main() {
    if let Err(e) = refuse_under_github_actions(running_under_github_actions()) {
        eprintln!("hnsw_compare_bench: {e}");
        std::process::exit(1);
    }

    let detected = isa::current().isa();
    let env = EnvReport::capture(format!("{detected:?}"));
    println!("{env}");

    let rows = resolve_rows();
    let dim = resolve_dim();
    let queries_count = resolve_queries();
    let ladder = resolve_thread_ladder();
    println!("{}", render_header_line(rows, dim, queries_count, &ladder));

    let params = HnswParams::default();
    println!(
        "{}",
        render_self_params_line(params.m, params.ef_construction, params.ef_search)
    );
    let usearch_options = usearch_index_options(dim);
    println!(
        "{}",
        render_usearch_params_line(
            usearch_options.connectivity,
            usearch_options.expansion_add,
            usearch_options.expansion_search,
        )
    );
    let hnsw_rs_max_layer = max_layer_for(rows);
    println!(
        "{}",
        render_hnsw_rs_params_line(
            HNSW_RS_MAX_NB_CONNECTION,
            HNSW_RS_EF_CONSTRUCTION,
            EF_SEARCH,
            hnsw_rs_max_layer,
        )
    );

    let raw_corpus = match generate_corpus(0xC0BA_1234 ^ rows as u64, dim, rows) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_compare_bench: corpus generation failed: {e}");
            std::process::exit(1);
        }
    };
    // クエリはコーパスとは別シードで、同じ生成器（`generate_corpus`）から
    // 「行数 = クエリ数」として生成する（モジュールコメント参照）。
    let raw_query_flat =
        match generate_corpus(0xC0BA_9999 ^ queries_count as u64, dim, queries_count) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("hnsw_compare_bench: query generation failed: {e}");
                std::process::exit(1);
            }
        };

    // 3 エンジン（self・usearch・hnsw_rs）すべてを同一条件で比較するため、
    // コーパス・クエリをベンチ冒頭で 1 回だけ L2 正規化し、以降は全エンジンへ
    // 同じ正規化済みバッファを渡す（モジュールコメント「コーパス正規化は
    // 3 エンジン共通」参照。ゼロベクトル検出時は fail-closed で中断する）。
    let corpus = match l2_normalize_corpus(&raw_corpus, dim) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_compare_bench: corpus normalization failed: {e}");
            std::process::exit(1);
        }
    };
    let query_flat = match l2_normalize_corpus(&raw_query_flat, dim) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("hnsw_compare_bench: query normalization failed: {e}");
            std::process::exit(1);
        }
    };
    let queries: Vec<&[f32]> = query_flat.chunks(dim).collect();

    let mut had_error = false;
    let mut self_baseline: Option<std::time::Duration> = None;
    let mut usearch_baseline: Option<std::time::Duration> = None;
    let mut hnsw_rs_baseline: Option<std::time::Duration> = None;
    let mut max_threads = 1usize;

    for &threads in &ladder {
        max_threads = max_threads.max(threads);

        match measure_self_build(&corpus, dim, params, threads) {
            Ok(median) => {
                if threads == 1 {
                    self_baseline = Some(median);
                }
                match speedup(self_baseline, median) {
                    Ok(sp) => println!("{}", render_build_line("self", threads, median, sp)),
                    Err(e) => {
                        eprintln!("hnsw_compare_bench: self speedup threads={threads}: {e}");
                        had_error = true;
                    }
                }

                match measure_usearch_build(rows, dim, &corpus, threads) {
                    Ok(usearch_median) => {
                        if threads == 1 {
                            usearch_baseline = Some(usearch_median);
                        }
                        match speedup(usearch_baseline, usearch_median) {
                            Ok(sp) => println!(
                                "{}",
                                render_build_line("usearch", threads, usearch_median, sp)
                            ),
                            Err(e) => {
                                eprintln!(
                                    "hnsw_compare_bench: usearch speedup threads={threads}: {e}"
                                );
                                had_error = true;
                            }
                        }

                        match ratio_self_over_usearch(median, usearch_median) {
                            Ok(ratio) => println!("{}", render_ratio_line(threads, ratio)),
                            Err(e) => {
                                eprintln!("hnsw_compare_bench: ratio threads={threads}: {e}");
                                had_error = true;
                            }
                        }

                        match measure_hnsw_rs_build(rows, dim, &corpus, threads) {
                            Ok(hnsw_rs_median) => {
                                if threads == 1 {
                                    hnsw_rs_baseline = Some(hnsw_rs_median);
                                }
                                match speedup(hnsw_rs_baseline, hnsw_rs_median) {
                                    Ok(sp) => println!(
                                        "{}",
                                        render_build_line("hnsw_rs", threads, hnsw_rs_median, sp)
                                    ),
                                    Err(e) => {
                                        eprintln!(
                                            "hnsw_compare_bench: hnsw_rs speedup threads={threads}: {e}"
                                        );
                                        had_error = true;
                                    }
                                }

                                match ratio_self_over_hnsw_rs(median, hnsw_rs_median) {
                                    Ok(ratio) => println!(
                                        "{}",
                                        render_ratio_self_over_hnsw_rs_line(threads, ratio)
                                    ),
                                    Err(e) => {
                                        eprintln!(
                                            "hnsw_compare_bench: hnsw_rs ratio threads={threads}: {e}"
                                        );
                                        had_error = true;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("hnsw_compare_bench: {e}");
                                had_error = true;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("hnsw_compare_bench: {e}");
                        had_error = true;
                    }
                }
            }
            Err(e) => {
                eprintln!("hnsw_compare_bench: {e}");
                had_error = true;
            }
        }
    }

    if had_error {
        std::process::exit(1);
    }

    // Recall@10: ラダー最大スレッド数の索引（engine・usearch・hnsw_rs とも）を
    // 単一の brute-force（`CpuScalarProvider`）対照で評価する。3 エンジンとも
    // 同じ正規化済みコーパス・クエリを使うため brute-force 対照も 1 つで済む
    // （モジュールコメント「コーパス正規化は 3 エンジン共通」参照。是正前
    // 〔PR #445〕は hnsw_rs 用に別の正規化済み brute-force 対照を取り直して
    // いたが、コーパスが全エンジン共通になったため不要になった）。engine は
    // threads=1（逐次 `build` と同一グラフ）でも評価し、並列構築で Recall が
    // 落ちていないことをあわせて記録する。
    let ids: Vec<u64> = (0..rows as u64).collect();
    let brute = CpuScalarProvider;
    let mut brute_topk: Vec<Vec<u64>> = Vec::with_capacity(queries.len());
    for query in &queries {
        match brute.search(SearchInput {
            ids: &ids,
            vectors: &corpus,
            dim: dim as u32,
            query,
            k: TOP_K,
        }) {
            Ok(hits) => brute_topk.push(hits.into_iter().map(|h| h.id).collect()),
            Err(e) => {
                eprintln!("hnsw_compare_bench: brute-force reference search failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let self_index_seq = match HnswIndex::build_with_threads(params, dim as u32, &corpus, 1, 1) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("hnsw_compare_bench: self sequential build for recall failed: {e}");
            std::process::exit(1);
        }
    };
    let self_index_max = if max_threads == 1 {
        None
    } else {
        match HnswIndex::build_with_threads(params, dim as u32, &corpus, 1, max_threads) {
            Ok(idx) => Some(idx),
            Err(e) => {
                eprintln!("hnsw_compare_bench: self max-threads build for recall failed: {e}");
                std::process::exit(1);
            }
        }
    };
    let usearch_index_max = match build_usearch_index_parallel(rows, dim, &corpus, max_threads) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("hnsw_compare_bench: usearch build for recall failed: {e}");
            std::process::exit(1);
        }
    };
    let hnsw_rs_index_max = build_hnsw_rs_index(rows, dim, &corpus, max_threads.max(1));

    let mut recall_had_error = false;
    let mut self_seq_pairs = Vec::with_capacity(queries.len());
    let mut scratch = HnswSearchScratch::default();
    for (i, query) in queries.iter().enumerate() {
        match self_index_seq.search(query, TOP_K, EF_SEARCH, &mut scratch) {
            Ok(hits) => {
                self_seq_pairs.push((
                    brute_topk[i].clone(),
                    hits.into_iter().map(|h| h.id).collect(),
                ));
            }
            Err(e) => {
                eprintln!("hnsw_compare_bench: self sequential search failed: {e}");
                recall_had_error = true;
            }
        }
    }
    match average_recall_at_k(&self_seq_pairs) {
        Ok(recall) => println!("{}", render_recall_line("self", 1, recall)),
        Err(e) => {
            eprintln!("hnsw_compare_bench: self sequential recall: {e}");
            recall_had_error = true;
        }
    }

    if let Some(self_index_max) = &self_index_max {
        let mut self_max_pairs = Vec::with_capacity(queries.len());
        for (i, query) in queries.iter().enumerate() {
            match self_index_max.search(query, TOP_K, EF_SEARCH, &mut scratch) {
                Ok(hits) => {
                    self_max_pairs.push((
                        brute_topk[i].clone(),
                        hits.into_iter().map(|h| h.id).collect(),
                    ));
                }
                Err(e) => {
                    eprintln!("hnsw_compare_bench: self max-threads search failed: {e}");
                    recall_had_error = true;
                }
            }
        }
        match average_recall_at_k(&self_max_pairs) {
            Ok(recall) => println!("{}", render_recall_line("self", max_threads, recall)),
            Err(e) => {
                eprintln!("hnsw_compare_bench: self max-threads recall: {e}");
                recall_had_error = true;
            }
        }
    }

    let mut usearch_pairs = Vec::with_capacity(queries.len());
    for (i, query) in queries.iter().enumerate() {
        match usearch_search_topk(&usearch_index_max, query, TOP_K) {
            Ok(hit_ids) => usearch_pairs.push((brute_topk[i].clone(), hit_ids)),
            Err(e) => {
                eprintln!("hnsw_compare_bench: usearch search failed: {e}");
                recall_had_error = true;
            }
        }
    }
    match average_recall_at_k(&usearch_pairs) {
        Ok(recall) => println!("{}", render_recall_line("usearch", max_threads, recall)),
        Err(e) => {
            eprintln!("hnsw_compare_bench: usearch recall: {e}");
            recall_had_error = true;
        }
    }

    // hnsw_rs の Recall@10 も self・usearch と同じ正規化済みコーパス・クエリ・
    // `brute_topk` 基準（モジュールコメント参照。3 エンジンとも同一入力）。
    let mut hnsw_rs_pairs = Vec::with_capacity(queries.len());
    for (i, query) in queries.iter().enumerate() {
        match hnsw_rs_search_topk(&hnsw_rs_index_max, query, TOP_K, EF_SEARCH) {
            Ok(hit_ids) => hnsw_rs_pairs.push((brute_topk[i].clone(), hit_ids)),
            Err(e) => {
                eprintln!("hnsw_compare_bench: hnsw_rs search failed: {e}");
                recall_had_error = true;
            }
        }
    }
    match average_recall_at_k(&hnsw_rs_pairs) {
        Ok(recall) => println!("{}", render_recall_line("hnsw_rs", max_threads, recall)),
        Err(e) => {
            eprintln!("hnsw_compare_bench: hnsw_rs recall: {e}");
            recall_had_error = true;
        }
    }

    if recall_had_error {
        std::process::exit(1);
    }

    // 探索レイテンシ（参考値）: 同じクエリ集合を巡回し、1 クエリあたりの
    // 所要時間中央値を両者で出す。合否閾値は持たない情報提供専用の計測。
    // warmup・本計測とも回数をクエリ数の整数倍（protocol 下限 20 以上）に
    // 合わせ、全クエリが均等に評価されるようにする（codex-review 指摘・PR #445。
    // 固定 20 回では既定 queries=200 のうち 20 件しか計測されない）。
    let latency_index = self_index_max.as_ref().unwrap_or(&self_index_seq);
    let latency_iterations = latency_iterations_for(queries.len());
    let latency_config =
        match MeasurementConfig::new(latency_iterations, latency_iterations, 0xFACE_CAFE) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("hnsw_compare_bench: latency config: {e}");
                std::process::exit(1);
            }
        };

    let mut self_qi = 0usize;
    let self_latency = run(&latency_config, || {
        let query = queries[self_qi % queries.len()];
        self_qi += 1;
        latency_index
            .search(query, TOP_K, EF_SEARCH, &mut scratch)
            .expect("self search must succeed for well-formed synthetic input")
            .len()
    });
    match self_latency {
        Ok(m) => println!("{}", render_latency_line("self", m.summary.median)),
        Err(e) => {
            eprintln!("hnsw_compare_bench: self search latency: {e}");
            std::process::exit(1);
        }
    }

    let mut usearch_qi = 0usize;
    let usearch_latency = run(&latency_config, || {
        let query = queries[usearch_qi % queries.len()];
        usearch_qi += 1;
        usearch_search_topk(&usearch_index_max, query, TOP_K)
            .expect("usearch search must succeed for well-formed synthetic input")
            .len()
    });
    match usearch_latency {
        Ok(m) => println!("{}", render_latency_line("usearch", m.summary.median)),
        Err(e) => {
            eprintln!("hnsw_compare_bench: usearch search latency: {e}");
            std::process::exit(1);
        }
    }

    // hnsw_rs も self・usearch と同じ正規化済みクエリ・`latency_config` を使う。
    let mut hnsw_rs_qi = 0usize;
    let hnsw_rs_latency = run(&latency_config, || {
        let query = queries[hnsw_rs_qi % queries.len()];
        hnsw_rs_qi += 1;
        hnsw_rs_search_topk(&hnsw_rs_index_max, query, TOP_K, EF_SEARCH)
            .expect("hnsw_rs search must succeed for well-formed synthetic input")
            .len()
    });
    match hnsw_rs_latency {
        Ok(m) => println!("{}", render_latency_line("hnsw_rs", m.summary.median)),
        Err(e) => {
            eprintln!("hnsw_compare_bench: hnsw_rs search latency: {e}");
            std::process::exit(1);
        }
    }
}
