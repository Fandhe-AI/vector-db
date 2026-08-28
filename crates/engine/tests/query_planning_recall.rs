//! TASK-112（対象ビヘイビア: PLAN-1, PLAN-2。ポインタ: `docs/spec/05-tasks.md`
//! TASK-112・`docs/spec/04-behavior/query-planning.md` PLAN-1, PLAN-2）。
//!
//! TASK-110（クエリ展開クライアント `crates/engine/src/query_planner.rs`）が返す
//! [`QueryExpansion`] が、実際に検索 Recall@20 を改善するかどうかを実測する受け入れ
//! 基準の回帰テスト。`crates/engine/tests/rerank_recall.rs`（TASK-108）と同型の
//! 構成（決定的合成コーパス・[`RecallResult`] 相当・2 層構成）を踏襲する
//! （「複製・踏襲」方式。既存テストの固定値アサーションへは手を入れない）。
//! production コード（`crates/engine/src/`）は変更しない。
//!
//! **2 カテゴリの QA**（いずれも同じ潜在キーワードペア `(a, b)` から生成し、
//! 正解集合 `correct` を共有する。カテゴリ間で難易度以外の条件を揃えるため）:
//! - **direct**: クエリ語がコーパスの内容語トークン（`kw_XXXX`）と一致する（`hybrid_recall.rs`/
//!   `rerank_recall.rs` の QA と同じ構成）
//! - **intent**: クエリ語がコーパス語彙と重ならない「言い換え語彙」（`syn_XXXX`）のみで
//!   構成される。展開なしの baseline は疎チャネル（トークン不一致）・密チャネル（対応する
//!   ベクトル信号を持たない = ゼロベクトル）のいずれからも手がかりを得られない構成にし、
//!   「クエリ展開なしでは初見の言い換えに対応できない」状況を模する
//!
//! **展開なし / 展開あり の比較**（production API のみを使用。BM25/RRF/JSON パースの
//! 再実装はテスト内で行わない）:
//! - **baseline（展開なし）**: 各カテゴリのクエリ語をそのまま `hybrid::hybrid_search`
//!   （`RrfConfig::default()`）へ渡した Recall@20
//! - **after（展開あり）**: [`MockLlmClient`]（決定的な同義語対応表で `syn_XXXX` を
//!   `kw_XXXX` へ写像し、他の語はそのまま通す）を `query_planner::render_full_prompt`
//!   （固定接頭辞は空文字列。接頭辞レンダリング自体は `tests/query_planner.rs` が
//!   別途検証済みのため本ファイルの対象外）→ `LlmClient::complete` → `query_planner::
//!   parse_expansion` の一連（LLM 出力の fail-closed 検証経路を含む）に通して得た
//!   [`QueryExpansion::search_terms`] からクエリを再構成し、同じ `hybrid_search` で
//!   測定した Recall@20（`path_hint`/`kind_hint` を用いたソフトブースト経路は
//!   `tests/soft_boost.rs` が別途検証済みのため本ファイルの対象外）
//!
//! Recall@20 は `hybrid_recall.rs::RecallResult` と同じ理由で、分母に正解集合の
//! 総数ではなく理論上限 `ceil20`（Σmin(20,\|correct_q\|)）を使う。
//!
//! 2 層構成（`rerank_recall.rs` と同方針。ただし本ファイルは合成コーパスの実測値
//! （Recall・hit 数等）をコード・CI ログのいずれにも数値として残さない
//! （`.claude/rules/spec-confidentiality.md`／AGENTS.md「数値基準・実測値・spec
//! 本文の転記は引き続き P0」）:
//! - 層 A（`#[test]`・PR CI 常時実行）: baseline/after の hits をカテゴリ間・
//!   before/after 間の相対関係（不等号・等号）のみで回帰トラッキングする（絶対数値の
//!   固定値アサーション・数値の標準出力は行わない）。「intent は after が baseline を
//!   上回る」「direct は after が baseline を下回らない（展開が既存の強みを破壊
//!   しない）」ことを独立にアサートする
//! - 層 B（`#[ignore]`・`make query-planning-regression` 経由）: spec 由来の下限
//!   （`QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT`＝intent の改善幅下限・
//!   `QUERY_PLANNING_RECALL_MIN_R20_DIRECT`＝direct の絶対下限・
//!   `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED`＝[`NoisyLlmClient`]
//!   （非 oracle・劣化展開品質）による intent 改善幅下限。`.github/workflows/
//!   recall.yml` が environment `recall-gate` の Actions variables から注入）と
//!   実測値を比較する閾値ゲート。`QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS=1`
//!   （`recall.yml` の Run step からのみ注入）で未設定を fail-closed にする strict
//!   モードを持つ（`rerank_recall.rs::resolve_gate_threshold` と同型。ログには
//!   pass/fail のみを出力し、注入された閾値・実測値のいずれの数値も出力しない）。
//!   `MockLlmClient`（完全 oracle 写像）の下限のみでは production の展開品質劣化を
//!   検出できない（codex-review・PR #265・P1 指摘）ため、[`NoisyLlmClient`] による
//!   劣化展開の実測を、oracle 写像の下限とは独立の第 3 の下限として同一ゲートへ
//!   接続する（既存 2 下限のオーバーロードによる誤検知は既に実測で確認済み・
//!   `docs/design/query-planning-recall-regression.md` 参照。値そのものは
//!   引き続き spec・オーナー側の判断事項）
//!
//! 既知の制約（スコープ外・フォローアップ）:
//! - 実 Ollama 接続での実測は対象外（TASK-110 時点からの継続制約。本テストは
//!   決定的スタブ `LlmClient` で LLM 出力の受理契約のみを固定する）
//! - 数万チャンク規模ケースの追加は TASK-113 が本ファイルへ後続で追加する
//! - `search_query:` プレフィックス再埋め込み（TASK-114）は未実装のため、本テストの
//!   再構成クエリは埋め込みの使い回しではなく合成 one-hot ベクトルの再合成で代替する
//! - 合成コーパスによる暫定測定であり、実コーパスでの評価は未了（`hybrid_recall.rs`
//!   と同種の制約）

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::query_planner::parse_expansion;
use engine::query_planner::{render_full_prompt, LlmClient, PlanError, QueryExpansion};
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

