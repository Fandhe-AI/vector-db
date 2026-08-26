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
//! 分割方針の詳細は spec（private）が正であり、本コメントには転記しない
//! （TASK-119・INDEX-3 のポインタのみ）。公開 API の契約として説明が必要な
//! 実装上の性質は以下に限る。
//!
//! - 行分割は `str::lines()` を使い CRLF (`\r\n`) を LF 相当として正規化する
//! - fenced code block（```` ``` ```` / `~~~` の対）の内側にある `#` 始まりの行は
//!   Markdown 仕様上見出しではないため、節の境界にしない
//! - 短いチャンクを間引くフィルタは持たない（DB では内容を無音で失うため）
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
/// `ChunkingConfig` に `None` を渡すと上限なし（節を分割しない）。
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
    // 判定対象は最終パス要素（ファイル名）の拡張子のみ。パス全体から最後の `.`
    // を探すと `docs.md/README` のようにディレクトリ側のドットを拾いうるため、
    // 先に区切り（`/`・Windows 形式の `\`）で末尾要素を切り出す。
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = match file_name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() && !stem.is_empty() => ext,
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

/// 行頭の半角スペース数を数える（`is_atx_heading` と `fence_marker` が
/// 共有するインデント判定の単一実装）。
///
/// CommonMark ではインデント 4 個以上の行はインデントコードブロックの内容と
/// なり、見出しにも fence にもならない。両判定が別々にインデントを数えると
/// 基準が非対称になり、片方だけがコードブロック内の行を構文として誤認する
/// （Review 指摘の根本原因）ため、数え方をここに一本化する。
/// 戻り値は行頭スペースのバイトオフセットでもある（スペースは 1 バイト文字）。
fn leading_indent_spaces(line: &str) -> usize {
    line.chars().take_while(|&c| c == ' ').count()
}

/// ATX 見出し行かどうかを判定する（`#` 1〜3 個＋空白で始まる行。4 段以上は
/// 節の境界にしない）。
///
/// 行頭の空白は [`leading_indent_spaces`] の基準（0〜3 個まで）に従う。
fn is_atx_heading(line: &str) -> bool {
    let leading_spaces = leading_indent_spaces(line);
    if leading_spaces >= 4 {
        return false;
    }
    let trimmed_start = match line.get(leading_spaces..) {
        Some(rest) => rest,
        None => return false,
    };
    let hashes = trimmed_start.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 3 {
        return false;
    }
    matches!(
        trimmed_start.as_bytes().get(hashes),
        Some(b' ') | Some(b'\t')
    )
}

/// fenced code block のデリミタ行（```` ``` ```` / `~~~` の 3 個以上の連続）の
/// 情報。デリミタ行でなければ [`fence_marker`] が `None` を返す。
///
/// CommonMark の開閉条件を判定するために必要な最小限の情報のみを保持する。
/// `chunk_markdown` が開始フェンスの `kind`・`len` を状態として持ち、後続行の
/// マーカーと突き合わせて閉じフェンスの成立を判定する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FenceMarker {
    /// デリミタ文字種（`` ` `` か `~`）。
    kind: char,
    /// デリミタ文字の連続数（3 以上）。
    len: usize,
    /// デリミタの後ろ（info string 相当）が空白のみか。
    info_is_blank: bool,
    /// デリミタの後ろにバッククォートを含むか（backtick 開始フェンスの可否判定用）。
    info_has_backtick: bool,
}

/// 行が fenced code block のデリミタ行かを判定し、開閉判定に必要な情報を返す。
///
/// 行頭インデントは [`leading_indent_spaces`] の基準（0〜3 個まで）に従い、
/// 4 個以上インデントされた行は fence と見なさない（`is_atx_heading` と同じ
/// 基準。非対称だとインデントコードブロック内の ``` 行で fence が開きっぱなしに
/// なり、以降の見出し境界がすべて失われる）。
fn fence_marker(line: &str) -> Option<FenceMarker> {
    let indent = leading_indent_spaces(line);
    if indent >= 4 {
        return None;
    }
    // インデントは半角スペース（1 バイト）のみを数えているため、この境界は
    // 常に char 境界（添字ではなく get で取り、失敗時は None）。
    let rest = line.get(indent..)?;
    let kind = match rest.chars().next()? {
        '`' => '`',
        '~' => '~',
        _ => return None,
    };
    let len = rest.chars().take_while(|&c| c == kind).count();
    if len < 3 {
        return None;
    }
    // kind は ASCII 1 バイト文字なので len はそのままバイト長でもある。
    let info = rest.get(len..)?;
    Some(FenceMarker {
        kind,
        len,
        info_is_blank: info.trim().is_empty(),
        info_has_backtick: info.contains('`'),
    })
}

