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

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fmt::Write as _;

// Issue #387 PR #416 codex-review P2 指摘対応（2 巡目）: `sparse_refetch_observed`
// は非既定 feature `bench-internals` 限定公開（`hybrid.rs` 参照）のため、
// これに依存する関数（`sparse_refetch_schedule`）だけを同 feature の背後に置き、
// `RrfConfig`（同関数内でのみ使う）の import もそこに限定する。モジュール本体は
// 既定 feature でもコンパイルする（`mod.rs` の `pub mod hybrid_profile;` コメント
// 参照）。
#[cfg(feature = "bench-internals")]
use engine::hybrid::{sparse_refetch_observed, RrfConfig};
use engine::kernel::{SearchInput, SearchProvider};
use engine::sparse::{tokenize, DocId, ScoredDoc, SparseIndex};

use super::hybrid_latency::RefetchSummary;
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
    /// [`ProfileSparseIndex::build`] に同一 `DocId` が複数回渡された（Issue #387。
    /// `sparse.rs::with_params` は `id_index` を後勝ちで上書きするだけで重複自体は
    /// 拒否しないが、複製の忠実性検証（[`replica_matches_real`]）は「1 対 1 の
    /// `DocId` → エントリ対応」を前提にしているため、本複製は fail-closed に拒否する）。
    DuplicateDocId,
    /// ベンチ側複製（[`ProfileSparseIndex::search_within_replica`]）の出力が実
    /// `SparseIndex::search_within` の出力と一致しなかった（Issue #387。複製の
    /// 忠実性検証。転記ミス・演算順の食い違いを起動時に検知するための fail-closed
    /// ゲート）。`position` は不一致を検出した要素位置（列長が異なる場合は短い方の
    /// 長さ）。
    ReplicaMismatch { position: usize, detail: String },
    /// 疎側再取得スケジュール予測（[`sparse_refetch_schedule`]）と密側予測
    /// （[`dense_refetch_schedule`]）を、実 `hybrid_search`/`RefetchTrackingProvider`
    /// の観測値と突き合わせた際に不一致が生じた（Issue #387。境界同点判定の複製
    /// （[`boundary_tie_decision`]）が実装〔`hybrid.rs::resolve_boundary_tie_group`/
    /// `complete_boundary_tie_group_by`〕と食い違っている場合に起動時検知する）。
    RefetchMismatch {
        query: usize,
        predicted: usize,
        observed: usize,
    },
    /// 複製・スケジュール再現ロジックが自身の契約（例: `search_within` が要求
    /// `fetch_k` を超える件数を返した、再取得ラウンド数が [`MAX_REFETCH_ROUNDS`]
    /// を超えた）に違反した場合の fail-closed 拒否（coding-rust.md「曖昧な場合は
    /// 拒否側に倒す」）。
    ContractViolation(String),
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
            ProfileError::DuplicateDocId => {
                write!(f, "ProfileSparseIndex::build received a duplicate DocId")
            }
            ProfileError::ReplicaMismatch { position, detail } => {
                write!(f, "replica mismatch at position {position}: {detail}")
            }
            ProfileError::RefetchMismatch {
                query,
                predicted,
                observed,
            } => write!(
                f,
                "refetch schedule mismatch for query {query}: predicted={predicted} observed={observed}"
            ),
            ProfileError::ContractViolation(detail) => {
                write!(f, "contract violation: {detail}")
            }
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

// --- Issue #387: search_within の段別プロファイル・再取得ループ発火回数 ----------
//
// `hybrid.rs::MAX_POOL_DEPTH`/`MAX_FETCH_K`・`sparse.rs::SparseIndex` の内部
// フィールド（`docs`・`id_index`・`k1`・`b`）はいずれも `pub(crate)`/private の
// ため、本クレート外のベンチ・統合テストから直接参照できない。以下は Issue #356
// の build 複製（上記）と同じ方針で、`search_within` の 3 区間（可視 subset 構築／
// df 再計算パス／スコアリングパス）・境界同点判定（`resolve_boundary_tie_group`/
// `complete_boundary_tie_group_by`）・再取得の `fetch_k` スケジュール
// （`hybrid_search_boosted` の再取得ループ）をベンチ側に複製する。
//
// 複製固有の限界（`docs/design/hybrid-rrf-latency-breakdown.md`「Issue #387」節
// 参照）: `MAX_POOL_DEPTH_MIRROR`/`MAX_FETCH_K_MIRROR` は `hybrid.rs` の
// `pub(crate)` 定数の鏡像であり、値のドリフトはコード上の同期を強制できない
// （`max_pool_depth_mirror_matches_rrf_config_bounds` アクセプトテストで
// `RrfConfig::new` の受理境界と突き合わせ、ドリフトを検知する）。

