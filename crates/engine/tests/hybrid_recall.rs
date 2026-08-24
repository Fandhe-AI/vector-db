//! TASK-104（対象ビヘイビア: SEARCH-1, SEARCH-2。ポインタ: `docs/spec/05-tasks.md`
//! TASK-104・`docs/spec/04-behavior/search.md`）。ハイブリッド検索カーネル
//! （疎 BM25 = TASK-102 / RRF 融合 = TASK-103 / CJK ストップワード除去 = TASK-105）の
//! Recall 受け入れ基準を自動チェックする回帰テスト。
//!
//! `crates/engine/tests/cjk_tokenizer_impact.rs`（TASK-106）の決定的合成コーパス生成
//! （自前 xorshift64*・外部クレート不使用）＋ Recall 実測＋固定値アサーションによる
//! 回帰トラッキング方式を踏襲し、疎（BM25）側と同じ「AND 一致度」を密ベクトルにも
//! one-hot 符号化で持たせる（[`one_hot_sum`] のドキュメント参照）ことで
//! `engine::hybrid::hybrid_search`（密・疎の RRF 融合）を通しで測定する。測定は
//! production API（[`SparseIndex::build`]・[`ParallelSearchProvider`]・
//! [`hybrid_search`]＋[`RrfConfig::default`]）のみを使用し、production コード
//! （`crates/engine/src/`）は変更しない。
//!
//! 2 段のスケール条件（小規模・大規模）を持つ:
//! - 小規模段（`SMALL_NUM_DOCS` 件オーダ）: Recall@20（SEARCH-1 対応）
//! - 大規模段（`LARGE_NUM_DOCS` 件オーダ）: Recall@20・Recall@100（SEARCH-2 対応）
//!
//! 2 層構成（PR CI と閾値ゲートの分離）:
//! - 層 A（`#[test]`・常時 `cargo test` 対象）: 決定的コーパスでのヒット数を固定値
//!   アサーションで回帰トラッキングする（`cjk_tokenizer_impact.rs` と同方式）。
//!   spec の数値基準は使わないため public 資産に閾値を持ち込まない
//!   （`.claude/rules/spec-confidentiality.md`）。
//! - 層 B（`#[ignore]`・`make recall-regression` 経由）: spec 由来の Recall 下限
//!   （`HYBRID_RECALL_MIN_*` 環境変数。`.github/workflows/recall.yml` が Actions
//!   variables から注入）と実測値を比較する閾値ゲート。未設定・非数値・範囲外は
//!   fail-closed でテスト失敗とし、ログには実測値と pass/fail のみを出力する
//!   （`crates/engine/benches/parallel_bench.rs` と同方針）。
//!
//! 既知の制約（スコープ外・フォローアップ）:
//! - 合成コーパスによる暫定測定であり、実コーパスでの評価は未了
//!   （`docs/design/hybrid-recall-regression.md` 参照。`cjk_tokenizer_impact.rs` と同種の制約）
//! - クエリ展開（PLAN-5 系、TASK-109 以降）は未実装のため、本ハーネスはハイブリッド
//!   検索単体（クエリ展開なし）の測定に留める

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::sparse::SparseIndex;
use std::collections::{BTreeMap, BTreeSet};

// ---------- 決定的擬似乱数（xorshift64*。外部クレート不使用。cjk_tokenizer_impact.rs と同一実装） ----------

/// コーパス生成専用の決定的擬似乱数生成器（テスト再現性のため外部の乱数クレートは
/// 使わず、xorshift64* をこのファイル内に自前実装する。依存最小方針）。
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

/// トピック `idx` に対応するキーワードトークンを決定的に合成する。固定語彙表を
/// 使わず語彙数（`vocab_size`）をコーパス規模に応じて可変にすることで、規模が
/// 大きくなっても QA セットの正解集合（キーワード 2 語の AND 交差）が肥大化せず
/// 絞り込まれた状態を維持できるようにする（`sparse::tokenize` の ASCII 単語境界規則の
/// 下で必ず 1 トークンになるよう、アンダースコア区切りの接頭辞＋4 桁ゼロ埋め数字とする）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

/// 文脈語プール（内容語の間に挟む機能語。BM25 の相対比較には影響しない飾り）。
const FILLER_WORDS: [&str; 10] = [
    "the", "a", "an", "of", "for", "and", "with", "in", "on", "note",
];

/// 合成コーパス 1 文書（`keywords` は生成時に既知の「正解トピック集合」＝トピック
/// インデックス（[`topic_token`] 参照）で、QA セットの正解文書判定と密ベクトル合成の
/// 両方に使う）。
struct Doc {
    id: u64,
    text: String,
    keywords: BTreeSet<usize>,
    vector: Vec<f32>,
}

