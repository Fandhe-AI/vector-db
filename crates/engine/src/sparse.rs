//! 疎検索（BM25 Okapi）モジュール（TASK-102、対象ビヘイビア: SEARCH-1, SEARCH-3。
//! ポインタ: `docs/spec/05-tasks.md` TASK-102・`docs/spec/04-behavior/search.md`）。
//! 関連: TASK-103, TASK-104, TASK-105。
//!
//! 責務境界: コーパスからトークン頻度・文書長統計を持つ [`SparseIndex`] を構築し、
//! クエリに対する BM25 スコア降順の Top-k 検索を提供する純関数的な API を提供し、
//! storage/catalog とは結線しない。[`SparseIndex::search`] はインデックス全体
//! （構築時のコーパス全体）を母数に統計・Top-k を計算するのに対し、
//! [`SparseIndex::search_within`] は呼び出し元が渡す可視集合（`visible_ids`）へ
//! 統計計算・候補選出そのものを縮約する版であり、`hybrid.rs::hybrid_search`
//! （TASK-103）がテナント境界（RLS 相当）を保つために後者を使う（Issue #36
//! codex-review P0 指摘対応。事後フィルタでは防げない理由は
//! [`SparseIndex::search_within`] のドキュメントを参照）。
//!
//! Okapi BM25（公知のアルゴリズム）を実装する。クエリ語 `q` に対する文書 `d` のスコアは
//! 各語 `t` について
//!
//! ```text
//! score(d, q) = Σ_t idf(t) * ( f(t, d) * (k1 + 1) )
//!                            / ( f(t, d) + k1 * (1 - b + b * |d| / avgdl) )
//! ```
//!
//! で与え、`idf(t) = ln( (N - df(t) + 0.5) / (df(t) + 0.5) + 1 )` とする（`+ 1` により
//! 常に非負となる）。`k1`・`b` は [`SparseIndex::with_params`] で調整可能とし、既定値は
//! `k1 = 1.2`, `b = 0.75`。
//!
//! トークナイザ（[`tokenize`]）は ASCII 英数字・アンダースコアの連続を単語トークンとし、
//! CJK（ひらがな・カタカナ・CJK 統合漢字。カタカナ側は長音符 `ー`（U+30FC）を含むが
//! 記号の中黒 `・`（U+30FB）は単語区切りとして除外する）はユニグラム＋文字バイグラムを
//! 生成する（小文字化した上で処理）。既定（[`tokenize`]）ではユニグラムのうち助詞・
//! 機能語（TASK-105・SEARCH-5 の CJK ストップワード除去。詳細は [`tokenize_with_options`]）
//! の単独トークン化を抑制するが、CJK コンテンツ（内容語のバイグラム等）自体は保持する。
//! 対応範囲外の文字（全角英数字・アクセント付きラテン文字・ハングル・半角カタカナ等）は
//! 無音で破棄される。ASCII 単語の途中に出現した場合はその文字が欠落するだけでなく、
//! その位置で単語が分割される（前後が結合されるのではない。例: `"cafés"` →
//! `["caf", "s"]`）。この分割により生じた偽トークンは `term_freq`・`doc_freq`・
//! `doc_len` の統計を汚染しうる。
//!
//! [`SparseIndex::search`] は文書集合への線形走査で実装するが、走査中に各文書について
//! クエリの一意語集合（最大 `Q` 語）を `BTreeMap` で検索するため、時間計算量は
//! 文書数のみに比例するのではなく `O(N * Q * log V + M log k)`（`N`: コーパス文書数、
//! `Q`: クエリの一意語数、`V`: コーパス全体の語彙数、`M`: スコア `> 0` の一致文書数）
//! である。長いクエリは `N * Q` の積として CPU コストを増幅するため、一意語数の上限を
//! `MAX_QUERY_TERMS` で検証し、超過時は `Err` を返す（fail-closed）。ただし
//! `tokenize()` 自体はクエリのバイト長に比例したコストを持つため、一意語数の少ない
//! 繰り返し入力（同じ語や区切り文字の反復）はこの検証だけでは防げない。そのため
//! クエリのバイト長にも `MAX_QUERY_BYTES` の上限を設け、`tokenize()` を呼ぶ前に
//! 検証する。
//!
//! [`SparseIndex::build`]/[`SparseIndex::with_params`] も同様の理由で、各文書に対して
//! `tokenize()` を呼ぶ前に文書 1 件のバイト長（`MAX_DOC_BYTES`）とコーパスの文書数
//! （`MAX_CORPUS_DOCS`）を検証する。この 2 つは互いに独立な検証のため、両方の上限
//! ちょうどの組み合わせだけではコーパス全体のバイト数を有界に保てない。そのため
//! 走査済み文書のバイト長累計にも `MAX_CORPUS_BYTES` の上限を設ける。ただしバイト長
//! の上限は `tokenize()` が生成するトークン数（＝ヒープ確保数）を直接制限しない
//! （CJK 入力はユニグラム＋バイグラムにより 1 文字あたり最大 2 トークンを生じる
//! ため、バイト長に比例しない）。そのため各文書を `tokenize()` した直後にも走査済み
//! トークン数の累計に `MAX_CORPUS_TOKENS` の上限を設ける。詳細は
//! [`SparseIndex::with_params`] を参照。
//!
//! untrusted 入力の扱い: すべての処理を入力長に対して線形に保つ（バイグラム生成含む）。
//! `Vec::with_capacity` は入力を `chars()` で数えた実際の文字数からのみ見積もる。
//! クエリはバイト長を `MAX_QUERY_BYTES`、一意語数を `MAX_QUERY_TERMS` で上限検証し
//! （詳細は [`SparseIndex::search`]）、文書はバイト長を `MAX_DOC_BYTES`、コーパスの
//! 文書数を `MAX_CORPUS_DOCS`、コーパス全体のバイト長合計を `MAX_CORPUS_BYTES`、
//! コーパス全体のトークン数合計を `MAX_CORPUS_TOKENS` で上限検証する（詳細は
//! [`SparseIndex::with_params`]）。バイト長・件数の検証は `tokenize()` を呼ぶ前に
//! バイト長・件数のみを見る `O(1)` の判定で完結し追加アロケーションを要しないが、
//! トークン数の検証（`MAX_CORPUS_TOKENS`）だけは性質上 `tokenize()` の結果
//! （`Vec<String>` の長さ）が必要なため、`tokenize()` の直後・`term_freq`/`doc_freq`
//! の構築前に判定する。公開関数 [`tokenize`] 自体はこれらの上限を強制しない
//! （呼び出し側が上限検証済みの入力のみを渡す契約とする。詳細は [`tokenize`] の
//! ドキュメントを参照）。頻度・長さの演算はすべて `checked_*`/`saturating_*` を用い、
//! オーバーフローを未定義動作にしない。`tokenize()` 内の添字アクセスは事前の
//! ループ境界チェックにより範囲内が証明可能（panic しない）。

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::BinaryHeap;

/// 文書 ID。[`SparseIndex`] は呼び出し側が割り当てた ID をそのまま透過的に扱う。
pub type DocId = u64;

/// BM25 の Okapi パラメータ既定値（項の飽和度）。
const DEFAULT_K1: f64 = 1.2;
/// BM25 の Okapi パラメータ既定値（文書長正規化の強さ）。
const DEFAULT_B: f64 = 0.75;

/// [`SparseIndex::search`] が受け付けるクエリの一意語数の上限。
///
/// `search()` は一致判定のためコーパスの全文書についてクエリの一意語集合を走査する
/// ため、処理コストはコーパス文書数とクエリの一意語数の積に比例して増える
/// （モジュールコメントの計算量を参照）。untrusted なクエリ文字列の語数を無制限に
/// 許すと、この積によって CPU コストを増幅させられる。1024 語は通常のキーワード
/// クエリを十分満たす一方、積を有界に保つための上限として妥当な値とする。
const MAX_QUERY_TERMS: usize = 1024;

/// [`SparseIndex::search`] が受け付けるクエリのバイト長の上限。
///
/// [`MAX_QUERY_TERMS`] は一意語数のみを制限するため、同じ語や区切り文字を大量に
/// 繰り返すクエリ（一意語数は少数のままバイト長だけが増大する入力）を防げない。
/// `tokenize()` はクエリ全体を小文字化して `Vec<char>` へ収集し各トークンごとに
/// `String` を確保するため、その繰り返しはバイト長に比例した CPU・メモリを消費する。
/// この検証は `query.len()`（バイト長）を `tokenize()` 呼び出し前に見るだけで済み、
/// 追加アロケーションなしで `O(1)` に判定できる。16 KiB は
/// `MAX_QUERY_TERMS`（1024 語）の一意語を平均的な単語長で表現するのに十分な余裕を
/// 持たせつつ（1 語あたり数バイト～十数バイト換算で 1024 語は数 KiB 程度に収まる）、
/// 繰り返し入力によるバイト数だけの増幅を有界に保つための上限とする。
const MAX_QUERY_BYTES: usize = 16 * 1024;

/// [`SparseIndex::search`]／[`SparseIndex::search_within`] が課すクエリ入力検証
/// （バイト長 [`MAX_QUERY_BYTES`]・一意語数 [`MAX_QUERY_TERMS`]）を、疎検索本体を
/// 呼ぶ前に単独で行う（`sql::using_plan::expanded_query_text` が組み立てる疎側
/// クエリ文字列〔`hybrid_search` の全文検索側入力〕を、密側の再埋め込み
/// （`Embedder::embed_batch`）前に検証するための呼び出し口。codex-review P1 指摘
/// 対応、PR #266）。
///
/// 上限値・判定順序（バイト長 → `tokenize()` → 一意語数）は [`SparseIndex::
/// search`]／[`SparseIndex::search_within`] 内の入力検証と同一の値・同一の
/// 判定条件を用いる（`MAX_QUERY_BYTES`・`MAX_QUERY_TERMS` の単一真実源を保つ）。
/// スコアリングに使う一意語集合の再構築コストを避けるため、`search`/
/// `search_within` 自体は本関数を経由せず従来どおり自前で検証する（多層防御。
/// 呼び出し元が異なるタイミング〔本関数は再埋め込み前、`search`系は疎検索
/// 実行直前〕で同じ契約を課すことが目的であり、実装の一本化ではない）。
pub(crate) fn validate_query_bounds(query: &str) -> Result<(), SparseError> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(SparseError::QueryTooLong {
            len: query.len(),
            max: MAX_QUERY_BYTES,
        });
    }

    let mut unique_terms: BTreeSet<String> = BTreeSet::new();
    for term in tokenize(query) {
        unique_terms.insert(term);
    }
    if unique_terms.len() > MAX_QUERY_TERMS {
        return Err(SparseError::TooManyQueryTerms {
            unique_terms: unique_terms.len(),
            max: MAX_QUERY_TERMS,
        });
    }
    Ok(())
}

