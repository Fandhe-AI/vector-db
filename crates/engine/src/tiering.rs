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
//! 類型→ティア割り当て・判定基準・fail-safe の方向は TASK-115・PLAN-8 の設計事項
//! （最終確定含めオーナー判断待ちの範囲を含む）であり、本コメントでは詳細を記載
//! しない。詳細は各公開 API（[`tier_for_class`]・[`TieringCriteria`]・[`classify`]）
//! のドキュメンテーションコメント、または `docs/design/query-tiering-criteria.md`
//! （ポインタ表記）を参照。
//!
//! untrusted 入力対応: `question` は wire 経由の未検証入力であるため、添字アクセス・
//! `unwrap`/`expect` を使わず、決定的・線形時間のトークナイズのみを行う
//! （coding-rust.md「untrusted 入力の扱い」）。長さ・トークン数はいずれも上限
//! （[`MAX_QUESTION_CHARS`]・[`MAX_TOKENS`]）で頭打ちにし、無制限確保を避ける。
//! トークン数が [`MAX_TOKENS`] を超える入力は、判定材料を黙って切り捨てて誤分類する
//! のではなく [`ClassificationSignal::Degenerate`] の fail-safe 側へ倒す（超過を検出
//! できない切り捨てのまま `classify` へ渡さない）。トークンの前後に付く句読点
//! （`?`・`,`・全角句読点等）は比較前に除去し、自然文中のシンボル名・パス様トークンの
//! 認識を妨げないようにする。

use std::collections::BTreeSet;

use crate::dictionary::Dictionary;

/// 質問文として判定に用いる最大文字数（[`crate::query_planner::MAX_QUESTION_CHARS`]
/// と同じ上限を再利用し、`plan_query` 経路全体で一貫した有界化契約を保つ）。
pub const MAX_QUESTION_CHARS: usize = crate::query_planner::MAX_QUESTION_CHARS;

/// 判定のトークナイズで走査するトークン数の上限（DoS 耐性: 空白区切りのトークンが
/// 極端に多い入力でも判定処理の線形走査量を頭打ちにする）。
pub const MAX_TOKENS: usize = 256;

/// 質問の類型。既定の類型→ティア割り当ては [`tier_for_class`] を参照
/// （モジュールドキュメント「既定の類型→ティア割り当て」）。
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

/// `class` に対する既定のティア割り当て（本モジュールの現行実装が採用する既定値。
/// モジュールドキュメント「既定の類型→ティア割り当て」参照）。
pub fn tier_for_class(class: QuestionClass) -> Tier {
    match class {
        QuestionClass::Direct => Tier::Dialogue,
        QuestionClass::Intent | QuestionClass::Abstraction => Tier::HighPrecision,
    }
}

/// 判定基準の調整可能な既定値（[`TieringCriteria::default`]）。手掛かり語等の
/// 具体値は人間設計の共同タスクであり、ここでは差し替え可能な仮置き値として実装する
/// （最終確定はオーナー判断待ち。`docs/design/query-tiering-criteria.md` 参照）。
#[derive(Debug, Clone)]
pub struct TieringCriteria {
    /// 概念・説明要求の言い回しとみなす手掛かり語（小文字化して比較。ASCII
    /// 小文字化のみ行い、マルチバイト文字はそのまま比較する。モジュール
    /// ドキュメント「untrusted 入力対応」参照）。
    pub abstraction_cues: BTreeSet<String>,
    /// パス様トークンとみなす拡張子（先頭のドットなし。小文字比較）。
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
/// `Dictionary::file_tree` 側の生パスと一致させるため。Bugbot 指摘対応）。
const TOKEN_BOUNDARY_PUNCTUATION: &[char] = &[
    '?', '!', ',', ';', ':', '\'', '"', '(', ')', '[', ']', '{', '}', '<', '>', '、', '。', '！',
    '？', '「', '」', '『', '』',
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
        .map(|t| ascii_lower(strip_boundary_punctuation(t)))
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

/// `question` と `dictionary`（[`crate::core::EngineCore::dictionary_snapshot`] 経由で
/// テナント境界済みのスナップショット）から質問類型を判定する。
///
/// 呼び出し元は `core.rs::EngineCore::plan_query`（テナント別辞書スナップショット
/// 経由。本関数自体はテナント境界の判断を持たず、渡された `dictionary` をそのまま
/// 使う純粋関数）。
///
/// 優先順（モジュールドキュメント「fail-safe の方向」も参照）:
/// 1. トークンが辞書シンボル名（[`Dictionary::symbols`]）に完全一致 → [`QuestionClass::Direct`]
/// 2. パス様トークン（[`Dictionary::file_tree`] のパスへの一致、または
///    `criteria.path_like_extensions` の拡張子を持つトークン）→ [`QuestionClass::Direct`]
/// 3. `criteria.abstraction_cues` に一致する手掛かり語を含む → [`QuestionClass::Abstraction`]
/// 4. 上記以外 → [`QuestionClass::Intent`]
///
/// 空入力・[`MAX_QUESTION_CHARS`] 超過・[`MAX_TOKENS`] 超過はいずれも縮退値として
/// fail-safe 側（[`QuestionClass::Intent`]）へ倒す（トークン数超過を黙って切り捨てて
/// `Direct`/`Dialogue` 側に誤分類しない）。
pub fn classify(
    question: &str,
    dictionary: &Dictionary,
    criteria: &TieringCriteria,
) -> Classification {
    let degenerate = question.trim().is_empty() || question.chars().count() > MAX_QUESTION_CHARS;
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

    // シンボル名（`BTreeSet<Symbol>` の決定的な反復順序。`dictionary.rs`
    // モジュールドキュメント「決定性」参照）と ASCII 小文字化トークンを比較する。
    let symbol_names: BTreeSet<String> = dictionary
        .symbols
        .iter()
        .map(|s| ascii_lower(&s.name))
        .collect();
    if tokens.iter().any(|t| symbol_names.contains(t)) {
        return make(QuestionClass::Direct, ClassificationSignal::SymbolMatch);
    }

    let path_tokens: BTreeSet<String> = dictionary
        .file_tree
        .paths
        .iter()
        .map(|p| ascii_lower(p))
        .collect();
    let has_path_match = tokens.iter().any(|t| {
        path_tokens.contains(t)
            || extension_of(t)
                .map(|ext| criteria.path_like_extensions.contains(ext))
                .unwrap_or(false)
    });
    if has_path_match {
        return make(QuestionClass::Direct, ClassificationSignal::PathMatch);
    }

    let has_abstraction_cue = tokens.iter().any(|t| criteria.abstraction_cues.contains(t));
    if has_abstraction_cue {
        return make(
            QuestionClass::Abstraction,
            ClassificationSignal::AbstractionCue,
        );
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

    /// `question` を `dictionary` に基づき分類し、対応するティアの
    /// [`crate::query_planner::LlmClient`] とその判定結果を返す。
    pub fn select(
        &self,
        question: &str,
        dictionary: &Dictionary,
    ) -> (&dyn crate::query_planner::LlmClient, Classification) {
        let classification = classify(question, dictionary, &self.criteria);
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
        let result = classify("what does parse_expansion do", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn path_extension_match_yields_direct() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let result = classify("open core.rs and check it", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::PathMatch);
    }

    #[test]
    fn abstraction_cue_yields_high_precision() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let result = classify("explain the overall architecture", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::AbstractionCue);
    }

    #[test]
    fn no_cue_yields_intent_and_high_precision() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let result = classify("something totally unrelated here", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::NoCue);
    }

    #[test]
    fn empty_input_fails_safe_to_intent() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let result = classify("   ", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::Degenerate);
    }

    #[test]
    fn over_limit_input_fails_safe_to_intent() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let long_question = "a".repeat(MAX_QUESTION_CHARS + 1);
        let result = classify(&long_question, &dict, &criteria);
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
        let mut question = String::from("core.rs");
        for _ in 0..MAX_TOKENS {
            question.push_str(" x");
        }
        let result = classify(&question, &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Intent);
        assert_eq!(result.tier, Tier::HighPrecision);
        assert_eq!(result.signal, ClassificationSignal::Degenerate);
    }

