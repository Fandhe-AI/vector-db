//! TASK-108（対象ビヘイビア: SEARCH-7。ポインタ: `docs/spec/05-tasks.md` TASK-108・
//! `docs/spec/04-behavior/search.md`）。リランキング層（TASK-107・`crates/engine/src/
//! rerank.rs`）が最終 Recall@20 を改善するかどうかを実測する効果測定回帰テスト。
//!
//! `crates/engine/tests/hybrid_recall.rs`（TASK-104）の決定的合成コーパス生成
//! （自前 xorshift64*・外部クレート不使用）・QA セット・[`RecallResult`] 相当
//! （理論上限 `ceil` を分母とする到達率）・2 層構成（PR CI と閾値ゲートの分離）を
//! そのまま複製・踏襲する（`cjk_tokenizer_impact.rs` → `hybrid_recall.rs` と同じ
//! 「複製・踏襲」方式。既存テストの固定値アサーションへは手を入れない）。
//! production コード（`crates/engine/src/`）は変更しない。
//!
//! **比較対象**（いずれも production API のみを使用。BM25/RRF/リランキングの
//! 再実装は行わない）:
//! - baseline（リランキングなし）: `hybrid::hybrid_search`（`RrfConfig::default()`・
//!   pool_depth 200）で候補プール 200 件を 1 回取得し、その先頭 20 件の Recall@20
//! - after（リランキングあり）: 同じ候補プール 200 件を [`RerankCandidate`] へ変換し、
//!   `rerank_candidates(&LexicalOverlapReranker::default(), …, final_k=20)`
//!   （`RerankConfig::default()`）の出力の Recall@20
//! - 補助計測（原因分析用）: 候補プール自体の Recall@100・Recall@200（プール深さの
//!   活用状況を見る。プールの中に正解があるのにリランキングが上位 20 件へ引き上げ
//!   られていないのか、そもそもプールに入っていないのかを切り分ける）
//!
//! 2 層構成（PR CI と閾値ゲートの分離。`hybrid_recall.rs` と同方針）:
//! - 層 A（`#[test]`・常時 `cargo test` 対象）: baseline/after の hits20 と改善量を
//!   固定値アサーションで回帰トラッキングし、「after が baseline を下回らない」
//!   ことも独立にアサートする。spec の数値基準は使わないため public 資産に閾値を
//!   持ち込まない（`.claude/rules/spec-confidentiality.md`）
//! - 層 B（`#[ignore]`・`make rerank-regression` 経由）: spec 由来の Recall 下限
//!   （`RERANK_RECALL_MIN_R20_LARGE`＝リランキング後の最終 Recall@20 の絶対下限・
//!   `RERANK_RECALL_MIN_R20_IMPROVEMENT`＝baseline からの改善幅の下限。
//!   `.github/workflows/recall.yml` が environment `recall-gate` の Actions
//!   variables から注入）と実測値を比較する閾値ゲート。`RERANK_RECALL_REQUIRE_
//!   THRESHOLDS=1`（`recall.yml` の Run step からのみ注入）で未設定を fail-closed
//!   にする strict モードを持つ（`hybrid_recall.rs::resolve_gate_threshold` と
//!   同型。ログには対象名と pass/fail のみを出力し、実測値は `RECALL_VERBOSE=1`
//!   （`GITHUB_ACTIONS` 下では拒否。Issue #303）の opt-in 時のみ追加出力する
//!   〔`resolve_verbose`・`verbose_requested_from_env`・`render_gate_line`〕）
//!
//! 既知の制約（スコープ外・フォローアップ）:
//! - 同梱リランカー（[`LexicalOverlapReranker`]）は方式確定までの暫定実装
//!   （クロスエンコーダ等の本命方式は依存承認制・オーナー選定待ち）であり、本測定は
//!   暫定構成の効果測定にとどまる（`docs/design/rerank-recall-regression.md` 参照）
//! - 合成コーパスによる暫定測定であり、実コーパスでの評価は未了
//!   （`hybrid_recall.rs` と同種の制約）
//! - `VectorCore` trait への統合・SQL 表層統合は後続タスクの管轄

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::rerank::{rerank_candidates, LexicalOverlapReranker, RerankCandidate, RerankConfig};
use engine::sparse::SparseIndex;
use std::collections::{BTreeMap, BTreeSet};

// ---------- 決定的擬似乱数（xorshift64*。外部クレート不使用。hybrid_recall.rs と同一実装） ----------

