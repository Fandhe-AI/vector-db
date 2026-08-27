//! 一括投入（複数ファイルをまとめたバッチ）に対する処理量ガード（TASK-122、
//! 対象ビヘイビア: INDEX-4。ポインタ: `docs/spec/05-tasks.md` TASK-122・
//! `docs/spec/04-behavior/indexing.md` INDEX-4）。
//!
//! `chunking::MAX_INPUT_BYTES`・`incremental::MAX_CHUNKS_PER_FILE`・
//! `incremental::MAX_INDEX_TOTAL_BYTES` はいずれもファイル 1 件分（TASK-120）の
//! 上限であり、複数ファイル合計に対する上限を持たない。本モジュールはその欠落を
//! 埋める、バッチ全体に対する 4 種の上限を扱う:
//!
//! 1. バッチあたり最大ファイル数（[`BatchLimits::max_files_per_batch`]）
//! 2. 1 ファイルあたり最大本文サイズ（[`BatchLimits::max_file_body_bytes`]）
//! 3. バッチ合計最大サイズ（`path` + 本文の合計。[`BatchLimits::max_batch_total_bytes`]）
//! 4. バッチあたり最大生成チャンク数（サーバー側算定。[`BatchLimits::max_batch_chunks`]）
//!
//! 判定タイミングの契約: 1 は `core::EngineCore::execute_insert_sql_batch` が
//! 束縛（`sql::parser::bind_insert_form`）より前に判定する。2〜3 は同メソッドの
//! 束縛ループ内で、各文の束縛直後に逐次判定する（束縛済みの `path`/`body` 長を
//! 使うため束縛自体は避けられないが、違反を検出した時点で残りの文の束縛を
//! 打ち切ることで複製の増幅を抑える。TASK-122 レビュー対応）。[`validate_batch_shape`]
//! はその最終防衛線として、バッチの解析段階（チャンク化・埋め込み・write
//! トランザクションのいずれよりも前）に 1〜3 をまとめて再検証する。4 は
//! [`validate_chunk_total`] でチャンク分割後・埋め込み処理の開始前に行う
//! （呼び出し元 `incremental::index_file_batch` のドキュメント参照）。いずれの
//! 超過も副作用ゼロ（redb・インメモリ索引・`operation_id` 台帳とも変更なし）で
//! `54000`（`PAYLOAD_TOO_LARGE`。ERR-2・TASK-152）として拒否する。
//!
//! wire 層の 1 メッセージ 1 MiB 上限（WIRE-4、`wire-server/src/limits.rs`）は
//! transport 層の別防御であり、本モジュールの代替にはならない（engine 側の
//! ローカル API 経由の一括投入にも本ガードが独立して適用される）。
//!
//! 判定関数はいずれも純粋関数で、本文・パスの複製や追加確保を行わない
//! （長さ情報のみで判定する。coding-rust.md「不安全な設計 / DoS」対応）。
//! 累算はすべて `checked_add` を用い、オーバーフローは拒否側へ倒す
//! （coding-rust.md「整数演算は checked_*／saturating_* を使う」）。

/// 一括投入 4 上限の設定値。既定値は [`Default`] 実装を参照。
#[derive(Debug, Clone, Copy)]
pub struct BatchLimits {
    /// ① バッチあたり最大ファイル数。
    pub max_files_per_batch: usize,
    /// ② 1 ファイルあたり最大本文サイズ（バイト）。単一ファイル経由の
    /// `chunking::MAX_INPUT_BYTES` と一致させ、バッチ経由で単発より大きい本文を
    /// 通さない。
    pub max_file_body_bytes: usize,
    /// ③ バッチ合計最大サイズ（バイト）。全ファイルの `path` 長 + 本文長の合計。
    pub max_batch_total_bytes: usize,
    /// ④ バッチあたり最大生成チャンク数（全ファイルの合計。サーバー側算定）。
    pub max_batch_chunks: usize,
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_files_per_batch: 128,
            max_file_body_bytes: crate::chunking::MAX_INPUT_BYTES,
            max_batch_total_bytes: 64 * 1024 * 1024,
            max_batch_chunks: crate::incremental::MAX_CHUNKS_PER_FILE * 4,
        }
    }
}

