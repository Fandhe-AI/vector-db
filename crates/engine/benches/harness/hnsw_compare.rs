//! 自作 HNSW（`engine::hnsw::HnswIndex`）と外部フレームワーク usearch（承認済み
//! optional 依存 `usearch =2.26.1`・`contrast-bench` feature 限定）の構築時間
//! （スレッド数ラダー）・Recall@10・探索レイテンシを同一条件で比較する
//! `benches/hnsw_compare_bench.rs` が使う時間非依存ヘルパ。
//!
//! `benches/hnsw_build.rs`（TASK-132・Issue #404）・`harness/contrast.rs`
//! （TASK-127 CORE-5・Issue #176）と同じく、時間依存の実測本体（`hnsw_compare_bench.rs`。
//! `make ci` 対象外）と時間非依存のロジック（本ファイル。`tests/hnsw_compare_accept.rs`
//! から `#[path]` で取り込み `make ci` 対象）を分離する。
//!
//! usearch 依存の部分（[`UsearchBuildOptions`]・[`build_usearch_index_parallel`]・
//! [`usearch_search_topk`]）は `contrast-bench` feature 限定で
//! `harness::contrast`（Issue #176）と同様の理由——feature 無効時は usearch の
//! C++ FFI を含む依存を一切コンパイル対象に入れない——により `#[cfg(feature =
//! "contrast-bench")]` を付ける。env 解析・構築時間比率・Recall 集計等の
//! フレームワーク非依存ロジックは feature の有無に関わらずコンパイル・
//! テストできるようにする（本タスクの検証観点「usearch に依存しない純粋部分は
//! feature なしでもテストが回るよう分離」に対応）。
//!
//! # 暗号用途禁止
//!
//! [`super::rng::DeterministicRng`] を経由する [`super::hnsw_build::generate_corpus`]
//! を使うため非暗号 PRNG である。ベンチ入力生成専用。

use std::fmt;
use std::time::Duration;

use engine::hnsw::MAX_BUILD_THREADS;

/// [`resolve_rows`] が許容する行数の上限（DoS 防止・上限検証。
/// `harness::hnsw_build::MAX_CORPUS_ROWS_GUARD` と同一方針の固定上限）。
pub const MAX_ROWS_GUARD: usize = 200_000;

/// [`resolve_dim`] が受理する次元。usearch 側・engine 側の双方で同一次元を使う
/// 契約のため、任意値ではなく代表的な 2 点（64・128）のみを受理する
/// （`.claude/rules/coding-rust.md` の上限検証方針に加え、比較対象を絞ることで
/// 出力の読みやすさを保つ）。
pub const ALLOWED_DIMS: [usize; 2] = [64, 128];

/// [`resolve_queries`] が許容するクエリ数の上限（DoS 防止・上限検証）。
pub const MAX_QUERIES_GUARD: usize = 2_000;

/// 既定の行数（`BENCH_HNSW_COMPARE_ROWS` 未設定・不正値時のフォールバック）。
pub const DEFAULT_ROWS: usize = 100_000;

/// 既定の次元（`BENCH_HNSW_COMPARE_DIM` 未設定・不正値時のフォールバック）。
pub const DEFAULT_DIM: usize = 64;

/// 既定のクエリ数（`BENCH_HNSW_COMPARE_QUERIES` 未設定・不正値時のフォールバック）。
pub const DEFAULT_QUERIES: usize = 200;

/// Top-k 探索の `k`（受け入れ条件が固定する値。env での上書きは対象外）。
pub const TOP_K: usize = 10;

/// 探索時の候補幅 `ef_search`（engine 側 [`engine::hnsw::HnswParams::default`]・
/// usearch 側 `expansion_search` の双方に使う共通値。呼び出し元の
/// `hnsw_compare_bench.rs` が単一の情報源として本定数を参照する）。
pub const EF_SEARCH: usize = 64;

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq)]
pub enum HnswCompareBenchError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された。
    RefusedUnderGitHubActions,
    /// 実効指数・比率計算に必要な所要時間が 0 以下、または分母が 0。
    InsufficientSamples,
    /// [`l2_normalize_corpus`] でノルムが 0（全成分 0）の行を検出した
    /// （fail-closed。当該行のインデックスを保持する）。
    ZeroNormRow(usize),
}