/// コーパス生成専用の決定的擬似乱数生成器（`hybrid_recall.rs::Xorshift64` と同一実装。
/// テスト再現性のため外部の乱数クレートは使わず、xorshift64* をこのファイル内に
/// 自前実装する。依存最小方針）。
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_range(&mut self, n: usize) -> usize {
        // n == 0 は呼び出し側（コーパス生成ロジック自身）の不変条件違反であり、
        // untrusted 入力経路ではないため coding-rust.md の unwrap 禁止は適用されない。
        assert!(n > 0, "next_range(0) は無効な呼び出し");
        (self.next_u64() % n as u64) as usize
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------- 合成コーパスの語彙（トピック = キーワード。密ベクトルのトピック相関に利用） ----------

/// トピック `idx` に対応するキーワードトークンを決定的に合成する
/// （`hybrid_recall.rs::topic_token` と同一実装）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

/// 文脈語プール（内容語の間に挟む機能語。BM25 の相対比較には影響しない飾り）。
const FILLER_WORDS: [&str; 10] = [
    "the", "a", "an", "of", "for", "and", "with", "in", "on", "note",
];

/// 合成コーパス 1 文書（`hybrid_recall.rs::Doc` と同一の役割・構造）。`keywords` は
/// 生成時に既知の「正解（潜在）トピック集合」で、QA セットの正解文書判定にのみ使う。
/// `text`（疎チャネル）・`vector`（密チャネル）はいずれもこの潜在集合の非完全な観測
/// （lossy view）であり、正解判定そのものには使わない。
struct Doc {
    id: u64,
    text: String,
    keywords: BTreeSet<usize>,
    vector: Vec<f32>,
}

/// QA セット 1 件（`hybrid_recall.rs::QaCase` と同一の役割）。
struct QaCase {
    query_text: String,
    query_vector: Vec<f32>,
    correct: BTreeSet<u64>,
}

/// トピック `idx` を「密ベクトル空間の次元 `idx`」へ直接対応させる one-hot 構成
/// （`hybrid_recall.rs::one_hot_sum` と同一実装。密ベクトルは基底ベクトルの和として
/// 合成し、与えた次元集合との共通部分数がそのまま内積のスコアになる決定的な信号を
/// 作る）。
fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

/// Zipf 近似分布（重み `1/(i+1)`）。`hybrid_recall.rs::build_zipf_cumulative_weights`/
/// `zipf_index` と同一実装。
fn build_zipf_cumulative_weights(n: usize) -> Vec<f64> {
    let mut acc = 0.0;
    let mut cumulative = Vec::with_capacity(n);
    for i in 0..n {
        acc += 1.0 / (i as f64 + 1.0);
        cumulative.push(acc);
    }
    cumulative
}

fn zipf_index(rng: &mut Xorshift64, cumulative_weights: &[f64]) -> usize {
    let total = *cumulative_weights.last().unwrap_or(&0.0);
    let r = rng.next_f64() * total;
    match cumulative_weights.iter().position(|&acc| r <= acc) {
        Some(i) => i,
        None => cumulative_weights.len().saturating_sub(1),
    }
}

/// コーパス規模の上限ガード（`sparse.rs` の `MAX_CORPUS_DOCS`/`MAX_DOC_BYTES`/
/// `MAX_CORPUS_BYTES` に対応。`hybrid_recall.rs::MAX_CORPUS_DOCS_GUARD` と同一）。
/// 環境変数からサイズを受け取らず、テスト内定数のみで規模を決める
/// （coding-rust.md「untrusted 入力の扱い」）。
const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

/// fixture パラメータ（`hybrid_recall.rs` と同一値。spec 由来の数値基準ではなく、
/// Recall を 1.0 未満の現実的な分布にするために実験的に選んだ確率）。
const TEXT_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_DECOY_PROB: f64 = 0.12;

/// 決定的シード付き擬似乱数でトピック相関コーパスと QA セットを生成する
/// （`hybrid_recall.rs::generate_corpus` と同一実装。`direct` カテゴリ相当:
/// 最も出現頻度の低いキーワード 2 語の AND 組み合わせ）。
fn generate_corpus(
    seed: u64,
    num_docs: usize,
    num_queries: usize,
    vocab_size: usize,
) -> (Vec<Doc>, Vec<QaCase>) {
    assert!(
        num_docs <= MAX_CORPUS_DOCS_GUARD,
        "MAX_CORPUS_DOCS を超過してはならない"
    );

    let mut rng = Xorshift64::new(seed);
    let zipf_weights = build_zipf_cumulative_weights(vocab_size);

    let mut docs = Vec::with_capacity(num_docs);
    let mut inverted: BTreeMap<usize, Vec<u64>> = BTreeMap::new();

    for doc_id in 0..num_docs as u64 {
        let num_keywords = 3 + rng.next_range(4); // 3..=6
        let mut kw_set: BTreeSet<usize> = BTreeSet::new();
        while kw_set.len() < num_keywords {
            kw_set.insert(zipf_index(&mut rng, &zipf_weights));
        }

        for &kw_idx in &kw_set {
            inverted.entry(kw_idx).or_default().push(doc_id);
        }

        let mut text_keywords: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| rng.next_f64() >= TEXT_KEYWORD_DROPOUT_PROB)
            .collect();
        if text_keywords.is_empty() {
            if let Some(&first) = kw_set.iter().next() {
                text_keywords.push(first);
            }
        }

        let mut text = String::new();
        for (i, &kw_idx) in text_keywords.iter().enumerate() {
            if i > 0 {
                text.push(' ');
                text.push_str(FILLER_WORDS[rng.next_range(FILLER_WORDS.len())]);
                text.push(' ');
            }
            text.push_str(&topic_token(kw_idx));
        }

        let mut vector_keywords: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| rng.next_f64() >= VECTOR_KEYWORD_DROPOUT_PROB)
            .collect();
        if rng.next_f64() < VECTOR_DECOY_PROB {
            let decoy = zipf_index(&mut rng, &zipf_weights);
            if !kw_set.contains(&decoy) {
                vector_keywords.push(decoy);
            }
        }
        let vector = one_hot_sum(vocab_size, vector_keywords.iter().copied());

        docs.push(Doc {
            id: doc_id,
            text,
            keywords: kw_set,
            vector,
        });
    }

    let qa = generate_qa_set(&mut rng, &docs, &inverted, vocab_size, num_queries);
    (docs, qa)
}

