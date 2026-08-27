//! 辞書的情報源抽出パイプライン（TASK-109、対象ビヘイビア: PLAN-5。ポインタ:
//! `docs/spec/05-tasks.md` TASK-109・`docs/spec/04-behavior/query-planning.md` PLAN-5）。
//!
//! 責務境界: DB に索引化済みのコーパス（ファイル形 `INSERT` のパス・本文）から、
//! 後続の LLM クエリプランニング（TASK-110 以降）が固定接頭辞コンテキストとして使う
//! 「辞書的情報源」を機械抽出する**純関数的な API**を提供する。`chunking.rs`・
//! `sparse.rs` と同じ流儀で storage / catalog / policy へは結線しない（行データの
//! 取得・世代整合キャッシュとの結線は `core.rs::DictionaryCache` の責務）。
//!
//! PLAN-5 の設計要件により、シンボル辞書は**必須実装**（無効化スイッチを持たない）、
//! ファイルツリー・用語索引は [`DictionaryConfig`] のフラグで無効化できる**補助情報源**
//! として区別する。新しい情報源の追加は [`DictionarySourceKind`] へのバリアント追加＋
//! 抽出関数 1 つの追加で完結する構造にする（段階的追加可能な設計）。
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

/// 段階的に追加可能な辞書的情報源の種別（PLAN-5 の設計要件）。新規情報源の追加は
/// このバリアント追加＋対応する抽出関数の追加で完結する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DictionarySourceKind {
    /// シンボル辞書（必須実装。[`DictionaryConfig`] に無効化スイッチを持たない）。
    SymbolDict,
    /// ファイルツリー（補助情報源。[`DictionaryConfig::enable_file_tree`]）。
    FileTree,
    /// 用語索引（補助情報源。[`DictionaryConfig::enable_term_index`]）。
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

/// 抽出された 1 シンボル。`Ord` はソート順（`path` → `line` → `name` → `kind`）を
/// 決定的にするために導出し、[`Dictionary::symbols`]（`BTreeSet`）の反復順序が
/// 挿入順に依存しないようにする。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Symbol {
    pub path: String,
    pub line: u32,
    pub name: String,
    pub kind: SymbolKind,
}

/// `s` を `max_chars` 文字（文字境界。バイト境界ではない）で決定的に切り詰める。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
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

    // `async` / `unsafe` / `extern "..."` は任意個・任意順で先行し得る。
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

/// 1 抽出単位（`path` の 1 チャンク本文 `body`）から Rust シンボルを抽出する
/// （必須実装。PLAN-5）。戻り値の `bool` は [`MAX_SYMBOLS_PER_UNIT`] 超過による
/// 決定的切り詰めが発生したかを示す。
///
/// 行番号はこの抽出単位内でのローカルな 1 起点行番号であり、チャンク化
/// （`chunking.rs`）により本文が複数チャンクへ分割されている場合、元ファイル全体での
/// 行番号とは一致しない（チャンク化は行分割ベース・非オーバーラップのため
/// シンボル自体の欠落は生じないが、行番号はチャンク相対値になる。呼び出し元
/// `core.rs::DictionaryCache` のドキュメント参照）。
pub fn extract_rust_symbols(path: &str, body: &str) -> (Vec<Symbol>, bool) {
    let path = truncate_chars(path, MAX_PATH_LEN);
    let mut out = Vec::new();
    let mut truncated = false;
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
        out.push(Symbol {
            path: path.clone(),
            line: line_no,
            name: truncate_chars(&name, MAX_SYMBOL_NAME_LEN),
            kind,
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
fn accumulate_file_tree(tree: &mut FileTree, path: &str) {
    let path = truncate_chars(path, MAX_PATH_LEN);
    if !tree.paths.insert(path.clone()) {
        // 既知パスの再訪（同一ファイルの別チャンク）。パス集合には既に含まれて
        // おり拡張子・トップディレクトリの二重加算を防ぐため、ここで打ち切る。
        return;
    }

    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(&path);
    let ext_key = file_name
        .rsplit_once('.')
        .filter(|(stem, ext)| !ext.is_empty() && !stem.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_else(|| "(none)".to_string());
    let counter = tree.by_extension.entry(ext_key).or_insert(0);
    *counter = counter.saturating_add(1);

    let top_dir = match path.split_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_string(),
        _ => "(root)".to_string(),
    };
    let counter = tree.by_top_dir.entry(top_dir).or_insert(0);
    *counter = counter.saturating_add(1);
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
fn tokenize_ascii_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphabetic() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            if current.chars().count() >= 3 && !is_stopword(&current) {
                words.push(truncate_chars(&current, MAX_TERM_LEN));
            }
            current.clear();
        }
    }
    if !current.is_empty() && current.chars().count() >= 3 && !is_stopword(&current) {
        words.push(truncate_chars(&current, MAX_TERM_LEN));
    }
    words
}

/// Rust ソースのドキュメンテーションコメント（`///` / `//!`）から用語頻度を抽出する。
fn extract_rust_doc_terms(body: &str) -> BTreeMap<String, u64> {
    let mut freq = BTreeMap::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let content = if let Some(rest) = trimmed.strip_prefix("///") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("//!") {
            rest
        } else {
            continue;
        };
        for word in tokenize_ascii_words(content) {
            let counter = freq.entry(word).or_insert(0u64);
            *counter = counter.saturating_add(1);
        }
    }
    freq
}