impl fmt::Display for HnswCompareBenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HnswCompareBenchError::RefusedUnderGitHubActions => write!(
                f,
                "hnsw_compare_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); \
                 this bench is manual-only and not wired into any workflow"
            ),
            HnswCompareBenchError::InsufficientSamples => {
                write!(f, "need positive, non-zero durations to compute a ratio")
            }
            HnswCompareBenchError::ZeroNormRow(row) => write!(
                f,
                "corpus row {row} has zero L2 norm and cannot be normalized \
                 (refusing rather than dividing by zero / emitting NaN)"
            ),
        }
    }
}

impl std::error::Error for HnswCompareBenchError {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`harness::hnsw_build::
/// refuse_under_github_actions` と同一パターン）。
pub fn refuse_under_github_actions(
    under_github_actions: bool,
) -> Result<(), HnswCompareBenchError> {
    if under_github_actions {
        return Err(HnswCompareBenchError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// `BENCH_HNSW_COMPARE_ROWS` の生文字列を読み、`1..=MAX_ROWS_GUARD` の範囲で
/// 検証する。未設定・不正値・範囲外は [`DEFAULT_ROWS`] へフォールバックする
/// （時間依存ベンチの入力なので fail-closed に拒否するより既定値へ倒す方が
/// 運用上有用。`hnsw_parallel_build_bench.rs::resolve_rows` と同一方針）。
pub fn parse_rows(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=MAX_ROWS_GUARD).contains(&n))
        .unwrap_or(DEFAULT_ROWS)
}

/// `BENCH_HNSW_COMPARE_DIM` の生文字列を読み、[`ALLOWED_DIMS`]（`{64, 128}`）の
/// いずれかのみを受理する。未設定・不正値・非許容値は [`DEFAULT_DIM`] へ
/// フォールバックする（[`parse_rows`] と同一方針）。
pub fn parse_dim(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|n| ALLOWED_DIMS.contains(n))
        .unwrap_or(DEFAULT_DIM)
}

/// `BENCH_HNSW_COMPARE_QUERIES` の生文字列を読み、`1..=MAX_QUERIES_GUARD` の
/// 範囲で検証する。未設定・不正値・範囲外は [`DEFAULT_QUERIES`] へフォール
/// バックする（[`parse_rows`] と同一方針）。
pub fn parse_queries(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=MAX_QUERIES_GUARD).contains(&n))
        .unwrap_or(DEFAULT_QUERIES)
}

/// `BENCH_HNSW_COMPARE_THREADS`（カンマ区切り）を読み、`1..=max_threads` に
/// 検証したうえで昇順・重複なしに正規化する。未設定・全滅時は
/// `[1, 2, 4, 8, .., available_parallelism]`（`max_threads` でクランプ）を
/// 既定ラダーとする（`hnsw_parallel_build_bench.rs::resolve_thread_ladder` と
/// 同一方針。`available_parallelism` は呼び出し元から渡す——env 依存の実行時
/// 値をテスト対象の純粋関数へ直接埋め込まないため）。
pub fn parse_thread_ladder(
    raw: Option<&str>,
    max_threads: usize,
    available_parallelism: usize,
) -> Vec<usize> {
    if let Some(raw) = raw {
        let mut values: Vec<usize> = raw
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .filter(|&t| (1..=max_threads).contains(&t))
            .collect();
        values.sort_unstable();
        values.dedup();
        if !values.is_empty() {
            return values;
        }
    }

    let available = available_parallelism.max(1).min(max_threads);
    let mut ladder = vec![1usize];
    let mut t = 2usize;
    while t < available {
        ladder.push(t);
        t *= 2;
    }
    if available > 1 {
        ladder.push(available);
    }
    ladder.sort_unstable();
    ladder.dedup();
    ladder
}

/// 環境から行数を解決する（`std::env::var` 経由。[`parse_rows`] のうすいラッパー）。
pub fn resolve_rows() -> usize {
    parse_rows(std::env::var("BENCH_HNSW_COMPARE_ROWS").ok().as_deref())
}

/// 環境から次元を解決する（[`parse_dim`] のうすいラッパー）。
pub fn resolve_dim() -> usize {
    parse_dim(std::env::var("BENCH_HNSW_COMPARE_DIM").ok().as_deref())
}