/// [`validate_batch_shape`]・[`validate_chunk_total`] の失敗理由。全 variant が
/// `wire_code() == "54000"`（`PAYLOAD_TOO_LARGE`）へ写像される（ERR-2・TASK-152）。
/// `detail`／`Display` には件数・上限値のみを含め、本文・パス・テナント情報は
/// 一切含めない（security.md「エラー・ログ経由で他テナントのデータ・存在情報を
/// 漏らさない」）。
#[derive(Debug)]
pub enum BatchLimitsError {
    /// ①超過。
    TooManyFiles { count: usize, max: usize },
    /// ②超過。`index` はバッチ内の 0 起点インデックス。
    FileBodyTooLarge {
        index: usize,
        len: usize,
        max: usize,
    },
    /// ③超過（累算オーバーフローも本 variant へ倒す。`total` はオーバーフロー時
    /// `usize::MAX` を用いる）。
    BatchTotalTooLarge { total: usize, max: usize },
    /// ④超過（累算オーバーフローも本 variant へ倒す）。
    TooManyChunks { total: usize, max: usize },
}

impl BatchLimitsError {
    /// SQLSTATE 風 `wire_code`（ERR-2 の共通分類。`sql/exec.rs` が
    /// `SqlSurfaceError::payload_too_large` へ写像する際の根拠）。
    pub fn wire_code(&self) -> &'static str {
        "54000"
    }
}

impl std::fmt::Display for BatchLimitsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchLimitsError::TooManyFiles { count, max } => {
                write!(f, "batch file count {count} exceeds limit {max}")
            }
            BatchLimitsError::FileBodyTooLarge { index, len, max } => {
                write!(
                    f,
                    "file at batch index {index} body size {len} exceeds limit {max}"
                )
            }
            BatchLimitsError::BatchTotalTooLarge { total, max } => {
                write!(f, "batch total size {total} exceeds limit {max}")
            }
            BatchLimitsError::TooManyChunks { total, max } => {
                write!(f, "batch chunk count {total} exceeds limit {max}")
            }
        }
    }
}

impl std::error::Error for BatchLimitsError {}

/// ①②③（ファイル数・ファイル単体本文サイズ・バッチ合計サイズ）を判定する
/// （バッチの解析段階。チャンク化・埋め込み・write トランザクションより前に
/// 呼ぶ契約。`incremental::index_file_batch` の唯一の呼び出し元）。
///
/// `files` はバッチ内の各ファイルの `(path.len(), body.len())`。本文・パス自体は
/// 受け取らない（長さ情報のみで判定し、複製・追加確保を行わない）。
pub(crate) fn validate_batch_shape(
    files: &[(usize, usize)],
    limits: &BatchLimits,
) -> Result<(), BatchLimitsError> {
    if files.len() > limits.max_files_per_batch {
        return Err(BatchLimitsError::TooManyFiles {
            count: files.len(),
            max: limits.max_files_per_batch,
        });
    }

    let mut total: usize = 0;
    for (index, (path_len, body_len)) in files.iter().enumerate() {
        if *body_len > limits.max_file_body_bytes {
            return Err(BatchLimitsError::FileBodyTooLarge {
                index,
                len: *body_len,
                max: limits.max_file_body_bytes,
            });
        }
        let file_total =
            path_len
                .checked_add(*body_len)
                .ok_or(BatchLimitsError::BatchTotalTooLarge {
                    total: usize::MAX,
                    max: limits.max_batch_total_bytes,
                })?;
        total = total
            .checked_add(file_total)
            .ok_or(BatchLimitsError::BatchTotalTooLarge {
                total: usize::MAX,
                max: limits.max_batch_total_bytes,
            })?;
    }

    if total > limits.max_batch_total_bytes {
        return Err(BatchLimitsError::BatchTotalTooLarge {
            total,
            max: limits.max_batch_total_bytes,
        });
    }
    Ok(())
}

