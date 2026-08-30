//! 境界同点グループ再取得ループ（Issue #320・`hybrid.rs::hybrid_search_boosted`）の
//! レイテンシ影響を計測するための時間非依存ヘルパ（Issue #324。ポインタ:
//! `docs/spec/04-behavior/core-engine.md` CORE-7・`docs/spec/04-behavior/
//! query-planning.md` PLAN-4, PLAN-6, PLAN-7）。
//!
//! `benches/hybrid_latency_bench.rs`（実測。時間依存・`make ci` 対象外）が計測に使う
//! フィクスチャ生成・再取得統計の集計・`GITHUB_ACTIONS` 拒否判定を提供する。
//! `tests/hybrid_latency_accept.rs` が `#[path]` で本モジュールを取り込み、実測タイマー・
//! 実際の検索呼び出しに依存しない契約のみを `cargo test`（`make ci` 対象）で検証する
//! （`harness/tier.rs`・`tests/tier_latency_accept.rs` と同一パターン）。
//!
//! `harness/tier.rs` と同様に `engine::` （本クレートの公開 API。`crate::` ではなく
//! 外部クレート名としての絶対パス参照）に依存してよい——本ファイルは `cargo bench`
//! バイナリと統合テストの 2 つの独立したコンパイル単位へ `#[path]` で取り込まれるが、
//! いずれも `engine` ライブラリクレートをリンクする外部バイナリであり、`crate::` の
//! ような自己参照の曖昧さは生じない（`harness/mod.rs` 冒頭コメントの禁止事項は
//! `crate::` 参照であって `engine::` 参照ではない）。
//!
//! # 再取得ループを誘発する「プロトタイプクラスタ」モード
//!
//! `hybrid_search_boosted` の境界同点グループ完全化（Issue #310・#320）は、密チャネルの
//! `pool_depth` 境界に同点スコアの候補が並ぶと `fetch_k` を倍増して provider を再度
//! 呼び出す。通常の連続値ベクトル（[`DeterministicRng::next_vector`] で文書ごとに
//! 独立生成）ではほぼ同点は起きないため、[`generate_corpus`] は
//! `quantize_levels: Some(n)` を渡すことで文書を `n` 個のプロトタイプベクトルへ
//! クラスタ化し、内積スコアの衝突（同一プロトタイプの文書群は任意のクエリに対し
//! 厳密に同一スコアを持つ）を人為的に多発させる「最悪ケース」フィクスチャを作れる
//! （`quantize_levels: None` は通常の連続値ベクトルで、再取得がほぼ発生しない
//! 「無再取得」対照として使う）。両モードを同一ビルド内で比較することで、再取得
//! ループそのものの寄与を単一環境・単一ビルドで切り分けられる（2 コミット間の
//! worktree A/B より再現性が高い。Issue #324 の計測方針）。
//!
//! # 暗号用途禁止
//!
//! [`crate::rng::DeterministicRng`] を経由するため非暗号 PRNG である
//! （`rng.rs` モジュールドキュメント参照）。ベンチ入力生成専用。

use std::sync::atomic::{AtomicUsize, Ordering};

use engine::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider};
use engine::sparse::{DocId, SparseError, SparseIndex};

use super::rng::DeterministicRng;

/// [`generate_corpus`] が許容する文書数の安全上限（coding-rust.md「無制限確保禁止」。
/// `tests/hybrid_recall.rs::MAX_CORPUS_DOCS_GUARD` と同一方針・同一値）。
pub const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

/// 合成コーパス 1 件分（密ベクトル・疎テキストの両方を持つ）。
#[derive(Debug, Clone)]
pub struct Corpus {
    /// 文書 id（0 始まりの連番。[`SearchInput::ids`]・[`SparseIndex::build`] の
    /// `DocId` 双方にそのまま使う）。
    pub ids: Vec<DocId>,
    /// `ids.len() * dim` 要素のフラット化済みベクトル（[`SearchInput::vectors`] と
    /// 同じレイアウト）。
    pub vectors: Vec<f32>,
    /// 文書ごとの疎チャネル本文（[`SparseIndex::build`] への入力）。
    pub texts: Vec<String>,
    pub dim: u32,
}

impl Corpus {
    /// [`SparseIndex::build`] へ渡す `(DocId, &str)` スライスを組み立てる。
    pub fn sparse_docs(&self) -> Vec<(DocId, &str)> {
        self.ids
            .iter()
            .copied()
            .zip(self.texts.iter().map(String::as_str))
            .collect()
    }

