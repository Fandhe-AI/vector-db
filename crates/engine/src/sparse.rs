//! 疎検索（BM25 Okapi）モジュール（TASK-102、対象ビヘイビア: SEARCH-1, SEARCH-3。
//! ポインタ: `docs/spec/05-tasks.md` TASK-102・`docs/spec/04-behavior/search.md`）。
//!
//! 責務境界: コーパスからトークン頻度・文書長統計を持つ [`SparseIndex`] を構築し、
//! クエリに対する BM25 スコア降順の Top-k 検索を提供する。ハイブリッド検索の疎検索
//! 構成要素であり、密検索（ベクトル距離）との融合（RRF）は TASK-103 の管轄、定量的な
//! 受け入れ基準の回帰テスト化は TASK-104 の評価ハーネスの管轄とする（本モジュールは
//! それらの土台となる純関数的な API のみを提供し、storage/catalog とは結線しない）。
//!
//! スコアリング式は Robertson らの Okapi BM25 を実装する（本モジュールが定義する式が
//! 正であり、外部実装との数値一致は主張しない。TASK-104 で定量比較を行う）。
//! クエリ語 `q` に対する文書 `d` のスコアは各語 `t` について
//!
//! ```text
//! score(d, q) = Σ_t idf(t) * ( f(t, d) * (k1 + 1) )
//!                            / ( f(t, d) + k1 * (1 - b + b * |d| / avgdl) )
//! ```
//!
//! で与え、`idf(t) = ln( (N - df(t) + 0.5) / (df(t) + 0.5) + 1 )` とする（`+ 1` により
//! 常に非負となるため、負の IDF 補正の特別扱いは不要）。`k1`・`b` は
//! [`SparseIndex::with_params`] で調整可能とし、既定値は Okapi BM25 の一般的な値
//! （`k1 = 1.2`, `b = 0.75`）を用いる。
//!
//! トークナイザ（[`tokenize`]）は ASCII 英数字・アンダースコアの連続を単語トークンとし、
//! CJK（ひらがな・カタカナ・CJK 統合漢字）はユニグラム＋文字バイグラムを生成する
//! （小文字化した上で処理）。CJK ストップワード除去は TASK-105 の管轄のため、この境界を
//! 差し替え可能な関数単位（[`tokenize`]）で切り出しておく。
//!
//! untrusted 入力の扱い: 将来 wire 経由のクエリ文字列が本モジュールへ渡る前提のため、
//! すべての処理を入力長に対して線形に保ち（バイグラム生成含む）、`Vec::with_capacity` は
//! 検証済みの文字数からのみ確保する。頻度・長さの演算はすべて `checked_*`/`saturating_*`
//! を用い、オーバーフローを未定義動作にしない。呼び出し側はクエリ・文書の各トークン数を
//! 妥当な範囲に収める契約とする（本モジュールは上限を強制しないが、線形処理のため
//! 入力長に比例した処理時間のみを要求する）。本モジュールは現時点で storage/catalog/
//! wire-server に未結線であり、`tokenize()` 内の添字アクセスは事前のループ境界チェックに
//! より本モジュール単体では範囲内が証明可能（panic しない）。TASK-103 以降で実際に
//! wire 入力経路へ接続する際は、AGENTS.md P0（受信データ経路での `[]` 禁止）に合わせて
//! `get()` ベースへの置き換えを検討する。

use std::collections::BTreeMap;

/// 文書 ID。storage/catalog との結線は後続タスクの管轄のため、ここでは汎用の `u64` に
/// 留める（[`SparseIndex`] は呼び出し側が割り当てた ID をそのまま透過的に扱う）。
pub type DocId = u64;

/// BM25 の Okapi パラメータ既定値（項の飽和度）。
const DEFAULT_K1: f64 = 1.2;
/// BM25 の Okapi パラメータ既定値（文書長正規化の強さ）。
const DEFAULT_B: f64 = 0.75;

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
        }
    }
}

impl std::error::Error for SparseError {}

/// Top-k 検索結果 1 件（文書 ID と BM25 スコア）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredDoc {
    pub doc_id: DocId,
    pub score: f64,
}

/// 小文字化した 1 文字が ASCII 単語トークンの構成要素かどうか。
fn is_ascii_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 小文字化した 1 文字が CJK（ひらがな・カタカナ・CJK 統合漢字）かどうか。
/// `char` の範囲判定のみで実装し、外部の正規表現・Unicode データクレートに依存しない
/// （`.claude/rules/dependency-policy.md`: 依存最小方針）。
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}' // ひらがな
        | '\u{30A0}'..='\u{30FF}' // カタカナ
        | '\u{4E00}'..='\u{9FFF}' // CJK 統合漢字
    )
}

/// クエリ・文書テキストをトークン列へ分割する（TASK-102 の簡易トークナイザ）。
///
/// 小文字化した上で、ASCII 英数字・アンダースコアの連続を単語トークンとし、CJK 文字は
/// 文字ユニグラム＋隣接 2 文字のバイグラムを生成する。CJK ストップワード除去は含まない
/// （TASK-105 の管轄。差し替え可能な関数境界として本関数を維持する）。
///
/// 入力長に対して線形（`O(n)`）に処理し、`Vec` の初期容量は入力の文字数（検証済みの
/// 長さ）からのみ見積もる。トークンが 1 つも得られない入力（空文字列・記号のみ等）は
/// 空の `Vec` を返す（呼び出し側はこれをエラーではなく空結果として扱う契約とする）。
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
/// 結線は行わず、コーパスは呼び出し側が渡した `(DocId, &str)` のスライスから構築する
/// （TASK-103 の RRF 融合から呼ばれる想定の純関数的コンポーネント）。
#[derive(Debug)]
pub struct SparseIndex {
    k1: f64,
    b: f64,
    /// コーパス内の文書数（`N`）。
    doc_count: u32,
    /// 平均文書長（`avgdl`）。
    avg_doc_len: f64,
    docs: Vec<DocEntry>,
    /// トークン → 出現文書数（`df`）。`BTreeMap` で決定的な走査順を保つ。
    doc_freq: BTreeMap<String, u32>,
}

