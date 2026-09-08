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
//! （Issue #514・#521。索引ノード常駐精度の opt-in）へ渡す。
//!
//! フィルタ付き ANN の探索パラメータ opt-in（Issue #657。親 Issue #656 の
//! 「対象外」節で持ち越された 3 パラメータ）: `full_scan_ratio`
//! （Issue #409。可視カーディナリティ切替の閾値比。既定 1/10）・
//! `acorn_max_visible_ratio`（Issue #501。ACORN-1 の 2-hop 展開を有効化する
//! 可視比率の上限。既定 `None`＝無効）・`sparse_visited_max`（Issue #497。
//! visited 集合の実装切替閾値。既定 0＝常に dense）の 3 つを
//! `--hnsw-full-scan-ratio`／`--hnsw-acorn-max-visible-ratio`／
//! `--hnsw-sparse-visited-max` として opt-in 露出する。`m`／`ef_*` の CLI 露出は
//! 引き続き対象外（既定値のまま）。
//!
//! 意味検証（分母 0・`num > den`・`acorn < full_scan_ratio` 等）は
//! `ValidatedHnswParams::with_full_scan_ratio`／`with_acorn_max_visible_ratio`
//! （engine 側の唯一の検証入口）へ一本化し、本モジュールでは形状（`<num>/<den>`
//! の `u32`／`u32`、`sparse_visited_max` の `usize`）のみを厳密パースする
//! （二重実装しない）。[`HnswTuning::is_empty`] が偽（＝ 1 つ以上指定）なのに
//! `SearchEngineChoice::Default` の場合は fail-closed で拒否する
//! （`--search-engine` 未指定／`default` のまま `--hnsw-*` を指定すると
//! 黙って無視され「ANN チューニングが効いている」と誤認したまま brute-force
//! 経路が走る事故を防ぐ）。テナント存在情報に繋がる `full_scan_ratio`／
//! `acorn_max_visible_ratio` は `EXPLAIN` の `hnsw_params:` 行へ出さない現行方針
//! （Issue #411）を維持する（`sparse_visited_max=` は Issue #497 で既に露出済み
//! の静的閾値区分のため現状維持）。

use engine::hnsw::{HnswParams, Ratio, ResidentPrecision, ValidatedHnswParams};
use engine::search_engine::SearchEngineKind;

/// `--hnsw-full-scan-ratio` の CLI フラグ名（Issue #657）。
pub const FULL_SCAN_RATIO_FLAG: &str = "--hnsw-full-scan-ratio";
/// `--hnsw-acorn-max-visible-ratio` の CLI フラグ名（Issue #657）。
pub const ACORN_MAX_VISIBLE_RATIO_FLAG: &str = "--hnsw-acorn-max-visible-ratio";
/// `--hnsw-sparse-visited-max` の CLI フラグ名（Issue #657）。
pub const SPARSE_VISITED_MAX_FLAG: &str = "--hnsw-sparse-visited-max";

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

/// `--hnsw-*` 3 フラグの解決済み値（Issue #657）。フィールドはいずれも
/// `None` が「未指定」を表す（`Default` 導出＝全 `None`）。`main.rs` が CLI
/// パースの結果を詰め、[`SearchEngineChoice::to_engine_kind_with`] へ渡す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HnswTuning {
    pub full_scan_ratio: Option<Ratio>,
    pub acorn_max_visible_ratio: Option<Ratio>,
    pub sparse_visited_max: Option<usize>,
}

impl HnswTuning {
    /// 3 フィールドすべてが未指定（`None`）かどうか。`SearchEngineChoice::
    /// Default`（`--search-engine` 未指定／`default`）との組合せ検証
    /// （D1）に使う。
    pub fn is_empty(&self) -> bool {
        self.full_scan_ratio.is_none()
            && self.acorn_max_visible_ratio.is_none()
            && self.sparse_visited_max.is_none()
    }
}

