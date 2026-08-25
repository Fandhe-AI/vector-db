//! チャンク化モジュール（TASK-119、対象ビヘイビア: INDEX-3。ポインタ:
//! `docs/spec/05-tasks.md` TASK-119・`docs/spec/04-behavior/indexing.md` INDEX-3）。
//!
//! 責務境界: `INSERT` 経由で受け取ったファイル内容（パス＋ UTF-8 本文）を、
//! ファイル種別に応じた方針でチャンク列へ分割する純関数的な API を提供し、
//! storage / catalog / sql / policy とは結線しない（`sparse.rs` と同じ
//! 「純関数的 API・結線しない」方針）。呼び出し元は将来の増分インデックス結線
//! （TASK-120）と一括投入上限（TASK-122、対象ビヘイビア: INDEX-4）を想定する。
//! 本モジュール自身は単一入力（1 ファイル）に対する走査量の有界化のみを担い、
//! 複数ファイルにまたがる合計サイズ・ファイル数上限は TASK-122 の管轄である。
//!
//! 入力は SQL 表層で既に UTF-8 検証済みの `&str` を前提とする（本モジュールは
//! バイト列のデコードを行わない）。パスはファイル種別判定のみに使い、
//! ファイルシステムへはアクセスしない。
//!
//! 分割方針は spec（private）が定める詳細の正であり、ここでは README 公開範囲の
//! 1 行要約に留める: Markdown は見出し単位、非 Markdown は固定行数単位（既定
//! 60 行）。移植元は `docs/spec/03-poc/eval-base/scripts/build_chunks.py`
//! （PoC-0）だが、文面は転記せず本モジュール独自の実装として以下の点を変更した。
//!
//! - 行分割は `str::lines()` を使い CRLF (`\r\n`) を LF 相当として正規化する
//!   （PoC は `\n` 固定分割だったため CRLF 入力で `\r` が本文に残っていた点の改善）
//! - fenced code block（```` ``` ```` / `~~~` の対）の内側にある `#` 始まりの行は
//!   見出しと見なさない（Markdown 仕様上見出しではなく、コードを多く含む
//!   コーパスでの誤分割を防ぐための安全側の改善）
//! - 20 文字未満のチャンクを間引くフィルタは採用しない（評価データ生成用の
//!   間引きであり、DB では内容を無音で失う結果になるため）
//! - 言語判定（`lang` 相当の分類）は対象外（INDEX-3 のスコープ外）
//!
//! untrusted 入力に対する有界化（fail-closed。.claude/rules/coding-rust.md）:
//! 走査に入る前に入力全体のバイト長・行数を検証し、上限超過時は副作用なく
//! `Err` を返す。行番号・文字数の累積は `checked_add` / `saturating_add` を用い、
//! `unwrap` / `expect` / 添字アクセス・正規表現は使わない。

/// 単一入力本文の上限バイト数（この上限を超える入力は走査前に拒否する）。
///
/// wire → SQL `INSERT` 経由で届く untrusted 入力を前提とした DoS 対策
/// （`sparse.rs::MAX_DOC_BYTES` と同じ流儀）。複数ファイル合計の上限は
/// TASK-122（INDEX-4）が別途担う。
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// 単一入力本文の上限行数（この上限を超える入力は走査前に拒否する）。
pub const MAX_INPUT_LINES: usize = 1_000_000;

/// 非 Markdown ファイルの既定チャンク行数。
pub const DEFAULT_LINES_PER_CHUNK: usize = 60;

/// Markdown の 1 節あたりの既定上限文字数（`chars().count()` で計測）。
///
/// PoC-0 の実績値を移植した既定値。`None` を渡すと上限なし（節を分割しない）。
pub const DEFAULT_MAX_MARKDOWN_SECTION_CHARS: Option<usize> = Some(600);

/// ファイルパスから判定したファイル種別。
///
/// [`detect_file_kind`] の戻り値。[`chunk_file`] がこの種別に応じて
/// [`chunk_markdown`] / [`chunk_generic`] のいずれかへ委譲する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Markdown（拡張子 `.md` / `.markdown`。ASCII 大小無視）。
    Markdown,
    /// Markdown 以外すべて（拡張子なし・未知拡張子を含む）。
    Generic,
}