/// ④（バッチあたり最大生成チャンク数）を判定する（チャンク分割後・埋め込み処理の
/// 開始前に呼ぶ契約。呼び出し元が全ファイルの `chunk_phase` 結果のチャンク数を
/// `checked_add` で累算した値を渡す）。
pub(crate) fn validate_chunk_total(
    total_chunks: usize,
    limits: &BatchLimits,
) -> Result<(), BatchLimitsError> {
    if total_chunks > limits.max_batch_chunks {
        return Err(BatchLimitsError::TooManyChunks {
            total: total_chunks,
            max: limits.max_batch_chunks,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> BatchLimits {
        BatchLimits {
            max_files_per_batch: 4,
            max_file_body_bytes: 10,
            max_batch_total_bytes: 30,
            max_batch_chunks: 5,
        }
    }

    #[test]
    fn shape_accepts_within_all_limits() {
        // 4 ファイル・各本文 5 バイト・path 1 バイト = 合計 24 バイト（上限 30 以内）。
        let files = vec![(1, 5), (1, 5), (1, 5), (1, 5)];
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_one_file_over_count_limit() {
        let files = vec![(1, 1), (1, 1), (1, 1), (1, 1), (1, 1)];
        assert!(matches!(
            validate_batch_shape(&files, &limits()),
            Err(BatchLimitsError::TooManyFiles { count: 5, max: 4 })
        ));
    }

    #[test]
    fn shape_accepts_file_count_exactly_at_limit() {
        let files = vec![(1, 1), (1, 1), (1, 1), (1, 1)];
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_single_file_body_over_limit() {
        let files = vec![(1, 11)];
        assert!(matches!(
            validate_batch_shape(&files, &limits()),
            Err(BatchLimitsError::FileBodyTooLarge {
                index: 0,
                len: 11,
                max: 10
            })
        ));
    }

    #[test]
    fn shape_accepts_single_file_body_exactly_at_limit() {
        let files = vec![(1, 10)];
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_batch_total_over_limit_even_when_each_file_is_within_its_own_limit() {
        // 各ファイルは②の 10 バイト以内だが、合計 33 バイトは③の 30 を超える。
        let files = vec![(1, 10), (1, 10), (1, 10)];
        assert!(matches!(
            validate_batch_shape(&files, &limits()),
            Err(BatchLimitsError::BatchTotalTooLarge { total: 33, max: 30 })
        ));
    }

    #[test]
    fn shape_accepts_batch_total_exactly_at_limit() {
        let files = vec![(1, 10), (1, 9), (1, 8)];
        // 合計 = (1+10) + (1+9) + (1+8) = 30。
        assert!(validate_batch_shape(&files, &limits()).is_ok());
    }

    #[test]
    fn shape_rejects_batch_total_overflow_instead_of_panicking() {
        let files = vec![(usize::MAX, usize::MAX)];
        // 本文サイズ自体は②を通らないため FileBodyTooLarge が先に出る場合もあるが、
        // ここでは②を無効化した上限で③のオーバーフロー処理を確認する。
        let overflow_limits = BatchLimits {
            max_file_body_bytes: usize::MAX,
            ..limits()
        };
        assert!(matches!(
            validate_batch_shape(&files, &overflow_limits),
            Err(BatchLimitsError::BatchTotalTooLarge { .. })
        ));
    }

    #[test]
    fn chunk_total_accepts_up_to_limit() {
        assert!(validate_chunk_total(5, &limits()).is_ok());
    }

    #[test]
    fn chunk_total_rejects_one_over_limit() {
        assert!(matches!(
            validate_chunk_total(6, &limits()),
            Err(BatchLimitsError::TooManyChunks { total: 6, max: 5 })
        ));
    }

    #[test]
    fn wire_code_is_payload_too_large_for_all_variants() {
        assert_eq!(
            BatchLimitsError::TooManyFiles { count: 1, max: 0 }.wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::FileBodyTooLarge {
                index: 0,
                len: 1,
                max: 0
            }
            .wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::BatchTotalTooLarge { total: 1, max: 0 }.wire_code(),
            "54000"
        );
        assert_eq!(
            BatchLimitsError::TooManyChunks { total: 1, max: 0 }.wire_code(),
            "54000"
        );
    }
}
