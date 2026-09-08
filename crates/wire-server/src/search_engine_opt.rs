//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--search-engine`
//! opt-in CLI 引数の閉じた語彙パーサ（Issue #656）。
//!
//! `--planner-endpoint`／`--planner-model`／`--embedder-hashing-dim`
//! （TASK-117・PLAN-9）と同型の「プロセス起動時にのみ明示指定する注入点」で
//! あり、`engine::search_engine::SearchEngineKind`（Issue #407。ANN opt-in の
//! 選択は `EngineCore::open`／`open_with_engine` の呼び出し元がコード上で
//! 明示指定する以外の経路を持たない設計。同モジュールドキュメント参照）へ
//! untrusted な CLI 文字列から到達する唯一の入口を本モジュールに置く。
//!
//! `engine::search_engine` は意図的に `FromStr` を持たない
//! （`docs/design/hnsw-search-engine-wiring.md`「`FromStr`／設定文字列
//! パーサは追加しない」節）。本モジュールは untrusted な文字列を **wire-server
//! 側のみで閉じた 4 値の語彙**として判定し、通過した値だけを
//! `engine::search_engine::hnsw_kind`（Issue #407 が守る「未検証 `HnswParams`
//! からの唯一の検証入口」）・`ValidatedHnswParams::with_resident_precision`
//! （Issue #514・#521。索引ノード常駐精度の opt-in）へ渡す。探索パラメータ
//! （`m`／`ef_*`／`full_scan_ratio`／`sparse_visited_max`／ACORN）は本 Issue の
//! 対象外で常に既定値のまま（Issue 本文「対象外」節）。

use engine::hnsw::{HnswParams, ResidentPrecision, ValidatedHnswParams};
use engine::search_engine::SearchEngineKind;

/// `--search-engine` が受理する語彙（順序は `parse` の分岐・エラーメッセージの
/// 一覧順・README 記載順の単一情報源）。
pub const TOKENS: [&str; 4] = ["default", "hnsw", "hnsw_f16", "hnsw_i8"];

/// `--search-engine <token>` の解決結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngineChoice {
    /// 未指定と同じ既定経路（`EngineCore::open` をそのまま通す。ブルート
    /// フォース）。
    Default,
    /// HNSW opt-in（索引ノードは f32 常駐）。
    Hnsw,
    /// HNSW opt-in・索引ノード f16 常駐（Issue #514）。
    HnswF16,
    /// HNSW opt-in・索引ノード I8（SQ8）常駐（Issue #521・#522）。
    HnswI8,
}

/// `raw`（CLI 引数の値）を [`TOKENS`] の厳密一致でのみ受理する
/// （trim・大文字小文字の読み替えはしない。`sql/mode.rs`・
/// `tests/fixtures/recall_engine.rs::RecallEngine::parse` と同じ
/// 「厳密一致のみ受理」方針。曖昧な入力を黙って読み替えると、typo で意図と
/// 異なるエンジンが選ばれる事故を fail-closed で防げなくなる）。
/// ベンチ env のトークン `brute_force`（`RecallEngine`）はここでは受理しない
/// （CLI の語彙は本モジュールの 4 値へ閉じる。README に対応関係を明記する）。
pub fn parse(raw: &str) -> Result<SearchEngineChoice, String> {
    match raw {
        "default" => Ok(SearchEngineChoice::Default),
        "hnsw" => Ok(SearchEngineChoice::Hnsw),
        "hnsw_f16" => Ok(SearchEngineChoice::HnswF16),
        "hnsw_i8" => Ok(SearchEngineChoice::HnswI8),
        other => Err(format!(
            "--search-engine must be one of {TOKENS:?} (got {other:?})"
        )),
    }
}