/// [`hybrid::MAX_POOL_DEPTH`](../../src/hybrid.rs)（`pub(crate)`）の鏡像
/// （Issue #387）。値自体はこのファイルへ手動転記したものであり、`hybrid.rs`
/// 側が変わっても自動追従しない。ドリフトは
/// `max_pool_depth_mirror_matches_rrf_config_bounds`（`hybrid_profile_accept.rs`）
/// が `RrfConfig::new` の受理境界（`1..=MAX_POOL_DEPTH`）との突き合わせで検知する。
pub const MAX_POOL_DEPTH_MIRROR: usize = 10_000;

/// `hybrid.rs::MAX_FETCH_K`（`MAX_POOL_DEPTH * 4`）の鏡像（導出式ごと転記）。
pub const MAX_FETCH_K_MIRROR: usize = MAX_POOL_DEPTH_MIRROR * 4;

/// `sql/exec.rs::DEFAULT_HYBRID_POOL_DEPTH` の鏡像。SQL 表層の `hybrid_rrf` 経由
/// クエリは `pool_depth = k_eff.max(DEFAULT_HYBRID_POOL_DEPTH)` を使うため、
/// `LIMIT 10` 程度の小さな `k` では実質この値が `pool_depth` になる。
pub const SQL_DEFAULT_HYBRID_POOL_DEPTH: usize = 200;

/// [`sparse_refetch_schedule`]/[`dense_refetch_schedule`] の反復回数の防御的上限
/// （coding-rust.md「曖昧な場合は拒否側に倒す」）。`next_fetch_k` は倍増 → cap で
/// 頭打ちになるため理論上ごく少数回で終端するが、判定ロジックの複製に誤りがあり
/// 無限ループしてしまう場合に備え、有限回で必ず打ち切る。
pub const MAX_REFETCH_ROUNDS: usize = 64;

/// [`ProfileSparseIndex`] 内の 1 文書分の統計（`sparse.rs::DocEntry` の複製）。
#[derive(Debug, Clone)]
struct ProfileDocEntry {
    doc_id: DocId,
    term_freq: BTreeMap<String, u32>,
    doc_len: u32,
}

/// BM25 候補 1 件（`sparse.rs::Candidate` の複製。`Ord` は同一の「スコア降順・
/// 同点 doc_id 昇順」規約を持つ）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct ProfileCandidate {
    score: f64,
    doc_id: DocId,
}

impl Eq for ProfileCandidate {}

impl PartialOrd for ProfileCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ProfileCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.doc_id.cmp(&self.doc_id))
    }
}

/// `sparse.rs::SparseIndex` の計測用複製（Issue #387）。private フィールド
/// （`docs`・`id_index`・`k1`・`b`）を持つ実 `SparseIndex` の代わりに、
/// `search_within` の内部区間（可視 subset 構築／df 再計算パス／スコアリング
/// パス）を個別に呼び分けられる同型構造を持つ。既定パラメータ（`k1=1.2`・
/// `b=0.75`）は `sparse.rs::DEFAULT_K1`/`DEFAULT_B` の鏡像。
///
/// 出力の忠実性は [`replica_matches_real`] が実 `SparseIndex::search_within` の
/// 出力と突き合わせて検証する（Issue #356 の build 複製と異なり、本複製は
/// 公開 API 経由で数値比較可能）。
#[derive(Debug)]
pub struct ProfileSparseIndex {
    k1: f64,
    b: f64,
    docs: Vec<ProfileDocEntry>,
    id_index: BTreeMap<DocId, usize>,
}

