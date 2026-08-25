//! TASK-163（対象ビヘイビア: SEARCH-10。ポインタ: `docs/spec/05-tasks.md` TASK-163・
//! `docs/spec/04-behavior/search.md` SEARCH-10・`docs/spec/06-roadmap.md` MS-3）。
//!
//! `precision` モード（TASK-162。`crates/engine/src/precision.rs`）の評価基準を
//! 実測する評価ハーネス。`tests/hybrid_recall.rs`（TASK-104）・
//! `tests/rerank_recall.rs`（TASK-108）と同じ「決定的合成コーパス＋2 層構成
//! （層 A 固定値回帰／層 B spec 閾値ゲート）」方式を踏襲するが、対象が Recall では
//! なく **Top-1 Accuracy・MRR@10・正解不在クエリでの誤返却率** である点が異なる。
//!
//! # 測定経路
//!
//! production の SQL 経路（`EngineCore::execute_sql`。`crates/engine/tests/
//! sql_precision_mode.rs` と同じ流儀の実 `Storage`＋`CpuScalarProvider`）を使う。
//! `precision::apply_gate` を直接呼ぶとゲートを起動する側の配線（`sql/exec.rs` の
//! `k_eff` 拡張・正規化 RRF 適用）をテスト側で再実装することになり production と
//! 乖離しうるため、production コード（`crates/engine/src/`）は変更せず SQL 経由のみで
//! 測定する。ランキングは **hybrid**（`hybrid_rrf(embedding, ..., body, ...)`）を主、
//! **dense**（`embedding <=> ...`）を副として同一コーパスで併測する
//! （`PrecisionPolicy` が dense/hybrid 別閾値を持つため）。
//!
//! # 指標定義
//!
//! QA セットを「正解あり Q+」「正解不在 Q0」に分ける。
//!
//! - **Top-1 Accuracy**（Q+ が分母）: `precision` 出力が非空かつ先頭行 id が正解集合に
//!   含まれるクエリの割合。**空集合は不正解扱い**（fail-closed 側の保守的な定義）。
//! - **MRR@10**（Q+ が分母）: `recall` モード `LIMIT 10`（候補生成段は precision と
//!   共通。SEARCH-9）の順位列で最初の正解の逆順位（10 位以内に無ければ 0）の平均。
//!   `precision` 出力そのものではなく共通の候補生成段で測る理由: 既定
//!   `max_results`（1）では `precision` 出力の MRR は Top-1 Accuracy と数学的に同値へ
//!   退化し、別指標として測る意味がなくなるため（ユーザー確認事項。ADR
//!   `docs/design/precision-eval-regression.md` 参照）。
//! - **誤返却率**（Q0 が分母）: 正解が存在しないクエリで `precision` 出力が非空になる
//!   割合。
//!
//! 診断値（層 A のアサート対象外・`println!` のみ）: coverage（Q+ で非空を返した
//! 割合）・条件付き Top-1 精度（非空のうち先頭が正解の割合）。
//!
//! # 2 層構成
//!
//! - 層 A（`#[test]`・`make ci` 対象）: 決定的コーパスでの QA 件数・Top-1 命中数・
//!   MRR 順位別命中分布（`rank -> 件数` を丸ごと固定値アサーション。総ヒット件数
//!   のみだと順位の入れ替わりを検知できないため）・誤返却件数を固定値アサーションで
//!   回帰トラッキングする。spec の数値基準は使わない
//!   （`.claude/rules/spec-confidentiality.md`）。
//! - 層 B（`#[ignore]`・`make precision-regression`）: `PRECISION_EVAL_MIN_TOP1_ACC`・
//!   `PRECISION_EVAL_MIN_MRR10`・`PRECISION_EVAL_MAX_FALSE_RETURN` 環境変数
//!   （`hybrid_recall.rs::resolve_gate_threshold` と同型の解決規則）による閾値ゲート。
//!   TASK-163 のスコープは実測・判断材料の提示までであり目標値の確定は含まないため、
//!   `.github/workflows/recall.yml` への接続は本タスクでは行わない（目標値確定後の
//!   フォローアップとする。申し送り・README 参照）。
//! - 感度スイープ（`#[ignore]`・アサートなし）: `PrecisionPolicy::new` の閾値を
//!   小さな格子で差し替え、3 指標の変化を `println!` で表示する（目標値確定の判断
//!   材料。production の既定値は変更しない）。
//!
//! # TASK-158（性能計測プロトコル基盤）準拠
//!
//! 合成コーパスの疑似乱数は `harness::rng::DeterministicRng`（`#[path]` で
//! `benches/harness/mod.rs` を取り込む。`tests/bench_harness.rs`・`tests/
//! bench_accept.rs` と同一方式）を使う。決定的コーパス上の指標は入力の決定的関数
//! であり、同一シードから常に同一値が再現される（層 A の固定値アサーションがその
//! 保証を兼ねる）ため、warmup／計測回数・中央値等の時間計測プロトコルはレイテンシ
//! 基準を持たない本タスクでは適用対象外とする。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::precision::PrecisionPolicy;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use harness::rng::DeterministicRng;
use std::collections::{BTreeMap, BTreeSet};

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した（`sql_precision_mode.rs`
// と同一の取り込み方式）。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- RNG ヘルパ（`DeterministicRng::next_u64` から導出。TASK-158 準拠） ----------

