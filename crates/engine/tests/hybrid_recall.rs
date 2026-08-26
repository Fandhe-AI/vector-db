//! TASK-104（対象ビヘイビア: SEARCH-1, SEARCH-2。ポインタ: `docs/spec/05-tasks.md`
//! TASK-104・`docs/spec/04-behavior/search.md`）。ハイブリッド検索カーネル
//! （疎 BM25 = TASK-102 / RRF 融合 = TASK-103 / CJK ストップワード除去 = TASK-105）の
//! Recall 受け入れ基準を自動チェックする回帰テスト。
//!
//! `crates/engine/tests/cjk_tokenizer_impact.rs`（TASK-106）の決定的合成コーパス生成
//! （自前 xorshift64*・外部クレート不使用）＋ Recall 実測＋固定値アサーションによる
//! 回帰トラッキング方式を踏襲する。正解判定は文書の潜在トピック集合
//! （[`Doc::keywords`]）から独立に構築し、疎チャネル（テキスト）・密チャネル
//! （ベクトル）はいずれもその非完全な観測（lossy view）として生成する
//! （[`generate_corpus`] のドキュメント参照）。これにより疎のみ／密のみでしか
//! 見つからない正解例が構造的に生まれ、Recall が 1.0（理論上限への 100% 到達）に
//! 機械的に張り付かない。`engine::hybrid::hybrid_search`（密・疎の RRF 融合）を
//! 通しで測定する。測定は production API（[`SparseIndex::build`]・
//! [`ParallelSearchProvider`]・[`hybrid_search`]＋[`RrfConfig::default`]）のみを
//! 使用し、production コード（`crates/engine/src/`）は変更しない。
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
//!   （`HYBRID_RECALL_MIN_*` 環境変数。`.github/workflows/recall.yml` が
//!   environment `recall-gate` の Actions variables から注入）と実測値を比較する
//!   閾値ゲート。既定（非 strict）では未設定・空文字列を「ゲート未設定＝明示的に
//!   対象外」として成功終了する（silent skip にはしない。
//!   `simd_bench.rs::core5_requested_from_env` と同じ opt-in 方式）。
//!   `HYBRID_RECALL_REQUIRE_THRESHOLDS=1`（`recall.yml` からの実行（dispatch /
//!   schedule）時のみ注入される strict モードフラグ）が立っている場合は、未設定も
//!   非数値・範囲外と同様に fail-closed でテスト失敗とする——environment 作成漏れ・
//!   variable 名の誤り・variable の誤削除により「一度も評価していない run」が
//!   基準を満たした run と同じ green になる事故を防ぐ（PR #147 codex-review P1
//!   継続指摘対応。[`GateThreshold`]・[`resolve_gate_threshold`] 参照）。ログには
//!   実測値と pass/fail のみを出力する（`crates/engine/benches/simd_bench.rs`
//!   と同方針）。実測値（Recall@k）は
//!   [`RecallResult::recall20`]/[`RecallResult::recall100`] が定義するとおり
//!   理論上限（`ceil20`/`ceil100`）を分母とする到達率であり、正解集合の総数
//!   （`total_correct`）を分母にしない（層 A と分母の意味を揃える）。
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

/// 合成コーパス 1 文書。`keywords` は生成時に既知の「正解（潜在）トピック集合」＝
/// トピックインデックス（[`topic_token`] 参照）で、QA セットの正解文書判定
/// （[`generate_qa_set`] の `inverted` 構築）にのみ使う——`text`（疎チャネル）・
/// `vector`（密チャネル）はいずれもこの潜在集合の非完全な観測（lossy view。
/// [`generate_corpus`] のドロップアウト／デコイのドキュメント参照）であり、
/// 正解判定そのものには使わない。正解集合の生成（`keywords`）と検索特徴量
/// （`text`・`vector`）を分離することで、疎のみ／密のみでしか見つからない
/// 正解例・どちらからも見つけにくい正解例が構造的に生まれ、Recall が
/// 天井（1.0）に張り付かない現実的な分布になる（codex-review 指摘対応）。
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
/// （次元数 = `vocab_size`）。密ベクトルは基底ベクトル（この関数）の和として
/// 合成するため、与えた次元集合との共通部分数がそのまま内積のスコアになる——
/// BM25 が AND 一致の文書を上位に置く挙動と同型の、密チャネル側の決定的な信号を
/// 作るための設計（ランダム方向ベクトルの平均ではトピック間のクロス項ノイズが
/// AND 一致信号を上回り、スコアの再現性・解釈性が失われるため採用しない。
/// `docs/design/hybrid-recall-regression.md` 参照）。与える次元集合は文書の潜在
/// トピック集合そのものとは限らない（[`generate_corpus`] のドロップアウト／デコイに
/// よる lossy view）ため、密ベクトルと疎テキストが常に一致するとは限らない。
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

