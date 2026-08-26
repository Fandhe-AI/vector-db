//! 増分インデックス反映モジュール（TASK-120、対象ビヘイビア: INDEX-1, INDEX-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-120・`docs/spec/04-behavior/indexing.md`
//! INDEX-1, INDEX-2）。
//!
//! 責務境界: ファイル形 `INSERT`（`sql::parser::BoundFileInsert`）1 件分を、
//! チャンク化（`chunking::chunk_file`。TASK-119）→ 埋め込み（`embedding::Embedder`。
//! write トランザクションの外で実行）→ テナント境界付き置換書き込み
//! （`tenant::replace_typed_rows_by_text_key`）の順で結線する（`sql/exec.rs::execute_file_insert`
//! から呼ばれる。同一パス再送時の置換セマンティクスは TASK-123 の決定に従う。
//! ポインタ: `docs/design/resend-semantics.md`）。
//!
//! 途中で失敗した場合は write トランザクションを一切開始しない、または開始済み
//! トランザクションを commit せず abort する（副作用ゼロ。coding-rust.md
//! 「エラー契約は fail-closed とする」）。

use std::time::{Duration, Instant};

use crate::embedding::{EmbedError, Embedder};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::storage::{Storage, Visibility};
use crate::tenant::{ReplaceOutcome, TenantWriteError};

/// ファイル単位で生成してよいチャンク数の上限。埋め込み呼び出し・確保量を有界化する
/// （coding-rust.md「不安全な設計 / DoS」対応。一括投入 4 件上限は TASK-122 の管轄で
/// あり、本上限はそれを代替しない）。
pub const MAX_CHUNKS_PER_FILE: usize = 4_096;

/// [`index_file`] の挙動を調整する設定。
#[derive(Debug, Clone)]
pub struct IncrementalConfig {
    pub chunking: crate::chunking::ChunkingConfig,
    pub max_chunks_per_file: usize,
}

impl Default for IncrementalConfig {
    fn default() -> Self {
        Self {
            chunking: crate::chunking::ChunkingConfig::default(),
            max_chunks_per_file: MAX_CHUNKS_PER_FILE,
        }
    }
}

/// 各段階の所要時間（INDEX-1 の計測対象。呼び出し元がベンチマーク・回帰テスト
/// （TASK-121）で利用できるよう公開する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTiming {
    pub chunking: Duration,
    pub embedding: Duration,
    pub write: Duration,
}

/// [`index_file`] の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOutcome {
    pub chunks_written: usize,
    pub rows_replaced: usize,
    pub timing: IndexTiming,
}

/// [`index_file`] の失敗理由。`wire_code()` で `sql::allowlist::SqlSurfaceError` への
/// 写像先を示す（実際の写像は `sql/exec.rs::execute_file_insert` が集約する）。
#[derive(Debug)]
pub enum IncrementalError {
    /// チャンク化入力・出力チャンク数の上限超過（`54000` 相当）。クライアントが
    /// 送った本文サイズ・チャンク数に起因するものだけをこの variant にする。
    ChunkingTooLarge(String),
    /// サーバー側の事象に起因する失敗（`XX000` 相当）。チャンク化設定の不正
    /// （`chunking::ChunkingError::InvalidConfig`。例: `lines_per_chunk == 0`）や
    /// 行バッファ確保失敗（メモリ逼迫）が該当する。これらをクライアント入力起因の
    /// `54000` として返すと、再試行・障害判定を誤らせるため分離する
    /// （Cursor Bugbot 指摘・PR #221）。`detail` は原因を展開しない固定文言のみ。
    Internal(&'static str),
    /// 埋め込みサービスの失敗・次元不一致（`XX000` 相当。応答本文・入力本文を
    /// 含めない）。
    Embed(EmbedError),
    /// テナント境界付き書き込みの失敗（[`TenantWriteError`] をそのまま保持）。
    Write(TenantWriteError),
    /// チャンク化の結果、書き込み対象のチャンクが 0 件になった（`22000` 相当）。
    /// 空・空白のみの本文を送った場合に、既存チャンクを全削除したまま
    /// 挿入 0 件で世代だけ bump するのを防ぐ（`replace_typed_rows_by_text_key`
    /// の「同一パスの既存行を全削除 → 新チャンク行を挿入」契約が、意図しない
    /// 全削除（実質的な索引破壊）に化けるのを fail-closed に拒否する）。
    EmptyChunks,
}

impl std::fmt::Display for IncrementalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IncrementalError::ChunkingTooLarge(detail) => {
                write!(f, "chunking limit exceeded: {detail}")
            }
            IncrementalError::Internal(detail) => write!(f, "{detail}"),
            IncrementalError::Embed(e) => write!(f, "embedding failed: {e}"),
            IncrementalError::Write(_) => write!(f, "incremental index write failed"),
            IncrementalError::EmptyChunks => {
                write!(f, "file body produced no chunks to index")
            }
        }
    }
}