/// 環境からクエリ数を解決する（[`parse_queries`] のうすいラッパー）。
pub fn resolve_queries() -> usize {
    parse_queries(std::env::var("BENCH_HNSW_COMPARE_QUERIES").ok().as_deref())
}

/// 環境からスレッド数ラダーを解決する（[`parse_thread_ladder`] のうすいラッパー。
/// `available_parallelism` は `std::thread::available_parallelism()` から得る。
/// 取得失敗時は 1 に倒す——`hnsw_parallel_build_bench.rs::resolve_thread_ladder`
/// と同一方針）。
pub fn resolve_thread_ladder() -> Vec<usize> {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    parse_thread_ladder(
        std::env::var("BENCH_HNSW_COMPARE_THREADS").ok().as_deref(),
        MAX_BUILD_THREADS,
        available,
    )
}

/// `median(1)` に対する `median(threads)` の高速化率（`speedup = baseline /
/// current`。`baseline` が `None`（threads=1 の計測がまだ無い）場合は 1.0
/// （speedup なし）を返す。`current` が 0 以下の場合は
/// [`HnswCompareBenchError::InsufficientSamples`]。
pub fn speedup(
    baseline: Option<Duration>,
    current: Duration,
) -> Result<f64, HnswCompareBenchError> {
    let current_secs = current.as_secs_f64();
    if current_secs <= 0.0 {
        return Err(HnswCompareBenchError::InsufficientSamples);
    }
    Ok(match baseline {
        Some(b) => b.as_secs_f64() / current_secs,
        None => 1.0,
    })
}

/// 同一スレッド数点での「自作エンジン所要時間 ÷ usearch 所要時間」比率。
/// 1.0 未満は自作エンジンが速い、1.0 超過は usearch が速いことを表す。
pub fn ratio_self_over_usearch(
    self_median: Duration,
    usearch_median: Duration,
) -> Result<f64, HnswCompareBenchError> {
    let usearch_secs = usearch_median.as_secs_f64();
    if usearch_secs <= 0.0 || self_median.as_secs_f64() <= 0.0 {
        return Err(HnswCompareBenchError::InsufficientSamples);
    }
    Ok(self_median.as_secs_f64() / usearch_secs)
}

/// クエリ 1 件分の Top-k 比較（brute-force 対照 `expected_ids` と被検
/// `actual_ids` の id 集合一致率）を [`super::accept::recall_at_k`] に委譲し、
/// 複数クエリの平均を返す（`tests/hnsw_search.rs::recall_at_10` と同型だが、
/// 呼び出し元がクエリごとに brute-force・被検双方の Top-k を計算済みで
/// 渡す設計にすることで、本関数自体は engine/usearch いずれの検索 API にも
/// 依存しない——[`super::accept::recall_at_k`] は id の集合演算のみで判定し、
/// 同点近傍の内部順序入れ替わりは Recall を過小評価しない
/// （`harness/accept.rs::recall_at_k` ドキュメンテーションコメント参照。
/// 連続一様分布の合成ベクトルでは Top-k 境界での完全な同点はほぼ発生しない
/// ため、この平均化方式で同点特有の歪みを個別に扱う必要はないと判断した））。
///
/// `per_query` が空の場合は [`HnswCompareBenchError::InsufficientSamples`]。
pub fn average_recall_at_k(
    per_query: &[(Vec<u64>, Vec<u64>)],
) -> Result<f64, HnswCompareBenchError> {
    if per_query.is_empty() {
        return Err(HnswCompareBenchError::InsufficientSamples);
    }
    let mut total = 0.0f64;
    for (expected, actual) in per_query {
        let recall = super::accept::recall_at_k(expected, actual)
            .map_err(|_| HnswCompareBenchError::InsufficientSamples)?;
        total += recall;
    }
    Ok(total / per_query.len() as f64)
}

/// 構築時間 1 行分の出力整形（`hnsw_compare: engine=<engine> threads=<T>
/// build_median=<ms>ms speedup=<x>x`）。
pub fn render_build_line(engine: &str, threads: usize, median: Duration, speedup: f64) -> String {
    format!(
        "hnsw_compare: engine={engine} threads={threads} build_median={:.3}ms speedup={speedup:.3}x",
        median.as_secs_f64() * 1e3
    )
}