impl ProfileSparseIndex {
    /// `sparse.rs::SparseIndex::with_params` の tokenize → term_freq 構築部分の
    /// 複製（上限検証は行わない。本ベンチのコーパスは [`generate_corpus`] 生成物
    /// のみで、上限を超える入力は構造的に発生しない——`build_actually_succeeds`
    /// と同じ限界）。
    pub fn build(docs: &[(DocId, &str)]) -> Result<Self, ProfileError> {
        let mut entries = Vec::with_capacity(docs.len());
        let mut id_index = BTreeMap::new();
        for (idx, (doc_id, text)) in docs.iter().enumerate() {
            if id_index.contains_key(doc_id) {
                return Err(ProfileError::DuplicateDocId);
            }
            let toks = tokenize(text);
            let mut term_freq: BTreeMap<String, u32> = BTreeMap::new();
            for tok in &toks {
                let counter = term_freq.entry(tok.clone()).or_insert(0u32);
                *counter = counter.saturating_add(1);
            }
            let doc_len = u32::try_from(toks.len()).unwrap_or(u32::MAX);
            id_index.insert(*doc_id, idx);
            entries.push(ProfileDocEntry {
                doc_id: *doc_id,
                term_freq,
                doc_len,
            });
        }
        Ok(Self {
            k1: 1.2,
            b: 0.75,
            docs: entries,
            id_index,
        })
    }

    /// 可視集合へ縮約した部分集合を求める（`search_within` 区間 1 の複製。
    /// `id_index` に存在しない `visible` の id は無視する — 実 API と同じ
    /// fail-closed の除外方針）。
    fn subset(&self, visible: &BTreeSet<DocId>) -> Vec<&ProfileDocEntry> {
        visible
            .iter()
            .filter_map(|id| self.id_index.get(id))
            .filter_map(|&idx| self.docs.get(idx))
            .collect()
    }

    fn query_terms(query: &str) -> BTreeMap<String, ()> {
        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in tokenize(query) {
            unique_terms.insert(t, ());
        }
        unique_terms
    }

    /// 区間 1（可視 subset 構築）のみ。戻り値は subset 件数（検算用）。
    pub fn subset_only(&self, _query: &str, visible: &BTreeSet<DocId>) -> usize {
        self.subset(visible).len()
    }

    /// 区間 1+2（可視 subset 構築 + df 再計算パス）。戻り値は Σdf（クエリ語ごとの
    /// 可視文書内出現件数の合計。検算用）。
    pub fn subset_df(&self, query: &str, visible: &BTreeSet<DocId>) -> usize {
        let subset = self.subset(visible);
        let unique_terms = Self::query_terms(query);
        let mut total_df = 0usize;
        for term in unique_terms.keys() {
            total_df += subset
                .iter()
                .filter(|d| d.term_freq.contains_key(term))
                .count();
        }
        total_df
    }

    /// 区間 1+2+3（可視 subset 構築 + df 再計算 + スコアリング・Top-k 選出）。
    /// `sparse.rs::SparseIndex::search_within` と演算順まで 1 対 1 対応させた
    /// 複製で、出力形状（`engine::sparse::ScoredDoc`・スコア降順同点 id 昇順）も
    /// 同一にする（[`replica_matches_real`] で数値一致を検証）。
    pub fn search_within_replica(
        &self,
        query: &str,
        k: usize,
        visible: &BTreeSet<DocId>,
    ) -> Vec<ScoredDoc> {
        let unique_terms = Self::query_terms(query);
        if k == 0 || unique_terms.is_empty() || visible.is_empty() {
            return Vec::new();
        }
        let subset = self.subset(visible);
        if subset.is_empty() {
            return Vec::new();
        }
        let local_n = subset.len();
        let local_total_len: u64 = subset.iter().map(|d| u64::from(d.doc_len)).sum();
        let local_avg_doc_len = local_total_len as f64 / local_n as f64;

        let mut local_doc_freq: BTreeMap<&str, u32> = BTreeMap::new();
        for term in unique_terms.keys() {
            let df = subset
                .iter()
                .filter(|d| d.term_freq.contains_key(term))
                .count();
            local_doc_freq.insert(term.as_str(), u32::try_from(df).unwrap_or(u32::MAX));
        }

        let heap_capacity = k.min(local_n);
        let mut heap: BinaryHeap<Reverse<ProfileCandidate>> =
            BinaryHeap::with_capacity(heap_capacity);
        for doc in &subset {
            let mut score = 0.0f64;
            for term in unique_terms.keys() {
                let Some(&f) = doc.term_freq.get(term.as_str()) else {
                    continue;
                };
                let df = *local_doc_freq.get(term.as_str()).unwrap_or(&0);
                let idf = idf_for_replica(local_n as f64, df);
                let numerator = f64::from(f) * (self.k1 + 1.0);
                let len_norm = 1.0 - self.b
                    + self.b * (f64::from(doc.doc_len) / local_avg_doc_len.max(f64::MIN_POSITIVE));
                let denominator = f64::from(f) + self.k1 * len_norm;
                if denominator > 0.0 {
                    score += idf * (numerator / denominator);
                }
            }
            if score > 0.0 {
                let candidate = ProfileCandidate {
                    score,
                    doc_id: doc.doc_id,
                };
                if heap.len() < k {
                    heap.push(Reverse(candidate));
                } else if let Some(Reverse(worst)) = heap.peek() {
                    if candidate > *worst {
                        heap.pop();
                        heap.push(Reverse(candidate));
                    }
                }
            }
        }

        let mut scored: Vec<ScoredDoc> = heap
            .into_iter()
            .map(|Reverse(ProfileCandidate { score, doc_id })| ScoredDoc { doc_id, score })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id)));
        scored
    }
}

