//! TASK-163（対象ビヘイビア: SEARCH-10。ポインタ: `docs/spec/05-tasks.md` TASK-163・
//! `docs/spec/04-behavior/search.md` SEARCH-10・`docs/spec/06-roadmap.md` MS-3）。
//!
//! `precision` モード（TASK-162。`crates/engine/src/precision.rs`）の評価基準を
//! 実測する評価ハーネス。`tests/hybrid_recall.rs`（TASK-104）・
//! `tests/rerank_recall.rs`（TASK-108）の決定的合成コーパス方式を踏襲しつつ、
//! 2 層構成は「層 A 構造不変条件／層 B 閾値ゲート」とする。
//!
//! **SEARCH-10 の評価指標の実測値は public な本リポジトリに一切記録しない**
//! （`.claude/rules/spec-confidentiality.md`。PR #212 codex-review P0）。実測値・
//! パラメータ感度・目標値確定の判断材料の記録先は spec 側（上記ポインタ）であり、
//! 本ファイル・`docs/design/precision-eval-regression.md` には具体値を書かない。
//! 品質の回帰判定は層 B の `PRECISION_EVAL_*` 環境変数で注入された閾値との比較
//! だけが行う。
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
//! # 2 層構成
//!
//! - 層 A（`#[test]`・`make ci` 対象）: 評価を production の SQL 経路で通しで実行し、
//!   構造不変条件（カウンタの上下関係・`PrecisionPolicy::max_results` の遵守・
//!   指標が `[0.0, 1.0]` に収まること）と測定の決定性のみを検査する。指標の実測値は
//!   アサートも出力もしない（値が public な Actions ログ・テストコードに残らない）。
//! - 層 B（`#[ignore]`・`make precision-regression`。pass/fail のみ出力）: `PRECISION_EVAL_MIN_TOP1_ACC`・
//!   `PRECISION_EVAL_MIN_MRR10`・`PRECISION_EVAL_MAX_FALSE_RETURN` 環境変数
//!   （`hybrid_recall.rs::resolve_gate_threshold` と同型の解決規則）による閾値ゲート。
//!   未設定時は「評価は実行するが判定はスキップ」（`PRECISION_EVAL_REQUIRE_THRESHOLDS=1`
//!   の strict モードでは fail-closed）。`.github/workflows/recall.yml` への接続は
//!   本タスクでは行わない（README 参照）。`RECALL_VERBOSE=1`（`GITHUB_ACTIONS` 下では
//!   拒否。Issue #303）の opt-in 時のみローカル診断用に実測値〔`value=`〕を追加出力
//!   する（`hybrid_recall.rs` 等と方針統一。既定出力は変更しない）。
//! - 判断材料レポート（`#[ignore]`・アサートなし）: 既定ポリシーでの hybrid・dense
//!   双方の指標を出力する。実測値を標準出力へ出すため**ローカル専用**の
//!   `make precision-report` からのみ実行し、CI・GitHub Actions からは実行しない
//!   （`GITHUB_ACTIONS` 検出時は測定前に fail-closed で拒否する。Issue #303）。
//! - 感度スイープ（`#[ignore]`・アサートなし）: `PrecisionPolicy::new` の閾値を
//!   小さな格子で差し替え、hybrid 系列・dense 系列それぞれの指標の変化を `println!`
//!   で表示する（目標値確定の判断材料。production の既定値は変更しない。結果の
//!   記録先は spec 側）。
//!
//! # TASK-158（性能計測プロトコル基盤）準拠
//!
//! 合成コーパスの疑似乱数は `harness::rng::DeterministicRng`（`#[path]` で
//! `benches/harness/mod.rs` を取り込む。`tests/bench_harness.rs`・`tests/
//! bench_accept.rs` と同一方式）を使う。決定的コーパス上の指標は入力の決定的関数
//! であり、同一シードから常に同一値が再現される（層 A の決定性検査がその保証を
//! 兼ねる）ため、warmup／計測回数・中央値等の時間計測プロトコルはレイテンシ基準を
//! 持たない本タスクでは適用対象外とする。

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
        // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
        // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
        // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{}", doc.id))
                .expect("valid operation_id");
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
            &op_id,
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
    /// 1 クエリあたり返却行数の最大値（`PrecisionPolicy::max_results` の遵守を
    /// 検査する構造不変条件用。[`precision_eval_hybrid_invariants`] 参照）。
    max_result_rows: usize,
}

