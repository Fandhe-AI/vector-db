//! hybrid_rrf クエリ内訳プロファイル計測の時間非依存ヘルパ（Issue #356・親 Issue
//! #355。ポインタ: `docs/spec/04-behavior/search.md` SEARCH-1, SEARCH-3）。
//!
//! `benches/hybrid_profile_bench.rs`（実測。時間依存・`make ci` 対象外）が使う
//! 決定的コーパス生成・`SparseIndex::build`（`sparse.rs::with_params`）の内部段の
//! 複製関数・SQL 文字列組み立てを提供する。`tests/hybrid_profile_accept.rs` が
//! `#[path]` で本モジュールを取り込み、実測タイマー・実ストレージに依存しない
//! 契約のみを `cargo test`（`make ci` 対象）で検証する
//! （`harness/hybrid_latency.rs`・`tests/hybrid_latency_accept.rs` と同一パターン）。
//!
//! # 実測値の比較可能性についての重要な注意
//!
//! Issue #355 は「feature_bench（25,000 行）実測で hybrid_rrf p50 288.6ms」と
//! 記録しているが、その計測に使われたとされる `crates/engine/examples/
//! feature_bench.rs` はコミット `f749af6`（本ブランチの分岐元）時点のリポジトリ
//! 履歴に一度も存在しない（`git log --all --diff-filter=A -- 'crates/engine/
//! examples/*'` で確認済み。おそらく一時的なローカルスクリプトで計測され、
//! コミットされなかった）。そのため本モジュール・`hybrid_profile_bench.rs` の
//! コーパス（`generate_corpus`）は feature_bench の複製ではなく、Issue #356 本文の
//! 記述（行数・次元）にのみ合わせて新規に組み立てる。**したがって本ベンチの
//! 実測 ms は Issue #355 の 288ms と直接比較可能ではない**（コーパス内容・
//! 実行環境が異なるため）。本 Issue が実際に必要とするのは絶対値の再現ではなく
//! 「どの段が支配的か」という相対的な内訳の分解能であり、その目的には十分に
//! 応えられる（`hybrid_profile_bench.rs` 冒頭コメント・ADR
//! `docs/design/hybrid-rrf-latency-breakdown.md` 参照）。
//!
//! # スコープ（Issue #356 計画のレビューによる絞り込み）
//!
//! 境界同点グループ再取得ループの寄与は Issue #324（`harness/hybrid_latency.rs`・
//! `docs/design/hybrid-refetch-latency.md`）で既に測定済み・構造的に不変と
//! 確認済みのため、本モジュールでは再取得ループの統計収集を再実装しない。
//! 本モジュールが分解するのは (1) SQL 表層 hybrid_rrf 対 dense KNN の対照、
//! (2) 本文 String 収集、(3) `SparseIndex::build` 全体、(4) build 内部の
//! tokenize / term_freq / doc_freq 3 段、の 4 点である。
//!
//! # 単一テナント・Public のみに単純化
//!
//! Issue #355 の feature_bench 記述は tenant-a/tenant-b の 2 テナント構成だが、
//! 本 Issue が切り分けたいのは RLS 境界ではなく hybrid_rrf 内部の段別コストで
//! あるため、単一テナント・全行 Public に単純化する（`sql_c1_bench.rs` と同じ
//! 単純化方針）。可視行数と投入行数が一致するため、SQL 段のコーパスと直接 API 段の
//! コーパスの整合を `SELECT COUNT(*)` の突き合わせなしに構造的に保証できる。
//!
//! # 暗号用途禁止
//!
//! [`super::rng::DeterministicRng`] を経由するため非暗号 PRNG である
//! （`rng.rs` モジュールドキュメント参照）。ベンチ入力生成専用。

use std::collections::BTreeMap;
use std::fmt::Write as _;

use engine::sparse::{tokenize, DocId, SparseIndex};

use super::rng::DeterministicRng;