/// `[0, n)` の一様乱数を返す（`n == 0` は呼び出し側の不変条件違反であり、
/// untrusted 入力経路ではないため coding-rust.md の unwrap 禁止は適用されない）。
fn next_range(rng: &mut DeterministicRng, n: usize) -> usize {
    assert!(n > 0, "next_range(0) は無効な呼び出し");
    (rng.next_u64() % n as u64) as usize
}

/// `[0.0, 1.0)` の f64 一様乱数を返す（`hybrid_recall.rs::Xorshift64::next_f64` と
/// 同じ導出。上位 53bit を使い f64 仮数部精度内で一様性を保つ）。
fn next_f64(rng: &mut DeterministicRng) -> f64 {
    (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

// ---------- 合成コーパスの語彙 ----------

/// トピック `idx` に対応するキーワードトークン（`hybrid_recall.rs::topic_token` と
/// 同一規則。`sparse::tokenize` の ASCII 単語境界規則の下で必ず 1 トークンになる）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

const FILLER_WORDS: [&str; 10] = [
    "the", "a", "an", "of", "for", "and", "with", "in", "on", "note",
];

/// 合成コーパス 1 文書。`keywords` は正解判定専用の潜在トピック集合
/// （`hybrid_recall.rs::Doc` と同じ役割分離）。
struct Doc {
    id: u64,
    text: String,
    keywords: BTreeSet<usize>,
    vector: Vec<f32>,
}

/// 正解ありクエリ（Q+）1 件。
struct QaCase {
    query_text: String,
    query_vector: Vec<f32>,
    correct: BTreeSet<u64>,
}

/// 正解不在クエリ（Q0）1 件。
struct NoAnswerCase {
    query_text: String,
    query_vector: Vec<f32>,
}

fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

fn build_zipf_cumulative_weights(n: usize) -> Vec<f64> {
    let mut acc = 0.0;
    let mut cumulative = Vec::with_capacity(n);
    for i in 0..n {
        acc += 1.0 / (i as f64 + 1.0);
        cumulative.push(acc);
    }
    cumulative
}

fn zipf_index(rng: &mut DeterministicRng, cumulative_weights: &[f64]) -> usize {
    let total = *cumulative_weights.last().unwrap_or(&0.0);
    let r = next_f64(rng) * total;
    match cumulative_weights.iter().position(|&acc| r <= acc) {
        Some(i) => i,
        None => cumulative_weights.len().saturating_sub(1),
    }
}

/// `sparse.rs` の各上限に対するコーパス規模ガード（`hybrid_recall.rs::
/// MAX_CORPUS_DOCS_GUARD` と同じ役割）。環境変数からサイズを受け取らず、テスト内
/// 定数のみで規模を決める（coding-rust.md「untrusted 入力の扱い」）。
const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

/// fixture パラメータ（`hybrid_recall.rs` と同値。疎チャネルへの潜在トピック脱落率）。
const TEXT_KEYWORD_DROPOUT_PROB: f64 = 0.18;
/// fixture パラメータ（同上。密チャネルへの潜在トピック脱落率）。
const VECTOR_KEYWORD_DROPOUT_PROB: f64 = 0.18;
/// fixture パラメータ（同上。密チャネルへの無関係トピック混入率）。
const VECTOR_DECOY_PROB: f64 = 0.12;

/// 決定的シード付き擬似乱数でトピック相関コーパスを生成する（`hybrid_recall.rs::
/// generate_corpus` と同方式。正解判定用の `inverted` も併せて返す）。
fn generate_corpus(
    seed: u64,
    num_docs: usize,
    vocab_size: usize,
) -> (Vec<Doc>, BTreeMap<usize, Vec<u64>>) {
    assert!(
        num_docs <= MAX_CORPUS_DOCS_GUARD,
        "MAX_CORPUS_DOCS を超過してはならない"
    );

    let mut rng = DeterministicRng::new(seed);
    let zipf_weights = build_zipf_cumulative_weights(vocab_size);

    let mut docs = Vec::with_capacity(num_docs);
    let mut inverted: BTreeMap<usize, Vec<u64>> = BTreeMap::new();

    for doc_id in 0..num_docs as u64 {
        let num_keywords = 3 + next_range(&mut rng, 4); // 3..=6
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
            .filter(|_| next_f64(&mut rng) >= TEXT_KEYWORD_DROPOUT_PROB)
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
                text.push_str(FILLER_WORDS[next_range(&mut rng, FILLER_WORDS.len())]);
                text.push(' ');
            }
            text.push_str(&topic_token(kw_idx));
        }

        let mut vector_keywords: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| next_f64(&mut rng) >= VECTOR_KEYWORD_DROPOUT_PROB)
            .collect();
        if next_f64(&mut rng) < VECTOR_DECOY_PROB {
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

    (docs, inverted)
}

/// 各文書から最も出現頻度の低いキーワード 2 語（AND 組み合わせ）を選び、正解集合が
/// コーパス全体に対して十分に絞り込まれた Q+ クエリを構成する
/// （`hybrid_recall.rs::generate_qa_set` と同方式）。
fn generate_qa_set(
    rng: &mut DeterministicRng,
    docs: &[Doc],
    inverted: &BTreeMap<usize, Vec<u64>>,
    vocab_size: usize,
    num_queries: usize,
) -> Vec<QaCase> {
    let mut order: Vec<usize> = (0..docs.len()).collect();
    for i in (1..order.len()).rev() {
        let j = next_range(rng, i + 1);
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

/// 正解不在クエリ（Q0。TASK-163 の中心的な追加）: 語彙中でそれぞれ単独では出現する
/// が、同一文書内で共起しない 2 語 `(a, b)` を選ぶ。各語は単独では部分一致文書を
/// 持つため BM25／密チャネルとも「もっともらしい候補」を提示し得る——`precision`
/// ゲートが空集合へ倒せるかを問う本命ケース（自明な語彙外クエリと異なり、確信度
/// 判定そのものの実力を測る）。
///
/// 正規化済みペア（`(min, max)`）で重複を除外し、共起しないペアの探索には試行回数の
/// 上限を設ける（無限ループ防止。fail-closed の `assert!` で上限到達を検出する）。
fn generate_no_answer_set(
    rng: &mut DeterministicRng,
    inverted: &BTreeMap<usize, Vec<u64>>,
    vocab_size: usize,
    num_queries: usize,
) -> Vec<NoAnswerCase> {
    let vocab: Vec<usize> = inverted.keys().copied().collect();
    let mut cases = Vec::with_capacity(num_queries);
    let mut seen_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();

    if vocab.len() < 2 {
        return cases;
    }

    const MAX_ATTEMPTS_PER_QUERY: usize = 2_000;
    for _ in 0..num_queries {
        let mut found = false;
        for _ in 0..MAX_ATTEMPTS_PER_QUERY {
            let a = vocab[next_range(rng, vocab.len())];
            let b = vocab[next_range(rng, vocab.len())];
            if a == b {
                continue;
            }
            let pair = (a.min(b), a.max(b));
            if seen_pairs.contains(&pair) {
                continue;
            }
            let set_a: BTreeSet<u64> = inverted.get(&a).into_iter().flatten().copied().collect();
            let set_b: BTreeSet<u64> = inverted.get(&b).into_iter().flatten().copied().collect();
            if set_a.intersection(&set_b).next().is_none() {
                seen_pairs.insert(pair);
                cases.push(NoAnswerCase {
                    query_text: format!("{} {}", topic_token(a), topic_token(b)),
                    query_vector: one_hot_sum(vocab_size, [a, b]),
                });
                found = true;
                break;
            }
        }
        assert!(
            found,
            "generate_no_answer_set: MAX_ATTEMPTS_PER_QUERY 内で共起しない語ペアを見つけられなかった"
        );
    }

    cases
}

/// 語彙外クエリ（自明な Q0）: 未知トークン＋ゼロベクトル。cosine 類似度が
/// 定義できず（`None`）密チャネルは常に空集合になり、疎チャネルも一致文書が
/// 存在しない。比率は小さく保つ（[`generate_no_answer_set`] が本命ケース）。
fn generate_out_of_vocab_set(vocab_size: usize, num_queries: usize) -> Vec<NoAnswerCase> {
    (0..num_queries)
        .map(|i| NoAnswerCase {
            query_text: format!("oov_token_{i:04}"),
            query_vector: vec![0.0f32; vocab_size],
        })
        .collect()
}

/// コーパスが `sparse.rs` の各上限に収まることを検証する（`hybrid_recall.rs::
/// assert_corpus_within_limits` と同一の健全性チェック）。
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

// ---------- SQL 経由の測定（production の EngineCore::execute_sql のみを使う） ----------

/// 密ベクトルは one-hot 和のため常に `0.0`／`1.0`／`2.0`（decoy 混入時）の値域に
/// 収まり、`{:.1}` で決定的に整形できる（合成トークンのみから組み立て、外部入力を
/// 連結しない。coding-rust.md「SQL / プラン文字列の組み立てに未検証入力を連結
/// しない」）。
fn vector_literal(v: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x:.1}"));
    }
    s.push(']');
    s
}

/// 単一テナント・`Visibility::Public` でコーパス全件を投入した `EngineCore` を返す
/// （RLS 検査を迂回する API は使わない。security.md「テナント境界」）。
/// 呼び出し側は返る `CleanupGuard` を `EngineCore` と同じスコープ（テスト関数末尾
/// まで）で保持すること。`tests/sql_precision_mode.rs` と同じ流儀で `CleanupGuard`
/// を `Storage::open` より先に宣言し（`Drop` は宣言の逆順のため、先に宣言した
/// ガードは `Storage` の後に drop され、redb のファイルハンドルが閉じてから
/// 一時ファイルを削除できる。`temp_db.rs` の Windows 向け注意参照）、一時 DB
/// ファイルの残留を防ぐ。
fn setup_core(docs: &[Doc], vocab_size: usize) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("precision-eval-corpus");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(vocab_size as u32), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let ctx = PolicyContext::new("precision-eval-tenant").expect("valid tenant");
    for doc in docs {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            doc.id,
            Visibility::Public,
            &[
                Value::Vector(doc.vector.clone()),
                Value::Text(doc.text.clone()),
            ],
        )
        .expect("insert row");
    }

    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