impl EvalResult {
    /// Top-1 Accuracy = top1_hits / n_qplus（空集合は不正解扱い。SEARCH-10）。
    fn top1_accuracy(&self) -> f64 {
        self.top1_hits as f64 / self.n_qplus as f64
    }

    /// MRR@10 = Σ(1/rank) / n_qplus（`mrr_hits_by_rank` は `rank -> 件数` の集計。
    /// 順位は `precision` モードの返却列における最初の正解行の位置で、正解を含まない
    /// クエリ・空集合クエリは寄与 0 として分母 `n_qplus` に算入する）。
    ///
    /// `PrecisionPolicy::max_results` が返却行数の上限であるため、順位の取りうる
    /// 範囲は `1..=max_results` に限られる。既定ポリシー（`max_results == 1`）の下では
    /// 本値は Top-1 Accuracy と構造的に一致し、順位の広がりは
    /// [`precision_eval_policy_sweep`] で `max_results` を差し替えたときにのみ現れる。
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

    /// 診断値: 非空クエリ 1 件あたりの平均返却行数。Top-1 Accuracy・誤返却率は
    /// ゲート通過の有無と先頭行のみに依存し `PrecisionPolicy::max_results` には
    /// 反応しないため、`precision_eval_policy_sweep` で `max_results` の効果を
    /// 読み取れるよう MRR@10（順位の広がりに反応する）と併せて表に出す
    /// （閾値ゲートの判定対象外の診断値）。
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
    let mut max_result_rows = 0usize;