/// `sparse.rs::SparseIndex::idf_for`（private）の複製。BM25 の IDF 式そのもの
/// （モジュールドキュメントを持たない私的関数のため、式の由来は `sparse.rs`
/// 冒頭コメントを参照）。
fn idf_for_replica(n: f64, df: u32) -> f64 {
    let df = f64::from(df);
    ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
}

/// 複製（[`ProfileSparseIndex::search_within_replica`]）の出力を実
/// `SparseIndex::search_within` の出力と突き合わせる（Issue #387。起動時
/// fail-closed 検証。`hybrid_profile_bench.rs::main` が忠実性検証として呼ぶ）。
pub fn replica_matches_real(
    real: &SparseIndex,
    replica: &ProfileSparseIndex,
    query: &str,
    k: usize,
    visible: &BTreeSet<DocId>,
) -> Result<(), ProfileError> {
    let real_hits = real
        .search_within(query, k, visible)
        .map_err(|e| ProfileError::ContractViolation(format!("real search_within failed: {e}")))?;
    let replica_hits = replica.search_within_replica(query, k, visible);
    if real_hits.len() != replica_hits.len() {
        return Err(ProfileError::ReplicaMismatch {
            position: real_hits.len().min(replica_hits.len()),
            detail: format!(
                "length mismatch: real={} replica={}",
                real_hits.len(),
                replica_hits.len()
            ),
        });
    }
    for (i, (r, p)) in real_hits.iter().zip(replica_hits.iter()).enumerate() {
        if r.doc_id != p.doc_id {
            return Err(ProfileError::ReplicaMismatch {
                position: i,
                detail: format!("doc_id mismatch: real={} replica={}", r.doc_id, p.doc_id),
            });
        }
        if r.score.total_cmp(&p.score) != std::cmp::Ordering::Equal {
            return Err(ProfileError::ReplicaMismatch {
                position: i,
                detail: format!(
                    "score mismatch at doc_id={}: real={} replica={}",
                    r.doc_id, r.score, p.score
                ),
            });
        }
    }
    Ok(())
}

/// [`resolve_boundary_tie_group`]/[`complete_boundary_tie_group_by`]（`hybrid.rs`。
/// いずれも `pub(crate)`/private）の判定結果のみを複製した型（Issue #387）。
/// 列そのものの切り詰めは行わず「終端確定できたか」だけを表す（呼び出し元の
/// 再取得スケジュール再現に必要なのは判定結果のみのため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieDecision {
    Resolved,
    Undetermined,
}

/// `hybrid.rs::complete_boundary_tie_group_by` の判定部の複製（列の切り詰めは
/// 行わない）。
fn complete_boundary_tie_decision(
    scores: &[f64],
    pool_depth: usize,
    exhaustive: bool,
) -> TieDecision {
    if pool_depth == 0 || scores.len() < pool_depth {
        return TieDecision::Resolved;
    }
    if scores.len() == pool_depth {
        return if exhaustive {
            TieDecision::Resolved
        } else {
            TieDecision::Undetermined
        };
    }
    let boundary_score = scores[pool_depth - 1];
    let next_score = scores[pool_depth];
    if boundary_score.total_cmp(&next_score) != std::cmp::Ordering::Equal {
        return TieDecision::Resolved;
    }
    let mut group_end = pool_depth;
    while group_end < scores.len()
        && scores[group_end].total_cmp(&boundary_score) == std::cmp::Ordering::Equal
    {
        group_end += 1;
    }
    // グループ終端が取得済み範囲内で確定できた（`group_end < scores.len()`）、
    // または取得済み範囲の末尾まで同点だが `exhaustive` によりそれが真の終端だと
    // 確定できる場合はいずれも `Resolved`（`hybrid.rs::complete_boundary_tie_group_by`
    // と同じ 2 条件の合流。clippy `if_same_then_else` 対応で 1 つの条件式へ統合）。
    if group_end < scores.len() || exhaustive {
        TieDecision::Resolved
    } else {
        TieDecision::Undetermined
    }
}