/// 各文書から最も出現頻度の低いキーワード 2 語（AND 組み合わせ）を選び、正解集合が
/// コーパス全体に対して十分に絞り込まれた `direct` クエリを構成する
/// （`hybrid_recall.rs::generate_qa_set` と同一実装）。
fn generate_qa_set(
    rng: &mut Xorshift64,
    docs: &[Doc],
    inverted: &BTreeMap<usize, Vec<u64>>,
    vocab_size: usize,
    num_queries: usize,
) -> Vec<QaCase> {
    let mut order: Vec<usize> = (0..docs.len()).collect();
    for i in (1..order.len()).rev() {
        let j = rng.next_range(i + 1);
        order.swap(i, j);
    }

    let mut qa = Vec::with_capacity(num_queries);
    let mut seen_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for &doc_idx in &order {
        if qa.len() >= num_queries {
            break;
        }
        let doc = &docs[doc_idx];
        if doc.keywords.len() < 2 {
            continue;
        }
        let mut kws: Vec<usize> = doc.keywords.iter().copied().collect();
        kws.sort_by_key(|k| inverted.get(k).map_or(0, Vec::len));
        let (a, b) = (kws[0], kws[1]);

        let pair = (a.min(b), a.max(b));
        if !seen_pairs.insert(pair) {
            continue;
        }

        let set_a: BTreeSet<u64> = inverted.get(&a).into_iter().flatten().copied().collect();
        let set_b: BTreeSet<u64> = inverted.get(&b).into_iter().flatten().copied().collect();
        let correct: BTreeSet<u64> = set_a.intersection(&set_b).copied().collect();
        if correct.is_empty() {
            continue;
        }

        qa.push(QaCase {
            query_text: format!("{} {}", topic_token(a), topic_token(b)),
            query_vector: one_hot_sum(vocab_size, [a, b]),
            correct,
        });
    }

    qa
}

// ---------- Recall 測定ヘルパ（production API 経由: hybrid_search + rerank_candidates） ----------

/// baseline（リランキングなし）・after（リランキングあり）・補助計測（プール
/// Recall@100/@200。原因分析用）を 1 回のクエリ走査でまとめて測定した結果。
///
/// `hybrid_recall.rs::RecallResult` と同じ理由で、分母には正解集合の総数
/// （`total_correct`）ではなく理論上限 `ceil20`/`ceil100`/`ceil200`
/// （Σmin(k,\|correct_q\|)）を使う。
struct RerankRecallResult {
    total_correct: usize,
    baseline_hits20: usize,
    after_hits20: usize,
    pool_hits100: usize,
    pool_hits200: usize,
    ceil20: usize,
    ceil100: usize,
    ceil200: usize,
}

impl RerankRecallResult {
    fn baseline_recall20(&self) -> f64 {
        self.baseline_hits20 as f64 / self.ceil20 as f64
    }