/// [`SparseIndex::with_params`]（構築）が受け付ける 1 文書のバイト長の上限。
///
/// `with_params()` は各文書テキストに対して `tokenize()` を呼ぶため、
/// [`MAX_QUERY_BYTES`] と同じ理由（アロケーションを伴う走査コストがバイト長に
/// 比例する）で、文書 1 件のバイト長にも上限が必要になる。文書はクエリより
/// 大きな単位（チャンク・段落等）になりうるため、[`MAX_QUERY_BYTES`]（16 KiB）の
/// 64 倍にあたる 1 MiB を上限とする。
const MAX_DOC_BYTES: usize = 1024 * 1024;

/// [`SparseIndex::with_params`]（構築）が受け付けるコーパスの文書数の上限。
///
/// [`MAX_DOC_BYTES`] は 1 文書あたりのコストを有界にするが、文書数 `N` を
/// 無制限に許すとコーパス全体の処理コスト（総バイト数は最悪 `N * MAX_DOC_BYTES`
/// に達する）が無制限に増える。10 万件は単一の `SparseIndex` に載せるコーパス
/// として十分大きい一方、`N * MAX_DOC_BYTES` の積を有界に保つための上限として
/// 妥当な値とする。
pub(crate) const MAX_CORPUS_DOCS: usize = 100_000;

/// [`SparseIndex::with_params`]（構築）が受け付けるコーパス全体のバイト長合計の上限。
///
/// [`MAX_DOC_BYTES`]・[`MAX_CORPUS_DOCS`] は互いに独立な検証のため、両方の上限
/// ちょうど（1 MiB 文書 × 10 万件）を組み合わせると最大で約 100 GiB もの入力を
/// 許してしまい、単体では OOM を防げない。64 MiB は単一の `SparseIndex` に載せる
/// テキストコーパスの入力サイズとして十分実用的な規模を許容する上限とする。
///
/// ただしこのバイト長上限は入力テキストのサイズのみを制限し、`tokenize()` が
/// 生成するトークン数（＝ヒープオブジェクト・`BTreeMap` ノードの生成数）を直接
/// 制限しない。CJK 文字はユニグラム＋バイグラムを生成するため 1 文字（3 バイト）
/// あたり最大 2 トークンを生じ、バイト数から見積もれる最悪ケースのトークン数は
/// 大きい。実効的なヒープ割当量の上限は [`MAX_CORPUS_TOKENS`] が別途担う。
pub(crate) const MAX_CORPUS_BYTES: usize = 64 * 1024 * 1024;

// `MAX_CORPUS_BYTES` が `MAX_DOC_BYTES` の整数倍であることをコンパイル時に固定する。
// 境界値テストは MAX_DOC_BYTES サイズの文書を複数回参照して MAX_CORPUS_BYTES ちょうどの
// コーパスを組み立てるため（巨大な単一バッファの重複確保を避ける）、この関係が崩れると
// 境界値テストの前提が壊れる。
const _: () = assert!(MAX_CORPUS_BYTES.is_multiple_of(MAX_DOC_BYTES));

/// [`SparseIndex::with_params`]（構築）が受け付けるコーパス全体のトークン数合計の上限。
///
/// [`MAX_CORPUS_BYTES`] は入力テキストのバイト数のみを制限し、`tokenize()` が
/// 生成するトークン数を直接制限しない。CJK 文字はユニグラム＋バイグラムを生成する
/// ため 1 文字（3 バイト）あたり最大 2 トークンとなり、64 MiB の CJK 主体の入力から
/// 数千万規模のトークンが生じうる。各トークンは `term_freq`・`doc_freq` の双方の
/// `BTreeMap` キーとして個別にヒープ確保される（`String` の見出し用ヒープ確保
/// ×2／トークン、加えて `BTreeMap` ノードのオーバーヘッド）ため、トークン 1 件
/// あたり数百バイト程度の実効メモリを要すると見積もる。800 万トークンはこの見積もり
/// でも実行時メモリを概ね 1 GiB 程度の現実的な範囲に収めつつ、典型的な（CJK 一辺倒
/// ではない）コーパスであれば [`MAX_CORPUS_BYTES`] に近いサイズでも許容できる規模の
/// 上限とする。
const MAX_CORPUS_TOKENS: usize = 8_000_000;

/// 疎検索モジュールの公開エラー型。fail-closed 方針に従い、構築時の異常入力は
/// 曖昧に握りつぶさず `Err` として明示する（`.claude/rules/coding-rust.md`）。
///
/// `InvalidParams` が `f64` を保持するため `Eq` は導出しない（`PartialEq` のみ）。
#[derive(Debug, Clone, PartialEq)]
pub enum SparseError {
    /// コーパスが空で構築できない（Top-k 検索の対象が存在しないため）。
    EmptyCorpus,
    /// コーパス内に重複する `DocId` が存在する（統計の整合性が壊れるため拒否する）。
    DuplicateDocId(DocId),
    /// `k1`・`b` が不正（有限でない、または定義域外）。fail-open な空結果を返す
    /// 代わりに構築時点で拒否する（`.claude/rules/coding-rust.md`: エラー契約は
    /// fail-closed とする）。
    InvalidParams { k1: f64, b: f64 },
    /// クエリの一意語数が [`MAX_QUERY_TERMS`] を超える。長いクエリはコーパス文書数との
    /// 積で走査コストを増幅させるため（モジュールコメント参照）、`search()` の入口で
    /// fail-closed に拒否する（`.claude/rules/coding-rust.md`: untrusted 入力の長さは
    /// 上限検証してから処理する）。
    TooManyQueryTerms { unique_terms: usize, max: usize },
    /// クエリのバイト長が [`MAX_QUERY_BYTES`] を超える。`TooManyQueryTerms` は一意語数
    /// のみを制限するため、同じ語・区切り文字の繰り返しによるバイト長の増大は防げない
    /// （[`MAX_QUERY_BYTES`] のコメント参照）。`tokenize()` を呼ぶ前に `query.len()`
    /// （アロケーション不要）で判定し、`search()` の入口で fail-closed に拒否する。
    QueryTooLong { len: usize, max: usize },
    /// コーパス内の 1 文書のバイト長が [`MAX_DOC_BYTES`] を超える。`with_params()` は
    /// 各文書に対して `tokenize()` を呼ぶため、`QueryTooLong` と同じ理由で
    /// `tokenize()` を呼ぶ前に `text.len()`（アロケーション不要）で判定し、
    /// fail-closed に拒否する。
    DocTooLong {
        doc_id: DocId,
        len: usize,
        max: usize,
    },
    /// コーパスの文書数が [`MAX_CORPUS_DOCS`] を超える。`with_params()` の入口で
    /// （各文書の走査に入る前に）`docs.len()`（アロケーション不要）で判定し、
    /// fail-closed に拒否する。
    TooManyDocs { len: usize, max: usize },
    /// コーパス全体のバイト長合計が [`MAX_CORPUS_BYTES`] を超える。`MAX_DOC_BYTES`・
    /// `MAX_CORPUS_DOCS` は互いに独立な検証のため、両方の上限ちょうどの組み合わせでも
    /// 巨大な入力を許してしまう（[`MAX_CORPUS_BYTES`] のコメント参照）。`with_params()`
    /// は各文書の `tokenize()` を呼ぶ前に、それまでの文書バイト長の累計を
    /// `saturating_add` で求めて判定し、超過した時点で fail-closed に拒否する
    /// （オーバーフロー時は `total` を `usize::MAX` として報告し、必ず拒否する）。
    CorpusTooLarge { total: usize, max: usize },
    /// コーパス全体のトークン数合計が [`MAX_CORPUS_TOKENS`] を超える。`CorpusTooLarge`
    /// は入力テキストのバイト長のみを制限するため、CJK 主体の入力（ユニグラム＋
    /// バイグラムで 1 文字あたり最大 2 トークンを生じる）ではトークン数・ヒープ確保数
    /// がバイト長に比例しない（[`MAX_CORPUS_TOKENS`] のコメント参照）。`with_params()`
    /// は各文書を `tokenize()` した直後（`term_freq`・`doc_freq` を構築する前）に、
    /// それまでのトークン数の累計を `saturating_add` で求めて判定し、超過した時点で
    /// fail-closed に拒否する。
    TooManyTokens { total: usize, max: usize },
}

impl std::fmt::Display for SparseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SparseError::EmptyCorpus => write!(f, "sparse index corpus must not be empty"),
            SparseError::DuplicateDocId(id) => {
                write!(f, "duplicate doc id in corpus: {id}")
            }
            SparseError::InvalidParams { k1, b } => {
                write!(f, "invalid BM25 params: k1={k1}, b={b} (require k1 finite & >= 0.0, b finite & in [0.0, 1.0])")
            }
            SparseError::TooManyQueryTerms { unique_terms, max } => {
                write!(f, "too many unique query terms: {unique_terms} (max {max})")
            }
            SparseError::QueryTooLong { len, max } => {
                write!(f, "query too long: {len} bytes (max {max})")
            }
            SparseError::DocTooLong { doc_id, len, max } => {
                write!(
                    f,
                    "document too long: doc_id={doc_id}, {len} bytes (max {max})"
                )
            }
            SparseError::TooManyDocs { len, max } => {
                write!(f, "too many documents in corpus: {len} (max {max})")
            }
            SparseError::CorpusTooLarge { total, max } => {
                write!(f, "corpus too large: {total} bytes (max {max})")
            }
            SparseError::TooManyTokens { total, max } => {
                write!(f, "too many tokens in corpus: {total} (max {max})")
            }
        }
    }
}

impl std::error::Error for SparseError {}

/// Top-k 検索結果 1 件（文書 ID と BM25 スコア）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredDoc {
    /// スコア対象の文書 ID（呼び出し側が [`SparseIndex::build`]/`with_params` に渡した ID）。
    pub doc_id: DocId,
    /// BM25 スコア（降順ソート済み。同点は `doc_id` 昇順でタイブレークする）。
    pub score: f64,
}