    for case in qa {
        let precision_sql = ranking.precision_sql(&case.query_text, &case.query_vector);
        let precision_ids = result_ids(core, ctx, &precision_sql);
        max_result_rows = max_result_rows.max(precision_ids.len());
        if let Some(&top1) = precision_ids.first() {
            coverage_hits += 1;
            coverage_row_count += precision_ids.len();
            if case.correct.contains(&top1) {
                top1_hits += 1;
            }
        }

        // MRR@10 も `precision` モードの返却列（`precision_sql`）から算出する。
        // `recall` モードの結果で測ると確信度ゲート・`PrecisionPolicy` が指標に
        // 反映されず、`PRECISION_EVAL_MIN_MRR10` が別モードの品質を判定してしまう
        // （PR #212 codex-review P1）。ゲートが空集合へ倒したクエリ・正解を含まない
        // クエリは順位を記録せず、寄与 0 として分母 `n_qplus` に算入する。
        if let Some(rank) = precision_ids
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
        max_result_rows = max_result_rows.max(precision_ids.len());
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
        max_result_rows,
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

// ---------- 層 A: 構造不変条件・決定性の検査（`#[test]`。`make ci` 対象。実測値は扱わない） ----------

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

/// [`measure`] の結果が満たすべき構造不変条件を検査する（実測値そのものは
/// 検査しない）。`EvalResult` の各カウンタは定義上の上下関係と `PrecisionPolicy`
/// の返却行数上限に従うはずであり、これを破るのはハーネス・ゲート配線のバグ。
/// 品質の良し悪しの判定は層 B（`PRECISION_EVAL_*` 閾値ゲート）だけが行う
/// （SEARCH-10 の実測値・目標値は spec 側で管理する。PR #212 codex-review P0）。
fn assert_structural_invariants(r: &EvalResult, max_results: usize) {
    assert!(r.n_qplus > 0 && r.n_qzero > 0, "評価セットが空");
    assert!(
        r.top1_hits <= r.coverage_hits && r.coverage_hits <= r.n_qplus,
        "Top-1 命中数・coverage 件数の上下関係が破れた"
    );
    assert!(r.false_returns <= r.n_qzero, "誤返却件数が Q0 件数を超えた");
    assert!(
        r.max_result_rows <= max_results,
        "precision モードが PrecisionPolicy::max_results を超える行を返した"
    );
    for (&rank, &count) in &r.mrr_hits_by_rank {
        assert!(
            rank >= 1 && rank <= max_results,
            "MRR の順位が返却行数上限の範囲外"
        );
        assert!(count <= r.n_qplus, "順位別命中数が Q+ 件数を超えた");
    }
    assert!(
        r.mrr_hits_by_rank.values().sum::<usize>() <= r.n_qplus,
        "順位別命中数の総和が Q+ 件数を超えた"
    );
    for metric in [
        r.top1_accuracy(),
        r.mrr10(),
        r.false_return_rate(),
        r.coverage(),
        r.conditional_top1_accuracy(),
    ] {
        assert!((0.0..=1.0).contains(&metric), "指標が [0.0, 1.0] の外");
    }
}

/// 2 回の [`measure`] が完全に一致することを検査する（決定的コーパス上の指標は
/// 入力の決定的関数であるという前提の検査。失敗時も実測値は出力しない）。
fn assert_same_measurement(a: &EvalResult, b: &EvalResult) {
    assert!(
        a.n_qplus == b.n_qplus
            && a.n_qzero == b.n_qzero
            && a.top1_hits == b.top1_hits
            && a.coverage_hits == b.coverage_hits
            && a.coverage_row_count == b.coverage_row_count
            && a.false_returns == b.false_returns
            && a.max_result_rows == b.max_result_rows
            && a.mrr_hits_by_rank == b.mrr_hits_by_rank,
        "同一フィクスチャに対する測定が再現しなかった（非決定性）"
    );
}

/// TASK-163（SEARCH-10）層 A: hybrid ランキング（主）の評価を production の SQL 経路
/// で通しで実行し、構造不変条件と決定性のみを検査する。
///
/// 指標の実測値は public リポジトリに記録せず（`.claude/rules/spec-confidentiality.md`。
/// 記録先は spec 側〔ポインタ: `docs/spec/05-tasks.md` TASK-163・
/// `docs/spec/04-behavior/search.md` SEARCH-10〕）、品質の回帰判定は層 B の
/// `PRECISION_EVAL_*` 閾値ゲート（[`precision_eval_threshold_gate`]）だけが行う。
/// 本テストが守るのは「ハーネスが production 経路で完走し、結果が決定的で、
/// `PrecisionPolicy` の契約を破らない」ことまで。
#[test]
fn precision_eval_hybrid_invariants() {
    let (core, _guard, ctx, qa, no_answer) = build_fixture();
    // フィクスチャ規模の drift 検知（生成側の重複除外・生成規則が変わると崩れる）。
    // 期待値はテスト内定数（`NUM_*`）から導かれるもので、実測値ではない。
    assert_eq!(
        qa.len(),
        NUM_QPLUS_QUERIES,
        "重複除外後の Q+ 件数がフィクスチャ定数と一致しない"
    );
    assert_eq!(
        no_answer.len(),
        NUM_QZERO_HARD_NEGATIVE + NUM_QZERO_OOV,
        "Q0 件数がフィクスチャ定数と一致しない"
    );

    let max_results = PrecisionPolicy::default().max_results();
    let r = measure(&core, &ctx, Ranking::Hybrid, &qa, &no_answer);
    assert_structural_invariants(&r, max_results);
    assert_same_measurement(&r, &measure(&core, &ctx, Ranking::Hybrid, &qa, &no_answer));
}

/// TASK-163（SEARCH-10）層 A: dense ランキング（副）版
/// （[`precision_eval_hybrid_invariants`] と同一フィクスチャ・同一契約）。
#[test]
fn precision_eval_dense_invariants() {
    let (core, _guard, ctx, qa, no_answer) = build_fixture();

    let max_results = PrecisionPolicy::default().max_results();
    let r = measure(&core, &ctx, Ranking::Dense, &qa, &no_answer);
    assert_structural_invariants(&r, max_results);
    assert_same_measurement(&r, &measure(&core, &ctx, Ranking::Dense, &qa, &no_answer));
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

// ---------- 実測値の既定非出力（Issue #303）。`RECALL_VERBOSE` opt-in・`GITHUB_ACTIONS`
// 下の実測値出力の二重拒否ゲート ----------
// `hybrid_recall.rs`/`rerank_recall.rs`/`query_planning_recall.rs` の同名ヘルパと
// 同一実装（`tests/` 直下は独立 test crate・共有モジュール無しの既存慣行に合わせて
// ファイルごとに複製する）。

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
/// する（[`precision_eval_threshold_gate`] の冒頭で呼ぶ）。
fn verbose_requested_from_env() -> bool {
    let raw = std::env::var("RECALL_VERBOSE").ok();
    match resolve_verbose(raw.as_deref(), std::env::var_os("GITHUB_ACTIONS").is_some()) {
        Ok(v) => v,
        Err(msg) => panic!("{msg}"),
    }
}

/// `verbose=true` のときのみ実測値（`value=<f64:.4>`）を付加した診断行を描画する
/// （`query_planning_recall.rs::render_verbose_value_line` と同型）。
fn render_verbose_value_line(
    gate: &str,
    metric: &str,
    value: f64,
    verbose: bool,
) -> Option<String> {
    verbose.then(|| format!("{gate}: {metric} value={value:.4}"))
}

/// [`precision_eval_report`]・[`precision_eval_policy_sweep`]（実測値を常時出力する
/// 判断材料専用テスト。ローカル専用の `make precision-report` 経由を想定）の冒頭で
/// 呼ぶ fail-closed ガード。`GITHUB_ACTIONS`（値を解釈せず存在有無のみ判定）が
/// 設定された環境での実行は、コーパス生成・測定の前に `panic!` で拒否する
/// （実測値そのものを出力する専用テストのため、他ハーネスの `RECALL_VERBOSE` opt-in
/// ゲートと異なり opt-in の余地を設けない。従来 `Makefile` 運用〔`make
/// precision-report` はローカル専用〕のみで守られていた「ローカル専用」制約を、
/// テスト側の拒否で二重化する。Issue #303）。
fn refuse_measured_output_under_github_actions(test_name: &str) {
    if let Err(msg) =
        check_measured_output_allowed(test_name, std::env::var_os("GITHUB_ACTIONS").is_some())
    {
        panic!("{msg}");
    }
}

/// [`refuse_measured_output_under_github_actions`] の判定本体を環境変数から切り離した
/// 純関数（単体テスト可能）。`under_github_actions` が真なら `Err` を返す。
fn check_measured_output_allowed(
    test_name: &str,
    under_github_actions: bool,
) -> Result<(), String> {
    if under_github_actions {
        return Err(format!(
            "{test_name} prints measured values and is refused while running under GitHub Actions (GITHUB_ACTIONS is set); run locally via `make precision-report`"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod verbose_gate_tests {
    use super::{check_measured_output_allowed, render_verbose_value_line, resolve_verbose};

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
    fn render_verbose_value_line_non_verbose_is_none() {
        assert_eq!(
            render_verbose_value_line(
                "precision_eval_threshold_gate",
                "top1_accuracy",
                0.1234,
                false
            ),
            None
        );
    }

    #[test]
    fn render_verbose_value_line_verbose_includes_measured_value() {
        let line = render_verbose_value_line(
            "precision_eval_threshold_gate",
            "top1_accuracy",
            0.1234,
            true,
        )
        .expect("verbose line");
        assert!(line.contains("value=0.1234"));
    }

    #[test]
    fn check_measured_output_allowed_ok_outside_github_actions() {
        assert!(check_measured_output_allowed("precision_eval_report", false).is_ok());
    }

    #[test]
    fn check_measured_output_allowed_fails_closed_under_github_actions() {
        let err = check_measured_output_allowed("precision_eval_report", true)
            .expect_err("must be refused under GitHub Actions");
        assert!(err.contains("precision_eval_report"));
        assert!(err.contains("GitHub Actions"));
    }
}

/// TASK-163（SEARCH-10）層 B: hybrid ランキングの 3 指標が `PRECISION_EVAL_MIN_TOP1_ACC`・
/// `PRECISION_EVAL_MIN_MRR10`・`PRECISION_EVAL_MAX_FALSE_RETURN`（未確定。Actions
/// variables 由来を想定）を満たすかを判定する閾値ゲート。評価自体は閾値の設定状況に
/// 関わらず実行し、未設定は既定（非 strict）では「判定のみスキップ＝対象外」として
/// 明示的に成功終了し、strict モードでは fail-closed でテスト失敗とする。設定済みで非数値・範囲外は常に fail-closed（`hybrid_recall.rs::
/// hybrid_recall_small_scale_threshold_gate` と同一契約）。ログには指標名と pass/fail
/// のみを出力し、注入された閾値の数値も実測値も出力しない（`make precision-regression`
/// は将来 public runner から実行されうるため）。`RECALL_VERBOSE=1`（`GITHUB_ACTIONS`
/// 下では拒否。Issue #303）の opt-in 時のみローカル診断用に `value=` を追加出力する
/// （[`render_verbose_value_line`]。既定出力は変更しない）。3 指標はいずれも
/// `precision` モードの返却列から算出する（[`measure`] 参照）。
#[test]
#[ignore = "spec 閾値（目標値。Actions variables 由来を想定）が必要なため既定では実行しない。make precision-regression で実行する"]
fn precision_eval_threshold_gate() {
    let verbose = verbose_requested_from_env();
    let min_top1 = resolve_gate_threshold("PRECISION_EVAL_MIN_TOP1_ACC", ThresholdKind::Min);
    let min_mrr10 = resolve_gate_threshold("PRECISION_EVAL_MIN_MRR10", ThresholdKind::Min);
    let max_false_return =
        resolve_gate_threshold("PRECISION_EVAL_MAX_FALSE_RETURN", ThresholdKind::Max);

    // 閾値の設定状況に関わらず、まず評価を production の SQL 経路で通しで実行する。
    // 「閾値未設定でも評価は実行し、判定だけをスキップする」契約（README・
    // `docs/design/precision-eval-regression.md`）を満たし、fixture・SQL 経路の破損を
    // 未設定状態の実行でも検出できるようにするため（PR #212 codex-review P1）。
    let (core, _guard, ctx, qa, no_answer) = build_fixture();
    let r = measure(&core, &ctx, Ranking::Hybrid, &qa, &no_answer);

    if min_top1.is_none() && min_mrr10.is_none() && max_false_return.is_none() {
        println!(
            "precision_eval_threshold_gate: PRECISION_EVAL_MIN_TOP1_ACC/PRECISION_EVAL_MIN_MRR10/PRECISION_EVAL_MAX_FALSE_RETURN not configured; evaluation ran, comparison skipped (explicit no-op, not a failure)"
        );
        return;
    }

    // 出力は既定では指標名と pass/fail のみ。閾値の数値も実測値も出さない——本テストは
    // `make precision-regression` を通じて将来 public runner（`recall.yml`）から
    // 実行されうるため、Actions ログに非公開値を残さない
    // （`.claude/rules/spec-confidentiality.md`。PR #212 codex-review P0）。
    // `RECALL_VERBOSE=1`（`GITHUB_ACTIONS` 下では拒否）の opt-in 時のみローカル
    // 診断用に `value=` を追加出力する（Issue #303）。判断材料としての実測値出力は
    // 引き続きローカル専用の `make precision-report`
    // （[`precision_eval_report`]・[`precision_eval_policy_sweep`]）が担う。
    let mut pass = true;
    if let Some(min) = min_top1 {
        let value = r.top1_accuracy();
        let p = value >= min;
        pass &= p;
        println!("precision_eval_threshold_gate: top1_accuracy pass={p}");
        if let Some(line) = render_verbose_value_line(
            "precision_eval_threshold_gate",
            "top1_accuracy",
            value,
            verbose,
        ) {
            println!("{line}");
        }
    }
    if let Some(min) = min_mrr10 {
        let value = r.mrr10();
        let p = value >= min;
        pass &= p;
        println!("precision_eval_threshold_gate: mrr10 pass={p}");
        if let Some(line) =
            render_verbose_value_line("precision_eval_threshold_gate", "mrr10", value, verbose)
        {
            println!("{line}");
        }
    }
    if let Some(max) = max_false_return {
        let value = r.false_return_rate();
        let p = value <= max;
        pass &= p;
        println!("precision_eval_threshold_gate: false_return_rate pass={p}");
        if let Some(line) = render_verbose_value_line(
            "precision_eval_threshold_gate",
            "false_return_rate",
            value,
            verbose,
        ) {
            println!("{line}");
        }
    }

    assert!(
        pass,
        "hybrid precision evaluation below configured PRECISION_EVAL_* threshold"
    );
}

// ---------- 判断材料レポート（`#[ignore]`。層 B 側の出力専用。アサートなし） ----------

/// 既定ポリシー（`precision::DEFAULT_*`）での hybrid・dense 双方の指標を同一
/// コーパス上で出力する（`PrecisionPolicy` が dense/hybrid 別々の既定閾値を持つため、
/// 両方の妥当性判断には両系列の実測が要る）。
///
/// 実測値の出力はローカル専用の `make precision-report` に限定する（`make
/// precision-regression`＝閾値ゲートは pass/fail しか出さないため、将来 public runner
/// から実行されても値が漏れない）
/// （`.claude/rules/spec-confidentiality.md`。値の記録先は spec 側〔ポインタ:
/// `docs/spec/05-tasks.md` TASK-163・`docs/spec/04-behavior/search.md` SEARCH-10〕）。
#[test]
#[ignore = "判断材料の提示専用（実測値の出力）。ローカル専用の make precision-report または cargo test -- --ignored precision_eval_report --nocapture で実行する"]
fn precision_eval_report() {
    refuse_measured_output_under_github_actions("precision_eval_report");
    let (core, _guard, ctx, qa, no_answer) = build_fixture();
    print_eval_result(
        "hybrid",
        &measure(&core, &ctx, Ranking::Hybrid, &qa, &no_answer),
    );
    print_eval_result(
        "dense",
        &measure(&core, &ctx, Ranking::Dense, &qa, &no_answer),
    );
}

// ---------- 感度スイープ（`#[ignore]`。判断材料の提示専用。アサートなし） ----------

/// `PrecisionPolicy::new` の hybrid 閾値・dense 閾値をそれぞれ小さな格子で差し替え、
/// 対応するランキング（hybrid 系列・dense 系列）の 3 指標の変化を表形式で出力する
/// （dense/hybrid の既定閾値は独立に評価する必要があるため両系列を出す。
/// 目標値確定のための判断材料。
/// production の既定値〔`precision::DEFAULT_*`〕は変更しない。`with_precision_policy`
/// によるテスト内差し替えのみ）。
#[test]
#[ignore = "判断材料の提示専用（表出力）。ローカル専用の make precision-report または cargo test -- --ignored precision_eval_policy_sweep --nocapture で実行する"]
fn precision_eval_policy_sweep() {
    refuse_measured_output_under_github_actions("precision_eval_policy_sweep");
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
    // Top-1 Accuracy・誤返却率はゲート通過の有無と先頭行のみに依存し `max_results`
    // には反応しない。MRR@10 は `max_results` を広げたときに 2 位以下の正解を拾い、
    // `avg_result_rows`（非空クエリ 1 件あたりの平均返却行数）は返却量そのものを
    // 表すため、この 2 つで `max_results` の効果が表から読み取れる。
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

    println!("=== TASK-163 precision パラメータ感度スイープ（dense。判断材料専用） ===");
    println!(
        "dense_min_top1  dense_min_margin  max_results  top1_acc  mrr10  false_return_rate  avg_result_rows"
    );
    for &dense_min_top1 in &[0.60, 0.80, 0.90] {
        for &dense_min_margin in &[0.01, 0.05, 0.10] {
            for &max_results in &[1usize, 3] {
                let policy = PrecisionPolicy::new(
                    dense_min_top1,
                    dense_min_margin,
                    engine::precision::DEFAULT_HYBRID_MIN_TOP1,
                    engine::precision::DEFAULT_HYBRID_MIN_MARGIN,
                    max_results,
                )
                .expect("swept policy must construct");
                let (core, _guard) = setup_core(&docs, VOCAB_SIZE);
                let core = core.with_precision_policy(policy);
                let r = measure(&core, &ctx, Ranking::Dense, &qa, &no_answer);
                println!(
                    "{dense_min_top1:.3}  {dense_min_margin:.3}  {max_results}  {:.4}  {:.4}  {:.4}  {:.4}",
                    r.top1_accuracy(),
                    r.mrr10(),
                    r.false_return_rate(),
                    r.avg_result_rows(),
                );
            }
        }
    }
}