impl std::error::Error for IncrementalError {}

impl From<EmbedError> for IncrementalError {
    fn from(e: EmbedError) -> Self {
        IncrementalError::Embed(e)
    }
}

impl From<TenantWriteError> for IncrementalError {
    fn from(e: TenantWriteError) -> Self {
        IncrementalError::Write(e)
    }
}

/// ファイル形 `INSERT` の束縛結果（`sql::parser::bind_insert_form` が構築）。
///
/// `path`/`body` 以外の Text 列（例 `lang`）は全チャンク行へ同じ値を複製するため、
/// スキーマ列順の「チャンク本文以外」のテンプレート値をあらかじめ保持する
/// （`sql/parser.rs` モジュールドキュメント参照）。
pub struct BoundFileIndexInput<'a> {
    pub table: &'a str,
    pub path: &'a str,
    pub body: &'a str,
    /// スキーマ列順のテンプレート値。`path_column_index`/`body_column_index`/
    /// VECTOR 列の位置は呼び出し時点でプレースホルダ（`Value::Null`）でよい
    /// （各チャンク行の構築時に上書きするため）。
    pub template_values: &'a [Value],
    pub path_column_index: usize,
    pub body_column_index: usize,
    pub vector_column_index: usize,
}

/// ファイル 1 件分（パス＋本文）をチャンク化・埋め込み・置換書き込みする
/// （TASK-120・INDEX-1, INDEX-2）。`sql/exec.rs::execute_file_insert` から呼ばれる
/// 唯一の入口。
///
/// 手順（コメント順序が実装順序と一致する。§2.3 の設計順序）:
/// 1. チャンク化（`chunking::chunk_file`）— 上限超過は副作用ゼロで `Err`
/// 2. ファイル単位のチャンク数上限ガード（`config.max_chunks_per_file`）
/// 3. 埋め込み（`embedder.embed_batch`）— **write トランザクションの外**で実行
/// 4. `tenant::replace_typed_rows_by_text_key` で単一 write トランザクション内の
///    「同一パスの既存行を全削除 → 新チャンク行を挿入 → 世代 bump」
///
/// `Embedder::dim()` とスキーマの `VECTOR(N)` の不一致はチャンク化・埋め込み呼び出し
/// より前（手順の最初）に検出する。埋め込み実装が自己申告と異なる次元のベクトルを
/// 返した場合の防御的検証は埋め込み呼び出し直後で別途行う。次元検証自体は
/// `tenant.rs` 側の `validate_embedding_dim` が書き込みトランザクション内でも
/// 行うため、いずれも二重防御になる。
pub fn index_file(
    storage: &Storage,
    ctx: &PolicyContext,
    embedder: &dyn Embedder,
    config: &IncrementalConfig,
    input: &BoundFileIndexInput<'_>,
) -> Result<IndexOutcome, IncrementalError> {
    // `embedder.dim()` を対象テーブルの `VECTOR(N)` と突き合わせる（サーバー側の
    // Embedder 設定とスキーマの不整合。チャンク化・埋め込み呼び出しの前に検出し、
    // 誤設定時の無駄な計算・外部 I/O を避ける。`tenant.rs` 側の
    // `validate_embedding_dim` が書き込みトランザクション内でも同じ検証を行うため
    // 二重防御になる）。
    let schema = storage
        .get_table_schema(input.table)
        .map_err(TenantWriteError::Catalog)?;
    let table_dim = schema.vector_dim().ok_or_else(|| {
        TenantWriteError::Catalog(crate::catalog::CatalogError::Invalid(
            "table has no VECTOR column".to_string(),
        ))
    })?;
    if embedder.dim() != table_dim {
        return Err(IncrementalError::Embed(EmbedError::DimMismatch {
            expected: table_dim,
            got: embedder.dim() as usize,
        }));
    }

    let chunking_start = Instant::now();
    // `chunk_file` の失敗を原因別に写像する。入力本文サイズ超過のみクライアント起因
    // （`54000`）とし、チャンク化設定の不正はサーバー構成の誤りとして `XX000` へ倒す
    // （Cursor Bugbot 指摘・PR #221。`detail` に設定値・本文を載せない）。
    let chunks = crate::chunking::chunk_file(input.path, input.body, &config.chunking).map_err(
        |e| match e {
            crate::chunking::ChunkingError::InputTooLarge { .. }
            | crate::chunking::ChunkingError::TooManyLines { .. } => {
                IncrementalError::ChunkingTooLarge(e.to_string())
            }
            crate::chunking::ChunkingError::InvalidConfig { .. } => {
                IncrementalError::Internal("invalid chunking configuration")
            }
        },
    )?;
    let chunking_elapsed = chunking_start.elapsed();

    // 空・空白のみの本文（`chunk_file` が 0 チャンクを返す入力）は、ここで
    // fail-closed に拒否する。ガードなしで `replace_typed_rows_by_text_key`
    // まで進むと、同一パスの既存チャンクを全削除したうえで挿入 0 件のまま
    // 世代だけ bump してコミットし、実質的な索引破壊が「成功」応答になる
    // （Issue #68 レビュー指摘）。
    if chunks.is_empty() {
        return Err(IncrementalError::EmptyChunks);
    }

    if chunks.len() > config.max_chunks_per_file {
        return Err(IncrementalError::ChunkingTooLarge(format!(
            "chunk count {} exceeds limit {}",
            chunks.len(),
            config.max_chunks_per_file
        )));
    }

    let embedding_start = Instant::now();
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let vectors = embedder.embed_batch(&texts)?;
    if vectors.len() != chunks.len() {
        return Err(IncrementalError::Embed(EmbedError::InvalidResponse));
    }
    let expected_dim = embedder.dim();
    for v in &vectors {
        if v.len() != expected_dim as usize {
            return Err(IncrementalError::Embed(EmbedError::DimMismatch {
                expected: expected_dim,
                got: v.len(),
            }));
        }
    }
    let embedding_elapsed = embedding_start.elapsed();

    let mut rows: Vec<Vec<Value>> = Vec::new();
    rows.try_reserve_exact(chunks.len())
        .map_err(|_| IncrementalError::Internal("failed to reserve chunk rows"))?;
    for (chunk, vector) in chunks.iter().zip(vectors) {
        let mut values = input.template_values.to_vec();
        if let Some(slot) = values.get_mut(input.path_column_index) {
            *slot = Value::Text(input.path.to_string());
        }
        if let Some(slot) = values.get_mut(input.body_column_index) {
            *slot = Value::Text(chunk.text.clone());
        }
        if let Some(slot) = values.get_mut(input.vector_column_index) {
            *slot = Value::Vector(vector);
        }
        rows.push(values);
    }

    let write_start = Instant::now();
    let ReplaceOutcome {
        removed: _,
        inserted,
        first_id: _,
    } = crate::tenant::replace_typed_rows_by_text_key(
        storage,
        input.table,
        ctx,
        // ファイル形 INSERT のチャンク置換キーは常に `path` 列（`sql/parser.rs`
        // モジュールドキュメントの判別規則 §2.1 で固定されている）。
        "path",
        input.path,
        Visibility::Private,
        &rows,
    )?;
    let write_elapsed = write_start.elapsed();

    Ok(IndexOutcome {
        chunks_written: chunks.len(),
        rows_replaced: inserted,
        timing: IndexTiming {
            chunking: chunking_elapsed,
            embedding: embedding_elapsed,
            write: write_elapsed,
        },
    })
}