// ---------- 合成コーパスの語彙 ----------

/// トピック `idx` に対応するキーワードトークン（コーパス内容語・direct クエリの語彙）
/// （`hybrid_recall.rs::topic_token` と同一実装）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

/// トピック `idx` に対応する「言い換え語彙」トークン（intent クエリの語彙。コーパスの
/// 内容語トークンとは文字列上重複しない別語彙を割り当てる）。[`MockLlmClient`] が
/// この形式のトークンだけを [`topic_token`] へ写像する。
fn synonym_token(idx: usize) -> String {
    format!("syn_{idx:04}")
}

/// [`synonym_token`] の逆写像。プレフィックス不一致・非数値は `None`（想定外の語は
/// 写像しない = そのまま通す。[`MockLlmClient::complete`] 参照）。
fn parse_synonym_index(token: &str) -> Option<usize> {
    token.strip_prefix("syn_")?.parse().ok()
}

/// [`topic_token`] の逆写像。展開結果（[`QueryExpansion::search_terms`]）から再構成
/// クエリの密ベクトルを組み立てる際に使う。
fn parse_topic_index(token: &str) -> Option<usize> {
    token.strip_prefix("kw_")?.parse().ok()
}

/// 文脈語プール（内容語の間に挟む機能語。BM25 の相対比較には影響しない飾り）。
const FILLER_WORDS: [&str; 10] = [
    "the", "a", "an", "of", "for", "and", "with", "in", "on", "note",
];

/// 合成コーパス 1 文書（`hybrid_recall.rs::Doc` と同一の役割・構造）。
struct Doc {
    id: u64,
    text: String,
    keywords: BTreeSet<usize>,
    vector: Vec<f32>,
}

/// キーワードペア `(a, b)` を AND 条件とする QA 1 件分の共通部分（direct/intent 両
/// カテゴリが同じ `correct` を共有する。難易度以外の条件を揃えるため）。
struct PairQa {
    keyword_a: usize,
    keyword_b: usize,
    correct: BTreeSet<u64>,
}

/// トピック `idx` を「密ベクトル空間の次元 `idx`」へ直接対応させる one-hot 構成
/// （`hybrid_recall.rs::one_hot_sum` と同一実装）。
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

/// fixture パラメータ（`hybrid_recall.rs`/`rerank_recall.rs` と同一値。spec 由来の
/// 数値基準ではなく、Recall を 1.0 未満の現実的な分布にするために実験的に選んだ確率）。
const TEXT_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_DECOY_PROB: f64 = 0.12;

/// 決定的シード付き擬似乱数でトピック相関コーパスと `PairQa` セットを生成する
/// （`hybrid_recall.rs::generate_corpus`/`rerank_recall.rs::generate_corpus` と同一の
/// コーパス生成部＋`rerank_recall.rs::generate_qa_set` と同一の QA 選出部）。
fn generate_corpus(
    seed: u64,
    num_docs: usize,
    num_pairs: usize,
    vocab_size: usize,
) -> (Vec<Doc>, Vec<PairQa>) {
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

    let pairs = generate_pair_qa_set(&mut rng, &docs, &inverted, num_pairs);
    (docs, pairs)
}