/// [`generate_corpus`] が許容する文書数の安全上限（coding-rust.md「無制限確保
/// 禁止」。`harness::hybrid_latency::MAX_CORPUS_DOCS_GUARD` と同一方針・同一値。
/// `SparseIndex::build` 自身の上限〔`pub(crate)` で本モジュールから参照不可〕とは
/// 独立に、ベンチ入力生成側でも上限を持たせる）。
pub const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

/// 疎チャネル本文を組み立てる語彙（40 語。Issue #356 計画が参照する feature_bench
/// 記述の「WORDS 周回 40 語」の規模感に合わせた、本モジュール独自の合成語彙。
/// spec 由来の値ではない）。
const WORDS: [&str; 40] = [
    "vector", "search", "database", "index", "query", "hybrid", "dense", "sparse", "rank", "score",
    "token", "term", "corpus", "document", "field", "value", "match", "filter", "tenant", "policy",
    "storage", "engine", "kernel", "cache", "cluster", "shard", "batch", "stream", "graph", "plan",
    "cost", "latency", "profile", "metric", "sample", "seed", "vocab", "weight", "rerank", "fuse",
];

/// 1 文書あたりの疎チャネル語数（固定。tokenize/term_freq/doc_freq 各段が
/// 無視できない計算量を持つ程度の非自明な文書長にする）。
const BODY_WORD_COUNT: usize = 30;

/// 合成コーパス 1 件分（密ベクトル・疎テキストの両方を持つ）。
#[derive(Debug, Clone)]
pub struct ProfileCorpus {
    /// 文書 id（0 始まりの連番。`SearchInput::ids`・`SparseIndex::build` の
    /// `DocId` 双方にそのまま使う）。
    pub ids: Vec<u64>,
    /// `ids.len() * dim` 要素のフラット化済みベクトル。
    pub vectors: Vec<f32>,
    /// 文書ごとの疎チャネル本文。
    pub bodies: Vec<String>,
    pub dim: u32,
}

impl ProfileCorpus {
    /// [`SparseIndex::build`] へ渡す `(DocId, &str)` スライスを組み立てる
    /// （`harness::hybrid_latency::Corpus::sparse_docs` と同型）。
    pub fn sparse_docs(&self) -> Vec<(DocId, &str)> {
        self.ids
            .iter()
            .copied()
            .zip(self.bodies.iter().map(String::as_str))
            .collect()
    }
}