/// ファイルパスの拡張子から [`FileKind`] を判定する。
///
/// ファイルシステムへはアクセスせず、文字列としてのパスのみを見る
/// （untrusted なパス文字列に対しても安全に呼べる）。
pub fn detect_file_kind(path: &str) -> FileKind {
    let ext = match path.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => return FileKind::Generic,
    };
    if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") {
        FileKind::Markdown
    } else {
        FileKind::Generic
    }
}

/// チャンク化の挙動を調整する設定。
#[derive(Debug, Clone)]
pub struct ChunkingConfig {
    /// [`chunk_generic`] が 1 チャンクにまとめる行数。0 は不正（[`ChunkingError::InvalidConfig`]）。
    pub lines_per_chunk: usize,
    /// [`chunk_markdown`] が 1 節に許容する上限文字数。`Some(0)` は不正。
    /// `None` は上限なし（節を分割しない）。
    pub max_markdown_section_chars: Option<usize>,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            lines_per_chunk: DEFAULT_LINES_PER_CHUNK,
            max_markdown_section_chars: DEFAULT_MAX_MARKDOWN_SECTION_CHARS,
        }
    }
}

/// チャンク化の結果 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// ファイル内 0 始まりの連番。
    pub index: usize,
    /// このチャンクが対応する元テキストの開始行（1 始まり・両端含む）。
    pub start_line: usize,
    /// このチャンクが対応する元テキストの終了行（1 始まり・両端含む）。
    pub end_line: usize,
    /// 前後の空白行・行末空白を除去済みの本文。
    pub text: String,
}

/// チャンク化の失敗理由。
///
/// メッセージは英語（.claude/rules/japanese-style.md: プログラム出力文字列は英語）。
/// 長さ・上限値のみを含み、入力本文そのものは含めない（エラー経由で内容を
/// 漏らさないため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkingError {
    /// 入力本文のバイト長が [`MAX_INPUT_BYTES`] を超えた。
    InputTooLarge { len: usize, max: usize },
    /// 入力本文の行数が [`MAX_INPUT_LINES`] を超えた。
    TooManyLines { len: usize, max: usize },
    /// [`ChunkingConfig`] の値が不正。
    InvalidConfig { reason: &'static str },
}

impl std::fmt::Display for ChunkingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkingError::InputTooLarge { len, max } => {
                write!(f, "chunking input too large: {len} bytes (max {max})")
            }
            ChunkingError::TooManyLines { len, max } => {
                write!(f, "chunking input has too many lines: {len} (max {max})")
            }
            ChunkingError::InvalidConfig { reason } => {
                write!(f, "invalid chunking config: {reason}")
            }
        }
    }
}

impl std::error::Error for ChunkingError {}

/// 入力本文を走査前に検証する（fail-closed）。
///
/// バイト長・行数の両方を走査に入る前にチェックすることで、上限超過入力に
/// 対して線形走査すら行わずに拒否する。行数のカウントも `chars`/`bytes` を
/// 数え上げるのではなく `lines()` のイテレータで行い、途中で上限を超えた
/// 時点で打ち切る（無制限に数え続けない）。
fn validate_input(text: &str, config: &ChunkingConfig) -> Result<(), ChunkingError> {
    if config.lines_per_chunk == 0 {
        return Err(ChunkingError::InvalidConfig {
            reason: "lines_per_chunk must be greater than 0",
        });
    }
    if config.max_markdown_section_chars == Some(0) {
        return Err(ChunkingError::InvalidConfig {
            reason: "max_markdown_section_chars must be greater than 0 when set",
        });
    }

    let len = text.len();
    if len > MAX_INPUT_BYTES {
        return Err(ChunkingError::InputTooLarge {
            len,
            max: MAX_INPUT_BYTES,
        });
    }

    let mut line_count: usize = 0;
    for _ in text.lines() {
        line_count = line_count.saturating_add(1);
        if line_count > MAX_INPUT_LINES {
            return Err(ChunkingError::TooManyLines {
                len: line_count,
                max: MAX_INPUT_LINES,
            });
        }
    }

    Ok(())
}