/// 各文書から最も出現頻度の低いキーワード 2 語（AND 組み合わせ）を選び、正解集合が
/// コーパス全体に対して十分に絞り込まれたペアを構成する（`rerank_recall.rs::
/// generate_qa_set` と同一のペア選出ロジック。生成結果は [`PairQa`]（direct/intent
/// 両カテゴリの共通部分）にとどめ、カテゴリ別の query_text/query_vector への変換は
/// 呼び出し側 [`direct_case`]/[`intent_case`] が行う）。
fn generate_pair_qa_set(
    rng: &mut Xorshift64,
    docs: &[Doc],
    inverted: &BTreeMap<usize, Vec<u64>>,
    num_pairs: usize,
) -> Vec<PairQa> {
    let mut order: Vec<usize> = (0..docs.len()).collect();
    for i in (1..order.len()).rev() {
        let j = rng.next_range(i + 1);
        order.swap(i, j);
    }

    let mut pairs = Vec::with_capacity(num_pairs);
    let mut seen_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for &doc_idx in &order {
        if pairs.len() >= num_pairs {
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

        pairs.push(PairQa {
            keyword_a: a,
            keyword_b: b,
            correct,
        });
    }

    pairs
}

/// コーパスが `sparse.rs` の各上限に収まることを検証する（`hybrid_recall.rs::
/// assert_corpus_within_limits` と同一実装）。
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

// ---------- 展開なし（baseline）クエリの構成（カテゴリ別） ----------

/// direct カテゴリの baseline クエリ（コーパス内容語トークンそのもの・対応する
/// one-hot 密ベクトル）。
fn direct_baseline(vocab_size: usize, pair: &PairQa) -> (String, Vec<f32>) {
    let text = format!(
        "{} {}",
        topic_token(pair.keyword_a),
        topic_token(pair.keyword_b)
    );
    let vector = one_hot_sum(vocab_size, [pair.keyword_a, pair.keyword_b]);
    (text, vector)
}

/// intent カテゴリの baseline クエリ（言い換え語彙トークンのみ。コーパスの内容語
/// トークンと文字列上重複しないため疎チャネルは手がかりを得られず、対応する密
/// ベクトル信号も存在しないためゼロベクトルとする。「クエリ展開なしでは初見の
/// 言い換えに対応できない」状況の最小モデル）。
fn intent_baseline(vocab_size: usize, pair: &PairQa) -> (String, Vec<f32>) {
    let text = format!(
        "{} {}",
        synonym_token(pair.keyword_a),
        synonym_token(pair.keyword_b)
    );
    let vector = vec![0.0f32; vocab_size];
    (text, vector)
}

// ---------- 展開あり（after）クエリの再構成（production API 経由） ----------

/// [`query_planner::render_full_prompt`]（TASK-110）が組み立てるプロンプトを受け取り、
/// 決定的な同義語対応表で言い換え語彙（`syn_XXXX`）だけをコーパス内容語
/// （`kw_XXXX`）へ写像し、他の語（すでに `kw_XXXX` 形式の direct クエリ語）はそのまま
/// 通す。実 Ollama へは接続しない（TASK-110 時点からの継続制約。ファイル冒頭の
/// 「既知の制約」参照）。
struct MockLlmClient;

impl LlmClient for MockLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        // `render_full_prompt` は接頭辞（本テストでは空文字列）の直後に
        // "\n# Question\n" ＋サニタイズ済み質問＋"\n" を追記する契約
        // （`query_planner.rs::render_full_prompt` 参照）。マーカ以降の 1 行を
        // 質問本文として取り出す。
        let question = prompt
            .split("# Question\n")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .unwrap_or("");

        let mapped_terms: Vec<String> = question
            .split_whitespace()
            .map(|token| match parse_synonym_index(token) {
                Some(idx) => topic_token(idx),
                None => token.to_string(),
            })
            .collect();

        // `parse_expansion`（production API）がそのまま受理できる最小 JSON を
        // 組み立てる（検索語は制御文字を含まない ASCII トークンのみなので、
        // 文字列エスケープは不要）。
        let terms_json = mapped_terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(",");
        Ok(format!(
            "{{\"search_terms\":[{terms_json}],\"path_hint\":null,\"kind_hint\":null}}"
        ))
    }
}

/// [`MockLlmClient`]（完全 oracle 写像）よりも劣化した展開品質を模する決定的スタブ
/// `LlmClient`。言い換え語彙のうち半数（インデックス偶奇で決定的に選出）だけを
/// コーパス内容語へ正しく写像し、残り半数は写像に失敗した production LLM を模して
/// 元の言い換え語彙のまま通す（= 密チャネル・疎チャネルいずれからも手がかりを
/// 得られない語として残る）。codex-review（PR #265・P2）が指摘した「層 B の受け入れ
/// ゲートが `MockLlmClient` の完全 oracle 写像に依存し、展開品質の劣化を検出できない」
/// という制約に対応するため、[`query_planning_recall_detects_degraded_expansion_quality`]
/// （層 A）でこの劣化構成を実測し完全写像との Recall@20 差を独立にアサートするのに
/// 加え、[`query_planning_recall_threshold_gate`]（層 B）でも独立の下限
/// `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED` と比較する
/// （codex-review・PR #265・P1 指摘への対応。既存の oracle 写像下限を非 oracle
/// スタブへ流用すると誤検知することが実測済みのため、専用の下限を別変数として
/// 独立に解決する。`docs/design/query-planning-recall-regression.md` 参照）。
struct NoisyLlmClient;