impl SparseIndex {
    /// Okapi BM25 の既定パラメータ（`k1 = 1.2`, `b = 0.75`）でインデックスを構築する。
    pub fn build(docs: &[(DocId, &str)]) -> Result<Self, SparseError> {
        Self::with_params(docs, DEFAULT_K1, DEFAULT_B)
    }

    /// `k1`・`b` を明示してインデックスを構築する（TASK-103 以降でのパラメータ調整用に
    /// 公開する）。
    ///
    /// `k1`・`b` は構築時に検証する（有限値かつ `k1 >= 0.0`・`b` は `[0.0, 1.0]`）。
    /// 不正値は `search()` 内で NaN 伝播・ガード節（`if score > 0.0` 等）により
    /// サイレントな空結果へ落ちてしまい fail-open になるため、ここで拒否して
    /// fail-closed を保つ（`.claude/rules/coding-rust.md`）。
    pub fn with_params(docs: &[(DocId, &str)], k1: f64, b: f64) -> Result<Self, SparseError> {
        if !k1.is_finite() || k1 < 0.0 || !b.is_finite() || !(0.0..=1.0).contains(&b) {
            return Err(SparseError::InvalidParams { k1, b });
        }
        if docs.is_empty() {
            return Err(SparseError::EmptyCorpus);
        }

        let mut seen_ids: BTreeMap<DocId, ()> = BTreeMap::new();
        let mut entries: Vec<DocEntry> = Vec::with_capacity(docs.len());
        let mut doc_freq: BTreeMap<String, u32> = BTreeMap::new();
        let mut total_len: u64 = 0;

        for &(doc_id, text) in docs {
            if seen_ids.insert(doc_id, ()).is_some() {
                return Err(SparseError::DuplicateDocId(doc_id));
            }

            let doc_tokens = tokenize(text);
            let mut term_freq: BTreeMap<String, u32> = BTreeMap::new();
            for tok in &doc_tokens {
                let counter = term_freq.entry(tok.clone()).or_insert(0u32);
                *counter = counter.saturating_add(1);
            }

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

        let doc_count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        let avg_doc_len = if doc_count == 0 {
            0.0
        } else {
            total_len as f64 / f64::from(doc_count)
        };

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
    pub fn search(&self, query: &str, k: usize) -> Vec<ScoredDoc> {
        if k == 0 {
            return Vec::new();
        }
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Vec::new();
        }

        // 重複クエリ語の IDF を二重計上しないよう一意化する（順序は不問。BTreeMap で決定的に）。
        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in &query_terms {
            unique_terms.insert(t.clone(), ());
        }

        let mut scored: Vec<ScoredDoc> = Vec::with_capacity(self.docs.len());
        for doc in &self.docs {
            let mut score = 0.0f64;
            for term in unique_terms.keys() {
                let Some(&f) = doc.term_freq.get(term) else {
                    continue;
                };
                let df = *self.doc_freq.get(term).unwrap_or(&0);
                let idf = self.idf(df);
                let numerator = f64::from(f) * (self.k1 + 1.0);
                let len_norm = 1.0 - self.b
                    + self.b * (f64::from(doc.doc_len) / self.avg_doc_len.max(f64::MIN_POSITIVE));
                let denominator = f64::from(f) + self.k1 * len_norm;
                if denominator > 0.0 {
                    score += idf * (numerator / denominator);
                }
            }
            if score > 0.0 {
                scored.push(ScoredDoc {
                    doc_id: doc.doc_id,
                    score,
                });
            }
        }

        // スコア降順、同点は doc_id 昇順（決定的タイブレーク。`total_cmp` は NaN を含めても
        // panic しない全順序を与える。スコアは有限値のみを積むため NaN は理論上生じない）。
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id)));
        scored.truncate(k);
        scored
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
        assert!(idx.search("", 10).is_empty());
        assert!(idx.search("!!!", 10).is_empty());
    }

    #[test]
    fn search_k_zero_returns_empty() {
        let docs = vec![(1u64, "alpha beta")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.search("alpha", 0).is_empty());
    }

    #[test]
    fn search_k_larger_than_corpus_returns_all_matching() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "alpha gamma")];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 100);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_tie_breaks_by_doc_id_ascending() {
        // 2 文書が完全に同一内容（同スコア）になるようにする。
        let docs = vec![(2u64, "alpha beta"), (1u64, "alpha beta")];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 10);
        assert_eq!(results.len(), 2);
        assert!((results[0].score - results[1].score).abs() < 1e-12);
        assert_eq!(results[0].doc_id, 1);
        assert_eq!(results[1].doc_id, 2);
    }

    #[test]
    fn search_keyword_match_ranks_above_non_match() {
        let docs = vec![
            (1u64, "the quick brown fox jumps over the lazy dog"),
            (2u64, "vector databases store embeddings for search"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("vector embeddings", 10);
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
        let first = idx.search("gamma delta", 10);
        let second = idx.search("gamma delta", 10);
        assert_eq!(first, second);
    }

    #[test]
    fn search_cjk_query_matches_expected_document() {
        let docs = vec![
            (1u64, "東京都渋谷区のカフェ"),
            (2u64, "大阪府大阪市のレストラン"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("東京", 10);
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
        let results = idx.search("shared", 10);
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

        let results = idx.search("alpha", 10);
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

        let results = idx.search("alpha", 10);
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
}