/// [`generate_corpus`]・[`generate_queries`] の失敗系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// [`MAX_CORPUS_DOCS_GUARD`] を超過した。
    CorpusTooLarge,
    /// テーブル名・列名が識別子として不正だった（SQL 文字列組み立て時）。
    InvalidIdentifier(&'static str),
    /// クエリテキストに単一引用符等の SQL リテラルを壊す文字が含まれていた
    /// （本モジュールが生成するクエリテキストは常に [`WORDS`] のみから合成する
    /// ため通常到達しないが、`.claude/rules/coding-rust.md`「SQL / プラン文字列の
    /// 組み立てに未検証入力を連結しない」に従い、埋め込み直前に必ず検証する）。
    UnsafeQueryText,
    /// `GITHUB_ACTIONS` 実行環境下で本ベンチの実行が要求された。
    RefusedUnderGitHubActions,
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileError::CorpusTooLarge => {
                write!(f, "num_docs exceeds {MAX_CORPUS_DOCS_GUARD}")
            }
            ProfileError::InvalidIdentifier(field) => {
                write!(f, "{field} is not a valid identifier")
            }
            ProfileError::UnsafeQueryText => {
                write!(
                    f,
                    "query text contains a character unsafe for SQL literal embedding"
                )
            }
            ProfileError::RefusedUnderGitHubActions => write!(
                f,
                "hybrid_profile_bench is refused while running under GitHub Actions \
                 (GITHUB_ACTIONS is set); this bench is not wired into any workflow \
                 and must be run locally via `make bench-hybrid-profile`"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

/// `GITHUB_ACTIONS` が設定された実行環境下では本ベンチの実行自体を拒否する
/// 純関数（`harness::hybrid_latency::refuse_under_github_actions` と同型）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), ProfileError> {
    if under_github_actions {
        return Err(ProfileError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// 決定的シードから合成コーパスを生成する。`num_docs`・`dim` はベンチ内定数のみを
/// 受け取り、env 経由では受け取らない（coding-rust.md「untrusted 入力の扱い」）。
///
/// 密チャネル（`vectors`）と疎チャネル（`bodies`）は独立した RNG 系列から生成する
/// （`harness::hybrid_latency::generate_corpus` と同じ理由: 片方の生成方式を
/// 変えてももう片方の内容へ波及させないため）。
pub fn generate_corpus(
    seed: u64,
    num_docs: usize,
    dim: usize,
) -> Result<ProfileCorpus, ProfileError> {
    if num_docs > MAX_CORPUS_DOCS_GUARD {
        return Err(ProfileError::CorpusTooLarge);
    }

    let mut vector_rng = DeterministicRng::new(seed);
    let mut ids = Vec::with_capacity(num_docs);
    let mut vectors = Vec::with_capacity(num_docs * dim);
    let mut bodies = Vec::with_capacity(num_docs);

    for doc_id in 0..num_docs as u64 {
        ids.push(doc_id);
        vectors.extend(vector_rng.next_vector(dim));

        // 疎チャネルは RNG を消費せず、文書インデックスに基づく決定的な語彙回転
        // のみで組み立てる（密側の RNG 消費量〔`dim` に依存〕から完全に独立させ、
        // `dim` を変えても `bodies` の内容が変わらないようにする）。
        let mut body = String::with_capacity(8 + BODY_WORD_COUNT * 8);
        let _ = write!(body, "doc-{doc_id}");
        for j in 0..BODY_WORD_COUNT {
            body.push(' ');
            body.push_str(WORDS[(doc_id as usize + j) % WORDS.len()]);
        }
        bodies.push(body);
    }

    Ok(ProfileCorpus {
        ids,
        vectors,
        bodies,
        dim: dim as u32,
    })
}

/// クエリ 1 件（密クエリベクトル・疎クエリ文字列）。
#[derive(Debug, Clone)]
pub struct ProfileQuery {
    pub vector: Vec<f32>,
    pub text: String,
}

/// [`generate_corpus`] とは独立した RNG 系列からクエリ集合を生成する。
/// クエリテキストは常に [`WORDS`] からのみ合成するため、埋め込み直前の
/// [`validate_query_text`] を必ず通過する（構造的に安全）。
pub fn generate_queries(seed: u64, count: usize, dim: usize) -> Vec<ProfileQuery> {
    let mut rng = DeterministicRng::new(seed.wrapping_add(0x5151_5151_5151_5151));
    (0..count)
        .map(|i| {
            let vector = rng.next_vector(dim);
            let w1 = WORDS[(rng.next_u64() as usize) % WORDS.len()];
            let w2 = WORDS[(rng.next_u64() as usize) % WORDS.len()];
            ProfileQuery {
                vector,
                text: format!("{w1} {w2} doc-{i}"),
            }
        })
        .collect()
}

// --- SQL 文字列組み立て（untrusted 入力を連結しない。sql_c1.rs と同一方針） ---

/// `[A-Za-z_][A-Za-z0-9_]*` に一致するかを検証する（`sql_c1.rs::is_valid_identifier`
/// と同型。ベンチ本体は定数のみを渡すが、未検証文字列連結の経路をコード上に
/// 残さないための検証をここでも省かない）。
fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// クエリテキストが SQL 単一引用符リテラルへそのまま埋め込んで安全か検証する
/// （ASCII 英数字・空白・ハイフンのみを許可。[`generate_queries`] が組み立てる
/// テキストは常にこの部分集合だが、埋め込み直前に必ず検証してから連結する
/// ——`.claude/rules/coding-rust.md`「SQL / プラン文字列の組み立てに未検証入力を
/// 連結しない」）。
fn validate_query_text(text: &str) -> Result<(), ProfileError> {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
    {
        Ok(())
    } else {
        Err(ProfileError::UnsafeQueryText)
    }
}

/// `f32` のベクトルを SQL のベクトルリテラル形式（`[v1,v2,...]`）へ整形する
/// （`sql_c1.rs::vector_literal` の簡略版。本ベンチのベクトルは
/// [`generate_corpus`]/[`generate_queries`] の RNG 生成物のため非有限値・
/// サイズ上限超過は構造的に起こらないが、[`sql_hybrid_statement`]/
/// [`sql_dense_statement`] からの呼び出し規約を明示するため `Result` で返す）。
fn vector_literal(values: &[f32]) -> Result<String, ProfileError> {
    if values.iter().any(|v| !v.is_finite()) {
        return Err(ProfileError::InvalidIdentifier("query vector"));
    }
    let mut out = String::from("[");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{v}");
    }
    out.push(']');
    Ok(out)
}

/// hybrid_rrf 経由の規範形クエリ文字列を組み立てる: `SELECT * FROM <table>
/// ORDER BY hybrid_rrf(<vector_column>, '<literal>', <text_column>, '<query_text>')
/// LIMIT <k>`（`tests/sql_surface.rs` 等が使う受理形状と同一）。
pub fn sql_hybrid_statement(
    table: &str,
    vector_column: &str,
    text_column: &str,
    query_vector: &[f32],
    query_text: &str,
    k: usize,
) -> Result<String, ProfileError> {
    if !is_valid_identifier(table) {
        return Err(ProfileError::InvalidIdentifier("table"));
    }
    if !is_valid_identifier(vector_column) {
        return Err(ProfileError::InvalidIdentifier("vector_column"));
    }
    if !is_valid_identifier(text_column) {
        return Err(ProfileError::InvalidIdentifier("text_column"));
    }
    validate_query_text(query_text)?;
    let literal = vector_literal(query_vector)?;
    Ok(format!(
        "SELECT * FROM {table} ORDER BY hybrid_rrf({vector_column}, '{literal}', {text_column}, '{query_text}') LIMIT {k}"
    ))
}

/// 密 KNN のみの対照クエリ文字列を組み立てる: `SELECT * FROM <table> ORDER BY
/// <vector_column> <=> '<literal>' LIMIT <k>`（`sql_c1.rs::c1_statement` と同型だが
/// `SELECT *` にして hybrid 側と投影コストを揃える）。
pub fn sql_dense_statement(
    table: &str,
    vector_column: &str,
    query_vector: &[f32],
    k: usize,
) -> Result<String, ProfileError> {
    if !is_valid_identifier(table) {
        return Err(ProfileError::InvalidIdentifier("table"));
    }
    if !is_valid_identifier(vector_column) {
        return Err(ProfileError::InvalidIdentifier("vector_column"));
    }
    let literal = vector_literal(query_vector)?;
    Ok(format!(
        "SELECT * FROM {table} ORDER BY {vector_column} <=> '{literal}' LIMIT {k}"
    ))
}

// --- SparseIndex::build 内部 3 段の複製（`sparse.rs::with_params` の近似） -------
//
// `SparseIndex::with_params` は private フィールドを持つため、内部の tokenize /
// term_freq 構築 / doc_freq マージを個別に計測する入口が存在しない。以下は
// `sparse.rs:517-590`（`with_params`）のロジックを計測用に複製した近似実装であり、
// 上限検証（`MAX_DOC_BYTES` 等）は行わない（本ベンチのコーパスは
// [`generate_corpus`] 生成物のみで、上限を超える入力は構造的に発生しない）。
// 複製ロジックの妥当性は [`build_actually_succeeds`] の構造的整合性チェックで
// 検証する（`hybrid_profile_bench.rs` の起動時アサーション・ADR「複製近似の
// 限界」節）。

/// tokenize 段のみ（各文書を `tokenize()` するだけ）。戻り値は総トークン数
/// （検算・`black_box` 用の非自明な戻り値）。
pub fn tokenize_only(bodies: &[String]) -> usize {
    bodies.iter().map(|b| tokenize(b).len()).sum()
}

/// tokenize + term_freq 構築（文書内 `BTreeMap<String, u32>` の構築）。戻り値は
/// 全文書の一意語数の合計（tokenize 段からの追加コストを検算できるようにする）。
pub fn tokenize_term_freq(bodies: &[String]) -> usize {
    let mut total_unique_terms = 0usize;
    for b in bodies {
        let toks = tokenize(b);
        let mut term_freq: BTreeMap<String, u32> = BTreeMap::new();
        for tok in &toks {
            let counter = term_freq.entry(tok.clone()).or_insert(0u32);
            *counter = counter.saturating_add(1);
        }
        total_unique_terms += term_freq.len();
    }
    total_unique_terms
}

/// tokenize + term_freq + doc_freq マージ（`with_params` が構築する
/// `DocEntry`/`id_index` を除いた残り全て）。戻り値はコーパス全体の語彙数
/// （`doc_freq.len()`）で、`SparseIndex::build` の語彙数と一致するはずの検算値。
pub fn tokenize_term_doc_freq(bodies: &[String]) -> usize {
    let mut doc_freq: BTreeMap<String, u32> = BTreeMap::new();
    for b in bodies {
        let toks = tokenize(b);
        let mut term_freq: BTreeMap<String, u32> = BTreeMap::new();
        for tok in &toks {
            let counter = term_freq.entry(tok.clone()).or_insert(0u32);
            *counter = counter.saturating_add(1);
        }
        for term in term_freq.keys() {
            let counter = doc_freq.entry(term.clone()).or_insert(0u32);
            *counter = counter.saturating_add(1);
        }
    }
    doc_freq.len()
}

/// 本文 String 収集（`sql/exec.rs:370-509` の `sparse_docs: Vec<(u64, String)>`
/// 蓄積の近似下限）。`bodies` を clone して `(id, String)` へ詰め直すだけの純粋な
/// 複製コストを測る（redb 行デコード自体は含まない。ADR「帰属できない残差」節）。
pub fn collect_body_strings(ids: &[u64], bodies: &[String]) -> Vec<(u64, String)> {
    ids.iter().copied().zip(bodies.iter().cloned()).collect()
}

/// [`tokenize_term_doc_freq`] の複製実装が構造的に妥当であることを確認する
/// 整合性チェック（複製近似の限界: `SparseIndex` の `doc_freq`/`docs` は private
/// フィールドのため、公開 API 経由で実際の内部語彙数・文書統計を読み出して
/// 複製実装と数値比較する手段が存在しない。そのため fidelity は「同一入力に
/// 対して `SparseIndex::build` 自体が成功するか」という構造的な確認に留める。
/// 複製ロジック〔tokenize → term_freq → doc_freq〕は `sparse.rs::with_params` の
/// 該当行と 1 対 1 対応する手動転記であり、この整合性チェックはロジックの
/// 転記ミス〔例: 誤った BTreeMap キー〕によって build 自体が失敗する場合のみを
/// 検出できる。ADR「複製近似の限界」節参照）。
pub fn build_actually_succeeds(corpus: &ProfileCorpus) -> bool {
    SparseIndex::build(&corpus.sparse_docs()).is_ok()
}

/// 1 段の実測結果行を描画する（`hybrid_profile_bench.rs::main` から呼ぶ）。本ベンチは
/// spec 由来の非公開閾値を持たない情報提供専用のため、`sql_c1_bench.rs` のような
/// verbose opt-in ゲートは設けず、実測値（p95・median）を常に含める
/// （`hybrid_latency_bench.rs::render_stage_line` と同一方針）。
pub fn render_stage_line(stage: &str, median_us: u128, p95_us: u128, check_value: usize) -> String {
    format!("hybrid_profile: stage={stage} p95_us={p95_us} median_us={median_us} check_value={check_value}")
}
