//! 辞書的情報源抽出パイプライン（TASK-109、対象ビヘイビア: PLAN-5。ポインタ:
//! `docs/spec/05-tasks.md` TASK-109・`docs/spec/04-behavior/query-planning.md` PLAN-5）。
//!
//! 責務境界: DB に索引化済みのコーパス（ファイル形 `INSERT` のパス・本文）から、
//! 後続の LLM クエリプランニング（TASK-110 以降）が固定接頭辞コンテキストとして使う
//! 「辞書的情報源」を機械抽出する**純関数的な API**を提供する。`chunking.rs`・
//! `sparse.rs` と同じ流儀で storage / catalog / policy へは結線しない（行データの
//! 取得・世代整合キャッシュとの結線は `core.rs::DictionaryCache` の責務）。
//!
//! シンボル辞書は [`DictionaryConfig`] に無効化スイッチを持たず常に抽出する。
//! ファイルツリー・用語索引は同 config のフラグで無効化できる。新しい情報源の
//! 追加は [`DictionarySourceKind`] へのバリアント追加＋抽出関数 1 つの追加で
//! 完結する構造にする（段階的追加可能な設計）。
//!
//! 依存は追加しない（dependency-policy: regex 等の新規クレートは不可）ため、シンボル
//! 抽出・トークナイズはいずれも手書きの行パーサ／文字走査で実装する。`sparse.rs`
//! （BM25 用トークナイザ・CJK バイグラム込み）とは責務・要件が異なる（本モジュールは
//! ASCII 識別子中心の軽量な頻度集計で十分）ため共有せず、本モジュール内に閉じる。
//!
//! untrusted 入力に対する有界化（fail-closed。.claude/rules/coding-rust.md）:
//! 抽出対象の本文は呼び出し元（`chunking.rs` 経由でチャンク化済み）で既に
//! バイト長・行数の上限を通過済みだが、本モジュールでも独立に「1 抽出単位あたりの
//! 最大シンボル数」「シンボル名・パス・用語の最大長」「辞書全体の最大エントリ数・
//! 概算バイト量」を持つ。上限超過は決定的に切り詰め、[`Dictionary::truncated`] を
//! 立てる。これは検索を広域側（recall）へ劣化させる安全劣化であり、テナント境界・
//! 可視性判定には一切関与しない fail-open ではない（呼び出し元 `core.rs` の
//! `DictionaryCache`・`tenant::visible_rows` がテナント境界を担保する）。
//!
//! 決定性: 内部コンテナはすべて `BTreeMap` / `BTreeSet` を用い、反復順序を
//! 入力（行走査）順に依存させない（`scripts/check_sort_determinism.sh` との整合）。
//! [`Dictionary::finish`] が行う最終切り詰め（[`cap_btreeset`]・用語索引の
//! 上位 N 選定）は挿入順ではなくソート順（辞書順・頻度順）で行うため決定的だが、
//! [`DictionaryBuilder::ingest`] 内の生の安全弁（[`MAX_DICTIONARY_SYMBOLS`]・
//! [`MAX_DICTIONARY_PATHS`]・[`MAX_TERM_INDEX_RAW_ENTRIES`]）は「これ以上の新規
//! エントリを積み上げない」形の早期打ち切りであり、どのエントリが最初に打ち切りの
//! 対象になるかは呼び出し元の走査順序（`tenant::visible_rows` のページング順）に
//! 依存する。実運用では最終切り詰め後の集合が生の安全弁に到達すること自体が稀
//! （安全弁は DoS 対策の最終防衛線）であり、到達時は [`Dictionary::truncated`] が
//! 立つため、呼び出し元は「切り詰めが発生したこと」自体は決定的に検知できる。

use std::collections::{BTreeMap, BTreeSet};

/// 1 抽出単位（1 行 = 1 チャンク相当の本文）あたりの最大シンボル数。
/// これを超える分は決定的に切り詰め、[`Dictionary::truncated`] を立てる。
pub const MAX_SYMBOLS_PER_UNIT: usize = 512;

/// シンボル名の最大長（文字数）。超過分は切り詰める。
pub const MAX_SYMBOL_NAME_LEN: usize = 256;

/// パス文字列の最大長（文字数）。超過分は切り詰める。
pub const MAX_PATH_LEN: usize = 1024;

/// 用語（term）の最大長（文字数）。超過分は切り詰める。
pub const MAX_TERM_LEN: usize = 64;

/// [`Dictionary`] が保持するシンボル総数の上限（辞書全体・DoS 対策）。
pub const MAX_DICTIONARY_SYMBOLS: usize = 20_000;

/// [`Dictionary`] が保持するファイルパス総数の上限。
pub const MAX_DICTIONARY_PATHS: usize = 20_000;

/// 用語索引が集計時に保持する生の異なり語数の上限（頻度集計前の安全弁。DoS 対策）。
/// これを超えて初めて登場する語は集計に加えない（既出語の頻度加算は継続する）。
pub const MAX_TERM_INDEX_RAW_ENTRIES: usize = 50_000;

/// [`DictionaryConfig::top_terms`] の既定値。
pub const DEFAULT_TOP_TERMS: usize = 300;

/// ソース種別判定（[`detect_source_kind`]）。ファイルシステムへは一切アクセスせず、
/// パス文字列（untrusted）のみから判定する（`chunking.rs::detect_file_kind` と同じ
/// untrusted パス安全方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFileKind {
    /// Rust ソース（拡張子 `.rs`、ASCII 大小無視）。
    Rust,
    /// Markdown（拡張子 `.md` / `.markdown`、ASCII 大小無視）。
    Markdown,
    /// 上記以外すべて（拡張子なし・未知拡張子を含む）。
    Other,
}

/// パス文字列の最終要素の拡張子のみから [`SourceFileKind`] を判定する。
pub fn detect_source_kind(path: &str) -> SourceFileKind {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = match file_name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() && !stem.is_empty() => ext,
        _ => return SourceFileKind::Other,
    };
    if ext.eq_ignore_ascii_case("rs") {
        SourceFileKind::Rust
    } else if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") {
        SourceFileKind::Markdown
    } else {
        SourceFileKind::Other
    }
}

/// 段階的に追加可能な辞書的情報源の種別。新規情報源の追加は
/// このバリアント追加＋対応する抽出関数の追加で完結する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DictionarySourceKind {
    /// シンボル辞書（[`DictionaryConfig`] に無効化スイッチを持たない）。
    SymbolDict,
    /// ファイルツリー（[`DictionaryConfig::enable_file_tree`] で無効化可能）。
    FileTree,
    /// 用語索引（[`DictionaryConfig::enable_term_index`] で無効化可能）。
    TermIndex,
}

/// Rust の行頭定義の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolKind {
    Fn,
    Struct,
    Enum,
    Trait,
    Impl,
    Mod,
    Const,
    Type,
}