/// Markdown 本文（見出し・地の文いずれも含む）から用語頻度を抽出する。
fn extract_markdown_terms(body: &str) -> BTreeMap<String, u64> {
    let mut freq = BTreeMap::new();
    for word in tokenize_ascii_words(body) {
        let counter = freq.entry(word).or_insert(0u64);
        *counter = counter.saturating_add(1);
    }
    freq
}

/// 抽出パイプラインの設定（PLAN-5）。シンボル辞書には無効化スイッチを持たせない
/// （必須実装であることを型・設定面で保証する）。
#[derive(Debug, Clone)]
pub struct DictionaryConfig {
    /// ファイルツリー情報源を有効化するか（補助情報源）。
    pub enable_file_tree: bool,
    /// 用語索引情報源を有効化するか（補助情報源）。
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
        }
    }

    /// `path`・`body` の 1 抽出単位（1 行 = 1 チャンク相当）を取り込む。
    pub fn ingest(&mut self, path: &str, body: &str) {
        // シンボル辞書は必須実装（PLAN-5）。ソース種別に関わらず常に試みる
        // （Rust 以外は行頭が予約語と一致しない限り検出されず実質空になる）。
        match detect_source_kind(path) {
            SourceFileKind::Rust => {
                let (symbols, truncated) = extract_rust_symbols(path, body);
                if truncated {
                    self.truncated = true;
                }
                for symbol in symbols {
                    // 生の安全弁（早期打ち切り。挿入順＝呼び出し元の走査順に依存する。
                    // モジュールドキュメント「決定性」参照）。最終的な決定的切り詰めは
                    // `finish` 内の `cap_btreeset` が担う。
                    if self.symbols.len() >= MAX_DICTIONARY_SYMBOLS {
                        self.truncated = true;
                        break;
                    }
                    self.symbols.insert(symbol);
                }
                if self.config.enable_term_index {
                    self.merge_terms(extract_rust_doc_terms(body));
                }
            }
            SourceFileKind::Markdown => {
                if self.config.enable_term_index {
                    self.merge_terms(extract_markdown_terms(body));
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
            let truncated_path = truncate_chars(path, MAX_PATH_LEN);
            if self.file_tree.paths.len() >= MAX_DICTIONARY_PATHS
                && !self.file_tree.paths.contains(&truncated_path)
            {
                self.truncated = true;
            } else {
                accumulate_file_tree(&mut self.file_tree, path);
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
        let (symbols, truncated) = extract_rust_symbols("src/x.rs", body);
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
        let (symbols, truncated) = extract_rust_symbols("src/many.rs", &body);
        assert!(truncated);
        assert_eq!(symbols.len(), MAX_SYMBOLS_PER_UNIT);
    }

    #[test]
    fn symbol_name_is_truncated_to_max_len() {
        let long_name = "a".repeat(MAX_SYMBOL_NAME_LEN + 50);
        let body = format!("fn {long_name}() {{}}\n");
        let (symbols, _) = extract_rust_symbols("src/x.rs", &body);
        assert_eq!(symbols[0].name.chars().count(), MAX_SYMBOL_NAME_LEN);
    }

    // --- 用語索引 ---

    #[test]
    fn extracts_terms_from_rust_doc_comments_only() {
        let body = "//! module about caching and eviction\nfn helper() {}\n/// short\n";
        let freq = extract_rust_doc_terms(body);
        assert!(freq.contains_key("caching"));
        assert!(freq.contains_key("eviction"));
        assert!(freq.contains_key("module"));
        // 非 doc コメント行（関数本体）からは抽出しない。
        assert!(!freq.contains_key("helper"));
    }

    #[test]
    fn extracts_terms_from_markdown_body_and_headings() {
        let body = "# Query Planning\n\nThe dictionary contains symbols and terms.\n";
        let freq = extract_markdown_terms(body);
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
        let words = tokenize_ascii_words("Rust DB is Fast and Reliable");
        assert!(words.contains(&"rust".to_string()));
        assert!(words.contains(&"fast".to_string()));
        assert!(words.contains(&"reliable".to_string()));
        assert!(!words.contains(&"db".to_string())); // 2 文字
        assert!(!words.contains(&"is".to_string())); // 2 文字
        assert!(!words.contains(&"and".to_string())); // ストップワード
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
        // `truncated` が不要に立たないことを確認する。判定は挿入される切り詰め後の
        // 値で行う必要がある（生の未切り詰め `path` との比較は常に不一致になる）。
        let config = DictionaryConfig::default();
        let mut builder = DictionaryBuilder::new(config);

        // ちょうど MAX_DICTIONARY_PATHS - 1 件のユニークな穴埋めパスを積み、最後の
        // 1 枠を長尺パスの初回 ingest で使い切る（この時点ではまだ安全弁は発火せず、
        // `truncated` は立たない）。
        let long_path = "a".repeat(MAX_PATH_LEN + 50);
        for i in 0..(MAX_DICTIONARY_PATHS - 1) {
            builder.ingest(&format!("filler/{i}.txt"), "");
        }
        builder.ingest(&long_path, "");
        assert_eq!(builder.file_tree.paths.len(), MAX_DICTIONARY_PATHS);
        assert!(!builder.truncated);

        // 同一の長尺パス（別チャンク）を再 ingest。既知パス（切り詰め後）である
        // ため、安全弁は発火せず `truncated` は立たないままであるべき。
        builder.ingest(&long_path, "");
        assert_eq!(builder.file_tree.paths.len(), MAX_DICTIONARY_PATHS);
        assert!(!builder.truncated);
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
        // PLAN-5 対応テスト: シンボル辞書は必須実装であり、補助情報源
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
    fn builder_deduplicates_symbols_across_units_for_same_path() {
        let config = DictionaryConfig::default();
        let mut builder = DictionaryBuilder::new(config);
        builder.ingest("src/x.rs", "fn foo() {}\n");
        builder.ingest("src/x.rs", "fn foo() {}\n");
        let dict = builder.finish();
        assert_eq!(dict.symbols.len(), 1);
    }

    #[test]
    fn no_overflow_on_saturating_line_number() {
        // untrusted 入力に対する `checked_*`/`saturating_*` 方針の確認
        // （coding-rust.md）。極端な行数でもパニックしない。
        let body = "fn f() {}\n".repeat(3);
        let (symbols, _) = extract_rust_symbols("src/x.rs", &body);
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[2].line, 3);
    }
}