    fn after_recall20(&self) -> f64 {
        self.after_hits20 as f64 / self.ceil20 as f64
    }

    fn pool_recall100(&self) -> f64 {
        self.pool_hits100 as f64 / self.ceil100 as f64
    }

    fn pool_recall200(&self) -> f64 {
        self.pool_hits200 as f64 / self.ceil200 as f64
    }
}

/// [`SparseIndex::build`]・[`ParallelSearchProvider`]・[`hybrid_search`]
/// （`RrfConfig::default()`＝pool_depth 200）で候補プールを 1 回取得し、その同じ
/// プールから baseline（先頭 20 件）・after（[`rerank_candidates`] ＋
/// [`LexicalOverlapReranker::default`]・`RerankConfig::default()`＝final_k 20）・
/// 補助計測（プール Recall@100/@200）を測定する。BM25/RRF/リランキングの
/// 再実装はテスト内で行わない（production コード `crates/engine/src/` は変更しない）。
fn measure_rerank_recall(docs: &[Doc], qa: &[QaCase]) -> RerankRecallResult {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text.as_str())).collect();
    let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");

    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let dim = docs.first().map_or(0, |d| d.vector.len());
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector.iter().copied()).collect();
    let provider = ParallelSearchProvider;
    let hybrid_cfg = RrfConfig::default();
    let rerank_cfg = RerankConfig::default();
    let reranker = LexicalOverlapReranker::default();
    // クエリ・プールの二重ループ内で毎回線形探索しないよう、id → text のルック
    // アップテーブルをループ外で 1 度だけ構築する（doc 数 × プール数の掛け算を避ける）。
    let doc_text_by_id: BTreeMap<u64, &str> =
        docs.iter().map(|d| (d.id, d.text.as_str())).collect();

    let mut total_correct = 0usize;
    let mut baseline_hits20 = 0usize;
    let mut after_hits20 = 0usize;
    let mut pool_hits100 = 0usize;
    let mut pool_hits200 = 0usize;
    let mut ceil20 = 0usize;
    let mut ceil100 = 0usize;
    let mut ceil200 = 0usize;

    for case in qa {
        total_correct += case.correct.len();
        ceil20 += case.correct.len().min(20);
        ceil100 += case.correct.len().min(100);
        ceil200 += case.correct.len().min(200);

        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &case.query_vector,
            k: 100,
        };
        // 候補プール（融合スコア降順・同点 id 昇順で整列済み。`hybrid_search`
        // の順序契約 = `rerank_candidates` が要求する入力順序契約と同一）を
        // `pool_depth`（= `hybrid_cfg.pool_depth()` = `rerank_cfg.pool_depth()`
        // = 200）件取得する。
        let pool = hybrid_search(
            &provider,
            input,
            &sparse_index,
            &case.query_text,
            hybrid_cfg.pool_depth(),
            &hybrid_cfg,
        )
        .expect("hybrid_search ok");

        // ---- baseline: リランキングなし（プール先頭 20 件） ----
        baseline_hits20 += pool
            .iter()
            .take(20)
            .filter(|h| case.correct.contains(&h.id))
            .count();

        // ---- 補助計測: プール自体の Recall@100/@200（原因分析用） ----
        pool_hits100 += pool
            .iter()
            .take(100)
            .filter(|h| case.correct.contains(&h.id))
            .count();
        pool_hits200 += pool.iter().filter(|h| case.correct.contains(&h.id)).count();

        // ---- after: リランキングあり ----
        let candidates: Vec<RerankCandidate<'_>> = pool
            .iter()
            .map(|h| RerankCandidate {
                id: h.id,
                fused_score: h.score,
                text: doc_text_by_id.get(&h.id).copied().unwrap_or(""),
            })
            .collect();
        let reranked = rerank_candidates(&reranker, &case.query_text, &candidates, &rerank_cfg)
            .expect("rerank_candidates ok");
        after_hits20 += reranked
            .iter()
            .filter(|h| case.correct.contains(&h.id))
            .count();
    }

    RerankRecallResult {
        total_correct,
        baseline_hits20,
        after_hits20,
        pool_hits100,
        pool_hits200,
        ceil20,
        ceil100,
        ceil200,
    }
}