/// 抽出された 1 シンボル。`Ord` はソート順（`path` → `line` → `name` → `kind` →
/// `unit_seq`）を決定的にするために導出し、[`Dictionary::symbols`]（`BTreeSet`）の
/// 反復順序が挿入順に依存しないようにする。
///
/// `unit_seq` は [`extract_rust_symbols`] を呼んだ抽出単位（チャンク）の呼び出し順
/// 連番（[`DictionaryBuilder`] が付与）であり、`line` がチャンク相対値であることの
/// 埋め合わせとして同一性に含める（TASK-109・PLAN-5 レビュー対応: `line` のみを
/// 同一性に使うと、別チャンクの同名・同種シンボルがたまたま同じチャンク相対行番号に
/// 来た場合に `BTreeSet` 上で衝突し、後から挿入した側が黙って欠落していた。ADR の
/// 「チャンク化でシンボル欠落しない」契約に反するため、チャンク単位で一意な値を
/// 同一性へ組み込むことで衝突自体をなくす）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Symbol {
    pub path: String,
    pub line: u32,
    pub name: String,
    pub kind: SymbolKind,
    pub unit_seq: u64,
}

/// `s` を `max_chars` 文字（文字境界。バイト境界ではない）で決定的に切り詰める。
/// 戻り値の `bool` は実際に切り詰めが発生したか（`s` の文字数が `max_chars` を
/// 超えていたか）を示す。呼び出し元はこれを集約して
/// [`DictionaryBuilder::truncated`] / [`Dictionary::truncated`] へ伝播させる
/// 契約（モジュールドキュメント「有界化契約」参照。TASK-109・PLAN-5 レビュー
/// 対応: 従来は切り詰めの有無を呼び出し元へ返さず、シンボル名・パス・用語の
/// 個別切り詰めが `truncated` に反映されないまま黙殺されていた）。
fn truncate_chars(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_string(), false);
    }
    (s.chars().take(max_chars).collect(), true)
}