impl LlmClient for NoisyLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        let question = prompt
            .split("# Question\n")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .unwrap_or("");

        let mapped_terms: Vec<String> = question
            .split_whitespace()
            .map(|token| match parse_synonym_index(token) {
                // 偶数インデックスのみ正しく写像し、奇数インデックスは意図的に
                // 未写像のまま通す（production LLM の部分的な写像失敗を模する）。
                Some(idx) if idx % 2 == 0 => topic_token(idx),
                _ => token.to_string(),
            })
            .collect();

        let terms_json = mapped_terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(",");
        Ok(format!(
            "{{\"search_terms\":[{terms_json}],\"path_hint\":null,\"kind_hint\":null}}"
        ))
    }
}

/// `baseline_query_text` を質問として [`query_planner::render_full_prompt`]（固定
/// 接頭辞は空文字列）→ `client.complete` → [`query_planner::
/// parse_expansion`] の一連（production API）に通し、得られた [`QueryExpansion::
/// search_terms`] から再構成クエリ（疎チャネル用テキスト・密チャネル用 one-hot
/// ベクトル）を組み立てる。`client` を差し替え可能にすることで、[`MockLlmClient`]
/// の完全 oracle 写像に限らず、[`NoisyLlmClient`] のような劣化展開品質のシナリオも
/// 同じ経路で測定できる。
fn expand_and_reconstruct_with(
    client: &dyn LlmClient,
    vocab_size: usize,
    baseline_query_text: &str,
) -> (String, Vec<f32>) {
    let prompt = render_full_prompt("", baseline_query_text).expect("render_full_prompt ok");
    let response = client.complete(&prompt).expect("mock complete ok");
    let expansion: QueryExpansion = parse_expansion(&response).expect("parse_expansion ok");

    let text = expansion.search_terms.join(" ");
    let indices = expansion
        .search_terms
        .iter()
        .filter_map(|t| parse_topic_index(t));
    let vector = one_hot_sum(vocab_size, indices);
    (text, vector)
}

// ---------- Recall 測定 ----------

/// 1 カテゴリ（direct または intent）の baseline/after Recall@20 測定結果
/// （`rerank_recall.rs::RerankRecallResult` と同じ理由で、分母には正解集合の総数
/// ではなく理論上限 `ceil20` を使う）。
struct CategoryRecallResult {
    total_correct: usize,
    baseline_hits20: usize,
    after_hits20: usize,
    ceil20: usize,
}

impl CategoryRecallResult {
    fn baseline_recall20(&self) -> f64 {
        self.baseline_hits20 as f64 / self.ceil20 as f64
    }

    fn after_recall20(&self) -> f64 {
        self.after_hits20 as f64 / self.ceil20 as f64
    }
}

/// [`SparseIndex::build`]・[`ParallelSearchProvider`]・[`hybrid_search`]
/// （`RrfConfig::default()`）で baseline（`baseline_query_fn` が組み立てるクエリ）・
/// after（`client` を通した [`expand_and_reconstruct_with`] 経由の再構成クエリ）の
/// Recall@20 を測定する。BM25/RRF/JSON パースの再実装はテスト内で行わない
/// （production コード `crates/engine/src/` は変更しない）。
fn measure_category_recall_with_client(
    docs: &[Doc],
    pairs: &[PairQa],
    vocab_size: usize,
    baseline_query_fn: impl Fn(usize, &PairQa) -> (String, Vec<f32>),
    client: &dyn LlmClient,
) -> CategoryRecallResult {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text.as_str())).collect();
    let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");

    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector.iter().copied()).collect();
    let provider = ParallelSearchProvider;
    let cfg = RrfConfig::default();

    let mut total_correct = 0usize;
    let mut baseline_hits20 = 0usize;
    let mut after_hits20 = 0usize;
    let mut ceil20 = 0usize;

    for pair in pairs {
        total_correct += pair.correct.len();
        ceil20 += pair.correct.len().min(20);

        let (baseline_text, baseline_vector) = baseline_query_fn(vocab_size, pair);
        let baseline_input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: vocab_size as u32,
            query: &baseline_vector,
            // `hybrid::hybrid_search` は密チャネルを常に `cfg.pool_depth()`
            // （`RrfConfig::default()` では 200）で問い合わせ、この `k` フィールドは
            // 上書きして無視する（`hybrid.rs::hybrid_search` ドキュメント「密側は
            // provider.search() を k = cfg.pool_depth で実行し（input.k は本関数が
            // 上書きするため呼び出し元の値は無視される）」参照）。呼び出し元の
            // 検証（`SearchInput` の型契約）を満たすためだけの値であり、
            // Recall@20 の測定対象（末尾 `hybrid_search(..., 20, &cfg)` の `20`）
            // とは無関係。
            k: DENSE_CANDIDATE_DEPTH,
        };
        let baseline_hits = hybrid_search(
            &provider,
            baseline_input,
            &sparse_index,
            &baseline_text,
            20,
            &cfg,
        )
        .expect("hybrid_search (baseline) ok");
        baseline_hits20 += baseline_hits
            .iter()
            .filter(|h| pair.correct.contains(&h.id))
            .count();

        let (after_text, after_vector) =
            expand_and_reconstruct_with(client, vocab_size, &baseline_text);
        let after_input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: vocab_size as u32,
            query: &after_vector,
            k: DENSE_CANDIDATE_DEPTH,
        };
        let after_hits =
            hybrid_search(&provider, after_input, &sparse_index, &after_text, 20, &cfg)
                .expect("hybrid_search (after) ok");
        after_hits20 += after_hits
            .iter()
            .filter(|h| pair.correct.contains(&h.id))
            .count();
    }

    CategoryRecallResult {
        total_correct,
        baseline_hits20,
        after_hits20,
        ceil20,
    }
}