/// 同一スレッド数点の比率 1 行分の出力整形（`hnsw_compare: ratio threads=<T>
/// self_over_usearch=<x>x`）。
pub fn render_ratio_line(threads: usize, ratio: f64) -> String {
    format!("hnsw_compare: ratio threads={threads} self_over_usearch={ratio:.3}x")
}

/// Recall@10 1 行分の出力整形（`hnsw_compare: recall engine=<engine>
/// threads=<T> recall@10=<0.xxxx>`）。
pub fn render_recall_line(engine: &str, threads: usize, recall: f64) -> String {
    format!("hnsw_compare: recall engine={engine} threads={threads} recall@10={recall:.4}")
}

/// 探索レイテンシ 1 行分の出力整形（`hnsw_compare: search_latency
/// engine=<engine> median_us=<us>`）。
pub fn render_latency_line(engine: &str, median: Duration) -> String {
    format!(
        "hnsw_compare: search_latency engine={engine} median_us={:.3}",
        median.as_secs_f64() * 1e6
    )
}

/// engine 側パラメータ 1 行分の出力整形（`params engine=self ...`）。
pub fn render_self_params_line(m: usize, ef_construction: usize, ef_search: usize) -> String {
    format!(
        "hnsw_compare: params engine=self m={m} ef_construction={ef_construction} ef_search={ef_search}"
    )
}

/// usearch 側パラメータ 1 行分の出力整形（`params engine=usearch ...`）。
pub fn render_usearch_params_line(
    connectivity: usize,
    expansion_add: usize,
    expansion_search: usize,
) -> String {
    format!(
        "hnsw_compare: params engine=usearch connectivity={connectivity} \
         expansion_add={expansion_add} expansion_search={expansion_search} \
         metric=IP quantization=F32 multi=false"
    )
}

/// hnsw_rs 側パラメータ 1 行分の出力整形（`params engine=hnsw_rs ...`）。
/// `max_layer` は [`hnsw_rs_adapter::max_layer_for`]（行数依存のためコーパス
/// 行数が確定してから呼び出し元が計算する）の結果を渡す契約。
/// `dist=DistDot(simdeez_f)` の `simdeez_f` は `Cargo.toml` で有効化した
/// hnsw_rs（推移依存 `anndists`）の SIMD 距離計算 feature 名（実行時 CPU
/// 検出。self 側 `isa.rs` の AVX2+FMA dot・usearch 側の SIMD 内蔵と対照条件を
/// 揃える目的で有効化している）を明示し、比較条件の記録漏れを防ぐ。
pub fn render_hnsw_rs_params_line(
    max_nb_connection: usize,
    ef_construction: usize,
    ef_search: usize,
    max_layer: usize,
) -> String {
    format!(
        "hnsw_compare: params engine=hnsw_rs max_nb_connection={max_nb_connection} \
         ef_construction={ef_construction} ef_search={ef_search} max_layer={max_layer} \
         dist=DistDot(simdeez_f)"
    )
}

/// 同一スレッド数点の「自作エンジン所要時間 ÷ hnsw_rs 所要時間」比率。
/// [`ratio_self_over_usearch`] と同一の計算方針（1.0 未満は自作エンジンが
/// 速いことを表す）だが、出力ラベル（`self_over_hnsw_rs`）を区別するため
/// 独立した関数として持つ（呼び出し元 `hnsw_compare_bench.rs` の可読性のため）。
pub fn ratio_self_over_hnsw_rs(
    self_median: Duration,
    hnsw_rs_median: Duration,
) -> Result<f64, HnswCompareBenchError> {
    let hnsw_rs_secs = hnsw_rs_median.as_secs_f64();
    if hnsw_rs_secs <= 0.0 || self_median.as_secs_f64() <= 0.0 {
        return Err(HnswCompareBenchError::InsufficientSamples);
    }
    Ok(self_median.as_secs_f64() / hnsw_rs_secs)
}

/// 同一スレッド数点の比率 1 行分の出力整形（`hnsw_compare: ratio threads=<T>
/// self_over_hnsw_rs=<x>x`）。
pub fn render_ratio_self_over_hnsw_rs_line(threads: usize, ratio: f64) -> String {
    format!("hnsw_compare: ratio threads={threads} self_over_hnsw_rs={ratio:.3}x")
}