/// `<num>/<den>` 形式（`u32`／`u32`）の厳密パース（Issue #657）。
///
/// trim・符号・小数・パーセント表記・空白混入はいずれも不受理（`parse` の
/// 「厳密一致のみ受理」方針を踏襲）。`den == 0`・`num > den` の意味検証は
/// ここでは行わず `ValidatedHnswParams::with_full_scan_ratio`／
/// `with_acorn_max_visible_ratio` へ委ねる（D4。二重実装しない）。
///
/// untrusted な CLI 引数からのパースのため `unwrap`/`expect`/添字アクセスは
/// 使わず、`split_once`・`str::parse::<u32>` の `Result` のみで判定する。
pub fn parse_ratio(raw: &str) -> Result<Ratio, String> {
    let Some((num_str, den_str)) = raw.split_once('/') else {
        return Err(format!("expected <num>/<den>, got {raw:?}"));
    };
    let numerator: u32 = parse_strict_decimal(num_str)
        .ok_or_else(|| format!("expected <num>/<den>, got {raw:?}"))?;
    let denominator: u32 = parse_strict_decimal(den_str)
        .ok_or_else(|| format!("expected <num>/<den>, got {raw:?}"))?;
    Ok(Ratio {
        numerator,
        denominator,
    })
}

/// `sparse_visited_max` の非負整数パース（Issue #657）。trim・符号・小数・
/// 空白混入はいずれも不受理。
pub fn parse_sparse_visited_max(raw: &str) -> Result<usize, String> {
    parse_strict_decimal(raw).ok_or_else(|| format!("expected a non-negative integer, got {raw:?}"))
}

