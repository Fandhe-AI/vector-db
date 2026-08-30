//! Issue #333（SEARCH-7 方式変更）: `rerank::CrossEncoderBackend`（`rerank.rs`）の実
//! ONNX 推論実装。`rerank::CrossEncoderReranker` から `&self` のみで呼ばれる純粋な
//! 推論アダプタで、テナント境界・RLS 判定・SQL 表層とは一切結線しない
//! （`cross-encoder` feature 限定・オーナー承認済み依存 `ort`/`tokenizers`。
//! 承認記録: `crates/engine/Cargo.toml` コメント・Issue #333 再 open コメント
//! 2026-08-30）。
//!
//! # `ORT_DYLIB_PATH` の扱い（fail-closed 契約の要）
//!
//! `ort` の `load-dynamic` feature は既定で `libonnxruntime.so`（linux）という
//! 固定名を `dlopen` するが、多くのディストリビューションの実ファイル名は
//! バージョン付き（`libonnxruntime.so.N`）でこれと一致しない。既定の名前解決に
//! 任せて `ort` の他 API（`Session::builder()` 等）へ触れると、`ort` 内部の
//! `setup_api()` が dylib ロード失敗を `.expect(..)` で **panic** させる
//! （`Result` を返さない）。coding-rust.md「ライブラリコードでは `Result` を返し、
//! panic させない」に反するため、本モジュールは [`OnnxCrossEncoderBackend::from_files`]
//! の冒頭で環境変数 `ORT_DYLIB_PATH` を自前で読み、未設定ならここで
//! [`CrossEncoderError::Backend`] を返して **`ort::` の他 API を一切呼ばずに**
//! 打ち切る。設定済みの場合のみ `ort::init_from`（`Result` を返す明示ロード API）で
//! dylib を確定させてから `Session` を構築するため、`ORT_DYLIB_PATH` 未設定環境
//! （`--all-features` の `make test`/`make ci`・pre-push フックがモデル・dylib なしで
//! 走る既定経路を含む）でも本モジュールの呼び出しは panic せず `Err` を返す。

use std::env;
use std::path::Path;
use std::sync::Mutex;

use ort::session::Session;
use ort::value::Tensor;
use tokenizers::utils::padding::PaddingParams;
use tokenizers::utils::truncation::TruncationParams;
use tokenizers::Tokenizer;

use super::{CrossEncoderBackend, CrossEncoderError, MAX_CROSS_ENCODER_SEQ_LEN};

/// `ort`（ONNX Runtime）+ `tokenizers` による実推論バックエンド。
/// [`CrossEncoderReranker`](super::CrossEncoderReranker) から `&self` で呼ばれるため、
/// `ort::session::Session::run` が要求する `&mut self` を内部の [`Mutex`] で吸収する
/// （`unsafe`ブロックは使わない。poisoned lock は fail-closed で `Backend` エラーへ変換する）。
pub struct OnnxCrossEncoderBackend {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    max_seq_len: usize,
    /// モデルが `token_type_ids` 入力を持つか（構築時に [`Session::inputs`] から
    /// 検査済み。BERT 系クロスエンコーダは通常持つが、モデルによっては省略される
    /// ため、無い場合は渡さない）。
    has_token_type_ids: bool,
}