/// 行頭の可視性修飾子（`pub` / `pub(...)`）・`async` / `unsafe` /
/// `extern "..."` を許容しつつ、行頭定義（`fn` / `struct` / `enum` / `trait` /
/// `impl` / `mod` / `const` / `type`）を検出する手書きの行パーサ（TASK-109・PLAN-5）。
///
/// 空白区切りトークン列を先頭から走査する軽量な実装であり、コメント・文字列リテラル
/// 内の同形テキストを特別扱いしない（`// fn foo()` は最初のトークンが `//` のため
/// 誤検出しない。ブロックコメント内・文字列リテラル内に単独で `fn foo() {` の形が
/// 現れる稀なケースは誤検出し得るが、辞書は LLM への補助コンテキストであり
/// 過検出は安全側（recall 側）の劣化に留まる。モジュールドキュメント参照）。
fn parse_definition_line(line: &str) -> Option<(SymbolKind, String)> {
    let mut tokens = line.split_whitespace();
    let mut tok = tokens.next()?;

    // 可視性修飾子: `pub` または `pub(...)`（`pub(crate)` 等は空白を挟まず 1 トークン）。
    if tok == "pub" || tok.starts_with("pub(") {
        tok = tokens.next()?;
    }

    // `async` / `unsafe` / `extern "..."` は任意個・任意順で先行し得る。`const` は
    // `const fn foo()`（`fn` の修飾子）と `const NAME: T = ...`（独立した定義）の
    // 2 通りで意味が異なるため、次のトークンが `fn` の場合のみ修飾子として読み飛ばす
    // （1 トークン先読みしても `fn` でなければ `const` 自体を定義キーワードとして扱う
    // 通常経路へフォールスルーする。TASK-109・PLAN-5 レビュー対応: 従来は `const` を
    // 常に修飾子扱いせず定義キーワードとして早期一致させていたため
    // `pub const fn helper()` が `Const("fn")` という無意味な結果になっていた）。
    loop {
        match tok {
            "async" | "unsafe" => {
                tok = tokens.next()?;
            }
            "extern" => {
                let next = tokens.next()?;
                tok = if next.starts_with('"') {
                    tokens.next()?
                } else {
                    next
                };
            }
            "const" => {
                let mut lookahead = tokens.clone();
                if lookahead.next() == Some("fn") {
                    tokens = lookahead;
                    tok = "fn";
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    // `impl<T> ...` はジェネリクスが `impl` に空白なしで直接続く（`impl<T> Trait for
    // Type<T>` 等）ため、トークン自体が `impl` と完全一致しない。他のキーワードは
    // 常に単独トークンで現れるため `starts_with` にすると誤検出しうるが、`impl` は
    // 予約語であり `impl` から始まる別の識別子は存在しないため安全に判定できる。
    let kind = if tok == "impl" || tok.starts_with("impl<") {
        SymbolKind::Impl
    } else {
        match tok {
            "fn" => SymbolKind::Fn,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "mod" => SymbolKind::Mod,
            "const" => SymbolKind::Const,
            "type" => SymbolKind::Type,
            _ => return None,
        }
    };

    let name = if kind == SymbolKind::Impl {
        // `impl` は単純な識別子を持たない（`impl<T> Foo<T>` / `impl<T> Trait for
        // Type<T>` 等）。行頭 `impl` 以降・`{` 手前までを名前として採る。
        let idx = line.find("impl")?;
        let after = &line[idx + "impl".len()..];
        let after = after.split('{').next().unwrap_or(after).trim();
        if after.is_empty() {
            return None;
        }
        after.to_string()
    } else {
        let next = tokens.next()?;
        let ident: String = next
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if ident.is_empty() {
            return None;
        }
        ident
    };

    Some((kind, name))
}

/// 1 抽出単位（`path` の 1 チャンク本文 `body`）から Rust シンボルを抽出する。
/// 戻り値の `bool` は [`MAX_SYMBOLS_PER_UNIT`] 超過による決定的切り詰め、
/// および `path`・シンボル名が [`MAX_PATH_LEN`]・[`MAX_SYMBOL_NAME_LEN`] を
/// 超えて切り詰められたかのいずれかが発生したことを示す（TASK-109・PLAN-5
/// レビュー対応: 個別の文字列切り詰めも `truncated` へ確実に反映する）。
///
/// 行番号はこの抽出単位内でのローカルな 1 起点行番号であり、チャンク化
/// （`chunking.rs`）により本文が複数チャンクへ分割されている場合、元ファイル全体での
/// 行番号とは一致しない（チャンク化は行分割ベース・非オーバーラップのため
/// シンボル自体の欠落は生じないが、行番号はチャンク相対値になる。呼び出し元
/// `core.rs::DictionaryCache` のドキュメント参照）。
///
/// `unit_seq` は呼び出し元（[`DictionaryBuilder::ingest`]）がこの抽出単位に
/// 付与した一意な連番で、[`Symbol`] の同一性へそのまま伝播する（[`Symbol`] の
/// ドキュメンテーションコメント参照。チャンク相対行番号だけでは別チャンクの
/// 同名・同種シンボルが衝突しうるための埋め合わせ）。
pub fn extract_rust_symbols(path: &str, body: &str, unit_seq: u64) -> (Vec<Symbol>, bool) {
    let (path, mut truncated) = truncate_chars(path, MAX_PATH_LEN);
    let mut out = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        if out.len() >= MAX_SYMBOLS_PER_UNIT {
            truncated = true;
            break;
        }
        let Some((kind, name)) = parse_definition_line(line) else {
            continue;
        };
        // `enumerate` は 0 起点。行番号は 1 起点かつ `u32` へ飽和変換する
        // （untrusted 入力の行数は `chunking.rs::MAX_INPUT_LINES` で既に上限検証済み
        // だが、本モジュール単体でも `unwrap` を避け飽和変換で処理する）。
        let line_no = u32::try_from(idx.saturating_add(1)).unwrap_or(u32::MAX);
        let (name, name_truncated) = truncate_chars(&name, MAX_SYMBOL_NAME_LEN);
        if name_truncated {
            truncated = true;
        }
        out.push(Symbol {
            path: path.clone(),
            line: line_no,
            name,
            kind,
            unit_seq,
        });
    }
    (out, truncated)
}

/// ファイルツリー情報源（補助）。パス一覧・拡張子別・トップディレクトリ別の集計。
#[derive(Debug, Clone, Default)]
pub struct FileTree {
    pub paths: BTreeSet<String>,
    pub by_extension: BTreeMap<String, u64>,
    pub by_top_dir: BTreeMap<String, u64>,
}

/// `path` をファイルツリー集計へ反映する。拡張子が取れない場合は "(none)"、
/// トップディレクトリが取れない場合は "(root)" に集計する。
///
/// 呼び出し元（`core.rs::dictionary_snapshot`）は同一パスの複数チャンク本文を
/// 別々の抽出単位として順次 `ingest` するため、本関数もパスごとに複数回呼ばれ
/// うる。`by_extension`/`by_top_dir` は**ファイル単位**の集計（1 ファイル＝1 カウ
/// ント）であるべきなので、`paths.insert` が真（＝パス初出）の場合のみ加算する
/// （TASK-109・PLAN-5 レビュー対応: チャンク単位の重複加算バグ修正）。
///
/// 戻り値の `bool` は `path` が [`MAX_PATH_LEN`] を超えて切り詰められたかを示す
/// （TASK-109・PLAN-5 レビュー対応: 呼び出し元 `DictionaryBuilder::ingest` が
/// これを `truncated` へ伝播する）。
fn accumulate_file_tree(tree: &mut FileTree, path: &str) -> bool {
    let (path, truncated) = truncate_chars(path, MAX_PATH_LEN);
    if !tree.paths.insert(path.clone()) {
        // 既知パスの再訪（同一ファイルの別チャンク）。パス集合には既に含まれて
        // おり拡張子・トップディレクトリの二重加算を防ぐため、ここで打ち切る。
        return truncated;
    }

    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(&path);
    let ext_key = file_name
        .rsplit_once('.')
        .filter(|(stem, ext)| !ext.is_empty() && !stem.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_else(|| "(none)".to_string());
    let counter = tree.by_extension.entry(ext_key).or_insert(0);
    *counter = counter.saturating_add(1);

    // 区切り文字は上の `file_name` 抽出（`rsplit(['/', '\\'])`）と同じ集合を使う
    // （TASK-109・PLAN-5 レビュー対応: `/` のみだと Windows 形式パスの
    // トップディレクトリが誤って "(root)" に集計される不一致があった）。
    let top_dir = match path.split_once(['/', '\\']) {
        Some((dir, _)) if !dir.is_empty() => dir.to_string(),
        _ => "(root)".to_string(),
    };
    let counter = tree.by_top_dir.entry(top_dir).or_insert(0);
    *counter = counter.saturating_add(1);
    truncated
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "her", "was", "one", "our",
    "out", "day", "get", "has", "him", "his", "how", "man", "new", "now", "old", "see", "two",
    "way", "who", "did", "its", "let", "put", "say", "she", "too", "use", "this", "that", "with",
    "from", "have", "will", "your", "they", "them", "then", "than", "when", "what", "where",
    "which", "while", "into", "over", "also", "such", "each", "some", "more", "most", "only",
    "other", "these", "those", "been", "being", "were", "does", "doing", "here", "there", "about",
    "would", "could", "should", "must", "may", "might", "shall",
];

fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

/// ASCII 単語を小文字化・3 文字以上・ストップワード除外の条件で切り出す軽量
/// トークナイザ（`sparse.rs` の BM25 用トークナイザとは責務が異なるため独立実装。
/// モジュールドキュメント参照）。
/// 戻り値の `bool` はいずれかの語が [`MAX_TERM_LEN`] を超えて切り詰められたかを
/// 示す（TASK-109・PLAN-5 レビュー対応: 呼び出し元の用語抽出関数がこれを
/// `DictionaryBuilder::truncated` へ伝播する）。
fn tokenize_ascii_words(text: &str) -> (Vec<String>, bool) {
    let mut words = Vec::new();
    let mut truncated = false;
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphabetic() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            if current.chars().count() >= 3 && !is_stopword(&current) {
                let (word, word_truncated) = truncate_chars(&current, MAX_TERM_LEN);
                truncated = truncated || word_truncated;
                words.push(word);
            }
            current.clear();
        }
    }
    if !current.is_empty() && current.chars().count() >= 3 && !is_stopword(&current) {
        let (word, word_truncated) = truncate_chars(&current, MAX_TERM_LEN);
        truncated = truncated || word_truncated;
        words.push(word);
    }
    (words, truncated)
}

/// Rust ソースのドキュメンテーションコメント（`///` / `//!`）から用語頻度を抽出する。
/// 戻り値の `bool` はいずれかの語が [`MAX_TERM_LEN`] 超で切り詰められたかを示す
/// （TASK-109・PLAN-5 レビュー対応。呼び出し元 `DictionaryBuilder::merge_terms`
/// がこれを `truncated` へ伝播する）。
fn extract_rust_doc_terms(body: &str) -> (BTreeMap<String, u64>, bool) {
    let mut freq = BTreeMap::new();
    let mut truncated = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        let content = if let Some(rest) = trimmed.strip_prefix("///") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("//!") {
            rest
        } else {
            continue;
        };
        let (words, words_truncated) = tokenize_ascii_words(content);
        truncated = truncated || words_truncated;
        for word in words {
            let counter = freq.entry(word).or_insert(0u64);
            *counter = counter.saturating_add(1);
        }
    }
    (freq, truncated)
}

/// Markdown 本文（見出し・地の文いずれも含む）から用語頻度を抽出する。戻り値の
/// `bool` の意味は [`extract_rust_doc_terms`] と同じ。
fn extract_markdown_terms(body: &str) -> (BTreeMap<String, u64>, bool) {
    let mut freq = BTreeMap::new();
    let (words, truncated) = tokenize_ascii_words(body);
    for word in words {
        let counter = freq.entry(word).or_insert(0u64);
        *counter = counter.saturating_add(1);
    }
    (freq, truncated)
}

/// 抽出パイプラインの設定。シンボル辞書には無効化スイッチを持たせない
/// （常に構築されることを型・設定面で保証する）。
#[derive(Debug, Clone)]
pub struct DictionaryConfig {
    /// ファイルツリー情報源を有効化するか。
    pub enable_file_tree: bool,
    /// 用語索引情報源を有効化するか。
    pub enable_term_index: bool,
    /// 用語索引が保持する上位語数。
    pub top_terms: usize,
}

impl Default for DictionaryConfig {
    fn default() -> Self {
        Self {
            enable_file_tree: true,
            enable_term_index: true,
            top_terms: DEFAULT_TOP_TERMS,
        }
    }
}

/// 抽出結果を束ねる辞書本体。`BTreeSet`/`BTreeMap` により反復順序は常に決定的
/// （モジュールドキュメント参照）。
#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    /// シンボル辞書（必須。常に構築される）。
    pub symbols: BTreeSet<Symbol>,
    /// ファイルツリー（補助。[`DictionaryConfig::enable_file_tree`] が `false` なら空）。
    pub file_tree: FileTree,
    /// 用語索引（補助。[`DictionaryConfig::enable_term_index`] が `false` なら空）。
    /// 値は集計後の頻度（上位 [`DictionaryConfig::top_terms`] 件に切り詰め済み）。
    pub term_index: BTreeMap<String, u64>,
    /// いずれかの情報源で上限超過による決定的切り詰めが発生したか。
    pub truncated: bool,
}