/// ATX 見出し行かどうかを判定する（`#` 1〜3 個＋空白で始まる行。4 段以上は
/// 境界にしない。PoC 同等の判定範囲）。
fn is_atx_heading(line: &str) -> bool {
    let trimmed_start = line.trim_start_matches(' ');
    let hashes = trimmed_start.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 3 {
        return false;
    }
    matches!(
        trimmed_start.as_bytes().get(hashes),
        Some(b' ') | Some(b'\t')
    )
}

/// fenced code block の開始・終了デリミタ行かどうかを判定する
/// （```` ``` ```` または `~~~` で始まる行。言語指定の有無は問わない）。
fn is_fence_delimiter(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// 行の並び（1 始まり行番号付き）から前後の空行を除いた [`Chunk`] を作る。
/// 全行が空白のみの場合は `None`（呼び出し元は空チャンクを捨てる）。
fn trim_block(index: usize, lines: &[(usize, &str)]) -> Option<Chunk> {
    let first_non_blank = lines.iter().position(|(_, l)| !l.trim().is_empty())?;
    let last_non_blank = lines.iter().rposition(|(_, l)| !l.trim().is_empty())?;
    let block = &lines[first_non_blank..=last_non_blank];
    let start_line = block.first()?.0;
    let end_line = block.last()?.0;
    let text = block
        .iter()
        .map(|(_, l)| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(Chunk {
        index,
        start_line,
        end_line,
        text,
    })
}

/// 非 Markdown ファイルを固定行数の窓でチャンク化する。
///
/// 先頭から `config.lines_per_chunk` 行ずつ窓を切り、各窓を trim して
/// 空でなければチャンクにする（150 行 → 60/60/30 のように末尾窓は短くなる）。
pub fn chunk_generic(text: &str, config: &ChunkingConfig) -> Result<Vec<Chunk>, ChunkingError> {
    validate_input(text, config)?;

    let numbered_lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i.saturating_add(1), l))
        .collect();

    let mut chunks =
        Vec::with_capacity(numbered_lines.len().div_ceil(config.lines_per_chunk.max(1)));
    let mut index = 0usize;
    for window in numbered_lines.chunks(config.lines_per_chunk) {
        if let Some(chunk) = trim_block(index, window) {
            index = index.saturating_add(1);
            chunks.push(chunk);
        }
    }
    Ok(chunks)
}

/// 段落（空行区切り）単位で、上限文字数に収まるよう詰め直す。
///
/// 1 段落単体が上限を超える場合は分割せずそのまま 1 チャンクとする
/// （段落内部をさらに割ると文脈が失われるため、PoC の方針を踏襲）。
fn split_oversized_section(
    start_index: usize,
    lines: &[(usize, &str)],
    max_chars: usize,
) -> Vec<Chunk> {
    // 空行を境界に段落へ分割する。
    let mut paragraphs: Vec<Vec<(usize, &str)>> = Vec::new();
    let mut current: Vec<(usize, &str)> = Vec::new();
    for &(ln, l) in lines {
        if l.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            current.push((ln, l));
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }

    let mut chunks = Vec::new();
    let mut index = start_index;
    let mut acc: Vec<(usize, &str)> = Vec::new();
    let mut acc_chars: usize = 0;

    for paragraph in paragraphs {
        let paragraph_chars: usize = paragraph
            .iter()
            .map(|(_, l)| l.chars().count())
            .fold(0usize, |a, b| a.saturating_add(b));

        if !acc.is_empty() && acc_chars.saturating_add(paragraph_chars) > max_chars {
            if let Some(chunk) = trim_block(index, &acc) {
                index = index.saturating_add(1);
                chunks.push(chunk);
            }
            acc = Vec::new();
            acc_chars = 0;
        }

        acc.extend(paragraph.iter().copied());
        acc_chars = acc_chars.saturating_add(paragraph_chars);
    }

    if !acc.is_empty() {
        if let Some(chunk) = trim_block(index, &acc) {
            chunks.push(chunk);
        }
    }

    chunks
}