/// QA セット 1 件。`correct` は生成時に既知の正解文書 ID 集合、`query_vector` は
/// 正解集合と相関する密クエリベクトル。
struct QaCase {
    query_text: String,
    query_vector: Vec<f32>,
    correct: BTreeSet<u64>,
}

/// トピック `idx` を「密ベクトル空間の次元 `idx`」へ直接対応させる one-hot 構成
/// （次元数 = `vocab_size`）。密ベクトルは基底ベクトル（[`one_hot_sum`]）の和として
/// 合成するため、キーワード集合の共通部分数（`|doc.keywords ∩ query_topics|`）が
/// そのまま内積のスコアになる——BM25 が AND 一致の文書を上位に置く挙動と同型の、
/// 密チャネル側の決定的な信号を作るための設計（ランダム方向ベクトルの平均では
/// トピック間のクロス項ノイズが AND 一致信号を上回りうるため採用しない。
/// `docs/design/hybrid-recall-regression.md` 参照）。
fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

/// [`build_zipf_cumulative_weights`]/[`zipf_index`] は `cjk_tokenizer_impact.rs` と同一の
/// Zipf 近似分布（重み `1/(i+1)`）で、先頭語ほど高頻度・末尾語ほど低頻度にし、QA セットの
/// 正解集合を小さく絞り込める語彙分布を作る。
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
/// `MAX_CORPUS_BYTES` に対応）。環境変数からサイズを受け取らず、テスト内定数のみで
/// 規模を決めることで無制限アロケーションを防ぐ（coding-rust.md「untrusted 入力の扱い」）。
const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

/// 決定的シード付き擬似乱数でトピック相関コーパスと QA セット（`direct` カテゴリ相当:
/// 最も出現頻度の低いキーワード 2 語の AND 組み合わせ）を生成する。`num_docs`・
/// `num_queries`・`vocab_size` は呼び出し側（テスト内定数）が固定し、環境変数からは
/// 受け取らない。`vocab_size` はコーパス規模に応じて選ぶ（QA 正解集合の絞り込み度を
/// 保つため。[`topic_token`] のドキュメント参照）。
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

        let mut text = String::new();
        for (i, &kw_idx) in kw_set.iter().enumerate() {
            if i > 0 {
                text.push(' ');
                text.push_str(FILLER_WORDS[rng.next_range(FILLER_WORDS.len())]);
                text.push(' ');
            }
            text.push_str(&topic_token(kw_idx));
        }

        for &kw_idx in &kw_set {
            inverted.entry(kw_idx).or_default().push(doc_id);
        }
        // 密ベクトルは文書のキーワード集合そのものを one-hot の和として符号化する
        // （[`one_hot_sum`] のドキュメント参照）。疎（BM25）側と同じ「AND 一致度」の
        // 信号を密チャネルにも持たせ、RRF 融合（等重み）で片方の信号がもう片方の
        // ノイズに押し流されないようにする。
        let vector = one_hot_sum(vocab_size, kw_set.iter().copied());
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
/// （`cjk_tokenizer_impact.rs::generate_qa_set` と同方式）。密クエリベクトルは選んだ
/// 2 語を [`one_hot_sum`] で符号化したもの（疎クエリと対応する AND 信号）。
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

// ---------- Recall 測定ヘルパ（production API 経由。engine::hybrid::hybrid_search） ----------

/// Recall@20・Recall@100 の実測結果（ヒット数と理論上限）。
struct RecallResult {
    total_correct: usize,
    hits20: usize,
    hits100: usize,
    ceil20: usize,
    ceil100: usize,
}

/// [`SparseIndex::build`]・[`ParallelSearchProvider`]・[`hybrid_search`]
/// （`RrfConfig::default()` = spec 採用構成: 等重み・pool_depth 200・k_const 60）という
/// production の検索経路のみを用いて Recall@20・Recall@100 を測定する。テスト内で
/// BM25/RRF の再実装は行わない（production コード `crates/engine/src/` は変更しない）。
fn measure_recall(docs: &[Doc], qa: &[QaCase]) -> RecallResult {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text.as_str())).collect();
    let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");

    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let dim = docs.first().map_or(0, |d| d.vector.len());
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector.iter().copied()).collect();
    let provider = ParallelSearchProvider;
    let cfg = RrfConfig::default();

    let mut total_correct = 0usize;
    let mut hits20 = 0usize;
    let mut hits100 = 0usize;
    let mut ceil20 = 0usize;
    let mut ceil100 = 0usize;

    for case in qa {
        total_correct += case.correct.len();
        ceil20 += case.correct.len().min(20);
        ceil100 += case.correct.len().min(100);

        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &case.query_vector,
            k: 100,
        };
        let hits = hybrid_search(&provider, input, &sparse_index, &case.query_text, 100, &cfg)
            .expect("hybrid_search ok");

        hits20 += hits
            .iter()
            .take(20)
            .filter(|h| case.correct.contains(&h.id))
            .count();
        hits100 += hits.iter().filter(|h| case.correct.contains(&h.id)).count();
    }

    RecallResult {
        total_correct,
        hits20,
        hits100,
        ceil20,
        ceil100,
    }
}