    #[test]
    fn token_count_at_limit_does_not_fail_safe() {
        let dict = dictionary_with_symbol("parse_expansion", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let mut question = String::from("parse_expansion");
        for _ in 0..(MAX_TOKENS - 1) {
            question.push_str(" x");
        }
        let result = classify(&question, &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn trailing_punctuation_does_not_block_symbol_match() {
        let dict = dictionary_with_symbol("parse_expansion", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let result = classify("what is parse_expansion?", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::SymbolMatch);
    }

    #[test]
    fn trailing_punctuation_does_not_block_path_extension_match() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let result = classify("open core.rs, please", &dict, &criteria);
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
        let result = classify("check .gitignore please", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Direct);
        assert_eq!(result.tier, Tier::Dialogue);
        assert_eq!(result.signal, ClassificationSignal::PathMatch);
    }

    #[test]
    fn multibyte_input_does_not_panic_and_is_deterministic() {
        let dict = empty_dictionary();
        let criteria = TieringCriteria::default();
        let question = "これはテストです。なぜ動くのか説明して";
        let first = classify(question, &dict, &criteria);
        let second = classify(question, &dict, &criteria);
        assert_eq!(first, second);
    }

    #[test]
    fn same_input_is_deterministic() {
        let dict = dictionary_with_symbol("render_prompt_prefix", "src/query_planner.rs");
        let criteria = TieringCriteria::default();
        let question = "how does render_prompt_prefix work";
        let a = classify(question, &dict, &criteria);
        let b = classify(question, &dict, &criteria);
        assert_eq!(a, b);
    }

    #[test]
    fn criteria_adjustment_changes_classification() {
        let dict = empty_dictionary();
        let mut criteria = TieringCriteria::default();
        criteria.abstraction_cues.insert("frobnicate".to_string());
        let result = classify("please frobnicate the thing", &dict, &criteria);
        assert_eq!(result.class, QuestionClass::Abstraction);
        assert_eq!(result.tier, Tier::HighPrecision);
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
        let planner = TieredPlanner::new(
            Box::new(RecordingClient::new("dialogue")),
            Box::new(RecordingClient::new("high_precision")),
            TieringCriteria::default(),
        );
        let (client, classification) = planner.select("what is parse_expansion", &dict);
        assert_eq!(classification.tier, Tier::Dialogue);
        let response = client.complete("prompt").unwrap();
        assert!(response.contains("dialogue"));
    }

    #[test]
    fn tiered_planner_routes_abstract_question_to_high_precision_client() {
        let dict = empty_dictionary();
        let planner = TieredPlanner::new(
            Box::new(RecordingClient::new("dialogue")),
            Box::new(RecordingClient::new("high_precision")),
            TieringCriteria::default(),
        );
        let (client, classification) = planner.select("explain the design", &dict);
        assert_eq!(classification.tier, Tier::HighPrecision);
        let response = client.complete("prompt").unwrap();
        assert!(response.contains("high_precision"));
    }
}