    /// [`SparseIndex`] を構築する（`sparse_docs` を経由するだけの薄いヘルパ）。
    pub fn build_sparse_index(&self) -> Result<SparseIndex, SparseError> {
        SparseIndex::build(&self.sparse_docs())
    }
}

/// クエリ 1 件（密クエリベクトル・疎クエリ文字列）。
#[derive(Debug, Clone)]
pub struct Query {
    pub vector: Vec<f32>,
    pub text: String,
}

/// 決定的シードから合成コーパスを生成する。`num_docs`・`vocab_size`・`dim` は
/// 呼び出し側（ベンチ内定数）が固定し、環境変数からは受け取らない
/// （`.claude/rules/coding-rust.md`「untrusted 入力の扱い」: フィクスチャ規模を
/// env 経由の untrusted 入力にしない）。
///
/// `quantize_levels`: `Some(n)`（`n >= 2`）を渡すと、連続値ベクトルの代わりに
/// あらかじめ生成した `n` 個のプロトタイプベクトルのいずれかを各文書へそのまま
/// 割り当てる「クラスタモード」になる（モジュールドキュメント参照）。同一
/// プロトタイプを割り当てられた文書群はビット単位で同一のベクトルを持つため、
/// 任意のクエリに対して内積スコアが厳密に一致し、密チャネルの `pool_depth`
/// 境界へ巨大な同点グループを確実に発生させる（各次元を独立に離散化する方式
/// （高次元では組み合わせ数が爆発し、現実的な文書数では衝突がほぼ起きない）
/// より遥かに強い同点誘発効果を持つ）。`None` は量子化なしの連続値ベクトル
/// （文書ごとに独立生成し、衝突はほぼ起きない）。
///
/// `n < 2` はクラスタとして無意味（1 個のプロトタイプに全文書が潰れるなら
/// `n = 1` を明示する意味がない）ため [`HybridLatencyError::InvalidQuantizeLevels`]
/// を返す。
pub fn generate_corpus(
    seed: u64,
    num_docs: usize,
    vocab_size: usize,
    dim: usize,
    quantize_levels: Option<usize>,
) -> Result<Corpus, HybridLatencyError> {
    if num_docs > MAX_CORPUS_DOCS_GUARD {
        return Err(HybridLatencyError::CorpusTooLarge);
    }
    if let Some(levels) = quantize_levels {
        if levels < 2 {
            return Err(HybridLatencyError::InvalidQuantizeLevels);
        }
    }

    let mut rng = DeterministicRng::new(seed);
    // 疎チャネル（`texts`）専用の独立 RNG 系列。`rng`（密ベクトル用）と同じ系列を
    // 共有すると、クラスタモード（`Some(levels)`）はプロトタイプ生成
    // （`levels` 回の `next_vector` 消費）と各文書のインデックス選択（1 回の
    // `next_u64` 消費）という `quantize_levels` の値に依存した消費量になり、
    // 通常モード（`None`。文書ごとに `next_vector(dim)` で `dim` 回消費）とは
    // `rng` の消費ペースが食い違う。同一 `rng` から続けて `texts` を生成すると、
    // その消費量の差がそのまま `texts` の内容差として伝播し、A/B 比較
    // （`hybrid_latency_bench.rs`）が密チャネルの再取得ループ以外の要因
    // （疎チャネルの内容差 → `SparseIndex` の内容・疎候補・RRF 融合結果の変化）
    // まで含んでしまう（codex-review P1・PR #325 指摘）。`texts` を `rng` から
    // 完全に切り離した専用系列（`seed` に固定オフセットを加えるだけで、
    // ベクトル生成の消費量には一切依存しない）にすることで、同一
    // `(seed, num_docs, vocab_size)` に対して `texts` は `quantize_levels` の値に
    // 関わらず常に同一になる。オフセット定数は [`generate_query`] が使う
    // `0x5151_5151_5151_5151`（+ クエリ番号 `0..NUM_QUERIES` 分の加算）と衝突しない
    // 値を選ぶ。
    let mut text_rng = DeterministicRng::new(seed.wrapping_add(0x9e37_79b9_9e37_79b9));
    let mut ids = Vec::with_capacity(num_docs);
    let mut vectors = Vec::with_capacity(num_docs * dim);
    let mut texts = Vec::with_capacity(num_docs);

    // クラスタモード（`Some(levels)`）は、文書ごとに乱数を消費してベクトルを
    // 生成するのではなく、先にプロトタイプを `levels` 個だけ生成し、各文書へ
    // インデックスで割り当てる（プロトタイプの実体を共有することで、同一
    // クラスタの文書が厳密に同一ビットパターンを持つことを保証する）。
    let prototypes: Option<Vec<Vec<f32>>> =
        quantize_levels.map(|levels| (0..levels).map(|_| rng.next_vector(dim)).collect());

    for doc_id in 0..num_docs as u64 {
        ids.push(doc_id);

        let vector = match &prototypes {
            Some(protos) => {
                let idx = (rng.next_u64() as usize) % protos.len();
                protos[idx].clone()
            }
            None => rng.next_vector(dim),
        };
        vectors.extend_from_slice(&vector);

        // 疎チャネル: 語彙サイズ内のトークンを数語連結するだけの合成文（BM25 統計が
        // 退化しない程度の非自明な内容であれば足りる。QA 的な正解判定は本ベンチの
        // 対象外——密側再取得ループの所要時間のみを計測する）。`text_rng`（上記）
        // から消費するため、`rng` 側の消費量（`quantize_levels` に依存）とは
        // 無関係に決定的な内容になる。
        let num_tokens = 3 + (text_rng.next_u64() % 4) as usize; // 3..=6
        let mut text = String::new();
        for i in 0..num_tokens {
            if i > 0 {
                text.push(' ');
            }
            let token_idx = (text_rng.next_u64() as usize) % vocab_size.max(1);
            text.push_str(&format!("tok{token_idx}"));
        }
        texts.push(text);
    }

    Ok(Corpus {
        ids,
        vectors,
        texts,
        dim: dim as u32,
    })
}