/// コーパスが `sparse.rs` の各上限に収まることを検証する（健全性チェック。テスト
/// ハーネス自身にも「無制限なコーパス生成を許さない」設計指針を適用する）。
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

// ---------- 層 A: 小規模段（数百件オーダ。SEARCH-1 対応。固定値回帰トラッキング） ----------

const SMALL_NUM_DOCS: usize = 400;
const SMALL_NUM_QUERIES: usize = 60;
const SMALL_VOCAB_SIZE: usize = 60;
const SMALL_SEED: u64 = 0x5EED_0104_5341_1101;

/// TASK-104（SEARCH-1）: 小規模コーパス（数百件オーダ）での Recall@20 を実測する。
///
/// 決定的コーパス・QA セットのため実測値は再現可能であり、下部の固定値アサーションで
/// 回帰トラッキングする。検索カーネル（トークナイザ・BM25・RRF 融合・密検索 provider）
/// への変更で数値が変化した場合はこのテストが失敗する。
#[test]
fn hybrid_recall_small_scale_regression() {
    let (docs, qa) = generate_corpus(
        SMALL_SEED,
        SMALL_NUM_DOCS,
        SMALL_NUM_QUERIES,
        SMALL_VOCAB_SIZE,
    );
    assert_corpus_within_limits(&docs);
    assert!(!qa.is_empty());
    for case in &qa {
        assert!(!case.correct.is_empty());
    }

    let r = measure_recall(&docs, &qa);
    println!(
        "=== TASK-104 小規模段 Recall（docs={} queries={} total_correct={}） ===",
        docs.len(),
        qa.len(),
        r.total_correct
    );
    println!(
        "Recall@20={:.4} ({}/{})",
        r.hits20 as f64 / r.total_correct as f64,
        r.hits20,
        r.total_correct
    );

    // `total_correct` は理論上限（`ceil20` = Σmin(20,|correct_q|)）より大きくなりうる:
    // Zipf 分布による語彙選択のばらつきで、一部クエリの正解集合が 20 件を超えることが
    // あるため（`cjk_tokenizer_impact.rs` と同じ天井効果）。`hits20 == ceil20`
    // （達成可能な上限に対して 100%）であることを固定値で回帰トラッキングする。
    assert_eq!(r.total_correct, 182, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 178, "Recall@20 の理論上限が変化した");
    assert_eq!(
        r.hits20, 178,
        "小規模段の Recall@20 hit 数が変化した（理論上限に対し 100%）"
    );
}

// ---------- 層 A: 大規模段（数万件オーダ。SEARCH-2 対応。固定値回帰トラッキング） ----------

const LARGE_NUM_DOCS: usize = 20_000;
const LARGE_NUM_QUERIES: usize = 100;
const LARGE_VOCAB_SIZE: usize = 800;
const LARGE_SEED: u64 = 0x5EED_0104_4C41_5247;

/// TASK-104（SEARCH-2）: 大規模コーパス（数万件オーダ）での Recall@20・Recall@100 を
/// 実測する。デバッグビルドでの実行時間を計測した上で PR CI（`cargo test`）に含めている
/// （`ParallelSearchProvider` の並列化・低次元密ベクトルにより実測上許容範囲。実行時間が
/// 悪化した場合は本テストを `#[ignore]` 側へ移し `.github/workflows/recall.yml` 専用に
/// する判断へ切り替えること）。
#[test]
fn hybrid_recall_large_scale_regression() {
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

    let r = measure_recall(&docs, &qa);
    println!(
        "=== TASK-104 大規模段 Recall（docs={} queries={} total_correct={}） ===",
        docs.len(),
        qa.len(),
        r.total_correct
    );
    println!(
        "Recall@20={:.4} ({}/{})  Recall@100={:.4} ({}/{})",
        r.hits20 as f64 / r.total_correct as f64,
        r.hits20,
        r.total_correct,
        r.hits100 as f64 / r.total_correct as f64,
        r.hits100,
        r.total_correct
    );

    // `hybrid_recall_small_scale_regression` と同じ天井効果（一部クエリの正解集合が
    // 20/100 件を超えるため `total_correct` は理論上限より大きい）。`hits == ceil`
    // （達成可能な上限に対して 100%）であることを固定値で回帰トラッキングする。
    assert_eq!(r.total_correct, 1217, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 333, "Recall@20 の理論上限が変化した");
    assert_eq!(r.ceil100, 736, "Recall@100 の理論上限が変化した");
    assert_eq!(
        r.hits20, 333,
        "大規模段の Recall@20 hit 数が変化した（理論上限に対し 100%）"
    );
    assert_eq!(
        r.hits100, 736,
        "大規模段の Recall@100 hit 数が変化した（理論上限に対し 100%）"
    );
}