/// `hybrid.rs::resolve_boundary_tie_group` の判定部の複製（Issue #387。密・疎
/// 双方の再取得ループが `TieBoundary::Resolved`/`Undetermined` のどちらを返すかを
/// 予測する）。`zero_is_no_signal` は疎チャネル呼び出しでのみ `true`
/// （`hybrid_search_boosted` の疎側呼び出しと同じ引数）。
pub fn boundary_tie_decision(
    scores: &[f64],
    pool_depth: usize,
    exhaustive: bool,
    zero_is_no_signal: bool,
) -> TieDecision {
    if exhaustive || pool_depth == 0 || scores.len() < pool_depth || !zero_is_no_signal {
        return complete_boundary_tie_decision(scores, pool_depth, exhaustive);
    }
    if scores[pool_depth - 1] <= 0.0 {
        return TieDecision::Resolved;
    }
    complete_boundary_tie_decision(scores, pool_depth, exhaustive)
}

/// 可視集合サイズから再取得の上限（`MAX_FETCH_K_MIRROR` と可視集合サイズの
/// 小さい方）を求める（`hybrid_search_boosted` の `dense_cap`/`sparse_cap` の複製）。
pub fn fetch_cap(visible_len: usize) -> usize {
    MAX_FETCH_K_MIRROR.min(visible_len)
}

/// 初期 `fetch_k`（`pool_depth * 2` を `cap` で有界化。`checked_mul` オーバーフロー時は
/// `cap` へ飽和）。
pub fn initial_fetch_k(pool_depth: usize, cap: usize) -> usize {
    pool_depth.checked_mul(2).unwrap_or(cap).min(cap)
}

/// 次の `fetch_k`（倍増 → `cap` で頭打ち）。`current >= cap` なら再取得の余地が
/// ないため `None`。
pub fn next_fetch_k(current: usize, cap: usize) -> Option<usize> {
    if current >= cap {
        None
    } else {
        Some(current.saturating_mul(2).min(cap))
    }
}

/// 取得列が可視集合全体を覆い切っているか（`hybrid_search_boosted` の
/// `exhaustive` 算出の複製）。
pub fn is_exhaustive(fetch_k: usize, hits_len: usize, visible_len: usize) -> bool {
    fetch_k >= visible_len || hits_len < fetch_k
}

/// 再取得スケジュール 1 クエリ分の結果（Issue #387）。`fetch_ks` は実際に呼ばれた
/// `fetch_k` の列（呼び出し順）。
#[derive(Debug, Clone, PartialEq)]
pub struct RefetchSchedule {
    pub fetch_ks: Vec<usize>,
    pub final_hits: usize,
    pub reached_cap: bool,
}