/// クエリ結果の行 ID 列を返す（`sql_precision_mode.rs::result_ids` と同型の
/// ヘルパ。`EngineCore::execute_sql` はセッション変数を持たない後方互換 API で
/// `QueryResult` を直接返す）。
fn result_ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core.execute_sql(ctx, sql).expect("query must succeed");
    result.rows.iter().map(|r| r.id).collect()
}

/// ランキング方式（hybrid が主・dense が副。SEARCH-10）。
#[derive(Clone, Copy)]
enum Ranking {
    Hybrid,
    Dense,
}

impl Ranking {
    fn precision_sql(self, query_text: &str, query_vector: &[f32]) -> String {
        let vec_lit = vector_literal(query_vector);
        match self {
            Ranking::Hybrid => format!(
                "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{vec_lit}', body, '{query_text}') LIMIT 10 USING MODE 'precision'"
            ),
            Ranking::Dense => format!(
                "SELECT * FROM docs ORDER BY embedding <=> '{vec_lit}' LIMIT 10 USING MODE 'precision'"
            ),
        }
    }

    fn recall_top10_sql(self, query_text: &str, query_vector: &[f32]) -> String {
        let vec_lit = vector_literal(query_vector);
        match self {
            Ranking::Hybrid => format!(
                "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{vec_lit}', body, '{query_text}') LIMIT 10 USING MODE 'recall'"
            ),
            Ranking::Dense => format!(
                "SELECT * FROM docs ORDER BY embedding <=> '{vec_lit}' LIMIT 10 USING MODE 'recall'"
            ),
        }
    }
}

