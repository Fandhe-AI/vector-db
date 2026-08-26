//! 埋め込み抽象モジュール（TASK-120、対象ビヘイビア: INDEX-1, INDEX-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-120・`docs/spec/04-behavior/indexing.md`
//! INDEX-1, INDEX-2）。
//!
//! 責務境界: `incremental.rs`（ファイル形 `INSERT` の増分インデックス反映）が
//! チャンク本文をベクトルへ変換する際に呼び出す差し替え可能な注入点を提供する。
//! 本モジュール自身は外部埋め込みサービスへ一切接続せず（`dependency-policy.md`:
//! 依存追加は本タスクのスコープ外）、決定的・ネットワーク不要な参照実装
//! [`HashingEmbedder`] のみを提供する。実サービス（Ollama 等）クライアントは
//! 独立した設計・レビュー単位として後続タスクの管轄とする（PR 本文「対象外」参照）。
//!
//! `core::EngineCore` は `Option<Box<dyn Embedder>>` として本 trait の実装を保持し
//! （[`Self::with_embedder`] で注入）、未設定時はファイル形 `INSERT` を fail-closed に
//! 拒否する（意味のないベクトルが黙って索引化される fail-open を防ぐ。
//! `core.rs` モジュールドキュメント参照）。

/// チャンク本文の列をベクトル列へ変換する差し替え可能な注入点。
///
/// 呼び出し元は `incremental.rs::index_file`（write トランザクションの外で実行し、
/// 単一ライタの長時間占有を防ぐ。coding-rust.md「不安全な設計 / DoS」対応）。
pub trait Embedder: Send + Sync {
    /// この実装が返すベクトルの次元。呼び出し元は対象テーブルの `VECTOR(N)` と
    /// 突き合わせて次元不一致を検出する（`TableSchema::validate_embedding_dim`）。
    fn dim(&self) -> u32;

    /// `texts` の各要素を同じ順序でベクトルへ変換する。
    ///
    /// - 戻り値の長さは `texts.len()` と一致する
    /// - 各ベクトルの長さは [`Self::dim`] と一致する
    /// - 入力本文（`texts`）をエラーへ含めない（security.md「エラー・ログ経由で
    ///   他テナントのデータ・存在情報を漏らさない」と同じ方針。本文は untrusted）
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// [`Embedder::embed_batch`] の失敗理由。
///
/// メッセージは英語（japanese-style.md: プログラム出力文字列は英語）。入力本文・
/// 応答本文を含めない（security.md P0）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedError {
    /// 埋め込みサービスが利用不能（未接続・タイムアウト等。実サービス実装向け。
    /// 参照実装 [`HashingEmbedder`] は返さない）。
    Unavailable,
    /// 埋め込みサービスの応答が想定形状ではなかった（実サービス実装向け）。
    InvalidResponse,
    /// 返されたベクトルの次元が [`Embedder::dim`] と一致しなかった。
    DimMismatch { expected: u32, got: usize },
    /// 1 回のバッチ呼び出しに対する入力件数が上限を超えた
    /// （coding-rust.md「長さフィールドは上限検証してからアロケーションに使う」対応）。
    TooManyInputs { len: usize, max: usize },
    /// 構築時に指定された次元が受理範囲（`1..=`[`MAX_EMBEDDER_DIM`]）外だった
    /// （codex-review P1 指摘・PR #221。無検証の巨大次元で infallible な確保を
    /// 行わないため、構築を `Result` にして境界で弾く）。
    InvalidDim { dim: u32, max: u32 },
    /// ベクトル確保に失敗した（メモリ逼迫）。`vec![]` の infallible 確保による
    /// abort を避け、`try_reserve_exact` の失敗をエラーとして返す。
    AllocationFailed,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Unavailable => write!(f, "embedding service unavailable"),
            EmbedError::InvalidResponse => write!(f, "embedding service returned invalid response"),
            EmbedError::DimMismatch { expected, got } => {
                write!(f, "embedding dim mismatch: expected {expected}, got {got}")
            }
            EmbedError::TooManyInputs { len, max } => {
                write!(f, "embedding batch too large: {len} inputs (max {max})")
            }
            EmbedError::InvalidDim { dim, max } => {
                write!(f, "invalid embedding dim: {dim} (must be 1..={max})")
            }
            EmbedError::AllocationFailed => write!(f, "embedding allocation failed"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// 1 回の [`Embedder::embed_batch`] 呼び出しで受理する最大入力件数。
///
/// `incremental.rs` のファイル単位チャンク数上限（`MAX_CHUNKS_PER_FILE`）とは
/// 独立した本モジュール自身の防御線（呼び出し元の上限設定に関わらず、確保量を
/// 有界に保つ）。
pub const MAX_EMBED_BATCH: usize = 8_192;

/// [`Embedder`] 実装が受理してよい最大次元。カタログの `VECTOR(N)` 上限
/// （`storage::MAX_EMBEDDING_DIM`）と同値に固定する（この上限を超える次元は
/// どのみちテーブルへ書けないため、埋め込み側で先に弾いて確保量を有界にする）。
pub const MAX_EMBEDDER_DIM: u32 = crate::storage::MAX_EMBEDDING_DIM;

/// 決定的・ネットワーク不要な参照実装（feature hashing → 固定次元 → L2 正規化）。
///
/// **意味的埋め込みではない**（類似語・類義語を近づける学習済みモデルを一切
/// 使わない、小文字化トークンのハッシュ値による決定的な特徴射影）。用途はテスト・
/// ローカル検証・TASK-121（増分/全再構築比率の受け入れ基準回帰）の計測であり、
/// 検索品質（Recall）の実運用基準にはならない。
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    dim: u32,
}

impl HashingEmbedder {
    /// `dim` で参照実装を構築する。受理範囲は `1..=`[`MAX_EMBEDDER_DIM`]。
    ///
    /// 範囲外を `Result` で拒否する（`embed_batch` が入力ごとに `dim` 要素を確保する
    /// ため、未検証の巨大次元を受け付けると `Result` を返す API でありながら確保失敗が
    /// abort になる。codex-review P1 指摘・PR #221。coding-rust.md「長さフィールドは
    /// 上限検証してからアロケーションに使う」）。
    pub fn new(dim: u32) -> Result<Self, EmbedError> {
        if dim == 0 || dim > MAX_EMBEDDER_DIM {
            return Err(EmbedError::InvalidDim {
                dim,
                max: MAX_EMBEDDER_DIM,
            });
        }
        Ok(Self { dim })
    }