/// 実行条件のヘッダ 1 行分の出力整形。3 エンジン（self・usearch・hnsw_rs）は
/// すべて [`l2_normalize_corpus`] 済みの同一コーパス・クエリで比較する
/// （是正前〔PR #445〕は hnsw_rs だけが正規化済み・self/usearch は無正規化と
/// いう非対称条件だったため実測値を単純比較できなかった。是正後の本行には
/// 常に `corpus=l2_normalized` が付き、条件の違いを実測値の隣で明示する）。
pub fn render_header_line(rows: usize, dim: usize, queries: usize, ladder: &[usize]) -> String {
    format!(
        "hnsw_compare: rows={rows} dim={dim} queries={queries} thread_ladder={ladder:?} \
         corpus=l2_normalized"
    )
}

/// 3 エンジン（self・usearch・hnsw_rs）共通のコーパス正規化ヘルパ。
///
/// `vectors`（`rows * dim` の row-major フラットバッファ）の各行を L2 単位
/// ノルムへ正規化した新しいバッファを返す。正規化後は内積の最大化と
/// コサイン類似度の最大化が一致するため、self（`kernel::dot`）・usearch
/// （`MetricKind::IP`）・hnsw_rs（`DistDot`。単位ベクトル前提は
/// `hnsw_rs_adapter` モジュールコメント「DistDot の単位ベクトル前提」参照）の
/// いずれの距離契約も同時に満たす同一入力になる。
///
/// ベンチ冒頭で本関数を 1 回だけ呼び出し、以降は全エンジンへ同じ正規化済み
/// コーパス・クエリを渡す契約にする（3 エンジンを同一入力で比較する
/// ための是正。PR #445 時点は hnsw_rs だけ正規化していたため条件が異なっていた）。
///
/// ノルムが 0（全成分 0 の行）の場合は 0 除算・NaN 混入を招くため、
/// その行を残すのではなく fail-closed で拒否する
/// （[`HnswCompareBenchError::ZeroNormRow`]）。本ベンチの一様乱数コーパス
/// （`harness::hnsw_build::generate_corpus`）では実質発生しないが、決定的
/// とはいえ生成器の変更で発生しうるため呼び出し元（`hnsw_compare_bench.rs`）
/// はこの `Err` を握りつぶさず `exit(1)` する契約とする。
pub fn l2_normalize_corpus(vectors: &[f32], dim: usize) -> Result<Vec<f32>, HnswCompareBenchError> {
    let step = dim.max(1);
    let mut out = Vec::with_capacity(vectors.len());
    for (row_idx, chunk) in vectors.chunks(step).enumerate() {
        let norm_sq: f32 = chunk.iter().map(|v| v * v).sum();
        let norm = norm_sq.sqrt();
        if norm > 0.0 {
            out.extend(chunk.iter().map(|v| v / norm));
        } else {
            return Err(HnswCompareBenchError::ZeroNormRow(row_idx));
        }
    }
    Ok(out)
}

/// `rows` 件を `threads` ワーカーへ連続範囲で静的分割した `[start, end)` の
/// 一覧を返す（usearch 側の並列 `add` 分割・engine 側の対称性検証で共有する
/// 純粋ロジック。`threads == 0` は空の分割を返す——呼び出し元が `threads >= 1`
/// を別途検証する契約）。余りは前方のワーカーへ 1 件ずつ多く割り当てる。
pub fn partition_rows(rows: usize, threads: usize) -> Vec<(usize, usize)> {
    if threads == 0 {
        return Vec::new();
    }
    let base = rows / threads;
    let remainder = rows % threads;
    let mut out = Vec::with_capacity(threads);
    let mut start = 0usize;
    for i in 0..threads {
        let extra = if i < remainder { 1 } else { 0 };
        let end = start + base + extra;
        out.push((start, end));
        start = end;
    }
    out
}

#[cfg(feature = "contrast-bench")]
pub mod usearch_adapter {
    //! usearch 依存部分（`contrast-bench` feature 限定）。
    //!
    //! [`super::partition_rows`] による静的行分割を使い、`usearch::Index` へ
    //! `threads` 本のワーカーで並列 `add` する構築手順を提供する。
    //! `usearch::Index` は `Send + Sync`（`usearch` crate `rust/lib.rs`
    //! 673-674 行目の `unsafe impl Send for Index {}` / `unsafe impl Sync for
    //! Index {}`）であるため、`std::thread::scope` の各ワーカーへ `&Index` を
    //! そのまま共有でき、`Arc` は不要（本クレート側で `unsafe` を書く必要は
    //! ない。`.claude/rules/coding-rust.md` の `unsafe` 原則禁止に抵触しない）。