/// SEARCH-10 の 3 指標＋診断値の実測結果（1 ランキング方式分）。
struct EvalResult {
    n_qplus: usize,
    n_qzero: usize,
    top1_hits: usize,
    coverage_hits: usize,
    coverage_row_count: usize,
    mrr_hits_by_rank: BTreeMap<usize, usize>,
    false_returns: usize,
}

impl EvalResult {
    /// Top-1 Accuracy = top1_hits / n_qplus（空集合は不正解扱い。SEARCH-10）。
    fn top1_accuracy(&self) -> f64 {
        self.top1_hits as f64 / self.n_qplus as f64
    }

    /// MRR@10 = Σ(1/rank) / n_qplus（`mrr_hits_by_rank` は `rank -> 件数` の集計。
    /// 整数集計を保つことで層 A の固定値アサーションが浮動小数点丸めに依存しない）。
    fn mrr10(&self) -> f64 {
        let sum: f64 = self
            .mrr_hits_by_rank
            .iter()
            .map(|(&rank, &count)| count as f64 / rank as f64)
            .sum();
        sum / self.n_qplus as f64
    }

    /// 誤返却率 = false_returns / n_qzero（Q0 が分母）。
    fn false_return_rate(&self) -> f64 {
        self.false_returns as f64 / self.n_qzero as f64
    }