/// [`SparseIndex::search`] が Top-k 選出中に走査する 1 件の候補。
///
/// `Ord` は最終的な検索結果の順序契約（スコア降順、同点は `doc_id` 昇順）で
/// 「大きい方が良い」を表すよう実装する。`BinaryHeap<Reverse<Candidate>>` に載せることで
/// 現在の k 件中「最も悪い」候補を `O(log k)` で特定・入れ替えでき、スコア計算済みの
/// 候補（`M` 件、`M` はスコア `> 0` の一致文書数）から Top-k を選ぶ部分を
/// `O(M log k)` 時間・`O(k)` 追加メモリで完結させる（全一致候補を `Vec` に蓄積してから
/// 全体ソートする場合の `O(M log M)` 時間・`O(M)` 追加メモリより小さい。BM25 スコア
/// 計算自体の計算量は [`SparseIndex::search`] のドキュメントを参照）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    score: f64,
    doc_id: DocId,
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // スコアは `total_cmp` で全順序化する（NaN を含めても panic しない。スコアは
        // 有限値のみを積むため NaN は理論上生じない）。同点時は `doc_id` が小さいほど
        // 「良い」（`Ordering::Greater`）とするため、比較対象を入れ替えて逆順にする。
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.doc_id.cmp(&self.doc_id))
    }
}

/// 小文字化した 1 文字が ASCII 単語トークンの構成要素かどうか。
fn is_ascii_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 小文字化した 1 文字が CJK（ひらがな・カタカナ・CJK 統合漢字）かどうか。
/// `char` の範囲判定のみで実装し、外部の正規表現・Unicode データクレートに依存しない
/// （`.claude/rules/dependency-policy.md`: 依存最小方針）。カタカナのレンジ
/// （U+30A0..=U+30FF）のうち中黒 `・`（U+30FB）は記号であり単語区切りとして扱うため
/// このレンジから除外する（TASK-105・SEARCH-5 対応）。長音符 `ー`（U+30FC）は
/// カタカナ語（例: 「サーバー」）の構成要素であり除外すると内容語が壊れるため、
/// 引き続き CJK 扱いのまま保持する。
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}' // ひらがな
        | '\u{30A0}'..='\u{30FA}' // カタカナ（中黒 U+30FB の手前まで）
        | '\u{30FC}'..='\u{30FF}' // カタカナ（長音符 U+30FC 以降。中黒のみ除外）
        | '\u{4E00}'..='\u{9FFF}' // CJK 統合漢字
    )
}

/// [`tokenize_with_options`] がユニグラムの単独トークン化を抑制する CJK 助詞・
/// 形式的機能語の 1 文字集合（TASK-105・対象ビヘイビア SEARCH-5。判断理由は
/// spec 側の定義を参照）。文字一致のみで判定するため、この 1 文字自体が内容語で
/// あるケースは区別できず除去対象になる（既知の制約。回避手段は
/// [`tokenize_with_options`] を参照）。
const CJK_STOPWORD_UNIGRAMS: [char; 10] =
    ['は', 'が', 'を', 'に', 'で', 'と', 'も', 'の', 'へ', 'や'];

/// 小文字化した 1 文字が [`CJK_STOPWORD_UNIGRAMS`] に含まれる助詞・機能語かどうか。
fn is_cjk_stopword_unigram(c: char) -> bool {
    CJK_STOPWORD_UNIGRAMS.contains(&c)
}

/// クエリ・文書テキストをトークン列へ分割する（TASK-102 の簡易トークナイザ、
/// CJK ストップワード除去は TASK-105・SEARCH-5 対応）。
///
/// [`tokenize_with_options`] を `remove_stopwords = true`（既定で除去 ON）で呼ぶ薄い
/// ラッパ。詳細な挙動・契約は [`tokenize_with_options`] を参照。
pub fn tokenize(text: &str) -> Vec<String> {
    tokenize_with_options(text, true)
}