    use super::super::stats::BenchError;
    use super::partition_rows;
    use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

    /// engine 側 [`engine::hnsw::HnswParams::default`]（m=16・ef_construction=100・
    /// ef_search=64。Issue #403 の本リポ採用既定値）とパラメータの意味を対応
    /// させた usearch 側の構築オプション。`connectivity` は `m` に、
    /// `expansion_add` は `ef_construction` に、`expansion_search` は
    /// `ef_search`（[`super::EF_SEARCH`]）に、それぞれ対応する
    /// （`include/usearch/index_dense.hpp` のコメント・`usearch::IndexOptions`
    /// フィールド定義参照）。`contrast_bench.rs`（`harness/contrast.rs`）の
    /// `connectivity=3, expansion_add=4` は厳密最近傍（`exact_search`）用の
    /// ビルド時間短縮目的の最小値であり、本比較（近似探索の構築品質・速度
    /// 比較）には流用しない。
    pub fn usearch_index_options(dim: usize) -> IndexOptions {
        IndexOptions {
            dimensions: dim,
            metric: MetricKind::IP,
            quantization: ScalarKind::F32,
            connectivity: 16,
            expansion_add: 100,
            expansion_search: super::EF_SEARCH,
            multi: false,
        }
    }

    /// `rows` 件の `vectors`（`rows * dim` 要素の row-major 連続バッファ）から
    /// `threads` 本のワーカーで並列に `add`（key は行番号 `u64`）して usearch
    /// インデックスを構築する。
    ///
    /// 手順（`Index::new` → `reserve_capacity_and_threads` → 並列 `add`）は
    /// engine 側 [`engine::hnsw::HnswIndex::build_with_threads`] が毎回の呼び
    /// 出しで `Arc::from` によるスナップショットコピー等の初期化コストを
    /// 含めて計測しているのと対称にするため、`Index::new` からこの関数の
    /// 内側で行い呼び出し元（`hnsw_compare_bench.rs`）の計測区間に含める。
    ///
    /// `add` の `Err` はいずれかのワーカーで発生した場合でも panic させず、
    /// 全ワーカー完了後に集約して `Result` として返す（coding-rust.md:
    /// ライブラリコードで `Result` を返す方針。本コードは bench 専用だが
    /// 同一の防御規律を保つ）。
    pub fn build_usearch_index_parallel(
        rows: usize,
        dim: usize,
        vectors: &[f32],
        threads: usize,
    ) -> Result<Index, BenchError> {
        let options = usearch_index_options(dim);
        let index = Index::new(&options).map_err(|err| {
            BenchError::ExternalEngine(format!("usearch Index::new failed: {err}"))
        })?;
        index
            .reserve_capacity_and_threads(rows, threads.max(1))
            .map_err(|err| {
                BenchError::ExternalEngine(format!(
                    "usearch reserve_capacity_and_threads failed: {err}"
                ))
            })?;

        let ranges = partition_rows(rows, threads.max(1));
        let errors = std::sync::Mutex::new(Vec::<String>::new());
        std::thread::scope(|scope| {
            for (start, end) in ranges {
                let index_ref = &index;
                let errors_ref = &errors;
                scope.spawn(move || {
                    for row in start..end {
                        let vec_start = row * dim;
                        let vec_end = vec_start + dim;
                        let Some(vector) = vectors.get(vec_start..vec_end) else {
                            if let Ok(mut guard) = errors_ref.lock() {
                                guard.push(format!("vectors slice out of bounds for row {row}"));
                            }
                            continue;
                        };
                        if let Err(err) = index_ref.add(row as u64, vector) {
                            if let Ok(mut guard) = errors_ref.lock() {
                                guard.push(format!("usearch add failed for row {row}: {err}"));
                            }
                        }
                    }
                });
            }
        });

        let errors = errors.into_inner().unwrap_or_default();
        if !errors.is_empty() {
            return Err(BenchError::ExternalEngine(format!(
                "usearch parallel add failed for {} row(s); first error: {}",
                errors.len(),
                errors[0]
            )));
        }
        Ok(index)
    }