    /// 診断値: coverage（Q+ で非空を返した割合）。
    fn coverage(&self) -> f64 {
        self.coverage_hits as f64 / self.n_qplus as f64
    }

    /// 診断値: 条件付き Top-1 精度（非空のうち先頭が正解の割合）。
    fn conditional_top1_accuracy(&self) -> f64 {
        if self.coverage_hits == 0 {
            return 0.0;
        }
        self.top1_hits as f64 / self.coverage_hits as f64
    }

    /// 診断値: 非空クエリ 1 件あたりの平均返却行数。SEARCH-10 の 3 指標
    /// （Top-1 Accuracy・MRR@10・誤返却率）はいずれもゲート通過の有無・先頭行のみに
    /// 依存し `PrecisionPolicy::max_results` の値そのものには反応しない構造のため、
    /// `precision_eval_policy_sweep` で `max_results` を差し替えても 3 指標が
    /// 変化しない（`docs/design/precision-eval-regression.md` 記載の既知の観察）。
    /// 本値は `max_results` の効果を直接反映する唯一の測定値として、感度スイープの
    /// 判断材料に加える（層 A のアサート対象外）。
    fn avg_result_rows(&self) -> f64 {
        if self.coverage_hits == 0 {
            return 0.0;
        }
        self.coverage_row_count as f64 / self.coverage_hits as f64
    }
}

/// [`Ranking`] 1 方式分の 3 指標＋診断値を、production の SQL 経路（`EngineCore::
/// execute_sql`）のみを使って測定する（`precision::apply_gate` は直接呼ばない。
/// モジュールドキュメント「測定経路」参照）。
fn measure(
    core: &EngineCore,
    ctx: &PolicyContext,
    ranking: Ranking,
    qa: &[QaCase],
    no_answer: &[NoAnswerCase],
) -> EvalResult {
    let mut top1_hits = 0usize;
    let mut coverage_hits = 0usize;
    let mut coverage_row_count = 0usize;
    let mut mrr_hits_by_rank: BTreeMap<usize, usize> = BTreeMap::new();

    for case in qa {
        let precision_sql = ranking.precision_sql(&case.query_text, &case.query_vector);
        let precision_ids = result_ids(core, ctx, &precision_sql);
        if let Some(&top1) = precision_ids.first() {
            coverage_hits += 1;
            coverage_row_count += precision_ids.len();
            if case.correct.contains(&top1) {
                top1_hits += 1;
            }
        }

        let recall_sql = ranking.recall_top10_sql(&case.query_text, &case.query_vector);
        let recall_ids = result_ids(core, ctx, &recall_sql);
        if let Some(rank) = recall_ids
            .iter()
            .position(|id| case.correct.contains(id))
            .map(|idx| idx + 1)
        {
            *mrr_hits_by_rank.entry(rank).or_insert(0) += 1;
        }
    }

    let mut false_returns = 0usize;
    for case in no_answer {
        let precision_sql = ranking.precision_sql(&case.query_text, &case.query_vector);
        let precision_ids = result_ids(core, ctx, &precision_sql);
        if !precision_ids.is_empty() {
            false_returns += 1;
        }
    }

    EvalResult {
        n_qplus: qa.len(),
        n_qzero: no_answer.len(),
        top1_hits,
        coverage_hits,
        coverage_row_count,
        mrr_hits_by_rank,
        false_returns,
    }
}

fn print_eval_result(label: &str, r: &EvalResult) {
    println!(
        "=== TASK-163 precision 評価（{label}）: |Q+|={} |Q0|={} ===",
        r.n_qplus, r.n_qzero
    );
    println!(
        "Top-1 Accuracy={:.4} ({}/{})  MRR@10={:.4}  誤返却率={:.4} ({}/{})",
        r.top1_accuracy(),
        r.top1_hits,
        r.n_qplus,
        r.mrr10(),
        r.false_return_rate(),
        r.false_returns,
        r.n_qzero
    );
    println!(
        "診断値: coverage={:.4} ({}/{})  条件付き Top-1 精度={:.4} ({}/{})",
        r.coverage(),
        r.coverage_hits,
        r.n_qplus,
        r.conditional_top1_accuracy(),
        r.top1_hits,
        r.coverage_hits
    );
}

// ---------- 層 A: 固定値回帰トラッキング（`#[test]`。`make ci` 対象） ----------