/// 疎側の再取得スケジュールを、production の疎側再取得ループ実装そのもの
/// （`hybrid.rs::sparse_refetch_loop`。テスト・ベンチ向け公開フック
/// [`engine::hybrid::sparse_refetch_observed`] 経由）を呼んで**実際に観測する**
/// （Issue #387 PR #416 codex-review P1 指摘対応）。
///
/// 以前は境界同点判定（`hybrid.rs::resolve_boundary_tie_group`）をこのモジュール
/// 側で複製・予測しており、production の分岐・定数が変わっても複製が追随せず
/// 誤った基線を実測値として報告しうる構造だった。`sparse_refetch_observed` は
/// production と同一のコードパスを実行して実際に呼ばれた `fetch_k` の列を返す
/// ため、本関数はそれをそのまま [`RefetchSchedule`] へ写すだけであり、判定ロジック
/// の予測・複製は行わない（[`boundary_tie_decision`]・[`TieDecision`] は密側
/// [`dense_refetch_schedule`] の忠実性検証でのみ引き続き使う）。
///
/// [`sparse_refetch_observed`] が非既定 feature `bench-internals` 限定公開
/// のため、本関数も同 feature の背後に置く（Issue #387 PR #416 codex-review
/// P2 指摘対応・2 巡目）。
#[cfg(feature = "bench-internals")]
pub fn sparse_refetch_schedule(
    index: &SparseIndex,
    query: &str,
    visible: &BTreeSet<DocId>,
    pool_depth: usize,
) -> Result<RefetchSchedule, ProfileError> {
    let cap = fetch_cap(visible.len());
    // 疎側再取得ループはスコア重み（`k_const`・`dense_weight`・`sparse_weight`）を
    // 参照しない（`fetch_k` の決定は `cfg.pool_depth()` のみに依存。
    // `hybrid.rs::sparse_refetch_loop` 参照）ため、ここでは妥当性検証さえ通る
    // 任意の正値を渡し、`pool_depth` のみを呼び出し元の指定値に合わせる。
    let cfg = RrfConfig::new(1.0, 1.0, 1.0, pool_depth).map_err(|e| {
        ProfileError::ContractViolation(format!("RrfConfig::new failed for pool_depth: {e}"))
    })?;
    let (hits, sparse_limit, fetch_ks) = sparse_refetch_observed(index, query, visible, &cfg)
        .map_err(|e| {
            ProfileError::ContractViolation(format!("sparse_refetch_observed failed: {e}"))
        })?;
    Ok(RefetchSchedule {
        fetch_ks,
        final_hits: hits.len(),
        reached_cap: sparse_limit >= cap,
    })
}

/// 密側の再取得スケジュールを、実 `provider.search` を呼びながら再現する
/// （Issue #387。忠実性検証専用: 実測本体は既存
/// [`super::hybrid_latency::RefetchTrackingProvider`] を使う）。
pub fn dense_refetch_schedule(
    provider: &dyn SearchProvider,
    ids: &[u64],
    vectors: &[f32],
    dim: u32,
    query_vec: &[f32],
    pool_depth: usize,
) -> Result<RefetchSchedule, ProfileError> {
    let cap = fetch_cap(ids.len());
    let mut fetch_k = initial_fetch_k(pool_depth, cap);
    let mut fetch_ks = Vec::new();
    for _round in 0..MAX_REFETCH_ROUNDS {
        fetch_ks.push(fetch_k);
        let input = SearchInput {
            ids,
            vectors,
            dim,
            query: query_vec,
            k: fetch_k,
        };
        let hits = provider.search(input).map_err(|e| {
            ProfileError::ContractViolation(format!(
                "provider.search failed during schedule reproduction: {e}"
            ))
        })?;
        if hits.len() > fetch_k {
            return Err(ProfileError::ContractViolation(
                "provider.search returned more hits than requested fetch_k".to_string(),
            ));
        }
        let scores: Vec<f64> = hits.iter().map(|h| f64::from(h.score)).collect();
        let exhaustive = is_exhaustive(fetch_k, hits.len(), ids.len());
        match boundary_tie_decision(&scores, pool_depth, exhaustive, false) {
            TieDecision::Resolved => {
                return Ok(RefetchSchedule {
                    fetch_ks,
                    final_hits: hits.len(),
                    reached_cap: fetch_k >= cap,
                });
            }
            TieDecision::Undetermined => {
                if fetch_k >= cap {
                    return Ok(RefetchSchedule {
                        fetch_ks,
                        final_hits: hits.len(),
                        reached_cap: true,
                    });
                }
                fetch_k = next_fetch_k(fetch_k, cap).unwrap_or(cap);
            }
        }
    }
    Err(ProfileError::ContractViolation(
        "dense refetch schedule reproduction exceeded MAX_REFETCH_ROUNDS".to_string(),
    ))
}

/// 予測した再取得スケジュールの呼び出し回数と、実測観測（
/// [`super::hybrid_latency::RefetchTrackingProvider`] の `calls()`）が一致するか
/// 検証する（Issue #387。密側の忠実性検証。`hybrid_profile_bench.rs::main` が
/// 起動時に呼ぶ）。
pub fn refetch_schedule_matches_observed_calls(
    query_idx: usize,
    predicted: &RefetchSchedule,
    observed_calls: usize,
) -> Result<(), ProfileError> {
    if predicted.fetch_ks.len() != observed_calls {
        return Err(ProfileError::RefetchMismatch {
            query: query_idx,
            predicted: predicted.fetch_ks.len(),
            observed: observed_calls,
        });
    }
    Ok(())
}