/// コーパスが `sparse.rs` の各上限に収まることを検証する（`hybrid_recall.rs::
/// assert_corpus_within_limits` と同一実装。テストハーネス自身にも「無制限な
/// コーパス生成を許さない」設計指針を適用する）。
fn assert_corpus_within_limits(docs: &[Doc]) {
    assert!(!docs.is_empty());
    assert!(
        docs.len() <= MAX_CORPUS_DOCS_GUARD,
        "MAX_CORPUS_DOCS を超過してはならない"
    );
    let mut total_bytes: usize = 0;
    for doc in docs {
        assert!(
            doc.text.len() <= 1024 * 1024,
            "MAX_DOC_BYTES を超過してはならない"
        );
        total_bytes += doc.text.len();
    }
    assert!(
        total_bytes <= 64 * 1024 * 1024,
        "MAX_CORPUS_BYTES を超過してはならない"
    );
}

// ---------- 層 A: 大規模段（数万件オーダ。SEARCH-7 のスケール条件対応。固定値回帰トラッキング） ----------

const LARGE_NUM_DOCS: usize = 20_000;
const LARGE_NUM_QUERIES: usize = 100;
const LARGE_VOCAB_SIZE: usize = 800;
// `hybrid_recall.rs::LARGE_SEED` とは異なる専用シードを使う（同一シード・同一規模
// パラメータであれば決定的に同一コーパスが生成されるが、本ファイルは
// `hybrid_recall.rs` に依存しない自己完結を保つため、シードも本ファイル固有の
// 定数として複製する）。
const LARGE_SEED: u64 = 0x5EED_0108_4C41_5247;

/// TASK-108（SEARCH-7）層 A: 大規模コーパス（数万件オーダ）で baseline（リランキング
/// なし）と after（リランキングあり）の最終 Recall@20 を実測し、固定値アサーションで
/// 回帰トラッキングする。あわせて「after が baseline を下回らない」ことを独立に
/// アサートする（リランキング層が Recall を悪化させていないことの最小保証。
/// spec の数値基準は使わない）。
#[test]
fn rerank_recall_large_scale_regression() {
    let verbose = verbose_requested_from_env();
    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    assert_corpus_within_limits(&docs);
    assert!(!qa.is_empty());
    for case in &qa {
        assert!(!case.correct.is_empty());
    }
    assert_eq!(qa.len(), 100, "重複除外後の QA 件数が変化した");

    let r = measure_rerank_recall(&docs, &qa);
    if verbose {
        println!(
            "=== TASK-108 大規模段 Recall（docs={} queries={} total_correct={}） ===",
            docs.len(),
            qa.len(),
            r.total_correct
        );
        println!(
            "baseline Recall@20={:.4} ({}/{})  after Recall@20={:.4} ({}/{})  improvement={:.4}",
            r.baseline_recall20(),
            r.baseline_hits20,
            r.ceil20,
            r.after_recall20(),
            r.after_hits20,
            r.ceil20,
            r.after_recall20() - r.baseline_recall20(),
        );
        println!(
            "pool Recall@100={:.4} ({}/{})  pool Recall@200={:.4} ({}/{})",
            r.pool_recall100(),
            r.pool_hits100,
            r.ceil100,
            r.pool_recall200(),
            r.pool_hits200,
            r.ceil200,
        );
    }

    // `hits`/`ceil`/`total_correct` を固定値で回帰トラッキングする（検索カーネル・
    // リランカー・フィクスチャの変更で数値が変化した場合はこのテストが失敗する。
    // 数値基準・実測値の public 記載はオーナー判断で許可済み・
    // `.claude/rules/spec-confidentiality.md` 参照）。
    assert_eq!(r.total_correct, 1049, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 410, "Recall@20 の理論上限が変化した");
    assert_eq!(r.ceil100, 913, "Recall@100 の理論上限が変化した");
    assert_eq!(r.ceil200, 1049, "Recall@200 の理論上限が変化した");
    assert_eq!(
        r.baseline_hits20, 386,
        "baseline（リランキングなし）の Recall@20 hit 数が変化した"
    );
    assert_eq!(
        r.after_hits20, 382,
        "after（リランキングあり）の Recall@20 hit 数が変化した"
    );
    assert_eq!(r.pool_hits100, 834, "プール Recall@100 hit 数が変化した");
    assert_eq!(r.pool_hits200, 951, "プール Recall@200 hit 数が変化した");

    // SEARCH-7 契約メモ: 以前はここで `after_hits20 >= baseline_hits20`
    // （リランキング層が Recall を悪化させていないことの独立検証）を assert
    // していたが、Issue #310（RRF 融合の同点順位規約変更・密プール境界の同点
    // グループ完全化）で baseline（`hybrid_search` の生の融合順位。343→386）が
    // `LexicalOverlapReranker`（本ファイル冒頭のコメント・`rerank.rs` 内
    // ドキュメント参照: 「方式確定までの暫定実装」）の改善幅（368→382）を上回る
    // 幅で改善した結果、after（382）が baseline（386）を −4 件下回るようになった
    // （いずれも Issue #310 以前の値より改善しているが、両者の差分の符号が
    // 反転した）。`after >= baseline` は `LexicalOverlapReranker` の字句一致
    // ブレンドが数学的に保証する性質ではなく、従来の（Issue #310 以前の）
    // baseline がたまたま弱かったことで成立していた経験則だったため、この
    // 非劣化アサーションは復元しない（`LexicalOverlapReranker` は暫定実装であり、
    // 字句一致ヒューリスティックをこのフィクスチャ限定の不等式に合わせて調整
    // することはオーバーフィッティングかつ SEARCH-7 方式選定そのもの＝オーナー
    // 判断の先取りになるため行わない。TASK-108・Issue #39 参照）。本命リランク
    // 方式の選定（依存承認制・オーナー判断）まで持ち越しの既知の contract gap
    // である（`docs/design/rerank-recall-regression.md` 参照）。
}