const NUM_DOCS: usize = 850;
const VOCAB_SIZE: usize = 100;
const NUM_QPLUS_QUERIES: usize = 100;
const NUM_QZERO_HARD_NEGATIVE: usize = 50;
const NUM_QZERO_OOV: usize = 5;
const CORPUS_SEED: u64 = 0x5EED_0163_5052_4543;
const QA_SEED_OFFSET: u64 = 0x0001;
const NO_ANSWER_SEED_OFFSET: u64 = 0x0002;

/// 決定的コーパス・QA/Q0 セットを一括生成する（層 A・層 B・感度スイープが共有する
/// フィクスチャ構築ロジック）。返る `CleanupGuard` は呼び出し側が `EngineCore` と
/// 同じスコープで保持し、一時 DB ファイルの削除を保証する（[`setup_core`] 参照）。
fn build_fixture() -> (
    EngineCore,
    CleanupGuard,
    PolicyContext,
    Vec<QaCase>,
    Vec<NoAnswerCase>,
) {
    let (docs, inverted) = generate_corpus(CORPUS_SEED, NUM_DOCS, VOCAB_SIZE);
    assert_corpus_within_limits(&docs);

    let mut qa_rng = DeterministicRng::new(CORPUS_SEED.wrapping_add(QA_SEED_OFFSET));
    let qa = generate_qa_set(&mut qa_rng, &docs, &inverted, VOCAB_SIZE, NUM_QPLUS_QUERIES);
    for case in &qa {
        assert!(!case.correct.is_empty());
    }

    let mut no_answer_rng = DeterministicRng::new(CORPUS_SEED.wrapping_add(NO_ANSWER_SEED_OFFSET));
    let mut no_answer = generate_no_answer_set(
        &mut no_answer_rng,
        &inverted,
        VOCAB_SIZE,
        NUM_QZERO_HARD_NEGATIVE,
    );
    no_answer.extend(generate_out_of_vocab_set(VOCAB_SIZE, NUM_QZERO_OOV));

    let (core, guard) = setup_core(&docs, VOCAB_SIZE);
    let ctx = PolicyContext::new("precision-eval-tenant").expect("valid tenant");

    (core, guard, ctx, qa, no_answer)
}

/// TASK-163（SEARCH-10）層 A: hybrid ランキング（主）での 3 指標を実測し、固定値で
/// 回帰トラッキングする。決定的コーパス・QA/Q0 セットのため実測値は再現可能であり、
/// `precision`／`recall` 検索カーネルへの変更で数値が変化した場合はこのテストが
/// 失敗する。
#[test]
fn precision_eval_hybrid_regression() {
    let (core, _guard, ctx, qa, no_answer) = build_fixture();
    assert_eq!(qa.len(), 100, "重複除外後の Q+ 件数が変化した");
    assert_eq!(no_answer.len(), 55, "Q0 件数が変化した");

    let r = measure(&core, &ctx, Ranking::Hybrid, &qa, &no_answer);
    print_eval_result("hybrid", &r);

    // 疎（テキスト）・密（ベクトル）の各チャネルは正解トピック集合の非完全な観測
    // （[`generate_corpus`] のドロップアウト／デコイ）であるため、precision の
    // 各指標は 1.0 に張り付かない。実測値を固定値で回帰トラッキングする
    // （検索カーネル・ゲート・フィクスチャの変更で数値が変化した場合はこのテストが
    // 失敗する）。
    assert_eq!(r.top1_hits, 60, "hybrid Top-1 命中数が変化した");
    assert_eq!(r.coverage_hits, 65, "hybrid coverage 件数が変化した");
    // `rank -> 件数` の分布そのものを固定値アサートする（`.values().sum()` のみだと
    // 総ヒット件数は不変のまま順位が入れ替わる劣化（例: 1 位ヒットが 5 位へ後退）を
    // 検知できないため。BTreeMap の `Debug` 出力はキー昇順で決定的）。
    assert_eq!(
        format!("{:?}", r.mrr_hits_by_rank),
        "{1: 76, 2: 4, 3: 2, 4: 3, 5: 1, 6: 1, 7: 1, 8: 2, 9: 1, 10: 1}",
        "hybrid MRR@10 の順位別命中分布が変化した"
    );
    assert_eq!(r.false_returns, 7, "hybrid 誤返却件数が変化した");
}