// ---------- 層 B: spec 閾値ゲート（`#[ignore]`。`make recall-regression` 専用） ----------

/// `HYBRID_RECALL_MIN_*` 環境変数（`(0.0, 1.0]` の浮動小数点）を読み取る。未設定・
/// 非数値・範囲外は fail-closed（`Err`）とし、呼び出し側でテスト失敗として扱う
/// （`crates/engine/benches/parallel_bench.rs::min_recall_from_env` と同方針）。
/// 数値そのもの（spec の Recall 下限）はこのファイル・ログのいずれにもハードコード
/// しない（`.claude/rules/spec-confidentiality.md`）。
fn min_recall_from_env(var: &str) -> Result<f64, String> {
    let raw = std::env::var(var)
        .map_err(|_| format!("{var} is not set (see .github/workflows/recall.yml vars)"))?;
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{var} must be a floating-point number"))?;
    if !(value > 0.0 && value <= 1.0) {
        return Err(format!("{var} must be within (0.0, 1.0]"));
    }
    Ok(value)
}

/// TASK-104（SEARCH-1）層 B: 小規模段 Recall@20 が `HYBRID_RECALL_MIN_R20_SMALL`
/// （Actions variables 由来）以上であることを確認する閾値ゲート。未設定・非数値・
/// 範囲外は fail-closed でテスト失敗とする（skip しない）。ログには実測値と pass/fail
/// のみを出力し、注入された閾値の数値は出力しない。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make recall-regression で実行する"]
fn hybrid_recall_small_scale_threshold_gate() {
    let min_r20 = min_recall_from_env("HYBRID_RECALL_MIN_R20_SMALL")
        .expect("HYBRID_RECALL_MIN_R20_SMALL invalid");

    let (docs, qa) = generate_corpus(
        SMALL_SEED,
        SMALL_NUM_DOCS,
        SMALL_NUM_QUERIES,
        SMALL_VOCAB_SIZE,
    );
    let r = measure_recall(&docs, &qa);
    let recall20 = r.hits20 as f64 / r.total_correct as f64;
    let pass = recall20 >= min_r20;

    println!("hybrid_recall_small_scale_threshold_gate: recall@20={recall20:.4} pass={pass}");
    assert!(
        pass,
        "small-scale Recall@20 below HYBRID_RECALL_MIN_R20_SMALL"
    );
}

/// TASK-104（SEARCH-2）層 B: 大規模段 Recall@20・Recall@100 がそれぞれ
/// `HYBRID_RECALL_MIN_R20_LARGE`・`HYBRID_RECALL_MIN_R100_LARGE` 以上であることを
/// 確認する閾値ゲート。契約は
/// [`hybrid_recall_small_scale_threshold_gate`] と同一。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make recall-regression で実行する"]
fn hybrid_recall_large_scale_threshold_gate() {
    let min_r20 = min_recall_from_env("HYBRID_RECALL_MIN_R20_LARGE")
        .expect("HYBRID_RECALL_MIN_R20_LARGE invalid");
    let min_r100 = min_recall_from_env("HYBRID_RECALL_MIN_R100_LARGE")
        .expect("HYBRID_RECALL_MIN_R100_LARGE invalid");

    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    let r = measure_recall(&docs, &qa);
    let recall20 = r.hits20 as f64 / r.total_correct as f64;
    let recall100 = r.hits100 as f64 / r.total_correct as f64;
    let pass20 = recall20 >= min_r20;
    let pass100 = recall100 >= min_r100;

    println!(
        "hybrid_recall_large_scale_threshold_gate: recall@20={recall20:.4} pass20={pass20} recall@100={recall100:.4} pass100={pass100}"
    );
    assert!(
        pass20,
        "large-scale Recall@20 below HYBRID_RECALL_MIN_R20_LARGE"
    );
    assert!(
        pass100,
        "large-scale Recall@100 below HYBRID_RECALL_MIN_R100_LARGE"
    );
}