/// [`measure_category_recall_with_client`] を [`MockLlmClient`]（完全 oracle 写像）で
/// 固定した従来の呼び出し口（既存の層 A/B テストが使う既定経路）。
fn measure_category_recall(
    docs: &[Doc],
    pairs: &[PairQa],
    vocab_size: usize,
    baseline_query_fn: impl Fn(usize, &PairQa) -> (String, Vec<f32>),
) -> CategoryRecallResult {
    measure_category_recall_with_client(docs, pairs, vocab_size, baseline_query_fn, &MockLlmClient)
}

// ---------- 層 A: 相対関係による回帰トラッキング（PR CI 常時実行） ----------

const NUM_DOCS: usize = 4_000;
const NUM_PAIRS: usize = 80;
const VOCAB_SIZE: usize = 500;
const SEED: u64 = 0x5EED_0112_5150_4C41;
/// 密チャネルの候補取得件数（`rerank_recall.rs::measure_rerank_recall` の
/// `SearchInput.k` と同一値。[`measure_category_recall`] 参照）。
const DENSE_CANDIDATE_DEPTH: usize = 100;

/// TASK-112（PLAN-1, PLAN-2）層 A: intent/direct 両カテゴリの baseline（展開なし）・
/// after（展開あり）Recall@20 を実測し、カテゴリ間・before/after 間の相対関係
/// （不等号・等号）のみで回帰トラッキングする（絶対数値はコード・標準出力のいずれにも
/// 残さない。AGENTS.md「数値基準・実測値・spec 本文の転記は引き続き P0」）。あわせて
/// 「intent は after が baseline を上回る」（PLAN-1）「direct は after が baseline と
/// 一致する」（PLAN-2。本テストの `MockLlmClient` は direct 語彙を無変換で通すため
/// 理論上完全一致になる）ことを独立にアサートする（spec の数値基準は使わない）。
#[test]
fn query_planning_recall_regression() {
    let (docs, pairs) = generate_corpus(SEED, NUM_DOCS, NUM_PAIRS, VOCAB_SIZE);
    assert_corpus_within_limits(&docs);
    assert!(!pairs.is_empty());
    for pair in &pairs {
        assert!(!pair.correct.is_empty());
    }
    assert_eq!(pairs.len(), NUM_PAIRS, "重複除外後のペア件数が変化した");

    let direct = measure_category_recall(&docs, &pairs, VOCAB_SIZE, direct_baseline);
    let intent = measure_category_recall(&docs, &pairs, VOCAB_SIZE, intent_baseline);

    // 実測の Recall・hit 数は標準出力（public な CI ログ）へ出さない。固定構成
    // （docs/pairs/vocab は本ファイルのフィクスチャ定数であり実測値ではない）のみ出力する。
    println!(
        "=== TASK-112 Recall（docs={} pairs={} vocab={}） ===",
        docs.len(),
        pairs.len(),
        VOCAB_SIZE
    );

    // PLAN-1: intent は展開ありが展開なしを上回ることの独立したアサーション
    // （固定値の再確定漏れでもこの性質が崩れれば検出できるようにする）。
    assert!(
        intent.after_hits20 > intent.baseline_hits20,
        "intent カテゴリで展開ありが展開なしを上回らなかった"
    );
    // PLAN-2: direct は展開ありが展開なし比で下回らないことの独立したアサーション
    // （展開が既存の強みを破壊しないことの最小保証）。
    assert!(
        direct.after_hits20 >= direct.baseline_hits20,
        "direct カテゴリで展開ありの Recall@20 が展開なしを下回った"
    );

    // `hits`/`ceil`/`total_correct` をカテゴリ間・before/after 間の相対関係
    // （不等号・等号）のみで回帰トラッキングする（検索カーネル・クエリ展開パーサ・
    // フィクスチャの変更でこれらの関係が崩れた場合にこのテストが失敗する。絶対数値の
    // 固定値アサーションは行わない）。direct/intent は同一 `pairs`（同一 `correct`）
    // から生成するため `total_correct`/`ceil20` は両カテゴリで一致する。
    assert_eq!(
        direct.total_correct,
        pairs.iter().map(|p| p.correct.len()).sum::<usize>()
    );
    assert_eq!(intent.total_correct, direct.total_correct);
    assert_eq!(
        direct.ceil20,
        pairs.iter().map(|p| p.correct.len().min(20)).sum::<usize>()
    );
    assert_eq!(direct.ceil20, intent.ceil20);
    // direct カテゴリは `MockLlmClient` が語彙を無変換で通すため、展開の有無で
    // Recall@20 hit 数が理論上完全一致する（PLAN-2 の `>=` より強い性質）。
    assert_eq!(
        direct.after_hits20, direct.baseline_hits20,
        "direct カテゴリで展開ありが展開なしと一致しなかった（MockLlmClient の無変換パススルー性質が崩れた）"
    );
    // 展開後の intent クエリは direct クエリと語彙的に等価になるため、after の
    // hit 数はカテゴリを跨いで一致する（`syn_XXXX` → `kw_XXXX` の同義語対応表が
    // 全射であることの回帰検知）。
    assert_eq!(
        intent.after_hits20, direct.after_hits20,
        "intent カテゴリの after hit 数が direct カテゴリの after hit 数と一致しなかった"
    );
    // 展開なしの intent は「初見の言い換えに対応できない」構成のため、direct の
    // baseline を厳密に下回る（コーパス語彙と無関係な `syn_XXXX` のみで構成される
    // ため、疎チャネル・密チャネルいずれからも手がかりを得られない）。
    assert!(
        intent.baseline_hits20 < direct.baseline_hits20,
        "intent カテゴリ baseline（展開なし）が direct カテゴリ baseline を下回らなかった"
    );
}

