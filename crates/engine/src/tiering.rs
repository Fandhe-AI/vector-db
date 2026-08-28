//! 質問類型推定・ティアリング（TASK-115、対象ビヘイビア: PLAN-8。ポインタ:
//! `docs/spec/05-tasks.md` TASK-115）。
//!
//! 責務境界: 自然言語の質問文と辞書的情報源（TASK-109・`dictionary.rs`）から、
//! 質問の類型（[`QuestionClass`]）と対話ティア／高精度ティアのどちらへ振り分けるか
//! （[`Tier`]）を決定的・純粋に判定する層を提供する。`storage`/`catalog`/`policy`
//! へは一切結線しない（`dictionary.rs`・`query_planner.rs` と同じ流儀）。
//!
//! ティア別 [`crate::query_planner::LlmClient`] の実際の注入・ルーティング統合は
//! `core.rs::EngineCore::with_tiered_query_planner` / `core.rs::EngineCore::plan_query`
//! の管轄とし、本モジュールは判定ロジックのみを持つ（`query_planner.rs`
//! モジュールドキュメントが担う「プロンプト構築・LLM 呼び出し・応答パース」との
//! 責務境界を保つ）。
//!
//! 類型→ティア割り当て・判定基準・fail-safe の方向は本リポの実装既定値である
//! （[`tier_for_class`]・[`TieringCriteria`]・[`classify`] の各ドキュメンテーション
//! コメント、または `docs/design/query-tiering-criteria.md` 参照）。
//!
//! fail-safe の方向（security.md「不安全な設計」対応）: 判定が不確実・空入力・
//! 上限超過などの縮退時は [`QuestionClass::Intent`]（＝高精度ティア）へ倒す。
//! 誤って対話ティア（軽量）に倒して品質劣化するより、高精度ティアでレイテンシを
//! 払う側を安全側とする（「正解を含むデータ群を広く返す」設計思想と整合）。
//! 同じ理由で、抽象的な手掛かり語（[`TieringCriteria::abstraction_cues`]）と
//! 辞書シンボル名の両方に一致した質問は [`QuestionClass::Abstraction`]（＝高精度
//! ティア）を優先する（[`classify`] ドキュメンテーションコメント「優先順」参照。
//! Bugbot 指摘対応・PR #261。この優先順を逆に「戻す」修正はしないこと）。
//!
//! untrusted 入力対応: `question` は wire 経由の未検証入力であるため、添字アクセス・
//! `unwrap`/`expect` を使わず、決定的・線形時間のトークナイズのみを行う
//! （coding-rust.md「untrusted 入力の扱い」）。長さ・トークン数はいずれも上限
//! （[`MAX_QUESTION_CHARS`]・[`MAX_TOKENS`]）で頭打ちにし、無制限確保を避ける。
//! トークン数が [`MAX_TOKENS`] を超える入力は、判定材料を黙って切り捨てて誤分類する
//! のではなく [`ClassificationSignal::Degenerate`] の fail-safe 側へ倒す（超過を検出
//! できない切り捨てのまま `classify` へ渡さない）。トークンの前後に付く句読点
//! （`?`・`,`・全角句読点等）は比較前に除去し、自然文中のシンボル名・パス様トークンの
//! 認識を妨げないようにする。トークン内部の英語短縮形接尾辞（`'s`・`n't` 等）も
//! 末尾から除去し基底語へ正規化する（[`strip_contraction_suffix`] 参照。Bugbot
//! 指摘対応・PR #261。`what's` が手掛かり語 `what` と一致しないままシンボル一致へ
//! フォールスルーし対話ティアへ誤ルーティングされる問題への対応）。

use std::collections::BTreeSet;

use crate::dictionary::Dictionary;

/// 質問文として判定に用いる最大文字数（[`crate::query_planner::MAX_QUESTION_CHARS`]
/// と同じ上限を再利用し、`plan_query` 経路全体で一貫した有界化契約を保つ）。
pub const MAX_QUESTION_CHARS: usize = crate::query_planner::MAX_QUESTION_CHARS;

/// 判定のトークナイズで走査するトークン数の上限（DoS 耐性: 空白区切りのトークンが
/// 極端に多い入力でも判定処理の線形走査量を頭打ちにする）。
pub const MAX_TOKENS: usize = 256;

/// 質問の類型。既定の類型→ティア割り当ては [`tier_for_class`] を参照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionClass {
    /// 具体的なシンボル名・パスを名指ししている（対話ティア相当）。
    Direct,
    /// 意図はあるが、具体的なシンボル名・パスの手掛かりがない。
    Intent,
    /// 概念・説明を求める抽象的な言い回し。
    Abstraction,
}

/// LLM クエリプランニングのティア（対話ティア＝軽量モデル／高精度ティア＝重めの
/// モデル。モジュールドキュメント参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 対話ティア（軽量モデル）。
    Dialogue,
    /// 高精度ティア（重めのモデル）。
    HighPrecision,
}