    /// `query` に対する Top-`k` 近似最近傍 id 列を、usearch の `search`（近似
    /// 探索。`exact_search` ではない——`harness::contrast::ContrastIndex` は
    /// 厳密最近傍比較用で本比較の対象ではない）で返す。
    pub fn usearch_search_topk(
        index: &Index,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<u64>, BenchError> {
        let matches = index
            .search(query, k)
            .map_err(|err| BenchError::ExternalEngine(format!("usearch search failed: {err}")))?;
        Ok(matches.keys)
    }
}

#[cfg(feature = "contrast-bench")]
pub mod hnsw_rs_adapter {
    //! hnsw_rs（純 Rust HNSW。承認済み optional 依存 `hnsw_rs =0.3.4`・
    //! `contrast-bench` feature 限定。オーナー承認 2026-09-05）依存部分。
    //! [`super::usearch_adapter`] と同型の構築・探索アダプタを提供する。
    //!
    //! # DistDot の単位ベクトル前提
    //!
    //! `anndists =0.1.5`（hnsw_rs の推移依存）の `dist::distances::DistDot::eval`
    //! （`src/dist/distances.rs` 258-281 行目付近）は `1 - dot(va, vb)` を返し、
    //! コーシー・シュワルツにより単位ベクトル同士の内積が高々 1 であることを
    //! 前提に `assert!(dot >= 0.)`（`scalar_dot_f32`。同ファイル 272-281 行目）
    //! で検証する。本ベンチのコーパス（`harness::hnsw_build::generate_corpus`。
    //! 各成分が `[-1, 1)` の一様乱数でノルムは不揃い）を無正規化のまま渡すと
    //! 内積が 1 を超えて assert パニック（プロセス強制終了）になりうる。
    //! hnsw_rs 公式サンプル `examples/ann-glove25-angular.rs`
    //! （74-75 行目のコメント「pre normalisation to use Dot computations
    //! instead of Cosine」・`anndata.do_l2_normalization()` 呼び出し）も
    //! 同じ前提を裏付ける。
    //!
    //! そのため呼び出し元（`hnsw_compare_bench.rs`）はベンチ冒頭で
    //! [`super::l2_normalize_corpus`] を 1 回呼び出し、self・usearch・hnsw_rs
    //! の 3 エンジンすべてへ同じ正規化済みコーパス・クエリを渡す（是正前
    //! 〔PR #445〕は hnsw_rs だけ正規化・self/usearch は無正規化という非対称
    //! 条件だったため、3 エンジンの Recall・構築時間が同一入力に基づかず
    //! 単純比較できなかった。是正後は正規化後の内積〔＝コサイン類似度〕の
    //! 最大化が生内積の最大化と一致するため、usearch 側 `MetricKind::IP` の
    //! 評価基盤とも一致する）。
    //!
    //! # スレッド数制御
    //!
    //! hnsw_rs はスレッド数を指定する公開 API を持たない。`parallel_insert`／
    //! `parallel_insert_slice`（`src/hnsw.rs` 1212-1226 行目）は内部で rayon の
    //! グローバルスレッドプールを暗黙に使うのみで、プロセス内で複数回異なる
    //! スレッド数点を計測する用途には向かない。本クレート（`engine`）へ
    //! `rayon` を直接依存として追加せずにスレッド数ラダーを計測するため、
    //! `parallel_insert` は使わず `Hnsw::insert`（`&self` を取る。`src/hnsw.rs`
    //! 1060-1067 行目）を [`super::partition_rows`] で分割した `threads` 本の
    //! `std::thread::scope` ワーカーから直接呼び出す（usearch_adapter の
    //! `build_usearch_index_parallel` と同型の構成）。`Hnsw<'_, f32, DistDot>`
    //! は内部状態が `Arc<RwLock<..>>`（`src/hnsw.rs` 396-402 行目
    //! `PointIndexation`）のみで構成されるため自動導出で `Sync` になり、
    //! `&Hnsw` を複数スレッドから安全に共有できる（コンパイラが検証する。
    //! 本コードは `unsafe` を書かない）。