/// TASK-112（PLAN-1, PLAN-2）層 A 追補: codex-review（PR #265・P2）が指摘した
/// 「層 B の受け入れゲートが `MockLlmClient` の完全 oracle 写像に依存し、production
/// の LLM 応答品質・展開戦略の劣化を検出できない」という制約に対応する。[`MockLlmClient`]
/// （言い換え語彙を完全に写像）と [`NoisyLlmClient`]（半数だけ写像し、残り半数は
/// 未写像のまま通す＝展開品質が劣化した production LLM を模する）の両方で intent
/// カテゴリの Recall@20 を実測し、劣化側が完全写像側を厳密に下回ることを独立に
/// アサートする。これにより、本ハーネスは「`MockLlmClient` が返す固定値をなぞる」
/// だけでなく、展開品質そのものの劣化を検出できることを回帰保証する（oracle 写像に
/// 依存しない展開品質評価の追加）。
#[test]
fn query_planning_recall_detects_degraded_expansion_quality() {
    let (docs, pairs) = generate_corpus(SEED, NUM_DOCS, NUM_PAIRS, VOCAB_SIZE);
    assert_corpus_within_limits(&docs);
    assert!(!pairs.is_empty());

    let full_oracle = measure_category_recall_with_client(
        &docs,
        &pairs,
        VOCAB_SIZE,
        intent_baseline,
        &MockLlmClient,
    );
    let degraded = measure_category_recall_with_client(
        &docs,
        &pairs,
        VOCAB_SIZE,
        intent_baseline,
        &NoisyLlmClient,
    );

    // 実測の Recall・hit 数は標準出力（public な CI ログ）へ出さない。
    println!(
        "=== TASK-112 展開品質劣化検出（docs={} pairs={} vocab={}） ===",
        docs.len(),
        pairs.len(),
        VOCAB_SIZE
    );

    // 展開品質が劣化した NoisyLlmClient は、完全 oracle 写像の MockLlmClient を
    // Recall@20 で厳密に下回る（本ハーネスが展開戦略の劣化を検出できることの
    // 独立したアサーション。この不等号が崩れた場合、劣化検出感度が失われている）。
    assert!(
        degraded.after_hits20 < full_oracle.after_hits20,
        "展開品質が劣化した NoisyLlmClient の Recall@20 hit 数が、完全 oracle 写像の \
         MockLlmClient を下回らなかった（劣化検出感度が失われている）"
    );
    // それでも NoisyLlmClient は baseline（展開なし）よりは改善する（半数は正しく
    // 写像されるため）。劣化検出テストが「展開が完全に無意味になった」極端な構成に
    // 依存していないことを示す。
    assert!(
        degraded.after_hits20 > degraded.baseline_hits20,
        "展開品質が劣化した NoisyLlmClient でも baseline（展開なし）を上回らなかった"
    );
}

// ---------- 層 B: spec 閾値ゲート（`#[ignore]`。`make query-planning-regression` 専用） ----------