/// [`Classification::signal`] が示す判定根拠（観測用。将来の EXPLAIN 露出
/// 〔SQL-6・PLAN-11／TASK-164〕を見据えるが、本モジュールでは露出しない
/// ＝呼び出し元が意図的に外部公開する場合のみ意味を持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationSignal {
    /// 質問中のトークンが辞書のシンボル名に完全一致した。
    SymbolMatch,
    /// 質問中のトークンがファイルパス・拡張子付きトークンに一致した。
    PathMatch,
    /// 概念・説明要求の手掛かり語に一致した。
    AbstractionCue,
    /// 上記いずれにも一致せず、意図型として判定した。
    NoCue,
    /// 判定不能な縮退入力（空・上限超過等）を fail-safe 側へ倒した。
    Degenerate,
}

/// [`classify`] の結果。`tier` は `class` から [`tier_for_class`] で導出した値
/// （常に整合する。呼び出し元が独立に組み立てて不整合を作れない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub class: QuestionClass,
    pub tier: Tier,
    pub signal: ClassificationSignal,
}

/// `class` に対する既定のティア割り当て（本リポの実装既定。モジュールドキュメント
/// 参照）。
pub fn tier_for_class(class: QuestionClass) -> Tier {
    match class {
        QuestionClass::Direct => Tier::Dialogue,
        QuestionClass::Intent | QuestionClass::Abstraction => Tier::HighPrecision,
    }
}

/// 判定基準の調整可能な既定値（[`TieringCriteria::default`]）。手掛かり語・拡張子等の
/// 具体値は本リポの実装既定値として持ち、呼び出し元が差し替え可能にする
/// （`docs/design/query-tiering-criteria.md` 参照）。
#[derive(Debug, Clone)]
pub struct TieringCriteria {
    /// 概念・説明要求の言い回しとみなす手掛かり語（小文字化して比較。ASCII
    /// 小文字化のみ行い、マルチバイト文字はそのまま比較する。モジュール
    /// ドキュメント「untrusted 入力対応」参照）。公開フィールドで呼び出し元が
    /// 差し替え可能な契約のため、値そのものは大文字混じりでも構わない。
    /// `classify` 側が比較のたびに ASCII 小文字化してから照合する
    /// （codex P2 指摘対応・PR #261。`"Explain"` のような未正規化の差し替え値も
    /// 質問側の小文字化トークンと一致する）。
    pub abstraction_cues: BTreeSet<String>,
    /// パス様トークンとみなす拡張子（先頭のドットなし。小文字比較。
    /// `abstraction_cues` と同じく `classify` 側で比較時に ASCII 小文字化する
    /// ため `"RS"` のような大文字混じりの差し替え値も一致する。codex P2 指摘
    /// 対応・PR #261）。
    pub path_like_extensions: BTreeSet<String>,
}