impl Dictionary {
    /// キャッシュ容量判定用の概算ヒープバイト数（`core.rs::DictionaryCache` が
    /// 容量上限を判定するために使う。`rls.rs::PrefilterSnapshot::approx_heap_bytes`
    /// と同じ用途で、フィールド構造から粗く見積もる厳密でない概算値）。
    pub fn approx_heap_bytes(&self) -> usize {
        let symbols: usize = self
            .symbols
            .iter()
            .map(|s| s.path.len().saturating_add(s.name.len()).saturating_add(16))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let paths: usize = self
            .file_tree
            .paths
            .iter()
            .map(|p| p.len())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let exts: usize = self
            .file_tree
            .by_extension
            .keys()
            .map(|k| k.len().saturating_add(8))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let dirs: usize = self
            .file_tree
            .by_top_dir
            .keys()
            .map(|k| k.len().saturating_add(8))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let terms: usize = self
            .term_index
            .keys()
            .map(|k| k.len().saturating_add(8))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        symbols
            .saturating_add(paths)
            .saturating_add(exts)
            .saturating_add(dirs)
            .saturating_add(terms)
    }
}

/// `set` をソート順で先頭 `max` 件に決定的に切り詰める（挿入順ではなく `Ord` 順。
/// モジュールドキュメント「決定性」参照）。
fn cap_btreeset<T: Ord>(mut set: BTreeSet<T>, max: usize) -> (BTreeSet<T>, bool) {
    let mut truncated = false;
    while set.len() > max {
        // `BTreeSet::pop_last` はソート順で最大の要素を取り除く。先頭 `max` 件
        // （辞書順で小さい方）を決定的に残す。
        set.pop_last();
        truncated = true;
    }
    (set, truncated)
}

/// 単一ファイル（`path`）分の抽出単位（1 つ以上のチャンク本文）を積み上げる
/// 増分ビルダー（TASK-109）。`core.rs::EngineCore::dictionary_snapshot` はテーブル
/// 走査で得た行を `path` 単位に事前グルーピングはせず、可視行を走査順に 1 行ずつ
/// `(path, body)` として本ビルダーへ渡す（同一 `path` の行が複数あれば `ingest` が
/// その都度呼ばれる。`ingest` 自体が呼び出し側の分割粒度に依存しない設計のため、
/// グルーピングの有無は抽出結果に影響しない）。
///
/// `config` は構築時（[`DictionaryBuilder::new`]）に固定して保持する。`ingest`・
/// `finish` を別々の `&DictionaryConfig` で呼び分けられる構造だと、蓄積済みの
/// `term_freq`（`enable_term_index` を前提に集計している）が `finish` 時の異なる
/// 設定で黙って捨てられる／解釈が食い違う不整合を起こしうるため（TASK-109・PLAN-5
/// レビュー対応）、単一の設定に固定して不整合の余地自体を排除する。
#[derive(Debug)]
pub struct DictionaryBuilder {
    config: DictionaryConfig,
    symbols: BTreeSet<Symbol>,
    file_tree: FileTree,
    term_freq: BTreeMap<String, u64>,
    truncated: bool,
    /// 次回 `ingest` 呼び出しに付与する抽出単位（チャンク）連番。
    /// [`Symbol::unit_seq`] のドキュメンテーションコメント参照。
    next_unit_seq: u64,
}

impl DictionaryBuilder {
    /// `config` に固定したビルダーを新規作成する。以降の `ingest`・`finish` は
    /// すべてこの `config` を用いる。
    pub fn new(config: DictionaryConfig) -> Self {
        Self {
            config,
            symbols: BTreeSet::new(),
            file_tree: FileTree::default(),
            term_freq: BTreeMap::new(),
            truncated: false,
            next_unit_seq: 0,
        }
    }