impl OnnxCrossEncoderBackend {
    /// `model_path`（ONNX モデルファイル）・`tokenizer_path`（`tokenizer.json`）から
    /// バックエンドを構築する。`max_seq_len` はトークナイザの truncation 上限として
    /// ロード時に設定し（推論時に無制限へ伸びる経路を構造的に塞ぐ契約。
    /// [`CrossEncoderBackend::max_seq_len`] のドキュメント参照）、以後
    /// [`CrossEncoderReranker::new`](super::CrossEncoderReranker::new) が
    /// [`CrossEncoderConfig::max_seq_len`](super::CrossEncoderConfig::max_seq_len)
    /// との一致を検証する。
    ///
    /// untrusted 入力の扱い（coding-rust.md）: `model_path`・`tokenizer_path` は
    /// ローカル運用者が指定するファイルパスであり wire 経由の入力ではないが、
    /// 本関数自身は該当ファイルが存在しない・破損している場合でも panic せず
    /// [`CrossEncoderError`] を返す契約を維持する（モジュール冒頭ドキュメント参照）。
    pub fn from_files(
        model_path: &Path,
        tokenizer_path: &Path,
        max_seq_len: usize,
    ) -> Result<Self, CrossEncoderError> {
        // codex-review 指摘（PR #336 P1）: `CrossEncoderConfig::new` を経由しない
        // 直接呼び出し経路でも `MAX_CROSS_ENCODER_SEQ_LEN` の上限を必ず強制する
        // （`CrossEncoderReranker::new` の一致検査だけに委ねると、`from_files`／
        // `score_pairs` を直接呼ぶ利用者がこの上限契約を回避してトークナイズ時の
        // テンソル確保を無制限化できてしまうため、構築時点で fail-closed に拒否する）。
        if max_seq_len == 0 || max_seq_len > MAX_CROSS_ENCODER_SEQ_LEN {
            return Err(CrossEncoderError::TruncationFailed);
        }

        // モジュール冒頭ドキュメント参照: `ort::` の他 API より前に、ここで
        // 明示的に dylib を解決する。未設定のまま先へ進むと `ort` 内部の
        // panic 経路（`.expect("Failed to load ONNX Runtime dylib")`）に到達しうる。
        let dylib_path = env::var_os("ORT_DYLIB_PATH").filter(|s| !s.is_empty());
        let dylib_path = match dylib_path {
            Some(p) => p,
            None => {
                return Err(CrossEncoderError::Backend(
                    "ORT_DYLIB_PATH is not set".to_string(),
                ));
            }
        };
        ort::init_from(&dylib_path)
            .map_err(|e| {
                CrossEncoderError::Backend(format!("failed to load onnxruntime dylib: {e}"))
            })?
            .commit();

        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| CrossEncoderError::Backend(format!("failed to load tokenizer: {e}")))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_seq_len,
                ..Default::default()
            }))
            .map_err(|_| CrossEncoderError::TruncationFailed)?;
        // バッチ内最長へパディング（`max_seq_len` は truncation で既に上限化済み
        // なので、常に `max_seq_len` へ固定パディングするより無駄なテンソル要素を
        // 減らせる）。
        tokenizer.with_padding(Some(PaddingParams::default()));

        let mut builder = Session::builder().map_err(|e| {
            CrossEncoderError::Backend(format!("failed to init onnxruntime session builder: {e}"))
        })?;
        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| CrossEncoderError::Backend(format!("failed to load onnx model: {e}")))?;

        // 入力構成の検証（3.2 節の契約）: `input_ids`/`attention_mask` を持たない
        // モデルは推論経路の前提が崩れるため構築時に拒否する（fail-closed）。
        let input_names: Vec<&str> = session.inputs().iter().map(|o| o.name()).collect();
        if !input_names.contains(&"input_ids") || !input_names.contains(&"attention_mask") {
            return Err(CrossEncoderError::Backend(format!(
                "unexpected onnx model input names (expected input_ids/attention_mask): {input_names:?}"
            )));
        }
        let has_token_type_ids = input_names.contains(&"token_type_ids");

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            max_seq_len,
            has_token_type_ids,
        })
    }
}