/// fixture パラメータ（spec 由来の数値基準ではなく、本テストハーネスが Recall を
/// 1.0 未満の現実的な分布にするために実験的に選んだ確率）。文書の潜在トピック集合
/// （[`Doc::keywords`]）のうち、テキスト（疎チャネル）へ反映されず脱落する確率。
/// 「latent トピックはあるが埋め込み・索引が捉え損ねる」を模する（[`generate_corpus`]
/// のドキュメント参照）。
const TEXT_KEYWORD_DROPOUT_PROB: f64 = 0.18;

/// fixture パラメータ（同上）。潜在トピック集合のうち密ベクトルへ反映されず脱落する
/// 確率。
const VECTOR_KEYWORD_DROPOUT_PROB: f64 = 0.18;

/// fixture パラメータ（同上）。密ベクトルに、文書の潜在トピック集合に含まれない
/// 無関係な次元が 1 つ紛れ込む（decoy）確率。埋め込みが無関係トピックへ誤って
/// 反応する状況を模し、密チャネルの偽陽性（クラウディング）を作る。
const VECTOR_DECOY_PROB: f64 = 0.12;

/// 決定的シード付き擬似乱数でトピック相関コーパスと QA セット（`direct` カテゴリ相当:
/// 最も出現頻度の低いキーワード 2 語の AND 組み合わせ）を生成する。`num_docs`・
/// `num_queries`・`vocab_size` は呼び出し側（テスト内定数）が固定し、環境変数からは
/// 受け取らない。`vocab_size` はコーパス規模に応じて選ぶ（QA 正解集合の絞り込み度を
/// 保つため。[`topic_token`] のドキュメント参照）。
///
/// 正解判定（`inverted` → QA の `correct`）は文書の潜在トピック集合
/// （[`Doc::keywords`]）から構築し、疎チャネル（`text`）・密チャネル（`vector`）は
/// いずれもこの潜在集合の非完全な観測として独立に生成する
/// （[`TEXT_KEYWORD_DROPOUT_PROB`]・[`VECTOR_KEYWORD_DROPOUT_PROB`]・
/// [`VECTOR_DECOY_PROB`]）。ドロップアウト・デコイはいずれも 0/1 の one-hot 次元への
/// 操作のみで、浮動小数点の連続ノイズは加えない——`ParallelSearchProvider`
/// の加算順序（並列分割）に依存する丸め誤差を避け、層 A の固定値アサーションが
/// スレッド数に左右されず再現可能であることを保証するため。
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

        // 正解判定用の inverted index は潜在トピック集合 `kw_set` から構築する
        // （疎・密いずれの lossy view にも依存しない。ground truth の独立性）。
        for &kw_idx in &kw_set {
            inverted.entry(kw_idx).or_default().push(doc_id);
        }

        // ---- 疎チャネル（テキスト）の lossy view: 潜在トピックを確率的に脱落させる ----
        let mut text_keywords: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| rng.next_f64() >= TEXT_KEYWORD_DROPOUT_PROB)
            .collect();
        if text_keywords.is_empty() {
            // 全脱落は疎チャネルを完全に無効化してしまうため、最低 1 語は残す
            // （`num_keywords >= 3` により `kw_set` は必ず非空）。
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

        // ---- 密チャネル（ベクトル）の lossy view: 脱落＋無関係トピックの混入 ----
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
/// （`cjk_tokenizer_impact.rs::generate_qa_set` と同方式）。密クエリベクトルは選んだ
/// 2 語を [`one_hot_sum`] で符号化したもの（疎クエリと対応する AND 信号。クエリ自体は
/// 潜在トピック集合から直接構成する「意図が明確なクエリ」であり、[`generate_corpus`]
/// の lossy view はドキュメント側にのみ適用する）。
///
/// 正規化済み語ペア（`(min(a,b), max(a,b))`）を [`BTreeSet`] で追跡し、同一ペアの
/// 重複登録を除外する（codex-review 指摘対応。重複を許すと Recall が特定の語ペアへ
/// 偏り、少数の「簡単な」クエリの繰り返しで指標が水増しされるため）。重複除外の結果
/// `qa.len()` は `num_queries` を下回りうる（コーパス中のユニークな語ペア数が上限のため）。
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

// ---------- Recall 測定ヘルパ（production API 経由。engine::hybrid::hybrid_search） ----------

/// Recall@20・Recall@100 の実測結果（ヒット数と理論上限）。
///
/// `total_correct`（正解集合の総数）を分母にすると、正解集合が k 件を超えるクエリが
/// 混ざった場合に理論上の最大値が 1.0 未満に頭打ちになる（天井効果）。本ハーネスの
/// [`recall20`](RecallResult::recall20)/[`recall100`](RecallResult::recall100) は
/// 分母に理論上限 `ceil20`/`ceil100`（Σmin(k,\|correct_q\|)）を使い、達成可能な上限に
/// 対する到達率として層 A（回帰トラッキング）・層 B（spec 閾値ゲート）の両方で同じ
/// 意味の値を扱えるようにする。`total_correct` 自体は層 A の固定値アサーション対象
/// として残す。
struct RecallResult {
    total_correct: usize,
    hits20: usize,
    hits100: usize,
    ceil20: usize,
    ceil100: usize,
}

impl RecallResult {
    /// Recall@20 = hits20 / ceil20（理論上限に対する到達率）。`ceil20 == 0` は
    /// QA セットが空の場合のみ起こり得る呼び出し側の不変条件違反であり、
    /// その場合は NaN となって呼び出し側の `>=` 比較が false（fail-closed）になる。
    fn recall20(&self) -> f64 {
        self.hits20 as f64 / self.ceil20 as f64
    }

    /// Recall@100 = hits100 / ceil100（[`recall20`](Self::recall20) と同じ理由で
    /// 理論上限を分母にする）。
    fn recall100(&self) -> f64 {
        self.hits100 as f64 / self.ceil100 as f64
    }
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
    // QA 件数（[`generate_qa_set`] の語ペア重複除外後）も決定的コーパスに対しては
    // 固定値であり、フィクスチャが実質的に縮退していないことの回帰トラッキングを兼ねる。
    assert_eq!(qa.len(), 60, "重複除外後の QA 件数が変化した");

    let r = measure_recall(&docs, &qa);
    println!(
        "=== TASK-104 小規模段 Recall（docs={} queries={} total_correct={}） ===",
        docs.len(),
        qa.len(),
        r.total_correct
    );
    println!(
        "Recall@20={:.4} ({}/{} of ceil20; total_correct={})",
        r.recall20(),
        r.hits20,
        r.ceil20,
        r.total_correct
    );

    // 疎（テキスト）・密（ベクトル）の各チャネルは正解トピック集合の非完全な観測
    // （[`generate_corpus`] のドロップアウト／デコイ）であるため、Recall@20 は 1.0
    // （理論上限 `ceil20` への 100% 到達）に張り付かない。`hits20`/`ceil20`/
    // `total_correct` を固定値で回帰トラッキングする（検索カーネルやフィクスチャの
    // 変更で数値が変化した場合はこのテストが失敗する）。
    assert_eq!(r.total_correct, 202, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 202, "Recall@20 の理論上限が変化した");
    assert_eq!(r.hits20, 171, "小規模段の Recall@20 hit 数が変化した");
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
    assert_eq!(qa.len(), 100, "重複除外後の QA 件数が変化した");

    let r = measure_recall(&docs, &qa);
    println!(
        "=== TASK-104 大規模段 Recall（docs={} queries={} total_correct={}） ===",
        docs.len(),
        qa.len(),
        r.total_correct
    );
    println!(
        "Recall@20={:.4} ({}/{} of ceil20)  Recall@100={:.4} ({}/{} of ceil100; total_correct={})",
        r.recall20(),
        r.hits20,
        r.ceil20,
        r.recall100(),
        r.hits100,
        r.ceil100,
        r.total_correct
    );

    // `hybrid_recall_small_scale_regression` と同じ理由（[`generate_corpus`] の
    // lossy view）で Recall@20/Recall@100 は 1.0 に張り付かない。`hits`/`ceil`/
    // `total_correct` を固定値で回帰トラッキングする。
    assert_eq!(r.total_correct, 997, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 421, "Recall@20 の理論上限が変化した");
    assert_eq!(r.ceil100, 707, "Recall@100 の理論上限が変化した");
    assert_eq!(r.hits20, 328, "大規模段の Recall@20 hit 数が変化した");
    assert_eq!(r.hits100, 645, "大規模段の Recall@100 hit 数が変化した");
}

// ---------- 層 B: spec 閾値ゲート（`#[ignore]`。`make recall-regression` 専用） ----------

/// `HYBRID_RECALL_MIN_*` 環境変数（`(0.0, 1.0]` の浮動小数点）の解決結果。
enum GateThreshold {
    /// 環境変数が未設定、または GitHub Actions の未設定 repo/environment variable が
    /// 解決する空文字列（`.github/workflows/bench.yml` の `BENCH_MAX_P95_MS` 等と
    /// 同じ経路。`crates/engine/benches/simd_bench.rs::min_recall_from_env`
    /// 冒頭コメント参照）。
    NotConfigured,
    /// 設定済みで `(0.0, 1.0]` の範囲内。この場合のみ実測値と比較する。
    Value(f64),
}

/// `HYBRID_RECALL_MIN_*` 環境変数を読み取る。未設定・空文字列は
/// [`GateThreshold::NotConfigured`]、非数値・範囲外は fail-closed（`Err`。呼び出し側で
/// テスト失敗として扱う）、それ以外は [`GateThreshold::Value`] を返す。数値そのもの
/// （spec の Recall 下限）はこのファイル・ログのいずれにもハードコードしない
/// （`.claude/rules/spec-confidentiality.md`）。
fn recall_threshold_from_env(var: &str) -> Result<GateThreshold, String> {
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
    if !(value > 0.0 && value <= 1.0) {
        return Err(format!("{var} must be within (0.0, 1.0]"));
    }
    Ok(GateThreshold::Value(value))
}

/// `HYBRID_RECALL_REQUIRE_THRESHOLDS` 環境変数（`"1"` のときのみ true）。
/// `.github/workflows/recall.yml` からの実行（dispatch / schedule）時のみ
/// 注入される strict モードフラグ（PR #147 codex-review P1 継続指摘対応）。
///
/// strict モードが無効（ローカル `make recall-regression`・仮に PR 経由で
/// `--ignored` を明示指定して実行した場合）は、閾値未設定を「対象外」として
/// 明示的に成功終了する opt-in 挙動を維持する
/// （`simd_bench.rs::core5_requested_from_env` と同型）。
///
/// strict モードが有効な場合は未設定を「一度も評価していない run」とみなし、
/// 非数値・範囲外と同様に fail-closed でテスト失敗とする。これにより
/// environment 作成漏れ・variable 名の打ち間違い・variable の誤削除で
/// `HYBRID_RECALL_MIN_*` が読めなくなった週次 run が、実際に基準を満たした
/// run と同じ green として埋もれる事故を防ぐ（PR #147 codex-review P1 指摘対応）。
fn strict_thresholds_required() -> bool {
    std::env::var("HYBRID_RECALL_REQUIRE_THRESHOLDS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// [`recall_threshold_from_env`] を読み取り、[`GateThreshold::NotConfigured`] を
/// strict モード（[`strict_thresholds_required`]）に応じて分岐させる共通ヘルパ。
/// strict モード有効時の未設定は fail-closed（`panic!`）、無効時は `None`
/// （呼び出し側で「対象外」を出力して early return する）。非数値・範囲外は
/// strict モードの有無によらず常に fail-closed とする。
fn resolve_gate_threshold(var: &str) -> Option<f64> {
    match recall_threshold_from_env(var) {
        Ok(GateThreshold::Value(v)) => Some(v),
        Ok(GateThreshold::NotConfigured) => {
            if strict_thresholds_required() {
                panic!(
                    "{var} is not configured but HYBRID_RECALL_REQUIRE_THRESHOLDS=1 (strict mode: this run must evaluate all HYBRID_RECALL_MIN_* thresholds; see .github/workflows/recall.yml and the recall-gate environment variables)"
                );
            }
            None
        }
        Err(msg) => panic!("{var} invalid: {msg}"),
    }
}

/// TASK-104（SEARCH-1）層 B: 小規模段 Recall@20（[`RecallResult::recall20`]。分母は
/// 理論上限 `ceil20`）が `HYBRID_RECALL_MIN_R20_SMALL`（Actions variables 由来）以上
/// であることを確認する閾値ゲート。未設定は既定（非 strict）では「対象外」として
/// 明示的に成功終了し、strict モード（[`strict_thresholds_required`]）では
/// fail-closed でテスト失敗とする。設定済みで非数値・範囲外は常に fail-closed
/// でテスト失敗とする（skip しない）。ログには実測値と pass/fail のみを出力し、
/// 注入された閾値の数値は出力しない。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make recall-regression で実行する"]
fn hybrid_recall_small_scale_threshold_gate() {
    let min_r20 = match resolve_gate_threshold("HYBRID_RECALL_MIN_R20_SMALL") {
        Some(v) => v,
        None => {
            println!(
                "hybrid_recall_small_scale_threshold_gate: HYBRID_RECALL_MIN_R20_SMALL not configured; gate not enabled (explicit no-op, not a failure)"
            );
            return;
        }
    };

    let (docs, qa) = generate_corpus(
        SMALL_SEED,
        SMALL_NUM_DOCS,
        SMALL_NUM_QUERIES,
        SMALL_VOCAB_SIZE,
    );
    let r = measure_recall(&docs, &qa);
    let recall20 = r.recall20();
    let pass = recall20 >= min_r20;

    println!("hybrid_recall_small_scale_threshold_gate: recall@20={recall20:.4} pass={pass}");
    assert!(
        pass,
        "small-scale Recall@20 below HYBRID_RECALL_MIN_R20_SMALL"
    );
}

/// TASK-104（SEARCH-2）層 B: 大規模段 Recall@20・Recall@100 がそれぞれ
/// `HYBRID_RECALL_MIN_R20_LARGE`・`HYBRID_RECALL_MIN_R100_LARGE` 以上であることを
/// 確認する閾値ゲート。契約は [`hybrid_recall_small_scale_threshold_gate`] と同一だが、
/// 2 つの下限を独立に解決する（片方のみ設定済みの場合は設定済みの側だけを判定し、
/// 未設定側は既定では「対象外」を出力する。両方未設定かつ非 strict の場合のみ
/// コーパス生成前に早期 return して成功終了する。strict モードでは
/// [`resolve_gate_threshold`] が未設定を検出した時点で fail-closed になる）。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make recall-regression で実行する"]
fn hybrid_recall_large_scale_threshold_gate() {
    let min_r20 = resolve_gate_threshold("HYBRID_RECALL_MIN_R20_LARGE");
    let min_r100 = resolve_gate_threshold("HYBRID_RECALL_MIN_R100_LARGE");

    if min_r20.is_none() && min_r100.is_none() {
        println!(
            "hybrid_recall_large_scale_threshold_gate: HYBRID_RECALL_MIN_R20_LARGE/HYBRID_RECALL_MIN_R100_LARGE not configured; gate not enabled (explicit no-op, not a failure)"
        );
        return;
    }

    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    let r = measure_recall(&docs, &qa);
    let recall20 = r.recall20();
    let recall100 = r.recall100();

    let mut pass = true;
    match min_r20 {
        Some(min) => {
            let pass20 = recall20 >= min;
            pass &= pass20;
            println!(
                "hybrid_recall_large_scale_threshold_gate: recall@20={recall20:.4} pass20={pass20}"
            );
        }
        None => {
            println!(
                "hybrid_recall_large_scale_threshold_gate: HYBRID_RECALL_MIN_R20_LARGE not configured; sub-check not enabled"
            );
        }
    }
    match min_r100 {
        Some(min) => {
            let pass100 = recall100 >= min;
            pass &= pass100;
            println!(
                "hybrid_recall_large_scale_threshold_gate: recall@100={recall100:.4} pass100={pass100}"
            );
        }
        None => {
            println!(
                "hybrid_recall_large_scale_threshold_gate: HYBRID_RECALL_MIN_R100_LARGE not configured; sub-check not enabled"
            );
        }
    }

    assert!(
        pass,
        "large-scale Recall@20/Recall@100 below configured HYBRID_RECALL_MIN_* threshold"
    );
}