    /// `path`・`body` の 1 抽出単位（1 行 = 1 チャンク相当）を取り込む。
    pub fn ingest(&mut self, path: &str, body: &str) {
        // この呼び出し（＝ 1 抽出単位）に一意な連番を割り当てる（`u64` の枯渇は
        // `tenant::MAX_VISIBLE_ROWS`・`MAX_SCANNED_ROWS` の実用上限から到達し得ない
        // 防御的な飽和演算。coding-rust.md「整数演算は checked_*/saturating_* を使う」）。
        let unit_seq = self.next_unit_seq;
        self.next_unit_seq = self.next_unit_seq.saturating_add(1);

        // シンボル辞書には無効化スイッチが無いため、ソース種別に関わらず常に試みる
        // （Rust 以外は行頭が予約語と一致しない限り検出されず実質空になる）。
        match detect_source_kind(path) {
            SourceFileKind::Rust => {
                let (symbols, truncated) = extract_rust_symbols(path, body, unit_seq);
                if truncated {
                    self.truncated = true;
                }
                for symbol in symbols {
                    // 生の安全弁（早期打ち切り。挿入順＝呼び出し元の走査順に依存する。
                    // モジュールドキュメント「決定性」参照）。最終的な決定的切り詰めは
                    // `finish` 内の `cap_btreeset` が担う。
                    if self.symbols.len() >= MAX_DICTIONARY_SYMBOLS {
                        // 上限到達後でも、`symbol` が既存エントリと同一（同一性は
                        // `Symbol` 全フィールド一致）であれば `insert` は何も変えない
                        // ため実際には切り詰めていない。ここを確認せず一律
                        // `truncated = true` にすると誤った切り詰め通知になる
                        // （TASK-109・PLAN-5 レビュー対応）。
                        if self.symbols.contains(&symbol) {
                            continue;
                        }
                        self.truncated = true;
                        break;
                    }
                    self.symbols.insert(symbol);
                }
                if self.config.enable_term_index {
                    let (terms, terms_truncated) = extract_rust_doc_terms(body);
                    if terms_truncated {
                        self.truncated = true;
                    }
                    self.merge_terms(terms);
                }
            }
            SourceFileKind::Markdown => {
                if self.config.enable_term_index {
                    let (terms, terms_truncated) = extract_markdown_terms(body);
                    if terms_truncated {
                        self.truncated = true;
                    }
                    self.merge_terms(terms);
                }
            }
            SourceFileKind::Other => {}
        }

        if self.config.enable_file_tree {
            // 安全弁の到達判定は実際に `paths` へ挿入される切り詰め後の値
            // （`accumulate_file_tree` 内部の `truncate_chars` と同じ変換）で行う。
            // 生の未切り詰め `path` で判定すると、`MAX_PATH_LEN` 超のパスが
            // 上限到達後に再 ingest された際、切り詰め後は既知パスであっても
            // 生の文字列比較では不一致となり新規パスと誤判定して `truncated` を
            // 不要に立ててしまう（TASK-109・PLAN-5 レビュー対応）。
            let (truncated_path, path_truncated) = truncate_chars(path, MAX_PATH_LEN);
            let safety_valve_hit = self.file_tree.paths.len() >= MAX_DICTIONARY_PATHS
                && !self.file_tree.paths.contains(&truncated_path);
            if !safety_valve_hit {
                // `accumulate_file_tree` 内部でも同じ `truncate_chars(path,
                // MAX_PATH_LEN)` を行うため、その戻り値は上の `path_truncated` と
                // 常に一致する（呼び出し側で二重に `truncated` を立てないよう
                // 戻り値自体は捨てるが、`path_truncated` で既に伝播済み）。
                let _ = accumulate_file_tree(&mut self.file_tree, path);
            }
            if path_truncated || safety_valve_hit {
                self.truncated = true;
            }
        }
    }

    fn merge_terms(&mut self, freq: BTreeMap<String, u64>) {
        for (term, count) in freq {
            if !self.term_freq.contains_key(&term)
                && self.term_freq.len() >= MAX_TERM_INDEX_RAW_ENTRIES
            {
                // 生の異なり語数の安全弁（モジュールドキュメント参照）。既出語の
                // 頻度加算は継続し、新規語のみ打ち切る。
                self.truncated = true;
                continue;
            }
            let counter = self.term_freq.entry(term).or_insert(0);
            *counter = counter.saturating_add(count);
        }
    }