/// ASCII 数字のみからなる非空文字列を厳密パースする（`str::parse::<u32|usize>`
/// が受理してしまう先頭 `+`・空白・全角数字等を弾くための下請け。`raw` が
/// 空・ASCII 数字以外を 1 文字でも含む場合は `None`。untrusted な CLI 引数
/// からのパースのため `unwrap`/`expect`/添字アクセスは使わない）。
fn parse_strict_decimal<T: std::str::FromStr>(raw: &str) -> Option<T> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
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
        self.to_engine_kind_with(HnswTuning::default())
    }

    /// [`Self::to_engine_kind`] へ `--hnsw-*` 探索パラメータ opt-in（Issue #657）
    /// の反映を加えたもの。`tuning` が空（全 `None`）のときは
    /// [`Self::to_engine_kind`] とビット同一の結果を返す（R2。3 パラメータは
    /// `ValidatedHnswParams::new` の既定値のまま）。
    ///
    /// `Self::Default` に対して `tuning` が空でない場合は `Err`（D1。
    /// `--search-engine` 未指定／`default` のまま `--hnsw-*` を指定する構成を
    /// 拒否し、黙って無視して「チューニングが効いている」と誤認したまま
    /// brute-force が走る事故を防ぐ）。
    ///
    /// 適用順序は `with_full_scan_ratio` → `with_acorn_max_visible_ratio` →
    /// `with_sparse_visited_max` に固定する（D5。相互検証
    /// （`full_scan_ratio <= acorn_max_visible_ratio`）はどちらの適用順でも
    /// 安全だが、順序を固定することでエラー文言を決定的にする）。エラーには
    /// 該当フラグ名（[`FULL_SCAN_RATIO_FLAG`] 等）を接頭辞として付け、
    /// 呼び出し元・子プロセステストがどのフラグに起因するかを判別できる
    /// ようにする。
    pub fn to_engine_kind_with(
        self,
        tuning: HnswTuning,
    ) -> Result<Option<SearchEngineKind>, String> {
        let precision = match self {
            Self::Default => {
                if !tuning.is_empty() {
                    return Err(format!(
                        "{FULL_SCAN_RATIO_FLAG}/{ACORN_MAX_VISIBLE_RATIO_FLAG}/{SPARSE_VISITED_MAX_FLAG} require --search-engine to be one of hnsw, hnsw_f16, hnsw_i8 (got default/unset)"
                    ));
                }
                return Ok(None);
            }
            Self::Hnsw => ResidentPrecision::F32,
            Self::HnswF16 => ResidentPrecision::F16,
            Self::HnswI8 => ResidentPrecision::I8,
        };
        let mut validated = ValidatedHnswParams::new(HnswParams::default())
            .map_err(|e| format!("default HNSW params failed to validate: {e}"))?
            .with_resident_precision(precision);
        if let Some(ratio) = tuning.full_scan_ratio {
            validated = validated
                .with_full_scan_ratio(ratio)
                .map_err(|e| format!("{FULL_SCAN_RATIO_FLAG}: {e}"))?;
        }
        if let Some(ratio) = tuning.acorn_max_visible_ratio {
            validated = validated
                .with_acorn_max_visible_ratio(ratio)
                .map_err(|e| format!("{ACORN_MAX_VISIBLE_RATIO_FLAG}: {e}"))?;
        }
        if let Some(max) = tuning.sparse_visited_max {
            validated = validated.with_sparse_visited_max(max);
        }
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

    // Issue #657: `--hnsw-*` 探索パラメータ opt-in のパーサ・組み立て単体テスト。
    // wire 経由の実行契約（`EXPLAIN` 非露出・RLS 非漏えい）は
    // `tests/wire_search_engine_opt.rs`（in-process）・
    // `tests/wire_search_engine_cli.rs`（子プロセス）が担う。

    #[test]
    fn parse_ratio_accepts_well_formed_values() {
        for raw in ["1/10", "1/1", "0/1", "4294967295/4294967295"] {
            let ratio = parse_ratio(raw).unwrap_or_else(|e| panic!("{raw:?} rejected: {e}"));
            assert_eq!(ratio.to_string(), raw);
        }
    }

    #[test]
    fn parse_ratio_rejects_malformed_values() {
        for raw in [
            "",
            "1",
            "1/",
            "/2",
            "1/2/3",
            " 1/2",
            "1/2 ",
            "+1/2",
            "-1/2",
            "0.5",
            "50%",
            "4294967296/1",
            "1/4294967296",
            "1\0/2",
        ] {
            assert!(parse_ratio(raw).is_err(), "expected {raw:?} to be rejected");
        }
    }

    #[test]
    fn parse_ratio_rejection_error_does_not_leak_control_characters_unescaped() {
        let err = parse_ratio("1\0/2").expect_err("must reject control characters");
        // untrusted な raw が Debug（`{:?}`）表記でエスケープされ、生の NUL が
        // そのままエラー文言へ混入しないことを確認する（`parse` の
        // `parse_rejects_control_character_injection` と同じ防御的姿勢）。
        assert!(err.contains("\\0") || !err.contains('\0'));
    }

    #[test]
    fn parse_sparse_visited_max_accepts_well_formed_values() {
        for raw in ["0", "8", &usize::MAX.to_string()] {
            let v =
                parse_sparse_visited_max(raw).unwrap_or_else(|e| panic!("{raw:?} rejected: {e}"));
            assert_eq!(v.to_string(), *raw);
        }
    }

    #[test]
    fn parse_sparse_visited_max_rejects_malformed_values() {
        for raw in ["", "-1", "1.5", " 8", "8 ", "abc"] {
            assert!(
                parse_sparse_visited_max(raw).is_err(),
                "expected {raw:?} to be rejected"
            );
        }
    }

    /// R2: `tuning` が空のとき [`SearchEngineChoice::to_engine_kind_with`] は
    /// [`SearchEngineChoice::to_engine_kind`] とビット同一（既定値のまま）。
    #[test]
    fn empty_tuning_matches_to_engine_kind_for_all_hnsw_tokens() {
        for choice in [
            SearchEngineChoice::Hnsw,
            SearchEngineChoice::HnswF16,
            SearchEngineChoice::HnswI8,
        ] {
            assert_eq!(
                choice.to_engine_kind_with(HnswTuning::default()),
                choice.to_engine_kind()
            );
            let kind = choice
                .to_engine_kind_with(HnswTuning::default())
                .expect("valid")
                .expect("Some");
            let SearchEngineKind::Hnsw(params) = kind else {
                panic!("expected Hnsw kind");
            };
            assert_eq!(
                params.full_scan_ratio(),
                Ratio {
                    numerator: 1,
                    denominator: 10
                }
            );
            assert_eq!(params.acorn_max_visible_ratio(), None);
            assert_eq!(params.sparse_visited_max(), 0);
        }
    }

    /// D1: `Default`（`--search-engine` 未指定／`default`）に対して `tuning` が
    /// 非空だと fail-closed で拒否し、該当フラグ名と `--search-engine` を
    /// エラーへ含む。
    #[test]
    fn default_choice_with_nonempty_tuning_is_rejected() {
        let cases = [
            HnswTuning {
                full_scan_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 4,
                }),
                ..HnswTuning::default()
            },
            HnswTuning {
                acorn_max_visible_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 1,
                }),
                ..HnswTuning::default()
            },
            HnswTuning {
                sparse_visited_max: Some(8),
                ..HnswTuning::default()
            },
        ];
        for tuning in cases {
            let err = SearchEngineChoice::Default
                .to_engine_kind_with(tuning)
                .expect_err("must reject tuning without opt-in engine");
            assert!(err.contains("--search-engine"), "unexpected error: {err}");
        }
    }

    /// R3: 意味検証（`ValidatedHnswParams::with_full_scan_ratio`／
    /// `with_acorn_max_visible_ratio`）の失敗が呼び出し元へ伝播し、
    /// フラグ名を含む。
    #[test]
    fn semantic_validation_errors_propagate_with_flag_name() {
        // 分母 0。
        let err = SearchEngineChoice::Hnsw
            .to_engine_kind_with(HnswTuning {
                full_scan_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 0,
                }),
                ..HnswTuning::default()
            })
            .expect_err("den=0 must be rejected");
        assert!(
            err.contains(FULL_SCAN_RATIO_FLAG),
            "unexpected error: {err}"
        );

        // num > den。
        let err = SearchEngineChoice::Hnsw
            .to_engine_kind_with(HnswTuning {
                full_scan_ratio: Some(Ratio {
                    numerator: 3,
                    denominator: 2,
                }),
                ..HnswTuning::default()
            })
            .expect_err("num>den must be rejected");
        assert!(
            err.contains(FULL_SCAN_RATIO_FLAG),
            "unexpected error: {err}"
        );

        // acorn < 既定 full_scan_ratio（1/10）。
        let err = SearchEngineChoice::Hnsw
            .to_engine_kind_with(HnswTuning {
                acorn_max_visible_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 20,
                }),
                ..HnswTuning::default()
            })
            .expect_err("acorn < full_scan_ratio must be rejected");
        assert!(
            err.contains(ACORN_MAX_VISIBLE_RATIO_FLAG),
            "unexpected error: {err}"
        );

        // full=1/2 かつ acorn=1/4（acorn < full）。
        let err = SearchEngineChoice::Hnsw
            .to_engine_kind_with(HnswTuning {
                full_scan_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 2,
                }),
                acorn_max_visible_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 4,
                }),
                ..HnswTuning::default()
            })
            .expect_err("acorn < full must be rejected");
        assert!(
            err.contains(ACORN_MAX_VISIBLE_RATIO_FLAG),
            "unexpected error: {err}"
        );

        // full=1/4 かつ acorn=1/2（acorn >= full）は受理される。
        SearchEngineChoice::Hnsw
            .to_engine_kind_with(HnswTuning {
                full_scan_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 4,
                }),
                acorn_max_visible_ratio: Some(Ratio {
                    numerator: 1,
                    denominator: 2,
                }),
                ..HnswTuning::default()
            })
            .expect("acorn >= full must be accepted");
    }

    /// 指定値がそのまま `ValidatedHnswParams` の getter へ反映されることを
    /// 確認する（3 パラメータ・3 トークン全部の組合せの代表例）。
    #[test]
    fn tuning_values_are_reflected_in_validated_params() {
        let tuning = HnswTuning {
            full_scan_ratio: Some(Ratio {
                numerator: 1,
                denominator: 2,
            }),
            acorn_max_visible_ratio: Some(Ratio {
                numerator: 1,
                denominator: 1,
            }),
            sparse_visited_max: Some(8),
        };
        for (choice, expected_precision) in [
            (SearchEngineChoice::Hnsw, ResidentPrecision::F32),
            (SearchEngineChoice::HnswF16, ResidentPrecision::F16),
            (SearchEngineChoice::HnswI8, ResidentPrecision::I8),
        ] {
            let kind = choice
                .to_engine_kind_with(tuning)
                .expect("valid tuning")
                .expect("Some");
            let SearchEngineKind::Hnsw(params) = kind else {
                panic!("expected Hnsw kind");
            };
            assert_eq!(
                params.full_scan_ratio(),
                Ratio {
                    numerator: 1,
                    denominator: 2
                }
            );
            assert_eq!(
                params.acorn_max_visible_ratio(),
                Some(Ratio {
                    numerator: 1,
                    denominator: 1
                })
            );
            assert_eq!(params.sparse_visited_max(), 8);
            assert_eq!(params.resident_precision(), expected_precision);
        }
    }
}