/// Markdown ファイルを見出し単位でチャンク化する。
///
/// ATX 見出し（[`is_atx_heading`]）の行を節の開始境界とし、最初の見出しより
/// 前の前文は独立した 1 節として扱う。fenced code block（[`is_fence_delimiter`]）
/// 内の `#` 行は見出しと見なさない。`config.max_markdown_section_chars` を
/// 超える節は [`split_oversized_section`] で段落単位に詰め直す。
pub fn chunk_markdown(text: &str, config: &ChunkingConfig) -> Result<Vec<Chunk>, ChunkingError> {
    validate_input(text, config)?;

    let numbered_lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i.saturating_add(1), l))
        .collect();

    // 見出し境界で節へ分割する（fence 内の見出しらしき行は境界にしない）。
    let mut sections: Vec<Vec<(usize, &str)>> = Vec::new();
    let mut current: Vec<(usize, &str)> = Vec::new();
    let mut in_fence = false;
    for &(ln, l) in &numbered_lines {
        if is_fence_delimiter(l) {
            in_fence = !in_fence;
            current.push((ln, l));
            continue;
        }
        if !in_fence && is_atx_heading(l) && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        current.push((ln, l));
    }
    if !current.is_empty() {
        sections.push(current);
    }

    let mut chunks = Vec::with_capacity(sections.len());
    let mut index = 0usize;
    for section in sections {
        match config.max_markdown_section_chars {
            Some(max_chars) => {
                let section_chars: usize = section
                    .iter()
                    .map(|(_, l)| l.chars().count())
                    .fold(0usize, |a, b| a.saturating_add(b));
                if section_chars > max_chars {
                    let split = split_oversized_section(index, &section, max_chars);
                    index = index.saturating_add(split.len());
                    chunks.extend(split);
                    continue;
                }
                if let Some(chunk) = trim_block(index, &section) {
                    index = index.saturating_add(1);
                    chunks.push(chunk);
                }
            }
            None => {
                if let Some(chunk) = trim_block(index, &section) {
                    index = index.saturating_add(1);
                    chunks.push(chunk);
                }
            }
        }
    }

    Ok(chunks)
}

/// パスからファイル種別を判定し、[`chunk_markdown`] / [`chunk_generic`] へ委譲する。
///
/// `INSERT` 経由で届く未検証のパス・本文を受け取る想定の公開入口
/// （呼び出し元は将来の TASK-120 増分インデックス結線・TASK-122 一括投入上限）。
pub fn chunk_file(
    path: &str,
    text: &str,
    config: &ChunkingConfig,
) -> Result<Vec<Chunk>, ChunkingError> {
    match detect_file_kind(path) {
        FileKind::Markdown => chunk_markdown(text, config),
        FileKind::Generic => chunk_generic(text, config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atx_heading_boundaries() {
        assert!(is_atx_heading("# h1"));
        assert!(is_atx_heading("## h2"));
        assert!(is_atx_heading("### h3"));
        assert!(!is_atx_heading("#### h4"));
        assert!(!is_atx_heading("#nospace"));
        assert!(!is_atx_heading("plain"));
        assert!(is_atx_heading("  # indented"));
    }

    #[test]
    fn fence_delimiter_detection() {
        assert!(is_fence_delimiter("```"));
        assert!(is_fence_delimiter("```rust"));
        assert!(is_fence_delimiter("~~~"));
        assert!(!is_fence_delimiter("plain text"));
    }

    #[test]
    fn trim_block_adjusts_line_numbers() {
        let lines = vec![(1, ""), (2, "hello"), (3, "world"), (4, "")];
        let chunk = trim_block(0, &lines).expect("non-empty block");
        assert_eq!(chunk.start_line, 2);
        assert_eq!(chunk.end_line, 3);
        assert_eq!(chunk.text, "hello\nworld");
    }

    #[test]
    fn trim_block_all_blank_yields_none() {
        let lines = vec![(1, ""), (2, "   ")];
        assert!(trim_block(0, &lines).is_none());
    }
}