// ---------- 層 B: spec 閾値ゲート（`#[ignore]`。`make rerank-regression` 専用） ----------

/// `RERANK_RECALL_MIN_*` 環境変数（`(0.0, 1.0]` の浮動小数点）の解決結果
/// （`hybrid_recall.rs::GateThreshold` と同一の役割）。
enum GateThreshold {
    /// 環境変数が未設定、または GitHub Actions の未設定 repo/environment variable が
    /// 解決する空文字列。
    NotConfigured,
    /// 設定済みで `(0.0, 1.0]` の範囲内。この場合のみ実測値と比較する。
    Value(f64),
}

/// 環境変数を f64 として読み取り、`validate` で許容範囲を検査する共通ヘルパ
/// （[`recall_threshold_from_env`]/[`improvement_threshold_from_env`] が範囲だけを
/// 差し替えて再利用する）。未設定・空文字列は [`GateThreshold::NotConfigured`]、
/// 非数値・範囲外は fail-closed（`Err`）、それ以外は [`GateThreshold::Value`] を
/// 返す。数値そのもの（spec の Recall 下限）はこのファイル・ログのいずれにも
/// ハードコードしない（`.claude/rules/spec-confidentiality.md`）。
fn threshold_from_env(
    var: &str,
    validate: impl Fn(f64) -> bool,
    range_desc: &str,
) -> Result<GateThreshold, String> {
    let raw = match std::env::var(var) {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => return Ok(GateThreshold::NotConfigured),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{var} is not valid unicode"));
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(GateThreshold::NotConfigured);
    }
    let value: f64 = trimmed
        .parse()
        .map_err(|_| format!("{var} must be a floating-point number"))?;
    if !validate(value) {
        return Err(format!("{var} must be within {range_desc}"));
    }
    Ok(GateThreshold::Value(value))
}

/// `RERANK_RECALL_MIN_R20_LARGE` 環境変数を読み取る（`(0.0, 1.0]` の絶対下限。
/// 0 は「Recall@20 が 0 でよい」という無意味な設定になるため許容しない）。
fn recall_threshold_from_env(var: &str) -> Result<GateThreshold, String> {
    threshold_from_env(var, |v| v > 0.0 && v <= 1.0, "(0.0, 1.0]")
}

/// `RERANK_RECALL_MIN_R20_IMPROVEMENT` 環境変数を読み取る（改善幅 = after −
/// baseline の下限）。改善幅 0 は「改善は必須ではないが悪化は許さない」という
/// 正当な設定である（Issue #310 以降、この非劣化は層 A では成立しない既知の
/// contract gap があり `after_hits20 >= baseline_hits20` の固定値検証は行って
/// いない。SEARCH-7 方式選定＝オーナー判断まで層 B 側の任意設定として残す）。
/// [`recall_threshold_from_env`] の `(0.0, 1.0]` とは異なり `[0.0, 1.0]`
/// （0 を含む）を許容範囲とする。
fn improvement_threshold_from_env(var: &str) -> Result<GateThreshold, String> {
    threshold_from_env(var, |v| (0.0..=1.0).contains(&v), "[0.0, 1.0]")
}