    /// 1 語（小文字化済み）を FNV-1a でハッシュし、`dim` 個の次元へ符号付きで
    /// 加算する決定的な feature hashing（外部クレート不使用）。
    fn hash_token_into(dim: usize, token: &str, out: &mut [f32]) {
        // FNV-1a: 64bit オフセットバイアス・素数は標準定数（依存追加なしで手書き）。
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in token.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        if dim == 0 {
            return;
        }
        let bucket = (hash as usize) % dim;
        // ハッシュの最上位ビットを符号として使い、異なるトークンが単純加算だけで
        // 際限なく同方向へ積み上がるのを避ける（決定的な擬似乱数符号）。
        let sign = if hash & (1 << 63) != 0 {
            1.0f32
        } else {
            -1.0f32
        };
        if let Some(slot) = out.get_mut(bucket) {
            *slot += sign;
        }
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.len() > MAX_EMBED_BATCH {
            return Err(EmbedError::TooManyInputs {
                len: texts.len(),
                max: MAX_EMBED_BATCH,
            });
        }
        let dim = self.dim as usize;
        let mut out = Vec::new();
        out.try_reserve_exact(texts.len())
            .map_err(|_| EmbedError::TooManyInputs {
                len: texts.len(),
                max: MAX_EMBED_BATCH,
            })?;
        for text in texts {
            // `vec![0.0; dim]` の infallible 確保を避け、失敗をエラーへ変換する。
            let mut v: Vec<f32> = Vec::new();
            v.try_reserve_exact(dim)
                .map_err(|_| EmbedError::AllocationFailed)?;
            v.resize(dim, 0.0f32);
            for token in text.split_whitespace() {
                let lower = token.to_lowercase();
                Self::hash_token_into(dim, &lower, &mut v);
            }
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            out.push(v);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_batch_is_deterministic() {
        let e = HashingEmbedder::new(16).expect("valid dim");
        let a = e.embed_batch(&["hello world", "second text"]).unwrap();
        let b = e.embed_batch(&["hello world", "second text"]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn embed_batch_output_matches_configured_dim() {
        let e = HashingEmbedder::new(32).expect("valid dim");
        let out = e.embed_batch(&["some text here"]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 32);
    }

    #[test]
    fn embed_batch_empty_input_returns_empty_output() {
        let e = HashingEmbedder::new(8).expect("valid dim");
        let out = e.embed_batch(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn embed_batch_output_is_l2_normalized() {
        let e = HashingEmbedder::new(16).expect("valid dim");
        let out = e
            .embed_batch(&["some non-empty text with several tokens"])
            .unwrap();
        let norm: f32 = out[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4 || norm == 0.0);
    }

    #[test]
    fn embed_batch_rejects_over_max_batch() {
        let e = HashingEmbedder::new(4).expect("valid dim");
        let texts: Vec<&str> = std::iter::repeat_n("x", MAX_EMBED_BATCH + 1).collect();
        let err = e.embed_batch(&texts).unwrap_err();
        assert_eq!(
            err,
            EmbedError::TooManyInputs {
                len: MAX_EMBED_BATCH + 1,
                max: MAX_EMBED_BATCH,
            }
        );
    }

    // codex-review P1 指摘・PR #221: 未検証の次元で infallible な巨大確保をしない。
    #[test]
    fn new_rejects_zero_and_over_max_dim() {
        assert_eq!(
            HashingEmbedder::new(0).unwrap_err(),
            EmbedError::InvalidDim {
                dim: 0,
                max: MAX_EMBEDDER_DIM
            }
        );
        assert_eq!(
            HashingEmbedder::new(u32::MAX).unwrap_err(),
            EmbedError::InvalidDim {
                dim: u32::MAX,
                max: MAX_EMBEDDER_DIM
            }
        );
        assert!(HashingEmbedder::new(MAX_EMBEDDER_DIM).is_ok());
    }

    #[test]
    fn embed_batch_different_texts_produce_different_vectors() {
        let e = HashingEmbedder::new(64).expect("valid dim");
        let out = e
            .embed_batch(&["alpha beta gamma", "completely different words"])
            .unwrap();
        assert_ne!(out[0], out[1]);
    }
}