/// `QUERY_PLANNING_RECALL_MIN_*` 環境変数（`(0.0, 1.0]` または `[0.0, 1.0]` の
/// 浮動小数点）の解決結果（`rerank_recall.rs::GateThreshold` と同一の役割）。
enum GateThreshold {
    /// 環境変数が未設定、または GitHub Actions の未設定 repo/environment variable が
    /// 解決する空文字列。
    NotConfigured,
    /// 設定済みで許容範囲内。この場合のみ実測値と比較する。
    Value(f64),
}

/// 環境変数を f64 として読み取り、`validate` で許容範囲を検査する共通ヘルパ
/// （`rerank_recall.rs::threshold_from_env` と同一実装）。数値そのもの（spec の
/// Recall 下限）はこのファイル・ログのいずれにもハードコードしない
/// （`.claude/rules/spec-confidentiality.md`）。
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

/// `QUERY_PLANNING_RECALL_MIN_R20_DIRECT` 環境変数を読み取る（`(0.0, 1.0]` の絶対
/// 下限。0 は「Recall@20 が 0 でよい」という無意味な設定になるため許容しない）。
fn recall_threshold_from_env(var: &str) -> Result<GateThreshold, String> {
    threshold_from_env(var, |v| v > 0.0 && v <= 1.0, "(0.0, 1.0]")
}

/// `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT` 環境変数を読み取る（改善幅 =
/// after − baseline の下限。`rerank_recall.rs::improvement_threshold_from_env` と
/// 同じ理由で `[0.0, 1.0]`（0 を含む）を許容範囲とする）。
fn improvement_threshold_from_env(var: &str) -> Result<GateThreshold, String> {
    threshold_from_env(var, |v| (0.0..=1.0).contains(&v), "[0.0, 1.0]")
}

