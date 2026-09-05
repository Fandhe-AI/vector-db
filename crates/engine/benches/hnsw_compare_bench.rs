//! 自作 HNSW（`engine::hnsw::HnswIndex`）と外部フレームワーク usearch（承認済み
//! optional 依存 `usearch =2.26.1`・`contrast-bench` feature 限定）の構築時間
//! （スレッド数ラダー）・Recall@10・探索レイテンシを同一条件で比較する手動専用
//! ベンチ（Issue #402 系 ADR `docs/design/ann-index-adoption.md` の実測補強。
//! `docs/spec/05-tasks.md` TASK-132 のポインタ）。
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
//!   Top-10 を取り brute-force（`engine::kernel::CpuScalarProvider`。内積
//!   最大）対照で Recall@10 平均を出す（`harness::accept::recall_at_k` は
//!   id の集合演算のみで判定するため、同点近傍の内部順序入れ替わりは
//!   Recall を過小評価しない。連続一様分布の合成ベクトルでは Top-k 境界での
//!   完全な同点はほぼ発生しないため、本ベンチでは同点の特別扱いをしない）。
//!   engine 側は threads=1（逐次 `build` と同一グラフ）でも Recall@10 を
//!   出し、並列構築で Recall が落ちていないことをあわせて記録する。
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
use harness::hnsw_compare::usearch_adapter::{
    build_usearch_index_parallel, usearch_index_options, usearch_search_topk,
};
use harness::hnsw_compare::{
    average_recall_at_k, ratio_self_over_usearch, refuse_under_github_actions, render_build_line,
    render_header_line, render_latency_line, render_ratio_line, render_recall_line,
    render_self_params_line, render_usearch_params_line, resolve_dim, resolve_queries,
    resolve_rows, resolve_thread_ladder, speedup, EF_SEARCH, TOP_K,
};
use harness::protocol::{run, MeasurementConfig};

use engine::hnsw::{HnswIndex, HnswParams, HnswSearchScratch};
use engine::isa;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// engine 側 1 スレッド数点の構築時間中央値を計測する。
fn measure_self_build(
    corpus: &[f32],
    dim: usize,
    params: HnswParams,
    threads: usize,
) -> Result<std::time::Duration, String> {
    let config = MeasurementConfig::new(20, 20, 0xC0FF_EE00 ^ threads as u64)
        .map_err(|e| format!("threads={threads}: {e}"))?;
    let measurement = run(&config, || {
        HnswIndex::build_with_threads(params, dim as u32, corpus, 1, threads)
            .expect("self build should succeed on well-formed corpus")
    })
    .map_err(|e| format!("threads={threads}: {e}"))?;
    Ok(measurement.summary.median)
}

/// usearch 側 1 スレッド数点の構築時間中央値を計測する。
fn measure_usearch_build(
    rows: usize,
    dim: usize,
    corpus: &[f32],
    threads: usize,
) -> Result<std::time::Duration, String> {
    let config = MeasurementConfig::new(20, 20, 0xBEEF_0000 ^ threads as u64)
        .map_err(|e| format!("threads={threads}: {e}"))?;
    let measurement = run(&config, || {
        build_usearch_index_parallel(rows, dim, corpus, threads)
            .expect("usearch parallel build should succeed on well-formed corpus")
    })
    .map_err(|e| format!("threads={threads}: {e}"))?;
    Ok(measurement.summary.median)
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

    let corpus = match generate_corpus(0xC0BA_1234 ^ rows as u64, dim, rows) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hnsw_compare_bench: corpus generation failed: {e}");
            std::process::exit(1);
        }
    };
    // クエリはコーパスとは別シードで、同じ生成器（`generate_corpus`）から
    // 「行数 = クエリ数」として生成する（モジュールコメント参照）。
    let query_flat = match generate_corpus(0xC0BA_9999 ^ queries_count as u64, dim, queries_count) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("hnsw_compare_bench: query generation failed: {e}");
            std::process::exit(1);
        }
    };
    let queries: Vec<&[f32]> = query_flat.chunks(dim).collect();

    let mut had_error = false;
    let mut self_baseline: Option<std::time::Duration> = None;
    let mut usearch_baseline: Option<std::time::Duration> = None;
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

    // Recall@10: ラダー最大スレッド数の索引（engine・usearch とも）を
    // brute-force（`CpuScalarProvider`）対照で評価する。engine は threads=1
    // （逐次 `build` と同一グラフ）でも評価し、並列構築で Recall が落ちて
    // いないことをあわせて記録する（モジュールコメント参照）。
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

    if recall_had_error {
        std::process::exit(1);
    }

    // 探索レイテンシ（参考値）: 同じクエリ集合を巡回し、1 クエリあたりの
    // 所要時間中央値を両者で出す。合否閾値は持たない情報提供専用の計測。
    let latency_index = self_index_max.as_ref().unwrap_or(&self_index_seq);
    let latency_config = match MeasurementConfig::new(20, 20, 0xFACE_CAFE) {
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
}
