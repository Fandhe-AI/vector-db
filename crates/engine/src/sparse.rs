//! 疎検索（BM25 Okapi）モジュール（TASK-102、対象ビヘイビア: SEARCH-1, SEARCH-3。
//! ポインタ: `docs/spec/05-tasks.md` TASK-102・`docs/spec/04-behavior/search.md`）。
//! 関連: TASK-103, TASK-104, TASK-105。
//!
//! 責務境界: コーパスからトークン頻度・文書長統計を持つ [`SparseIndex`] を構築し、
//! クエリに対する BM25 スコア降順の Top-k 検索を提供する純関数的な API を提供し、
//! storage/catalog とは結線しない。
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
//! CJK（ひらがな・カタカナ・CJK 統合漢字。カタカナ側は記号の `・`（U+30FB）・
//! 長音符 `ー`（U+30FC）も含むレンジ判定のため取り込まれる）はユニグラム＋文字バイグラムを
//! 生成する（小文字化した上で処理）。対応範囲外の文字（全角英数字・アクセント付きラテン
//! 文字・ハングル・半角カタカナ等）は無音で破棄される。ASCII 単語の途中に出現した場合は
//! その文字が欠落するだけでなく、その位置で単語が分割される（前後が結合されるのではない。
//! 例: `"cafés"` → `["caf", "s"]`）。この分割により生じた偽トークンは `term_freq`・
//! `doc_freq`・`doc_len` の統計を汚染しうる。
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
//! （`MAX_CORPUS_DOCS`）を検証する。詳細は [`SparseIndex::with_params`] を参照。
//!
//! untrusted 入力の扱い: すべての処理を入力長に対して線形に保つ（バイグラム生成含む）。
//! `Vec::with_capacity` は入力を `chars()` で数えた実際の文字数からのみ見積もる。
//! クエリはバイト長を `MAX_QUERY_BYTES`、一意語数を `MAX_QUERY_TERMS` で上限検証し
//! （詳細は [`SparseIndex::search`]）、文書はバイト長を `MAX_DOC_BYTES`、コーパスの
//! 文書数を `MAX_CORPUS_DOCS` で上限検証する（詳細は [`SparseIndex::with_params`]）。
//! いずれの検証も `tokenize()` を呼ぶ前にバイト長・件数のみを見る `O(1)` の判定で
//! 完結し、追加アロケーションを要しない。公開関数 [`tokenize`] 自体はこれらの上限を
//! 強制しない（呼び出し側が上限検証済みの入力のみを渡す契約とする。詳細は
//! [`tokenize`] のドキュメントを参照）。頻度・長さの演算はすべて
//! `checked_*`/`saturating_*` を用い、オーバーフローを未定義動作にしない。
//! `tokenize()` 内の添字アクセスは事前のループ境界チェックにより範囲内が証明可能
//! （panic しない）。

use std::cmp::Reverse;
use std::collections::BTreeMap;
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
const MAX_CORPUS_DOCS: usize = 100_000;

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
/// （U+30A0..=U+30FF）は文字単位の判定のため、仮名以外の記号（中黒 `・` U+30FB・
/// 長音符 `ー` U+30FC）も含めて CJK 扱いになる（意図的な仕様。除外はしない）。
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}' // ひらがな
        | '\u{30A0}'..='\u{30FF}' // カタカナ（記号の「・」「ー」を含むレンジ）
        | '\u{4E00}'..='\u{9FFF}' // CJK 統合漢字
    )
}

/// クエリ・文書テキストをトークン列へ分割する（TASK-102 の簡易トークナイザ）。
///
/// 小文字化した上で、ASCII 英数字・アンダースコアの連続を単語トークンとし、CJK 文字は
/// 文字ユニグラム＋隣接 2 文字のバイグラムを生成する。CJK ストップワード除去は含まない。
/// 関連: TASK-105。
///
/// 入力長に対して線形（`O(n)`）に処理し、`Vec` の初期容量は入力を `chars()` で数えた
/// 実際の文字数からのみ見積もる。トークンが 1 つも得られない入力（空文字列・記号のみ等）
/// は空の `Vec` を返す（呼び出し側はこれをエラーではなく空結果として扱う契約とする）。
///
/// 対応範囲は ASCII 単語文字と CJK（ひらがな・カタカナ・CJK 統合漢字）に限る。それ以外の
/// 文字（全角英数字・アクセント付きラテン文字・ハングル・半角カタカナ等）は無音で破棄され、
/// ASCII 単語の途中に出現した場合はその文字が欠落するだけでなく**その位置で単語が
/// 分割される**（前後が 1 トークンへ結合されるのではない。例: `"cafés"` →
/// `["caf", "s"]`）。この分割で生じる偽トークンは統計（`term_freq`・`doc_freq`・
/// `doc_len`）を汚染しうる。関連: TASK-105。
///
/// 契約: この関数自体は `text` のバイト長・文字数に上限を設けない。呼び出し側
/// （[`SparseIndex::with_params`]・[`SparseIndex::search`]）が `tokenize()` を呼ぶ前に
/// それぞれの上限（`MAX_DOC_BYTES`・`MAX_QUERY_BYTES`）を検証する経路からのみ呼ばれる
/// ことを前提とする。本関数を直接 untrusted 入力に対して呼ぶ場合は、呼び出し側で
/// 同等の長さ検証を行うこと。
pub fn tokenize(text: &str) -> Vec<String> {
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
            // ユニグラム
            tokens.push(c.to_string());
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
    /// `docs` はコーパス全体のサイズを制限する 2 段の上限検証を、各文書に対して
    /// `tokenize()`（アロケーションを伴う）を呼ぶ前に行う（`.claude/rules/coding-rust.md`:
    /// untrusted 入力の長さは上限検証してから処理する）。文書数が [`MAX_CORPUS_DOCS`]
    /// を超える場合は走査に入る前に [`SparseError::TooManyDocs`] で拒否し、各文書の
    /// バイト長が [`MAX_DOC_BYTES`] を超える場合は該当文書の `tokenize()` を呼ぶ前に
    /// [`SparseError::DocTooLong`] で拒否する。
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
        let mut doc_freq: BTreeMap<String, u32> = BTreeMap::new();
        let mut total_len: u64 = 0;

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

            let doc_tokens = tokenize(text);
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
    fn idf(&self, df: u32) -> f64 {
        let n = f64::from(self.doc_count);
        let df = f64::from(df);
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
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
}