/// TASK-163（SEARCH-10）層 A: dense ランキング（副）での 3 指標を実測し、固定値で
/// 回帰トラッキングする（[`precision_eval_hybrid_regression`] と同一フィクスチャ・
/// 同一契約）。
#[test]
fn precision_eval_dense_regression() {
    let (core, _guard, ctx, qa, no_answer) = build_fixture();

    let r = measure(&core, &ctx, Ranking::Dense, &qa, &no_answer);
    print_eval_result("dense", &r);

    assert_eq!(r.top1_hits, 10, "dense Top-1 命中数が変化した");
    assert_eq!(r.coverage_hits, 10, "dense coverage 件数が変化した");
    // 理由は [`precision_eval_hybrid_regression`] のコメント参照（順位分布そのものを固定値アサートする）。
    assert_eq!(
        format!("{:?}", r.mrr_hits_by_rank),
        "{1: 75, 2: 3, 3: 3, 5: 1, 8: 2, 10: 1}",
        "dense MRR@10 の順位別命中分布が変化した"
    );
    assert_eq!(r.false_returns, 0, "dense 誤返却件数が変化した");
}

// ---------- 層 B: spec 閾値ゲート（`#[ignore]`。`make precision-regression` 専用） ----------

/// `PRECISION_EVAL_MIN_*`/`PRECISION_EVAL_MAX_*` 環境変数の解決結果
/// （`hybrid_recall.rs::GateThreshold` と同型）。
enum GateThreshold {
    NotConfigured,
    Value(f64),
}

/// 最小値系（Top-1 Accuracy・MRR@10）は `(0.0, 1.0]`、最大値系（誤返却率）は
/// `[0.0, 1.0)` を許容範囲とする。最大値系の `1.0`（常時 pass）は fail-open と
/// 等価のため拒否する。
enum ThresholdKind {
    Min,
    Max,
}

fn threshold_from_env(var: &str, kind: ThresholdKind) -> Result<GateThreshold, String> {
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
    let in_range = match kind {
        ThresholdKind::Min => value > 0.0 && value <= 1.0,
        ThresholdKind::Max => (0.0..1.0).contains(&value),
    };
    if !in_range {
        return Err(match kind {
            ThresholdKind::Min => format!("{var} must be within (0.0, 1.0]"),
            ThresholdKind::Max => format!("{var} must be within [0.0, 1.0)"),
        });
    }
    Ok(GateThreshold::Value(value))
}