impl SearchEngineChoice {
    /// [`TOKENS`] のうち自身に対応する文字列表現（診断・テスト用）。
    pub fn token(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Hnsw => "hnsw",
            Self::HnswF16 => "hnsw_f16",
            Self::HnswI8 => "hnsw_i8",
        }
    }

    /// `engine::core::EngineCore::open_with_engine`／`from_storage_with_engine`
    /// へ渡す `SearchEngineKind` を組み立てる。`Default` は `None`（呼び出し元
    /// は既存の `EngineCore::open` をそのまま呼び、`open_with_engine` へは
    /// 委譲しない契約。既定経路をビット同一に保つため）。
    ///
    /// `HnswParams::default()` は本モジュールの製造元であり `validate()` を
    /// 拒否しない値のみを渡す既知の定数のため `Result` は理論上失敗しないが、
    /// `unwrap`/`expect` を避け `ValidatedHnswParams::new` の `Result` を
    /// そのまま呼び出し元へ伝播する（`ValidatedHnswParams::new` が唯一の
    /// 未検証入力の検証入口という契約を、本関数もコード上迂回しないことを
    /// 明示するため。`crates/engine/tests/fixtures/recall_engine.rs` と同型）。
    pub fn to_engine_kind(self) -> Result<Option<SearchEngineKind>, String> {
        let precision = match self {
            Self::Default => return Ok(None),
            Self::Hnsw => ResidentPrecision::F32,
            Self::HnswF16 => ResidentPrecision::F16,
            Self::HnswI8 => ResidentPrecision::I8,
        };
        let validated = ValidatedHnswParams::new(HnswParams::default())
            .map_err(|e| format!("default HNSW params failed to validate: {e}"))?
            .with_resident_precision(precision);
        Ok(Some(SearchEngineKind::Hnsw(validated)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_all_tokens() {
        for tok in TOKENS {
            assert!(parse(tok).is_ok(), "expected {tok:?} to be accepted");
        }
    }

    #[test]
    fn parse_rejects_case_variants_and_whitespace() {
        for raw in [
            "HNSW",
            " hnsw",
            "hnsw ",
            "hnsw-f16",
            "HNSW_F16",
            "Default",
            "",
            "brute_force",
            "hnsw_i8\n",
        ] {
            assert!(
                parse(raw).is_err(),
                "expected {raw:?} to be rejected (strict match only)"
            );
        }
    }

    #[test]
    fn parse_rejects_control_character_injection() {
        // untrusted 引数に制御文字が混じっても厳密一致で弾かれ、エラー文言へ
        // そのまま埋め込まれても Debug 表記（`{:?}`）でエスケープされることを
        // 確認する（`build_query_planner` の model 名検証と同じ防御的姿勢）。
        let err = parse("hnsw\0bogus").expect_err("must reject control characters");
        assert!(err.contains("--search-engine"));
    }

    #[test]
    fn default_token_maps_to_none() {
        assert_eq!(
            SearchEngineChoice::Default.to_engine_kind(),
            Ok(None),
            "Default must map to None so callers keep using EngineCore::open unchanged"
        );
    }

    #[test]
    fn hnsw_token_maps_to_f32_resident() {
        let kind = SearchEngineChoice::Hnsw
            .to_engine_kind()
            .expect("valid params")
            .expect("Some for hnsw");
        match kind {
            SearchEngineKind::Hnsw(params) => {
                assert_eq!(params.resident_precision(), ResidentPrecision::F32);
            }
            other => panic!("expected Hnsw kind, got {other:?}"),
        }
    }

    #[test]
    fn hnsw_f16_token_maps_to_f16_resident() {
        let kind = SearchEngineChoice::HnswF16
            .to_engine_kind()
            .expect("valid params")
            .expect("Some for hnsw_f16");
        match kind {
            SearchEngineKind::Hnsw(params) => {
                assert_eq!(params.resident_precision(), ResidentPrecision::F16);
            }
            other => panic!("expected Hnsw kind, got {other:?}"),
        }
    }

    #[test]
    fn hnsw_i8_token_maps_to_i8_resident() {
        let kind = SearchEngineChoice::HnswI8
            .to_engine_kind()
            .expect("valid params")
            .expect("Some for hnsw_i8");
        match kind {
            SearchEngineKind::Hnsw(params) => {
                assert_eq!(params.resident_precision(), ResidentPrecision::I8);
            }
            other => panic!("expected Hnsw kind, got {other:?}"),
        }
    }

    #[test]
    fn token_round_trips_through_parse() {
        for tok in TOKENS {
            let choice = parse(tok).expect("valid token");
            assert_eq!(choice.token(), tok);
        }
    }
}