impl Default for TieringCriteria {
    fn default() -> Self {
        let abstraction_cues = [
            "why",
            "how",
            "what",
            "explain",
            "overview",
            "architecture",
            "design",
            "concept",
            "概要",
            "設計",
            "なぜ",
            "理由",
            "とは",
            "仕組み",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let path_like_extensions = ["rs", "toml", "md", "yml", "yaml", "json", "txt"]
            .into_iter()
            .map(str::to_string)
            .collect();

        Self {
            abstraction_cues,
            path_like_extensions,
        }
    }
}

/// ASCII 範囲のみ小文字化する（`str::to_lowercase` はマルチバイト文字の大小文字
/// マッピングにより文字数・バイト境界が変わりうるため、比較専用のこの用途では
/// ASCII 部分だけを決定的に正規化する。日本語手掛かり語はそのまま比較する）。
fn ascii_lower(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// トークンの前後から除去する境界句読点。シンボル名・パスの内部で意味を持つ
/// `_`・`-`・`/` は対象外とする。`.` は末尾側のみの対象とし（[`TRAILING_ONLY`]）、
/// 先頭側では除去しない（`.gitignore` のような先頭ドット付きファイル名を
/// `Dictionary::file_tree` 側の生パスと一致させるため。Bugbot 指摘対応）。バッククォート
/// （\`）も対象に含める（Markdown のコードスパン記法 \`symbol\` で囲んだシンボル名・
/// パスを自然文中で名指しした場合に、除去せず残すと辞書側の生の値と一致しなくなる
/// ため。Bugbot 指摘対応）。
const TOKEN_BOUNDARY_PUNCTUATION: &[char] = &[
    '?', '!', ',', ';', ':', '\'', '"', '`', '(', ')', '[', ']', '{', '}', '<', '>', '、', '。',
    '！', '？', '「', '」', '『', '』',
];

/// [`TOKEN_BOUNDARY_PUNCTUATION`] に加え、末尾側でのみ除去する句読点（`core.rs.`
/// のような文末の `.` を落としつつ内側の拡張子区切りの `.` は保持するため、末尾専用
/// とする。先頭側にも含めると `.gitignore` の先頭ドットまで削られ、パス一致の
/// 判定契約が壊れる）。
const TRAILING_ONLY_PUNCTUATION: char = '.';

/// トークン前後の境界句読点を除去する（`what is parse_expansion?` のような自然文で
/// シンボル・パス認識を妨げないための正規化）。先頭側と末尾側で対象文字集合を分け、
/// 先頭ドットは保持する（[`TOKEN_BOUNDARY_PUNCTUATION`] ドキュメント参照）。
fn strip_boundary_punctuation(token: &str) -> &str {
    token
        .trim_start_matches(|c: char| TOKEN_BOUNDARY_PUNCTUATION.contains(&c))
        .trim_end_matches(|c: char| {
            TOKEN_BOUNDARY_PUNCTUATION.contains(&c) || c == TRAILING_ONLY_PUNCTUATION
        })
}

/// 英語の短縮形（アポストロフィ付き接尾辞）。トークン末尾からこれらを除去して
/// 基底語へ正規化する（[`strip_contraction_suffix`] 参照。長い接尾辞から先に
/// 判定順を並べる必要はない。互いに接尾辞関係を持たないため判定順は結果に
/// 影響しない）。
const CONTRACTION_SUFFIXES: &[&str] = &["n't", "'re", "'ve", "'ll", "'s", "'d", "'m"];

/// `strip_boundary_punctuation` 適用後・ASCII 小文字化後のトークン末尾から、
/// 英語の短縮形接尾辞（`'s`・`n't` 等）を除去し基底語を返す（Cursor Bugbot 指摘
/// 対応・PR #261）。`strip_boundary_punctuation` は先頭・末尾の境界句読点のみを
/// 対象とし、`what's` のようにトークン内部にアポストロフィを持つ短縮形は対象外の
/// ため除去されず、抽象手掛かり語（[`TieringCriteria::abstraction_cues`]）との
/// 完全一致に失敗してシンボル一致へフォールスルーし、対話ティアへ誤ルーティング
/// されうる問題への対応。除去後に空文字列になる場合（トークンが短縮形接尾辞
/// そのものだった場合）は元のトークンをそのまま返し、判定材料を失わない
/// （fail-safe の方向を変えない）。
fn strip_contraction_suffix(token: &str) -> &str {
    for suffix in CONTRACTION_SUFFIXES {
        if let Some(stripped) = token.strip_suffix(suffix) {
            if !stripped.is_empty() {
                return stripped;
            }
        }
    }
    token
}

/// `question` を空白でトークナイズし、[`MAX_TOKENS`] 件までを返す（untrusted 入力の
/// 有界化。制御文字を含むトークンは判定材料として使わずスキップし、境界句読点は
/// 除去する）。戻り値の `bool` は [`MAX_TOKENS`] を超過して切り捨てが発生したかを示す
/// （呼び出し元はこれを見て fail-safe 側へ倒す。切り捨てを黙って進めない）。
/// 超過判定は素の空白区切り件数（フィルタ前）で行うため、[`MAX_TOKENS`] + 1 件先読み
/// する以外は走査量が増えない。
fn tokenize(question: &str) -> (Vec<String>, bool) {
    let raw: Vec<&str> = question.split_whitespace().take(MAX_TOKENS + 1).collect();
    let truncated = raw.len() > MAX_TOKENS;
    let tokens = raw
        .into_iter()
        .take(MAX_TOKENS)
        .filter(|t| !t.chars().any(|c| c.is_control()))
        .map(|t| {
            let lowered = ascii_lower(strip_boundary_punctuation(t));
            strip_contraction_suffix(&lowered).to_string()
        })
        .filter(|t| !t.is_empty())
        .collect();
    (tokens, truncated)
}

/// トークンの拡張子部分（最後の `.` 以降。存在しなければ `None`）を返す。
fn extension_of(token: &str) -> Option<&str> {
    let (_, ext) = token.rsplit_once('.')?;
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

/// [`Dictionary::file_tree`]・[`Dictionary::symbols`] を ASCII 小文字化した
/// 照会専用の索引（codex-review P1 指摘対応・PR #261）。
///
/// 以前の `classify` は呼び出しのたびに `Dictionary::file_tree.paths`・
/// `Dictionary::symbols`（辞書は各最大 20,000 件・パス最大 1,024 文字
/// 〔`dictionary.rs::MAX_DICTIONARY_PATHS`・`MAX_DICTIONARY_SYMBOLS`・
/// `MAX_PATH_LEN`〕）の全要素を小文字化して新しい `String`/`BTreeSet` へ複製して
/// いたため、wire 経由の短い質問を並行送信するだけでリクエストごとに数十 MB 規模の
/// 一時確保と全辞書走査が発生し、メモリ・CPU 枯渇経路になっていた
/// （security.md「不安全な設計」対応）。
///
/// `core.rs::DictionaryCache` は `(table, ctx)` 単位の辞書スナップショット
/// （`Arc<Dictionary>`）を世代整合の間キャッシュし続けるため、本構造体もその
/// スナップショットの構築時（`DictionaryCache::insert`）に一度だけ構築し
/// `Arc` で共有する。以降の `classify` 呼び出しは本構造体への借用のみで完結し、
/// リクエストごとの複製・全量走査は発生しない。
#[derive(Debug, Clone, Default)]
pub struct NormalizedDictionaryIndex {
    /// [`Dictionary::file_tree`] のパスを ASCII 小文字化した集合。
    path_tokens: BTreeSet<String>,
    /// [`Dictionary::symbols`] のシンボル名を ASCII 小文字化した集合。
    symbol_names: BTreeSet<String>,
}

impl NormalizedDictionaryIndex {
    /// `dictionary` から正規化索引を構築する（辞書全量を走査するのはこの構築時
    /// 1 回のみ。呼び出し元は辞書スナップショットの構築時に 1 度だけ呼び出し、
    /// 結果を `Arc` で保持して使い回す契約とする〔`core.rs::DictionaryCache`
    /// 参照〕）。
    pub fn build(dictionary: &Dictionary) -> Self {
        let path_tokens = dictionary
            .file_tree
            .paths
            .iter()
            .map(|p| ascii_lower(p))
            .collect();
        let symbol_names = dictionary
            .symbols
            .iter()
            .map(|s| ascii_lower(&s.name))
            .collect();
        Self {
            path_tokens,
            symbol_names,
        }
    }

    /// キャッシュ容量判定用の概算ヒープバイト数（`core.rs::DictionaryCache` が
    /// 元の [`Dictionary::approx_heap_bytes`] に加算し、正規化索引の保持分
    /// （小文字化パス・シンボル名の複製）も容量上限の対象に含める。
    /// [`Dictionary::approx_heap_bytes`] と同じ粗い概算方針。
    pub fn approx_heap_bytes(&self) -> usize {
        let paths: usize = self
            .path_tokens
            .iter()
            .map(|p| p.len().saturating_add(16))
            .fold(0usize, usize::saturating_add);
        let symbols: usize = self
            .symbol_names
            .iter()
            .map(|s| s.len().saturating_add(16))
            .fold(0usize, usize::saturating_add);
        paths.saturating_add(symbols)
    }
}

/// `question` と `index`（[`NormalizedDictionaryIndex`]。
/// [`crate::core::EngineCore::dictionary_snapshot`] 経由でテナント境界済みの辞書
/// スナップショットから 1 度だけ構築された正規化索引）から質問類型を判定する。
///
/// 呼び出し元は `core.rs::EngineCore::plan_query`（テナント別辞書スナップショット
/// 経由。本関数自体はテナント境界の判断を持たず、渡された `index` をそのまま
/// 使う純粋関数）。引数が生の [`Dictionary`] ではなく [`NormalizedDictionaryIndex`]
/// である点が、リクエストごとの辞書全量複製が発生しないことの契約（型で保証。
/// [`NormalizedDictionaryIndex`] ドキュメント「codex-review P1 指摘対応」参照）。
///
/// 優先順（本リポの実装既定。`docs/design/query-tiering-criteria.md` も参照）:
/// 1. パス様トークン（[`Dictionary::file_tree`] のパスへの一致、または
///    `criteria.path_like_extensions` の拡張子を持つトークン）→ [`QuestionClass::Direct`]
/// 2. `criteria.abstraction_cues` に一致する手掛かり語を含む → [`QuestionClass::Abstraction`]
/// 3. トークンが辞書シンボル名（[`Dictionary::symbols`]）に完全一致 → [`QuestionClass::Direct`]
/// 4. 上記以外 → [`QuestionClass::Intent`]
///
/// パス一致より後段（手掛かり語より後）でシンボル一致を判定する理由（Bugbot 指摘
/// 対応・PR #261）: Rust コーパスの辞書シンボル名には `new`・`main`・`read` のような
/// 一般英語と衝突するありふれた識別子が大量に含まれる。シンボル完全一致を手掛かり語
/// より優先すると、「概要を説明して」のような抽象的な質問が、たまたま本文中に
/// 一致した識別子だけを理由に対話ティア（軽量）へ誤ってルーティングされ、
/// fail-safe の方向（迷ったら高精度側）と逆になる。一方でパス様トークンは
/// `.` + 既知拡張子、または辞書の生パスとの完全一致を要求するため、一般英語との
/// 衝突が起きにくく、従来どおり手掛かり語より優先してよい。
///
/// 空入力・[`MAX_QUESTION_CHARS`] 超過・[`MAX_TOKENS`] 超過は縮退値として fail-safe 側
/// （[`QuestionClass::Intent`]）へ倒す。
pub fn classify(
    question: &str,
    index: &NormalizedDictionaryIndex,
    criteria: &TieringCriteria,
) -> Classification {
    // untrusted 入力（wire 経由）に対する走査量の頭打ち契約: 上限超過検知は
    // `MAX_QUESTION_CHARS + 1` 文字先読みで打ち切り、全体走査を避ける
    // （codex-review 指摘対応。CPU DoS 経路を防ぐ）。空判定もこの有界走査へ
    // 統合する: 先読み範囲内に非空白文字が 1 つも無ければ、上限超過であっても
    // 非超過であっても縮退（空相当）とみなしてよい。上限超過の場合は後続の
    // `too_long` 判定で既に縮退が確定するため、`is_whitespace` 判定を先読み
    // 範囲に限定しても classify の結果は元の全体走査版と一致する
    // （`str::trim` と同じ Unicode 空白判定 `char::is_whitespace` を使う）。
    let mut bounded_len: usize = 0;
    let mut has_non_whitespace = false;
    for c in question.chars().take(MAX_QUESTION_CHARS + 1) {
        bounded_len += 1;
        if !c.is_whitespace() {
            has_non_whitespace = true;
        }
    }
    let too_long = bounded_len > MAX_QUESTION_CHARS;
    let degenerate = !has_non_whitespace || too_long;
    if degenerate {
        return fail_safe(ClassificationSignal::Degenerate);
    }

    let (tokens, truncated) = tokenize(question);
    if truncated {
        return fail_safe(ClassificationSignal::Degenerate);
    }
    if tokens.is_empty() {
        return fail_safe(ClassificationSignal::Degenerate);
    }

    // パス様トークン（拡張子付き、または辞書の生パスとの完全一致）を最優先で
    // 判定する。一般英語との衝突が起きにくく、手掛かり語より先に判定してよい
    // （`classify` ドキュメンテーションコメント「優先順」参照）。`index` は辞書
    // スナップショット構築時に 1 度だけ正規化済みのため、ここでは借用による
    // 照会のみで完結する（[`NormalizedDictionaryIndex`] ドキュメント参照）。
    // `criteria` は呼び出し元が差し替え可能な公開フィールドであり、値の大小文字は
    // 保証されない。質問側トークンは `tokenize` で既に ASCII 小文字化済みのため、
    // 基準値側も比較のたびに ASCII 小文字化してから照合する（codex P2 指摘対応・
    // PR #261。`TieringCriteria` ドキュメンテーションコメント参照）。
    let normalized_path_extensions: BTreeSet<String> = criteria
        .path_like_extensions
        .iter()
        .map(|ext| ascii_lower(ext))
        .collect();
    let has_path_match = tokens.iter().any(|t| {
        index.path_tokens.contains(t)
            || extension_of(t)
                .map(|ext| normalized_path_extensions.contains(ext))
                .unwrap_or(false)
    });
    if has_path_match {
        return make(QuestionClass::Direct, ClassificationSignal::PathMatch);
    }

    // 手掛かり語一致は辞書シンボル一致より先に判定する（Bugbot 指摘対応・PR #261。
    // `classify` ドキュメンテーションコメント「優先順」参照）。ASCII cue（英語
    // 手掛かり語）はトークン完全一致のまま維持し、通常の英単語（"design" など）
    // が他語の部分文字列として誤爆しないようにする。一方で日本語（非 ASCII）cue
    // は日本語文に語間空白が無いため、空白区切りトークンとの完全一致がほぼ
    // 成立しない（codex-review 指摘: PR #261）。非 ASCII cue のみ、正規化済み
    // 質問文字列全体への部分一致（substring）で判定する。fail-safe の方向は
    // 変えず、判定漏れ（見逃し）を減らす側の変更にとどめる。
    let normalized_question = ascii_lower(question);
    let has_abstraction_cue = criteria.abstraction_cues.iter().any(|cue| {
        // 基準値側も比較のたびに ASCII 小文字化する（`normalized_path_extensions`
        // と同じ理由。codex P2 指摘対応・PR #261）。
        let normalized_cue = ascii_lower(cue);
        if normalized_cue.is_ascii() {
            tokens.iter().any(|t| t == &normalized_cue)
        } else {
            normalized_question.contains(normalized_cue.as_str())
        }
    });
    if has_abstraction_cue {
        return make(
            QuestionClass::Abstraction,
            ClassificationSignal::AbstractionCue,
        );
    }

    // シンボル名（`BTreeSet<Symbol>` の決定的な反復順序。`dictionary.rs`
    // モジュールドキュメント「決定性」参照）と ASCII 小文字化トークンを比較する。
    // 手掛かり語一致より後段で判定する理由は `classify` ドキュメンテーション
    // コメント「優先順」参照（Bugbot 指摘対応・PR #261）。`index.symbol_names` は
    // 辞書スナップショット構築時に 1 度だけ正規化済み（[`NormalizedDictionaryIndex`]
    // ドキュメント参照）。
    if tokens.iter().any(|t| index.symbol_names.contains(t)) {
        return make(QuestionClass::Direct, ClassificationSignal::SymbolMatch);
    }

    make(QuestionClass::Intent, ClassificationSignal::NoCue)
}

fn make(class: QuestionClass, signal: ClassificationSignal) -> Classification {
    Classification {
        class,
        tier: tier_for_class(class),
        signal,
    }
}

/// 縮退時の fail-safe 判定（[`QuestionClass::Intent`]＝高精度ティアへ倒す。
/// モジュールドキュメント「fail-safe の方向」参照）。
fn fail_safe(signal: ClassificationSignal) -> Classification {
    make(QuestionClass::Intent, signal)
}

/// ティア別 [`crate::query_planner::LlmClient`] を束ね、[`classify`] の結果に応じて
/// どちらのクライアントを使うかを選択する。`core.rs::EngineCore` の
/// `PlannerBinding::Tiered` 経由でのみ構築される（本モジュール自体は `EngineCore` へ
/// 結線しない。モジュールドキュメント参照）。
pub struct TieredPlanner {
    dialogue: Box<dyn crate::query_planner::LlmClient>,
    high_precision: Box<dyn crate::query_planner::LlmClient>,
    criteria: TieringCriteria,
}

impl TieredPlanner {
    pub fn new(
        dialogue: Box<dyn crate::query_planner::LlmClient>,
        high_precision: Box<dyn crate::query_planner::LlmClient>,
        criteria: TieringCriteria,
    ) -> Self {
        Self {
            dialogue,
            high_precision,
            criteria,
        }
    }

    /// `question` を `index`（[`NormalizedDictionaryIndex`]。辞書スナップショット
    /// 構築時に 1 度だけ正規化済み）に基づき分類し、対応するティアの
    /// [`crate::query_planner::LlmClient`] とその判定結果を返す。
    pub fn select(
        &self,
        question: &str,
        index: &NormalizedDictionaryIndex,
    ) -> (&dyn crate::query_planner::LlmClient, Classification) {
        let classification = classify(question, index, &self.criteria);
        let client: &dyn crate::query_planner::LlmClient = match classification.tier {
            Tier::Dialogue => self.dialogue.as_ref(),
            Tier::HighPrecision => self.high_precision.as_ref(),
        };
        (client, classification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::{Dictionary, FileTree, Symbol, SymbolKind};
    use crate::query_planner::{LlmClient, PlanError};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn empty_dictionary() -> Dictionary {
        Dictionary {
            symbols: BTreeSet::new(),
            file_tree: FileTree {
                paths: BTreeSet::new(),
                by_extension: BTreeMap::new(),
                by_top_dir: BTreeMap::new(),
            },
            term_index: BTreeMap::new(),
            truncated: false,
        }
    }

    fn dictionary_with_symbol(name: &str, path: &str) -> Dictionary {
        let mut dict = empty_dictionary();
        dict.symbols.insert(Symbol {
            path: path.to_string(),
            line: 1,
            name: name.to_string(),
            kind: SymbolKind::Fn,
            unit_seq: 0,
        });
        dict.file_tree.paths.insert(path.to_string());
        dict
    }

    #[test]
    fn symbol_match_yields_direct_and_dialogue_tier() {
        let dict = dictionary_with_symbol("parse_expansion", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        // 手掛かり語を含まない質問文で純粋なシンボル一致を検証する（手掛かり語が
        // 混在する場合は Abstraction が優先される。下記
        // `abstraction_cue_takes_priority_over_symbol_match` 参照。Bugbot 指摘
        // 対応・PR #261）。
        let result = classify("call parse_expansion now", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn path_extension_match_yields_direct() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("open core.rs and check it", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::PathMatch);
    }

    #[test]
    fn abstraction_cue_yields_high_precision() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("explain the overall architecture", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::AbstractionCue);
    }

    #[test]
    fn abstraction_cue_takes_priority_over_symbol_match() {
        // Bugbot 指摘（PR #261）: Rust コーパスの辞書には `new`・`main`・`read` の
        // ような一般英語と衝突するありふれた識別子が大量に含まれるため、シンボル
        // 完全一致を手掛かり語より優先すると、説明・意図の質問（抽象 cue を含む
        // 文）が対話ティアへ誤ってルーティングされ fail-safe の方向と逆になる。
        // 手掛かり語（"explain"／"how"）と辞書シンボル名（"new"）の両方に一致する
        // 質問が Abstraction（＝高精度ティア）へ分類されることを確認する。
        let dict = dictionary_with_symbol("new", "src/lib.rs");
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("explain how the new parser works", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::AbstractionCue);
    }

    #[test]
    fn symbol_match_without_abstraction_cue_still_yields_direct() {
        // 上記 `abstraction_cue_takes_priority_over_symbol_match` と対をなす
        // 回帰テスト: 手掛かり語を含まない質問では、従来どおりシンボル完全一致が
        // Direct（対話ティア）へ分類されることを固定する（優先順の変更が
        // シンボル一致そのものを壊していないことの確認）。
        let dict = dictionary_with_symbol("new", "src/lib.rs");
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("call new to construct the parser", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn japanese_abstraction_cue_without_token_boundary_yields_abstraction() {
        // codex-review P1 指摘（PR #261）: 日本語の手掛かり語は語間空白なしの
        // 通常の日本語文中では `tokenize` の空白区切りトークンと完全一致せず、
        // Abstraction に分類されるべき質問が Intent（fail-safe 側）へ落ちて
        // いた。日本語（非 ASCII）cue は正規化済み質問文字列への部分一致で
        // 判定することを確認する回帰テスト。
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("これはなぜ必要ですか", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::AbstractionCue);
    }

    #[test]
    fn no_cue_yields_intent_and_high_precision() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("something totally unrelated here", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::NoCue);
    }

    #[test]
    fn empty_input_fails_safe_to_intent() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("   ", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::Degenerate);
    }

    #[test]
    fn over_limit_input_fails_safe_to_intent() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let long_question = "a".repeat(MAX_QUESTION_CHARS + 1);
        let result = classify(&long_question, &index, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::Degenerate);
    }

    #[test]
    fn over_limit_whitespace_only_input_fails_safe_to_intent() {
        // 有界走査へ統合した空判定（`has_non_whitespace`）と上限超過判定
        // （`too_long`）が独立に効くことを確認する回帰テスト（codex-review P1
        // 指摘対応: 空白のみで MAX_QUESTION_CHARS を超える入力でも、先読み
        // 範囲内に非空白が無いまま超過を検出でき、Degenerate へ倒れる）。
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let long_whitespace = " ".repeat(MAX_QUESTION_CHARS + 1);
        let result = classify(&long_whitespace, &index, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::Degenerate);
    }

    #[test]
    fn token_count_over_limit_fails_safe_to_intent_even_with_leading_path_hint() {
        // 先頭に `core.rs` のようなパス様トークンを置いても、MAX_TOKENS を超える
        // 入力は黙って切り捨てて Direct/Dialogue に誤分類せず、Degenerate として
        // fail-safe（Intent/HighPrecision）側へ倒れることを確認する
        // （codex-review P1 指摘: TASK-115/PLAN-8）。
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let mut question = String::from("core.rs");
        for _ in 0..MAX_TOKENS {
            question.push_str(" x");
        }
        let result = classify(&question, &index, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::Degenerate);
    }

    #[test]
    fn token_count_at_limit_does_not_fail_safe() {
        let dict = dictionary_with_symbol("parse_expansion", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let mut question = String::from("parse_expansion");
        for _ in 0..(MAX_TOKENS - 1) {
            question.push_str(" x");
        }
        let result = classify(&question, &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn trailing_punctuation_does_not_block_symbol_match() {
        let dict = dictionary_with_symbol("parse_expansion", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("call parse_expansion?", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn trailing_punctuation_does_not_block_path_extension_match() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("open core.rs, please", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::PathMatch);
    }

    #[test]
    fn backtick_wrapped_symbol_matches_despite_code_span_markup() {
        // Markdown のコードスパン記法 `symbol` で囲んだシンボル名がバッククォート
        // ごと比較され辞書側の生の値と不一致にならないことを確認する
        // （Bugbot 指摘: Backticks block symbol and path matches）。
        let dict = dictionary_with_symbol("parse_expansion", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("call `parse_expansion` now", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn backtick_wrapped_path_matches_despite_code_span_markup() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("open `core.rs` and check it", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::PathMatch);
    }

    #[test]
    fn leading_dot_path_matches_hidden_file_in_dictionary() {
        // 先頭ドット付きファイル名（`.gitignore` 等）が trim で削られて
        // `file_tree` 側の生パスと不一致になり Intent へ誤分類されないことを確認する
        // （Bugbot 指摘: Leading dots break hidden-file matching）。
        let mut dict = empty_dictionary();
        dict.file_tree.paths.insert(".gitignore".to_string());
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("check .gitignore please", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::PathMatch);
    }

    #[test]
    fn multibyte_input_does_not_panic_and_is_deterministic() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let question = "これはテストです。なぜ動くのか説明して";
        let first = classify(question, &index, &criteria);
        let second = classify(question, &index, &criteria);
        assert_eq!(first, second);
    }

    #[test]
    fn same_input_is_deterministic() {
        let dict = dictionary_with_symbol("render_prompt_prefix", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let question = "how does render_prompt_prefix work";
        let a = classify(question, &index, &criteria);
        let b = classify(question, &index, &criteria);
        assert_eq!(a, b);
    }

    #[test]
    fn criteria_adjustment_changes_classification() {
        let dict = empty_dictionary();
        let index = NormalizedDictionaryIndex::build(&dict);
        let mut criteria = TieringCriteria::default();
        criteria.abstraction_cues.insert("frobnicate".to_string());
        let result = classify("please frobnicate the thing", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
    }

    #[test]
    fn mixed_case_abstraction_cue_in_criteria_still_matches_lowercase_question() {
        // codex P2 指摘（PR #261）: `TieringCriteria` は公開フィールドで呼び出し元が
        // 差し替え可能だが、質問側トークンのみ `ascii_lower` し基準値
        // （`abstraction_cues`）を正規化しないまま比較すると、"Explain" のような
        // 大文字混じりの差し替え値が小文字化済み質問トークンと一致しなくなる。
        // 基準値も比較時に ASCII 小文字化することを確認する回帰テスト。
        let dict = empty_dictionary();
        let index = NormalizedDictionaryIndex::build(&dict);
        let mut criteria = TieringCriteria::default();
        criteria.abstraction_cues.insert("Frobnicate".to_string());
        let result = classify("please frobnicate the widget", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::AbstractionCue);
    }

    #[test]
    fn mixed_case_path_extension_in_criteria_still_matches_lowercase_token() {
        // codex P2 指摘（PR #261）: `path_like_extensions` も同様に基準値側が
        // 正規化されないと "RS" のような大文字混じりの差し替え値が、小文字化済み
        // トークンの拡張子（"rs"）と一致しなくなる。
        let dict = empty_dictionary();
        let index = NormalizedDictionaryIndex::build(&dict);
        let mut criteria = TieringCriteria::default();
        criteria.path_like_extensions.clear();
        criteria.path_like_extensions.insert("RS".to_string());
        let result = classify("open core.rs and check it", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::PathMatch);
    }

    #[test]
    fn apostrophe_contraction_yields_abstraction_via_cue_stem() {
        // Cursor Bugbot 指摘（PR #261）: 境界句読点除去は先頭・末尾のみを対象と
        // するため、`what's` のような内部にアポストロフィを持つ短縮形は cue
        // "what" と完全一致せず、シンボル一致へフォールスルーして誤って対話
        // ティアへルーティングされうる。短縮形接尾辞（`'s` 等）を落として cue と
        // 一致することを確認する。
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("what's happening in the loader", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::AbstractionCue);
    }

    #[test]
    fn negation_contraction_strips_to_base_token() {
        // `don't` のような否定短縮形が `n't` を落として基底トークン `do` へ
        // 正規化されることを確認する（`strip_contraction_suffix` の `n't` 分岐）。
        // 辞書シンボル一致ではなく手掛かり語一致で検証し、一般英語とシンボル名の
        // 衝突（`classify` ドキュメンテーションコメント「優先順」参照）を経由せず
        // `n't` 分岐そのものを孤立させて確認する。
        let dict = empty_dictionary();
        let mut criteria = TieringCriteria::default();
        criteria.abstraction_cues.insert("do".to_string());
        let index = NormalizedDictionaryIndex::build(&dict);
        let result = classify("don't panic here", &index, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::AbstractionCue);
    }

    /// 呼び出し記録付きスタブ `LlmClient`（`core.rs` 側テストとも共通の目的だが、
    /// 本モジュール単体でも `TieredPlanner::select` のルーティングを検証する）。
    struct RecordingClient {
        calls: AtomicUsize,
        label: &'static str,
    }

    impl RecordingClient {
        fn new(label: &'static str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                label,
            }
        }
    }

    impl LlmClient for RecordingClient {
        fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!(
                "{{\"search_terms\":[],\"path_hint\":null,\"kind_hint\":null}} from {}",
                self.label
            ))
        }
    }

    #[test]
    fn tiered_planner_routes_direct_question_to_dialogue_client() {
        let dict = dictionary_with_symbol("parse_expansion", "src/query_planner.rs");
        let index = NormalizedDictionaryIndex::build(&dict);
        let planner = TieredPlanner::new(
            Box::new(RecordingClient::new("dialogue")),
            Box::new(RecordingClient::new("high_precision")),
            TieringCriteria::default(),
        );
        let (client, classification) = planner.select("call parse_expansion", &index);
        assert_eq!(classification.tier, Tier::Dialogue);
        let response = client.complete("prompt").unwrap();
        assert!(response.contains("dialogue"));
    }

    #[test]
    fn tiered_planner_routes_abstract_question_to_high_precision_client() {
        let dict = empty_dictionary();
        let index = NormalizedDictionaryIndex::build(&dict);
        let planner = TieredPlanner::new(
            Box::new(RecordingClient::new("dialogue")),
            Box::new(RecordingClient::new("high_precision")),
            TieringCriteria::default(),
        );
        let (client, classification) = planner.select("explain the design", &index);
        assert_eq!(classification.tier, Tier::HighPrecision);
        let response = client.complete("prompt").unwrap();
        assert!(response.contains("high_precision"));
    }
}