/// [`generate_corpus`] と同じ決定的 RNG 系列の続きからクエリを 1 件生成する。
///
/// プロトタイプクラスタモード（[`generate_corpus`] の `quantize_levels`）の同点誘発は
/// 「同一クラスタの文書群が厳密に同一ベクトルを持つ」ことのみに依存し、クエリ側の
/// 値には依存しない（同一クラスタの文書はどんなクエリに対しても内積スコアが一致する）。
/// そのためクエリは常に通常の連続値ベクトルとして生成する（コーパス側のモードに
/// 合わせて特別扱いする必要がない）。
pub fn generate_query(seed: u64, dim: usize, vocab_size: usize) -> Query {
    // コーパス生成と系列を分離するため、シードへ固定オフセットを加えた別系列を使う
    // （同一シードから同一クエリを再現しつつ、コーパス生成の消費量に依存しない
    // 決定性を保つ）。
    let mut rng = DeterministicRng::new(seed.wrapping_add(0x5151_5151_5151_5151));
    let vector = rng.next_vector(dim);
    let token_idx = (rng.next_u64() as usize) % vocab_size.max(1);
    Query {
        vector,
        text: format!("tok{token_idx}"),
    }
}

/// [`generate_corpus`]／[`generate_query`] の失敗系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HybridLatencyError {
    /// [`MAX_CORPUS_DOCS_GUARD`] を超過した。
    CorpusTooLarge,
    /// `quantize_levels` に 2 未満の値が渡された。
    InvalidQuantizeLevels,
    /// `GITHUB_ACTIONS` 実行環境下で本ベンチの実行が要求された（本ベンチは
    /// `.github/workflows/*` へ配線しない運用のため、誤って CI 経由で実行された
    /// 場合に defense-in-depth で拒否する。計画「fail-closed」節参照）。
    RefusedUnderGitHubActions,
}

impl std::fmt::Display for HybridLatencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HybridLatencyError::CorpusTooLarge => {
                write!(f, "num_docs exceeds {MAX_CORPUS_DOCS_GUARD}")
            }
            HybridLatencyError::InvalidQuantizeLevels => {
                write!(f, "quantize_levels must be >= 2")
            }
            HybridLatencyError::RefusedUnderGitHubActions => write!(
                f,
                "hybrid_latency_bench is refused while running under GitHub Actions \
                 (GITHUB_ACTIONS is set); this bench is not wired into any workflow \
                 and must be run locally via `make bench-hybrid`"
            ),
        }
    }
}

impl std::error::Error for HybridLatencyError {}