impl CrossEncoderBackend for OnnxCrossEncoderBackend {
    fn score_pairs(&self, query: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        if passages.is_empty() {
            return Ok(Vec::new());
        }

        // (query, passage) ペアをクロスエンコーダの規範形（sentence-pair）で
        // トークナイズする。`add_special_tokens = true` で [CLS]/[SEP] 等を付与する。
        let pairs: Vec<(&str, &str)> = passages.iter().map(|&p| (query, p)).collect();
        let encodings = self
            .tokenizer
            .encode_batch(pairs, true)
            .map_err(|e| CrossEncoderError::Backend(format!("tokenization failed: {e}")))?;

        let batch = encodings.len();
        let seq_len = encodings.first().map(|e| e.get_ids().len()).unwrap_or(0);
        // `PaddingParams::default()`（`BatchLongest`）契約の前提: バッチ内の全
        // encoding が同じ長さになっていることを検証する（崩れている場合は
        // テンソル化できないため fail-closed で拒否する）。
        if encodings.iter().any(|e| e.get_ids().len() != seq_len) {
            return Err(CrossEncoderError::Backend(
                "tokenizer batch padding produced inconsistent lengths".to_string(),
            ));
        }

        let total = batch
            .checked_mul(seq_len)
            .ok_or_else(|| CrossEncoderError::Backend("token tensor size overflow".to_string()))?;
        let mut input_ids: Vec<i64> = Vec::with_capacity(total);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(total);
        let mut token_type_ids: Vec<i64> = Vec::with_capacity(total);
        for enc in &encodings {
            input_ids.extend(enc.get_ids().iter().map(|&id| i64::from(id)));
            attention_mask.extend(enc.get_attention_mask().iter().map(|&m| i64::from(m)));
            token_type_ids.extend(enc.get_type_ids().iter().map(|&t| i64::from(t)));
        }

        let shape = vec![batch as i64, seq_len as i64];
        let mut inputs: Vec<(String, ort::session::SessionInputValue<'_>)> = Vec::with_capacity(3);
        inputs.push((
            "input_ids".to_string(),
            Tensor::from_array((shape.clone(), input_ids))
                .map_err(|e| {
                    CrossEncoderError::Backend(format!("failed to build input_ids tensor: {e}"))
                })?
                .into(),
        ));
        inputs.push((
            "attention_mask".to_string(),
            Tensor::from_array((shape.clone(), attention_mask))
                .map_err(|e| {
                    CrossEncoderError::Backend(format!(
                        "failed to build attention_mask tensor: {e}"
                    ))
                })?
                .into(),
        ));
        if self.has_token_type_ids {
            inputs.push((
                "token_type_ids".to_string(),
                Tensor::from_array((shape, token_type_ids))
                    .map_err(|e| {
                        CrossEncoderError::Backend(format!(
                            "failed to build token_type_ids tensor: {e}"
                        ))
                    })?
                    .into(),
            ));
        }

        let mut session = self.session.lock().map_err(|_| {
            CrossEncoderError::Backend("onnxruntime session mutex poisoned".to_string())
        })?;
        let outputs = session.run(inputs).map_err(|e| {
            CrossEncoderError::Backend(format!("onnxruntime inference failed: {e}"))
        })?;
        let output = outputs.values().next().ok_or_else(|| {
            CrossEncoderError::Backend("onnxruntime session returned no outputs".to_string())
        })?;
        let (_shape, data) = output.try_extract_tensor::<f32>().map_err(|e| {
            CrossEncoderError::Backend(format!("failed to extract onnxruntime output tensor: {e}"))
        })?;

        if data.len() == batch {
            Ok(data.iter().map(|&s| f64::from(s)).collect())
        } else {
            Err(CrossEncoderError::LengthMismatch {
                expected: batch,
                got: data.len(),
            })
        }
    }

    fn max_seq_len(&self) -> usize {
        self.max_seq_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// モジュール冒頭ドキュメント参照: `ORT_DYLIB_PATH` 未設定・モデル/トークナイザ
    /// ファイル不在の環境（`--all-features` の `make test`/`make ci` の既定経路）
    /// でも `from_files` が panic せず `Err` を返すことを固定する契約テスト。
    /// 実モデル・実 dylib を必要とする推論そのものの検証は
    /// `tests/rerank_cross_encoder_recall.rs`（`#[ignore]`・opt-in）が担う。
    #[test]
    fn from_files_with_nonexistent_paths_returns_err_without_panicking() {
        let bogus = PathBuf::from("/nonexistent/path/does-not-exist.onnx");
        let bogus_tokenizer = PathBuf::from("/nonexistent/path/does-not-exist-tokenizer.json");
        let result = OnnxCrossEncoderBackend::from_files(&bogus, &bogus_tokenizer, 256);
        assert!(result.is_err());
    }

    #[test]
    fn from_files_rejects_zero_max_seq_len() {
        let bogus = PathBuf::from("/nonexistent/path/does-not-exist.onnx");
        let bogus_tokenizer = PathBuf::from("/nonexistent/path/does-not-exist-tokenizer.json");
        match OnnxCrossEncoderBackend::from_files(&bogus, &bogus_tokenizer, 0) {
            Err(e) => assert_eq!(e, CrossEncoderError::TruncationFailed),
            Ok(_) => panic!("expected TruncationFailed error for max_seq_len == 0"),
        }
    }

    /// codex-review 指摘（PR #336 P1）の固定テスト: `CrossEncoderConfig::new` を
    /// 経由しない `from_files` 直接呼び出しでも `MAX_CROSS_ENCODER_SEQ_LEN` を
    /// 超える `max_seq_len` を拒否することを確認する。
    #[test]
    fn from_files_rejects_max_seq_len_exceeding_shared_limit() {
        let bogus = PathBuf::from("/nonexistent/path/does-not-exist.onnx");
        let bogus_tokenizer = PathBuf::from("/nonexistent/path/does-not-exist-tokenizer.json");
        match OnnxCrossEncoderBackend::from_files(
            &bogus,
            &bogus_tokenizer,
            MAX_CROSS_ENCODER_SEQ_LEN + 1,
        ) {
            Err(e) => assert_eq!(e, CrossEncoderError::TruncationFailed),
            Ok(_) => panic!(
                "expected TruncationFailed error for max_seq_len > MAX_CROSS_ENCODER_SEQ_LEN"
            ),
        }
    }
}