    use super::super::stats::BenchError;
    use super::partition_rows;
    use hnsw_rs::prelude::{DistDot, Hnsw, Neighbour};

    /// engine 側 [`engine::hnsw::HnswParams::default`]（m=16）に対応させた
    /// hnsw_rs 側の `max_nb_connection`。
    pub const MAX_NB_CONNECTION: usize = 16;

    /// engine 側 `HnswParams::default().ef_construction`（100）に対応させた
    /// hnsw_rs 側の `ef_construction`。
    pub const EF_CONSTRUCTION: usize = 100;

    /// `max_layer` を hnsw_rs 公式サンプル（`examples/ann-glove25-angular.rs`
    /// 87 行目 `let nb_layer = 16.min((nb_elem as f32).ln().trunc() as usize);`）
    /// と同じ式で決める。`rows == 0` は `ln` が定義できないため 1 に倒す
    /// （空コーパスは呼び出し元が別途拒否する契約）。
    pub fn max_layer_for(rows: usize) -> usize {
        if rows == 0 {
            return 1;
        }
        let ln_rows = (rows as f32).ln().trunc();
        if ln_rows < 1.0 {
            1
        } else {
            16usize.min(ln_rows as usize)
        }
    }

    /// `rows` 件の `normalized_vectors`（呼び出し元が [`super::l2_normalize_corpus`]
    /// で正規化済みのものを渡す契約。正規化前のコーパスを渡すとモジュール
    /// コメントの assert パニックを招きうる）から hnsw_rs の
    /// `Hnsw<f32, DistDot>` を構築する。
    ///
    /// `threads == 1` は逐次 `insert` ループ、`threads >= 2` は
    /// [`super::partition_rows`] による静的分割 + `std::thread::scope` の
    /// 並列ワーカーから `insert` を呼ぶ（モジュールコメント「スレッド数制御」
    /// 参照）。`usearch_adapter::build_usearch_index_parallel` と対称に
    /// `Hnsw::new` からこの関数の内側で行い、呼び出し元の計測区間へ初期化
    /// コストを含める。
    pub fn build_hnsw_rs_index(
        rows: usize,
        dim: usize,
        normalized_vectors: &[f32],
        threads: usize,
    ) -> Hnsw<'static, f32, DistDot> {
        let max_layer = max_layer_for(rows);
        let index = Hnsw::<f32, DistDot>::new(
            MAX_NB_CONNECTION,
            rows,
            max_layer,
            EF_CONSTRUCTION,
            DistDot {},
        );
        let threads = threads.max(1);
        if threads <= 1 {
            for row in 0..rows {
                let start = row * dim;
                let end = start + dim;
                if let Some(slice) = normalized_vectors.get(start..end) {
                    index.insert((slice, row));
                }
            }
        } else {
            let ranges = partition_rows(rows, threads);
            std::thread::scope(|scope| {
                for (start, end) in ranges {
                    let index_ref = &index;
                    scope.spawn(move || {
                        for row in start..end {
                            let vec_start = row * dim;
                            let vec_end = vec_start + dim;
                            if let Some(slice) = normalized_vectors.get(vec_start..vec_end) {
                                index_ref.insert((slice, row));
                            }
                        }
                    });
                }
            });
        }
        index
    }

    /// `query`（[`super::l2_normalize_corpus`] と同じ正規化を経た後の値を渡す契約）に
    /// 対する Top-`k` 近似最近傍 id 列を、hnsw_rs の `search`（`Neighbour::d_id`
    /// が挿入時に渡した行番号）で返す。`Hnsw::search` は既に距離昇順（＝
    /// `DistDot` の契約上は類似度降順）で返す実装
    /// （`src/hnsw.rs` `knn_search` 内 `neighbours_heap.into_sorted_vec()` の
    /// コメント「go from heap of points with negative dist to a sorted vec of
    /// increasing points」）のため、呼び出し元での並べ替えは不要。
    pub fn hnsw_rs_search_topk(
        index: &Hnsw<'_, f32, DistDot>,
        query: &[f32],
        knbn: usize,
        ef_search: usize,
    ) -> Result<Vec<u64>, BenchError> {
        let neighbours: Vec<Neighbour> = index.search(query, knbn, ef_search);
        Ok(neighbours.into_iter().map(|n| n.d_id as u64).collect())
    }
}