/// `GITHUB_ACTIONS`（値を解釈せず存在有無のみ判定。`tests/hybrid_recall.rs::
/// resolve_verbose` 等と同じ規約）が設定された実行環境下では本ベンチの実行自体を
/// 拒否する純関数（単体テスト可能）。呼び出し元（`hybrid_latency_bench.rs::main`）は
/// `std::env::var_os("GITHUB_ACTIONS").is_some()` を渡す。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), HybridLatencyError> {
    if under_github_actions {
        return Err(HybridLatencyError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// 再取得ループの進行を観測する [`SearchProvider`] ラッパ（`tests/hybrid_recall.rs::
/// MaxKTrackingProvider` と同型。呼び出し回数・要求された `k`（＝密側 `fetch_k`）の
/// 最大値をクエリ単位で集計する診断用ラッパで、`hybrid_search`/`hybrid_search_boosted`
/// は `SearchProvider` を `&dyn` で受け取るため、内部の再取得進行はこのラッパ経由
/// でのみ観測できる（`hybrid.rs::MAX_FETCH_K` は `pub(crate)` のため本クレート外の
/// ベンチ・統合テストからは参照できない）。挙動そのものは変えない（`search` は
/// 無条件に `inner` へ委譲する）。
pub struct RefetchTrackingProvider<P> {
    inner: P,
    calls: AtomicUsize,
    max_k_seen: AtomicUsize,
}

impl<P: SearchProvider> RefetchTrackingProvider<P> {
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            max_k_seen: AtomicUsize::new(0),
        }
    }

    /// クエリ 1 件ごとの計測前に呼ぶ（累積カウンタをリセットする）。
    pub fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.max_k_seen.store(0, Ordering::Relaxed);
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    pub fn max_k_seen(&self) -> usize {
        self.max_k_seen.load(Ordering::Relaxed)
    }
}

impl<P: SearchProvider> SearchProvider for RefetchTrackingProvider<P> {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.max_k_seen.fetch_max(input.k, Ordering::Relaxed);
        self.inner.search(input)
    }
}

/// 1 クエリ分の再取得統計（[`RefetchTrackingProvider`] の計測後の値を集約した
/// 時間非依存の結果型。単体テストで直接構築して [`aggregate_refetch_stats`] の
/// 判定条件を検証できるようにする）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefetchStats {
    /// provider が呼ばれた回数（初回 1 回 + 再取得回数）。
    pub calls: usize,
    /// クエリ内で観測された `k`（密側 `fetch_k`）の最大値。
    pub max_k_seen: usize,
    /// 再取得ループが可視集合サイズ（実質的な上限）まで到達したか
    /// （`tests/hybrid_recall.rs::hybrid_recall_large_scale_dense_refetch_is_bounded_by_visible_set_size`
    /// と同じ判定条件）。
    pub reached_visible_set: bool,
}

/// [`RefetchTrackingProvider`] の計測値から [`RefetchStats`] を組み立てる純関数。
pub fn aggregate_refetch_stats(
    calls: usize,
    max_k_seen: usize,
    visible_set_size: usize,
) -> RefetchStats {
    RefetchStats {
        calls,
        max_k_seen,
        reached_visible_set: max_k_seen >= visible_set_size,
    }
}

/// クエリ集合全体（複数クエリ分の [`RefetchStats`]）の要約。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefetchSummary {
    pub queries: usize,
    pub calls_max: usize,
    pub max_k_across_queries: usize,
    pub reached_visible_set_count: usize,
}

/// [`RefetchStats`] の列から [`RefetchSummary`] を集計する純関数。
///
/// `stats` が空の場合は全フィールド 0 の要約を返す（`Result` を返す必要のある
/// エラーではない: 0 件のクエリで計測を回すこと自体は呼び出し元の構成ミスであり、
/// ここでは「観測結果が空だった」という事実をそのまま表す）。
pub fn summarize_refetch_stats(stats: &[RefetchStats]) -> RefetchSummary {
    let queries = stats.len();
    let calls_max = stats.iter().map(|s| s.calls).max().unwrap_or(0);
    let max_k_across_queries = stats.iter().map(|s| s.max_k_seen).max().unwrap_or(0);
    let reached_visible_set_count = stats.iter().filter(|s| s.reached_visible_set).count();
    RefetchSummary {
        queries,
        calls_max,
        max_k_across_queries,
        reached_visible_set_count,
    }
}

/// 1 段（stage）分の実測結果行を描画する（`hybrid_latency_bench.rs::main` から
/// 呼ぶ）。本ベンチは spec 由来の非公開閾値を持たない情報提供専用のため
/// （計画「出力規約」節）、`sql_c1_bench.rs`/`batch_bench.rs` のような verbose
/// opt-in ゲートは設けず、実測値（p95・median・再取得統計）を常に含める。
pub fn render_stage_line(
    stage: &str,
    median_us: u128,
    p95_us: u128,
    summary: RefetchSummary,
) -> String {
    format!(
        "hybrid_latency: stage={stage} p95_us={p95_us} median_us={median_us} \
         queries={} provider_calls_max={} max_k_across_queries={} reached_visible_set={}/{}",
        summary.queries,
        summary.calls_max,
        summary.max_k_across_queries,
        summary.reached_visible_set_count,
        summary.queries,
    )
}