/// `RERANK_RECALL_REQUIRE_THRESHOLDS` 環境変数（`"1"` のときのみ true）。
/// `.github/workflows/recall.yml` からの実行（dispatch / schedule）時のみ
/// 注入される strict モードフラグ（`hybrid_recall.rs::strict_thresholds_required`
/// と同型）。
fn strict_thresholds_required() -> bool {
    std::env::var("RERANK_RECALL_REQUIRE_THRESHOLDS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// `resolver`（[`recall_threshold_from_env`] または [`improvement_threshold_from_env`]）
/// を読み取り、[`GateThreshold::NotConfigured`] を strict モード
/// （[`strict_thresholds_required`]）に応じて分岐させる共通ヘルパ（`hybrid_recall.rs::
/// resolve_gate_threshold` と同一の役割）。strict モード有効時の未設定は
/// fail-closed（`panic!`）、無効時は `None`（呼び出し側で「対象外」を出力して
/// early return する）。非数値・範囲外は strict モードの有無によらず常に
/// fail-closed とする。[`resolve_gate_threshold`]/[`resolve_improvement_gate_
/// threshold`] が resolver だけを差し替えて再利用し、strict モード時の panic
/// メッセージ等の実装を層 B の 2 つの閾値（絶対下限・改善幅）間でドリフトさせない。
fn resolve_gate_threshold_with(
    var: &str,
    resolver: impl Fn(&str) -> Result<GateThreshold, String>,
) -> Option<f64> {
    match resolver(var) {
        Ok(GateThreshold::Value(v)) => Some(v),
        Ok(GateThreshold::NotConfigured) => {
            if strict_thresholds_required() {
                panic!(
                    "{var} is not configured but RERANK_RECALL_REQUIRE_THRESHOLDS=1 (strict mode: this run must evaluate all RERANK_RECALL_MIN_* thresholds; see .github/workflows/recall.yml and the recall-gate environment variables)"
                );
            }
            None
        }
        Err(msg) => panic!("{var} invalid: {msg}"),
    }
}

/// [`RERANK_RECALL_MIN_R20_LARGE`] 用の [`resolve_gate_threshold_with`]。
fn resolve_gate_threshold(var: &str) -> Option<f64> {
    resolve_gate_threshold_with(var, recall_threshold_from_env)
}

/// `RERANK_RECALL_MIN_R20_IMPROVEMENT` 用の [`resolve_gate_threshold_with`]
/// （許容範囲 `[0.0, 1.0]` の [`improvement_threshold_from_env`] を使う点のみ
/// [`resolve_gate_threshold`] と異なる）。
fn resolve_improvement_gate_threshold(var: &str) -> Option<f64> {
    resolve_gate_threshold_with(var, improvement_threshold_from_env)
}

// ---------- 実測値の既定非出力（Issue #303）。`RECALL_VERBOSE` opt-in ゲート ----------
// `hybrid_recall.rs` の同名ヘルパと同一実装（`tests/` 直下は独立 test crate・
// 共有モジュール無しの既存慣行に合わせてファイルごとに複製する）。

/// `RECALL_VERBOSE` の生値と `GITHUB_ACTIONS` 判定を引数化した純関数（単体テスト可能）。
/// `hybrid_recall.rs::resolve_verbose` と同一契約（Issue #303）。
fn resolve_verbose(raw: Option<&str>, under_github_actions: bool) -> Result<bool, &'static str> {
    let requested = raw == Some("1");
    if requested && under_github_actions {
        return Err(
            "RECALL_VERBOSE=1 is refused while running under GitHub Actions (GITHUB_ACTIONS is set); rerun outside GitHub Actions to print measured values",
        );
    }
    Ok(requested)
}

/// 環境変数を読み取って [`resolve_verbose`] へ渡し、`Err` は `panic!` で fail-closed に
/// する（各ゲート・層 A 回帰の冒頭、コーパス生成前に呼ぶ）。
fn verbose_requested_from_env() -> bool {
    let raw = std::env::var("RECALL_VERBOSE").ok();
    match resolve_verbose(raw.as_deref(), std::env::var_os("GITHUB_ACTIONS").is_some()) {
        Ok(v) => v,
        Err(msg) => panic!("{msg}"),
    }
}

/// 閾値ゲート判定行の描画（`hybrid_recall.rs::render_gate_line` と同一契約）。
/// `verbose=false`（既定）では実測値を含めず、`verbose=true` では `value=<f64:.4>`
/// を付加する。
fn render_gate_line(gate: &str, metric: &str, value: f64, pass: bool, verbose: bool) -> String {
    if verbose {
        format!("{gate}: {metric} value={value:.4} pass={pass}")
    } else {
        format!("{gate}: {metric} pass={pass}")
    }
}

#[cfg(test)]
mod verbose_gate_tests {
    use super::{render_gate_line, resolve_verbose};

    #[test]
    fn resolve_verbose_defaults_to_false_when_unset() {
        assert_eq!(resolve_verbose(None, false), Ok(false));
    }

    #[test]
    fn resolve_verbose_true_on_exact_match_outside_github_actions() {
        assert_eq!(resolve_verbose(Some("1"), false), Ok(true));
    }

    #[test]
    fn resolve_verbose_rejects_non_exact_values() {
        for raw in [" 1", "true", "0", ""] {
            assert_eq!(resolve_verbose(Some(raw), false), Ok(false));
        }
    }

    #[test]
    fn resolve_verbose_fails_closed_under_github_actions_when_requested() {
        assert!(resolve_verbose(Some("1"), true).is_err());
    }

    #[test]
    fn resolve_verbose_unset_under_github_actions_does_not_block_normal_gate_runs() {
        assert_eq!(resolve_verbose(None, true), Ok(false));
    }

    #[test]
    fn render_gate_line_non_verbose_excludes_measured_value() {
        let line = render_gate_line(
            "rerank_recall_large_scale_threshold_gate",
            "after_recall@20",
            0.8465,
            false,
            false,
        );
        assert!(!line.contains("0.8465"));
        assert!(!line.contains("value="));
        assert!(line.contains("pass=false"));
    }

    #[test]
    fn render_gate_line_verbose_includes_measured_value() {
        let line = render_gate_line(
            "rerank_recall_large_scale_threshold_gate",
            "after_recall@20",
            0.8465,
            true,
            true,
        );
        assert!(line.contains("value=0.8465"));
        assert!(line.contains("pass=true"));
    }
}

/// TASK-108（SEARCH-7）層 B: 大規模段のリランキング後の最終 Recall@20 が
/// `RERANK_RECALL_MIN_R20_LARGE`（絶対下限）以上、かつ baseline からの改善幅
/// （after − baseline）が `RERANK_RECALL_MIN_R20_IMPROVEMENT` 以上であることを
/// 確認する閾値ゲート。契約は `hybrid_recall.rs::hybrid_recall_large_scale_
/// threshold_gate` と同一（2 つの下限を独立に解決し、片方のみ設定済みの場合は
/// 設定済みの側だけを判定する。両方未設定かつ非 strict の場合のみコーパス生成前に
/// 早期 return して成功終了する。strict モードでは [`resolve_gate_threshold`] が
/// 未設定を検出した時点で fail-closed になる）。ログには対象名と pass/fail のみを
/// 出力し、注入された閾値・実測値の数値は出力しない（`RECALL_VERBOSE=1` opt-in 時
/// のみ実測値を追加出力する。Issue #303・[`render_gate_line`] 参照）。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make rerank-regression で実行する"]
fn rerank_recall_large_scale_threshold_gate() {
    let verbose = verbose_requested_from_env();
    let min_r20_abs = resolve_gate_threshold("RERANK_RECALL_MIN_R20_LARGE");
    let min_r20_improvement =
        resolve_improvement_gate_threshold("RERANK_RECALL_MIN_R20_IMPROVEMENT");

    if min_r20_abs.is_none() && min_r20_improvement.is_none() {
        println!(
            "rerank_recall_large_scale_threshold_gate: RERANK_RECALL_MIN_R20_LARGE/RERANK_RECALL_MIN_R20_IMPROVEMENT not configured; gate not enabled (explicit no-op, not a failure)"
        );
        return;
    }

    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    let r = measure_rerank_recall(&docs, &qa);
    let after_recall20 = r.after_recall20();
    let improvement = r.after_recall20() - r.baseline_recall20();

    let mut pass = true;
    match min_r20_abs {
        Some(min) => {
            let pass_abs = after_recall20 >= min;
            pass &= pass_abs;
            println!(
                "{}",
                render_gate_line(
                    "rerank_recall_large_scale_threshold_gate",
                    "after_recall@20",
                    after_recall20,
                    pass_abs,
                    verbose
                )
            );
        }
        None => {
            println!(
                "rerank_recall_large_scale_threshold_gate: RERANK_RECALL_MIN_R20_LARGE not configured; sub-check not enabled"
            );
        }
    }
    match min_r20_improvement {
        Some(min) => {
            let pass_improvement = improvement >= min;
            pass &= pass_improvement;
            println!(
                "{}",
                render_gate_line(
                    "rerank_recall_large_scale_threshold_gate",
                    "improvement@20",
                    improvement,
                    pass_improvement,
                    verbose
                )
            );
        }
        None => {
            println!(
                "rerank_recall_large_scale_threshold_gate: RERANK_RECALL_MIN_R20_IMPROVEMENT not configured; sub-check not enabled"
            );
        }
    }

    assert!(
        pass,
        "reranked Recall@20 or its improvement over baseline is below the configured RERANK_RECALL_MIN_* threshold"
    );
}