/// [`tokenize`] の除去有無を選べる版（TASK-105・対象ビヘイビア SEARCH-5）。
///
/// 小文字化した上で、ASCII 英数字・アンダースコアの連続を単語トークンとし、CJK 文字は
/// 文字ユニグラム＋隣接 2 文字のバイグラムを生成する。`remove_stopwords = true` の場合、
/// [`CJK_STOPWORD_UNIGRAMS`] に含まれる文字がユニグラムとして単独出現した際にそのユニ
/// グラムだけを出力から除く（バイグラムは対象外）。除去有無で `SparseIndex::build`/
/// `search` の対称性は自動的に保たれる（`tokenize()` は常に除去 ON のため index/query
/// 間でトークナイザが一致する）。
///
/// 既知の制約: 除去は文字一致のみで判定するため、除去対象の 1 文字自体が内容語で
/// あるクエリ・文書は用法を区別できず該当文字が失われる。`SparseIndex` はこの除去を
/// 常に適用する（`tokenize()` 経由）ため回避できない。この挙動を避けたい呼び出し側は
/// `tokenize_with_options(text, false)` を直接呼ぶこと。除去有無によるランキング品質の
/// 比較測定は `crates/engine/tests/sparse_stopwords.rs` を参照。
///
/// 入力長に対して線形（`O(n)`）に処理し、`Vec` の初期容量は入力を `chars()` で数えた
/// 実際の文字数からのみ見積もる。トークンが 1 つも得られない入力（空文字列・記号のみ・
/// 助詞のみ等）は空の `Vec` を返す（呼び出し側はこれをエラーではなく空結果として扱う
/// 契約とする）。
///
/// 対応範囲は ASCII 単語文字と CJK（ひらがな・カタカナ・CJK 統合漢字）に限る。それ以外の
/// 文字（全角英数字・アクセント付きラテン文字・ハングル・半角カタカナ・中黒 `・`
/// U+30FB 等）は単語区切りとして無音で破棄され、ASCII 単語の途中に出現した場合は
/// その文字が欠落するだけでなく**その位置で単語が分割される**（前後が 1 トークンへ
/// 結合されるのではない。例: `"cafés"` → `["caf", "s"]`）。この分割で生じる偽トークンは
/// 統計（`term_freq`・`doc_freq`・`doc_len`）を汚染しうる。
///
/// 契約: この関数自体は `text` のバイト長・文字数に上限を設けない。呼び出し側
/// （[`SparseIndex::with_params`]・[`SparseIndex::search`]）が `tokenize()` を呼ぶ前に
/// それぞれの上限（`MAX_DOC_BYTES`・`MAX_QUERY_BYTES`）を検証する経路からのみ呼ばれる
/// ことを前提とする。本関数を直接 untrusted 入力に対して呼ぶ場合は、呼び出し側で
/// 同等の長さ検証を行うこと。
pub fn tokenize_with_options(text: &str, remove_stopwords: bool) -> Vec<String> {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    // ASCII 単語 + CJK ユニグラム/バイグラムを見積もった容量（過小でも Vec は伸長するため
    // 上限としての正確性は不要。検証済み長さの char 数のみを根拠にする）。
    let mut tokens: Vec<String> = Vec::with_capacity(lower.len());
    let mut word = String::new();

    let mut i = 0usize;
    while i < lower.len() {
        let c = lower[i];
        if is_ascii_word_char(c) {
            word.push(c);
            i += 1;
            continue;
        }
        if !word.is_empty() {
            tokens.push(std::mem::take(&mut word));
        }
        if is_cjk_char(c) {
            // ユニグラム（助詞の単独トークン化のみ抑制。バイグラムは除去しないため
            // 内容語の一部として現れる助詞文字は保持される）。
            if !(remove_stopwords && is_cjk_stopword_unigram(c)) {
                tokens.push(c.to_string());
            }
            // 直後の文字も CJK ならバイグラムを追加する（境界をまたぐ語の再現性向上）。
            if let Some(&next) = lower.get(i + 1) {
                if is_cjk_char(next) {
                    let mut bigram = String::with_capacity(c.len_utf8() + next.len_utf8());
                    bigram.push(c);
                    bigram.push(next);
                    tokens.push(bigram);
                }
            }
        }
        i += 1;
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    tokens
}

/// 1 文書分の統計（トークン頻度・文書長）。
#[derive(Debug)]
struct DocEntry {
    /// この統計が属する文書の ID（呼び出し側が割り当てた ID をそのまま保持する）。
    doc_id: DocId,
    /// トークン → 出現回数。イテレーション順が出力に影響しないよう `BTreeMap` を用いる
    /// （決定性確保。`HashMap` はイテレーション順が不定なため使わない）。
    term_freq: BTreeMap<String, u32>,
    /// この文書の総トークン数（`term_freq` の値の合計）。
    doc_len: u32,
}

/// BM25 Okapi による疎検索インデックス。
///
/// `build`/`with_params` で構築後は不変（再構築のみで更新する）。storage/catalog との
/// 結線は行わず、コーパスは呼び出し側が渡した `(DocId, &str)` のスライスから構築する。
#[derive(Debug)]
pub struct SparseIndex {
    /// BM25 の項飽和度パラメータ（構築時に有限値かつ `>= 0.0` を検証済み）。
    k1: f64,
    /// BM25 の文書長正規化パラメータ（構築時に有限値かつ `[0.0, 1.0]` を検証済み）。
    b: f64,
    /// コーパス内の文書数（`N`）。
    doc_count: u32,
    /// 平均文書長（`avgdl`）。
    avg_doc_len: f64,
    /// 文書ごとの統計（構築時の入力順を保持。`search()` はこの順に線形走査する）。
    docs: Vec<DocEntry>,
    /// トークン → 出現文書数（`df`）。`BTreeMap` で決定的な走査順を保つ。
    doc_freq: BTreeMap<String, u32>,
    /// `DocId` → `docs` 内のインデックス。[`SparseIndex::search_within`] が
    /// `visible_ids`（呼び出し元の可視集合）から該当文書へ `O(log n)` で辿るために使う
    /// （Issue #36 codex-review P0 指摘対応。`docs` は構築時の入力順であり `doc_id` で
    /// ソートされていないため、この逆引きマップなしでは可視集合ごとに線形走査が
    /// 必要になる）。値は構築時に自分自身が割り当てた `docs` のインデックスのみを
    /// 保持するため、`search_within` からの参照は常に範囲内である。
    id_index: BTreeMap<DocId, usize>,
}

impl SparseIndex {
    /// Okapi BM25 の既定パラメータ（`k1 = 1.2`, `b = 0.75`）でインデックスを構築する。
    pub fn build(docs: &[(DocId, &str)]) -> Result<Self, SparseError> {
        Self::with_params(docs, DEFAULT_K1, DEFAULT_B)
    }

    /// `k1`・`b` を明示してインデックスを構築する。
    ///
    /// `k1`・`b` は構築時に検証する（有限値かつ `k1 >= 0.0`・`b` は `[0.0, 1.0]`）。
    /// 不正値は `search()` 内で NaN 伝播・ガード節（`if score > 0.0` 等）により
    /// サイレントな空結果へ落ちてしまい fail-open になるため、ここで拒否して
    /// fail-closed を保つ（`.claude/rules/coding-rust.md`）。
    ///
    /// `docs` はコーパス全体のサイズを制限する 4 段の上限検証を行う
    /// （`.claude/rules/coding-rust.md`: untrusted 入力の長さは上限検証してから
    /// 処理する）。文書数が [`MAX_CORPUS_DOCS`] を超える場合は走査に入る前に
    /// [`SparseError::TooManyDocs`] で拒否し、各文書のバイト長が [`MAX_DOC_BYTES`]
    /// を超える場合は該当文書の `tokenize()`（アロケーションを伴う）を呼ぶ前に
    /// [`SparseError::DocTooLong`] で拒否する。[`MAX_DOC_BYTES`]・[`MAX_CORPUS_DOCS`]
    /// は互いに独立な検証のため、それらの組み合わせだけでは総入力サイズを有界に
    /// 保てない（[`MAX_CORPUS_BYTES`] のコメント参照）。そのため、これまでに走査した
    /// 文書のバイト長の累計を `saturating_add` で求め、[`MAX_CORPUS_BYTES`] を超えた時点で
    /// 該当文書の `tokenize()` を呼ぶ前に [`SparseError::CorpusTooLarge`] で拒否する。
    /// さらに、バイト長上限は `tokenize()` が生成するトークン数（CJK 入力では
    /// バイト長に比例しない。[`MAX_CORPUS_TOKENS`] のコメント参照）を直接制限しない
    /// ため、該当文書の `tokenize()` を呼んだ直後（`term_freq`・`doc_freq` を構築する
    /// 前）に、それまでのトークン数の累計を `saturating_add` で求め、
    /// [`MAX_CORPUS_TOKENS`] を超えた時点で [`SparseError::TooManyTokens`] で拒否する。
    pub fn with_params(docs: &[(DocId, &str)], k1: f64, b: f64) -> Result<Self, SparseError> {
        if !k1.is_finite() || k1 < 0.0 || !b.is_finite() || !(0.0..=1.0).contains(&b) {
            return Err(SparseError::InvalidParams { k1, b });
        }
        if docs.is_empty() {
            return Err(SparseError::EmptyCorpus);
        }
        if docs.len() > MAX_CORPUS_DOCS {
            return Err(SparseError::TooManyDocs {
                len: docs.len(),
                max: MAX_CORPUS_DOCS,
            });
        }

        let mut seen_ids: BTreeMap<DocId, ()> = BTreeMap::new();
        let mut entries: Vec<DocEntry> = Vec::with_capacity(docs.len());
        let mut id_index: BTreeMap<DocId, usize> = BTreeMap::new();
        let mut doc_freq: BTreeMap<String, u32> = BTreeMap::new();
        let mut total_len: u64 = 0;
        // コーパス全体のバイト長累計。`saturating_add` によりオーバーフロー時は
        // `usize::MAX` へ飽和させる（この桁数の入力は現実的に想定しないが、
        // fail-closed のため未定義動作にせず必ず MAX_CORPUS_BYTES 超過として扱う）。
        let mut corpus_bytes_seen: usize = 0;
        // コーパス全体のトークン数累計（同様に `saturating_add` で飽和させる）。
        // バイト長の累計だけでは CJK 主体の入力によるトークン数・ヒープ確保数の
        // 増幅を捕捉できないため、こちらは各文書の `tokenize()` 直後に加算する
        // （MAX_CORPUS_TOKENS のコメント参照）。
        let mut corpus_tokens_seen: usize = 0;

        for &(doc_id, text) in docs {
            if seen_ids.insert(doc_id, ()).is_some() {
                return Err(SparseError::DuplicateDocId(doc_id));
            }
            if text.len() > MAX_DOC_BYTES {
                return Err(SparseError::DocTooLong {
                    doc_id,
                    len: text.len(),
                    max: MAX_DOC_BYTES,
                });
            }
            corpus_bytes_seen = corpus_bytes_seen.saturating_add(text.len());
            if corpus_bytes_seen > MAX_CORPUS_BYTES {
                return Err(SparseError::CorpusTooLarge {
                    total: corpus_bytes_seen,
                    max: MAX_CORPUS_BYTES,
                });
            }

            let doc_tokens = tokenize(text);
            // `term_freq`/`doc_freq` の構築（トークンごとの追加ヒープ確保）に入る前に、
            // ここまでのトークン数累計を検証する（MAX_CORPUS_TOKENS のコメント参照）。
            corpus_tokens_seen = corpus_tokens_seen.saturating_add(doc_tokens.len());
            if corpus_tokens_seen > MAX_CORPUS_TOKENS {
                return Err(SparseError::TooManyTokens {
                    total: corpus_tokens_seen,
                    max: MAX_CORPUS_TOKENS,
                });
            }

            let mut term_freq: BTreeMap<String, u32> = BTreeMap::new();
            for tok in &doc_tokens {
                let counter = term_freq.entry(tok.clone()).or_insert(0u32);
                *counter = counter.saturating_add(1);
            }

            // `doc_tokens.len()` が `u32::MAX` を超える場合は `u32::MAX` に飽和させる
            // （文書長の理論上限を意図的に切り詰め、オーバーフローを未定義動作にしない）。
            // 実運用でこの桁数の単一文書は想定しないが、fail-closed のため checked に倒す。
            let doc_len = u32::try_from(doc_tokens.len()).unwrap_or(u32::MAX);
            total_len = total_len.saturating_add(u64::from(doc_len));

            for term in term_freq.keys() {
                let counter = doc_freq.entry(term.clone()).or_insert(0u32);
                *counter = counter.saturating_add(1);
            }

            // `id_index` は自分がこれから push する要素自身のインデックスのみを記録する
            // ため、値は常に `entries` の範囲内になる（範囲外を指す不変条件違反は
            // 構造的に起こり得ない）。
            id_index.insert(doc_id, entries.len());
            entries.push(DocEntry {
                doc_id,
                term_freq,
                doc_len,
            });
        }

        // `entries.len() == docs.len()` であり、`docs.is_empty()` は関数冒頭で拒否済みの
        // ため、ここでの `doc_count` は必ず 1 以上（0 除算にはならない）。
        let doc_count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        let avg_doc_len = total_len as f64 / f64::from(doc_count);

        Ok(SparseIndex {
            k1,
            b,
            doc_count,
            avg_doc_len,
            docs: entries,
            doc_freq,
            id_index,
        })
    }

    /// クエリに対する BM25 スコア降順の Top-k を返す。
    ///
    /// クエリからトークンが 1 つも得られない場合（空文字列・記号のみ等）は空の `Vec` を
    /// 返す仕様とする（エラーにはしない。決定的な空結果への fail-closed 寄りの挙動）。
    /// `k == 0` の場合、または `k` がコーパスの文書数を上回る場合もそれぞれ空/全件で
    /// 決定的に扱う。スコア同点時は `doc_id` 昇順でタイブレークする（再現性確保）。
    ///
    /// 契約: 入力検証（クエリのバイト長・一意語数の上限検証）は `k == 0` の早期 return
    /// より常に優先される。すなわち `k == 0` であっても、クエリが [`MAX_QUERY_BYTES`]・
    /// [`MAX_QUERY_TERMS`] を超えていれば空の `Vec` ではなく `Err` を返す（fail-closed の
    /// 原則を優先し、入力検証の可否が `k` の値に依存しない統一された契約とする）。
    /// 実装上もバイト長検証 → `tokenize()` → 一意語数検証の全経路を終えてから
    /// `k == 0` を判定する順序を維持する。
    ///
    /// 計算量: コーパスの全文書（`N` 件）についてクエリの一意語集合（`Q` 語）を
    /// `BTreeMap`（語彙数 `V`）で検索するため `O(N * Q * log V)`、その上で
    /// スコア `> 0` の一致文書（`M` 件）から Top-k をヒープ選出するため
    /// `O(M log k)` を要する（合計 `O(N * Q * log V + M log k)`）。クエリの一意語数
    /// `Q` が大きいほどコーパス全体との積で処理コストが増幅するため、`Q` が
    /// [`MAX_QUERY_TERMS`] を超える場合は走査に入る前に
    /// [`SparseError::TooManyQueryTerms`] で拒否する。また `tokenize()` はクエリ全体を
    /// 走査してアロケーションを行うため、一意語数の少ない繰り返し入力（同じ語や区切り
    /// 文字の反復）でもバイト長に比例したコストがかかる。そのためクエリのバイト長が
    /// [`MAX_QUERY_BYTES`] を超える場合は、`tokenize()` を呼ぶ前に
    /// [`SparseError::QueryTooLong`] で拒否する（両者とも fail-closed。
    /// `.claude/rules/coding-rust.md`: untrusted 入力の長さは上限検証してから処理する）。
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<ScoredDoc>, SparseError> {
        // アロケーションを伴う `tokenize()` を呼ぶ前に、バイト長（`str::len()`。文字列を
        // 走査しない `O(1)` 操作）で untrusted なクエリを検証する。一意語数の検証
        // （`MAX_QUERY_TERMS`）だけでは、同じ語・区切り文字を大量に繰り返す入力の
        // バイト長そのものの増大を防げないため、この検証が必要になる。
        if query.len() > MAX_QUERY_BYTES {
            return Err(SparseError::QueryTooLong {
                len: query.len(),
                max: MAX_QUERY_BYTES,
            });
        }

        let query_terms = tokenize(query);

        // 重複クエリ語の IDF を二重計上しないよう一意化する（順序は不問。BTreeMap で決定的に）。
        // `k == 0` の早期 return より前にここまで（バイト長・一意語数）の入力検証を
        // すべて終える。入力検証の可否が `k` の値に依存しない統一された契約とするため
        // （doc コメントの契約を参照）。
        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in &query_terms {
            unique_terms.insert(t.clone(), ());
        }

        if unique_terms.len() > MAX_QUERY_TERMS {
            return Err(SparseError::TooManyQueryTerms {
                unique_terms: unique_terms.len(),
                max: MAX_QUERY_TERMS,
            });
        }

        // 入力検証をすべて終えた後で、決定的な空結果ケース（`k == 0`・クエリがトークン
        // を 1 つも含まない）を処理する。
        if k == 0 || query_terms.is_empty() {
            return Ok(Vec::new());
        }

        // 現在の Top-k 候補を保持する固定サイズ（最大 k 件）のヒープ。`Reverse` により
        // `Candidate` の自然順序（大きい方が良い）に対する min-heap として働くため、
        // `heap.peek()` は常に「保持中で最も悪い」候補を指す。悪い候補から順に入れ替える
        // ことで、スコア計算済みの候補から Top-k を選ぶ部分を全件バッファリング＋全体
        // ソートではなく `O(M log k)` に抑える（`M`: スコア `> 0` の一致文書数）。
        let heap_capacity = k.min(self.docs.len());
        let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(heap_capacity);
        for doc in &self.docs {
            let mut score = 0.0f64;
            for term in unique_terms.keys() {
                let Some(&f) = doc.term_freq.get(term) else {
                    continue;
                };
                // `term` はこの `doc` の `term_freq` に存在する（`f` を得た時点の前提）ため、
                // build() 側で同じ `term` が必ず `doc_freq` にも計上済みであり、`df >= 1` が
                // 保証される。`unwrap_or(&0)` は `doc_freq` の型契約を壊さないための防御的
                // フォールバックであり、この呼び出し経路では実質到達しない。
                let df = *self.doc_freq.get(term).unwrap_or(&0);
                let idf = self.idf(df);
                let numerator = f64::from(f) * (self.k1 + 1.0);
                let len_norm = 1.0 - self.b
                    + self.b * (f64::from(doc.doc_len) / self.avg_doc_len.max(f64::MIN_POSITIVE));
                let denominator = f64::from(f) + self.k1 * len_norm;
                // `f >= 1`（`term_freq` にヒットした時点で出現回数は 1 以上）かつ `k1 >= 0`・
                // `len_norm >= 0`（`b` は `[0.0, 1.0]` に検証済みのため）であり、
                // `denominator = f + k1 * len_norm >= f >= 1.0` となる。よってこのガードは
                // 実質到達しない（0 以下になる入力の組み合わせは構築時に拒否済み）。
                if denominator > 0.0 {
                    score += idf * (numerator / denominator);
                }
            }
            if score > 0.0 {
                let candidate = Candidate {
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

        // ヒープの走査順は決定的な出力順を保証しないため、最終結果はスコア降順・
        // 同点は doc_id 昇順で明示的にソートし直す（決定的タイブレーク。再現性確保）。
        let mut scored: Vec<ScoredDoc> = heap
            .into_iter()
            .map(|Reverse(Candidate { score, doc_id })| ScoredDoc { doc_id, score })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id)));
        Ok(scored)
    }

    /// `idf(t) = ln( (N - df + 0.5) / (df + 0.5) + 1 )`。`+ 1` により常に非負となるため、
    /// 負の IDF に対する特別な補正処理は不要（モジュールコメントの式を参照）。
    /// インデックス全体（`self.doc_count`）を母数とする。可視集合へ縮約した母数を使う
    /// 場合は [`Self::idf_for`] を直接呼ぶ（[`Self::search_within`] 参照）。
    fn idf(&self, df: u32) -> f64 {
        Self::idf_for(f64::from(self.doc_count), df)
    }

    /// [`Self::idf`] の母数 `n`（文書数）を外部から指定できる版。`search()` は
    /// インデックス全体の `self.doc_count` を、`search_within()` は可視集合に縮約した
    /// 部分集合の文書数を、それぞれ `n` として渡すことで同一の IDF 計算式を共有する。
    fn idf_for(n: f64, df: u32) -> f64 {
        let df = f64::from(df);
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }

    /// [`Self::search`] の可視性縮約版（Issue #36 codex-review P0 指摘対応）。
    ///
    /// `search()` は文書数（N）・逆文書頻度（df）をインデックス全体（構築時の
    /// コーパス全体）から計算するため、呼び出し元が結果を `visible_ids` で事後
    /// フィルタするだけでは (a) `visible_ids` 外の文書が Top-k のヒープ選出枠
    /// （`k` 件のプール）を占有して可視文書を押し出す、(b) `doc_count`/`doc_freq`
    /// を通じて可視集合外の文書の内容・存在が可視文書の IDF・順位へ影響する、
    /// という 2 つの経路でテナント境界（RLS 相当）を弱める。件数・順位の違いから
    /// 他テナントの存在情報を推測できてしまうため、事後フィルタでは防げない
    /// （統計計算・候補選出そのものを可視集合へ限定する必要がある）。
    ///
    /// 本メソッドは統計（文書数・平均文書長・df）と Top-k のヒープ選出の両方を
    /// `visible_ids` に含まれる文書だけへ限定して計算し直す。`visible_ids` に
    /// 含まれるがインデックス構築時のコーパスに存在しない id は無視する
    /// （該当文書が単に存在しないものとして扱うだけであり、可視性判定そのものを
    /// 緩めるものではない。可視性判定自体は `visible_ids` を渡す呼び出し元
    /// （`hybrid.rs::hybrid_search`）の責務のまま変わらない）。
    ///
    /// クエリのバイト長・一意語数検証、`k == 0`・無トークンクエリでの空結果、
    /// 同点 `doc_id` 昇順のタイブレークといった契約は [`Self::search`] と同一。
    pub fn search_within(
        &self,
        query: &str,
        k: usize,
        visible_ids: &BTreeSet<DocId>,
    ) -> Result<Vec<ScoredDoc>, SparseError> {
        // 入力検証（クエリのバイト長・一意語数上限）は `search()` と同一の順序・契約を
        // 維持する（`k == 0`・空可視集合より常に優先する）。
        if query.len() > MAX_QUERY_BYTES {
            return Err(SparseError::QueryTooLong {
                len: query.len(),
                max: MAX_QUERY_BYTES,
            });
        }

        let query_terms = tokenize(query);

        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in &query_terms {
            unique_terms.insert(t.clone(), ());
        }

        if unique_terms.len() > MAX_QUERY_TERMS {
            return Err(SparseError::TooManyQueryTerms {
                unique_terms: unique_terms.len(),
                max: MAX_QUERY_TERMS,
            });
        }

        if k == 0 || query_terms.is_empty() || visible_ids.is_empty() {
            return Ok(Vec::new());
        }

        // 可視文書のみへ縮約した部分集合。`id_index` は構築時に自分自身の `docs`
        // インデックスのみを記録しているため範囲外を指すことはないが、`.get()` で
        // 明示的に防御し、万一の不整合時は当該文書を候補から除外する
        // （fail-closed。範囲外アクセスで panic させない）。
        let subset: Vec<&DocEntry> = visible_ids
            .iter()
            .filter_map(|id| self.id_index.get(id))
            .filter_map(|&idx| self.docs.get(idx))
            .collect();

        if subset.is_empty() {
            return Ok(Vec::new());
        }

        // 統計（N・avgdl・df）は縮約後の部分集合のみから計算する。インデックス全体の
        // `self.doc_count`/`self.avg_doc_len`/`self.doc_freq`（可視集合外の文書を含む
        // 統計）は一切参照しない。これにより不可視文書の内容・存在が可視文書の
        // スコア・順位へ影響する経路（本メソッド追加の動機（b））を断つ。
        let local_n = subset.len();
        let local_total_len: u64 = subset.iter().map(|d| u64::from(d.doc_len)).sum();
        let local_avg_doc_len = local_total_len as f64 / local_n as f64;

        // 可視部分集合内での df（クエリ語ごとに何件の可視文書が含むか）を数え直す
        // （インデックス全体の `self.doc_freq` は使わない）。
        let mut local_doc_freq: BTreeMap<&str, u32> = BTreeMap::new();
        for term in unique_terms.keys() {
            let df = subset
                .iter()
                .filter(|d| d.term_freq.contains_key(term))
                .count();
            // `df <= local_n <= self.docs.len() <= MAX_CORPUS_DOCS` であり `u32` へ
            // 安全に収まるが、将来の変更に備え `try_from` 失敗時は飽和させる
            // （coding-rust.md: 整数演算は checked/saturating を用いる）。
            local_doc_freq.insert(term.as_str(), u32::try_from(df).unwrap_or(u32::MAX));
        }

        // 選出ロジック（ヒープ・タイブレーク）は `search()` と同一だが、母数を
        // 可視部分集合（`local_n`・`local_avg_doc_len`・`local_doc_freq`）に限定する
        // ことで、`visible_ids` 外の文書が Top-k のプールを占有できないようにする
        // （本メソッド追加の動機（a））。
        let heap_capacity = k.min(local_n);
        let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(heap_capacity);
        for doc in &subset {
            let mut score = 0.0f64;
            for term in unique_terms.keys() {
                let Some(&f) = doc.term_freq.get(term) else {
                    continue;
                };
                let df = *local_doc_freq.get(term.as_str()).unwrap_or(&0);
                let idf = Self::idf_for(local_n as f64, df);
                let numerator = f64::from(f) * (self.k1 + 1.0);
                let len_norm = 1.0 - self.b
                    + self.b * (f64::from(doc.doc_len) / local_avg_doc_len.max(f64::MIN_POSITIVE));
                let denominator = f64::from(f) + self.k1 * len_norm;
                if denominator > 0.0 {
                    score += idf * (numerator / denominator);
                }
            }
            if score > 0.0 {
                let candidate = Candidate {
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
            .map(|Reverse(Candidate { score, doc_id })| ScoredDoc { doc_id, score })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id)));
        Ok(scored)
    }

    /// `BTreeMap`/`Vec` の 1 エントリあたりのアロケータ管理領域・ノードポインタ分の
    /// 概算オーバーヘッド（Issue #357 レビュー指摘対応・codex-review P1:
    /// [`Self::approx_heap_bytes`] が文字列長・キー/値サイズしか加算せず
    /// `MAX_SPARSE_CACHE_TOTAL_BYTES` の DoS 防御を過少計上で回避できた問題への
    /// 対応。実際の `BTreeMap` B-tree ノードは複数エントリを 1 ブロックへまとめて
    /// 確保するため実測値はこれより小さくなり得るが、本関数はそもそも「厳密な
    /// メモリ計測ではなく DoS 対策のための粗い上限判定用」（下記コメント）の
    /// ため、実確保量を下回らないよう安全側に保守的な固定値を使う）。
    const BTREE_ENTRY_OVERHEAD_BYTES: usize = 48;

    /// この `SparseIndex` が保持するヒープ確保分の概算バイト量（Issue #357・
    /// `sql/sparse_cache.rs::SparseIndexCache` の容量判定用）。`dictionary.rs::
    /// Dictionary::approx_heap_bytes` と同じ「厳密なメモリ計測ではなく DoS 対策の
    /// ための粗い上限判定用」という位置づけの概算だが、Issue #357 レビュー指摘
    /// 対応（codex-review P1）により以下をすべて加算するよう見直した:
    /// `docs`（`Vec<DocEntry>` 自体の確保領域〔`capacity() * size_of::<DocEntry>()`〕
    /// ＋各文書の `term_freq` の `String` キー容量・`u32` 値・`BTreeMap` ノード
    /// オーバーヘッド分）・`doc_freq`（語彙 `String` キー容量・`u32` 値・ノード
    /// オーバーヘッド分）・`id_index`（`DocId` → `usize` のエントリ分＋ノード
    /// オーバーヘッド分）。`String` は `len()` ではなく `capacity()` を使う
    /// （成長により確保容量が長さを上回り得るため、下回らない側の概算にする）。
    pub fn approx_heap_bytes(&self) -> usize {
        let docs_container = self
            .docs
            .capacity()
            .saturating_mul(std::mem::size_of::<DocEntry>());
        let docs_term_freq: usize = self
            .docs
            .iter()
            .map(|d| {
                d.term_freq
                    .keys()
                    .map(|k| {
                        k.capacity()
                            .saturating_add(std::mem::size_of::<u32>())
                            .saturating_add(Self::BTREE_ENTRY_OVERHEAD_BYTES)
                    })
                    .fold(0usize, |acc, n| acc.saturating_add(n))
            })
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let docs = docs_container.saturating_add(docs_term_freq);
        let doc_freq: usize = self
            .doc_freq
            .keys()
            .map(|k| {
                k.capacity()
                    .saturating_add(std::mem::size_of::<u32>())
                    .saturating_add(Self::BTREE_ENTRY_OVERHEAD_BYTES)
            })
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let id_index: usize = self.id_index.len().saturating_mul(
            std::mem::size_of::<(DocId, usize)>().saturating_add(Self::BTREE_ENTRY_OVERHEAD_BYTES),
        );
        docs.saturating_add(doc_freq).saturating_add(id_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- トークナイザ（境界値含む） ---

    #[test]
    fn tokenize_ascii_words() {
        assert_eq!(tokenize("Hello World"), vec!["hello", "world"]);
    }

    #[test]
    fn tokenize_empty_string_yields_no_tokens() {
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn tokenize_punctuation_only_yields_no_tokens() {
        assert!(tokenize("!!! ??? ...").is_empty());
    }

    #[test]
    fn tokenize_cjk_yields_unigrams_and_bigrams() {
        // 「東京都」→ ユニグラム(東,京,都) + バイグラム(東京,京都)
        let toks = tokenize("東京都");
        assert_eq!(toks, vec!["東", "東京", "京", "京都", "都"]);
    }

    #[test]
    fn tokenize_mixed_ascii_and_cjk() {
        let toks = tokenize("vector検索");
        assert_eq!(toks, vec!["vector", "検", "検索", "索"]);
    }

    // --- 対応範囲外文字の無音破棄（現挙動の固定。モジュールコメント参照） ---

    #[test]
    fn tokenize_fullwidth_alnum_yields_no_tokens() {
        // 全角英数字は ASCII 単語文字でも CJK レンジでもないため、無音で破棄される。
        assert!(tokenize("ＶＥＣＴＯＲ").is_empty());
    }

    #[test]
    fn tokenize_accented_latin_is_silently_dropped() {
        // アクセント付きラテン文字（'é'）は対応範囲外のため欠落する。単語末尾のケースだけ
        // では「分割」と「結合」の挙動を区別できないため、区別可能なケースは
        // tokenize_accented_latin_mid_word_splits_the_word で pin する。
        assert_eq!(tokenize("café"), vec!["caf"]);
    }

    #[test]
    fn tokenize_accented_latin_mid_word_splits_the_word() {
        // アクセント付きラテン文字が単語の途中に出現すると、その文字が欠落するだけでなく
        // その位置で単語が分割される（前後の ASCII 部分が結合されるのではない）。
        assert_eq!(tokenize("cafés"), vec!["caf", "s"]);
    }

    #[test]
    fn tokenize_hangul_yields_no_tokens() {
        assert!(tokenize("안녕").is_empty());
    }

    #[test]
    fn tokenize_halfwidth_katakana_yields_no_tokens() {
        // 半角カタカナは `is_cjk_char` のレンジ（全角カタカナ U+30A0..=U+30FF）に含まれない。
        assert!(tokenize("ﾃｽﾄ").is_empty());
    }

    // --- CJK ストップワード除去（TASK-105・対象ビヘイビア SEARCH-5） ---

    #[test]
    fn tokenize_particle_unigram_is_removed_by_default() {
        // 「本の」→ ユニグラム(本, の) + バイグラム(本の) のうち、助詞「の」の
        // 単独ユニグラムのみが既定（除去 ON）で出力から除かれる。
        let toks = tokenize("本の");
        assert_eq!(toks, vec!["本", "本の"]);
        assert!(!toks.contains(&"の".to_string()));
    }

    #[test]
    fn tokenize_with_options_remove_stopwords_false_keeps_particle_unigram() {
        // 除去 OFF（`remove_stopwords = false`）では助詞ユニグラムも保持される
        // （除去有無の比較測定のための対称 API。関連: SEARCH-5）。
        let toks = tokenize_with_options("本の", false);
        assert_eq!(toks, vec!["本", "本の", "の"]);
    }

    #[test]
    fn tokenize_particle_inside_bigram_is_preserved() {
        // 助詞文字が内容語の内部（バイグラム）に現れる場合は除去しない
        // （例: 「もの」の「の」。CJK コンテンツ自体を保持する方針）。
        let toks = tokenize("もの");
        assert_eq!(toks, vec!["もの"]);
        // 「も」「の」は単独ユニグラムとして除去されるが、バイグラム「もの」は残る。
        assert!(toks.contains(&"もの".to_string()));
        assert!(!toks.contains(&"も".to_string()));
        assert!(!toks.contains(&"の".to_string()));
    }

    #[test]
    fn tokenize_nakaguro_is_treated_as_separator() {
        // 中黒「・」は記号として単語区切りに扱われ、トークンを生成しない
        // （TASK-105 対応で is_cjk_char のレンジから除外）。
        let toks = tokenize("東京・大阪");
        assert!(!toks.iter().any(|t| t.contains('・')));
        // 区切りの前後にある「東京」「大阪」はそれぞれ独立に CJK トークン化される
        // （中黒をまたぐバイグラムは生成されない）。
        assert!(toks.contains(&"東京".to_string()));
        assert!(toks.contains(&"大阪".to_string()));
        assert!(!toks.iter().any(|t| t.contains("京・")));
        assert!(!toks.iter().any(|t| t.contains("・大")));
    }

    #[test]
    fn tokenize_choonpu_is_preserved_as_cjk_content() {
        // 長音符「ー」はカタカナ語の構成要素のため CJK 扱いのまま保持する
        // （例: 「サーバー」。中黒のみを除外し長音符は除外しない）。
        let toks = tokenize("サーバー");
        assert!(toks.contains(&"サー".to_string()));
        assert!(toks.contains(&"ーバ".to_string()));
        assert!(toks.contains(&"バー".to_string()));
    }

    #[test]
    fn tokenize_cjk_punctuation_still_yields_no_tokens() {
        // 句読点「、」「。」は is_cjk_char のレンジ外のため、TASK-105 前後で挙動は
        // 変わらずトークン化されない（現状挙動の固定）。
        assert!(tokenize("、。").is_empty());
    }

    // --- SparseIndex 構築・境界値 ---

    #[test]
    fn build_rejects_empty_corpus() {
        let docs: Vec<(DocId, &str)> = vec![];
        assert_eq!(
            SparseIndex::build(&docs).unwrap_err(),
            SparseError::EmptyCorpus
        );
    }

    #[test]
    fn build_rejects_duplicate_doc_id() {
        let docs = vec![(1u64, "alpha"), (1u64, "beta")];
        assert_eq!(
            SparseIndex::build(&docs).unwrap_err(),
            SparseError::DuplicateDocId(1)
        );
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "gamma delta")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.search("", 10).unwrap().is_empty());
        assert!(idx.search("!!!", 10).unwrap().is_empty());
    }

    #[test]
    fn search_k_zero_returns_empty() {
        let docs = vec![(1u64, "alpha beta")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.search("alpha", 0).unwrap().is_empty());
    }

    #[test]
    fn search_k_larger_than_corpus_returns_all_matching() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "alpha gamma")];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 100).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_tie_breaks_by_doc_id_ascending() {
        // 2 文書が完全に同一内容（同スコア）になるようにする。
        let docs = vec![(2u64, "alpha beta"), (1u64, "alpha beta")];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert!((results[0].score - results[1].score).abs() < 1e-12);
        assert_eq!(results[0].doc_id, 1);
        assert_eq!(results[1].doc_id, 2);
    }

    #[test]
    fn search_selection_boundary_prefers_smaller_doc_id_on_tie() {
        // 4 文書すべてが同スコアになるようにし、k=2（コーパスの半分）で打ち切る。
        // ヒープによる Top-k 選出でも、同点の切り捨て境界で doc_id が小さい方を
        // 優先して残すこと（＝出力順のタイブレークだけでなく、選出自体の境界でも
        // 同じ優先順位が一貫して使われること）を確認する。
        let docs = vec![
            (30u64, "alpha beta"),
            (10u64, "alpha beta"),
            (40u64, "alpha beta"),
            (20u64, "alpha beta"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].doc_id, 10);
        assert_eq!(results[1].doc_id, 20);
    }

    #[test]
    fn search_keyword_match_ranks_above_non_match() {
        let docs = vec![
            (1u64, "the quick brown fox jumps over the lazy dog"),
            (2u64, "vector databases store embeddings for search"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("vector embeddings", 10).unwrap();
        assert_eq!(results[0].doc_id, 2);
    }

    #[test]
    fn search_is_deterministic_across_repeated_calls() {
        let docs = vec![
            (1u64, "alpha beta gamma"),
            (2u64, "beta gamma delta"),
            (3u64, "gamma delta epsilon"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let first = idx.search("gamma delta", 10).unwrap();
        let second = idx.search("gamma delta", 10).unwrap();
        assert_eq!(first, second);
    }

    // --- search_within（Issue #36 codex-review P0 指摘対応: テナント境界縮約） ---

    #[test]
    fn search_within_excludes_invisible_docs_from_pool_occupation() {
        // [P0] `search()`（インデックス全体を母数に Top-k を選出する API）を事後
        // フィルタするだけでは、不可視文書が Top-k のプールを占有して可視文書を
        // 押し出す経路を防げない。doc_id=3 は "cat" を大量に繰り返すため
        // `search()` では 1 位を独占するが、可視集合 `{1, 2}` には含まれない。
        let docs = vec![
            (1u64, "cat"),
            (2u64, "cat"),
            (3u64, "cat cat cat cat cat cat cat cat cat cat"),
        ];
        let idx = SparseIndex::build(&docs).expect("build ok");

        // 旧実装が脆弱だったことの回帰確認: `search()` は k=1 で不可視の id=3 だけを
        // 返す（呼び出し元がこれを可視集合で事後フィルタすると空になり、可視文書
        // id=1・id=2 のどちらも返せない）。
        let legacy = idx.search("cat", 1).expect("search ok");
        assert_eq!(legacy[0].doc_id, 3);

        // `search_within` は id=3 を候補にすら含めないため、k=1 でも可視文書
        // （id=1・id=2 は同点のため doc_id 昇順で id=1）が正しく返る。
        let visible: BTreeSet<u64> = [1u64, 2u64].into_iter().collect();
        let scoped = idx
            .search_within("cat", 1, &visible)
            .expect("search_within ok");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].doc_id, 1);
    }

    #[test]
    fn search_within_statistics_are_isolated_from_invisible_docs() {
        // [P0] 可視文書の IDF・avgdl（＝スコア）が、共有インデックスに含まれる
        // 不可視文書の有無・内容によって変化しないことを確認する（`doc_count`/
        // `doc_freq` を通じた統計汚染の防止）。同一の可視部分集合（id=1 のみが
        // "cat" を含む）に対し、共有インデックスへ大量の不可視文書（"cat" を含む
        // ものを含む）を追加しても `search_within` のスコアは不変であるべき。
        let small_docs = vec![(1u64, "cat"), (2u64, "dog")];
        let small_idx = SparseIndex::build(&small_docs).expect("build ok");
        let visible: BTreeSet<u64> = [1u64].into_iter().collect();
        let small_result = small_idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");

        let mut large_docs: Vec<(u64, &str)> = vec![(1u64, "cat"), (2u64, "dog")];
        let filler: Vec<(u64, &str)> = (100u64..150).map(|id| (id, "cat cat cat")).collect();
        large_docs.extend(filler.iter().copied());
        let large_idx = SparseIndex::build(&large_docs).expect("build ok");
        let large_result = large_idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");

        assert_eq!(
            small_result, large_result,
            "invisible corpus growth must not change visible-only score"
        );
    }

    #[test]
    fn search_within_ignores_visible_ids_absent_from_corpus() {
        // `visible_ids` に構築時のコーパスへ存在しない id が混ざっていても panic せず、
        // 単に無視されることを確認する（呼び出し元の可視集合が本インデックス外の id を
        // 含みうる契約。`hybrid.rs::hybrid_search` の呼び出し文脈を参照）。
        let docs = vec![(1u64, "cat"), (2u64, "dog")];
        let idx = SparseIndex::build(&docs).expect("build ok");
        let visible: BTreeSet<u64> = [1u64, 999u64].into_iter().collect();
        let result = idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].doc_id, 1);
    }

    #[test]
    fn search_within_empty_visible_ids_returns_empty() {
        let docs = vec![(1u64, "cat")];
        let idx = SparseIndex::build(&docs).expect("build ok");
        let visible: BTreeSet<u64> = BTreeSet::new();
        let result = idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");
        assert!(result.is_empty());
    }

    #[test]
    fn search_within_query_too_long_is_rejected_before_visible_ids_check() {
        // [Low 相当の一貫性確認] `search()` と同じ契約: クエリのバイト長検証は
        // 可視集合の中身（空かどうか等）より常に優先される。
        let docs = vec![(1u64, "cat")];
        let idx = SparseIndex::build(&docs).expect("build ok");
        let visible: BTreeSet<u64> = BTreeSet::new();
        let long_query = "a".repeat(17 * 1024);
        let err = idx.search_within(&long_query, 10, &visible).unwrap_err();
        assert!(matches!(err, SparseError::QueryTooLong { .. }));
    }

    #[test]
    fn build_with_symbol_only_document_included_in_avg_doc_len() {
        // 記号のみの文書（トークン化結果が空）が混在するコーパスでも構築・検索が
        // 破綻しないことを確認する（avg_doc_len のゼロ割ガードの回帰検出）。
        // doc2 のトークン数は 0 のため avgdl = (2 tokens + 0 tokens) / 2 docs = 1.0 となり、
        // ゼロ割ガード（`avg_doc_len.max(f64::MIN_POSITIVE)`）を経由せず素の平均が使われる。
        let docs = vec![(1u64, "alpha beta"), (2u64, "!!! ???")];
        let idx = SparseIndex::build(&docs).unwrap();

        let idf_alpha = ((2.0 - 1.0 + 0.5) / (1.0 + 0.5) + 1.0f64).ln();
        let k1 = 1.2f64;
        let b = 0.75f64;
        let f = 1.0f64;
        let len_norm = 1.0 - b + b * (2.0 / 1.0);
        let expected = idf_alpha * (f * (k1 + 1.0)) / (f + k1 * len_norm);

        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
        assert!((results[0].score - expected).abs() < 1e-9);
    }

    #[test]
    fn build_with_all_symbol_only_documents_does_not_panic() {
        // 全文書がトークン化結果ゼロ（記号のみ）のコーパスは EmptyCorpus ではなく正常に
        // 構築でき、`avg_doc_len` は 0 除算なく 0.0 になる。ただし全文書の `term_freq` が
        // 空のため、search() 内でどのクエリ語も `doc.term_freq.get(term)` にヒットせず、
        // avg_doc_len を参照する `len_norm` の計算（0 除算ガード `max(f64::MIN_POSITIVE)`
        // 含む）自体は実行されない。ここで確認するのは「avg_doc_len == 0.0 のまま
        // 構築・検索が NaN・panic なく決定的に空を返す」ことであり、search() 内のゼロ割
        // ガード行そのものの実行はカバーしない。
        let docs = vec![(1u64, "!!!"), (2u64, "???")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.search("alpha", 10).unwrap().is_empty());
    }

    #[test]
    fn search_cjk_query_matches_expected_document() {
        let docs = vec![
            (1u64, "東京都渋谷区のカフェ"),
            (2u64, "大阪府大阪市のレストラン"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("東京", 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].doc_id, 1);
    }

    // --- 手計算による BM25 期待値一致（数式レベルの一致を担保） ---

    #[test]
    fn idf_matches_hand_computed_formula() {
        // N=3, df("shared")=3（全文書に出現）→ idf = ln((3-3+0.5)/(3+0.5)+1)
        let docs = vec![(1u64, "shared"), (2u64, "shared"), (3u64, "shared")];
        let idx = SparseIndex::build(&docs).unwrap();
        let expected_idf = ((3.0 - 3.0 + 0.5) / (3.0 + 0.5) + 1.0f64).ln();
        // 単一トークン文書（doc_len == avgdl）なので長さ正規化項は 1 になり、
        // score = idf * (1 * (k1+1)) / (1 + k1) = idf。
        let results = idx.search("shared", 10).unwrap();
        for r in &results {
            assert!((r.score - expected_idf).abs() < 1e-9);
        }
    }

    #[test]
    fn score_matches_hand_computed_bm25_value() {
        // 単純な 2 文書コーパスで BM25 スコアを手計算し一致を確認する。
        // doc1: "alpha alpha beta" (len=3), doc2: "beta beta beta" (len=3)
        // avgdl = 3, N = 2
        let docs = vec![(1u64, "alpha alpha beta"), (2u64, "beta beta beta")];
        let idx = SparseIndex::build(&docs).unwrap();

        // query "alpha": df(alpha) = 1 → idf = ln((2-1+0.5)/(1+0.5)+1) = ln(2.0)
        let idf_alpha = ((2.0 - 1.0 + 0.5) / (1.0 + 0.5) + 1.0f64).ln();
        // doc1 での f(alpha, doc1) = 2, k1=1.2, b=0.75, doc_len=3, avgdl=3 → len_norm = 1
        let k1 = 1.2f64;
        let b = 0.75f64;
        let f = 2.0f64;
        let len_norm = 1.0 - b + b * (3.0 / 3.0);
        let expected = idf_alpha * (f * (k1 + 1.0)) / (f + k1 * len_norm);

        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
        assert!((results[0].score - expected).abs() < 1e-9);
    }

    #[test]
    fn score_matches_hand_computed_bm25_value_with_doc_len_ne_avgdl() {
        // doc_len != avgdl のケース（文書長正規化項 len_norm が 1 にキャンセルしない）で
        // 手計算値と一致することを確認する。b の符号反転・比率逆転・項の脱落等の回帰を
        // 検出するための境界値テスト。
        // doc1: "alpha alpha beta beta beta" (len=5)
        // doc2: "beta" (len=1)
        // avgdl = (5 + 1) / 2 = 3, N = 2
        let docs = vec![(1u64, "alpha alpha beta beta beta"), (2u64, "beta")];
        let idx = SparseIndex::build(&docs).unwrap();

        // query "alpha": df(alpha) = 1 → idf = ln((2-1+0.5)/(1+0.5)+1) = ln(2.0)
        let idf_alpha = ((2.0 - 1.0 + 0.5) / (1.0 + 0.5) + 1.0f64).ln();
        // doc1 での f(alpha, doc1) = 2, k1=1.2, b=0.75, doc_len=5, avgdl=3
        let k1 = 1.2f64;
        let b = 0.75f64;
        let f = 2.0f64;
        let len_norm = 1.0 - b + b * (5.0 / 3.0);
        let expected = idf_alpha * (f * (k1 + 1.0)) / (f + k1 * len_norm);

        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
        assert!((results[0].score - expected).abs() < 1e-9);
    }

    // --- パラメータ検証（fail-closed） ---

    #[test]
    fn with_params_rejects_nan_k1() {
        let docs = vec![(1u64, "alpha beta")];
        let err = SparseIndex::with_params(&docs, f64::NAN, 0.75).unwrap_err();
        assert!(matches!(err, SparseError::InvalidParams { .. }));
    }

    #[test]
    fn with_params_rejects_negative_k1() {
        let docs = vec![(1u64, "alpha beta")];
        let err = SparseIndex::with_params(&docs, -1.0, 0.75).unwrap_err();
        assert!(matches!(err, SparseError::InvalidParams { .. }));
    }

    #[test]
    fn with_params_rejects_b_out_of_unit_range() {
        let docs = vec![(1u64, "alpha beta")];
        assert!(matches!(
            SparseIndex::with_params(&docs, 1.2, 1.5).unwrap_err(),
            SparseError::InvalidParams { .. }
        ));
        assert!(matches!(
            SparseIndex::with_params(&docs, 1.2, -0.1).unwrap_err(),
            SparseError::InvalidParams { .. }
        ));
    }

    #[test]
    fn with_params_rejects_infinite_b() {
        let docs = vec![(1u64, "alpha beta")];
        let err = SparseIndex::with_params(&docs, 1.2, f64::INFINITY).unwrap_err();
        assert!(matches!(err, SparseError::InvalidParams { .. }));
    }

    #[test]
    fn with_params_accepts_boundary_b_values() {
        let docs = vec![(1u64, "alpha beta")];
        assert!(SparseIndex::with_params(&docs, 1.2, 0.0).is_ok());
        assert!(SparseIndex::with_params(&docs, 1.2, 1.0).is_ok());
        assert!(SparseIndex::with_params(&docs, 0.0, 0.75).is_ok());
    }

    // --- クエリの一意語数上限（fail-closed。MAX_QUERY_TERMS 境界） ---

    /// `n` 個の相異なる ASCII 単語トークン（`"w0" "w1" ... "w{n-1}"`）からなるクエリ文字列
    /// を組み立てる（`tokenize()` は ASCII 単語をそのまま 1 トークン化するため、
    /// 一意語数がちょうど `n` になる）。
    fn distinct_term_query(n: usize) -> String {
        (0..n)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn search_accepts_query_at_max_query_terms_boundary() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = distinct_term_query(MAX_QUERY_TERMS);
        // 上限ちょうどは拒否されない（一致文書がないため結果は空だが、Err にはならない）。
        assert!(idx.search(&query, 10).unwrap().is_empty());
    }

    #[test]
    fn search_rejects_query_exceeding_max_query_terms() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = distinct_term_query(MAX_QUERY_TERMS + 1);
        let err = idx.search(&query, 10).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyQueryTerms {
                unique_terms: MAX_QUERY_TERMS + 1,
                max: MAX_QUERY_TERMS,
            }
        );
    }

    // --- クエリのバイト長上限（fail-closed。MAX_QUERY_BYTES 境界） ---

    #[test]
    fn search_accepts_query_at_max_query_bytes_boundary() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        // ASCII 単語文字のみからなる 1 トークン（一意語数は 1 のため MAX_QUERY_TERMS
        // には抵触しない）で、バイト長だけを上限ちょうどにする。同じ語・区切り文字の
        // 反復で一意語数を増やさずにバイト長だけを膨らませる攻撃形を模した境界値。
        let query = "a".repeat(MAX_QUERY_BYTES);
        assert_eq!(query.len(), MAX_QUERY_BYTES);
        assert!(idx.search(&query, 10).unwrap().is_empty());
    }

    #[test]
    fn search_rejects_query_exceeding_max_query_bytes() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = "a".repeat(MAX_QUERY_BYTES + 1);
        let err = idx.search(&query, 10).unwrap_err();
        assert_eq!(
            err,
            SparseError::QueryTooLong {
                len: MAX_QUERY_BYTES + 1,
                max: MAX_QUERY_BYTES,
            }
        );
    }

    // --- `validate_query_bounds`（`search`/`search_within` 呼び出し前の単独検証。
    //     codex-review P1 指摘対応、PR #266） ---

    #[test]
    fn validate_query_bounds_accepts_query_within_limits() {
        assert!(validate_query_bounds("alpha beta").is_ok());
    }

    #[test]
    fn validate_query_bounds_rejects_query_exceeding_max_query_bytes() {
        let query = "a".repeat(MAX_QUERY_BYTES + 1);
        let err = validate_query_bounds(&query).unwrap_err();
        assert_eq!(
            err,
            SparseError::QueryTooLong {
                len: MAX_QUERY_BYTES + 1,
                max: MAX_QUERY_BYTES,
            }
        );
    }

    #[test]
    fn validate_query_bounds_rejects_query_exceeding_max_query_terms() {
        // `distinct_term_query` は本ファイル内の既存ヘルパー（一意語数上限テスト群で
        // 使用済み）。同じ生成規則を再利用し、値の食い違いを避ける。
        let query = distinct_term_query(MAX_QUERY_TERMS + 1);
        let err = validate_query_bounds(&query).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyQueryTerms {
                unique_terms: MAX_QUERY_TERMS + 1,
                max: MAX_QUERY_TERMS,
            }
        );
    }

    // --- 文書 1 件のバイト長上限（fail-closed。MAX_DOC_BYTES 境界） ---

    #[test]
    fn build_accepts_doc_at_max_doc_bytes_boundary() {
        let text = "a".repeat(MAX_DOC_BYTES);
        let docs = vec![(1u64, text.as_str())];
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_doc_exceeding_max_doc_bytes() {
        let text = "a".repeat(MAX_DOC_BYTES + 1);
        let docs = vec![(1u64, text.as_str())];
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::DocTooLong {
                doc_id: 1,
                len: MAX_DOC_BYTES + 1,
                max: MAX_DOC_BYTES,
            }
        );
    }

    // --- コーパス文書数上限（fail-closed。MAX_CORPUS_DOCS 境界） ---

    #[test]
    fn build_accepts_corpus_at_max_corpus_docs_boundary() {
        let docs: Vec<(DocId, &str)> = (0..MAX_CORPUS_DOCS as u64)
            .map(|id| (id, "alpha"))
            .collect();
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_corpus_exceeding_max_corpus_docs() {
        let docs: Vec<(DocId, &str)> = (0..(MAX_CORPUS_DOCS as u64 + 1))
            .map(|id| (id, "alpha"))
            .collect();
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyDocs {
                len: MAX_CORPUS_DOCS + 1,
                max: MAX_CORPUS_DOCS,
            }
        );
    }

    // --- コーパス全体のバイト長上限（fail-closed。MAX_CORPUS_BYTES 境界） ---

    #[test]
    fn build_accepts_corpus_at_max_corpus_bytes_boundary() {
        // `MAX_DOC_BYTES` サイズの単一バッファを `MAX_CORPUS_BYTES / MAX_DOC_BYTES` 回
        // 参照するコーパス（合計はちょうど `MAX_CORPUS_BYTES`）を組み立てる。文書ごとに
        // 個別のバッファを確保せず、1 つの `text` を使い回すことで無駄な重複確保を避ける。
        let text = "a".repeat(MAX_DOC_BYTES);
        let doc_count = MAX_CORPUS_BYTES / MAX_DOC_BYTES;
        let docs: Vec<(DocId, &str)> = (0..doc_count as u64)
            .map(|id| (id, text.as_str()))
            .collect();
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_corpus_exceeding_max_corpus_bytes() {
        let text = "a".repeat(MAX_DOC_BYTES);
        let doc_count = MAX_CORPUS_BYTES / MAX_DOC_BYTES;
        let mut docs: Vec<(DocId, &str)> = (0..doc_count as u64)
            .map(|id| (id, text.as_str()))
            .collect();
        // 1 バイトの文書を追加し、合計をちょうど MAX_CORPUS_BYTES + 1 にする。
        docs.push((doc_count as u64, "a"));
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::CorpusTooLarge {
                total: MAX_CORPUS_BYTES + 1,
                max: MAX_CORPUS_BYTES,
            }
        );
    }

    // --- コーパス全体のトークン数上限（fail-closed。MAX_CORPUS_TOKENS 境界） ---

    /// 合計トークン数がちょうど `target_tokens` になるコーパスを、CJK の同一文字を
    /// 繰り返した文書の組み合わせで構築する（token/byte 比を意図的に大きくし、
    /// `MAX_CORPUS_BYTES` の遥か手前で `MAX_CORPUS_TOKENS` に到達させるため）。
    /// `tokenize()` は同一 CJK 文字（3 バイト/文字）を N 回繰り返した入力から常に
    /// `2 * N - 1` 個のトークン（ユニグラム N 個＋隣接バイグラム `N - 1` 個）を生成する
    /// ため、この式を逆算して任意の奇数トークン数を持つ文書を組み立てられる。
    /// 1 文書あたりの繰り返し数は `MAX_DOC_BYTES` を超えないよう上限を設ける。
    fn cjk_corpus_with_token_count(target_tokens: usize) -> Vec<(DocId, String)> {
        let max_n_per_doc = MAX_DOC_BYTES / 3;
        let max_tokens_per_doc = 2 * max_n_per_doc - 1;

        let mut docs: Vec<(DocId, String)> = Vec::new();
        let mut remaining = target_tokens;
        let mut doc_id: DocId = 0;
        while remaining > 0 {
            let mut t = remaining.min(max_tokens_per_doc);
            if t.is_multiple_of(2) {
                // 2 * N - 1 は常に奇数のため、偶数になった場合は 1 引いて次の文書へ
                // 繰り越す（ループはいずれ収束し、最終的な合計は target_tokens に一致）。
                t -= 1;
            }
            let n = t.div_ceil(2);
            docs.push((doc_id, "東".repeat(n)));
            doc_id += 1;
            remaining -= t;
        }
        docs
    }

    #[test]
    fn build_accepts_corpus_at_max_corpus_tokens_boundary() {
        let owned_docs = cjk_corpus_with_token_count(MAX_CORPUS_TOKENS);
        let docs: Vec<(DocId, &str)> = owned_docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_corpus_exceeding_max_corpus_tokens() {
        let owned_docs = cjk_corpus_with_token_count(MAX_CORPUS_TOKENS + 1);
        let docs: Vec<(DocId, &str)> = owned_docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyTokens {
                total: MAX_CORPUS_TOKENS + 1,
                max: MAX_CORPUS_TOKENS,
            }
        );
    }

    // --- 入力検証の優先順位（fail-closed。k == 0 の早期 return より常に優先） ---

    #[test]
    fn search_rejects_overlong_query_even_when_k_is_zero() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = "a".repeat(MAX_QUERY_BYTES + 1);
        let err = idx.search(&query, 0).unwrap_err();
        assert_eq!(
            err,
            SparseError::QueryTooLong {
                len: MAX_QUERY_BYTES + 1,
                max: MAX_QUERY_BYTES,
            }
        );
    }

    #[test]
    fn search_rejects_too_many_query_terms_even_when_k_is_zero() {
        // バイト長は上限内でも一意語数が MAX_QUERY_TERMS を超えるクエリは、k == 0 でも
        // TooManyQueryTerms を返す（tokenize → 一意化 → MAX_QUERY_TERMS 検証を終えてから
        // k == 0 の早期 return を処理する順序の回帰検出）。
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = distinct_term_query(MAX_QUERY_TERMS + 1);
        let err = idx.search(&query, 0).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyQueryTerms {
                unique_terms: MAX_QUERY_TERMS + 1,
                max: MAX_QUERY_TERMS,
            }
        );
    }

    // --- approx_heap_bytes（Issue #357・sql/sparse_cache.rs の容量判定用） ---

    #[test]
    fn approx_heap_bytes_is_positive_for_nonempty_corpus() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "beta gamma")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.approx_heap_bytes() > 0);
    }

    #[test]
    fn approx_heap_bytes_grows_with_corpus_size() {
        let small = vec![(1u64, "alpha")];
        let large = vec![
            (1u64, "alpha beta gamma delta epsilon"),
            (2u64, "zeta eta theta"),
        ];
        let small_idx = SparseIndex::build(&small).unwrap();
        let large_idx = SparseIndex::build(&large).unwrap();
        assert!(large_idx.approx_heap_bytes() > small_idx.approx_heap_bytes());
    }
}