/// `QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS` 環境変数（`"1"` のときのみ true）。
/// `.github/workflows/recall.yml` からの実行（dispatch / schedule）時のみ注入される
/// strict モードフラグ（`rerank_recall.rs::strict_thresholds_required` と同型）。
fn strict_thresholds_required() -> bool {
    std::env::var("QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// `resolver` を読み取り、[`GateThreshold::NotConfigured`] を strict モード
/// （[`strict_thresholds_required`]）に応じて分岐させる共通ヘルパ（`rerank_recall.rs::
/// resolve_gate_threshold_with` と同一の役割）。strict モード有効時の未設定は
/// fail-closed（`panic!`）、無効時は `None`（呼び出し側で「対象外」を出力して
/// early return する）。非数値・範囲外は strict モードの有無によらず常に fail-closed
/// とする。
fn resolve_gate_threshold_with(
    var: &str,
    resolver: impl Fn(&str) -> Result<GateThreshold, String>,
) -> Option<f64> {
    match resolver(var) {
        Ok(GateThreshold::Value(v)) => Some(v),
        Ok(GateThreshold::NotConfigured) => {
            if strict_thresholds_required() {
                panic!(
                    "{var} is not configured but QUERY_PLANNING_RECALL_REQUIRE_THRESHOLDS=1 (strict mode: this run must evaluate all QUERY_PLANNING_RECALL_MIN_* thresholds; see .github/workflows/recall.yml and the recall-gate environment variables)"
                );
            }
            None
        }
        Err(msg) => panic!("{var} invalid: {msg}"),
    }
}

/// `QUERY_PLANNING_RECALL_MIN_R20_DIRECT` 用の [`resolve_gate_threshold_with`]。
fn resolve_gate_threshold(var: &str) -> Option<f64> {
    resolve_gate_threshold_with(var, recall_threshold_from_env)
}

/// `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT` 用の [`resolve_gate_threshold_with`]
/// （許容範囲 `[0.0, 1.0]` の [`improvement_threshold_from_env`] を使う点のみ
/// [`resolve_gate_threshold`] と異なる）。
fn resolve_improvement_gate_threshold(var: &str) -> Option<f64> {
    resolve_gate_threshold_with(var, improvement_threshold_from_env)
}

/// `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED` 用の
/// [`resolve_gate_threshold_with`]（許容範囲 `[0.0, 1.0]` の
/// [`improvement_threshold_from_env`] を使う点は [`resolve_improvement_gate_threshold`]
/// と同じ。[`NoisyLlmClient`]（非 oracle・劣化展開品質）による intent 改善幅の下限を
/// 独立に解決する。既存 2 変数の値をそのまま流用しない理由は
/// [`query_planning_recall_threshold_gate`] のドキュメンテーションコメント参照）。
fn resolve_degraded_improvement_gate_threshold(var: &str) -> Option<f64> {
    resolve_gate_threshold_with(var, improvement_threshold_from_env)
}

/// TASK-112（PLAN-1, PLAN-2）層 B: intent カテゴリの改善幅（after − baseline）が
/// `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT` 以上、direct カテゴリの
/// after Recall@20 が `QUERY_PLANNING_RECALL_MIN_R20_DIRECT`（絶対下限）以上、かつ
/// [`NoisyLlmClient`]（非 oracle・劣化展開品質）による intent カテゴリの改善幅が
/// `QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED` 以上であることを確認する
/// 閾値ゲート。契約は `rerank_recall.rs::rerank_recall_large_scale_threshold_gate` と
/// 同型（各下限を独立に解決し、設定済みの下限のみ判定する。3 つとも未設定かつ非
/// strict の場合のみコーパス生成前に早期 return して成功終了する。strict モードでは
/// [`resolve_gate_threshold_with`] が未設定を検出した時点で fail-closed になる）。
/// ログには pass/fail のみを出力し、注入された閾値・実測値のいずれの数値も出力しない。
///
/// **3 番目の下限を独立変数にする理由**（codex-review・PR #265・P1 指摘への対応）:
/// `MockLlmClient`（完全 oracle 写像）専用に較正された既存 2 下限を
/// `NoisyLlmClient`（非 oracle・劣化展開）の実測へそのまま適用する変更を一度試み
/// たところ、両クライアントで pass/fail が分かれる閾値域が実測により確認された
/// （`docs/design/query-planning-recall-regression.md` 参照）。oracle 写像基準の
/// 下限を非 oracle スタブへ流用すると、production の展開品質が劣化していなくても
/// 誤って fail しうるため、劣化シナリオ専用の下限を別の Actions variable として
/// 独立に解決し、それが未設定の間は（strict モードでない限り）この副検査を
/// 「対象外」として no-op にする（他の 2 下限のいずれかが設定済みなら、それらの
/// 判定は独立に継続する）。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make query-planning-regression で実行する"]
fn query_planning_recall_threshold_gate() {
    let min_intent_improvement =
        resolve_improvement_gate_threshold("QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT");
    let min_r20_direct = resolve_gate_threshold("QUERY_PLANNING_RECALL_MIN_R20_DIRECT");
    let min_intent_improvement_degraded = resolve_degraded_improvement_gate_threshold(
        "QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED",
    );

    if min_intent_improvement.is_none()
        && min_r20_direct.is_none()
        && min_intent_improvement_degraded.is_none()
    {
        println!(
            "query_planning_recall_threshold_gate: QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT/QUERY_PLANNING_RECALL_MIN_R20_DIRECT/QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED not configured; gate not enabled (explicit no-op, not a failure)"
        );
        return;
    }

    let (docs, pairs) = generate_corpus(SEED, NUM_DOCS, NUM_PAIRS, VOCAB_SIZE);
    let direct = measure_category_recall(&docs, &pairs, VOCAB_SIZE, direct_baseline);
    let intent = measure_category_recall(&docs, &pairs, VOCAB_SIZE, intent_baseline);
    let intent_improvement = intent.after_recall20() - intent.baseline_recall20();
    let direct_after_recall20 = direct.after_recall20();

    let mut pass = true;
    match min_intent_improvement {
        Some(min) => {
            let pass_intent = intent_improvement >= min;
            pass &= pass_intent;
            println!("query_planning_recall_threshold_gate: pass_intent={pass_intent}");
        }
        None => {
            println!(
                "query_planning_recall_threshold_gate: QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT not configured; sub-check not enabled"
            );
        }
    }
    match min_r20_direct {
        Some(min) => {
            let pass_direct = direct_after_recall20 >= min;
            pass &= pass_direct;
            println!("query_planning_recall_threshold_gate: pass_direct={pass_direct}");
        }
        None => {
            println!(
                "query_planning_recall_threshold_gate: QUERY_PLANNING_RECALL_MIN_R20_DIRECT not configured; sub-check not enabled"
            );
        }
    }
    match min_intent_improvement_degraded {
        Some(min) => {
            // `NoisyLlmClient` は intent の言い換え語彙の半数のみ写像するため、
            // measure_category_recall はこの副検査専用にここでのみ実行する
            // （oracle 写像の上記 2 検査と生成コストを共有しない独立経路）。
            let intent_degraded = measure_category_recall_with_client(
                &docs,
                &pairs,
                VOCAB_SIZE,
                intent_baseline,
                &NoisyLlmClient,
            );
            let intent_improvement_degraded =
                intent_degraded.after_recall20() - intent_degraded.baseline_recall20();
            let pass_degraded = intent_improvement_degraded >= min;
            pass &= pass_degraded;
            println!("query_planning_recall_threshold_gate: pass_degraded={pass_degraded}");
        }
        None => {
            println!(
                "query_planning_recall_threshold_gate: QUERY_PLANNING_RECALL_MIN_INTENT_IMPROVEMENT_DEGRADED not configured; sub-check not enabled"
            );
        }
    }

    assert!(
        pass,
        "intent recall improvement, direct after-expansion recall@20, or degraded-expansion intent recall improvement is below the configured QUERY_PLANNING_RECALL_MIN_* threshold"
    );
}