/// `PRECISION_EVAL_REQUIRE_THRESHOLDS` 環境変数（`"1"` のときのみ true）。
/// `hybrid_recall.rs::strict_thresholds_required` と同型の strict モードフラグ。
/// 目標値確定前の本 PR 時点では `.github/workflows/recall.yml` から注入されない
/// （申し送り。README 参照）。
fn strict_thresholds_required() -> bool {
    std::env::var("PRECISION_EVAL_REQUIRE_THRESHOLDS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

fn resolve_gate_threshold(var: &str, kind: ThresholdKind) -> Option<f64> {
    match threshold_from_env(var, kind) {
        Ok(GateThreshold::Value(v)) => Some(v),
        Ok(GateThreshold::NotConfigured) => {
            if strict_thresholds_required() {
                panic!(
                    "{var} is not configured but PRECISION_EVAL_REQUIRE_THRESHOLDS=1 (strict mode: this run must evaluate all PRECISION_EVAL_* thresholds)"
                );
            }
            None
        }
        Err(msg) => panic!("{var} invalid: {msg}"),
    }
}

/// TASK-163（SEARCH-10）層 B: hybrid ランキングの 3 指標が `PRECISION_EVAL_MIN_TOP1_ACC`・
/// `PRECISION_EVAL_MIN_MRR10`・`PRECISION_EVAL_MAX_FALSE_RETURN`（未確定。Actions
/// variables 由来を想定）を満たすかを判定する閾値ゲート。未設定は既定（非 strict）
/// では「対象外」として明示的に成功終了し、strict モードでは fail-closed でテスト
/// 失敗とする。設定済みで非数値・範囲外は常に fail-closed（`hybrid_recall.rs::
/// hybrid_recall_small_scale_threshold_gate` と同一契約）。ログには実測値と
/// pass/fail のみを出力し、注入された閾値の数値は出力しない。
#[test]
#[ignore = "spec 閾値（目標値。Actions variables 由来を想定）が必要なため既定では実行しない。make precision-regression で実行する"]
fn precision_eval_threshold_gate() {
    let min_top1 = resolve_gate_threshold("PRECISION_EVAL_MIN_TOP1_ACC", ThresholdKind::Min);
    let min_mrr10 = resolve_gate_threshold("PRECISION_EVAL_MIN_MRR10", ThresholdKind::Min);
    let max_false_return =
        resolve_gate_threshold("PRECISION_EVAL_MAX_FALSE_RETURN", ThresholdKind::Max);

    if min_top1.is_none() && min_mrr10.is_none() && max_false_return.is_none() {
        println!(
            "precision_eval_threshold_gate: PRECISION_EVAL_MIN_TOP1_ACC/PRECISION_EVAL_MIN_MRR10/PRECISION_EVAL_MAX_FALSE_RETURN not configured; gate not enabled (explicit no-op, not a failure)"
        );
        return;
    }

    let (core, _guard, ctx, qa, no_answer) = build_fixture();
    let r = measure(&core, &ctx, Ranking::Hybrid, &qa, &no_answer);

    let mut pass = true;
    if let Some(min) = min_top1 {
        let p = r.top1_accuracy() >= min;
        pass &= p;
        println!(
            "precision_eval_threshold_gate: top1_accuracy={:.4} pass={p}",
            r.top1_accuracy()
        );
    }
    if let Some(min) = min_mrr10 {
        let p = r.mrr10() >= min;
        pass &= p;
        println!(
            "precision_eval_threshold_gate: mrr10={:.4} pass={p}",
            r.mrr10()
        );
    }
    if let Some(max) = max_false_return {
        let p = r.false_return_rate() <= max;
        pass &= p;
        println!(
            "precision_eval_threshold_gate: false_return_rate={:.4} pass={p}",
            r.false_return_rate()
        );
    }

    assert!(
        pass,
        "hybrid precision evaluation below configured PRECISION_EVAL_* threshold"
    );
}

// ---------- 感度スイープ（`#[ignore]`。判断材料の提示専用。アサートなし） ----------

/// `PrecisionPolicy::new` の dense/hybrid 閾値を小さな格子で差し替え、hybrid
/// ランキングの 3 指標の変化を表形式で出力する（目標値確定のための判断材料。
/// production の既定値〔`precision::DEFAULT_*`〕は変更しない。`with_precision_policy`
/// によるテスト内差し替えのみ）。
#[test]
#[ignore = "判断材料の提示専用（表出力）。make precision-regression または cargo test -- --ignored precision_eval_policy_sweep --nocapture で実行する"]
fn precision_eval_policy_sweep() {
    let (docs, inverted) = generate_corpus(CORPUS_SEED, NUM_DOCS, VOCAB_SIZE);
    let mut qa_rng = DeterministicRng::new(CORPUS_SEED.wrapping_add(QA_SEED_OFFSET));
    let qa = generate_qa_set(&mut qa_rng, &docs, &inverted, VOCAB_SIZE, NUM_QPLUS_QUERIES);
    let mut no_answer_rng = DeterministicRng::new(CORPUS_SEED.wrapping_add(NO_ANSWER_SEED_OFFSET));
    let mut no_answer = generate_no_answer_set(
        &mut no_answer_rng,
        &inverted,
        VOCAB_SIZE,
        NUM_QZERO_HARD_NEGATIVE,
    );
    no_answer.extend(generate_out_of_vocab_set(VOCAB_SIZE, NUM_QZERO_OOV));

    let ctx = PolicyContext::new("precision-eval-tenant").expect("valid tenant");

    println!("=== TASK-163 precision パラメータ感度スイープ（hybrid。判断材料専用） ===");
    // Top-1 Accuracy・MRR@10・誤返却率はゲート通過の有無・先頭行のみに依存し
    // `max_results` そのものには反応しない（構造上の理由は [`EvalResult::
    // avg_result_rows`] 参照）。`avg_result_rows`（非空クエリ 1 件あたりの平均返却
    // 行数）を併記し、`max_results` を差し替えたことの効果が表から読み取れるように
    // する。
    println!(
        "hybrid_min_top1  hybrid_min_margin  max_results  top1_acc  mrr10  false_return_rate  avg_result_rows"
    );
    for &hybrid_min_top1 in &[0.90, 0.98, 0.995] {
        for &hybrid_min_margin in &[0.001, 0.005, 0.02] {
            for &max_results in &[1usize, 3] {
                let policy = PrecisionPolicy::new(
                    engine::precision::DEFAULT_DENSE_MIN_TOP1,
                    engine::precision::DEFAULT_DENSE_MIN_MARGIN,
                    hybrid_min_top1,
                    hybrid_min_margin,
                    max_results,
                )
                .expect("swept policy must construct");
                let (core, _guard) = setup_core(&docs, VOCAB_SIZE);
                let core = core.with_precision_policy(policy);
                let r = measure(&core, &ctx, Ranking::Hybrid, &qa, &no_answer);
                println!(
                    "{hybrid_min_top1:.3}  {hybrid_min_margin:.3}  {max_results}  {:.4}  {:.4}  {:.4}  {:.4}",
                    r.top1_accuracy(),
                    r.mrr10(),
                    r.false_return_rate(),
                    r.avg_result_rows(),
                );
            }
        }
    }
}