/// 複数クエリ分の疎側再取得スケジュールの要約（Issue #387）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseRefetchSummary {
    pub queries: usize,
    pub calls_max: usize,
    pub calls_total: usize,
    pub reached_cap_count: usize,
    pub max_fetch_k: usize,
}

/// [`RefetchSchedule`] の列から [`SparseRefetchSummary`] を集計する純関数。
pub fn summarize_sparse_refetch(schedules: &[RefetchSchedule]) -> SparseRefetchSummary {
    let queries = schedules.len();
    let calls_max = schedules
        .iter()
        .map(|s| s.fetch_ks.len())
        .max()
        .unwrap_or(0);
    let calls_total: usize = schedules.iter().map(|s| s.fetch_ks.len()).sum();
    let reached_cap_count = schedules.iter().filter(|s| s.reached_cap).count();
    let max_fetch_k = schedules
        .iter()
        .flat_map(|s| s.fetch_ks.iter().copied())
        .max()
        .unwrap_or(0);
    SparseRefetchSummary {
        queries,
        calls_max,
        calls_total,
        reached_cap_count,
        max_fetch_k,
    }
}

/// 1 クエリ分の疎側再取得スケジュールを描画する（`hybrid_profile_bench.rs::main`
/// から呼ぶ）。
pub fn render_sparse_refetch_line(query_idx: usize, schedule: &RefetchSchedule) -> String {
    let fetch_ks_str = schedule
        .fetch_ks
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "hybrid_profile: sparse_refetch query={query_idx} calls={} fetch_ks={fetch_ks_str} \
         final_hits={} reached_cap={}",
        schedule.fetch_ks.len(),
        schedule.final_hits,
        schedule.reached_cap
    )
}

/// 疎側再取得の要約行を描画する。`estimated_cumulative_mixed_median_us` は
/// 呼び出し元（`hybrid_profile_bench.rs::main`）が
/// `search_within_fetch_k=<k>` 段（各 `fetch_k` を全クエリで round-robin
/// 測定した**全クエリ混合集団**の実測中央値。クエリ別の実測値ではない）を、
/// 最も再取得回数が多いクエリのスケジュールに沿って合算した**推定値**
/// （Issue #387 PR #416 codex-review P1 指摘対応。以前は
/// `cumulative_median_us`・「実測中央値」「最悪ケース」と表記しており、混合
/// 集団の中央値をクエリ別の実測累積コストであるかのように扱っていた）。
/// クエリ別の真の累積コストが必要な場合は各 `(query, fetch_k)` を個別に
/// 測定する必要があるが、本ベンチはその代わりに全クエリ混合中央値による
/// 推定を採用している（`fetch_k` ごとの実測が `fetch_k` によらずほぼ一定
/// （`docs/design/hybrid-rrf-latency-breakdown.md` 参照）であれば、この
/// 推定とクエリ別実測の乖離は小さいと考えられるが未検証）。
pub fn render_sparse_refetch_summary_line(
    summary: &SparseRefetchSummary,
    estimated_cumulative_mixed_median_us: u128,
) -> String {
    format!(
        "hybrid_profile: sparse_refetch_summary queries={} calls_max={} calls_total={} \
         reached_cap_count={} max_fetch_k={} \
         estimated_cumulative_mixed_median_us={estimated_cumulative_mixed_median_us}",
        summary.queries,
        summary.calls_max,
        summary.calls_total,
        summary.reached_cap_count,
        summary.max_fetch_k
    )
}

/// 密側再取得の実測段（`hybrid_search_cached_index`）を描画する
/// （`harness::hybrid_latency::render_stage_line` と同型の情報量。段名を
/// `hybrid_profile:` プレフィックスへ揃える）。
pub fn render_dense_refetch_line(
    stage: &str,
    median_us: u128,
    p95_us: u128,
    summary: &RefetchSummary,
) -> String {
    format!(
        "hybrid_profile: stage={stage} p95_us={p95_us} median_us={median_us} queries={} \
         provider_calls_max={} max_k_across_queries={} reached_visible_set={}/{}",
        summary.queries,
        summary.calls_max,
        summary.max_k_across_queries,
        summary.reached_visible_set_count,
        summary.queries,
    )
}