impl FenceMarker {
    /// 開始フェンスとして成立するか。
    ///
    /// CommonMark では backtick 開始フェンスの info string にバッククォートを
    /// 含められない（`` ```a`b `` は fence 開始ではない）。tilde 開始フェンスには
    /// この制約がない。
    fn can_open(&self) -> bool {
        self.kind != '`' || !self.info_has_backtick
    }

    /// `open` で開いたフェンスを閉じられるか。
    ///
    /// CommonMark の閉じ条件は「開始と同じ文字種・開始以上の長さ・後続が空白のみ」。
    /// 短い ``` や info string 付きの行で閉じたと誤認すると、コードブロック内の
    /// `#` 行が見出し境界と判定されチャンクが誤分割される。
    fn can_close(&self, open: &FenceMarker) -> bool {
        self.kind == open.kind && self.len >= open.len && self.info_is_blank
    }
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

    // lines_per_chunk == 0 は validate_input が事前に Err 化するため、
    // ここでは常に 1 以上（0 除算しない）。
    let mut chunks = Vec::with_capacity(numbered_lines.len().div_ceil(config.lines_per_chunk));
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
/// （段落内部をさらに割ると文脈が失われるため）。
///
/// 段落は `lines` 内の連続範囲 `[start, end)` として保持し、チャンク化の際も
/// 「最初の段落の先頭行から最後の段落の末尾行まで」の連続スライスを
/// [`trim_block`] へ渡す。段落そのものを詰め替えると段落間の空行が本文から
/// 落ち、非オーバーサイズ経路（`chunk_markdown` が `trim_block` を直接呼ぶ分岐）
/// と整形が食い違ううえ、`start_line`/`end_line` の行スパンと本文の行数が
/// 対応しなくなる（TASK-120 の増分インデックス結線が行スパンを使う前提を壊す）。
fn split_oversized_section(
    start_index: usize,
    lines: &[(usize, &str)],
    max_chars: usize,
) -> Vec<Chunk> {
    // 空行を境界に段落の範囲（lines 内の [start, end)）を求める。空行自体は
    // どの段落にも属さないが、範囲の間に残るためチャンク本文には保持される。
    let mut paragraphs: Vec<(usize, usize)> = Vec::new();
    let mut paragraph_start: Option<usize> = None;
    for (offset, &(_, l)) in lines.iter().enumerate() {
        if l.trim().is_empty() {
            if let Some(start) = paragraph_start.take() {
                paragraphs.push((start, offset));
            }
        } else if paragraph_start.is_none() {
            paragraph_start = Some(offset);
        }
    }
    if let Some(start) = paragraph_start {
        paragraphs.push((start, lines.len()));
    }

    let mut chunks = Vec::new();
    let mut index = start_index;
    // 累積中のチャンクが覆う lines 内の範囲（[start, end)）と文字数。
    let mut acc: Option<(usize, usize)> = None;
    let mut acc_chars: usize = 0;

    for (start, end) in paragraphs {
        let paragraph_chars: usize = lines
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .map(|(_, l)| l.chars().count())
            .fold(0usize, |a, b| a.saturating_add(b));

        if let Some((acc_start, acc_end)) = acc {
            if acc_chars.saturating_add(paragraph_chars) > max_chars {
                if let Some(chunk) = lines
                    .get(acc_start..acc_end)
                    .and_then(|block| trim_block(index, block))
                {
                    index = index.saturating_add(1);
                    chunks.push(chunk);
                }
                acc = None;
                acc_chars = 0;
            }
        }

        acc = match acc {
            Some((acc_start, _)) => Some((acc_start, end)),
            None => Some((start, end)),
        };
        acc_chars = acc_chars.saturating_add(paragraph_chars);
    }

    if let Some((acc_start, acc_end)) = acc {
        if let Some(chunk) = lines
            .get(acc_start..acc_end)
            .and_then(|block| trim_block(index, block))
        {
            chunks.push(chunk);
        }
    }

    chunks
}

/// Markdown ファイルを見出し単位でチャンク化する。
///
/// ATX 見出し（[`is_atx_heading`]）の行を節の開始境界とし、最初の見出しより
/// 前の前文は独立した 1 節として扱う。fenced code block（[`fence_marker`] の
/// 開閉判定）内の `#` 行は見出しと見なさない。`config.max_markdown_section_chars` を
/// 超える節は [`split_oversized_section`] で段落単位に詰め直す。
pub fn chunk_markdown(text: &str, config: &ChunkingConfig) -> Result<Vec<Chunk>, ChunkingError> {
    validate_input(text, config)?;

    let numbered_lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i.saturating_add(1), l))
        .collect();

    // 見出し境界で節へ分割する（fence 内の見出しらしき行は境界にしない）。
    // 閉じフェンスは FenceMarker::can_close が定める CommonMark の条件
    // （同じ文字種・開始以上の長さ・後続が空白のみ）を満たす行でのみ成立させる。
    // 満たさない行はフェンス内部の通常行として扱い、開閉状態を変化させない。
    let mut sections: Vec<Vec<(usize, &str)>> = Vec::new();
    let mut current: Vec<(usize, &str)> = Vec::new();
    let mut open_fence: Option<FenceMarker> = None;
    for &(ln, l) in &numbered_lines {
        match (open_fence, fence_marker(l)) {
            // 開いているフェンスを閉じられる行だけを閉じ扱いにする。
            (Some(open), Some(marker)) if marker.can_close(&open) => {
                open_fence = None;
            }
            // 閉じ条件を満たさないデリミタ行はフェンス内部の通常行のまま。
            (Some(_), _) => {}
            (None, Some(marker)) if marker.can_open() => {
                open_fence = Some(marker);
            }
            (None, _) => {
                if is_atx_heading(l) && !current.is_empty() {
                    sections.push(std::mem::take(&mut current));
                }
            }
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
    fn fence_marker_detection() {
        assert_eq!(fence_marker("```").map(|m| (m.kind, m.len)), Some(('`', 3)));
        assert_eq!(
            fence_marker("````rust").map(|m| (m.kind, m.len)),
            Some(('`', 4))
        );
        assert_eq!(fence_marker("~~~").map(|m| (m.kind, m.len)), Some(('~', 3)));
        assert_eq!(fence_marker("plain text"), None);
        assert_eq!(fence_marker("``"), None);
        // インデント基準は is_atx_heading と対称（0〜3 個まで）。
        assert_eq!(fence_marker("   ```").map(|m| m.kind), Some('`'));
        assert_eq!(fence_marker("    ```"), None);
        assert_eq!(fence_marker("    ~~~"), None);
    }

    #[test]
    fn fence_open_close_conditions() {
        let open3 = fence_marker("```").expect("fence marker");
        let open4 = fence_marker("````").expect("fence marker");

        // backtick 開始フェンスの info string にバッククォートは置けない。
        assert!(fence_marker("```rust").expect("marker").can_open());
        assert!(!fence_marker("```a`b").expect("marker").can_open());
        assert!(fence_marker("~~~a`b").expect("marker").can_open());

        // 閉じは同じ文字種・開始以上の長さ・後続が空白のみ。
        assert!(fence_marker("```").expect("marker").can_close(&open3));
        assert!(fence_marker("````").expect("marker").can_close(&open3));
        assert!(!fence_marker("```").expect("marker").can_close(&open4));
        assert!(!fence_marker("```text").expect("marker").can_close(&open3));
        assert!(!fence_marker("~~~").expect("marker").can_close(&open3));
    }

    #[test]
    fn leading_indent_is_counted_consistently() {
        assert_eq!(leading_indent_spaces("no indent"), 0);
        assert_eq!(leading_indent_spaces("   three"), 3);
        assert_eq!(leading_indent_spaces("    four"), 4);
        assert_eq!(leading_indent_spaces("\tタブは対象外"), 0);
        // 同じ行に対して見出し・fence の判定基準が一致すること。
        assert!(!is_atx_heading("    # indented code"));
        assert_eq!(fence_marker("    ```"), None);
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