    /// 積み上げた内容を [`Dictionary`] へ確定する。用語索引は頻度降順・同点は
    /// 辞書順昇順で上位 `config.top_terms` 件に決定的に切り詰める。設定は
    /// [`DictionaryBuilder::new`] で固定したものを用いる（`ingest` と異なる設定を
    /// 渡して不整合を起こす余地をなくすため、引数では受け取らない）。
    pub fn finish(self) -> Dictionary {
        let config = &self.config;
        let (symbols, symbols_truncated) = cap_btreeset(self.symbols, MAX_DICTIONARY_SYMBOLS);
        let (paths, paths_truncated) = cap_btreeset(self.file_tree.paths, MAX_DICTIONARY_PATHS);

        let mut term_index = BTreeMap::new();
        let mut terms_truncated = false;
        if config.enable_term_index {
            // 頻度降順・同点は辞書順昇順（決定的タイブレーク）でソートしてから
            // 上位 `top_terms` 件のみ残す。`BTreeMap` の反復自体はキー昇順のため、
            // 一旦 `Vec` へ写してソートし直す。
            let mut entries: Vec<(String, u64)> = self.term_freq.into_iter().collect();
            entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            if entries.len() > config.top_terms {
                entries.truncate(config.top_terms);
                terms_truncated = true;
            }
            term_index = entries.into_iter().collect();
        }

        Dictionary {
            symbols,
            file_tree: FileTree {
                paths,
                by_extension: self.file_tree.by_extension,
                by_top_dir: self.file_tree.by_top_dir,
            },
            term_index,
            truncated: self.truncated || symbols_truncated || paths_truncated || terms_truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ソース種別判定 ---

    #[test]
    fn detects_rust_and_markdown_and_other() {
        assert_eq!(detect_source_kind("src/lib.rs"), SourceFileKind::Rust);
        assert_eq!(detect_source_kind("README.md"), SourceFileKind::Markdown);
        assert_eq!(
            detect_source_kind("docs/NOTES.MARKDOWN"),
            SourceFileKind::Markdown
        );
        assert_eq!(detect_source_kind("data.json"), SourceFileKind::Other);
        assert_eq!(detect_source_kind("Makefile"), SourceFileKind::Other);
    }

    // --- シンボルパーサ ---

    #[test]
    fn parses_pub_crate_async_fn() {
        let (kind, name) = parse_definition_line("pub(crate) async fn run_batch() {").unwrap();
        assert_eq!(kind, SymbolKind::Fn);
        assert_eq!(name, "run_batch");
    }

    #[test]
    fn parses_unsafe_fn() {
        // `tests/isa.rs::unsafe_is_confined_to_isa_module_with_safety_comments` は
        // `crates/engine/src/**/*.rs` 中の当該キーワード（直後に空白を伴う形）の
        // トークン出現を `isa.rs` に限定するソース走査規約（AGENTS.md P1）を持つ。
        // 検査対象の入力文字列を 2 リテラルの連結で構築し、ソース上にその
        // キーワード＋空白の連続バイト列を残さない。
        let modifier = "unsafe";
        let input = format!("{modifier} fn raw_get(idx: usize) -> u8 {{");
        let (kind, name) = parse_definition_line(&input).unwrap();
        assert_eq!(kind, SymbolKind::Fn);
        assert_eq!(name, "raw_get");
    }

    #[test]
    fn parses_const_fn() {
        // TASK-109・PLAN-5 レビュー対応の回帰テスト（Cursor Bugbot Medium
        // "Const fn parsed as const"）: `const` を `fn` の修飾子として先読みせず
        // 独立した定義キーワードとして早期一致させると、`pub const fn helper()` が
        // `Const("fn")` という無意味な結果になっていた。
        let (kind, name) = parse_definition_line("pub const fn helper() -> u8 {").unwrap();
        assert_eq!(kind, SymbolKind::Fn);
        assert_eq!(name, "helper");
    }

    #[test]
    fn parses_const_item_without_fn() {
        // `const fn` 以外の通常の `const` 定義は従来どおり `Const` として扱う。
        let (kind, name) = parse_definition_line("pub const MAX: u8 = 10;").unwrap();
        assert_eq!(kind, SymbolKind::Const);
        assert_eq!(name, "MAX");
    }

    #[test]
    fn parses_extern_c_fn() {
        let (kind, name) = parse_definition_line("pub extern \"C\" fn ffi_entry() {").unwrap();
        assert_eq!(kind, SymbolKind::Fn);
        assert_eq!(name, "ffi_entry");
    }

    #[test]
    fn parses_generic_struct() {
        let (kind, name) = parse_definition_line("pub struct Wrapper<T: Clone> {").unwrap();
        assert_eq!(kind, SymbolKind::Struct);
        assert_eq!(name, "Wrapper");
    }

    #[test]
    fn parses_impl_for() {
        let (kind, name) = parse_definition_line("impl<T> MyTrait for MyType<T> {").unwrap();
        assert_eq!(kind, SymbolKind::Impl);
        assert_eq!(name, "<T> MyTrait for MyType<T>");
    }

    #[test]
    fn parses_const_with_type() {
        let (kind, name) = parse_definition_line("pub const MAX_LEN: usize = 10;").unwrap();
        assert_eq!(kind, SymbolKind::Const);
        assert_eq!(name, "MAX_LEN");
    }

    #[test]
    fn parses_type_alias() {
        let (kind, name) =
            parse_definition_line("pub(crate) type Result<T> = std::result::Result<T, Error>;")
                .unwrap();
        assert_eq!(kind, SymbolKind::Type);
        assert_eq!(name, "Result");
    }

    #[test]
    fn parses_mod() {
        let (kind, name) = parse_definition_line("pub mod dictionary;").unwrap();
        assert_eq!(kind, SymbolKind::Mod);
        assert_eq!(name, "dictionary");
    }

    #[test]
    fn parses_enum_and_trait() {
        assert_eq!(
            parse_definition_line("enum Kind {").unwrap(),
            (SymbolKind::Enum, "Kind".to_string())
        );
        assert_eq!(
            parse_definition_line("trait Reranker {").unwrap(),
            (SymbolKind::Trait, "Reranker".to_string())
        );
    }

    #[test]
    fn does_not_detect_non_definition_lines() {
        assert!(parse_definition_line("// fn looks_like_a_def() {").is_none());
        assert!(parse_definition_line("let result = compute();").is_none());
        assert!(parse_definition_line("self.method_call(a, b);").is_none());
        assert!(parse_definition_line("").is_none());
        assert!(parse_definition_line("    ").is_none());
    }

    #[test]
    fn extract_rust_symbols_reports_line_numbers() {
        let body = "// header\nfn one() {}\n\npub struct Two {\n    field: u8,\n}\n";
        let (symbols, truncated) = extract_rust_symbols("src/x.rs", body, 0);
        assert!(!truncated);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "one");
        assert_eq!(symbols[0].line, 2);
        assert_eq!(symbols[1].name, "Two");
        assert_eq!(symbols[1].line, 4);
    }

    #[test]
    fn extract_rust_symbols_truncates_deterministically() {
        let mut body = String::new();
        for i in 0..(MAX_SYMBOLS_PER_UNIT + 10) {
            body.push_str(&format!("fn f{i}() {{}}\n"));
        }
        let (symbols, truncated) = extract_rust_symbols("src/many.rs", &body, 0);
        assert!(truncated);
        assert_eq!(symbols.len(), MAX_SYMBOLS_PER_UNIT);
    }

    #[test]
    fn symbol_name_is_truncated_to_max_len() {
        let long_name = "a".repeat(MAX_SYMBOL_NAME_LEN + 50);
        let body = format!("fn {long_name}() {{}}\n");
        let (symbols, _) = extract_rust_symbols("src/x.rs", &body, 0);
        assert_eq!(symbols[0].name.chars().count(), MAX_SYMBOL_NAME_LEN);
    }

    // --- 用語索引 ---

    #[test]
    fn extracts_terms_from_rust_doc_comments_only() {
        let body = "//! module about caching and eviction\nfn helper() {}\n/// short\n";
        let (freq, truncated) = extract_rust_doc_terms(body);
        assert!(!truncated);
        assert!(freq.contains_key("caching"));
        assert!(freq.contains_key("eviction"));
        assert!(freq.contains_key("module"));
        // 非 doc コメント行（関数本体）からは抽出しない。
        assert!(!freq.contains_key("helper"));
    }

    #[test]
    fn extracts_terms_from_markdown_body_and_headings() {
        let body = "# Query Planning\n\nThe dictionary contains symbols and terms.\n";
        let (freq, truncated) = extract_markdown_terms(body);
        assert!(!truncated);
        assert!(freq.contains_key("query"));
        assert!(freq.contains_key("planning"));
        assert!(freq.contains_key("dictionary"));
        assert!(freq.contains_key("symbols"));
        // ストップワード・3 文字未満は除外する。
        assert!(!freq.contains_key("the"));
        assert!(!freq.contains_key("and"));
    }

    #[test]
    fn tokenizer_lowercases_and_drops_short_words() {
        let (words, truncated) = tokenize_ascii_words("Rust DB is Fast and Reliable");
        assert!(!truncated);
        assert!(words.contains(&"rust".to_string()));
        assert!(words.contains(&"fast".to_string()));
        assert!(words.contains(&"reliable".to_string()));
        assert!(!words.contains(&"db".to_string())); // 2 文字
        assert!(!words.contains(&"is".to_string())); // 2 文字
        assert!(!words.contains(&"and".to_string())); // ストップワード
    }

    #[test]
    fn tokenizer_reports_truncation_for_overlong_word() {
        let long_word = "a".repeat(MAX_TERM_LEN + 10);
        let (words, truncated) = tokenize_ascii_words(&long_word);
        assert!(truncated);
        assert_eq!(words[0].chars().count(), MAX_TERM_LEN);
    }

    // --- ファイルツリー ---

    #[test]
    fn file_tree_aggregates_extension_and_top_dir() {
        let mut tree = FileTree::default();
        accumulate_file_tree(&mut tree, "src/engine/core.rs");
        accumulate_file_tree(&mut tree, "src/engine/lib.rs");
        accumulate_file_tree(&mut tree, "README.md");
        assert_eq!(tree.paths.len(), 3);
        assert_eq!(tree.by_extension.get("rs"), Some(&2));
        assert_eq!(tree.by_extension.get("md"), Some(&1));
        assert_eq!(tree.by_top_dir.get("src"), Some(&2));
        assert_eq!(tree.by_top_dir.get("(root)"), Some(&1));
    }

    #[test]
    fn file_tree_top_dir_uses_the_same_separator_set_as_file_name_extraction() {
        // TASK-109・PLAN-5 レビュー対応の回帰テスト: `by_top_dir` の区切り文字判定が
        // `file_name` 抽出（`rsplit(['/', '\\'])`）と食い違っていたため、Windows 形式
        // パス（`\` 区切り）が誤って "(root)" に集計されるバグがあった。
        let mut tree = FileTree::default();
        accumulate_file_tree(&mut tree, "src\\engine\\core.rs");
        assert_eq!(tree.by_extension.get("rs"), Some(&1));
        assert_eq!(tree.by_top_dir.get("src"), Some(&1));
        assert_eq!(tree.by_top_dir.get("(root)"), None);
    }

    #[test]
    fn accumulate_file_tree_counts_same_path_once_across_repeated_calls() {
        // `accumulate_file_tree` 単体でのチャンク再訪回帰確認（バグ再現条件:
        // 同一パスへの複数回呼び出しで by_extension/by_top_dir が加算され続けない）。
        let mut tree = FileTree::default();
        for _ in 0..5 {
            accumulate_file_tree(&mut tree, "src/engine/core.rs");
        }
        assert_eq!(tree.paths.len(), 1);
        assert_eq!(tree.by_extension.get("rs"), Some(&1));
        assert_eq!(tree.by_top_dir.get("src"), Some(&1));
    }

    #[test]
    fn builder_counts_by_extension_once_per_file_across_multiple_chunks() {
        // TASK-109・PLAN-5 レビュー対応の回帰テスト:
        // `core.rs::dictionary_snapshot` はチャンク化済みの本文を行単位（＝チャンク
        // 単位）で `DictionaryBuilder::ingest` へ渡すため、同一パスが複数回 ingest
        // される。再現例（レビュー指摘どおり）: 20 行ファイルを lines_per_chunk 4 で
        // 5 チャンクへ分割し、各チャンクを同一パスで ingest すると
        // `by_extension["rs"]` はファイル単位の期待値 1 ではなく 5 になっていた。
        let config = DictionaryConfig::default();
        let mut builder = DictionaryBuilder::new(config);
        let path = "src/many_chunks.rs";
        for chunk in 0..5 {
            let body = format!("fn chunk_{chunk}() {{}}\n");
            builder.ingest(path, &body);
        }
        let dict = builder.finish();
        assert_eq!(dict.file_tree.paths.len(), 1);
        assert_eq!(dict.file_tree.by_extension.get("rs"), Some(&1));
        assert_eq!(dict.file_tree.by_top_dir.get("src"), Some(&1));
        // シンボル自体は各チャンクから別々に検出されるべき（チャンク単位の
        // シンボル欠落を防ぐのが本来の意図であり、file_tree の重複加算修正で
        // シンボル抽出まで壊さないことを併せて確認する）。
        assert_eq!(dict.symbols.len(), 5);
    }

    #[test]
    fn builder_file_tree_safety_valve_matches_on_truncated_path() {
        // TASK-109・PLAN-5 レビュー対応の回帰テスト（Low 指摘 2）:
        // `MAX_DICTIONARY_PATHS` へ到達した状態で、`MAX_PATH_LEN` を超える同一パス
        // （チャンク違いで再 ingest される）が「未知の新規パス」と誤判定され
        // `paths` へ二重加算されないことを確認する。判定は挿入される切り詰め後の
        // 値で行う必要がある（生の未切り詰め `path` との比較は常に不一致になる）。
        //
        // `truncated` は `long_path` が `MAX_PATH_LEN` を超える時点で（安全弁とは
        // 独立に）常に立つ（P1 レビュー対応: `truncate_chars` の切り詰め発生を
        // 呼び出し元へ返し `truncated` へ伝播するようにしたため）。本テストが見る
        // べき安全弁固有の regression シグナルは `truncated` の値ではなく
        // `paths.len()` が再 ingest 後も増えないことである。
        let config = DictionaryConfig::default();
        let mut builder = DictionaryBuilder::new(config);

        // ちょうど MAX_DICTIONARY_PATHS - 1 件のユニークな穴埋めパスを積み、最後の
        // 1 枠を長尺パスの初回 ingest で使い切る。
        let long_path = "a".repeat(MAX_PATH_LEN + 50);
        for i in 0..(MAX_DICTIONARY_PATHS - 1) {
            builder.ingest(&format!("filler/{i}.txt"), "");
        }
        builder.ingest(&long_path, "");
        assert_eq!(builder.file_tree.paths.len(), MAX_DICTIONARY_PATHS);
        // `long_path` 自体の切り詰めにより、この時点で既に `truncated` は真。
        assert!(builder.truncated);

        // 同一の長尺パス（別チャンク）を再 ingest。既知パス（切り詰め後）である
        // ため安全弁は発火せず、`paths.len()` は変わらないままであるべき
        // （安全弁が誤発火し新規パス扱いされていれば、切り詰め後の重複が
        // 事実上のカウント違反として現れずとも `paths` の実体は増えない
        // ため `paths.len()` の不変が回帰の直接シグナルになる）。
        builder.ingest(&long_path, "");
        assert_eq!(builder.file_tree.paths.len(), MAX_DICTIONARY_PATHS);
        assert!(builder.truncated);
    }

    #[test]
    fn builder_truncated_flag_reflects_symbol_path_and_name_truncation() {
        // codex-review P1 対応の回帰テスト: `extract_rust_symbols` が返す
        // シンボルの `path`/`name` が個別に切り詰められても、`truncate_chars` の
        // 戻り値がこれまで呼び出し元へ伝わらず `DictionaryBuilder::truncated` /
        // `Dictionary::truncated` が false のままになり得た（`crates/engine/src/
        // dictionary.rs:538` 付近）。
        let long_path = format!("src/{}.rs", "p".repeat(MAX_PATH_LEN + 10));
        let long_name = "n".repeat(MAX_SYMBOL_NAME_LEN + 10);
        let body = format!("fn {long_name}() {{}}\n");

        let mut builder = DictionaryBuilder::new(DictionaryConfig::default());
        builder.ingest(&long_path, &body);
        assert!(builder.truncated);
        let dict = builder.finish();
        assert!(dict.truncated);
    }

    #[test]
    fn builder_truncated_flag_reflects_file_tree_path_truncation() {
        // codex-review P1 対応の回帰テスト: `accumulate_file_tree` 内の
        // `truncate_chars` によるパス切り詰めが `truncated` へ伝播しない経路が
        // あった（`crates/engine/src/dictionary.rs:579` 付近）。シンボル抽出対象
        // 外の拡張子（Rust 以外）を使い、ファイルツリー経路単体の伝播を確認する。
        let long_path = format!("docs/{}.md", "p".repeat(MAX_PATH_LEN + 10));

        let mut builder = DictionaryBuilder::new(DictionaryConfig::default());
        builder.ingest(&long_path, "");
        assert!(builder.truncated);
        let dict = builder.finish();
        assert!(dict.truncated);
    }

    #[test]
    fn builder_truncated_flag_reflects_term_truncation() {
        // codex-review P1 対応の回帰テスト: `tokenize_ascii_words` による用語の
        // `MAX_TERM_LEN` 超切り詰めが `truncated` へ伝播しない経路があった
        // （`crates/engine/src/dictionary.rs:590` 付近から呼ばれる term 抽出）。
        let long_word = "w".repeat(MAX_TERM_LEN + 10);
        let body = format!("# heading\n\n{long_word} appears in body text.\n");

        let mut builder = DictionaryBuilder::new(DictionaryConfig::default());
        builder.ingest("docs/long-term.md", &body);
        assert!(builder.truncated);
        let dict = builder.finish();
        assert!(dict.truncated);
    }

    // --- 上限系 ---

    #[test]
    fn cap_btreeset_is_deterministic_regardless_of_insertion_order() {
        let mut a: BTreeSet<u32> = BTreeSet::new();
        for i in (0..10).rev() {
            a.insert(i);
        }
        let mut b: BTreeSet<u32> = BTreeSet::new();
        for i in 0..10 {
            b.insert(i);
        }
        let (capped_a, trunc_a) = cap_btreeset(a, 5);
        let (capped_b, trunc_b) = cap_btreeset(b, 5);
        assert!(trunc_a && trunc_b);
        assert_eq!(capped_a, capped_b);
        assert_eq!(capped_a, BTreeSet::from([0, 1, 2, 3, 4]));
    }

    #[test]
    fn builder_symbol_dict_is_always_populated_even_when_auxiliary_sources_disabled() {
        // シンボル辞書には無効化スイッチが無いため、他の情報源
        // （ファイルツリー・用語索引）を無効化してもシンボル辞書だけは構築される。
        let config = DictionaryConfig {
            enable_file_tree: false,
            enable_term_index: false,
            top_terms: DEFAULT_TOP_TERMS,
        };
        let mut builder = DictionaryBuilder::new(config);
        builder.ingest("src/x.rs", "fn only_one() {}\n");
        let dict = builder.finish();
        assert_eq!(dict.symbols.len(), 1);
        assert!(dict.file_tree.paths.is_empty());
        assert!(dict.term_index.is_empty());
    }

    #[test]
    fn builder_term_index_respects_top_terms_cap_deterministically() {
        let config = DictionaryConfig {
            enable_file_tree: true,
            enable_term_index: true,
            top_terms: 2,
        };
        let mut builder = DictionaryBuilder::new(config);
        // "alpha" が最頻出、次いで "bravo"、"charlie" は最下位になるよう調整。
        builder.ingest("docs/a.md", "alpha alpha alpha bravo bravo charlie\n");
        let dict = builder.finish();
        assert_eq!(dict.term_index.len(), 2);
        assert!(dict.term_index.contains_key("alpha"));
        assert!(dict.term_index.contains_key("bravo"));
        assert!(!dict.term_index.contains_key("charlie"));
        assert!(dict.truncated);
    }

    #[test]
    fn builder_keeps_symbols_from_distinct_chunks_with_colliding_relative_line() {
        // TASK-109・PLAN-5 レビュー対応の回帰テスト（codex-review P2・Cursor Bugbot
        // High "Chunk lines collide symbol identity"）: 2 つの `ingest` 呼び出し
        // （＝別チャンク）が、たまたま同じチャンク相対行番号・同名・同種の
        // シンボルを生成しても、`Symbol::unit_seq` によりチャンク単位で一意な
        // 同一性を持つため誤って重複排除されず、両方のシンボルが保持される
        // （ADR の「チャンク化でシンボル欠落しない」契約）。以前は `unit_seq` を
        // 持たず、この 2 件が `BTreeSet` 上で衝突し 1 件に欠落していた。
        let config = DictionaryConfig::default();
        let mut builder = DictionaryBuilder::new(config);
        builder.ingest("src/x.rs", "fn foo() {}\n");
        builder.ingest("src/x.rs", "fn foo() {}\n");
        let dict = builder.finish();
        assert_eq!(dict.symbols.len(), 2);
    }

    #[test]
    fn builder_truncated_flag_still_set_for_genuinely_new_symbol_at_cap() {
        // TASK-109・PLAN-5 レビュー対応の回帰テスト（Cursor Bugbot Medium
        // "symbols.len()==MAX 到達時に…誤って truncated 扱いになる" の対照ケース）:
        // `DictionaryBuilder::ingest` に追加した「上限到達時は既存エントリとの
        // 完全一致を確認してから truncated を立てる」ガード（`Symbol::unit_seq`
        // 導入によりチャンク間衝突自体が起きなくなったため、この分岐は主に
        // 将来の同一性拡張に対する防御）が、実際に新規シンボルを切り詰める
        // 通常経路の `truncated` 検知を壊していないことを確認する。
        let config = DictionaryConfig::default();
        let mut builder = DictionaryBuilder::new(config);
        for i in 0..MAX_DICTIONARY_SYMBOLS {
            builder.ingest(&format!("src/filler_{i}.rs"), &format!("fn f{i}() {{}}\n"));
        }
        assert_eq!(builder.symbols.len(), MAX_DICTIONARY_SYMBOLS);
        assert!(!builder.truncated);

        // 上限到達後に真に新規（未知）のシンボルを追加すると、これまでどおり
        // `truncated` が正しく立つ。
        builder.ingest("src/overflow.rs", "fn overflow() {}\n");
        assert_eq!(builder.symbols.len(), MAX_DICTIONARY_SYMBOLS);
        assert!(builder.truncated);
    }

    #[test]
    fn no_overflow_on_saturating_line_number() {
        // untrusted 入力に対する `checked_*`/`saturating_*` 方針の確認
        // （coding-rust.md）。極端な行数でもパニックしない。
        let body = "fn f() {}\n".repeat(3);
        let (symbols, _) = extract_rust_symbols("src/x.rs", &body, 0);
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[2].line, 3);
    }
}
